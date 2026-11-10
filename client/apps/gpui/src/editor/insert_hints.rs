//! Column-name inlay hints for `INSERT … VALUES` lists.
//!
//! Pure mapping: tokenize a statement (or buffer), emit display-only
//! `column:` labels at each value expression. The tokenizer is the
//! dialect-invariant scanner (ADR-016); this feature must look the same
//! on every engine.

use sqlly_core::ObjectKind;
use sqlly_intel::intellisense::CompletionCatalog;
use sqlly_intel::query::tokenizer::{Token, TokenKind, tokenize};

use super::statements::statement_spans_for;

/// One inlay: paint `label` immediately before the value expression that
/// starts at byte `offset` (offset is into the text passed to the function).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct InsertHint {
    pub offset: usize,
    pub label: String, // e.g. "customer_id:" — the renderer adds the trailing space
}

/// Column names of `table` in ordinal order, or `None` when unknown.
/// `table` is passed as typed (may be schema-qualified, may be quoted with
/// [] "" or ``); the callback normalizes.
pub(super) type ColumnsFor<'a> = dyn Fn(&str) -> Option<Vec<String>> + 'a;

/// Cheap reject: true when `content` cannot contain an INSERT (ASCII
/// case-insensitive search for "insert"); callers skip tokenizing entirely.
pub(super) fn may_contain_insert(content: &str) -> bool {
    content
        .as_bytes()
        .windows(6)
        .any(|w| w.eq_ignore_ascii_case(b"insert"))
}

/// Hints for a whole buffer: cheap reject, then `statement_spans` + per
/// statement mapping with offsets rebased to the buffer. Sorted by offset.
pub(super) fn insert_hints_for_buffer(
    content: &str,
    dialect: sqlly_lang::sqlmodel::Dialect,
    columns_for: &ColumnsFor<'_>,
) -> Vec<InsertHint> {
    if !may_contain_insert(content) {
        return Vec::new();
    }
    let mut out = Vec::new();
    for span in statement_spans_for(content, dialect) {
        if span.end <= span.start || span.end > content.len() {
            continue;
        }
        let slice = &content[span.start..span.end];
        if !may_contain_insert(slice) {
            continue;
        }
        for mut hint in insert_value_hints(slice, columns_for) {
            hint.offset += span.start;
            out.push(hint);
        }
    }
    out
}

/// Hints for a single INSERT statement. Empty for anything that is not
/// `INSERT [INTO] <table> [(cols)] [OUTPUT ...] VALUES (...)[, (...)]`.
pub(super) fn insert_value_hints(statement: &str, columns_for: &ColumnsFor<'_>) -> Vec<InsertHint> {
    let tokens: Vec<Token<'_>> = tokenize(statement)
        .into_iter()
        .filter(|t| t.kind != TokenKind::Whitespace && t.kind != TokenKind::Comment)
        .collect();
    if tokens.is_empty() {
        return Vec::new();
    }
    if tokens[0].kind != TokenKind::Keyword || !eq_ci(tokens[0].text, "insert") {
        return Vec::new();
    }
    let mut i = 1;
    if i < tokens.len() && is_word(&tokens[i], "into") {
        i += 1;
    }
    let Some((table_text, next)) = take_table_name(&tokens, i) else {
        return Vec::new();
    };
    i = next;
    if i >= tokens.len() || !is_table_follower(&tokens[i]) {
        return Vec::new();
    }

    let columns = if tokens[i].text == "(" {
        match take_column_list(&tokens, i) {
            Some((cols, next)) => {
                i = next;
                cols
            }
            None => return Vec::new(),
        }
    } else {
        match columns_for(&table_text) {
            Some(cols) => cols,
            None => return Vec::new(),
        }
    };

    if !skip_to_values(&tokens, &mut i) {
        return Vec::new();
    }
    // `i` now indexes the VALUES token; walk from the next token.
    i += 1;
    collect_value_hints(&tokens, i, &columns)
}

