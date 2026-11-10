//! View Dependencies / Find Usages dialog for one server-explorer object.
//!
//! Two bounded, read-only sections for the object the dialog was opened on:
//! DEPENDENCIES (catalog-declared references in both directions — what the
//! target uses and what uses the target) and USAGES (a textual search for the
//! object's name across stored module source). Both are engine-dispatched:
//! SQL Server reads `sys.sql_expression_dependencies` / `sys.sql_modules`,
//! PostgreSQL reads `pg_depend` / `pg_constraint` / stored source, MySQL and
//! MariaDB combine foreign keys with visible stored definitions, and SQLite
//! combines `sqlite_schema` SQL with `PRAGMA foreign_key_list`. Results name
//! best-effort limitations and permission gaps instead of presenting textual
//! matches as exhaustive. Each section offers "Open as Query" for refinement.

use gpui::{
    Entity, EventEmitter, FocusHandle, IntoElement, ParentElement, Render, Styled, Window, div, px,
};
use gpui_component::Disableable as _;
use gpui_component::Sizable as _;
use gpui_component::button::Button;
use sqlly_datatable::SqllyDataTable;
use sqlly_lang::sqlmodel::{ConnectionKind, DatabaseType};

use crate::dialog::{Dialog, DialogFrame};
use crate::theme::Theme;
use crate::tool_dialog::{
    TOOL_QUERY_ROW_CAP, ToolDialogContext, ToolDialogEvent, ToolQueryOutcome,
};

// ---------------------------------------------------------------------------
// Pure SQL construction (unit-tested; schema/name are hostile strings)
// ---------------------------------------------------------------------------

/// Double embedded single quotes so `s` is inert inside a `'…'` literal.
fn sql_literal(s: &str) -> String {
    s.replace('\'', "''")
}

/// `[name]` with embedded `]` doubled — T-SQL identifier quoting.
fn bracket_quote(s: &str) -> String {
    format!("[{}]", s.replace(']', "]]"))
}

/// `"name"` with embedded `"` doubled — PostgreSQL identifier quoting.
fn pg_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// Body of the `OBJECT_ID(N'…')` literal for the target: bracket-quote each
/// part (so `]` cannot escape the identifier), then literal-escape (so `'`
/// cannot escape the string).
fn tds_target_literal(schema: &str, name: &str) -> String {
    let qualified = if schema.is_empty() {
        bracket_quote(name)
    } else {
        format!("{}.{}", bracket_quote(schema), bracket_quote(name))
    };
    sql_literal(&qualified)
}

/// Body of the `to_regclass('…')` literal for the target: double-quote each
/// part, then literal-escape.
fn pg_target_literal(schema: &str, name: &str) -> String {
    let qualified = if schema.is_empty() {
        pg_quote(name)
    } else {
        format!("{}.{}", pg_quote(schema), pg_quote(name))
    };
    sql_literal(&qualified)
}

/// LIKE-pattern fragment for T-SQL: backslash-escape the escape char itself
/// and the wildcards `% _ [ ]` (paired with `ESCAPE '\'`), then literal-escape.
fn tds_like_fragment(name: &str) -> String {
    sql_literal(
        &name
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_")
            .replace('[', "\\[")
            .replace(']', "\\]"),
    )
}

/// LIKE-pattern fragment for PostgreSQL: backslash is the default escape
/// character, so escape it and the `% _` wildcards, then literal-escape.
fn pg_like_fragment(name: &str) -> String {
    sql_literal(
        &name
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_"),
    )
}

/// MySQL string-literal body: backslash is an escape character under the
/// default sql_mode, so it is doubled along with the quote — a catalog name
/// ending in `\` must not swallow the closing quote.
fn mysql_literal(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\'', "''")
}

fn mysql_schema_value(schema: &str) -> String {
    if schema.is_empty() {
        "DATABASE()".to_string()
    } else {
        format!("'{}'", mysql_literal(schema))
    }
}

/// ClickHouse string-literal body: backslash is an escape character there.
fn clickhouse_literal(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\'', "''")
}

fn clickhouse_database_value(schema: &str) -> String {
    if schema.is_empty() {
        "currentDatabase()".to_string()
    } else {
        format!("'{}'", clickhouse_literal(schema))
    }
}

fn metadata_note(db: DatabaseType) -> String {
    crate::localization::text(match db.connection_kind() {
        ConnectionKind::Tds => "explorer.object_dependencies.metadata_sql_server",
        ConnectionKind::PostgresWire => "explorer.object_dependencies.metadata_postgres",
        ConnectionKind::MySqlWire => "explorer.object_dependencies.metadata_mysql",
        ConnectionKind::SqliteFile | ConnectionKind::HranaHttp => {
            "explorer.object_dependencies.metadata_sqlite"
        }
        ConnectionKind::DuckDbFile => "explorer.object_dependencies.metadata_duckdb",
        ConnectionKind::ClickHouseHttp => "explorer.object_dependencies.metadata_clickhouse",
        ConnectionKind::OracleNet => "explorer.object_dependencies.metadata_oracle",
        _ => "explorer.object_dependencies.metadata_unsupported",
    })
}

fn permission_note(db: DatabaseType) -> String {
    crate::localization::text(match db.connection_kind() {
        ConnectionKind::Tds => "explorer.object_dependencies.permission_sql_server",
        ConnectionKind::PostgresWire => "explorer.object_dependencies.permission_postgres",
        ConnectionKind::MySqlWire => "explorer.object_dependencies.permission_mysql",
        ConnectionKind::SqliteFile | ConnectionKind::HranaHttp => {
            "explorer.object_dependencies.permission_sqlite"
        }
        ConnectionKind::DuckDbFile => "explorer.object_dependencies.permission_duckdb",
        ConnectionKind::ClickHouseHttp => "explorer.object_dependencies.permission_clickhouse",
        ConnectionKind::OracleNet => "explorer.object_dependencies.permission_oracle",
        _ => "explorer.object_dependencies.permission_unsupported",
    })
}

