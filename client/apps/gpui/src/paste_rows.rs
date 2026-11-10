//! Clipboard text → typed rows for grid paste. Pure; no gpui.

use sqlly_core::CellValue;
use sqlly_lang::sqlmodel::DatabaseType;
use sqlly_services::row_editing::{RowEditColumn, RowEditSession};

use crate::row_editor_model::{coerce_text, field_kind_for, validate_text};

/// Detected separator of the pasted block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasteDelimiter {
    Tab,
    Comma,
}

/// Split clipboard text into rows of string cells. Tab wins when any line
/// contains a tab; otherwise comma with RFC 4180 double-quote handling.
/// Trailing empty line is dropped. Never errors — a single cell is one row.
pub fn parse_clipboard(text: &str) -> (PasteDelimiter, Vec<Vec<String>>) {
    let delimiter = if text.contains('\t') {
        PasteDelimiter::Tab
    } else {
        PasteDelimiter::Comma
    };
    let mut rows = match delimiter {
        PasteDelimiter::Tab => parse_tsv(text),
        PasteDelimiter::Comma => parse_csv(text),
    };
    if rows
        .last()
        .is_some_and(|row| row.len() == 1 && row[0].is_empty())
    {
        rows.pop();
    }
    if rows.is_empty() {
        rows.push(vec![String::new()]);
    }
    (delimiter, rows)
}

fn parse_tsv(text: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        rows.push(line.split('\t').map(|s| s.to_string()).collect());
    }
    rows
}

/// RFC 4180 CSV: commas split fields unless inside double quotes; `""` is a literal quote.
fn parse_csv(text: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut chars = text.chars().peekable();
    let mut in_quotes = false;
    while let Some(ch) = chars.next() {
        if in_quotes {
            if ch == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    field.push('"');
                } else {
                    in_quotes = false;
                }
            } else {
                field.push(ch);
            }
            continue;
        }
        match ch {
            '"' => in_quotes = true,
            ',' => {
                row.push(std::mem::take(&mut field));
            }
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            '\n' => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            _ => field.push(ch),
        }
    }
    row.push(field);
    rows.push(row);
    rows
}

/// One coercion failure, addressed by pasted-block coordinates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasteCellError {
    pub row: usize,
    pub column: String,
    pub text: String,
    pub reason: String,
}

/// A pasted block mapped onto table columns by position from the anchor.
#[derive(Debug, Clone, PartialEq)]
pub struct MappedRows {
    /// Column name → typed value for every pasted cell that mapped to a writable column.
    pub rows: Vec<Vec<(String, CellValue)>>,
    pub errors: Vec<PasteCellError>,
    /// Pasted columns beyond the last table column, dropped.
    pub dropped_columns: usize,
}

/// Fill a selected range from a smaller clipboard, as a spreadsheet does:
/// one pasted row repeats down every row of the range, and a single pasted
/// cell repeats across every column too. `range` is the selected
/// `(rows, columns)` of a contiguous range, or `None` when there is none
/// (a single cell or scattered cells), in which case nothing is filled. A
/// multi-row clipboard always pastes as itself.
pub fn fill_selection(cells: Vec<Vec<String>>, range: Option<(usize, usize)>) -> Vec<Vec<String>> {
    let Some((range_rows, range_cols)) = range else {
        return cells;
    };
    let [row] = cells.as_slice() else {
        return cells;
    };
    let row = match row.as_slice() {
        [single] if range_cols > 1 => vec![single.clone(); range_cols],
        _ => row.clone(),
    };
    vec![row; range_rows.max(1)]
}