/// Column names of `table` in ordinal order from the completion catalog,
/// or `None` when the table/view is unknown. `table` may be schema-qualified
/// and quoted with `[]`, `""`, or backticks.
pub(super) fn catalog_columns_for(catalog: &CompletionCatalog, table: &str) -> Option<Vec<String>> {
    let (schema, name) = split_table_ref(table);
    catalog
        .objects
        .iter()
        .find(|obj| {
            matches!(obj.kind, ObjectKind::Table | ObjectKind::View)
                && obj.name.eq_ignore_ascii_case(&name)
                && match (&schema, &obj.schema) {
                    (Some(want), Some(got)) => got.eq_ignore_ascii_case(want),
                    (Some(_), None) => false,
                    (None, _) => true,
                }
        })
        .map(|obj| obj.columns.clone())
}

/// Split a raw (possibly quoted, possibly qualified) table reference into
/// `(schema, name)` with identifier quotes stripped. Shared with the
/// IDENTITY_INSERT lint.
pub(super) fn split_table_ref(table: &str) -> (Option<String>, String) {
    let parts: Vec<String> = table
        .split('.')
        .map(strip_ident_quotes)
        .filter(|s| !s.is_empty())
        .collect();
    match parts.as_slice() {
        [] => (None, String::new()),
        [name] => (None, name.clone()),
        [.., schema, name] => (Some(schema.clone()), name.clone()),
    }
}

/// Quote-stripped identifier value, with doubled closing quotes unescaped.
fn strip_ident_quotes(raw: &str) -> String {
    sqlly_lang::sqllex::unquote_identifier(raw).into_owned()
}

fn eq_ci(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn is_word(t: &Token<'_>, word: &str) -> bool {
    eq_ci(t.text, word)
}

fn is_name_token(t: &Token<'_>) -> bool {
    matches!(t.kind, TokenKind::Identifier | TokenKind::Keyword)
}

fn is_quote_symbol(t: &Token<'_>) -> bool {
    t.kind == TokenKind::Symbol && matches!(t.text, "\"" | "`")
}

fn is_table_follower(t: &Token<'_>) -> bool {
    t.text == "("
        || is_word(t, "values")
        || is_word(t, "output")
        || is_word(t, "default")
        || is_word(t, "select")
        || is_word(t, "with")
        || is_word(t, "set")
}

fn is_abort_before_values(t: &Token<'_>) -> bool {
    is_word(t, "select")
        || is_word(t, "set")
        || is_word(t, "default")
        || is_word(t, "exec")
        || is_word(t, "execute")
        || is_word(t, "with")
}

fn take_table_name<'a>(tokens: &[Token<'a>], mut i: usize) -> Option<(String, usize)> {
    let mut raw = String::new();
    while i < tokens.len() {
        let t = &tokens[i];
        if is_table_follower(t) {
            break;
        }
        if is_name_token(t) || t.text == "." || is_quote_symbol(t) {
            raw.push_str(t.text);
            i += 1;
            continue;
        }
        return None;
    }
    if raw.is_empty() { None } else { Some((raw, i)) }
}

