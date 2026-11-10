//! Spill-to-disk machinery for the streaming query pipeline: the manifest
//! types that travel on [`QueryEvent`], the runtime-tunable row threshold and
//! byte budget, the spill directory + orphan sweep, and the per-result-set
//! [`SpillTracker`] the drain loop feeds.

#[cfg(not(target_arch = "wasm32"))]
use std::collections::HashMap;

#[cfg(not(target_arch = "wasm32"))]
use super::QueryEvent;
#[cfg(not(target_arch = "wasm32"))]
use super::large_cells::{CellLimits, truncate_rows};
use sqlly_core::CellValue;
#[cfg(not(target_arch = "wasm32"))]
use sqlly_core::ResultColumn;
#[cfg(not(target_arch = "wasm32"))]
use tokio::sync::mpsc;

/// One result set persisted to a spill file by the executor when the row
/// count exceeds `SPILL_ROW_THRESHOLD`. Mirrors the gpui app's
/// `SpillResultSet` but lives in the services crate so it can travel on
/// [`QueryEvent`].
#[derive(Debug, Clone, PartialEq)]
pub struct SpillResultSetData {
    pub result_set_index: usize,
    pub columns: Vec<sqlly_core::ResultColumn>,
    pub row_count: usize,
    pub spill_file: String,
}

/// Manifest describing all spill files produced by a single query execution.
/// Emitted as [`QueryEvent::SpillManifest`](super::QueryEvent::SpillManifest)
/// when any result set exceeds the spill threshold.
#[derive(Debug, Clone, PartialEq)]
pub struct SpillManifestData {
    pub result_sets: Vec<SpillResultSetData>,
    pub messages: Vec<String>,
    pub error: Option<String>,
    pub elapsed_ms: Option<i64>,
}

/// Reads a spilled result set's COMPLETE rows back from its file and deletes
/// the file plus its `.idx` sidecar. For collect-until-Completed consumers
/// (metadata queries, value lookups, showplan capture, script generation)
/// that materialize whole result sets and never page from disk: the on-disk
/// set supersedes whatever `Rows` prefix was forwarded before payload
/// suppression, so ignoring the manifest would silently lose the withheld
/// rows AND leak the files. Returns `None` when the file cannot be read —
/// the caller keeps its forwarded prefix; the files are removed either way.
/// Wasm builds never spill — results stay in memory under the byte budget —
/// so a manifest can never reference a readable file. Present so portable
/// collect-until-Completed consumers compile unchanged and keep their
/// forwarded prefix.
#[must_use]
#[cfg(target_arch = "wasm32")]
pub fn take_spilled_rows(set: &SpillResultSetData) -> Option<Vec<Vec<CellValue>>> {
    let _ = set;
    None
}

#[must_use]
#[cfg(not(target_arch = "wasm32"))]
pub fn take_spilled_rows(set: &SpillResultSetData) -> Option<Vec<Vec<CellValue>>> {
    let rows = sqlly_core::SpillReader::open(&set.spill_file)
        .and_then(|mut reader| {
            // Clamp to the manifest count: a set frozen by a mid-write
            // failure can leave the on-disk index ahead of its safely
            // flushed prefix; `set.row_count` is the authoritative bound.
            let count = reader.row_count().min(set.row_count);
            reader.page(0, count)
        })
        .ok();
    // The reader is dropped before removal — required on Windows.
    let _ = std::fs::remove_file(format!("{}.idx", set.spill_file));
    let _ = std::fs::remove_file(&set.spill_file);
    rows
}

/// Default row count at which the executor stops streaming
/// [`QueryEvent::Rows`](super::QueryEvent::Rows) and emits a
/// [`QueryEvent::SpillManifest`](super::QueryEvent::SpillManifest) instead.
/// This keeps resident memory bounded for large result sets. Configurable at
/// runtime via [`set_spill_row_threshold`].
pub const SPILL_ROW_THRESHOLD: usize = 50_000;

/// Runtime override for the spill threshold. `0` means "use the default".
static SPILL_ROW_THRESHOLD_OVERRIDE: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Sets the spill-row threshold for subsequently executed queries. The host
/// app calls this at startup and whenever the user edits the Performance
/// preferences. A value of `0` restores [`SPILL_ROW_THRESHOLD`].
pub fn set_spill_row_threshold(rows: usize) {
    SPILL_ROW_THRESHOLD_OVERRIDE.store(rows, std::sync::atomic::Ordering::Relaxed);
}

/// The spill-row threshold currently in effect.
#[must_use]
pub fn spill_row_threshold() -> usize {
    match SPILL_ROW_THRESHOLD_OVERRIDE.load(std::sync::atomic::Ordering::Relaxed) {
        0 => SPILL_ROW_THRESHOLD,
        n => n,
    }
}

/// Default cap on the estimated payload bytes forwarded as
/// [`QueryEvent::Rows`](super::QueryEvent::Rows) per result set. Complements
/// the row threshold so wide rows stop consuming UI memory long before 50k of
/// them arrive. Configurable at runtime via
/// [`set_result_memory_budget_bytes`].
pub const RESULT_MEMORY_BUDGET_BYTES: usize = 64 * 1024 * 1024;

/// Runtime override for the memory budget. `0` means "use the default".
static RESULT_MEMORY_BUDGET_OVERRIDE: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Sets the per-result-set forwarded-payload byte budget for subsequently
/// executed queries. The host app calls this at startup and whenever the user
/// edits the Performance preferences. A value of `0` restores
/// [`RESULT_MEMORY_BUDGET_BYTES`].
pub fn set_result_memory_budget_bytes(bytes: usize) {
    RESULT_MEMORY_BUDGET_OVERRIDE.store(bytes, std::sync::atomic::Ordering::Relaxed);
}

/// The forwarded-payload byte budget currently in effect.
#[must_use]
pub fn result_memory_budget_bytes() -> usize {
    match RESULT_MEMORY_BUDGET_OVERRIDE.load(std::sync::atomic::Ordering::Relaxed) {
        0 => RESULT_MEMORY_BUDGET_BYTES,
        n => n,
    }
}

/// Cheap upper-pressure estimate of one row's forwarded payload size.
/// Variable-length cells count their byte length plus a small fixed overhead;
/// scalar cells a flat 16 bytes. This is a budget heuristic, not RSS
/// accounting — no re-encoding, no allocation.
fn estimate_row_bytes(row: &[CellValue]) -> usize {
    row.iter()
        .map(|cell| match cell {
            CellValue::String(s) => s.len() + 24,
            CellValue::Bytes(b) => b.len() + 24,
            _ => 16,
        })
        .sum()
}

/// Returns the directory where spill files are written:
/// `<data_dir>/SQLLY/spill/` — `~/Library/Application Support/SQLLY/spill/` on
/// macOS, `~/.local/share/SQLLY/spill/` on Linux, `%APPDATA%/SQLLY/spill/` on
/// Windows — falling back to the system temp dir if the data dir is unavailable.
#[cfg(not(target_arch = "wasm32"))]
fn spill_dir() -> std::path::PathBuf {
    let dir = dirs::data_dir().unwrap_or_else(std::env::temp_dir);
    let spill = dir.join("SQLLY").join("spill");
    let _ = std::fs::create_dir_all(&spill);
    // Spilled rows are query results; keep the directory owner-only so a
    // permissive umask can never expose them (files are 0o600 themselves).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&spill, std::fs::Permissions::from_mode(0o700));
    }
    sweep_orphaned_spill_files_once(&spill);
    spill
}

