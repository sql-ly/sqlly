//! The spill contract for an executor that cannot write spill files itself:
//! [`SpillStage`] sits on a grid run's event stream and moves the rows past
//! the spill threshold (or the per-set memory budget) into a [`StageStore`].
//! The browser build stores them in the Origin Private File System
//! (`crate::browser::spill`); keeping the logic here, behind the store
//! trait, lets it be tested natively. [`RowCache`] is the page cache the
//! browser's pager answers synchronous lookups from.

use std::collections::HashMap;
use std::sync::Arc;

use sqlly_core::{CellValue, ResultColumn};

use super::{QueryEvent, SpillManifestData, SpillResultSetData, estimate_row_bytes};

/// Rows of each spilled set kept in the page cache when it lands, so the grid
/// shows its first window without waiting for a read.
pub(crate) const SEED_ROWS: usize = 8_192;

/// A set only moves to storage when at least this much quota is free.
const MIN_FREE_BYTES: u64 = 64 << 20;

/// Quota left unused, so the rest of the origin's storage keeps working.
const QUOTA_RESERVE_BYTES: u64 = 32 << 20;

/// How often a withheld set reports its running row count.
const PROGRESS_MS: f64 = 50.0;

/// One result set being written to storage.
pub(crate) trait StageSink {
    fn append(&mut self, row: &[CellValue]) -> Result<(), String>;
    /// Starts writing to storage; rows appended before this were buffered.
    fn open(&mut self) -> Result<(), String>;
    fn rows(&self) -> usize;
    /// Bytes written so far.
    fn bytes(&self) -> u64;
    /// Ends the set and returns its spill path.
    fn finish(self) -> Result<String, String>;
}

/// Where a [`SpillStage`] puts withheld rows.
pub(crate) trait StageStore {
    type Sink: StageSink;
    fn sink(&self) -> Option<Self::Sink>;
    /// Storage quota still free.
    fn free_bytes(&self) -> u64;
    /// Milliseconds on any monotonic clock, for the progress cadence.
    fn now_ms(&self) -> f64;
    /// A spill landed at `path`; `seed` holds its first rows.
    fn landed(&self, path: &str, seed: Vec<Vec<CellValue>>);
}

/// Where a result set's rows go.
enum Mode {
    /// Under the limits: rows are forwarded and mirrored into the sink.
    Forwarding,
    /// Past the limits: rows go to storage only.
    Spilling,
    /// Storage filled up: further rows are counted and dropped.
    Full,
    /// Spilling is not possible: every row is forwarded.
    Memory,
}

struct SetSpill<K> {
    columns: Vec<ResultColumn>,
    mode: Mode,
    sink: Option<K>,
    total: usize,
    forwarded: usize,
    forwarded_bytes: usize,
    seed: Vec<Vec<CellValue>>,
    quota: u64,
    last_progress: f64,
}

/// Turns a grid run's events into the spill contract the desktop executor
/// follows: each result set's first rows (up to the spill threshold and the
/// memory budget) are forwarded as [`QueryEvent::Rows`], the rest go to the
/// store with count-only [`QueryEvent::RowsProgress`] while they stream,
/// and a [`QueryEvent::SpillManifest`] naming every spilled set comes just
/// before the terminal event. Rows are mirrored into the sink from the
/// first one, so a set that crosses the limit is stored whole.
pub(crate) struct SpillStage<S: StageStore> {
    store: S,
    threshold: usize,
    budget: usize,
    sets: HashMap<usize, SetSpill<S::Sink>>,
    order: Vec<usize>,
    messages: Vec<String>,
}

impl<S: StageStore> SpillStage<S> {
    pub(crate) fn new(store: S, threshold: usize, budget: usize) -> Self {
        Self {
            store,
            threshold: threshold.max(1),
            budget: budget.max(1),
            sets: HashMap::new(),
            order: Vec::new(),
            messages: Vec::new(),
        }
    }

