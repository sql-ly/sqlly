//! Dialect-generic SQL construction for row editing on PostgreSQL,
//! MySQL/MariaDB, SQLite, and DuckDB.
//!
//! Mirrors the guarantees of the T-SQL builders in [`crate::row_editing`]:
//! rows are addressed only by real key columns, every value travels as a
//! parameter (`$n` on the PostgreSQL wire, `?` elsewhere), NULL comparisons
//! render as `IS NULL` in the SQL text so parameters never need typed NULL
//! binding, and identifiers are quoted with the engine's own quoting style
//! via [`DatabaseType::quote_identifier`]. Everything here is pure and unit
//! tested without a server.

use sqlly_core::{CellValue, ConnectionKind, DatabaseType, SqlParam, StudioResult};

use crate::row_editing::{
    ColumnExpression, DeleteRowRequest, FkLabelRequest, FkSearchRequest, InsertRowRequest,
    RelatedRowsRequest, TableTarget, UpdateRowRequest,
};

/// Dialect spelling of a [`ColumnExpression`].
pub fn expression_sql(db: DatabaseType, expr: ColumnExpression) -> &'static str {
    match (expr, db.connection_kind()) {
        (ColumnExpression::Default, _) => "DEFAULT",
        (ColumnExpression::ServerTime, ConnectionKind::Tds) => "SYSDATETIME()",
        (ColumnExpression::ServerTime, ConnectionKind::MySqlWire) => "NOW()",
        (ColumnExpression::ServerTime, ConnectionKind::OracleNet) => "SYSTIMESTAMP",
        (ColumnExpression::ServerTime, _) => "CURRENT_TIMESTAMP", // PostgreSQL, SQLite, DuckDB
        (ColumnExpression::ServerDate, ConnectionKind::Tds) => "CAST(SYSDATETIME() AS date)",
        (ColumnExpression::ServerDate, ConnectionKind::MySqlWire) => "CURDATE()",
        (ColumnExpression::ServerDate, ConnectionKind::OracleNet) => "TRUNC(SYSDATE)",
        (ColumnExpression::ServerDate, _) => "CURRENT_DATE",
        (ColumnExpression::ServerUuid, ConnectionKind::Tds) => "NEWID()",
        (ColumnExpression::ServerUuid, ConnectionKind::PostgresWire) => "gen_random_uuid()",
        (ColumnExpression::ServerUuid, ConnectionKind::MySqlWire) => "UUID()",
        (ColumnExpression::ServerUuid, ConnectionKind::OracleNet) => "SYS_GUID()",
        (ColumnExpression::ServerUuid, ConnectionKind::DuckDbFile) => "uuid()",
        (ColumnExpression::ServerUuid, ConnectionKind::ClickHouseHttp) => "generateUUIDv4()",
        // SQLite / libSQL have no UUID function; 16 random bytes as hex is
        // the idiomatic stand-in.
        (ColumnExpression::ServerUuid, _) => "lower(hex(randomblob(16)))",
        (ColumnExpression::CurrentUser, ConnectionKind::Tds) => "SUSER_SNAME()",
        (ColumnExpression::CurrentUser, ConnectionKind::MySqlWire) => "CURRENT_USER()",
        (ColumnExpression::CurrentUser, ConnectionKind::OracleNet) => "USER",
        (ColumnExpression::CurrentUser, _) => "CURRENT_USER",
    }
}

/// Whether the engine can evaluate `expr`. File-embedded engines (SQLite,
/// libSQL, DuckDB) have no login, so `CurrentUser` is withheld there; every
/// other expression has a spelling on every row-editable engine.
pub fn expression_supported(db: DatabaseType, expr: ColumnExpression) -> bool {
    match expr {
        ColumnExpression::CurrentUser => matches!(
            db.connection_kind(),
            ConnectionKind::Tds
                | ConnectionKind::PostgresWire
                | ConnectionKind::MySqlWire
                | ConnectionKind::OracleNet
        ),
        _ => true,
    }
}

/// `schema.table` (or just `table` when the target has no schema part),
/// each part quoted with the engine's identifier style.
pub fn quote_table(db: DatabaseType, target: &TableTarget) -> String {
    if target.schema.is_empty() {
        db.quote_identifier(&target.table)
    } else {
        format!(
            "{}.{}",
            db.quote_identifier(&target.schema),
            db.quote_identifier(&target.table)
        )
    }
}

/// The engine's positional parameter placeholder for 1-based ordinal `n`.
pub fn placeholder(db: DatabaseType, n: usize) -> String {
    match db.connection_kind() {
        ConnectionKind::PostgresWire => format!("${n}"),
        ConnectionKind::Tds => format!("@P{n}"),
        ConnectionKind::OracleNet => format!(":{n}"),
        _ => "?".to_string(),
    }
}

/// The engine's bounded-fetch clause (`LIMIT n`, or `FETCH FIRST n ROWS
/// ONLY` where LIMIT does not exist), with its leading space.
fn limit_clause(db: DatabaseType, n: usize) -> String {
    match db.connection_kind() {
        ConnectionKind::OracleNet => format!(" FETCH FIRST {n} ROWS ONLY"),
        _ => format!(" LIMIT {n}"),
    }
}

