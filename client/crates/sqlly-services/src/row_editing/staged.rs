//! Pure staged-batch model: statement assembly, preview, and rollback scripts.

use sqlly_core::{CellValue, ConnectionKind, DatabaseType, SqlParam};

use super::oracle_sql;
use super::sql::{self, quote_ident};
use super::types::{DeleteRowRequest, InsertRowRequest, TableTarget, UpdateRowRequest};
use crate::row_editing_sql::{self, expression_sql, quote_table};

/// One staged row operation, already normalized against the table session.
/// Serializable so a relay client can hand the batch to the bridge-side
/// engine (`row_edit` op `apply_batch`).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum StagedOp {
    Update(UpdateRowRequest),
    Insert(InsertRowRequest),
    Delete {
        request: DeleteRowRequest,
        /// Full original row (all columns in catalog order) for rollback INSERT.
        #[serde(default)]
        original_row: Vec<(String, CellValue)>,
    },
}

/// The ordered batch the user will commit as one transaction.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StagedBatch {
    #[serde(default)]
    pub target: Option<TableTarget>,
    /// Primary-key column names for rollback DELETE of staged inserts.
    #[serde(default)]
    pub key_columns: Vec<String>,
    #[serde(default)]
    pub ops: Vec<StagedOp>,
}

/// A statement ready to execute: text plus bound parameters.
#[derive(Clone, Debug, PartialEq)]
pub struct StagedStatement {
    pub sql: String,
    pub params: Vec<SqlParam>,
    pub expect_affected: Option<u64>,
}

pub fn build_batch_statements(db: DatabaseType, batch: &StagedBatch) -> Vec<StagedStatement> {
    let mut out = Vec::new();
    for op in ordered_ops(batch) {
        match op {
            StagedOp::Delete { request, .. } => {
                let (sql, params) = build_delete(db, request);
                out.push(StagedStatement {
                    sql,
                    params,
                    expect_affected: Some(1),
                });
            }
            StagedOp::Update(request) => {
                let (sql, params) = build_update(db, request);
                out.push(StagedStatement {
                    sql,
                    params,
                    expect_affected: Some(1),
                });
            }
            StagedOp::Insert(request) => {
                let (sql, params) = build_insert(db, request);
                out.push(StagedStatement {
                    sql,
                    params,
                    expect_affected: None,
                });
            }
        }
    }
    out
}

/// Human-readable preview with literals inlined (never executed).
pub fn render_preview_script(db: DatabaseType, batch: &StagedBatch) -> String {
    let body = ordered_ops(batch)
        .iter()
        .map(|op| render_op_inline(db, op))
        .collect::<Vec<_>>()
        .join("\n");
    wrap_transaction(db, &body)
}

/// Reverse script for manual undo. Ops are emitted in reverse order.
pub fn render_rollback_script(db: DatabaseType, batch: &StagedBatch) -> String {
    let mut lines: Vec<String> = Vec::new();
    for op in batch.ops.iter().rev() {
        match op {
            StagedOp::Update(request) => {
                lines.push(render_rollback_update(db, request));
            }
            StagedOp::Delete {
                original_row,
                request,
            } => {
                lines.push(render_rollback_delete(db, &request.target, original_row));
            }
            StagedOp::Insert(request) => {
                lines.push(render_rollback_insert(db, request, &batch.key_columns));
            }
        }
    }
    let body = lines.join("\n");
    wrap_transaction(db, &body)
}

