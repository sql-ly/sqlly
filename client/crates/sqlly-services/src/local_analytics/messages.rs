//! Localizable messages for workspace errors and partial-relation badges.
//! Each carries a `notebook.duck.*` catalog key, its arguments, and the
//! English text as the fallback.

use sqlly_core::DisplayMessage;
use sqlly_engines::duckdb::local_store::{LocalStoreError, PartialReason, PathRejection};

/// `template` with each `{name}` replaced by its argument.
fn fill(template: &str, arguments: &[(&str, String)]) -> String {
    arguments
        .iter()
        .fold(template.to_string(), |text, (name, value)| {
            text.replace(&format!("{{{name}}}"), value)
        })
}

fn message(key: &'static str, template: &str, arguments: &[(&str, String)]) -> DisplayMessage {
    arguments.iter().fold(
        DisplayMessage::new(key, fill(template, arguments)),
        |message, (name, value)| message.with_arg(*name, value.clone()),
    )
}

/// What to show for a workspace error.
pub fn error_message(error: &LocalStoreError) -> DisplayMessage {
    match error {
        LocalStoreError::InvalidNotebookId(id) => message(
            "notebook.duck.error.invalid_notebook_id",
            "The notebook id \"{id}\" can't be used for local data.",
            &[("id", id.clone())],
        ),
        LocalStoreError::InvalidRelationName { name, .. } => message(
            "notebook.duck.error.invalid_relation_name",
            "\"{name}\" can't be used as a local table name. Use up to 63 letters, digits and underscores, starting with a letter or underscore, and don't start it with _sqlly.",
            &[("name", name.clone())],
        ),
        LocalStoreError::RelationExists(name) => message(
            "notebook.duck.error.relation_exists",
            "A local table named \"{name}\" already exists.",
            &[("name", name.clone())],
        ),
        LocalStoreError::RelationNotFound(name) => message(
            "notebook.duck.error.relation_not_found",
            "There is no table named \"{name}\".",
            &[("name", name.clone())],
        ),
        LocalStoreError::PathRejected(reason) => path_message(*reason),
        LocalStoreError::FileTooLarge { size, limit } => message(
            "notebook.duck.error.file_too_large",
            "The file is {size} bytes, more than the {limit}-byte import limit.",
            &[("size", size.to_string()), ("limit", limit.to_string())],
        ),
        LocalStoreError::ExportTooLarge { size, limit } => message(
            "notebook.duck.error.export_too_large",
            "The export is {size} bytes, more than the {limit}-byte export limit.",
            &[("size", size.to_string()), ("limit", limit.to_string())],
        ),
        LocalStoreError::DiskQuotaExceeded { used, limit } => message(
            "notebook.duck.error.disk_quota",
            "This notebook's local data uses {used} bytes and has reached its {limit}-byte limit. Drop local tables or clear them to make room.",
            &[("used", used.to_string()), ("limit", limit.to_string())],
        ),
        LocalStoreError::NotReadOnly(_) => message(
            "notebook.duck.error.not_read_only",
            "Local SQL can only read data. Use queries such as SELECT, WITH, SUMMARIZE or DESCRIBE.",
            &[],
        ),
        LocalStoreError::FileExchangeUnavailable => message(
            "notebook.duck.error.file_exchange_unavailable",
            "Importing and exporting files is turned off for local data in this engine.",
            &[],
        ),
        LocalStoreError::EncryptionKey => message(
            "notebook.duck.error.encryption_key",
            "This notebook's local data is encrypted, and its key is missing or wrong.",
            &[],
        ),
        LocalStoreError::NotEncrypted => message(
            "notebook.fu_final.encryption.not_encrypted",
            "This notebook's local data isn't encrypted, but a key for it was given.",
            &[],
        ),
        LocalStoreError::UnprotectedInstance => message(
            "notebook.duck.error.unprotected_instance",
            "This notebook's local database is open elsewhere without the notebook's protections. Close it there and try again.",
            &[],
        ),
        LocalStoreError::Malformed {
            format,
            message: detail,
        } => message(
            "notebook.duck.error.malformed",
            "The file couldn't be read as {format}: {detail}",
            &[
                ("format", (*format).to_string()),
                ("detail", detail.clone()),
            ],
        ),
        LocalStoreError::Canceled => message("notebook.duck.error.canceled", "Canceled.", &[]),
        LocalStoreError::Source(detail) => message(
            "notebook.duck.error.source_failed",
            "The source stopped sending rows: {detail}",
            &[("detail", detail.clone())],
        ),
        LocalStoreError::Engine(detail) => message(
            "notebook.duck.error.engine",
            "Local data error: {detail}",
            &[("detail", detail.clone())],
        ),
        LocalStoreError::Io(detail) => message(
            "notebook.duck.error.io",
            "Local data file error: {detail}",
            &[("detail", detail.clone())],
        ),
        LocalStoreError::WorkspaceOpen(_) => message(
            "notebook.duck.error.workspace_open",
            "This notebook's local data is still open. Close the notebook and try again.",
            &[],
        ),
        LocalStoreError::WorkspaceNotEmpty(_) => message(
            "notebook.fu_final.encryption.clear_first",
            "Encryption can only be switched while this notebook has no local tables. Clear local data first; tables come back when their cells run again.",
            &[],
        ),
        LocalStoreError::WorkerStopped(detail) => message(
            "notebook.duck.error.worker_stopped",
            "Local data stopped working unexpectedly: {detail}",
            &[("detail", detail.clone())],
        ),
    }
}

