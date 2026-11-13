//! Drill-through on spill-backed results: the rows behind a pivot cell or
//! chart mark, written to a filtered spill file and shown in the grid.
//!
//! A spill-backed grid holds only a window of rows, so it cannot filter
//! itself. When the user double-clicks a pivot cell (or clicks an aggregated
//! chart mark or histogram bin), the datatable hands over a
//! [`sqlly_datatable::RowSourceDrill`]: banner text plus a test that
//! reproduces exactly how the pivot or chart grouped the rows. This module
//! reads the whole spill file on the background executor, writes the rows
//! the test accepts to a new spill file beside it, and swaps that file in as
//! the slot's backing. The full result's backing is parked meanwhile, so the
//! Pivot and Chart tabs keep reading every row, and the banner's Clear puts
//! it back. Memory stays flat: the matching rows go to disk (browser storage
//! in the browser build).

use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Rows per page while a drill reads the full spill file.
const DRILL_PAGE_ROWS: usize = 4_096;

/// How often the progress caption repaints while a drill runs.
const DRILL_PROGRESS_TICK: Duration = Duration::from_millis(200);

/// Deletes a spill file (and its `.idx` sidecar) when dropped, unless kept.
/// Owned by the backing of a filtered view, after the pager, so the file is
/// closed before it is removed (Windows cannot delete an open file).
pub(super) struct SpillFileGuard {
    path: Option<String>,
}

impl SpillFileGuard {
    pub(super) fn new(path: String) -> Self {
        Self { path: Some(path) }
    }
}

impl Drop for SpillFileGuard {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            crate::spill_pager::remove_spill_files(&path);
        }
    }
}

/// A slot's drill-through: the parked full result while a filtered file is
/// shown, and the drill being written, if any.
#[derive(Default)]
pub(super) struct SpillDrill {
    /// The full result's backing, parked while the grid shows a filtered
    /// file. `None` until the first drill lands.
    pub(super) original: Option<SpillBacking>,
    /// Banner text of the filter currently shown.
    label: Option<String>,
    /// The drill being written.
    running: Option<DrillRun>,
}

/// A drill being written on the background executor.
struct DrillRun {
    generation: u64,
    cancel: Arc<AtomicBool>,
    progress: Arc<AtomicUsize>,
    total: usize,
    _task: Task<()>,
    _ticker: Task<()>,
}

impl Drop for DrillRun {
    /// A replaced or abandoned drill stops reading at its next page and
    /// removes what it wrote.
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// What a finished background drill produced.
enum DrillOutcome {
    /// The filtered file and how many rows it holds.
    Written { path: String, rows: usize },
    /// The full spill file could not be read or the filtered one written.
    Failed,
    /// Superseded or abandoned; nothing was left on disk.
    Cancelled,
}

/// Inputs for converting spill rows to grid rows exactly like the grid's
/// pages, so the drill test sees what the pivot and chart saw.
struct DrillConversion {
    columns: Vec<ResultColumn>,
    format_hints: Vec<crate::query_format::QueryFormatHint>,
    formatting: ResultFormattingPreferences,
    db: Option<sqlly_lang::sqlmodel::DatabaseType>,
}

/// Read the first `total` rows of the spill at `source`, keep those `drill`
/// accepts, and write them to a new spill beside it (on disk, named under
/// the `sqlly-spill-` prefix the startup orphan sweep cleans; in the browser,
/// in OPFS). Runs on the background executor.
async fn write_drill_file(
    source: &str,
    total: usize,
    conversion: &DrillConversion,
    drill: &sqlly_datatable::RowSourceDrill,
    cancel: &AtomicBool,
    progress: &AtomicUsize,
) -> DrillOutcome {
    // Dropping the writer on an early return below removes the partial file.
    let Some(mut writer) = crate::spill_pager::SpillFileWriter::create(source) else {
        tracing::warn!(source, "drill-through could not create its spill file");
        return DrillOutcome::Failed;
    };
    let mut offset = 0usize;
    let mut kept = 0usize;
    while offset < total {
        if cancel.load(Ordering::Relaxed) {
            return DrillOutcome::Cancelled;
        }
        let len = DRILL_PAGE_ROWS.min(total - offset);
        let Some(page) = crate::spill_pager::read_spill_rows(source.to_string(), offset, len).await
        else {
            tracing::warn!(source, "drill-through could not read the spill");
            return DrillOutcome::Failed;
        };
        if page.len() < len {
            tracing::warn!(source, offset, "drill-through read a short spill page");
            return DrillOutcome::Failed;
        }
        let grid_rows = crate::grid_bridge::convert_rows_with_formats(
            conversion.db,
            &page,
            &conversion.columns,
            &conversion.format_hints,
            Some(&conversion.formatting),
        );
        for (core_row, grid_row) in page.iter().zip(&grid_rows) {
            if drill.matches(grid_row) {
                if let Err(error) = writer.append(core_row) {
                    tracing::warn!(%error, "drill-through could not write a row");
                    return DrillOutcome::Failed;
                }
                kept += 1;
            }
        }
        offset += len;
        progress.store(offset, Ordering::Relaxed);
    }
    match writer.finish() {
        Ok(path) => DrillOutcome::Written { path, rows: kept },
        Err(error) => {
            tracing::warn!(%error, "drill-through could not finish its spill file");
            DrillOutcome::Failed
        }
    }
}

impl ResultsView {
    /// The backing holding `slot`'s complete result: the parked original
    /// while a filtered view is shown, else the live backing.
    pub(super) fn full_spill_backing(&self, slot: usize) -> Option<&SpillBacking> {
        self.spill_drills
            .get(slot)
            .and_then(Option::as_ref)
            .and_then(|drill| drill.original.as_ref())
            .or_else(|| self.spill_backings.get(slot)?.as_ref())
    }

