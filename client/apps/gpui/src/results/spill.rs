//! Spill-file backing for large result sets, split from the flat
//! `results` module.

use super::*;

/// A result set served from its on-disk spill file: the pager reads row
/// windows on demand while the grid presents the full virtual `total`.
/// `RefCell` because paging needs `&mut` seeks from `&self` render/stat
/// paths — accesses are short and never reentrant.
pub(super) struct SpillBacking {
    pub(super) pager: std::cell::RefCell<crate::spill_pager::SpillRowPager>,
    pub(super) total: usize,
    /// Spill-file path, so async scroll paging can open ephemeral readers on
    /// the background executor instead of borrowing the (non-`Send`) pager.
    pub(super) path: String,
    /// The whole file as the pivot and chart read it, built on first use
    /// and kept while `total` is unchanged (see [`Self::row_source`]).
    pub(super) row_source: std::cell::RefCell<Option<(usize, Arc<dyn sqlly_datatable::RowSource>)>>,
    /// A drill's filtered file, deleted with this backing. Declared after
    /// `pager` so the file is closed before it is removed.
    pub(super) _owned_file: Option<super::spill_drill::SpillFileGuard>,
}

/// Rows per page when the pivot or chart reads a whole spill file.
const ROW_SOURCE_PAGE_ROWS: usize = 4_096;

/// A spill-backed result as the Pivot and Chart tabs see it: every row of
/// the file, converted page by page exactly like the grid's windows. Each
/// scan opens its own reader, so it runs on the background executor
/// without touching the backing's (non-`Send`) pager. In the browser the
/// page thread cannot read the spill synchronously, so
/// [`sqlly_datatable::RowSource::ready`] first loads its bytes from the spill
/// worker into a JavaScript buffer, which the scan then decodes.
pub(super) struct SpillRowSource {
    pub(super) path: String,
    pub(super) total: usize,
    pub(super) columns: Vec<ResultColumn>,
    pub(super) format_hints: Vec<crate::query_format::QueryFormatHint>,
    pub(super) formatting: ResultFormattingPreferences,
    pub(super) db: Option<sqlly_lang::sqlmodel::DatabaseType>,
    #[cfg(target_arch = "wasm32")]
    pub(super) body: Arc<parking_lot::Mutex<Option<sqlly_services::browser::spill::SpillBody>>>,
}

