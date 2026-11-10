//! Engine-agnostic table structure snapshots and editable drafts.

use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TableSnapshot {
    pub schema: Option<String>,
    pub table: String,
    pub columns: Vec<ColumnDef>,
    pub primary_key: Vec<String>,
    pub pk_constraint_name: Option<String>,
    pub indexes: Vec<IndexDef>,
    pub foreign_keys: Vec<ForeignKeyDef>,
    /// Verbatim stored DDL for indexes (SQLite `sqlite_master.sql`, PG
    /// `indexdef`, SQL Server built DDL). Replayed by the SQLite rebuild.
    pub stored_index_ddl: Vec<String>,
    pub stored_trigger_ddl: Vec<String>,
    /// Indexes the structure model cannot express as column lists
    /// (PostgreSQL expression or descending keys), as the engine's own
    /// `CREATE INDEX …;` text. They stay out of `indexes`, so the structure
    /// editor leaves them alone, and a CREATE script replays them verbatim.
    pub opaque_index_ddl: Vec<String>,
    /// True when the SQLite connection reported `PRAGMA foreign_keys = 1`.
    pub sqlite_foreign_keys_on: bool,
    /// Tables that declare a foreign key referencing this table.
    pub referenced_by: Vec<String>,
    /// Views whose definitions reference this table (for drop/recreate around column changes).
    pub dependent_views: Vec<DependentView>,
    /// The table's server-side description (comment), when the engine stores
    /// one and it is set.
    pub table_comment: Option<String>,
    /// Live column descriptions by column name; only set (non-empty) comments
    /// appear, so a missing key means "no comment".
    pub column_comments: BTreeMap<String, String>,
    /// The engine's own `CREATE TABLE` text (`SHOW CREATE TABLE`), fetched on
    /// engines whose ALTER grammar restates whole column definitions. Used so
    /// a description edit can restate the definition the engine itself wrote,
    /// instead of one rebuilt from the loss-free subset above. `None` when
    /// not fetched (engine does not need it, or the fetch failed).
    pub stored_table_ddl: Option<String>,
    /// Column attributes the CREATE script restates but the structure editor
    /// does not edit (identity seed/increment, generated expressions, MySQL
    /// `ON UPDATE`), keyed by column name. Missing key = none of them.
    pub column_extras: BTreeMap<String, ColumnExtras>,
    /// Table-level CHECK constraints, in the engine's own expression text.
    pub check_constraints: Vec<CheckConstraintDef>,
    /// Why the descriptions could not be read, when the catalog read failed.
    /// `table_comment`/`column_comments` are then unknown (not "unset"), so
    /// description edits are refused instead of guessing add-vs-update.
    pub comment_read_error: Option<String>,
    /// Non-fatal catalog read problems worth surfacing to the user (a CHECK
    /// constraint or definition read that failed, a lower-fidelity fallback).
    pub load_warnings: Vec<sqlly_core::DisplayMessage>,
    /// CREATE-only table storage options; see [`TableStorage`].
    pub storage: TableStorage,
}

/// CREATE-only column attributes; see [`TableSnapshot::column_extras`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ColumnExtras {
    /// SQL Server `IDENTITY(seed, increment)` arguments, as catalog text.
    pub identity_seed_increment: Option<(String, String)>,
    /// PostgreSQL `GENERATED ALWAYS AS IDENTITY` (vs `BY DEFAULT`).
    pub identity_always: bool,
    /// A computed / generated column's expression.
    pub generated: Option<GeneratedColumn>,
    /// MySQL/MariaDB `ON UPDATE …` expression (`CURRENT_TIMESTAMP(3)`).
    pub on_update: Option<String>,
    /// SQL Server column collation, set only when it differs from the
    /// database default (a matching one is implied and not restated).
    pub collation: Option<String>,
}

/// Table-level storage options the CREATE script restates (SQL Server), plus
/// the table features it deliberately does not, which the script warns about.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TableStorage {
    /// `ON [filegroup]`: the heap / clustered index lives off the default
    /// filegroup.
    pub filegroup: Option<String>,
    /// `TEXTIMAGE_ON [filegroup]`: large-value columns live off the default
    /// filegroup.
    pub lob_filegroup: Option<String>,
    /// `WITH (DATA_COMPRESSION = ROW | PAGE)` of the heap / clustered index.
    pub data_compression: Option<String>,
    /// Per-index `DATA_COMPRESSION` (`ROW` / `PAGE`) by index name; indexes
    /// without compression are absent.
    pub index_compression: BTreeMap<String, String>,
    /// Table features present on the server that the script does not
    /// recreate (system versioning, partitioning, memory optimisation, …),
    /// each a sentence for the script's warnings.
    pub unscripted: Vec<sqlly_core::DisplayMessage>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedColumn {
    /// The expression exactly as the catalog stores it.
    pub expression: String,
    /// Stored / persisted (vs computed on read).
    pub stored: bool,
}

/// A table-level CHECK constraint. `name` is `None` for a system-named
/// constraint the script should let the engine name again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckConstraintDef {
    pub name: Option<String>,
    /// The condition without the `CHECK` keyword, parenthesised as the engine
    /// printed it.
    pub expression: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnDef {
    pub name: String,
    /// Full engine type spelling as the user sees it.
    pub data_type: String,
    pub nullable: bool,
    pub identity: bool,
    /// Default expression text without a surrounding constraint.
    pub default: Option<String>,
    /// SQL Server only: named default constraint carrying `default`.
    pub default_constraint_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexDef {
    pub name: String,
    pub columns: Vec<String>,
    pub unique: bool,
    pub include: Vec<String>,
    pub filter: Option<String>,
    /// Access method / kind as the engine names it: PG `btree|hash|gin|gist|spgist|brin`,
    /// MySQL `BTREE|HASH`, SQL Server `CLUSTERED|NONCLUSTERED`. `None` = engine default.
    pub method: Option<String>,
    /// PG / SQL Server `fillfactor` (1..=100). `None` = engine default.
    pub fillfactor: Option<u8>,
}

/// A view whose definition references the table (used to drop/recreate it
/// around column changes). `definition` is the complete `CREATE VIEW …`
/// statement when it could be fetched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependentView {
    pub schema: Option<String>,
    pub name: String,
    pub materialized: bool,
    pub definition: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignKeyDef {
    pub name: String,
    pub columns: Vec<String>,
    pub ref_schema: Option<String>,
    pub ref_table: String,
    pub ref_columns: Vec<String>,
    pub on_delete: Option<String>,
    pub on_update: Option<String>,
}

/// One column in an editable draft: `origin` is the snapshot column name
/// (equal to `name` when unchanged); `None` means newly added.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftColumn {
    pub origin: Option<String>,
    pub def: ColumnDef,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableDraft {
    pub schema: Option<String>,
    pub table: String,
    pub columns: Vec<DraftColumn>,
    pub primary_key: Vec<String>,
    pub pk_constraint_name: Option<String>,
    pub indexes: Vec<IndexDef>,
    pub foreign_keys: Vec<ForeignKeyDef>,
    /// Desired table description; `None` clears it when the snapshot had one.
    pub table_comment: Option<String>,
    /// Desired column descriptions by (current) column name; keys missing
    /// from the map have no comment in the draft.
    pub column_comments: BTreeMap<String, String>,
    /// The snapshot's [`TableSnapshot::stored_table_ddl`], carried through so
    /// comment DDL can restate the engine's own column definitions.
    pub source_ddl: Option<String>,
}

impl TableDraft {
    pub fn column_defs(&self) -> Vec<ColumnDef> {
        self.columns.iter().map(|c| c.def.clone()).collect()
    }
}