    /// Whether `slot`'s grid shows a filtered drill file rather than the
    /// full result.
    pub(super) fn spill_drill_shown(&self, slot: usize) -> bool {
        self.spill_drills
            .get(slot)
            .and_then(Option::as_ref)
            .is_some_and(|drill| drill.original.is_some())
    }

    /// The handler `slot`'s grid calls with a drill over its row source.
    pub(super) fn spill_drill_handler(
        &self,
        slot: usize,
        cx: &mut Context<Self>,
    ) -> sqlly_datatable::RowSourceDrillHandler {
        let weak = cx.weak_entity();
        std::rc::Rc::new(move |drill, cx| {
            let _ = weak.update(cx, |this, cx| this.start_spill_drill(slot, drill, cx));
        })
    }

    /// Re-show the drill banner on a rebuilt grid for `slot` while a
    /// filtered view is shown.
    pub(super) fn restore_spill_drill_banner(
        &self,
        slot: usize,
        table: &mut SqllyDataTable,
        cx: &mut Context<Self>,
    ) {
        let Some(label) = self
            .spill_drills
            .get(slot)
            .and_then(Option::as_ref)
            .filter(|drill| drill.original.is_some())
            .and_then(|drill| drill.label.clone())
        else {
            return;
        };
        table.show_row_source_drill(label, self.spill_drill_clear_handler(slot, cx));
    }

    fn spill_drill_clear_handler(
        &self,
        slot: usize,
        cx: &mut Context<Self>,
    ) -> sqlly_datatable::RowSourceDrillClearHandler {
        let weak = cx.weak_entity();
        std::rc::Rc::new(move |cx| {
            let _ = weak.update(cx, |this, cx| this.clear_spill_drill(slot, cx));
        })
    }

