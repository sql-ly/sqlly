//! What the Cell Details dock shows for a grid selection, computed from the
//! result set and the datatable selection without any UI types so it can be
//! unit-tested.

use sqlly_core::{CellValue, ResultColumn};
use sqlly_datatable::Selection;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FocusedCell {
    pub display_row: usize,
    pub source_row: usize,
    pub column: usize,
}

/// The single "focus" cell for any selection shape: the cell itself, the
/// top-left of a range, the first cell of a row/column selection.
pub fn focused_cell(selection: &Selection, display_indices: &[usize]) -> Option<FocusedCell> {
    let (display_row, column) = match selection {
        Selection::None => return None,
        Selection::Cell(row, col) => (*row, *col),
        Selection::Row(row) => (*row, 0),
        Selection::Column(col) => (0, *col),
        Selection::Columns(cols) => (0, *cols.first()?),
        Selection::CellRange(r1, c1, _, _) => (*r1, *c1),
        Selection::RowRange(r1, _) => (*r1, 0),
        Selection::Rows(rows) => (*rows.first()?, 0),
        Selection::Cells(cells) => {
            let (row, col) = *cells.first()?;
            (row, col)
        }
    };
    let source_row = map_display_row(display_row, display_indices);
    Some(FocusedCell {
        display_row,
        source_row,
        column,
    })
}

