//! Slot type validation — port of `SlotTypeValidation.swift`.
//!
//! Pure, UI-free. Generates placeholder literals for parameter slots based on
//! the SQL type name, and normalizes type names to their base form. Shared by
//! the gpui editor and the LSP server so procedure-snippet generation is
//! identical across frontends.

/// Reduces a full SQL type name (e.g. `nvarchar(128)`, `decimal(10,2)`) to its
/// lowercase base form (e.g. `nvarchar`, `decimal`).
pub fn base_type(type_name: &str) -> String {
    let trimmed = type_name.trim();
    let without_parens = trimmed.split('(').next().unwrap_or(trimmed);
    let first_token = without_parens.split_whitespace().next().unwrap_or("");
    first_token.to_lowercase()
}

/// Produces a SQL literal placeholder appropriate for the given type name.
///
/// - Numeric types → `0`
/// - Date/time and uniqueidentifier types → `''`
/// - Everything else (strings, text, binary-ish) → `N''`
pub fn placeholder_literal(type_name: &str) -> &'static str {
    let base = base_type(type_name);
    if is_numeric_type(&base) {
        "0"
    } else if is_date_type(&base) || base == "uniqueidentifier" || base == "uuid" {
        "''"
    } else {
        "N''"
    }
}

fn is_numeric_type(base: &str) -> bool {
    matches!(
        base,
        "tinyint"
            | "smallint"
            | "int"
            | "integer"
            | "bigint"
            | "decimal"
            | "numeric"
            | "float"
            | "real"
            | "money"
            | "smallmoney"
            | "bit"
    )
}

fn is_date_type(base: &str) -> bool {
    matches!(
        base,
        "date" | "datetime" | "datetime2" | "smalldatetime" | "datetimeoffset" | "time"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_type_strips_size() {
        assert_eq!(base_type("nvarchar(128)"), "nvarchar");
        assert_eq!(base_type("DECIMAL(10,2)"), "decimal");
        assert_eq!(base_type("varchar(max)"), "varchar");
    }

    #[test]
    fn base_type_strips_whitespace_prefix() {
        assert_eq!(base_type(" nvarchar(128)"), "nvarchar");
    }

    #[test]
    fn base_type_plain() {
        assert_eq!(base_type("int"), "int");
        assert_eq!(base_type("NVARCHAR"), "nvarchar");
    }

    #[test]
    fn placeholder_numeric_types() {
        assert_eq!(placeholder_literal("int"), "0");
        assert_eq!(placeholder_literal("bigint"), "0");
        assert_eq!(placeholder_literal("decimal(10,2)"), "0");
        assert_eq!(placeholder_literal("bit"), "0");
        assert_eq!(placeholder_literal("money"), "0");
    }

    #[test]
    fn placeholder_date_types() {
        assert_eq!(placeholder_literal("date"), "''");
        assert_eq!(placeholder_literal("datetime2"), "''");
        assert_eq!(placeholder_literal("time"), "''");
        assert_eq!(placeholder_literal("datetimeoffset"), "''");
    }

    #[test]
    fn placeholder_uuid() {
        assert_eq!(placeholder_literal("uniqueidentifier"), "''");
        assert_eq!(placeholder_literal("uuid"), "''");
    }

    #[test]
    fn placeholder_string_types() {
        assert_eq!(placeholder_literal("nvarchar(100)"), "N''");
        assert_eq!(placeholder_literal("varchar(50)"), "N''");
        assert_eq!(placeholder_literal("text"), "N''");
        assert_eq!(placeholder_literal("ntext"), "N''");
    }

    #[test]
    fn placeholder_empty_type() {
        assert_eq!(placeholder_literal(""), "N''");
        assert_eq!(placeholder_literal("   "), "N''");
    }
}
