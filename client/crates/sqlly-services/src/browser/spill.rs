//! Large results in the browser, kept in the Origin Private File System.
//!
//! The desktop app writes a result set past the spill threshold (or the
//! per-set memory budget) to a file and pages the grid from it. The browser
//! build does the same with OPFS. [`SpillStage`] sits on a grid run's event
//! stream: it forwards the first rows as usual and encodes the rest in the
//! `sqlly-core` spill format. The encoded bytes go to a dedicated worker
//! (`apps/browser/www/spill-worker.js`, reached through the
//! `globalThis.sqllySpill` bridge that `boot.js` installs), because OPFS sync
//! access handles only exist in workers. Reads come back the same way, as
//! byte runs that are decoded here.
//!
//! A spill is named by a path, `opfs:<name>`, so the results view, its pager
//! and the spill manifest handle it like a file. Reads are asynchronous; the
//! page cache ([`cached_rows`]) keeps the rows the grid has loaded, for the
//! view's synchronous lookups of on-screen rows.
//!
//! JavaScript values are `!Send`, so every bridge call that answers runs in a
//! `spawn_local` task and replies over a oneshot channel, the same shape as
//! [`super::pglite`]. Without OPFS (no bridge, a private window that refuses
//! it, too little quota) nothing spills and results stay in memory.

use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use async_trait::async_trait;
use sqlly_core::spill::{decode_complete_rows, decode_rows};
use sqlly_core::{CellValue, ConnectionProfile, SpillWriter, StudioResult};
use tokio::sync::{mpsc, oneshot};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::{JsFuture, spawn_local};

use crate::query::spill_stage::{CHUNK_ROWS, RowCache, SpillStage, StageSink, StageStore};
use crate::query::{QueryExecutor, QueryHandle, result_memory_budget_bytes, spill_row_threshold};

/// Prefix of every browser spill path.
pub const PATH_PREFIX: &str = "opfs:";

/// Encoded bytes buffered on the page before they are sent to the worker.
const SHIP_BYTES: usize = 1 << 20;

/// Estimated row bytes the page cache holds across all spills.
const CACHE_BYTES: usize = 64 << 20;

/// Bytes copied into wasm memory per step of a full scan.
const SCAN_SLICE_BYTES: usize = 4 << 20;

// ---------------------------------------------------------------------------
// Bridge

fn bridge() -> Option<JsValue> {
    js_sys::Reflect::get(&js_sys::global(), &JsValue::from_str("sqllySpill"))
        .ok()
        .filter(JsValue::is_object)
}

fn js_text(value: &JsValue) -> String {
    value
        .as_string()
        .or_else(|| {
            js_sys::Reflect::get(value, &JsValue::from_str("message"))
                .ok()
                .and_then(|m| m.as_string())
        })
        .unwrap_or_else(|| format!("{value:?}"))
}

fn invoke(method: &str, args: &[JsValue]) -> Result<JsValue, String> {
    let bridge = bridge().ok_or_else(|| "browser storage is not available".to_string())?;
    let function: js_sys::Function = js_sys::Reflect::get(&bridge, &JsValue::from_str(method))
        .ok()
        .and_then(|f| f.dyn_into().ok())
        .ok_or_else(|| format!("the spill bridge has no {method}"))?;
    let args: js_sys::Array = args.iter().collect();
    function.apply(&bridge, &args).map_err(|e| js_text(&e))
}

async fn invoke_async(method: &'static str, args: Vec<JsValue>) -> Result<JsValue, String> {
    let promise: js_sys::Promise = invoke(method, &args)?
        .dyn_into()
        .map_err(|_| format!("sqllySpill.{method} did not return a Promise"))?;
    JsFuture::from(promise).await.map_err(|e| js_text(&e))
}

/// Runs `work` on the browser thread and hands its result back over a
/// channel, so the caller's future stays `Send`.
fn on_page<T: Send + 'static>(
    work: impl Future<Output = Result<T, String>> + 'static,
) -> impl Future<Output = Result<T, String>> + Send {
    let (tx, rx) = oneshot::channel();
    spawn_local(async move {
        let _ = tx.send(work.await);
    });
    async move {
        rx.await
            .unwrap_or_else(|_| Err("the browser storage request was dropped".into()))
    }
}

// ---------------------------------------------------------------------------
// Availability

