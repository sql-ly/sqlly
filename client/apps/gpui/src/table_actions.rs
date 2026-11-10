//! Pure SQL script generators for the schema explorer's right-click menu on
//! table nodes. These are direct ports of the Swift `TableActions` enum — no
//! I/O, fully unit-testable. Identifier escaping uses the `[name]` doubling
//! rule and value literalisation uses the `N'…'` doubling rule so generated
//! scripts are injection-safe.

use sqlly_core::CellValue;
use sqlly_lang::sqlmodel::{
    ConnectionKind, DatabaseType, IdentifierQuoting, LogicalType, SqlOperation, UpsertPattern,
};

/// A qualified table reference: `database.schema.table` with optional
/// database/schema. Generators escape whatever is present and fall back to
/// `dbo` when schema is blank.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QualifiedTable {
    pub database: Option<String>,
    pub schema: Option<String>,
    pub table: String,
}

impl QualifiedTable {
    pub fn new(table: impl Into<String>) -> Self {
        Self {
            database: None,
            schema: None,
            table: table.into(),
        }
    }

    pub fn with_schema(mut self, schema: impl Into<String>) -> Self {
        self.schema = Some(schema.into());
        self
    }

    #[allow(dead_code)]
    pub fn with_database(mut self, db: impl Into<String>) -> Self {
        self.database = Some(db.into());
        self
    }

    /// `database.schema.table` with each component `[…]` escaped.
    pub fn multipart_identifier(&self) -> String {
        let mut parts = Vec::new();
        if let Some(db) = non_empty(&self.database) {
            parts.push(escape_identifier(db));
        }
        if let Some(schema) = non_empty(&self.schema) {
            parts.push(escape_identifier(schema));
        }
        parts.push(escape_identifier(&self.table));
        parts.join(".")
    }

    /// Defaults a blank schema to `dbo` (SSMS behavior).
    #[cfg(test)]
    pub fn resolved_schema(&self) -> String {
        non_empty(&self.schema)
            .map(|s| s.to_string())
            .unwrap_or_else(|| "dbo".to_string())
    }

    /// `schema.table` (with dbo default) — for `SELECT TOP n * FROM …`.
    #[cfg(test)]
    pub fn schema_qualified_table(&self) -> String {
        format!(
            "{}.{}",
            escape_identifier(&self.resolved_schema()),
            escape_identifier(&self.table)
        )
    }

    /// `schema.table` quoted with the engine's identifier style, defaulting a
    /// blank schema to the engine's default schema (or leaving it off for
    /// engines without schemas, like SQLite and MySQL).
    pub fn schema_qualified_table_for(&self, db: DatabaseType) -> String {
        let table = quote_identifier_for(db, &self.table);
        let schema = non_empty(&self.schema)
            .map(|s| s.to_string())
            .or_else(|| db.default_schema().map(str::to_string));
        match schema {
            Some(schema) => format!("{}.{}", quote_identifier_for(db, &schema), table),
            None => table,
        }
    }

    /// The fullest name the engine can address: `database.schema.table` on
    /// the SQL Server family (byte-identical to the legacy bracket path),
    /// `schema.table` elsewhere — three-part names are invalid on PostgreSQL,
    /// and the MySQL lineage's namespace already rides in the schema slot.
    pub fn multipart_identifier_for(&self, db: DatabaseType) -> String {
        if matches!(db.connection_kind(), ConnectionKind::Tds) {
            self.multipart_identifier()
        } else {
            self.schema_qualified_table_for(db)
        }
    }
}

/// Quote one identifier with the engine's style. The bracket style uses
/// `escape_identifier` (`]` doubling, `[Column]` fallback for empty names).
fn quote_identifier_for(db: DatabaseType, identifier: &str) -> String {
    match db.identifier_quoting() {
        IdentifierQuoting::Brackets => escape_identifier(identifier),
        // The engine's own rule (ClickHouse/Spanner also escape `\`).
        _ => db.quote_identifier(identifier.trim()),
    }
}

/// Column metadata for CREATE TABLE DDL generation.
#[derive(Clone, Debug)]
pub struct ColumnDescriptor {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
}

/// Escapes a SQL Server identifier: `[name]` with `]` → `]]` doubling.
/// Already-bracketed names are unwrapped and re-escaped (never trusted
/// verbatim). Empty → `[Column]`.
pub fn escape_identifier(identifier: &str) -> String {
    let trimmed = identifier.trim();
    if trimmed.is_empty() {
        return "[Column]".to_string();
    }
    let inner = if trimmed.starts_with('[') && trimmed.ends_with(']') && trimmed.len() >= 2 {
        trimmed[1..trimmed.len() - 1].replace("]]", "]")
    } else {
        trimmed.to_string()
    };
    if inner.trim().is_empty() {
        return "[Column]".to_string();
    }
    format!("[{}]", inner.replace(']', "]]"))
}

/// Renders a `CellValue` as a SQL Server literal.
/// Null→NULL, Bool→1/0, I64→string, F64→string (NULL if !finite),
/// String→N'…' with ' → '' doubling, Bytes→0x….
#[cfg(test)]
pub fn sql_literal(value: &CellValue) -> String {
    sql_literal_for(DatabaseType::SqlServer, value)
}

/// Renders a `CellValue` as a literal in the engine's own forms: string,
/// boolean, and binary literals all come from the engine's rules. SQL Server
/// output is byte-identical to the legacy `sql_literal`.
pub fn sql_literal_for(db: DatabaseType, value: &CellValue) -> String {
    match value {
        CellValue::Null => "NULL".to_string(),
        CellValue::Bool(b) => db.bool_literal(*b).to_string(),
        CellValue::I64(n) => n.to_string(),
        CellValue::F64(f) => {
            if f.is_finite() {
                f.to_string()
            } else {
                "NULL".to_string()
            }
        }
        CellValue::String(s) => db.string_literal(s),
        CellValue::Bytes(b) => db.bytes_literal(b),
    }
}

/// `SELECT TOP n * FROM [schema].[table]` (no ORDER BY — mirrors SSMS).
/// T-SQL flavor; prefer `select_top_rows_sql_for` when the engine is known.
#[cfg(test)]
pub fn select_top_rows_sql(n: usize, table: &QualifiedTable) -> String {
    select_top_rows_sql_for(DatabaseType::SqlServer, n, table)
}

/// "Select top n rows" rendered with the engine's row-limit pattern and
/// identifier quoting: `TOP` on SQL Server/Access/Teradata, `LIMIT` on the
/// Postgres/MySQL/SQLite lineages, `FETCH FIRST` on Oracle/Db2, `FIRST` on
/// Informix.
pub fn select_top_rows_sql_for(db: DatabaseType, n: usize, table: &QualifiedTable) -> String {
    db.render_limited_select("*", &table.schema_qualified_table_for(db), n)
}

/// `SELECT COUNT(*) FROM [schema].[table]` (T-SQL; tests only).
#[cfg(test)]
pub fn count_rows_sql(table: &QualifiedTable) -> String {
    count_rows_sql_for(DatabaseType::SqlServer, table)
}

/// `SELECT COUNT(*) AS …` with the engine's identifier quoting.
pub fn count_rows_sql_for(db: DatabaseType, table: &QualifiedTable) -> String {
    format!(
        "SELECT COUNT(*) AS {}\nFROM {}",
        quote_identifier_for(db, "RowCount"),
        table.schema_qualified_table_for(db)
    )
}

/// `SELECT * FROM [schema].[table]` (T-SQL; tests only).
#[cfg(test)]
pub fn view_data_sql(table: &QualifiedTable) -> String {
    view_data_sql_for(DatabaseType::SqlServer, table)
}

/// `SELECT * FROM …` (all rows) with the engine's identifier quoting.
pub fn view_data_sql_for(db: DatabaseType, table: &QualifiedTable) -> String {
    format!("SELECT *\nFROM {}", table.schema_qualified_table_for(db))
}

/// `DELETE FROM [schema].[table]` (T-SQL; tests only).
#[cfg(test)]
pub fn delete_sql(table: &QualifiedTable) -> String {
    delete_sql_for(DatabaseType::SqlServer, table)
}

/// `DELETE FROM …` (no WHERE — caller appends if needed).
pub fn delete_sql_for(db: DatabaseType, table: &QualifiedTable) -> String {
    format!("DELETE FROM {}", table.schema_qualified_table_for(db))
}

/// `TRUNCATE TABLE [schema].[table]` (T-SQL; tests only).
#[cfg(test)]
pub fn truncate_sql(table: &QualifiedTable) -> String {
    truncate_sql_for(DatabaseType::SqlServer, table)
}

/// `TRUNCATE TABLE …` where the engine has it; engines without TRUNCATE
/// (SQLite, Access) get the honest equivalent — a commented DELETE.
pub fn truncate_sql_for(db: DatabaseType, table: &QualifiedTable) -> String {
    let target = table.schema_qualified_table_for(db);
    if db.supports_operation(SqlOperation::TruncateTable) {
        format!("TRUNCATE TABLE {target}")
    } else {
        format!(
            "-- {} has no TRUNCATE TABLE; DELETE removes all rows instead.\nDELETE FROM {target}",
            db.display_name()
        )
    }
}

/// `DROP TABLE [database].[schema].[table]` (T-SQL; tests only).
#[cfg(test)]
pub fn drop_table_sql(table: &QualifiedTable) -> String {
    drop_table_sql_for(DatabaseType::SqlServer, table)
}

