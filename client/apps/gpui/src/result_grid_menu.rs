//! Context-menu provider for the result grid.
//!
//! Implements `sqlly_datatable::ContextMenuProvider` to supply the
//! result-grid right-click menu: export, copy, select-all, structured-data
//! detection, and WHERE-clause actions. Bridges between the grid's
//! `GridState` and the app's `ResultsView` / `SqlEditor` via weak entity
//! handles.

use gpui::{App, ClipboardItem, WeakEntity};
use sqlly_core::{CellValue, ResultColumn};
use sqlly_datatable::{
    ContextMenuItem, ContextMenuProvider, ContextMenuRequest, ContextMenuTarget, GridState,
    SelectedCellContext, Selection,
};

use crate::editor::SqlEditor;
use crate::preferences::{CopyFormat, ExportPreferences, MaskFormat};
use crate::result_export_dialog::ExportPayload;
use crate::result_profiler::{PROFILE_MAX_ROWS, ResultProfiler};
use crate::results::{ExportFormat, FkLabelHeaderMenuInfo, ResultSet, ResultsView};
use crate::structured_data;
use crate::where_actions::{
    self, WhereAction, WhereActionMode, WhereLogicalJoin, WhereOperator, WhereSelectedValue,
};

mod redis_keys;
pub(crate) use redis_keys::SelectedKeysAction;

/// Action IDs used by the provider. Kept as constants so both `menu_items`
/// and `on_action` reference the same strings.
pub(crate) mod action_id {
    pub const SELECT_ALL: &str = "select_all";
    pub const COPY_SELECTION: &str = "copy_selection";
    pub const COPY_SELECTION_WITH_HEADERS: &str = "copy_selection_with_headers";
    pub const COPY_HEADERS: &str = "copy_headers";
    pub const COPY_AS_TSV: &str = "copy_as_tsv";
    pub const COPY_AS_CSV: &str = "copy_as_csv";
    pub const COPY_AS_JSON: &str = "copy_as_json";
    pub const COPY_AS_JSON_LINES: &str = "copy_as_json_lines";
    pub const COPY_AS_MARKDOWN: &str = "copy_as_markdown";
    pub const COPY_AS_INSERT: &str = "copy_as_insert";
    pub const COPY_SQL_IN_LIST: &str = "copy_sql_in_list";
    pub const COPY_SQL_IN_LIST_DISTINCT: &str = "copy_sql_in_list_distinct";
    pub const COPY_SQL_UPDATE: &str = "copy_sql_update";

    /// Opens the consolidated export dialog; individual per-format export
    /// IDs are gone — the dialog owns format selection now.
    pub const EXPORT_DIALOG: &str = "export_results_dialog";
    /// Temp-file export of the selection (or whole set) opened in the OS
    /// default spreadsheet app.
    pub const OPEN_IN_SPREADSHEET: &str = "open_in_spreadsheet";

    pub const STRUCTURED_DATA_PRETTY: &str = "structured_data_pretty";
    pub const STRUCTURED_DATA_COPY: &str = "structured_data_copy";

    pub const FIND_IN_GRID: &str = "find_in_grid";
    pub const HIDE_EMPTY_COLUMNS: &str = "hide_empty_columns";
    pub const GOTO_COLUMN: &str = "goto_column";
    pub const FILTER_BUILDER: &str = "filter_builder";
    pub const QUICK_FILTER_EQ: &str = "quick_filter_eq";
    pub const QUICK_FILTER_NE: &str = "quick_filter_ne";
    pub const QUICK_FILTER_CONTAINS: &str = "quick_filter_contains";
    pub const QUICK_FILTER_IS_NULL: &str = "quick_filter_is_null";
    pub const QUICK_FILTER_IS_NOT_NULL: &str = "quick_filter_is_not_null";
    pub const COPY_DISTINCT_VALUES: &str = "copy_distinct_values";
    pub const COPY_TOP_DISTINCT_VALUES: &str = "copy_top_distinct_values";
    pub const TOP_VALUES: &str = "top_values";
    pub const CLIENT_STATISTICS: &str = "client_statistics";
    pub const TRANSPOSE: &str = "transpose";
    pub const FK_OPEN_REFERENCED: &str = "fk_open_referenced";
    pub const FK_LABEL_TOGGLE: &str = "fk_label_toggle";
    pub const FK_LABEL_COLUMN_PREFIX: &str = "fk_label_column_";
    pub const FK_LABEL_COLUMN_OFF: &str = "fk_label_column_Off";

    pub fn fk_label_column(name: &str) -> String {
        format!("{FK_LABEL_COLUMN_PREFIX}{name}")
    }

    pub fn parse_fk_label_column(action_id: &str) -> Option<&str> {
        action_id.strip_prefix(FK_LABEL_COLUMN_PREFIX)
    }
    pub const VIEW_AS_HEX: &str = "view_as_hex";
    /// Show the Cell Details panel on the clicked cell.
    pub const CELL_DETAILS: &str = "cell_details";
    pub const VIEW_FULL_VALUE: &str = "view_full_value";
    pub const VIEW_IMAGE: &str = "view_image";
    pub const VIEW_GEOMETRY: &str = "view_geometry";

    pub const CELL_SET_NULL: &str = "cell_set_null";
    pub const CELL_SET_DEFAULT: &str = "cell_set_default";
    pub const CELL_SERVER_TIME: &str = "cell_server_time";
    pub const CELL_SET_TRUE: &str = "cell_set_true";
    pub const CELL_SET_FALSE: &str = "cell_set_false";
    pub const CELL_LOAD_FILE: &str = "cell_load_file";
    pub const CELL_SAVE_FILE: &str = "cell_save_file";
    pub const CELL_NEW_UUID: &str = "cell_new_uuid";
    pub const CELL_LOCAL_TIMESTAMP: &str = "cell_local_timestamp";
    pub const CELL_SQL_SERVER_DATE: &str = "cell_sql_server_date";
    pub const CELL_SQL_SERVER_UUID: &str = "cell_sql_server_uuid";
    pub const CELL_SQL_CURRENT_USER: &str = "cell_sql_current_user";

    pub const WHERE_EQUALS: &str = "where_equals";
    pub const WHERE_NOT_EQUALS: &str = "where_not_equals";
    pub const WHERE_GREATER_THAN: &str = "where_greater_than";
    pub const WHERE_GREATER_THAN_OR_EQUAL: &str = "where_greater_than_or_equal";
    pub const WHERE_LESS_THAN: &str = "where_less_than";
    pub const WHERE_LESS_THAN_OR_EQUAL: &str = "where_less_than_or_equal";
    pub const WHERE_IN: &str = "where_in";
    pub const WHERE_NOT_IN: &str = "where_not_in";
    pub const WHERE_LIKE: &str = "where_like";
    pub const WHERE_NOT_LIKE: &str = "where_not_like";
    pub const WHERE_STARTS_WITH: &str = "where_starts_with";
    pub const WHERE_ENDS_WITH: &str = "where_ends_with";
    pub const WHERE_IS_NULL: &str = "where_is_null";
    pub const WHERE_IS_NOT_NULL: &str = "where_is_not_null";
    pub const WHERE_MATCH_ROW: &str = "where_match_row";
    pub const WHERE_BETWEEN: &str = "where_between";
    pub const WHERE_FILTERS_TO_WHERE: &str = "where_filters_to_where";

    pub const LAYOUT_STACKED: &str = "layout_stacked";
    pub const LAYOUT_TABBED: &str = "layout_tabbed";
    pub const LAYOUT_CANVAS: &str = "layout_canvas";
    pub const RESET_COLUMN_LAYOUT: &str = "reset_column_layout";
    pub const WIDEN_COLUMN: &str = "widen_column";
    pub const NARROW_COLUMN: &str = "narrow_column";
    pub const SORT_ADD: &str = "sort_add";
    pub const SORT_DESC: &str = "sort_desc";
    pub const SORT_REMOVE: &str = "sort_remove";

    pub const ROUNDING_0: &str = "rounding_0";
    pub const ROUNDING_1: &str = "rounding_1";
    pub const ROUNDING_2: &str = "rounding_2";
    pub const ROUNDING_4: &str = "rounding_4";
    pub const ROUNDING_6: &str = "rounding_6";
    pub const FORMAT_COLUMN: &str = "format_column";
    pub const ALIGN_LEFT: &str = "align_left";
    pub const ALIGN_CENTER: &str = "align_center";
    pub const ALIGN_RIGHT: &str = "align_right";
    pub const MASK_REVEAL_COLUMN: &str = "mask_reveal_column";
    pub const MASK_REVEAL_CELL: &str = "mask_reveal_cell";
    pub const MASK_REMASK: &str = "mask_remask";
    pub const MASK_MANAGE_RULES: &str = "mask_manage_rules";
    pub const MASK_EXCLUDE_ON_CONNECTION: &str = "mask_exclude_on_connection";
    pub const MASK_INCLUDE_ON_CONNECTION: &str = "mask_include_on_connection";

    pub const EDIT_ROW: &str = "edit_row";
    pub const DUPLICATE_ROW: &str = "duplicate_row";
    pub const INSERT_ROW: &str = "insert_row";
    pub const STAGE_EDIT_ROW: &str = "stage_edit_row";
    pub const STAGE_DUPLICATE_ROW: &str = "stage_duplicate_row";
    pub const STAGE_DUPLICATE_ROW_NO_KEY: &str = "stage_duplicate_row_no_key";
    pub const STAGE_INSERT_ROW: &str = "stage_insert_row";
    pub const BATCH_INSERT: &str = "batch_insert";
    pub const STAGE_DELETE_ROW: &str = "stage_delete_row";
    pub const UNSTAGE_ROW: &str = "unstage_row";

    pub const CHART_VIEW: &str = "chart_view";
    pub const SHOW_ON_MAP: &str = "show_on_map";
    pub const SAVE_CHART_TO_LIBRARY: &str = "save_chart_to_library";
    pub const VIEW_ROW_FORM: &str = "view_row_form";
    pub const CONDITIONAL_FORMAT: &str = "conditional_format";
    pub const ADD_COMPUTED_COLUMN: &str = "add_computed_column";
    pub const EDIT_FORMULA: &str = "edit_formula";
    pub const SAVE_SNAPSHOT: &str = "save_snapshot";
    pub const PROFILE_DATA: &str = "profile_data";
    pub const PROFILE_COPY_MARKDOWN: &str = "profile_copy_markdown";
    pub const PROFILE_ALL: &str = "profile_all";
}

/// Scope export data to the grid selection. Rows are display-ordered; the
/// selection stores display indices, so both sides line up.
///
/// - `None` or a single cursor cell → the whole set (columns as-is).
/// - Whole rows (`Row`/`RowRange`/`Rows`) → those rows, all columns.
/// - Whole columns (`Column`/`Columns`) → those columns, all rows.
/// - `CellRange` → the rectangle's rows and columns.
/// - `Cells` (non-contiguous) → unique selected rows × unique selected
///   columns; positions not actually selected export as NULL.
pub(crate) fn selection_export_data(
    columns: &[ResultColumn],
    rows: Vec<Vec<CellValue>>,
    selection: &sqlly_datatable::Selection,
) -> (Vec<ResultColumn>, Vec<Vec<CellValue>>) {
    use sqlly_datatable::Selection;

    let pick_rows = |rows: Vec<Vec<CellValue>>, keep: &[usize]| -> Vec<Vec<CellValue>> {
        keep.iter().filter_map(|&r| rows.get(r).cloned()).collect()
    };
    let pick_cols =
        |rows: Vec<Vec<CellValue>>, keep: &[usize]| -> (Vec<ResultColumn>, Vec<Vec<CellValue>>) {
            let cols: Vec<ResultColumn> = keep
                .iter()
                .filter_map(|&c| columns.get(c).cloned())
                .collect();
            let rows = rows
                .into_iter()
                .map(|row| {
                    keep.iter()
                        .map(|&c| row.get(c).cloned().unwrap_or(CellValue::Null))
                        .collect()
                })
                .collect();
            (cols, rows)
        };

    match selection {
        Selection::None | Selection::Cell(_, _) => (columns.to_vec(), rows),
        Selection::Row(r) => (columns.to_vec(), pick_rows(rows, &[*r])),
        Selection::RowRange(r1, r2) => {
            let (lo, hi) = (*r1.min(r2), *r1.max(r2));
            let keep: Vec<usize> = (lo..=hi).collect();
            (columns.to_vec(), pick_rows(rows, &keep))
        }
        Selection::Rows(keep) => (columns.to_vec(), pick_rows(rows, keep)),
        Selection::Column(c) => pick_cols(rows, &[*c]),
        Selection::Columns(keep) => pick_cols(rows, keep),
        Selection::CellRange(r1, c1, r2, c2) => {
            let (rlo, rhi) = (*r1.min(r2), *r1.max(r2));
            let (clo, chi) = (*c1.min(c2), *c1.max(c2));
            let row_keep: Vec<usize> = (rlo..=rhi).collect();
            let col_keep: Vec<usize> = (clo..=chi).collect();
            pick_cols(pick_rows(rows, &row_keep), &col_keep)
        }
        Selection::Cells(cells) => {
            let mut row_keep: Vec<usize> = cells.iter().map(|&(r, _)| r).collect();
            row_keep.sort_unstable();
            row_keep.dedup();
            let mut col_keep: Vec<usize> = cells.iter().map(|&(_, c)| c).collect();
            col_keep.sort_unstable();
            col_keep.dedup();
            let cols: Vec<ResultColumn> = col_keep
                .iter()
                .filter_map(|&c| columns.get(c).cloned())
                .collect();
            let out_rows: Vec<Vec<CellValue>> = row_keep
                .iter()
                .map(|&r| {
                    col_keep
                        .iter()
                        .map(|&c| {
                            if cells.contains(&(r, c)) {
                                rows.get(r)
                                    .and_then(|row| row.get(c).cloned())
                                    .unwrap_or(CellValue::Null)
                            } else {
                                CellValue::Null
                            }
                        })
                        .collect()
                })
                .collect();
            (cols, out_rows)
        }
    }
}

/// Whether a grid cell's displayed text looks like a binary/varbinary value,
/// so a "View as Hex" action should be offered. Matches the grid's default
/// `<N bytes>` placeholder, an explicit `0x…` hex literal, or a bare
/// even-length hex run that contains at least one a–f letter — the letter
/// requirement keeps ordinary digit-only strings (IDs, phone numbers) from
/// being offered as hex. Pure, so the recognizer is unit-testable.
fn looks_like_binary_cell(text: &str) -> bool {
    let text = text.trim();
    // `<123 bytes>` placeholder.
    if let Some(inner) = text.strip_prefix('<').and_then(|s| s.strip_suffix('>')) {
        return inner.split_once(' ').is_some_and(|(count, unit)| {
            !count.is_empty()
                && count.bytes().all(|b| b.is_ascii_digit())
                && !unit.trim().is_empty()
        });
    }
    let prefixed = text.starts_with("0x") || text.starts_with("0X");
    let hex = text
        .strip_prefix("0x")
        .or_else(|| text.strip_prefix("0X"))
        .unwrap_or(text);
    if hex.len() < 4 || !hex.len().is_multiple_of(2) || !hex.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return false;
    }
    // Bare hex needs more length and at least one hex letter; a `0x` prefix is
    // already an explicit binary marker.
    prefixed || (hex.len() >= 8 && hex.bytes().any(|b| b.is_ascii_alphabetic()))
}

/// Whether a clicked cell holds bytes, for the menu's binary-only actions
/// (View as Hex, Save/Load binary file) and to withhold text transforms.
/// Decided by the column's type when the menu knows it (`bytes_column` is
/// `Some`): a binary column holds bytes whatever the grid shows for them
/// (`<16 bytes>`, hex or base64 from a `binary=` directive, decoded text);
/// any other column holds bytes only where the grid shows its `<N bytes>`
/// stand-in, which it draws for nothing but a byte value (a SQLite BLOB in a
/// TEXT column). Text that merely looks like hex — a `0x…` wallet address
/// in a text column — is text. Only with no type known does the text
/// decide, through [`looks_like_binary_cell`].
fn cell_holds_bytes(bytes_column: Option<bool>, text: &str) -> bool {
    match bytes_column {
        Some(true) => true,
        Some(false) => is_binary_placeholder(text),
        None => looks_like_binary_cell(text),
    }
}

/// True when displayed grid text contains a run of the default mask character
/// (`•`), long enough that a suffix-kept value like `••••abcd` counts.
/// Whether an engine-reported type can carry a fractional part. Unknown
/// (empty) types answer `true` so the rounding option is never withheld from
/// a column we can't classify.
fn type_can_have_decimals(data_type: &str) -> bool {
    let t = data_type.trim().to_ascii_lowercase();
    if t.is_empty() {
        return true;
    }
    [
        "decimal", "numeric", "float", "real", "double", "money", "number",
    ]
    .iter()
    .any(|token| t.contains(token))
}

/// The engine type name of the clicked grid column, from a snapshot keyed
/// by grid column (see `ResultGridMenuProvider::clicked_column_type`).
fn column_type_at<'a>(column_types: &'a [String], request: &ContextMenuRequest) -> Option<&'a str> {
    let grid_col = request.target.column_index()?;
    column_types.get(grid_col).map(String::as_str)
}

/// Whether the rounding submenu applies to the clicked column: only a type
/// that can carry decimals, with an unknown type keeping the option. The
/// pivot and transposed grids snapshot their synthetic columns' types
/// (`text`, `nvarchar`) for the WHERE menu; those say nothing about what
/// the cells hold, so `synthetic_types` keeps the option there too.
fn offers_rounding(
    column_types: &[String],
    request: &ContextMenuRequest,
    synthetic_types: bool,
) -> bool {
    synthetic_types || column_type_at(column_types, request).is_none_or(type_can_have_decimals)
}

fn looks_masked(text: &str) -> bool {
    let ch = MaskFormat::default().mask_char;
    let mut run = 0usize;
    for c in text.chars() {
        if c == ch {
            run += 1;
            if run >= 2 {
                return true;
            }
        } else {
            run = 0;
        }
    }
    false
}

/// True when the grid appended a lazy-hydration size badge (` … [2.4 MiB]`).
pub(crate) fn looks_truncated_marker(text: &str) -> bool {
    text.rfind(" … [")
        .is_some_and(|pos| text[pos..].ends_with(']'))
}

/// True for the grid's `<N bytes>` stand-in for a binary value. Every value
/// of that length displays the same, so it can never be filtered on as text.
fn is_binary_placeholder(text: &str) -> bool {
    binary_placeholder_len(text).is_some()
}

/// The length a `<N bytes>` stand-in reports, when `text` is one.
fn binary_placeholder_len(text: &str) -> Option<usize> {
    text.trim()
        .strip_prefix('<')
        .and_then(|s| s.strip_suffix('>'))
        .and_then(|inner| inner.split_once(' '))
        .filter(|(count, unit)| {
            !count.is_empty()
                && count.bytes().all(|b| b.is_ascii_digit())
                && !unit.trim().is_empty()
        })
        .and_then(|(count, _)| count.parse().ok())
}

/// A `<0 bytes>` stand-in in a binary column hides nothing: the value is
/// empty bytes, exactly as shown, and compares as `0x` / `X''` with no raw
/// row needed. Any other `<N bytes>` is a placeholder for bytes the text
/// does not spell. Only a binary column shows the stand-in; the same text
/// in a text column is just text, which is why the callers ask the column
/// first.
fn empty_bytes_shown(text: &str) -> bool {
    binary_placeholder_len(text) == Some(0)
}

/// The raw bytes behind a grid cell, as the engine returned them, looked up
/// by `(grid source row, result column)`: `Some` only for a binary cell.
/// The host's closure answers `None` when there is nothing to look it up
/// from (a pivot grid) or when the cell is not bytes, so the grid text (an
/// exact rendering of anything else) is what the predicate uses and no
/// multi-megabyte text cell is copied just to find that out.
type RawCellLookup<'a> = &'a dyn Fn(usize, usize) -> Option<CellValue>;

/// Whether the host cut `(source row, result column)` to a preview. Asked
/// before anything else: a cut cell never becomes a value predicate,
/// whatever its grid text looks like and whether or not its raw row is
/// read. A cheap set lookup, separate from the raw row read.
type TruncatedLookup<'a> = &'a dyn Fn(usize, usize) -> bool;

/// What a grid cell is, as far as a WHERE predicate is concerned, decided
/// once for the menu and the executed action so the two never read a cell
/// differently: a preview cut short (` … [2.4 MiB]`), an empty binary
/// value (`<0 bytes>` in a binary column spells the whole value), a
/// `<N bytes>` placeholder for bytes the text does not spell, or the value
/// the text is an exact rendering of. `bytes_column` says whether the
/// column's declared type is binary: a `<0 bytes>` stand-in is empty bytes
/// there and plain text anywhere else.
#[derive(Debug)]
enum CellShape {
    Cut,
    EmptyBytes,
    Placeholder,
    Value(CellValue),
}

/// Whether Match Row can compare a cell, by the same [`cell_shape_of`]
/// reading as the executed action, without copying the cell: a text cell's
/// text is read in place (Match Row looks at every cell of the row on every
/// right-click, and a row may hold several multi-megabyte values). A cut
/// preview never compares; a `<N bytes>` placeholder only where the raw row
/// can be read behind it.
fn comparable_in_match_row(
    grid: &sqlly_datatable::CellValue,
    bytes_column: bool,
    raw_rows_available: bool,
) -> bool {
    let owned;
    let text: &str = match grid {
        sqlly_datatable::CellValue::Text(t) => t,
        other => {
            owned = format_grid_cell_text(other);
            &owned
        }
    };
    if looks_truncated_marker(text) {
        return false;
    }
    if empty_bytes_shown(text) && bytes_column {
        return true;
    }
    if is_binary_placeholder(text) && !empty_bytes_shown(text) {
        return raw_rows_available;
    }
    true
}

/// Reads a cell's shape from its grid text, formatted once by the caller.
fn cell_shape_of(grid: &sqlly_datatable::CellValue, text: &str, bytes_column: bool) -> CellShape {
    if looks_truncated_marker(text) {
        CellShape::Cut
    } else if empty_bytes_shown(text) && bytes_column {
        CellShape::EmptyBytes
    } else if is_binary_placeholder(text) && !empty_bytes_shown(text) {
        CellShape::Placeholder
    } else {
        CellShape::Value(grid_cell_to_core(grid))
    }
}

/// The value a WHERE predicate compares a grid cell against, or `None` when
/// the grid holds no whole value to compare: a preview cut short, or a
/// binary placeholder whose bytes are not at hand. Binary cells use their
/// raw bytes (rendered as the engine's hex literal) whatever the grid shows
/// for them — `<16 bytes>`, hex or base64 from a `binary=` directive, or
/// plain text for a BLOB in a column the engine does not call binary (a
/// SQLite TEXT column, a MySQL binary collation).
///
/// The raw row is asked for the bytes when the cell may hold some: the
/// column is binary, the text looks binary, or `read_raw` says to ask
/// regardless (SQLite's dynamic typing puts bytes under any column; Match
/// row reads the whole row once anyway, shared across its cells by
/// [`RawRowCache`]). A plain number or date cell on a spilled grid is
/// exact as the grid shows it, and is not worth a disk read of its row.
/// The rest follows [`cell_shape_of`].
fn predicate_value(
    grid: &sqlly_datatable::CellValue,
    source_row: usize,
    result_col: usize,
    read: CellRead,
    raw: RawCellLookup<'_>,
    truncated: TruncatedLookup<'_>,
) -> Option<CellValue> {
    if truncated(source_row, result_col) {
        return None;
    }
    let text = format_grid_cell_text(grid);
    let shape = cell_shape_of(grid, &text, read.bytes_column);
    if matches!(shape, CellShape::Cut) {
        return None;
    }
    if (read.read_raw || read.bytes_column || looks_like_binary_cell(&text))
        && let Some(bytes @ CellValue::Bytes(_)) = raw(source_row, result_col)
    {
        return Some(bytes);
    }
    match shape {
        CellShape::Cut => None,
        CellShape::EmptyBytes => Some(CellValue::Bytes(Vec::new())),
        CellShape::Placeholder => None,
        CellShape::Value(value) => Some(value),
    }
}

/// How [`predicate_value`] reads one column's cells.
#[derive(Clone, Copy)]
struct CellRead {
    /// The column's declared type is binary.
    bytes_column: bool,
    /// Ask the raw row for bytes whatever the column and text look like.
    read_raw: bool,
}

impl CellRead {
    /// A single clicked cell: the raw row is read only when the column is
    /// binary, the text looks binary, or the engine types dynamically
    /// (SQLite), where any column may hold bytes.
    fn for_column(db: Option<sqlly_lang::sqlmodel::DatabaseType>, bytes_column: bool) -> Self {
        let dynamic_typing =
            db.is_some_and(|db| db.family() == sqlly_lang::sqlmodel::Dialect::Sqlite);
        Self {
            bytes_column,
            read_raw: dynamic_typing,
        }
    }

    /// Match row: the whole row is read once anyway, so every cell asks.
    fn whole_row(bytes_column: bool) -> Self {
        Self {
            bytes_column,
            read_raw: true,
        }
    }
}

/// Serves single cells out of raw rows, reading each row once however many
/// of its cells are asked for (Match row asks for all of them, and a spilled
/// row is a disk read). `read` fetches a whole row by its grid source row.
struct RawRowCache<R, F: Fn(usize) -> Option<R>> {
    read: F,
    last: std::cell::RefCell<Option<(usize, Option<R>)>>,
}

impl<R, F: Fn(usize) -> Option<R>> RawRowCache<R, F> {
    fn new(read: F) -> Self {
        Self {
            read,
            last: std::cell::RefCell::new(None),
        }
    }

    /// `pick` one cell out of the row behind `row`.
    fn cell(&self, row: usize, pick: impl FnOnce(&R) -> Option<CellValue>) -> Option<CellValue> {
        let mut cached = self.last.borrow_mut();
        if cached.as_ref().is_none_or(|(r, _)| *r != row) {
            *cached = Some((row, (self.read)(row)));
        }
        pick(cached.as_ref()?.1.as_ref()?)
    }
}

/// [`predicate_value`] for building the menu, which has no raw rows: the
/// value and whether it is a `<N bytes>` placeholder standing in for bytes
/// the menu cannot see (then the value is empty bytes, so the menu offers
/// what a binary value supports, and the availability gate knows there is
/// nothing in it to compare with). A cut preview offers no value predicate.
/// The same [`cell_shape_of`] as the executed action.
fn menu_predicate_value(
    grid: &sqlly_datatable::CellValue,
    bytes_column: bool,
) -> Option<(CellValue, bool)> {
    menu_predicate_value_of(grid, &format_grid_cell_text(grid), bytes_column)
}

/// [`menu_predicate_value`] with the cell's text already formatted.
fn menu_predicate_value_of(
    grid: &sqlly_datatable::CellValue,
    text: &str,
    bytes_column: bool,
) -> Option<(CellValue, bool)> {
    match cell_shape_of(grid, text, bytes_column) {
        CellShape::Cut => None,
        CellShape::EmptyBytes => Some((CellValue::Bytes(Vec::new()), false)),
        CellShape::Placeholder => Some((CellValue::Bytes(Vec::new()), true)),
        CellShape::Value(value) => Some((value, false)),
    }
}

/// Transform macros and generator inserts for a clicked cell (feature #8).
/// Gated loosely on the grid's inferred column kind — the host re-checks
/// every action against the table's real column metadata before staging.
fn cell_generator_items(
    request: &ContextMenuRequest,
    clicked: &sqlly_datatable::SelectedCellContext,
    text: &str,
    via_relay: bool,
    bytes_column: Option<bool>,
) -> Vec<ContextMenuItem> {
    use sqlly_datatable::ColumnKind;
    let kind = request.clicked_row().and_then(|row| {
        row.columns
            .get(clicked.column_index)
            .map(|column| column.kind)
    });
    let textish = matches!(kind, None | Some(ColumnKind::Text) | Some(ColumnKind::None));
    let dateish = matches!(kind, None | Some(ColumnKind::Date) | Some(ColumnKind::None));
    let mut items = Vec::new();
    let has_text = matches!(&clicked.value, sqlly_datatable::CellValue::Text(t) if !t.is_empty());
    if textish && has_text && !cell_holds_bytes(bytes_column, text) && !looks_masked(text) {
        items.push(ContextMenuItem::separator());
        for transform in crate::cell_transform::CellTransform::all() {
            items.push(ContextMenuItem::action(
                transform.action_id(),
                crate::localization::format(
                    "results.grid_menu.edit_cell.transform_action",
                    &[("transform_name", &transform.localized_label())],
                ),
            ));
        }
    }
    if textish || dateish {
        items.push(ContextMenuItem::separator());
    }
    if textish {
        items.push(ContextMenuItem::action(
            action_id::CELL_NEW_UUID,
            crate::localization::text("results.grid_menu.edit_cell.set_new_uuid"),
        ));
    }
    if dateish {
        items.push(ContextMenuItem::action(
            action_id::CELL_LOCAL_TIMESTAMP,
            crate::localization::text("results.grid_menu.edit_cell.set_local_timestamp"),
        ));
        // Server-evaluated generators are withheld on relay connections whose
        // bridge did not advertise them: an older bridge rejects the whole
        // edit request over them.
        if !via_relay {
            items.push(ContextMenuItem::action(
                action_id::CELL_SQL_SERVER_DATE,
                crate::localization::text("results.grid_menu.edit_cell.set_server_date"),
            ));
        }
    }
    if textish && !via_relay {
        items.push(ContextMenuItem::action(
            action_id::CELL_SQL_SERVER_UUID,
            crate::localization::text("results.grid_menu.edit_cell.set_server_uuid"),
        ));
        items.push(ContextMenuItem::action(
            action_id::CELL_SQL_CURRENT_USER,
            crate::localization::text("results.grid_menu.edit_cell.set_current_user"),
        ));
    }
    items
}