/// Engine-dispatched dependency listing for the target object, both
/// directions, three columns: Direction ('Uses' / 'Used by'), Object
/// (schema-qualified), Type. `None` when the engine has no server-side
/// dependency catalog wired yet (the dialog shows the capability reason).
pub fn dependencies_sql(db: DatabaseType, schema: &str, name: &str) -> Option<String> {
    match db.connection_kind() {
        ConnectionKind::Tds => {
            let target = tds_target_literal(schema, name);
            Some(format!(
                "SELECT N'Uses' AS [Direction],\n\
                 \x20      QUOTENAME(SCHEMA_NAME(o.schema_id)) + N'.' + QUOTENAME(o.name) AS [Object],\n\
                 \x20      o.type_desc AS [Type]\n\
                 FROM sys.sql_expression_dependencies d\n\
                 JOIN sys.objects o ON o.object_id = d.referenced_id\n\
                 WHERE d.referencing_id = OBJECT_ID(N'{target}')\n\
                 UNION\n\
                 SELECT N'Used by',\n\
                 \x20      QUOTENAME(SCHEMA_NAME(o.schema_id)) + N'.' + QUOTENAME(o.name),\n\
                 \x20      o.type_desc\n\
                 FROM sys.sql_expression_dependencies d\n\
                 JOIN sys.objects o ON o.object_id = d.referencing_id\n\
                 WHERE d.referenced_id = OBJECT_ID(N'{target}')\n\
                 ORDER BY [Direction], [Object];"
            ))
        }
        ConnectionKind::PostgresWire => {
            let target = pg_target_literal(schema, name);
            Some(format!(
                r#"SELECT "Direction", "Object", "Type" FROM (
  SELECT DISTINCT 'Used by' AS "Direction",
         n.nspname || '.' || c.relname AS "Object",
         CASE c.relkind WHEN 'r' THEN 'TABLE' WHEN 'p' THEN 'PARTITIONED TABLE'
              WHEN 'v' THEN 'VIEW' WHEN 'm' THEN 'MATERIALIZED VIEW'
              WHEN 'f' THEN 'FOREIGN TABLE' ELSE c.relkind::text END AS "Type"
  FROM pg_catalog.pg_depend d
  JOIN pg_catalog.pg_rewrite rw ON rw.oid = d.objid
  JOIN pg_catalog.pg_class c ON c.oid = rw.ev_class
  JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
  WHERE d.classid = 'pg_rewrite'::regclass
    AND d.refclassid = 'pg_class'::regclass
    AND d.refobjid = to_regclass('{target}')
    AND c.oid <> to_regclass('{target}')
    AND n.nspname NOT IN ('pg_catalog', 'information_schema')
  UNION
  SELECT DISTINCT 'Used by',
         n2.nspname || '.' || c2.relname,
         'FOREIGN KEY ' || con.conname
  FROM pg_catalog.pg_constraint con
  JOIN pg_catalog.pg_class c2 ON c2.oid = con.conrelid
  JOIN pg_catalog.pg_namespace n2 ON n2.oid = c2.relnamespace
  WHERE con.contype = 'f'
    AND con.confrelid = to_regclass('{target}')
    AND c2.oid <> to_regclass('{target}')
    AND n2.nspname NOT IN ('pg_catalog', 'information_schema')
  UNION
  SELECT DISTINCT 'Uses',
         n3.nspname || '.' || c3.relname,
         CASE c3.relkind WHEN 'r' THEN 'TABLE' WHEN 'p' THEN 'PARTITIONED TABLE'
              WHEN 'v' THEN 'VIEW' WHEN 'm' THEN 'MATERIALIZED VIEW'
              WHEN 'f' THEN 'FOREIGN TABLE' ELSE c3.relkind::text END
  FROM pg_catalog.pg_depend d2
  JOIN pg_catalog.pg_rewrite rw2 ON rw2.oid = d2.objid
  JOIN pg_catalog.pg_class c3 ON c3.oid = d2.refobjid
  JOIN pg_catalog.pg_namespace n3 ON n3.oid = c3.relnamespace
  WHERE d2.classid = 'pg_rewrite'::regclass
    AND d2.refclassid = 'pg_class'::regclass
    AND rw2.ev_class = to_regclass('{target}')
    AND c3.oid <> to_regclass('{target}')
    AND n3.nspname NOT IN ('pg_catalog', 'information_schema')
  UNION
  SELECT DISTINCT 'Uses',
         n4.nspname || '.' || c4.relname,
         'FOREIGN KEY ' || con2.conname
  FROM pg_catalog.pg_constraint con2
  JOIN pg_catalog.pg_class c4 ON c4.oid = con2.confrelid
  JOIN pg_catalog.pg_namespace n4 ON n4.oid = c4.relnamespace
  WHERE con2.contype = 'f'
    AND con2.conrelid = to_regclass('{target}')
    AND c4.oid <> to_regclass('{target}')
    AND n4.nspname NOT IN ('pg_catalog', 'information_schema')
) dep ORDER BY "Direction", "Object""#
            ))
        }
        ConnectionKind::MySqlWire => {
            let schema_value = mysql_schema_value(schema);
            let name = mysql_literal(name);
            Some(format!(
                r#"SELECT Direction, Object, Type FROM (
  SELECT 'Uses' AS Direction,
         CONCAT('`', referenced_table_schema, '`.`', referenced_table_name, '`') AS Object,
         CONCAT('FOREIGN KEY ', constraint_name) AS Type
  FROM information_schema.key_column_usage
  WHERE table_schema = {schema_value} AND table_name = '{name}'
    AND referenced_table_name IS NOT NULL
  UNION
  SELECT 'Used by',
         CONCAT('`', table_schema, '`.`', table_name, '`'),
         CONCAT('FOREIGN KEY ', constraint_name)
  FROM information_schema.key_column_usage
  WHERE referenced_table_schema = {schema_value} AND referenced_table_name = '{name}'
  UNION
  SELECT 'Uses',
         CONCAT('`', t.table_schema, '`.`', t.table_name, '`'),
         'VIEW DEFINITION MATCH'
  FROM information_schema.views v
  JOIN information_schema.tables t ON t.table_schema = v.table_schema AND t.table_name <> v.table_name
  WHERE v.table_schema = {schema_value} AND v.table_name = '{name}'
    AND LOCATE(LOWER(t.table_name), LOWER(COALESCE(v.view_definition, ''))) > 0
  UNION
  SELECT 'Used by', CONCAT('`', v.table_schema, '`.`', v.table_name, '`'), 'VIEW DEFINITION MATCH'
  FROM information_schema.views v
  WHERE v.table_schema = {schema_value} AND v.table_name <> '{name}'
    AND LOCATE(LOWER('{name}'), LOWER(COALESCE(v.view_definition, ''))) > 0
  UNION
  SELECT 'Used by', CONCAT('`', r.routine_schema, '`.`', r.routine_name, '`'), r.routine_type
  FROM information_schema.routines r
  WHERE r.routine_schema = {schema_value}
    AND LOCATE(LOWER('{name}'), LOWER(COALESCE(r.routine_definition, ''))) > 0
  UNION
  SELECT 'Used by', CONCAT('`', tr.trigger_schema, '`.`', tr.trigger_name, '`'), 'TRIGGER'
  FROM information_schema.triggers tr
  WHERE tr.trigger_schema = {schema_value}
    AND LOCATE(LOWER('{name}'), LOWER(COALESCE(tr.action_statement, ''))) > 0
  UNION
  SELECT 'Used by', CONCAT('`', e.event_schema, '`.`', e.event_name, '`'), 'EVENT'
  FROM information_schema.events e
  WHERE e.event_schema = {schema_value}
    AND LOCATE(LOWER('{name}'), LOWER(COALESCE(e.event_definition, ''))) > 0
) dep ORDER BY Direction, Object LIMIT {TOOL_QUERY_ROW_CAP}"#
            ))
        }
        ConnectionKind::SqliteFile | ConnectionKind::HranaHttp => {
            let name = sql_literal(name);
            Some(format!(
                r#"WITH target AS (
  SELECT sql FROM sqlite_schema WHERE name = '{name}' AND sql IS NOT NULL LIMIT 1
)
SELECT Direction, Object, Type FROM (
  SELECT 'Uses' AS Direction, fk."table" AS Object, 'FOREIGN KEY' AS Type
  FROM pragma_foreign_key_list('{name}') fk
  UNION
  SELECT 'Used by', m.name, 'FOREIGN KEY'
  FROM sqlite_schema m
  JOIN pragma_foreign_key_list(m.name) fk
  WHERE m.type = 'table' AND lower(fk."table") = lower('{name}')
  UNION
  SELECT 'Uses', candidate.name, upper(candidate.type) || ' SQL DEFINITION MATCH'
  FROM target
  JOIN sqlite_schema candidate
    ON candidate.name <> '{name}'
   AND instr(lower(COALESCE(target.sql, '')), lower(candidate.name)) > 0
  WHERE candidate.type IN ('table', 'view')
    AND candidate.name NOT IN (SELECT fk2."table" FROM pragma_foreign_key_list('{name}') fk2)
  UNION
  SELECT 'Used by', m.name, upper(m.type) || ' SQL DEFINITION MATCH'
  FROM sqlite_schema m
  WHERE m.name <> '{name}' AND m.sql IS NOT NULL
    AND m.type IN ('table', 'view', 'trigger', 'index')
    AND instr(lower(m.sql), lower('{name}')) > 0
) dep ORDER BY Direction, Object LIMIT {TOOL_QUERY_ROW_CAP}"#
            ))
        }
        // DuckDB records foreign keys, indexes, and sequence defaults in its
        // catalog functions; views and macros are late-bound, so their
        // source text is matched like SQLite's. `duckdb_dependencies()` is
        // deliberately not used: its OIDs join to nothing stable.
        ConnectionKind::DuckDbFile => {
            let name = sql_literal(name);
            let schema_value = duckdb_schema_value(schema);
            Some(format!(
                r#"SELECT "Direction", "Object", "Type" FROM (
  SELECT 'Uses' AS "Direction", schema_name || '.' || referenced_table AS "Object",
         'FOREIGN KEY ' || COALESCE(constraint_name, constraint_type || '_' || CAST(constraint_index AS VARCHAR)) AS "Type"
  FROM duckdb_constraints()
  WHERE database_name = current_database() AND schema_name = {schema_value}
    AND lower(table_name) = lower('{name}') AND constraint_type = 'FOREIGN KEY'
  UNION
  SELECT 'Used by', schema_name || '.' || table_name,
         'FOREIGN KEY ' || COALESCE(constraint_name, constraint_type || '_' || CAST(constraint_index AS VARCHAR))
  FROM duckdb_constraints()
  WHERE database_name = current_database() AND schema_name = {schema_value}
    AND lower(referenced_table) = lower('{name}') AND constraint_type = 'FOREIGN KEY'
  UNION
  SELECT 'Used by', schema_name || '.' || index_name, 'INDEX'
  FROM duckdb_indexes()
  WHERE database_name = current_database() AND schema_name = {schema_value}
    AND lower(table_name) = lower('{name}')
  UNION
  SELECT 'Uses', regexp_extract(column_default, 'nextval\(''([^'']+)''\)', 1), 'SEQUENCE'
  FROM duckdb_columns()
  WHERE database_name = current_database() AND schema_name = {schema_value}
    AND lower(table_name) = lower('{name}') AND column_default LIKE 'nextval(%'
  UNION
  SELECT 'Uses', t.schema_name || '.' || t.table_name, 'TABLE SQL DEFINITION MATCH'
  FROM duckdb_views() v
  JOIN duckdb_tables() t ON t.database_name = v.database_name AND NOT t.internal
  WHERE v.database_name = current_database() AND v.schema_name = {schema_value}
    AND lower(v.view_name) = lower('{name}')
    AND instr(lower(v.sql), lower(t.table_name)) > 0
  UNION
  SELECT 'Used by', schema_name || '.' || view_name, 'VIEW SQL DEFINITION MATCH'
  FROM duckdb_views()
  WHERE database_name = current_database() AND NOT internal
    AND lower(view_name) <> lower('{name}') AND instr(lower(sql), lower('{name}')) > 0
  UNION
  SELECT 'Used by', schema_name || '.' || function_name, 'MACRO SQL DEFINITION MATCH'
  FROM duckdb_functions()
  WHERE database_name = current_database() AND NOT internal
    AND function_type IN ('macro', 'table_macro')
    AND instr(lower(macro_definition), lower('{name}')) > 0
) dep ORDER BY "Direction", "Object" LIMIT {TOOL_QUERY_ROW_CAP}"#
            ))
        }
        ConnectionKind::ClickHouseHttp => {
            let db = clickhouse_database_value(schema);
            let name = clickhouse_literal(name);
            Some(format!(
                r#"SELECT Direction, Object, Type FROM (
  SELECT 'Used by' AS Direction, concat('`', d.database, '`.`', d.name, '`') AS Object,
         multiIf(d.engine = 'MaterializedView', 'MATERIALIZED VIEW', d.engine = 'Dictionary', 'DICTIONARY', d.engine IN ('View', 'LiveView', 'WindowView'), 'VIEW', 'TABLE') AS Type
  FROM system.tables AS d
  WHERE (d.database, d.name) IN (
    SELECT dep_database, dep_table FROM system.tables
    ARRAY JOIN dependencies_database AS dep_database, dependencies_table AS dep_table
    WHERE database = {db} AND name = '{name}')
  UNION ALL
  SELECT 'Uses', concat('`', src_database, '`.`', src_table, '`'), 'TABLE'
  FROM system.tables
  ARRAY JOIN loading_dependencies_database AS src_database, loading_dependencies_table AS src_table
  WHERE database = {db} AND name = '{name}'
  UNION ALL
  SELECT 'Used by', concat('`', v.database, '`.`', v.name, '`'),
         if(v.engine = 'MaterializedView', 'MATERIALIZED VIEW', 'VIEW') || ' DEFINITION MATCH'
  FROM system.tables AS v
  WHERE v.engine IN ('View', 'LiveView', 'WindowView', 'MaterializedView')
    AND NOT (v.database = {db} AND v.name = '{name}')
    AND positionCaseInsensitive(v.as_select, '{name}') > 0
  UNION ALL
  SELECT 'Uses', concat('`', t.database, '`.`', t.name, '`'), 'TABLE DEFINITION MATCH'
  FROM system.tables AS t
  WHERE t.database = {db} AND t.name <> '{name}' AND t.engine NOT IN ('View', 'LiveView', 'WindowView', 'MaterializedView')
    AND positionCaseInsensitive((SELECT as_select FROM system.tables WHERE database = {db} AND name = '{name}'), t.name) > 0
  UNION ALL
  SELECT 'Used by', concat('`', r.database, '`.`', r.view, '`'), 'REFRESHABLE MATERIALIZED VIEW'
  FROM system.view_refreshes AS r
  WHERE (r.database, r.view) IN (
    SELECT dep_database, dep_table FROM system.tables
    ARRAY JOIN dependencies_database AS dep_database, dependencies_table AS dep_table
    WHERE database = {db} AND name = '{name}')
) AS dep ORDER BY Direction, Object LIMIT {TOOL_QUERY_ROW_CAP}"#
            ))
        }
        ConnectionKind::OracleNet => {
            Some(sqlly_services::admin::oracle_dependencies::dependencies_sql(schema, name))
        }
        _ => None,
    }
}