    /// Start writing the rows `drill` accepts from `slot`'s full result to a
    /// filtered spill file. A drill already running on the slot is
    /// cancelled; the grid caption shows progress.
    pub(super) fn start_spill_drill(
        &mut self,
        slot: usize,
        drill: sqlly_datatable::RowSourceDrill,
        cx: &mut Context<Self>,
    ) {
        let Some(full) = self.full_spill_backing(slot) else {
            return;
        };
        let (source, total) = (full.path.clone(), full.total);
        let Some(columns) = self.result_sets.get(slot).map(|rs| rs.columns.clone()) else {
            return;
        };
        let conversion = DrillConversion {
            columns,
            format_hints: self.format_hints.clone(),
            formatting: self.effective_formatting_for_slot(slot),
            db: self.classification_db(),
        };
        while self.spill_drills.len() <= slot {
            self.spill_drills.push(None);
        }
        self.spill_drill_generation = self.spill_drill_generation.wrapping_add(1);
        let generation = self.spill_drill_generation;
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(AtomicUsize::new(0));
        let label = drill.label().to_string();

        let (task_cancel, task_progress) = (Arc::clone(&cancel), Arc::clone(&progress));
        let task = cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_executor()
                .spawn(async move {
                    write_drill_file(
                        &source,
                        total,
                        &conversion,
                        &drill,
                        &task_cancel,
                        &task_progress,
                    )
                    .await
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.finish_spill_drill(slot, generation, label, outcome, cx);
            });
        });
        let ticker = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(DRILL_PROGRESS_TICK).await;
                if this.update(cx, |_this, cx| cx.notify()).is_err() {
                    break;
                }
            }
        });
        // Replacing the run drops the previous one, which cancels it.
        let state = self.spill_drills[slot].get_or_insert_with(SpillDrill::default);
        state.running = Some(DrillRun {
            generation,
            cancel,
            progress,
            total,
            _task: task,
            _ticker: ticker,
        });
        cx.notify();
    }

    /// Foreground end of [`Self::start_spill_drill`]: show the filtered
    /// file, unless a newer drill (or a new result) replaced this one.
    fn finish_spill_drill(
        &mut self,
        slot: usize,
        generation: u64,
        label: String,
        outcome: DrillOutcome,
        cx: &mut Context<Self>,
    ) {
        let current = self
            .spill_drills
            .get(slot)
            .and_then(Option::as_ref)
            .and_then(|drill| drill.running.as_ref())
            .is_some_and(|run| run.generation == generation);
        if !current {
            if let DrillOutcome::Written { path, .. } = outcome {
                crate::spill_pager::remove_spill_files(&path);
            }
            return;
        }
        if let Some(Some(state)) = self.spill_drills.get_mut(slot) {
            state.running = None;
        }
        cx.notify();
        let (path, rows) = match outcome {
            DrillOutcome::Written { path, rows } => (path, rows),
            DrillOutcome::Cancelled => return,
            DrillOutcome::Failed => {
                self.note_copy_notice(crate::localization::text("results.spill_drill.failed"), cx);
                return;
            }
        };
        let Some(pager) = crate::spill_pager::SpillRowPager::open(&path) else {
            crate::spill_pager::remove_spill_files(&path);
            self.note_copy_notice(crate::localization::text("results.spill_drill.failed"), cx);
            return;
        };
        let backing = SpillBacking {
            pager: std::cell::RefCell::new(pager),
            total: rows,
            path: path.clone(),
            row_source: std::cell::RefCell::new(None),
            _owned_file: Some(SpillFileGuard::new(path)),
        };
        while self.spill_backings.len() <= slot {
            self.spill_backings.push(None);
        }
        let previous = self.spill_backings[slot].replace(backing);
        if let Some(Some(state)) = self.spill_drills.get_mut(slot) {
            // The first drill parks the full result; a later one replaces
            // the previous filtered file, which its guard deletes.
            if state.original.is_none() {
                state.original = previous;
            }
            state.label = Some(label.clone());
        }
        self.show_spill_window_from_top(slot, cx);
        let on_clear = self.spill_drill_clear_handler(slot, cx);
        if let Some(Some(view)) = self.grid_views.get(slot) {
            view.update(cx, |table, cx| {
                table.show_row_source_drill(label, on_clear);
                cx.notify();
            });
        }
        cx.notify();
    }

    /// Put `slot`'s full result back in the grid after a drill (the banner's
    /// Clear), deleting the filtered file and stopping a drill in progress.
    pub(super) fn clear_spill_drill(&mut self, slot: usize, cx: &mut Context<Self>) {
        let Some(state) = self.spill_drills.get_mut(slot).and_then(Option::take) else {
            return;
        };
        let SpillDrill {
            original, running, ..
        } = state;
        drop(running);
        if let Some(original) = original {
            // Dropping the filtered backing closes and deletes its file.
            self.spill_backings[slot] = Some(original);
            self.show_spill_window_from_top(slot, cx);
        }
        if let Some(Some(view)) = self.grid_views.get(slot) {
            view.update(cx, |table, cx| {
                table.dismiss_row_source_drill();
                cx.notify();
            });
        }
        cx.notify();
    }

    /// Drop every slot's drill state: running drills stop, parked full
    /// results close. Called before a new result replaces the backings.
    pub(super) fn discard_spill_drills(&mut self) {
        self.spill_drills.clear();
    }

    /// Point `slot`'s grid at the top of its (just swapped) backing: page in
    /// the first window and reset scroll and selection.
    fn show_spill_window_from_top(&mut self, slot: usize, cx: &mut Context<Self>) {
        while self.spill_paging.len() <= slot {
            self.spill_paging.push(SpillPageState::default());
        }
        self.spill_paging[slot].invalidate();
        let Some(Some(backing)) = self.spill_backings.get(slot) else {
            return;
        };
        let total = backing.total;
        let end = (self.perf_prefs.window_rows() * 2).min(total);
        let columns = self
            .result_sets
            .get(slot)
            .map(|result| result.columns.clone())
            .unwrap_or_default();
        let page = backing.pager.borrow_mut().page(0, end);
        // The browser's pager only has cached rows; read the rest.
        let short = page.len() < end;
        let mut rows = crate::grid_bridge::convert_rows_owned_with_formats(
            self.classification_db(),
            page,
            &columns,
            &self.format_hints,
            Some(&self.effective_formatting_for_slot(slot)),
        );
        self.apply_truncation_markers(slot, 0, &mut rows);
        let Some(Some(view)) = self.grid_views.get(slot) else {
            return;
        };
        let state = view.read(cx).state.clone();
        state.update(cx, |s, cx| {
            s.scroll_handle.set_offset(gpui::Point::default());
            s.selection = sqlly_datatable::Selection::None;
            if let Err(e) = s.set_row_window(total, 0, rows) {
                tracing::warn!("drill window rejected: {e}");
            }
            cx.notify();
        });
        if short {
            self.schedule_spill_page(slot, 0, end, cx);
        }
    }

    /// The grid caption while a drill is written on `slot`.
    pub(super) fn spill_drill_caption(&self, slot: usize) -> Option<String> {
        let run = self
            .spill_drills
            .get(slot)
            .and_then(Option::as_ref)
            .and_then(|drill| drill.running.as_ref())?;
        let percent = run
            .progress
            .load(Ordering::Relaxed)
            .saturating_mul(100)
            .checked_div(run.total)
            .unwrap_or(0)
            .min(99);
        Some(crate::localization::format(
            "results.spill_drill.progress",
            &[("percent", &percent.to_string())],
        ))
    }
}