/// Map a transform/generator menu id to its cell action.
fn cell_generator_action(id: &str) -> Option<crate::cell_actions::CellAction> {
    use crate::cell_actions::CellAction;
    use sqlly_services::row_editing::ColumnExpression;
    if let Some(transform) = crate::cell_transform::CellTransform::from_action_id(id) {
        return Some(CellAction::Transform(transform));
    }
    Some(match id {
        action_id::CELL_NEW_UUID => CellAction::NewUuid,
        action_id::CELL_LOCAL_TIMESTAMP => CellAction::LocalTimestamp,
        action_id::CELL_SQL_SERVER_DATE => CellAction::SqlFunction(ColumnExpression::ServerDate),
        action_id::CELL_SQL_SERVER_UUID => CellAction::SqlFunction(ColumnExpression::ServerUuid),
        action_id::CELL_SQL_CURRENT_USER => CellAction::SqlFunction(ColumnExpression::CurrentUser),
        _ => return None,
    })
}

/// The display text of a clicked datatable cell (only `Text` carries a hex
/// string worth parsing; everything else yields an empty string).
fn clicked_text(value: &sqlly_datatable::CellValue) -> String {
    match value {
        sqlly_datatable::CellValue::Text(s) => s.clone(),
        _ => String::new(),
    }
}

fn copy_as_action_id(format: CopyFormat) -> &'static str {
    match format {
        CopyFormat::Tsv => action_id::COPY_AS_TSV,
        CopyFormat::Csv => action_id::COPY_AS_CSV,
        CopyFormat::Json => action_id::COPY_AS_JSON,
        CopyFormat::JsonLines => action_id::COPY_AS_JSON_LINES,
        CopyFormat::Markdown => action_id::COPY_AS_MARKDOWN,
        CopyFormat::SqlInsert => action_id::COPY_AS_INSERT,
    }
}

fn copy_format_for_action_id(id: &str) -> Option<CopyFormat> {
    Some(match id {
        action_id::COPY_AS_TSV => CopyFormat::Tsv,
        action_id::COPY_AS_CSV => CopyFormat::Csv,
        action_id::COPY_AS_JSON => CopyFormat::Json,
        action_id::COPY_AS_JSON_LINES => CopyFormat::JsonLines,
        action_id::COPY_AS_MARKDOWN => CopyFormat::Markdown,
        action_id::COPY_AS_INSERT => CopyFormat::SqlInsert,
        _ => return None,
    })
}

fn copy_format_label(format: CopyFormat) -> String {
    crate::localization::text(match format {
        CopyFormat::Tsv => "results.copy_formats.tsv",
        CopyFormat::Csv => "results.copy_formats.csv",
        CopyFormat::Json => "results.copy_formats.json",
        CopyFormat::JsonLines => "results.copy_formats.json_lines",
        CopyFormat::Markdown => "results.copy_formats.markdown",
        CopyFormat::SqlInsert => "results.copy_formats.sql_insert",
    })
}

/// Column index of a single-column selection, if any.
fn single_column_selection(
    selection: &Selection,
    selected: &[SelectedCellContext],
) -> Option<usize> {
    match selection {
        Selection::Column(c) => Some(*c),
        Selection::CellRange(_, c1, _, c2) if c1 == c2 => Some(*c1),
        Selection::Row(_) | Selection::RowRange(_, _) | Selection::Rows(_) => None,
        Selection::Columns(cols) if cols.len() == 1 => Some(cols[0]),
        _ => {
            if selected.is_empty() {
                return None;
            }
            let col = selected[0].column_index;
            if selected.iter().all(|c| c.column_index == col) {
                Some(col)
            } else {
                None
            }
        }
    }
}

fn single_column_from_request(request: &ContextMenuRequest) -> Option<usize> {
    if request.is_column_oriented() {
        return request.target.column_index().or_else(|| {
            request
                .selection
                .as_ref()
                .and_then(|s| (s.column_start == s.column_end).then_some(s.column_start))
        });
    }
    if matches!(request.target, ContextMenuTarget::RowHeader { .. }) {
        return None;
    }
    if let Some(s) = &request.selection
        && s.column_start == s.column_end
    {
        return Some(s.column_start);
    }
    single_column_selection(&Selection::None, &request.selected_cells())
}

fn selection_is_row_based(selection: &Selection) -> bool {
    matches!(
        selection,
        Selection::Row(_) | Selection::RowRange(_, _) | Selection::Rows(_)
    )
}

/// The context-menu provider for a single result-set grid.
///
/// Holds weak entity handles to the `ResultsView` (for export and layout
/// changes) and the `SqlEditor` (for WHERE-clause insertion), plus the
/// result-set slot index so it can find the right columns and rows.
#[derive(Clone)]
pub struct ResultGridMenuProvider {
    results: WeakEntity<ResultsView>,
    editor: Option<WeakEntity<SqlEditor>>,
    /// Index into `ResultsView.result_sets` — NOT the query's result-set
    /// index, but the slot position in the vec.
    slot_index: usize,
    /// True when this provider serves the directive-pivot grid: exports read
    /// the pivoted table (what the user sees) instead of the source rows,
    /// and row/WHERE actions that only make sense on source rows are hidden.
    pivot: bool,
    /// Tests inject fixed preferences here so menu contents don't depend on
    /// the developer's real persisted export.json.
    export_prefs_override: Option<ExportPreferences>,
    /// Labelled FK columns for this slot, snapshotted when the grid provider
    /// is built (`menu_items` has no `&App` to read `ResultsView`).
    fk_label_headers: std::collections::HashMap<usize, FkLabelHeaderMenuInfo>,
    /// Engine-reported data type per column index, snapshotted like
    /// `fk_label_headers`; gates type-dependent menu entries (rounding) and
    /// tells the WHERE menu what a text cell is where the type name is
    /// ambiguous (SQL Server's `timestamp` is bytes, not a date).
    column_types: Vec<String>,
    /// The result columns behind this grid and the width of its staged-edit
    /// marker prefix, snapshotted with `column_types`: the WHERE menu builds
    /// every action it lists through the same builder the click runs, and
    /// lists only those that build (see [`Self::where_item_builds`]). Empty
    /// until the host provides them; then the menu's own gating stands
    /// alone.
    columns: Vec<ResultColumn>,
    marker: usize,
    /// Names of this grid's `-- sqlly column:` computed columns, so their
    /// header menu offers Edit Formula….
    computed_columns: Vec<String>,
    /// The engine the results came from, snapshotted with `column_types`:
    /// the WHERE menu classifies cells by engine and type name so it offers
    /// exactly the operators the executed action can build. `None` for a
    /// view that has not run a query, where a type name is read without an
    /// engine (`timestamp` a date-time, not SQL Server's rowversion).
    source_db: Option<sqlly_lang::sqlmodel::DatabaseType>,
    /// The connection is reached through a relay bridge that has not
    /// advertised the server-evaluated generators: they are hidden (an older
    /// bridge rejects them; see `cell_actions::server_expressions_blocked`).
    via_relay: bool,
}

impl ResultGridMenuProvider {
    pub fn new(
        results: WeakEntity<ResultsView>,
        editor: Option<WeakEntity<SqlEditor>>,
        slot_index: usize,
    ) -> Self {
        Self {
            results,
            editor,
            slot_index,
            pivot: false,
            export_prefs_override: None,
            fk_label_headers: std::collections::HashMap::new(),
            column_types: Vec::new(),
            columns: Vec::new(),
            marker: 0,
            computed_columns: Vec::new(),
            source_db: None,
            via_relay: false,
        }
    }

    pub fn with_computed_columns(mut self, names: Vec<String>) -> Self {
        self.computed_columns = names;
        self
    }

    pub fn with_column_types(mut self, types: Vec<String>) -> Self {
        self.column_types = types;
        self
    }

    /// Snapshot the result columns behind this grid (`menu_items` has no
    /// `&App` to read them) and the staged-edit marker width in front of
    /// them, so the WHERE menu can dry-run the builder the click runs.
    pub fn with_columns(mut self, columns: Vec<ResultColumn>, marker: usize) -> Self {
        self.columns = columns;
        self.marker = marker;
        self
    }

    /// Snapshot the results' engine for menu building (`menu_items` has no
    /// `&App` to read it from the view); `None` when the view has none.
    pub fn with_source_db(mut self, db: Option<sqlly_lang::sqlmodel::DatabaseType>) -> Self {
        self.source_db = db;
        self
    }

    /// Snapshot whether the results' connection routes through a relay.
    pub fn with_relay_route(mut self, via_relay: bool) -> Self {
        self.via_relay = via_relay;
        self
    }

    pub fn with_fk_label_headers(
        mut self,
        headers: std::collections::HashMap<usize, FkLabelHeaderMenuInfo>,
    ) -> Self {
        self.fk_label_headers = headers;
        self
    }

    /// Test-only: pin the export preferences instead of reading the
    /// persisted file, keeping menu tests hermetic.
    #[cfg(test)]
    pub(crate) fn with_export_prefs(mut self, prefs: ExportPreferences) -> Self {
        self.export_prefs_override = Some(prefs);
        self
    }

    /// Provider for the directive-pivot grid (see `pivot` field).
    pub fn new_pivot(
        results: WeakEntity<ResultsView>,
        editor: Option<WeakEntity<SqlEditor>>,
        slot_index: usize,
    ) -> Self {
        Self {
            results,
            editor,
            slot_index,
            pivot: true,
            export_prefs_override: None,
            fk_label_headers: std::collections::HashMap::new(),
            column_types: Vec::new(),
            columns: Vec::new(),
            marker: 0,
            computed_columns: Vec::new(),
            source_db: None,
            via_relay: false,
        }
    }

    /// Whether the grid column's engine type is binary, from the snapshotted
    /// column types: `None` when the type is not known (no snapshot, or an
    /// empty type name), so [`cell_holds_bytes`] falls back to the text.
    /// The pivot and transposed grids' synthetic types are text, which only
    /// their `<N bytes>` stand-ins override.
    fn bytes_column_at(&self, grid_col: usize) -> Option<bool> {
        let data_type = self.column_types.get(grid_col)?.trim();
        if data_type.is_empty() {
            return None;
        }
        Some(
            where_actions::kind_for_type_name(self.source_db, data_type)
                == Some(where_actions::WhereValueKind::Bytes),
        )
    }

    /// The engine of the connection that produced this view's results, as
    /// the host knows it right now: `None` until it ran a query, which
    /// classifies type names engine-independently. The same reading the
    /// grid and the menu snapshot use, so the executed action can never
    /// read a column differently from the menu that offered it.
    fn source_db(&self, cx: &App) -> Option<sqlly_lang::sqlmodel::DatabaseType> {
        self.results
            .upgrade()
            .and_then(|rv| rv.read(cx).classification_db())
    }

    /// The dialect generated SQL (WHERE actions, SQL exports, row edits)
    /// renders in: the results' engine, or the default dialect until one
    /// is known.
    fn render_db(&self, cx: &App) -> sqlly_lang::sqlmodel::DatabaseType {
        self.source_db(cx).unwrap_or_default()
    }

    /// Export-type actions on a windowed result: read the first
    /// [`crate::result_export::EXPORT_MAX_ROWS`] rows on the background
    /// executor (the grid caption says so), then run `action` again from
    /// outside the grid's update with the rows in memory. Returns `false`
    /// when there is nothing to wait for (resident rows, the pivot grid, or
    /// rows already read) and the action should run now.
    fn defer_until_export_rows(
        &self,
        action: &str,
        request: &ContextMenuRequest,
        cx: &mut App,
    ) -> bool {
        if self.pivot {
            return false;
        }
        let Some(rv) = self.results.upgrade() else {
            return false;
        };
        let slot = self.slot_index;
        let provider = self.clone();
        let request = request.clone();
        let action = action.to_string();
        let weak = self.results.clone();
        rv.update(cx, |view, cx| {
            let limit = crate::result_export::EXPORT_MAX_ROWS;
            let caption = crate::localization::format(
                "results.grid_menu.export.preparing_spill_rows",
                &[("row_count", &view.spill_read_count(slot, limit).to_string())],
            );
            view.defer_until_spill_prefixes(vec![(slot, limit)], caption, cx, move |cx| {
                let Some(state) = weak
                    .upgrade()
                    .and_then(|rv| rv.read(cx).grid_state_for_slot(slot, cx))
                else {
                    return;
                };
                state.update(cx, |state, cx| {
                    provider.on_action(&action, &request, state, cx)
                });
            })
        })
    }

    /// Run `f` against the host `ResultsView` once the grid's update has
    /// settled. `on_action` fires from inside the grid's own update (the
    /// `SqllyDataTable` render drains the clicked menu item), and nearly
    /// every host method reads or rebuilds that grid, so calling one inline
    /// is a fatal double lease ("cannot read SqllyDataTable while it is
    /// already being updated").
    fn defer_host(
        &self,
        cx: &mut App,
        f: impl FnOnce(&mut ResultsView, &mut gpui::Context<ResultsView>) + 'static,
    ) {
        let results = self.results.clone();
        cx.defer(move |cx| {
            // The results pane can close before the deferred call runs.
            let _ = results.update(cx, f);
        });
    }

    /// Width of the staged-edit marker prefix on this menu's grid: grid
    /// column `g` is result column `g - marker`. The pivot grid has none.
    fn marker_width(&self, state: &GridState, cx: &App) -> usize {
        if self.pivot {
            return 0;
        }
        self.results.upgrade().map_or(0, |rv| {
            rv.read(cx).grid_marker_width_of(self.slot_index, state)
        })
    }

    /// The result column behind a grid column of this menu's grid; `None`
    /// for the staged-edit marker column.
    fn result_column(&self, grid_col: usize, state: &GridState, cx: &App) -> Option<usize> {
        grid_col.checked_sub(self.marker_width(state, cx))
    }

    /// The grid's selection in result columns (what `resolve_visible_rows`
    /// returns), so exports never shift a column onto its neighbour while
    /// edits are staged.
    fn result_selection(&self, state: &GridState, cx: &App) -> Selection {
        crate::results::selection_in_result_columns(&state.selection, self.marker_width(state, cx))
    }

    /// Read the result set (columns + rows in core types) from the
    /// `ResultsView` for export and WHERE actions. The pivot provider reads
    /// the pivoted table so exports match what the grid shows.
    fn result_set(&self, cx: &App) -> Option<ResultSet> {
        let rv = self.results.upgrade()?;
        let rv = rv.read(cx);
        if self.pivot {
            let (columns, rows) = rv.pivot_export_table(self.slot_index, cx)?;
            let hint = rows.len();
            return Some(ResultSet {
                index: self.slot_index,
                columns,
                rows,
                rows_affected: None,
                row_count_hint: hint,
                ..Default::default()
            });
        }
        let rs = rv.result_set_at(self.slot_index)?;
        Some(ResultSet {
            index: rs.index,
            columns: rs.columns.clone(),
            rows: rs.rows.clone(),
            rows_affected: rs.rows_affected,
            row_count_hint: rs.row_count_hint,
            truncated: rs.truncated.clone(),
        })
    }

    fn map_row(&self, row: usize, cx: &App) -> usize {
        self.results
            .upgrade()
            .map(|rv| rv.read(cx).source_index(self.slot_index, row))
            .unwrap_or(row)
    }

    /// Look up the raw core cell value behind a clicked grid cell, so a binary
    /// value can be hex-dumped from its actual bytes rather than the `<N bytes>`
    /// placeholder the grid shows. Resident rows come from the result set;
    /// spill-backed sets (whose `rs.rows` stays empty by design) page the one
    /// clicked row from the spill file instead. `row` is already a backing row
    /// ([`crate::results::backing_row_for_grid_row`]); `windowed` says it is
    /// an absolute spill row rather than a grid source index.
    fn raw_cell(
        &self,
        row: usize,
        col: usize,
        windowed: bool,
        cx: &App,
    ) -> Option<sqlly_core::CellValue> {
        let rv = self.results.upgrade()?;
        let rv = rv.read(cx);
        // `col` is a grid column; rows hold result columns. The grid's own
        // state is leased while the menu runs, so the marker width comes from
        // the staged-edit state that builds it.
        let marker = if self.pivot {
            0
        } else {
            rv.marker_offset(self.slot_index)
        };
        let col = col.checked_sub(marker)?;
        if windowed {
            return rv.spill_cell_for_slot(self.slot_index, row, col);
        }
        let row = rv.source_index(self.slot_index, row);
        if let Some(cell) = rv
            .result_set_at(self.slot_index)
            .and_then(|rs| rs.rows.get(row))
            .and_then(|r| r.get(col))
        {
            return Some(cell.clone());
        }
        rv.spill_cell_for_slot(self.slot_index, row, col)
    }

    /// Classify the raw core cell (after reveal / ignoring display formatters)
    /// and open the matching preview window.
    fn open_clicked_preview(
        &self,
        clicked: &SelectedCellContext,
        window_offset: Option<usize>,
        cx: &mut App,
    ) {
        let row = crate::results::backing_row_for_grid_row(window_offset, clicked.source_row_index);
        let cell = self
            .raw_cell(row, clicked.column_index, window_offset.is_some(), cx)
            .unwrap_or_else(|| CellValue::String(clicked_text(&clicked.value)));
        let preview = structured_data::classify_cell(&cell);
        // Grid text is read back as hex only where the cell holds bytes (a
        // binary column shown as hex with no raw row behind it): a `0x…`
        // wallet address in a text column is text and opens as such.
        let bytes_column = self.bytes_column_at(clicked.column_index);
        let (preview, raw_text) = if matches!(preview, structured_data::ContentPreview::Text) {
            let shown = clicked_text(&clicked.value);
            let bytes = match &cell {
                CellValue::Bytes(b) => Some(b.clone()),
                _ if cell_holds_bytes(bytes_column, &shown) => {
                    where_actions::hex_string_to_bytes(&shown)
                }
                _ => None,
            };
            if let Some(bytes) = bytes {
                (
                    structured_data::classify_cell(&CellValue::Bytes(bytes.clone())),
                    crate::structured_data::hex_dump(&bytes),
                )
            } else {
                (
                    preview,
                    match &cell {
                        CellValue::String(s) => s.clone(),
                        _ => clicked_text(&clicked.value),
                    },
                )
            }
        } else {
            let raw_text = match &cell {
                CellValue::String(s) => s.clone(),
                CellValue::Bytes(b) => crate::structured_data::hex_dump(b),
                _ => clicked_text(&clicked.value),
            };
            (preview, raw_text)
        };
        // The byte-exact payload feeds the viewer's format picker (a binary
        // cell can be re-read as MessagePack); the identity lets the picked
        // format stick to this value for the session.
        let payload = match (&cell, &preview) {
            (_, structured_data::ContentPreview::Image(..))
            | (_, structured_data::ContentPreview::Spatial(_)) => None,
            (CellValue::Bytes(bytes), _) => Some(bytes.clone()),
            (CellValue::String(text), structured_data::ContentPreview::Hex) => {
                where_actions::hex_string_to_bytes(text)
            }
            (CellValue::String(text), _) => Some(text.as_bytes().to_vec()),
            _ => None,
        };
        // A Redis key's identity carries the connection and database, so its
        // format is remembered across sessions; other cells per session.
        let identity = self.results.upgrade().and_then(|rv| {
            let rv = rv.read(cx);
            let scope = rv.value_format_scope();
            rv.query_sql().map(|sql| {
                structured_data::scoped_value_format_identity(
                    scope.as_ref(),
                    sql,
                    self.slot_index,
                    clicked.source_row_index,
                    &clicked.column_name,
                )
            })
        });
        // An edited, re-compressed value comes back as a staged cell edit,
        // through the same path as Load from File… (review, then save).
        let save: crate::structured_data_viewer::SaveBytes = {
            let results = self.results.clone();
            let slot = self.slot_index;
            let column = clicked.column_index;
            std::rc::Rc::new(move |bytes: Vec<u8>, cx: &mut App| {
                let _ = results.update(cx, |rv, cx| {
                    rv.request_cell_action(
                        slot,
                        row,
                        column,
                        crate::cell_actions::CellAction::SetBytes(bytes),
                        cx,
                    );
                });
            })
        };
        crate::structured_data_viewer::open_cell_preview_with_save(
            &clicked.column_name,
            preview,
            &raw_text,
            payload,
            identity,
            Some(save),
            cx,
        );
    }

    /// Build structured-data / image / geometry items from the clicked cell's
    /// displayed text. `menu_items` has no `&App`, so this cannot look up
    /// `raw_cell`; handlers re-classify from the raw core value (unmasked,
    /// unformatted) when the action runs.
    fn structured_data_items(
        request: &ContextMenuRequest,
        bytes_column: Option<bool>,
    ) -> Vec<ContextMenuItem> {
        let mut items = Vec::new();
        let Some(clicked) = request.clicked_cell() else {
            return items;
        };
        let text = match &clicked.value {
            sqlly_datatable::CellValue::Text(s) => s.as_str(),
            _ => return items,
        };
        if looks_truncated_marker(text) {
            items.push(ContextMenuItem::separator());
            items.push(ContextMenuItem::action(
                action_id::VIEW_FULL_VALUE,
                crate::localization::text("results.grid_menu.view.full_value"),
            ));
            return items;
        }
        if looks_masked(text) {
            if cell_holds_bytes(bytes_column, text) {
                items.push(ContextMenuItem::separator());
                items.push(ContextMenuItem::action(
                    action_id::VIEW_AS_HEX,
                    crate::localization::text("results.grid_menu.view.masked_binary_as_hex"),
                ));
            }
            return items;
        }
        if structured_data::detect(text).is_some() {
            items.push(ContextMenuItem::separator());
            items.push(ContextMenuItem::action(
                action_id::STRUCTURED_DATA_PRETTY,
                crate::localization::text("results.grid_menu.view.structured_data"),
            ));
            items.push(ContextMenuItem::action(
                action_id::STRUCTURED_DATA_COPY,
                crate::localization::text("results.grid_menu.copy_as.pretty_formatted"),
            ));
            return items;
        }
        match structured_data::classify_cell(&sqlly_core::CellValue::String(text.to_string())) {
            structured_data::ContentPreview::Image(_, _) => {
                items.push(ContextMenuItem::separator());
                items.push(ContextMenuItem::action(
                    action_id::VIEW_IMAGE,
                    crate::localization::text("results.grid_menu.view.image"),
                ));
            }
            structured_data::ContentPreview::Spatial(_) => {
                items.push(ContextMenuItem::separator());
                items.push(ContextMenuItem::action(
                    action_id::VIEW_GEOMETRY,
                    crate::localization::text("results.grid_menu.view.geometry"),
                ));
            }
            _ if cell_holds_bytes(bytes_column, text) => {
                items.push(ContextMenuItem::separator());
                items.push(ContextMenuItem::action(
                    action_id::VIEW_AS_HEX,
                    crate::localization::text("results.grid_menu.view.binary_as_hex"),
                ));
            }
            // Plain text still opens in the viewer, whose format picker can
            // re-read it as UTF-8 / JSON / hex / MessagePack.
            _ if !text.is_empty() => {
                items.push(ContextMenuItem::separator());
                items.push(ContextMenuItem::action(
                    action_id::STRUCTURED_DATA_PRETTY,
                    crate::localization::text("results.grid_menu.view.plain_value"),
                ));
            }
            _ => {}
        }
        items
    }

    fn cell_action_items(
        request: &ContextMenuRequest,
        via_relay: bool,
        bytes_column: Option<bool>,
    ) -> Vec<ContextMenuItem> {
        let mut items = Vec::new();
        let Some(clicked) = request.clicked_cell() else {
            return items;
        };
        // The panel edits (and shows) the cell in full; it leads the submenu
        // because the top level is at its 12-entry budget.
        items.push(ContextMenuItem::action(
            action_id::CELL_DETAILS,
            crate::localization::text("results.grid_menu.view.cell_details"),
        ));
        items.push(ContextMenuItem::separator());
        items.push(ContextMenuItem::action(
            action_id::CELL_SET_NULL,
            crate::localization::text("results.grid_menu.edit_cell.set_null"),
        ));
        items.push(ContextMenuItem::action(
            action_id::CELL_SET_DEFAULT,
            crate::localization::text("results.grid_menu.edit_cell.set_default"),
        ));
        items.push(ContextMenuItem::action(
            action_id::CELL_SERVER_TIME,
            crate::localization::text("results.grid_menu.edit_cell.set_server_time"),
        ));
        let is_bool = request.clicked_row().is_some_and(|row| {
            row.columns
                .get(clicked.column_index)
                .is_some_and(|column| column.kind == sqlly_datatable::ColumnKind::Boolean)
        });
        if is_bool {
            items.push(ContextMenuItem::action(
                action_id::CELL_SET_TRUE,
                crate::localization::text("results.grid_menu.edit_cell.set_true"),
            ));
            items.push(ContextMenuItem::action(
                action_id::CELL_SET_FALSE,
                crate::localization::text("results.grid_menu.edit_cell.set_false"),
            ));
        }
        let text = clicked_text(&clicked.value);
        items.extend(cell_generator_items(
            request,
            &clicked,
            &text,
            via_relay,
            bytes_column,
        ));
        if cell_holds_bytes(bytes_column, &text) {
            items.push(ContextMenuItem::action(
                action_id::CELL_SAVE_FILE,
                crate::localization::text("results.grid_menu.edit_cell.save_binary_file"),
            ));
            items.push(ContextMenuItem::action(
                action_id::CELL_LOAD_FILE,
                crate::localization::text("results.grid_menu.edit_cell.load_binary_file"),
            ));
        }
        items
    }
    /// Quick-filter items: seed the grid's filter bar from the clicked cell
    /// and apply immediately. Unlike the "Add WHERE …" actions these never
    /// touch the editor's SQL — they build a filter-builder row for the grid.
    /// Offered on data cells; a NULL cell only gets the null tests.
    fn quick_filter_items(request: &ContextMenuRequest) -> Vec<ContextMenuItem> {
        let mut items = Vec::new();
        if !matches!(request.target, ContextMenuTarget::Cell { .. }) {
            return items;
        }
        let Some(clicked) = request.clicked_cell() else {
            return items;
        };
        let col_name = clicked.column_name.clone();
        items.push(ContextMenuItem::separator());
        // The grid filters on displayed text: a binary placeholder or a cut
        // preview would match every value that displays the same.
        let displayed = format_grid_cell_text(&clicked.value);
        let filterable_text =
            !is_binary_placeholder(&displayed) && !looks_truncated_marker(&displayed);
        if filterable_text && !matches!(clicked.value, sqlly_datatable::CellValue::None) {
            let shown = menu_value(&format_grid_cell_text(&clicked.value));
            for (id, key) in [
                (
                    action_id::QUICK_FILTER_EQ,
                    "results.grid_menu.filter.quick_equals",
                ),
                (
                    action_id::QUICK_FILTER_NE,
                    "results.grid_menu.filter.quick_not_equals",
                ),
                (
                    action_id::QUICK_FILTER_CONTAINS,
                    "results.grid_menu.filter.quick_contains",
                ),
            ] {
                items.push(ContextMenuItem::action(
                    id,
                    crate::localization::format(
                        key,
                        &[("column_name", &col_name), ("cell_value", &shown)],
                    ),
                ));
            }
        }
        for (id, key) in [
            (
                action_id::QUICK_FILTER_IS_NULL,
                "results.grid_menu.filter.quick_is_null",
            ),
            (
                action_id::QUICK_FILTER_IS_NOT_NULL,
                "results.grid_menu.filter.quick_is_not_null",
            ),
        ] {
            items.push(ContextMenuItem::action(
                id,
                crate::localization::format(key, &[("column_name", &col_name)]),
            ));
        }
        items
    }