impl SpillRowSource {
    /// Converts one page of spill rows like the grid does and visits them;
    /// `false` when `visit` asked to stop.
    fn visit_page(
        &self,
        page: Vec<Vec<CellValue>>,
        visit: &mut dyn FnMut(&[sqlly_datatable::CellValue]) -> bool,
    ) -> bool {
        let rows = crate::grid_bridge::convert_rows_owned_with_formats(
            self.db,
            page,
            &self.columns,
            &self.format_hints,
            Some(&self.formatting),
        );
        rows.iter().all(|row| visit(row))
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn scan_rows(
        &self,
        visit: &mut dyn FnMut(&[sqlly_datatable::CellValue]) -> bool,
    ) -> Result<(), sqlly_datatable::RowSourceError> {
        let Some(mut pager) = crate::spill_pager::SpillRowPager::open(&self.path) else {
            tracing::warn!(path = %self.path, "pivot/chart scan could not open the spill file");
            return Err(sqlly_datatable::RowSourceError(format!(
                "could not open {}",
                self.path
            )));
        };
        let mut offset = 0usize;
        while offset < self.total {
            let len = ROW_SOURCE_PAGE_ROWS.min(self.total - offset);
            let page = pager.page(offset, len);
            if page.len() < len {
                tracing::warn!(
                    path = %self.path,
                    offset,
                    expected = len,
                    read = page.len(),
                    "pivot/chart scan read a short spill page"
                );
                return Err(sqlly_datatable::RowSourceError(format!(
                    "short read at row {offset}"
                )));
            }
            if !self.visit_page(page, visit) {
                return Ok(());
            }
            offset += len;
        }
        Ok(())
    }

    /// The loaded body, decoded in pages; a result's computed columns are
    /// added with the rows they look back over carried between pages.
    #[cfg(target_arch = "wasm32")]
    fn scan_rows(
        &self,
        visit: &mut dyn FnMut(&[sqlly_datatable::CellValue]) -> bool,
    ) -> Result<(), sqlly_datatable::RowSourceError> {
        let guard = self.body.lock();
        let body = guard.as_ref().ok_or_else(|| {
            sqlly_datatable::RowSourceError(format!("{} was not loaded", self.path))
        })?;
        let computed = crate::computed_columns::spill_window(&self.path);
        let lookback = computed.as_ref().map_or(0, |w| w.plan.max_lookback());
        let mut tail: Vec<Vec<CellValue>> = Vec::new();
        let mut page: Vec<Vec<CellValue>> = Vec::with_capacity(ROW_SOURCE_PAGE_ROWS);
        let mut seen = 0usize;
        let mut stopped = false;
        let mut flush = |page: &mut Vec<Vec<CellValue>>, tail: &mut Vec<Vec<CellValue>>| -> bool {
            let raw = std::mem::take(page);
            let rows = match &computed {
                Some(window) => {
                    let len = raw.len();
                    let offset = tail.len();
                    let mut window_rows = std::mem::take(tail);
                    window_rows.extend(raw);
                    let keep = window_rows.len().saturating_sub(lookback);
                    *tail = window_rows[keep..].to_vec();
                    window.page(offset, len, move |_, _| window_rows)
                }
                None => raw,
            };
            self.visit_page(rows, visit)
        };
        body.scan(&mut |row| {
            if seen >= self.total {
                return false;
            }
            seen += 1;
            page.push(row);
            if page.len() == ROW_SOURCE_PAGE_ROWS && !flush(&mut page, &mut tail) {
                stopped = true;
                return false;
            }
            true
        })
        .map_err(|error| {
            tracing::warn!(path = %self.path, %error, "pivot/chart scan failed");
            sqlly_datatable::RowSourceError(error)
        })?;
        if !stopped && !page.is_empty() {
            flush(&mut page, &mut tail);
        }
        Ok(())
    }
}

impl sqlly_datatable::RowSource for SpillRowSource {
    fn row_count(&self) -> usize {
        self.total
    }

    fn scan(
        &self,
        visit: &mut dyn FnMut(&[sqlly_datatable::CellValue]) -> bool,
    ) -> Result<(), sqlly_datatable::RowSourceError> {
        self.scan_rows(visit)
    }

    #[cfg(target_arch = "wasm32")]
    fn ready(&self) -> Option<sqlly_datatable::RowSourceReady> {
        if self.body.lock().is_some() {
            return None;
        }
        let body = Arc::clone(&self.body);
        let load = sqlly_services::browser::spill::load_body(&self.path);
        Some(Box::pin(async move {
            let loaded = load.await.map_err(sqlly_datatable::RowSourceError)?;
            *body.lock() = Some(loaded);
            Ok(())
        }))
    }
}

/// What the foreground apply should do with a completed spill page read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SpillReadUi {
    /// Generation mismatch: drop the result and do not message.
    Silent,
    /// Matching generation and `None` rows: a real open/IO failure.
    ReportError,
    /// Matching generation and rows present (including an empty page).
    Apply,
}

/// Distinguishes a superseded read from a failed open. `None` is only
/// produced when `SpillRowPager::open` fails; a successfully opened empty
/// page is `Some([])`, not `None`.
pub(super) fn spill_read_outcome<T>(generation_ok: bool, rows: Option<&T>) -> SpillReadUi {
    if !generation_ok {
        SpillReadUi::Silent
    } else if rows.is_none() {
        SpillReadUi::ReportError
    } else {
        SpillReadUi::Apply
    }
}

