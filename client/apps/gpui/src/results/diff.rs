//! Result-set difference tracking types and the detailed-diff table,
//! split from the flat `results` module.

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DiffDirection {
    #[default]
    InPrevious,
    InCurrent,
}

#[derive(Clone)]
pub(super) struct RetainedDiffSet {
    pub(super) columns: Vec<ResultColumn>,
    pub(super) rows: Option<Vec<Vec<CellValue>>>,
    pub(super) row_count: usize,
    pub(super) unavailable_reason: Option<String>,
}

#[derive(Clone)]
pub(super) struct RetainedDiffRun {
    pub(super) id: u64,
    pub(super) label: String,
    pub(super) sets: Vec<RetainedDiffSet>,
}

/// A named snapshot loaded as the diff baseline.
pub(super) struct DiffSnapshotBaseline {
    pub(super) name: String,
    pub(super) columns: Vec<ResultColumn>,
    pub(super) rows: Vec<Vec<CellValue>>,
}

/// The classification table the diff view shows and exports: one row per
/// added/removed row and per changed cell, optionally filtered to one class.
pub(crate) fn detailed_diff_table(
    diff: &result_set_difference::DetailedDiff,
    class_filter: Option<&str>,
) -> (Vec<ResultColumn>, Vec<Vec<CellValue>>) {
    let columns = [
        "results.diff.classification_column",
        "results.diff.key_column",
        "results.diff.column_name_column",
        "results.diff.before_column",
        "results.diff.after_column",
    ]
    .into_iter()
    .map(|key| ResultColumn {
        name: crate::localization::text(key),
        data_type: "text".to_string(),
        nullable: true,
    })
    .collect::<Vec<_>>();
    let include = |class: &str| class_filter.is_none_or(|filter| filter == class);
    let row_text = |row: &[CellValue]| {
        row.iter()
            .map(typed_cell_text)
            .collect::<Vec<_>>()
            .join(" | ")
    };
    let key_text = |row: &[CellValue]| {
        if diff.key_columns.is_empty() {
            crate::localization::text("results.diff.typed_row_match")
        } else {
            diff.key_columns
                .iter()
                .filter_map(|index| row.get(*index))
                .map(typed_cell_text)
                .collect::<Vec<_>>()
                .join(" | ")
        }
    };
    let mut rows = Vec::new();
    if include("Added") {
        for row in &diff.added {
            rows.push(vec![
                CellValue::String(crate::localization::text("results.diff.added_value")),
                CellValue::String(key_text(row)),
                CellValue::String(crate::localization::text("results.diff.whole_row_value")),
                CellValue::Null,
                CellValue::String(row_text(row)),
            ]);
        }
    }
    if include("Removed") {
        for row in &diff.removed {
            rows.push(vec![
                CellValue::String(crate::localization::text("results.diff.removed_value")),
                CellValue::String(key_text(row)),
                CellValue::String(crate::localization::text("results.diff.whole_row_value")),
                CellValue::String(row_text(row)),
                CellValue::Null,
            ]);
        }
    }
    if include("Changed") {
        for changed in &diff.changed {
            let key = changed
                .key
                .iter()
                .map(typed_cell_text)
                .collect::<Vec<_>>()
                .join(" | ");
            for change in &changed.changes {
                rows.push(vec![
                    CellValue::String(crate::localization::text("results.diff.changed_value")),
                    CellValue::String(key.clone()),
                    CellValue::String(change.column_name.clone()),
                    CellValue::String(typed_cell_text(&change.before)),
                    CellValue::String(typed_cell_text(&change.after)),
                ]);
            }
        }
    }
    (columns, rows)
}
