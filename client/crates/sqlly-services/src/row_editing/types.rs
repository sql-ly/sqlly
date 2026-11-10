//! Shared vocabulary of the row editor: request/response types, the resolved
//! editing session, and the [`RowEditingService`] trait. Everything here is
//! pure data (serde-serializable where it crosses the service boundary); the
//! connection-policy gate at the bottom is the only profile-aware logic.

use async_trait::async_trait;
use sqlly_core::{CellValue, ConnectionProfile, ResultColumn, StudioResult};

use super::sql::quote_ident;

/// A concrete base table (two-part name) resolved from result provenance.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TableTarget {
    pub schema: String,
    pub table: String,
}

impl TableTarget {
    /// `[schema].[table]` with `]` doubled, safe for direct SQL embedding.
    pub fn quoted(&self) -> String {
        format!("{}.{}", quote_ident(&self.schema), quote_ident(&self.table))
    }
}

/// Everything the service needs to resolve a clicked result row.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RowEditOpenRequest {
    /// The SQL text whose execution produced the grid.
    pub sql: String,
    /// Which result set of the batch the grid shows (0-based).
    pub result_set_index: usize,
    /// The projected columns exactly as displayed.
    pub columns: Vec<ResultColumn>,
    /// The clicked row's values, aligned with `columns`.
    pub row: Vec<CellValue>,
}

/// One column of the target table, from authoritative catalog metadata.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RowEditColumn {
    pub name: String,
    pub data_type: String,
    /// Declared length in characters (`nvarchar` byte length already halved);
    /// `None` for `max` types and non-length types.
    pub char_max_length: Option<i64>,
    pub precision: Option<u8>,
    pub scale: Option<u8>,
    pub nullable: bool,
    pub is_primary_key: bool,
    pub is_identity: bool,
    pub is_computed: bool,
    pub is_rowversion: bool,
    pub default_definition: Option<String>,
    /// Index of this column in the projected result, when it was selected.
    pub result_ordinal: Option<usize>,
    /// Allowed values when the column is a PostgreSQL enum, a MySQL `ENUM`/`SET`, or constrained by a single-column `CHECK (col IN (...))`; empty otherwise.
    #[serde(default)]
    pub enum_values: Vec<String>,
}

impl RowEditColumn {
    /// Whether the editor may write this column in v1.
    pub fn is_editable(&self) -> bool {
        !(self.is_identity || self.is_computed || self.is_rowversion || self.is_primary_key)
    }
}

/// One outgoing foreign key of the edited table: the referenced table, the
/// participating column pairs (in key order), and a `Name`-like display column
/// on the referenced table when one exists. Composite keys carry every pair.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ForeignKeySpec {
    /// Constraint name (used only for grouping/diagnostics).
    pub constraint_name: String,
    /// The referenced (parent) table.
    pub referenced: TableTarget,
    /// `(local column, referenced column)` pairs, in constraint key order.
    pub column_pairs: Vec<(String, String)>,
    /// A referenced-table column suitable for display (e.g. `Name`), when one
    /// was found. The raw key is always what gets stored; this is cosmetic.
    pub display_column: Option<String>,
}

impl ForeignKeySpec {
    /// The local (child) columns of this FK, in key order.
    pub fn local_columns(&self) -> Vec<String> {
        self.column_pairs.iter().map(|(l, _)| l.clone()).collect()
    }

    /// The referenced (parent) columns of this FK, in key order.
    pub fn referenced_columns(&self) -> Vec<String> {
        self.column_pairs.iter().map(|(_, r)| r.clone()).collect()
    }
}

/// A `CHECK` constraint on the edited table. `definition` is the raw SQL Server
/// expression, e.g. `([Age]>=(0))`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CheckConstraintSpec {
    pub name: String,
    pub definition: String,
}

/// An *incoming* foreign key: a child table that references the edited table.
/// Drives the read-only "related child rows" panel.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IncomingForeignKey {
    pub constraint_name: String,
    /// The referencing (child) table.
    pub child: TableTarget,
    /// `(child column, this-table column)` pairs, in key order.
    pub column_pairs: Vec<(String, String)>,
}