/// Per-slot state for asynchronous scroll-driven spill paging: the grid
/// observer computes the desired window on the UI thread, a short debounce
/// timer coalesces bursts, and a background task reads + converts the page
/// (see [`ResultsView::schedule_spill_page`]). The monotonic `generation` is
/// bumped at every read spawn and on clear/reinstall, so a stale in-flight
/// read's foreground apply is a silent no-op.
#[derive(Default)]
pub(super) struct SpillPageState {
    /// Monotonic per-slot token; only an apply carrying the latest value
    /// wins. Never reset — entries survive `clear()` so a later query
    /// reusing the slot index cannot collide with an old in-flight token.
    pub(super) generation: u64,
    /// Latest desired window `(start, end)`, replaced in place by newer
    /// requests while the debounce timer is armed.
    pub(super) pending: Option<(usize, usize)>,
    /// The armed debounce timer, if any. Deliberately NOT restarted per
    /// request — it stays armed across a burst so a steady scroll cannot
    /// starve paging (same shape as `schedule_streaming_notify`).
    pub(super) debounce: Option<Task<()>>,
    /// The in-flight background read; dropping it cancels the read.
    pub(super) read: Option<Task<()>>,
    /// Window of the in-flight read, so an identical re-request is skipped
    /// instead of respawning the same read.
    pub(super) inflight: Option<(usize, usize)>,
    /// A matching-generation page-read failure has already been shown for
    /// this slot. Cleared on invalidate and on a successful apply — not on
    /// each scroll-driven generation bump, so a missing file cannot spam
    /// Messages on every scrollbar tick.
    pub(super) read_error_reported: bool,
}

impl SpillPageState {
    /// Bumps the generation and drops pending/in-flight tasks: a read still
    /// executing is cancelled, and one whose apply already left the
    /// background executor no-ops on the generation check.
    pub(super) fn invalidate(&mut self) {
        self.generation += 1;
        self.pending = None;
        self.debounce = None;
        self.read = None;
        self.inflight = None;
        self.read_error_reported = false;
    }
}

/// Owns the active spill manifest and deletes its on-disk files on drop —
/// covering tab close and view teardown without a `Drop` impl on
/// `ResultsView` itself (which would forbid struct-update syntax in tests).
#[derive(Default)]
pub(super) struct SpillManifestGuard(pub(super) Option<crate::spill_pager::SpillManifest>);

impl Drop for SpillManifestGuard {
    fn drop(&mut self) {
        if let Some(manifest) = self.0.take() {
            manifest.cleanup();
        }
    }
}

impl SpillBacking {
    /// The row source over this backing's first `total` rows, reusing the
    /// one built for the same `total` so the pivot and chart only rescan
    /// when the set actually grew. `build` makes a new one.
    pub(super) fn row_source(
        &self,
        build: impl FnOnce() -> SpillRowSource,
    ) -> Arc<dyn sqlly_datatable::RowSource> {
        let mut cached = self.row_source.borrow_mut();
        if let Some((total, source)) = cached.as_ref()
            && *total == self.total
        {
            return Arc::clone(source);
        }
        let source: Arc<dyn sqlly_datatable::RowSource> = Arc::new(build());
        *cached = Some((self.total, Arc::clone(&source)));
        source
    }

    /// Reads the full backing set, paged. Interactive UI uses windows;
    /// user copy/export must use [`Self::first_rows`] so a million-row
    /// spill cannot be fully materialized. Reads clamp to `total`: for a
    /// provisional (mid-query) backing the reader's own row count may
    /// exceed the safely flushed prefix.
    #[cfg(test)]
    pub(super) fn all_rows(&self) -> Vec<Vec<CellValue>> {
        let mut pager = self.pager.borrow_mut();
        let mut out = Vec::with_capacity(self.total);
        let mut offset = 0usize;
        while offset < self.total {
            let page = pager.page(offset, 10_000.min(self.total - offset));
            if page.is_empty() {
                break;
            }
            offset += page.len();
            out.extend(page);
        }
        out
    }