    /// Feeds one event; the events to pass on are pushed to `out`.
    pub(crate) fn push(&mut self, event: QueryEvent, out: &mut Vec<QueryEvent>) {
        match event {
            QueryEvent::ResultSet { index, columns } => {
                self.sets.insert(
                    index,
                    SetSpill {
                        columns: columns.clone(),
                        mode: Mode::Forwarding,
                        sink: None,
                        total: 0,
                        forwarded: 0,
                        forwarded_bytes: 0,
                        seed: Vec::new(),
                        quota: 0,
                        last_progress: f64::NEG_INFINITY,
                    },
                );
                self.order.push(index);
                out.push(QueryEvent::ResultSet { index, columns });
            }
            QueryEvent::Rows { index, rows } => self.rows(index, rows, out),
            terminal @ (QueryEvent::Completed { .. }
            | QueryEvent::Error { .. }
            | QueryEvent::Cancelled) => {
                self.finish(out);
                out.push(terminal);
            }
            other => out.push(other),
        }
    }

    fn rows(&mut self, index: usize, rows: Vec<Vec<CellValue>>, out: &mut Vec<QueryEvent>) {
        let Some(set) = self.sets.get_mut(&index) else {
            out.push(QueryEvent::Rows { index, rows });
            return;
        };
        let mut forward = Vec::new();
        for row in rows {
            set.total += 1;
            match set.mode {
                Mode::Memory => forward.push(row),
                Mode::Full => {}
                Mode::Forwarding => {
                    if set.sink.is_none() {
                        set.sink = self.store.sink();
                    }
                    if let Some(sink) = &mut set.sink
                        && sink.append(&row).is_err()
                    {
                        set.sink = None;
                    }
                    let bytes = estimate_row_bytes(&row);
                    if set.forwarded < self.threshold && set.forwarded_bytes + bytes <= self.budget
                    {
                        set.forwarded += 1;
                        set.forwarded_bytes += bytes;
                        if set.seed.len() < SEED_ROWS {
                            set.seed.push(row.clone());
                        }
                        forward.push(row);
                        continue;
                    }
                    // The first withheld row: move the set to storage.
                    let free = self.store.free_bytes();
                    let opened = free >= MIN_FREE_BYTES
                        && set.sink.as_mut().is_some_and(|sink| sink.open().is_ok());
                    if opened {
                        set.quota = free - QUOTA_RESERVE_BYTES;
                        set.mode = Mode::Spilling;
                    } else {
                        set.sink = None;
                        set.seed = Vec::new();
                        set.mode = Mode::Memory;
                        self.messages.push(format!(
                            "Result set {}: browser storage is not available, so all of its rows stay in memory.",
                            index + 1
                        ));
                        forward.push(row);
                    }
                }
                Mode::Spilling => {
                    let Some(sink) = &mut set.sink else {
                        continue;
                    };
                    let written = sink.append(&row).is_ok();
                    if !written || sink.bytes() > set.quota {
                        set.mode = Mode::Full;
                        self.messages.push(format!(
                            "Result set {}: browser storage is full, so only its first {} rows were kept.",
                            index + 1,
                            sink.rows()
                        ));
                    }
                }
            }
        }
        if !forward.is_empty() {
            out.push(QueryEvent::Rows {
                index,
                rows: forward,
            });
        }
        if matches!(set.mode, Mode::Spilling | Mode::Full) {
            let now = self.store.now_ms();
            if now - set.last_progress >= PROGRESS_MS {
                set.last_progress = now;
                out.push(QueryEvent::RowsProgress {
                    index,
                    rows_seen: set.total,
                });
            }
        }
    }

    fn finish(&mut self, out: &mut Vec<QueryEvent>) {
        let mut result_sets = Vec::new();
        for index in std::mem::take(&mut self.order) {
            let Some(set) = self.sets.remove(&index) else {
                continue;
            };
            if !matches!(set.mode, Mode::Spilling | Mode::Full) {
                continue;
            }
            let Some(sink) = set.sink else {
                continue;
            };
            let row_count = sink.rows();
            match sink.finish() {
                Ok(path) => {
                    self.store.landed(&path, set.seed);
                    result_sets.push(SpillResultSetData {
                        result_set_index: index,
                        columns: set.columns,
                        row_count,
                        spill_file: path,
                    });
                }
                Err(error) => self.messages.push(format!(
                    "Result set {}: could not save its rows to browser storage ({error}).",
                    index + 1
                )),
            }
        }
        for text in std::mem::take(&mut self.messages) {
            out.push(QueryEvent::Message { text });
        }
        if !result_sets.is_empty() {
            out.push(QueryEvent::SpillManifest {
                manifest: SpillManifestData {
                    result_sets,
                    messages: Vec::new(),
                    error: None,
                    elapsed_ms: None,
                },
            });
        }
    }
}

