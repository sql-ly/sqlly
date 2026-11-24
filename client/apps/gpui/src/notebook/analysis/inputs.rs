//! How analysis cells read upstream results: one function,
//! [`cell_output`], over the notebook's own state. It is the seam the
//! shared output registry plugs into.

use std::ops::ControlFlow;

use gpui::App;
use sqlly_core::CellValue;
use sqlly_services::analytics::RowSource;
use sqlly_services::notebook::fingerprint::Fingerprint;
use sqlly_services::notebook::output::DEFAULT_OUTPUT_ROW_CAP;
use sqlly_services::notebook::{CellOutput, CellStatus, Completeness, Freshness, Provenance};

use crate::notebook::view::NotebookView;

/// An upstream output, plus its spill file when the result went to disk so
/// whole-result analyses can stream every row.
#[derive(Debug, Clone)]
pub struct InputData {
    pub output: CellOutput,
    /// `(spill file, rows in it)`.
    pub spill: Option<(String, usize)>,
}

/// The current output of `cell_id`, or `None` when it has none to read
/// (never ran, failed, or ran with a result set no longer held).
pub fn cell_output(view: &NotebookView, cell_id: &str, cx: &App) -> Option<InputData> {
    if let Some(output) = view.outputs.get(cell_id) {
        // The spill file holds previews of values the output has whole.
        let spill = view
            .cells
            .get(cell_id)
            .filter(|_| view.full_value_rows(cell_id).is_none())
            .and_then(|rt| rt.results.clone())
            .and_then(|r| r.read(cx).spill_scan_source(0));
        return Some(InputData {
            output: (*output).clone(),
            spill,
        });
    }
    if let Some(output) = view.analysis.outputs.get(cell_id) {
        return Some(InputData {
            output: output.clone(),
            spill: None,
        });
    }
    let status = view.doc.status(cell_id);
    if matches!(
        status,
        CellStatus::Idle | CellStatus::Failed | CellStatus::Blocked | CellStatus::Unavailable
    ) {
        return None;
    }
    let results = view.cells.get(cell_id)?.results.clone()?;
    let results = results.read(cx);
    let rows_complete = results.result_set_at(0)?.rows_complete();
    let spill = results.spill_scan_source(0);
    // Only the first rows are copied, read from the spill file when the
    // result went to disk (the rows in memory are then a window, not the
    // start of the result).
    let snapshot = crate::notebook::outputs::read_results(results, DEFAULT_OUTPUT_ROW_CAP)?;
    let mut output = CellOutput::capped(
        snapshot.columns,
        snapshot.rows,
        Some(snapshot.logical_rows),
        DEFAULT_OUTPUT_ROW_CAP,
        view.cells
            .get(cell_id)
            .and_then(|rt| rt.elapsed_ms)
            .unwrap_or(0),
        view.doc
            .cell_state(cell_id)
            .output
            .map(|o| o.run_id)
            .unwrap_or_default(),
        Fingerprint(view.doc.fingerprint_text(cell_id).unwrap_or_default()),
        Provenance::default(),
    );
    output.completeness = if status == CellStatus::Partial || status == CellStatus::Canceled {
        Completeness::Partial
    } else if snapshot.spilled && spill.is_some() {
        Completeness::Spilled {
            total: snapshot.logical_rows,
        }
    } else if !rows_complete && output.completeness == Completeness::Complete {
        Completeness::Truncated {
            cap: output.rows.len() as u64,
        }
    } else {
        output.completeness
    };
    output.freshness = Freshness::Live;
    Some(InputData { output, spill })
}

/// Every row of a spill file, paged from disk on each pass.
pub struct SpillRows {
    pub path: String,
    pub total: usize,
}

const SPILL_PAGE: usize = 4096;

impl RowSource for SpillRows {
    fn for_each_batch(&self, visit: &mut dyn FnMut(&[Vec<CellValue>]) -> ControlFlow<()>) {
        let Some(mut pager) = crate::spill_pager::SpillRowPager::open(&self.path) else {
            return;
        };
        let total = self.total.min(pager.row_count());
        let mut offset = 0;
        while offset < total {
            let page = pager.page(offset, SPILL_PAGE.min(total - offset));
            if page.is_empty() {
                break;
            }
            offset += page.len();
            if visit(&page).is_break() {
                break;
            }
        }
    }

    fn row_count_hint(&self) -> Option<u64> {
        Some(self.total as u64)
    }
}

/// An output built from rows an analysis cell produced itself.
pub fn derived_output(
    columns: Vec<sqlly_core::ResultColumn>,
    rows: Vec<Vec<CellValue>>,
    run_id: &str,
    elapsed_ms: u64,
    sources: Vec<Provenance>,
) -> CellOutput {
    CellOutput::capped(
        columns,
        rows,
        None,
        DEFAULT_OUTPUT_ROW_CAP,
        elapsed_ms,
        run_id,
        Fingerprint(String::new()),
        Provenance {
            connection_label: "notebook".into(),
            sources,
            ..Provenance::default()
        },
    )
}