/// Appends `col = <placeholder>` (binding `value`) or `col IS NULL` to a
/// predicate list, mirroring the T-SQL `push_predicate`.
fn push_predicate(
    db: DatabaseType,
    predicates: &mut Vec<String>,
    params: &mut Vec<SqlParam>,
    column: &str,
    value: &CellValue,
) {
    if matches!(value, CellValue::Null) {
        predicates.push(format!("{} IS NULL", db.quote_identifier(column)));
    } else {
        params.push(SqlParam::from(value));
        predicates.push(format!(
            "{} = {}",
            db.quote_identifier(column),
            placeholder(db, params.len())
        ));
    }
}

/// Builds the parameterized guarded UPDATE. The WHERE clause covers the key
/// plus the rowversion when available (none of these engines has one, but the
/// contract is shared), otherwise the original values of every changed column
/// — the same optimistic-concurrency fallback as the T-SQL builder.
pub fn build_update_sql_for(
    db: DatabaseType,
    request: &UpdateRowRequest,
) -> (String, Vec<SqlParam>) {
    let mut params: Vec<SqlParam> = Vec::new();
    let mut assignments: Vec<String> = Vec::new();
    for (column, value) in &request.changes {
        if matches!(value, CellValue::Null) {
            assignments.push(format!("{} = NULL", db.quote_identifier(column)));
        } else {
            params.push(SqlParam::from(value));
            assignments.push(format!(
                "{} = {}",
                db.quote_identifier(column),
                placeholder(db, params.len())
            ));
        }
    }
    for (column, expr) in &request.expressions {
        assignments.push(format!(
            "{} = {}",
            db.quote_identifier(column),
            expression_sql(db, *expr)
        ));
    }

    let mut predicates: Vec<String> = Vec::new();
    for (column, value) in &request.key {
        push_predicate(db, &mut predicates, &mut params, column, value);
    }
    match &request.rowversion {
        Some((column, value)) => push_predicate(db, &mut predicates, &mut params, column, value),
        None => {
            for (column, value) in &request.original_values {
                push_predicate(db, &mut predicates, &mut params, column, value);
            }
        }
    }

    let sql = format!(
        "UPDATE {} SET {} WHERE {}",
        quote_table(db, &request.target),
        assignments.join(", "),
        predicates.join(" AND ")
    );
    (sql, params)
}

/// Builds the parameterized guarded DELETE (key plus original values, the
/// same shape as the T-SQL builder).
pub fn build_delete_sql_for(
    db: DatabaseType,
    request: &DeleteRowRequest,
) -> (String, Vec<SqlParam>) {
    let mut params: Vec<SqlParam> = Vec::new();
    let mut predicates: Vec<String> = Vec::new();
    for (column, value) in &request.key {
        push_predicate(db, &mut predicates, &mut params, column, value);
    }
    match &request.rowversion {
        Some((column, value)) => push_predicate(db, &mut predicates, &mut params, column, value),
        None => {
            for (column, value) in &request.original_values {
                push_predicate(db, &mut predicates, &mut params, column, value);
            }
        }
    }
    let sql = format!(
        "DELETE FROM {} WHERE {}",
        quote_table(db, &request.target),
        predicates.join(" AND ")
    );
    (sql, params)
}

/// Builds the parameterized single-row fetch by key.
pub fn build_select_row_sql_for(
    db: DatabaseType,
    target: &TableTarget,
    key: &[(String, CellValue)],
) -> (String, Vec<SqlParam>) {
    let mut params: Vec<SqlParam> = Vec::new();
    let mut predicates: Vec<String> = Vec::new();
    for (column, value) in key {
        push_predicate(db, &mut predicates, &mut params, column, value);
    }
    let sql = format!(
        "SELECT * FROM {} WHERE {}",
        quote_table(db, target),
        predicates.join(" AND ")
    );
    (sql, params)
}

/// Builds the parameterized INSERT with a `RETURNING *` clause, for engines
/// that have one (PostgreSQL, SQLite, DuckDB). With no supplied values, emits
/// `DEFAULT VALUES`.
pub fn build_insert_returning_sql_for(
    db: DatabaseType,
    request: &InsertRowRequest,
) -> (String, Vec<SqlParam>) {
    let mut params: Vec<SqlParam> = Vec::new();
    let table = quote_table(db, &request.target);
    if request.values.is_empty() && request.expressions.is_empty() {
        return (
            format!("INSERT INTO {table} DEFAULT VALUES RETURNING *"),
            params,
        );
    }
    let mut columns: Vec<String> =
        Vec::with_capacity(request.values.len() + request.expressions.len());
    let mut placeholders: Vec<String> =
        Vec::with_capacity(request.values.len() + request.expressions.len());
    for (column, value) in &request.values {
        columns.push(db.quote_identifier(column));
        if matches!(value, CellValue::Null) {
            placeholders.push("NULL".to_string());
        } else {
            params.push(SqlParam::from(value));
            placeholders.push(placeholder(db, params.len()));
        }
    }
    for (column, expr) in &request.expressions {
        columns.push(db.quote_identifier(column));
        placeholders.push(expression_sql(db, *expr).to_string());
    }
    let sql = format!(
        "INSERT INTO {table} ({}) VALUES ({}) RETURNING *",
        columns.join(", "),
        placeholders.join(", ")
    );
    (sql, params)
}