/// `DROP TABLE …` with the fullest name the engine can address.
#[cfg(test)]
pub fn drop_table_sql_for(db: DatabaseType, table: &QualifiedTable) -> String {
    format!("DROP TABLE {}", table.multipart_identifier_for(db))
}

/// `CREATE TABLE [schema].[table] (…)` (T-SQL; tests only).
#[cfg(test)]
pub fn create_table_ddl(table: &QualifiedTable, columns: &[ColumnDescriptor]) -> String {
    create_table_ddl_for(DatabaseType::SqlServer, table, columns)
}

/// `CREATE TABLE … (…)` with one `column type NULL|NOT NULL` line per
/// column, quoted and type-defaulted for the engine. Identity/computed/
/// default columns degrade to NULL.
pub fn create_table_ddl_for(
    db: DatabaseType,
    table: &QualifiedTable,
    columns: &[ColumnDescriptor],
) -> String {
    let tbl = table.multipart_identifier_for(db);
    if columns.is_empty() {
        return format!("CREATE TABLE {} (\n);\n", tbl);
    }
    let mut lines = vec![format!("CREATE TABLE {} (", tbl)];
    for (i, col) in columns.iter().enumerate() {
        let col_name = quote_identifier_for(db, &col.name);
        let sql_type = sql_type_for(db, col);
        let nullability = if col.nullable { "NULL" } else { "NOT NULL" };
        let suffix = if i == columns.len() - 1 { "" } else { "," };
        lines.push(format!(
            "    {} {} {}{}",
            col_name, sql_type, nullability, suffix
        ));
    }
    lines.push(");".to_string());
    lines.join("\n")
}

/// `INSERT INTO [table] (cols…) VALUES (…)` (T-SQL; tests only).
#[cfg(test)]
pub fn insert_into_sql(
    table: &QualifiedTable,
    columns: &[ColumnDescriptor],
    identity_columns: &[String],
    rows: &[Vec<CellValue>],
) -> String {
    insert_into_sql_for(
        DatabaseType::SqlServer,
        table,
        columns,
        identity_columns,
        rows,
    )
}

/// `INSERT INTO … (cols…) VALUES (…)` per row with the engine's quoting and
/// literals. The `SET IDENTITY_INSERT` guard is a SQL Server concept and only
/// wraps output on that family.
pub fn insert_into_sql_for(
    db: DatabaseType,
    table: &QualifiedTable,
    columns: &[ColumnDescriptor],
    identity_columns: &[String],
    rows: &[Vec<CellValue>],
) -> String {
    let tbl = table.multipart_identifier_for(db);
    let col_list: Vec<String> = columns
        .iter()
        .map(|c| quote_identifier_for(db, &c.name))
        .collect();
    let col_text = col_list.join(", ");
    if rows.is_empty() {
        return format!("-- No rows returned for {}.", tbl);
    }
    let needs_guard = matches!(db.connection_kind(), ConnectionKind::Tds)
        && !identity_columns.is_empty()
        && columns.iter().any(|c| identity_columns.contains(&c.name));

    let mut lines = Vec::new();
    if needs_guard {
        lines.push(format!("SET IDENTITY_INSERT {} ON;", tbl));
        lines.push(String::new());
    }
    lines.push(format!("INSERT INTO {} ({})", tbl, col_text));
    lines.push("VALUES".to_string());
    lines.extend(sql_value_rows(db, rows, columns.len(), ";"));
    if needs_guard {
        lines.push(String::new());
        lines.push(format!("SET IDENTITY_INSERT {} OFF;", tbl));
    }
    lines.join("\n")
}

#[allow(dead_code)]
/// `SET IDENTITY_INSERT [table] ON/OFF`.
pub fn identity_insert_guard(table: &QualifiedTable, turning_on: bool) -> String {
    let keyword = if turning_on { "ON" } else { "OFF" };
    format!(
        "SET IDENTITY_INSERT {} {};",
        table.multipart_identifier(),
        keyword
    )
}

/// `MERGE [target] USING (VALUES …)` (T-SQL; tests only).
#[cfg(test)]
pub fn merge_upsert_sql(
    table: &QualifiedTable,
    columns: &[ColumnDescriptor],
    key_columns: &[String],
    rows: &[Vec<CellValue>],
) -> String {
    upsert_sql_for(DatabaseType::SqlServer, table, columns, key_columns, rows)
}

/// Upsert script in the engine's native pattern: `MERGE` where that is the
/// engine's form (byte-identical to the legacy T-SQL output on SQL Server),
/// `INSERT … ON CONFLICT` on the PostgreSQL/SQLite lineage, `INSERT … ON
/// DUPLICATE KEY UPDATE` on the MySQL lineage. Empty/missing keys → comment.
pub fn upsert_sql_for(
    db: DatabaseType,
    table: &QualifiedTable,
    columns: &[ColumnDescriptor],
    key_columns: &[String],
    rows: &[Vec<CellValue>],
) -> String {
    let tbl = table.multipart_identifier_for(db);
    if rows.is_empty() {
        return format!("-- No rows returned for {}.", tbl);
    }
    if key_columns.is_empty() {
        return format!("-- Upsert skipped: no key columns supplied for {}.\n", tbl);
    }
    let missing: Vec<String> = key_columns
        .iter()
        .filter(|k| !columns.iter().any(|c| &c.name == *k))
        .cloned()
        .collect();
    if !missing.is_empty() {
        return format!(
            "-- Upsert skipped: key column(s) missing from result set: {}\n",
            missing.join(", ")
        );
    }

    match db.upsert_pattern() {
        UpsertPattern::Merge => merge_pattern_sql(db, &tbl, columns, key_columns, rows),
        UpsertPattern::OnConflict => insert_upsert_tail_sql(
            db,
            &tbl,
            columns,
            key_columns,
            rows,
            |_cols, keys, updates| {
                if updates.is_empty() {
                    vec![format!("ON CONFLICT ({}) DO NOTHING;", keys.join(", "))]
                } else {
                    vec![
                        format!("ON CONFLICT ({}) DO UPDATE SET", keys.join(", ")),
                        format!(
                            "{};",
                            updates
                                .iter()
                                .map(|c| format!("    {c} = EXCLUDED.{c}"))
                                .collect::<Vec<_>>()
                                .join(",\n")
                        ),
                    ]
                }
            },
        ),
        UpsertPattern::OnDuplicateKey => {
            insert_upsert_tail_sql(
                db,
                &tbl,
                columns,
                key_columns,
                rows,
                |_cols, keys, updates| {
                    let assignments = if updates.is_empty() {
                        // MySQL requires at least one assignment; a key self-
                        // assignment leaves duplicates unchanged.
                        vec![format!("    {k} = {k}", k = keys[0])]
                    } else {
                        updates
                            .iter()
                            .map(|c| format!("    {c} = VALUES({c})"))
                            .collect()
                    };
                    vec![
                        "ON DUPLICATE KEY UPDATE".to_string(),
                        format!("{};", assignments.join(",\n")),
                    ]
                },
            )
        }
        UpsertPattern::InsertOrUpdate => {
            // Spanner's single-statement form: INSERT OR UPDATE.
            let cols: Vec<String> = columns
                .iter()
                .map(|c| quote_identifier_for(db, &c.name))
                .collect();
            let mut lines = vec![
                format!("INSERT OR UPDATE INTO {} ({})", tbl, cols.join(", ")),
                "VALUES".to_string(),
            ];
            lines.extend(sql_value_rows(db, rows, columns.len(), ";"));
            lines.join("\n")
        }
        UpsertPattern::SelectThenInsertOrUpdate => format!(
            "-- {} has no single-statement upsert; check for the row first, then INSERT or UPDATE.\n-- Keys: {}\n",
            db.display_name(),
            key_columns.join(", ")
        ),
    }
}

