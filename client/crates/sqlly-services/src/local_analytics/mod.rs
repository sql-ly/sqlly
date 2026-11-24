//! Notebook local analytics: each notebook's private DuckDB workspace, where
//! cell outputs are materialized as named relations, approved files are
//! imported, local SQL cells run read-only, and relations export to Parquet
//! or CSV.
//!
//! The engine primitives live in `sqlly_engines::duckdb::local_store`; this
//! module is the async service the app and the CLI runner use:
//! [`LocalAnalytics`] owns the workspace root, hands out one shared
//! [`NotebookWorkspace`] per open notebook and keeps the root inside its
//! disk budget by evicting the least recently used closed workspaces.
//!
//! DuckDB is native-only; in the browser build [`is_available`] is false and
//! local cells show [`unavailable_message`].

#[cfg(not(target_arch = "wasm32"))]
mod messages;
#[cfg(not(target_arch = "wasm32"))]
mod service;

#[cfg(not(target_arch = "wasm32"))]
pub use messages::{error_message, partial_message};
#[cfg(not(target_arch = "wasm32"))]
pub use service::{
    ExportTarget, ImportSpec, LocalAnalytics, LocalAnalyticsConfig, NotebookWorkspace, OpenOptions,
    SnapshotRows, generate_encryption_key,
};
#[cfg(not(target_arch = "wasm32"))]
pub use sqlly_engines::duckdb::local_store::{
    CancelToken, EncryptionKey, EvictionReport, ExportFormat, ExportOutcome, ImportFormat,
    LocalColumn, LocalResultSet, LocalStoreError, LocalType, MaterializeOutcome, PartialReason,
    PathRejection, ProvenanceRecord, RelationInfo, SourceInfo, SourceKind, WorkspaceQuota,
    WorkspaceUsage, WriteMode, local_type_for, validate_relation_name,
};

use sha2::{Digest, Sha256};

/// Whether this build can keep notebook data locally.
pub fn is_available() -> bool {
    cfg!(not(target_arch = "wasm32"))
}

/// Shown in place of local cells where DuckDB is absent (the browser).
pub fn unavailable_message() -> sqlly_core::DisplayMessage {
    sqlly_core::DisplayMessage::new(
        "notebook.duck.unavailable",
        "Local notebook data isn't available in the browser. Open the notebook in the desktop app to use it.",
    )
}

/// The provenance hash of a cell's SQL: `sha256:` plus the hex digest of
/// the text with line endings normalized and surrounding whitespace
/// trimmed. Provenance stores this, never the SQL itself.
pub fn source_sql_hash(sql: &str) -> String {
    let normalized = sql.replace("\r\n", "\n");
    let digest = Sha256::digest(normalized.trim().as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("sha256:{hex}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sql_hash_ignores_line_endings_and_outer_whitespace() {
        let a = source_sql_hash("SELECT 1\nFROM t\n");
        assert_eq!(a, source_sql_hash("  SELECT 1\r\nFROM t"));
        assert_ne!(a, source_sql_hash("SELECT 2\nFROM t"));
        assert!(a.starts_with("sha256:"));
        assert_eq!(a.len(), "sha256:".len() + 64);
    }

    #[test]
    fn availability_matches_the_target() {
        assert_eq!(is_available(), cfg!(not(target_arch = "wasm32")));
        assert_eq!(
            unavailable_message().key(),
            Some("notebook.duck.unavailable")
        );
    }
}