const UNCHECKED: u8 = 0;
const USABLE: u8 = 1;
const UNAVAILABLE: u8 = 2;

static STATE: AtomicU8 = AtomicU8::new(UNCHECKED);
static FREE_BYTES: AtomicU64 = AtomicU64::new(0);

/// Checks for OPFS support, measures the free quota and removes spill files
/// left behind by tabs that closed. Called once when the app starts.
pub fn start() {
    if bridge().is_none() {
        STATE.store(UNAVAILABLE, Ordering::Relaxed);
        return;
    }
    spawn_local(async {
        let supported = invoke_async("supported", Vec::new())
            .await
            .ok()
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if !supported {
            tracing::info!("OPFS is not available; large results stay in memory");
            STATE.store(UNAVAILABLE, Ordering::Relaxed);
            return;
        }
        refresh_free_bytes().await;
        STATE.store(USABLE, Ordering::Relaxed);
        match invoke_async("sweep", Vec::new()).await {
            Ok(removed) => {
                let removed = removed.as_f64().unwrap_or(0.0);
                if removed > 0.0 {
                    tracing::info!(removed, "removed spill files left by closed tabs");
                }
            }
            Err(error) => tracing::warn!(%error, "spill sweep failed"),
        }
    });
}

async fn refresh_free_bytes() {
    match invoke_async("estimate", Vec::new()).await {
        Ok(estimate) => {
            let field = |key: &str| {
                js_sys::Reflect::get(&estimate, &JsValue::from_str(key))
                    .ok()
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0)
            };
            let free = (field("quota") - field("usage")).max(0.0);
            FREE_BYTES.store(free as u64, Ordering::Relaxed);
        }
        Err(error) => tracing::warn!(%error, "could not read the storage estimate"),
    }
}

/// Whether results can spill to browser storage in this tab.
pub fn available() -> bool {
    STATE.load(Ordering::Relaxed) == USABLE
}