/// How old a spill file must be before the startup sweep reclaims it. Files
/// belonging to the current session are younger by construction; an hour of
/// slack keeps a hypothetical concurrent instance's active files safe.
#[cfg(not(target_arch = "wasm32"))]
const SPILL_ORPHAN_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// Deletes `sqlly-spill-*` files left behind by a crashed or killed session.
/// Normal lifecycles delete spill files deterministically (below-threshold
/// discard, consumption, result close), but a hard crash never reaches those
/// paths — without a sweep, orphaned result data would sit on disk forever.
/// Runs at most once per process, on the first spill-directory access.
#[cfg(not(target_arch = "wasm32"))]
fn sweep_orphaned_spill_files_once(dir: &std::path::Path) {
    static SWEEP: std::sync::Once = std::sync::Once::new();
    SWEEP.call_once(|| sweep_orphaned_spill_files(dir, SPILL_ORPHAN_MAX_AGE));
}

#[cfg(not(target_arch = "wasm32"))]
fn sweep_orphaned_spill_files(dir: &std::path::Path, max_age: std::time::Duration) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with("sqlly-spill-") {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age >= max_age);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Thin wrapper over the canonical [`sqlly_core::SpillWriter`] (typed binary
/// format + `.idx` sidecar) that owns the temp-file path for the manifest.
#[cfg(not(target_arch = "wasm32"))]
struct SpillWriter {
    inner: sqlly_core::SpillWriter,
    path: String,
}

#[cfg(not(target_arch = "wasm32"))]
impl SpillWriter {
    fn new() -> Result<Self, sqlly_core::SpillError> {
        let dir = spill_dir();
        let file_name = format!("sqlly-spill-{}.bin", uuid::Uuid::new_v4());
        let path = dir.join(&file_name);
        let inner = sqlly_core::SpillWriter::create(&path)?;
        Ok(Self {
            inner,
            path: path.to_string_lossy().to_string(),
        })
    }

    fn write_row(&mut self, row: &[CellValue]) -> Result<(), sqlly_core::SpillError> {
        self.inner.append(row)
    }

    /// Flushes the growing file (data before index) so the prefix written so
    /// far is safe for a concurrent reader. See [`sqlly_core::SpillWriter::flush`].
    fn flush(&mut self) -> Result<(), sqlly_core::SpillError> {
        self.inner.flush()
    }

    fn finish(self) -> Result<(String, usize), sqlly_core::SpillError> {
        let count = self.inner.row_count();
        self.inner.finish()?;
        Ok((self.path, count))
    }

    /// Finish the writer and delete its files — used for result sets that
    /// ended below the spill threshold and never need the on-disk copy.
    fn discard(self) {
        if let Ok((path, _)) = self.finish() {
            let _ = std::fs::remove_file(format!("{path}.idx"));
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Tracks spill state per result set during query draining.
///
/// EVERY result set is written to a spill file from row 0 as rows pass
/// through — so a spilled set's file always contains the COMPLETE result and
/// can back a windowed grid on its own. Rows are additionally forwarded as
/// `QueryEvent::Rows` until the set crosses the spill threshold; beyond that
/// the consumer switches to paging from the file. At the end, files for sets
/// that stayed under the threshold are deleted, and a `SpillManifestData`
/// (with FULL per-set row counts) is yielded if any set crossed it.
#[cfg(not(target_arch = "wasm32"))]
pub(super) struct SpillTracker {
    threshold: usize,
    /// Estimated-byte cap on forwarded `Rows` payloads per result set; see
    /// [`estimate_row_bytes`].
    byte_budget: usize,
    limits: CellLimits,
    /// Mirror rows to spill files and withhold past the threshold. Off for
    /// streaming consumers (see [`Self::streaming_full_values`]).
    spill_files: bool,
    pub(super) columns_by_index: HashMap<usize, Vec<ResultColumn>>,
    row_counts: HashMap<usize, usize>,
    /// Estimated bytes forwarded as `Rows` payloads, per result set.
    forwarded_bytes: HashMap<usize, usize>,
    /// Sets that have had at least one row withheld from forwarding (by row
    /// threshold or byte budget). A suppressed set MUST reach the manifest —
    /// even if its final total lands under the row threshold — because the
    /// spill file holds rows the consumer never received.
    suppressed: std::collections::HashSet<usize>,
    /// Last `RowsProgress` send per result set. Absent until the first
    /// post-suppression progress, which is therefore never throttled.
    last_progress_at: HashMap<usize, std::time::Instant>,
    /// Minimum spacing between `RowsProgress` events per result set. Field
    /// (not const) so tests can shrink it.
    progress_interval: std::time::Duration,
    /// Complete-file writer per result set. `None` when creating the file
    /// failed — that set falls back to forwarding every row.
    writers: HashMap<usize, Option<SpillWriter>>,
    /// Last `rows_available` advertised on a
    /// [`QueryEvent::SpillWindow`](super::QueryEvent::SpillWindow) per
    /// result set. Once a path is advertised the consumer may be windowed
    /// over the file, so NO failure path may unlink it — deletion ownership
    /// must still transfer via the manifest, as the event contract promises.
    advertised: HashMap<usize, usize>,
    /// Sets whose writer failed AFTER their file was advertised: `(path,
    /// flushed_rows)` of the kept prefix, published in the manifest. Rows
    /// arriving after the failure are dropped (counted only) — they can be
    /// neither spilled nor contiguously forwarded, because the rows between
    /// the flushed prefix and the failure exist nowhere.
    frozen: HashMap<usize, (String, usize)>,
    pub(super) messages: Vec<String>,
    pub(super) error: Option<String>,
    pub(super) elapsed_ms: Option<i64>,
}

#[cfg(not(target_arch = "wasm32"))]
impl SpillTracker {
    pub(super) fn new(threshold: usize, byte_budget: usize) -> Self {
        Self {
            threshold: threshold.max(1),
            byte_budget: byte_budget.max(1),
            limits: CellLimits::default(),
            spill_files: true,
            columns_by_index: HashMap::new(),
            row_counts: HashMap::new(),
            forwarded_bytes: HashMap::new(),
            suppressed: std::collections::HashSet::new(),
            last_progress_at: HashMap::new(),
            progress_interval: std::time::Duration::from_millis(50),
            writers: HashMap::new(),
            advertised: HashMap::new(),
            frozen: HashMap::new(),
            messages: Vec::new(),
            error: None,
            elapsed_ms: None,
        }
    }

    /// Replace the per-cell preview limits (see [`CellLimits::full_values`]).
    pub(super) fn with_limits(mut self, limits: CellLimits) -> Self {
        self.limits = limits;
        self
    }

    /// The tracker for a consumer that writes rows out as they arrive
    /// (exports, dumps, transfers, the CLI): every cell whole, every row
    /// forwarded as `Rows`, no spill file. Memory stays bounded by the
    /// bounded event channel — the drain waits for the consumer before it
    /// pulls more rows — so nothing needs withholding, and no row can be
    /// stranded in a spill file a consumer does not read.
    pub(super) fn streaming_full_values() -> Self {
        let mut tracker = Self::new(usize::MAX, usize::MAX).with_limits(CellLimits::full_values());
        tracker.spill_files = false;
        tracker
    }

    pub(super) fn record_columns(&mut self, index: usize, columns: Vec<ResultColumn>) {
        self.columns_by_index.insert(index, columns);
    }

    pub(super) async fn handle_rows(
        &mut self,
        index: usize,
        mut rows: Vec<Vec<CellValue>>,
        event_tx: &mpsc::Sender<QueryEvent>,
    ) -> Result<(), ()> {
        let already = *self.row_counts.get(&index).unwrap_or(&0);
        let truncated = truncate_rows(&mut rows, already, &self.limits);

        // Mirror every row to the set's complete spill file (a streaming
        // tracker has none, so it forwards everything below).
        let spill_files = self.spill_files;
        let writer = self
            .writers
            .entry(index)
            .or_insert_with(|| spill_files.then(|| SpillWriter::new().ok()).flatten());
        let mut file_ok = false;
        if let Some(writer) = writer {
            file_ok = true;
            for row in &rows {
                if writer.write_row(row).is_err() {
                    file_ok = false;
                    break;
                }
            }
            if !file_ok {
                self.handle_writer_failure(index);
            }
        }

        let count = self.row_counts.entry(index).or_insert(0);
        *count += rows.len();
        let total = *count;

        // A frozen set's advertised file is sealed at its flushed prefix;
        // rows past the gap are dropped (counted only) — forwarding them
        // would splice discontinuous data onto the preview, and a windowed
        // consumer could not render them anyway.
        if self.frozen.contains_key(&index) {
            return Self::emit_truncated_cells(index, truncated, event_tx).await;
        }

        // No spill file: forward EVERYTHING so no data is lost.
        if !file_ok {
            if event_tx
                .send(QueryEvent::Rows { index, rows })
                .await
                .is_err()
            {
                return Err(());
            }
            return Self::emit_truncated_cells(index, truncated, event_tx).await;
        }

        // Forward only the prefix that fits both the remaining row allowance
        // and the remaining byte budget; the spill file holds the rest.
        // Suppression is terminal: resuming after a withheld row would splice
        // later rows onto the forwarded prefix and present an interior gap as
        // contiguous data.
        let row_allowance = if self.suppressed.contains(&index) {
            0
        } else {
            self.threshold.saturating_sub(already)
        };
        let bytes_used = self.forwarded_bytes.entry(index).or_insert(0);
        let mut fit = 0usize;
        for row in rows.iter().take(row_allowance) {
            let est = estimate_row_bytes(row);
            if *bytes_used + est > self.byte_budget {
                break;
            }
            *bytes_used += est;
            fit += 1;
        }
        let withheld = fit < rows.len();

        if fit > 0 {
            let mut payload = rows;
            payload.truncate(fit);
            if event_tx
                .send(QueryEvent::Rows {
                    index,
                    rows: payload,
                })
                .await
                .is_err()
            {
                return Err(());
            }
        }

        if withheld {
            self.suppressed.insert(index);
        }
        if self.suppressed.contains(&index) {
            self.maybe_send_progress(index, total, event_tx).await?;
        }
        Self::emit_truncated_cells(index, truncated, event_tx).await
    }

    async fn emit_truncated_cells(
        index: usize,
        cells: Vec<super::large_cells::TruncatedCell>,
        event_tx: &mpsc::Sender<QueryEvent>,
    ) -> Result<(), ()> {
        if cells.is_empty() {
            return Ok(());
        }
        event_tx
            .send(QueryEvent::TruncatedCells { index, cells })
            .await
            .map_err(|_| ())
    }

    /// Disk trouble mid-write: retire the set's writer. An unadvertised file
    /// is discarded and the set falls back to forwarding every row. An
    /// ADVERTISED file may be actively paged by the consumer (a provisional
    /// backing windows the grid over it), so it is never unlinked here:
    /// the set is frozen at its flushed prefix, which the manifest publishes
    /// so the consumer's backing finalizes and deletion ownership transfers
    /// as the `SpillWindow` contract promises.
    fn handle_writer_failure(&mut self, index: usize) {
        if let Some(broken) = self.writers.insert(index, None).flatten() {
            match self.advertised.get(&index).copied() {
                Some(flushed) => {
                    self.messages.push(format!(
                        "Result set {}: spill write failed; rows truncated to the {} already flushed to disk",
                        index + 1,
                        flushed
                    ));
                    // Dropping closes the handles; the flushed prefix stays
                    // on disk for the manifest.
                    self.frozen.insert(index, (broken.path.clone(), flushed));
                }
                None => broken.discard(),
            }
        }
    }

    /// Emits live progress for a suppressed set — immediately on the first
    /// call, then at most once per `progress_interval`. The set's growing
    /// spill file is flushed at the same cadence (the throttle bounds flush
    /// cost), and on success a
    /// [`QueryEvent::SpillWindow`](super::QueryEvent::SpillWindow) advertises
    /// the readable prefix; the count rides on it. A failed flush falls back
    /// to count-only [`QueryEvent::RowsProgress`](super::QueryEvent::RowsProgress)
    /// WITHOUT advertising the file (it may still finish cleanly later, so
    /// the writer is kept). The final authoritative count travels on the
    /// spill manifest, so a missed trailing tick here never loses the total.
    async fn maybe_send_progress(
        &mut self,
        index: usize,
        rows_seen: usize,
        event_tx: &mpsc::Sender<QueryEvent>,
    ) -> Result<(), ()> {
        let now = std::time::Instant::now();
        let due = match self.last_progress_at.get(&index) {
            None => true,
            Some(last) => now.duration_since(*last) >= self.progress_interval,
        };
        if !due {
            return Ok(());
        }
        self.last_progress_at.insert(index, now);
        // Every row of a suppressed set has already been mirrored to the
        // writer, so post-flush the durable prefix is exactly `rows_seen`.
        let mut spill_file = None;
        if let Some(writer) = self.writers.get_mut(&index).and_then(|w| w.as_mut())
            && writer.flush().is_ok()
        {
            spill_file = Some(writer.path.clone());
        }
        let event = match spill_file {
            Some(spill_file) => {
                // Advertised = the consumer may now be windowed over the
                // file; from here on no failure path may unlink it (see
                // `handle_writer_failure` / `into_manifest`).
                self.advertised.insert(index, rows_seen);
                QueryEvent::SpillWindow {
                    index,
                    spill_file,
                    rows_available: rows_seen,
                }
            }
            None => QueryEvent::RowsProgress { index, rows_seen },
        };
        event_tx.send(event).await.map_err(|_| ())
    }

    pub(super) fn into_manifest(mut self) -> Option<SpillManifestData> {
        let mut result_sets: Vec<SpillResultSetData> = Vec::new();
        // Frozen sets: their writer is gone but the flushed prefix is on
        // disk and the consumer may hold a provisional backing over it —
        // the manifest entry finalizes that backing and transfers deletion
        // ownership as usual.
        for (index, (path, flushed)) in std::mem::take(&mut self.frozen) {
            result_sets.push(SpillResultSetData {
                result_set_index: index,
                columns: self
                    .columns_by_index
                    .get(&index)
                    .cloned()
                    .unwrap_or_default(),
                row_count: flushed,
                spill_file: path,
            });
        }
        for (index, writer) in self.writers.drain() {
            let Some(writer) = writer else { continue };
            let total = self.row_counts.get(&index).copied().unwrap_or(0);
            // Keep the file whenever forwarding was suppressed — a set capped
            // by the byte budget can finish under the row threshold, and its
            // withheld rows exist only in the spill file.
            if total <= self.threshold && !self.suppressed.contains(&index) {
                writer.discard();
                continue;
            }
            // Columns are recorded before any rows; an advertised set keeps
            // its file even in the impossible columns-missing case (never
            // unlink a live-borrowed path).
            let columns = match self.columns_by_index.get(&index).cloned() {
                Some(columns) => columns,
                None if self.advertised.contains_key(&index) => Vec::new(),
                None => {
                    writer.discard();
                    continue;
                }
            };
            let advertised_path = writer.path.clone();
            match writer.finish() {
                Ok((path, row_count)) => result_sets.push(SpillResultSetData {
                    result_set_index: index,
                    columns,
                    row_count,
                    spill_file: path,
                }),
                Err(_) => {
                    // finish() failed at completion. For an advertised file
                    // the flushed prefix is still on disk and live-borrowed —
                    // publish it at the last advertised count rather than
                    // silently dropping the set (which would strand the
                    // consumer's backing AND leak the file).
                    if let Some(flushed) = self.advertised.get(&index).copied() {
                        result_sets.push(SpillResultSetData {
                            result_set_index: index,
                            columns,
                            row_count: flushed,
                            spill_file: advertised_path,
                        });
                    }
                }
            }
        }
        if result_sets.is_empty() {
            return None;
        }
        result_sets.sort_by_key(|rs| rs.result_set_index);
        Some(SpillManifestData {
            result_sets,
            messages: self.messages,
            error: self.error,
            elapsed_ms: self.elapsed_ms,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orphan_sweep_reclaims_stale_spill_files_only() {
        let dir = std::env::temp_dir().join(format!("sqlly-sweep-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        let stale = dir.join("sqlly-spill-stale.bin");
        let stale_idx = dir.join("sqlly-spill-stale.bin.idx");
        let fresh = dir.join("sqlly-spill-fresh.bin");
        let unrelated = dir.join("not-a-spill.bin");
        for p in [&stale, &stale_idx, &fresh, &unrelated] {
            std::fs::write(p, b"x").unwrap();
        }

        // Zero max-age makes every spill file "stale" without mtime games;
        // the unrelated file must survive regardless.
        sweep_orphaned_spill_files(&dir, std::time::Duration::ZERO);
        assert!(!stale.exists());
        assert!(!stale_idx.exists());
        assert!(!fresh.exists());
        assert!(unrelated.exists());

        // A long max-age keeps just-written files.
        std::fs::write(&fresh, b"x").unwrap();
        sweep_orphaned_spill_files(&dir, std::time::Duration::from_secs(3600));
        assert!(fresh.exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The estimator is a pure budget heuristic: variable-length cells count
    /// content bytes + 24 overhead, everything else a flat 16.
    #[test]
    fn estimate_row_bytes_counts_variable_and_scalar_cells() {
        assert_eq!(estimate_row_bytes(&[]), 0);
        assert_eq!(estimate_row_bytes(&[CellValue::String("abcde".into())]), 29);
        assert_eq!(estimate_row_bytes(&[CellValue::String(String::new())]), 24);
        assert_eq!(estimate_row_bytes(&[CellValue::Bytes(vec![0u8; 10])]), 34);
        assert_eq!(estimate_row_bytes(&[CellValue::I64(7)]), 16);
        assert_eq!(estimate_row_bytes(&[CellValue::Null]), 16);
        // Mixed row: sums per-cell estimates.
        assert_eq!(
            estimate_row_bytes(&[
                CellValue::String("ab".into()),
                CellValue::I64(1),
                CellValue::Bytes(vec![0u8; 4]),
            ]),
            (2 + 24) + 16 + (4 + 24)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spill_tracker_streams_small_result_set() {
        let (event_tx, mut event_rx) = mpsc::channel::<QueryEvent>(32);
        let mut tracker = SpillTracker::new(SPILL_ROW_THRESHOLD, RESULT_MEMORY_BUDGET_BYTES);
        tracker.record_columns(
            0,
            vec![ResultColumn {
                name: "id".into(),
                data_type: "int".into(),
                nullable: false,
            }],
        );
        let rows = vec![vec![CellValue::I64(1)]];
        tracker.handle_rows(0, rows, &event_tx).await.unwrap();
        drop(event_tx);
        let mut events = Vec::new();
        while let Some(ev) = event_rx.recv().await {
            events.push(ev);
        }
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], QueryEvent::Rows { .. }));
        assert!(tracker.into_manifest().is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn handle_rows_truncates_and_emits_sidecar() {
        let (event_tx, mut event_rx) = mpsc::channel::<QueryEvent>(32);
        let mut tracker = SpillTracker::new(SPILL_ROW_THRESHOLD, RESULT_MEMORY_BUDGET_BYTES);
        tracker.record_columns(
            0,
            vec![ResultColumn {
                name: "body".into(),
                data_type: "text".into(),
                nullable: true,
            }],
        );
        let rows = vec![vec![CellValue::String("x".repeat(20 * 1024))]];
        tracker.handle_rows(0, rows, &event_tx).await.unwrap();
        drop(event_tx);
        let mut events = Vec::new();
        while let Some(ev) = event_rx.recv().await {
            events.push(ev);
        }
        assert_eq!(events.len(), 2);
        let QueryEvent::Rows { rows, .. } = &events[0] else {
            panic!("expected Rows");
        };
        let CellValue::String(s) = &rows[0][0] else {
            panic!("expected string cell");
        };
        assert_eq!(s.len(), 8192);
        let QueryEvent::TruncatedCells { cells, .. } = &events[1] else {
            panic!("expected TruncatedCells");
        };
        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0].row, 0);
        assert_eq!(cells[0].column, 0);
        assert_eq!(cells[0].total_len, (20 * 1024) as u64);
        assert_eq!(cells[0].kind, crate::query::TruncatedKind::Text);
    }

    /// Export consumers ask for full values: nothing is cut — neither the
    /// forwarded prefix nor the rows withheld to the spill file — and no
    /// `TruncatedCells` sidecar is sent.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn full_values_tracker_forwards_and_spills_whole_cells() {
        let (event_tx, mut event_rx) = mpsc::channel::<QueryEvent>(32);
        let mut tracker =
            SpillTracker::new(1, RESULT_MEMORY_BUDGET_BYTES).with_limits(CellLimits::full_values());
        tracker.record_columns(
            0,
            vec![ResultColumn {
                name: "body".into(),
                data_type: "text".into(),
                nullable: true,
            }],
        );
        let big = |c: char| c.to_string().repeat(20 * 1024);
        let rows = vec![
            vec![CellValue::String(big('a'))],
            vec![CellValue::String(big('b'))],
        ];
        tracker.handle_rows(0, rows, &event_tx).await.unwrap();
        drop(event_tx);
        let mut forwarded = Vec::new();
        while let Some(ev) = event_rx.recv().await {
            match ev {
                QueryEvent::Rows { rows, .. } => forwarded.extend(rows),
                QueryEvent::TruncatedCells { .. } => panic!("nothing may be cut"),
                _ => {}
            }
        }
        assert_eq!(forwarded, vec![vec![CellValue::String(big('a'))]]);
        let manifest = tracker.into_manifest().expect("row 2 was withheld");
        let spilled = take_spilled_rows(&manifest.result_sets[0]).expect("spill readable");
        assert_eq!(spilled[1], vec![CellValue::String(big('b'))]);
    }

    /// The streaming full-values tracker forwards every row whole and
    /// writes no spill file, so a streaming consumer (dump, transfer) can
    /// never lose a withheld tail.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn streaming_tracker_forwards_every_row_whole_without_spilling() {
        let (event_tx, mut event_rx) = mpsc::channel::<QueryEvent>(8);
        let mut tracker = SpillTracker::streaming_full_values();
        tracker.record_columns(
            0,
            vec![ResultColumn {
                name: "body".into(),
                data_type: "text".into(),
                nullable: true,
            }],
        );
        let reader = tokio::spawn(async move {
            let mut rows = Vec::new();
            while let Some(ev) = event_rx.recv().await {
                match ev {
                    QueryEvent::Rows { rows: batch, .. } => rows.extend(batch),
                    QueryEvent::TruncatedCells { .. } => panic!("nothing may be cut"),
                    _ => {}
                }
            }
            rows
        });
        let big = "z".repeat(20 * 1024);
        for _ in 0..60 {
            let batch = vec![vec![CellValue::String(big.clone())]; 50];
            tracker.handle_rows(0, batch, &event_tx).await.unwrap();
        }
        drop(event_tx);
        let rows = reader.await.unwrap();
        assert_eq!(rows.len(), 3_000, "every row is forwarded");
        assert!(rows.iter().all(|r| r[0] == CellValue::String(big.clone())));
        assert!(
            tracker.into_manifest().is_none(),
            "no spill file, no manifest"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn handle_rows_sidecar_rows_are_absolute() {
        let (event_tx, mut event_rx) = mpsc::channel::<QueryEvent>(64);
        let mut tracker = SpillTracker::new(SPILL_ROW_THRESHOLD, RESULT_MEMORY_BUDGET_BYTES);
        tracker.record_columns(
            0,
            vec![ResultColumn {
                name: "body".into(),
                data_type: "text".into(),
                nullable: true,
            }],
        );
        for batch in 0..2 {
            let rows: Vec<Vec<CellValue>> = (0..3)
                .map(|i| {
                    if batch == 1 && i == 1 {
                        vec![CellValue::String("y".repeat(20 * 1024))]
                    } else {
                        vec![CellValue::String("ok".into())]
                    }
                })
                .collect();
            tracker.handle_rows(0, rows, &event_tx).await.unwrap();
        }
        drop(event_tx);
        let mut truncated = Vec::new();
        while let Some(ev) = event_rx.recv().await {
            if let QueryEvent::TruncatedCells { cells, .. } = ev {
                truncated.extend(cells);
            }
        }
        assert_eq!(truncated.len(), 1);
        assert_eq!(truncated[0].row, 4);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn handle_rows_no_sidecar_when_nothing_cut() {
        let (event_tx, mut event_rx) = mpsc::channel::<QueryEvent>(32);
        let mut tracker = SpillTracker::new(SPILL_ROW_THRESHOLD, RESULT_MEMORY_BUDGET_BYTES);
        tracker.record_columns(
            0,
            vec![ResultColumn {
                name: "id".into(),
                data_type: "int".into(),
                nullable: false,
            }],
        );
        let rows = vec![vec![CellValue::String("short".into())]];
        tracker.handle_rows(0, rows, &event_tx).await.unwrap();
        drop(event_tx);
        let mut events = Vec::new();
        while let Some(ev) = event_rx.recv().await {
            events.push(ev);
        }
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], QueryEvent::Rows { .. }));
    }

    /// A result set that crosses the threshold must (a) stream rows only up
    /// to the threshold, and (b) yield a manifest whose spill file contains
    /// the COMPLETE result set from row 0 with the full row count — the file
    /// alone backs the windowed grid.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spill_tracker_spills_complete_set_past_threshold() {
        let threshold = 10usize;
        let (event_tx, mut event_rx) = mpsc::channel::<QueryEvent>(64);
        let mut tracker = SpillTracker::new(threshold, RESULT_MEMORY_BUDGET_BYTES);
        tracker.record_columns(
            0,
            vec![ResultColumn {
                name: "id".into(),
                data_type: "int".into(),
                nullable: false,
            }],
        );
        // 5 batches of 5 rows = 25 total; only the first 2 batches (10 rows)
        // fit under the threshold.
        for batch in 0..5i64 {
            let rows: Vec<Vec<CellValue>> = (batch * 5..batch * 5 + 5)
                .map(|i| vec![CellValue::I64(i)])
                .collect();
            tracker.handle_rows(0, rows, &event_tx).await.unwrap();
        }
        drop(event_tx);

        let mut streamed = 0usize;
        while let Some(ev) = event_rx.recv().await {
            if let QueryEvent::Rows { rows, .. } = ev {
                streamed += rows.len();
            }
        }
        assert_eq!(streamed, threshold, "streaming stops at the threshold");

        let manifest = tracker.into_manifest().expect("set crossed threshold");
        assert_eq!(manifest.result_sets.len(), 1);
        let rs = &manifest.result_sets[0];
        assert_eq!(rs.row_count, 25, "manifest carries the FULL row count");
        let mut reader = sqlly_core::SpillReader::open(&rs.spill_file).expect("file opens");
        assert_eq!(reader.row_count(), 25, "file holds the complete set");
        assert_eq!(reader.row(0).expect("row 0")[0], CellValue::I64(0));
        assert_eq!(reader.row(24).expect("row 24")[0], CellValue::I64(24));
        std::fs::remove_file(&rs.spill_file).ok();
        std::fs::remove_file(format!("{}.idx", rs.spill_file)).ok();
    }

    /// A result set that stays under the threshold must leave no spill files
    /// behind — the mirror file is deleted when the tracker finishes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spill_tracker_deletes_files_for_small_sets() {
        let (event_tx, mut event_rx) = mpsc::channel::<QueryEvent>(32);
        let mut tracker = SpillTracker::new(100, RESULT_MEMORY_BUDGET_BYTES);
        tracker.record_columns(
            0,
            vec![ResultColumn {
                name: "id".into(),
                data_type: "int".into(),
                nullable: false,
            }],
        );
        tracker
            .handle_rows(0, vec![vec![CellValue::I64(1)]], &event_tx)
            .await
            .unwrap();
        let path = tracker
            .writers
            .get(&0)
            .and_then(|w| w.as_ref())
            .map(|w| w.path.clone())
            .expect("mirror file exists during the stream");
        assert!(std::path::Path::new(&path).exists());
        drop(event_tx);
        while event_rx.recv().await.is_some() {}

        assert!(tracker.into_manifest().is_none());
        assert!(
            !std::path::Path::new(&path).exists(),
            "under-threshold spill file must be deleted"
        );
        assert!(
            !std::path::Path::new(&format!("{path}.idx")).exists(),
            "sidecar must be deleted too"
        );
    }

    fn int_column() -> Vec<ResultColumn> {
        vec![ResultColumn {
            name: "id".into(),
            data_type: "int".into(),
            nullable: false,
        }]
    }

    fn discard_manifest_files(manifest: &SpillManifestData) {
        for rs in &manifest.result_sets {
            std::fs::remove_file(&rs.spill_file).ok();
            std::fs::remove_file(format!("{}.idx", rs.spill_file)).ok();
        }
    }

    /// Drains all events and returns (forwarded row total, progress counts in
    /// arrival order). Suppressed-set counts normally travel on `SpillWindow`
    /// (with `RowsProgress` as the flush-failure fallback); both feed the
    /// same progress list. Callers drop the sender first.
    async fn collect_rows_and_progress(
        event_rx: &mut mpsc::Receiver<QueryEvent>,
    ) -> (usize, Vec<usize>) {
        let mut forwarded = 0usize;
        let mut progress = Vec::new();
        while let Some(ev) = event_rx.recv().await {
            match ev {
                QueryEvent::Rows { rows, .. } => forwarded += rows.len(),
                QueryEvent::RowsProgress { rows_seen, .. } => progress.push(rows_seen),
                QueryEvent::SpillWindow { rows_available, .. } => progress.push(rows_available),
                _ => {}
            }
        }
        (forwarded, progress)
    }

    /// Once the row threshold is crossed, count-only `RowsProgress` events
    /// keep flowing with increasing totals while row payloads stop.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spill_tracker_emits_progress_after_threshold() {
        let (event_tx, mut event_rx) = mpsc::channel::<QueryEvent>(64);
        let mut tracker = SpillTracker::new(10, RESULT_MEMORY_BUDGET_BYTES);
        tracker.progress_interval = std::time::Duration::ZERO;
        tracker.record_columns(0, int_column());
        for batch in 0..5i64 {
            let rows: Vec<Vec<CellValue>> = (batch * 5..batch * 5 + 5)
                .map(|i| vec![CellValue::I64(i)])
                .collect();
            tracker.handle_rows(0, rows, &event_tx).await.unwrap();
        }
        drop(event_tx);

        let (forwarded, progress) = collect_rows_and_progress(&mut event_rx).await;
        assert_eq!(forwarded, 10, "payloads stop at the threshold");
        assert_eq!(
            progress,
            vec![15, 20, 25],
            "progress continues counting past the threshold"
        );

        let manifest = tracker.into_manifest().expect("set crossed threshold");
        discard_manifest_files(&manifest);
    }

    /// Sets that never cross either limit emit no `RowsProgress` at all.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spill_tracker_no_progress_for_small_sets() {
        let (event_tx, mut event_rx) = mpsc::channel::<QueryEvent>(32);
        let mut tracker = SpillTracker::new(100, RESULT_MEMORY_BUDGET_BYTES);
        tracker.progress_interval = std::time::Duration::ZERO;
        tracker.record_columns(0, int_column());
        let rows: Vec<Vec<CellValue>> = (0..5i64).map(|i| vec![CellValue::I64(i)]).collect();
        tracker.handle_rows(0, rows, &event_tx).await.unwrap();
        drop(event_tx);

        let (forwarded, progress) = collect_rows_and_progress(&mut event_rx).await;
        assert_eq!(forwarded, 5);
        assert!(progress.is_empty(), "under-limit sets emit no progress");
        assert!(tracker.into_manifest().is_none());
    }

    /// A batch that crosses the threshold is split: only the prefix that
    /// still fits is forwarded, so exactly `threshold` payload rows arrive.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spill_tracker_splits_threshold_crossing_batch() {
        let threshold = 100usize;
        let (event_tx, mut event_rx) = mpsc::channel::<QueryEvent>(64);
        let mut tracker = SpillTracker::new(threshold, RESULT_MEMORY_BUDGET_BYTES);
        tracker.progress_interval = std::time::Duration::ZERO;
        tracker.record_columns(0, int_column());
        // 4 batches of 30 = 120 rows; the fourth crosses at row 100.
        for batch in 0..4i64 {
            let rows: Vec<Vec<CellValue>> = (batch * 30..batch * 30 + 30)
                .map(|i| vec![CellValue::I64(i)])
                .collect();
            tracker.handle_rows(0, rows, &event_tx).await.unwrap();
        }
        drop(event_tx);

        let (forwarded, progress) = collect_rows_and_progress(&mut event_rx).await;
        assert_eq!(
            forwarded, threshold,
            "crossing batch forwards only its prefix"
        );
        assert_eq!(
            progress,
            vec![120],
            "full total still reported via progress"
        );

        let manifest = tracker.into_manifest().expect("set crossed threshold");
        assert_eq!(manifest.result_sets[0].row_count, 120);
        discard_manifest_files(&manifest);
    }

    /// Wide rows exhaust the byte budget long before the row threshold;
    /// payloads stop early but the count keeps flowing via progress.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spill_tracker_byte_budget_suppresses_wide_rows() {
        // Each row estimates 100 (string) + 24 (overhead) = 124 bytes, so a
        // 300-byte budget fits exactly 2 rows.
        let wide = || vec![CellValue::String("x".repeat(100))];
        let (event_tx, mut event_rx) = mpsc::channel::<QueryEvent>(64);
        let mut tracker = SpillTracker::new(1000, 300);
        tracker.progress_interval = std::time::Duration::ZERO;
        tracker.record_columns(0, int_column());
        tracker
            .handle_rows(0, (0..5).map(|_| wide()).collect(), &event_tx)
            .await
            .unwrap();
        tracker
            .handle_rows(0, (0..5).map(|_| wide()).collect(), &event_tx)
            .await
            .unwrap();
        drop(event_tx);

        let (forwarded, progress) = collect_rows_and_progress(&mut event_rx).await;
        assert_eq!(
            forwarded, 2,
            "byte budget caps payloads before the row threshold"
        );
        assert_eq!(progress, vec![5, 10], "count continues past the byte cap");

        let manifest = tracker.into_manifest().expect("suppressed set must spill");
        discard_manifest_files(&manifest);
    }

    /// Once the byte budget withholds a row, forwarding stops for good —
    /// narrower rows in later batches must not resume and splice an interior
    /// gap into the forwarded prefix.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spill_tracker_byte_suppression_is_sticky() {
        // narrow = 10 + 24 = 34 bytes; wide = 476 + 24 = 500 bytes. The wide
        // row breaks the 300-byte budget while plenty of budget remains for
        // later narrow rows — which must stay suppressed anyway.
        let narrow = || vec![CellValue::String("x".repeat(10))];
        let wide = || vec![CellValue::String("x".repeat(476))];
        let (event_tx, mut event_rx) = mpsc::channel::<QueryEvent>(64);
        let mut tracker = SpillTracker::new(1000, 300);
        tracker.progress_interval = std::time::Duration::ZERO;
        tracker.record_columns(0, int_column());
        tracker
            .handle_rows(0, vec![narrow(), wide(), narrow()], &event_tx)
            .await
            .unwrap();
        tracker
            .handle_rows(0, vec![narrow(), narrow()], &event_tx)
            .await
            .unwrap();
        drop(event_tx);

        let (forwarded, progress) = collect_rows_and_progress(&mut event_rx).await;
        assert_eq!(forwarded, 1, "no payload resumes after a withheld row");
        assert_eq!(progress, vec![3, 5], "count still flows via progress");

        let manifest = tracker.into_manifest().expect("suppressed set must spill");
        assert_eq!(manifest.result_sets[0].row_count, 5);
        discard_manifest_files(&manifest);
    }

    /// `take_spilled_rows` recovers the COMPLETE on-disk set for a
    /// collect-until-Completed consumer and deletes the spill files.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn take_spilled_rows_returns_complete_set_and_deletes_files() {
        let wide = || vec![CellValue::String("x".repeat(100))];
        let (event_tx, mut event_rx) = mpsc::channel::<QueryEvent>(64);
        let mut tracker = SpillTracker::new(1000, 300);
        tracker.record_columns(0, int_column());
        tracker
            .handle_rows(0, (0..5).map(|_| wide()).collect(), &event_tx)
            .await
            .unwrap();
        drop(event_tx);
        while event_rx.recv().await.is_some() {}

        let manifest = tracker.into_manifest().expect("suppressed set must spill");
        let set = &manifest.result_sets[0];
        let path = set.spill_file.clone();
        let rows = take_spilled_rows(set).expect("file reads back");
        assert_eq!(rows.len(), 5, "withheld rows are recovered");
        assert!(
            !std::path::Path::new(&path).exists(),
            "spill file must be deleted"
        );
        assert!(
            !std::path::Path::new(&format!("{path}.idx")).exists(),
            "sidecar must be deleted"
        );
    }

    /// A set suppressed by the byte budget that finishes UNDER the row
    /// threshold must still reach the manifest with its full count — its
    /// withheld rows exist only in the spill file.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spill_tracker_byte_suppressed_set_under_threshold_reaches_manifest() {
        let wide = || vec![CellValue::String("x".repeat(100))];
        let (event_tx, mut event_rx) = mpsc::channel::<QueryEvent>(64);
        let mut tracker = SpillTracker::new(1000, 300);
        tracker.record_columns(0, int_column());
        tracker
            .handle_rows(0, (0..5).map(|_| wide()).collect(), &event_tx)
            .await
            .unwrap();
        drop(event_tx);
        while event_rx.recv().await.is_some() {}

        let manifest = tracker
            .into_manifest()
            .expect("byte-suppressed set must not be discarded under the row threshold");
        assert_eq!(manifest.result_sets.len(), 1);
        let rs = &manifest.result_sets[0];
        assert_eq!(rs.row_count, 5, "manifest carries the FULL row count");
        let mut reader = sqlly_core::SpillReader::open(&rs.spill_file).expect("file opens");
        assert_eq!(reader.row_count(), 5, "file holds the complete set");
        assert!(reader.row(4).is_ok(), "withheld rows are in the file");
        discard_manifest_files(&manifest);
    }

    /// The first post-suppression progress is immediate; further batches
    /// inside the throttle interval stay silent. (The manifest carries the
    /// authoritative final count, so no trailing tick is required.)
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spill_tracker_throttles_progress_within_interval() {
        let (event_tx, mut event_rx) = mpsc::channel::<QueryEvent>(64);
        let mut tracker = SpillTracker::new(5, RESULT_MEMORY_BUDGET_BYTES);
        // Interval far larger than the test runtime: only the forced first
        // post-suppression tick may appear.
        tracker.progress_interval = std::time::Duration::from_secs(3600);
        tracker.record_columns(0, int_column());
        for batch in 0..4i64 {
            let rows: Vec<Vec<CellValue>> = (batch * 5..batch * 5 + 5)
                .map(|i| vec![CellValue::I64(i)])
                .collect();
            tracker.handle_rows(0, rows, &event_tx).await.unwrap();
        }
        drop(event_tx);

        let (forwarded, progress) = collect_rows_and_progress(&mut event_rx).await;
        assert_eq!(forwarded, 5);
        assert_eq!(
            progress,
            vec![10],
            "first post-suppression tick is forced; the rest are throttled"
        );

        let manifest = tracker.into_manifest().expect("set crossed threshold");
        assert_eq!(manifest.result_sets[0].row_count, 20);
        discard_manifest_files(&manifest);
    }

    /// After suppression, live progress travels as `SpillWindow` whose
    /// advertised prefix is genuinely readable MID-STREAM: a reader opened
    /// on the path between batches pages exactly `rows_available` rows, and
    /// `rows_available` grows monotonically.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spill_window_advertises_readable_prefix_mid_stream() {
        let (event_tx, mut event_rx) = mpsc::channel::<QueryEvent>(64);
        let mut tracker = SpillTracker::new(10, RESULT_MEMORY_BUDGET_BYTES);
        tracker.progress_interval = std::time::Duration::ZERO;
        tracker.record_columns(0, int_column());

        let mut windows: Vec<usize> = Vec::new();
        for batch in 0..5i64 {
            let rows: Vec<Vec<CellValue>> = (batch * 5..batch * 5 + 5)
                .map(|i| vec![CellValue::I64(i)])
                .collect();
            tracker.handle_rows(0, rows, &event_tx).await.unwrap();
            // Verify each advertised prefix NOW, while the writer is still
            // live and later batches have not been flushed.
            while let Ok(ev) = event_rx.try_recv() {
                if let QueryEvent::SpillWindow {
                    index,
                    spill_file,
                    rows_available,
                } = ev
                {
                    assert_eq!(index, 0);
                    let mut reader =
                        sqlly_core::SpillReader::open(&spill_file).expect("mid-stream open");
                    let page = reader.page(0, rows_available).expect("prefix pages");
                    assert_eq!(page.len(), rows_available, "full advertised prefix reads");
                    assert_eq!(
                        page[rows_available - 1][0],
                        CellValue::I64(rows_available as i64 - 1),
                        "prefix content matches the stream"
                    );
                    windows.push(rows_available);
                }
            }
        }
        drop(event_tx);
        assert_eq!(
            windows,
            vec![15, 20, 25],
            "SpillWindow carries the count duty post-suppression, monotonically"
        );

        // The advertised path was a borrow: the file must still be owned by
        // the tracker and reach the manifest as usual.
        let manifest = tracker.into_manifest().expect("suppressed set spills");
        assert_eq!(manifest.result_sets[0].row_count, 25);
        assert!(std::path::Path::new(&manifest.result_sets[0].spill_file).exists());
        discard_manifest_files(&manifest);
    }

    /// Sets that never suppress emit no `SpillWindow` (nor `RowsProgress`),
    /// and their advertise-nothing files are still deleted at the end.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn no_spill_window_for_unsuppressed_sets() {
        let (event_tx, mut event_rx) = mpsc::channel::<QueryEvent>(32);
        let mut tracker = SpillTracker::new(100, RESULT_MEMORY_BUDGET_BYTES);
        tracker.progress_interval = std::time::Duration::ZERO;
        tracker.record_columns(0, int_column());
        let rows: Vec<Vec<CellValue>> = (0..50i64).map(|i| vec![CellValue::I64(i)]).collect();
        tracker.handle_rows(0, rows, &event_tx).await.unwrap();
        drop(event_tx);

        while let Some(ev) = event_rx.recv().await {
            assert!(
                !matches!(
                    ev,
                    QueryEvent::SpillWindow { .. } | QueryEvent::RowsProgress { .. }
                ),
                "unsuppressed sets must not advertise a window, got {ev:?}"
            );
        }
        assert!(tracker.into_manifest().is_none());
    }

    /// Cancel path: the drain runs `into_manifest()` even for a cancelled
    /// query, so a set that advertised `SpillWindow`s mid-stream hands its
    /// files to the manifest (consumer-owned deletion) — the windows never
    /// transferred ownership, and nothing is left behind once the manifest
    /// consumer cleans up. Under-threshold sets in the same query are
    /// discarded as usual.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_stream_transfers_windowed_files_to_manifest() {
        let (event_tx, mut event_rx) = mpsc::channel::<QueryEvent>(64);
        let mut tracker = SpillTracker::new(5, RESULT_MEMORY_BUDGET_BYTES);
        tracker.progress_interval = std::time::Duration::ZERO;
        tracker.record_columns(0, int_column());
        tracker.record_columns(1, int_column());
        let rows: Vec<Vec<CellValue>> = (0..20i64).map(|i| vec![CellValue::I64(i)]).collect();
        tracker.handle_rows(0, rows, &event_tx).await.unwrap();
        tracker
            .handle_rows(1, vec![vec![CellValue::I64(0)]], &event_tx)
            .await
            .unwrap();

        let mut advertised: Option<String> = None;
        while let Ok(ev) = event_rx.try_recv() {
            if let QueryEvent::SpillWindow { spill_file, .. } = ev {
                advertised = Some(spill_file);
            }
        }
        let advertised = advertised.expect("suppressed set advertised a window");
        let small_path = tracker
            .writers
            .get(&1)
            .and_then(|w| w.as_ref())
            .map(|w| w.path.clone())
            .expect("small set has a mirror file");
        drop(event_tx);

        // Simulated cancel: the drain still calls into_manifest() before the
        // synthetic Cancelled terminal.
        let manifest = tracker.into_manifest().expect("suppressed set spills");
        assert_eq!(manifest.result_sets.len(), 1);
        assert_eq!(manifest.result_sets[0].spill_file, advertised);
        assert!(
            std::path::Path::new(&advertised).exists(),
            "windowed file survives cancel until the manifest owner deletes it"
        );
        assert!(
            !std::path::Path::new(&small_path).exists(),
            "under-threshold file is discarded on cancel"
        );
        discard_manifest_files(&manifest);
        assert!(!std::path::Path::new(&advertised).exists());
        assert!(!std::path::Path::new(&format!("{advertised}.idx")).exists());
    }

    /// A write failure AFTER the file was advertised on a `SpillWindow`
    /// must never unlink it — the consumer may be windowed over it. The set
    /// freezes at its flushed prefix: later rows are dropped (not forwarded
    /// as discontinuous `Rows`), and the manifest publishes the kept prefix
    /// so the consumer's provisional backing finalizes and deletion
    /// ownership transfers as promised.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn advertised_file_survives_write_failure_and_reaches_manifest() {
        let (event_tx, mut event_rx) = mpsc::channel::<QueryEvent>(64);
        let mut tracker = SpillTracker::new(10, RESULT_MEMORY_BUDGET_BYTES);
        tracker.progress_interval = std::time::Duration::ZERO;
        tracker.record_columns(0, int_column());
        // Cross the threshold so the flush succeeds and the path is
        // advertised (rows_available = 15).
        let rows: Vec<Vec<CellValue>> = (0..15i64).map(|i| vec![CellValue::I64(i)]).collect();
        tracker.handle_rows(0, rows, &event_tx).await.unwrap();
        let path = tracker
            .writers
            .get(&0)
            .and_then(|w| w.as_ref())
            .map(|w| w.path.clone())
            .expect("writer exists");
        assert_eq!(tracker.advertised.get(&0).copied(), Some(15));

        // Disk trouble strikes the advertised set.
        tracker.handle_writer_failure(0);
        assert!(
            std::path::Path::new(&path).exists(),
            "an advertised file must NOT be unlinked on write failure"
        );
        assert_eq!(
            tracker.frozen.get(&0),
            Some(&(path.clone(), 15)),
            "set freezes at the flushed prefix"
        );

        // Rows past the gap are counted but neither spilled nor forwarded.
        let more: Vec<Vec<CellValue>> = (15..25i64).map(|i| vec![CellValue::I64(i)]).collect();
        tracker.handle_rows(0, more, &event_tx).await.unwrap();
        drop(event_tx);
        let (forwarded, _progress) = collect_rows_and_progress(&mut event_rx).await;
        assert_eq!(forwarded, 10, "no rows forwarded after the freeze");

        let manifest = tracker
            .into_manifest()
            .expect("frozen set reaches manifest");
        assert_eq!(manifest.result_sets.len(), 1);
        assert_eq!(manifest.result_sets[0].spill_file, path);
        assert_eq!(
            manifest.result_sets[0].row_count, 15,
            "manifest publishes the flushed prefix count"
        );
        assert!(
            manifest
                .messages
                .iter()
                .any(|m| m.contains("spill write failed")),
            "truncation is surfaced, not silent"
        );
        discard_manifest_files(&manifest);
    }

    /// Baseline preserved: a write failure BEFORE any advertisement discards
    /// the file and the set falls back to forwarding every row.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unadvertised_write_failure_discards_and_forwards_everything() {
        let (event_tx, mut event_rx) = mpsc::channel::<QueryEvent>(64);
        let mut tracker = SpillTracker::new(100, RESULT_MEMORY_BUDGET_BYTES);
        tracker.progress_interval = std::time::Duration::ZERO;
        tracker.record_columns(0, int_column());
        let rows: Vec<Vec<CellValue>> = (0..5i64).map(|i| vec![CellValue::I64(i)]).collect();
        tracker.handle_rows(0, rows, &event_tx).await.unwrap();
        let path = tracker
            .writers
            .get(&0)
            .and_then(|w| w.as_ref())
            .map(|w| w.path.clone())
            .expect("writer exists");

        tracker.handle_writer_failure(0);
        assert!(
            !std::path::Path::new(&path).exists(),
            "unadvertised file is discarded"
        );
        assert!(tracker.frozen.is_empty());

        let more: Vec<Vec<CellValue>> = (5..10i64).map(|i| vec![CellValue::I64(i)]).collect();
        tracker.handle_rows(0, more, &event_tx).await.unwrap();
        drop(event_tx);
        let (forwarded, progress) = collect_rows_and_progress(&mut event_rx).await;
        assert_eq!(forwarded, 10, "everything forwarded so no data is lost");
        assert!(progress.is_empty());
        assert!(tracker.into_manifest().is_none());
    }

    #[test]
    fn spill_manifest_data_is_cloneable() {
        let data = SpillManifestData {
            result_sets: vec![SpillResultSetData {
                result_set_index: 0,
                columns: vec![],
                row_count: 100,
                spill_file: "/tmp/test.bin".to_string(),
            }],
            messages: vec!["ok".to_string()],
            error: None,
            elapsed_ms: Some(42),
        };
        let cloned = data.clone();
        assert_eq!(cloned.result_sets[0].row_count, 100);
        assert_eq!(cloned.messages[0], "ok");
    }
}