fn map_display_row(display_row: usize, display_indices: &[usize]) -> usize {
    if display_indices.is_empty() {
        display_row
    } else {
        display_indices
            .get(display_row)
            .copied()
            .unwrap_or(display_row)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RowField {
    pub name: String,
    pub data_type: String,
    pub value: String,
    pub is_null: bool,
    pub is_focused: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CellDetail {
    pub column_name: String,
    pub data_type: String,
    pub value: CellValue,
    pub fields: Vec<RowField>,
    pub row_label: String,
}

pub fn build_detail(
    columns: &[ResultColumn],
    row: &[CellValue],
    focus: FocusedCell,
    total_rows: usize,
) -> Option<CellDetail> {
    let column = columns.get(focus.column)?;
    let value = row.get(focus.column)?.clone();
    let fields = columns
        .iter()
        .enumerate()
        .map(|(i, col)| {
            let cell = row.get(i);
            let is_null = matches!(cell, Some(CellValue::Null) | None);
            RowField {
                name: col.name.clone(),
                data_type: col.data_type.clone(),
                value: cell.map(display_text).unwrap_or_else(|| "NULL".to_string()),
                is_null,
                is_focused: i == focus.column,
            }
        })
        .collect();
    Some(CellDetail {
        column_name: column.name.clone(),
        data_type: column.data_type.clone(),
        value,
        fields,
        row_label: crate::localization::format(
            "results.cell_details.row_position",
            &[
                (
                    "row_number",
                    &focus.display_row.saturating_add(1).to_string(),
                ),
                ("total_rows", &total_rows.to_string()),
            ],
        ),
    })
}

/// Text for the plain view and the form: Strings verbatim, numbers via Display,
/// NULL as "NULL", bytes as "<N bytes>" (the full hex lives in the hex view).
pub fn display_text(value: &CellValue) -> String {
    match value {
        CellValue::Null => "NULL".to_string(),
        CellValue::Bool(b) => b.to_string(),
        CellValue::I64(n) => n.to_string(),
        CellValue::F64(n) => n.to_string(),
        CellValue::String(s) => s.clone(),
        CellValue::Bytes(b) => crate::localization::format(
            "results.cell_details.byte_count",
            &[("byte_count", &b.len().to_string())],
        ),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DockSide {
    Right,
    Bottom,
}

/// Whether the panel may edit its cell, and if not, why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CellEditGate {
    Editable,
    /// The cell shows a truncated preview; editing waits for the full value.
    AwaitingFullValue,
    ReadOnly(String),
}

/// What the results view knows about the focused cell when deciding whether
/// the panel may edit it. The authoritative checks (a key, provenance,
/// identity/computed table columns, nullability, type) run when the edit is
/// staged, exactly as for the grid's other cell edits.
#[derive(Debug, Clone, Default)]
pub struct EditGateInputs {
    /// The result's rows cannot be edited at all (history, connection
    /// policy, not a single-table SELECT, …): the row editor's reason.
    pub row_edit_block: Option<String>,
    /// The cell belongs to a staged (unsaved) inserted row.
    pub staged_insert: bool,
    /// A `-- sqlly column:` formula column, not stored in the table.
    pub computed: bool,
    pub masked: bool,
    pub binary: bool,
    /// The cell already has a staged expression (DEFAULT, server time, …).
    pub staged_expression: bool,
    /// The panel holds a truncated preview, not the full value.
    pub truncated: bool,
}

pub fn edit_gate(inputs: EditGateInputs) -> CellEditGate {
    let read_only = |key: &str| CellEditGate::ReadOnly(crate::localization::text(key));
    if let Some(reason) = inputs.row_edit_block {
        return CellEditGate::ReadOnly(reason);
    }
    if inputs.staged_insert {
        return read_only("results.cell_details.read_only_staged_insert");
    }
    if inputs.computed {
        return read_only("results.cell_details.read_only_computed");
    }
    if inputs.masked {
        return read_only("results.cell_details.read_only_masked");
    }
    if inputs.binary {
        return read_only("results.cell_details.read_only_binary");
    }
    if inputs.staged_expression {
        return read_only("results.cell_details.read_only_staged_expression");
    }
    if inputs.truncated {
        return CellEditGate::AwaitingFullValue;
    }
    CellEditGate::Editable
}

/// An edit committed from the panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PanelEdit {
    /// Literal text; an empty string stays an empty string, never NULL.
    Text(String),
    Null,
}

/// The editor's starting text for `value`: NULL starts empty (Set NULL is
/// the explicit way back), binary values are not text-editable here.
pub fn edit_seed_text(value: &CellValue) -> Option<String> {
    match value {
        CellValue::Null => Some(String::new()),
        CellValue::Bytes(_) => None,
        other => Some(display_text(other)),
    }
}

/// The edit to stage when the user applies `draft` over `original`, or
/// `None` when nothing changed. Applying over a NULL always stages text
/// (an empty draft stages the empty string).
pub fn edit_for_apply(original: &CellValue, draft: &str) -> Option<PanelEdit> {
    match original {
        CellValue::Null => Some(PanelEdit::Text(draft.to_string())),
        other if edit_seed_text(other).as_deref() == Some(draft) => None,
        _ => Some(PanelEdit::Text(draft.to_string())),
    }
}

/// A non-blocking warning for text applied to a JSON-typed column that does
/// not parse as JSON. The edit is still staged; the server stays the
/// authority.
pub fn json_warning(data_type: &str, text: &str) -> Option<String> {
    let json_typed = matches!(data_type.to_ascii_lowercase().as_str(), "json" | "jsonb");
    if !json_typed {
        return None;
    }
    let error = serde_json::from_str::<serde_json::Value>(text).err()?;
    Some(crate::localization::format(
        "results.cell_details.invalid_json_warning",
        &[("error", &error.to_string())],
    ))
}

/// True when the dock should rebuild for a new `(slot, focus)` pair.
pub fn focus_changed(
    last: Option<(usize, FocusedCell)>,
    next: Option<(usize, FocusedCell)>,
) -> bool {
    last != next
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, data_type: &str) -> ResultColumn {
        ResultColumn {
            name: name.into(),
            data_type: data_type.into(),
            nullable: true,
        }
    }

    #[test]
    fn focused_cell_for_every_selection_shape() {
        let empty: &[usize] = &[];
        assert_eq!(
            focused_cell(&Selection::Cell(3, 2), empty),
            Some(FocusedCell {
                display_row: 3,
                source_row: 3,
                column: 2,
            })
        );
        assert_eq!(
            focused_cell(&Selection::Row(4), empty),
            Some(FocusedCell {
                display_row: 4,
                source_row: 4,
                column: 0,
            })
        );
        assert_eq!(
            focused_cell(&Selection::Column(5), empty),
            Some(FocusedCell {
                display_row: 0,
                source_row: 0,
                column: 5,
            })
        );
        assert_eq!(
            focused_cell(&Selection::Columns(vec![2, 7]), empty),
            Some(FocusedCell {
                display_row: 0,
                source_row: 0,
                column: 2,
            })
        );
        assert_eq!(
            focused_cell(&Selection::CellRange(1, 3, 8, 9), empty),
            Some(FocusedCell {
                display_row: 1,
                source_row: 1,
                column: 3,
            })
        );
        assert_eq!(
            focused_cell(&Selection::RowRange(6, 10), empty),
            Some(FocusedCell {
                display_row: 6,
                source_row: 6,
                column: 0,
            })
        );
        assert_eq!(
            focused_cell(&Selection::Rows(vec![9, 11]), empty),
            Some(FocusedCell {
                display_row: 9,
                source_row: 9,
                column: 0,
            })
        );
        assert_eq!(
            focused_cell(&Selection::Cells(vec![(2, 4), (3, 1)]), empty),
            Some(FocusedCell {
                display_row: 2,
                source_row: 2,
                column: 4,
            })
        );
        assert_eq!(focused_cell(&Selection::None, empty), None);
    }

    #[test]
    fn focused_cell_maps_display_to_source_when_sorted() {
        let indices = vec![4, 1, 0];
        assert_eq!(
            focused_cell(&Selection::Cell(0, 1), &indices),
            Some(FocusedCell {
                display_row: 0,
                source_row: 4,
                column: 1,
            })
        );
        assert_eq!(
            focused_cell(&Selection::Cell(2, 0), &indices),
            Some(FocusedCell {
                display_row: 2,
                source_row: 0,
                column: 0,
            })
        );
    }

    #[test]
    fn build_detail_marks_focused_field_and_labels_nulls() {
        let columns = vec![col("id", "int"), col("name", "text")];
        let row = vec![CellValue::I64(7), CellValue::Null];
        let focus = FocusedCell {
            display_row: 11,
            source_row: 11,
            column: 1,
        };
        let detail = build_detail(&columns, &row, focus, 500).unwrap();
        assert_eq!(detail.column_name, "name");
        assert_eq!(detail.data_type, "text");
        assert_eq!(detail.value, CellValue::Null);
        assert_eq!(detail.row_label, "Row 12 of 500");
        assert!(!detail.fields[0].is_focused);
        assert!(detail.fields[1].is_focused);
        assert!(detail.fields[1].is_null);
        assert_eq!(detail.fields[1].value, "NULL");
        assert_eq!(detail.fields[0].value, "7");
    }

    #[test]
    fn display_text_renders_null_bytes_and_numbers() {
        assert_eq!(display_text(&CellValue::Null), "NULL");
        assert_eq!(display_text(&CellValue::Bytes(vec![1, 2, 3])), "<3 bytes>");
        assert_eq!(display_text(&CellValue::I64(-4)), "-4");
        assert_eq!(display_text(&CellValue::F64(1.5)), "1.5");
        assert_eq!(display_text(&CellValue::String("hi".into())), "hi");
        assert_eq!(display_text(&CellValue::Bool(true)), "true");
    }

    #[test]
    fn build_detail_returns_none_for_out_of_range_column() {
        let columns = vec![col("id", "int")];
        let row = vec![CellValue::I64(1)];
        let focus = FocusedCell {
            display_row: 0,
            source_row: 0,
            column: 3,
        };
        assert!(build_detail(&columns, &row, focus, 1).is_none());
    }

    #[test]
    fn focus_changed_detects_slot_or_cell() {
        let a = FocusedCell {
            display_row: 0,
            source_row: 0,
            column: 1,
        };
        let b = FocusedCell {
            display_row: 1,
            source_row: 1,
            column: 1,
        };
        assert!(!focus_changed(Some((0, a)), Some((0, a))));
        assert!(focus_changed(Some((0, a)), Some((0, b))));
        assert!(focus_changed(Some((0, a)), Some((1, a))));
        assert!(focus_changed(None, Some((0, a))));
    }

    #[test]
    fn null_and_empty_string_stay_distinct_when_applying() {
        // Over NULL, an empty draft stages the empty string, not "no change".
        assert_eq!(
            edit_for_apply(&CellValue::Null, ""),
            Some(PanelEdit::Text(String::new()))
        );
        // Over an empty string, an empty draft changes nothing.
        assert_eq!(edit_for_apply(&CellValue::String(String::new()), ""), None);
        // The text "NULL" is text.
        assert_eq!(
            edit_for_apply(&CellValue::String("x".into()), "NULL"),
            Some(PanelEdit::Text("NULL".into()))
        );
        assert_eq!(edit_for_apply(&CellValue::I64(5), "5"), None);
        assert_eq!(edit_seed_text(&CellValue::Null).as_deref(), Some(""));
        assert_eq!(edit_seed_text(&CellValue::Bytes(vec![1])), None);
    }

    #[test]
    fn edit_gate_reports_the_first_reason() {
        assert_eq!(edit_gate(EditGateInputs::default()), CellEditGate::Editable);
        assert_eq!(
            edit_gate(EditGateInputs {
                truncated: true,
                ..Default::default()
            }),
            CellEditGate::AwaitingFullValue
        );
        assert_eq!(
            edit_gate(EditGateInputs {
                row_edit_block: Some("policy".into()),
                binary: true,
                ..Default::default()
            }),
            CellEditGate::ReadOnly("policy".into())
        );
        assert!(matches!(
            edit_gate(EditGateInputs {
                binary: true,
                truncated: true,
                ..Default::default()
            }),
            CellEditGate::ReadOnly(_)
        ));
    }

    #[test]
    fn json_warning_only_for_invalid_json_in_json_columns() {
        assert!(json_warning("jsonb", "{\"a\": 1}").is_none());
        assert!(json_warning("JSON", "{oops").is_some());
        assert!(json_warning("text", "{oops").is_none());
    }
}