fn free_bytes() -> u64 {
    FREE_BYTES.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Spills and the page cache

/// A finished spill: its row count (the worker holds the bytes).
struct Spill {
    rows: usize,
}

thread_local! {
    static SPILLS: RefCell<HashMap<String, Spill>> = RefCell::new(HashMap::new());
    static CACHE: RefCell<RowCache> = RefCell::new(RowCache::new(CACHE_BYTES));
    static BODIES: RefCell<HashMap<u64, js_sys::Uint8Array>> = RefCell::new(HashMap::new());
}

/// The path of the spill called `name`.
pub fn path_for(name: &str) -> String {
    format!("{PATH_PREFIX}{name}")
}

/// The spill name in `path`, if it is a browser spill path.
pub fn name_of(path: &str) -> Option<&str> {
    path.strip_prefix(PATH_PREFIX)
}

/// Rows in the finished spill at `path`.
pub fn row_count(path: &str) -> Option<usize> {
    let name = name_of(path)?;
    SPILLS.with(|s| s.borrow().get(name).map(|spill| spill.rows))
}

/// Deletes the spill at `path` from browser storage. Unknown paths are
/// ignored.
pub fn remove(path: &str) {
    let Some(name) = name_of(path) else {
        return;
    };
    let known = SPILLS.with(|s| s.borrow_mut().remove(name)).is_some();
    CACHE.with(|c| c.borrow_mut().forget(name));
    if known {
        if let Err(error) = invoke("remove", &[JsValue::from_str(name)]) {
            tracing::warn!(%error, name, "could not remove a spill");
        }
        spawn_local(refresh_free_bytes());
    }
}

/// `(name, start, len)` with `len` clamped to the spill's rows.
fn clamp(path: &str, start: usize, len: usize) -> Result<(String, usize, usize), String> {
    let name = name_of(path).ok_or_else(|| format!("{path} is not a browser spill"))?;
    let total = row_count(path).ok_or_else(|| format!("{path} is not open"))?;
    let len = len.min(total.saturating_sub(start));
    Ok((name.to_string(), start, len))
}

/// Rows `[start, start + len)` from the page cache, clamped to the spill's
/// rows; `None` unless every one of them is cached.
pub fn cached_rows(path: &str, start: usize, len: usize) -> Option<Vec<Vec<CellValue>>> {
    let (name, start, len) = clamp(path, start, len).ok()?;
    CACHE.with(|c| c.borrow_mut().get(&name, start, len))
}

async fn fetch(name: String, start: usize, end: usize) -> Result<Vec<Vec<CellValue>>, String> {
    if start >= end {
        return Ok(Vec::new());
    }
    let bytes: js_sys::Uint8Array = invoke_async(
        "page",
        vec![
            JsValue::from_str(&name),
            JsValue::from_f64(start as f64),
            JsValue::from_f64(end as f64),
        ],
    )
    .await?
    .dyn_into()
    .map_err(|_| "sqllySpill.page did not return bytes".to_string())?;
    let rows = decode_rows(&bytes.to_vec()).map_err(|e| e.to_string())?;
    if rows.len() != end - start {
        return Err(format!(
            "spill read returned {} of {} rows",
            rows.len(),
            end - start
        ));
    }
    Ok(rows)
}

/// Reads rows `[start, start + len)` (clamped to the spill's rows) without
/// caching them: for scans that walk a whole spill.
pub fn read_rows(
    path: &str,
    start: usize,
    len: usize,
) -> impl Future<Output = Result<Vec<Vec<CellValue>>, String>> + Send + use<> {
    let request = clamp(path, start, len);
    let cached = request
        .as_ref()
        .ok()
        .and_then(|(name, start, len)| CACHE.with(|c| c.borrow_mut().get(name, *start, *len)));
    async move {
        if let Some(rows) = cached {
            return Ok(rows);
        }
        let (name, start, len) = request?;
        on_page(fetch(name, start, start + len)).await
    }
}

/// Reads rows `[start, start + len)` like [`read_rows`] and keeps them in the
/// page cache: for the grid's windows, whose rows the view looks up again.
pub fn read_rows_cached(
    path: &str,
    start: usize,
    len: usize,
) -> impl Future<Output = Result<Vec<Vec<CellValue>>, String>> + Send + use<> {
    let request = clamp(path, start, len);
    let cached = request
        .as_ref()
        .ok()
        .and_then(|(name, start, len)| CACHE.with(|c| c.borrow_mut().get(name, *start, *len)));
    let total = row_count(path).unwrap_or(0);
    async move {
        if let Some(rows) = cached {
            return Ok(rows);
        }
        let (name, start, len) = request?;
        let from = start - start % CHUNK_ROWS;
        let to = (start + len).div_ceil(CHUNK_ROWS) * CHUNK_ROWS;
        let to = to.min(total);
        on_page(async move {
            let rows = fetch(name.clone(), from, to).await?;
            let wanted = rows[start - from..start - from + len].to_vec();
            CACHE.with(|c| c.borrow_mut().put(&name, from, rows, total));
            Ok(wanted)
        })
        .await
    }
}

/// A spill's whole data body, held as a JavaScript buffer (outside the app's
/// wasm memory) while the pivot or chart reads it. The buffer is released
/// when this is dropped.
pub struct SpillBody {
    token: u64,
    rows: usize,
}

static NEXT_BODY: AtomicU64 = AtomicU64::new(1);

/// Loads the whole of the spill at `path` for [`SpillBody::scan`].
pub fn load_body(path: &str) -> impl Future<Output = Result<SpillBody, String>> + Send + use<> {
    let request = clamp(path, 0, usize::MAX);
    async move {
        let (name, _, rows) = request?;
        on_page(async move {
            let bytes: js_sys::Uint8Array = invoke_async("body", vec![JsValue::from_str(&name)])
                .await?
                .dyn_into()
                .map_err(|_| "sqllySpill.body did not return bytes".to_string())?;
            let token = NEXT_BODY.fetch_add(1, Ordering::Relaxed);
            BODIES.with(|b| b.borrow_mut().insert(token, bytes));
            Ok(SpillBody { token, rows })
        })
        .await
    }
}

impl SpillBody {
    pub fn row_count(&self) -> usize {
        self.rows
    }

    /// Visits every row in order, decoding a few megabytes at a time;
    /// `visit` returns `false` to stop.
    pub fn scan(&self, visit: &mut dyn FnMut(Vec<CellValue>) -> bool) -> Result<(), String> {
        let bytes = BODIES
            .with(|b| b.borrow().get(&self.token).cloned())
            .ok_or_else(|| "the spill body is no longer loaded".to_string())?;
        let total = bytes.length() as usize;
        let mut at = 0usize;
        let mut carry: Vec<u8> = Vec::new();
        let mut slice = SCAN_SLICE_BYTES;
        let mut seen = 0usize;
        while at < total {
            let end = (at + slice).min(total);
            carry.extend_from_slice(&bytes.subarray(at as u32, end as u32).to_vec());
            at = end;
            let (rows, used) = decode_complete_rows(&carry).map_err(|e| e.to_string())?;
            if used == 0 {
                // One row larger than the slice: read further before decoding.
                slice = slice.saturating_mul(2);
                continue;
            }
            carry.drain(..used);
            for row in rows {
                seen += 1;
                if !visit(row) {
                    return Ok(());
                }
            }
        }
        if !carry.is_empty() || seen != self.rows {
            return Err(format!("spill body held {seen} of {} rows", self.rows));
        }
        Ok(())
    }
}

impl Drop for SpillBody {
    fn drop(&mut self) {
        BODIES.with(|b| b.borrow_mut().remove(&self.token));
    }
}

// ---------------------------------------------------------------------------
// Writing

/// A growable byte buffer shared with the spill writer. A mutex rather than
/// a `RefCell` so a sink can live in a `Send` background future.
#[derive(Clone, Default)]
struct Buffer(Arc<std::sync::Mutex<Vec<u8>>>);

impl Buffer {
    fn bytes(&self) -> std::sync::MutexGuard<'_, Vec<u8>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn take(&self) -> Vec<u8> {
        std::mem::take(&mut *self.bytes())
    }

    fn len(&self) -> usize {
        self.bytes().len()
    }
}

