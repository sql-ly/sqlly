//! SQL Server literal & identifier formatting for result-driven SQL generation.
//!
//! Port of `SQLLYExports/SQLLiteralFormatter.swift`. Pure, UI-free, unit-tested.
//! Complements `table_actions::escape_identifier` (which covers the identifier
//! case) with type-aware string-literal coercion and LIKE-pattern rendering
//! used by the result WHERE-action builder.
//!
//! Type-aware literal rules (matching Swift):
//! - Numeric types (`int`/`bigint`/`decimal`/`float`/...) pass through when the
//!   value parses as a double.
//! - `bit` maps `true`/`1` → `1` and `false`/`0` → `0`.
//! - Everything else is rendered as `N'…'` with `'` doubled.
//!
//! Uses `sqlly_core::CellValue` for the typed-cell variant, mirroring the
//! Swift `ResultCellValue` enum.

use sqlly_core::{CellValue, ResultColumn};
use sqlly_lang::sqlmodel::DatabaseType;

/// Brackets a SQL Server identifier, doubling any `]`.
///
/// Equivalent to Swift `SQLLiteralFormatter.sqlIdentifier(_:)` and to
/// `table_actions::escape_identifier`. Re-exported here so callers building
/// WHERE clauses from results have a single import for all literal needs.
#[cfg(test)]
pub fn sql_identifier(name: &str) -> String {
    format!("[{}]", name.replace(']', "]]"))
}

/// Quotes an identifier with the engine's own style; the SQL Server family
/// is byte-identical to [`sql_identifier`].
pub fn sql_identifier_for(db: DatabaseType, name: &str) -> String {
    db.quote_identifier(name)
}

#[allow(dead_code)]
/// Renders a string value as a SQL Server literal, choosing the form based on
/// the column's declared `data_type`.
///
/// - Numeric types pass through when `value` parses as a finite number.
/// - `bit` maps truthy/falsy literals to `1`/`0`.
/// - All other types (and non-numeric values in a numeric column) render as
///   `N'…'` with `'` doubled.
pub fn sql_literal(value: &str, data_type: &str) -> String {
    let lower = data_type.to_ascii_lowercase();
    let is_numeric = [
        "int",
        "bigint",
        "smallint",
        "tinyint",
        "decimal",
        "numeric",
        "float",
        "real",
        "money",
        "smallmoney",
    ]
    .iter()
    .any(|t| lower.contains(t));

    if is_numeric
        && value
            .trim()
            .parse::<f64>()
            .map(|f| f.is_finite())
            .unwrap_or(false)
    {
        return value.to_string();
    }

    if lower.contains("bit") {
        let v = value.trim().to_ascii_lowercase();
        if v == "true" || v == "1" {
            return "1".to_string();
        }
        if v == "false" || v == "0" {
            return "0".to_string();
        }
    }

    n_string_literal(value)
}

#[allow(dead_code)]
/// Renders a string value as a SQL literal using the data type of the column
/// at `column` index in `metadata`. Falls back to the generic string form when
/// the index is out of range or the descriptor lacks a type.
pub fn sql_literal_for_column(value: &str, column: usize, metadata: &[ResultColumn]) -> String {
    let data_type = metadata
        .get(column)
        .map(|c| c.data_type.as_str())
        .unwrap_or("");
    sql_literal(value, data_type)
}

/// Renders a typed `CellValue` as a SQL Server literal. Returns `None` for
/// `Null`; `Bytes` render as the engine's hex literal (`0x…`).
#[cfg(test)]
pub fn sql_literal_cell(cell: &CellValue) -> Option<String> {
    sql_literal_cell_for(DatabaseType::SqlServer, cell)
}

/// [`sql_literal_cell`] in the engine's own literal forms (string quoting,
/// boolean spelling). SQL Server output is byte-identical to the legacy
/// function.
pub fn sql_literal_cell_for(db: DatabaseType, cell: &CellValue) -> Option<String> {
    match cell {
        CellValue::Null => None,
        CellValue::Bool(b) => Some(db.bool_literal(*b).to_string()),
        CellValue::I64(n) => Some(n.to_string()),
        CellValue::F64(f) => Some(f.to_string()),
        CellValue::String(s) => Some(db.string_literal(s)),
        CellValue::Bytes(b) => Some(db.bytes_literal(b)),
    }
}