fn path_message(reason: PathRejection) -> DisplayMessage {
    let (key, text) = match reason {
        PathRejection::NotFound => (
            "notebook.duck.error.path_not_found",
            "The file doesn't exist.",
        ),
        PathRejection::NotARegularFile => (
            "notebook.duck.error.path_not_a_file",
            "The path isn't a regular file. Choose a file, not a folder or device.",
        ),
        PathRejection::Symlink => (
            "notebook.duck.error.path_symlink",
            "The path is a symbolic link. Choose the file it points to instead.",
        ),
        PathRejection::ChangedWhileReading => (
            "notebook.duck.error.path_changed",
            "The file changed while it was being read. Try again.",
        ),
        PathRejection::DestinationExists => (
            "notebook.duck.error.destination_exists",
            "The destination file already exists.",
        ),
        PathRejection::DestinationFolderMissing => (
            "notebook.duck.error.destination_folder_missing",
            "The destination folder doesn't exist.",
        ),
    };
    message(key, text, &[])
}

/// The badge text for a partial relation.
pub fn partial_message(reason: &PartialReason) -> DisplayMessage {
    match reason {
        PartialReason::RowLimit { limit } => message(
            "notebook.duck.partial.row_limit",
            "Partial: stopped at the {limit}-row limit for local tables.",
            &[("limit", limit.to_string())],
        ),
        PartialReason::ByteLimit { limit } => message(
            "notebook.duck.partial.byte_limit",
            "Partial: stopped at the {limit}-byte limit for local tables.",
            &[("limit", limit.to_string())],
        ),
        PartialReason::DiskQuota { .. } => message(
            "notebook.duck.partial.disk_quota",
            "Partial: stopped when the notebook's local data reached its disk limit.",
            &[],
        ),
        PartialReason::SourceIncomplete { detail } => message(
            "notebook.duck.partial.source_incomplete",
            "Partial: the source result was already incomplete ({detail}).",
            &[("detail", detail.clone())],
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_carry_keys_arguments_and_english() {
        let message = error_message(&LocalStoreError::FileTooLarge { size: 10, limit: 5 });
        assert_eq!(message.key(), Some("notebook.duck.error.file_too_large"));
        assert_eq!(
            message.fallback(),
            "The file is 10 bytes, more than the 5-byte import limit."
        );
        let arguments: Vec<(&str, &str)> = message.arguments().collect();
        assert_eq!(arguments, [("size", "10"), ("limit", "5")]);

        let partial = partial_message(&PartialReason::RowLimit { limit: 7 });
        assert_eq!(partial.key(), Some("notebook.duck.partial.row_limit"));
        assert!(partial.fallback().contains("7-row"));
        assert_eq!(
            error_message(&LocalStoreError::PathRejected(PathRejection::Symlink)).key(),
            Some("notebook.duck.error.path_symlink")
        );
    }
}