/// The schema literal for DuckDB catalog filters: the given schema, or the
/// connection's current schema when the caller has none.
fn duckdb_schema_value(schema: &str) -> String {
    if schema.is_empty() {
        "current_schema()".to_string()
    } else {
        format!("'{}'", sql_literal(schema))
    }
}

/// Engine-dispatched textual search for the object name across stored module
/// source, three columns: Object, Type, Preview (first 200 chars). `None`
/// when the engine has no server-side source catalog wired yet.
pub fn usages_sql(db: DatabaseType, schema: &str, name: &str) -> Option<String> {
    match db.connection_kind() {
        ConnectionKind::Tds => {
            let pattern = tds_like_fragment(name);
            Some(format!(
                "SELECT QUOTENAME(SCHEMA_NAME(o.schema_id)) + N'.' + QUOTENAME(o.name) AS [Object],\n\
                 \x20      o.type_desc AS [Type],\n\
                 \x20      LEFT(m.definition, 200) AS [Preview]\n\
                 FROM sys.sql_modules m\n\
                 JOIN sys.objects o ON o.object_id = m.object_id\n\
                 WHERE m.definition LIKE N'%{pattern}%' ESCAPE '\\'\n\
                 ORDER BY [Object];"
            ))
        }
        ConnectionKind::PostgresWire => {
            let pattern = pg_like_fragment(name);
            Some(format!(
                r#"SELECT "Object", "Type", "Preview" FROM (
  SELECT v.schemaname || '.' || v.viewname AS "Object",
         'VIEW' AS "Type",
         left(v.definition, 200) AS "Preview"
  FROM pg_catalog.pg_views v
  WHERE v.schemaname NOT IN ('pg_catalog', 'information_schema')
    AND v.definition LIKE '%{pattern}%'
  UNION ALL
  SELECT m.schemaname || '.' || m.matviewname,
         'MATERIALIZED VIEW',
         left(m.definition, 200)
  FROM pg_catalog.pg_matviews m
  WHERE m.schemaname NOT IN ('pg_catalog', 'information_schema')
    AND m.definition LIKE '%{pattern}%'
  UNION ALL
  SELECT n.nspname || '.' || p.proname,
         CASE p.prokind WHEN 'p' THEN 'PROCEDURE' WHEN 'a' THEN 'AGGREGATE'
              ELSE 'FUNCTION' END,
         left(p.prosrc, 200)
  FROM pg_catalog.pg_proc p
  JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
  WHERE n.nspname NOT IN ('pg_catalog', 'information_schema')
    AND p.prosrc LIKE '%{pattern}%'
) u ORDER BY "Object""#
            ))
        }
        ConnectionKind::MySqlWire => {
            let schema_value = mysql_schema_value(schema);
            let name = mysql_literal(name);
            Some(format!(
                r#"SELECT Object, Type, Preview FROM (
  SELECT CONCAT('`', v.table_schema, '`.`', v.table_name, '`') AS Object,
         'VIEW' AS Type, LEFT(v.view_definition, 200) AS Preview
  FROM information_schema.views v
  WHERE v.table_schema = {schema_value}
    AND LOCATE(LOWER('{name}'), LOWER(COALESCE(v.view_definition, ''))) > 0
  UNION ALL
  SELECT CONCAT('`', r.routine_schema, '`.`', r.routine_name, '`'),
         r.routine_type, LEFT(r.routine_definition, 200)
  FROM information_schema.routines r
  WHERE r.routine_schema = {schema_value}
    AND LOCATE(LOWER('{name}'), LOWER(COALESCE(r.routine_definition, ''))) > 0
  UNION ALL
  SELECT CONCAT('`', tr.trigger_schema, '`.`', tr.trigger_name, '`'),
         'TRIGGER', LEFT(tr.action_statement, 200)
  FROM information_schema.triggers tr
  WHERE tr.trigger_schema = {schema_value}
    AND LOCATE(LOWER('{name}'), LOWER(COALESCE(tr.action_statement, ''))) > 0
  UNION ALL
  SELECT CONCAT('`', e.event_schema, '`.`', e.event_name, '`'),
         'EVENT', LEFT(e.event_definition, 200)
  FROM information_schema.events e
  WHERE e.event_schema = {schema_value}
    AND LOCATE(LOWER('{name}'), LOWER(COALESCE(e.event_definition, ''))) > 0
) usage_rows ORDER BY Object LIMIT {TOOL_QUERY_ROW_CAP}"#
            ))
        }
        ConnectionKind::SqliteFile | ConnectionKind::HranaHttp => {
            let name = sql_literal(name);
            Some(format!(
                "SELECT name AS Object, upper(type) AS Type, substr(sql, 1, 200) AS Preview\n\
                 FROM sqlite_schema\n\
                 WHERE sql IS NOT NULL AND name <> '{name}'\n\
                 AND instr(lower(sql), lower('{name}')) > 0\n\
                 ORDER BY name LIMIT {TOOL_QUERY_ROW_CAP}"
            ))
        }
        ConnectionKind::DuckDbFile => {
            let name = sql_literal(name);
            Some(format!(
                r#"SELECT "Object", "Type", "Preview" FROM (
  SELECT schema_name || '.' || view_name AS "Object", 'VIEW' AS "Type", left(sql, 200) AS "Preview"
  FROM duckdb_views()
  WHERE database_name = current_database() AND NOT internal
    AND lower(view_name) <> lower('{name}') AND instr(lower(sql), lower('{name}')) > 0
  UNION ALL
  SELECT schema_name || '.' || function_name, 'MACRO', left(macro_definition, 200)
  FROM duckdb_functions()
  WHERE database_name = current_database() AND NOT internal
    AND function_type IN ('macro', 'table_macro')
    AND instr(lower(macro_definition), lower('{name}')) > 0
  UNION ALL
  SELECT schema_name || '.' || table_name, 'TABLE', left(sql, 200)
  FROM duckdb_tables()
  WHERE database_name = current_database() AND NOT internal
    AND lower(table_name) <> lower('{name}') AND instr(lower(sql), lower('{name}')) > 0
) usage_rows ORDER BY "Object" LIMIT {TOOL_QUERY_ROW_CAP}"#
            ))
        }
        // Stored source in ClickHouse is the CREATE statement of every
        // table, view and dictionary plus user-defined functions.
        ConnectionKind::ClickHouseHttp => {
            let db = clickhouse_database_value(schema);
            let name = clickhouse_literal(name);
            Some(format!(
                r#"SELECT Object, Type, Preview FROM (
  SELECT concat('`', database, '`.`', name, '`') AS Object,
         multiIf(engine = 'MaterializedView', 'MATERIALIZED VIEW', engine = 'Dictionary', 'DICTIONARY', engine IN ('View', 'LiveView', 'WindowView'), 'VIEW', 'TABLE') AS Type,
         substring(create_table_query, 1, 200) AS Preview
  FROM system.tables
  WHERE NOT (database = {db} AND name = '{name}')
    AND positionCaseInsensitive(create_table_query, '{name}') > 0
  UNION ALL
  SELECT name, 'FUNCTION', substring(create_query, 1, 200)
  FROM system.functions
  WHERE origin <> 'System' AND positionCaseInsensitive(create_query, '{name}') > 0
) AS hits ORDER BY Object LIMIT {TOOL_QUERY_ROW_CAP}"#
            ))
        }
        ConnectionKind::OracleNet => Some(sqlly_services::admin::oracle_dependencies::usages_sql(
            schema,
            name,
            TOOL_QUERY_ROW_CAP,
        )),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// The view
