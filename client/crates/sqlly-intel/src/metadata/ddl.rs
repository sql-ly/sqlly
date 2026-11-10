//! DDL retrieval and generation for "go to definition" navigation.
//!
//! Pure SQL-string builders and CREATE-statement generators — no I/O. The
//! `sqlly-services` DDL fetcher executes these queries and feeds the rows
//! back into the generators.
//!
//! SQL Server specifics this module encodes:
//! - `OBJECT_DEFINITION` returns the source of views/procedures/functions/
//!   triggers but **NULL for tables** (and for encrypted objects or when the
//!   caller lacks `VIEW DEFINITION`). Table DDL must be generated from
//!   catalog metadata instead.
//! - `sp_helptext` is the fallback for old-style sources; it returns the
//!   text as multiple ordered rows that must be concatenated verbatim (the
//!   rows already contain their own line breaks).

use crate::metadata::{escape_literal, quote_name};

// ---------------------------------------------------------------------------
// Query builders (safe escaping; never interpolate raw clicked text)
// ---------------------------------------------------------------------------

/// Source text of a view/procedure/function via `OBJECT_DEFINITION`.
/// Returns one row with one nullable column.
pub fn object_definition_sql(schema: &str, name: &str) -> String {
    format!(
        "SELECT OBJECT_DEFINITION(OBJECT_ID(N'{}')) AS definition",
        escape_literal(&format!("{schema}.{name}"))
    )
}

/// `sp_helptext` fallback. Returns the source as multiple ordered rows in a
/// single `Text` column; concatenate them in order without inserting
/// separators.
pub fn sp_helptext_sql(schema: &str, name: &str) -> String {
    format!(
        "EXEC sp_helptext N'{}'",
        escape_literal(&format!("{schema}.{name}"))
    )
}

/// Rich column metadata for CREATE TABLE generation. Columns per row:
/// name, type_name, max_length, precision, scale, is_nullable, is_identity,
/// identity_seed, identity_increment, is_computed, computed_definition,
/// default_definition, collation_name.
pub fn table_ddl_columns_sql(schema: &str, table: &str) -> String {
    let object = escape_literal(&format!("{schema}.{table}"));
    format!(
        "SELECT c.name, t.name AS type_name, c.max_length, c.precision, c.scale, \
         c.is_nullable, c.is_identity, \
         CAST(IDENT_SEED(N'{object}') AS bigint) AS identity_seed, \
         CAST(IDENT_INCR(N'{object}') AS bigint) AS identity_increment, \
         c.is_computed, cc.definition AS computed_definition, \
         dc.definition AS default_definition, c.collation_name \
         FROM sys.columns c \
         JOIN sys.types t ON c.user_type_id = t.user_type_id \
         LEFT JOIN sys.computed_columns cc ON cc.object_id = c.object_id AND cc.column_id = c.column_id \
         LEFT JOIN sys.default_constraints dc ON dc.object_id = c.default_object_id \
         WHERE c.object_id = OBJECT_ID(N'{object}') \
         ORDER BY c.column_id"
    )
}

/// Primary-key constraint of a table: constraint name, clustered-ness, and
/// key columns in key order. Columns per row: constraint_name, type_desc,
/// column_name, is_descending_key.
pub fn table_primary_key_sql(schema: &str, table: &str) -> String {
    let object = escape_literal(&format!("{schema}.{table}"));
    format!(
        "SELECT kc.name AS constraint_name, i.type_desc, col.name AS column_name, ic.is_descending_key \
         FROM sys.key_constraints kc \
         JOIN sys.indexes i ON i.object_id = kc.parent_object_id AND i.index_id = kc.unique_index_id \
         JOIN sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
         JOIN sys.columns col ON col.object_id = ic.object_id AND col.column_id = ic.column_id \
         WHERE kc.parent_object_id = OBJECT_ID(N'{object}') AND kc.type = 'PK' \
         ORDER BY ic.key_ordinal"
    )
}

