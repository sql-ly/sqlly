//! Row-editing service: resolves whether a result-grid row can be edited,
//! loads authoritative table metadata, fetches the current row, and executes
//! guarded parameterized updates.
//!
//! Design:
//! - Provenance comes from the engine's own description of the executed
//!   statement — never from column-name matching or client-side SQL parsing.
//!   SQL Server uses `sp_describe_first_result_set` with browse information;
//!   PostgreSQL, MySQL/MariaDB, and SQLite use their prepared-statement
//!   column-origin metadata (see `row_editing_pg` / `row_editing_mysql` /
//!   `row_editing_sqlite`, orchestrated by `row_editing_engines`); Oracle,
//!   whose client exposes no column origins, parses the statement for its
//!   single source table and describes the projection server-side (see
//!   [`oracle_sql`] and `row_editing_oracle`).
//! - v1 edits exactly one base table, requires the full primary key to be
//!   present in the projected result, and treats identity/computed/rowversion
//!   and key columns as read-only.
//! - Writes are parameterized (`@P1`… / `$n` / `?`) and wrapped in a
//!   transaction with an affected-row-count check; a rowversion column (when
//!   present) or the original values of the changed columns provide
//!   optimistic concurrency.
//!
//! All SQL construction and provenance analysis are pure functions, unit
//! tested without a server; only the `Default*` impl performs I/O. Layout:
//! - [`types`]: shared vocabulary (requests, session, outcomes) and the
//!   [`RowEditingService`] trait;
//! - [`introspect`]: describe-result parsing, provenance analysis, declared
//!   lengths, and FK metadata grouping;
//! - [`sql`]: the pure T-SQL builders;
//! - [`service`]: `DefaultRowEditingService`, which dispatches on
//!   `DatabaseType::connection_kind()` exactly like the query executor does.

pub(crate) mod errors;
mod introspect;
pub(crate) mod not_editable;
pub mod oracle_sql;
mod replace;
#[cfg(not(target_arch = "wasm32"))]
mod service;
mod sql;
pub(crate) mod staged;
mod types;

pub(crate) use introspect::LoadedTable;
pub use introspect::{
    DescribeColumn, FkColumnRow, IncomingFkColumnRow, Provenance, analyze_provenance,
    declared_char_length, enum_values_from_check, fill_enum_values_from_checks, group_foreign_keys,
    group_incoming_foreign_keys, parse_describe_result, parse_mysql_enum_list,
};
pub use replace::{
    ReplaceEdit, column_replaceable, is_plain_text_column, is_plain_text_type, plan_replacements,
    replaced_cell, session_aligned_row, table_column_for_result,
};
#[cfg(not(target_arch = "wasm32"))]
pub use service::DefaultRowEditingService;
pub use sql::{
    build_delete_sql, build_fk_labels_sql, build_fk_search_sql, build_insert_sql,
    build_related_open_sql, build_related_rows_sql, build_select_row_sql, build_update_sql,
    escape_like, parse_fk_candidates, parse_fk_labels, quote_ident, render_cell, sql_literal,
};
pub use staged::{
    StagedBatch, StagedOp, StagedStatement, build_batch_statements, render_copy_script,
    render_preview_script, render_rollback_script,
};
pub use types::{
    BatchOutcome, BatchRequest, CheckConstraintSpec, ColumnExpression, DeleteRowOutcome,
    DeleteRowRequest, FkCandidate, FkLabelRequest, FkLabelResult, FkSearchRequest, FkSearchResult,
    ForeignKeySpec, IncomingForeignKey, InsertRowOutcome, InsertRowRequest, RelatedRowsRequest,
    RelatedRowsResult, RowEditColumn, RowEditOpenOutcome, RowEditOpenRequest, RowEditSession,
    RowEditingService, TableTarget, UpdateRowOutcome, UpdateRowRequest, policy_block_message,
};