/// A bounded, read-only fetch of rows related to the edited row.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RelatedRowsRequest {
    /// The table to read from.
    pub table: TableTarget,
    /// `(column, value)` equality predicates identifying the related rows.
    pub predicates: Vec<(String, CellValue)>,
    /// Maximum rows to return (one extra is read to detect truncation).
    pub limit: usize,
}

/// Result of a [`RelatedRowsRequest`].
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RelatedRowsResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<CellValue>>,
    /// More rows exist than were returned.
    pub truncated: bool,
    /// A ready-to-run query (values inlined) that returns the full related set,
    /// for the "Open query" action.
    pub open_sql: String,
}

/// A server-filtered search of a referenced table for FK autocomplete.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FkSearchRequest {
    /// The referenced (parent) table to search.
    pub referenced: TableTarget,
    /// Referenced key columns to return, in FK key order.
    pub key_columns: Vec<String>,
    /// Referenced display column, when known (drives the label and the filter).
    pub display_column: Option<String>,
    /// The user's filter text. Empty means "first N rows".
    pub query: String,
    /// Maximum candidates to return (the service reads one extra to detect
    /// truncation).
    pub limit: usize,
}

/// One candidate row from an [`FkSearchRequest`].
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FkCandidate {
    /// Referenced key values, aligned with [`FkSearchRequest::key_columns`].
    pub key: Vec<CellValue>,
    /// Human label: the display column value, or the rendered key otherwise.
    pub label: String,
}

/// Result of an FK autocomplete search.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FkSearchResult {
    pub candidates: Vec<FkCandidate>,
    /// More rows matched than were returned (the caller should refine).
    pub truncated: bool,
}

/// Resolve display labels for many key values of one single-column FK in one
/// round-trip. Read-only, parameterised, bounded by `keys.len()` (callers chunk).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FkLabelRequest {
    pub referenced: TableTarget,
    pub key_column: String,
    pub display_column: String,
    pub keys: Vec<CellValue>,
}

#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct FkLabelResult {
    /// `(key, label)` pairs as returned; keys absent from the referenced table are simply missing.
    pub labels: Vec<(CellValue, String)>,
}

/// A resolved, editable row: target table, key, full metadata, and the
/// authoritative current row (fetched by key, not taken from the grid).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RowEditSession {
    pub target: TableTarget,
    /// Primary-key column names, in key order.
    pub key_columns: Vec<String>,
    /// Name of the table's rowversion/timestamp column, when present.
    pub rowversion_column: Option<String>,
    /// All table columns in catalog order.
    pub columns: Vec<RowEditColumn>,
    /// Current row values aligned with `columns`.
    pub original_row: Vec<CellValue>,
    /// Outgoing foreign keys of the target table.
    pub foreign_keys: Vec<ForeignKeySpec>,
    /// Preloaded display label for each foreign key's *current* value, aligned
    /// with `foreign_keys`. `None` for composite FKs, FKs without a display
    /// column, or when the referenced row could not be resolved.
    pub fk_display: Vec<Option<String>>,
    /// Enabled `CHECK` constraints on the target table.
    pub check_constraints: Vec<CheckConstraintSpec>,
    /// Incoming foreign keys: child tables that reference this one.
    pub incoming_foreign_keys: Vec<IncomingForeignKey>,
}

impl RowEditSession {
    /// The foreign key a column participates in, with the column's position in
    /// that FK's key, if any. Returns the first FK that lists the column.
    pub fn foreign_key_for_column(&self, column: &str) -> Option<(usize, &ForeignKeySpec)> {
        self.foreign_keys
            .iter()
            .enumerate()
            .find(|(_, fk)| fk.column_pairs.iter().any(|(local, _)| local == column))
    }