/// User-defined type descriptor. Columns per row: name, base_type_name,
/// max_length, precision, scale, is_nullable, is_table_type,
/// is_assembly_type.
pub fn type_definition_sql(schema: &str, name: &str) -> String {
    format!(
        "SELECT t.name, TYPE_NAME(t.system_type_id) AS base_type_name, t.max_length, \
         t.precision, t.scale, t.is_nullable, t.is_table_type, t.is_assembly_type \
         FROM sys.types t JOIN sys.schemas s ON s.schema_id = t.schema_id \
         WHERE t.is_user_defined = 1 AND s.name = N'{}' AND t.name = N'{}'",
        escape_literal(schema),
        escape_literal(name)
    )
}

/// Columns of a user-defined table type. Columns per row: name, type_name,
/// max_length, precision, scale, is_nullable.
pub fn table_type_columns_sql(schema: &str, name: &str) -> String {
    format!(
        "SELECT c.name, TYPE_NAME(c.user_type_id) AS type_name, c.max_length, c.precision, \
         c.scale, c.is_nullable \
         FROM sys.table_types tt \
         JOIN sys.columns c ON c.object_id = tt.type_table_object_id \
         JOIN sys.schemas s ON s.schema_id = tt.schema_id \
         WHERE s.name = N'{}' AND tt.name = N'{}' \
         ORDER BY c.column_id",
        escape_literal(schema),
        escape_literal(name)
    )
}

/// Server-built `CREATE INDEX` statements for a table's non-primary-key,
/// non-unique-constraint indexes. Returns one row per index: a single
/// `index_ddl` string, already fully rendered (key columns with sort order, any
/// `INCLUDE (...)` covering columns, and any filter predicate). Column
/// concatenation uses `STUFF`/`FOR XML PATH` so it works on the SQL Server
/// versions the app targets without `STRING_AGG`.
///
/// Scope limits (documented rather than silently wrong): only rowstore indexes
/// (`type IN (1, 2)`) are scripted — columnstore, XML, and spatial indexes are
/// skipped — and storage options (`WITH (DATA_COMPRESSION/FILLFACTOR)`) and
/// filegroup placement are not emitted.
pub fn table_indexes_ddl_sql(schema: &str, table: &str) -> String {
    let object = escape_literal(&format!("{schema}.{table}"));
    format!(
        "SELECT 'CREATE ' + CASE WHEN i.is_unique = 1 THEN 'UNIQUE ' ELSE '' END \
         + i.type_desc COLLATE DATABASE_DEFAULT \
         + ' INDEX ' + QUOTENAME(i.name) + ' ON ' + QUOTENAME(SCHEMA_NAME(t.schema_id)) + '.' \
         + QUOTENAME(t.name) + ' (' + STUFF((SELECT ', ' + QUOTENAME(c.name) \
         + CASE WHEN ic.is_descending_key = 1 THEN ' DESC' ELSE ' ASC' END \
         FROM sys.index_columns ic JOIN sys.columns c ON c.object_id = ic.object_id \
         AND c.column_id = ic.column_id WHERE ic.object_id = i.object_id \
         AND ic.index_id = i.index_id AND ic.is_included_column = 0 ORDER BY ic.key_ordinal \
         FOR XML PATH('')), 1, 2, '') + ')' \
         + ISNULL(' INCLUDE (' + STUFF((SELECT ', ' + QUOTENAME(c.name) \
         FROM sys.index_columns ic JOIN sys.columns c ON c.object_id = ic.object_id \
         AND c.column_id = ic.column_id WHERE ic.object_id = i.object_id \
         AND ic.index_id = i.index_id AND ic.is_included_column = 1 ORDER BY ic.index_column_id \
         FOR XML PATH('')), 1, 2, '') + ')', '') \
         + ISNULL(' WHERE ' + i.filter_definition COLLATE DATABASE_DEFAULT, '') + ';' \
         AS index_ddl \
         FROM sys.indexes i JOIN sys.tables t ON t.object_id = i.object_id \
         WHERE i.object_id = OBJECT_ID(N'{object}') AND i.is_primary_key = 0 \
         AND i.is_unique_constraint = 0 AND i.type IN (1, 2) AND i.name IS NOT NULL \
         ORDER BY i.name"
    )
}

