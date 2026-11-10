//! Pure SQL planning for cross-connection data transfer — no I/O.

use crate::scripting::{ScriptObject, topological_order};
use sqlly_lang::sqlmodel::{
    ConnectionKind, DatabaseType, MappingNote, SqlOperation, UpsertPattern, map_column_type,
};

pub const BATCH_ROWS: usize = 1000;
pub const TDS_MAX_PARAMS: usize = 2100;
pub const PG_MAX_PARAMS: usize = 65_535;
pub const OTHER_MAX_PARAMS: usize = 30_000;
/// Oracle batches are `INSERT … SELECT … FROM DUAL UNION ALL …` statements
/// whose parse time grows with the number of branches; a small bind budget
/// keeps each one quick.
pub const ORACLE_MAX_PARAMS: usize = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferMode {
    CreateTable,
    Append,
    TruncateThenInsert,
    UpsertOnPrimaryKey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameCasing {
    AsIs,
    Lower,
    Upper,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferColumn {
    pub name: String,
    pub source_type: String,
    pub nullable: bool,
    pub is_identity: bool,
    pub pk_ordinal: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferTable {
    pub schema: Option<String>,
    pub name: String,
    pub columns: Vec<TransferColumn>,
    pub depends_on: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct TransferPlan {
    pub source: DatabaseType,
    pub target: DatabaseType,
    pub target_schema: Option<String>,
    pub mode: TransferMode,
    pub casing: NameCasing,
    pub tables: Vec<TransferTable>,
}

#[derive(Debug, Clone)]
pub struct TransferStatements {
    pub pre: Vec<String>,
    pub insert_sql: String,
    pub rows_per_statement: usize,
    pub post: Vec<String>,
    pub warnings: Vec<sqlly_core::DisplayMessage>,
}

pub fn apply_casing(name: &str, casing: NameCasing) -> String {
    match casing {
        NameCasing::AsIs => name.to_string(),
        NameCasing::Lower => name.to_ascii_lowercase(),
        NameCasing::Upper => name.to_ascii_uppercase(),
    }
}

fn table_key(table: &TransferTable) -> String {
    let schema = table.schema.as_deref().unwrap_or("").to_ascii_lowercase();
    let name = table.name.to_ascii_lowercase();
    if schema.is_empty() {
        name
    } else {
        format!("{schema}.{name}")
    }
}

pub fn ordered_tables(
    plan: &TransferPlan,
) -> (Vec<&TransferTable>, Vec<sqlly_core::DisplayMessage>) {
    let objects: Vec<ScriptObject> = plan
        .tables
        .iter()
        .map(|table| {
            ScriptObject::new(
                table_key(table),
                table
                    .depends_on
                    .iter()
                    .map(|k| k.to_ascii_lowercase())
                    .collect(),
            )
        })
        .collect();
    let order = topological_order(&objects);
    let mut ordered = Vec::new();
    for key in &order.order {
        if let Some(table) = plan.tables.iter().find(|t| table_key(t) == *key) {
            ordered.push(table);
        }
    }
    (ordered, order.warnings)
}

fn max_params(target: DatabaseType) -> usize {
    match target.connection_kind() {
        ConnectionKind::Tds => TDS_MAX_PARAMS,
        ConnectionKind::PostgresWire => PG_MAX_PARAMS,
        ConnectionKind::OracleNet => ORACLE_MAX_PARAMS,
        _ => OTHER_MAX_PARAMS,
    }
}

pub fn rows_per_statement(target: DatabaseType, column_count: usize) -> usize {
    let cols = column_count.max(1);
    let per_row_budget = max_params(target).saturating_sub(100) / cols;
    BATCH_ROWS.min(per_row_budget).max(1)
}

pub fn placeholder(target: DatabaseType, n: usize) -> String {
    match target.connection_kind() {
        ConnectionKind::Tds => format!("@P{n}"),
        ConnectionKind::PostgresWire => format!("${n}"),
        ConnectionKind::OracleNet => format!(":{n}"),
        _ => "?".to_string(),
    }
}

pub fn qualified_target(plan: &TransferPlan, table: &TransferTable) -> String {
    let schema = plan
        .target_schema
        .as_deref()
        .or(table.schema.as_deref())
        .unwrap_or("");
    let name = apply_casing(&table.name, plan.casing);
    let table_q = plan.target.quote_identifier(&name);
    if schema.trim().is_empty()
        || matches!(plan.target, DatabaseType::Sqlite | DatabaseType::LibSql)
    {
        table_q
    } else {
        format!(
            "{}.{}",
            plan.target
                .quote_identifier(&apply_casing(schema, plan.casing)),
            table_q
        )
    }
}

fn qualified_source(plan: &TransferPlan, table: &TransferTable) -> String {
    let schema = table.schema.as_deref().unwrap_or("");
    let table_q = plan.source.quote_identifier(&table.name);
    if schema.trim().is_empty()
        || matches!(plan.source, DatabaseType::Sqlite | DatabaseType::LibSql)
    {
        table_q
    } else {
        format!("{}.{}", plan.source.quote_identifier(schema), table_q)
    }
}

pub fn select_sql(plan: &TransferPlan, table: &TransferTable) -> String {
    let cols: Vec<String> = table
        .columns
        .iter()
        .map(|c| plan.source.quote_identifier(&c.name))
        .collect();
    format!(
        "SELECT {} FROM {}",
        cols.join(", "),
        qualified_source(plan, table)
    )
}

fn pk_columns(table: &TransferTable) -> Vec<&TransferColumn> {
    let mut pks: Vec<_> = table
        .columns
        .iter()
        .filter(|c| c.pk_ordinal.is_some())
        .collect();
    pks.sort_by_key(|c| c.pk_ordinal);
    pks
}

fn mapping_warning(
    table: &str,
    column: &str,
    source_type: &str,
    target_ddl: &str,
    note: &MappingNote,
) -> sqlly_core::DisplayMessage {
    let (key, detail) = match note {
        MappingNote::Widened => ("services.transfer.mapping.widened", "widened".to_string()),
        MappingNote::Narrowed => ("services.transfer.mapping.narrowed", "narrowed".to_string()),
        MappingNote::Encoded(storage) => (
            "services.transfer.mapping.encoded",
            format!("encoded as {storage:?}"),
        ),
        MappingNote::Unknown => (
            "services.transfer.mapping.unknown",
            "unknown type — stored as text".to_string(),
        ),
    };
    let message = sqlly_core::DisplayMessage::new(
        key,
        format!("{table}.{column}: {source_type} → {target_ddl} ({detail})"),
    )
    .with_arg("table_name", table)
    .with_arg("column_name", column)
    .with_arg("source_type", source_type)
    .with_arg("target_type", target_ddl);
    match note {
        MappingNote::Encoded(storage) => message.with_arg("storage", format!("{storage:?}")),
        _ => message,
    }
}

pub fn create_table_sql(
    plan: &TransferPlan,
    table: &TransferTable,
) -> (String, Vec<sqlly_core::DisplayMessage>) {
    let mut warnings = Vec::new();
    let table_name = table.name.clone();
    let mut lines = Vec::new();
    // ClickHouse columns are non-nullable unless wrapped in Nullable(…), a
    // MergeTree table needs an ORDER BY (its primary key), and key columns
    // cannot be Nullable.
    let clickhouse = matches!(
        plan.target.connection_kind(),
        ConnectionKind::ClickHouseHttp
    );
    for column in &table.columns {
        let mapped = map_column_type(&column.source_type, plan.source, plan.target);
        if let Some(note) = &mapped.note {
            warnings.push(mapping_warning(
                &table_name,
                &column.name,
                &column.source_type,
                &mapped.target_ddl,
                note,
            ));
        }
        let col_name = apply_casing(&column.name, plan.casing);
        if clickhouse {
            let ddl = if column.nullable && column.pk_ordinal.is_none() {
                format!("Nullable({})", mapped.target_ddl)
            } else {
                mapped.target_ddl.clone()
            };
            lines.push(format!(
                "  {} {ddl}",
                plan.target.quote_identifier(&col_name)
            ));
            continue;
        }
        let null_sql = if column.nullable { "" } else { " NOT NULL" };
        lines.push(format!(
            "  {} {}{}",
            plan.target.quote_identifier(&col_name),
            mapped.target_ddl,
            null_sql
        ));
    }
    if table.columns.iter().any(|c| c.is_identity) {
        warnings.push(sqlly_core::DisplayMessage::new(
            "services.transfer.warnings.identity_not_recreated",
            "identity/auto-increment is not recreated on the target",
        ));
    }
    let pks = pk_columns(table);
    let pk_list: Vec<String> = pks
        .iter()
        .map(|c| {
            plan.target
                .quote_identifier(&apply_casing(&c.name, plan.casing))
        })
        .collect();
    if clickhouse {
        let order_by = match pk_list.as_slice() {
            [] => {
                warnings.push(
                    sqlly_core::DisplayMessage::new(
                        "services.transfer.warnings.clickhouse_no_primary_key",
                        format!("{table_name}: no primary key — created as MergeTree ORDER BY tuple() (no sort key)"),
                    )
                    .with_arg("table_name", table_name.clone()),
                );
                "tuple()".to_string()
            }
            [single] => single.clone(),
            many => format!("({})", many.join(", ")),
        };
        let ddl = format!(
            "CREATE TABLE {} (\n{}\n) ENGINE = MergeTree ORDER BY {order_by}",
            qualified_target(plan, table),
            lines.join(",\n")
        );
        return (ddl, warnings);
    }
    if !pk_list.is_empty() {
        lines.push(format!("  PRIMARY KEY ({})", pk_list.join(", ")));
    }
    let ddl = format!(
        "CREATE TABLE {} (\n{}\n)",
        qualified_target(plan, table),
        lines.join(",\n")
    );
    (ddl, warnings)
}

fn value_placeholders(target: DatabaseType, rows: usize, columns: usize) -> String {
    let mut parts = Vec::with_capacity(rows);
    let mut n = 1usize;
    for _ in 0..rows {
        let row: Vec<String> = (0..columns)
            .map(|_| {
                let ph = placeholder(target, n);
                n += 1;
                ph
            })
            .collect();
        parts.push(format!("({})", row.join(",")));
    }
    parts.join(",")
}

fn quoted_columns(plan: &TransferPlan, table: &TransferTable) -> Vec<String> {
    table
        .columns
        .iter()
        .map(|c| {
            plan.target
                .quote_identifier(&apply_casing(&c.name, plan.casing))
        })
        .collect()
}

pub fn insert_sql(plan: &TransferPlan, table: &TransferTable, rows: usize) -> String {
    let qualified = qualified_target(plan, table);
    let cols = quoted_columns(plan, table);
    let col_count = cols.len().max(1);
    let values = value_placeholders(plan.target, rows.max(1), col_count);
    let col_list = cols.join(", ");

    if plan.target.connection_kind() == ConnectionKind::OracleNet {
        return oracle_insert_sql(plan, table, &qualified, &cols, rows.max(1));
    }

    if plan.mode != TransferMode::UpsertOnPrimaryKey {
        return format!("INSERT INTO {qualified} ({col_list}) VALUES {values}");
    }

    let pks = pk_columns(table);
    let pk_names: Vec<String> = pks
        .iter()
        .map(|c| apply_casing(&c.name, plan.casing))
        .collect();
    let quoted_pks: Vec<String> = pk_names
        .iter()
        .map(|k| plan.target.quote_identifier(k))
        .collect();
    let non_pk: Vec<String> = table
        .columns
        .iter()
        .filter(|c| c.pk_ordinal.is_none())
        .map(|c| {
            plan.target
                .quote_identifier(&apply_casing(&c.name, plan.casing))
        })
        .collect();

    match plan.target.upsert_pattern() {
        UpsertPattern::Merge => merge_insert_sql(
            plan.target,
            &qualified,
            &cols,
            &quoted_pks,
            &non_pk,
            rows.max(1),
            col_count,
        ),
        UpsertPattern::OnConflict => {
            let updates: Vec<String> = non_pk
                .iter()
                .map(|c| format!("{c} = EXCLUDED.{c}"))
                .collect();
            let tail = if updates.is_empty() {
                format!(" ON CONFLICT ({}) DO NOTHING", quoted_pks.join(", "))
            } else {
                format!(
                    " ON CONFLICT ({}) DO UPDATE SET {}",
                    quoted_pks.join(", "),
                    updates.join(", ")
                )
            };
            format!("INSERT INTO {qualified} ({col_list}) VALUES {values}{tail}")
        }
        UpsertPattern::OnDuplicateKey => {
            let assignments = if non_pk.is_empty() {
                vec![format!("{} = {}", quoted_pks[0], quoted_pks[0])]
            } else {
                non_pk
                    .iter()
                    .map(|c| format!("{c} = VALUES({c})"))
                    .collect()
            };
            format!(
                "INSERT INTO {qualified} ({col_list}) VALUES {values} ON DUPLICATE KEY UPDATE {}",
                assignments.join(", ")
            )
        }
        UpsertPattern::InsertOrUpdate | UpsertPattern::SelectThenInsertOrUpdate => {
            format!("INSERT INTO {qualified} ({col_list}) VALUES {values}")
        }
    }
}

/// Oracle has no multi-row `VALUES` list and no `AS` on table aliases, and
/// `INSERT ALL` is not an option either: a sequence or identity default on a
/// column the copy does not supply is evaluated once per source row there, so
/// every `INTO` clause would get the same value. Rows therefore travel as a
/// single-table `INSERT … SELECT … FROM DUAL UNION ALL SELECT … FROM DUAL`
/// (binds numbered across the whole statement); upserts render a `MERGE`
/// whose source is the same `UNION ALL` of one-row selects. Every bind is
/// wrapped in the column's Oracle type ([`oracle_bind_expr`]) because the
/// branches of a `UNION ALL` must agree on each column's type and an untyped
/// NULL binds as VARCHAR2. With no non-key columns there is nothing to
/// update, so the `WHEN MATCHED` clause is left out and existing rows are
/// simply kept.
fn oracle_insert_sql(
    plan: &TransferPlan,
    table: &TransferTable,
    qualified: &str,
    cols: &[String],
    rows: usize,
) -> String {
    let col_list = cols.join(", ");
    let target_types: Vec<String> = table
        .columns
        .iter()
        .map(|c| map_column_type(&c.source_type, plan.source, plan.target).target_ddl)
        .collect();
    let mut n = 1usize;
    let mut source_row = |aliased: bool| -> String {
        let exprs: Vec<String> = target_types
            .iter()
            .enumerate()
            .map(|(i, ddl)| {
                let expr = oracle_bind_expr(ddl, &placeholder(plan.target, n));
                n += 1;
                if aliased {
                    format!("{expr} AS {}", cols[i])
                } else {
                    expr
                }
            })
            .collect();
        format!("SELECT {} FROM DUAL", exprs.join(", "))
    };

    if plan.mode != TransferMode::UpsertOnPrimaryKey {
        let mut lines = vec![format!("INSERT INTO {qualified} ({col_list})")];
        for i in 0..rows {
            let row = source_row(false);
            lines.push(if i == 0 {
                format!("  {row}")
            } else {
                format!("  UNION ALL {row}")
            });
        }
        return lines.join("\n");
    }

    let pk_names: Vec<String> = pk_columns(table)
        .iter()
        .map(|c| apply_casing(&c.name, plan.casing))
        .collect();
    let quoted_pks: Vec<String> = pk_names
        .iter()
        .map(|k| plan.target.quote_identifier(k))
        .collect();
    let non_pk: Vec<String> = table
        .columns
        .iter()
        .filter(|c| c.pk_ordinal.is_none())
        .map(|c| {
            plan.target
                .quote_identifier(&apply_casing(&c.name, plan.casing))
        })
        .collect();

    let source_rows: Vec<String> = (0..rows).map(|_| source_row(true)).collect();
    let on: Vec<String> = quoted_pks
        .iter()
        .map(|pk| format!("target.{pk} = src.{pk}"))
        .collect();
    let mut lines = vec![
        format!("MERGE INTO {qualified} target"),
        format!("USING ({}) src", source_rows.join(" UNION ALL ")),
        format!("ON ({})", on.join(" AND ")),
    ];
    if !non_pk.is_empty() {
        let updates: Vec<String> = non_pk
            .iter()
            .map(|c| format!("    target.{c} = src.{c}"))
            .collect();
        lines.push("WHEN MATCHED THEN".into());
        lines.push("    UPDATE SET".into());
        lines.push(updates.join(",\n"));
    }
    lines.push("WHEN NOT MATCHED THEN".into());
    lines.push(format!("    INSERT ({col_list})"));
    lines.push(format!(
        "    VALUES ({})",
        cols.iter()
            .map(|c| format!("src.{c}"))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    lines.join("\n")
}

/// A bind wrapped in the target column's Oracle type, so every branch of a
/// `UNION ALL` agrees and NULLs land typed: `CAST(:n AS number(10,2))`,
/// `TO_CLOB(:n)` / `TO_BLOB(:n)` for the LOB types `CAST` refuses, and the
/// bare bind for JSON / XMLType, which take character input as they are.
pub fn oracle_bind_expr(target_ddl: &str, bind: &str) -> String {
    let base = target_ddl
        .trim()
        .split(['(', ' '])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    match base.as_str() {
        "clob" | "nclob" => format!("TO_CLOB({bind})"),
        "blob" => format!("TO_BLOB({bind})"),
        "json" | "xmltype" => bind.to_string(),
        _ => format!("CAST({bind} AS {})", target_ddl.trim()),
    }
}

fn merge_insert_sql(
    target: DatabaseType,
    qualified: &str,
    cols: &[String],
    quoted_pks: &[String],
    non_pk: &[String],
    rows: usize,
    col_count: usize,
) -> String {
    let tds = matches!(target.connection_kind(), ConnectionKind::Tds);
    let values = value_placeholders(target, rows, col_count);
    let alias = "SqllyUpsertSource";
    let target_alias = if tds { "[target]" } else { "target" };
    let on_parts: Vec<String> = quoted_pks
        .iter()
        .map(|pk| format!("{target_alias}.{pk} = {alias}.{pk}"))
        .collect();
    let on = on_parts.join(" AND ");
    let mut lines = vec![
        format!("MERGE {qualified} AS {target_alias}"),
        format!("USING (VALUES {values}) AS {alias} ({})", cols.join(", ")),
    ];
    if tds {
        lines.push(format!("ON {on}"));
    } else {
        lines.push(format!("ON ({on})"));
    }
    if non_pk.is_empty() {
        lines.push("WHEN MATCHED THEN".into());
        lines.push("    DELETE".into());
    } else {
        let updates: Vec<String> = non_pk
            .iter()
            .map(|c| format!("    {target_alias}.{c} = {alias}.{c}"))
            .collect();
        lines.push("WHEN MATCHED THEN".into());
        lines.push("    UPDATE SET".into());
        lines.push(updates.join(",\n"));
    }
    lines.push("WHEN NOT MATCHED THEN".into());
    lines.push(format!("    INSERT ({})", cols.join(", ")));
    lines.push(format!(
        "    VALUES ({})",
        cols.iter()
            .map(|c| format!("{alias}.{c}"))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    lines.join("\n")
}

pub fn statements_for(
    plan: &TransferPlan,
    table: &TransferTable,
) -> Result<TransferStatements, sqlly_core::DisplayMessage> {
    let mut warnings = Vec::new();
    let (_, order_warnings) = ordered_tables(plan);
    warnings.extend(order_warnings);

    if plan.mode == TransferMode::UpsertOnPrimaryKey && pk_columns(table).is_empty() {
        return Err(sqlly_core::DisplayMessage::new(
            "services.transfer.errors.upsert_no_primary_key",
            format!("{} has no primary key — upsert mode cannot run", table.name),
        )
        .with_arg("table_name", table.name.clone()));
    }
    if plan.mode == TransferMode::UpsertOnPrimaryKey
        && matches!(
            plan.target.connection_kind(),
            ConnectionKind::ClickHouseHttp
        )
    {
        return Err(sqlly_core::DisplayMessage::new(
            "services.transfer.errors.clickhouse_upsert",
            format!("{}: ClickHouse has no upsert (rows are appended; duplicates are not merged) — use Append or Truncate then insert", table.name),
        )
        .with_arg("table_name", table.name.clone()));
    }

    let mut pre = Vec::new();
    let mut post = Vec::new();

    match plan.mode {
        TransferMode::CreateTable => {
            let (ddl, mut ddl_warnings) = create_table_sql(plan, table);
            warnings.append(&mut ddl_warnings);
            pre.push(ddl);
        }
        TransferMode::TruncateThenInsert => {
            let q = qualified_target(plan, table);
            if plan.target.supports_operation(SqlOperation::TruncateTable) {
                pre.push(format!("TRUNCATE TABLE {q}"));
            } else {
                pre.push(format!("DELETE FROM {q}"));
            }
        }
        TransferMode::Append | TransferMode::UpsertOnPrimaryKey => {}
    }

    let has_identity = table.columns.iter().any(|c| c.is_identity);
    let qualified = qualified_target(plan, table);
    if has_identity
        && plan.mode != TransferMode::CreateTable
        && matches!(plan.target.connection_kind(), ConnectionKind::Tds)
    {
        pre.push(format!("SET IDENTITY_INSERT {qualified} ON"));
        post.push(format!("SET IDENTITY_INSERT {qualified} OFF"));
    }

    let batch_rows = rows_per_statement(plan.target, table.columns.len());
    let insert = insert_sql(plan, table, batch_rows);

    Ok(TransferStatements {
        pre,
        insert_sql: insert,
        rows_per_statement: batch_rows,
        post,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_table() -> TransferTable {
        TransferTable {
            schema: Some("dbo".into()),
            name: "items".into(),
            columns: vec![
                TransferColumn {
                    name: "id".into(),
                    source_type: "int".into(),
                    nullable: false,
                    is_identity: false,
                    pk_ordinal: Some(1),
                },
                TransferColumn {
                    name: "name".into(),
                    source_type: "nvarchar(50)".into(),
                    nullable: true,
                    is_identity: false,
                    pk_ordinal: None,
                },
            ],
            depends_on: Vec::new(),
        }
    }

    #[test]
    fn placeholder_per_engine() {
        assert_eq!(placeholder(DatabaseType::SqlServer, 3), "@P3");
        assert_eq!(placeholder(DatabaseType::Postgres, 2), "$2");
        assert_eq!(placeholder(DatabaseType::MySql, 5), "?");
        assert_eq!(placeholder(DatabaseType::Sqlite, 1), "?");
    }

    #[test]
    fn rows_per_statement_caps() {
        assert_eq!(rows_per_statement(DatabaseType::SqlServer, 3), 666);
        assert_eq!(rows_per_statement(DatabaseType::SqlServer, 1), 1000);
        assert_eq!(rows_per_statement(DatabaseType::Postgres, 50), 1000);
        assert_eq!(rows_per_statement(DatabaseType::Sqlite, 40), 747);
        assert_eq!(rows_per_statement(DatabaseType::MySql, 2000), 14);
    }

    #[test]
    fn insert_sql_two_by_two_sql_server() {
        let plan = TransferPlan {
            source: DatabaseType::SqlServer,
            target: DatabaseType::SqlServer,
            target_schema: Some("dbo".into()),
            mode: TransferMode::Append,
            casing: NameCasing::AsIs,
            tables: vec![sample_table()],
        };
        let sql = insert_sql(&plan, &sample_table(), 2);
        assert_eq!(
            sql,
            "INSERT INTO [dbo].[items] ([id], [name]) VALUES (@P1,@P2),(@P3,@P4)"
        );
    }

    #[test]
    fn insert_sql_two_by_two_postgres() {
        let plan = TransferPlan {
            source: DatabaseType::SqlServer,
            target: DatabaseType::Postgres,
            target_schema: Some("public".into()),
            mode: TransferMode::Append,
            casing: NameCasing::AsIs,
            tables: vec![sample_table()],
        };
        let mut table = sample_table();
        table.schema = Some("public".into());
        let sql = insert_sql(&plan, &table, 2);
        assert_eq!(
            sql,
            r#"INSERT INTO "public"."items" ("id", "name") VALUES ($1,$2),($3,$4)"#
        );
    }

    #[test]
    fn insert_sql_two_by_two_mysql() {
        let plan = TransferPlan {
            source: DatabaseType::SqlServer,
            target: DatabaseType::MySql,
            target_schema: None,
            mode: TransferMode::Append,
            casing: NameCasing::AsIs,
            tables: vec![sample_table()],
        };
        let mut table = sample_table();
        table.schema = None;
        let sql = insert_sql(&plan, &table, 2);
        assert_eq!(sql, "INSERT INTO `items` (`id`, `name`) VALUES (?,?),(?,?)");
    }

    #[test]
    fn insert_sql_two_by_two_sqlite() {
        let plan = TransferPlan {
            source: DatabaseType::SqlServer,
            target: DatabaseType::Sqlite,
            target_schema: None,
            mode: TransferMode::Append,
            casing: NameCasing::AsIs,
            tables: vec![sample_table()],
        };
        let sql = insert_sql(&plan, &sample_table(), 2);
        assert_eq!(
            sql,
            r#"INSERT INTO "items" ("id", "name") VALUES (?,?),(?,?)"#
        );
    }

    #[test]
    fn upsert_on_conflict_postgres() {
        let plan = TransferPlan {
            source: DatabaseType::Postgres,
            target: DatabaseType::Postgres,
            target_schema: Some("public".into()),
            mode: TransferMode::UpsertOnPrimaryKey,
            casing: NameCasing::AsIs,
            tables: vec![sample_table()],
        };
        let mut table = sample_table();
        table.schema = Some("public".into());
        let sql = insert_sql(&plan, &table, 1);
        assert!(sql.contains("ON CONFLICT"));
        assert!(sql.contains("DO UPDATE SET"));
    }

    #[test]
    fn upsert_on_duplicate_key_mysql() {
        let plan = TransferPlan {
            source: DatabaseType::SqlServer,
            target: DatabaseType::MySql,
            target_schema: None,
            mode: TransferMode::UpsertOnPrimaryKey,
            casing: NameCasing::AsIs,
            tables: vec![sample_table()],
        };
        let sql = insert_sql(&plan, &sample_table(), 1);
        assert!(sql.contains("ON DUPLICATE KEY UPDATE"));
    }

    #[test]
    fn upsert_merge_sql_server() {
        let plan = TransferPlan {
            source: DatabaseType::SqlServer,
            target: DatabaseType::SqlServer,
            target_schema: Some("dbo".into()),
            mode: TransferMode::UpsertOnPrimaryKey,
            casing: NameCasing::AsIs,
            tables: vec![sample_table()],
        };
        let sql = insert_sql(&plan, &sample_table(), 1);
        assert!(sql.starts_with("MERGE [dbo].[items]"));
        assert!(sql.contains("WHEN MATCHED THEN"));
        assert!(sql.contains("WHEN NOT MATCHED THEN"));
    }

    fn oracle_plan(mode: TransferMode) -> TransferPlan {
        TransferPlan {
            source: DatabaseType::Sqlite,
            target: DatabaseType::Oracle,
            target_schema: Some("HR".into()),
            mode,
            casing: NameCasing::Upper,
            tables: vec![sample_table()],
        }
    }

    #[test]
    fn oracle_placeholders_and_bind_budget() {
        assert_eq!(placeholder(DatabaseType::Oracle, 7), ":7");
        // 1000-bind budget minus the 100 reserve, two columns per row.
        assert_eq!(rows_per_statement(DatabaseType::Oracle, 2), 450);
        assert_eq!(rows_per_statement(DatabaseType::Oracle, 1), 900);
        assert_eq!(rows_per_statement(DatabaseType::Oracle, 300), 3);
    }

    #[test]
    fn insert_sql_oracle_is_one_typed_union_all_per_batch() {
        let plan = oracle_plan(TransferMode::Append);
        let sql = insert_sql(&plan, &sample_table(), 2);
        assert_eq!(
            sql,
            "INSERT INTO \"HR\".\"ITEMS\" (\"ID\", \"NAME\")\n  SELECT CAST(:1 AS number(10)), CAST(:2 AS varchar2(50)) FROM DUAL\n  UNION ALL SELECT CAST(:3 AS number(10)), CAST(:4 AS varchar2(50)) FROM DUAL"
        );
        // Every load mode that is not an upsert uses the same shape; there is
        // never an INSERT ALL (its sequence defaults repeat per statement).
        for mode in [TransferMode::CreateTable, TransferMode::TruncateThenInsert] {
            let sql = insert_sql(&oracle_plan(mode), &sample_table(), 1);
            assert!(
                sql.starts_with("INSERT INTO \"HR\".\"ITEMS\" (\"ID\", \"NAME\")\n  SELECT"),
                "{sql}"
            );
            assert!(sql.ends_with("FROM DUAL"), "{sql}");
            assert!(
                !sql.contains("INSERT ALL") && !sql.contains("VALUES"),
                "{sql}"
            );
        }
    }

    #[test]
    fn oracle_bind_expressions_type_every_column() {
        assert_eq!(
            oracle_bind_expr("number(10,2)", ":3"),
            "CAST(:3 AS number(10,2))"
        );
        assert_eq!(
            oracle_bind_expr("varchar2(40)", ":1"),
            "CAST(:1 AS varchar2(40))"
        );
        assert_eq!(oracle_bind_expr("timestamp", ":1"), "CAST(:1 AS timestamp)");
        assert_eq!(
            oracle_bind_expr("timestamp with time zone", ":1"),
            "CAST(:1 AS timestamp with time zone)"
        );
        assert_eq!(oracle_bind_expr("raw(16)", ":1"), "CAST(:1 AS raw(16))");
        assert_eq!(oracle_bind_expr("clob", ":2"), "TO_CLOB(:2)");
        assert_eq!(oracle_bind_expr("blob", ":2"), "TO_BLOB(:2)");
        assert_eq!(oracle_bind_expr("json", ":2"), ":2");
        assert_eq!(oracle_bind_expr("xmltype", ":2"), ":2");
    }

    #[test]
    fn upsert_merge_oracle_uses_typed_dual_source_without_as_aliases() {
        let plan = oracle_plan(TransferMode::UpsertOnPrimaryKey);
        let sql = insert_sql(&plan, &sample_table(), 2);
        assert!(
            sql.starts_with("MERGE INTO \"HR\".\"ITEMS\" target\n"),
            "{sql}"
        );
        assert!(
            sql.contains(
                "USING (SELECT CAST(:1 AS number(10)) AS \"ID\", CAST(:2 AS varchar2(50)) AS \"NAME\" FROM DUAL UNION ALL SELECT CAST(:3 AS number(10)) AS \"ID\", CAST(:4 AS varchar2(50)) AS \"NAME\" FROM DUAL) src"
            ),
            "{sql}"
        );
        assert!(sql.contains("ON (target.\"ID\" = src.\"ID\")"), "{sql}");
        assert!(
            sql.contains("WHEN MATCHED THEN\n    UPDATE SET\n    target.\"NAME\" = src.\"NAME\""),
            "{sql}"
        );
        assert!(
            sql.contains(
                "WHEN NOT MATCHED THEN\n    INSERT (\"ID\", \"NAME\")\n    VALUES (src.\"ID\", src.\"NAME\")"
            ),
            "{sql}"
        );
        assert!(
            !sql.contains(" AS target") && !sql.contains(" AS src"),
            "{sql}"
        );

        // A key-only table has nothing to update: no WHEN MATCHED clause.
        let mut table = sample_table();
        table.columns.truncate(1);
        let sql = insert_sql(&plan, &table, 1);
        assert!(!sql.contains("WHEN MATCHED"), "{sql}");
        assert!(
            sql.contains("WHEN NOT MATCHED THEN\n    INSERT (\"ID\")"),
            "{sql}"
        );
    }

    #[test]
    fn create_table_sqlite_to_oracle_spells_oracle_types() {
        let table = TransferTable {
            schema: None,
            name: "orders".into(),
            columns: vec![
                TransferColumn {
                    name: "id".into(),
                    source_type: "INTEGER".into(),
                    nullable: false,
                    is_identity: false,
                    pk_ordinal: Some(1),
                },
                TransferColumn {
                    name: "title".into(),
                    source_type: "varchar(120)".into(),
                    nullable: false,
                    is_identity: false,
                    pk_ordinal: None,
                },
                TransferColumn {
                    name: "label".into(),
                    source_type: "nchar".into(),
                    nullable: true,
                    is_identity: false,
                    pk_ordinal: None,
                },
                TransferColumn {
                    name: "price".into(),
                    source_type: "decimal(10,2)".into(),
                    nullable: true,
                    is_identity: false,
                    pk_ordinal: None,
                },
                TransferColumn {
                    name: "active".into(),
                    source_type: "boolean".into(),
                    nullable: false,
                    is_identity: false,
                    pk_ordinal: None,
                },
                TransferColumn {
                    name: "notes".into(),
                    source_type: "text".into(),
                    nullable: true,
                    is_identity: false,
                    pk_ordinal: None,
                },
                TransferColumn {
                    name: "blob".into(),
                    source_type: "blob".into(),
                    nullable: true,
                    is_identity: false,
                    pk_ordinal: None,
                },
            ],
            depends_on: Vec::new(),
        };
        let plan = TransferPlan {
            source: DatabaseType::Sqlite,
            target: DatabaseType::Oracle,
            target_schema: None,
            mode: TransferMode::CreateTable,
            casing: NameCasing::Upper,
            tables: vec![table.clone()],
        };
        let (ddl, _) = create_table_sql(&plan, &table);
        assert!(ddl.starts_with("CREATE TABLE \"ORDERS\" ("), "{ddl}");
        assert!(ddl.contains("\"TITLE\" varchar2(120) NOT NULL"), "{ddl}");
        // A length-less character type must still get a length on Oracle.
        assert!(ddl.contains("\"LABEL\" varchar2(4000)"), "{ddl}");
        assert!(ddl.contains("\"PRICE\" number(10,2)"), "{ddl}");
        assert!(ddl.contains("\"ACTIVE\" number(1) NOT NULL"), "{ddl}");
        assert!(ddl.contains("\"NOTES\" clob"), "{ddl}");
        assert!(ddl.contains("\"BLOB\" blob"), "{ddl}");
        assert!(ddl.contains("PRIMARY KEY (\"ID\")"), "{ddl}");
        assert!(!ddl.to_ascii_uppercase().contains("IDENTITY"), "{ddl}");
        let stmts = statements_for(&plan, &table).unwrap();
        assert!(stmts.pre.iter().all(|s| !s.contains("IDENTITY_INSERT")));
        assert!(stmts.post.is_empty());
    }

    #[test]
    fn create_table_into_clickhouse_is_mergetree_ordered_by_the_key() {
        let plan = TransferPlan {
            source: DatabaseType::Sqlite,
            target: DatabaseType::ClickHouse,
            target_schema: Some("shop".into()),
            mode: TransferMode::CreateTable,
            casing: NameCasing::AsIs,
            tables: vec![sample_table()],
        };
        let (ddl, warnings) = create_table_sql(&plan, &plan.tables[0]);
        assert!(ddl.starts_with("CREATE TABLE `shop`.`items` (\n"), "{ddl}");
        assert!(ddl.ends_with(") ENGINE = MergeTree ORDER BY `id`"), "{ddl}");
        assert!(!ddl.contains("NOT NULL"), "{ddl}");
        assert!(!ddl.contains("PRIMARY KEY"), "{ddl}");
        assert!(ddl.contains("`name` Nullable("), "{ddl}");
        assert!(
            warnings.iter().all(|w| !w.contains("tuple()")),
            "{warnings:?}"
        );
        let mut two_keys = sample_table();
        two_keys.columns[1].pk_ordinal = Some(2);
        let (ddl, _) = create_table_sql(&plan, &two_keys);
        assert!(ddl.ends_with("ORDER BY (`id`, `name`)"), "{ddl}");
        assert!(
            !ddl.contains("Nullable("),
            "key columns are never Nullable: {ddl}"
        );

        let mut keyless = sample_table();
        for c in &mut keyless.columns {
            c.pk_ordinal = None;
        }
        let plan = TransferPlan {
            tables: vec![keyless],
            ..plan
        };
        let (ddl, warnings) = create_table_sql(&plan, &plan.tables[0]);
        assert!(ddl.ends_with("ORDER BY tuple()"), "{ddl}");
        assert!(
            warnings.iter().any(|w| w.contains("tuple()")),
            "{warnings:?}"
        );
        assert_eq!(placeholder(DatabaseType::ClickHouse, 4), "?");
        let stmts = statements_for(&plan, &plan.tables[0]).unwrap();
        assert!(stmts.insert_sql.starts_with("INSERT INTO `shop`.`items` ("));
    }

    #[test]
    fn upsert_into_clickhouse_is_refused_with_a_reason() {
        let plan = TransferPlan {
            source: DatabaseType::Postgres,
            target: DatabaseType::ClickHouse,
            target_schema: None,
            mode: TransferMode::UpsertOnPrimaryKey,
            casing: NameCasing::AsIs,
            tables: vec![sample_table()],
        };
        let err = statements_for(&plan, &plan.tables[0]).unwrap_err();
        assert!(err.contains("no upsert"), "{err}");
        let truncate = TransferPlan {
            mode: TransferMode::TruncateThenInsert,
            ..plan
        };
        let stmts = statements_for(&truncate, &truncate.tables[0]).unwrap();
        assert_eq!(stmts.pre, vec!["TRUNCATE TABLE `dbo`.`items`".to_string()]);
    }

    #[test]
    fn create_table_sql_server_to_postgres() {
        let table = TransferTable {
            schema: Some("dbo".into()),
            name: "orders".into(),
            columns: vec![
                TransferColumn {
                    name: "title".into(),
                    source_type: "nvarchar(120)".into(),
                    nullable: false,
                    is_identity: false,
                    pk_ordinal: Some(2),
                },
                TransferColumn {
                    name: "id".into(),
                    source_type: "bigint".into(),
                    nullable: false,
                    is_identity: true,
                    pk_ordinal: Some(1),
                },
                TransferColumn {
                    name: "created".into(),
                    source_type: "datetime2".into(),
                    nullable: false,
                    is_identity: false,
                    pk_ordinal: None,
                },
                TransferColumn {
                    name: "active".into(),
                    source_type: "bit".into(),
                    nullable: false,
                    is_identity: false,
                    pk_ordinal: None,
                },
            ],
            depends_on: Vec::new(),
        };
        let plan = TransferPlan {
            source: DatabaseType::SqlServer,
            target: DatabaseType::Postgres,
            target_schema: Some("public".into()),
            mode: TransferMode::CreateTable,
            casing: NameCasing::AsIs,
            tables: vec![table.clone()],
        };
        let (ddl, warnings) = create_table_sql(&plan, &table);
        assert!(ddl.starts_with(r#"CREATE TABLE "public"."orders" ("#));
        assert!(ddl.contains(r#""title" varchar(120) NOT NULL"#));
        assert!(ddl.contains(r#""id" bigint NOT NULL"#));
        assert!(ddl.contains(r#""created" timestamp NOT NULL"#));
        assert!(ddl.contains(r#""active" boolean NOT NULL"#));
        assert!(ddl.contains(r#"PRIMARY KEY ("id", "title")"#));
        assert!(warnings.iter().any(|w| w.contains("identity")));
    }

    #[test]
    fn truncate_fallback_sqlite() {
        let plan = TransferPlan {
            source: DatabaseType::Sqlite,
            target: DatabaseType::Sqlite,
            target_schema: None,
            mode: TransferMode::TruncateThenInsert,
            casing: NameCasing::AsIs,
            tables: vec![sample_table()],
        };
        let stmts = statements_for(&plan, &sample_table()).unwrap();
        assert_eq!(stmts.pre, vec![r#"DELETE FROM "items""#.to_string()]);
    }

    #[test]
    fn fk_ordering_parent_first() {
        let parent = TransferTable {
            schema: Some("dbo".into()),
            name: "parent".into(),
            columns: vec![TransferColumn {
                name: "id".into(),
                source_type: "int".into(),
                nullable: false,
                is_identity: false,
                pk_ordinal: Some(1),
            }],
            depends_on: Vec::new(),
        };
        let child = TransferTable {
            schema: Some("dbo".into()),
            name: "child".into(),
            columns: vec![TransferColumn {
                name: "id".into(),
                source_type: "int".into(),
                nullable: false,
                is_identity: false,
                pk_ordinal: Some(1),
            }],
            depends_on: vec!["dbo.parent".into()],
        };
        let plan = TransferPlan {
            source: DatabaseType::SqlServer,
            target: DatabaseType::SqlServer,
            target_schema: Some("dbo".into()),
            mode: TransferMode::Append,
            casing: NameCasing::AsIs,
            tables: vec![child.clone(), parent.clone()],
        };
        let (ordered, _) = ordered_tables(&plan);
        assert_eq!(ordered.len(), 2);
        assert_eq!(ordered[0].name, "parent");
        assert_eq!(ordered[1].name, "child");
    }

    #[test]
    fn name_casing_lower_in_ddl_and_insert() {
        let plan = TransferPlan {
            source: DatabaseType::SqlServer,
            target: DatabaseType::Postgres,
            target_schema: Some("public".into()),
            mode: TransferMode::Append,
            casing: NameCasing::Lower,
            tables: vec![sample_table()],
        };
        let mut table = sample_table();
        table.schema = Some("public".into());
        let sql = insert_sql(&plan, &table, 1);
        assert!(sql.contains(r#""items""#));
        assert!(sql.contains(r#""id""#));
        let (ddl, _) = create_table_sql(
            &TransferPlan {
                mode: TransferMode::CreateTable,
                ..plan.clone()
            },
            &table,
        );
        assert!(ddl.contains(r#""items""#));
    }

    #[test]
    fn upsert_without_pk_errors() {
        let mut table = sample_table();
        for col in &mut table.columns {
            col.pk_ordinal = None;
        }
        let plan = TransferPlan {
            source: DatabaseType::SqlServer,
            target: DatabaseType::SqlServer,
            target_schema: Some("dbo".into()),
            mode: TransferMode::UpsertOnPrimaryKey,
            casing: NameCasing::AsIs,
            tables: vec![table.clone()],
        };
        assert!(statements_for(&plan, &table).is_err());
    }
}