impl Write for Buffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.bytes().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// One result set being written in the spill format. Rows collect on the
/// page until [`Self::open`] creates the files in browser storage; from then
/// on they go to the worker a megabyte at a time. Dropping an opened sink
/// before [`Self::finish`] deletes its files.
pub struct SpillSink {
    name: String,
    writer: SpillWriter<Buffer>,
    data: Buffer,
    index: Buffer,
    opened: bool,
    finished: bool,
    shipped_bytes: u64,
}

impl SpillSink {
    pub fn new() -> Result<Self, String> {
        let (data, index) = (Buffer::default(), Buffer::default());
        let writer = SpillWriter::new(data.clone(), index.clone()).map_err(|e| e.to_string())?;
        Ok(Self {
            name: format!("sqlly-spill-{}.bin", uuid::Uuid::new_v4().simple()),
            writer,
            data,
            index,
            opened: false,
            finished: false,
            shipped_bytes: 0,
        })
    }

    pub fn append(&mut self, row: &[CellValue]) -> Result<(), String> {
        self.writer.append(row).map_err(|e| e.to_string())?;
        if self.opened && self.data.len() >= SHIP_BYTES {
            self.ship()?;
        }
        Ok(())
    }

    pub fn rows(&self) -> usize {
        self.writer.row_count()
    }

    /// Bytes written so far, sent or not.
    pub fn bytes(&self) -> u64 {
        self.shipped_bytes + (self.data.len() + self.index.len()) as u64
    }

    /// Creates the files in browser storage and sends what is buffered.
    pub fn open(&mut self) -> Result<(), String> {
        if !self.opened {
            invoke("create", &[JsValue::from_str(&self.name)])?;
            self.opened = true;
        }
        self.ship()
    }

    fn ship(&mut self) -> Result<(), String> {
        self.writer.flush().map_err(|e| e.to_string())?;
        let (data, index) = (self.data.take(), self.index.take());
        if data.is_empty() && index.is_empty() {
            return Ok(());
        }
        self.shipped_bytes += (data.len() + index.len()) as u64;
        invoke(
            "append",
            &[
                JsValue::from_str(&self.name),
                js_sys::Uint8Array::from(&data[..]).into(),
                js_sys::Uint8Array::from(&index[..]).into(),
            ],
        )
        .map(|_| ())
    }

    /// Sends the rest and registers the spill for reading; returns its path.
    pub fn finish(mut self) -> Result<String, String> {
        self.open()?;
        self.finished = true;
        let rows = self.rows();
        SPILLS.with(|s| s.borrow_mut().insert(self.name.clone(), Spill { rows }));
        Ok(path_for(&self.name))
    }
}

impl Drop for SpillSink {
    fn drop(&mut self) {
        if self.opened && !self.finished {
            let _ = invoke("remove", &[JsValue::from_str(&self.name)]);
        }
    }
}