/// Rows per [`RowCache`] entry.
pub(crate) const CHUNK_ROWS: usize = 1_024;

struct CachedChunk {
    rows: Arc<Vec<Vec<CellValue>>>,
    bytes: usize,
    used: u64,
}

/// Decoded rows of recent reads, by `(spill name, chunk)`; least recently
/// used chunks go first once the estimated size passes the budget.
pub(crate) struct RowCache {
    chunks: HashMap<(String, usize), CachedChunk>,
    bytes: usize,
    budget: usize,
    tick: u64,
}

impl RowCache {
    pub(crate) fn new(budget: usize) -> Self {
        Self {
            chunks: HashMap::new(),
            bytes: 0,
            budget,
            tick: 0,
        }
    }

    /// Rows `[start, start + len)` of `name`, if every one is cached.
    pub(crate) fn get(
        &mut self,
        name: &str,
        start: usize,
        len: usize,
    ) -> Option<Vec<Vec<CellValue>>> {
        let end = start + len;
        let mut out = Vec::with_capacity(len);
        let mut at = start;
        while at < end {
            let chunk = at / CHUNK_ROWS;
            let base = chunk * CHUNK_ROWS;
            self.tick += 1;
            let entry = self.chunks.get_mut(&(name.to_string(), chunk))?;
            entry.used = self.tick;
            let to = (end - base).min(entry.rows.len());
            if at - base >= to {
                return None;
            }
            out.extend_from_slice(&entry.rows[at - base..to]);
            at = base + to;
        }
        Some(out)
    }

    /// Caches `rows`, which start at the chunk boundary `start` of a spill of
    /// `total` rows. Only whole chunks are kept, plus the spill's last chunk
    /// when `rows` reach `total`.
    pub(crate) fn put(
        &mut self,
        name: &str,
        start: usize,
        rows: Vec<Vec<CellValue>>,
        total: usize,
    ) {
        debug_assert_eq!(start % CHUNK_ROWS, 0);
        let mut base = start;
        let mut rows = rows.into_iter().peekable();
        while rows.peek().is_some() {
            let chunk: Vec<_> = rows.by_ref().take(CHUNK_ROWS).collect();
            if chunk.len() < CHUNK_ROWS && base + chunk.len() < total {
                break;
            }
            let len = chunk.len();
            let bytes = chunk.iter().map(|r| estimate_row_bytes(r)).sum::<usize>() + 64;
            self.tick += 1;
            let entry = CachedChunk {
                rows: Arc::new(chunk),
                bytes,
                used: self.tick,
            };
            if let Some(old) = self
                .chunks
                .insert((name.to_string(), base / CHUNK_ROWS), entry)
            {
                self.bytes -= old.bytes;
            }
            self.bytes += bytes;
            base += len;
        }
        while self.bytes > self.budget {
            let Some(oldest) = self
                .chunks
                .iter()
                .min_by_key(|(_, c)| c.used)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some(gone) = self.chunks.remove(&oldest) {
                self.bytes -= gone.bytes;
            }
        }
    }