fn take_column_list(tokens: &[Token<'_>], i: usize) -> Option<(Vec<String>, usize)> {
    if i >= tokens.len() || tokens[i].text != "(" {
        return None;
    }
    let mut cols = Vec::new();
    let mut i = i + 1;
    let mut expect_name = true;
    while i < tokens.len() {
        let t = &tokens[i];
        if t.text == ")" {
            return Some((cols, i + 1));
        }
        if t.text == "(" {
            // Nested list — not a plain column list.
            return None;
        }
        if t.text == "," {
            if expect_name {
                return None;
            }
            expect_name = true;
            i += 1;
            continue;
        }
        if is_quote_symbol(t) {
            i += 1;
            continue;
        }
        if is_name_token(t) {
            if !expect_name {
                return None;
            }
            cols.push(strip_ident_quotes(t.text));
            expect_name = false;
            i += 1;
            continue;
        }
        return None;
    }
    None
}

fn skip_to_values(tokens: &[Token<'_>], i: &mut usize) -> bool {
    let mut depth = 0usize;
    while *i < tokens.len() {
        let t = &tokens[*i];
        if t.text == "(" {
            depth = depth.saturating_add(1);
            *i += 1;
            continue;
        }
        if t.text == ")" {
            depth = depth.saturating_sub(1);
            *i += 1;
            continue;
        }
        if depth == 0 {
            if is_word(t, "values") {
                return true;
            }
            if is_abort_before_values(t) {
                return false;
            }
        }
        *i += 1;
    }
    false
}

fn collect_value_hints(tokens: &[Token<'_>], mut i: usize, columns: &[String]) -> Vec<InsertHint> {
    let mut hints = Vec::new();
    let mut depth = 0usize;
    let mut index = 0usize;
    let mut next_is_value = false;
    while i < tokens.len() {
        let t = &tokens[i];
        if t.text == "(" {
            if depth == 0 {
                index = 0;
                next_is_value = true;
            }
            depth = depth.saturating_add(1);
            i += 1;
            continue;
        }
        if t.text == ")" {
            if depth == 0 {
                break;
            }
            depth = depth.saturating_sub(1);
            next_is_value = false;
            i += 1;
            continue;
        }
        if t.text == "," {
            if depth == 1 {
                next_is_value = true;
            } else if depth == 0 {
                next_is_value = false;
            }
            i += 1;
            continue;
        }
        if depth == 0 {
            break;
        }
        if depth == 1 && next_is_value {
            if index < columns.len() {
                hints.push(InsertHint {
                    offset: t.start,
                    label: format!("{}:", columns[index]),
                });
            }
            index += 1;
            next_is_value = false;
        }
        i += 1;
    }
    hints
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_intel::intellisense::CompletionObject;
    use std::collections::HashMap;

    fn columns_for(table: &str) -> Option<Vec<String>> {
        let mut map = HashMap::new();
        map.insert(
            "orders".to_string(),
            vec![
                "id".to_string(),
                "customer".to_string(),
                "total".to_string(),
            ],
        );
        map.insert(
            "dbo.orders".to_string(),
            vec![
                "id".to_string(),
                "customer".to_string(),
                "total".to_string(),
            ],
        );
        map.insert("t".to_string(), vec!["a".to_string(), "b".to_string()]);
        let parts: Vec<String> = table
            .split('.')
            .map(strip_ident_quotes)
            .map(|s| s.to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect();
        let joined = parts.join(".");
        let last = parts.last()?.clone();
        map.get(&joined).or_else(|| map.get(&last)).cloned()
    }

    fn hints(sql: &str) -> Vec<InsertHint> {
        insert_value_hints(sql, &columns_for)
    }

    fn labels_at(sql: &str) -> Vec<(String, usize)> {
        hints(sql)
            .into_iter()
            .map(|h| (h.label, h.offset))
            .collect()
    }

    #[test]
    fn explicit_column_list_maps_in_order() {
        let sql = "INSERT INTO t (a, b) VALUES (1, 'x')";
        let got = labels_at(sql);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].0, "a:");
        assert_eq!(&sql[got[0].1..got[0].1 + 1], "1");
        assert_eq!(got[1].0, "b:");
        assert_eq!(&sql[got[1].1..got[1].1 + 3], "'x'");
    }

    #[test]
    fn table_order_when_no_list() {
        let sql = "INSERT INTO orders VALUES (1, 'ACME', 9.5)";
        let got = labels_at(sql);
        assert_eq!(
            got.iter().map(|(l, _)| l.as_str()).collect::<Vec<_>>(),
            ["id:", "customer:", "total:"]
        );
        assert_eq!(&sql[got[0].1..got[0].1 + 1], "1");
        assert_eq!(&sql[got[1].1..got[1].1 + 6], "'ACME'");
        assert_eq!(&sql[got[2].1..got[2].1 + 3], "9.5");
    }

    #[test]
    fn schema_qualified_and_bracketed_table() {
        let sql = "INSERT [dbo].[orders] VALUES (1, 'a', 2)";
        let got = labels_at(sql);
        assert_eq!(got.len(), 3);
        assert_eq!(
            got.iter().map(|(l, _)| l.as_str()).collect::<Vec<_>>(),
            ["id:", "customer:", "total:"]
        );
    }

    #[test]
    fn multi_row_resets_per_row() {
        let sql = "INSERT INTO orders VALUES (1,'a',2), (3,'b',4)";
        let got = labels_at(sql);
        assert_eq!(got.len(), 6);
        let labels: Vec<&str> = got.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(
            labels,
            ["id:", "customer:", "total:", "id:", "customer:", "total:"]
        );
    }

    #[test]
    fn nested_function_calls_and_parens_do_not_split() {
        let sql = "INSERT INTO orders VALUES (1, CONCAT('a', ','), COALESCE((SELECT 1), 2))";
        let got = labels_at(sql);
        assert_eq!(got.len(), 3);
        assert_eq!(&sql[got[0].1..got[0].1 + 1], "1");
        assert_eq!(&sql[got[1].1..got[1].1 + 6], "CONCAT");
        assert_eq!(&sql[got[2].1..got[2].1 + 8], "COALESCE");
    }

    #[test]
    fn strings_with_commas_and_parens_are_one_value() {
        let sql = "INSERT INTO orders VALUES (1, 'x, (y)', 2)";
        let got = labels_at(sql);
        assert_eq!(got.len(), 3);
        assert_eq!(&sql[got[1].1..got[1].1 + 8], "'x, (y)'");
    }

    #[test]
    fn more_values_than_columns_stops_labelling() {
        let sql = "INSERT INTO t (a, b) VALUES (1, 2, 3)";
        let got = labels_at(sql);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].0, "a:");
        assert_eq!(got[1].0, "b:");
    }

    #[test]
    fn insert_select_yields_nothing() {
        let sql = "INSERT INTO t (a) SELECT a FROM u";
        assert!(hints(sql).is_empty());
    }

    #[test]
    fn default_values_and_insert_set_yield_nothing() {
        assert!(hints("INSERT INTO t DEFAULT VALUES").is_empty());
        assert!(hints("INSERT INTO t SET a = 1").is_empty());
    }

    #[test]
    fn unknown_table_without_list_yields_nothing() {
        assert!(hints("INSERT INTO nope VALUES (1)").is_empty());
    }

    #[test]
    fn output_clause_between_list_and_values_is_tolerated() {
        let sql = "INSERT INTO t (a) OUTPUT inserted.a VALUES (1)";
        let got = labels_at(sql);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, "a:");
        assert_eq!(&sql[got[0].1..got[0].1 + 1], "1");
    }

    #[test]
    fn buffer_scan_rebases_offsets_and_skips_non_insert_statements() {
        let sql = "SELECT 1;\nINSERT INTO t (a) VALUES (7);";
        let got =
            insert_hints_for_buffer(sql, sqlly_lang::sqlmodel::Dialect::SqlServer, &columns_for);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].label, "a:");
        assert_eq!(&sql[got[0].offset..got[0].offset + 1], "7");
    }

    #[test]
    fn may_contain_insert_is_case_insensitive_and_cheap() {
        assert!(!may_contain_insert("select 1"));
        assert!(may_contain_insert("InSeRt"));
    }

    #[test]
    fn catalog_columns_for_unqualified_lookup() {
        let catalog = CompletionCatalog {
            objects: vec![CompletionObject::table(
                "dbo",
                "orders",
                vec!["id".into(), "customer".into()],
            )],
            ..CompletionCatalog::default()
        };
        assert_eq!(
            catalog_columns_for(&catalog, "orders"),
            Some(vec!["id".into(), "customer".into()])
        );
        assert_eq!(
            catalog_columns_for(&catalog, "[Orders]"),
            Some(vec!["id".into(), "customer".into()])
        );
    }

    #[test]
    fn catalog_columns_for_qualified_lookup_strips_quoting() {
        let catalog = CompletionCatalog {
            objects: vec![CompletionObject::table("dbo", "orders", vec!["id".into()])],
            ..CompletionCatalog::default()
        };
        assert_eq!(
            catalog_columns_for(&catalog, "[dbo].[orders]"),
            Some(vec!["id".into()])
        );
        assert_eq!(
            catalog_columns_for(&catalog, "\"dbo\".\"orders\""),
            Some(vec!["id".into()])
        );
        assert!(catalog_columns_for(&catalog, "other.orders").is_none());
    }

    #[test]
    fn quoted_names_with_escaped_quotes_are_unescaped() {
        assert_eq!(strip_ident_quotes("[a]]b]"), "a]b");
        assert_eq!(strip_ident_quotes("\"a\"\"b\""), "a\"b");
        assert_eq!(strip_ident_quotes("`a``b`"), "a`b");
    }
}
