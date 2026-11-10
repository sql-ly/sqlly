//! Oracle Database spellings of the row-editing statements, plus the
//! statement analysis that stands in for column provenance there. Pure and
//! unit tested without a server; the native lane in `row_editing_oracle`
//! drives these with live catalog types, and the staged-batch builders use
//! them typeless (the lane's session pins the NLS formats so a plain string
//! bind still converts to DATE / TIMESTAMP).
//!
//! Oracle's client library describes a result set's columns but never their
//! source table, so provenance is a two-step check: the statement is parsed
//! with the Oracle grammar and accepted only when it is ONE `SELECT` over ONE
//! table (no joins, derived tables, CTEs or set operations), then the server
//! describes its projection and each projected column maps back to a column
//! of that table by name — aliases and expressions stay read-only. Rows are
//! addressed by the enabled primary key, or by `ROWID` when the table has
//! none: the pseudo-column is surfaced as the key so the grid can ask for it
//! (`SELECT ROWID, t.* FROM t`).
//!
//! Binds are positional (`:1`, `:2`, …). Temporal and interval columns get
//! explicit `TO_DATE` / `TO_TIMESTAMP` / `TO_TIMESTAMP_TZ` / `TO_DSINTERVAL`
//! conversions in the formats the driver renders them in, `RAW` and `ROWID`
//! get `HEXTORAW` / `CHARTOROWID`, LOB columns compare through
//! `DBMS_LOB.COMPARE`, and `UPDATE … SET col = DEFAULT` is native syntax.

use sqlly_core::{CellValue, DatabaseType, SqlParam};
use sqlly_lang::sqlmodel::Dialect;
use sqlly_lang::sqlparse::{StatementNode, parse};

use crate::row_editing::{
    DeleteRowRequest, InsertRowRequest, RowEditColumn, TableTarget, UpdateRowRequest,
};
use crate::row_editing_sql as dialect;

/// The pseudo-column that identifies a row when a table has no primary key.
pub const ROWID_COLUMN: &str = "ROWID";

/// Name of the OUT bind an insert returns the new row's `ROWID` through.
pub const ROWID_OUT_BIND: &str = "sqlly_rowid";

/// Format masks matching how the driver renders DATE / TIMESTAMP text.
pub const DATE_FORMAT: &str = "YYYY-MM-DD HH24:MI:SS";
pub const TIMESTAMP_FORMAT: &str = "YYYY-MM-DD HH24:MI:SS.FF";
pub const TIMESTAMP_TZ_FORMAT: &str = "YYYY-MM-DD HH24:MI:SS.FF TZH:TZM";

/// Session setup for the row-editing lane and the preview script: pin the
/// NLS formats to the driver's own text renderings so a typeless string
/// bind or literal converts to DATE / TIMESTAMP without `TO_DATE`.
pub fn session_setup_sql() -> String {
    format!(
        "ALTER SESSION SET NLS_DATE_FORMAT = '{DATE_FORMAT}' NLS_TIMESTAMP_FORMAT = '{TIMESTAMP_FORMAT}' NLS_TIMESTAMP_TZ_FORMAT = '{TIMESTAMP_TZ_FORMAT}'"
    )
}

/// One column of the target table as the catalog describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OracleTableColumn {
    pub name: String,
    /// `ALL_TAB_COLS.DATA_TYPE`: `NUMBER`, `VARCHAR2`, `DATE`, `TIMESTAMP(6)`,
    /// `TIMESTAMP(6) WITH TIME ZONE`, `RAW`, `CLOB`, …
    pub data_type: String,
    pub char_len: Option<i64>,
    pub precision: Option<i64>,
    pub scale: Option<i64>,
    pub nullable: bool,
    pub is_identity: bool,
    pub is_virtual: bool,
    pub default_definition: Option<String>,
}

/// Oracle identifier quoting, except for the `ROWID` pseudo-column, which
/// must stay bare (`"ROWID"` would name an ordinary column).
pub fn ident(db: DatabaseType, name: &str) -> String {
    if name.eq_ignore_ascii_case(ROWID_COLUMN) {
        ROWID_COLUMN.to_string()
    } else {
        db.quote_identifier(name)
    }
}

