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
pub(crate) fn read_spill_column(path: &str, column: usize, count: usize) -> Option<Vec<CellValue>> {
    let mut pager = crate::spill_pager::SpillRowPager::open(path)?;
    let mut out = Vec::with_capacity(count.min(COLUMN_SCAN_PAGE_ROWS * 8));
    let mut offset = 0usize;
    while offset < count {
        let len = COLUMN_SCAN_PAGE_ROWS.min(count - offset);
        let page = pager.page(offset, len);
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