/// Server-built `ALTER TABLE … ADD CONSTRAINT … FOREIGN KEY …` statements for
/// the foreign keys a table declares. One `fk_ddl` string per key, with local
/// and referenced column lists in key order.
pub fn table_foreign_keys_ddl_sql(schema: &str, table: &str) -> String {
    let object = escape_literal(&format!("{schema}.{table}"));
    format!(
        "SELECT 'ALTER TABLE ' + QUOTENAME(SCHEMA_NAME(fk.schema_id)) + '.' \
         + QUOTENAME(OBJECT_NAME(fk.parent_object_id)) + ' ADD CONSTRAINT ' + QUOTENAME(fk.name) \
         + ' FOREIGN KEY (' + STUFF((SELECT ', ' + QUOTENAME(pc.name) \
         FROM sys.foreign_key_columns fkc JOIN sys.columns pc ON pc.object_id = fkc.parent_object_id \
         AND pc.column_id = fkc.parent_column_id WHERE fkc.constraint_object_id = fk.object_id \
         ORDER BY fkc.constraint_column_id FOR XML PATH('')), 1, 2, '') + ') REFERENCES ' \
         + QUOTENAME(SCHEMA_NAME(rt.schema_id)) + '.' + QUOTENAME(rt.name) + ' (' \
         + STUFF((SELECT ', ' + QUOTENAME(rc.name) FROM sys.foreign_key_columns fkc \
         JOIN sys.columns rc ON rc.object_id = fkc.referenced_object_id \
         AND rc.column_id = fkc.referenced_column_id WHERE fkc.constraint_object_id = fk.object_id \
         ORDER BY fkc.constraint_column_id FOR XML PATH('')), 1, 2, '') + ')' \
         + CASE fk.delete_referential_action WHEN 1 THEN ' ON DELETE CASCADE' \
         WHEN 2 THEN ' ON DELETE SET NULL' WHEN 3 THEN ' ON DELETE SET DEFAULT' ELSE '' END \
         + CASE fk.update_referential_action WHEN 1 THEN ' ON UPDATE CASCADE' \
         WHEN 2 THEN ' ON UPDATE SET NULL' WHEN 3 THEN ' ON UPDATE SET DEFAULT' ELSE '' END \
         + ';' AS fk_ddl \
         FROM sys.foreign_keys fk JOIN sys.tables rt ON rt.object_id = fk.referenced_object_id \
         WHERE fk.parent_object_id = OBJECT_ID(N'{object}') ORDER BY fk.name"
    )
}

/// Trigger sources for a table. One row per trigger: `name`, then the trigger's
/// `OBJECT_DEFINITION` (nullable when encrypted or unreadable).
pub fn table_triggers_definition_sql(schema: &str, table: &str) -> String {
    let object = escape_literal(&format!("{schema}.{table}"));
    format!(
        "SELECT tr.name, OBJECT_DEFINITION(tr.object_id) AS definition \
         FROM sys.triggers tr WHERE tr.parent_id = OBJECT_ID(N'{object}') \
         AND tr.is_ms_shipped = 0 ORDER BY tr.name"
    )
}

// ---------------------------------------------------------------------------
// Row models + CREATE-statement generators (pure)
// ---------------------------------------------------------------------------

/// One column of a table, parsed from [`table_ddl_columns_sql`] rows.
#[derive(Debug, Clone, Default)]
pub struct TableDdlColumn {
    pub name: String,
    pub type_name: String,
    pub max_length: Option<i64>,
    pub precision: Option<i64>,
    pub scale: Option<i64>,
    pub nullable: bool,
    pub is_identity: bool,
    pub identity_seed: Option<i64>,
    pub identity_increment: Option<i64>,
    pub is_computed: bool,
    pub computed_definition: Option<String>,
    pub default_definition: Option<String>,
    pub collation: Option<String>,
}