// ---------------------------------------------------------------------------
// The stage on a grid run's event stream

/// Browser storage as a [`SpillStage`]'s store.
pub(crate) struct OpfsStore;

impl StageStore for OpfsStore {
    type Sink = SpillSink;

    fn sink(&self) -> Option<SpillSink> {
        SpillSink::new().ok()
    }

    fn free_bytes(&self) -> u64 {
        free_bytes()
    }

    fn now_ms(&self) -> f64 {
        js_sys::Date::now()
    }

    fn landed(&self, path: &str, seed: Vec<Vec<CellValue>>) {
        if let (Some(name), Some(total)) = (name_of(path), row_count(path)) {
            CACHE.with(|c| c.borrow_mut().put(name, 0, seed, total));
        }
        spawn_local(refresh_free_bytes());
    }
}

impl StageSink for SpillSink {
    fn append(&mut self, row: &[CellValue]) -> Result<(), String> {
        SpillSink::append(self, row)
    }

    fn open(&mut self) -> Result<(), String> {
        SpillSink::open(self)
    }

    fn rows(&self) -> usize {
        SpillSink::rows(self)
    }

    fn bytes(&self) -> u64 {
        SpillSink::bytes(self)
    }

    fn finish(self) -> Result<String, String> {
        SpillSink::finish(self)
    }
}

/// The spill stage for a grid run in this tab, or `None` when results
/// cannot spill here.
pub(crate) fn stage() -> Option<SpillStage<OpfsStore>> {
    available().then(|| {
        SpillStage::new(
            OpfsStore,
            spill_row_threshold(),
            result_memory_budget_bytes(),
        )
    })
}

/// Puts a [`SpillStage`] on the grid runs of an executor that streams its
/// rows (a relay or SQLly Embedded session); every other call passes
/// through unchanged.
pub struct SpillingQueryExecutor {
    inner: Arc<dyn QueryExecutor>,
}

impl SpillingQueryExecutor {
    pub fn new(inner: Arc<dyn QueryExecutor>) -> Self {
        Self { inner }
    }
}

/// Passes `handle`'s events through a fresh [`SpillStage`].
pub fn spill_stream(handle: QueryHandle, row_buffer: usize) -> QueryHandle {
    let Some(mut stage) = stage() else {
        return handle;
    };
    let QueryHandle {
        task_id,
        mut events,
    } = handle;
    let (tx, rx) = mpsc::channel(row_buffer.max(16));
    spawn_local(async move {
        let mut out = Vec::new();
        while let Some(event) = events.recv().await {
            stage.push(event, &mut out);
            for event in out.drain(..) {
                if tx.send(event).await.is_err() {
                    // The consumer is gone: dropping the stage deletes any
                    // spill it had started.
                    return;
                }
            }
        }
    });
    QueryHandle {
        task_id,
        events: rx,
    }
}

#[async_trait]
impl QueryExecutor for SpillingQueryExecutor {
    async fn execute(
        &self,
        profile: &ConnectionProfile,
        sql: &str,
        row_buffer: usize,
    ) -> StudioResult<QueryHandle> {
        self.inner.execute(profile, sql, row_buffer).await
    }

    async fn execute_in_session(
        &self,
        profile: &ConnectionProfile,
        session: Option<&str>,
        sql: &str,
        row_buffer: usize,
    ) -> StudioResult<QueryHandle> {
        self.inner
            .execute_in_session(profile, session, sql, row_buffer)
            .await
    }

    async fn execute_full_values(
        &self,
        profile: &ConnectionProfile,
        session: Option<&str>,
        sql: &str,
        row_buffer: usize,
    ) -> StudioResult<QueryHandle> {
        self.inner
            .execute_full_values(profile, session, sql, row_buffer)
            .await
    }

    async fn execute_for_grid(
        &self,
        profile: &ConnectionProfile,
        session: Option<&str>,
        sql: &str,
        row_buffer: usize,
    ) -> StudioResult<QueryHandle> {
        let handle = self
            .inner
            .execute_for_grid(profile, session, sql, row_buffer)
            .await?;
        Ok(spill_stream(handle, row_buffer))
    }

    fn evict_profile(&self, profile: &ConnectionProfile) {
        self.inner.evict_profile(profile);
    }

    async fn cancel(&self, task_id: &str) -> StudioResult<()> {
        self.inner.cancel(task_id).await
    }
}