// ---------------------------------------------------------------------------

/// Which of the two sections a shared handler acts on.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SectionKind {
    Dependencies,
    Usages,
}

/// Per-section state: the engine's SQL (`None` when unsupported), the load
/// flag, the first error as honest text, and the built grid when rows landed.
#[derive(Default)]
struct SectionState {
    sql: Option<String>,
    loading: bool,
    error: Option<String>,
    grid: Option<Entity<SqllyDataTable>>,
    row_count: usize,
}

impl SectionState {
    fn truncated(&self) -> bool {
        self.row_count >= TOOL_QUERY_ROW_CAP
    }
}

pub struct ObjectDependenciesView {
    context: ToolDialogContext,
    /// Target object name, unqualified (used in the usages empty-state).
    name: String,
    /// Kind label the explorer opened this for ("Table", "View", …).
    kind_label: String,
    /// Display-qualified name (`schema.name`, or just `name`).
    qualified: String,
    /// Capability reason when this engine has no dependency metadata.
    unsupported_reason: Option<String>,
    completeness_note: String,
    permission_note: String,
    deps: SectionState,
    usages: SectionState,
    refresh_generation: u64,
    focus_handle: FocusHandle,
    frame: DialogFrame,
}

impl EventEmitter<ToolDialogEvent> for ObjectDependenciesView {}

impl ObjectDependenciesView {
    pub fn new(
        context: ToolDialogContext,
        schema: String,
        name: String,
        kind_label: &str,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        let db = context.profile.database_type;
        let qualified = if schema.is_empty() {
            name.clone()
        } else {
            format!("{schema}.{name}")
        };
        let mut this = Self {
            context,
            name,
            kind_label: kind_label.to_string(),
            qualified,
            unsupported_reason: crate::capabilities::dependencies(db).reason(),
            completeness_note: metadata_note(db),
            permission_note: permission_note(db),
            deps: SectionState::default(),
            usages: SectionState::default(),
            refresh_generation: 0,
            focus_handle: cx.focus_handle(),
            // Fixed frame: the two section grids manage their own scrolling.
            frame: DialogFrame::new(Theme::scaled(860.0), Theme::scaled(560.0)),
        };
        this.deps.sql = dependencies_sql(db, &schema, &this.name);
        this.usages.sql = usages_sql(db, &schema, &this.name);
        this.refresh(cx);
        this
    }