/// Primary key parsed from [`table_primary_key_sql`] rows.
#[derive(Debug, Clone)]
pub struct PrimaryKeyDdl {
    pub constraint_name: String,
    pub clustered: bool,
    /// `(column_name, is_descending)` in key order.
    pub columns: Vec<(String, bool)>,
}

/// Render a SQL Server type spelling with its length/precision/scale
/// arguments (`nvarchar(50)`, `nvarchar(max)`, `decimal(18,2)`, `int`, …).
/// `max_length` is the raw `sys.columns.max_length` (bytes; `-1` = MAX;
/// nchar/nvarchar report byte counts, so character length is half).
pub fn render_sql_type(
    type_name: &str,
    max_length: Option<i64>,
    precision: Option<i64>,
    scale: Option<i64>,
) -> String {
    let lower = type_name.to_ascii_lowercase();
    match lower.as_str() {
        "char" | "varchar" | "binary" | "varbinary" => match max_length {
            Some(-1) => format!("{type_name}(max)"),
            Some(len) if len > 0 => format!("{type_name}({len})"),
            _ => type_name.to_string(),
        },
        "nchar" | "nvarchar" => match max_length {
            Some(-1) => format!("{type_name}(max)"),
            Some(len) if len > 0 => format!("{type_name}({})", len / 2),
            _ => type_name.to_string(),
        },
        "decimal" | "numeric" => match (precision, scale) {
            (Some(p), Some(s)) if p > 0 => format!("{type_name}({p},{s})"),
            (Some(p), None) if p > 0 => format!("{type_name}({p})"),
            _ => type_name.to_string(),
        },
        "datetime2" | "datetimeoffset" | "time" => match scale {
            Some(s) if s != 7 => format!("{type_name}({s})"),
            _ => type_name.to_string(),
        },
        _ => type_name.to_string(),
    }
}