    /// Key column name/value pairs from the fetched row.
    pub fn key_values(&self) -> Vec<(String, CellValue)> {
        self.key_columns
            .iter()
            .filter_map(|key| {
                self.columns
                    .iter()
                    .position(|column| &column.name == key)
                    .map(|ordinal| (key.clone(), self.original_row[ordinal].clone()))
            })
            .collect()
    }

    /// Rowversion name/value pair from the fetched row, when the table has one.
    pub fn rowversion_value(&self) -> Option<(String, CellValue)> {
        let name = self.rowversion_column.as_ref()?;
        let ordinal = self
            .columns
            .iter()
            .position(|column| &column.name == name)?;
        Some((name.clone(), self.original_row[ordinal].clone()))
    }
}

/// Result of resolving a clicked row: an editable session, or a clear reason
/// the row cannot be edited (shown to the user, never a silent failure).
///
/// Crosses the engine protocol as JSON (`FRAME_TAG_ROW_EDIT_REPORT`). The
/// `NotEditable` shape stays compatible in both directions: `reason` always
/// carries the English sentence, so an older peer that knows only `reason`
/// still shows it (serde ignores the unknown `message` field), and `message`
/// is optional, so a report from an older peer without it still decodes.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum RowEditOpenOutcome {
    Editable(Box<RowEditSession>),
    NotEditable {
        /// English explanation: logs, the CLI, and older peers show it as is.
        reason: String,
        /// Catalog key and arguments of `reason` when the app authored it,
        /// so a localized UI renders it in the active language. Absent for
        /// raw driver text and in reports from older peers.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<sqlly_core::StoredMessage>,
    },
}

impl RowEditOpenOutcome {
    /// A "not editable" outcome from a presentation message: the English
    /// fallback becomes `reason`, and a keyed message also travels as
    /// `message` for localized rendering.
    pub fn not_editable(message: sqlly_core::DisplayMessage) -> Self {
        let stored = message.key().map(|key| sqlly_core::StoredMessage {
            key: key.to_string(),
            arguments: message
                .arguments()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect(),
        });
        Self::NotEditable {
            reason: message.into_fallback(),
            message: stored,
        }
    }
}

/// A non-value write for one column: the engine evaluates the expression.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ColumnExpression {
    /// `SET col = DEFAULT` (UPDATE) / `DEFAULT` in the VALUES list (INSERT).
    Default,
    /// The server clock: `SYSDATETIME()`, `CURRENT_TIMESTAMP`, `NOW()`.
    ServerTime,
    /// The server's current date: `CURRENT_DATE`, `CURDATE()`, …
    ServerDate,
    /// A server-generated UUID: `NEWID()`, `gen_random_uuid()`, `UUID()`, …
    ServerUuid,
    /// The session's login/user name: `SUSER_SNAME()`, `CURRENT_USER`, …
    /// Only offered where [`crate::row_editing_sql::expression_supported`]
    /// says the engine has one.
    CurrentUser,
}

impl ColumnExpression {
    /// True for the variants added after relay bridges first shipped
    /// (`ServerDate`, `ServerUuid`, `CurrentUser`). A bridge-side engine that
    /// predates them fails the whole `row_edit` request with "unknown
    /// variant", so a relay client sends them only when the engine advertises
    /// `row_edit.server_expressions` (see
    /// [`RowEditingService::server_expressions_supported`]). `Default` and
    /// `ServerTime` predate the relay wire format and are always safe.
    #[must_use]
    pub const fn needs_bridge_capability(self) -> bool {
        matches!(
            self,
            Self::ServerDate | Self::ServerUuid | Self::CurrentUser
        )
    }
}

/// A guarded single-row update.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct UpdateRowRequest {
    pub target: TableTarget,
    /// Primary/unique key column name/value pairs identifying the row.
    pub key: Vec<(String, CellValue)>,
    /// Rowversion column name and original value, when the table has one.
    pub rowversion: Option<(String, CellValue)>,
    /// Dirty editable columns with their new values.
    pub changes: Vec<(String, CellValue)>,
    /// Original values of the changed columns; used for optimistic
    /// concurrency when no rowversion column exists.
    pub original_values: Vec<(String, CellValue)>,
    /// Non-value writes (`DEFAULT`, server clock) rendered by the SQL builders.
    #[serde(default)]
    pub expressions: Vec<(String, ColumnExpression)>,
}