/// The `MERGE …` shape shared by engines whose upsert is MERGE. On the SQL
/// Server family the output is byte-identical to the legacy generator
/// (bracket quoting, `[target]` alias, bare ON); elsewhere the alias is
/// plain `target` and the ON condition is parenthesized (Oracle requires it).
fn merge_pattern_sql(
    db: DatabaseType,
    tbl: &str,
    columns: &[ColumnDescriptor],
    key_columns: &[String],
    rows: &[Vec<CellValue>],
) -> String {
    let tds = matches!(db.connection_kind(), ConnectionKind::Tds);
    let escaped_cols: Vec<String> = columns
        .iter()
        .map(|c| quote_identifier_for(db, &c.name))
        .collect();
    let escaped_keys: Vec<String> = key_columns
        .iter()
        .map(|k| quote_identifier_for(db, k))
        .collect();
    let alias = "SqllyUpsertSource";
    let target_alias = if tds { "[target]" } else { "target" };

    let mut lines = Vec::new();
    lines.push(format!("MERGE {} AS {}", tbl, target_alias));
    lines.push("USING (VALUES".to_string());
    lines.extend(sql_value_rows(
        db,
        rows,
        columns.len(),
        &format!(") AS {}({})", alias, escaped_cols.join(", ")),
    ));
    let on = on_clause(target_alias, alias, &escaped_keys);
    if tds {
        lines.push(format!("ON {}", on));
    } else {
        lines.push(format!("ON ({})", on));
    }

    let update_assignments: Vec<String> = columns
        .iter()
        .zip(&escaped_cols)
        .filter(|(col, _)| !key_columns.contains(&col.name))
        .map(|(_, escaped)| format!("    {}.{} = {}.{}", target_alias, escaped, alias, escaped))
        .collect();

    if update_assignments.is_empty() {
        lines.push("WHEN MATCHED THEN".to_string());
        lines.push("    DELETE;".to_string());
    } else {
        lines.push("WHEN MATCHED THEN".to_string());
        lines.push("    UPDATE SET".to_string());
        lines.push(format!("{},", update_assignments.join(",\n")));
    }
    lines.push("WHEN NOT MATCHED THEN".to_string());
    lines.push(format!("    INSERT ({})", escaped_cols.join(", ")));
    lines.push(format!(
        "    VALUES ({})",
        escaped_cols
            .iter()
            .map(|c| format!("{}.{}", alias, c))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    lines.join("\n")
}

/// `INSERT INTO … VALUES …` followed by an engine-specific upsert tail
/// (ON CONFLICT / ON DUPLICATE KEY). The tail builder receives the quoted
/// column list, quoted key list, and quoted non-key (update) list.
fn insert_upsert_tail_sql(
    db: DatabaseType,
    tbl: &str,
    columns: &[ColumnDescriptor],
    key_columns: &[String],
    rows: &[Vec<CellValue>],
    tail: impl FnOnce(&[String], &[String], &[String]) -> Vec<String>,
) -> String {
    let quoted_cols: Vec<String> = columns
        .iter()
        .map(|c| quote_identifier_for(db, &c.name))
        .collect();
    let quoted_keys: Vec<String> = columns
        .iter()
        .filter(|c| key_columns.contains(&c.name))
        .map(|c| quote_identifier_for(db, &c.name))
        .collect();
    let quoted_updates: Vec<String> = columns
        .iter()
        .filter(|c| !key_columns.contains(&c.name))
        .map(|c| quote_identifier_for(db, &c.name))
        .collect();
    let mut lines = vec![
        format!("INSERT INTO {} ({})", tbl, quoted_cols.join(", ")),
        "VALUES".to_string(),
    ];
    lines.extend(sql_value_rows(db, rows, columns.len(), ""));
    lines.extend(tail(&quoted_cols, &quoted_keys, &quoted_updates));
    lines.join("\n")
}

/// The object kinds explorer scripting distinguishes. Mapped from the tree's
/// `NodeKind` by the caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScriptObjectKind {
    Table,
    View,
    Procedure,
    Function,
    Trigger,
    Sequence,
    Synonym,
    Extension,
    Event,
    MaterializedView,
    Constraint,
}

impl ScriptObjectKind {
    pub fn sql_keyword(self) -> &'static str {
        match self {
            ScriptObjectKind::Table => "TABLE",
            ScriptObjectKind::View => "VIEW",
            ScriptObjectKind::Procedure => "PROCEDURE",
            ScriptObjectKind::Function => "FUNCTION",
            ScriptObjectKind::Trigger => "TRIGGER",
            ScriptObjectKind::Sequence => "SEQUENCE",
            ScriptObjectKind::Synonym => "SYNONYM",
            ScriptObjectKind::Extension => "EXTENSION",
            ScriptObjectKind::Event => "EVENT",
            ScriptObjectKind::MaterializedView => "MATERIALIZED VIEW",
            ScriptObjectKind::Constraint => "CONSTRAINT",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ScriptObjectKind::Table => "table",
            ScriptObjectKind::View => "view",
            ScriptObjectKind::Procedure => "procedure",
            ScriptObjectKind::Function => "function",
            ScriptObjectKind::Trigger => "trigger",
            ScriptObjectKind::Sequence => "sequence",
            ScriptObjectKind::Synonym => "synonym",
            ScriptObjectKind::Extension => "extension",
            ScriptObjectKind::Event => "event",
            ScriptObjectKind::MaterializedView => "materialized view",
            ScriptObjectKind::Constraint => "constraint",
        }
    }
}

/// Script-first rename scaffold: the engine's own rename statement with a
/// `<new_name>` placeholder, plus a review comment. Never auto-runs.
/// Validation for a user-typed rename target. Characters that would break
/// out of the dialect quoting (quotes, brackets, backticks, control chars)
/// are rejected rather than escaped — a rename is not the place for exotic
/// identifiers.
pub fn rename_identifier_error(name: &str) -> Option<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Some("Enter the new name".to_string());
    }
    if trimmed.len() > 128 {
        return Some("Names longer than 128 characters are not supported".to_string());
    }
    if trimmed
        .chars()
        .any(|c| matches!(c, '\'' | '"' | '`' | '[' | ']' | ';') || c.is_control())
    {
        return Some(
            "The name cannot contain quotes, brackets, backticks, or semicolons".to_string(),
        );
    }
    None
}

/// The rename script with the user's validated new name substituted for the
/// `<new_name>` placeholder.
pub fn rename_sql_with_name(
    db: DatabaseType,
    kind: ScriptObjectKind,
    table: &QualifiedTable,
    new_name: &str,
) -> Result<String, String> {
    if let Some(error) = rename_identifier_error(new_name) {
        return Err(error);
    }
    Ok(rename_sql_for(db, kind, table).replace("<new_name>", new_name.trim()))
}

pub fn rename_sql_for(db: DatabaseType, kind: ScriptObjectKind, table: &QualifiedTable) -> String {
    use ScriptObjectKind as K;
    let target = table.schema_qualified_table_for(db);
    let header = format!(
        "-- Rename {} {}: replace <new_name>, review, then run.\n-- Renaming does not update objects that reference the old name.\n",
        kind.label(),
        target
    );
    match db.connection_kind() {
        ConnectionKind::Tds => {
            // sp_rename takes the current name as a string literal.
            let schema = non_empty(&table.schema).unwrap_or("dbo");
            let old = format!("{}.{}", schema, table.table).replace('\'', "''");
            format!("{header}EXEC sp_rename N'{old}', N'<new_name>';")
        }
        ConnectionKind::PostgresWire => match kind {
            K::Table => format!("{header}ALTER TABLE {target} RENAME TO \"<new_name>\";"),
            K::View => format!("{header}ALTER VIEW {target} RENAME TO \"<new_name>\";"),
            K::Procedure => format!(
                "{header}ALTER PROCEDURE {target}(/* argument types */) RENAME TO \"<new_name>\";"
            ),
            K::Function => format!(
                "{header}ALTER FUNCTION {target}(/* argument types */) RENAME TO \"<new_name>\";"
            ),
            _ => format!(
                "{header}-- Renaming a {0} is not supported; recreate it under the new name.",
                kind.label()
            ),
        },
        ConnectionKind::MySqlWire => match kind {
            K::Table | K::View => format!("{header}RENAME TABLE {target} TO `<new_name>`;"),
            K::Procedure | K::Function => format!(
                "{header}-- MySQL cannot rename a {0}; recreate it under the new name:\n-- DROP {1} IF EXISTS {target};\n-- CREATE {1} `<new_name>` …;",
                kind.label(),
                kind.sql_keyword()
            ),
            _ => format!(
                "{header}-- Renaming a {0} is not supported; recreate it under the new name.",
                kind.label()
            ),
        },
        ConnectionKind::SqliteFile | ConnectionKind::HranaHttp => match kind {
            K::Table => format!("{header}ALTER TABLE {target} RENAME TO \"<new_name>\";"),
            _ => format!(
                "{header}-- SQLite cannot rename a {0}; recreate it under the new name:\n-- DROP {1} IF EXISTS {target};\n-- CREATE {1} \"<new_name>\" …;",
                kind.label(),
                kind.sql_keyword()
            ),
        },
        _ => format!(
            "{header}ALTER {} {target} RENAME TO \"<new_name>\";",
            kind.sql_keyword()
        ),
    }
}

/// Script-first guarded DROP CONSTRAINT on the owning table.
pub fn drop_constraint_sql_for(
    db: DatabaseType,
    table: &QualifiedTable,
    constraint: &str,
) -> String {
    let tbl = table.schema_qualified_table_for(db);
    let con = quote_identifier_for(db, constraint);
    match db.connection_kind() {
        ConnectionKind::SqliteFile | ConnectionKind::HranaHttp => format!(
            "-- SQLite cannot drop constraints; recreate the table.\n-- ALTER TABLE {tbl} DROP CONSTRAINT {con};"
        ),
        _ => format!(
            "-- Review carefully: this drops constraint {con} on {tbl}.\nALTER TABLE {tbl} DROP CONSTRAINT {con};"
        ),
    }
}

/// Script-first guarded DROP: a review comment plus the engine's drop form
/// (`IF EXISTS` on the engines that support it). Never auto-runs.
pub fn drop_object_sql_for(
    db: DatabaseType,
    kind: ScriptObjectKind,
    table: &QualifiedTable,
) -> String {
    let target = table.multipart_identifier_for(db);
    let if_exists = match db.connection_kind() {
        ConnectionKind::Tds
        | ConnectionKind::PostgresWire
        | ConnectionKind::MySqlWire
        | ConnectionKind::SqliteFile
        | ConnectionKind::HranaHttp => "IF EXISTS ",
        _ => "",
    };
    format!(
        "-- Review carefully: this permanently drops {} {}.\nDROP {} {}{};",
        kind.label(),
        target,
        kind.sql_keyword(),
        if_exists,
        target
    )
}

