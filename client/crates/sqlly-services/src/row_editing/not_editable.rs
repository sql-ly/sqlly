//! Keyed "not editable" reasons. Each carries a catalog key and its named
//! arguments plus the English sentence, so `RowEditOpenOutcome::not_editable`
//! sends both: older peers and the CLI show the English `reason`, the GPUI
//! renders the key in the user's language.

use sqlly_core::DisplayMessage;

fn keyed(key: &'static str, english: impl Into<String>) -> DisplayMessage {
    DisplayMessage::new(key, english)
}

pub(crate) fn later_result_set() -> DisplayMessage {
    keyed(
        "services.row_editing.not_editable.later_result_set",
        "only the first result set of a batch can be edited (provenance for later result sets \
         is not available)",
    )
}

pub(crate) fn later_result_set_template() -> DisplayMessage {
    keyed(
        "services.row_editing.not_editable.later_result_set_template",
        "only the first result set of a batch can be used as an insert template",
    )
}

pub(crate) fn provenance_failed(error: impl std::fmt::Display) -> DisplayMessage {
    let error = error.to_string();
    keyed(
        "services.row_editing.not_editable.provenance_failed",
        format!("the statement's provenance could not be determined: {error}"),
    )
    .with_arg("error_message", error)
}

pub(crate) fn result_shape_unknown() -> DisplayMessage {
    keyed(
        "services.row_editing.not_editable.result_shape_unknown",
        "the statement's result shape could not be determined",
    )
}

pub(crate) fn column_count_mismatch(described: usize, grid: usize) -> DisplayMessage {
    keyed(
        "services.row_editing.not_editable.column_count_mismatch",
        format!(
            "the described result has {described} columns but the grid has {grid}; provenance \
             cannot be trusted"
        ),
    )
    .with_arg("described_count", described.to_string())
    .with_arg("grid_count", grid.to_string())
}

pub(crate) fn multiple_tables(first: &str, second: &str) -> DisplayMessage {
    keyed(
        "services.row_editing.not_editable.multiple_tables",
        format!(
            "the result draws from more than one table ({first} and {second}); joined results \
             are read-only"
        ),
    )
    .with_arg("first_table", first)
    .with_arg("second_table", second)
}

pub(crate) fn no_source_column() -> DisplayMessage {
    keyed(
        "services.row_editing.not_editable.no_source_column",
        "no column in this result maps back to a table column; the result is read-only",
    )
}

pub(crate) fn no_primary_key(table: &str) -> DisplayMessage {
    keyed(
        "services.row_editing.not_editable.no_primary_key",
        format!("{table} has no primary key; rows cannot be identified safely"),
    )
    .with_arg("table_name", table)
}

pub(crate) fn key_column_not_projected(column: &str) -> DisplayMessage {
    keyed(
        "services.row_editing.not_editable.key_column_not_projected",
        format!(
            "key column {column} is not part of this result; include it in the SELECT to edit rows"
        ),
    )
    .with_arg("column_name", column)
}

pub(crate) fn key_column_null(column: &str) -> DisplayMessage {
    keyed(
        "services.row_editing.not_editable.key_column_null",
        format!("key column {column} is NULL in this row; the row cannot be identified"),
    )
    .with_arg("column_name", column)
}

pub(crate) fn row_not_found() -> DisplayMessage {
    keyed(
        "services.row_editing.not_editable.row_not_found",
        "the row could not be found; it may have been deleted since the query ran",
    )
}

pub(crate) fn key_matched_many_rows(count: usize) -> DisplayMessage {
    keyed(
        "services.row_editing.not_editable.key_matched_many_rows",
        format!("the key matched {count} rows; the primary key metadata is inconsistent"),
    )
    .with_arg("count", count.to_string())
}

pub(crate) fn not_base_table(table: &str) -> DisplayMessage {
    keyed(
        "services.row_editing.not_editable.not_base_table",
        format!("{table} is not a base table (views are read-only in the row editor)"),
    )
    .with_arg("table_name", table)
}

pub(crate) fn table_not_found(table: &str) -> DisplayMessage {
    keyed(
        "services.row_editing.not_editable.table_not_found",
        format!("{table} was not found in this database"),
    )
    .with_arg("table_name", table)
}

pub(crate) fn empty_statement() -> DisplayMessage {
    keyed(
        "services.row_editing.not_editable.empty_statement",
        "the statement is empty",
    )
}

pub(crate) fn several_statements() -> DisplayMessage {
    keyed(
        "services.row_editing.not_editable.several_statements",
        "only a single SELECT statement can be edited; the tab holds several statements",
    )
}

pub(crate) fn batch_read_only() -> DisplayMessage {
    keyed(
        "services.row_editing.not_editable.batch_read_only",
        "only a single SELECT statement can be edited (batches are read-only)",
    )
}

pub(crate) fn with_query_read_only() -> DisplayMessage {
    keyed(
        "services.row_editing.not_editable.with_query_read_only",
        "results of a WITH query are read-only",
    )
}

pub(crate) fn not_select() -> DisplayMessage {
    keyed(
        "services.row_editing.not_editable.not_select",
        "only the rows of a SELECT statement can be edited",
    )
}

pub(crate) fn derived_source() -> DisplayMessage {
    keyed(
        "services.row_editing.not_editable.derived_source",
        "results built on a derived table, subquery, table function, or PIVOT are read-only",
    )
}

pub(crate) fn no_source_table() -> DisplayMessage {
    keyed(
        "services.row_editing.not_editable.no_source_table",
        "the statement does not read from a table",
    )
}

pub(crate) fn dual_has_no_rows() -> DisplayMessage {
    keyed(
        "services.row_editing.not_editable.dual_has_no_rows",
        "DUAL has no rows to edit",
    )
}

pub(crate) fn synonym_unresolved(synonym: &str) -> DisplayMessage {
    keyed(
        "services.row_editing.not_editable.synonym_unresolved",
        format!("{synonym} is a synonym whose target could not be resolved"),
    )
    .with_arg("synonym_name", synonym)
}

pub(crate) fn synonym_not_base_table(synonym: &str) -> DisplayMessage {
    keyed(
        "services.row_editing.not_editable.synonym_not_base_table",
        format!(
            "{synonym} is a synonym for something other than a base table (views are read-only \
             in the row editor)"
        ),
    )
    .with_arg("synonym_name", synonym)
}

pub(crate) fn joined_tables() -> DisplayMessage {
    keyed(
        "services.row_editing.not_editable.joined_tables",
        "the result draws from more than one table; joined results are read-only",
    )
}