    /// Re-run both section queries. Outcomes from a superseded refresh are
    /// dropped by the generation check in the apply methods.
    fn refresh(&mut self, cx: &mut gpui::Context<Self>) {
        self.refresh_generation += 1;
        let generation = self.refresh_generation;
        if let Some(sql) = self.deps.sql.clone() {
            self.deps.loading = true;
            self.deps.error = None;
            crate::tool_dialog::run_query(&self.context, sql, cx, move |this, outcome, cx| {
                this.apply_dependencies_outcome(generation, outcome, cx);
            });
        }
        if let Some(sql) = self.usages.sql.clone() {
            self.usages.loading = true;
            self.usages.error = None;
            crate::tool_dialog::run_query(&self.context, sql, cx, move |this, outcome, cx| {
                this.apply_usages_outcome(generation, outcome, cx);
            });
        }
        cx.notify();
    }

    fn apply_dependencies_outcome(
        &mut self,
        generation: u64,
        outcome: ToolQueryOutcome,
        cx: &mut gpui::Context<Self>,
    ) {
        if self.refresh_generation != generation {
            return;
        }
        Self::apply_outcome(&mut self.deps, outcome, cx);
        cx.notify();
    }

    fn apply_usages_outcome(
        &mut self,
        generation: u64,
        outcome: ToolQueryOutcome,
        cx: &mut gpui::Context<Self>,
    ) {
        if self.refresh_generation != generation {
            return;
        }
        Self::apply_outcome(&mut self.usages, outcome, cx);
        cx.notify();
    }

    fn apply_outcome(section: &mut SectionState, outcome: ToolQueryOutcome, cx: &mut gpui::App) {
        section.loading = false;
        if let Some(err) = outcome.error {
            section.error = Some(err);
            section.grid = None;
            section.row_count = 0;
            return;
        }
        let set = outcome.sets.into_iter().next().unwrap_or_default();
        section.error = None;
        section.row_count = set.rows.len();
        section.grid = if set.rows.is_empty() {
            None
        } else {
            crate::tool_dialog::build_result_grid(&set, cx)
        };
    }

    /// Script the section's SQL into an editor tab for the user to run or
    /// refine, then dismiss (mirrors Query Store's open-selected flow).
    fn open_section(&mut self, kind: SectionKind, cx: &mut gpui::Context<Self>) {
        let sql = match kind {
            SectionKind::Dependencies => self.deps.sql.clone(),
            SectionKind::Usages => self.usages.sql.clone(),
        };
        match sql {
            Some(sql) => {
                cx.emit(ToolDialogEvent::ScriptToEditor { sql });
                cx.emit(ToolDialogEvent::Close);
            }
            None => cx.emit(ToolDialogEvent::Status {
                level: crate::app::StatusLevel::Info,
                message: crate::localization::text("explorer.object_dependencies.no_section_sql"),
            }),
        }
    }

    fn close(&mut self, cx: &mut gpui::Context<Self>) {
        cx.emit(ToolDialogEvent::Close);
    }

    fn render_section(&self, kind: SectionKind, cx: &mut gpui::Context<Self>) -> gpui::AnyElement {
        let s = Theme::scaled;
        let (section, label, open_id) = match kind {
            SectionKind::Dependencies => (
                &self.deps,
                "explorer.object_dependencies.dependencies_section",
                "od-deps-open",
            ),
            SectionKind::Usages => (
                &self.usages,
                "explorer.object_dependencies.usages_section",
                "od-usages-open",
            ),
        };
        let empty_text = match kind {
            SectionKind::Dependencies => crate::localization::format(
                "explorer.object_dependencies.dependencies_empty",
                &[("object", &self.qualified)],
            ),
            SectionKind::Usages => crate::localization::format(
                "explorer.object_dependencies.usages_empty",
                &[("object", &self.name)],
            ),
        };

        let content: gpui::AnyElement = if section.sql.is_none() {
            div()
                .p(px(s(12.0)))
                .text_color(Theme::text_dim())
                .text_size(px(Theme::font_size_ratio(0.87)))
                .whitespace_normal()
                .child(self.unsupported_reason.clone().unwrap_or_else(|| {
                    crate::localization::text("explorer.object_dependencies.unsupported")
                }))
                .into_any_element()
        } else if section.loading {
            div()
                .p(px(s(12.0)))
                .text_color(Theme::text_dim())
                .child(crate::localization::text(
                    if kind == SectionKind::Dependencies {
                        "explorer.object_dependencies.dependencies_loading"
                    } else {
                        "explorer.object_dependencies.usages_loading"
                    },
                ))
                .into_any_element()
        } else if let Some(error) = &section.error {
            div()
                .p(px(s(12.0)))
                .text_color(Theme::error())
                .text_size(px(Theme::font_size_ratio(0.87)))
                .whitespace_normal()
                .child(crate::localization::format(
                    "explorer.object_dependencies.query_failed",
                    &[
                        ("section", &crate::localization::text(label)),
                        ("error", error),
                        ("permission", &self.permission_note),
                    ],
                ))
                .into_any_element()
        } else if let Some(grid) = &section.grid {
            div().size_full().child(grid.clone()).into_any_element()
        } else {
            div()
                .p(px(s(12.0)))
                .text_color(Theme::text_dim())
                .child(empty_text)
                .into_any_element()
        };

        let mut header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(s(8.0)))
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.93)))
                    .text_color(Theme::text())
                    .child(crate::localization::text(label)),
            );
        if section.truncated() {
            header = header.child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.74)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::format(
                        "explorer.object_dependencies.truncated",
                        &[("count", &TOOL_QUERY_ROW_CAP.to_string())],
                    )),
            );
        }
        header = header.child(div().flex_1()).child(
            Button::new(open_id)
                .small()
                .label(crate::localization::text(
                    "explorer.object_dependencies.open_query_button",
                ))
                .disabled(section.sql.is_none())
                .on_click(cx.listener(move |this, _, _window, cx| this.open_section(kind, cx))),
        );

        div()
            .flex_1()
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .gap(px(s(4.0)))
            .child(header)
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .border_1()
                    .border_color(Theme::border())
                    .rounded(px(s(4.0)))
                    .child(content),
            )
            .into_any_element()
    }
}