fn placeholder(n: usize) -> String {
    format!(":{n}")
}

/// Upper-cased base type without length/precision (`TIMESTAMP(6) WITH TIME
/// ZONE` → `TIMESTAMP WITH TIME ZONE`).
pub fn base_type(data_type: &str) -> String {
    let upper = data_type.to_ascii_uppercase();
    let mut out = String::with_capacity(upper.len());
    let mut depth = 0;
    for ch in upper.chars() {
        match ch {
            '(' => depth += 1,
            ')' => depth -= 1,
            _ if depth == 0 => out.push(ch),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn is_lob(base: &str) -> bool {
    matches!(base, "CLOB" | "NCLOB" | "BLOB")
}

fn is_numeric(base: &str) -> bool {
    matches!(
        base,
        "NUMBER" | "FLOAT" | "BINARY_FLOAT" | "BINARY_DOUBLE" | "INTEGER" | "DECIMAL"
    )
}

/// The bind expression for parameter `n` carrying `value` into `column`:
/// explicit conversions where Oracle's implicit ones would need NLS luck.
pub fn param_expr(n: usize, value: &CellValue, column: Option<&OracleTableColumn>) -> String {
    let ph = placeholder(n);
    let Some(column) = column else {
        return ph;
    };
    let base = base_type(&column.data_type);
    match value {
        CellValue::Null => "NULL".to_string(),
        CellValue::String(_) => match base.as_str() {
            "DATE" => format!("TO_DATE({ph}, '{DATE_FORMAT}')"),
            "TIMESTAMP" | "TIMESTAMP WITH LOCAL TIME ZONE" => {
                format!("TO_TIMESTAMP({ph}, '{TIMESTAMP_FORMAT}')")
            }
            "TIMESTAMP WITH TIME ZONE" => {
                format!("TO_TIMESTAMP_TZ({ph}, '{TIMESTAMP_TZ_FORMAT}')")
            }
            "INTERVAL DAY TO SECOND" => format!("TO_DSINTERVAL({ph})"),
            "INTERVAL YEAR TO MONTH" => format!("TO_YMINTERVAL({ph})"),
            "RAW" | "LONG RAW" | "BLOB" => format!("HEXTORAW({ph})"),
            "ROWID" | "UROWID" => format!("CHARTOROWID({ph})"),
            "CLOB" => format!("TO_CLOB({ph})"),
            "NCLOB" => format!("TO_NCLOB({ph})"),
            b if is_numeric(b) => format!("TO_NUMBER({ph})"),
            _ => ph,
        },
        CellValue::Bytes(_) => match base.as_str() {
            "BLOB" => format!("TO_BLOB({ph})"),
            _ => ph,
        },
        _ => ph,
    }
}

/// The bind value for a cell going into `column`: booleans become `1`/`0`
/// for NUMBER columns (Oracle's pre-23ai boolean convention).
fn bind_value(value: &CellValue, column: Option<&OracleTableColumn>) -> SqlParam {
    match (value, column) {
        (CellValue::Bool(b), Some(column)) if is_numeric(&base_type(&column.data_type)) => {
            SqlParam::I64(i64::from(*b))
        }
        (value, _) => SqlParam::from(value),
    }
}

/// `col = <expr>` / `col IS NULL`, with LOB columns compared through
/// `DBMS_LOB.COMPARE`; LONG / XML columns cannot be compared in SQL and are
/// skipped (the key still identifies the row).
pub fn push_predicate(
    db: DatabaseType,
    predicates: &mut Vec<String>,
    params: &mut Vec<SqlParam>,
    column: &str,
    value: &CellValue,
    types: &[OracleTableColumn],
) {
    let info = types.iter().find(|c| c.name == column);
    let base = info.map(|c| base_type(&c.data_type)).unwrap_or_default();
    if matches!(
        base.as_str(),
        "LONG" | "LONG RAW" | "XMLTYPE" | "SDO_GEOMETRY"
    ) {
        return;
    }
    if matches!(value, CellValue::Null) {
        predicates.push(format!("{} IS NULL", ident(db, column)));
        return;
    }
    params.push(bind_value(value, info));
    let expr = param_expr(params.len(), value, info);
    if is_lob(&base) {
        predicates.push(format!(
            "DBMS_LOB.COMPARE({}, {expr}) = 0",
            ident(db, column)
        ));
    } else {
        predicates.push(format!("{} = {expr}", ident(db, column)));
    }
}

fn where_clause(
    db: DatabaseType,
    params: &mut Vec<SqlParam>,
    key: &[(String, CellValue)],
    rowversion: &Option<(String, CellValue)>,
    original_values: &[(String, CellValue)],
    types: &[OracleTableColumn],
) -> String {
    let mut predicates: Vec<String> = Vec::new();
    for (column, value) in key {
        push_predicate(db, &mut predicates, params, column, value, types);
    }
    match rowversion {
        Some((column, value)) => push_predicate(db, &mut predicates, params, column, value, types),
        None => {
            for (column, value) in original_values {
                push_predicate(db, &mut predicates, params, column, value, types);
            }
        }
    }
    predicates.join(" AND ")
}

/// Guarded UPDATE: key plus the original values of the changed columns.
pub fn build_update_sql(
    db: DatabaseType,
    request: &UpdateRowRequest,
    types: &[OracleTableColumn],
) -> (String, Vec<SqlParam>) {
    let mut params: Vec<SqlParam> = Vec::new();
    let mut assignments: Vec<String> = Vec::new();
    for (column, value) in &request.changes {
        if matches!(value, CellValue::Null) {
            assignments.push(format!("{} = NULL", ident(db, column)));
        } else {
            let info = types.iter().find(|c| &c.name == column);
            params.push(bind_value(value, info));
            assignments.push(format!(
                "{} = {}",
                ident(db, column),
                param_expr(params.len(), value, info)
            ));
        }
    }
    for (column, expr) in &request.expressions {
        assignments.push(format!(
            "{} = {}",
            ident(db, column),
            dialect::expression_sql(db, *expr)
        ));
    }
    let predicates = where_clause(
        db,
        &mut params,
        &request.key,
        &request.rowversion,
        &request.original_values,
        types,
    );
    (
        format!(
            "UPDATE {} SET {} WHERE {predicates}",
            dialect::quote_table(db, &request.target),
            assignments.join(", ")
        ),
        params,
    )
}

/// Guarded DELETE, same predicate shape as the UPDATE.
pub fn build_delete_sql(
    db: DatabaseType,
    request: &DeleteRowRequest,
    types: &[OracleTableColumn],
) -> (String, Vec<SqlParam>) {
    let mut params: Vec<SqlParam> = Vec::new();
    let predicates = where_clause(
        db,
        &mut params,
        &request.key,
        &request.rowversion,
        &request.original_values,
        types,
    );
    (
        format!(
            "DELETE FROM {} WHERE {predicates}",
            dialect::quote_table(db, &request.target)
        ),
        params,
    )
}

/// Single-row fetch by key. `ROWID` rides along as a projected column so a
/// table without a primary key still gets its key back.
pub fn build_select_row_sql(
    db: DatabaseType,
    target: &TableTarget,
    key: &[(String, CellValue)],
    types: &[OracleTableColumn],
) -> (String, Vec<SqlParam>) {
    let mut params: Vec<SqlParam> = Vec::new();
    let mut predicates: Vec<String> = Vec::new();
    for (column, value) in key {
        push_predicate(db, &mut predicates, &mut params, column, value, types);
    }
    (
        format!(
            "SELECT t.ROWID AS \"{ROWID_COLUMN}\", t.* FROM {} t WHERE {}",
            dialect::quote_table(db, target),
            predicates.join(" AND ")
        ),
        params,
    )
}

/// Parameterized INSERT. Oracle has no `DEFAULT VALUES`; an insert with
/// nothing supplied writes `DEFAULT` into the first catalog column instead.
/// With `returning_rowid`, the new row's `ROWID` comes back through the
/// [`ROWID_OUT_BIND`] OUT bind (the lane refetches the row with it).
pub fn build_insert_sql(
    db: DatabaseType,
    request: &InsertRowRequest,
    types: &[OracleTableColumn],
    returning_rowid: bool,
) -> (String, Vec<SqlParam>) {
    let mut params: Vec<SqlParam> = Vec::new();
    let table = dialect::quote_table(db, &request.target);
    let mut columns: Vec<String> = Vec::new();
    let mut values: Vec<String> = Vec::new();
    for (column, value) in &request.values {
        columns.push(ident(db, column));
        if matches!(value, CellValue::Null) {
            values.push("NULL".to_string());
        } else {
            let info = types.iter().find(|c| &c.name == column);
            params.push(bind_value(value, info));
            values.push(param_expr(params.len(), value, info));
        }
    }
    for (column, expr) in &request.expressions {
        columns.push(ident(db, column));
        values.push(dialect::expression_sql(db, *expr).to_string());
    }
    if columns.is_empty()
        && let Some(first) = types.first()
    {
        columns.push(ident(db, &first.name));
        values.push("DEFAULT".to_string());
    }
    let returning = if returning_rowid {
        format!(" RETURNING ROWID INTO :{ROWID_OUT_BIND}")
    } else {
        String::new()
    };
    (
        format!(
            "INSERT INTO {table} ({}) VALUES ({}){returning}",
            columns.join(", "),
            values.join(", ")
        ),
        params,
    )
}

/// Wraps the statement so the server describes its projection without
/// producing rows.
pub fn describe_wrapper_sql(sql: &str) -> String {
    let inner = sql.trim().trim_end_matches(';').trim_end();
    format!("SELECT * FROM ({inner}) sqlly_describe WHERE 1 = 0")
}

/// The statement's single source table, from the Oracle grammar: exactly one
/// `SELECT` over exactly one table, with no derived tables or CTEs. Returns
/// `(schema, table)` as Oracle folds them (unquoted names upper-cased);
/// `schema` is `None` when the statement did not qualify the table.
pub fn single_source_table(
    sql: &str,
) -> Result<(Option<String>, String), sqlly_core::DisplayMessage> {
    let tree = parse(sql, Dialect::Oracle);
    if tree.statements.len() != 1 {
        return Err(crate::row_editing::not_editable::batch_read_only());
    }
    if !tree.cte_definitions.is_empty() {
        return Err(crate::row_editing::not_editable::with_query_read_only());
    }
    let StatementNode::Select(query) = &tree.statements[0] else {
        return Err(crate::row_editing::not_editable::not_select());
    };
    if !query.derived_aliases.is_empty() {
        return Err(crate::row_editing::not_editable::derived_source());
    }
    match query.tables.as_slice() {
        [] => Err(crate::row_editing::not_editable::no_source_table()),
        [table] => {
            let name = table.reference.name.fold_key().to_string();
            if name.eq_ignore_ascii_case("DUAL") {
                return Err(crate::row_editing::not_editable::dual_has_no_rows());
            }
            let schema = table
                .reference
                .namespace
                .as_ref()
                .map(|n| n.fold_key().to_string());
            Ok((schema, name))
        }
        _ => Err(crate::row_editing::not_editable::joined_tables()),
    }
}

/// The catalog columns of a table as the row editor models them, plus the
/// key: the enabled primary key, or a synthetic read-only `ROWID` column
/// when there is none.
pub fn row_edit_columns(
    types: &[OracleTableColumn],
    key_columns: &[String],
) -> (Vec<RowEditColumn>, Vec<String>) {
    let mut columns: Vec<RowEditColumn> = types
        .iter()
        .map(|c| RowEditColumn {
            name: c.name.clone(),
            data_type: base_type(&c.data_type).to_ascii_lowercase(),
            char_max_length: c.char_len.filter(|n| *n > 0),
            precision: c.precision.map(|p| p as u8),
            scale: c.scale.map(|s| s as u8),
            nullable: c.nullable,
            is_primary_key: key_columns.contains(&c.name),
            is_identity: c.is_identity,
            is_computed: c.is_virtual,
            is_rowversion: false,
            default_definition: c.default_definition.clone(),
            result_ordinal: None,
            enum_values: Vec::new(),
        })
        .collect();
    if key_columns.is_empty() {
        columns.insert(
            0,
            RowEditColumn {
                name: ROWID_COLUMN.to_string(),
                data_type: "rowid".to_string(),
                char_max_length: None,
                precision: None,
                scale: None,
                nullable: false,
                is_primary_key: true,
                is_identity: false,
                is_computed: true,
                is_rowversion: false,
                default_definition: None,
                result_ordinal: None,
                enum_values: Vec::new(),
            },
        );
        return (columns, vec![ROWID_COLUMN.to_string()]);
    }
    (columns, key_columns.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::row_editing::ColumnExpression;

    fn column(name: &str, data_type: &str) -> OracleTableColumn {
        OracleTableColumn {
            name: name.into(),
            data_type: data_type.into(),
            char_len: None,
            precision: None,
            scale: None,
            nullable: true,
            is_identity: false,
            is_virtual: false,
            default_definition: None,
        }
    }

    fn target() -> TableTarget {
        TableTarget {
            schema: "HR".into(),
            table: "EMP".into(),
        }
    }

    #[test]
    fn single_source_table_accepts_one_plain_select_and_folds_names() {
        assert_eq!(
            single_source_table("SELECT id, name FROM hr.emp WHERE id > 1 ORDER BY id"),
            Ok((Some("HR".into()), "EMP".into()))
        );
        assert_eq!(
            single_source_table("select * from \"Mixed\""),
            Ok((None, "Mixed".into()))
        );
        assert_eq!(
            single_source_table("SELECT ROWID, e.* FROM emp e FETCH FIRST 10 ROWS ONLY;"),
            Ok((None, "EMP".into()))
        );
    }

    #[test]
    fn single_source_table_rejects_joins_batches_subqueries_and_dml() {
        assert!(
            single_source_table("SELECT * FROM emp e JOIN dept d ON d.id = e.dept_id")
                .unwrap_err()
                .contains("more than one table")
        );
        assert!(
            single_source_table("SELECT 1 FROM dual; SELECT 2 FROM dual")
                .unwrap_err()
                .contains("single SELECT")
        );
        assert!(
            single_source_table("SELECT * FROM (SELECT * FROM emp) x")
                .unwrap_err()
                .contains("derived")
        );
        // A pivoted or applied row is not a row of the table underneath.
        for sql in [
            "SELECT * FROM emp PIVOT (SUM(sal) FOR deptno IN (10 AS d10, 20 AS d20))",
            "SELECT * FROM emp UNPIVOT INCLUDE NULLS (val FOR col IN (sal, comm))",
            "SELECT * FROM emp e CROSS APPLY (SELECT 1 AS x FROM dual) a",
        ] {
            assert!(
                single_source_table(sql).unwrap_err().contains("derived"),
                "{sql}"
            );
        }
        assert!(
            single_source_table("WITH c AS (SELECT 1 AS x FROM dual) SELECT * FROM c")
                .unwrap_err()
                .contains("WITH")
        );
        assert!(
            single_source_table("UPDATE emp SET x = 1")
                .unwrap_err()
                .contains("SELECT")
        );
        assert!(single_source_table("SELECT 1 FROM dual").is_err());
    }

    #[test]
    fn describe_wrapper_runs_nothing() {
        assert_eq!(
            describe_wrapper_sql("SELECT * FROM emp;"),
            "SELECT * FROM (SELECT * FROM emp) sqlly_describe WHERE 1 = 0"
        );
    }

    #[test]
    fn param_exprs_convert_temporal_raw_rowid_and_lob_binds() {
        let s = CellValue::String("x".into());
        assert_eq!(
            param_expr(1, &s, Some(&column("d", "DATE"))),
            "TO_DATE(:1, 'YYYY-MM-DD HH24:MI:SS')"
        );
        assert_eq!(
            param_expr(2, &s, Some(&column("t", "TIMESTAMP(6)"))),
            "TO_TIMESTAMP(:2, 'YYYY-MM-DD HH24:MI:SS.FF')"
        );
        assert_eq!(
            param_expr(3, &s, Some(&column("t", "TIMESTAMP(6) WITH TIME ZONE"))),
            "TO_TIMESTAMP_TZ(:3, 'YYYY-MM-DD HH24:MI:SS.FF TZH:TZM')"
        );
        assert_eq!(
            param_expr(
                4,
                &s,
                Some(&column("t", "TIMESTAMP(9) WITH LOCAL TIME ZONE"))
            ),
            "TO_TIMESTAMP(:4, 'YYYY-MM-DD HH24:MI:SS.FF')"
        );
        assert_eq!(
            param_expr(5, &s, Some(&column("i", "INTERVAL DAY(2) TO SECOND(6)"))),
            "TO_DSINTERVAL(:5)"
        );
        assert_eq!(param_expr(6, &s, Some(&column("r", "RAW"))), "HEXTORAW(:6)");
        assert_eq!(
            param_expr(7, &s, Some(&column("r", "ROWID"))),
            "CHARTOROWID(:7)"
        );
        assert_eq!(param_expr(8, &s, Some(&column("c", "CLOB"))), "TO_CLOB(:8)");
        assert_eq!(
            param_expr(9, &s, Some(&column("n", "NUMBER"))),
            "TO_NUMBER(:9)"
        );
        assert_eq!(param_expr(10, &s, Some(&column("v", "VARCHAR2"))), ":10");
        assert_eq!(param_expr(11, &s, None), ":11");
        assert_eq!(
            param_expr(12, &CellValue::Bytes(vec![1]), Some(&column("b", "BLOB"))),
            "TO_BLOB(:12)"
        );
        assert_eq!(
            param_expr(13, &CellValue::I64(1), Some(&column("d", "DATE"))),
            ":13"
        );
    }

    #[test]
    fn booleans_bind_as_numbers_for_number_columns() {
        assert_eq!(
            bind_value(&CellValue::Bool(true), Some(&column("f", "NUMBER"))),
            SqlParam::I64(1)
        );
        assert_eq!(
            bind_value(&CellValue::Bool(false), Some(&column("f", "BOOLEAN"))),
            SqlParam::Bool(false)
        );
    }

    #[test]
    fn update_is_guarded_by_key_and_original_values_with_typed_binds() {
        let types = vec![
            column("ID", "NUMBER"),
            column("NAME", "VARCHAR2"),
            column("HIRED", "DATE"),
            column("NOTE", "CLOB"),
        ];
        let request = UpdateRowRequest {
            target: target(),
            key: vec![("ID".into(), CellValue::I64(7))],
            rowversion: None,
            changes: vec![
                ("NAME".into(), CellValue::String("Ada".into())),
                (
                    "HIRED".into(),
                    CellValue::String("2024-01-02 03:04:05".into()),
                ),
                ("NOTE".into(), CellValue::Null),
            ],
            original_values: vec![
                ("NAME".into(), CellValue::String("Old".into())),
                ("HIRED".into(), CellValue::Null),
                ("NOTE".into(), CellValue::String("text".into())),
            ],
            expressions: vec![("UPDATED_AT".into(), ColumnExpression::ServerTime)],
        };
        let (sql, params) = build_update_sql(DatabaseType::Oracle, &request, &types);
        assert_eq!(
            sql,
            "UPDATE \"HR\".\"EMP\" SET \"NAME\" = :1, \"HIRED\" = TO_DATE(:2, 'YYYY-MM-DD HH24:MI:SS'), \"NOTE\" = NULL, \"UPDATED_AT\" = SYSTIMESTAMP WHERE \"ID\" = :3 AND \"NAME\" = :4 AND \"HIRED\" IS NULL AND DBMS_LOB.COMPARE(\"NOTE\", TO_CLOB(:5)) = 0"
        );
        assert_eq!(params.len(), 5);
        assert_eq!(params[2], SqlParam::I64(7));
    }

    #[test]
    fn default_expression_and_rowid_key_render_natively() {
        let request = UpdateRowRequest {
            target: target(),
            key: vec![("ROWID".into(), CellValue::String("AAAR".into()))],
            rowversion: None,
            changes: vec![],
            original_values: vec![],
            expressions: vec![("NAME".into(), ColumnExpression::Default)],
        };
        let (sql, params) = build_update_sql(DatabaseType::Oracle, &request, &[]);
        assert_eq!(
            sql,
            "UPDATE \"HR\".\"EMP\" SET \"NAME\" = DEFAULT WHERE ROWID = :1"
        );
        assert_eq!(params, vec![SqlParam::String("AAAR".into())]);
        let types = vec![column("ROWID", "ROWID")];
        let (sql, _) = build_update_sql(DatabaseType::Oracle, &request, &types);
        assert!(sql.ends_with("WHERE ROWID = CHARTOROWID(:1)"), "{sql}");
    }

    #[test]
    fn delete_and_select_row_share_the_predicate_shape() {
        let request = DeleteRowRequest {
            target: target(),
            key: vec![("ID".into(), CellValue::I64(1))],
            rowversion: None,
            original_values: vec![("NAME".into(), CellValue::String("x".into()))],
        };
        let (sql, params) = build_delete_sql(DatabaseType::Oracle, &request, &[]);
        assert_eq!(
            sql,
            "DELETE FROM \"HR\".\"EMP\" WHERE \"ID\" = :1 AND \"NAME\" = :2"
        );
        assert_eq!(params.len(), 2);
        let (sql, _) = build_select_row_sql(
            DatabaseType::Oracle,
            &target(),
            &[("ID".into(), CellValue::I64(1))],
            &[],
        );
        assert_eq!(
            sql,
            "SELECT t.ROWID AS \"ROWID\", t.* FROM \"HR\".\"EMP\" t WHERE \"ID\" = :1"
        );
    }

    #[test]
    fn insert_returns_rowid_and_has_no_default_values_form() {
        let types = vec![column("ID", "NUMBER"), column("NAME", "VARCHAR2")];
        let request = InsertRowRequest {
            target: target(),
            values: vec![
                ("NAME".into(), CellValue::String("Ada".into())),
                ("ID".into(), CellValue::Null),
            ],
            expressions: vec![("CREATED".into(), ColumnExpression::ServerTime)],
        };
        let (sql, params) = build_insert_sql(DatabaseType::Oracle, &request, &types, true);
        assert_eq!(
            sql,
            "INSERT INTO \"HR\".\"EMP\" (\"NAME\", \"ID\", \"CREATED\") VALUES (:1, NULL, SYSTIMESTAMP) RETURNING ROWID INTO :sqlly_rowid"
        );
        assert_eq!(params.len(), 1);
        let (plain, _) = build_insert_sql(DatabaseType::Oracle, &request, &[], false);
        assert_eq!(
            plain,
            "INSERT INTO \"HR\".\"EMP\" (\"NAME\", \"ID\", \"CREATED\") VALUES (:1, NULL, SYSTIMESTAMP)"
        );
        let empty = InsertRowRequest {
            target: target(),
            values: vec![],
            expressions: vec![],
        };
        let (sql, params) = build_insert_sql(DatabaseType::Oracle, &empty, &types, true);
        assert_eq!(
            sql,
            "INSERT INTO \"HR\".\"EMP\" (\"ID\") VALUES (DEFAULT) RETURNING ROWID INTO :sqlly_rowid"
        );
        assert!(params.is_empty());
    }

    #[test]
    fn rowid_becomes_the_key_when_a_table_has_no_primary_key() {
        let types = vec![column("A", "NUMBER"), column("B", "VARCHAR2")];
        let (columns, key) = row_edit_columns(&types, &[]);
        assert_eq!(key, vec!["ROWID"]);
        assert_eq!(columns[0].name, "ROWID");
        assert!(columns[0].is_primary_key && columns[0].is_computed);
        assert_eq!(columns.len(), 3);
        let (columns, key) = row_edit_columns(&types, &["A".to_string()]);
        assert_eq!(key, vec!["A"]);
        assert!(columns[0].is_primary_key);
        assert_eq!(columns.len(), 2);
        assert_eq!(columns[1].data_type, "varchar2");
    }

    #[test]
    fn session_setup_pins_the_drivers_text_formats() {
        let sql = session_setup_sql();
        assert!(sql.starts_with("ALTER SESSION SET NLS_DATE_FORMAT = 'YYYY-MM-DD HH24:MI:SS'"));
        assert!(sql.contains("NLS_TIMESTAMP_TZ_FORMAT = 'YYYY-MM-DD HH24:MI:SS.FF TZH:TZM'"));
    }
}
