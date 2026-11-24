//! Values the results grid cut to its 8 KiB preview, read again whole for
//! the notebook's output registry.
//!
//! The grid of a SQL cell holds only the start of long text and binary
//! values. Cells that read its output (tests, diffs, profiles, formulas,
//! related rows, the run history) need the values themselves, so when a
//! finished result holds cut values in the rows its output keeps, the
//! cell's query runs once more, read-only, through the host's full-values
//! path before the output is published. That read is used only when it
//! returns the rows the grid shows. When it can't be used (the cell is not
//! a read-only query, the values are too large to hold, or the rows came
//! back different) the output keeps the previews and the activity log says
//! so.

use std::collections::HashMap;
use std::sync::Arc;

use gpui::Context;
use sqlly_core::CellValue;
use sqlly_intel::query::safety::classify_select_only_for;
use sqlly_lang::sqlmodel::DatabaseType;

use super::run::Pending;
use super::{AnalysisReply, AnalysisWork};
use crate::notebook::view::NotebookView;

/// Most bytes of cut values one cell's output reads again whole.
pub(crate) const FULL_VALUES_BUDGET: u64 = 64 * 1024 * 1024;

type WholeRows = Arc<Vec<Vec<CellValue>>>;

/// Per SQL cell: the run whose cut values were read again, and the whole
/// rows when that read could be used.
#[derive(Default)]
pub(crate) struct FullValues {
    by_cell: HashMap<String, (String, Option<WholeRows>)>,
}

/// Whether `full` is `preview` with its cut values whole: the same rows,
/// each value equal or (text and binary) starting with the preview.
pub(crate) fn same_rows(preview: &[Vec<CellValue>], full: &[Vec<CellValue>]) -> bool {
    preview.len() == full.len()
        && preview.iter().zip(full).all(|(p, f)| {
            p.len() == f.len()
                && p.iter().zip(f).all(|(p, f)| match (p, f) {
                    (CellValue::String(p), CellValue::String(f)) => f.starts_with(p.as_str()),
                    (CellValue::Bytes(p), CellValue::Bytes(f)) => f.starts_with(p),
                    (p, f) => p == f,
                })
        })
}

impl NotebookView {
    /// Read `id`'s cut values again when its result holds any in the first
    /// `cap` rows. True while that read is in flight: the cell finishes when
    /// the host answers.
    pub(crate) fn read_full_values(
        &mut self,
        id: &str,
        cap: usize,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(run_id) = self.cells.get(id).and_then(|rt| rt.run_id.clone()) else {
            return false;
        };
        if self
            .analysis
            .full_values
            .by_cell
            .get(id)
            .is_some_and(|(run, _)| *run == run_id)
        {
            return false;
        }
        let cut_bytes = {
            let Some(results) = self.cells.get(id).and_then(|rt| rt.results.clone()) else {
                return false;
            };
            let results = results.read(cx);
            let Some(set) = results.result_set_at(0) else {
                return false;
            };
            let cut: Vec<u64> = set
                .truncated
                .values()
                .filter(|c| c.row < cap)
                .map(|c| c.total_len)
                .collect();
            if cut.is_empty() {
                return false;
            }
            cut.iter().sum::<u64>()
        };
        self.analysis
            .full_values
            .by_cell
            .insert(id.to_string(), (run_id.clone(), None));
        let request = self.run_request(id).ok().filter(|request| {
            let db = if request.target.local {
                DatabaseType::DuckDb
            } else {
                self.doc.database_type_for(id)
            };
            classify_select_only_for(db, &request.sql).select_only
        });
        let Some(request) = request.filter(|_| cut_bytes <= FULL_VALUES_BUDGET) else {
            self.kept_previews(id);
            return false;
        };
        let mut target = request.target;
        target.read_only = true;
        self.send_request(
            id,
            target,
            AnalysisWork::Rows {
                queries: vec![request.sql],
                max_rows: cap,
            },
            Pending::FullValues { run_id },
            cx,
        );
        true
    }

    /// The host's answer to [`Self::read_full_values`]: keep the whole rows
    /// when they are the grid's rows, then finish the cell.
    pub(super) fn full_values_read(
        &mut self,
        id: &str,
        run_id: String,
        reply: AnalysisReply,
        cx: &mut Context<Self>,
    ) {
        if self.cells.get(id).and_then(|rt| rt.run_id.as_deref()) != Some(run_id.as_str()) {
            return;
        }
        let preview = self
            .cells
            .get(id)
            .and_then(|rt| rt.results.clone())
            .and_then(|r| crate::notebook::outputs::read_results(r.read(cx), self.output_cap(id)));
        let full = match (reply, preview) {
            (AnalysisReply::Rows(mut sets), Some(preview)) => match sets.pop() {
                Some(Ok(read)) if same_rows(&preview.rows, &read.rows) => Some(read.rows),
                _ => None,
            },
            _ => None,
        };
        match full {
            Some(rows) => {
                self.analysis
                    .full_values
                    .by_cell
                    .insert(id.to_string(), (run_id, Some(Arc::new(rows))));
            }
            None => self.kept_previews(id),
        }
        self.finish_sql_cell(id, cx);
    }

    /// `id`'s rows with their cut values whole, for its current run.
    pub(crate) fn full_value_rows(&self, id: &str) -> Option<WholeRows> {
        let run_id = self.cells.get(id)?.run_id.as_deref()?;
        match self.analysis.full_values.by_cell.get(id)? {
            (run, Some(rows)) if run == run_id => Some(rows.clone()),
            _ => None,
        }
    }

    fn kept_previews(&mut self, id: &str) {
        self.log(
            Some(id),
            crate::localization::text("notebook.fix_plan.kept_previews"),
            false,
            None,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_values_match_their_previews_and_nothing_else() {
        let preview = vec![vec![
            CellValue::I64(1),
            CellValue::String("abc".into()),
            CellValue::Bytes(vec![1, 2]),
        ]];
        let full = vec![vec![
            CellValue::I64(1),
            CellValue::String("abcdef".into()),
            CellValue::Bytes(vec![1, 2, 3]),
        ]];
        assert!(same_rows(&preview, &full));
        assert!(same_rows(&full, &full));
        let other_row = vec![vec![
            CellValue::I64(2),
            CellValue::String("abcdef".into()),
            CellValue::Bytes(vec![1, 2, 3]),
        ]];
        assert!(!same_rows(&preview, &other_row));
        let other_text = vec![vec![
            CellValue::I64(1),
            CellValue::String("xbcdef".into()),
            CellValue::Bytes(vec![1, 2, 3]),
        ]];
        assert!(!same_rows(&preview, &other_text));
        assert!(!same_rows(&preview, &[]));
        assert!(!same_rows(&preview, &[full[0].clone(), full[0].clone()]));
    }
}