    /// Reads at most the first `limit` rows without materializing the full
    /// spill file. Used by export/clipboard (capped at
    /// [`crate::result_export::EXPORT_MAX_ROWS`]), snapshots, charts, and
    /// bounded diagnostics such as data profiling.
    pub(super) fn first_rows(&self, limit: usize) -> Vec<Vec<CellValue>> {
        let count = limit.min(self.total);
        self.pager.borrow_mut().page(0, count)
    }
}

/// Rows per page when a background column scan walks a spill file.
const COLUMN_SCAN_PAGE_ROWS: usize = 2_000;

/// Read one column of the first `count` rows of the spill file at `path`
/// with a private reader — meant for the background executor, so column-wide
/// tools (Top Values, Copy Distinct, Profile) never read disk on the UI
/// thread. Pages through the file and keeps only the one column, so memory
/// is bounded by the column, not by the row width. `count` must already be
/// clamped to the backing's safe `total`. `None` when the file cannot be
/// opened; a short file yields the rows it has.
pub(crate) async fn read_spill_column(
    path: &str,
    column: usize,
    count: usize,
) -> Option<Vec<CellValue>> {
    let mut out = Vec::with_capacity(count.min(COLUMN_SCAN_PAGE_ROWS * 8));
    let mut offset = 0usize;
    while offset < count {
        let len = COLUMN_SCAN_PAGE_ROWS.min(count - offset);
        let page = crate::spill_pager::read_spill_rows(path.to_string(), offset, len).await?;
        if page.is_empty() {
            break;
        }
        offset += page.len();
        out.extend(
            page.into_iter()
                .map(|row| row.into_iter().nth(column).unwrap_or(CellValue::Null)),
        );
    }
    Some(out)
}

impl ResultsView {
    /// The row source for `slot`'s Pivot and Chart tabs: the whole spill
    /// file when the slot is spill-backed and its grid shows exactly the
    /// result's columns (a staged-edit marker column would shift every
    /// field), else `None`.
    pub(super) fn row_source_for_slot(
        &self,
        slot: usize,
        grid_columns: usize,
    ) -> Option<Arc<dyn sqlly_datatable::RowSource>> {
        // The full result even while a drill's filtered file is shown.
        let backing = self.full_spill_backing(slot)?;
        let columns = &self.result_sets.get(slot)?.columns;
        if columns.len() != grid_columns {
            return None;
        }
        Some(backing.row_source(|| SpillRowSource {
            path: backing.path.clone(),
            total: backing.total,
            columns: columns.clone(),
            format_hints: self.format_hints.clone(),
            formatting: self.effective_formatting_for_slot(slot),
            db: self.classification_db(),
            #[cfg(target_arch = "wasm32")]
            body: Arc::default(),
        }))
    }

    /// Give `slot`'s grid the row source its Pivot and Chart tabs should
    /// read (or take it away), then refresh their lock: a spill-backed slot
    /// whose rows cannot be read as a whole stays locked with a reason.
    pub(super) fn sync_row_source_for_slot(&mut self, slot: usize, cx: &mut Context<Self>) {
        let Some(Some(view)) = self.grid_views.get(slot) else {
            return;
        };
        let view = view.clone();
        let grid_columns = view.read(cx).state.read(cx).data.columns.len();
        let source = self.row_source_for_slot(slot, grid_columns);
        let has_source = source.is_some();
        let (locked, status) = self.pivot_chart_lock_for_slot(slot, has_source);
        let drill_handler = has_source.then(|| self.spill_drill_handler(slot, cx));
        view.update(cx, |table, cx| {
            table.set_row_source(source, cx);
            table.set_row_source_drill_handler(drill_handler, cx);
            table.set_pivot_locked(locked, status.clone());
            table.set_chart_locked(locked, status);
            cx.notify();
        });
    }
}