    /// Drops every cached row of `name`.
    pub(crate) fn forget(&mut self, name: &str) {
        let bytes = &mut self.bytes;
        self.chunks.retain(|(spill, _), chunk| {
            let keep = spill != name;
            if !keep {
                *bytes -= chunk.bytes;
            }
            keep
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    fn row(i: usize) -> Vec<CellValue> {
        vec![CellValue::I64(i as i64), CellValue::String(format!("r{i}"))]
    }

    fn rows(range: std::ops::Range<usize>) -> Vec<Vec<CellValue>> {
        range.map(row).collect()
    }

    /// What the fake store saw.
    #[derive(Default)]
    struct Log {
        stored: RefCell<HashMap<String, Vec<Vec<CellValue>>>>,
        seeds: RefCell<HashMap<String, usize>>,
        dropped: Cell<usize>,
        next: Cell<usize>,
    }

    struct FakeSink {
        log: Rc<Log>,
        rows: Vec<Vec<CellValue>>,
        opened: bool,
        fail_open: bool,
        fail_after: Option<usize>,
    }

    impl StageSink for FakeSink {
        fn append(&mut self, row: &[CellValue]) -> Result<(), String> {
            if self.fail_after.is_some_and(|n| self.rows.len() >= n) {
                return Err("disk full".into());
            }
            self.rows.push(row.to_vec());
            Ok(())
        }
        fn open(&mut self) -> Result<(), String> {
            if self.fail_open {
                return Err("no OPFS".into());
            }
            self.opened = true;
            Ok(())
        }
        fn rows(&self) -> usize {
            self.rows.len()
        }
        fn bytes(&self) -> u64 {
            self.rows.len() as u64 * 100
        }
        fn finish(mut self) -> Result<String, String> {
            let n = self.log.next.get();
            self.log.next.set(n + 1);
            let path = format!("opfs:spill-{n}");
            self.log
                .stored
                .borrow_mut()
                .insert(path.clone(), std::mem::take(&mut self.rows));
            Ok(path)
        }
    }

    impl Drop for FakeSink {
        fn drop(&mut self) {
            if !self.rows.is_empty() {
                self.log.dropped.set(self.log.dropped.get() + 1);
            }
        }
    }

    struct FakeStore {
        log: Rc<Log>,
        free: u64,
        fail_open: bool,
        fail_after: Option<usize>,
        clock: Cell<f64>,
    }

    impl FakeStore {
        fn new(log: &Rc<Log>) -> Self {
            Self {
                log: Rc::clone(log),
                free: 1 << 40,
                fail_open: false,
                fail_after: None,
                clock: Cell::new(0.0),
            }
        }
    }

    impl StageStore for FakeStore {
        type Sink = FakeSink;
        fn sink(&self) -> Option<FakeSink> {
            Some(FakeSink {
                log: Rc::clone(&self.log),
                rows: Vec::new(),
                opened: false,
                fail_open: self.fail_open,
                fail_after: self.fail_after,
            })
        }
        fn free_bytes(&self) -> u64 {
            self.free
        }
        fn now_ms(&self) -> f64 {
            // Each call moves the clock past the progress interval.
            let now = self.clock.get() + 100.0;
            self.clock.set(now);
            now
        }
        fn landed(&self, path: &str, seed: Vec<Vec<CellValue>>) {
            self.log
                .seeds
                .borrow_mut()
                .insert(path.to_string(), seed.len());
        }
    }

    fn columns() -> Vec<ResultColumn> {
        vec![
            ResultColumn {
                name: "id".into(),
                data_type: "int".into(),
                nullable: false,
            },
            ResultColumn {
                name: "name".into(),
                data_type: "text".into(),
                nullable: true,
            },
        ]
    }

    /// Runs one result set of `total` rows in batches of `batch` through
    /// `stage` and returns every event it passed on.
    fn run(stage: &mut SpillStage<FakeStore>, total: usize, batch: usize) -> Vec<QueryEvent> {
        let mut out = Vec::new();
        stage.push(QueryEvent::Started, &mut out);
        stage.push(
            QueryEvent::ResultSet {
                index: 0,
                columns: columns(),
            },
            &mut out,
        );
        let mut at = 0;
        while at < total {
            let end = (at + batch).min(total);
            stage.push(
                QueryEvent::Rows {
                    index: 0,
                    rows: rows(at..end),
                },
                &mut out,
            );
            at = end;
        }
        stage.push(QueryEvent::Completed { elapsed_ms: 1 }, &mut out);
        out
    }

    fn forwarded(events: &[QueryEvent]) -> Vec<Vec<CellValue>> {
        events
            .iter()
            .filter_map(|e| match e {
                QueryEvent::Rows { rows, .. } => Some(rows.clone()),
                _ => None,
            })
            .flatten()
            .collect()
    }

    fn manifest(events: &[QueryEvent]) -> Option<&SpillManifestData> {
        events.iter().find_map(|e| match e {
            QueryEvent::SpillManifest { manifest } => Some(manifest),
            _ => None,
        })
    }

    #[test]
    fn a_small_result_passes_through_and_stores_nothing() {
        let log = Rc::new(Log::default());
        let mut stage = SpillStage::new(FakeStore::new(&log), 100, usize::MAX);
        let events = run(&mut stage, 60, 25);
        assert_eq!(forwarded(&events), rows(0..60));
        assert!(manifest(&events).is_none());
        assert!(log.stored.borrow().is_empty());
        assert_eq!(log.dropped.get(), 1, "the mirrored rows are discarded");
        assert!(matches!(events.last(), Some(QueryEvent::Completed { .. })));
    }

    #[test]
    fn rows_past_the_threshold_are_stored_whole_and_named_before_completion() {
        let log = Rc::new(Log::default());
        let mut stage = SpillStage::new(FakeStore::new(&log), 100, usize::MAX);
        let events = run(&mut stage, 1_000, 64);

        assert_eq!(forwarded(&events), rows(0..100), "exactly the threshold");
        let manifest = manifest(&events).expect("a manifest");
        assert_eq!(manifest.result_sets.len(), 1);
        let set = &manifest.result_sets[0];
        assert_eq!((set.result_set_index, set.row_count), (0, 1_000));
        assert_eq!(set.columns, columns());
        assert_eq!(
            log.stored.borrow()[&set.spill_file],
            rows(0..1_000),
            "from row 0"
        );
        assert_eq!(
            log.seeds.borrow()[&set.spill_file],
            100,
            "the forwarded rows seed the cache"
        );

        let progress: Vec<usize> = events
            .iter()
            .filter_map(|e| match e {
                QueryEvent::RowsProgress { rows_seen, .. } => Some(*rows_seen),
                _ => None,
            })
            .collect();
        assert!(!progress.is_empty() && progress.windows(2).all(|w| w[0] < w[1]));
        let manifest_at = events
            .iter()
            .position(|e| matches!(e, QueryEvent::SpillManifest { .. }))
            .expect("manifest");
        assert_eq!(
            manifest_at,
            events.len() - 2,
            "manifest, then the terminal event"
        );
    }

    #[test]
    fn the_memory_budget_withholds_rows_before_the_threshold() {
        let log = Rc::new(Log::default());
        let one = estimate_row_bytes(&row(0));
        let mut stage = SpillStage::new(FakeStore::new(&log), 1_000, one * 10 + one / 2);
        let events = run(&mut stage, 500, 50);
        assert_eq!(forwarded(&events).len(), 10);
        assert_eq!(
            manifest(&events).expect("manifest").result_sets[0].row_count,
            500
        );
    }

    #[test]
    fn without_storage_every_row_stays_in_memory_with_a_message() {
        let log = Rc::new(Log::default());
        let mut store = FakeStore::new(&log);
        store.fail_open = true;
        let mut stage = SpillStage::new(store, 100, usize::MAX);
        let events = run(&mut stage, 300, 64);
        assert_eq!(forwarded(&events), rows(0..300));
        assert!(manifest(&events).is_none());
        assert!(events.iter().any(|e| matches!(e,
            QueryEvent::Message { text } if text.contains("stay in memory"))));
    }

    #[test]
    fn too_little_free_quota_keeps_rows_in_memory() {
        let log = Rc::new(Log::default());
        let mut store = FakeStore::new(&log);
        store.free = MIN_FREE_BYTES - 1;
        let mut stage = SpillStage::new(store, 100, usize::MAX);
        let events = run(&mut stage, 300, 64);
        assert_eq!(forwarded(&events).len(), 300);
        assert!(manifest(&events).is_none());
    }

    #[test]
    fn full_storage_keeps_the_stored_prefix_and_says_so() {
        let log = Rc::new(Log::default());
        let mut store = FakeStore::new(&log);
        store.fail_after = Some(400);
        let mut stage = SpillStage::new(store, 100, usize::MAX);
        let events = run(&mut stage, 1_000, 64);
        assert_eq!(forwarded(&events).len(), 100);
        let set = &manifest(&events).expect("manifest").result_sets[0];
        assert_eq!(set.row_count, 400);
        assert_eq!(log.stored.borrow()[&set.spill_file], rows(0..400));
        assert!(events.iter().any(|e| matches!(e,
            QueryEvent::Message { text } if text.contains("only its first 400 rows"))));
    }

    #[test]
    fn a_run_that_ends_early_still_names_its_spill() {
        let log = Rc::new(Log::default());
        let mut stage = SpillStage::new(FakeStore::new(&log), 10, usize::MAX);
        let mut out = Vec::new();
        stage.push(
            QueryEvent::ResultSet {
                index: 0,
                columns: columns(),
            },
            &mut out,
        );
        stage.push(
            QueryEvent::Rows {
                index: 0,
                rows: rows(0..50),
            },
            &mut out,
        );
        stage.push(QueryEvent::Cancelled, &mut out);
        assert_eq!(
            manifest(&out).expect("manifest").result_sets[0].row_count,
            50
        );
        assert!(matches!(out.last(), Some(QueryEvent::Cancelled)));
    }

    #[test]
    fn only_the_large_set_of_a_batch_spills() {
        let log = Rc::new(Log::default());
        let mut stage = SpillStage::new(FakeStore::new(&log), 100, usize::MAX);
        let mut out = Vec::new();
        for (index, total) in [(0usize, 20usize), (1, 500)] {
            stage.push(
                QueryEvent::ResultSet {
                    index,
                    columns: columns(),
                },
                &mut out,
            );
            stage.push(
                QueryEvent::Rows {
                    index,
                    rows: rows(0..total),
                },
                &mut out,
            );
        }
        stage.push(QueryEvent::Completed { elapsed_ms: 1 }, &mut out);
        let sets = &manifest(&out).expect("manifest").result_sets;
        assert_eq!(sets.len(), 1);
        assert_eq!((sets[0].result_set_index, sets[0].row_count), (1, 500));
    }

    #[test]
    fn cache_returns_only_fully_cached_ranges() {
        let mut cache = RowCache::new(usize::MAX);
        let total = CHUNK_ROWS * 3 + 10;
        cache.put("a", 0, rows(0..CHUNK_ROWS * 2), total);
        assert_eq!(cache.get("a", 5, 10), Some(rows(5..15)));
        assert_eq!(
            cache.get("a", CHUNK_ROWS - 3, 6),
            Some(rows(CHUNK_ROWS - 3..CHUNK_ROWS + 3)),
            "across a chunk edge"
        );
        assert_eq!(
            cache.get("a", CHUNK_ROWS * 2 - 1, 2),
            None,
            "runs into an uncached chunk"
        );
        assert_eq!(cache.get("b", 0, 1), None);
        assert_eq!(cache.get("a", 7, 0), Some(Vec::new()));
    }

    #[test]
    fn cache_keeps_whole_chunks_and_the_short_last_one() {
        let mut cache = RowCache::new(usize::MAX);
        let total = CHUNK_ROWS + 10;
        cache.put("a", 0, rows(0..CHUNK_ROWS + 5), total);
        assert_eq!(
            cache.get("a", CHUNK_ROWS, 1),
            None,
            "a cut-off chunk is not kept"
        );
        cache.put("a", CHUNK_ROWS, rows(CHUNK_ROWS..total), total);
        assert_eq!(
            cache.get("a", CHUNK_ROWS, 10),
            Some(rows(CHUNK_ROWS..total))
        );
    }

    #[test]
    fn cache_evicts_the_least_recently_used_chunk_and_forgets_spills() {
        let bytes = |n: usize| {
            rows(n * CHUNK_ROWS..(n + 1) * CHUNK_ROWS)
                .iter()
                .map(|r| estimate_row_bytes(r))
                .sum::<usize>()
                + 64
        };
        // Room for two chunks: the third evicts one.
        let mut cache = RowCache::new(bytes(0) + bytes(1));
        let total = CHUNK_ROWS * 10;
        cache.put("a", 0, rows(0..CHUNK_ROWS), total);
        cache.put("a", CHUNK_ROWS, rows(CHUNK_ROWS..CHUNK_ROWS * 2), total);
        assert!(cache.get("a", 0, 1).is_some(), "touch chunk 0");
        cache.put(
            "a",
            CHUNK_ROWS * 2,
            rows(CHUNK_ROWS * 2..CHUNK_ROWS * 3),
            total,
        );
        assert!(cache.get("a", 0, 1).is_some(), "recently used, kept");
        assert!(
            cache.get("a", CHUNK_ROWS, 1).is_none(),
            "least recently used, evicted"
        );
        cache.forget("a");
        assert!(cache.get("a", 0, 1).is_none());
        assert_eq!(cache.bytes, 0);
    }
}