/// Where a LIKE pattern's `%` wildcards go relative to the user's literal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LikePlacement {
    #[allow(dead_code)]
    Exact,
    StartsWith,
    EndsWith,
    Contains,
}

/// Builds a LIKE pattern string (without the surrounding `N'…'`) by appending
/// `%` wildcards per `placement` and escaping LIKE-special characters
/// (`\`, `%`, `_`, `[`, `]`) with a backslash.
#[cfg(test)]
pub fn sql_like_pattern(literal: &str, placement: LikePlacement) -> String {
    sql_like_pattern_for(DatabaseType::SqlServer, literal, placement)
}

/// [`sql_like_pattern`] escaping only the characters the engine's LIKE
/// treats specially: escaping `[`/`]` outside the SQL Server family would be
/// an invalid escape sequence on engines that reject undefined escapes.
pub fn sql_like_pattern_for(db: DatabaseType, literal: &str, placement: LikePlacement) -> String {
    let escaped = escape_like_special_characters(db, literal);
    match placement {
        LikePlacement::Exact => escaped,
        LikePlacement::StartsWith => format!("{}%", escaped),
        LikePlacement::EndsWith => format!("%{}", escaped),
        LikePlacement::Contains => format!("%{}%", escaped),
    }
}

/// The engine-rendered string literal for a LIKE pattern, plus the matching
/// `ESCAPE` clause literal. The MySQL lineage doubles backslashes inside
/// string literals, so both parts route through the engine's string rules —
/// except the SQL Server family, which keeps the legacy plain `'\'` escape
/// clause byte-identical.
pub fn like_clause_for(db: DatabaseType, pattern: &str) -> String {
    if matches!(
        db.connection_kind(),
        sqlly_lang::sqlmodel::ConnectionKind::Tds | sqlly_lang::sqlmodel::ConnectionKind::Odbc
    ) {
        format!("N'{}' ESCAPE '\\'", pattern.replace('\'', "''"))
    } else {
        format!(
            "{} ESCAPE {}",
            db.string_literal(pattern),
            db.string_literal("\\")
        )
    }
}