impl gpui::Focusable for ObjectDependenciesView {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ObjectDependenciesView {
    fn render(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let loading = self.deps.loading || self.usages.loading;
        let nothing_to_run = self.deps.sql.is_none() && self.usages.sql.is_none();

        // Bare body: the two embedded grids manage their own scrolling.
        let body = div()
            .flex_1()
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .gap(px(s(8.0)))
            .px(px(s(14.0)))
            .py(px(s(10.0)))
            .child(self.render_section(SectionKind::Dependencies, cx))
            .child(self.render_section(SectionKind::Usages, cx))
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.74)))
                    .text_color(Theme::text_dim())
                    .whitespace_normal()
                    .child(crate::localization::format(
                        "explorer.object_dependencies.footer",
                        &[
                            ("note", &self.completeness_note),
                            ("cap", &TOOL_QUERY_ROW_CAP.to_string()),
                        ],
                    )),
            );

        let focus_handle = self.focus_handle.clone();
        Dialog::new("object-dependencies-pane", |this: &mut Self| {
            &mut this.frame
        })
        .title(crate::localization::format(
            "explorer.object_dependencies.title",
            &[("kind", &self.kind_label), ("object", &self.qualified)],
        ))
        .easy_close(true)
        .key_context("ObjectDependenciesView")
        .track_focus(&focus_handle)
        .bare_body()
        .on_close(|this, _window, cx| this.close(cx))
        .child(body)
        .leading_button(
            Button::new("od-refresh")
                .label(crate::localization::text(
                    "explorer.object_dependencies.refresh_button",
                ))
                .disabled(loading || nothing_to_run)
                .on_click(cx.listener(|this, _, _window, cx| this.refresh(cx))),
        )
        .build(self, window, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// MySQL reads `\` as an escape inside '…' (default sql_mode), so a
    /// catalog name ending in a backslash must not swallow the closing quote
    /// of these auto-run catalog queries.
    #[test]
    fn mysql_dependency_queries_escape_backslashes_in_names() {
        let hostile = r"x\' OR 1=1 -- ";
        for sql in [
            dependencies_sql(DatabaseType::MySql, r"db\", hostile).unwrap(),
            usages_sql(DatabaseType::MariaDb, r"db\", hostile).unwrap(),
        ] {
            assert!(sql.contains(r"'db\\'"), "{sql}");
            assert!(sql.contains(r"x\\'' OR 1=1 -- "), "{sql}");
            assert!(!sql.contains(r"'x\''"), "{sql}");
        }
    }
    use crate::tool_dialog::ToolQuerySet;
    use sqlly_core::{CellValue, ConnectionProfile, ResultColumn, SecretProvider, SecretRef};
    use sqlly_services::Services;

    fn assert_quotes_balanced(sql: &str) {
        assert!(
            sql.matches('\'').count().is_multiple_of(2),
            "unbalanced single quotes in:\n{sql}"
        );
    }

    // -- dependencies_sql -----------------------------------------------------

    #[test]
    fn tds_dependencies_mentions_catalog_and_both_directions() {
        let sql = dependencies_sql(DatabaseType::SqlServer, "dbo", "Orders").unwrap();
        assert!(sql.contains("sys.sql_expression_dependencies"));
        assert!(sql.contains("'Uses'"));
        assert!(sql.contains("'Used by'"));
        assert!(sql.contains("referencing_id"));
        assert!(sql.contains("referenced_id"));
        assert!(sql.contains("OBJECT_ID(N'[dbo].[Orders]')"));
        assert_quotes_balanced(&sql);
    }

    #[test]
    fn tds_dependencies_without_schema_targets_bare_name() {
        let sql = dependencies_sql(DatabaseType::AzureSqlDatabase, "", "Orders").unwrap();
        assert!(sql.contains("OBJECT_ID(N'[Orders]')"));
        assert!(
            !sql.contains("].[Orders]"),
            "no schema part expected in the target literal"
        );
    }

    #[test]
    fn tds_dependencies_hostile_name_stays_inside_literals() {
        let sql = dependencies_sql(DatabaseType::SqlServer, "dbo", "O'Brien]x").unwrap();
        // The embedded quote is doubled and the bracket doubled — the raw
        // spellings never appear, so neither can escape its literal.
        assert!(!sql.contains("O'Brien"));
        assert!(sql.contains("OBJECT_ID(N'[dbo].[O''Brien]]x]')"));
        assert_quotes_balanced(&sql);
    }

    #[test]
    fn pg_dependencies_reads_pg_catalog_and_filters_system_schemas() {
        let sql = dependencies_sql(DatabaseType::Postgres, "public", "orders").unwrap();
        assert!(sql.contains("pg_depend"));
        assert!(sql.contains("pg_constraint"));
        assert!(sql.contains("pg_rewrite"));
        assert!(sql.contains("NOT IN ('pg_catalog', 'information_schema')"));
        assert!(sql.contains(r#"to_regclass('"public"."orders"')"#));
        assert!(sql.contains("'Uses'"));
        assert!(sql.contains("'Used by'"));
        assert_quotes_balanced(&sql);
    }

    #[test]
    fn pg_dependencies_hostile_name_stays_inside_literals() {
        let sql = dependencies_sql(DatabaseType::Postgres, "public", "O'Brien]x").unwrap();
        assert!(!sql.contains("O'Brien"));
        assert!(sql.contains(r#"to_regclass('"public"."O''Brien]x"')"#));
        assert_quotes_balanced(&sql);
    }

    #[test]
    fn mysql_dependencies_use_fk_and_visible_definition_metadata_with_a_bound() {
        let sql = dependencies_sql(DatabaseType::MySql, "sales", "orders").unwrap();
        assert!(sql.contains("information_schema.key_column_usage"));
        assert!(sql.contains("information_schema.views"));
        assert!(sql.contains("information_schema.routines"));
        assert!(sql.contains("information_schema.triggers"));
        assert!(sql.contains("information_schema.events"));
        assert!(sql.contains("table_schema = 'sales'"));
        assert!(sql.contains("'Uses'"));
        assert!(sql.contains("'Used by'"));
        assert!(sql.contains(&format!("LIMIT {TOOL_QUERY_ROW_CAP}")));
        assert_quotes_balanced(&sql);
    }

    #[test]
    fn mysql_dependencies_default_to_current_database_and_escape_literals() {
        let sql = dependencies_sql(DatabaseType::MariaDb, "", "O'Brien").unwrap();
        assert!(sql.contains("table_schema = DATABASE()"));
        assert!(!sql.contains("O'Brien"));
        assert!(sql.contains("O''Brien"));
        assert_quotes_balanced(&sql);
    }

    #[test]
    fn sqlite_dependencies_use_foreign_keys_and_schema_sql_with_a_bound() {
        let sql = dependencies_sql(DatabaseType::Sqlite, "main", "orders").unwrap();
        assert!(sql.contains("pragma_foreign_key_list"));
        assert!(sql.contains("sqlite_schema"));
        assert!(sql.contains("SQL DEFINITION MATCH"));
        assert!(sql.contains("'Uses'"));
        assert!(sql.contains("'Used by'"));
        assert!(sql.contains(&format!("LIMIT {TOOL_QUERY_ROW_CAP}")));
        let hostile = dependencies_sql(DatabaseType::Sqlite, "main", "O'Brien").unwrap();
        assert!(!hostile.contains("O'Brien"));
        assert_quotes_balanced(&hostile);
    }

    #[test]
    fn duckdb_dependencies_read_catalog_functions_and_match_source() {
        let sql = dependencies_sql(DatabaseType::DuckDb, "main", "O'Brien").unwrap();
        assert!(sql.contains("duckdb_constraints()"), "{sql}");
        assert!(sql.contains("duckdb_indexes()"), "{sql}");
        assert!(sql.contains("duckdb_columns()"), "{sql}");
        assert!(sql.contains("duckdb_views()"), "{sql}");
        assert!(sql.contains("duckdb_functions()"), "{sql}");
        assert!(!sql.contains("duckdb_dependencies()"), "{sql}");
        assert!(sql.contains("lower('O''Brien')"), "{sql}");
        assert!(sql.contains("schema_name = 'main'"), "{sql}");
        assert!(!sql.contains("O'Brien"));
        assert_quotes_balanced(&sql);
        // No schema: the connection's current schema scopes the catalog.
        let sql = dependencies_sql(DatabaseType::DuckDb, "", "t").unwrap();
        assert!(sql.contains("schema_name = current_schema()"), "{sql}");
        let usages = usages_sql(DatabaseType::DuckDb, "main", "o'r").unwrap();
        assert!(usages.contains("duckdb_views()"), "{usages}");
        assert!(usages.contains("duckdb_functions()"), "{usages}");
        assert!(usages.contains("left(sql, 200)"), "{usages}");
        assert!(usages.contains("lower('o''r')"), "{usages}");
        assert_quotes_balanced(&usages);
        assert!(metadata_note(DatabaseType::DuckDb).contains("DuckDB"));
        assert!(permission_note(DatabaseType::DuckDb).contains("DuckDB"));
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    async fn duckdb_dependency_queries_run_against_a_real_database() {
        use sqlly_core::{CellValue, ConnectionProfile, SecretProvider, SecretRef};
        let dir = tempfile::tempdir().unwrap();
        let mut profile = ConnectionProfile::sql_login(
            "deps",
            dir.path().join("deps.duckdb").to_string_lossy().as_ref(),
            "",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: String::new(),
            },
        );
        profile.database_type = DatabaseType::DuckDb;
        let client = sqlly_engines::duckdb::open_client(&profile).await.unwrap();
        sqlly_engines::duckdb::execute_batch_on_client(
            &client,
            "CREATE SEQUENCE order_seq;\
             CREATE TABLE customer (id INTEGER PRIMARY KEY, name VARCHAR);\
             CREATE TABLE orders (id INTEGER DEFAULT nextval('order_seq') PRIMARY KEY, \
             customer_id INTEGER REFERENCES customer(id));\
             CREATE INDEX ix_orders_customer ON orders(customer_id);\
             CREATE VIEW v_customer_orders AS SELECT c.name, o.id FROM customer c JOIN orders o ON o.customer_id = c.id;\
             CREATE MACRO customer_label(x) AS 'customer ' || x;",
        )
        .await
        .unwrap();
        let rows = |sql: String| {
            let client = &client;
            async move {
                let sets = sqlly_engines::duckdb::query_rows_on_client(client, &sql, &[])
                    .await
                    .unwrap();
                sets.into_iter()
                    .next()
                    .unwrap()
                    .rows
                    .into_iter()
                    .map(|row| {
                        row.into_iter()
                            .map(|cell| match cell {
                                CellValue::String(s) => s,
                                other => format!("{other:?}"),
                            })
                            .collect::<Vec<_>>()
                    })
                    .collect::<Vec<_>>()
            }
        };

        let deps = rows(dependencies_sql(DatabaseType::DuckDb, "main", "orders").unwrap()).await;
        let has = |direction: &str, object: &str, kind_prefix: &str| {
            deps.iter()
                .any(|r| r[0] == direction && r[1] == object && r[2].starts_with(kind_prefix))
        };
        assert!(has("Uses", "main.customer", "FOREIGN KEY"), "{deps:?}");
        assert!(has("Uses", "order_seq", "SEQUENCE"), "{deps:?}");
        assert!(
            has("Used by", "main.ix_orders_customer", "INDEX"),
            "{deps:?}"
        );
        assert!(has("Used by", "main.v_customer_orders", "VIEW"), "{deps:?}");

        let deps = rows(dependencies_sql(DatabaseType::DuckDb, "main", "customer").unwrap()).await;
        assert!(
            deps.iter().any(|r| r[0] == "Used by"
                && r[1] == "main.orders"
                && r[2].starts_with("FOREIGN KEY")),
            "{deps:?}"
        );
        assert!(
            deps.iter().any(|r| r[0] == "Used by"
                && r[1] == "main.customer_label"
                && r[2].starts_with("MACRO")),
            "{deps:?}"
        );

        let deps =
            rows(dependencies_sql(DatabaseType::DuckDb, "main", "v_customer_orders").unwrap())
                .await;
        assert!(has_uses_table(&deps, "main.customer"), "{deps:?}");
        assert!(has_uses_table(&deps, "main.orders"), "{deps:?}");

        let usages = rows(usages_sql(DatabaseType::DuckDb, "main", "customer").unwrap()).await;
        assert!(
            usages
                .iter()
                .any(|r| r[0] == "main.v_customer_orders" && r[1] == "VIEW"),
            "{usages:?}"
        );
        assert!(
            usages
                .iter()
                .any(|r| r[0] == "main.customer_label" && r[1] == "MACRO"),
            "{usages:?}"
        );
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn has_uses_table(deps: &[Vec<String>], object: &str) -> bool {
        deps.iter()
            .any(|r| r[0] == "Uses" && r[1] == object && r[2].starts_with("TABLE"))
    }

    #[test]
    fn dependencies_cover_all_native_driver_engines() {
        assert!(dependencies_sql(DatabaseType::Sqlite, "main", "t").is_some());
        assert!(dependencies_sql(DatabaseType::MySql, "db", "t").is_some());
        for db in DatabaseType::ALL {
            let supported = matches!(
                db.connection_kind(),
                ConnectionKind::Tds
                    | ConnectionKind::PostgresWire
                    | ConnectionKind::MySqlWire
                    | ConnectionKind::SqliteFile
                    | ConnectionKind::DuckDbFile
                    | ConnectionKind::HranaHttp
                    | ConnectionKind::ClickHouseHttp
                    | ConnectionKind::OracleNet
            );
            assert_eq!(
                dependencies_sql(db, "s", "n").is_some(),
                supported,
                "{db:?}"
            );
        }
    }

    #[test]
    fn clickhouse_dependencies_read_system_tables_and_escape_names() {
        let sql = dependencies_sql(DatabaseType::ClickHouse, "shop", "O'Brien\\x").unwrap();
        assert!(sql.contains("dependencies_database"), "{sql}");
        assert!(sql.contains("loading_dependencies_database"), "{sql}");
        assert!(sql.contains("system.view_refreshes"), "{sql}");
        assert!(sql.contains("positionCaseInsensitive(v.as_select"), "{sql}");
        assert!(sql.contains("database = 'shop'"), "{sql}");
        assert!(!sql.contains("O'Brien"), "{sql}");
        assert!(sql.contains("O''Brien\\\\x"), "{sql}");
        assert!(sql.contains(&format!("LIMIT {TOOL_QUERY_ROW_CAP}")));
        let current = dependencies_sql(DatabaseType::ClickHouse, "", "t").unwrap();
        assert!(
            current.contains("database = currentDatabase()"),
            "{current}"
        );
        let usages = usages_sql(DatabaseType::ClickHouse, "shop", "orders").unwrap();
        assert!(usages.contains("create_table_query"), "{usages}");
        assert!(usages.contains("system.functions"), "{usages}");
        assert!(usages.contains("origin <> 'System'"), "{usages}");
        assert!(usages.contains(&format!("LIMIT {TOOL_QUERY_ROW_CAP}")));
        assert!(metadata_note(DatabaseType::ClickHouse).contains("materialized views"));
        assert!(permission_note(DatabaseType::ClickHouse).contains("SHOW"));
    }

    // -- usages_sql -----------------------------------------------------------

    #[test]
    fn tds_usages_searches_modules_with_escaped_wildcards() {
        let sql = usages_sql(DatabaseType::SqlServer, "dbo", "100%_done[x]").unwrap();
        assert!(sql.contains("sys.sql_modules"));
        assert!(sql.contains("sys.objects"));
        assert!(sql.contains("LEFT(m.definition, 200)"));
        assert!(sql.contains("ESCAPE '\\'"));
        assert!(sql.contains("\\%"));
        assert!(sql.contains("\\_"));
        assert!(sql.contains("\\["));
        assert!(sql.contains("\\]"));
        assert_quotes_balanced(&sql);
    }

    #[test]
    fn tds_usages_hostile_name_stays_inside_the_pattern() {
        let sql = usages_sql(DatabaseType::SqlServer, "dbo", "O'Brien]x").unwrap();
        assert!(!sql.contains("O'Brien"));
        assert!(sql.contains("LIKE N'%O''Brien\\]x%' ESCAPE '\\'"));
        assert_quotes_balanced(&sql);
    }

    #[test]
    fn pg_usages_covers_views_matviews_and_procs() {
        let sql = usages_sql(DatabaseType::Postgres, "public", "orders").unwrap();
        assert!(sql.contains("pg_views"));
        assert!(sql.contains("pg_matviews"));
        assert!(sql.contains("pg_proc"));
        assert!(sql.contains("prosrc"));
        assert!(sql.contains("left(v.definition, 200)"));
        assert!(sql.contains("NOT IN ('pg_catalog', 'information_schema')"));
        assert_quotes_balanced(&sql);
    }

    #[test]
    fn pg_usages_escapes_wildcards_and_quotes() {
        let sql = usages_sql(DatabaseType::Postgres, "public", "o'r%d_r").unwrap();
        assert!(sql.contains("o''r\\%d\\_r"));
        assert_quotes_balanced(&sql);
    }

    #[test]
    fn mysql_usages_search_visible_stored_source_and_stays_bounded() {
        let sql = usages_sql(DatabaseType::MySql, "sales", "O'Brien").unwrap();
        assert!(sql.contains("information_schema.views"));
        assert!(sql.contains("information_schema.routines"));
        assert!(sql.contains("information_schema.triggers"));
        assert!(sql.contains("information_schema.events"));
        assert!(sql.contains("O''Brien"));
        assert!(sql.contains(&format!("LIMIT {TOOL_QUERY_ROW_CAP}")));
        assert_quotes_balanced(&sql);
    }

    #[test]
    fn sqlite_usages_search_schema_sql_and_stay_bounded() {
        let sql = usages_sql(DatabaseType::Sqlite, "main", "orders").unwrap();
        assert!(sql.contains("FROM sqlite_schema"));
        assert!(sql.contains("substr(sql, 1, 200)"));
        assert!(sql.contains(&format!("LIMIT {TOOL_QUERY_ROW_CAP}")));
    }

    #[test]
    fn usages_cover_native_engines_and_reject_unwired_engines() {
        assert!(usages_sql(DatabaseType::Sqlite, "main", "t").is_some());
        assert!(usages_sql(DatabaseType::MySql, "db", "t").is_some());
        assert!(usages_sql(DatabaseType::Oracle, "s", "t").is_some());
        assert!(usages_sql(DatabaseType::DuckDb, "s", "t").is_some());
    }

    /// Oracle delegates to the service-layer generators (live-tested there)
    /// and carries its own completeness and permission notes.
    #[test]
    fn oracle_dependencies_delegate_to_the_service_generators() {
        let sql = dependencies_sql(DatabaseType::Oracle, "HR", "EMP").unwrap();
        assert!(sql.contains("FROM all_dependencies d"));
        assert!(sql.contains("d.referenced_owner = 'HR' AND d.referenced_name = 'EMP'"));
        let usages = usages_sql(DatabaseType::Oracle, "HR", "emp_v1").unwrap();
        assert!(usages.contains("FROM all_source s"));
        assert!(usages.contains(&format!("FETCH FIRST {TOOL_QUERY_ROW_CAP} ROWS ONLY")));
        assert!(metadata_note(DatabaseType::Oracle).contains("ALL_DEPENDENCIES"));
        assert!(permission_note(DatabaseType::Oracle).contains("ALL_SOURCE"));
    }

    #[test]
    fn unsupported_reason_comes_from_capabilities() {
        // The dialog's empty-state text for unsupported engines is the
        // capability reason; make sure it exists where the SQL is None.
        for db in [DatabaseType::Db2, DatabaseType::Snowflake] {
            assert!(dependencies_sql(db, "s", "n").is_none());
            assert!(crate::capabilities::dependencies(db).reason().is_some());
        }
    }

    #[test]
    fn completeness_and_permission_notes_are_engine_specific() {
        assert!(metadata_note(DatabaseType::MySql).contains("Best effort"));
        assert!(permission_note(DatabaseType::MySql).contains("SHOW VIEW"));
        assert!(metadata_note(DatabaseType::Sqlite).contains("false positives"));
        assert!(permission_note(DatabaseType::Sqlite).contains("PRAGMA"));
    }

    // -- windowed view tests --------------------------------------------------

    /// gpui-component widgets read the library's global theme during render;
    /// windowed tests must install it (idempotently) before opening windows.
    fn ensure_components_initialized(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            if cx.try_global::<gpui_component::Theme>().is_none() {
                gpui_component::init(cx);
            }
        });
    }

    fn test_context(db: DatabaseType) -> ToolDialogContext {
        let secret = SecretRef {
            provider: SecretProvider::InMemory,
            key: "k".to_string(),
        };
        let mut profile = ConnectionProfile::sql_login("test", "host", "sa", secret);
        profile.database_type = db;
        ToolDialogContext::new(profile, Services::new())
    }

    #[gpui::test]
    fn opens_with_honest_unsupported_sections_on_an_unwired_engine(cx: &mut gpui::TestAppContext) {
        ensure_components_initialized(cx);
        let (view, cx) = cx.add_window_view(|_window, cx| {
            ObjectDependenciesView::new(
                test_context(DatabaseType::Db2),
                String::new(),
                "orders".to_string(),
                "Table",
                cx,
            )
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _cx| {
            // No engine SQL → nothing runs, nothing loads, and the capability
            // reason is what both sections render instead of empty grids.
            assert!(view.deps.sql.is_none());
            assert!(view.usages.sql.is_none());
            assert!(!view.deps.loading);
            assert!(!view.usages.loading);
            assert!(view.deps.grid.is_none());
            assert!(view.usages.grid.is_none());
            assert!(view.unsupported_reason.is_some());
            assert_eq!(view.qualified, "orders");
        });
    }

    #[gpui::test]
    fn stale_generation_outcome_is_ignored(cx: &mut gpui::TestAppContext) {
        ensure_components_initialized(cx);
        let (view, cx) = cx.add_window_view(|_window, cx| {
            ObjectDependenciesView::new(
                test_context(DatabaseType::Db2),
                "main".to_string(),
                "orders".to_string(),
                "Table",
                cx,
            )
        });
        view.update(cx, |view, cx| {
            let stale = view.refresh_generation;
            // A newer refresh supersedes the in-flight one…
            view.refresh_generation += 1;
            view.apply_dependencies_outcome(
                stale,
                ToolQueryOutcome {
                    sets: Vec::new(),
                    error: Some("boom".to_string()),
                },
                cx,
            );
            assert!(
                view.deps.error.is_none(),
                "a stale outcome must not land on the section"
            );
            // …while the current generation's outcome does land.
            let current = view.refresh_generation;
            view.apply_dependencies_outcome(
                current,
                ToolQueryOutcome {
                    sets: Vec::new(),
                    error: Some("boom".to_string()),
                },
                cx,
            );
            assert_eq!(view.deps.error.as_deref(), Some("boom"));
            assert!(!view.deps.loading);
        });
    }

    #[gpui::test]
    fn fresh_outcome_with_rows_builds_the_section_grid(cx: &mut gpui::TestAppContext) {
        ensure_components_initialized(cx);
        let (view, cx) = cx.add_window_view(|_window, cx| {
            ObjectDependenciesView::new(
                test_context(DatabaseType::Db2),
                "dbo".to_string(),
                "Orders".to_string(),
                "Table",
                cx,
            )
        });
        view.update(cx, |view, cx| {
            let generation = view.refresh_generation;
            let set = ToolQuerySet {
                columns: vec![
                    ResultColumn {
                        name: "Object".into(),
                        data_type: "text".into(),
                        nullable: false,
                    },
                    ResultColumn {
                        name: "Type".into(),
                        data_type: "text".into(),
                        nullable: false,
                    },
                    ResultColumn {
                        name: "Preview".into(),
                        data_type: "text".into(),
                        nullable: true,
                    },
                ],
                rows: vec![vec![
                    CellValue::String("dbo.OrdersView".into()),
                    CellValue::String("VIEW".into()),
                    CellValue::String("SELECT * FROM dbo.Orders".into()),
                ]],
            };
            view.apply_usages_outcome(
                generation,
                ToolQueryOutcome {
                    sets: vec![set],
                    error: None,
                },
                cx,
            );
            assert!(view.usages.grid.is_some());
            assert_eq!(view.usages.row_count, 1);
            assert!(view.usages.error.is_none());
            assert!(!view.usages.truncated());
        });
    }
}