/// Map pasted cells onto `session.columns` starting at `anchor_column`
/// (an index into `result_columns`, resolved through `RowEditColumn::result_ordinal`).
/// Rules: empty text → `CellValue::Null` for nullable columns, else error;
/// the literal `NULL` (case-insensitive) → Null; identity/computed/rowversion
/// columns are skipped (value dropped, no error); text is coerced with
/// `row_editor_model::field_kind_for` + `validate_text` + `FieldState::draft_cell`
/// semantics (integer/float/bool parse, everything else stays a string).
pub fn map_rows(
    session: &RowEditSession,
    db: DatabaseType,
    anchor_result_column: usize,
    cells: &[Vec<String>],
) -> MappedRows {
    let mut by_ordinal: Vec<(usize, &RowEditColumn)> = session
        .columns
        .iter()
        .filter_map(|col| col.result_ordinal.map(|ord| (ord, col)))
        .collect();
    by_ordinal.sort_by_key(|(ord, _)| *ord);
    let max_ordinal = by_ordinal.last().map(|(ord, _)| *ord);
    let mut dropped_columns = 0;
    let mut errors = Vec::new();
    let mut rows = Vec::with_capacity(cells.len());

    for (row_idx, pasted) in cells.iter().enumerate() {
        let mut mapped = Vec::new();
        for (offset, text) in pasted.iter().enumerate() {
            let result_col = anchor_result_column + offset;
            if max_ordinal.is_none_or(|max| result_col > max) {
                dropped_columns = dropped_columns.max(pasted.len() - offset);
                break;
            }
            let Some(column) = by_ordinal
                .iter()
                .find(|(ord, _)| *ord == result_col)
                .map(|(_, col)| *col)
            else {
                continue;
            };
            if let Some(pair) = coerce_cell(db, column, row_idx, text, &mut errors) {
                mapped.push(pair);
            }
        }
        rows.push(mapped);
    }

    MappedRows {
        rows,
        errors,
        dropped_columns,
    }
}