/// Builds the parameterized INSERT without a returning clause (MySQL/MariaDB;
/// the inserted row is refetched inside the same transaction). With no
/// supplied values, emits the empty-list form `() VALUES ()`.
pub fn build_insert_plain_sql_for(
    db: DatabaseType,
    request: &InsertRowRequest,
) -> (String, Vec<SqlParam>) {
    let mut params: Vec<SqlParam> = Vec::new();
    let table = quote_table(db, &request.target);
    if request.values.is_empty() && request.expressions.is_empty() {
        return (format!("INSERT INTO {table} () VALUES ()"), params);
    }
    let mut columns: Vec<String> =
        Vec::with_capacity(request.values.len() + request.expressions.len());
    let mut placeholders: Vec<String> =
        Vec::with_capacity(request.values.len() + request.expressions.len());
    for (column, value) in &request.values {
        columns.push(db.quote_identifier(column));
        if matches!(value, CellValue::Null) {
            placeholders.push("NULL".to_string());
        } else {
            params.push(SqlParam::from(value));
            placeholders.push(placeholder(db, params.len()));
        }
    }
    for (column, expr) in &request.expressions {
        columns.push(db.quote_identifier(column));
        placeholders.push(expression_sql(db, *expr).to_string());
    }
    let sql = format!(
        "INSERT INTO {table} ({}) VALUES ({})",
        columns.join(", "),
        placeholders.join(", ")
    );
    (sql, params)
}