/// Generate a `CREATE TABLE` statement from catalog metadata. SQL Server's
/// `OBJECT_DEFINITION` returns NULL for tables, so this generated form is
/// the authoritative navigation source for table targets.
pub fn build_create_table_ddl(
    schema: &str,
    table: &str,
    columns: &[TableDdlColumn],
    primary_key: Option<&PrimaryKeyDdl>,
) -> String {
    let mut lines: Vec<String> = Vec::with_capacity(columns.len() + 1);

    for col in columns {
        let mut line = format!("    {}", quote_name(&col.name));
        if col.is_computed {
            let expr = col.computed_definition.as_deref().unwrap_or("(?)");
            line.push_str(&format!(" AS {expr}"));
        } else {
            line.push(' ');
            line.push_str(&render_sql_type(
                &col.type_name,
                col.max_length,
                col.precision,
                col.scale,
            ));
            if let Some(collation) = &col.collation {
                line.push_str(&format!(" COLLATE {collation}"));
            }
            if col.is_identity {
                let seed = col.identity_seed.unwrap_or(1);
                let increment = col.identity_increment.unwrap_or(1);
                line.push_str(&format!(" IDENTITY({seed},{increment})"));
            }
            line.push_str(if col.nullable { " NULL" } else { " NOT NULL" });
            if let Some(default) = &col.default_definition {
                line.push_str(&format!(" DEFAULT {default}"));
            }
        }
        lines.push(line);
    }

    if let Some(pk) = primary_key {
        let cols = pk
            .columns
            .iter()
            .map(|(name, desc)| {
                if *desc {
                    format!("{} DESC", quote_name(name))
                } else {
                    quote_name(name)
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        let clustered = if pk.clustered {
            "CLUSTERED"
        } else {
            "NONCLUSTERED"
        };
        lines.push(format!(
            "    CONSTRAINT {} PRIMARY KEY {clustered} ({cols})",
            quote_name(&pk.constraint_name)
        ));
    }

    format!(
        "CREATE TABLE {}.{} (\n{}\n)",
        quote_name(schema),
        quote_name(table),
        lines.join(",\n")
    )
}

/// Generate `CREATE TYPE … FROM …` DDL for an alias type.
pub fn build_alias_type_ddl(
    schema: &str,
    name: &str,
    base_type: &str,
    max_length: Option<i64>,
    precision: Option<i64>,
    scale: Option<i64>,
    nullable: bool,
) -> String {
    format!(
        "CREATE TYPE {}.{} FROM {}{}",
        quote_name(schema),
        quote_name(name),
        render_sql_type(base_type, max_length, precision, scale),
        if nullable { " NULL" } else { " NOT NULL" }
    )
}

/// Generate `CREATE TYPE … AS TABLE (…)` DDL for a table type.
pub fn build_table_type_ddl(schema: &str, name: &str, columns: &[TableDdlColumn]) -> String {
    let body = columns
        .iter()
        .map(|col| {
            format!(
                "    {} {}{}",
                quote_name(&col.name),
                render_sql_type(&col.type_name, col.max_length, col.precision, col.scale),
                if col.nullable { " NULL" } else { " NOT NULL" }
            )
        })
        .collect::<Vec<_>>()
        .join(",\n");
    format!(
        "CREATE TYPE {}.{} AS TABLE (\n{}\n)",
        quote_name(schema),
        quote_name(name),
        body
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_definition_sql_escapes_quotes() {
        let sql = object_definition_sql("dbo", "Weird'Proc");
        assert!(sql.contains("OBJECT_ID(N'dbo.Weird''Proc')"));
    }

    #[test]
    fn sp_helptext_sql_escapes_quotes() {
        let sql = sp_helptext_sql("dbo", "Weird'Proc");
        assert_eq!(sql, "EXEC sp_helptext N'dbo.Weird''Proc'");
    }

    #[test]
    fn table_ddl_columns_sql_targets_object() {
        let sql = table_ddl_columns_sql("dbo", "Users");
        assert!(sql.contains("OBJECT_ID(N'dbo.Users')"));
        assert!(sql.contains("sys.computed_columns"));
        assert!(sql.contains("sys.default_constraints"));
        assert!(sql.contains("ORDER BY c.column_id"));
    }

    #[test]
    fn table_indexes_ddl_sql_excludes_primary_key_and_builds_create_index() {
        let sql = table_indexes_ddl_sql("dbo", "Users");
        assert!(sql.contains("OBJECT_ID(N'dbo.Users')"));
        assert!(sql.contains("'CREATE '"));
        assert!(sql.contains("i.is_primary_key = 0"));
        assert!(sql.contains("i.is_unique_constraint = 0"));
        assert!(sql.contains("FOR XML PATH"));
        // Covering (INCLUDE) columns are rendered from a second XML-PATH subquery
        // filtered to included columns, wrapped in ISNULL so a key with none omits
        // the clause entirely rather than emitting an empty `INCLUDE ()`.
        assert!(sql.contains("' INCLUDE ('"), "{sql}");
        assert!(sql.contains("ic.is_included_column = 1"), "{sql}");
        assert!(sql.contains("ic.is_included_column = 0"), "{sql}");
    }

    #[test]
    fn table_foreign_keys_ddl_sql_builds_alter_add_constraint() {
        let sql = table_foreign_keys_ddl_sql("dbo", "Users");
        assert!(sql.contains("OBJECT_ID(N'dbo.Users')"));
        assert!(sql.contains("ADD CONSTRAINT"));
        assert!(sql.contains("FOREIGN KEY"));
        assert!(sql.contains("REFERENCES"));
        assert!(sql.contains("sys.foreign_keys fk"));
    }

    #[test]
    fn table_triggers_definition_sql_targets_parent_and_uses_object_definition() {
        let sql = table_triggers_definition_sql("dbo", "Users");
        assert!(sql.contains("sys.triggers tr"));
        assert!(sql.contains("tr.parent_id = OBJECT_ID(N'dbo.Users')"));
        assert!(sql.contains("OBJECT_DEFINITION(tr.object_id)"));
    }

    #[test]
    fn type_definition_sql_escapes_both_parts() {
        let sql = type_definition_sql("we'ird", "ty'pe");
        assert!(sql.contains("s.name = N'we''ird'"));
        assert!(sql.contains("t.name = N'ty''pe'"));
    }

    #[test]
    fn render_types() {
        assert_eq!(render_sql_type("int", Some(4), Some(10), Some(0)), "int");
        assert_eq!(
            render_sql_type("nvarchar", Some(100), None, None),
            "nvarchar(50)"
        );
        assert_eq!(
            render_sql_type("nvarchar", Some(-1), None, None),
            "nvarchar(max)"
        );
        assert_eq!(
            render_sql_type("varchar", Some(80), None, None),
            "varchar(80)"
        );
        assert_eq!(
            render_sql_type("varbinary", Some(-1), None, None),
            "varbinary(max)"
        );
        assert_eq!(
            render_sql_type("decimal", Some(9), Some(18), Some(2)),
            "decimal(18,2)"
        );
        assert_eq!(
            render_sql_type("datetime2", Some(8), None, Some(7)),
            "datetime2"
        );
        assert_eq!(
            render_sql_type("datetime2", Some(8), None, Some(3)),
            "datetime2(3)"
        );
    }

    fn col(name: &str, ty: &str) -> TableDdlColumn {
        TableDdlColumn {
            name: name.to_string(),
            type_name: ty.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn create_table_ddl_covers_core_column_shapes() {
        let columns = vec![
            TableDdlColumn {
                max_length: Some(4),
                is_identity: true,
                identity_seed: Some(1),
                identity_increment: Some(1),
                ..col("UserId", "int")
            },
            TableDdlColumn {
                max_length: Some(200),
                nullable: true,
                collation: Some("Latin1_General_CI_AS".into()),
                ..col("Name", "nvarchar")
            },
            TableDdlColumn {
                default_definition: Some("((0))".into()),
                ..col("IsActive", "bit")
            },
            TableDdlColumn {
                is_computed: true,
                computed_definition: Some("([UserId]*(2))".into()),
                nullable: true,
                ..col("Doubled", "int")
            },
        ];
        let pk = PrimaryKeyDdl {
            constraint_name: "PK_Users".into(),
            clustered: true,
            columns: vec![("UserId".into(), false)],
        };
        let ddl = build_create_table_ddl("dbo", "Users", &columns, Some(&pk));

        assert!(ddl.starts_with("CREATE TABLE [dbo].[Users] (\n"));
        assert!(ddl.contains("[UserId] int IDENTITY(1,1) NOT NULL"));
        assert!(ddl.contains("[Name] nvarchar(100) COLLATE Latin1_General_CI_AS NULL"));
        assert!(ddl.contains("[IsActive] bit NOT NULL DEFAULT ((0))"));
        assert!(ddl.contains("[Doubled] AS ([UserId]*(2))"));
        assert!(ddl.contains("CONSTRAINT [PK_Users] PRIMARY KEY CLUSTERED ([UserId])"));
        assert!(ddl.trim_end().ends_with(')'));
    }

    #[test]
    fn create_table_ddl_without_pk() {
        let ddl = build_create_table_ddl("dbo", "Log", &[col("Line", "int")], None);
        assert!(!ddl.contains("CONSTRAINT"));
        assert!(ddl.contains("[Line] int NOT NULL"));
    }

    #[test]
    fn alias_type_ddl() {
        let ddl = build_alias_type_ddl("dbo", "Phone", "varchar", Some(20), None, None, false);
        assert_eq!(ddl, "CREATE TYPE [dbo].[Phone] FROM varchar(20) NOT NULL");
    }

    #[test]
    fn table_type_ddl() {
        let ddl = build_table_type_ddl(
            "dbo",
            "OrderLines",
            &[
                TableDdlColumn {
                    max_length: Some(4),
                    ..col("OrderId", "int")
                },
                TableDdlColumn {
                    max_length: Some(9),
                    precision: Some(18),
                    scale: Some(2),
                    nullable: true,
                    ..col("Amount", "decimal")
                },
            ],
        );
        assert!(ddl.starts_with("CREATE TYPE [dbo].[OrderLines] AS TABLE (\n"));
        assert!(ddl.contains("[OrderId] int NOT NULL"));
        assert!(ddl.contains("[Amount] decimal(18,2) NULL"));
    }
}
