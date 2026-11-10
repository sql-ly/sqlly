use crate::intellisense::catalog::CompletionCatalog;
use crate::intellisense::types::{CompletionItem, CompletionKind, CompletionSource};
use sqlly_lang::fsmodel::SqlDataType;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueKind {
    Text,
    Numeric,
    Unknown,
}

/// Classify a SQL type name (`nvarchar(50)`, `int`, `uuid`) — mirrors
/// sqlly_services::base_type/is_numeric_type semantics but is kept local so
/// this crate stays dependency-free.
pub fn value_kind_for_type(type_name: Option<&str>) -> ValueKind {
    let Some(type_name) = type_name else {
        return ValueKind::Unknown;
    };
    let base = base_type(type_name);
    if is_numeric_type(&base) {
        ValueKind::Numeric
    } else if is_text_type(&base) {
        ValueKind::Text
    } else {
        ValueKind::Unknown
    }
}

fn base_type(type_name: &str) -> String {
    let trimmed = type_name.trim();
    let without_parens = trimmed.split('(').next().unwrap_or(trimmed);
    let first_token = without_parens.split_whitespace().next().unwrap_or("");
    first_token.to_ascii_lowercase()
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

fn is_text_type(base: &str) -> bool {
    matches!(
        base,
        "char"
            | "nchar"
            | "varchar"
            | "nvarchar"
            | "text"
            | "ntext"
            | "string"
            | "uuid"
            | "uniqueidentifier"
    )
}

/// Resolve a column's [`ValueKind`] from filesystem-modelled schema metadata.
pub fn column_value_kind_from_catalog(
    catalog: &CompletionCatalog,
    schema: Option<&str>,
    table: Option<&str>,
    column: Option<&str>,
) -> ValueKind {
    let (Some(schema), Some(table), Some(column)) = (schema, table, column) else {
        return ValueKind::Unknown;
    };

    for fs_schema in &catalog.filesystem_schemas {
        if !fs_schema.node.name.eq_ignore_ascii_case(schema) {
            continue;
        }
        for table_node in &fs_schema.tables {
            if !table_node.source_node.node.name.eq_ignore_ascii_case(table) {
                continue;
            }
            for col in &table_node.columns {
                if col.node.name.eq_ignore_ascii_case(column) {
                    let type_name = match &col.node.data_type {
                        SqlDataType::BuiltIn { name, .. } => Some(name.as_str()),
                        _ => None,
                    };
                    return value_kind_for_type(type_name);
                }
            }
        }
    }

    ValueKind::Unknown
}

/// Literal templates offered right after `col =` / `col LIKE`. `is_like` selects
/// the `'%%'` template. Scores sit at 90 so they land above columns (70+fuzzy)
/// but below FK joins (95) and value-lookup rows (1000-i).
///
/// `not_null` marks a column known to be `NOT NULL`: comparing it with `NULL`
/// or testing `IS [NOT] NULL` can never be useful, so those templates are left
/// out (feedback ifb_9b71). An unknown nullability keeps them.
pub fn comparison_value_items(
    kind: ValueKind,
    is_like: bool,
    has_opening_quote: bool,
    replace_span: Option<(usize, usize)>,
    operator_span: Option<(usize, usize)>,
    not_null: bool,
) -> Vec<CompletionItem> {
    if is_like {
        return vec![value_item("'%%'", "'%%'", 92, "pattern", replace_span)];
    }

    let mut items = Vec::new();
    if matches!(kind, ValueKind::Text | ValueKind::Unknown) && !has_opening_quote {
        items.push(value_item("''", "''", 92, "text literal", replace_span));
    }
    if matches!(kind, ValueKind::Numeric | ValueKind::Unknown) {
        items.push(value_item("0", "0", 92, "numeric literal", replace_span));
    }
    if not_null {
        return items;
    }
    items.push(value_item("NULL", "NULL", 91, "null literal", replace_span));
    if let Some(span) = operator_span {
        items.push(value_item(
            "IS NULL",
            "IS NULL",
            90,
            "null literal",
            Some(span),
        ));
        items.push(value_item(
            "IS NOT NULL",
            "IS NOT NULL",
            89,
            "null literal",
            Some(span),
        ));
    }
    items
}

fn value_item(
    label: &str,
    insert_text: &str,
    score: i32,
    detail: &str,
    replace_span: Option<(usize, usize)>,
) -> CompletionItem {
    CompletionItem {
        label: label.to_string(),
        insert_text: insert_text.to_string(),
        kind: CompletionKind::Value,
        source: CompletionSource::Inferred,
        score,
        detail: detail.to_string(),
        replace_span,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_kind_for_type_classification() {
        assert_eq!(value_kind_for_type(Some("nvarchar(50)")), ValueKind::Text);
        assert_eq!(
            value_kind_for_type(Some("decimal(10,2)")),
            ValueKind::Numeric
        );
        assert_eq!(value_kind_for_type(None), ValueKind::Unknown);
    }

    #[test]
    fn unknown_equals_emits_all_templates_in_score_order() {
        let items = comparison_value_items(
            ValueKind::Unknown,
            false,
            false,
            Some((10, 10)),
            Some((8, 10)),
            false,
        );
        assert_eq!(
            items
                .iter()
                .map(|item| item.label.as_str())
                .collect::<Vec<_>>(),
            vec!["''", "0", "NULL", "IS NULL", "IS NOT NULL"]
        );
        assert_eq!(items[0].score, 92);
        assert_eq!(items[1].score, 92);
        assert_eq!(items[2].score, 91);
        assert_eq!(items[3].score, 90);
        assert_eq!(items[4].score, 89);
    }

    #[test]
    fn numeric_equals_omits_text_literal() {
        let items = comparison_value_items(
            ValueKind::Numeric,
            false,
            false,
            Some((0, 0)),
            Some((0, 0)),
            false,
        );
        assert!(!items.iter().any(|item| item.label == "''"));
    }

    #[test]
    fn text_equals_omits_numeric_literal() {
        let items = comparison_value_items(
            ValueKind::Text,
            false,
            false,
            Some((0, 0)),
            Some((0, 0)),
            false,
        );
        assert!(!items.iter().any(|item| item.label == "0"));
    }

    #[test]
    fn like_emits_pattern_only() {
        let items =
            comparison_value_items(ValueKind::Unknown, true, false, Some((0, 0)), None, false);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "'%%'");
    }

    #[test]
    fn opening_quote_suppresses_text_literal() {
        let items = comparison_value_items(
            ValueKind::Unknown,
            false,
            true,
            Some((0, 0)),
            Some((0, 0)),
            false,
        );
        assert!(!items.iter().any(|item| item.label == "''"));
    }

    #[test]
    fn not_null_numeric_column_offers_only_the_number() {
        let items = comparison_value_items(
            ValueKind::Numeric,
            false,
            false,
            Some((0, 0)),
            Some((0, 0)),
            true,
        );
        assert_eq!(
            items
                .iter()
                .map(|item| item.label.as_str())
                .collect::<Vec<_>>(),
            vec!["0"]
        );
    }

    #[test]
    fn missing_operator_span_omits_is_null_items() {
        let items =
            comparison_value_items(ValueKind::Unknown, false, false, Some((0, 0)), None, false);
        assert_eq!(
            items
                .iter()
                .map(|item| item.label.as_str())
                .collect::<Vec<_>>(),
            vec!["''", "0", "NULL"]
        );
    }
}