fn coerce_cell(
    db: DatabaseType,
    column: &RowEditColumn,
    row: usize,
    text: &str,
    errors: &mut Vec<PasteCellError>,
) -> Option<(String, CellValue)> {
    if column.is_identity || column.is_computed || column.is_rowversion {
        return None;
    }
    let trimmed = text.trim();
    if trimmed.is_empty() {
        if column.nullable {
            return Some((column.name.clone(), CellValue::Null));
        }
        errors.push(PasteCellError {
            row,
            column: column.name.clone(),
            text: text.to_string(),
            reason: crate::localization::text("results.paste_rows.value_required"),
        });
        return None;
    }
    if trimmed.eq_ignore_ascii_case("NULL") {
        return Some((column.name.clone(), CellValue::Null));
    }
    let kind = field_kind_for(db, &column.data_type);
    if let Some(reason) = validate_text(text, kind, column) {
        errors.push(PasteCellError {
            row,
            column: column.name.clone(),
            text: text.to_string(),
            reason,
        });
        return None;
    }
    Some((column.name.clone(), coerce_text(text, kind)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_services::row_editing::{RowEditColumn, TableTarget};

    fn sample_session() -> RowEditSession {
        RowEditSession {
            target: TableTarget {
                schema: "dbo".to_string(),
                table: "Items".to_string(),
            },
            key_columns: vec!["id".to_string()],
            rowversion_column: None,
            columns: vec![
                RowEditColumn {
                    name: "id".to_string(),
                    data_type: "int".to_string(),
                    char_max_length: None,
                    precision: None,
                    scale: None,
                    nullable: false,
                    is_primary_key: true,
                    is_identity: true,
                    is_computed: false,
                    is_rowversion: false,
                    default_definition: None,
                    result_ordinal: Some(0),

                    enum_values: Vec::new(),
                },
                RowEditColumn {
                    name: "name".to_string(),
                    data_type: "nvarchar".to_string(),
                    char_max_length: Some(100),
                    precision: None,
                    scale: None,
                    nullable: true,
                    is_primary_key: false,
                    is_identity: false,
                    is_computed: false,
                    is_rowversion: false,
                    default_definition: None,
                    result_ordinal: Some(1),

                    enum_values: Vec::new(),
                },
            ],
            original_row: vec![CellValue::I64(1), CellValue::String("old".to_string())],
            foreign_keys: Vec::new(),
            fk_display: Vec::new(),
            check_constraints: Vec::new(),
            incoming_foreign_keys: Vec::new(),
        }
    }

    fn cells(rows: &[&[&str]]) -> Vec<Vec<String>> {
        rows.iter()
            .map(|row| row.iter().map(|s| s.to_string()).collect())
            .collect()
    }

    #[test]
    fn fill_selection_repeats_one_row_down_a_taller_range() {
        let filled = fill_selection(cells(&[&["x", "y"]]), Some((3, 2)));
        assert_eq!(filled, cells(&[&["x", "y"], &["x", "y"], &["x", "y"]]));
    }

    #[test]
    fn fill_selection_repeats_one_cell_across_the_whole_range() {
        let filled = fill_selection(cells(&[&["7"]]), Some((2, 3)));
        assert_eq!(filled, cells(&[&["7", "7", "7"], &["7", "7", "7"]]));
    }

    #[test]
    fn fill_selection_leaves_blocks_and_unranged_pastes_alone() {
        // A multi-row block pastes as itself, whatever the range size.
        let block = cells(&[&["a"], &["b"]]);
        assert_eq!(fill_selection(block.clone(), Some((4, 1))), block);
        // No contiguous range (single cell, scattered cells): no fill.
        assert_eq!(fill_selection(cells(&[&["x"]]), None), cells(&[&["x"]]));
        // A range no bigger than the clipboard: no fill.
        assert_eq!(
            fill_selection(cells(&[&["x", "y"]]), Some((1, 2))),
            cells(&[&["x", "y"]])
        );
    }

    #[test]
    fn parse_clipboard_prefers_tab_over_comma() {
        let (delim, rows) = parse_clipboard("a,b\tc\nx,y\tz");
        assert_eq!(delim, PasteDelimiter::Tab);
        assert_eq!(
            rows,
            vec![
                vec!["a,b".to_string(), "c".to_string()],
                vec!["x,y".to_string(), "z".to_string()]
            ]
        );
    }

    #[test]
    fn parse_clipboard_handles_quoted_commas_and_crlf() {
        let (delim, rows) = parse_clipboard("\"hello, world\",2\r\n3,4\r\n");
        assert_eq!(delim, PasteDelimiter::Comma);
        assert_eq!(
            rows,
            vec![
                vec!["hello, world".to_string(), "2".to_string()],
                vec!["3".to_string(), "4".to_string()]
            ]
        );
    }

    #[test]
    fn map_rows_skips_identity_and_reports_type_errors() {
        let session = sample_session();
        let cells = vec![
            vec!["99".to_string(), "ok".to_string()],
            vec!["1".to_string(), "x".repeat(101)],
        ];
        let mapped = map_rows(&session, DatabaseType::SqlServer, 0, &cells);
        assert_eq!(
            mapped.rows[0],
            vec![("name".to_string(), CellValue::String("ok".into()))]
        );
        assert!(
            mapped
                .errors
                .iter()
                .any(|e| e.row == 1 && e.column == "name")
        );
        assert!(mapped.rows[1].is_empty() || mapped.rows[1].iter().all(|(n, _)| n != "id"));
        assert!(
            mapped
                .rows
                .iter()
                .all(|row| row.iter().all(|(n, _)| n != "id"))
        );
    }

    #[test]
    fn map_rows_maps_by_position_from_anchor_and_drops_overflow() {
        let session = sample_session();
        let cells = vec![vec![
            "alpha".to_string(),
            "dropped".to_string(),
            "also".to_string(),
        ]];
        let mapped = map_rows(&session, DatabaseType::SqlServer, 1, &cells);
        assert_eq!(
            mapped.rows,
            vec![vec![(
                "name".to_string(),
                CellValue::String("alpha".into())
            )]]
        );
        assert_eq!(mapped.dropped_columns, 2);
        assert!(mapped.errors.is_empty());
    }

    #[test]
    fn map_rows_null_literal_and_empty_nullable_become_null() {
        let session = sample_session();
        let cells = vec![
            vec!["NULL".to_string()],
            vec!["".to_string()],
            vec!["null".to_string()],
        ];
        let mapped = map_rows(&session, DatabaseType::SqlServer, 1, &cells);
        assert_eq!(
            mapped.rows,
            vec![
                vec![("name".to_string(), CellValue::Null)],
                vec![("name".to_string(), CellValue::Null)],
                vec![("name".to_string(), CellValue::Null)],
            ]
        );
        assert!(mapped.errors.is_empty());
    }
}
