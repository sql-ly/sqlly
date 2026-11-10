//! Dialect-aware DDL helpers for the non-SQL Server definition path.
//!
//! [`crate::metadata::ddl`] encodes the SQL Server DDL-reconstruction queries; this
//! module carries their per-engine counterparts used by the definition
//! fetcher when a connection speaks PostgreSQL, MySQL/MariaDB, or SQLite:
//!
//! - Index DDL for a table, one rendered statement per row (PostgreSQL's
//!   `pg_indexes.indexdef`; SQLite's `sqlite_master.sql`). MySQL has no stored
//!   index DDL to read back, so it has no query here.
//! - A generic `CREATE TABLE` builder from the dialect-dispatched
//!   [`crate::metadata::MetadataQuery::Columns`] row shape, quoted per engine, for
//!   tables whose engine stores no verbatim CREATE source (PostgreSQL, MySQL).

use sqlly_core::DatabaseType;

fn lit(value: &str) -> String {
    value.replace('\'', "''")
}

/// PostgreSQL: every index on the table as a complete `CREATE INDEX`
/// statement, one per row.
pub fn pg_table_indexes_ddl_sql(schema: &str, table: &str) -> String {
    format!(
        "SELECT indexdef || ';' FROM pg_catalog.pg_indexes WHERE schemaname = '{}' AND tablename = '{}' ORDER BY indexname",
        lit(schema),
        lit(table)
    )
}

/// PostgreSQL: each index's name and its `CREATE INDEX` text, so indexes the
/// structure model cannot express (expression or descending keys) can be
/// replayed verbatim.
pub fn pg_table_index_definitions_sql(schema: &str, table: &str) -> String {
    format!(
        "SELECT indexname, indexdef || ';' FROM pg_catalog.pg_indexes WHERE schemaname = '{}' AND tablename = '{}' ORDER BY indexname",
        lit(schema),
        lit(table)
    )
}

/// SQLite: the stored CREATE source of every named index on the table.
/// Auto-created indexes (PK/UNIQUE backing) have NULL `sql` and are skipped.
pub fn sqlite_table_indexes_ddl_sql(table: &str) -> String {
    format!(
        "SELECT sql || ';' FROM sqlite_master WHERE type = 'index' AND tbl_name = '{}' AND sql IS NOT NULL ORDER BY name",
        lit(table)
    )
}

/// DuckDB: the stored CREATE source of every index on the table, from the
/// `duckdb_indexes()` catalog function (the stored text already ends in `;`).
pub fn duckdb_table_indexes_ddl_sql(schema: &str, table: &str) -> String {
    format!(
        "SELECT sql FROM duckdb_indexes() WHERE database_name = current_database() AND schema_name = '{}' AND table_name = '{}' AND sql IS NOT NULL ORDER BY index_name",
        lit(schema),
        lit(table)
    )
}

/// DuckDB: one `CREATE TYPE` statement for a user-defined type. ENUMs carry
/// their labels in the catalog; every other kind (STRUCT, LIST, and other
/// aliases) is described by its logical type only, which the catalog does
/// not store in full, so those come back as a commented placeholder.
pub fn duckdb_type_ddl_sql(schema: &str, name: &str) -> String {
    format!(
        "SELECT CASE WHEN logical_type = 'ENUM' THEN 'CREATE TYPE ' || type_name || ' AS ENUM (' || list_aggregate(list_transform(labels, x -> '''' || replace(x, '''', '''''') || ''''), 'string_agg', ', ') || ');' ELSE '-- ' || type_name || ' is a ' || logical_type || ' alias; DuckDB does not store its full definition' END FROM duckdb_types() WHERE database_name = current_database() AND schema_name = '{}' AND type_name = '{}' AND NOT internal",
        lit(schema),
        lit(name)
    )
}

/// A column as returned by the dialect-dispatched
/// [`crate::metadata::MetadataQuery::Columns`] positional shape: name, type_name,
/// max_length, precision, scale, is_nullable, is_identity, is_computed.
#[derive(Debug, Clone, Default)]
pub struct GenericDdlColumn {
    pub name: String,
    pub type_name: String,
    pub max_length: Option<i64>,
    pub precision: Option<i64>,
    pub scale: Option<i64>,
    pub nullable: bool,
    pub is_identity: bool,
    pub is_computed: bool,
}

/// Render one column's type with length/precision suffixes only where the
/// base type carries them (the dialect Columns queries return bare type names
/// with the parameters split out; SQLite returns the declared type verbatim
/// with `max_length = -1`, so it never gains a suffix here).
/// Render one column's type with length/precision suffixes (public for snapshot loader).
pub fn render_generic_column_type(column: &GenericDdlColumn) -> String {
    render_generic_type(column)
}

fn render_generic_type(column: &GenericDdlColumn) -> String {
    let base = column.type_name.trim();
    let lower = base.to_ascii_lowercase();
    let max_length = column.max_length.unwrap_or(-1);
    let precision = column.precision.unwrap_or(0);
    let scale = column.scale.unwrap_or(0);
    if (lower.contains("char") || lower.contains("binary") || lower == "bit varying")
        && max_length > 0
    {
        return format!("{base}({max_length})");
    }
    if (lower == "numeric" || lower == "decimal") && precision > 0 {
        return format!("{base}({precision}, {scale})");
    }
    base.to_string()
}