/// A parameterized single-row insert.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct InsertRowRequest {
    pub target: TableTarget,
    /// Column name/value pairs to insert. Identity/computed/rowversion columns
    /// must be omitted; omitted nullable/defaulted columns take their default.
    pub values: Vec<(String, CellValue)>,
    /// Non-value writes (`DEFAULT`, server clock) rendered by the SQL builders.
    #[serde(default)]
    pub expressions: Vec<(String, ColumnExpression)>,
}

/// Outcome of [`RowEditingService::insert_row`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum InsertRowOutcome {
    Inserted {
        /// Column names of the inserted row (`OUTPUT INSERTED.*`).
        columns: Vec<String>,
        /// The row as it exists after insert (identity/computed resolved).
        row: Vec<CellValue>,
    },
}

/// A guarded single-row delete.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DeleteRowRequest {
    pub target: TableTarget,
    /// Primary/unique key column name/value pairs identifying the row.
    pub key: Vec<(String, CellValue)>,
    /// Rowversion column and original value, when present.
    pub rowversion: Option<(String, CellValue)>,
    /// Original values of all columns; used for optimistic concurrency when no
    /// rowversion column exists.
    pub original_values: Vec<(String, CellValue)>,
}

/// Outcome of [`RowEditingService::delete_row`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum DeleteRowOutcome {
    Deleted,
    /// The guarded DELETE matched no row: it was already deleted or changed.
    Conflict {
        current_columns: Vec<String>,
        current_row: Option<Vec<CellValue>>,
    },
}

/// Outcome of [`RowEditingService::update_row`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum UpdateRowOutcome {
    Updated {
        /// Column names of the refetched row.
        columns: Vec<String>,
        /// The row as it exists after the update.
        row: Vec<CellValue>,
    },
    NoChanges,
    /// The guarded UPDATE matched no row: it was deleted or modified since it
    /// was loaded. `current_row` is the row as it exists now (`None` if gone),
    /// aligned with `current_columns`.
    Conflict {
        current_columns: Vec<String>,
        current_row: Option<Vec<CellValue>>,
    },
}

use super::staged::StagedBatch;

/// A staged batch commit request.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BatchRequest {
    pub batch: StagedBatch,
}

/// Outcome of [`RowEditingService::apply_batch`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum BatchOutcome {
    Committed {
        statements: usize,
    },
    /// Rolled back because a guarded statement affected an unexpected row count.
    Conflict {
        statement_index: usize,
        affected: u64,
    },
}

#[async_trait]
pub trait RowEditingService: Send + Sync {
    /// Whether the executing engine accepts the server-evaluated expressions
    /// ([`ColumnExpression::needs_bridge_capability`]). Always true
    /// in-process; a relay-backed service answers from the bridge engine's
    /// advertised capabilities, so an older bridge reports false.
    fn server_expressions_supported(&self) -> bool {
        true
    }

    /// Whether [`Self::apply_batch`] can run (a staged grid save). Always
    /// true in-process; a relay-backed service answers from the bridge
    /// engine's advertised `row_edit.apply_batch`, so an older bridge (and a
    /// bundle with no session) reports false.
    fn batch_commit_supported(&self) -> bool {
        true
    }

    /// Resolves whether the clicked row is editable and, when it is, loads
    /// metadata and the authoritative current row.
    async fn open_session(
        &self,
        profile: &ConnectionProfile,
        request: RowEditOpenRequest,
    ) -> StudioResult<RowEditOpenOutcome>;