/// Script-as-ALTER scaffold for a table: an editable `ALTER TABLE` opener
/// with the engine's own change forms one uncomment away. Tables have no
/// stored definition to round-trip, so this is a scaffold by design.
pub fn alter_table_scaffold_sql_for(db: DatabaseType, table: &QualifiedTable) -> String {
    let target = table.schema_qualified_table_for(db);
    let text_type = db.map_type(LogicalType::Text).native_name;
    let mut lines = vec![
        format!("-- ALTER scaffold for {target}: uncomment and edit the change you need."),
        format!("ALTER TABLE {target}"),
        format!(
            "  -- ADD {} {text_type}",
            quote_identifier_for(db, "new_column")
        ),
    ];
    match db.connection_kind() {
        ConnectionKind::Tds => {
            lines.push(format!(
                "  -- ALTER COLUMN {} {text_type}",
                quote_identifier_for(db, "existing_column")
            ));
            lines.push(format!(
                "  -- DROP COLUMN {}",
                quote_identifier_for(db, "existing_column")
            ));
        }
        ConnectionKind::PostgresWire => {
            lines.push(format!(
                "  -- ALTER COLUMN {} TYPE {text_type}",
                quote_identifier_for(db, "existing_column")
            ));
            lines.push(format!(
                "  -- DROP COLUMN {}",
                quote_identifier_for(db, "existing_column")
            ));
        }
        ConnectionKind::MySqlWire => {
            lines.push(format!(
                "  -- MODIFY {} {text_type}",
                quote_identifier_for(db, "existing_column")
            ));
            lines.push(format!(
                "  -- DROP COLUMN {}",
                quote_identifier_for(db, "existing_column")
            ));
        }
        ConnectionKind::SqliteFile | ConnectionKind::HranaHttp => {
            lines.push(format!(
                "  -- RENAME COLUMN {} TO {}",
                quote_identifier_for(db, "existing_column"),
                quote_identifier_for(db, "new_name")
            ));
            lines.push(
                "  -- SQLite cannot alter or drop columns in place; recreate the table for those changes."
                    .to_string(),
            );
        }
        _ => {
            lines.push(format!(
                "  -- DROP COLUMN {}",
                quote_identifier_for(db, "existing_column")
            ));
        }
    }
    lines.push(";".to_string());
    lines.join("\n")
}

/// Turn a fetched CREATE definition into the engine's alter form: `CREATE OR
/// ALTER` on SQL Server, `CREATE OR REPLACE` on PostgreSQL (and MySQL views);
/// engines without an in-place form get a guarded `DROP … IF EXISTS` ahead
/// of the original CREATE. When no CREATE token is found the definition is
/// returned with an explanatory comment for manual editing.
pub fn alter_from_definition_for(
    db: DatabaseType,
    kind: ScriptObjectKind,
    table: &QualifiedTable,
    definition: &str,
) -> String {
    let Some(pos) = find_create_token(definition) else {
        return format!(
            "-- Could not find CREATE in the fetched definition; edit manually.\n{definition}"
        );
    };
    let already_or_alter = definition[pos..]
        .get(..20)
        .map(|s| s.to_ascii_uppercase())
        .is_some_and(|s| s.starts_with("CREATE OR"));
    if already_or_alter {
        return definition.to_string();
    }
    // SQL Server stores `CREATE OR ALTER VIEW` as `CREATE   VIEW` (the
    // OR ALTER blanked out); keeping that gap would add two spaces to the
    // stored source on every Script as ALTER round trip, so a
    // whitespace-only gap after CREATE becomes the single space after the
    // replacement.
    let after_create = pos + "CREATE".len();
    let rest = &definition[after_create..];
    let gap = rest.len() - rest.trim_start().len();
    let splice = |replacement: &str| -> String {
        let mut out = String::with_capacity(definition.len() + replacement.len());
        out.push_str(&definition[..pos]);
        out.push_str(replacement);
        if gap > 0 {
            out.push(' ');
        }
        out.push_str(&definition[after_create + gap..]);
        out
    };
    match db.connection_kind() {
        ConnectionKind::Tds => splice("CREATE OR ALTER"),
        ConnectionKind::PostgresWire => splice("CREATE OR REPLACE"),
        ConnectionKind::MySqlWire if kind == ScriptObjectKind::View => splice("CREATE OR REPLACE"),
        _ => format!(
            "-- {} has no in-place alter for a {}; this recreates it.\nDROP {} IF EXISTS {};\n\n{}",
            db.display_name(),
            kind.label(),
            kind.sql_keyword(),
            table.schema_qualified_table_for(db),
            definition
        ),
    }
}

/// Byte offset of the first standalone CREATE token outside line comments.
fn find_create_token(sql: &str) -> Option<usize> {
    let bytes = sql.as_bytes();
    let upper = sql.to_ascii_uppercase();
    let mut search = 0;
    while let Some(rel) = upper[search..].find("CREATE") {
        let pos = search + rel;
        let before_ok =
            pos == 0 || !bytes[pos - 1].is_ascii_alphanumeric() && bytes[pos - 1] != b'_';
        let after = pos + "CREATE".len();
        let after_ok =
            after >= bytes.len() || (!bytes[after].is_ascii_alphanumeric() && bytes[after] != b'_');
        // Skip matches sitting inside a line comment.
        let line_start = sql[..pos].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let in_line_comment = sql[line_start..pos].contains("--");
        if before_ok && after_ok && !in_line_comment {
            return Some(pos);
        }
        search = after;
    }
    None
}

// ── Helpers ──────────────────────────────────────────────────────────────

fn non_empty(s: &Option<String>) -> Option<&str> {
    s.as_deref().and_then(|s| {
        let t = s.trim();
        if t.is_empty() { None } else { Some(t) }
    })
}

fn sql_type_for(db: DatabaseType, col: &ColumnDescriptor) -> String {
    let trimmed = col.data_type.trim();
    if trimmed.is_empty() || trimmed == "(unknown)" || trimmed == "(col)" {
        return db.map_type(LogicalType::Text).native_name.to_string();
    }
    trimmed.to_string()
}

fn sql_value_rows(
    db: DatabaseType,
    rows: &[Vec<CellValue>],
    column_count: usize,
    closing_suffix: &str,
) -> Vec<String> {
    rows.iter()
        .enumerate()
        .map(|(row_idx, row)| {
            let values: Vec<String> = (0..column_count)
                .map(|col| sql_literal_for(db, value_at(row, col)))
                .collect();
            let suffix = if row_idx == rows.len() - 1 {
                closing_suffix
            } else {
                ","
            };
            format!("    ({}){}", values.join(", "), suffix)
        })
        .collect()
}

fn value_at(row: &[CellValue], col: usize) -> &CellValue {
    row.get(col).unwrap_or(&CellValue::Null)
}

fn on_clause(target_alias: &str, alias: &str, key_columns: &[String]) -> String {
    let predicates: Vec<String> = key_columns
        .iter()
        .map(|key| {
            format!(
                "{}.{} = {}.{}\n        OR ({}.{} IS NULL AND {}.{} IS NULL)",
                target_alias, key, alias, key, target_alias, key, alias, key
            )
        })
        .collect();
    predicates.join("\n        OR ")
}

/// "Filter Data" scaffold (T-SQL; tests only).
#[cfg(test)]
pub fn filter_data_sql(table: &QualifiedTable, columns: &[ColumnDescriptor]) -> String {
    filter_data_sql_for(DatabaseType::SqlServer, table, columns)
}

/// "Filter Data" scaffold: a reviewed SELECT over the table with one
/// commented, type-aware predicate per known column — uncomment and fill the
/// ones you need. `WHERE 1=1` keeps every uncommented line a plain `AND`.
/// Renders with the engine's row limiting, quoting, and literal forms.
/// Opens in a NEW tab by contract; this never touches existing editor text.
pub fn filter_data_sql_for(
    db: DatabaseType,
    table: &QualifiedTable,
    columns: &[ColumnDescriptor],
) -> String {
    let (limit_prefix, limit_suffix) = db.row_limit_clauses(1000);
    let mut sql = format!(
        "SELECT {limit_prefix}*\nFROM {}\nWHERE 1=1\n",
        table.schema_qualified_table_for(db)
    );
    if columns.is_empty() {
        sql.push_str("  -- AND <column> = <value>\n");
    } else {
        for column in columns {
            let ident = quote_identifier_for(db, &column.name);
            let ty = column.data_type.to_ascii_lowercase();
            let predicate = if ty.contains("char") || ty.contains("text") {
                format!("{ident} LIKE {}", db.string_literal("%…%"))
            } else if ty.contains("date") || ty.contains("time") {
                format!("{ident} >= '…'")
            } else if ty == "bit" || ty.contains("bool") {
                format!("{ident} = {}", db.bool_literal(true))
            } else if ty.contains("uniqueidentifier") || ty == "uuid" {
                format!("{ident} = '…'")
            } else {
                format!("{ident} = …")
            };
            sql.push_str(&format!("  -- AND {predicate}    ({})\n", column.data_type));
        }
    }
    if !limit_suffix.is_empty() {
        sql.push_str(&limit_suffix);
        sql.push('\n');
    }
    sql
}

/// EXEC script for a stored procedure (T-SQL; tests only).
#[cfg(test)]
pub fn procedure_invocation_sql(
    schema: &str,
    name: &str,
    info: Option<&sqlly_services::ProcedureInfo>,
) -> String {
    procedure_invocation_sql_for(DatabaseType::SqlServer, schema, name, info)
}