/// The staged changes as a script to copy and run elsewhere ("Copy SQL").
///
/// Same statements as [`render_preview_script`], but closer to what a save
/// enforces: the app's save requires every UPDATE and DELETE to touch exactly
/// one row and rolls the whole batch back otherwise. Where the dialect can
/// express that in a plain script, each such statement is followed by a
/// row-count check that aborts the transaction:
///
/// * SQL Server: `IF @@ROWCOUNT <> 1 THROW …` (with `SET XACT_ABORT ON`).
/// * PostgreSQL-family engines except CockroachDB: the statement runs inside
///   a `DO` block that checks `ROW_COUNT` and raises.
/// * Oracle: an anonymous PL/SQL block checking `SQL%ROWCOUNT`.
///
/// Other engines (MySQL/MariaDB, SQLite, libSQL, DuckDB, CockroachDB) have
/// no scriptable check; each statement gets an `-- expect 1 row` note
/// instead, and the header says so. The header also states the difference
/// that remains everywhere: values are inlined as literals (the save binds
/// parameters). An engine without a transaction wrapper is called out too.
pub fn render_copy_script(db: DatabaseType, batch: &StagedBatch) -> String {
    let guard = RowCountGuard::for_db(db);
    let body = ordered_ops(batch)
        .iter()
        .map(|op| {
            let statement = render_op_inline(db, op);
            match op {
                StagedOp::Insert(_) => statement,
                StagedOp::Update(_) | StagedOp::Delete { .. } => guard.wrap(&statement),
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let mut header = vec![
        "-- Staged changes copied from SQLLY. Values are inlined as literals; SQLLY's own save binds them as parameters.".to_string(),
    ];
    header.push(match guard {
        RowCountGuard::None => "-- SQLLY's save requires each UPDATE/DELETE below to change exactly 1 row and rolls back otherwise; this engine has no scriptable check, so compare each row count yourself.".to_string(),
        RowCountGuard::Oracle => "-- Each UPDATE/DELETE is checked to change exactly 1 row. Run with WHENEVER SQLERROR EXIT ROLLBACK (SQL*Plus/SQLcl) so a failed check undoes the earlier statements.".to_string(),
        _ => "-- Each UPDATE/DELETE is checked to change exactly 1 row; a failed check aborts and rolls back the transaction.".to_string(),
    });
    if !has_transaction_wrapper(db) {
        header.push(
            "-- No transaction wrapper: this engine applies each statement on its own.".to_string(),
        );
    }
    format!("{}\n{}", header.join("\n"), wrap_transaction(db, &body))
}

/// How [`render_copy_script`] enforces "exactly one row" per statement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RowCountGuard {
    Tds,
    PlPgSql,
    Oracle,
    None,
}

const GUARD_MESSAGE: &str =
    "SQLLY: expected exactly 1 row; the row changed or was deleted since it was loaded";

impl RowCountGuard {
    fn for_db(db: DatabaseType) -> Self {
        match db.connection_kind() {
            ConnectionKind::Tds => Self::Tds,
            // CockroachDB's DO-block support is too recent to rely on.
            ConnectionKind::PostgresWire if db != DatabaseType::CockroachDb => Self::PlPgSql,
            ConnectionKind::OracleNet => Self::Oracle,
            _ => Self::None,
        }
    }

    /// `statement` (one rendered `…;` line) with its row-count check.
    fn wrap(self, statement: &str) -> String {
        match self {
            Self::Tds => {
                format!("{statement}\nIF @@ROWCOUNT <> 1 THROW 50001, N'{GUARD_MESSAGE}', 1;")
            }
            Self::PlPgSql => {
                let tag = dollar_quote_tag(statement);
                format!(
                    "DO {tag}\nDECLARE n bigint;\nBEGIN\n  {statement}\n  GET DIAGNOSTICS n = ROW_COUNT;\n  IF n <> 1 THEN\n    RAISE EXCEPTION '{GUARD_MESSAGE} (% rows matched)', n;\n  END IF;\nEND\n{tag};"
                )
            }
            Self::Oracle => format!(
                "BEGIN\n  {statement}\n  IF SQL%ROWCOUNT <> 1 THEN\n    RAISE_APPLICATION_ERROR(-20001, '{GUARD_MESSAGE}');\n  END IF;\nEND;\n/"
            ),
            Self::None => format!("{statement} -- expect 1 row"),
        }
    }
}

/// A `$tag$` dollar-quote delimiter that does not occur in `body`, so a
/// string literal inside the statement can never close the `DO` block early.
fn dollar_quote_tag(body: &str) -> String {
    let mut tag = "$sqlly$".to_string();
    let mut n = 0u32;
    while body.contains(&tag) {
        n += 1;
        tag = format!("$sqlly{n}$");
    }
    tag
}

fn has_transaction_wrapper(db: DatabaseType) -> bool {
    matches!(
        db.connection_kind(),
        ConnectionKind::Tds
            | ConnectionKind::PostgresWire
            | ConnectionKind::MySqlWire
            | ConnectionKind::SqliteFile
            | ConnectionKind::HranaHttp
            | ConnectionKind::DuckDbFile
            | ConnectionKind::OracleNet
    )
}

fn ordered_ops(batch: &StagedBatch) -> Vec<&StagedOp> {
    let mut deletes = Vec::new();
    let mut updates = Vec::new();
    let mut inserts = Vec::new();
    for op in &batch.ops {
        match op {
            StagedOp::Delete { .. } => deletes.push(op),
            StagedOp::Update(_) => updates.push(op),
            StagedOp::Insert(_) => inserts.push(op),
        }
    }
    let mut ordered = Vec::with_capacity(batch.ops.len());
    ordered.extend(deletes);
    ordered.extend(updates);
    ordered.extend(inserts);
    ordered
}

fn build_update(db: DatabaseType, request: &UpdateRowRequest) -> (String, Vec<SqlParam>) {
    if db.connection_kind() == ConnectionKind::Tds {
        sql::build_update_sql(request)
    } else if db.connection_kind() == ConnectionKind::OracleNet {
        // Typeless: the lane's session pins the NLS formats (see
        // `oracle_sql::session_setup_sql`) so string binds convert.
        oracle_sql::build_update_sql(db, request, &[])
    } else {
        row_editing_sql::build_update_sql_for(db, request)
    }
}

fn build_delete(db: DatabaseType, request: &DeleteRowRequest) -> (String, Vec<SqlParam>) {
    if db.connection_kind() == ConnectionKind::Tds {
        sql::build_delete_sql(request)
    } else if db.connection_kind() == ConnectionKind::OracleNet {
        oracle_sql::build_delete_sql(db, request, &[])
    } else {
        row_editing_sql::build_delete_sql_for(db, request)
    }
}

fn build_insert(db: DatabaseType, request: &InsertRowRequest) -> (String, Vec<SqlParam>) {
    if db.connection_kind() == ConnectionKind::Tds {
        sql::build_insert_sql(request)
    } else if db.connection_kind() == ConnectionKind::OracleNet {
        oracle_sql::build_insert_sql(db, request, &[], false)
    } else {
        row_editing_sql::build_insert_plain_sql_for(db, request)
    }
}

fn wrap_transaction(db: DatabaseType, body: &str) -> String {
    match db.connection_kind() {
        ConnectionKind::Tds => {
            format!("SET XACT_ABORT ON;\nBEGIN TRANSACTION;\n{body}\nCOMMIT TRANSACTION;")
        }
        ConnectionKind::PostgresWire => format!("BEGIN;\n{body}\nCOMMIT;"),
        ConnectionKind::MySqlWire => format!("START TRANSACTION;\n{body}\nCOMMIT;"),
        // libSQL runs SQLite's SQL, and its save opens `BEGIN IMMEDIATE` too.
        ConnectionKind::SqliteFile | ConnectionKind::HranaHttp => {
            format!("BEGIN IMMEDIATE;\n{body}\nCOMMIT;")
        }
        ConnectionKind::DuckDbFile => format!("BEGIN TRANSACTION;\n{body}\nCOMMIT;"),
        // Oracle transactions start implicitly; the NLS setup makes the
        // rendered date/timestamp literals convert exactly as the grid
        // showed them.
        ConnectionKind::OracleNet => {
            format!("{};\n{body}\nCOMMIT;", oracle_sql::session_setup_sql())
        }
        _ => body.to_string(),
    }
}

fn literal(db: DatabaseType, value: &CellValue) -> String {
    if db.connection_kind() == ConnectionKind::Tds {
        sql::sql_literal(value)
    } else {
        row_editing_sql::sql_literal_for(db, value)
    }
}

fn ident(db: DatabaseType, name: &str) -> String {
    if db.connection_kind() == ConnectionKind::Tds {
        quote_ident(name)
    } else if db.connection_kind() == ConnectionKind::OracleNet {
        oracle_sql::ident(db, name)
    } else {
        db.quote_identifier(name)
    }
}

fn table_name(db: DatabaseType, target: &TableTarget) -> String {
    if db.connection_kind() == ConnectionKind::Tds {
        target.quoted()
    } else {
        quote_table(db, target)
    }
}

fn render_predicate(db: DatabaseType, column: &str, value: &CellValue) -> String {
    if matches!(value, CellValue::Null) {
        format!("{} IS NULL", ident(db, column))
    } else {
        format!("{} = {}", ident(db, column), literal(db, value))
    }
}

fn render_op_inline(db: DatabaseType, op: &StagedOp) -> String {
    match op {
        StagedOp::Update(request) => render_inline_update(db, request),
        StagedOp::Delete { request, .. } => render_inline_delete(db, request),
        StagedOp::Insert(request) => render_inline_insert(db, request),
    }
}

fn render_inline_update(db: DatabaseType, request: &UpdateRowRequest) -> String {
    let mut assignments: Vec<String> = request
        .changes
        .iter()
        .map(|(column, value)| {
            if matches!(value, CellValue::Null) {
                format!("{} = NULL", ident(db, column))
            } else {
                format!("{} = {}", ident(db, column), literal(db, value))
            }
        })
        .collect();
    for (column, expr) in &request.expressions {
        assignments.push(format!(
            "{} = {}",
            ident(db, column),
            expression_sql(db, *expr)
        ));
    }
    let mut predicates: Vec<String> = request
        .key
        .iter()
        .map(|(column, value)| render_predicate(db, column, value))
        .collect();
    if let Some((column, value)) = &request.rowversion {
        predicates.push(render_predicate(db, column, value));
    } else {
        for (column, value) in &request.original_values {
            predicates.push(render_predicate(db, column, value));
        }
    }
    format!(
        "UPDATE {} SET {} WHERE {};",
        table_name(db, &request.target),
        assignments.join(", "),
        predicates.join(" AND ")
    )
}

fn render_inline_delete(db: DatabaseType, request: &DeleteRowRequest) -> String {
    let mut predicates: Vec<String> = request
        .key
        .iter()
        .map(|(column, value)| render_predicate(db, column, value))
        .collect();
    if let Some((column, value)) = &request.rowversion {
        predicates.push(render_predicate(db, column, value));
    } else {
        for (column, value) in &request.original_values {
            predicates.push(render_predicate(db, column, value));
        }
    }
    format!(
        "DELETE FROM {} WHERE {};",
        table_name(db, &request.target),
        predicates.join(" AND ")
    )
}

fn render_inline_insert(db: DatabaseType, request: &InsertRowRequest) -> String {
    let table = table_name(db, &request.target);
    if request.values.is_empty() && request.expressions.is_empty() {
        return match db.connection_kind() {
            ConnectionKind::Tds => format!("INSERT INTO {table} DEFAULT VALUES;"),
            ConnectionKind::MySqlWire => format!("INSERT INTO {table} () VALUES ();"),
            // The table name stays out of the comment: a quoted identifier
            // may contain a line break, which would end the comment and turn
            // the rest of the name into SQL.
            ConnectionKind::OracleNet => {
                "-- Oracle has no DEFAULT VALUES form: supply at least one column for this insert"
                    .to_string()
            }
            _ => format!("INSERT INTO {table} DEFAULT VALUES;"),
        };
    }
    let mut columns: Vec<String> = request.values.iter().map(|(c, _)| ident(db, c)).collect();
    let mut values: Vec<String> = request
        .values
        .iter()
        .map(|(_, value)| {
            if matches!(value, CellValue::Null) {
                "NULL".to_string()
            } else {
                literal(db, value)
            }
        })
        .collect();
    for (column, expr) in &request.expressions {
        columns.push(ident(db, column));
        values.push(expression_sql(db, *expr).to_string());
    }
    format!(
        "INSERT INTO {} ({}) VALUES ({});",
        table,
        columns.join(", "),
        values.join(", ")
    )
}

fn render_rollback_update(db: DatabaseType, request: &UpdateRowRequest) -> String {
    let assignments: Vec<String> = request
        .original_values
        .iter()
        .map(|(column, value)| {
            if matches!(value, CellValue::Null) {
                format!("{} = NULL", ident(db, column))
            } else {
                format!("{} = {}", ident(db, column), literal(db, value))
            }
        })
        .collect();
    let predicates: Vec<String> = request
        .key
        .iter()
        .map(|(column, value)| render_predicate(db, column, value))
        .collect();
    format!(
        "UPDATE {} SET {} WHERE {};",
        table_name(db, &request.target),
        assignments.join(", "),
        predicates.join(" AND ")
    )
}

fn render_rollback_delete(
    db: DatabaseType,
    target: &TableTarget,
    original_row: &[(String, CellValue)],
) -> String {
    let columns: Vec<String> = original_row.iter().map(|(c, _)| ident(db, c)).collect();
    let values: Vec<String> = original_row
        .iter()
        .map(|(_, value)| {
            if matches!(value, CellValue::Null) {
                "NULL".to_string()
            } else {
                literal(db, value)
            }
        })
        .collect();
    format!(
        "INSERT INTO {} ({}) VALUES ({});",
        table_name(db, target),
        columns.join(", "),
        values.join(", ")
    )
}

fn render_rollback_insert(
    db: DatabaseType,
    request: &InsertRowRequest,
    key_columns: &[String],
) -> String {
    if key_columns.is_empty() {
        return "-- cannot reverse insert: key generated server-side".to_string();
    }
    let value_map: std::collections::HashMap<&str, &CellValue> = request
        .values
        .iter()
        .map(|(name, value)| (name.as_str(), value))
        .collect();
    if !key_columns
        .iter()
        .all(|key| value_map.contains_key(key.as_str()))
    {
        return "-- cannot reverse insert: key generated server-side".to_string();
    }
    let predicates: Vec<String> = key_columns
        .iter()
        .map(|column| render_predicate(db, column, value_map[column.as_str()]))
        .collect();
    format!(
        "DELETE FROM {} WHERE {};",
        table_name(db, &request.target),
        predicates.join(" AND ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A staged batch crosses the relay as JSON (`row_edit` op
    /// `apply_batch`): it round-trips exactly, and fields a future client
    /// omits default instead of failing the whole save.
    #[test]
    fn staged_batch_round_trips_through_json_and_tolerates_missing_fields() {
        let batch = StagedBatch {
            target: Some(TableTarget {
                schema: "dbo".into(),
                table: "t".into(),
            }),
            key_columns: vec!["id".into()],
            ops: vec![StagedOp::Delete {
                request: DeleteRowRequest {
                    target: TableTarget {
                        schema: "dbo".into(),
                        table: "t".into(),
                    },
                    key: vec![("id".into(), CellValue::I64(1))],
                    rowversion: None,
                    original_values: Vec::new(),
                },
                original_row: vec![("id".into(), CellValue::I64(1))],
            }],
        };
        let json = serde_json::to_string(&batch).unwrap();
        assert_eq!(serde_json::from_str::<StagedBatch>(&json).unwrap(), batch);
        let sparse: StagedBatch = serde_json::from_str("{}").unwrap();
        assert!(sparse.ops.is_empty() && sparse.target.is_none());
    }
    use sqlly_core::DatabaseType;

    fn sample_target() -> TableTarget {
        TableTarget {
            schema: "dbo".to_string(),
            table: "Items".to_string(),
        }
    }

    fn key_pair() -> Vec<(String, CellValue)> {
        vec![("id".to_string(), CellValue::I64(1))]
    }

    fn sample_update() -> UpdateRowRequest {
        UpdateRowRequest {
            target: sample_target(),
            key: key_pair(),
            rowversion: None,
            changes: vec![("name".to_string(), CellValue::String("new".to_string()))],
            original_values: vec![("name".to_string(), CellValue::String("old".to_string()))],
            expressions: Vec::new(),
        }
    }

    fn sample_delete() -> StagedOp {
        StagedOp::Delete {
            request: DeleteRowRequest {
                target: sample_target(),
                key: key_pair(),
                rowversion: None,
                original_values: vec![("name".to_string(), CellValue::String("old".to_string()))],
            },
            original_row: vec![
                ("id".to_string(), CellValue::I64(1)),
                ("name".to_string(), CellValue::String("old".to_string())),
            ],
        }
    }

    fn sample_insert() -> InsertRowRequest {
        InsertRowRequest {
            target: sample_target(),
            values: vec![
                ("id".to_string(), CellValue::I64(2)),
                ("name".to_string(), CellValue::String("fresh".to_string())),
            ],
            expressions: Vec::new(),
        }
    }

    #[test]
    fn batch_orders_deletes_updates_inserts() {
        let batch = StagedBatch {
            target: Some(sample_target()),
            key_columns: vec!["id".to_string()],
            ops: vec![
                StagedOp::Insert(sample_insert()),
                StagedOp::Update(sample_update()),
                sample_delete(),
            ],
        };
        let stmts = build_batch_statements(DatabaseType::SqlServer, &batch);
        assert_eq!(stmts.len(), 3);
        assert!(stmts[0].sql.starts_with("DELETE FROM"));
        assert!(stmts[1].sql.starts_with("UPDATE"));
        assert!(stmts[2].sql.starts_with("INSERT INTO"));
    }

    #[test]
    fn tds_statements_use_at_placeholders_and_xact_abort_preview() {
        let batch = StagedBatch {
            target: Some(sample_target()),
            key_columns: vec!["id".to_string()],
            ops: vec![StagedOp::Update(sample_update())],
        };
        let stmts = build_batch_statements(DatabaseType::SqlServer, &batch);
        assert!(stmts[0].sql.contains("@P"));
        let preview = render_preview_script(DatabaseType::SqlServer, &batch);
        assert!(preview.contains("SET XACT_ABORT ON"));
        assert!(preview.contains("BEGIN TRANSACTION"));
        assert!(preview.contains("COMMIT TRANSACTION"));
        assert!(preview.contains("N'new'"));
    }

    #[test]
    fn pg_preview_uses_dollar_params_and_begin_commit() {
        let batch = StagedBatch {
            target: Some(sample_target()),
            key_columns: vec!["id".to_string()],
            ops: vec![StagedOp::Update(sample_update())],
        };
        let stmts = build_batch_statements(DatabaseType::Postgres, &batch);
        assert!(stmts[0].sql.contains("$1"));
        let preview = render_preview_script(DatabaseType::Postgres, &batch);
        assert!(preview.starts_with("BEGIN;"));
        assert!(preview.contains("COMMIT;"));
    }

    #[test]
    fn mysql_preview_uses_start_transaction() {
        let batch = StagedBatch {
            target: Some(sample_target()),
            key_columns: vec!["id".to_string()],
            ops: vec![StagedOp::Update(sample_update())],
        };
        let preview = render_preview_script(DatabaseType::MySql, &batch);
        assert!(preview.contains("START TRANSACTION"));
        assert!(preview.contains("COMMIT;"));
    }

    #[test]
    fn sqlite_preview_uses_begin_immediate() {
        let batch = StagedBatch {
            target: Some(sample_target()),
            key_columns: vec!["id".to_string()],
            ops: vec![StagedOp::Update(sample_update())],
        };
        let preview = render_preview_script(DatabaseType::Sqlite, &batch);
        assert!(preview.contains("BEGIN IMMEDIATE"));
        assert!(preview.contains("COMMIT;"));
    }

    #[test]
    fn rollback_reverses_update_to_original_values() {
        let batch = StagedBatch {
            target: Some(sample_target()),
            key_columns: vec!["id".to_string()],
            ops: vec![StagedOp::Update(sample_update())],
        };
        let script = render_rollback_script(DatabaseType::SqlServer, &batch);
        assert!(script.contains("N'old'"));
        assert!(script.contains("WHERE [id] = 1"));
    }

    #[test]
    fn rollback_reinserts_deleted_row_all_columns() {
        let batch = StagedBatch {
            target: Some(sample_target()),
            key_columns: vec!["id".to_string()],
            ops: vec![sample_delete()],
        };
        let script = render_rollback_script(DatabaseType::SqlServer, &batch);
        assert!(script.contains("INSERT INTO"));
        assert!(script.contains("N'old'"));
    }

    #[test]
    fn rollback_deletes_insert_when_key_supplied_else_comment() {
        let with_key = StagedBatch {
            target: Some(sample_target()),
            key_columns: vec!["id".to_string()],
            ops: vec![StagedOp::Insert(sample_insert())],
        };
        let script = render_rollback_script(DatabaseType::SqlServer, &with_key);
        assert!(script.contains("DELETE FROM"));

        let no_key = StagedBatch {
            target: Some(sample_target()),
            key_columns: vec!["id".to_string()],
            ops: vec![StagedOp::Insert(InsertRowRequest {
                target: sample_target(),
                values: vec![("name".to_string(), CellValue::String("x".to_string()))],
                expressions: Vec::new(),
            })],
        };
        let script = render_rollback_script(DatabaseType::SqlServer, &no_key);
        assert!(script.contains("-- cannot reverse insert"));
    }

    #[test]
    fn rollback_ops_are_in_reverse_order() {
        let batch = StagedBatch {
            target: Some(sample_target()),
            key_columns: vec!["id".to_string()],
            ops: vec![
                StagedOp::Update(sample_update()),
                StagedOp::Insert(sample_insert()),
            ],
        };
        let script = render_rollback_script(DatabaseType::SqlServer, &batch);
        let delete_pos = script.find("DELETE FROM").unwrap();
        let update_pos = script.find("UPDATE").unwrap();
        assert!(delete_pos < update_pos);
    }

    #[test]
    fn expect_affected_is_one_for_update_and_delete_none_for_insert() {
        let batch = StagedBatch {
            target: Some(sample_target()),
            key_columns: vec!["id".to_string()],
            ops: vec![
                sample_delete(),
                StagedOp::Update(sample_update()),
                StagedOp::Insert(sample_insert()),
            ],
        };
        let stmts = build_batch_statements(DatabaseType::Sqlite, &batch);
        assert_eq!(stmts[0].expect_affected, Some(1));
        assert_eq!(stmts[1].expect_affected, Some(1));
        assert_eq!(stmts[2].expect_affected, None);
    }

    #[test]
    fn null_values_render_is_null_in_predicates() {
        let mut update = sample_update();
        update.key = vec![("id".to_string(), CellValue::Null)];
        let batch = StagedBatch {
            target: Some(sample_target()),
            key_columns: vec!["id".to_string()],
            ops: vec![StagedOp::Update(update)],
        };
        let preview = render_preview_script(DatabaseType::Postgres, &batch);
        assert!(preview.contains("IS NULL"));
    }

    #[test]
    fn preview_shows_expression_text() {
        let mut update = sample_update();
        update.expressions = vec![
            (
                "b".to_string(),
                crate::row_editing::ColumnExpression::Default,
            ),
            (
                "UpdatedAt".to_string(),
                crate::row_editing::ColumnExpression::ServerTime,
            ),
        ];
        let batch = StagedBatch {
            target: Some(sample_target()),
            key_columns: vec!["id".to_string()],
            ops: vec![StagedOp::Update(update)],
        };
        let preview = render_preview_script(DatabaseType::SqlServer, &batch);
        assert!(preview.contains("[b] = DEFAULT"));
        assert!(preview.contains("[UpdatedAt] = SYSDATETIME()"));
    }

    fn guarded_batch() -> StagedBatch {
        StagedBatch {
            target: Some(sample_target()),
            key_columns: vec!["id".to_string()],
            ops: vec![
                StagedOp::Insert(sample_insert()),
                StagedOp::Update(sample_update()),
                sample_delete(),
            ],
        }
    }

    #[test]
    fn copy_script_guards_each_update_and_delete_on_sql_server() {
        let script = render_copy_script(DatabaseType::SqlServer, &guarded_batch());
        assert!(script.contains("SET XACT_ABORT ON"));
        assert_eq!(script.matches("IF @@ROWCOUNT <> 1 THROW 50001").count(), 2);
        // The guard follows its statement, and the insert is unguarded.
        let update = script.find("UPDATE ").unwrap();
        let first_guard = script.find("IF @@ROWCOUNT").unwrap();
        let delete = script.find("DELETE FROM").unwrap();
        assert!(delete < first_guard && first_guard < update);
        let insert = script.find("INSERT INTO").unwrap();
        assert!(script[insert..].find("@@ROWCOUNT").is_none());
        assert!(script.starts_with("-- Staged changes copied from SQLLY"));
    }

    #[test]
    fn copy_script_wraps_postgres_statements_in_checked_do_blocks() {
        let script = render_copy_script(DatabaseType::Postgres, &guarded_batch());
        assert_eq!(script.matches("DO $sqlly$").count(), 2);
        assert_eq!(script.matches("GET DIAGNOSTICS n = ROW_COUNT;").count(), 2);
        assert!(script.contains("RAISE EXCEPTION"));
        assert!(script.contains("BEGIN;\n"));
        assert!(script.trim_end().ends_with("COMMIT;"));
    }

    #[test]
    fn copy_script_dollar_tag_cannot_be_closed_by_a_value() {
        // A staged value containing the default tag must not terminate the
        // DO block early: the tag is chosen to be absent from the statement.
        let mut update = sample_update();
        update.changes = vec![(
            "name".to_string(),
            CellValue::String("x$sqlly$; DROP TABLE t; --".to_string()),
        )];
        let batch = StagedBatch {
            target: Some(sample_target()),
            key_columns: vec!["id".to_string()],
            ops: vec![StagedOp::Update(update)],
        };
        let script = render_copy_script(DatabaseType::Postgres, &batch);
        assert!(script.contains("DO $sqlly1$"), "{script}");
        assert_eq!(script.matches("$sqlly1$").count(), 2);
    }

    #[test]
    fn copy_script_uses_plsql_blocks_on_oracle() {
        let script = render_copy_script(DatabaseType::Oracle, &guarded_batch());
        assert_eq!(script.matches("IF SQL%ROWCOUNT <> 1 THEN").count(), 2);
        assert_eq!(script.matches("\nEND;\n/").count(), 2);
        assert!(script.contains("WHENEVER SQLERROR EXIT ROLLBACK"));
    }

    #[test]
    fn copy_script_is_honest_where_no_check_is_possible() {
        for db in [
            DatabaseType::MySql,
            DatabaseType::Sqlite,
            DatabaseType::CockroachDb,
        ] {
            let script = render_copy_script(db, &guarded_batch());
            assert_eq!(script.matches("-- expect 1 row").count(), 2, "{db:?}");
            assert!(script.contains("compare each row count yourself"), "{db:?}");
            assert!(!script.contains("No transaction wrapper"), "{db:?}");
        }
        // libSQL's save is transactional (BEGIN IMMEDIATE), so its script is too.
        let libsql = render_copy_script(DatabaseType::LibSql, &guarded_batch());
        assert!(libsql.contains("BEGIN IMMEDIATE;"), "{libsql}");
        assert!(!libsql.contains("No transaction wrapper"));
        let preview = render_preview_script(DatabaseType::LibSql, &guarded_batch());
        assert!(preview.starts_with("BEGIN IMMEDIATE;"), "{preview}");
        // An engine with no wrapper says so rather than implying atomicity.
        let clickhouse = render_copy_script(DatabaseType::ClickHouse, &guarded_batch());
        assert!(clickhouse.contains("No transaction wrapper"));
    }

    #[test]
    fn oracle_default_values_comment_never_embeds_the_table_name() {
        let insert = InsertRowRequest {
            target: TableTarget {
                schema: "S".to_string(),
                table: "evil\nDROP TABLE x".to_string(),
            },
            values: Vec::new(),
            expressions: Vec::new(),
        };
        let line = render_inline_insert(DatabaseType::Oracle, &insert);
        assert!(!line.contains('\n'), "{line}");
        assert!(line.starts_with("--"));
    }
}