    /// Resolves the target table for an insert/duplicate form: provenance and
    /// metadata only, no current-row fetch and no requirement that a key be
    /// present in the projection. `request.row` is aligned into catalog order
    /// and returned as `original_row` so callers can seed a "duplicate".
    async fn open_insert_session(
        &self,
        profile: &ConnectionProfile,
        request: RowEditOpenRequest,
    ) -> StudioResult<RowEditOpenOutcome>;

    /// Re-fetches the row by key. `None` when the row no longer exists.
    async fn fetch_row(
        &self,
        profile: &ConnectionProfile,
        target: &TableTarget,
        key: &[(String, CellValue)],
    ) -> StudioResult<Option<(Vec<String>, Vec<CellValue>)>>;

    /// Executes a guarded, transactional, parameterized UPDATE.
    async fn update_row(
        &self,
        profile: &ConnectionProfile,
        request: UpdateRowRequest,
    ) -> StudioResult<UpdateRowOutcome>;

    /// Server-filtered search of a referenced table for FK autocomplete.
    /// Bounded and read-only; never loads the full referenced table.
    async fn fk_search(
        &self,
        profile: &ConnectionProfile,
        request: FkSearchRequest,
    ) -> StudioResult<FkSearchResult>;

    /// Resolve display labels for many key values of one single-column FK.
    /// Bounded and read-only; callers chunk at 500 keys.
    async fn fk_labels(
        &self,
        profile: &ConnectionProfile,
        request: FkLabelRequest,
    ) -> StudioResult<FkLabelResult>;

    /// Bounded, read-only fetch of rows related to the edited row (child
    /// records, referenced parent rows). Never mutates.
    async fn related_rows(
        &self,
        profile: &ConnectionProfile,
        request: RelatedRowsRequest,
    ) -> StudioResult<RelatedRowsResult>;

    /// Executes a guarded, transactional, parameterized INSERT, returning the
    /// inserted row (with identity/computed values resolved).
    async fn insert_row(
        &self,
        profile: &ConnectionProfile,
        request: InsertRowRequest,
    ) -> StudioResult<InsertRowOutcome>;

    /// Executes a guarded, transactional, parameterized DELETE with an
    /// affected-row-count check and optimistic concurrency.
    async fn delete_row(
        &self,
        profile: &ConnectionProfile,
        request: DeleteRowRequest,
    ) -> StudioResult<DeleteRowOutcome>;

    /// Applies a staged batch of row edits in one transaction.
    async fn apply_batch(
        &self,
        profile: &ConnectionProfile,
        request: BatchRequest,
    ) -> StudioResult<BatchOutcome>;
}