/// Invocation script for a stored procedure in the engine's own call form:
/// `EXEC` on the SQL Server family (with typed parameter placeholders when
/// the loaded IntelliSense catalog knows the object), `CALL` elsewhere. The
/// typed-parameter snippet is T-SQL, so only the SQL Server family uses it.
pub fn procedure_invocation_sql_for(
    db: DatabaseType,
    schema: &str,
    name: &str,
    info: Option<&sqlly_services::ProcedureInfo>,
) -> String {
    if matches!(db.connection_kind(), ConnectionKind::Tds) {
        return match info {
            Some(info) => {
                let snippet = info.declaration_snippet();
                if snippet.trim_end().ends_with(';') || snippet.contains("; --") {
                    snippet
                } else {
                    format!("{snippet};")
                }
            }
            None => format!(
                "EXEC {}.{}; -- add parameters as needed",
                escape_identifier(schema),
                escape_identifier(name)
            ),
        };
    }
    let qualified = qualified_routine_for(db, schema, name);
    format!("CALL {qualified}(/* args */);")
}

/// Invocation scaffold for a user-defined function (T-SQL; tests only).
#[cfg(test)]
pub fn function_invocation_sql(schema: &str, name: &str) -> String {
    function_invocation_sql_for(DatabaseType::SqlServer, schema, name)
}

/// Invocation scaffold for a user-defined function in the engine's own call
/// form. The tree does not record whether the function is scalar or
/// table-valued, so engines with table functions get the table form runnable
/// and the scalar form one comment away; the MySQL lineage (scalar-only
/// functions) gets the scalar form directly.
pub fn function_invocation_sql_for(db: DatabaseType, schema: &str, name: &str) -> String {
    let qualified = qualified_routine_for(db, schema, name);
    if matches!(db.connection_kind(), ConnectionKind::MySqlWire) {
        return format!("SELECT {qualified}(/* args */) AS result;");
    }
    format!(
        "-- Scalar form: SELECT {qualified}(/* args */) AS result;\nSELECT * FROM {qualified}(/* args */);"
    )
}