/// Escapes the engine's LIKE-special characters with a backslash.
fn escape_like_special_characters(db: DatabaseType, value: &str) -> String {
    let specials = db.like_special_characters();
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        if specials.contains(&ch) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Renders a string as `N'…'` with `'` doubled.
fn n_string_literal(value: &str) -> String {
    format!("N'{}'", value.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifier_brackets_and_doubles_brackets() {
        assert_eq!(sql_identifier("Orders"), "[Orders]");
        assert_eq!(sql_identifier("a]b"), "[a]]b]");
        assert_eq!(sql_identifier("with space"), "[with space]");
    }

    #[test]
    fn numeric_type_passes_through_when_parseable() {
        assert_eq!(sql_literal("42", "int"), "42");
        assert_eq!(sql_literal("3.14", "decimal(10,2)"), "3.14");
        assert_eq!(sql_literal("-100", "bigint"), "-100");
        assert_eq!(sql_literal("1.5e3", "float"), "1.5e3");
    }

    #[test]
    fn numeric_type_falls_to_string_when_not_parseable() {
        assert_eq!(sql_literal("abc", "int"), "N'abc'");
        assert_eq!(sql_literal("nan", "float"), "N'nan'");
    }

    #[test]
    fn bit_maps_truthy_and_falsy() {
        assert_eq!(sql_literal("true", "bit"), "1");
        assert_eq!(sql_literal("1", "bit"), "1");
        assert_eq!(sql_literal("TRUE", "bit"), "1");
        assert_eq!(sql_literal("false", "bit"), "0");
        assert_eq!(sql_literal("0", "bit"), "0");
        assert_eq!(sql_literal("FALSE", "bit"), "0");
    }

    #[test]
    fn bit_non_boolean_falls_to_string() {
        assert_eq!(sql_literal("maybe", "bit"), "N'maybe'");
    }

    #[test]
    fn string_type_uses_n_prefix_and_doubles_quotes() {
        assert_eq!(sql_literal("hello", "nvarchar(max)"), "N'hello'");
        assert_eq!(sql_literal("it's", "varchar(10)"), "N'it''s'");
        assert_eq!(sql_literal("", "text"), "N''");
    }

    #[test]
    fn sql_literal_for_column_uses_metadata_type() {
        let cols = vec![
            ResultColumn {
                name: "id".into(),
                data_type: "int".into(),
                nullable: false,
            },
            ResultColumn {
                name: "name".into(),
                data_type: "nvarchar(50)".into(),
                nullable: true,
            },
        ];
        assert_eq!(sql_literal_for_column("42", 0, &cols), "42");
        assert_eq!(sql_literal_for_column("Alice", 1, &cols), "N'Alice'");
    }

    #[test]
    fn sql_literal_for_column_out_of_range_falls_to_string() {
        let cols: Vec<ResultColumn> = vec![];
        assert_eq!(sql_literal_for_column("x", 0, &cols), "N'x'");
    }

    #[test]
    #[allow(clippy::approx_constant)]
    fn cell_literal_dispatch() {
        assert_eq!(sql_literal_cell(&CellValue::Null), None);
        assert_eq!(sql_literal_cell(&CellValue::Bool(true)), Some("1".into()));
        assert_eq!(sql_literal_cell(&CellValue::Bool(false)), Some("0".into()));
        assert_eq!(sql_literal_cell(&CellValue::I64(42)), Some("42".into()));
        assert_eq!(sql_literal_cell(&CellValue::F64(3.14)), Some("3.14".into()));
        assert_eq!(
            sql_literal_cell(&CellValue::String("a'b".into())),
            Some("N'a''b'".into())
        );
        assert_eq!(
            sql_literal_cell(&CellValue::Bytes(vec![0x0A, 0xFF])),
            Some("0x0AFF".into())
        );
    }

    /// A binary key (a BINARY(16) UUID, a bytea hash) renders as the
    /// engine's own hex literal, so a predicate on it matches the row.
    #[test]
    fn bytes_render_as_engine_hex_literals() {
        let bytes = CellValue::Bytes(vec![0xDE, 0xAD]);
        assert_eq!(
            sql_literal_cell_for(DatabaseType::MySql, &bytes),
            Some("0xDEAD".into())
        );
        assert_eq!(
            sql_literal_cell_for(DatabaseType::Postgres, &bytes),
            Some("'\\xDEAD'".into())
        );
        assert_eq!(
            sql_literal_cell_for(DatabaseType::Sqlite, &bytes),
            Some("X'DEAD'".into())
        );
    }

    #[test]
    fn like_pattern_exact_is_escaped_only() {
        assert_eq!(sql_like_pattern("abc", LikePlacement::Exact), "abc");
        assert_eq!(sql_like_pattern("a%b_c", LikePlacement::Exact), "a\\%b\\_c");
    }

    #[test]
    fn like_pattern_starts_with() {
        assert_eq!(sql_like_pattern("abc", LikePlacement::StartsWith), "abc%");
    }

    #[test]
    fn like_pattern_ends_with() {
        assert_eq!(sql_like_pattern("abc", LikePlacement::EndsWith), "%abc");
    }

    #[test]
    fn like_pattern_contains() {
        assert_eq!(sql_like_pattern("abc", LikePlacement::Contains), "%abc%");
    }

    #[test]
    fn like_pattern_escapes_special_chars_before_wildcards() {
        let p = sql_like_pattern("50%_off", LikePlacement::Contains);
        assert_eq!(p, "%50\\%\\_off%");
    }

    #[test]
    fn like_pattern_escapes_brackets_and_backslash() {
        let p = sql_like_pattern(r"a\b[c]d", LikePlacement::Exact);
        assert_eq!(p, r"a\\b\[c\]d");
    }
}