/// Build a `CREATE TABLE` statement from the generic column shape, quoted and
/// decorated per `db`. Identity columns render the engine's native form
/// (`GENERATED BY DEFAULT AS IDENTITY` on PostgreSQL, `AUTO_INCREMENT` on
/// MySQL); computed columns are marked with a trailing comment because their
/// expressions are not part of the generic shape.
pub fn build_generic_create_table_ddl(
    db: DatabaseType,
    schema: Option<&str>,
    name: &str,
    columns: &[GenericDdlColumn],
) -> String {
    let table = match schema {
        Some(s) if !s.is_empty() => {
            format!("{}.{}", db.quote_identifier(s), db.quote_identifier(name))
        }
        _ => db.quote_identifier(name),
    };
    let mut body_lines = Vec::with_capacity(columns.len());
    for column in columns {
        let mut line = format!(
            "    {} {}",
            db.quote_identifier(&column.name),
            render_generic_column_type(column)
        );
        if column.is_identity {
            match db.family() {
                sqlly_core::Dialect::Postgres => line.push_str(" GENERATED BY DEFAULT AS IDENTITY"),
                sqlly_core::Dialect::MySql => line.push_str(" AUTO_INCREMENT"),
                _ => {}
            }
        }
        line.push_str(if column.nullable {
            " NULL"
        } else {
            " NOT NULL"
        });
        if column.is_computed {
            line.push_str(" /* computed: expression not stored in catalog shape */");
        }
        body_lines.push(line);
    }
    format!("CREATE TABLE {table} (\n{}\n)", body_lines.join(",\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, type_name: &str) -> GenericDdlColumn {
        GenericDdlColumn {
            name: name.into(),
            type_name: type_name.into(),
            nullable: true,
            ..GenericDdlColumn::default()
        }
    }

    #[test]
    fn pg_index_sql_escapes_literals() {
        let sql = pg_table_indexes_ddl_sql("pub'lic", "users");
        assert!(sql.contains("pub''lic"));
        assert!(sql.contains("pg_indexes"));
    }

    #[test]
    fn sqlite_index_sql_skips_unnamed() {
        let sql = sqlite_table_indexes_ddl_sql("users");
        assert!(sql.contains("sql IS NOT NULL"));
    }

    #[test]
    fn duckdb_index_and_type_sql_escape_literals() {
        let sql = duckdb_table_indexes_ddl_sql("ma'in", "users");
        assert!(sql.contains("duckdb_indexes()"));
        assert!(sql.contains("schema_name = 'ma''in'"));
        assert!(sql.contains("table_name = 'users'"));
        let sql = duckdb_type_ddl_sql("main", "mo'od");
        assert!(sql.contains("duckdb_types()"));
        assert!(sql.contains("type_name = 'mo''od'"));
        assert!(sql.contains("AS ENUM ("));
        assert!(sql.matches('\'').count().is_multiple_of(2), "{sql}");
    }

    #[test]
    fn generic_create_table_quotes_per_engine() {
        let columns = vec![
            GenericDdlColumn {
                name: "id".into(),
                type_name: "integer".into(),
                nullable: false,
                is_identity: true,
                ..GenericDdlColumn::default()
            },
            GenericDdlColumn {
                name: "name".into(),
                type_name: "character varying".into(),
                max_length: Some(80),
                nullable: true,
                ..GenericDdlColumn::default()
            },
        ];
        let pg = build_generic_create_table_ddl(
            DatabaseType::Postgres,
            Some("public"),
            "users",
            &columns,
        );
        assert!(pg.contains("CREATE TABLE \"public\".\"users\""), "{pg}");
        assert!(
            pg.contains("\"id\" integer GENERATED BY DEFAULT AS IDENTITY NOT NULL"),
            "{pg}"
        );
        assert!(pg.contains("\"name\" character varying(80) NULL"), "{pg}");

        let my =
            build_generic_create_table_ddl(DatabaseType::MySql, Some("app"), "users", &columns);
        assert!(my.contains("CREATE TABLE `app`.`users`"), "{my}");
        assert!(my.contains("`id` integer AUTO_INCREMENT NOT NULL"), "{my}");
    }

    #[test]
    fn generic_type_rendering_covers_decimal_and_verbatim() {
        let mut c = col("price", "numeric");
        c.precision = Some(10);
        c.scale = Some(2);
        assert_eq!(render_generic_type(&c), "numeric(10, 2)");
        // SQLite declared types arrive verbatim with no length metadata.
        let c = col("note", "VARCHAR(30)");
        assert_eq!(render_generic_type(&c), "VARCHAR(30)");
    }

    #[test]
    fn computed_columns_are_marked() {
        let mut c = col("total", "integer");
        c.is_computed = true;
        let sql =
            build_generic_create_table_ddl(DatabaseType::Postgres, None, "orders", &[c.clone()]);
        assert!(sql.contains("/* computed"), "{sql}");
    }
}