/// `schema.name` (or bare `name` where the schema slot is blank) quoted for
/// the engine. Keeps the legacy bracket pass-through on the SQL Server
/// family.
fn qualified_routine_for(db: DatabaseType, schema: &str, name: &str) -> String {
    if schema.trim().is_empty() {
        quote_identifier_for(db, name)
    } else {
        format!(
            "{}.{}",
            quote_identifier_for(db, schema),
            quote_identifier_for(db, name)
        )
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// ClickHouse reads backslash escapes inside backticks: generated
    /// statements must escape them so a hostile name stays one identifier.
    #[test]
    fn clickhouse_generated_sql_escapes_backslashes_in_identifiers() {
        let mut t = QualifiedTable::new(r"t\");
        t.schema = Some(r"db\".to_string());
        assert_eq!(
            view_data_sql_for(DatabaseType::ClickHouse, &t),
            "SELECT *\nFROM `db\\\\`.`t\\\\`"
        );
        assert!(count_rows_sql_for(DatabaseType::ClickHouse, &t).contains(r"`db\\`.`t\\`"));
    }

    #[test]
    fn escape_identifier_wraps_name() {
        assert_eq!(escape_identifier("Customers"), "[Customers]");
    }

    #[test]
    fn escape_identifier_doubles_brackets() {
        assert_eq!(escape_identifier("my]table"), "[my]]table]");
    }

    #[test]
    fn escape_identifier_passes_already_bracketed() {
        assert_eq!(escape_identifier("[Customers]"), "[Customers]");
    }

    #[test]
    fn escape_identifier_rescapes_malformed_bracketed() {
        assert_eq!(escape_identifier("[a]b]"), "[a]]b]");
    }

    #[test]
    fn escape_identifier_roundtrips_well_escaped_bracketed() {
        assert_eq!(escape_identifier("[my]]col]"), "[my]]col]");
    }

    #[test]
    fn escape_identifier_bracket_only_falls_back() {
        assert_eq!(escape_identifier("[]"), "[Column]");
    }

    #[test]
    fn escape_identifier_empty_returns_column() {
        assert_eq!(escape_identifier(""), "[Column]");
        assert_eq!(escape_identifier("   "), "[Column]");
    }

    #[test]
    fn qualified_table_multipart_identifier() {
        let t = QualifiedTable::new("Orders")
            .with_schema("dbo")
            .with_database("Sales");
        assert_eq!(t.multipart_identifier(), "[Sales].[dbo].[Orders]");
    }

    #[test]
    fn qualified_table_defaults_schema_to_dbo() {
        let t = QualifiedTable::new("Orders");
        assert_eq!(t.resolved_schema(), "dbo");
        assert_eq!(t.schema_qualified_table(), "[dbo].[Orders]");
    }

    #[test]
    fn qualified_table_explicit_schema() {
        let t = QualifiedTable::new("Orders").with_schema("sales");
        assert_eq!(t.schema_qualified_table(), "[sales].[Orders]");
    }

    #[test]
    fn select_top_rows_sql_basic() {
        let t = QualifiedTable::new("Orders").with_schema("dbo");
        assert_eq!(
            select_top_rows_sql(1000, &t),
            "SELECT TOP 1000 *\nFROM [dbo].[Orders]"
        );
    }

    #[test]
    fn select_top_rows_sql_clamps_to_one() {
        let t = QualifiedTable::new("T");
        assert!(select_top_rows_sql(0, &t).contains("SELECT TOP 1"));
    }

    #[test]
    fn select_top_rows_sql_for_sqlite_uses_limit_and_no_schema() {
        let t = QualifiedTable::new("Orders");
        assert_eq!(
            select_top_rows_sql_for(DatabaseType::Sqlite, 1000, &t),
            "SELECT *\nFROM \"Orders\"\nLIMIT 1000"
        );
    }

    #[test]
    fn select_top_rows_sql_for_postgres_defaults_public_schema() {
        let t = QualifiedTable::new("orders");
        assert_eq!(
            select_top_rows_sql_for(DatabaseType::Postgres, 1000, &t),
            "SELECT *\nFROM \"public\".\"orders\"\nLIMIT 1000"
        );
    }

    #[test]
    fn select_top_rows_sql_for_oracle_uses_fetch_first() {
        let t = QualifiedTable::new("ORDERS").with_schema("HR");
        assert_eq!(
            select_top_rows_sql_for(DatabaseType::Oracle, 1000, &t),
            "SELECT *\nFROM \"HR\".\"ORDERS\"\nFETCH FIRST 1000 ROWS ONLY"
        );
    }

    #[test]
    fn select_top_rows_sql_for_mysql_uses_backticks() {
        let t = QualifiedTable::new("orders");
        assert_eq!(
            select_top_rows_sql_for(DatabaseType::MySql, 1000, &t),
            "SELECT *\nFROM `orders`\nLIMIT 1000"
        );
    }

    #[test]
    fn select_top_rows_sql_for_sql_server_matches_legacy_output() {
        let t = QualifiedTable::new("Orders").with_schema("dbo");
        assert_eq!(
            select_top_rows_sql_for(DatabaseType::SqlServer, 1000, &t),
            select_top_rows_sql(1000, &t)
        );
    }

    #[test]
    fn delete_sql_basic() {
        let t = QualifiedTable::new("Orders").with_schema("dbo");
        assert_eq!(delete_sql(&t), "DELETE FROM [dbo].[Orders]");
    }

    #[test]
    fn truncate_sql_basic() {
        let t = QualifiedTable::new("Orders").with_schema("dbo");
        assert_eq!(truncate_sql(&t), "TRUNCATE TABLE [dbo].[Orders]");
    }

    #[test]
    fn drop_table_sql_includes_database() {
        let t = QualifiedTable::new("Orders")
            .with_schema("dbo")
            .with_database("Sales");
        assert_eq!(drop_table_sql(&t), "DROP TABLE [Sales].[dbo].[Orders]");
    }

    #[test]
    fn sql_literal_null() {
        assert_eq!(sql_literal(&CellValue::Null), "NULL");
    }

    #[test]
    fn sql_literal_bool() {
        assert_eq!(sql_literal(&CellValue::Bool(true)), "1");
        assert_eq!(sql_literal(&CellValue::Bool(false)), "0");
    }

    #[test]
    fn sql_literal_integer() {
        assert_eq!(sql_literal(&CellValue::I64(42)), "42");
    }

    #[test]
    fn sql_literal_string_doubles_quotes() {
        assert_eq!(
            sql_literal(&CellValue::String("O'Brien".to_string())),
            "N'O''Brien'"
        );
    }

    #[test]
    fn sql_literal_bytes() {
        assert_eq!(sql_literal(&CellValue::Bytes(vec![0xDE, 0xAD])), "0xDEAD");
    }

    #[test]
    fn sql_literal_non_finite_double() {
        assert_eq!(sql_literal(&CellValue::F64(f64::NAN)), "NULL");
        assert_eq!(sql_literal(&CellValue::F64(f64::INFINITY)), "NULL");
    }

    #[test]
    fn create_table_ddl_empty_columns() {
        let t = QualifiedTable::new("Foo");
        assert_eq!(create_table_ddl(&t, &[]), "CREATE TABLE [Foo] (\n);\n");
    }

    #[test]
    fn create_table_ddl_with_columns() {
        let t = QualifiedTable::new("Foo").with_schema("dbo");
        let cols = vec![
            ColumnDescriptor {
                name: "id".to_string(),
                data_type: "int".to_string(),
                nullable: false,
            },
            ColumnDescriptor {
                name: "name".to_string(),
                data_type: "nvarchar(100)".to_string(),
                nullable: true,
            },
        ];
        let ddl = create_table_ddl(&t, &cols);
        assert!(ddl.contains("CREATE TABLE [dbo].[Foo] ("));
        assert!(ddl.contains("[id] int NOT NULL,"));
        assert!(ddl.contains("[name] nvarchar(100) NULL"));
        assert!(ddl.ends_with(");"));
    }

    #[test]
    fn insert_into_empty_rows() {
        let t = QualifiedTable::new("Foo");
        let cols = vec![ColumnDescriptor {
            name: "id".to_string(),
            data_type: "int".to_string(),
            nullable: false,
        }];
        let result = insert_into_sql(&t, &cols, &[], &[]);
        assert!(result.starts_with("-- No rows"));
    }

    #[test]
    fn insert_into_with_identity_guard() {
        let t = QualifiedTable::new("Foo").with_schema("dbo");
        let cols = vec![
            ColumnDescriptor {
                name: "id".to_string(),
                data_type: "int".to_string(),
                nullable: false,
            },
            ColumnDescriptor {
                name: "name".to_string(),
                data_type: "nvarchar(50)".to_string(),
                nullable: true,
            },
        ];
        let rows = vec![vec![
            CellValue::I64(1),
            CellValue::String("Alice".to_string()),
        ]];
        let sql = insert_into_sql(&t, &cols, &["id".to_string()], &rows);
        assert!(sql.contains("SET IDENTITY_INSERT [dbo].[Foo] ON;"));
        assert!(sql.contains("INSERT INTO [dbo].[Foo] ([id], [name])"));
        assert!(sql.contains("(1, N'Alice')"));
        assert!(sql.contains("SET IDENTITY_INSERT [dbo].[Foo] OFF;"));
    }

    #[test]
    fn merge_upsert_empty_keys() {
        let t = QualifiedTable::new("Foo");
        let cols = vec![ColumnDescriptor {
            name: "id".to_string(),
            data_type: "int".to_string(),
            nullable: false,
        }];
        let rows = vec![vec![CellValue::I64(1)]];
        let sql = merge_upsert_sql(&t, &cols, &[], &rows);
        assert!(sql.contains("Upsert skipped: no key columns"));
    }

    #[test]
    fn merge_upsert_missing_key_column() {
        let t = QualifiedTable::new("Foo");
        let cols = vec![ColumnDescriptor {
            name: "id".to_string(),
            data_type: "int".to_string(),
            nullable: false,
        }];
        let rows = vec![vec![CellValue::I64(1)]];
        let sql = merge_upsert_sql(&t, &cols, &["missing_col".to_string()], &rows);
        assert!(sql.contains("key column(s) missing"));
    }

    #[test]
    fn merge_upsert_generates_update_and_insert() {
        let t = QualifiedTable::new("Foo").with_schema("dbo");
        let cols = vec![
            ColumnDescriptor {
                name: "id".to_string(),
                data_type: "int".to_string(),
                nullable: false,
            },
            ColumnDescriptor {
                name: "name".to_string(),
                data_type: "nvarchar(50)".to_string(),
                nullable: true,
            },
        ];
        let rows = vec![vec![
            CellValue::I64(1),
            CellValue::String("Alice".to_string()),
        ]];
        let sql = merge_upsert_sql(&t, &cols, &["id".to_string()], &rows);
        assert!(sql.contains("MERGE [dbo].[Foo] AS [target]"));
        assert!(sql.contains("USING (VALUES"));
        assert!(sql.contains("ON [target].[id] = SqllyUpsertSource.[id]"));
        assert!(sql.contains("WHEN MATCHED THEN"));
        assert!(sql.contains("UPDATE SET"));
        assert!(sql.contains("[target].[name] = SqllyUpsertSource.[name]"));
        assert!(sql.contains("WHEN NOT MATCHED THEN"));
        assert!(sql.contains("INSERT ([id], [name])"));
    }

    #[test]
    fn identity_insert_guard_on() {
        let t = QualifiedTable::new("Foo").with_schema("dbo");
        assert_eq!(
            identity_insert_guard(&t, true),
            "SET IDENTITY_INSERT [dbo].[Foo] ON;"
        );
    }

    #[test]
    fn identity_insert_guard_off() {
        let t = QualifiedTable::new("Foo").with_schema("dbo");
        assert_eq!(
            identity_insert_guard(&t, false),
            "SET IDENTITY_INSERT [dbo].[Foo] OFF;"
        );
    }

    #[test]
    fn count_rows_generates_correct_sql() {
        let t = QualifiedTable::new("Orders").with_schema("dbo");
        let sql = count_rows_sql(&t);
        assert!(sql.contains("SELECT COUNT(*)"));
        assert!(sql.contains("AS [RowCount]"));
        assert!(sql.contains("FROM [dbo].[Orders]"));
    }

    #[test]
    fn count_rows_with_database_qualified() {
        let t = QualifiedTable::new("Orders")
            .with_schema("dbo")
            .with_database("Sales");
        let sql = count_rows_sql(&t);
        assert!(sql.contains("FROM [dbo].[Orders]"));
    }

    #[test]
    fn view_data_generates_select_all() {
        let t = QualifiedTable::new("Orders").with_schema("dbo");
        let sql = view_data_sql(&t);
        assert!(sql.contains("SELECT *"));
        assert!(sql.contains("FROM [dbo].[Orders]"));
    }

    #[test]
    fn view_data_with_database_qualified() {
        let t = QualifiedTable::new("Orders")
            .with_schema("dbo")
            .with_database("Sales");
        let sql = view_data_sql(&t);
        assert!(sql.contains("FROM [dbo].[Orders]"));
    }

    #[test]
    fn filter_data_scaffold_is_type_aware() {
        let t = QualifiedTable::new("Orders").with_schema("dbo");
        let cols = vec![
            ColumnDescriptor {
                name: "OrderId".into(),
                data_type: "int".into(),
                nullable: false,
            },
            ColumnDescriptor {
                name: "CustomerName".into(),
                data_type: "nvarchar(200)".into(),
                nullable: true,
            },
            ColumnDescriptor {
                name: "CreatedAt".into(),
                data_type: "datetime2".into(),
                nullable: false,
            },
            ColumnDescriptor {
                name: "IsActive".into(),
                data_type: "bit".into(),
                nullable: false,
            },
        ];
        let sql = filter_data_sql(&t, &cols);
        assert!(sql.starts_with("SELECT TOP 1000 *\nFROM [dbo].[Orders]\nWHERE 1=1\n"));
        assert!(sql.contains("-- AND [OrderId] ="), "{sql}");
        assert!(sql.contains("[CustomerName] LIKE N'%"), "{sql}");
        assert!(sql.contains("[CreatedAt] >="), "{sql}");
        assert!(sql.contains("[IsActive] = 1"), "{sql}");
    }

    #[test]
    fn filter_data_scaffold_without_columns_still_valid() {
        let t = QualifiedTable::new("Orders").with_schema("dbo");
        let sql = filter_data_sql(&t, &[]);
        assert!(sql.contains("WHERE 1=1"));
        assert!(sql.contains("-- AND <column>"));
    }

    #[test]
    fn procedure_invocation_without_catalog_is_a_runnable_scaffold() {
        let sql = procedure_invocation_sql("dbo", "GetUser", None);
        assert!(sql.starts_with("EXEC [dbo].[GetUser];"), "{sql}");
        assert!(sql.contains("add parameters"));
    }

    #[test]
    fn procedure_invocation_uses_catalog_parameters() {
        let mut info = sqlly_services::ProcedureInfo::new("dbo", "GetUser");
        info.parameters
            .push(sqlly_services::ProcedureParameter::new("@id", "int"));
        let sql = procedure_invocation_sql("dbo", "GetUser", Some(&info));
        assert!(sql.contains("EXEC [dbo].[GetUser]"), "{sql}");
        assert!(sql.contains("@id ="), "{sql}");
        assert!(sql.trim_end().ends_with(';'), "{sql}");
    }

    #[test]
    fn procedure_invocation_escapes_bracket_in_name() {
        let sql = procedure_invocation_sql("dbo", "weird]proc", None);
        assert!(sql.contains("[weird]]proc]"), "{sql}");
    }

    #[test]
    fn function_invocation_offers_both_forms() {
        let sql = function_invocation_sql("dbo", "GetOrders");
        assert!(sql.contains("SELECT * FROM [dbo].[GetOrders]("), "{sql}");
        assert!(sql.contains("-- Scalar form:"), "{sql}");
    }

    // ── Per-engine generator coverage ───────────────────────────────────

    fn pg_table() -> QualifiedTable {
        QualifiedTable::new("orders").with_schema("public")
    }

    #[test]
    fn count_rows_sql_per_engine() {
        assert_eq!(
            count_rows_sql_for(DatabaseType::Postgres, &pg_table()),
            "SELECT COUNT(*) AS \"RowCount\"\nFROM \"public\".\"orders\""
        );
        assert_eq!(
            count_rows_sql_for(DatabaseType::MySql, &QualifiedTable::new("orders")),
            "SELECT COUNT(*) AS `RowCount`\nFROM `orders`"
        );
        // SQL Server output is byte-identical to the legacy generator.
        let t = QualifiedTable::new("Orders").with_schema("dbo");
        assert_eq!(
            count_rows_sql_for(DatabaseType::SqlServer, &t),
            count_rows_sql(&t)
        );
    }

    #[test]
    fn view_and_delete_sql_per_engine() {
        assert_eq!(
            view_data_sql_for(DatabaseType::Sqlite, &QualifiedTable::new("Orders")),
            "SELECT *\nFROM \"Orders\""
        );
        assert_eq!(
            delete_sql_for(DatabaseType::Postgres, &pg_table()),
            "DELETE FROM \"public\".\"orders\""
        );
    }

    #[test]
    fn truncate_sql_degrades_to_delete_where_unsupported() {
        assert_eq!(
            truncate_sql_for(DatabaseType::Postgres, &pg_table()),
            "TRUNCATE TABLE \"public\".\"orders\""
        );
        let sqlite = truncate_sql_for(DatabaseType::Sqlite, &QualifiedTable::new("Orders"));
        assert!(sqlite.contains("has no TRUNCATE TABLE"), "{sqlite}");
        assert!(sqlite.contains("DELETE FROM \"Orders\""), "{sqlite}");
    }

    #[test]
    fn drop_table_sql_skips_database_qualifier_off_sql_server() {
        let t = QualifiedTable::new("orders")
            .with_schema("public")
            .with_database("Sales");
        assert_eq!(
            drop_table_sql_for(DatabaseType::Postgres, &t),
            "DROP TABLE \"public\".\"orders\""
        );
        let t2 = QualifiedTable::new("Orders")
            .with_schema("dbo")
            .with_database("Sales");
        assert_eq!(
            drop_table_sql_for(DatabaseType::SqlServer, &t2),
            "DROP TABLE [Sales].[dbo].[Orders]"
        );
    }

    #[test]
    fn create_table_ddl_uses_engine_quoting_and_text_fallback() {
        let cols = vec![ColumnDescriptor {
            name: "note".into(),
            data_type: "(unknown)".into(),
            nullable: true,
        }];
        let pg = create_table_ddl_for(DatabaseType::Postgres, &pg_table(), &cols);
        assert!(pg.contains("CREATE TABLE \"public\".\"orders\" ("), "{pg}");
        assert!(pg.contains("\"note\" text NULL"), "{pg}");
        let my = create_table_ddl_for(DatabaseType::MySql, &QualifiedTable::new("orders"), &cols);
        assert!(my.contains("`note` longtext NULL"), "{my}");
    }

    #[test]
    fn insert_into_sql_for_postgres_drops_identity_guard_and_n_prefix() {
        let t = pg_table();
        let cols = vec![
            ColumnDescriptor {
                name: "id".into(),
                data_type: "integer".into(),
                nullable: false,
            },
            ColumnDescriptor {
                name: "name".into(),
                data_type: "text".into(),
                nullable: true,
            },
        ];
        let rows = vec![vec![CellValue::I64(1), CellValue::String("Alice".into())]];
        let sql = insert_into_sql_for(DatabaseType::Postgres, &t, &cols, &["id".into()], &rows);
        assert!(!sql.contains("IDENTITY_INSERT"), "{sql}");
        assert!(
            sql.contains("INSERT INTO \"public\".\"orders\" (\"id\", \"name\")"),
            "{sql}"
        );
        assert!(sql.contains("(1, 'Alice');"), "{sql}");
    }

    #[test]
    fn upsert_sql_for_postgres_uses_on_conflict() {
        let cols = vec![
            ColumnDescriptor {
                name: "id".into(),
                data_type: "integer".into(),
                nullable: false,
            },
            ColumnDescriptor {
                name: "name".into(),
                data_type: "text".into(),
                nullable: true,
            },
        ];
        let rows = vec![vec![CellValue::I64(1), CellValue::String("Alice".into())]];
        let sql = upsert_sql_for(
            DatabaseType::Postgres,
            &pg_table(),
            &cols,
            &["id".into()],
            &rows,
        );
        assert!(sql.contains("INSERT INTO \"public\".\"orders\""), "{sql}");
        assert!(sql.contains("ON CONFLICT (\"id\") DO UPDATE SET"), "{sql}");
        assert!(sql.contains("\"name\" = EXCLUDED.\"name\";"), "{sql}");
        assert!(!sql.contains("MERGE"), "{sql}");
    }

    #[test]
    fn upsert_sql_for_postgres_all_key_columns_does_nothing() {
        let cols = vec![ColumnDescriptor {
            name: "id".into(),
            data_type: "integer".into(),
            nullable: false,
        }];
        let rows = vec![vec![CellValue::I64(1)]];
        let sql = upsert_sql_for(
            DatabaseType::Postgres,
            &pg_table(),
            &cols,
            &["id".into()],
            &rows,
        );
        assert!(sql.contains("ON CONFLICT (\"id\") DO NOTHING;"), "{sql}");
    }

    #[test]
    fn upsert_sql_for_mysql_uses_on_duplicate_key() {
        let cols = vec![
            ColumnDescriptor {
                name: "id".into(),
                data_type: "int".into(),
                nullable: false,
            },
            ColumnDescriptor {
                name: "name".into(),
                data_type: "varchar(50)".into(),
                nullable: true,
            },
        ];
        let rows = vec![vec![CellValue::I64(1), CellValue::String("Alice".into())]];
        let sql = upsert_sql_for(
            DatabaseType::MySql,
            &QualifiedTable::new("orders"),
            &cols,
            &["id".into()],
            &rows,
        );
        assert!(sql.contains("INSERT INTO `orders` (`id`, `name`)"), "{sql}");
        assert!(sql.contains("ON DUPLICATE KEY UPDATE"), "{sql}");
        assert!(sql.contains("`name` = VALUES(`name`);"), "{sql}");
    }

    #[test]
    fn upsert_sql_for_sqlite_uses_on_conflict() {
        let cols = vec![
            ColumnDescriptor {
                name: "id".into(),
                data_type: "integer".into(),
                nullable: false,
            },
            ColumnDescriptor {
                name: "name".into(),
                data_type: "text".into(),
                nullable: true,
            },
        ];
        let rows = vec![vec![CellValue::I64(1), CellValue::String("Alice".into())]];
        let sql = upsert_sql_for(
            DatabaseType::Sqlite,
            &QualifiedTable::new("orders"),
            &cols,
            &["id".into()],
            &rows,
        );
        assert!(sql.contains("ON CONFLICT (\"id\") DO UPDATE SET"), "{sql}");
    }

    #[test]
    fn upsert_sql_for_sql_server_is_byte_identical_to_legacy_merge() {
        let t = QualifiedTable::new("Foo").with_schema("dbo");
        let cols = vec![
            ColumnDescriptor {
                name: "id".into(),
                data_type: "int".into(),
                nullable: false,
            },
            ColumnDescriptor {
                name: "name".into(),
                data_type: "nvarchar(50)".into(),
                nullable: true,
            },
        ];
        let rows = vec![vec![CellValue::I64(1), CellValue::String("Alice".into())]];
        let sql = upsert_sql_for(DatabaseType::SqlServer, &t, &cols, &["id".into()], &rows);
        assert!(sql.contains("MERGE [dbo].[Foo] AS [target]"), "{sql}");
        assert!(
            sql.contains("ON [target].[id] = SqllyUpsertSource.[id]"),
            "{sql}"
        );
    }

    #[test]
    fn upsert_sql_for_oracle_merge_parenthesizes_on() {
        let cols = vec![
            ColumnDescriptor {
                name: "ID".into(),
                data_type: "number".into(),
                nullable: false,
            },
            ColumnDescriptor {
                name: "NAME".into(),
                data_type: "varchar2(50)".into(),
                nullable: true,
            },
        ];
        let rows = vec![vec![CellValue::I64(1), CellValue::String("Alice".into())]];
        let sql = upsert_sql_for(
            DatabaseType::Oracle,
            &QualifiedTable::new("ORDERS").with_schema("HR"),
            &cols,
            &["ID".into()],
            &rows,
        );
        assert!(sql.contains("MERGE \"HR\".\"ORDERS\" AS target"), "{sql}");
        assert!(sql.contains("ON (target.\"ID\""), "{sql}");
    }

    #[test]
    fn filter_data_sql_for_limit_engines_appends_trailing_clause() {
        let cols = vec![ColumnDescriptor {
            name: "name".into(),
            data_type: "text".into(),
            nullable: true,
        }];
        let sql = filter_data_sql_for(DatabaseType::Postgres, &pg_table(), &cols);
        assert!(
            sql.starts_with("SELECT *\nFROM \"public\".\"orders\"\nWHERE 1=1\n"),
            "{sql}"
        );
        assert!(sql.contains("\"name\" LIKE '%…%'"), "{sql}");
        assert!(sql.trim_end().ends_with("LIMIT 1000"), "{sql}");
        // Legacy SQL Server shape is unchanged.
        let t = QualifiedTable::new("Orders").with_schema("dbo");
        let mssql = filter_data_sql_for(DatabaseType::SqlServer, &t, &[]);
        assert!(
            mssql.starts_with("SELECT TOP 1000 *\nFROM [dbo].[Orders]\nWHERE 1=1\n"),
            "{mssql}"
        );
    }

    #[test]
    fn filter_data_sql_for_postgres_boolean_predicate() {
        let cols = vec![ColumnDescriptor {
            name: "active".into(),
            data_type: "boolean".into(),
            nullable: false,
        }];
        let sql = filter_data_sql_for(DatabaseType::Postgres, &pg_table(), &cols);
        assert!(sql.contains("\"active\" = TRUE"), "{sql}");
    }

    #[test]
    fn procedure_invocation_sql_for_non_tsql_uses_call() {
        let pg = procedure_invocation_sql_for(DatabaseType::Postgres, "public", "sync_users", None);
        assert_eq!(pg, "CALL \"public\".\"sync_users\"(/* args */);");
        let my = procedure_invocation_sql_for(DatabaseType::MySql, "", "sync_users", None);
        assert_eq!(my, "CALL `sync_users`(/* args */);");
    }

    #[test]
    fn function_invocation_sql_for_mysql_is_scalar_only() {
        let sql = function_invocation_sql_for(DatabaseType::MySql, "", "total_for");
        assert_eq!(sql, "SELECT `total_for`(/* args */) AS result;");
        let pg = function_invocation_sql_for(DatabaseType::Postgres, "public", "total_for");
        assert!(
            pg.contains("SELECT * FROM \"public\".\"total_for\"("),
            "{pg}"
        );
    }

    #[test]
    fn rename_sql_per_engine() {
        let t = QualifiedTable::new("Orders").with_schema("dbo");
        let mssql = rename_sql_for(DatabaseType::SqlServer, ScriptObjectKind::Table, &t);
        assert!(
            mssql.contains("EXEC sp_rename N'dbo.Orders', N'<new_name>';"),
            "{mssql}"
        );
        let pg = rename_sql_for(
            DatabaseType::Postgres,
            ScriptObjectKind::Table,
            &QualifiedTable::new("orders").with_schema("public"),
        );
        assert!(
            pg.contains("ALTER TABLE \"public\".\"orders\" RENAME TO \"<new_name>\";"),
            "{pg}"
        );
        let my = rename_sql_for(
            DatabaseType::MySql,
            ScriptObjectKind::Procedure,
            &QualifiedTable::new("sync_users"),
        );
        assert!(my.contains("MySQL cannot rename a procedure"), "{my}");
        let lite = rename_sql_for(
            DatabaseType::Sqlite,
            ScriptObjectKind::Table,
            &QualifiedTable::new("orders"),
        );
        assert!(
            lite.contains("ALTER TABLE \"orders\" RENAME TO \"<new_name>\";"),
            "{lite}"
        );
    }

    #[test]
    fn rename_sql_escapes_hostile_names_on_sql_server() {
        let t = QualifiedTable::new("O'Brien").with_schema("dbo");
        let sql = rename_sql_for(DatabaseType::SqlServer, ScriptObjectKind::Table, &t);
        assert!(sql.contains("N'dbo.O''Brien'"), "{sql}");
    }

    #[test]
    fn drop_object_sql_is_guarded_and_engine_quoted() {
        let t = QualifiedTable::new("Orders").with_schema("dbo");
        let mssql = drop_object_sql_for(DatabaseType::SqlServer, ScriptObjectKind::Table, &t);
        assert!(mssql.starts_with("-- Review carefully"), "{mssql}");
        assert!(
            mssql.contains("DROP TABLE IF EXISTS [dbo].[Orders];"),
            "{mssql}"
        );
        let pg = drop_object_sql_for(
            DatabaseType::Postgres,
            ScriptObjectKind::View,
            &QualifiedTable::new("v_orders").with_schema("public"),
        );
        assert!(
            pg.contains("DROP VIEW IF EXISTS \"public\".\"v_orders\";"),
            "{pg}"
        );
    }

    #[test]
    fn alter_table_scaffold_uses_engine_change_forms() {
        let t = QualifiedTable::new("Orders").with_schema("dbo");
        let mssql = alter_table_scaffold_sql_for(DatabaseType::SqlServer, &t);
        assert!(mssql.contains("ALTER TABLE [dbo].[Orders]"), "{mssql}");
        assert!(mssql.contains("-- ALTER COLUMN"), "{mssql}");
        let pg = alter_table_scaffold_sql_for(DatabaseType::Postgres, &pg_table());
        assert!(pg.contains("TYPE text"), "{pg}");
        let my = alter_table_scaffold_sql_for(DatabaseType::MySql, &QualifiedTable::new("orders"));
        assert!(my.contains("-- MODIFY"), "{my}");
        let lite = alter_table_scaffold_sql_for(DatabaseType::Sqlite, &QualifiedTable::new("t"));
        assert!(lite.contains("cannot alter or drop columns"), "{lite}");
    }

    #[test]
    fn alter_from_definition_rewrites_create_per_engine() {
        let def = "CREATE PROCEDURE dbo.GetUser AS SELECT 1;";
        let t = QualifiedTable::new("GetUser").with_schema("dbo");
        assert!(
            alter_from_definition_for(
                DatabaseType::SqlServer,
                ScriptObjectKind::Procedure,
                &t,
                def
            )
            .starts_with("CREATE OR ALTER PROCEDURE"),
        );
        assert!(
            alter_from_definition_for(DatabaseType::Postgres, ScriptObjectKind::Function, &t, def)
                .starts_with("CREATE OR REPLACE PROCEDURE"),
        );
        let my =
            alter_from_definition_for(DatabaseType::MySql, ScriptObjectKind::Procedure, &t, def);
        assert!(my.contains("DROP PROCEDURE IF EXISTS"), "{my}");
        assert!(my.contains("CREATE PROCEDURE dbo.GetUser"), "{my}");
    }

    #[test]
    fn alter_from_definition_does_not_pile_up_spaces() {
        // SQL Server stores a deployed `CREATE OR ALTER VIEW` as
        // `CREATE   VIEW`; scripting that as ALTER again must not keep the
        // gap, or every round trip adds two more spaces.
        let t = QualifiedTable::new("v").with_schema("dbo");
        let stored = "CREATE   VIEW dbo.v AS SELECT 1";
        assert_eq!(
            alter_from_definition_for(DatabaseType::SqlServer, ScriptObjectKind::View, &t, stored),
            "CREATE OR ALTER VIEW dbo.v AS SELECT 1"
        );
        let proc = "CREATE\n  PROCEDURE dbo.p AS SELECT 1";
        assert_eq!(
            alter_from_definition_for(
                DatabaseType::SqlServer,
                ScriptObjectKind::Procedure,
                &t,
                proc
            ),
            "CREATE OR ALTER PROCEDURE dbo.p AS SELECT 1"
        );
    }

    #[test]
    fn alter_from_definition_leaves_existing_or_alter() {
        let def = "CREATE OR ALTER VIEW v AS SELECT 1;";
        let t = QualifiedTable::new("v");
        assert_eq!(
            alter_from_definition_for(DatabaseType::SqlServer, ScriptObjectKind::View, &t, def),
            def
        );
    }

    #[test]
    fn alter_from_definition_skips_commented_create() {
        let def = "-- CREATE note\nCREATE VIEW v AS SELECT 1;";
        let t = QualifiedTable::new("v");
        let out =
            alter_from_definition_for(DatabaseType::SqlServer, ScriptObjectKind::View, &t, def);
        assert!(
            out.contains("-- CREATE note\nCREATE OR ALTER VIEW"),
            "{out}"
        );
    }

    #[test]
    fn alter_from_definition_without_create_is_honest() {
        let def = "SELECT 1;";
        let t = QualifiedTable::new("v");
        let out =
            alter_from_definition_for(DatabaseType::SqlServer, ScriptObjectKind::View, &t, def);
        assert!(out.starts_with("-- Could not find CREATE"), "{out}");
        assert!(out.contains("SELECT 1;"), "{out}");
    }

    #[test]
    fn sql_literal_for_renders_engine_forms() {
        assert_eq!(
            sql_literal_for(DatabaseType::Postgres, &CellValue::String("a'b".into())),
            "'a''b'"
        );
        assert_eq!(
            sql_literal_for(DatabaseType::Postgres, &CellValue::Bool(true)),
            "TRUE"
        );
        assert_eq!(
            sql_literal_for(DatabaseType::Sqlite, &CellValue::Bytes(vec![0xDE, 0xAD])),
            "X'DEAD'"
        );
        assert_eq!(
            sql_literal_for(DatabaseType::MySql, &CellValue::String(r"a\".into())),
            r"'a\\'"
        );
    }
}

#[cfg(test)]
mod rename_dialog_tests {
    use super::*;
    use sqlly_lang::sqlmodel::DatabaseType;

    #[test]
    fn rename_identifier_rejects_dangerous_names() {
        assert!(rename_identifier_error("").is_some());
        assert!(rename_identifier_error("   ").is_some());
        assert!(rename_identifier_error("a'b").is_some());
        assert!(rename_identifier_error("a\"b").is_some());
        assert!(rename_identifier_error("a`b").is_some());
        assert!(rename_identifier_error("a]b").is_some());
        assert!(rename_identifier_error("a;b").is_some());
        assert!(rename_identifier_error("orders_v2").is_none());
        assert!(
            rename_identifier_error("Sales Orders").is_none(),
            "spaces are fine — quoted"
        );
    }

    #[test]
    fn rename_sql_with_name_substitutes_the_placeholder() {
        let qt = QualifiedTable::new("orders").with_schema("dbo");
        let sql = rename_sql_with_name(
            DatabaseType::SqlServer,
            ScriptObjectKind::Table,
            &qt,
            "orders_v2",
        )
        .unwrap();
        assert!(sql.contains("N'orders_v2'"), "{sql}");
        assert!(!sql.contains("<new_name>"), "{sql}");
        assert!(
            rename_sql_with_name(DatabaseType::Postgres, ScriptObjectKind::Table, &qt, "a'b")
                .is_err()
        );
    }
}