    /// This works without App access by using the column name and kind carried
    /// in `SelectedCellContext`, plus the engine (`None` when the view has
    /// none) and the columns' type names snapshotted on the provider, keyed
    /// by grid column (`None` for a column whose type is unknown). Shows all
    /// applicable operators: Equals, NotEquals, IS NULL, IS NOT NULL, LIKE,
    /// IN, NOT IN, BETWEEN, Match Row.
    /// `raw_rows_available` is false on the pivot grid, whose cells have no
    /// raw row behind them.
    fn where_items_from_request<'t>(
        db: Option<sqlly_lang::sqlmodel::DatabaseType>,
        column_type_of: &dyn Fn(usize) -> Option<&'t str>,
        request: &ContextMenuRequest,
        raw_rows_available: bool,
    ) -> Vec<ContextMenuItem> {
        let mut items = Vec::new();
        let Some(clicked) = request.clicked_cell() else {
            return items;
        };
        // The dialect the dry runs render in; with no engine known they
        // render in the default one, which decides nothing about what is
        // offered beyond what the classification already did.
        let render_db = db.unwrap_or_default();
        let column_type = column_type_of(clicked.column_index);
        let is_bytes_column = |column_type: Option<&str>| {
            column_type.is_some_and(|t| {
                where_actions::kind_for_type_name(db, t)
                    == Some(where_actions::WhereValueKind::Bytes)
            })
        };
        // The staged-edit marker column is app chrome, not a result column:
        // there is nothing to filter the query on.
        if clicked.column_index == 0
            && clicked.column_name == crate::grid_bridge::MARKER_COLUMN_NAME
        {
            return items;
        }
        let col_name = clicked.column_name.clone();
        let where_label = |key: &'static str, arguments: &[(&str, &str)]| {
            crate::localization::format(key, arguments)
        };
        let col_idx = clicked.column_index;
        let display = format_grid_cell_text(&clicked.value);
        let bytes_column = is_bytes_column(column_type);
        let Some((core_value, placeholder)) =
            menu_predicate_value_of(&clicked.value, &display, bytes_column)
        else {
            // A preview cut short is not the value: only the NULL tests apply.
            items.push(ContextMenuItem::separator());
            items.push(ContextMenuItem::action(
                action_id::WHERE_IS_NULL,
                where_label(
                    "results.grid_menu.where.is_null",
                    &[("column_name", &col_name)],
                ),
            ));
            items.push(ContextMenuItem::action(
                action_id::WHERE_IS_NOT_NULL,
                where_label(
                    "results.grid_menu.where.is_not_null",
                    &[("column_name", &col_name)],
                ),
            ));
            return items;
        };
        // The labels below inline the cell's value. `display` itself stays
        // whole — the probes and the executed action need the real value —
        // but what goes on screen is clipped, or a JSON blob in one cell
        // stretches the menu to the width of the row.
        let shown = menu_value(&display);
        // Same classification the executed action uses, so the menu never
        // offers an operator the builder then refuses.
        let kind = where_actions::classify_value_for_type(db, &core_value, column_type);
        // The values an operator is probed with, built once per list: one
        // clone of the selected values, shared by every dry run.
        let probe = |values: Vec<WhereSelectedValue>| WhereAction {
            title: String::new(),
            column_name: col_name.clone(),
            column_index: col_idx,
            values,
            operator: WhereOperator::Equals,
            mode: WhereActionMode::Add,
            join: WhereLogicalJoin::And,
            predicate_preview: String::new(),
            source: None,
        };
        // One selected cell of this column as the value the menu probes
        // with, classified the way the executed action renders it.
        let selected_value = |c: &sqlly_datatable::SelectedCellContext| {
            let (cv, placeholder) = menu_predicate_value(&c.value, bytes_column)?;
            Some(WhereSelectedValue {
                kind: where_actions::classify_value_for_type(db, &cv, column_type),
                cell_value: cv,
                display_text: format_grid_cell_text(&c.value),
                placeholder,
            })
        };
        // Whether an operator goes on the menu: the shared availability gate
        // (which knows whether the executed action can read raw bytes the
        // menu cannot), and for everything but bytes whether the builder
        // would render it (a NULL cell has no literal, for one). Bytes shown
        // as base64 have no literal here either, but the executed action
        // compares the real bytes, which is what the gate answers for. The
        // single-value operators render exactly the first value's literal,
        // so that one literal decides them; only the pattern, range and
        // list operators shape the values further and get a dry run, on
        // the probe's values in place.
        let offers = |operator: WhereOperator, probe: &mut WhereAction| -> bool {
            let (available, _reason) = where_actions::operator_availability_for(
                render_db,
                operator,
                &probe.values,
                kind,
                raw_rows_available,
            );
            if !available {
                return false;
            }
            if kind == where_actions::WhereValueKind::Bytes {
                return true;
            }
            match operator {
                WhereOperator::Equals
                | WhereOperator::NotEquals
                | WhereOperator::LessThan
                | WhereOperator::LessThanOrEqual
                | WhereOperator::GreaterThan
                | WhereOperator::GreaterThanOrEqual => probe
                    .values
                    .first()
                    .is_some_and(|v| where_actions::single_sql_literal_for(render_db, v).is_some()),
                _ => {
                    probe.operator = operator;
                    where_actions::render_predicate_for(render_db, probe).is_some()
                }
            }
        };

        items.push(ContextMenuItem::separator());

        let mut selected = probe(vec![WhereSelectedValue {
            cell_value: core_value.clone(),
            display_text: display.clone(),
            kind,
            placeholder,
        }]);
        // Equals / Not Equals
        if offers(WhereOperator::Equals, &mut selected) {
            items.push(ContextMenuItem::action(
                action_id::WHERE_EQUALS,
                where_label(
                    "results.grid_menu.where.equals",
                    &[("column_name", &col_name), ("cell_value", &shown)],
                ),
            ));
        }
        if offers(WhereOperator::NotEquals, &mut selected) {
            items.push(ContextMenuItem::action(
                action_id::WHERE_NOT_EQUALS,
                where_label(
                    "results.grid_menu.where.not_equals",
                    &[("column_name", &col_name), ("cell_value", &shown)],
                ),
            ));
        }

        // Comparison operators (>, >=, <, <=) — ordered value kinds only
        // (numbers and dates). Availability is decided by the shared
        // `operator_availability` gate so the menu and the predicate builder
        // never disagree.
        if matches!(
            kind,
            where_actions::WhereValueKind::Integer
                | where_actions::WhereValueKind::Decimal
                | where_actions::WhereValueKind::Date
        ) {
            for (operator, id, key) in [
                (
                    WhereOperator::GreaterThan,
                    action_id::WHERE_GREATER_THAN,
                    "results.grid_menu.where.greater_than",
                ),
                (
                    WhereOperator::GreaterThanOrEqual,
                    action_id::WHERE_GREATER_THAN_OR_EQUAL,
                    "results.grid_menu.where.greater_than_or_equal",
                ),
                (
                    WhereOperator::LessThan,
                    action_id::WHERE_LESS_THAN,
                    "results.grid_menu.where.less_than",
                ),
                (
                    WhereOperator::LessThanOrEqual,
                    action_id::WHERE_LESS_THAN_OR_EQUAL,
                    "results.grid_menu.where.less_than_or_equal",
                ),
            ] {
                if offers(operator, &mut selected) {
                    items.push(ContextMenuItem::action(
                        id,
                        where_label(key, &[("column_name", &col_name), ("cell_value", &shown)]),
                    ));
                }
            }
        }

        // IS NULL / IS NOT NULL
        items.push(ContextMenuItem::action(
            action_id::WHERE_IS_NULL,
            where_label(
                "results.grid_menu.where.is_null",
                &[("column_name", &col_name)],
            ),
        ));
        items.push(ContextMenuItem::action(
            action_id::WHERE_IS_NOT_NULL,
            where_label(
                "results.grid_menu.where.is_not_null",
                &[("column_name", &col_name)],
            ),
        ));

        // Text-pattern operators (string kind only): contains (LIKE),
        // does-not-contain (NOT LIKE), starts-with, ends-with. Gated by the
        // same `operator_availability` check as the ranges, so bytes shown
        // as hex text get no pattern the engine would reject.
        if matches!(kind, where_actions::WhereValueKind::String) {
            for (operator, id, key) in [
                (
                    WhereOperator::Like,
                    action_id::WHERE_LIKE,
                    "results.grid_menu.where.contains",
                ),
                (
                    WhereOperator::NotLike,
                    action_id::WHERE_NOT_LIKE,
                    "results.grid_menu.where.does_not_contain",
                ),
                (
                    WhereOperator::StartsWith,
                    action_id::WHERE_STARTS_WITH,
                    "results.grid_menu.where.starts_with",
                ),
                (
                    WhereOperator::EndsWith,
                    action_id::WHERE_ENDS_WITH,
                    "results.grid_menu.where.ends_with",
                ),
            ] {
                if offers(operator, &mut selected) {
                    items.push(ContextMenuItem::action(
                        id,
                        where_label(key, &[("column_name", &col_name), ("cell_value", &shown)]),
                    ));
                }
            }
        }

        // IN / NOT IN — when multiple cells are selected in the same column;
        // BETWEEN when exactly two are. Through the same gate as every other
        // operator: a range over bytes or a cut preview is refused there.
        let selected = request.selected_cells();
        let multi_cells: Vec<&sqlly_datatable::SelectedCellContext> = selected
            .iter()
            .filter(|c| c.column_index == col_idx)
            .collect();
        if multi_cells.len() > 1 {
            let multi: Vec<WhereSelectedValue> = multi_cells
                .iter()
                .filter_map(|c| selected_value(c))
                .collect();
            if multi.len() == multi_cells.len() {
                let mut multi = probe(multi);
                for (operator, id, key) in [
                    (
                        WhereOperator::In,
                        action_id::WHERE_IN,
                        "results.grid_menu.where.in_list",
                    ),
                    (
                        WhereOperator::NotIn,
                        action_id::WHERE_NOT_IN,
                        "results.grid_menu.where.not_in_list",
                    ),
                ] {
                    if offers(operator, &mut multi) {
                        items.push(ContextMenuItem::action(
                            id,
                            where_label(key, &[("column_name", &col_name)]),
                        ));
                    }
                }
                if multi.values.len() == 2 && offers(WhereOperator::Between, &mut multi) {
                    items.push(ContextMenuItem::action(
                        action_id::WHERE_BETWEEN,
                        where_label(
                            "results.grid_menu.where.between",
                            &[("column_name", &col_name)],
                        ),
                    ));
                }
            }
        }

        // Match Row — an AND of equals over every column of the clicked row.
        // Offered only when every cell of the row has a value to compare
        // (the same reading of each cell `build_match_row_action` makes): a
        // cut preview never does, and a `<N bytes>` placeholder only where
        // the raw row can be read behind it.
        if let Some(row) = request.clicked_row()
            && row.values.iter().enumerate().all(|(grid_col, cell)| {
                comparable_in_match_row(
                    cell,
                    is_bytes_column(column_type_of(grid_col)),
                    raw_rows_available,
                )
            })
        {
            items.push(ContextMenuItem::separator());
            items.push(ContextMenuItem::action(
                action_id::WHERE_MATCH_ROW,
                crate::localization::text("results.grid_menu.where.match_row"),
            ));
        }

        items
    }

    /// Widen Column / Narrow Column for the column the menu targets, placed
    /// beside the grid's Move Column Left/Right. Empty when the target has
    /// no column (a row header), where the actions would do nothing.
    fn column_width_items(request: &ContextMenuRequest) -> Vec<ContextMenuItem> {
        if request.target.column_index().is_none() {
            return Vec::new();
        }
        vec![
            ContextMenuItem::action(
                action_id::WIDEN_COLUMN,
                crate::localization::text("results.grid_menu.column.widen"),
            ),
            ContextMenuItem::action(
                action_id::NARROW_COLUMN,
                crate::localization::text("results.grid_menu.column.narrow"),
            ),
        ]
    }

    /// Build the layout submenu.
    fn layout_items() -> Vec<ContextMenuItem> {
        vec![
            ContextMenuItem::separator(),
            ContextMenuItem::action(
                action_id::LAYOUT_STACKED,
                crate::localization::text("results.grid_menu.layout.stacked"),
            ),
            ContextMenuItem::action(
                action_id::LAYOUT_TABBED,
                crate::localization::text("results.grid_menu.layout.tabbed"),
            ),
            ContextMenuItem::action(
                action_id::LAYOUT_CANVAS,
                crate::localization::text("results.grid_menu.layout.canvas"),
            ),
            ContextMenuItem::action(
                action_id::RESET_COLUMN_LAYOUT,
                crate::localization::text("results.grid_menu.layout.reset_columns"),
            ),
        ]
    }

    /// Build the alignment submenu: horizontal placement of the column's
    /// values within their cells, whatever the column's type.
    fn alignment_items() -> Vec<ContextMenuItem> {
        vec![
            ContextMenuItem::action(
                action_id::ALIGN_LEFT,
                crate::localization::text("results.grid_menu.alignment.left"),
            ),
            ContextMenuItem::action(
                action_id::ALIGN_CENTER,
                crate::localization::text("results.grid_menu.alignment.center"),
            ),
            ContextMenuItem::action(
                action_id::ALIGN_RIGHT,
                crate::localization::text("results.grid_menu.alignment.right"),
            ),
        ]
    }

    /// Build the rounding submenu for decimal/integer columns. Offers a set
    /// of fixed decimal-place presets that override the column's
    /// `NumberFormat.decimals` at runtime.
    fn rounding_items() -> Vec<ContextMenuItem> {
        vec![
            ContextMenuItem::separator(),
            ContextMenuItem::action(
                action_id::ROUNDING_0,
                crate::localization::text("results.grid_menu.rounding.zero_decimals"),
            ),
            ContextMenuItem::action(
                action_id::ROUNDING_1,
                crate::localization::text("results.grid_menu.rounding.one_decimal"),
            ),
            ContextMenuItem::action(
                action_id::ROUNDING_2,
                crate::localization::text("results.grid_menu.rounding.two_decimals"),
            ),
            ContextMenuItem::action(
                action_id::ROUNDING_4,
                crate::localization::text("results.grid_menu.rounding.four_decimals"),
            ),
            ContextMenuItem::action(
                action_id::ROUNDING_6,
                crate::localization::text("results.grid_menu.rounding.six_decimals"),
            ),
        ]
    }

    /// Reveal / remask / per-connection override actions. Gated by displayed
    /// mask-character runs (or a column header) because `menu_items` has no
    /// `App` to read `ResultsView`.
    fn masking_items(request: &ContextMenuRequest) -> Vec<ContextMenuItem> {
        let is_header = matches!(
            request.target,
            ContextMenuTarget::ColumnHeader { .. } | ContextMenuTarget::SortButton { .. }
        );
        let cell_masked = request
            .clicked_cell()
            .is_some_and(|cell| looks_masked(&clicked_text(&cell.value)));
        // Any column-targeted click gets a masking section: masked columns
        // offer reveal/re-mask, unmasked ones offer masking the column, and
        // everything links to the global rules in Settings.
        let column_targeted = is_header
            || request.target.column_index().is_some()
            || request.clicked_cell().is_some();
        if !column_targeted {
            return Vec::new();
        }
        let col_label = request
            .clicked_cell()
            .map(|cell| cell.column_name.clone())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| crate::localization::text("results.grid_menu.masking.this_column"));
        let mask_label = |key| crate::localization::format(key, &[("col_label", &col_label)]);
        let mut items = vec![ContextMenuItem::separator()];
        if is_header || cell_masked {
            items.push(ContextMenuItem::action(
                action_id::MASK_REVEAL_COLUMN,
                crate::localization::text("results.grid_menu.masking.reveal_column"),
            ));
            items.push(ContextMenuItem::action(
                action_id::MASK_REMASK,
                crate::localization::text("results.grid_menu.masking.remask_result"),
            ));
            if cell_masked {
                items.push(ContextMenuItem::action(
                    action_id::MASK_REVEAL_CELL,
                    crate::localization::text("results.grid_menu.masking.reveal_cell"),
                ));
            }
            items.push(ContextMenuItem::action(
                action_id::MASK_EXCLUDE_ON_CONNECTION,
                mask_label("results.grid_menu.masking.never_mask_connection"),
            ));
            items.push(ContextMenuItem::action(
                action_id::MASK_INCLUDE_ON_CONNECTION,
                mask_label("results.grid_menu.masking.always_mask_connection"),
            ));
        } else {
            items.push(ContextMenuItem::action(
                action_id::MASK_INCLUDE_ON_CONNECTION,
                mask_label("results.grid_menu.masking.mask_connection"),
            ));
        }
        items.push(ContextMenuItem::action(
            action_id::MASK_MANAGE_RULES,
            crate::localization::text("results.grid_menu.masking.manage_rules"),
        ));
        items
    }

    /// Foreign-key label choices for a labelled column header.
    fn fk_label_items(info: &FkLabelHeaderMenuInfo) -> Vec<ContextMenuItem> {
        let mut items = vec![ContextMenuItem::separator()];
        let display_column_action = action_id::fk_label_column(&info.display_column);
        let column_action = if info.column_label_on {
            action_id::FK_LABEL_COLUMN_OFF
        } else {
            display_column_action.as_str()
        };
        items.push(ContextMenuItem::action(
            column_action,
            crate::localization::text(if info.column_label_on {
                "results.grid_menu.foreign_key_label.hide_column_label"
            } else {
                "results.grid_menu.foreign_key_label.show_column_label"
            }),
        ));
        items.push(ContextMenuItem::action(
            action_id::fk_label_column(&info.display_column),
            crate::localization::format(
                "results.grid_menu.foreign_key_label.use_display_column",
                &[("column_name", &info.display_column)],
            ),
        ));
        items.push(ContextMenuItem::action(
            action_id::FK_LABEL_COLUMN_OFF,
            crate::localization::text("results.grid_menu.foreign_key_label.off"),
        ));
        items.push(ContextMenuItem::separator());
        let global = crate::localization::text(if info.show_fk_labels {
            "results.grid_menu.foreign_key_label.hide_globally"
        } else {
            "results.grid_menu.foreign_key_label.show_globally"
        });
        items.push(ContextMenuItem::action(action_id::FK_LABEL_TOGGLE, global));
        items
    }

    /// Apply a rounding override to the grid config for the given column
    /// index. Clones the current config, updates the column override's
    /// `NumberFormat.decimals`, then calls `set_config` to rebuild resolved
    /// formats and recompute the display.
    fn apply_rounding(state: &mut GridState, col_idx: usize, decimals: usize) {
        let mut config = state.config.clone();
        // Ensure column_overrides vec is long enough.
        if config.column_overrides.len() <= col_idx {
            config
                .column_overrides
                .resize(col_idx + 1, sqlly_datatable::ColumnOverride::default());
        }
        let override_entry = &mut config.column_overrides[col_idx];
        let mut number = override_entry.number.unwrap_or_else(|| {
            // Use the column's resolved number format as the base.
            let resolved = state.resolved_formats.get(col_idx);
            resolved
                .map(|r| r.number)
                .unwrap_or(state.config.default_number)
        });
        number.decimals = decimals;
        override_entry.number = Some(number);
        state.set_config(config);
    }

    /// Override the horizontal alignment of column `col_idx`. Alignment
    /// lives on each kind's format, so every kind is set: the column then
    /// keeps the choice whichever kind it resolves to.
    fn apply_alignment(
        state: &mut GridState,
        col_idx: usize,
        alignment: sqlly_datatable::TextAlignment,
    ) {
        let mut config = state.config.clone();
        if config.column_overrides.len() <= col_idx {
            config
                .column_overrides
                .resize(col_idx + 1, sqlly_datatable::ColumnOverride::default());
        }
        let resolved = state.resolved_formats.get(col_idx);
        let entry = &mut config.column_overrides[col_idx];
        let mut number = entry
            .number
            .or_else(|| resolved.map(|r| r.number))
            .unwrap_or(state.config.default_number);
        number.alignment = alignment;
        entry.number = Some(number);
        let mut date = entry
            .date
            .clone()
            .or_else(|| resolved.map(|r| r.date.clone()))
            .unwrap_or_else(|| state.config.default_date.clone());
        date.alignment = alignment;
        entry.date = Some(date);
        let mut boolean = entry
            .boolean
            .clone()
            .or_else(|| resolved.map(|r| r.boolean.clone()))
            .unwrap_or_else(|| state.config.default_boolean.clone());
        boolean.alignment = alignment;
        entry.boolean = Some(boolean);
        let mut string = entry
            .string
            .clone()
            .or_else(|| resolved.map(|r| r.string.clone()))
            .unwrap_or_else(|| state.config.default_string.clone());
        string.alignment = alignment;
        entry.string = Some(string);
        state.set_config(config);
    }

    /// Execute an export format, reading data from the ResultsView.
    /// Uses visible (filtered/sorted) rows from the grid state.
    /// Resolve the grid's current view into a stable export snapshot: visible
    /// rows after sort/filter (spill-backed sets capped at `EXPORT_MAX_ROWS`),
    /// narrowed to the active selection, with export masks applied. The
    /// dialog renders from this snapshot, so later grid changes don't affect
    /// the export.
    pub(crate) fn resolve_export_payload(
        &self,
        state: &GridState,
        cx: &mut App,
    ) -> Option<ExportPayload> {
        let rv = self.results.upgrade()?;
        let (all_columns, all_rows, spill_truncated) =
            rv.read(cx)
                .resolve_visible_rows(self.slot_index, state, self.pivot, cx)?;
        let whole_set = matches!(
            state.selection,
            sqlly_datatable::Selection::None | sqlly_datatable::Selection::Cell(_, _)
        );
        let formatting = rv.read(cx).effective_formatting_for_slot(self.slot_index);
        // The whole visible set, masks applied — the "export all" scope.
        let all_rows =
            crate::result_cell_format::apply_export_masks(&all_columns, all_rows, &formatting);
        // The selection-narrowed scope (equal to the whole set when there is no
        // real selection). Masks are already applied above, so narrowing the
        // masked rows keeps them masked.
        let selection = self.result_selection(state, cx);
        let (columns, rows) = selection_export_data(&all_columns, all_rows.clone(), &selection);
        Some(ExportPayload {
            columns,
            rows,
            all_columns,
            all_rows,
            has_selection: !whole_set,
            spill_truncated,
            whole_set,
            slot: self.slot_index,
        })
    }

    /// Copy-as menu items render a text format straight to the clipboard —
    /// the same payload resolution the export dialog uses, minus the
    /// destination choice.
    fn copy_export(&self, format: ExportFormat, state: &GridState, cx: &mut App) {
        let Some(payload) = self.resolve_export_payload(state, cx) else {
            return;
        };
        let prefs = self
            .export_prefs_override
            .unwrap_or_else(ExportPreferences::load);
        if let Some(text) = crate::results::export_text_for_format_with(
            format,
            self.render_db(cx),
            &payload.columns,
            &payload.rows,
            &crate::results::adhoc_export_options(format, &prefs),
        ) {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn copy_sql_in_list(&self, distinct: bool, state: &GridState, cx: &mut App) {
        let Some(rv) = self.results.upgrade() else {
            return;
        };
        let Some((columns, rows, _)) =
            rv.read(cx)
                .resolve_visible_rows(self.slot_index, state, self.pivot, cx)
        else {
            return;
        };
        let selection = self.result_selection(state, cx);
        let (columns, rows) = selection_export_data(&columns, rows, &selection);
        if columns.len() != 1 {
            return;
        }
        let formatting = rv.read(cx).effective_formatting_for_slot(self.slot_index);
        let rows = crate::result_cell_format::apply_export_masks(&columns, rows, &formatting);
        let column = &columns[0];
        let values: Vec<CellValue> = rows
            .iter()
            .map(|row| row.first().cloned().unwrap_or(CellValue::Null))
            .collect();
        let text =
            crate::result_export::sql_in_list_for(self.render_db(cx), column, &values, distinct);
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    /// "Copy Distinct Values" (header menu): every distinct value of the
    /// column as plain lines, first-occurrence order, export masks applied.
    /// `top` limits the copy to the N most frequent values (most frequent
    /// first); `None` copies every distinct value. Reads the rows currently
    /// visible after sorting and filtering — a filter that hides every row
    /// copies nothing and says so — and a windowed result reads its first
    /// 100,000 rows and says that too.
    fn copy_distinct_values(
        &self,
        column_index: usize,
        top: Option<usize>,
        state: &GridState,
        cx: &mut App,
    ) {
        let Some(rv) = self.results.upgrade() else {
            return;
        };
        let slot = self.slot_index;
        if !self.pivot
            && let Some(reason) = rv.read(cx).nothing_visible_to_copy(slot, state)
        {
            rv.update(cx, |view, cx| view.note_copy_notice(reason, cx));
            return;
        }
        // Rows come back in result columns; the header click is a grid column.
        let Some(column_index) = self.result_column(column_index, state, cx) else {
            return;
        };
        // A windowed result is read from disk in the background (the grid
        // caption says so meanwhile) instead of on this UI-thread callback.
        if !self.pivot
            && rv.update(cx, |view, cx| {
                view.start_column_scan(
                    slot,
                    column_index,
                    crate::results::ColumnScanPurpose::CopyDistinct { top },
                    cx,
                )
            })
        {
            return;
        }
        let Some((columns, rows, spill_truncated)) = rv
            .read(cx)
            .resolve_visible_rows(slot, state, self.pivot, cx)
        else {
            return;
        };
        if column_index >= columns.len() {
            return;
        }
        let read_rows = rows.len();
        let formatting = rv.read(cx).effective_formatting_for_slot(slot);
        let rows = crate::result_cell_format::apply_export_masks(&columns, rows, &formatting);
        let values: Vec<CellValue> = rows
            .iter()
            .map(|row| row.get(column_index).cloned().unwrap_or(CellValue::Null))
            .collect();
        let text = match top {
            Some(n) => top_distinct_column_lines(&values, n),
            None => distinct_column_lines(&values),
        };
        if text.is_empty() {
            rv.update(cx, |view, cx| {
                view.note_copy_notice(
                    crate::localization::text("results.grid_menu.copy_values.empty"),
                    cx,
                )
            });
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        if spill_truncated {
            let total = rv.read(cx).spill_scan_total(slot).unwrap_or(read_rows);
            rv.update(cx, |view, cx| {
                view.note_copy_notice(
                    crate::localization::format(
                        "results.grid_menu.copy_values.spill_sample_notice",
                        &[
                            ("copied_rows", &read_rows.to_string()),
                            ("total_rows", &total.to_string()),
                        ],
                    ),
                    cx,
                )
            });
        }
    }

    /// "Top Values…" (header menu): count the column's values and open the
    /// clickable frequency window. Windowed (spill-backed) sets are checked
    /// first and read their first [`crate::result_profiler::PROFILE_MAX_ROWS`]
    /// rows from disk — the resident window would undercount. Resident sets
    /// count the rows currently visible (display order already reflects
    /// header filters, and a builder tree rebuilds the grid from matching
    /// rows), the same rows the Copy Distinct/Top actions read. The window
    /// is bound to the current result and goes inert after a re-run.
    fn open_top_values(&self, column_index: usize, state: &GridState, cx: &mut App) {
        let Some(column) = state.data.columns.get(column_index) else {
            return;
        };
        let kind = column.kind;
        let column_name = column.name.clone();
        let Some(fmt) = state.resolved_formats.get(column_index).cloned() else {
            return;
        };
        let Some(rv) = self.results.upgrade() else {
            return;
        };
        // Spill rows and the quick filter a click applies speak result
        // columns; `column_index` (and this grid's own rows) are grid columns.
        let Some(result_column) = self.result_column(column_index, state, cx) else {
            return;
        };
        const SCAN_CAP: usize = crate::result_profiler::PROFILE_MAX_ROWS;
        let (epoch, spill) = {
            let rv = rv.read(cx);
            let spill = if self.pivot {
                None
            } else {
                rv.spill_scan_source(self.slot_index)
            };
            (rv.result_epoch(), spill)
        };
        let (cells, total_rows) = match spill {
            // Windowed: the window opens at once and counts in the
            // background — reading 100,000 rows from disk here would freeze
            // the UI thread.
            Some((path, total)) => (
                crate::top_values::TopValuesCells::Spill {
                    path,
                    column: result_column,
                    count: SCAN_CAP.min(total),
                },
                total,
            ),
            None => (
                crate::top_values::TopValuesCells::Ready(
                    state
                        .display_indices
                        .iter()
                        .take(SCAN_CAP)
                        .filter_map(|&i| state.data.rows.get(i))
                        .map(|row| {
                            row.get(column_index)
                                .cloned()
                                .unwrap_or(sqlly_datatable::CellValue::None)
                        })
                        .collect(),
                ),
                state.display_indices.len(),
            ),
        };
        crate::top_values::open_top_values_window(
            crate::top_values::TopValuesSource {
                results: self.results.clone(),
                slot: self.slot_index,
                epoch,
                column: result_column,
                kind,
                column_name,
                fmt,
                total_rows,
            },
            cells,
            cx,
        );
    }

    fn copy_sql_update(&self, request: &ContextMenuRequest, state: &GridState, cx: &mut App) {
        if crate::capabilities::row_editing(self.render_db(cx))
            .reason()
            .is_some()
        {
            return;
        }
        let Some(rv) = self.results.upgrade() else {
            return;
        };
        let Some((columns, rows, _)) =
            rv.read(cx)
                .resolve_visible_rows(self.slot_index, state, self.pivot, cx)
        else {
            return;
        };
        let sel = if selection_is_row_based(&state.selection) {
            state.selection.clone()
        } else if let Some(row) = request.clicked_row() {
            Selection::Row(row.display_row_index)
        } else {
            return;
        };
        let (_columns, rows) = selection_export_data(&columns, rows, &sel);
        if rows.is_empty() {
            return;
        }
        let slot = self.slot_index;
        rv.update(cx, |v, cx| v.request_copy_update(slot, rows, cx));
    }

    /// Profile one visible column and copy a bounded, null-aware Markdown
    /// summary without cloning the full in-memory or spill-backed result.
    fn do_profile(&self, column_index: usize, state: &GridState, cx: &mut App) {
        let Some(results) = self.results.upgrade() else {
            return;
        };
        // Result rows are indexed by result column, not grid column.
        let Some(column_index) = self.result_column(column_index, state, cx) else {
            return;
        };
        // A windowed result is profiled from disk in the background.
        if !self.pivot
            && results.update(cx, |view, cx| {
                view.start_column_scan(
                    self.slot_index,
                    column_index,
                    crate::results::ColumnScanPurpose::Profile,
                    cx,
                )
            })
        {
            return;
        }
        let results = results.read(cx);
        let Some(result_set) = results.result_set_at(self.slot_index) else {
            return;
        };
        let Some(column) = result_set.columns.get(column_index).cloned() else {
            return;
        };
        let limit = PROFILE_MAX_ROWS.saturating_add(1);
        let rows: Vec<Vec<CellValue>> = if state.display_indices.is_empty() {
            result_set
                .rows
                .iter()
                .take(limit)
                .map(|row| vec![row.get(column_index).cloned().unwrap_or(CellValue::Null)])
                .collect()
        } else {
            state
                .display_indices
                .iter()
                .take(limit)
                .filter_map(|&source_index| {
                    result_set
                        .rows
                        .get(results.source_index(self.slot_index, source_index))
                })
                .map(|row| vec![row.get(column_index).cloned().unwrap_or(CellValue::Null)])
                .collect()
        };
        let mut profiler = ResultProfiler::new(&[column]);
        profiler.push_rows(&rows);
        cx.write_to_clipboard(ClipboardItem::new_string(profiler.finish().to_markdown()));
    }

    /// Convert the grid's active column filters into WHERE predicates and
    /// apply them to the editor's SQL. Each non-empty filter becomes a
    /// `column LIKE '%filter%'` predicate; filters are AND'd together
    /// (matching the grid's filter semantics where all column filters must
    /// match).
    fn do_filters_to_where(&self, state: &GridState, cx: &mut App) {
        let Some(editor_handle) = &self.editor else {
            return;
        };
        let Some(editor) = editor_handle.upgrade() else {
            return;
        };
        let Some(rs) = self.result_set(cx) else {
            return;
        };

        let db = self.render_db(cx);
        let sql = editor.read(cx).text().to_string();
        // `filters` is per grid column; `rs.columns` per result column.
        let marker = self.marker_width(state, cx);
        let Some(combined) = crate::grid_filter_builder::column_filters_to_where(
            db,
            &rs.columns,
            state.filters.get(marker..).unwrap_or(&[]),
            Some(&sql),
        ) else {
            return;
        };

        let action = WhereAction {
            title: crate::localization::text("results.grid_menu.where.active_filters_title"),
            column_name: String::new(),
            column_index: 0,
            values: vec![],
            operator: WhereOperator::Equals,
            mode: WhereActionMode::Add,
            join: WhereLogicalJoin::And,
            predicate_preview: combined,
            source: None,
        };
        let result = where_actions::apply_where_action_for(db, &sql, &action);
        editor.update(cx, |ed, cx| {
            ed.set_text(result.updated_sql, cx);
        });
    }

    /// Whether the WHERE action behind `action_id` builds for this request,
    /// asked of the same [`build_where_action`] the click runs, with the
    /// result columns snapshotted at grid build. The menu has no `&App`,
    /// so the host's two lookups are stood in for from what the grid shows:
    ///
    /// - A cell cut to a preview carries the ` … [size]` marker in its grid
    ///   text (the host adds it to every truncated cell), which the builder
    ///   reads as cut whatever its truncation lookup says, so the stand-in
    ///   answers "not cut" and the text decides.
    /// - Raw bytes exist behind a cell only where the host has raw rows (not
    ///   on a pivot grid) and the cell may hold bytes: a binary column, or
    ///   a cell showing the `<N bytes>` stand-in (see
    ///   [`request_cell_looks_binary`]). There the stand-in answers with bytes, as
    ///   the host will. SQLite types dynamically, so the host may also find
    ///   bytes under a text column; every operator the builder offers on
    ///   such a value still builds (a pattern compares the bytes as text).
    ///
    /// With no columns snapshotted (a provider the host built without them)
    /// the menu's own gating stands alone. Match Row on a very wide row
    /// skips the dry run: reading every cell of a few hundred columns on
    /// each right-click costs more than the rare row it would hide.
    fn where_item_builds(&self, action_id: &str, request: &ContextMenuRequest) -> bool {
        const MATCH_ROW_DRY_RUN_MAX_COLUMNS: usize = 200;
        if self.columns.is_empty()
            || action_id == action_id::WHERE_FILTERS_TO_WHERE
            || action_id == action_id::WHERE_IS_NULL
            || action_id == action_id::WHERE_IS_NOT_NULL
        {
            return true;
        }
        if action_id == action_id::WHERE_MATCH_ROW
            && self.columns.len() > MATCH_ROW_DRY_RUN_MAX_COLUMNS
        {
            return true;
        }
        let raw_rows = !self.pivot;
        let marker = self.marker;
        let db = self.source_db;
        let columns = &self.columns;
        let dynamic_typing =
            db.is_some_and(|db| db.family() == sqlly_lang::sqlmodel::Dialect::Sqlite);
        let raw = |source_row: usize, result_col: usize| -> Option<CellValue> {
            if !raw_rows {
                return None;
            }
            let column = columns.get(result_col)?;
            let bytes_column = where_actions::classify_column_for(db, column)
                == where_actions::WhereValueKind::Bytes;
            let may_hold_bytes = bytes_column
                || request_cell_looks_binary(
                    request,
                    source_row,
                    result_col + marker,
                    dynamic_typing,
                );
            may_hold_bytes.then(|| CellValue::Bytes(vec![0]))
        };
        build_where_action(
            db,
            action_id,
            request,
            columns,
            None,
            marker,
            &raw,
            &|_, _| false,
        )
        .is_some()
    }

    /// Execute a WHERE action: build the predicate, apply it to the editor's
    /// SQL, and update the editor text.
    fn do_where_action(
        &self,
        action_id: &str,
        request: &ContextMenuRequest,
        rs: &ResultSet,
        state: &GridState,
        cx: &mut App,
    ) {
        let Some(editor_handle) = &self.editor else {
            return;
        };
        let Some(editor) = editor_handle.upgrade() else {
            return;
        };

        let sql = editor.read(cx).text().to_string();

        let db = self.source_db(cx);
        let marker = self.marker_width(state, cx);
        // Binary cells compare by their raw bytes, which only the host's
        // rows hold (the grid shows `<N bytes>`). Read through ResultsView
        // only, never this grid's state, which is leased right now.
        let window_offset = state.window.map(|w| w.offset);
        let action = {
            let app: &App = cx;
            let results = (!self.pivot)
                .then(|| self.results.upgrade())
                .flatten()
                .map(|results| results.read(app));
            let slot = self.slot_index;
            // Match row asks for several cells of one row. The absolute row
            // (what the truncation entries and the spill pager are keyed by)
            // is resolved once per grid row and shared by the cut check and
            // the raw read; the raw row itself is read once per row too. A
            // spilled row is one disk read; a resident row is only borrowed.
            let abs_rows = std::cell::RefCell::new(None::<(usize, usize)>);
            let abs_row = |row: usize| -> Option<usize> {
                let r = results?;
                let mut memo = abs_rows.borrow_mut();
                Some(match *memo {
                    Some((grid_row, abs)) if grid_row == row => abs,
                    _ => {
                        let abs = r.truncation_row_for_grid(slot, row, window_offset);
                        *memo = Some((row, abs));
                        abs
                    }
                })
            };
            let rows = RawRowCache::new(|row| results?.raw_row_at(slot, abs_row(row)?));
            // Only a byte cell is copied out: the grid text is exact for
            // everything else, and a row's big text cells stay put.
            let raw = |row: usize, col: usize| {
                rows.cell(row, |raw| {
                    raw.cells
                        .get(col)
                        .filter(|cell| matches!(cell, CellValue::Bytes(_)))
                        .cloned()
                })
            };
            // A cut cell's raw value is only a preview: no predicate.
            let truncated = |row: usize, col: usize| {
                results.is_some_and(|r| {
                    abs_row(row).is_some_and(|abs| r.truncated_cell(slot, abs, col).is_some())
                })
            };
            build_where_action(
                db,
                action_id,
                request,
                &rs.columns,
                Some(&sql),
                marker,
                &raw,
                &truncated,
            )
        };
        let Some(action) = action else {
            return;
        };

        let result = where_actions::apply_where_action_for(db.unwrap_or_default(), &sql, &action);
        editor.update(cx, |ed, cx| {
            ed.set_text(result.updated_sql, cx);
        });
    }
}

/// "Open Referenced Row" is offered on a non-NULL data cell. FK metadata is
/// resolved asynchronously by the host, the same rule as Edit Row.
fn offers_open_referenced_row(request: &ContextMenuRequest) -> bool {
    matches!(request.target, ContextMenuTarget::Cell { .. })
        && request
            .clicked_cell()
            .is_some_and(|c| !matches!(c.value, sqlly_datatable::CellValue::None))
}

/// Foreign Key Label header items appear on labelled FK column headers.
fn offers_fk_label_header(
    request: &ContextMenuRequest,
    headers: &std::collections::HashMap<usize, FkLabelHeaderMenuInfo>,
) -> bool {
    matches!(
        request.target,
        ContextMenuTarget::ColumnHeader { .. } | ContextMenuTarget::SortButton { .. }
    ) && request
        .target
        .column_index()
        .is_some_and(|idx| headers.contains_key(&idx))
}

impl ContextMenuProvider for ResultGridMenuProvider {
    fn menu_items(&self, request: &ContextMenuRequest) -> Vec<ContextMenuItem> {
        let mut items = Vec::new();

        // Try to read the result set for WHERE actions.
        // We use a dummy App-less read — but we can't call self.result_set
        // here because we don't have &App in menu_items. Instead, we rely
        // on the ContextMenuRequest's selected_cells/rows which carry the
        // grid cell values directly. For column metadata, we need the
        // ResultsView. Since we can't access App here, we'll build WHERE
        // items only when we have cell context from the request.
        //
        // For the column name, we can read it from SelectedCellContext.
        // For the column data type (needed for classify_value), we can't
        // get it here without App access. This is a limitation — the WHERE
        // menu items will be built with best-effort column info from the
        // request itself.

        // Copy items read the persisted copy-format preference. The provider
        // trait gives us no `App` handle, and menu opens are rare, so reading
        // the tiny JSON file here is the simplest correct source.
        let export_prefs = self
            .export_prefs_override
            .unwrap_or_else(ExportPreferences::load);

        // The single Export… item opens the consolidated dialog; the dialog
        // reads the persisted export preferences itself when it lists
        // formats, so the menu needs none of them here.
        // The pivot grid shows derived cells, so only the export dialog (and
        // the grid's own copy handling) apply — row editing, WHERE actions,
        // and profiling target source rows and are hidden here.
        if self.pivot {
            return vec![ContextMenuItem::action(
                action_id::EXPORT_DIALOG,
                crate::localization::text("results.grid_menu.pivot.export"),
            )];
        }

        // Layout (same conventions as the explorer menus): the everyday
        // actions sit at the top level in at most four groups, and the long
        // tail lives in named submenus. Every former item stays reachable
        // (guarded by `layout_tests::grid_menus_keep_every_action`).
        let sep = ContextMenuItem::separator;
        let sub = ContextMenuItem::submenu;
        let act = ContextMenuItem::action;

        // Grid-wide tools shared by every target, plus export destinations
        // other than the dialog and the layout switch.
        let tools = |extra: Vec<ContextMenuItem>| {
            let mut items = vec![
                // Grid-wide value search across every column of this result set.
                act(
                    action_id::FIND_IN_GRID,
                    crate::localization::text("results.grid_menu.tools.find_replace"),
                ),
                act(
                    action_id::CLIENT_STATISTICS,
                    crate::localization::text("results.grid_menu.tools.client_statistics"),
                ),
                // Transpose crashed from here while the host call ran inside
                // the grid's own update; it is deferred like every host
                // hand-off now.
                act(
                    action_id::TRANSPOSE,
                    crate::localization::text("results.grid_menu.tools.transpose"),
                ),
            ];
            items.extend(extra);
            items.push(sep());
            items.push(act(
                action_id::OPEN_IN_SPREADSHEET,
                crate::localization::text("results.grid_menu.tools.open_spreadsheet"),
            ));
            items.push(act(
                action_id::SAVE_SNAPSHOT,
                crate::localization::text("results.grid_menu.tools.save_snapshot"),
            ));
            items.push(sep());
            items.push(sub(
                crate::localization::text("results.grid_menu.tools.layout_submenu"),
                Self::layout_items(),
            ));
            sub(
                crate::localization::text("results.grid_menu.tools.submenu"),
                items,
            )
        };

        match request.target {
            ContextMenuTarget::ColumnHeader { .. } | ContextMenuTarget::SortButton { .. } => {
                // Standard column-header items (sort, filter, copy column),
                // split between the top level and the Sort & Filter and
                // Column submenus.
                let mut top_copy = Vec::new();
                let mut top_sort = Vec::new();
                let mut sort_filter = Vec::new();
                let mut column = Vec::new();
                for item in ContextMenuItem::standard_column_header_items() {
                    let ContextMenuItem::BuiltIn(action) = item else {
                        continue;
                    };
                    use sqlly_datatable::MenuAction as M;
                    match action {
                        M::CopyColumn | M::CopyColumnWithHeaders => top_copy.push(item),
                        M::SortAscending | M::SortDescending | M::FilterPrompt => {
                            top_sort.push(item);
                        }
                        M::ClearSort | M::ClearFilter => sort_filter.push(item),
                        // Widen / Narrow sit right after the grid's own Move
                        // Column Left/Right: the menu-driven alternative to
                        // dragging a header edge. Only a target with a
                        // column gets them; there is nothing to resize
                        // otherwise.
                        M::MoveColumnRight => {
                            column.push(item);
                            column.extend(Self::column_width_items(request));
                        }
                        _ => column.push(item),
                    }
                }
                sort_filter.extend([
                    act(
                        action_id::SORT_ADD,
                        crate::localization::text("results.grid_menu.sort_filter.add_ascending"),
                    ),
                    act(
                        action_id::SORT_DESC,
                        crate::localization::text("results.grid_menu.sort_filter.descending"),
                    ),
                    act(
                        action_id::SORT_REMOVE,
                        crate::localization::text("results.grid_menu.sort_filter.remove"),
                    ),
                    sep(),
                    act(
                        action_id::FILTER_BUILDER,
                        crate::localization::text("results.grid_menu.sort_filter.filter_rows"),
                    ),
                ]);
                column.extend([
                    sep(),
                    act(
                        action_id::HIDE_EMPTY_COLUMNS,
                        crate::localization::text("results.grid_menu.column.hide_empty"),
                    ),
                    act(
                        action_id::GOTO_COLUMN,
                        crate::localization::text("results.grid_menu.column.go_to"),
                    ),
                ]);
                top_sort.push(sub(
                    crate::localization::text("results.grid_menu.sort_filter.submenu"),
                    sort_filter,
                ));

                let values = vec![
                    act(
                        action_id::COPY_DISTINCT_VALUES,
                        crate::localization::text("results.grid_menu.values.copy_distinct"),
                    ),
                    ContextMenuItem::action(
                        action_id::COPY_TOP_DISTINCT_VALUES,
                        crate::localization::format(
                            "results.grid_menu.values.copy_top_frequent",
                            &[("value_count", &COPY_TOP_VALUES_LIMIT.to_string())],
                        ),
                    ),
                    act(
                        action_id::TOP_VALUES,
                        crate::localization::text("results.grid_menu.values.top_values"),
                    ),
                    sep(),
                    act(
                        action_id::PROFILE_DATA,
                        crate::localization::text("results.grid_menu.values.profile_column"),
                    ),
                    act(
                        action_id::PROFILE_COPY_MARKDOWN,
                        crate::localization::text("results.grid_menu.values.copy_profile_markdown"),
                    ),
                    act(
                        action_id::PROFILE_ALL,
                        crate::localization::text("results.grid_menu.values.profile_all"),
                    ),
                ];

                let mut format = vec![
                    act(
                        action_id::FORMAT_COLUMN,
                        crate::localization::text("results.grid_menu.format.header_override"),
                    ),
                    act(
                        action_id::CONDITIONAL_FORMAT,
                        crate::localization::text("results.grid_menu.format.conditional"),
                    ),
                    sub(
                        crate::localization::text("results.grid_menu.format.alignment_submenu"),
                        Self::alignment_items(),
                    ),
                ];
                // Rounding — only for columns whose type can carry decimals;
                // an unknown type keeps the option available.
                if offers_rounding(&self.column_types, request, self.pivot) {
                    format.push(sub(
                        crate::localization::text("results.grid_menu.format.rounding_submenu"),
                        Self::rounding_items(),
                    ));
                }
                if offers_fk_label_header(request, &self.fk_label_headers) {
                    let col_idx = request.target.column_index().expect("checked above");
                    let info = self.fk_label_headers.get(&col_idx).expect("checked above");
                    format.push(sub(
                        crate::localization::text("results.grid_menu.format.foreign_key_submenu"),
                        Self::fk_label_items(info),
                    ));
                }
                format.push(sub(
                    crate::localization::text("results.grid_menu.format.masking_submenu"),
                    Self::masking_items(request),
                ));

                items.extend(top_copy);
                items.push(sep());
                items.extend(top_sort);
                items.push(sep());
                items.push(sub(
                    crate::localization::text("results.grid_menu.column.submenu"),
                    column,
                ));
                items.push(sub(
                    crate::localization::text("results.grid_menu.values.submenu"),
                    values,
                ));
                items.push(sub(
                    crate::localization::text("results.grid_menu.format.header_submenu"),
                    format,
                ));
                // Excel-style computed columns: edit this one's formula, or
                // add a new column next to it.
                let clicked_computed = request
                    .target
                    .column_index()
                    .and_then(|c| c.checked_sub(self.marker))
                    .and_then(|c| self.columns.get(c))
                    .is_some_and(|c| {
                        self.computed_columns
                            .iter()
                            .any(|n| n.eq_ignore_ascii_case(&c.name))
                    });
                if clicked_computed {
                    items.push(act(
                        action_id::EDIT_FORMULA,
                        crate::localization::text("results.grid_menu.computed.edit_formula"),
                    ));
                }
                items.push(act(
                    action_id::ADD_COMPUTED_COLUMN,
                    crate::localization::text("results.grid_menu.computed.add_column"),
                ));
                // One Export… item opens the consolidated export dialog;
                // format choice and destination live there.
                items.push(sep());
                items.push(act(
                    action_id::EXPORT_DIALOG,
                    crate::localization::text("results.grid_menu.header.export"),
                ));
                items.push(tools(Vec::new()));
            }
            ContextMenuTarget::Cell { .. } | ContextMenuTarget::RowHeader { .. } => {
                // Row editing — row-header clicks only, and only when the
                // request resolves a clicked row. The menu stays cheap:
                // editability (provenance, keys) is resolved asynchronously
                // by the editor dialog itself, which explains unsupported
                // results instead of hiding the action. "View Row as Form"
                // and "Edit Row…" are the eye/pencil icons in the row-header
                // gutter (see the grid's `row_actions`), so they are not here.
                if matches!(request.target, ContextMenuTarget::RowHeader { .. })
                    && request.clicked_row().is_some()
                {
                    items.push(sub(
                        crate::localization::text("results.grid_menu.row.submenu"),
                        vec![
                            act(
                                action_id::DUPLICATE_ROW,
                                crate::localization::text("results.grid_menu.row.duplicate"),
                            ),
                            act(
                                action_id::INSERT_ROW,
                                crate::localization::text("results.grid_menu.row.insert"),
                            ),
                            act(
                                action_id::BATCH_INSERT,
                                crate::localization::text("results.grid_menu.row.batch_insert"),
                            ),
                            sep(),
                            act(
                                action_id::STAGE_EDIT_ROW,
                                crate::localization::text("results.grid_menu.row.stage_edit"),
                            ),
                            act(
                                action_id::STAGE_DUPLICATE_ROW,
                                crate::localization::text("results.grid_menu.row.stage_duplicate"),
                            ),
                            act(
                                action_id::STAGE_DUPLICATE_ROW_NO_KEY,
                                crate::localization::text(
                                    "results.grid_menu.row.stage_duplicate_without_key",
                                ),
                            ),
                            act(
                                action_id::STAGE_INSERT_ROW,
                                crate::localization::text("results.grid_menu.row.stage_insert"),
                            ),
                            act(
                                action_id::STAGE_DELETE_ROW,
                                crate::localization::text("results.grid_menu.row.stage_delete"),
                            ),
                            act(
                                action_id::UNSTAGE_ROW,
                                crate::localization::text("results.grid_menu.row.revert_staged"),
                            ),
                        ],
                    ));
                }

                // Copy: the default format and "with headers" at the top, the
                // one-off formats in Copy As.
                items.push(ContextMenuItem::action(
                    action_id::COPY_SELECTION,
                    crate::localization::format(
                        "results.grid_menu.copy.default_format",
                        &[(
                            "format_name",
                            &copy_format_label(export_prefs.default_copy_format),
                        )],
                    ),
                ));
                items.push(act(
                    action_id::COPY_SELECTION_WITH_HEADERS,
                    crate::localization::text("results.grid_menu.copy.with_headers"),
                ));
                let mut copy_as = vec![
                    act(
                        action_id::COPY_HEADERS,
                        crate::localization::text("results.grid_menu.copy_as.headers"),
                    ),
                    sep(),
                ];
                for fmt in CopyFormat::ALL {
                    if fmt == export_prefs.default_copy_format {
                        continue;
                    }
                    copy_as.push(ContextMenuItem::action(
                        copy_as_action_id(fmt),
                        crate::localization::format(
                            "results.grid_menu.copy_as.format",
                            &[("format_name", &copy_format_label(fmt))],
                        ),
                    ));
                }
                copy_as.push(sep());
                if single_column_from_request(request).is_some() {
                    copy_as.push(act(
                        action_id::COPY_SQL_IN_LIST,
                        crate::localization::text("results.grid_menu.copy_as.sql_in_list"),
                    ));
                    copy_as.push(act(
                        action_id::COPY_SQL_IN_LIST_DISTINCT,
                        crate::localization::text("results.grid_menu.copy_as.sql_in_list_distinct"),
                    ));
                }
                let row_based = request.clicked_row().is_some() || request.selected_row_count() > 0;
                if row_based {
                    copy_as.push(act(
                        action_id::COPY_SQL_UPDATE,
                        crate::localization::text("results.grid_menu.copy_as.sql_update"),
                    ));
                }

                // Structured data (if applicable): the viewer opens from the
                // top level; "Copy Pretty Formatted" is a copy format.
                let mut view = Vec::new();
                let clicked_bytes_column = request
                    .target
                    .column_index()
                    .and_then(|grid_col| self.bytes_column_at(grid_col));
                for item in Self::structured_data_items(request, clicked_bytes_column) {
                    match &item {
                        ContextMenuItem::Separator => {}
                        ContextMenuItem::Action { id, .. }
                            if id == action_id::STRUCTURED_DATA_COPY =>
                        {
                            copy_as.insert(0, item);
                        }
                        _ => view.push(item),
                    }
                }
                items.push(sub(
                    crate::localization::text("results.grid_menu.copy_as.submenu"),
                    copy_as,
                ));
                items.push(act(
                    action_id::SELECT_ALL,
                    crate::localization::text("results.grid_menu.selection.select_all"),
                ));

                items.push(sep());
                items.extend(view);
                if offers_open_referenced_row(request) {
                    items.push(act(
                        action_id::FK_OPEN_REFERENCED,
                        crate::localization::text("results.grid_menu.view.referenced_row"),
                    ));
                }

                // WHERE actions — comprehensive set built from the request's
                // cell context: Equals, NotEquals, IS NULL, IS NOT NULL,
                // LIKE, IN, NOT IN, BETWEEN, and Match Row.
                let mut where_items = Self::where_items_from_request(
                    self.source_db,
                    // The snapshot is keyed by grid column (the host shifts
                    // it past a staged-edit marker column, like the FK label
                    // headers).
                    &|grid_col| self.column_types.get(grid_col).map(String::as_str),
                    request,
                    !self.pivot,
                );
                // Every WHERE item the menu lists must build when clicked:
                // run the click's own builder on it now and drop any that
                // would not, rather than a click that does nothing.
                where_items.retain(|item| match item {
                    ContextMenuItem::Action { id, .. } if id.starts_with("where_") => {
                        self.where_item_builds(id, request)
                    }
                    _ => true,
                });
                // The most common WHERE item (= for one value, IN for several)
                // is on the top level; the rest are in Filter.
                let promoted = [action_id::WHERE_EQUALS, action_id::WHERE_IN]
                    .into_iter()
                    .find_map(|wanted| {
                        where_items.iter().position(
                            |i| matches!(i, ContextMenuItem::Action { id, .. } if id == wanted),
                        )
                    })
                    .map(|at| where_items.remove(at));

                // Quick filter — seeds the grid's filter bar from this cell
                // and applies immediately (no editor rewrite).
                let mut filter = Self::quick_filter_items(request);
                filter.push(sep());
                filter.extend(where_items);
                filter.push(sep());
                filter.push(act(
                    action_id::WHERE_FILTERS_TO_WHERE,
                    crate::localization::text("results.grid_menu.filter.active_to_where"),
                ));
                filter.push(act(
                    action_id::FILTER_BUILDER,
                    crate::localization::text("results.grid_menu.filter.rows_builder"),
                ));

                items.push(sep());
                items.extend(promoted);
                items.push(sub(
                    crate::localization::text("results.grid_menu.filter.submenu"),
                    filter,
                ));
                if matches!(request.target, ContextMenuTarget::Cell { .. }) {
                    items.push(sub(
                        crate::localization::text("results.grid_menu.edit_cell.submenu"),
                        Self::cell_action_items(request, self.via_relay, clicked_bytes_column),
                    ));
                }
                items.push(sub(
                    crate::localization::text("results.grid_menu.format.cell_submenu"),
                    vec![
                        act(
                            action_id::FORMAT_COLUMN,
                            crate::localization::text("results.grid_menu.format.cell_override"),
                        ),
                        sub(
                            crate::localization::text(
                                "results.grid_menu.format.cell_masking_submenu",
                            ),
                            Self::masking_items(request),
                        ),
                    ],
                ));

                // One Export… item opens the consolidated export dialog;
                // format choice and destination live there. Chart brings
                // forward the grid's own Chart tab over this result set.
                items.push(sep());
                items.push(act(
                    action_id::EXPORT_DIALOG,
                    crate::localization::text("results.grid_menu.cell.export"),
                ));
                let mut cell_tools = vec![
                    sep(),
                    act(
                        action_id::CHART_VIEW,
                        crate::localization::text("results.grid_menu.tools.chart"),
                    ),
                    act(
                        action_id::SAVE_CHART_TO_LIBRARY,
                        crate::localization::text("results.grid_menu.tools.save_chart"),
                    ),
                    act(
                        action_id::SHOW_ON_MAP,
                        crate::localization::text("map_view.grid_menu.show_on_map"),
                    ),
                ];
                // Redis key listings: act on the selected keys.
                cell_tools.extend(redis_keys::menu_items(self.source_db, &self.columns));
                items.push(tools(cell_tools));
            }
        }

        // Dropping WHERE items that would not build can leave the dividers
        // that framed them behind.
        let mut items = collapse_separators(items);
        if items.is_empty() {
            items.push(ContextMenuItem::action(
                "noop",
                crate::localization::text("results.grid_menu.empty.no_actions"),
            ));
        }

        items
    }

    fn on_action(
        &self,
        action_id: &str,
        request: &ContextMenuRequest,
        state: &mut GridState,
        cx: &mut App,
    ) {
        // Row indices in `request` are resident-relative while the grid is
        // windowed over a spill file. Copy the offset out now: the handlers
        // below run inside this state's update, so ResultsView cannot read
        // it later without double-leasing the entity.
        let window_offset = state.window.map(|w| w.offset);
        let backing_row = |row: usize| crate::results::backing_row_for_grid_row(window_offset, row);
        match action_id {
            id if redis_keys::SelectedKeysAction::from_action_id(id).is_some() => {
                self.redis_selected_keys_action(id, request, state, cx);
            }
            // Every arm that hands off to the host `ResultsView` goes
            // through `defer_host`: this callback runs inside the grid's own
            // update, and the host methods read or rebuild that grid.
            action_id::FIND_IN_GRID => {
                self.defer_host(cx, |view, cx| view.toggle_grid_find(cx));
            }
            action_id::HIDE_EMPTY_COLUMNS => {
                let slot = self.slot_index;
                self.defer_host(cx, move |view, cx| view.hide_empty_columns(slot, cx));
            }
            action_id::GOTO_COLUMN => {
                self.defer_host(cx, |view, cx| view.open_goto_column(cx));
            }
            action_id::FILTER_BUILDER => {
                let slot = self.slot_index;
                self.defer_host(cx, move |view, cx| view.open_filter_builder(slot, cx));
            }
            action_id::QUICK_FILTER_EQ
            | action_id::QUICK_FILTER_NE
            | action_id::QUICK_FILTER_CONTAINS
            | action_id::QUICK_FILTER_IS_NULL
            | action_id::QUICK_FILTER_IS_NOT_NULL => {
                // Deferred: applying the filter rewrites this slot's grid
                // state, and this callback fires from inside the grid's own
                // update — mutating inline is a double-lease (every host
                // hand-off here defers for the same reason).
                // The cell's displayed text: text operators match what the
                // grid shows (thousands separators and all).
                let display = request.clicked_cell().and_then(|clicked| {
                    state
                        .resolved_formats
                        .get(clicked.column_index)
                        .map(|fmt| sqlly_datatable::format::format_cell(&clicked.value, fmt).0)
                });
                let Some(mut row) = quick_filter_row(action_id, request, display.as_deref()) else {
                    return;
                };
                // Filter models index result columns; the click is on a grid
                // column (the staged-edit marker column shifts them).
                if let crate::grid_filter_builder::ColumnTarget::Index(grid_col) = row.column {
                    let Some(result_col) = self.result_column(grid_col, state, cx) else {
                        return;
                    };
                    row.column = crate::grid_filter_builder::ColumnTarget::Index(result_col);
                }
                let slot = self.slot_index;
                self.defer_host(cx, move |rv, cx| rv.apply_quick_filter(slot, row, cx));
            }
            action_id::COPY_DISTINCT_VALUES => {
                let Some(column_index) = request.target.column_index() else {
                    return;
                };
                self.copy_distinct_values(column_index, None, state, cx);
            }
            action_id::COPY_TOP_DISTINCT_VALUES => {
                let Some(column_index) = request.target.column_index() else {
                    return;
                };
                self.copy_distinct_values(column_index, Some(COPY_TOP_VALUES_LIMIT), state, cx);
            }
            action_id::TOP_VALUES => {
                let Some(column_index) = request.target.column_index() else {
                    return;
                };
                self.open_top_values(column_index, state, cx);
            }
            action_id::SORT_ADD => {
                let Some(column_index) = request.target.column_index() else {
                    return;
                };
                let slot = self.slot_index;
                self.defer_host(cx, move |view, cx| view.add_to_sort(slot, column_index, cx));
            }
            action_id::SORT_DESC => {
                let Some(column_index) = request.target.column_index() else {
                    return;
                };
                let slot = self.slot_index;
                self.defer_host(cx, move |view, cx| {
                    view.sort_column_descending(slot, column_index, cx)
                });
            }
            action_id::SORT_REMOVE => {
                let Some(column_index) = request.target.column_index() else {
                    return;
                };
                let slot = self.slot_index;
                self.defer_host(cx, move |view, cx| {
                    view.remove_from_sort(slot, column_index, cx)
                });
            }
            action_id::CLIENT_STATISTICS => {
                self.defer_host(cx, |view, cx| view.open_client_statistics(cx));
            }
            action_id::TRANSPOSE => {
                let slot = self.slot_index;
                self.defer_host(cx, move |view, cx| view.toggle_transpose(slot, cx));
            }
            action_id::CELL_DETAILS => {
                let ContextMenuTarget::Cell {
                    display_row_index,
                    column_index,
                    ..
                } = &request.target
                else {
                    return;
                };
                // The panel follows the grid selection, so select the
                // clicked cell; showing the panel is the host's job.
                state.selection = Selection::Cell(*display_row_index, *column_index);
                self.defer_host(cx, |rv, cx| rv.request_cell_detail_open(cx));
            }
            action_id::FK_OPEN_REFERENCED => {
                let Some(clicked) = request.clicked_cell() else {
                    return;
                };
                // The host resolves the FK by result column.
                let Some(column_index) = self.result_column(clicked.column_index, state, cx) else {
                    return;
                };
                let slot = self.slot_index;
                let row = backing_row(clicked.source_row_index);
                self.defer_host(cx, move |rv, cx| {
                    rv.request_follow_foreign_key(slot, row, column_index, cx)
                });
            }
            id if cell_generator_action(id).is_some() => {
                let (Some(clicked), Some(action)) =
                    (request.clicked_cell(), cell_generator_action(id))
                else {
                    return;
                };
                let slot = self.slot_index;
                let row = backing_row(clicked.source_row_index);
                let column = clicked.column_index;
                self.defer_host(cx, move |rv, cx| {
                    rv.request_cell_action(slot, row, column, action, cx);
                });
            }
            action_id::CELL_SET_NULL
            | action_id::CELL_SET_DEFAULT
            | action_id::CELL_SERVER_TIME
            | action_id::CELL_SET_TRUE
            | action_id::CELL_SET_FALSE => {
                let Some(clicked) = request.clicked_cell() else {
                    return;
                };
                let action = match action_id {
                    action_id::CELL_SET_NULL => crate::cell_actions::CellAction::SetNull,
                    action_id::CELL_SET_DEFAULT => crate::cell_actions::CellAction::SetDefault,
                    action_id::CELL_SERVER_TIME => crate::cell_actions::CellAction::ServerTime,
                    action_id::CELL_SET_TRUE => {
                        crate::cell_actions::CellAction::SetBool(Some(true))
                    }
                    action_id::CELL_SET_FALSE => {
                        crate::cell_actions::CellAction::SetBool(Some(false))
                    }
                    _ => return,
                };
                let slot = self.slot_index;
                let row = backing_row(clicked.source_row_index);
                let column = clicked.column_index;
                self.defer_host(cx, move |rv, cx| {
                    rv.request_cell_action(slot, row, column, action, cx);
                });
            }
            action_id::CELL_LOAD_FILE => {
                let Some(clicked) = request.clicked_cell() else {
                    return;
                };
                #[cfg(target_arch = "wasm32")]
                {
                    let _ = clicked;
                }
                #[cfg(not(target_arch = "wasm32"))]
                {
                    let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
                        files: true,
                        directories: false,
                        multiple: false,
                        prompt: None,
                    });
                    let results = self.results.clone();
                    let slot = self.slot_index;
                    let source_row_index = backing_row(clicked.source_row_index);
                    let column_index = clicked.column_index;
                    cx.spawn(async move |cx| {
                        if let Ok(Ok(Some(paths))) = rx.await
                            && let Some(path) = paths.into_iter().next()
                        {
                            let _ = results.update(cx, |rv, cx| {
                                rv.request_cell_action(
                                    slot,
                                    source_row_index,
                                    column_index,
                                    crate::cell_actions::CellAction::LoadFromFile(path),
                                    cx,
                                );
                            });
                        }
                    })
                    .detach();
                }
            }
            action_id::CELL_SAVE_FILE => {
                let Some(clicked) = request.clicked_cell() else {
                    return;
                };
                #[cfg(target_arch = "wasm32")]
                {
                    let _ = clicked;
                }
                #[cfg(not(target_arch = "wasm32"))]
                {
                    let hint = format!("{}.bin", clicked.column_name);
                    let rx = cx.prompt_for_new_path(std::path::Path::new("."), Some(&hint));
                    let results = self.results.clone();
                    let slot = self.slot_index;
                    let source_row_index = backing_row(clicked.source_row_index);
                    let column_index = clicked.column_index;
                    cx.spawn(async move |cx| {
                        if let Ok(Ok(Some(path))) = rx.await {
                            let _ = results.update(cx, |rv, cx| {
                                rv.request_cell_action(
                                    slot,
                                    source_row_index,
                                    column_index,
                                    crate::cell_actions::CellAction::SaveToFile(path),
                                    cx,
                                );
                            });
                        }
                    })
                    .detach();
                }
            }
            action_id::SAVE_SNAPSHOT => {
                let slot = self.slot_index;
                self.defer_host(cx, move |view, cx| view.open_snapshot_prompt(slot, cx));
            }
            action_id::ADD_COMPUTED_COLUMN | action_id::EDIT_FORMULA => {
                let column = request
                    .target
                    .column_index()
                    .and_then(|c| self.result_column(c, state, cx))
                    .and_then(|c| self.result_set(cx)?.columns.get(c).map(|c| c.name.clone()));
                let slot = self.slot_index;
                let add = action_id == action_id::ADD_COMPUTED_COLUMN;
                self.defer_host(cx, move |view, cx| {
                    // Edit opens the clicked computed column; Add always
                    // starts a new column placed after the clicked one.
                    if add {
                        view.open_formula_bar_after(slot, column, cx);
                    } else {
                        view.open_formula_bar(slot, column, cx);
                    }
                });
            }
            action_id::CONDITIONAL_FORMAT => {
                if let Some(column_index) = request
                    .target
                    .column_index()
                    .and_then(|c| self.result_column(c, state, cx))
                    && let Some(results) = self.results.upgrade()
                {
                    let column = results
                        .read(cx)
                        .result_set_at(self.slot_index)
                        .and_then(|rs| rs.columns.get(column_index))
                        .map(|column| column.name.clone());
                    if let Some(column) = column {
                        self.defer_host(cx, move |rv, cx| {
                            rv.open_conditional_formatting(column, cx);
                        });
                    }
                }
            }
            action_id::VIEW_ROW_FORM => {
                let Some(clicked) = request.clicked_row() else {
                    return;
                };
                let slot = self.slot_index;
                let source_row_index = backing_row(clicked.source_row_index);
                self.defer_host(cx, move |rv, cx| {
                    rv.open_row_form(slot, source_row_index, cx);
                });
            }
            action_id::EDIT_ROW | action_id::DUPLICATE_ROW | action_id::INSERT_ROW => {
                let Some(clicked) = request.clicked_row() else {
                    return;
                };
                let intent = match action_id {
                    action_id::DUPLICATE_ROW => crate::row_editor::RowEditorIntent::Duplicate,
                    action_id::INSERT_ROW => crate::row_editor::RowEditorIntent::Insert,
                    _ => crate::row_editor::RowEditorIntent::Edit,
                };
                let slot = self.slot_index;
                let source_row_index = backing_row(clicked.source_row_index);
                self.defer_host(cx, move |rv, cx| {
                    rv.open_row_editor_with(slot, source_row_index, intent, cx);
                });
            }
            action_id::STAGE_EDIT_ROW
            | action_id::STAGE_DUPLICATE_ROW
            | action_id::STAGE_DUPLICATE_ROW_NO_KEY
            | action_id::STAGE_INSERT_ROW
            | action_id::BATCH_INSERT => {
                let Some(clicked) = request.clicked_row() else {
                    return;
                };
                let intent = match action_id {
                    action_id::STAGE_DUPLICATE_ROW => crate::row_editor::RowEditorIntent::Duplicate,
                    action_id::STAGE_DUPLICATE_ROW_NO_KEY => {
                        crate::row_editor::RowEditorIntent::DuplicateWithoutKey
                    }
                    action_id::STAGE_INSERT_ROW => crate::row_editor::RowEditorIntent::Insert,
                    action_id::BATCH_INSERT => crate::row_editor::RowEditorIntent::BatchInsert,
                    _ => crate::row_editor::RowEditorIntent::Edit,
                };
                let slot = self.slot_index;
                let source_row_index = backing_row(clicked.source_row_index);
                self.defer_host(cx, move |rv, cx| {
                    rv.open_row_editor_stage_with(slot, source_row_index, intent, cx);
                });
            }
            action_id::STAGE_DELETE_ROW => {
                let selected = request.selected_rows();
                if selected.len() > 1 {
                    let indices: Vec<usize> = selected
                        .iter()
                        .map(|row| backing_row(row.source_row_index))
                        .collect();
                    let slot = self.slot_index;
                    self.defer_host(cx, move |rv, cx| {
                        rv.request_stage_delete_many(slot, indices, cx);
                    });
                    return;
                }
                let Some(clicked) = request.clicked_row() else {
                    return;
                };
                let slot = self.slot_index;
                let source_row_index = backing_row(clicked.source_row_index);
                self.defer_host(cx, move |rv, cx| {
                    rv.open_row_editor_stage_with(
                        slot,
                        source_row_index,
                        crate::row_editor::RowEditorIntent::Edit,
                        cx,
                    );
                });
            }
            action_id::UNSTAGE_ROW => {
                let Some(clicked) = request.clicked_row() else {
                    return;
                };
                let slot = self.slot_index;
                let source_row_index = backing_row(clicked.source_row_index);
                self.defer_host(cx, move |rv, cx| {
                    rv.unstage_row_at(slot, source_row_index, cx);
                });
            }
            action_id::SELECT_ALL => {
                state.select_all();
            }
            // Deferred: `copy_selection_default` reads the grid entity, but this
            // action fires from inside the grid's own render/update, where that
            // entity is already leased — reading it there panics ("cannot read
            // … while it is already being updated"). Run it after the update
            // settles, as the other grid-reading actions here already do.
            action_id::COPY_SELECTION => {
                let slot = self.slot_index;
                self.defer_host(cx, move |v, cx| v.copy_selection_default(slot, false, cx));
            }
            action_id::COPY_SELECTION_WITH_HEADERS => {
                let slot = self.slot_index;
                self.defer_host(cx, move |v, cx| v.copy_selection_default(slot, true, cx));
            }
            // Actions that export the visible rows: a windowed result's rows
            // are read off the UI thread first, then the action runs again.
            action_id::COPY_AS_TSV
            | action_id::COPY_AS_CSV
            | action_id::COPY_AS_JSON
            | action_id::COPY_AS_JSON_LINES
            | action_id::COPY_AS_MARKDOWN
            | action_id::COPY_AS_INSERT
            | action_id::COPY_SQL_IN_LIST
            | action_id::COPY_SQL_IN_LIST_DISTINCT
            | action_id::COPY_SQL_UPDATE
            | action_id::EXPORT_DIALOG
                if self.defer_until_export_rows(action_id, request, cx) => {}
            action_id::OPEN_IN_SPREADSHEET
                if self.defer_until_export_rows(action_id, request, cx) => {}
            action_id::COPY_AS_TSV
            | action_id::COPY_AS_CSV
            | action_id::COPY_AS_JSON
            | action_id::COPY_AS_JSON_LINES
            | action_id::COPY_AS_MARKDOWN
            | action_id::COPY_AS_INSERT => {
                if let Some(fmt) = copy_format_for_action_id(action_id) {
                    self.copy_export(fmt.export_format(), state, cx);
                }
            }
            action_id::COPY_SQL_IN_LIST | action_id::COPY_SQL_IN_LIST_DISTINCT => {
                self.copy_sql_in_list(action_id == action_id::COPY_SQL_IN_LIST_DISTINCT, state, cx);
            }
            action_id::COPY_SQL_UPDATE => {
                self.copy_sql_update(request, state, cx);
            }
            action_id::COPY_HEADERS => {
                if let Some(rs) = self.result_set(cx) {
                    let payload = headers_payload(&rs);
                    cx.write_to_clipboard(ClipboardItem::new_string(payload));
                }
            }
            action_id::EXPORT_DIALOG => {
                if let Some(payload) = self.resolve_export_payload(state, cx) {
                    let prefs = self
                        .export_prefs_override
                        .unwrap_or_else(ExportPreferences::load);
                    let db = self.render_db(cx);
                    crate::result_export_dialog::open_result_export_dialog(
                        payload,
                        &prefs,
                        db,
                        self.results.clone(),
                        cx,
                    );
                }
            }
            action_id::OPEN_IN_SPREADSHEET => {
                if let Some(payload) = self.resolve_export_payload(state, cx) {
                    let prefs = self
                        .export_prefs_override
                        .unwrap_or_else(ExportPreferences::load);
                    let slot = payload.slot;
                    if payload.spill_truncated && payload.whole_set {
                        self.defer_host(cx, move |view, cx| {
                            view.note_truncated_export_for_slot(slot, cx)
                        });
                    }
                    self.defer_host(cx, move |view, cx| {
                        view.note_preview_cells_in_export(slot, cx);
                    });
                    let columns = payload.columns;
                    let rows = payload.rows;
                    // The grid workbook (formats, formulas, pivot sheet) is
                    // assembled on the host; the write and launch run in the
                    // background.
                    self.defer_host(cx, move |view, cx| {
                        view.prepare_excel_export(slot, cx);
                        let built = view.spreadsheet_payload(slot, &prefs, &columns, &rows, cx);
                        #[cfg(not(target_arch = "wasm32"))]
                        {
                            let task = crate::open_in_spreadsheet::spawn_open(cx, move || built);
                            cx.spawn(async move |this, cx| {
                                if let Err(message) = task.await {
                                    let _ = this.update(cx, |view, cx| {
                                        view.note_export_write_failed_messages(message, cx);
                                    });
                                }
                            })
                            .detach();
                        }
                        // Browser build: the workbook downloads (see
                        // `open_in_spreadsheet::download`).
                        #[cfg(target_arch = "wasm32")]
                        if let Some((bytes, format)) = built
                            && let Err(message) =
                                crate::open_in_spreadsheet::download(&bytes, format)
                        {
                            view.note_export_write_failed_messages(message, cx);
                        }
                    });
                }
            }
            action_id::CHART_VIEW => {
                let slot = self.slot_index;
                self.defer_host(cx, move |view, cx| view.open_result_chart_tab(slot, cx));
            }
            action_id::SAVE_CHART_TO_LIBRARY => {
                let slot = self.slot_index;
                self.defer_host(cx, move |view, cx| {
                    view.request_save_chart_to_library(slot, cx)
                });
            }
            action_id::SHOW_ON_MAP => {
                let slot = self.slot_index;
                if self.pivot {
                    // Pivot rows are aggregates: map them as plain rows.
                    if let Some(rs) = self.result_set(cx) {
                        let title = crate::localization::format(
                            "map_view.result_title",
                            &[("number", slot + 1)],
                        );
                        cx.defer(move |cx| {
                            crate::map_view::open_rows_map(&title, rs.columns, rs.rows, cx)
                        });
                    }
                } else {
                    let results = self.results.clone();
                    cx.defer(move |cx| {
                        if let Some(rv) = results.upgrade() {
                            crate::map_view::open_for_result(&rv, slot, cx);
                        }
                    });
                }
            }
            action_id::PROFILE_DATA => {
                if let Some(column_index) = request
                    .target
                    .column_index()
                    .and_then(|c| self.result_column(c, state, cx))
                {
                    let slot = self.slot_index;
                    self.defer_host(cx, move |view, cx| {
                        view.open_result_profile_for_column(slot, column_index, cx)
                    });
                }
            }
            action_id::PROFILE_COPY_MARKDOWN => {
                if let Some(column_index) = request.target.column_index() {
                    self.do_profile(column_index, state, cx);
                }
            }
            action_id::PROFILE_ALL => {
                let slot = self.slot_index;
                self.defer_host(cx, move |view, cx| view.open_result_profile(slot, cx));
            }
            action_id::STRUCTURED_DATA_COPY => {
                if let Some(clicked) = request.clicked_cell() {
                    let cell = self
                        .raw_cell(
                            backing_row(clicked.source_row_index),
                            clicked.column_index,
                            window_offset.is_some(),
                            cx,
                        )
                        .unwrap_or_else(|| CellValue::String(clicked_text(&clicked.value)));
                    if let CellValue::String(ref s) = cell
                        && let Some(kind) = structured_data::detect(s)
                    {
                        let pretty = structured_data::pretty_print(s, kind);
                        cx.write_to_clipboard(ClipboardItem::new_string(pretty));
                    }
                }
            }
            action_id::STRUCTURED_DATA_PRETTY
            | action_id::VIEW_IMAGE
            | action_id::VIEW_GEOMETRY
            | action_id::VIEW_AS_HEX => {
                if let Some(clicked) = request.clicked_cell() {
                    self.open_clicked_preview(&clicked, window_offset, cx);
                }
            }
            action_id::VIEW_FULL_VALUE => {
                let Some(clicked) = request.clicked_cell() else {
                    return;
                };
                // Truncation entries are keyed by result column.
                let Some(col) = self.result_column(clicked.column_index, state, cx) else {
                    return;
                };
                let slot = self.slot_index;
                let source_row = clicked.source_row_index;
                self.defer_host(cx, move |rv, cx| {
                    // Spill-backed grids hold a window: truncation entries
                    // and the pager are keyed by the absolute row.
                    let source_row = rv.truncation_row_for_grid(slot, source_row, window_offset);
                    let purpose = rv
                        .truncated_cell(slot, source_row, col)
                        .map(|t| {
                            if t.kind == sqlly_services::TruncatedKind::Bytes {
                                crate::cell_hydration::HydratePurpose::Hex
                            } else {
                                crate::cell_hydration::HydratePurpose::Viewer
                            }
                        })
                        .unwrap_or(crate::cell_hydration::HydratePurpose::Viewer);
                    rv.request_cell_hydration(slot, source_row, col, purpose, cx);
                });
            }
            id if id.starts_with("where_") => {
                if id == action_id::WHERE_FILTERS_TO_WHERE {
                    self.do_filters_to_where(state, cx);
                } else if let Some(rs) = self.result_set(cx) {
                    self.do_where_action(id, request, &rs, state, cx);
                }
            }
            action_id::LAYOUT_STACKED | action_id::LAYOUT_TABBED | action_id::LAYOUT_CANVAS => {
                let mode = match action_id {
                    action_id::LAYOUT_STACKED => crate::results::LayoutMode::Stacked,
                    action_id::LAYOUT_TABBED => crate::results::LayoutMode::Tabbed,
                    _ => crate::results::LayoutMode::Canvas,
                };
                self.defer_host(cx, move |rv, cx| rv.set_layout_mode(mode, cx));
            }
            action_id::RESET_COLUMN_LAYOUT => {
                let slot = self.slot_index;
                self.defer_host(cx, move |view, cx| view.reset_column_layout(slot, cx));
            }
            action_id::WIDEN_COLUMN | action_id::NARROW_COLUMN => {
                let Some(column) = request.target.column_index() else {
                    return;
                };
                let slot = self.slot_index;
                let delta = if action_id == action_id::WIDEN_COLUMN {
                    40.0
                } else {
                    -40.0
                };
                self.defer_host(cx, move |view, cx| {
                    view.adjust_column_width(slot, column, delta, cx)
                });
            }
            action_id::FORMAT_COLUMN => {
                // Result rows and columns are indexed by result column;
                // `raw_cell` below takes the grid column.
                let Some(column_index) = request
                    .target
                    .column_index()
                    .and_then(|c| self.result_column(c, state, cx))
                else {
                    return;
                };
                let Some(column) = self
                    .result_set(cx)
                    .and_then(|result_set| result_set.columns.get(column_index).cloned())
                else {
                    return;
                };
                let Some(editor) = self.editor.as_ref().and_then(WeakEntity::upgrade) else {
                    return;
                };
                let sample = request
                    .clicked_cell()
                    .and_then(|cell| {
                        self.raw_cell(
                            backing_row(cell.source_row_index),
                            cell.column_index,
                            window_offset.is_some(),
                            cx,
                        )
                    })
                    .or_else(|| {
                        self.result_set(cx)
                            .and_then(|result_set| result_set.rows.first().cloned())
                            .and_then(|row| row.get(column_index).cloned())
                    });
                let column_name = column.name;
                let data_type = column.data_type;
                // The format dialog reads this grid back; open it once the
                // grid's update has settled.
                cx.defer(move |cx| {
                    editor.update(cx, |editor, cx| {
                        editor.open_result_column_format_dialog(
                            column_name,
                            &data_type,
                            sample,
                            cx,
                        );
                    });
                });
            }
            // The three reveal/re-mask arms rebuild the slot's grid, which
            // reads the grid entity — but this callback fires from INSIDE
            // that entity's update (`render_grid_view`), so running them
            // inline is a fatal double-lease. Defer past the update.
            action_id::MASK_REVEAL_COLUMN => {
                // Masks and reveals are kept per result column.
                let Some(col) = request
                    .target
                    .column_index()
                    .and_then(|c| self.result_column(c, state, cx))
                else {
                    return;
                };
                let slot = self.slot_index;
                self.defer_host(cx, move |view, cx| {
                    if view.masked_columns(slot).contains(&col) {
                        view.reveal_column(slot, col, cx);
                    }
                });
            }
            action_id::MASK_REVEAL_CELL => {
                let Some(clicked) = request.clicked_cell() else {
                    return;
                };
                let slot = self.slot_index;
                let row = self.map_row(clicked.source_row_index, cx);
                let Some(col) = self.result_column(clicked.column_index, state, cx) else {
                    return;
                };
                self.defer_host(cx, move |view, cx| {
                    if view.masked_columns(slot).contains(&col) {
                        view.reveal_cell(slot, row, col, cx);
                    }
                });
            }
            action_id::MASK_REMASK => {
                let slot = self.slot_index;
                self.defer_host(cx, move |view, cx| view.remask_all(slot, cx));
            }
            action_id::MASK_MANAGE_RULES => {
                self.defer_host(cx, |view, cx| view.request_open_masking_settings(cx));
            }
            action_id::MASK_EXCLUDE_ON_CONNECTION | action_id::MASK_INCLUDE_ON_CONNECTION => {
                let include = action_id == action_id::MASK_INCLUDE_ON_CONNECTION;
                let name = request
                    .clicked_cell()
                    .map(|cell| cell.column_name.clone())
                    .filter(|name| !name.is_empty())
                    .or_else(|| {
                        let col = self.result_column(request.target.column_index()?, state, cx)?;
                        self.result_set(cx)
                            .and_then(|rs| rs.columns.get(col).map(|c| c.name.clone()))
                    });
                let Some(column_name) = name else {
                    return;
                };
                self.defer_host(cx, move |view, cx| {
                    view.request_masking_override(column_name, include, cx);
                });
            }
            action_id::ROUNDING_0
            | action_id::ROUNDING_1
            | action_id::ROUNDING_2
            | action_id::ROUNDING_4
            | action_id::ROUNDING_6 => {
                let decimals = match action_id {
                    action_id::ROUNDING_0 => 0,
                    action_id::ROUNDING_1 => 1,
                    action_id::ROUNDING_2 => 2,
                    action_id::ROUNDING_4 => 4,
                    action_id::ROUNDING_6 => 6,
                    _ => return,
                };
                if let Some(col_idx) = request.target.column_index() {
                    Self::apply_rounding(state, col_idx, decimals);
                }
            }
            action_id::ALIGN_LEFT | action_id::ALIGN_CENTER | action_id::ALIGN_RIGHT => {
                let alignment = match action_id {
                    action_id::ALIGN_LEFT => sqlly_datatable::TextAlignment::Left,
                    action_id::ALIGN_CENTER => sqlly_datatable::TextAlignment::Center,
                    _ => sqlly_datatable::TextAlignment::Right,
                };
                if let Some(col_idx) = request.target.column_index() {
                    Self::apply_alignment(state, col_idx, alignment);
                }
            }
            action_id::FK_LABEL_TOGGLE => {
                self.defer_host(cx, |view, cx| {
                    let enabled = view.formatting_or_load().show_fk_labels;
                    view.set_show_fk_labels(!enabled, cx);
                });
            }
            id if action_id::parse_fk_label_column(id).is_some() => {
                let Some(col_idx) = request.target.column_index() else {
                    return;
                };
                let Some(info) = self.fk_label_headers.get(&col_idx) else {
                    return;
                };
                let Some(column_name) = action_id::parse_fk_label_column(id) else {
                    return;
                };
                let display_column = if column_name == "Off" {
                    None
                } else {
                    Some(column_name.to_string())
                };
                let local_column = info.local_column.clone();
                let slot = self.slot_index;
                self.defer_host(cx, move |view, cx| {
                    view.set_fk_label_column(slot, &local_column, display_column, cx);
                });
            }
            _ => {}
        }
    }
}

/// Tab-separated column-header payload for the "Copy Headers" action.
/// Mirrors `ResultSetExporter.headers(snapshot)` in the Swift app: all
/// column names joined by tabs.
fn headers_payload(rs: &ResultSet) -> String {
    rs.columns
        .iter()
        .map(|c| c.name.as_str())
        .collect::<Vec<_>>()
        .join("\t")
}

/// The filter-builder condition a "Filter: …" cell-menu action applies.
/// Native number/date cells compare numerically (`=` / `<>`) on the raw
/// value; every text operator (`is`, `is not`, `contains`) compares the
/// cell's `display` text, because that is what the grid shows and matches
/// against — a raw `1234` would never be found in a displayed `1,234`. Grid
/// text comparisons ignore case. `None` for a non-quick-filter action or a
/// request without a clicked cell. Pure, so it is unit-testable.
fn quick_filter_row(
    action: &str,
    request: &ContextMenuRequest,
    display: Option<&str>,
) -> Option<crate::grid_filter_builder::FilterRow> {
    use crate::grid_filter_builder::{BuilderOp, FilterRow};
    let clicked = request.clicked_cell()?;
    let numeric = matches!(
        clicked.value,
        sqlly_datatable::CellValue::Integer(_)
            | sqlly_datatable::CellValue::Decimal(_)
            | sqlly_datatable::CellValue::ExactDecimal(_)
            | sqlly_datatable::CellValue::Date(_)
    );
    let op = match action {
        action_id::QUICK_FILTER_EQ if numeric => BuilderOp::Eq,
        action_id::QUICK_FILTER_EQ => BuilderOp::Is,
        action_id::QUICK_FILTER_NE if numeric => BuilderOp::Ne,
        action_id::QUICK_FILTER_NE => BuilderOp::IsNot,
        action_id::QUICK_FILTER_CONTAINS => BuilderOp::Contains,
        action_id::QUICK_FILTER_IS_NULL => BuilderOp::IsNull,
        action_id::QUICK_FILTER_IS_NOT_NULL => BuilderOp::IsNotNull,
        _ => return None,
    };
    let mut row = FilterRow::new(clicked.column_index, op);
    if op.needs_a() {
        row.a = if op.is_number() {
            // The raw value: grouping separators or rounded decimals would
            // not parse back as a number operand.
            format_grid_cell_text(&clicked.value)
        } else {
            display
                .map(str::to_string)
                .unwrap_or_else(|| format_grid_cell_text(&clicked.value))
        };
    }
    Some(row)
}

/// "Copy Distinct Values" payload: distinct display values, one per line,
/// first-occurrence order. NULL appears as the standard `NULL` text. Pure,
/// so it is unit-testable.
pub(crate) fn distinct_column_lines(values: &[CellValue]) -> String {
    let mut seen = std::collections::HashSet::new();
    let mut lines = Vec::new();
    for value in values {
        let text = crate::result_export::cell_display(value);
        if seen.insert(text.clone()) {
            lines.push(text);
        }
    }
    lines.join("\n")
}

/// How many values "Copy Top N Values" copies.
const COPY_TOP_VALUES_LIMIT: usize = 25;

/// "Copy Top N Values" payload: the `n` most frequent display values, one
/// per line, most frequent first; ties keep first-occurrence order.
pub(crate) fn top_distinct_column_lines(values: &[CellValue], n: usize) -> String {
    let mut index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut counts: Vec<(String, usize)> = Vec::new();
    for value in values {
        let text = crate::result_export::cell_display(value);
        match index.get(&text) {
            Some(&i) => counts[i].1 += 1,
            None => {
                index.insert(text.clone(), counts.len());
                counts.push((text, 1));
            }
        }
    }
    // Stable sort keeps first-occurrence order among equal counts.
    counts.sort_by_key(|entry| std::cmp::Reverse(entry.1));
    counts
        .into_iter()
        .take(n)
        .map(|(text, _)| text)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Convert a grid `CellValue` back to a core `CellValue`.
/// Longest cell value inlined into a context-menu label.
const MENU_VALUE_MAX_CHARS: usize = 80;

/// Render a cell value for display *inside a menu label*: whitespace runs
/// collapse to single spaces and anything past [`MENU_VALUE_MAX_CHARS`] is
/// replaced by an ellipsis.
///
/// Without this a cell holding a JSON document (or any long string) widened
/// the whole context menu to the length of that value — the menu grew past
/// the window and every other item went with it. Only the label is clipped;
/// the value that builds the predicate is always the untouched original.
fn menu_value(text: &str) -> String {
    let flattened = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = flattened.chars();
    let clipped: String = chars.by_ref().take(MENU_VALUE_MAX_CHARS).collect();
    if chars.next().is_some() {
        format!("{clipped}…")
    } else {
        clipped
    }
}

fn grid_cell_to_core(grid: &sqlly_datatable::CellValue) -> CellValue {
    match grid {
        sqlly_datatable::CellValue::None => CellValue::Null,
        sqlly_datatable::CellValue::Boolean(b) => CellValue::Bool(*b),
        sqlly_datatable::CellValue::Integer(i) => CellValue::I64(*i),
        sqlly_datatable::CellValue::Decimal(f) => CellValue::F64(*f),
        sqlly_datatable::CellValue::ExactDecimal(d) => CellValue::String(d.as_str().to_owned()),
        sqlly_datatable::CellValue::Text(s) => CellValue::String(s.clone()),
        sqlly_datatable::CellValue::Date(_) => CellValue::Null, // No direct core equivalent
    }
}

/// Drops leading, trailing and back-to-back separators, so removing items
/// from a group never leaves an empty section or a doubled divider.
fn collapse_separators(items: Vec<ContextMenuItem>) -> Vec<ContextMenuItem> {
    let mut out: Vec<ContextMenuItem> = Vec::with_capacity(items.len());
    for item in items {
        // Submenus are tidied the same way, and one left with nothing to
        // choose is dropped rather than shown empty.
        let item = match item {
            ContextMenuItem::Submenu { label, items } => {
                let items = collapse_separators(items);
                if items.is_empty() {
                    continue;
                }
                ContextMenuItem::Submenu { label, items }
            }
            other => other,
        };
        let is_separator = matches!(item, ContextMenuItem::Separator);
        let after_separator = out
            .last()
            .is_none_or(|last| matches!(last, ContextMenuItem::Separator));
        if is_separator && after_separator {
            continue;
        }
        out.push(item);
    }
    if matches!(out.last(), Some(ContextMenuItem::Separator)) {
        out.pop();
    }
    out
}

/// Whether the grid cell the request carries at `(source_row, grid_col)`
/// (the clicked row's cells, then any other selected cell), in a column
/// whose type is not binary, may still hold bytes: it shows the grid's
/// `<N bytes>` stand-in, or, under an engine that types dynamically
/// (SQLite, where any column may hold a BLOB), text that looks binary. A
/// `0x…` value in a text column elsewhere is text, as the host's raw row
/// will say. Looked up in place: no cell is copied.
fn request_cell_looks_binary(
    request: &ContextMenuRequest,
    source_row: usize,
    grid_col: usize,
    dynamic_typing: bool,
) -> bool {
    let looks = |cell: &sqlly_datatable::CellValue| {
        matches!(cell, sqlly_datatable::CellValue::Text(t)
            if is_binary_placeholder(t) || (dynamic_typing && looks_like_binary_cell(t)))
    };
    if let Some(row) = request.clicked_row()
        && row.source_row_index == source_row
        && let Some(cell) = row.value_at(grid_col)
    {
        return looks(cell);
    }
    request
        .selected_cells()
        .iter()
        .find(|c| c.source_row_index == source_row && c.column_index == grid_col)
        .is_some_and(|c| looks(&c.value))
}

/// Format a grid cell value as display text for WHERE action previews.
pub(crate) fn format_grid_cell_text(grid: &sqlly_datatable::CellValue) -> String {
    match grid {
        sqlly_datatable::CellValue::None => "NULL".to_string(),
        sqlly_datatable::CellValue::Boolean(b) => b.to_string(),
        sqlly_datatable::CellValue::Integer(i) => i.to_string(),
        sqlly_datatable::CellValue::Decimal(f) => f.to_string(),
        sqlly_datatable::CellValue::ExactDecimal(d) => d.to_string(),
        sqlly_datatable::CellValue::Text(s) => s.clone(),
        sqlly_datatable::CellValue::Date(d) => d.to_string(),
    }
}

/// Build a `WhereAction` from an action ID and the context menu request.
/// How many result columns share `label` (case-insensitive) — the ambiguity
/// signal `column_provenance` needs to keep star expansion honest.
fn label_occurrences(columns: &[ResultColumn], label: &str) -> usize {
    columns
        .iter()
        .filter(|c| c.name.eq_ignore_ascii_case(label))
        .count()
}

/// `marker` is the width of the staged-edit marker prefix on the grid the
/// request came from: grid column `g` is result column `g - marker`, and the
/// provenance lookup and the action speak result columns.
/// [`build_where_action`] with no cell cut to a preview (tests; the host
/// passes its truncation entries).
#[cfg(test)]
fn build_where_action_from_id(
    db: sqlly_lang::sqlmodel::DatabaseType,
    action_id: &str,
    request: &ContextMenuRequest,
    rs: &ResultSet,
    sql: Option<&str>,
    marker: usize,
    raw: RawCellLookup<'_>,
) -> Option<WhereAction> {
    build_where_action(
        Some(db),
        action_id,
        request,
        &rs.columns,
        sql,
        marker,
        raw,
        &|_, _| false,
    )
}

/// Builds the WHERE action behind a menu id. Every cell's value goes
/// through [`predicate_value`], which asks `raw` for the bytes behind it
/// and `truncated` whether it is only a preview. `db` is the engine as the
/// host knows it, the same reading the menu classified with (`None` until
/// a query ran); the predicate renders in that dialect, or the default one.
#[allow(clippy::too_many_arguments)]
fn build_where_action(
    db: Option<sqlly_lang::sqlmodel::DatabaseType>,
    action_id: &str,
    request: &ContextMenuRequest,
    columns: &[ResultColumn],
    sql: Option<&str>,
    marker: usize,
    raw: RawCellLookup<'_>,
    truncated: TruncatedLookup<'_>,
) -> Option<WhereAction> {
    let clicked = request.clicked_cell()?;
    // Selected cells are compared in grid columns; everything else uses the
    // result column behind the click.
    let grid_col = clicked.column_index;
    let col_idx = grid_col.checked_sub(marker)?;
    let col = columns.get(col_idx)?;
    // The predicate must target the source expression behind the result
    // column (`p.Name` behind alias `PermissionName`), resolved from the
    // statement in the editor. `None` (computed column, un-attributable
    // star, multi-statement script) keeps the label-based rendering.
    let source = sql.and_then(|sql| {
        sqlly_intel::query::column_provenance::resolve_source_column(
            sql,
            &col.name,
            col_idx,
            label_occurrences(columns, &col.name),
        )
    });
    if action_id == action_id::WHERE_MATCH_ROW {
        // AND of equals for every column; the clicked cell is just one of
        // them, so it is not read ahead of the row here.
        return build_match_row_action(db, request, columns, sql, marker, raw, truncated);
    }
    let render_db = db.unwrap_or_default();
    // One selected cell of this column as the value it compares against,
    // classified the way the menu offered it.
    let bytes_column =
        where_actions::classify_column_for(db, col) == where_actions::WhereValueKind::Bytes;
    let read = CellRead::for_column(db, bytes_column);
    let value_for = |c: &sqlly_datatable::SelectedCellContext| -> Option<WhereSelectedValue> {
        let cv = predicate_value(&c.value, c.source_row_index, col_idx, read, raw, truncated)?;
        Some(WhereSelectedValue {
            cell_value: cv.clone(),
            display_text: format_grid_cell_text(&c.value),
            kind: where_actions::classify_value_for_type(db, &cv, Some(&col.data_type)),
            placeholder: false,
        })
    };
    let db = render_db;
    let operator = match action_id {
        action_id::WHERE_IS_NULL => WhereOperator::IsNull,
        action_id::WHERE_IS_NOT_NULL => WhereOperator::IsNotNull,
        action_id::WHERE_EQUALS => WhereOperator::Equals,
        action_id::WHERE_NOT_EQUALS => WhereOperator::NotEquals,
        action_id::WHERE_GREATER_THAN => WhereOperator::GreaterThan,
        action_id::WHERE_GREATER_THAN_OR_EQUAL => WhereOperator::GreaterThanOrEqual,
        action_id::WHERE_LESS_THAN => WhereOperator::LessThan,
        action_id::WHERE_LESS_THAN_OR_EQUAL => WhereOperator::LessThanOrEqual,
        action_id::WHERE_LIKE => WhereOperator::Like,
        action_id::WHERE_NOT_LIKE => WhereOperator::NotLike,
        action_id::WHERE_STARTS_WITH => WhereOperator::StartsWith,
        action_id::WHERE_ENDS_WITH => WhereOperator::EndsWith,
        action_id::WHERE_IN => {
            // Collect all selected values in this column
            let multi: Vec<WhereSelectedValue> = request
                .selected_cells()
                .iter()
                .filter(|c| c.column_index == grid_col)
                .map(value_for)
                .collect::<Option<Vec<_>>>()?;
            return where_actions::build_in_action_for(db, &col.name, col_idx, multi)?
                .with_source(db, source);
        }
        action_id::WHERE_NOT_IN => {
            // Collect all selected values in this column
            let multi: Vec<WhereSelectedValue> = request
                .selected_cells()
                .iter()
                .filter(|c| c.column_index == grid_col)
                .map(value_for)
                .collect::<Option<Vec<_>>>()?;
            let in_action = where_actions::build_in_action_for(db, &col.name, col_idx, multi)?
                .with_source(db, source)?;
            let action = WhereAction {
                operator: WhereOperator::NotIn,
                ..in_action
            };
            let preview = where_actions::render_predicate_for(db, &action)?;
            return Some(WhereAction {
                predicate_preview: preview,
                ..action
            });
        }
        action_id::WHERE_BETWEEN => {
            // Collect exactly 2 selected values in this column
            let multi: Vec<WhereSelectedValue> = request
                .selected_cells()
                .iter()
                .filter(|c| c.column_index == grid_col)
                .take(2)
                .map(value_for)
                .collect::<Option<Vec<_>>>()?;
            if multi.len() != 2 {
                return None;
            }
            let action = WhereAction {
                title: format!("WHERE {} BETWEEN ...", col.name),
                column_name: col.name.clone(),
                column_index: col_idx,
                values: multi,
                operator: WhereOperator::Between,
                mode: WhereActionMode::Add,
                join: WhereLogicalJoin::And,
                predicate_preview: String::new(),
                source,
            };
            let preview = where_actions::render_predicate_for(db, &action)?;
            return Some(WhereAction {
                title: format!("Add WHERE {}", col.name),
                predicate_preview: preview,
                ..action
            });
        }
        _ => return None,
    };

    // The NULL tests compare nothing, so they build whatever the cell shows
    // — a cut preview or a binary placeholder offers only these two.
    let selected = if operator.takes_value() {
        vec![value_for(&clicked)?]
    } else {
        vec![]
    };

    let action = WhereAction {
        title: format!("WHERE {} {:?}", col.name, operator),
        column_name: col.name.clone(),
        column_index: col_idx,
        values: selected,
        operator,
        mode: WhereActionMode::Add,
        join: WhereLogicalJoin::And,
        predicate_preview: String::new(),
        source,
    };
    let preview = where_actions::render_predicate_for(db, &action)?;
    Some(WhereAction {
        title: format!("Add WHERE {}", col.name),
        predicate_preview: preview,
        ..action
    })
}

/// Build a match-row WHERE action: AND of equals predicates for every
/// column in the clicked row, rendered in the results' engine dialect.
fn build_match_row_action(
    db: Option<sqlly_lang::sqlmodel::DatabaseType>,
    request: &ContextMenuRequest,
    columns: &[ResultColumn],
    sql: Option<&str>,
    marker: usize,
    raw: RawCellLookup<'_>,
    truncated: TruncatedLookup<'_>,
) -> Option<WhereAction> {
    let row = request.clicked_row()?;
    // Parse the statement once; every column resolves against the same parse.
    let provenance =
        sql.and_then(sqlly_intel::query::column_provenance::StatementProvenance::parse);
    let mut predicates: Vec<String> = Vec::new();
    let render_db = db.unwrap_or_default();
    for (col_idx, col) in columns.iter().enumerate() {
        // The row's cells are grid cells: skip the marker prefix.
        let grid_val = row.value_at(col_idx + marker)?;
        // Every cell of the row is read anyway (one row read, cached).
        let bytes_column =
            where_actions::classify_column_for(db, col) == where_actions::WhereValueKind::Bytes;
        let core_val = predicate_value(
            grid_val,
            row.source_row_index,
            col_idx,
            CellRead::whole_row(bytes_column),
            raw,
            truncated,
        )?;
        // Same provenance rule as the single-column actions: write each
        // predicate against the resolved source column when known.
        let source = provenance
            .as_ref()
            .and_then(|p| p.resolve(&col.name, col_idx, label_occurrences(columns, &col.name)));
        let identifier =
            where_actions::qualified_identifier_for(render_db, source.as_ref(), &col.name);
        if matches!(core_val, CellValue::Null) {
            predicates.push(format!("{identifier} IS NULL"));
            continue;
        }
        // Same classification and rendering as the single-column actions,
        // so a rowversion shown as hex text compares as bytes here too.
        let selected = WhereSelectedValue {
            kind: where_actions::classify_value_for_type(db, &core_val, Some(&col.data_type)),
            display_text: format_grid_cell_text(grid_val),
            cell_value: core_val,
            placeholder: false,
        };
        let lit = where_actions::single_sql_literal_for(render_db, &selected)?;
        predicates.push(format!("{identifier} = {lit}"));
    }
    if predicates.is_empty() {
        return None;
    }
    Some(WhereAction {
        title: "Match row".to_string(),
        column_name: columns.first().map(|c| c.name.clone()).unwrap_or_default(),
        column_index: 0,
        values: vec![],
        operator: WhereOperator::Equals,
        mode: WhereActionMode::Add,
        join: WhereLogicalJoin::And,
        predicate_preview: predicates.join(" AND "),
        source: None,
    })
}

#[cfg(test)]
#[path = "result_grid_menu_layout_tests.rs"]
mod layout_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn separators_never_lead_trail_or_double_up() {
        let sep = ContextMenuItem::separator;
        let act = |id: &str| ContextMenuItem::action(id, id);
        let ids = |items: Vec<ContextMenuItem>| -> Vec<String> {
            items
                .into_iter()
                .map(|item| match item {
                    ContextMenuItem::Separator => "-".to_string(),
                    ContextMenuItem::Action { id, .. } => id,
                    ContextMenuItem::BuiltIn(_) => "builtin".to_string(),
                    ContextMenuItem::Submenu { label, .. } => format!("{label} >"),
                })
                .collect()
        };
        let tidy = collapse_separators(vec![sep(), act("a"), sep(), sep(), sep(), act("b"), sep()]);
        assert_eq!(ids(tidy), ["a", "-", "b"]);
        assert!(collapse_separators(vec![sep(), sep()]).is_empty());
        assert_eq!(ids(collapse_separators(vec![act("a")])), ["a"]);
    }

    /// A cell holding a JSON document must not widen the context menu to the
    /// length of that document.
    #[test]
    fn menu_value_clips_long_cells_and_flattens_whitespace() {
        assert_eq!(menu_value("short"), "short");
        assert_eq!(menu_value("{\n  \"a\": 1\n}"), "{ \"a\": 1 }");

        let long = "x".repeat(500);
        let clipped = menu_value(&long);
        assert!(clipped.ends_with('…'));
        assert_eq!(clipped.chars().count(), MENU_VALUE_MAX_CHARS + 1);

        // Exactly at the cap is not an ellipsis case.
        let exact = "y".repeat(MENU_VALUE_MAX_CHARS);
        assert_eq!(menu_value(&exact), exact);
    }

    #[test]
    fn distinct_column_lines_dedupes_in_first_occurrence_order() {
        let values = vec![
            CellValue::String("b".into()),
            CellValue::I64(1),
            CellValue::String("a".into()),
            CellValue::String("b".into()),
            CellValue::Null,
            CellValue::I64(1),
            CellValue::Null,
        ];
        assert_eq!(distinct_column_lines(&values), "b\n1\na\nNULL");
        assert_eq!(distinct_column_lines(&[]), "");
        assert_eq!(
            distinct_column_lines(&[CellValue::Bool(true), CellValue::Bool(true)]),
            "true"
        );
    }

    #[test]
    fn top_distinct_column_lines_orders_by_frequency() {
        let values = vec![
            CellValue::String("a".into()),
            CellValue::String("b".into()),
            CellValue::String("b".into()),
            CellValue::Null,
            CellValue::String("c".into()),
            CellValue::Null,
            CellValue::String("b".into()),
            CellValue::String("d".into()),
        ];
        // b×3, then a/NULL/c/d — NULL×2 beats a×1; a, c, d tie in order.
        assert_eq!(top_distinct_column_lines(&values, 10), "b\nNULL\na\nc\nd");
        assert_eq!(top_distinct_column_lines(&values, 2), "b\nNULL");
        assert_eq!(top_distinct_column_lines(&values, 0), "");
        assert_eq!(top_distinct_column_lines(&[], 5), "");
    }

    fn export_fixture() -> (Vec<ResultColumn>, Vec<Vec<CellValue>>) {
        let columns = ["a", "b", "c"]
            .iter()
            .map(|n| ResultColumn {
                name: (*n).into(),
                data_type: "int".into(),
                nullable: false,
            })
            .collect();
        let rows = (0..4)
            .map(|r| {
                (0..3)
                    .map(|c| CellValue::I64((r * 10 + c) as i64))
                    .collect()
            })
            .collect();
        (columns, rows)
    }

    #[test]
    fn binary_cell_recognizer_and_hex_parser() {
        assert!(looks_like_binary_cell("<128 bytes>"));
        assert!(looks_like_binary_cell("0xDEADBEEF"));
        assert!(looks_like_binary_cell("0x1234")); // prefixed digits-only is explicit
        assert!(looks_like_binary_cell("deadbeef"));
        assert!(!looks_like_binary_cell("hello"));
        assert!(!looks_like_binary_cell("<not bytes>"));
        assert!(!looks_like_binary_cell("0xABC")); // odd length
        // Digit-only strings are numeric IDs, not binary — no hex offer.
        assert!(!looks_like_binary_cell("12345678"));
        assert!(!looks_like_binary_cell("202608271234"));
        // Bare hex still needs enough length even with letters.
        assert!(!looks_like_binary_cell("dead"));

        assert_eq!(
            where_actions::hex_string_to_bytes("0xDEAD"),
            Some(vec![0xDE, 0xAD])
        );
        assert_eq!(
            where_actions::hex_string_to_bytes("deadbeef"),
            Some(vec![0xDE, 0xAD, 0xBE, 0xEF])
        );
        assert_eq!(where_actions::hex_string_to_bytes("xyz"), None);
        assert_eq!(where_actions::hex_string_to_bytes("abc"), None); // odd length
    }

    #[test]
    fn selection_export_none_and_single_cell_export_everything() {
        let (columns, rows) = export_fixture();
        for sel in [
            sqlly_datatable::Selection::None,
            sqlly_datatable::Selection::Cell(1, 1),
        ] {
            let (cols, out) = selection_export_data(&columns, rows.clone(), &sel);
            assert_eq!(cols.len(), 3);
            assert_eq!(out.len(), 4);
        }
    }

    #[test]
    fn selection_export_non_contiguous_rows() {
        let (columns, rows) = export_fixture();
        let sel = sqlly_datatable::Selection::Rows(vec![0, 3]);
        let (cols, out) = selection_export_data(&columns, rows, &sel);
        assert_eq!(cols.len(), 3);
        assert_eq!(out.len(), 2);
        assert_eq!(out[1][0], CellValue::I64(30));
    }

    #[test]
    fn selection_export_non_contiguous_columns() {
        let (columns, rows) = export_fixture();
        let sel = sqlly_datatable::Selection::Columns(vec![0, 2]);
        let (cols, out) = selection_export_data(&columns, rows, &sel);
        assert_eq!(
            cols.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            ["a", "c"]
        );
        assert_eq!(out.len(), 4);
        assert_eq!(out[1], vec![CellValue::I64(10), CellValue::I64(12)]);
    }

    #[test]
    fn selection_export_cell_range_rectangle() {
        let (columns, rows) = export_fixture();
        let sel = sqlly_datatable::Selection::CellRange(2, 1, 1, 2);
        let (cols, out) = selection_export_data(&columns, rows, &sel);
        assert_eq!(
            cols.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            ["b", "c"]
        );
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], vec![CellValue::I64(11), CellValue::I64(12)]);
    }

    #[test]
    fn selection_export_sparse_cells_fill_unselected_with_null() {
        let (columns, rows) = export_fixture();
        let sel = sqlly_datatable::Selection::Cells(vec![(0, 0), (2, 2)]);
        let (cols, out) = selection_export_data(&columns, rows, &sel);
        assert_eq!(
            cols.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            ["a", "c"]
        );
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], vec![CellValue::I64(0), CellValue::Null]);
        assert_eq!(out[1], vec![CellValue::Null, CellValue::I64(22)]);
    }

    #[test]
    fn grid_cell_to_core_none() {
        assert_eq!(
            grid_cell_to_core(&sqlly_datatable::CellValue::None),
            CellValue::Null
        );
    }

    #[test]
    fn grid_cell_to_core_boolean() {
        assert_eq!(
            grid_cell_to_core(&sqlly_datatable::CellValue::Boolean(true)),
            CellValue::Bool(true)
        );
    }

    #[test]
    fn grid_cell_to_core_integer() {
        assert_eq!(
            grid_cell_to_core(&sqlly_datatable::CellValue::Integer(42)),
            CellValue::I64(42)
        );
    }

    #[test]
    fn grid_cell_to_core_decimal() {
        assert_eq!(
            grid_cell_to_core(&sqlly_datatable::CellValue::Decimal(1.5)),
            CellValue::F64(1.5)
        );
    }

    #[test]
    fn grid_cell_to_core_exact_decimal_keeps_digits() {
        let cell = sqlly_datatable::CellValue::from_decimal_text("99999999999999999999.5")
            .unwrap_or_default();
        assert_eq!(
            grid_cell_to_core(&cell),
            CellValue::String("99999999999999999999.5".into())
        );
        assert_eq!(format_grid_cell_text(&cell), "99999999999999999999.5");
    }

    #[test]
    fn grid_cell_to_core_text() {
        assert_eq!(
            grid_cell_to_core(&sqlly_datatable::CellValue::Text("hello".into())),
            CellValue::String("hello".into())
        );
    }

    #[test]
    fn grid_cell_to_core_date_maps_to_null() {
        assert_eq!(
            grid_cell_to_core(&sqlly_datatable::CellValue::Date(12345)),
            CellValue::Null
        );
    }

    #[test]
    fn format_grid_cell_text_none() {
        assert_eq!(
            format_grid_cell_text(&sqlly_datatable::CellValue::None),
            "NULL"
        );
    }

    #[test]
    fn format_grid_cell_text_integer() {
        assert_eq!(
            format_grid_cell_text(&sqlly_datatable::CellValue::Integer(42)),
            "42"
        );
    }

    #[test]
    fn action_ids_are_unique() {
        let ids = [
            action_id::ADD_COMPUTED_COLUMN,
            action_id::EDIT_FORMULA,
            action_id::SELECT_ALL,
            action_id::COPY_SELECTION,
            action_id::COPY_SELECTION_WITH_HEADERS,
            action_id::COPY_HEADERS,
            action_id::COPY_AS_TSV,
            action_id::COPY_AS_CSV,
            action_id::COPY_AS_JSON,
            action_id::COPY_AS_JSON_LINES,
            action_id::COPY_AS_MARKDOWN,
            action_id::COPY_AS_INSERT,
            action_id::COPY_SQL_IN_LIST,
            action_id::COPY_SQL_IN_LIST_DISTINCT,
            action_id::COPY_SQL_UPDATE,
            action_id::EXPORT_DIALOG,
            action_id::OPEN_IN_SPREADSHEET,
            action_id::STRUCTURED_DATA_PRETTY,
            action_id::STRUCTURED_DATA_COPY,
            action_id::VIEW_AS_HEX,
            action_id::VIEW_IMAGE,
            action_id::VIEW_GEOMETRY,
            action_id::WHERE_EQUALS,
            action_id::WHERE_NOT_EQUALS,
            action_id::WHERE_GREATER_THAN,
            action_id::WHERE_GREATER_THAN_OR_EQUAL,
            action_id::WHERE_LESS_THAN,
            action_id::WHERE_LESS_THAN_OR_EQUAL,
            action_id::WHERE_IN,
            action_id::WHERE_NOT_IN,
            action_id::WHERE_LIKE,
            action_id::WHERE_NOT_LIKE,
            action_id::WHERE_STARTS_WITH,
            action_id::WHERE_ENDS_WITH,
            action_id::WHERE_IS_NULL,
            action_id::WHERE_IS_NOT_NULL,
            action_id::WHERE_MATCH_ROW,
            action_id::WHERE_BETWEEN,
            action_id::WHERE_FILTERS_TO_WHERE,
            action_id::LAYOUT_STACKED,
            action_id::LAYOUT_TABBED,
            action_id::LAYOUT_CANVAS,
            action_id::RESET_COLUMN_LAYOUT,
            action_id::SORT_ADD,
            action_id::SORT_DESC,
            action_id::SORT_REMOVE,
            action_id::HIDE_EMPTY_COLUMNS,
            action_id::GOTO_COLUMN,
            action_id::ROUNDING_0,
            action_id::ROUNDING_1,
            action_id::ROUNDING_2,
            action_id::ROUNDING_4,
            action_id::ROUNDING_6,
            action_id::FORMAT_COLUMN,
            action_id::ALIGN_LEFT,
            action_id::ALIGN_CENTER,
            action_id::ALIGN_RIGHT,
            action_id::MASK_REVEAL_COLUMN,
            action_id::MASK_REVEAL_CELL,
            action_id::MASK_REMASK,
            action_id::MASK_EXCLUDE_ON_CONNECTION,
            action_id::MASK_INCLUDE_ON_CONNECTION,
            action_id::EDIT_ROW,
            action_id::DUPLICATE_ROW,
            action_id::INSERT_ROW,
            action_id::STAGE_EDIT_ROW,
            action_id::STAGE_DUPLICATE_ROW,
            action_id::STAGE_DUPLICATE_ROW_NO_KEY,
            action_id::STAGE_INSERT_ROW,
            action_id::BATCH_INSERT,
            action_id::STAGE_DELETE_ROW,
            action_id::UNSTAGE_ROW,
            action_id::FILTER_BUILDER,
            action_id::TRANSPOSE,
            action_id::FK_OPEN_REFERENCED,
            action_id::COPY_DISTINCT_VALUES,
            action_id::COPY_TOP_DISTINCT_VALUES,
        ];
        let mut sorted = ids.to_vec();
        sorted.sort();
        for i in 1..sorted.len() {
            assert_ne!(sorted[i], sorted[i - 1], "duplicate action id");
        }
    }

    #[test]
    fn headers_payload_joins_column_names_with_tabs() {
        let rs = ResultSet {
            index: 0,
            columns: vec![
                ResultColumn {
                    name: "id".to_string(),
                    data_type: "INTEGER".to_string(),
                    nullable: false,
                },
                ResultColumn {
                    name: "name".to_string(),
                    data_type: "TEXT".to_string(),
                    nullable: true,
                },
                ResultColumn {
                    name: "created_at".to_string(),
                    data_type: "TIMESTAMP".to_string(),
                    nullable: true,
                },
            ],
            rows: Vec::new(),
            rows_affected: None,
            row_count_hint: 0,
            ..Default::default()
        };
        assert_eq!(headers_payload(&rs), "id\tname\tcreated_at");
    }

    #[test]
    fn headers_payload_empty_columns() {
        let rs = ResultSet {
            index: 0,
            columns: Vec::new(),
            rows: Vec::new(),
            rows_affected: None,
            row_count_hint: 0,
            ..Default::default()
        };
        assert_eq!(headers_payload(&rs), "");
    }

    #[test]
    fn cell_context_menu_request_can_be_constructed() {
        // Verify that a ContextMenuRequest for a Cell target can be
        // constructed with the expected fields, and clicked_cell()
        // resolves correctly. This is the request shape the provider
        // receives when a data cell is right-clicked.
        let request = ContextMenuRequest::for_test(
            ContextMenuTarget::Cell {
                display_row_index: 0,
                source_row_index: 0,
                column_index: 0,
            },
            None,
            vec![vec![sqlly_datatable::CellValue::Integer(42)]],
            vec![sqlly_datatable::ColumnContext {
                index: 0,
                name: "id".into(),
                kind: sqlly_datatable::ColumnKind::Integer,
            }],
        );

        let clicked = request.clicked_cell().expect("clicked cell should resolve");
        assert_eq!(clicked.column_index, 0);
        assert_eq!(clicked.column_name, "id");
        assert!(matches!(
            clicked.value,
            sqlly_datatable::CellValue::Integer(42)
        ));
    }

    #[test]
    fn header_context_menu_request_can_be_constructed() {
        // Verify a ColumnHeader target request — the other right-click
        // surface that also shows the export submenu.
        let request = ContextMenuRequest::for_test(
            ContextMenuTarget::ColumnHeader { column_index: 0 },
            None,
            vec![],
            vec![],
        );
        assert!(request.clicked_cell().is_none());
        assert_eq!(request.target.column_index(), Some(0));
    }

    /// Collect the action IDs from a WHERE menu item list.
    /// A type snapshot that answers `t` for every grid column.
    fn every_column(t: Option<&'static str>) -> impl Fn(usize) -> Option<&'static str> {
        move |_| t
    }

    fn where_item_ids(items: &[ContextMenuItem]) -> Vec<&str> {
        items
            .iter()
            .filter_map(|i| match i {
                ContextMenuItem::Action { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect()
    }

    fn cell_request(
        value: sqlly_datatable::CellValue,
        kind: sqlly_datatable::ColumnKind,
    ) -> ContextMenuRequest {
        ContextMenuRequest::for_test(
            ContextMenuTarget::Cell {
                display_row_index: 0,
                source_row_index: 0,
                column_index: 0,
            },
            None,
            vec![vec![value]],
            vec![sqlly_datatable::ColumnContext {
                index: 0,
                name: "col".into(),
                kind,
            }],
        )
    }

    fn binary_row_request(cell: sqlly_datatable::CellValue) -> ContextMenuRequest {
        ContextMenuRequest::for_test(
            ContextMenuTarget::Cell {
                display_row_index: 0,
                source_row_index: 0,
                column_index: 1,
            },
            None,
            vec![vec![sqlly_datatable::CellValue::Integer(7), cell]],
            vec![
                sqlly_datatable::ColumnContext {
                    index: 0,
                    name: "id".into(),
                    kind: sqlly_datatable::ColumnKind::Integer,
                },
                sqlly_datatable::ColumnContext {
                    index: 1,
                    name: "user_id".into(),
                    kind: sqlly_datatable::ColumnKind::Text,
                },
            ],
        )
    }

    /// SQLite types dynamically: a BLOB can sit under a TEXT column and the
    /// grid (with a `binary=utf8` directive) shows it as its text. The menu
    /// cannot see that the cell is bytes, offers the text patterns, and the
    /// click, which reads the raw bytes, still builds them: SQLite's LIKE
    /// reads a BLOB as its text. Elsewhere a pattern over bytes is refused,
    /// and bytes that are not UTF-8 spell no pattern.
    #[test]
    fn sqlite_patterns_over_raw_bytes_build_from_their_text() {
        let request = binary_row_request(sqlly_datatable::CellValue::Text("hello".into()));
        let columns = binary_result_set("TEXT").columns;
        let build = |db, raw: &dyn Fn(usize, usize) -> Option<CellValue>| {
            build_where_action(
                Some(db),
                action_id::WHERE_LIKE,
                &request,
                &columns,
                None,
                0,
                raw,
                &|_, _| false,
            )
            .map(|a| a.predicate_preview)
        };
        let utf8 = |_: usize, col: usize| (col == 1).then(|| CellValue::Bytes(b"hello".to_vec()));
        let preview = build(sqlly_lang::sqlmodel::DatabaseType::Sqlite, &utf8)
            .expect("a pattern over a SQLite BLOB builds");
        assert!(
            preview.contains("LIKE") && preview.contains("hello"),
            "{preview}"
        );
        let not_utf8 =
            |_: usize, col: usize| (col == 1).then(|| CellValue::Bytes(vec![0xFF, 0xFE]));
        assert_eq!(
            build(sqlly_lang::sqlmodel::DatabaseType::Sqlite, &not_utf8),
            None
        );
        // SQL Server keeps bytes in binary columns; a pattern over them is
        // refused rather than guessed at.
        let varbinary = binary_result_set("varbinary").columns;
        assert_eq!(
            build_where_action(
                Some(sqlly_lang::sqlmodel::DatabaseType::SqlServer),
                action_id::WHERE_LIKE,
                &request,
                &varbinary,
                None,
                0,
                &utf8,
                &|_, _| false,
            ),
            None
        );
    }

    fn binary_result_set(data_type: &str) -> ResultSet {
        ResultSet {
            index: 0,
            columns: vec![
                ResultColumn {
                    name: "id".into(),
                    data_type: "int".into(),
                    nullable: false,
                },
                ResultColumn {
                    name: "user_id".into(),
                    data_type: data_type.into(),
                    nullable: true,
                },
            ],
            ..Default::default()
        }
    }

    /// The raw row behind the test grid: `id = 7`, `user_id` = 16 bytes.
    fn binary_raw(_row: usize, col: usize) -> Option<CellValue> {
        match col {
            0 => Some(CellValue::I64(7)),
            1 => Some(CellValue::Bytes(vec![0x11; 16])),
            _ => None,
        }
    }

    /// Sequel Ace #2068: filtering on a binary UUID must compare the real
    /// bytes. The grid shows `<16 bytes>`; the predicate uses the raw value
    /// as the engine's hex literal.
    #[test]
    fn where_equals_on_a_binary_cell_uses_the_raw_bytes() {
        let request = binary_row_request(sqlly_datatable::CellValue::Text("<16 bytes>".into()));
        let hex = "11".repeat(16);
        for (db, data_type, expected) in [
            (
                sqlly_lang::sqlmodel::DatabaseType::MySql,
                "binary(16)",
                format!("`user_id` = 0x{hex}"),
            ),
            (
                sqlly_lang::sqlmodel::DatabaseType::Postgres,
                "bytea",
                format!("\"user_id\" = '\\x{hex}'"),
            ),
            (
                sqlly_lang::sqlmodel::DatabaseType::Sqlite,
                "BLOB",
                format!("\"user_id\" = X'{hex}'"),
            ),
        ] {
            let rs = binary_result_set(data_type);
            let action = build_where_action_from_id(
                db,
                action_id::WHERE_EQUALS,
                &request,
                &rs,
                None,
                0,
                &binary_raw,
            )
            .expect("binary equality builds");
            assert_eq!(action.predicate_preview, expected, "{db:?}");
            let row = build_where_action_from_id(
                db,
                action_id::WHERE_MATCH_ROW,
                &request,
                &rs,
                None,
                0,
                &binary_raw,
            )
            .expect("match row builds");
            assert!(
                row.predicate_preview.ends_with(&expected),
                "{db:?}: {}",
                row.predicate_preview
            );
        }
    }

    /// Without the raw value (nothing to look it up from) the placeholder
    /// text must never be written into the query as if it were the value.
    #[test]
    fn binary_placeholder_without_raw_value_builds_nothing() {
        let request = binary_row_request(sqlly_datatable::CellValue::Text("<16 bytes>".into()));
        let rs = binary_result_set("BLOB");
        for id in [action_id::WHERE_EQUALS, action_id::WHERE_MATCH_ROW] {
            assert!(
                build_where_action_from_id(
                    sqlly_lang::sqlmodel::DatabaseType::Sqlite,
                    id,
                    &request,
                    &rs,
                    None,
                    0,
                    &|_, _| None,
                )
                .is_none(),
                "{id}"
            );
        }
    }

    /// A cell cut to a preview (` … [19.5 KiB]`) is not the whole value: no
    /// equality predicate is written from it.
    #[test]
    fn truncated_preview_builds_no_value_predicate() {
        let request = binary_row_request(sqlly_datatable::CellValue::Text(
            "It's a long note … [2.4 MiB]".into(),
        ));
        let rs = binary_result_set("text");
        let action = build_where_action_from_id(
            sqlly_lang::sqlmodel::DatabaseType::Sqlite,
            action_id::WHERE_EQUALS,
            &request,
            &rs,
            None,
            0,
            &|_, col| (col == 1).then(|| CellValue::String("It's a long note".into())),
        );
        assert!(action.is_none());
        let items = ResultGridMenuProvider::where_items_from_request(
            Some(sqlly_lang::sqlmodel::DatabaseType::default()),
            &every_column(None),
            &request,
            true,
        );
        let ids = where_item_ids(&items);
        assert!(!ids.contains(&action_id::WHERE_EQUALS), "{ids:?}");
        assert!(!ids.contains(&action_id::WHERE_LIKE), "{ids:?}");
        assert!(ids.contains(&action_id::WHERE_IS_NOT_NULL), "{ids:?}");
        let quick = ResultGridMenuProvider::quick_filter_items(&request);
        assert_eq!(
            where_item_ids(&quick),
            [
                action_id::QUICK_FILTER_IS_NULL,
                action_id::QUICK_FILTER_IS_NOT_NULL
            ]
        );
    }

    /// The NULL tests are the only WHERE items offered on a cut preview or a
    /// binary placeholder, so they must build without a value to compare —
    /// including on a pivot grid, where there are no raw rows at all.
    #[test]
    fn null_tests_build_without_a_predicate_value() {
        for cell in [
            sqlly_datatable::CellValue::Text("It's a long note … [2.4 MiB]".into()),
            sqlly_datatable::CellValue::Text("<16 bytes>".into()),
        ] {
            let request = binary_row_request(cell.clone());
            let rs = binary_result_set("BLOB");
            for (id, expected) in [
                (action_id::WHERE_IS_NULL, "\"user_id\" IS NULL"),
                (action_id::WHERE_IS_NOT_NULL, "\"user_id\" IS NOT NULL"),
            ] {
                let action = build_where_action_from_id(
                    sqlly_lang::sqlmodel::DatabaseType::Sqlite,
                    id,
                    &request,
                    &rs,
                    None,
                    0,
                    &|_, _| None,
                )
                .unwrap_or_else(|| panic!("{id} builds for {cell:?}"));
                assert_eq!(action.predicate_preview, expected);
            }
        }
    }

    /// A MySQL/PostgreSQL `timestamp` cell is a date-time, not SQL Server's
    /// rowversion: equality on it must build (Beekeeper-style regression of
    /// the binary-key fix).
    #[test]
    fn where_equals_on_a_mysql_timestamp_cell_builds() {
        let request = binary_row_request(sqlly_datatable::CellValue::Text(
            "2024-05-01 10:00:00".into(),
        ));
        let rs = binary_result_set("timestamp");
        let action = build_where_action_from_id(
            sqlly_lang::sqlmodel::DatabaseType::MySql,
            action_id::WHERE_EQUALS,
            &request,
            &rs,
            None,
            0,
            &|_, col| (col == 1).then(|| CellValue::String("2024-05-01 10:00:00".into())),
        )
        .expect("timestamp equality builds");
        assert_eq!(
            action.predicate_preview,
            "`user_id` = '2024-05-01 10:00:00'"
        );
    }

    /// Match row needs the raw row once, for its binary column; it must not
    /// read the (possibly spilled) row again for every other column.
    #[test]
    fn match_row_reads_the_raw_row_once() {
        let request = ContextMenuRequest::for_test(
            ContextMenuTarget::Cell {
                display_row_index: 0,
                source_row_index: 0,
                column_index: 1,
            },
            None,
            vec![vec![
                sqlly_datatable::CellValue::Integer(7),
                sqlly_datatable::CellValue::Text("<16 bytes>".into()),
                sqlly_datatable::CellValue::Text("alice".into()),
            ]],
            vec![
                sqlly_datatable::ColumnContext {
                    index: 0,
                    name: "id".into(),
                    kind: sqlly_datatable::ColumnKind::Integer,
                },
                sqlly_datatable::ColumnContext {
                    index: 1,
                    name: "user_id".into(),
                    kind: sqlly_datatable::ColumnKind::Text,
                },
                sqlly_datatable::ColumnContext {
                    index: 2,
                    name: "name".into(),
                    kind: sqlly_datatable::ColumnKind::Text,
                },
            ],
        );
        let mut rs = binary_result_set("BLOB");
        rs.columns.push(ResultColumn {
            name: "name".into(),
            data_type: "text".into(),
            nullable: true,
        });
        let reads = std::cell::Cell::new(0usize);
        let rows = RawRowCache::new(|_row: usize| {
            reads.set(reads.get() + 1);
            Some(vec![
                CellValue::I64(7),
                CellValue::Bytes(vec![0x11; 16]),
                CellValue::String("alice".into()),
            ])
        });
        let raw = |row: usize, col: usize| rows.cell(row, |cells| cells.get(col).cloned());
        let action = build_where_action_from_id(
            sqlly_lang::sqlmodel::DatabaseType::Sqlite,
            action_id::WHERE_MATCH_ROW,
            &request,
            &rs,
            None,
            0,
            &raw,
        )
        .expect("match row builds");
        assert_eq!(
            action.predicate_preview,
            format!(
                "\"id\" = 7 AND \"user_id\" = X'{}' AND \"name\" = 'alice'",
                "11".repeat(16)
            )
        );
        assert_eq!(reads.get(), 1, "raw row read once, for the binary column");
    }

    /// A cell the host cut to a preview never becomes a value predicate,
    /// even when the grid text carries no ` … [size]` marker and the column
    /// is plain text: the host's truncation entry is asked first, before
    /// the raw row. The NULL tests, which compare nothing, still build.
    #[test]
    fn a_truncated_cell_without_a_marker_builds_no_value_predicate() {
        let request = binary_row_request(sqlly_datatable::CellValue::Text("hello".into()));
        let rs = binary_result_set("varchar(max)");
        let build = |action_id: &str| {
            build_where_action(
                Some(sqlly_lang::sqlmodel::DatabaseType::SqlServer),
                action_id,
                &request,
                &rs.columns,
                None,
                0,
                &|_, _| unreachable!("a cut cell never reads its raw row"),
                &|_row, col| col == 1,
            )
        };
        assert!(
            build(action_id::WHERE_EQUALS).is_none(),
            "a preview is not the value"
        );
        assert!(build(action_id::WHERE_LIKE).is_none());
        assert_eq!(
            build(action_id::WHERE_IS_NULL)
                .expect("the NULL test builds")
                .predicate_preview,
            "[user_id] IS NULL"
        );
    }

    /// The rounding submenu reads the type of the clicked column through
    /// the same grid-keyed snapshot as the WHERE menu: with a staged-edit
    /// marker at grid column 0, the snapshot is shifted by one, so grid
    /// column 2 is the result's second column.
    #[test]
    fn rounding_follows_the_marker_shifted_column_types() {
        let header = |column_index: usize| {
            ContextMenuRequest::for_test(
                ContextMenuTarget::ColumnHeader { column_index },
                None,
                vec![],
                vec![],
            )
        };
        // Result columns: [decimal, int]; grid columns: [marker, decimal, int].
        let types: Vec<String> = ["", "decimal", "int"].map(String::from).to_vec();
        assert_eq!(column_type_at(&types, &header(1)), Some("decimal"));
        assert_eq!(column_type_at(&types, &header(2)), Some("int"));
        assert!(offers_rounding(&types, &header(1), false));
        assert!(
            !offers_rounding(&types, &header(2), false),
            "int at grid column 2 gets no rounding"
        );
        // Past the snapshot (or no snapshot at all) the option stays.
        assert!(offers_rounding(&types, &header(3), false));
        assert!(offers_rounding(&[], &header(1), false));
    }

    /// The pivot and transposed grids type every synthetic column as text
    /// (`text`, `nvarchar`); that says nothing about the cells, so rounding
    /// stays on offer there, while a regular grid's `int` column gets none.
    #[test]
    fn rounding_stays_on_pivot_and_transposed_grids() {
        let header = |column_index: usize| {
            ContextMenuRequest::for_test(
                ContextMenuTarget::ColumnHeader { column_index },
                None,
                vec![],
                vec![],
            )
        };
        let synthetic: Vec<String> = ["text", "nvarchar"].map(String::from).to_vec();
        assert!(offers_rounding(&synthetic, &header(0), true));
        assert!(offers_rounding(&synthetic, &header(1), true));
        assert!(!offers_rounding(&synthetic, &header(0), false));
        let regular: Vec<String> = ["int"].map(String::from).to_vec();
        assert!(!offers_rounding(&regular, &header(0), false));
    }

    /// A byte value in a column the type map does not call binary (SQL
    /// Server `rowversion`, Oracle `RAW`, MySQL `bit`), shown as short hex
    /// the sniffer does not recognise either, still compares by its bytes.
    #[test]
    fn where_equals_on_short_hex_bytes_uses_the_raw_bytes() {
        let request = binary_row_request(sqlly_datatable::CellValue::Text("0x0A".into()));
        let rs = binary_result_set("rowversion");
        let action = build_where_action_from_id(
            sqlly_lang::sqlmodel::DatabaseType::SqlServer,
            action_id::WHERE_EQUALS,
            &request,
            &rs,
            None,
            0,
            &|_, col| (col == 1).then(|| CellValue::Bytes(vec![0x0A])),
        )
        .expect("rowversion equality builds");
        assert_eq!(action.predicate_preview, "[user_id] = 0x0A");
    }

    /// A BLOB in a column the engine does not call binary (SQLite TEXT, an
    /// expression column), shown as text the sniffer does not recognise
    /// (base64 from a `binary=` directive, or plain text): the raw row is
    /// asked whatever the type and text say, so the predicate is the bytes.
    #[test]
    fn where_equals_on_base64_in_an_untyped_column_uses_the_raw_bytes() {
        let request = binary_row_request(sqlly_datatable::CellValue::Text("AAECAw==".into()));
        let rs = binary_result_set("TEXT");
        let reads = std::cell::Cell::new(0usize);
        let action = build_where_action_from_id(
            sqlly_lang::sqlmodel::DatabaseType::Sqlite,
            action_id::WHERE_EQUALS,
            &request,
            &rs,
            None,
            0,
            &|_, col| {
                reads.set(reads.get() + 1);
                (col == 1).then(|| CellValue::Bytes(vec![0, 1, 2, 3]))
            },
        )
        .expect("equality builds");
        assert_eq!(action.predicate_preview, "\"user_id\" = X'00010203'");
        assert_eq!(reads.get(), 1, "the raw row is asked once");
    }

    /// A binary column whose cell the engine returned as text (a SQLite
    /// BLOB holding `hello`): there are no raw bytes to read, so the text
    /// is the value and compares as text, for one cell and for Match row,
    /// rather than building nothing.
    #[test]
    fn text_in_a_binary_column_compares_as_text() {
        let request = binary_row_request(sqlly_datatable::CellValue::Text("hello".into()));
        let rs = binary_result_set("BLOB");
        let db = sqlly_lang::sqlmodel::DatabaseType::Sqlite;
        let items = ResultGridMenuProvider::where_items_from_request(
            Some(db),
            &every_column(Some("BLOB")),
            &request,
            true,
        );
        let ids = where_item_ids(&items);
        assert!(ids.contains(&action_id::WHERE_EQUALS), "{ids:?}");
        let action = build_where_action_from_id(
            db,
            action_id::WHERE_EQUALS,
            &request,
            &rs,
            None,
            0,
            &|_, _| None,
        )
        .expect("text under a binary column still compares");
        assert_eq!(action.predicate_preview, "\"user_id\" = 'hello'");
        let row = build_where_action_from_id(
            db,
            action_id::WHERE_MATCH_ROW,
            &request,
            &rs,
            None,
            0,
            &|_, _| None,
        )
        .expect("match row builds");
        assert_eq!(
            row.predicate_preview,
            "\"id\" = 7 AND \"user_id\" = 'hello'"
        );
    }

    /// `<0 bytes>` is only the grid's empty-binary stand-in in a binary
    /// column; a text column holding that very text compares as text.
    #[test]
    fn empty_bytes_text_in_a_text_column_is_text() {
        let request = binary_row_request(sqlly_datatable::CellValue::Text("<0 bytes>".into()));
        let rs = binary_result_set("varchar(50)");
        let db = sqlly_lang::sqlmodel::DatabaseType::SqlServer;
        let items = ResultGridMenuProvider::where_items_from_request(
            Some(db),
            &every_column(Some("varchar(50)")),
            &request,
            true,
        );
        let ids = where_item_ids(&items);
        assert!(ids.contains(&action_id::WHERE_LIKE), "{ids:?}");
        let action = build_where_action_from_id(
            db,
            action_id::WHERE_EQUALS,
            &request,
            &rs,
            None,
            0,
            &|_, _| None,
        )
        .expect("the text compares");
        assert_eq!(action.predicate_preview, "[user_id] = N'<0 bytes>'");
    }

    /// The same cell on a pivot grid, with no raw row behind it (the
    /// host's lookup answers `None`): the text is all there is, and the
    /// menu offers only the NULL tests rather than a dead click.
    #[test]
    fn pivot_rowversion_hex_text_offers_no_text_patterns() {
        let request = binary_row_request(sqlly_datatable::CellValue::Text(
            "0x00000000000007D1".into(),
        ));
        let items = ResultGridMenuProvider::where_items_from_request(
            Some(sqlly_lang::sqlmodel::DatabaseType::SqlServer),
            &every_column(Some("rowversion")),
            &request,
            false,
        );
        let ids = where_item_ids(&items);
        // Hex text spells its bytes, so equality still builds without a
        // raw row; a pattern or range over bytes is never offered.
        assert!(ids.contains(&action_id::WHERE_EQUALS), "{ids:?}");
        for absent in [
            action_id::WHERE_LIKE,
            action_id::WHERE_STARTS_WITH,
            action_id::WHERE_GREATER_THAN,
        ] {
            assert!(
                !ids.contains(&absent),
                "{absent} offered on a pivot rowversion"
            );
        }
    }

    /// A `<16 bytes>` placeholder on a pivot grid: there are no raw rows to
    /// read the bytes from and the text does not spell them, so the menu
    /// offers only the NULL tests, never an equality that would build
    /// nothing when clicked.
    #[test]
    fn pivot_binary_placeholder_offers_only_the_null_tests() {
        let request = binary_row_request(sqlly_datatable::CellValue::Text("<16 bytes>".into()));
        for column_type in [Some("varbinary"), None] {
            let items = ResultGridMenuProvider::where_items_from_request(
                Some(sqlly_lang::sqlmodel::DatabaseType::SqlServer),
                &every_column(column_type),
                &request,
                false,
            );
            let ids = where_item_ids(&items);
            assert!(ids.contains(&action_id::WHERE_IS_NULL), "{ids:?}");
            assert!(ids.contains(&action_id::WHERE_IS_NOT_NULL), "{ids:?}");
            // Nothing that compares the value, Match Row included: the row
            // holds a placeholder nobody can read the bytes behind.
            for offered in &ids {
                assert!(
                    matches!(
                        *offered,
                        action_id::WHERE_IS_NULL | action_id::WHERE_IS_NOT_NULL
                    ),
                    "{offered} offered on a pivot placeholder ({column_type:?})"
                );
            }
        }
    }

    /// A view that never ran a query has no engine: a restored PostgreSQL
    /// result's `timestamp` is read as a date-time (ranges offered), not as
    /// SQL Server's binary rowversion (equality only).
    #[test]
    fn no_engine_reads_timestamp_as_a_date() {
        let request = binary_row_request(sqlly_datatable::CellValue::Text(
            "2024-05-01 10:00:00".into(),
        ));
        let items = ResultGridMenuProvider::where_items_from_request(
            None,
            &every_column(Some("timestamp")),
            &request,
            true,
        );
        let ids = where_item_ids(&items);
        assert!(ids.contains(&action_id::WHERE_EQUALS), "{ids:?}");
        assert!(ids.contains(&action_id::WHERE_GREATER_THAN), "{ids:?}");
        assert!(ids.contains(&action_id::WHERE_LESS_THAN), "{ids:?}");
        assert!(!ids.contains(&action_id::WHERE_LIKE), "{ids:?}");
        let grid =
            crate::grid_bridge::build_grid_data(None, &binary_result_set("timestamp").columns, &[])
                .unwrap();
        assert_eq!(grid.columns[1].kind, sqlly_datatable::ColumnKind::Date);
        // The executed action reads the engine the same way (none known),
        // so the range the menu offered builds, as a date-time literal in
        // the default dialect, rather than being refused as bytes.
        let action = build_where_action(
            None,
            action_id::WHERE_GREATER_THAN,
            &request,
            &binary_result_set("timestamp").columns,
            None,
            0,
            &|_, _| None,
            &|_, _| false,
        )
        .expect("menu and action agree on an unknown engine");
        assert_eq!(
            action.predicate_preview,
            "[user_id] > N'2024-05-01 10:00:00'"
        );
        assert_eq!(action.values[0].kind, where_actions::WhereValueKind::Date);
    }

    /// A number cell is exact as the grid shows it: on a spilled grid its
    /// row is not read from disk just to learn it holds no bytes. SQLite,
    /// where any column may hold bytes, still asks.
    #[test]
    fn where_equals_on_a_plain_cell_reads_no_raw_row() {
        let request = binary_row_request(sqlly_datatable::CellValue::Integer(42));
        let rs = binary_result_set("integer");
        let reads = std::cell::Cell::new(0usize);
        let counting = |_: usize, _: usize| {
            reads.set(reads.get() + 1);
            None
        };
        let action = build_where_action_from_id(
            sqlly_lang::sqlmodel::DatabaseType::Postgres,
            action_id::WHERE_EQUALS,
            &request,
            &rs,
            None,
            0,
            &counting,
        )
        .expect("equality builds");
        assert_eq!(action.predicate_preview, "\"user_id\" = 42");
        assert_eq!(reads.get(), 0, "no raw read for a number on PostgreSQL");
        build_where_action_from_id(
            sqlly_lang::sqlmodel::DatabaseType::Sqlite,
            action_id::WHERE_EQUALS,
            &request,
            &rs,
            None,
            0,
            &counting,
        )
        .expect("equality builds");
        assert_eq!(
            reads.get(),
            1,
            "SQLite types dynamically, so the row is asked"
        );
    }

    /// Match Row goes through the same reading of each cell as the executed
    /// action: a row holding a cut preview is never offered it, and one
    /// holding a `<N bytes>` placeholder only where raw rows can be read.
    #[test]
    fn match_row_is_offered_only_when_every_cell_can_be_compared() {
        let placeholder = binary_row_request(sqlly_datatable::CellValue::Text("<16 bytes>".into()));
        let with_raw = ResultGridMenuProvider::where_items_from_request(
            Some(sqlly_lang::sqlmodel::DatabaseType::SqlServer),
            &every_column(Some("varbinary")),
            &placeholder,
            true,
        );
        assert!(where_item_ids(&with_raw).contains(&action_id::WHERE_MATCH_ROW));
        let without_raw = ResultGridMenuProvider::where_items_from_request(
            Some(sqlly_lang::sqlmodel::DatabaseType::SqlServer),
            &every_column(Some("varbinary")),
            &placeholder,
            false,
        );
        assert!(!where_item_ids(&without_raw).contains(&action_id::WHERE_MATCH_ROW));
        let cut = binary_row_request(sqlly_datatable::CellValue::Text(
            "It's a long note … [2.4 MiB]".into(),
        ));
        let items = ResultGridMenuProvider::where_items_from_request(
            Some(sqlly_lang::sqlmodel::DatabaseType::SqlServer),
            &every_column(Some("varchar(max)")),
            &cut,
            true,
        );
        assert!(!where_item_ids(&items).contains(&action_id::WHERE_MATCH_ROW));
    }

    /// A genuine empty binary value, shown as `<0 bytes>`, is not a
    /// placeholder: the text spells the whole value, so a pivot grid offers
    /// equality on it (where `<16 bytes>` gets only the NULL tests) and the
    /// executed action renders `0x` with no raw row to read.
    #[test]
    fn pivot_empty_bytes_keep_equality() {
        let request = binary_row_request(sqlly_datatable::CellValue::Text("<0 bytes>".into()));
        let db = sqlly_lang::sqlmodel::DatabaseType::SqlServer;
        let items = ResultGridMenuProvider::where_items_from_request(
            Some(db),
            &every_column(Some("varbinary")),
            &request,
            false,
        );
        let ids = where_item_ids(&items);
        assert!(ids.contains(&action_id::WHERE_EQUALS), "{ids:?}");
        assert!(ids.contains(&action_id::WHERE_NOT_EQUALS), "{ids:?}");
        assert!(!ids.contains(&action_id::WHERE_LIKE), "{ids:?}");
        let rs = binary_result_set("varbinary");
        let action = build_where_action_from_id(
            db,
            action_id::WHERE_EQUALS,
            &request,
            &rs,
            None,
            0,
            &|_, _| None,
        )
        .expect("an empty value compares as empty bytes");
        assert_eq!(action.predicate_preview, "[user_id] = 0x");
    }

    /// A varbinary column shown as base64: the menu cannot read the text as
    /// bytes, but the host has the raw rows, so = / <> / IN are offered and
    /// the executed action compares the real bytes. On a pivot grid, with
    /// no raw rows, only the NULL tests remain.
    #[test]
    fn base64_binary_column_keeps_equality_when_raw_rows_exist() {
        let request = binary_row_request(sqlly_datatable::CellValue::Text("AAECAw==".into()));
        let db = sqlly_lang::sqlmodel::DatabaseType::SqlServer;
        let offered = |raw_rows: bool| -> Vec<String> {
            let items = ResultGridMenuProvider::where_items_from_request(
                Some(db),
                &every_column(Some("varbinary")),
                &request,
                raw_rows,
            );
            where_item_ids(&items)
                .iter()
                .map(|id| id.to_string())
                .collect()
        };
        let has = |ids: &[String], id: &str| ids.iter().any(|i| i == id);
        let with_rows = offered(true);
        assert!(has(&with_rows, action_id::WHERE_EQUALS), "{with_rows:?}");
        assert!(
            has(&with_rows, action_id::WHERE_NOT_EQUALS),
            "{with_rows:?}"
        );
        assert!(!has(&with_rows, action_id::WHERE_LIKE), "{with_rows:?}");
        let pivot = offered(false);
        assert!(!has(&pivot, action_id::WHERE_EQUALS), "{pivot:?}");
        assert!(has(&pivot, action_id::WHERE_IS_NULL), "{pivot:?}");

        let action = build_where_action_from_id(
            db,
            action_id::WHERE_EQUALS,
            &request,
            &binary_result_set("varbinary"),
            None,
            0,
            &|_, col| (col == 1).then(|| CellValue::Bytes(vec![0, 1, 2, 3])),
        )
        .expect("equality builds from the raw bytes");
        assert_eq!(action.predicate_preview, "[user_id] = 0x00010203");
    }

    /// The same rowversion with no raw row to read (a pivot grid, a
    /// `binary=` directive): the hex text itself spells the bytes, so the
    /// filter still builds instead of being a dead click.
    #[test]
    fn where_equals_on_hex_text_without_raw_bytes_still_builds() {
        let request = binary_row_request(sqlly_datatable::CellValue::Text(
            "0x00000000000007D1".into(),
        ));
        let rs = binary_result_set("rowversion");
        let action = build_where_action_from_id(
            sqlly_lang::sqlmodel::DatabaseType::SqlServer,
            action_id::WHERE_EQUALS,
            &request,
            &rs,
            None,
            0,
            &|_, _| None,
        )
        .expect("rowversion equality builds from the hex text");
        assert_eq!(action.predicate_preview, "[user_id] = 0x00000000000007D1");
    }

    /// A SQL Server rowversion shown as hex text: the menu classifies it
    /// with the engine and type name, so no text pattern or range is
    /// offered, and the builder refuses the same operators if asked.
    #[test]
    fn rowversion_hex_text_menu_and_builder_agree_on_no_text_patterns() {
        let request = binary_row_request(sqlly_datatable::CellValue::Text(
            "0x00000000000007D1".into(),
        ));
        let items = ResultGridMenuProvider::where_items_from_request(
            Some(sqlly_lang::sqlmodel::DatabaseType::SqlServer),
            &every_column(Some("rowversion")),
            &request,
            true,
        );
        let ids = where_item_ids(&items);
        assert!(ids.contains(&action_id::WHERE_EQUALS), "{ids:?}");
        assert!(ids.contains(&action_id::WHERE_NOT_EQUALS), "{ids:?}");
        for absent in [
            action_id::WHERE_LIKE,
            action_id::WHERE_NOT_LIKE,
            action_id::WHERE_STARTS_WITH,
            action_id::WHERE_ENDS_WITH,
            action_id::WHERE_GREATER_THAN,
            action_id::WHERE_LESS_THAN,
        ] {
            assert!(!ids.contains(&absent), "{absent} offered on a rowversion");
        }
        let rs = binary_result_set("rowversion");
        for id in [
            action_id::WHERE_LIKE,
            action_id::WHERE_STARTS_WITH,
            action_id::WHERE_ENDS_WITH,
        ] {
            let action = build_where_action_from_id(
                sqlly_lang::sqlmodel::DatabaseType::SqlServer,
                id,
                &request,
                &rs,
                None,
                0,
                &|_, _| None,
            );
            assert!(action.is_none(), "{id} built on a rowversion");
        }
    }

    /// Two selected cells of one column: BETWEEN goes through the same
    /// gate as every other operator, so a range over bytes is not offered
    /// (and neither is a range over hex text that spells bytes), while a
    /// range over two numbers is.
    #[test]
    fn between_goes_through_the_operator_gate() {
        let two_rows = |cells: [sqlly_datatable::CellValue; 2],
                        kind: sqlly_datatable::ColumnKind| {
            ContextMenuRequest::for_test(
                ContextMenuTarget::Cell {
                    display_row_index: 0,
                    source_row_index: 0,
                    column_index: 0,
                },
                Some(sqlly_datatable::ContextMenuSelection {
                    row_start: 0,
                    row_end: 1,
                    column_start: 0,
                    column_end: 0,
                }),
                cells.into_iter().map(|c| vec![c]).collect(),
                vec![sqlly_datatable::ColumnContext {
                    index: 0,
                    name: "col".into(),
                    kind,
                }],
            )
        };
        let db = sqlly_lang::sqlmodel::DatabaseType::SqlServer;

        let bytes = two_rows(
            [
                sqlly_datatable::CellValue::Text("<16 bytes>".into()),
                sqlly_datatable::CellValue::Text("<16 bytes>".into()),
            ],
            sqlly_datatable::ColumnKind::Text,
        );
        let items = ResultGridMenuProvider::where_items_from_request(
            Some(db),
            &every_column(Some("varbinary")),
            &bytes,
            true,
        );
        let ids = where_item_ids(&items);
        assert!(ids.contains(&action_id::WHERE_IN), "{ids:?}");
        assert!(!ids.contains(&action_id::WHERE_BETWEEN), "{ids:?}");

        let hex = two_rows(
            [
                sqlly_datatable::CellValue::Text("0x00000000000007D1".into()),
                sqlly_datatable::CellValue::Text("0x00000000000007D2".into()),
            ],
            sqlly_datatable::ColumnKind::Text,
        );
        let items = ResultGridMenuProvider::where_items_from_request(
            Some(db),
            &every_column(Some("rowversion")),
            &hex,
            false,
        );
        let ids = where_item_ids(&items);
        assert!(!ids.contains(&action_id::WHERE_BETWEEN), "{ids:?}");

        let numbers = two_rows(
            [
                sqlly_datatable::CellValue::Integer(3),
                sqlly_datatable::CellValue::Integer(9),
            ],
            sqlly_datatable::ColumnKind::Integer,
        );
        let items = ResultGridMenuProvider::where_items_from_request(
            Some(db),
            &every_column(Some("int")),
            &numbers,
            true,
        );
        let ids = where_item_ids(&items);
        assert!(ids.contains(&action_id::WHERE_BETWEEN), "{ids:?}");
    }

    /// The same hex text on PostgreSQL is an ordinary string: the engine,
    /// not the text, decides.
    #[test]
    fn hex_text_in_a_text_column_keeps_its_text_patterns() {
        let request = binary_row_request(sqlly_datatable::CellValue::Text(
            "0x00000000000007D1".into(),
        ));
        let items = ResultGridMenuProvider::where_items_from_request(
            Some(sqlly_lang::sqlmodel::DatabaseType::Postgres),
            &every_column(Some("text")),
            &request,
            true,
        );
        let ids = where_item_ids(&items);
        assert!(ids.contains(&action_id::WHERE_LIKE), "{ids:?}");
    }

    /// Match row goes through the same literal rendering as the single
    /// actions: a rowversion shown as hex text, with no raw row to read,
    /// compares as the bytes the text spells, not as a varchar.
    #[test]
    fn match_row_renders_hex_text_rowversion_as_bytes() {
        let request = binary_row_request(sqlly_datatable::CellValue::Text(
            "0x00000000000007D1".into(),
        ));
        let rs = binary_result_set("rowversion");
        let action = build_where_action_from_id(
            sqlly_lang::sqlmodel::DatabaseType::SqlServer,
            action_id::WHERE_MATCH_ROW,
            &request,
            &rs,
            None,
            0,
            &|_, _| None,
        )
        .expect("match row builds");
        assert_eq!(
            action.predicate_preview,
            "[id] = 7 AND [user_id] = 0x00000000000007D1"
        );
    }

    /// The grid filters on displayed text, and every 16-byte value displays
    /// as `<16 bytes>`: a value quick-filter would match them all.
    #[test]
    fn binary_placeholder_offers_only_null_quick_filters() {
        let request = cell_request(
            sqlly_datatable::CellValue::Text("<16 bytes>".into()),
            sqlly_datatable::ColumnKind::Text,
        );
        let items = ResultGridMenuProvider::quick_filter_items(&request);
        assert_eq!(
            where_item_ids(&items),
            [
                action_id::QUICK_FILTER_IS_NULL,
                action_id::QUICK_FILTER_IS_NOT_NULL
            ]
        );
    }

    /// The WHERE menu on a binary cell offers equality, not text patterns
    /// over the placeholder.
    #[test]
    fn binary_placeholder_where_menu_offers_equality_not_patterns() {
        let request = binary_row_request(sqlly_datatable::CellValue::Text("<16 bytes>".into()));
        let items = ResultGridMenuProvider::where_items_from_request(
            Some(sqlly_lang::sqlmodel::DatabaseType::default()),
            &every_column(None),
            &request,
            true,
        );
        let ids = where_item_ids(&items);
        assert!(ids.contains(&action_id::WHERE_EQUALS), "{ids:?}");
        assert!(ids.contains(&action_id::WHERE_NOT_EQUALS), "{ids:?}");
        for absent in [
            action_id::WHERE_LIKE,
            action_id::WHERE_NOT_LIKE,
            action_id::WHERE_STARTS_WITH,
            action_id::WHERE_ENDS_WITH,
        ] {
            assert!(!ids.contains(&absent), "{absent} offered on a binary cell");
        }
    }

    #[test]
    fn quick_filter_items_offer_value_tests_for_non_null_cells() {
        let request = cell_request(
            sqlly_datatable::CellValue::Text("abc".into()),
            sqlly_datatable::ColumnKind::Text,
        );
        let items = ResultGridMenuProvider::quick_filter_items(&request);
        assert_eq!(
            where_item_ids(&items),
            [
                action_id::QUICK_FILTER_EQ,
                action_id::QUICK_FILTER_NE,
                action_id::QUICK_FILTER_CONTAINS,
                action_id::QUICK_FILTER_IS_NULL,
                action_id::QUICK_FILTER_IS_NOT_NULL,
            ]
        );
    }

    #[test]
    fn quick_filter_items_offer_only_null_tests_for_null_cells() {
        let request = cell_request(
            sqlly_datatable::CellValue::None,
            sqlly_datatable::ColumnKind::Text,
        );
        let items = ResultGridMenuProvider::quick_filter_items(&request);
        assert_eq!(
            where_item_ids(&items),
            [
                action_id::QUICK_FILTER_IS_NULL,
                action_id::QUICK_FILTER_IS_NOT_NULL
            ]
        );
    }

    #[test]
    fn quick_filter_row_picks_text_or_number_operators() {
        use crate::grid_filter_builder::{BuilderOp, ColumnTarget};
        let text = cell_request(
            sqlly_datatable::CellValue::Text("abc".into()),
            sqlly_datatable::ColumnKind::Text,
        );
        let row =
            super::quick_filter_row(action_id::QUICK_FILTER_EQ, &text, Some("abc")).expect("row");
        assert_eq!(row.op, BuilderOp::Is);
        assert_eq!(row.a, "abc");
        assert_eq!(row.column, ColumnTarget::Index(0));
        assert!(row.enabled);
        let row =
            super::quick_filter_row(action_id::QUICK_FILTER_NE, &text, Some("abc")).expect("row");
        assert_eq!(row.op, BuilderOp::IsNot);
        let row = super::quick_filter_row(action_id::QUICK_FILTER_CONTAINS, &text, Some("abc"))
            .expect("row");
        assert_eq!(row.op, BuilderOp::Contains);
        assert_eq!(row.a, "abc");

        let number = cell_request(
            sqlly_datatable::CellValue::Integer(1234),
            sqlly_datatable::ColumnKind::Integer,
        );
        let shown = Some("1,234");
        let row = super::quick_filter_row(action_id::QUICK_FILTER_EQ, &number, shown).expect("row");
        assert_eq!(row.op, BuilderOp::Eq);
        assert_eq!(row.a, "1234", "raw value, no display grouping");
        let row = super::quick_filter_row(action_id::QUICK_FILTER_NE, &number, shown).expect("row");
        assert_eq!(row.op, BuilderOp::Ne);

        let row =
            super::quick_filter_row(action_id::QUICK_FILTER_IS_NULL, &number, shown).expect("row");
        assert_eq!(row.op, BuilderOp::IsNull);
        assert!(row.a.is_empty(), "null tests carry no operand");
        let row = super::quick_filter_row(action_id::QUICK_FILTER_IS_NOT_NULL, &number, shown)
            .expect("row");
        assert_eq!(row.op, BuilderOp::IsNotNull);
        assert!(super::quick_filter_row(action_id::TOP_VALUES, &number, shown).is_none());
    }

    #[test]
    fn quick_filter_contains_on_a_formatted_number_keeps_the_clicked_row() {
        use crate::grid_filter_builder::FilterBuilderModel;
        use sqlly_datatable::{Column, ColumnKind, GridConfig};
        // Integers display with thousands separators by default, and text
        // operators match the displayed text.
        let value = sqlly_datatable::CellValue::Integer(1234);
        let request = cell_request(value.clone(), ColumnKind::Integer);
        let columns = vec![Column::new("n", ColumnKind::Integer, 80.0)];
        let mut config = GridConfig::default();
        config.default_number.thousands_separator = true;
        config.default_number.decimals = 0;
        let fmts = config.resolve_all(&columns);
        let shown = sqlly_datatable::format::format_cell(&value, &fmts[0]).0;
        assert_eq!(shown, "1,234", "precondition: the grid shows a separator");
        let row = super::quick_filter_row(action_id::QUICK_FILTER_CONTAINS, &request, Some(&shown))
            .expect("row");
        assert_eq!(row.a, "1,234", "the operand is what the grid shows");
        let mut model = FilterBuilderModel::default();
        model.push_row(row);
        assert!(
            model.row_passes(&[value], &[ColumnKind::Integer], &fmts, &["NULL".into()]),
            "the clicked row survives its own quick filter"
        );
    }

    #[test]
    fn cell_menu_offers_open_referenced_row_for_non_null_cell() {
        let request = cell_request(
            sqlly_datatable::CellValue::Integer(42),
            sqlly_datatable::ColumnKind::Integer,
        );
        assert!(super::offers_open_referenced_row(&request));
    }

    #[test]
    fn cell_menu_hides_it_for_null_cell() {
        let request = cell_request(
            sqlly_datatable::CellValue::None,
            sqlly_datatable::ColumnKind::Text,
        );
        assert!(!super::offers_open_referenced_row(&request));
    }

    #[test]
    fn header_menu_includes_foreign_key_label_for_labelled_column() {
        use std::collections::HashMap;

        let mut headers = HashMap::new();
        headers.insert(
            1,
            FkLabelHeaderMenuInfo {
                local_column: "customer_id".into(),
                display_column: "name".into(),
                column_label_on: true,
                show_fk_labels: true,
            },
        );
        let request = ContextMenuRequest::for_test(
            ContextMenuTarget::ColumnHeader { column_index: 1 },
            None,
            vec![],
            vec![sqlly_datatable::ColumnContext {
                index: 1,
                name: "customer_id".into(),
                kind: sqlly_datatable::ColumnKind::Integer,
            }],
        );
        assert!(super::offers_fk_label_header(&request, &headers));
        let labels = item_labels(&ResultGridMenuProvider::fk_label_items(
            headers.get(&1).expect("header info"),
        ));
        assert!(
            labels.iter().any(|l| l.starts_with("Foreign Key Label:")),
            "expected Foreign Key Label items, got {labels:?}"
        );
        assert!(labels.iter().any(|l| l == "Use name"));
        assert!(labels.iter().any(|l| l == "Off"));
    }

    #[test]
    fn where_items_numeric_cell_offers_comparison_not_pattern() {
        let request = cell_request(
            sqlly_datatable::CellValue::Integer(42),
            sqlly_datatable::ColumnKind::Integer,
        );
        let items = ResultGridMenuProvider::where_items_from_request(
            Some(sqlly_lang::sqlmodel::DatabaseType::default()),
            &every_column(None),
            &request,
            true,
        );
        let ids = where_item_ids(&items);
        // Comparison operators available for numbers.
        for expected in [
            action_id::WHERE_GREATER_THAN,
            action_id::WHERE_GREATER_THAN_OR_EQUAL,
            action_id::WHERE_LESS_THAN,
            action_id::WHERE_LESS_THAN_OR_EQUAL,
            action_id::WHERE_EQUALS,
            action_id::WHERE_NOT_EQUALS,
        ] {
            assert!(
                ids.contains(&expected),
                "missing {expected} for numeric cell"
            );
        }
        // Text-pattern operators must NOT appear for a numeric cell.
        for absent in [
            action_id::WHERE_LIKE,
            action_id::WHERE_NOT_LIKE,
            action_id::WHERE_STARTS_WITH,
            action_id::WHERE_ENDS_WITH,
        ] {
            assert!(
                !ids.contains(&absent),
                "{absent} should not appear for a numeric cell"
            );
        }
    }

    #[test]
    fn where_items_text_cell_offers_pattern_not_comparison() {
        let request = cell_request(
            sqlly_datatable::CellValue::Text("hello".into()),
            sqlly_datatable::ColumnKind::Text,
        );
        let items = ResultGridMenuProvider::where_items_from_request(
            Some(sqlly_lang::sqlmodel::DatabaseType::default()),
            &every_column(None),
            &request,
            true,
        );
        let ids = where_item_ids(&items);
        // Text-pattern operators available for strings.
        for expected in [
            action_id::WHERE_LIKE,
            action_id::WHERE_NOT_LIKE,
            action_id::WHERE_STARTS_WITH,
            action_id::WHERE_ENDS_WITH,
        ] {
            assert!(ids.contains(&expected), "missing {expected} for text cell");
        }
        // Comparison operators are gated to ordered kinds — not plain text.
        for absent in [action_id::WHERE_GREATER_THAN, action_id::WHERE_LESS_THAN] {
            assert!(
                !ids.contains(&absent),
                "{absent} should not appear for a text cell"
            );
        }
    }

    /// The reported bug, pinned end to end: right-clicking a
    /// PermissionName cell of the reported query and picking "<>" must emit
    /// p.[Name] <> N'CREATE' — not a predicate on the SELECT alias, which no
    /// table has (and T-SQL cannot even use in WHERE).
    #[test]
    fn where_action_resolves_the_source_column_behind_an_alias() {
        let sql = "SELECT\n    rm.Name as ModelName,\n    p.Name as PermissionName\nFROM Shared.ReferenceModelPermissions rmp\n    INNER JOIN Shared.Permission p ON p.PermissionId = rmp.PermissionId\n    INNER JOIN Shared.ReferenceModel rm ON rm.ReferenceModelId = rmp.ReferenceModelId";
        let request = ContextMenuRequest::for_test(
            ContextMenuTarget::Cell {
                display_row_index: 0,
                source_row_index: 0,
                column_index: 1,
            },
            None,
            vec![vec![
                sqlly_datatable::CellValue::Text("Model".into()),
                sqlly_datatable::CellValue::Text("CREATE".into()),
            ]],
            vec![
                sqlly_datatable::ColumnContext {
                    index: 0,
                    name: "ModelName".into(),
                    kind: sqlly_datatable::ColumnKind::Text,
                },
                sqlly_datatable::ColumnContext {
                    index: 1,
                    name: "PermissionName".into(),
                    kind: sqlly_datatable::ColumnKind::Text,
                },
            ],
        );
        let rs = ResultSet {
            index: 0,
            columns: vec![
                ResultColumn {
                    name: "ModelName".into(),
                    data_type: "nvarchar".into(),
                    nullable: false,
                },
                ResultColumn {
                    name: "PermissionName".into(),
                    data_type: "nvarchar".into(),
                    nullable: false,
                },
            ],
            rows: Vec::new(),
            rows_affected: None,
            row_count_hint: 0,
            ..Default::default()
        };

        let action = build_where_action_from_id(
            sqlly_lang::sqlmodel::DatabaseType::SqlServer,
            action_id::WHERE_NOT_EQUALS,
            &request,
            &rs,
            Some(sql),
            0,
            &|_, _| None,
        )
        .expect("action builds");
        assert_eq!(action.predicate_preview, "p.[Name] <> N'CREATE'");

        // No safe attribution (multi-statement script) → the legacy
        // label-based predicate, not a wrong guess.
        let action = build_where_action_from_id(
            sqlly_lang::sqlmodel::DatabaseType::SqlServer,
            action_id::WHERE_NOT_EQUALS,
            &request,
            &rs,
            Some("SELECT 1; SELECT 2"),
            0,
            &|_, _| None,
        )
        .expect("action builds");
        assert_eq!(action.predicate_preview, "[PermissionName] <> N'CREATE'");
    }

    /// The same resolution contract pinned for Postgres and MySQL: the
    /// predicate targets the real source column in each engine's own quoting
    /// and string-literal forms.
    #[test]
    fn where_action_resolves_the_source_column_per_dialect() {
        let sql = "SELECT\n    rm.Name as ModelName,\n    p.Name as PermissionName\nFROM Shared.ReferenceModelPermissions rmp\n    INNER JOIN Shared.Permission p ON p.PermissionId = rmp.PermissionId\n    INNER JOIN Shared.ReferenceModel rm ON rm.ReferenceModelId = rmp.ReferenceModelId";
        let request = ContextMenuRequest::for_test(
            ContextMenuTarget::Cell {
                display_row_index: 0,
                source_row_index: 0,
                column_index: 1,
            },
            None,
            vec![vec![
                sqlly_datatable::CellValue::Text("Model".into()),
                sqlly_datatable::CellValue::Text("CREATE".into()),
            ]],
            vec![
                sqlly_datatable::ColumnContext {
                    index: 0,
                    name: "ModelName".into(),
                    kind: sqlly_datatable::ColumnKind::Text,
                },
                sqlly_datatable::ColumnContext {
                    index: 1,
                    name: "PermissionName".into(),
                    kind: sqlly_datatable::ColumnKind::Text,
                },
            ],
        );
        let rs = ResultSet {
            index: 0,
            columns: vec![
                ResultColumn {
                    name: "ModelName".into(),
                    data_type: "text".into(),
                    nullable: false,
                },
                ResultColumn {
                    name: "PermissionName".into(),
                    data_type: "text".into(),
                    nullable: false,
                },
            ],
            rows: Vec::new(),
            rows_affected: None,
            row_count_hint: 0,
            ..Default::default()
        };

        for (db, expected) in [
            (
                sqlly_lang::sqlmodel::DatabaseType::Postgres,
                "p.\"Name\" <> 'CREATE'",
            ),
            (
                sqlly_lang::sqlmodel::DatabaseType::MySql,
                "p.`Name` <> 'CREATE'",
            ),
        ] {
            let action = build_where_action_from_id(
                db,
                action_id::WHERE_NOT_EQUALS,
                &request,
                &rs,
                Some(sql),
                0,
                &|_, _| None,
            )
            .expect("action builds");
            assert_eq!(action.predicate_preview, expected, "for {db:?}");
        }
    }

    /// While edits are staged the grid leads with the marker column: the
    /// action (and its provenance lookup) must use the result column behind
    /// the click, and Match Row must skip the marker cell.
    #[test]
    fn where_actions_skip_the_staged_edit_marker_column() {
        let sql = "SELECT p.Name AS PermissionName, p.Code FROM Shared.Permission p";
        let request = ContextMenuRequest::for_test(
            ContextMenuTarget::Cell {
                display_row_index: 0,
                source_row_index: 0,
                column_index: 1,
            },
            None,
            vec![vec![
                sqlly_datatable::CellValue::Text("~".into()),
                sqlly_datatable::CellValue::Text("CREATE".into()),
                sqlly_datatable::CellValue::Text("C1".into()),
            ]],
            vec![
                sqlly_datatable::ColumnContext {
                    index: 0,
                    name: crate::grid_bridge::MARKER_COLUMN_NAME.into(),
                    kind: sqlly_datatable::ColumnKind::Text,
                },
                sqlly_datatable::ColumnContext {
                    index: 1,
                    name: "PermissionName".into(),
                    kind: sqlly_datatable::ColumnKind::Text,
                },
                sqlly_datatable::ColumnContext {
                    index: 2,
                    name: "Code".into(),
                    kind: sqlly_datatable::ColumnKind::Text,
                },
            ],
        );
        let rs = ResultSet {
            index: 0,
            columns: vec![
                ResultColumn {
                    name: "PermissionName".into(),
                    data_type: "nvarchar".into(),
                    nullable: false,
                },
                ResultColumn {
                    name: "Code".into(),
                    data_type: "nvarchar".into(),
                    nullable: false,
                },
            ],
            rows: Vec::new(),
            rows_affected: None,
            row_count_hint: 0,
            ..Default::default()
        };
        let db = sqlly_lang::sqlmodel::DatabaseType::SqlServer;
        let action = build_where_action_from_id(
            db,
            action_id::WHERE_EQUALS,
            &request,
            &rs,
            Some(sql),
            1,
            &|_, _| None,
        )
        .expect("action builds");
        assert_eq!(action.predicate_preview, "p.[Name] = N'CREATE'");
        assert_eq!(action.column_index, 0);
        let row = build_where_action_from_id(
            db,
            action_id::WHERE_MATCH_ROW,
            &request,
            &rs,
            Some(sql),
            1,
            &|_, _| None,
        )
        .expect("match row builds");
        assert_eq!(
            row.predicate_preview,
            "p.[Name] = N'CREATE' AND p.[Code] = N'C1'"
        );
        // The marker cell itself offers no WHERE items.
        let marker_click = ContextMenuRequest::for_test(
            ContextMenuTarget::Cell {
                display_row_index: 0,
                source_row_index: 0,
                column_index: 0,
            },
            None,
            vec![vec![sqlly_datatable::CellValue::Text("~".into())]],
            vec![sqlly_datatable::ColumnContext {
                index: 0,
                name: crate::grid_bridge::MARKER_COLUMN_NAME.into(),
                kind: sqlly_datatable::ColumnKind::Text,
            }],
        );
        assert!(
            ResultGridMenuProvider::where_items_from_request(
                Some(sqlly_lang::sqlmodel::DatabaseType::default()),
                &every_column(None),
                &marker_click,
                true
            )
            .is_empty()
        );
    }

    #[test]
    fn looks_masked_recognises_default_mask_char() {
        assert!(looks_masked("••••abcd"));
        assert!(looks_masked("••••"));
        assert!(looks_masked("ab••cd"));
    }

    #[test]
    fn looks_masked_rejects_plain_text() {
        assert!(!looks_masked("xxxxabcd"));
        assert!(!looks_masked("password_hash"));
        assert!(!looks_masked("•"));
        assert!(!looks_masked(""));
    }

    fn item_labels(items: &[ContextMenuItem]) -> Vec<String> {
        items
            .iter()
            .filter_map(|i| match i {
                ContextMenuItem::Action { label, .. } => Some(label.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn menu_offers_image_and_geometry_viewers() {
        use base64::Engine as _;
        let png = base64::engine::general_purpose::STANDARD.encode(b"\x89PNG\r\n\x1a\n");
        let data_url = format!("data:image/png;base64,{png}");
        let image_req = cell_request(
            sqlly_datatable::CellValue::Text(data_url),
            sqlly_datatable::ColumnKind::Text,
        );
        let labels = item_labels(&ResultGridMenuProvider::structured_data_items(
            &image_req, None,
        ));
        assert!(
            labels.iter().any(|l| l == "View Image…"),
            "expected View Image…, got {labels:?}"
        );

        let geo_req = cell_request(
            sqlly_datatable::CellValue::Text("SRID=4326;POINT(1 2)".into()),
            sqlly_datatable::ColumnKind::Text,
        );
        let labels = item_labels(&ResultGridMenuProvider::structured_data_items(
            &geo_req, None,
        ));
        assert!(
            labels.iter().any(|l| l == "View Geometry…"),
            "expected View Geometry…, got {labels:?}"
        );

        let hex_req = cell_request(
            sqlly_datatable::CellValue::Text("<128 bytes>".into()),
            sqlly_datatable::ColumnKind::Text,
        );
        let labels = item_labels(&ResultGridMenuProvider::structured_data_items(
            &hex_req, None,
        ));
        assert!(
            labels.iter().any(|l| l == "View as Hex…"),
            "expected View as Hex…, got {labels:?}"
        );
    }

    #[test]
    fn plain_text_cells_offer_the_value_viewer() {
        let text_req = cell_request(
            sqlly_datatable::CellValue::Text("hello world".into()),
            sqlly_datatable::ColumnKind::Text,
        );
        let labels = item_labels(&ResultGridMenuProvider::structured_data_items(
            &text_req, None,
        ));
        assert!(
            labels.iter().any(|l| l == "Open in Value Viewer…"),
            "expected Open in Value Viewer…, got {labels:?}"
        );
        let empty_req = cell_request(
            sqlly_datatable::CellValue::Text(String::new()),
            sqlly_datatable::ColumnKind::Text,
        );
        let labels = item_labels(&ResultGridMenuProvider::structured_data_items(
            &empty_req, None,
        ));
        assert!(
            !labels.iter().any(|l| l == "Open in Value Viewer…"),
            "{labels:?}"
        );
    }
}

#[allow(dead_code)] // Retained for future use.
fn selected_cell(column_index: usize) -> SelectedCellContext {
    SelectedCellContext {
        display_row_index: 0,
        source_row_index: 0,
        column_index,
        column_name: format!("c{column_index}"),
        value: sqlly_datatable::CellValue::Integer(1),
    }
}

#[test]
fn single_column_selection_column() {
    assert_eq!(single_column_selection(&Selection::Column(2), &[]), Some(2));
}

#[test]
fn single_column_selection_cell_range_same_column() {
    assert_eq!(
        single_column_selection(&Selection::CellRange(0, 1, 5, 1), &[]),
        Some(1)
    );
}

#[test]
fn single_column_selection_cell_range_two_columns() {
    assert_eq!(
        single_column_selection(&Selection::CellRange(0, 1, 5, 2), &[]),
        None
    );
}

#[test]
fn single_column_selection_cells_spanning_two_columns() {
    assert_eq!(
        single_column_selection(&Selection::None, &[selected_cell(0), selected_cell(1)]),
        None
    );
}

#[test]
fn single_column_selection_row_is_none() {
    assert_eq!(single_column_selection(&Selection::Row(0), &[]), None);
}

#[cfg(test)]
mod cell_generator_tests {
    use super::*;
    use crate::cell_actions::CellAction;
    use sqlly_services::row_editing::ColumnExpression;

    fn request(
        value: sqlly_datatable::CellValue,
        kind: sqlly_datatable::ColumnKind,
    ) -> ContextMenuRequest {
        ContextMenuRequest::for_test(
            ContextMenuTarget::Cell {
                display_row_index: 0,
                source_row_index: 0,
                column_index: 0,
            },
            None,
            vec![vec![value]],
            vec![sqlly_datatable::ColumnContext {
                index: 0,
                name: "c".into(),
                kind,
            }],
        )
    }

    fn ids(value: sqlly_datatable::CellValue, kind: sqlly_datatable::ColumnKind) -> Vec<String> {
        ids_on_route(value, kind, false)
    }

    fn ids_on_route(
        value: sqlly_datatable::CellValue,
        kind: sqlly_datatable::ColumnKind,
        via_relay: bool,
    ) -> Vec<String> {
        let request = request(value, kind);
        let clicked = request.clicked_cell().expect("cell");
        let text = clicked_text(&clicked.value);
        cell_generator_items(&request, &clicked, &text, via_relay, None)
            .into_iter()
            .filter_map(|item| match item {
                ContextMenuItem::Action { id, .. } => Some(id),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn text_cell_offers_transforms_and_text_generators() {
        let ids = ids(
            sqlly_datatable::CellValue::Text("alice".into()),
            sqlly_datatable::ColumnKind::Text,
        );
        assert!(ids.iter().any(|id| id == "cell_transform_upper"));
        assert!(ids.iter().any(|id| id == "cell_transform_diacritics"));
        assert!(ids.iter().any(|id| id == action_id::CELL_NEW_UUID));
        assert!(ids.iter().any(|id| id == action_id::CELL_SQL_CURRENT_USER));
        assert!(!ids.iter().any(|id| id == action_id::CELL_LOCAL_TIMESTAMP));
    }

    #[test]
    fn null_text_cell_offers_generators_but_no_transforms() {
        let ids = ids(
            sqlly_datatable::CellValue::None,
            sqlly_datatable::ColumnKind::Text,
        );
        assert!(!ids.iter().any(|id| id.starts_with("cell_transform_")));
        assert!(ids.iter().any(|id| id == action_id::CELL_SQL_SERVER_UUID));
    }

    #[test]
    fn date_cell_offers_timestamp_generators_only() {
        let ids = ids(
            sqlly_datatable::CellValue::Text("2026-01-01".into()),
            sqlly_datatable::ColumnKind::Date,
        );
        assert_eq!(
            ids,
            vec![
                action_id::CELL_LOCAL_TIMESTAMP.to_string(),
                action_id::CELL_SQL_SERVER_DATE.to_string(),
            ]
        );
    }

    #[test]
    fn relay_route_hides_server_evaluated_generators() {
        let text = ids_on_route(
            sqlly_datatable::CellValue::Text("alice".into()),
            sqlly_datatable::ColumnKind::Text,
            true,
        );
        assert!(text.iter().any(|id| id == action_id::CELL_NEW_UUID));
        assert!(text.iter().any(|id| id == "cell_transform_upper"));
        assert!(!text.iter().any(|id| id == action_id::CELL_SQL_SERVER_UUID));
        assert!(!text.iter().any(|id| id == action_id::CELL_SQL_CURRENT_USER));
        let date = ids_on_route(
            sqlly_datatable::CellValue::Text("2026-01-01".into()),
            sqlly_datatable::ColumnKind::Date,
            true,
        );
        assert_eq!(date, vec![action_id::CELL_LOCAL_TIMESTAMP.to_string()]);
    }

    #[test]
    fn numeric_cell_offers_nothing() {
        assert!(
            ids(
                sqlly_datatable::CellValue::Integer(4),
                sqlly_datatable::ColumnKind::Integer
            )
            .is_empty()
        );
    }

    #[test]
    fn generator_ids_map_to_cell_actions() {
        assert_eq!(
            cell_generator_action("cell_transform_title"),
            Some(CellAction::Transform(
                crate::cell_transform::CellTransform::TitleCase
            ))
        );
        assert_eq!(
            cell_generator_action(action_id::CELL_NEW_UUID),
            Some(CellAction::NewUuid)
        );
        assert_eq!(
            cell_generator_action(action_id::CELL_SQL_SERVER_DATE),
            Some(CellAction::SqlFunction(ColumnExpression::ServerDate))
        );
        assert_eq!(cell_generator_action(action_id::CELL_SET_NULL), None);
    }
}