/// Escapes standard `LIKE` metacharacters (`%`, `_`, `\`) with a backslash so
/// a user's filter text is matched literally. Unlike the T-SQL variant this
/// does NOT escape `[` — outside SQL Server, `[` has no meaning in a LIKE
/// pattern and escaping it is either pointless or (MySQL) an error.
pub fn escape_like_std(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if matches!(ch, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// The engine's cast-to-text spelling for LIKE filtering on arbitrary columns.
fn cast_text(db: DatabaseType, expr: &str) -> String {
    match db.connection_kind() {
        ConnectionKind::MySqlWire => format!("CAST({expr} AS CHAR)"),
        ConnectionKind::OracleNet => format!("TO_CHAR({expr})"),
        _ => format!("CAST({expr} AS TEXT)"),
    }
}

/// Builds the bounded, parameterized referenced-table search for FK
/// autocomplete: same shape as the T-SQL builder, with `LIMIT n + 1` and the
/// engine's LIKE-escape convention (explicit `ESCAPE '\'` on PostgreSQL and
/// SQLite; MySQL's default escape is already the backslash).
pub fn build_fk_search_sql_for(
    db: DatabaseType,
    request: &FkSearchRequest,
) -> (String, Vec<SqlParam>) {
    let limit = request.limit.max(1);
    let mut selected: Vec<String> = request
        .key_columns
        .iter()
        .map(|c| db.quote_identifier(c))
        .collect();
    let order_by = match &request.display_column {
        Some(display) => {
            selected.push(db.quote_identifier(display));
            db.quote_identifier(display)
        }
        None => selected.first().cloned().unwrap_or_else(|| "1".to_string()),
    };

    let filter_column = request
        .display_column
        .as_ref()
        .map(|d| db.quote_identifier(d))
        .or_else(|| request.key_columns.first().map(|c| db.quote_identifier(c)));

    let mut params: Vec<SqlParam> = Vec::new();
    let where_clause = match (filter_column, request.query.trim().is_empty()) {
        (Some(column), false) => {
            params.push(SqlParam::String(format!(
                "%{}%",
                escape_like_std(request.query.trim())
            )));
            let escape = match db.connection_kind() {
                ConnectionKind::MySqlWire => "",
                _ => " ESCAPE '\\'",
            };
            // DuckDB's LIKE is case-sensitive; ILIKE gives the autocomplete
            // the same "type a few letters" feel as the other engines.
            let like = match db.connection_kind() {
                ConnectionKind::DuckDbFile => "ILIKE",
                _ => "LIKE",
            };
            format!(
                " WHERE {} {like} {}{}",
                cast_text(db, &column),
                placeholder(db, 1),
                escape
            )
        }
        _ => String::new(),
    };

    let sql = format!(
        "SELECT {} FROM {}{} ORDER BY {}{}",
        selected.join(", "),
        quote_table(db, &request.referenced),
        where_clause,
        order_by,
        limit_clause(db, limit + 1),
    );
    (sql, params)
}

/// Builds the bounded parameterized fetch of related rows (`LIMIT n + 1` so
/// the caller can flag truncation).
pub fn build_related_rows_sql_for(
    db: DatabaseType,
    request: &RelatedRowsRequest,
) -> (String, Vec<SqlParam>) {
    let limit = request.limit.max(1);
    let mut params: Vec<SqlParam> = Vec::new();
    let mut predicates: Vec<String> = Vec::new();
    for (column, value) in &request.predicates {
        push_predicate(db, &mut predicates, &mut params, column, value);
    }
    let where_clause = if predicates.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", predicates.join(" AND "))
    };
    let sql = format!(
        "SELECT * FROM {}{}{}",
        quote_table(db, &request.table),
        where_clause,
        limit_clause(db, limit + 1)
    );
    (sql, params)
}

/// Builds the bounded display-label lookup for one FK value:
/// `SELECT display FROM referenced WHERE ref_column = ? LIMIT 1`.
pub fn build_fk_display_sql_for(
    db: DatabaseType,
    referenced: &TableTarget,
    display: &str,
    ref_column: &str,
    value: &CellValue,
) -> (String, Vec<SqlParam>) {
    let mut params: Vec<SqlParam> = Vec::new();
    let mut predicates: Vec<String> = Vec::new();
    push_predicate(db, &mut predicates, &mut params, ref_column, value);
    let sql = format!(
        "SELECT {} FROM {} WHERE {}{}",
        db.quote_identifier(display),
        quote_table(db, referenced),
        predicates.join(" AND "),
        limit_clause(db, 1)
    );
    (sql, params)
}

const FK_LABELS_MAX_KEYS: usize = 500;

fn display_cast_sql(db: DatabaseType, column: &str) -> String {
    let quoted = db.quote_identifier(column);
    match db.connection_kind() {
        ConnectionKind::Tds => format!("CAST({quoted} AS NVARCHAR(MAX))"),
        ConnectionKind::MySqlWire => format!("CAST({quoted} AS CHAR)"),
        ConnectionKind::OracleNet => format!("TO_CHAR({quoted})"),
        _ => format!("CAST({quoted} AS TEXT)"),
    }
}

/// Builds a batched display-label lookup for many FK key values:
/// `SELECT key, CAST(display AS …) FROM referenced WHERE key IN (?, …)`.
pub fn build_fk_labels_sql_for(
    db: DatabaseType,
    request: &FkLabelRequest,
) -> StudioResult<(String, Vec<SqlParam>)> {
    if request.keys.len() > FK_LABELS_MAX_KEYS {
        return Err(sqlly_core::StudioError::InvalidInput(format!(
            "FK label lookup accepts at most {FK_LABELS_MAX_KEYS} keys per request"
        )));
    }
    if request.keys.is_empty() {
        return Ok((String::new(), Vec::new()));
    }
    let mut params: Vec<SqlParam> = Vec::new();
    let mut placeholders: Vec<String> = Vec::new();
    for key in &request.keys {
        if matches!(key, CellValue::Null) {
            continue;
        }
        params.push(SqlParam::from(key));
        placeholders.push(placeholder(db, params.len()));
    }
    if placeholders.is_empty() {
        return Ok((String::new(), Vec::new()));
    }
    let key_col = db.quote_identifier(&request.key_column);
    let display = display_cast_sql(db, &request.display_column);
    let sql = format!(
        "SELECT {key_col}, {display} FROM {} WHERE {key_col} IN ({})",
        quote_table(db, &request.referenced),
        placeholders.join(", ")
    );
    Ok((sql, params))
}

/// Renders a cell as a SQL literal in the engine's own syntax, for the
/// value-inlined "Open query" statement (which is not parameterized).
pub fn sql_literal_for(db: DatabaseType, value: &CellValue) -> String {
    match value {
        CellValue::Null => "NULL".to_string(),
        CellValue::Bool(b) => match db.connection_kind() {
            ConnectionKind::PostgresWire | ConnectionKind::DuckDbFile => {
                if *b { "TRUE" } else { "FALSE" }.to_string()
            }
            _ => if *b { "1" } else { "0" }.to_string(),
        },
        CellValue::I64(n) => n.to_string(),
        CellValue::F64(f) => f.to_string(),
        CellValue::String(s) => {
            let mut escaped = s.replace('\'', "''");
            if matches!(
                db.connection_kind(),
                ConnectionKind::MySqlWire | ConnectionKind::ClickHouseHttp
            ) {
                // MySQL's default string parser treats the backslash as an
                // escape character; double it so the literal stays literal.
                escaped = escaped.replace('\\', "\\\\");
            }
            format!("'{escaped}'")
        }
        CellValue::Bytes(b) => {
            let hex: String = b.iter().map(|byte| format!("{byte:02X}")).collect();
            match db.connection_kind() {
                ConnectionKind::PostgresWire => format!("'\\x{hex}'"),
                // DuckDB reads PostgreSQL's hex escape, but only as a BLOB
                // once cast; a bare string literal would compare as text.
                ConnectionKind::DuckDbFile => format!("'\\x{hex}'::BLOB"),
                ConnectionKind::OracleNet => format!("HEXTORAW('{hex}')"),
                _ => format!("X'{hex}'"),
            }
        }
    }
}

/// Builds the full (untruncated, value-inlined) related-rows query for the
/// "Open query" action, in the engine's own literal syntax.
pub fn build_related_open_sql_for(db: DatabaseType, request: &RelatedRowsRequest) -> String {
    let predicates: Vec<String> = request
        .predicates
        .iter()
        .map(|(column, value)| {
            if matches!(value, CellValue::Null) {
                format!("{} IS NULL", db.quote_identifier(column))
            } else {
                format!(
                    "{} = {}",
                    db.quote_identifier(column),
                    sql_literal_for(db, value)
                )
            }
        })
        .collect();
    let where_clause = if predicates.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", predicates.join(" AND "))
    };
    format!(
        "SELECT * FROM {}{};",
        quote_table(db, &request.table),
        where_clause
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_literals_escape_backslashes_on_clickhouse() {
        assert_eq!(
            sql_literal_for(DatabaseType::ClickHouse, &CellValue::String(r"a\".into())),
            r"'a\\'"
        );
    }

    fn target(schema: &str, table: &str) -> TableTarget {
        TableTarget {
            schema: schema.to_string(),
            table: table.to_string(),
        }
    }

    #[test]
    fn sql_function_expressions_spell_per_dialect() {
        use ColumnExpression::*;
        let ss = DatabaseType::SqlServer;
        let pg = DatabaseType::Postgres;
        let my = DatabaseType::MySql;
        let lite = DatabaseType::Sqlite;
        let ora = DatabaseType::Oracle;
        let duck = DatabaseType::DuckDb;
        assert_eq!(
            expression_sql(ss, ServerDate),
            "CAST(SYSDATETIME() AS date)"
        );
        assert_eq!(expression_sql(pg, ServerDate), "CURRENT_DATE");
        assert_eq!(expression_sql(my, ServerDate), "CURDATE()");
        assert_eq!(expression_sql(ora, ServerDate), "TRUNC(SYSDATE)");
        assert_eq!(expression_sql(ss, ServerUuid), "NEWID()");
        assert_eq!(expression_sql(pg, ServerUuid), "gen_random_uuid()");
        assert_eq!(expression_sql(my, ServerUuid), "UUID()");
        assert_eq!(expression_sql(duck, ServerUuid), "uuid()");
        assert_eq!(
            expression_sql(lite, ServerUuid),
            "lower(hex(randomblob(16)))"
        );
        assert_eq!(expression_sql(ss, CurrentUser), "SUSER_SNAME()");
        assert_eq!(expression_sql(pg, CurrentUser), "CURRENT_USER");
        assert_eq!(expression_sql(my, CurrentUser), "CURRENT_USER()");
        assert_eq!(expression_sql(ora, CurrentUser), "USER");
    }

    #[test]
    fn current_user_is_withheld_on_embedded_engines() {
        use ColumnExpression::*;
        for db in [
            DatabaseType::SqlServer,
            DatabaseType::Postgres,
            DatabaseType::MySql,
            DatabaseType::Oracle,
        ] {
            assert!(expression_supported(db, CurrentUser), "{db:?}");
        }
        for db in [
            DatabaseType::Sqlite,
            DatabaseType::LibSql,
            DatabaseType::DuckDb,
        ] {
            assert!(!expression_supported(db, CurrentUser), "{db:?}");
            assert!(expression_supported(db, ServerUuid), "{db:?}");
            assert!(expression_supported(db, ServerDate), "{db:?}");
        }
    }

    #[test]
    fn update_renders_sql_function_expressions_inline() {
        let request = UpdateRowRequest {
            target: target("public", "items"),
            key: vec![("id".to_string(), CellValue::I64(1))],
            rowversion: None,
            changes: Vec::new(),
            original_values: Vec::new(),
            expressions: vec![
                ("token".to_string(), ColumnExpression::ServerUuid),
                ("owner".to_string(), ColumnExpression::CurrentUser),
            ],
        };
        let (sql, _) = build_update_sql_for(DatabaseType::Postgres, &request);
        assert!(sql.contains("\"token\" = gen_random_uuid()"), "{sql}");
        assert!(sql.contains("\"owner\" = CURRENT_USER"), "{sql}");
    }

    #[test]
    fn quoting_follows_engine_style() {
        let t = target("public", "orders");
        assert_eq!(
            quote_table(DatabaseType::Postgres, &t),
            "\"public\".\"orders\""
        );
        assert_eq!(quote_table(DatabaseType::MySql, &t), "`public`.`orders`");
        assert_eq!(
            quote_table(DatabaseType::Sqlite, &target("main", "t")),
            "\"main\".\"t\""
        );
        assert_eq!(quote_table(DatabaseType::Sqlite, &target("", "t")), "\"t\"");
    }

    #[test]
    fn update_sql_pg_uses_dollar_placeholders_and_original_values() {
        let request = UpdateRowRequest {
            target: target("public", "orders"),
            key: vec![("id".to_string(), CellValue::I64(7))],
            rowversion: None,
            changes: vec![("name".to_string(), CellValue::String("x".into()))],
            original_values: vec![("name".to_string(), CellValue::String("old".into()))],
            expressions: Vec::new(),
        };
        let (sql, params) = build_update_sql_for(DatabaseType::Postgres, &request);
        assert_eq!(
            sql,
            "UPDATE \"public\".\"orders\" SET \"name\" = $1 WHERE \"id\" = $2 AND \"name\" = $3"
        );
        assert_eq!(
            params,
            vec![
                SqlParam::String("x".into()),
                SqlParam::I64(7),
                SqlParam::String("old".into()),
            ]
        );
    }

    #[test]
    fn update_sql_mysql_uses_question_placeholders_and_null_literals() {
        let request = UpdateRowRequest {
            target: target("app", "t"),
            key: vec![("id".to_string(), CellValue::I64(1))],
            rowversion: None,
            changes: vec![("a".to_string(), CellValue::Null)],
            original_values: vec![("a".to_string(), CellValue::Null)],
            expressions: Vec::new(),
        };
        let (sql, params) = build_update_sql_for(DatabaseType::MySql, &request);
        assert_eq!(
            sql,
            "UPDATE `app`.`t` SET `a` = NULL WHERE `id` = ? AND `a` IS NULL"
        );
        assert_eq!(params, vec![SqlParam::I64(1)]);
    }

    #[test]
    fn delete_sql_falls_back_to_original_values() {
        let request = DeleteRowRequest {
            target: target("main", "t"),
            key: vec![("id".to_string(), CellValue::I64(3))],
            rowversion: None,
            original_values: vec![
                ("name".to_string(), CellValue::String("x".into())),
                ("note".to_string(), CellValue::Null),
            ],
        };
        let (sql, params) = build_delete_sql_for(DatabaseType::Sqlite, &request);
        assert_eq!(
            sql,
            "DELETE FROM \"main\".\"t\" WHERE \"id\" = ? AND \"name\" = ? AND \"note\" IS NULL"
        );
        assert_eq!(params, vec![SqlParam::I64(3), SqlParam::String("x".into())]);
    }

    #[test]
    fn select_row_sql_binds_key_in_order() {
        let key = vec![
            ("a".to_string(), CellValue::I64(1)),
            ("b".to_string(), CellValue::String("x".into())),
        ];
        let (sql, params) =
            build_select_row_sql_for(DatabaseType::Postgres, &target("s", "t"), &key);
        assert_eq!(
            sql,
            "SELECT * FROM \"s\".\"t\" WHERE \"a\" = $1 AND \"b\" = $2"
        );
        assert_eq!(params.len(), 2);
    }

    #[test]
    fn insert_returning_sql_renders_null_literally_and_default_values() {
        let request = InsertRowRequest {
            target: target("main", "person"),
            values: vec![
                ("name".to_string(), CellValue::String("Ada".into())),
                ("note".to_string(), CellValue::Null),
            ],
            expressions: Vec::new(),
        };
        let (sql, params) = build_insert_returning_sql_for(DatabaseType::Sqlite, &request);
        assert_eq!(
            sql,
            "INSERT INTO \"main\".\"person\" (\"name\", \"note\") VALUES (?, NULL) RETURNING *"
        );
        assert_eq!(params, vec![SqlParam::String("Ada".into())]);

        let empty = InsertRowRequest {
            target: target("public", "t"),
            values: vec![],
            expressions: Vec::new(),
        };
        let (sql, params) = build_insert_returning_sql_for(DatabaseType::Postgres, &empty);
        assert_eq!(
            sql,
            "INSERT INTO \"public\".\"t\" DEFAULT VALUES RETURNING *"
        );
        assert!(params.is_empty());
    }

    #[test]
    fn insert_plain_sql_uses_empty_list_form_when_empty() {
        let request = InsertRowRequest {
            target: target("app", "t"),
            values: vec![],
            expressions: Vec::new(),
        };
        let (sql, params) = build_insert_plain_sql_for(DatabaseType::MySql, &request);
        assert_eq!(sql, "INSERT INTO `app`.`t` () VALUES ()");
        assert!(params.is_empty());

        let with_values = InsertRowRequest {
            target: target("app", "t"),
            values: vec![("name".to_string(), CellValue::String("Ada".into()))],
            expressions: Vec::new(),
        };
        let (sql, params) = build_insert_plain_sql_for(DatabaseType::MySql, &with_values);
        assert_eq!(sql, "INSERT INTO `app`.`t` (`name`) VALUES (?)");
        assert_eq!(params, vec![SqlParam::String("Ada".into())]);
    }

    #[test]
    fn escape_like_std_escapes_metacharacters_but_not_brackets() {
        assert_eq!(escape_like_std("a%b_c\\d"), "a\\%b\\_c\\\\d");
        assert_eq!(escape_like_std("a[b]"), "a[b]");
    }

    #[test]
    fn fk_search_sql_pg_uses_escape_clause_and_limit() {
        let request = FkSearchRequest {
            referenced: target("public", "customer"),
            key_columns: vec!["id".into()],
            display_column: Some("name".into()),
            query: "a%c".into(),
            limit: 50,
        };
        let (sql, params) = build_fk_search_sql_for(DatabaseType::Postgres, &request);
        assert_eq!(
            sql,
            "SELECT \"id\", \"name\" FROM \"public\".\"customer\" \
             WHERE CAST(\"name\" AS TEXT) LIKE $1 ESCAPE '\\' ORDER BY \"name\" LIMIT 51"
        );
        assert_eq!(params, vec![SqlParam::String("%a\\%c%".into())]);
    }

    #[test]
    fn fk_search_sql_mysql_relies_on_default_backslash_escape() {
        let request = FkSearchRequest {
            referenced: target("app", "customer"),
            key_columns: vec!["id".into()],
            display_column: None,
            query: "ab".into(),
            limit: 10,
        };
        let (sql, params) = build_fk_search_sql_for(DatabaseType::MySql, &request);
        assert_eq!(
            sql,
            "SELECT `id` FROM `app`.`customer` WHERE CAST(`id` AS CHAR) LIKE ? \
             ORDER BY `id` LIMIT 11"
        );
        assert_eq!(params, vec![SqlParam::String("%ab%".into())]);
    }

    #[test]
    fn fk_search_sql_without_query_or_display_orders_by_key() {
        let request = FkSearchRequest {
            referenced: target("main", "customer"),
            key_columns: vec!["id".into()],
            display_column: None,
            query: "   ".into(),
            limit: 10,
        };
        let (sql, params) = build_fk_search_sql_for(DatabaseType::Sqlite, &request);
        assert_eq!(
            sql,
            "SELECT \"id\" FROM \"main\".\"customer\" ORDER BY \"id\" LIMIT 11"
        );
        assert!(params.is_empty());
    }

    #[test]
    fn related_rows_sql_is_bounded_and_parameterized() {
        let request = RelatedRowsRequest {
            table: target("public", "orders"),
            predicates: vec![("customer_id".into(), CellValue::I64(7))],
            limit: 100,
        };
        let (sql, params) = build_related_rows_sql_for(DatabaseType::Postgres, &request);
        assert_eq!(
            sql,
            "SELECT * FROM \"public\".\"orders\" WHERE \"customer_id\" = $1 LIMIT 101"
        );
        assert_eq!(params, vec![SqlParam::I64(7)]);
    }

    #[test]
    fn fk_display_sql_is_bounded_single_row() {
        let (sql, params) = build_fk_display_sql_for(
            DatabaseType::MySql,
            &target("app", "customer"),
            "name",
            "id",
            &CellValue::I64(42),
        );
        assert_eq!(
            sql,
            "SELECT `name` FROM `app`.`customer` WHERE `id` = ? LIMIT 1"
        );
        assert_eq!(params, vec![SqlParam::I64(42)]);
    }

    #[test]
    fn literals_follow_engine_syntax() {
        assert_eq!(
            sql_literal_for(DatabaseType::Postgres, &CellValue::Bool(true)),
            "TRUE"
        );
        assert_eq!(
            sql_literal_for(DatabaseType::MySql, &CellValue::Bool(true)),
            "1"
        );
        assert_eq!(
            sql_literal_for(DatabaseType::MySql, &CellValue::String("a\\'b".into())),
            "'a\\\\''b'"
        );
        assert_eq!(
            sql_literal_for(DatabaseType::Sqlite, &CellValue::String("a'b".into())),
            "'a''b'"
        );
        assert_eq!(
            sql_literal_for(DatabaseType::Postgres, &CellValue::Bytes(vec![0xAB, 0x01])),
            "'\\xAB01'"
        );
        assert_eq!(
            sql_literal_for(DatabaseType::Sqlite, &CellValue::Bytes(vec![0xAB])),
            "X'AB'"
        );
        assert_eq!(
            sql_literal_for(DatabaseType::DuckDb, &CellValue::Bool(false)),
            "FALSE"
        );
        assert_eq!(
            sql_literal_for(DatabaseType::DuckDb, &CellValue::Bytes(vec![0xAB, 0x01])),
            "'\\xAB01'::BLOB"
        );
        assert_eq!(
            sql_literal_for(DatabaseType::DuckDb, &CellValue::String("a'b".into())),
            "'a''b'"
        );
    }

    #[test]
    fn duckdb_builders_use_question_placeholders_and_double_quotes() {
        let request = UpdateRowRequest {
            target: target("main", "person"),
            key: vec![("id".to_string(), CellValue::I64(1))],
            rowversion: None,
            changes: vec![("name".to_string(), CellValue::String("x".into()))],
            original_values: vec![("name".to_string(), CellValue::String("old".into()))],
            expressions: vec![("note".to_string(), ColumnExpression::Default)],
        };
        let (sql, params) = build_update_sql_for(DatabaseType::DuckDb, &request);
        assert_eq!(
            sql,
            "UPDATE \"main\".\"person\" SET \"name\" = ?, \"note\" = DEFAULT WHERE \"id\" = ? AND \"name\" = ?"
        );
        assert_eq!(params.len(), 3);
        let insert = InsertRowRequest {
            target: target("main", "person"),
            values: vec![("name".to_string(), CellValue::String("Ada".into()))],
            expressions: Vec::new(),
        };
        let (sql, _) = build_insert_returning_sql_for(DatabaseType::DuckDb, &insert);
        assert_eq!(
            sql,
            "INSERT INTO \"main\".\"person\" (\"name\") VALUES (?) RETURNING *"
        );
        let search = FkSearchRequest {
            referenced: target("main", "customer"),
            key_columns: vec!["id".into()],
            display_column: Some("name".into()),
            query: "a".into(),
            limit: 5,
        };
        let (sql, _) = build_fk_search_sql_for(DatabaseType::DuckDb, &search);
        assert_eq!(
            sql,
            "SELECT \"id\", \"name\" FROM \"main\".\"customer\" \
             WHERE CAST(\"name\" AS TEXT) ILIKE ? ESCAPE '\\' ORDER BY \"name\" LIMIT 6"
        );
    }

    fn update_with_expressions(db_target: TableTarget) -> UpdateRowRequest {
        UpdateRowRequest {
            target: db_target,
            key: vec![("id".to_string(), CellValue::I64(1))],
            rowversion: None,
            changes: vec![("a".to_string(), CellValue::String("x".into()))],
            original_values: vec![("a".to_string(), CellValue::String("old".into()))],
            expressions: vec![
                ("b".to_string(), ColumnExpression::Default),
                ("c".to_string(), ColumnExpression::ServerTime),
            ],
        }
    }

    #[test]
    fn update_renders_default_and_server_time_pg() {
        let request = update_with_expressions(target("public", "t"));
        let (sql, params) = build_update_sql_for(DatabaseType::Postgres, &request);
        assert_eq!(
            sql,
            "UPDATE \"public\".\"t\" SET \"a\" = $1, \"b\" = DEFAULT, \"c\" = CURRENT_TIMESTAMP WHERE \"id\" = $2 AND \"a\" = $3"
        );
        assert_eq!(
            params,
            vec![
                SqlParam::String("x".into()),
                SqlParam::I64(1),
                SqlParam::String("old".into()),
            ]
        );
    }

    #[test]
    fn update_renders_default_and_server_time_mysql() {
        let request = update_with_expressions(target("app", "t"));
        let (sql, _) = build_update_sql_for(DatabaseType::MySql, &request);
        assert_eq!(
            sql,
            "UPDATE `app`.`t` SET `a` = ?, `b` = DEFAULT, `c` = NOW() WHERE `id` = ? AND `a` = ?"
        );
    }

    #[test]
    fn update_renders_default_and_server_time_sqlite() {
        let request = update_with_expressions(target("main", "t"));
        let (sql, _) = build_update_sql_for(DatabaseType::Sqlite, &request);
        assert_eq!(
            sql,
            "UPDATE \"main\".\"t\" SET \"a\" = ?, \"b\" = DEFAULT, \"c\" = CURRENT_TIMESTAMP WHERE \"id\" = ? AND \"a\" = ?"
        );
    }

    #[test]
    fn related_open_sql_inlines_literals_per_engine() {
        let request = RelatedRowsRequest {
            table: target("public", "orders"),
            predicates: vec![
                ("code".into(), CellValue::String("O'Neil".into())),
                ("region".into(), CellValue::Null),
            ],
            limit: 50,
        };
        assert_eq!(
            build_related_open_sql_for(DatabaseType::Postgres, &request),
            "SELECT * FROM \"public\".\"orders\" WHERE \"code\" = 'O''Neil' AND \"region\" IS NULL;"
        );
    }

    fn fk_label_request(keys: Vec<CellValue>) -> FkLabelRequest {
        FkLabelRequest {
            referenced: target("public", "customer"),
            key_column: "id".into(),
            display_column: "name".into(),
            keys,
        }
    }

    #[test]
    fn fk_labels_sql_pg_binds_one_param_per_key() {
        let request = fk_label_request(vec![
            CellValue::I64(1),
            CellValue::I64(2),
            CellValue::String("x".into()),
        ]);
        let (sql, params) = build_fk_labels_sql_for(DatabaseType::Postgres, &request).expect("sql");
        assert_eq!(
            sql,
            "SELECT \"id\", CAST(\"name\" AS TEXT) FROM \"public\".\"customer\" \
             WHERE \"id\" IN ($1, $2, $3)"
        );
        assert_eq!(
            params,
            vec![
                SqlParam::I64(1),
                SqlParam::I64(2),
                SqlParam::String("x".into()),
            ]
        );
    }

    #[test]
    fn fk_labels_rejects_more_than_500_keys() {
        let keys = (0..501).map(CellValue::I64).collect();
        let request = fk_label_request(keys);
        assert!(build_fk_labels_sql_for(DatabaseType::Postgres, &request).is_err());
    }

    #[test]
    fn fk_labels_empty_keys_is_noop() {
        let request = fk_label_request(Vec::new());
        let (sql, params) = build_fk_labels_sql_for(DatabaseType::Postgres, &request).expect("sql");
        assert!(sql.is_empty());
        assert!(params.is_empty());
    }
}
