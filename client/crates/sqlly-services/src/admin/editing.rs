use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableRef {
    pub schema: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableKey {
    pub columns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EditOperation {
    Insert {
        values: Vec<(String, String)>,
    },
    Update {
        key: Vec<(String, String)>,
        values: Vec<(String, String)>,
        original_values: Vec<(String, String)>,
    },
    Delete {
        key: Vec<(String, String)>,
        original_values: Vec<(String, String)>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditTransaction {
    pub table: TableRef,
    pub primary_key: Option<TableKey>,
    pub operations: Vec<EditOperation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditPreview {
    pub sql: String,
    pub warnings: Vec<String>,
}

impl EditPreview {
    pub fn warning_messages(&self) -> Vec<sqlly_core::DisplayMessage> {
        self.warnings
            .iter()
            .map(|warning| {
                let key = if warning.starts_with("Editing without") {
                    "services.admin.editing.no_key_read_only"
                } else {
                    "services.admin.editing.no_key_original_values"
                };
                sqlly_core::DisplayMessage::new(key, warning)
            })
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CellEditorKind {
    Text,
    MultilineText,
    Integer,
    Decimal,
    Boolean,
    DateTime,
    Binary,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectPropertyQuery {
    pub title: String,
    pub sql: String,
}

pub fn select_top_sql(table: &TableRef, rows: u32) -> String {
    format!("SELECT TOP ({rows}) *\nFROM {};", table_name(table))
}

pub fn edit_top_sql(table: &TableRef, rows: u32, key: Option<&TableKey>) -> EditPreview {
    let mut warnings = Vec::new();
    if key.is_none_or(|key| key.columns.is_empty()) {
        warnings.push(
            "Editing without a primary key is read-only until a stable key is selected."
                .to_string(),
        );
    }
    EditPreview {
        sql: select_top_sql(table, rows),
        warnings,
    }
}

pub fn preview_transaction(tx: &EditTransaction) -> EditPreview {
    let mut warnings = Vec::new();
    if tx
        .primary_key
        .as_ref()
        .is_none_or(|key| key.columns.is_empty())
    {
        warnings.push(
            "No primary key detected; generated updates use original-value conflict predicates."
                .to_string(),
        );
    }
    let mut statements = vec!["BEGIN TRANSACTION;".to_string()];
    for operation in &tx.operations {
        statements.push(operation_sql(&tx.table, operation));
    }
    statements.push("-- Review row counts before commit.".to_string());
    statements.push("COMMIT TRANSACTION;".to_string());
    EditPreview {
        sql: statements.join("\n"),
        warnings,
    }
}

pub fn cell_editor_for_type(type_name: &str) -> CellEditorKind {
    match type_name.to_ascii_lowercase().as_str() {
        "bit" => CellEditorKind::Boolean,
        "tinyint" | "smallint" | "int" | "bigint" => CellEditorKind::Integer,
        "decimal" | "numeric" | "money" | "smallmoney" | "float" | "real" => {
            CellEditorKind::Decimal
        }
        "date" | "time" | "datetime" | "datetime2" | "smalldatetime" | "datetimeoffset" => {
            CellEditorKind::DateTime
        }
        "binary" | "varbinary" | "image" | "rowversion" | "timestamp" => CellEditorKind::Binary,
        "text" | "ntext" | "xml" | "nvarchar(max)" | "varchar(max)" => {
            CellEditorKind::MultilineText
        }
        _ => CellEditorKind::Text,
    }
}

pub fn validate_cell(type_name: &str, value: &str, nullable: bool) -> Result<(), String> {
    if value.eq_ignore_ascii_case("null") || value.is_empty() {
        return if nullable {
            Ok(())
        } else {
            Err("Column does not allow NULL".to_string())
        };
    }
    match cell_editor_for_type(type_name) {
        CellEditorKind::Integer => value
            .parse::<i64>()
            .map(|_| ())
            .map_err(|_| "Expected integer".to_string()),
        CellEditorKind::Decimal => value
            .parse::<f64>()
            .map(|_| ())
            .map_err(|_| "Expected decimal".to_string()),
        CellEditorKind::Boolean => match value.to_ascii_lowercase().as_str() {
            "0" | "1" | "true" | "false" => Ok(()),
            _ => Err("Expected bit value".to_string()),
        },
        _ => Ok(()),
    }
}

pub fn object_property_queries(schema: &str, name: &str) -> Vec<ObjectPropertyQuery> {
    let object = literal(&format!("{schema}.{name}"));
    vec![
        ObjectPropertyQuery {
            title: "General".to_string(),
            sql: format!(
                "SELECT SCHEMA_NAME(schema_id) AS schema_name, name, type_desc, create_date, modify_date FROM sys.objects WHERE object_id = OBJECT_ID(N'{object}')"
            ),
        },
        ObjectPropertyQuery {
            title: "Columns".to_string(),
            sql: format!(
                "SELECT c.name, t.name AS type_name, c.max_length, c.is_nullable FROM sys.columns c JOIN sys.types t ON c.user_type_id = t.user_type_id WHERE c.object_id = OBJECT_ID(N'{object}') ORDER BY c.column_id"
            ),
        },
        ObjectPropertyQuery {
            title: "Permissions".to_string(),
            sql: format!(
                "SELECT pr.name AS principal_name, pe.permission_name, pe.state_desc FROM sys.database_permissions pe JOIN sys.database_principals pr ON pe.grantee_principal_id = pr.principal_id WHERE pe.major_id = OBJECT_ID(N'{object}') ORDER BY pr.name, pe.permission_name"
            ),
        },
    ]
}

pub fn database_property_queries(database: &str) -> Vec<ObjectPropertyQuery> {
    let database = literal(database);
    vec![
        ObjectPropertyQuery { title: "General".to_string(), sql: format!("SELECT name, state_desc, recovery_model_desc, compatibility_level, collation_name FROM sys.databases WHERE name = N'{database}'") },
        ObjectPropertyQuery { title: "Files".to_string(), sql: format!("SELECT name, type_desc, physical_name, size FROM sys.master_files WHERE database_id = DB_ID(N'{database}')") },
        ObjectPropertyQuery { title: "Permissions".to_string(), sql: "SELECT permission_name, state_desc, class_desc FROM sys.fn_my_permissions(NULL, 'DATABASE')".to_string() },
    ]
}

fn operation_sql(table: &TableRef, operation: &EditOperation) -> String {
    match operation {
        EditOperation::Insert { values } => format!(
            "INSERT INTO {} ({})\nVALUES ({});",
            table_name(table),
            values
                .iter()
                .map(|(column, _)| quote_name(column))
                .collect::<Vec<_>>()
                .join(", "),
            values
                .iter()
                .map(|(_, value)| sql_value(value))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        EditOperation::Update {
            key,
            values,
            original_values,
        } => format!(
            "UPDATE {}\nSET {}\nWHERE {};",
            table_name(table),
            values
                .iter()
                .map(|(column, value)| format!("{} = {}", quote_name(column), sql_value(value)))
                .collect::<Vec<_>>()
                .join(", "),
            predicate(if key.is_empty() { original_values } else { key })
        ),
        EditOperation::Delete {
            key,
            original_values,
        } => format!(
            "DELETE FROM {}\nWHERE {};",
            table_name(table),
            predicate(if key.is_empty() { original_values } else { key })
        ),
    }
}

fn predicate(values: &[(String, String)]) -> String {
    values
        .iter()
        .map(|(column, value)| format!("{} = {}", quote_name(column), sql_value(value)))
        .collect::<Vec<_>>()
        .join(" AND ")
}

fn table_name(table: &TableRef) -> String {
    format!("{}.{}", quote_name(&table.schema), quote_name(&table.name))
}

fn quote_name(name: &str) -> String {
    format!("[{}]", name.replace(']', "]]"))
}

fn sql_value(value: &str) -> String {
    if value.eq_ignore_ascii_case("null") {
        return "NULL".to_string();
    }
    if is_numeric_literal(value) {
        return value.to_string();
    }
    format!("N'{}'", literal(value))
}

/// Returns true when `value` is an unambiguous SQL numeric literal that can be
/// emitted without quoting. Anything else (including values that merely start
/// with a digit, hex, or have leading zeros) is treated as a quoted string so
/// editing never silently corrupts data or relies on implicit conversion.
fn is_numeric_literal(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return false;
    }
    let candidate = trimmed.strip_prefix(['+', '-']).unwrap_or(trimmed);
    if candidate.is_empty() {
        return false;
    }
    // Preserve values whose leading zero is significant (e.g. "007" zip-style
    // codes) by treating them as strings rather than collapsing to 7.
    let integer_part = candidate.split('.').next().unwrap_or(candidate);
    if integer_part.len() > 1 && integer_part.starts_with('0') {
        return false;
    }
    let mut seen_dot = false;
    let mut seen_digit = false;
    for ch in candidate.chars() {
        match ch {
            '0'..='9' => seen_digit = true,
            '.' if !seen_dot => seen_dot = true,
            _ => return false,
        }
    }
    seen_digit
}

fn literal(value: &str) -> String {
    value.replace('\'', "''")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> TableRef {
        TableRef {
            schema: "sales".to_string(),
            name: "Orders".to_string(),
        }
    }

    #[test]
    fn generates_select_and_edit_top_with_primary_key_warning() {
        assert_eq!(
            select_top_sql(&table(), 100),
            "SELECT TOP (100) *\nFROM [sales].[Orders];"
        );
        let preview = edit_top_sql(&table(), 200, None);
        assert!(preview.warnings[0].contains("primary key"));
    }

    #[test]
    fn previews_transaction_with_conflict_predicates() {
        let preview = preview_transaction(&EditTransaction {
            table: table(),
            primary_key: Some(TableKey {
                columns: vec!["OrderId".to_string()],
            }),
            operations: vec![EditOperation::Update {
                key: vec![("OrderId".to_string(), "7".to_string())],
                values: vec![("Status".to_string(), "Closed".to_string())],
                original_values: vec![("Status".to_string(), "Open".to_string())],
            }],
        });
        assert!(preview.sql.starts_with("BEGIN TRANSACTION"));
        assert!(preview.sql.contains("UPDATE [sales].[Orders]"));
        assert!(preview.sql.contains("WHERE [OrderId] = 7"));
        assert!(preview.sql.contains("SET [Status] = N'Closed'"));
    }

    #[test]
    fn renders_numeric_values_unquoted_and_strings_quoted() {
        let insert = preview_transaction(&EditTransaction {
            table: table(),
            primary_key: Some(TableKey {
                columns: vec!["OrderId".to_string()],
            }),
            operations: vec![EditOperation::Insert {
                values: vec![
                    ("OrderId".to_string(), "42".to_string()),
                    ("Total".to_string(), "-19.95".to_string()),
                    ("Code".to_string(), "007".to_string()),
                    ("Note".to_string(), "O'Brien".to_string()),
                    ("Archived".to_string(), "null".to_string()),
                ],
            }],
        });
        // Pure integers/decimals are unquoted.
        assert!(insert.sql.contains("42, -19.95,"));
        // Leading-zero values stay quoted to preserve the literal.
        assert!(insert.sql.contains("N'007'"));
        // String values are escaped and quoted.
        assert!(insert.sql.contains("N'O''Brien'"));
        // NULL keyword passes through.
        assert!(insert.sql.contains("NULL"));
    }

    #[test]
    fn numeric_literal_detection_rejects_ambiguous_values() {
        assert!(is_numeric_literal("42"));
        assert!(is_numeric_literal("-3.14"));
        assert!(is_numeric_literal("+7"));
        assert!(!is_numeric_literal("007"));
        assert!(!is_numeric_literal("1,000"));
        assert!(!is_numeric_literal("0x1F"));
        assert!(!is_numeric_literal("1e5"));
        assert!(!is_numeric_literal(""));
        assert!(!is_numeric_literal("."));
    }

    #[test]
    fn maps_and_validates_cell_editors() {
        assert_eq!(cell_editor_for_type("bit"), CellEditorKind::Boolean);
        assert_eq!(
            cell_editor_for_type("nvarchar(max)"),
            CellEditorKind::MultilineText
        );
        assert!(validate_cell("int", "42", false).is_ok());
        assert!(validate_cell("int", "abc", false).is_err());
        assert!(validate_cell("varchar", "", false).is_err());
    }

    #[test]
    fn builds_object_and_database_property_queries() {
        let object = object_property_queries("dbo", "bad'name");
        assert_eq!(object.len(), 3);
        assert!(object[0].sql.contains("bad''name"));
        let database = database_property_queries("appdb");
        assert!(database.iter().any(|query| query.title == "Permissions"));
    }
}
