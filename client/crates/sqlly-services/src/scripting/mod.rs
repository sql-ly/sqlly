#![forbid(unsafe_code)]
pub mod dependency;
pub mod error;
pub mod generator;
pub mod identifier;
pub mod options;
pub mod template;

pub use dependency::{DependencyOrder, ScriptObject, topological_order};
pub use error::{ScriptingError, ScriptingResult};
pub use template::{
    ExplorerActionMode, ExplorerActionSpec, ExplorerObjectKind, ScriptTemplate, TemplateCategory,
    TemplateExplorer, TemplateObjectContext, TemplateOrigin, TemplateParameter,
    USER_TEMPLATES_DEFAULT_CATEGORY, USER_TEMPLATES_VERSION, UserTemplatesFile, built_in_templates,
    explorer_actions_for, parameters_from_body,
};

pub fn quote_identifier(identifier: &str) -> String {
    format!("[{}]", identifier.replace(']', "]]"))
}

pub fn render_multipart_name(database: Option<&str>, schema: Option<&str>, name: &str) -> String {
    let mut parts = Vec::new();
    if let Some(database) = database {
        parts.push(quote_identifier(database));
    }
    if let Some(schema) = schema {
        parts.push(quote_identifier(schema));
    }
    parts.push(quote_identifier(name));
    parts.join(".")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableScript {
    pub schema: String,
    pub name: String,
    pub columns: Vec<ColumnScript>,
    pub primary_key: Option<PrimaryKeyScript>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnScript {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub identity: Option<IdentityScript>,
    pub default: Option<String>,
    pub computed: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityScript {
    pub seed: i64,
    pub increment: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrimaryKeyScript {
    pub name: String,
    pub columns: Vec<String>,
    pub clustered: bool,
}

pub fn script_create_table(table: &TableScript) -> String {
    let table_name = render_multipart_name(None, Some(&table.schema), &table.name);
    let mut lines = Vec::new();

    for column in &table.columns {
        lines.push(format!("    {}", script_column(column)));
    }

    if let Some(primary_key) = &table.primary_key {
        let clustered = if primary_key.clustered {
            "CLUSTERED"
        } else {
            "NONCLUSTERED"
        };
        let columns = primary_key
            .columns
            .iter()
            .map(|column| format!("{} ASC", quote_identifier(column)))
            .collect::<Vec<_>>()
            .join(", ");
        lines.push(format!(
            "    CONSTRAINT {} PRIMARY KEY {clustered} ({columns})",
            quote_identifier(&primary_key.name)
        ));
    }

    format!("CREATE TABLE {table_name}\n(\n{}\n);", lines.join(",\n"))
}

pub fn script_drop_object(schema: &str, name: &str, object_type: &str) -> String {
    format!(
        "DROP {object_type} {};",
        render_multipart_name(None, Some(schema), name)
    )
}

/// Emits a guarded `DROP <type> IF EXISTS` followed by the `CREATE` body — the
/// "Drop and Create" scripting action. The drop is idempotent so the script can
/// be re-run safely.
pub fn script_drop_and_create(
    schema: &str,
    name: &str,
    object_type: &str,
    create_body: &str,
) -> String {
    let qualified = render_multipart_name(None, Some(schema), name);
    let create = create_body.trim_end();
    format!("DROP {object_type} IF EXISTS {qualified};\nGO\n{create}")
}

/// Convenience wrapper that drops and recreates a table from its [`TableScript`].
pub fn script_drop_and_create_table(table: &TableScript) -> String {
    script_drop_and_create(
        &table.schema,
        &table.name,
        "TABLE",
        &script_create_table(table),
    )
}

/// A literal cell value to be rendered into an `INSERT` statement.
#[derive(Debug, Clone, PartialEq)]
pub enum ScriptValue {
    Null,
    Number(String),
    /// A textual value; rendered as an N'...'-quoted, escaped string literal.
    Text(String),
    /// An already-formatted SQL expression emitted verbatim (e.g. `0x00`,
    /// `CAST(... AS ...)`); the caller is responsible for its safety.
    Raw(String),
}

/// Renders a single [`ScriptValue`] as a T-SQL literal.
pub fn script_value(value: &ScriptValue) -> String {
    match value {
        ScriptValue::Null => "NULL".to_string(),
        ScriptValue::Number(n) => n.clone(),
        ScriptValue::Text(t) => format!("N'{}'", t.replace('\'', "''")),
        ScriptValue::Raw(r) => r.clone(),
    }
}

/// Scripts table rows as `INSERT` statements (for "Script Data" on small
/// tables). Columns name the target columns; each row must have one value per
/// column. Returns one `INSERT ... VALUES (...)` per row so the output diffs
/// cleanly and tolerates IDENTITY/trigger semantics row-by-row.
pub fn script_insert_data(
    schema: &str,
    table: &str,
    columns: &[String],
    rows: &[Vec<ScriptValue>],
) -> ScriptingResult<String> {
    let qualified = render_multipart_name(None, Some(schema), table);
    let column_list = columns
        .iter()
        .map(|c| quote_identifier(c))
        .collect::<Vec<_>>()
        .join(", ");

    let mut out = Vec::with_capacity(rows.len());
    for (i, row) in rows.iter().enumerate() {
        if row.len() != columns.len() {
            return Err(ScriptingError::GenerationFailed(format!(
                "row {i} has {} values but there are {} columns",
                row.len(),
                columns.len()
            )));
        }
        let values = row.iter().map(script_value).collect::<Vec<_>>().join(", ");
        out.push(format!(
            "INSERT INTO {qualified} ({column_list}) VALUES ({values});"
        ));
    }
    Ok(out.join("\n"))
}

fn script_column(column: &ColumnScript) -> String {
    if let Some(expression) = &column.computed {
        return format!("{} AS {expression}", quote_identifier(&column.name),);
    }

    let mut parts = vec![quote_identifier(&column.name), column.data_type.clone()];
    if let Some(identity) = &column.identity {
        parts.push(format!(
            "IDENTITY({},{})",
            identity.seed, identity.increment
        ));
    }
    parts.push(if column.nullable { "NULL" } else { "NOT NULL" }.to_string());
    if let Some(default) = &column.default {
        parts.push(format!("DEFAULT {default}"));
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_closing_brackets() {
        assert_eq!(quote_identifier("a]b"), "[a]]b]");
    }

    #[test]
    fn renders_create_table_with_primary_key() {
        let sql = script_create_table(&TableScript {
            schema: "app".to_string(),
            name: "customers".to_string(),
            columns: vec![
                ColumnScript {
                    name: "customer_id".to_string(),
                    data_type: "int".to_string(),
                    nullable: false,
                    identity: Some(IdentityScript {
                        seed: 1,
                        increment: 1,
                    }),
                    default: None,
                    computed: None,
                },
                ColumnScript {
                    name: "display_name".to_string(),
                    data_type: "nvarchar(200)".to_string(),
                    nullable: false,
                    identity: None,
                    default: None,
                    computed: None,
                },
            ],
            primary_key: Some(PrimaryKeyScript {
                name: "pk_customers".to_string(),
                columns: vec!["customer_id".to_string()],
                clustered: true,
            }),
        });

        assert!(sql.starts_with("CREATE TABLE [app].[customers]"));
        assert!(sql.contains("[customer_id] int IDENTITY(1,1) NOT NULL"));
        assert!(sql.contains("CONSTRAINT [pk_customers] PRIMARY KEY CLUSTERED"));
    }

    #[test]
    fn renders_computed_column() {
        let column = ColumnScript {
            name: "full_name".to_string(),
            data_type: "nvarchar(400)".to_string(),
            nullable: true,
            identity: None,
            default: None,
            computed: Some("([first_name] + N' ' + [last_name])".to_string()),
        };

        assert_eq!(
            script_column(&column),
            "[full_name] AS ([first_name] + N' ' + [last_name])"
        );
    }

    fn sample_table() -> TableScript {
        TableScript {
            schema: "app".to_string(),
            name: "customers".to_string(),
            columns: vec![ColumnScript {
                name: "customer_id".to_string(),
                data_type: "int".to_string(),
                nullable: false,
                identity: None,
                default: None,
                computed: None,
            }],
            primary_key: None,
        }
    }

    #[test]
    fn drop_and_create_is_guarded_and_idempotent() {
        let sql = script_drop_and_create_table(&sample_table());
        assert!(sql.starts_with("DROP TABLE IF EXISTS [app].[customers];"));
        assert!(sql.contains("\nGO\n"));
        assert!(sql.contains("CREATE TABLE [app].[customers]"));
    }

    #[test]
    fn scripts_rows_as_insert_statements() {
        let sql = script_insert_data(
            "dbo",
            "people",
            &["id".to_string(), "name".to_string(), "note".to_string()],
            &[
                vec![
                    ScriptValue::Number("1".to_string()),
                    ScriptValue::Text("O'Brien".to_string()),
                    ScriptValue::Null,
                ],
                vec![
                    ScriptValue::Number("2".to_string()),
                    ScriptValue::Text("Ann".to_string()),
                    ScriptValue::Raw("SYSUTCDATETIME()".to_string()),
                ],
            ],
        )
        .expect("rows match columns");
        let lines: Vec<&str> = sql.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines[0],
            "INSERT INTO [dbo].[people] ([id], [name], [note]) VALUES (1, N'O''Brien', NULL);"
        );
        assert_eq!(
            lines[1],
            "INSERT INTO [dbo].[people] ([id], [name], [note]) VALUES (2, N'Ann', SYSUTCDATETIME());"
        );
    }

    #[test]
    fn insert_data_rejects_row_arity_mismatch() {
        let err = script_insert_data(
            "dbo",
            "t",
            &["a".to_string(), "b".to_string()],
            &[vec![ScriptValue::Null]],
        );
        assert!(err.is_err());
    }
}
