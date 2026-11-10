//! Keyed row-editing failures. Each error carries a catalog key and its
//! arguments (plus the English sentence for logs and the CLI), so the GPUI
//! renders it in the user's language through `localization::message`.

use sqlly_core::{ConnectionProfile, DisplayMessage, StudioError};

fn message(key: &'static str, english: String) -> DisplayMessage {
    DisplayMessage::new(key, english)
}

/// The connection policy refuses data changes (SELECT-only or auto-rollback).
pub(crate) fn policy_blocked(profile: &ConnectionProfile) -> Option<StudioError> {
    if profile.block_all_but_select {
        return Some(StudioError::PermissionDeniedMessage(DisplayMessage::new(
            "services.row_editing.errors.select_only_blocked",
            "Permission denied: this connection allows SELECT statements only",
        )));
    }
    if profile.auto_wrap_rollback {
        return Some(StudioError::PermissionDeniedMessage(DisplayMessage::new(
            "services.row_editing.errors.auto_rollback_blocked",
            "Permission denied: this connection automatically rolls back data changes; row edits would not persist",
        )));
    }
    None
}

pub(crate) fn value_count_mismatch() -> StudioError {
    StudioError::InvalidInputMessage(DisplayMessage::new(
        "services.row_editing.errors.value_count_mismatch",
        "The row's value count does not match its column count",
    ))
}

pub(crate) fn no_key_columns_for_update() -> StudioError {
    StudioError::InvalidInputMessage(DisplayMessage::new(
        "services.row_editing.errors.update_without_key",
        "The update has no key columns to identify the row",
    ))
}

pub(crate) fn no_key_columns_for_delete() -> StudioError {
    StudioError::InvalidInputMessage(DisplayMessage::new(
        "services.row_editing.errors.delete_without_key",
        "The delete has no key columns to identify the row",
    ))
}

pub(crate) fn no_key_columns_for_fk_search() -> StudioError {
    StudioError::InvalidInputMessage(DisplayMessage::new(
        "services.row_editing.errors.fk_search_without_key",
        "Searching the referenced table needs at least one key column",
    ))
}

pub(crate) fn fetched_row_missing_column(column: &str) -> StudioError {
    StudioError::QueryExecutionMessage(
        message(
            "services.row_editing.errors.fetched_row_missing_column",
            format!("Query execution error: the fetched row is missing column {column}"),
        )
        .with_arg("column_name", column),
    )
}

pub(crate) fn key_matched_rows(count: impl std::fmt::Display) -> StudioError {
    StudioError::QueryExecutionMessage(
        message(
            "services.row_editing.errors.key_matched_many_rows",
            format!("Query execution error: the key matched {count} rows; refusing to continue"),
        )
        .with_arg("count", count.to_string()),
    )
}

pub(crate) fn update_not_refetched() -> StudioError {
    StudioError::QueryExecutionMessage(DisplayMessage::new(
        "services.row_editing.errors.update_not_refetched",
        "Query execution error: the update succeeded but the row could not be read back",
    ))
}

pub(crate) fn update_affected_rows(count: impl std::fmt::Display) -> StudioError {
    StudioError::QueryExecutionMessage(
        message(
            "services.row_editing.errors.update_rolled_back",
            format!(
                "Query execution error: the update would have changed {count} rows, so it was rolled back (the key metadata is inconsistent)"
            ),
        )
        .with_arg("count", count.to_string()),
    )
}

pub(crate) fn delete_affected_rows(count: impl std::fmt::Display) -> StudioError {
    StudioError::QueryExecutionMessage(
        message(
            "services.row_editing.errors.delete_rolled_back",
            format!(
                "Query execution error: the delete would have removed {count} rows, so it was rolled back (the key metadata is inconsistent)"
            ),
        )
        .with_arg("count", count.to_string()),
    )
}

/// An INSERT … RETURNING / OUTPUT that did not hand back exactly one row.
pub(crate) fn insert_returned_rows(count: impl std::fmt::Display) -> StudioError {
    StudioError::QueryExecutionMessage(
        message(
            "services.row_editing.errors.insert_returned_rows",
            format!("Query execution error: the insert returned {count} rows; expected exactly 1"),
        )
        .with_arg("count", count.to_string()),
    )
}

/// An INSERT whose affected-row count was not exactly one.
pub(crate) fn insert_affected_rows(count: impl std::fmt::Display) -> StudioError {
    StudioError::QueryExecutionMessage(
        message(
            "services.row_editing.errors.insert_affected_rows",
            format!("Query execution error: the insert affected {count} rows; expected exactly 1"),
        )
        .with_arg("count", count.to_string()),
    )
}

pub(crate) fn oracle_insert_without_rowid() -> StudioError {
    StudioError::QueryExecutionMessage(DisplayMessage::new(
        "services.row_editing.errors.oracle_insert_without_rowid",
        "Query execution error: the insert succeeded but Oracle returned no ROWID for the new row",
    ))
}

pub(crate) fn metadata_result_sets(count: impl std::fmt::Display) -> StudioError {
    StudioError::QueryExecutionMessage(
        message(
            "services.row_editing.errors.metadata_result_sets",
            format!(
                "Query execution error: the table metadata query returned {count} result sets; expected 6"
            ),
        )
        .with_arg("count", count.to_string()),
    )
}

pub(crate) fn unsupported_engine(profile: &ConnectionProfile) -> StudioError {
    let engine = profile.database_type.display_name();
    let kind = format!("{:?}", profile.database_type.connection_kind());
    StudioError::CapabilityNotSupportedMessage(
        message(
            "services.row_editing.errors.unsupported_engine",
            format!("Row editing is not supported for {engine} ({kind} connections)"),
        )
        .with_arg("engine_name", engine)
        .with_arg("connection_kind", kind),
    )
}

/// libSQL: COMMIT or ROLLBACK with no transaction open. `statement` is the
/// SQL keyword and stays untranslated.
pub(crate) fn libsql_no_transaction(statement: &str) -> StudioError {
    StudioError::QueryExecutionMessage(
        message(
            "services.row_editing.errors.libsql_no_transaction",
            format!(
                "Query execution error: {statement} was requested but no libSQL transaction is open"
            ),
        )
        .with_arg("statement", statement),
    )
}

pub(crate) fn libsql_transaction_already_open() -> StudioError {
    StudioError::QueryExecutionMessage(DisplayMessage::new(
        "services.row_editing.errors.libsql_transaction_open",
        "Query execution error: a libSQL transaction is already open",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_engine_displays_the_capability_prefix_once() {
        let mut profile = ConnectionProfile::sql_login(
            "redis",
            "localhost",
            "",
            sqlly_core::SecretRef {
                provider: sqlly_core::SecretProvider::InMemory,
                key: String::new(),
            },
        );
        profile.database_type = sqlly_core::DatabaseType::Redis;
        let text = unsupported_engine(&profile).to_string();
        assert_eq!(
            text.matches("Server capability not supported: ").count(),
            1,
            "{text}"
        );
        assert!(
            text.starts_with("Server capability not supported: Row editing is not supported for "),
            "{text}"
        );
    }
}