/// Connection-policy gate shared by session open and update. Row editing is a
/// data mutation, so both the SELECT-only policy and the auto-rollback
/// playground policy make a connection non-editable. Shared with the
/// engine-generic flows in `row_editing_engines`.
pub fn policy_block_message(profile: &ConnectionProfile) -> Option<sqlly_core::DisplayMessage> {
    if profile.block_all_but_select {
        return Some(sqlly_core::DisplayMessage::new(
            "services.row_editing.policy.select_only",
            "this connection allows SELECT statements only",
        ));
    }
    if profile.auto_wrap_rollback {
        return Some(sqlly_core::DisplayMessage::new(
            "services.row_editing.policy.auto_rollback",
            "this connection automatically rolls back data changes; row edits would not persist",
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_key_and_rowversion_values_come_from_original_row() {
        let session = RowEditSession {
            target: TableTarget {
                schema: "dbo".into(),
                table: "T".into(),
            },
            key_columns: vec!["Id".into()],
            rowversion_column: Some("RV".into()),
            columns: vec![
                RowEditColumn {
                    name: "Id".into(),
                    data_type: "int".into(),
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
                    name: "RV".into(),
                    data_type: "timestamp".into(),
                    char_max_length: None,
                    precision: None,
                    scale: None,
                    nullable: false,
                    is_primary_key: false,
                    is_identity: false,
                    is_computed: false,
                    is_rowversion: true,
                    default_definition: None,
                    result_ordinal: None,

                    enum_values: Vec::new(),
                },
            ],
            original_row: vec![CellValue::I64(9), CellValue::Bytes(vec![0, 1])],
            foreign_keys: Vec::new(),
            fk_display: Vec::new(),
            check_constraints: Vec::new(),
            incoming_foreign_keys: Vec::new(),
        };
        assert_eq!(
            session.key_values(),
            vec![("Id".to_string(), CellValue::I64(9))]
        );
        assert_eq!(
            session.rowversion_value(),
            Some(("RV".to_string(), CellValue::Bytes(vec![0, 1])))
        );
        assert!(
            !session.columns[0].is_editable(),
            "identity PK is read-only"
        );
        assert!(!session.columns[1].is_editable(), "rowversion is read-only");
    }

    /// The engine-protocol shape of `NotEditable` before keyed messages:
    /// what an older app or bridge decodes a report into.
    #[derive(Debug, serde::Deserialize)]
    enum OldRowEditOpenOutcome {
        #[allow(dead_code)]
        Editable(Box<RowEditSession>),
        NotEditable {
            reason: String,
        },
    }

    #[test]
    fn keyed_not_editable_carries_english_reason_and_catalog_message() {
        let outcome = RowEditOpenOutcome::not_editable(
            crate::row_editing::not_editable::key_column_null("[Id]"),
        );
        let RowEditOpenOutcome::NotEditable { reason, message } = outcome else {
            panic!("expected NotEditable");
        };
        assert_eq!(
            reason,
            "key column [Id] is NULL in this row; the row cannot be identified"
        );
        assert_eq!(
            message,
            Some(sqlly_core::StoredMessage {
                key: "services.row_editing.not_editable.key_column_null".to_string(),
                arguments: vec![("column_name".to_string(), "[Id]".to_string())],
            })
        );
    }

    #[test]
    fn report_from_an_older_peer_without_message_still_decodes() {
        let json = r#"{"NotEditable":{"reason":"legacy bridge reason"}}"#;
        let outcome: RowEditOpenOutcome = serde_json::from_str(json).expect("old shape decodes");
        let RowEditOpenOutcome::NotEditable { reason, message } = outcome else {
            panic!("expected NotEditable");
        };
        assert_eq!(reason, "legacy bridge reason");
        assert_eq!(message, None);
    }

    #[test]
    fn keyed_report_decodes_into_the_older_peer_shape() {
        let outcome = RowEditOpenOutcome::not_editable(
            crate::row_editing::not_editable::no_primary_key("[dbo].[Logs]"),
        );
        let json = serde_json::to_string(&outcome).expect("serialize");
        assert!(json.contains("\"message\""), "{json}");
        let old: OldRowEditOpenOutcome =
            serde_json::from_str(&json).expect("older peer ignores the new field");
        let OldRowEditOpenOutcome::NotEditable { reason } = old else {
            panic!("expected NotEditable");
        };
        assert_eq!(
            reason,
            "[dbo].[Logs] has no primary key; rows cannot be identified safely"
        );
    }

    #[test]
    fn raw_reason_omits_message_on_the_wire_and_round_trips() {
        let outcome =
            RowEditOpenOutcome::not_editable(sqlly_core::DisplayMessage::raw("driver detail"));
        let json = serde_json::to_string(&outcome).expect("serialize");
        assert_eq!(json, r#"{"NotEditable":{"reason":"driver detail"}}"#);
        let keyed = RowEditOpenOutcome::not_editable(
            crate::row_editing::not_editable::key_matched_many_rows(3),
        );
        let json = serde_json::to_string(&keyed).expect("serialize");
        let back: RowEditOpenOutcome = serde_json::from_str(&json).expect("round trip");
        let RowEditOpenOutcome::NotEditable { message, .. } = back else {
            panic!("expected NotEditable");
        };
        let message = message.expect("message survives");
        assert_eq!(
            message.key,
            "services.row_editing.not_editable.key_matched_many_rows"
        );
        assert_eq!(
            message.arguments,
            vec![("count".to_string(), "3".to_string())]
        );
    }
}
