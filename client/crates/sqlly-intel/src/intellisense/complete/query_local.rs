//! Query-local completion items: parameters declared in the statement,
//! alias member access (`o.` -> columns of the aliased table), columns from
//! the tables referenced in the query's FROM/JOIN clauses, and alias-slot
//! detection (`FROM Schema.Table a`).

use super::context::{
    TableRef, depth_words, is_reserved_alias, query_cte_names, query_table_refs,
    query_table_refs_at_depth, script_drops, script_tables, statement_tokens,
};
use super::filesystem::find_filesystem_table_columns;
use crate::intellisense::score::qualified_object_score;
use crate::intellisense::score::{push_match, push_match_joined};
use crate::intellisense::types::{
    CompletionCatalog, CompletionItem, CompletionKind, CompletionObject, CompletionSource,
};

/// Returns `true` when any of the query table refs matches a catalog object
/// (live metadata or filesystem). Used to gate strict scoping: when query
/// refs exist and match, catalog-wide column emission is suppressed in
/// column-expecting contexts.
pub(crate) fn has_matching_query_refs(refs: &[TableRef], catalog: &CompletionCatalog) -> bool {
    for tr in refs {
        if tr.columns.is_some() || find_object(catalog, &tr.name).is_some() {
            return true;
        }
        if !find_filesystem_table_columns(catalog, &tr.name).is_empty() {
            return true;
        }
    }
    false
}

/// Offers columns from tables referenced in the FROM/JOIN clauses before the
/// cursor. Called in column-expecting contexts (WHERE, GROUP BY, ORDER BY,
/// JOIN ON, etc.) so the editor suggests in-scope columns rather than
/// columns from every catalog object.
pub(crate) fn add_columns_from_query_tables(
    items: &mut Vec<CompletionItem>,
    refs: &[TableRef],
    catalog: &CompletionCatalog,
    prefix: &str,
) {
    // An aliased source's columns are offered alias-qualified only — label and
    // insert both — since that is how the query must name them (feedback
    // ifb_56ee). An unaliased source's columns are offered bare.
    let mut label_buf = String::new();
    for table_ref in refs {
        if let Some(columns) = ref_columns(catalog, table_ref) {
            for column in columns {
                match &table_ref.alias {
                    Some(alias) => {
                        let qualified = format!("{alias}.{column}");
                        push_match_joined(
                            items,
                            alias,
                            column,
                            &qualified,
                            CompletionKind::Column,
                            CompletionSource::Inferred,
                            85,
                            prefix,
                            &mut label_buf,
                        );
                    }
                    None => push_match(
                        items,
                        column,
                        column,
                        CompletionKind::Column,
                        CompletionSource::Inferred,
                        85,
                        prefix,
                    ),
                }
            }
        }
        for (col_name, _schema) in find_filesystem_table_columns(catalog, &table_ref.name) {
            match &table_ref.alias {
                Some(alias) => {
                    let qualified = format!("{alias}.{col_name}");
                    push_match_joined(
                        items,
                        alias,
                        col_name,
                        &qualified,
                        CompletionKind::Column,
                        CompletionSource::Filesystem,
                        83,
                        prefix,
                        &mut label_buf,
                    );
                }
                None => push_match(
                    items,
                    col_name,
                    col_name,
                    CompletionKind::Column,
                    CompletionSource::Filesystem,
                    83,
                    prefix,
                ),
            }
        }
    }
}

pub(crate) fn add_query_local_items(
    items: &mut Vec<CompletionItem>,
    sql: &str,
    cursor: usize,
    catalog: &CompletionCatalog,
    prefix: &str,
) {
    let mut label_buf = String::new();
    for parameter in query_parameters(sql, cursor) {
        push_match(
            items,
            &parameter,
            &parameter,
            CompletionKind::Parameter,
            CompletionSource::Inferred,
            78,
            prefix,
        );
    }
    if let Some((alias, column_prefix)) = alias_prefix(prefix) {
        for table_ref in visible_aliases(sql, cursor) {
            let Some(found_alias) = table_ref.alias.as_deref() else {
                continue;
            };
            if !found_alias.eq_ignore_ascii_case(alias) {
                continue;
            }
            let object_name = &table_ref.name;
            if let Some(columns) = ref_columns(catalog, &table_ref) {
                for column in columns {
                    push_match_joined(
                        items,
                        found_alias,
                        column,
                        column,
                        CompletionKind::Column,
                        CompletionSource::Inferred,
                        92,
                        column_prefix,
                        &mut label_buf,
                    );
                }
            }
            for (col_name, col_schema) in find_filesystem_table_columns(catalog, object_name) {
                let _ = col_schema;
                push_match_joined(
                    items,
                    found_alias,
                    col_name,
                    col_name,
                    CompletionKind::Column,
                    CompletionSource::Filesystem,
                    90,
                    column_prefix,
                    &mut label_buf,
                );
            }
        }
    }
}

/// In a join condition after `alias.Col =`, the in-scope columns a foreign
/// key links to that column — outbound (`alias.Col` references them) or
/// inbound (they reference `alias.Col`) — ranked first with an `fk` detail,
/// then columns elsewhere in scope spelled the same (feedback ifb_e85a). The
/// generic in-scope column list follows at its usual score.
pub(crate) fn add_join_key_items(
    items: &mut Vec<CompletionItem>,
    sql: &str,
    cursor: usize,
    refs: &[TableRef],
    catalog: &CompletionCatalog,
    prefix: &str,
) {
    let Some((left_alias, left_column)) = comparison_left_operand(sql, cursor, prefix) else {
        return;
    };
    let Some(left_ref) = refs.iter().find(|r| {
        r.alias
            .as_deref()
            .is_some_and(|a| a.eq_ignore_ascii_case(&left_alias))
            || r.name.eq_ignore_ascii_case(&left_alias)
    }) else {
        return;
    };
    let left_object = find_object(catalog, &left_ref.name);

    // (schema, table, column) endpoints the left column is keyed to.
    let mut endpoints: Vec<(Option<String>, String, String)> = Vec::new();
    if let Some(object) = left_object {
        for fk in &object.foreign_keys {
            if fk.column.eq_ignore_ascii_case(&left_column) {
                let schema = (!fk.target_schema.is_empty()).then(|| fk.target_schema.clone());
                endpoints.push((schema, fk.target_table.clone(), fk.target_column.clone()));
            }
        }
        for other in &catalog.objects {
            for fk in &other.foreign_keys {
                let same_table = fk.target_table.eq_ignore_ascii_case(&object.name)
                    && (fk.target_schema.is_empty()
                        || object
                            .schema
                            .as_deref()
                            .is_none_or(|s| s.eq_ignore_ascii_case(&fk.target_schema)));
                if same_table && fk.target_column.eq_ignore_ascii_case(&left_column) {
                    endpoints.push((other.schema.clone(), other.name.clone(), fk.column.clone()));
                }
            }
        }
    }

    let mut label_buf = String::new();
    let mut pushed: Vec<String> = Vec::new();
    let mut push = |items: &mut Vec<CompletionItem>,
                    table_ref: &TableRef,
                    column: &str,
                    score: i32,
                    detail: &str,
                    pushed: &mut Vec<String>| {
        let (label, insert) = match &table_ref.alias {
            Some(alias) => (format!("{alias}.{column}"), format!("{alias}.{column}")),
            None => (column.to_string(), column.to_string()),
        };
        if pushed.iter().any(|seen| seen.eq_ignore_ascii_case(&label)) {
            return;
        }
        let Some(fuzzy) = qualified_object_score(prefix, &label) else {
            return;
        };
        pushed.push(label.clone());
        label_buf.clear();
        items.push(CompletionItem {
            label,
            insert_text: insert,
            kind: CompletionKind::Column,
            source: CompletionSource::Inferred,
            score: score + fuzzy,
            detail: detail.to_string(),
            replace_span: None,
        });
    };

    for table_ref in refs {
        if std::ptr::eq(table_ref, left_ref) {
            continue;
        }
        let object = find_object(catalog, &table_ref.name);
        if let Some(object) = object {
            for (schema, table, column) in &endpoints {
                let table_matches = object.name.eq_ignore_ascii_case(table)
                    && match (schema, object.schema.as_deref()) {
                        (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
                        _ => true,
                    };
                if table_matches
                    && object
                        .columns
                        .iter()
                        .any(|c| c.eq_ignore_ascii_case(column))
                {
                    push(
                        items,
                        table_ref,
                        column,
                        JOIN_KEY_FK_SCORE,
                        "fk",
                        &mut pushed,
                    );
                }
            }
        }
        if let Some(columns) = ref_columns(catalog, table_ref)
            && let Some(column) = columns
                .iter()
                .find(|c| c.eq_ignore_ascii_case(&left_column))
        {
            push(
                items,
                table_ref,
                column,
                JOIN_KEY_SAME_NAME_SCORE,
                "same name",
                &mut pushed,
            );
        }
    }
}

/// Score of a join-key column linked by a foreign key to the column on the
/// other side of the comparison: above every other in-scope column (85).
pub(crate) const JOIN_KEY_FK_SCORE: i32 = 97;
/// Score of a join-key column that merely shares the compared column's name.
pub(crate) const JOIN_KEY_SAME_NAME_SCORE: i32 = 90;

/// The `alias.Column` on the left of the `=` the caret follows (`a.Id = ▏`,
/// `a.Id = b▏`), when the left side is a qualified column reference.
fn comparison_left_operand(sql: &str, cursor: usize, prefix: &str) -> Option<(String, String)> {
    let cursor = cursor.min(sql.len());
    let before = sql[..cursor.saturating_sub(prefix.len())].trim_end();
    let before = before.strip_suffix('=')?.trim_end();
    let is_ident = |ch: char| {
        ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '#' | '@' | '$' | '[' | ']')
    };
    let start = before.rfind(|ch: char| !is_ident(ch)).map_or(0, |i| i + 1);
    let operand = &before[start..];
    let (alias, column) = operand.rsplit_once('.')?;
    let strip = |s: &str| s.trim_matches(|ch| ch == '[' || ch == ']').to_string();
    let (alias, column) = (strip(alias), strip(column));
    (!alias.is_empty() && !column.is_empty() && !alias.contains('.')).then_some((alias, column))
}

/// The caret statement's CTEs as FROM / JOIN sources, ranked above the
/// catalog's tables: a CTE exists to be read by the query that declares it
/// (feedback ifb_b360).
pub(crate) fn add_cte_source_items(
    items: &mut Vec<CompletionItem>,
    sql: &str,
    cursor: usize,
    prefix: &str,
) {
    for name in query_cte_names(sql, cursor) {
        if let Some(fuzzy) = qualified_object_score(prefix, &name) {
            items.push(CompletionItem {
                insert_text: name.clone(),
                label: name,
                kind: CompletionKind::Table,
                source: CompletionSource::Inferred,
                score: CTE_SOURCE_SCORE + fuzzy,
                detail: "cte".to_string(),
                replace_span: None,
            });
        }
    }
}

/// Score of a CTE offered as a FROM / JOIN source: above the FK join
/// suggestions (95) and the catalog tables (80–90).
pub(crate) const CTE_SOURCE_SCORE: i32 = 100;

/// After `DROP TABLE [IF EXISTS] ▏`, the tables the script itself creates —
/// `SELECT … INTO #t`, `CREATE [TEMP] TABLE t`, `INSERT INTO #t` — wherever
/// they sit in the script, so a cleanup line at the top can name the temp
/// table defined below it. A table that another `DROP TABLE` already clears
/// above its definition is left out; a drop below the definition (end-of-
/// script cleanup) does not hide it (feedback ifb_416b).
pub(crate) fn add_script_table_drop_items(
    items: &mut Vec<CompletionItem>,
    sql: &str,
    cursor: usize,
    prefix: &str,
) {
    let tables = script_tables(sql);
    if tables.is_empty() {
        return;
    }
    // The caret's own `DROP` (the last one before the cursor in its
    // statement) is the one being written: it never counts as "already
    // dropped". The statement splitter can lump an unterminated head with
    // the line below it, so the keyword, not the statement, identifies it.
    let (tokens, _) = statement_tokens(sql, cursor);
    let own_drop_at = tokens
        .iter()
        .filter(|t| t.byte_offset < cursor && t.text.as_str().eq_ignore_ascii_case("DROP"))
        .map(|t| t.byte_offset)
        .next_back();
    let drops = script_drops(sql);
    for table in tables {
        let already_dropped_above = drops.iter().any(|(name, at)| {
            name.eq_ignore_ascii_case(&table.name)
                && *at < table.defined_at
                && Some(*at) != own_drop_at
        });
        if already_dropped_above {
            continue;
        }
        if let Some(fuzzy) = qualified_object_score(prefix, &table.name) {
            items.push(CompletionItem {
                insert_text: table.name.clone(),
                label: table.name,
                kind: CompletionKind::Table,
                source: CompletionSource::Inferred,
                score: SCRIPT_TABLE_DROP_SCORE + fuzzy,
                detail: if table.is_temp {
                    "temp table".to_string()
                } else {
                    "created in this script".to_string()
                },
                replace_span: None,
            });
        }
    }
}

/// Score of a script-created table offered after `DROP TABLE`: above the
/// catalog tables (80–90).
pub(crate) const SCRIPT_TABLE_DROP_SCORE: i32 = 100;

/// The columns a FROM / JOIN source contributes: a derived table's own
/// select-list names, or the catalog object's columns.
fn ref_columns<'a>(
    catalog: &'a CompletionCatalog,
    table_ref: &'a TableRef,
) -> Option<&'a [String]> {
    match &table_ref.columns {
        Some(columns) => Some(columns),
        None => find_object(catalog, &table_ref.name).map(|object| object.columns.as_slice()),
    }
}

pub(crate) fn find_object<'a>(
    catalog: &'a CompletionCatalog,
    name: &str,
) -> Option<&'a CompletionObject> {
    let parts: Vec<&str> = name
        .split('.')
        .map(|part| part.trim_matches(|ch| ch == '[' || ch == ']'))
        .filter(|part| !part.is_empty())
        .collect();
    catalog
        .objects
        .iter()
        .find(|object| object_matches_parts(object, &parts))
}

fn object_matches_parts(object: &CompletionObject, parts: &[&str]) -> bool {
    match parts.len() {
        1 => object.name.eq_ignore_ascii_case(parts[0]),
        2 => {
            object.name.eq_ignore_ascii_case(parts[1])
                && object
                    .schema
                    .as_ref()
                    .is_some_and(|schema| schema.eq_ignore_ascii_case(parts[0]))
        }
        3 => {
            object.name.eq_ignore_ascii_case(parts[2])
                && object
                    .schema
                    .as_ref()
                    .is_some_and(|schema| schema.eq_ignore_ascii_case(parts[1]))
                && object
                    .database
                    .as_ref()
                    .is_some_and(|database| database.eq_ignore_ascii_case(parts[0]))
        }
        _ => false,
    }
}

fn alias_prefix(prefix: &str) -> Option<(&str, &str)> {
    let (alias, column) = prefix.split_once('.')?;
    if alias.is_empty() || alias.contains('.') {
        return None;
    }
    Some((alias, column))
}

pub fn query_aliases(sql: &str, cursor: usize) -> Vec<(String, String)> {
    query_table_refs(sql, cursor)
        .into_iter()
        .filter_map(|tr| tr.alias.map(|a| (a, tr.name)))
        .collect()
}

/// Aliases the cursor can legally reference: the ones declared in its own
/// parenthesis scope, plus every enclosing scope (a nested subquery may
/// reference an outer query's alias, but not a sibling's). Inner scopes are
/// collected first so a shadowed alias resolves to the nearest declaration.
///
/// Alias member-access used to resolve against `query_aliases(sql, sql.len())`
/// — the scope at the *end of the document*, which is depth 0 for any balanced
/// text. Inside a nested subquery (e.g. a `FOR JSON PATH` block in a procedure
/// body) that scope holds none of the caret's aliases, so `j.` produced nothing
/// while the unqualified position one column to the left listed `j.JournalId`
/// correctly. See feedback `ifb_5e41d289a60a47e08ff0476646fec0b5`.
///
/// Every returned ref has an alias; a derived table's carries its columns.
pub(crate) fn visible_aliases(sql: &str, cursor: usize) -> Vec<TableRef> {
    let (tokens, depth) = statement_tokens(sql, cursor);
    let mut out: Vec<TableRef> = Vec::new();
    for level in (0..=depth).rev() {
        for table_ref in query_table_refs_at_depth(&tokens, level) {
            let Some(alias) = table_ref.alias.as_deref() else {
                continue;
            };
            if out.iter().any(|seen| {
                seen.alias
                    .as_deref()
                    .is_some_and(|seen| seen.eq_ignore_ascii_case(alias))
            }) {
                continue;
            }
            out.push(table_ref);
        }
    }
    super::context::fill_script_table_columns(sql, &mut out);
    out
}

/// Where the cursor sits relative to a table-reference alias.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AliasContext {
    /// Not inside an alias slot; resolve completions normally.
    None,
    /// The alias name is actively being typed (cursor abuts the token, no
    /// trailing whitespace). Nothing should be suggested.
    Defining,
    /// The alias is complete and followed by whitespace. Only the clauses that
    /// may follow a table reference should be offered.
    After,
}

/// Classifies the cursor position against a `FROM`/`JOIN <object> [AS] <alias>`
/// clause. Detection is a single backward-bounded scan over the text before the
/// cursor so it stays well within the intellisense latency budget.
pub(crate) fn alias_context(sql: &str, cursor: usize) -> AliasContext {
    let cursor = cursor.min(sql.len());
    let before = &sql[..cursor];

    let mid_word = before
        .chars()
        .last()
        .is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == '_');

    let words = depth_words(sql, cursor, true);

    if words.is_empty() {
        return AliasContext::None;
    }

    let Some(from_idx) = words
        .iter()
        .rposition(|word| word.eq_ignore_ascii_case("FROM") || word.eq_ignore_ascii_case("JOIN"))
    else {
        return AliasContext::None;
    };
    let rest = &words[from_idx + 1..];
    if rest.is_empty() {
        return AliasContext::None;
    }

    if mid_word {
        let in_progress = rest.last().unwrap();
        if in_progress.eq_ignore_ascii_case("AS") || is_reserved_alias(in_progress) {
            return AliasContext::None;
        }
        let completed = &rest[..rest.len() - 1];
        let is_alias_slot = match completed {
            [object] => !is_reserved_alias(object),
            [object, as_kw] => !is_reserved_alias(object) && as_kw.eq_ignore_ascii_case("AS"),
            _ => false,
        };
        if is_alias_slot {
            return AliasContext::Defining;
        }
        return AliasContext::None;
    }

    let after = match rest {
        [object, alias] => {
            !is_reserved_alias(object)
                && !alias.eq_ignore_ascii_case("AS")
                && !is_reserved_alias(alias)
        }
        [object, as_kw, alias] => {
            !is_reserved_alias(object)
                && as_kw.eq_ignore_ascii_case("AS")
                && !is_reserved_alias(alias)
        }
        _ => false,
    };
    if after {
        AliasContext::After
    } else {
        AliasContext::None
    }
}

pub(crate) fn query_parameters(sql: &str, cursor: usize) -> Vec<String> {
    let mut params = Vec::<String>::new();
    let mut chars = sql[..cursor.min(sql.len())].char_indices().peekable();
    while let Some((idx, ch)) = chars.next() {
        if ch != '@' || sql[idx..].starts_with("@@") {
            continue;
        }
        let start = idx;
        let mut end = idx + 1;
        while let Some((next_idx, next)) = chars.peek().copied() {
            if next.is_ascii_alphanumeric() || next == '_' {
                end = next_idx + next.len_utf8();
                chars.next();
            } else {
                break;
            }
        }
        // The word being typed at the caret is not a suggestion for itself.
        if end > start + 1 && end < cursor {
            let value = sql[start..end].to_string();
            if !params
                .iter()
                .any(|existing| existing.eq_ignore_ascii_case(&value))
            {
                params.push(value);
            }
        }
    }
    params
}

#[cfg(test)]
mod tests {
    use super::super::testutil::{
        accounting_catalog, catalog, cookbook_catalog, labels, multi_schema_catalog,
    };
    use super::*;
    use crate::intellisense::complete::complete;
    use sqlly_core::ObjectKind;
    #[cfg(not(target_arch = "wasm32"))]
    use std::time::Instant;
    #[cfg(target_arch = "wasm32")]
    use web_time::Instant;

    #[test]
    fn find_object_resolves_bare_and_qualified_names() {
        let catalog = multi_schema_catalog();
        assert!(find_object(&catalog, "Orders").is_some());
        assert!(find_object(&catalog, "Sales.Orders").is_some());
        assert!(find_object(&catalog, "[Sales].[Orders]").is_some());
        assert!(find_object(&catalog, "dbo.Foo").is_none());
    }

    #[test]
    fn offers_alias_columns_from_query_context() {
        let items = complete("SELECT o. FROM sales.Orders o", 9, &catalog());
        assert!(items.iter().any(|item| {
            item.label == "o.OrderId"
                && item.kind == CompletionKind::Column
                && item.source == CompletionSource::Inferred
        }));
    }

    #[test]
    fn offers_query_parameters_after_at_prefix() {
        let items = complete("DECLARE @CustomerId int; SELECT @Cus", 35, &catalog());
        assert!(
            items.iter().any(|item| {
                item.label == "@CustomerId" && item.kind == CompletionKind::Parameter
            })
        );
    }

    #[test]
    fn alias_dot_still_resolves_columns_with_dotted_path_active() {
        // The dotted path must not regress alias member access.
        let sql = "SELECT o. FROM Sales.Orders o";
        let items = complete(sql, 9, &multi_schema_catalog());
        assert!(items.iter().any(|item| {
            item.label == "o.OrderId"
                && item.kind == CompletionKind::Column
                && item.source == CompletionSource::Inferred
        }));
    }

    #[test]
    fn defining_alias_suggests_nothing() {
        // Cursor sits immediately after the alias token with no trailing
        // whitespace: the user is still typing the alias name, so nothing
        // should be suggested.
        let sql = "SELECT * FROM Sales.Orders o";
        let items = complete(sql, sql.len(), &multi_schema_catalog());
        assert!(
            items.is_empty(),
            "expected no suggestions while defining an alias, got {:?}",
            labels(&items)
        );
    }

    #[test]
    fn defining_alias_with_as_suggests_nothing() {
        let sql = "SELECT * FROM Sales.Orders AS o";
        let items = complete(sql, sql.len(), &multi_schema_catalog());
        assert!(
            items.is_empty(),
            "expected no suggestions while defining an aliased name, got {:?}",
            labels(&items)
        );
    }

    /// Clause keywords lead — WHERE first (feedback ifb_92d9) — followed only
    /// by the source's alias-qualified columns, each writing the WHERE clause.
    fn assert_post_alias_items(items: &[CompletionItem]) {
        let found = labels(items);
        assert_eq!(
            found.first().map(String::as_str),
            Some("WHERE"),
            "got {found:?}"
        );
        let keyword_count = items
            .iter()
            .take_while(|item| item.kind == CompletionKind::Keyword)
            .count();
        for expected in ["GROUP BY", "ORDER BY", "INNER JOIN", "LEFT JOIN"] {
            assert!(
                found[..keyword_count].iter().any(|label| label == expected),
                "expected `{expected}` among the leading keywords, got {found:?}"
            );
        }
        for item in &items[keyword_count..] {
            assert_eq!(item.kind, CompletionKind::Column, "got {found:?}");
            assert!(item.label.starts_with("o."), "got {found:?}");
            assert_eq!(item.insert_text, format!("WHERE\n\t{}", item.label));
        }
        assert!(
            keyword_count < items.len(),
            "expected the source's columns after the keywords, got {found:?}"
        );
    }

    #[test]
    fn after_alias_offers_clause_keywords() {
        // A trailing space after the alias means the alias is complete: the
        // clauses that can follow a table reference lead, then the columns.
        let sql = "SELECT * FROM Sales.Orders o ";
        assert_post_alias_items(&complete(sql, sql.len(), &multi_schema_catalog()));
    }

    #[test]
    fn after_alias_with_as_offers_clause_keywords() {
        let sql = "SELECT * FROM Sales.Orders AS o ";
        assert_post_alias_items(&complete(sql, sql.len(), &multi_schema_catalog()));
    }

    #[test]
    fn object_without_alias_still_offers_normal_completions() {
        // `FROM Sales.Orders ` (no alias token yet) is ambiguous: the user may
        // be about to type an alias or a keyword. We must not suppress the
        // existing keyword/snippet suggestions in that case.
        let sql = "SELECT * FROM Sales.Orders ";
        let items = complete(sql, sql.len(), &multi_schema_catalog());
        assert!(
            !items.is_empty(),
            "object without an alias must keep offering completions"
        );
    }

    #[test]
    fn defining_alias_does_not_break_object_completion() {
        // Typing the object name itself must still resolve tables.
        let sql = "SELECT * FROM Sales.Ord";
        let items = complete(sql, sql.len(), &multi_schema_catalog());
        assert!(
            items.iter().any(|item| item.label == "Sales.Orders"),
            "object completion must survive alias detection, got {:?}",
            labels(&items)
        );
    }

    #[test]
    fn alias_aware_completion_stays_under_latency_budget() {
        let mut catalog = multi_schema_catalog();
        for idx in 0..1_000 {
            catalog
                .objects
                .push(crate::intellisense::types::CompletionObject {
                    database: Some("AppDb".to_string()),
                    schema: Some("Shared".to_string()),
                    name: format!("Table{idx}"),
                    kind: ObjectKind::Table,
                    columns: vec!["Id".to_string(), "Name".to_string()],
                    parameters: Vec::new(),
                    foreign_keys: Vec::new(),
                });
        }
        let sql = "SELECT * FROM Sales.Orders o ";
        let started = Instant::now();
        let items = complete(sql, sql.len(), &catalog);
        assert!(!items.is_empty());
        assert!(started.elapsed() < std::time::Duration::from_millis(50));
    }

    /// `visible_aliases` walks one scope level per enclosing paren, so a deeply
    /// nested caret must not turn alias-dot completion into an O(depth × doc)
    /// re-scan. Guards the fix for feedback `ifb_5e41` staying cheap.
    #[test]
    fn deeply_nested_alias_dot_stays_under_latency_budget() {
        let mut sql = String::new();
        for _ in 0..30 {
            sql.push_str("SELECT * FROM Sales.Orders o WHERE OrderId IN (");
        }
        sql.push_str("SELECT * FROM Sales.Orders o2 WHERE o2.");
        let cursor = sql.len();
        sql.push_str(&")".repeat(30));
        let started = Instant::now();
        let items = complete(&sql, cursor, &multi_schema_catalog());
        assert!(
            items.iter().any(|item| item.label == "o2.OrderId"),
            "innermost alias should still resolve, got {:?}",
            labels(&items)
        );
        assert!(started.elapsed() < std::time::Duration::from_millis(50));
    }

    #[test]
    fn alias_dot_completes_view_columns() {
        // Alias `v` for a view should expose the view's columns.
        let items = complete(
            "SELECT v. FROM Sales.vCustomerOrders v",
            9,
            &cookbook_catalog(),
        );
        assert!(
            items.iter().any(|item| {
                item.label == "v.CustomerId" && item.kind == CompletionKind::Column
            })
        );
    }

    // ---- Column completion in WHERE clause ----------------------------

    #[test]
    fn offers_columns_in_where_clause() {
        let sql = "SELECT * FROM Sales.SalesOrder WHERE Cust";
        let items = complete(sql, sql.len(), &cookbook_catalog());
        assert!(
            items.iter().any(|item| {
                item.label == "CustomerId"
                    && item.kind == CompletionKind::Column
                    && item.source == CompletionSource::Inferred
            }),
            "WHERE clause should offer columns from query tables, got {:?}",
            labels(&items)
        );
    }

    #[test]
    fn offers_columns_in_where_after_and() {
        let sql = "SELECT * FROM Sales.SalesOrder WHERE OrderId = 1 AND Cust";
        let items = complete(sql, sql.len(), &cookbook_catalog());
        assert!(
            items
                .iter()
                .any(|item| { item.label == "CustomerId" && item.kind == CompletionKind::Column }),
            "WHERE after AND should still offer columns, got {:?}",
            labels(&items)
        );
    }

    #[test]
    fn offers_columns_in_where_from_multiple_tables() {
        let sql = "SELECT * FROM Sales.Customer c JOIN Sales.SalesOrder o \
                   ON c.CustomerId = o.CustomerId WHERE Cust";
        let items = complete(sql, sql.len(), &cookbook_catalog());
        // "CustomerId" exists in both aliased tables — offered once per alias,
        // qualified, since the bare name would be ambiguous (feedback ifb_56ee).
        for expected in ["c.CustomerId", "o.CustomerId"] {
            assert!(
                items
                    .iter()
                    .any(|item| item.label == expected && item.kind == CompletionKind::Column),
                "WHERE with joined tables should offer {expected}, got {:?}",
                labels(&items)
            );
        }
        assert!(
            items.iter().all(|item| item.label != "CustomerId"),
            "an aliased source's columns are not offered bare, got {:?}",
            labels(&items)
        );
        // Also verify columns from the second table (SalesOrder) are offered
        let sql2 = "SELECT * FROM Sales.Customer c JOIN Sales.SalesOrder o \
                    ON c.CustomerId = o.CustomerId WHERE Order";
        let items2 = complete(sql2, sql2.len(), &cookbook_catalog());
        assert!(
            items2
                .iter()
                .any(|item| { item.label == "o.OrderId" && item.kind == CompletionKind::Column }),
            "WHERE with joined tables should offer o.OrderId from SalesOrder, got {:?}",
            labels(&items2)
        );
    }

    // ---- Column completion in GROUP BY clause -------------------------

    #[test]
    fn offers_columns_in_group_by_clause() {
        let sql = "SELECT * FROM Sales.Orders GROUP BY Cust";
        let items = complete(sql, sql.len(), &multi_schema_catalog());
        assert!(
            items
                .iter()
                .any(|item| { item.label == "CustomerId" && item.kind == CompletionKind::Column }),
            "GROUP BY should offer columns from query tables, got {:?}",
            labels(&items)
        );
    }

    // ---- Column completion in ORDER BY clause -------------------------

    #[test]
    fn offers_columns_in_order_by_clause() {
        let sql = "SELECT * FROM Sales.Orders ORDER BY Cust";
        let items = complete(sql, sql.len(), &multi_schema_catalog());
        assert!(
            items
                .iter()
                .any(|item| { item.label == "CustomerId" && item.kind == CompletionKind::Column }),
            "ORDER BY should offer columns from query tables, got {:?}",
            labels(&items)
        );
    }

    // ---- Column completion in JOIN ON clause --------------------------

    #[test]
    fn offers_columns_in_join_on_clause() {
        let sql = "SELECT * FROM Sales.Customer c JOIN Sales.SalesOrder o ON c.";
        let items = complete(sql, sql.len(), &cookbook_catalog());
        assert!(
            items.iter().any(|item| {
                item.label == "c.CustomerId" && item.kind == CompletionKind::Column
            }),
            "JOIN ON alias. should offer columns, got {:?}",
            labels(&items)
        );
    }

    #[test]
    fn offers_unqualified_columns_in_join_on_clause() {
        let sql = "SELECT * FROM Sales.Customer c JOIN Sales.SalesOrder o \
                   ON c.CustomerId = o.CustomerId AND Customer";
        let items = complete(sql, sql.len(), &cookbook_catalog());
        for expected in ["c.CustomerId", "o.CustomerId"] {
            assert!(
                items
                    .iter()
                    .any(|item| item.label == expected && item.kind == CompletionKind::Column),
                "JOIN ON unqualified should offer {expected} from both tables, got {:?}",
                labels(&items)
            );
        }
    }

    // ---- Column completion in HAVING clause ---------------------------

    #[test]
    fn offers_columns_in_having_clause() {
        let sql = "SELECT CustomerId, COUNT(*) FROM Sales.SalesOrder \
                   GROUP BY CustomerId HAVING Cust";
        let items = complete(sql, sql.len(), &cookbook_catalog());
        assert!(
            items
                .iter()
                .any(|item| { item.label == "CustomerId" && item.kind == CompletionKind::Column }),
            "HAVING should offer columns from query tables, got {:?}",
            labels(&items)
        );
    }

    // ---- Alias-qualified columns in expression contexts ---------------

    #[test]
    fn offers_alias_qualified_columns_in_where() {
        let sql = "SELECT * FROM Sales.Customer c WHERE c.Cust";
        let items = complete(sql, sql.len(), &cookbook_catalog());
        assert!(
            items.iter().any(|item| {
                item.label == "c.CustomerId" && item.kind == CompletionKind::Column
            }),
            "WHERE alias. should resolve columns, got {:?}",
            labels(&items)
        );
    }

    // ---- Strict scoping to query table refs ---------------------------

    #[test]
    fn where_clause_does_not_offer_unrelated_catalog_columns() {
        let sql = "SELECT * FROM Sales.Customer WHERE Cust";
        let items = complete(sql, sql.len(), &cookbook_catalog());
        assert!(
            items
                .iter()
                .any(|item| { item.label == "CustomerId" && item.kind == CompletionKind::Column }),
            "should offer CustomerId from in-scope Customer table"
        );
        assert!(
            !items.iter().any(
                |item| item.label == "OrderId" && item.source == CompletionSource::LiveMetadata
            ),
            "should NOT offer OrderId from out-of-scope SalesOrder (strict scoping)"
        );
        assert!(
            !items.iter().any(|item| {
                item.label == "DepartmentId" && item.source == CompletionSource::LiveMetadata
            }),
            "should NOT offer DepartmentId from out-of-scope Department (strict scoping)"
        );
    }

    #[test]
    fn where_clause_offers_query_scoped_columns_not_catalog_wide() {
        let sql = "SELECT * FROM HR.Department WHERE Dep";
        let items = complete(sql, sql.len(), &cookbook_catalog());
        assert!(
            items.iter().any(|item| {
                item.label == "DepartmentId" && item.kind == CompletionKind::Column
            }),
            "should offer DepartmentId from in-scope Department"
        );
        assert!(
            !items.iter().any(
                |item| item.label == "OrderId" && item.source == CompletionSource::LiveMetadata
            ),
            "should NOT offer OrderId from out-of-scope SalesOrder"
        );
    }

    #[test]
    fn strict_scoping_falls_back_when_no_refs_match() {
        // "FakeTable" is not in the catalog — strict scoping should fall back
        // to catalog-wide columns so the user still gets suggestions.
        let sql = "SELECT * FROM FakeTable WHERE Cust";
        let items = complete(sql, sql.len(), &cookbook_catalog());
        assert!(
            items.iter().any(|item| item.kind == CompletionKind::Column),
            "should fall back to catalog-wide columns when query refs don't match"
        );
    }

    #[test]
    fn subquery_inner_sees_inner_tables() {
        let sql = "SELECT * FROM (SELECT a FROM HR.Department) sub WHERE sub.a = 1 AND Dep";
        let items = complete(sql, sql.len(), &cookbook_catalog());
        // Inner table Department is at depth 1, cursor is at depth 0.
        // "sub" is not in the catalog, so no matching refs → fallback to catalog-wide.
        // The key assertion is that we don't crash and still offer something.
        assert!(!items.is_empty());
    }

    #[test]
    fn subquery_depth_filter_in_where() {
        // Cursor inside a subquery's WHERE should only see the subquery's tables.
        let sql = "SELECT * FROM HR.Department WHERE DepartmentId IN \
                   (SELECT DepartmentId FROM HR.Employee WHERE Dep";
        let items = complete(sql, sql.len(), &cookbook_catalog());
        // Cursor is at depth 1 (inside the subquery). Should see Employee's columns.
        assert!(
            items.iter().any(|item| {
                item.label == "DepartmentId" && item.kind == CompletionKind::Column
            }),
            "subquery WHERE should offer columns from in-scope Employee, got {:?}",
            labels(&items)
        );
    }

    #[test]
    fn fill_in_middle_offers_columns_from_after_cursor() {
        // Cursor is at "cu" (before FROM), but table is defined after cursor.
        let sql = "SELECT cu FROM Sales.Customer";
        let items = complete(sql, 9, &cookbook_catalog());
        assert!(
            items
                .iter()
                .any(|item| { item.label == "CustomerId" && item.kind == CompletionKind::Column }),
            "fill-in-the-middle: should offer CustomerId from table after cursor, got {:?}",
            labels(&items)
        );
    }

    #[test]
    fn fill_in_middle_offers_columns_from_aliased_table() {
        // Cursor at "c." prefix, table+alias defined after cursor.
        let sql = "SELECT c. FROM Sales.Customer c";
        let items = complete(sql, 8, &cookbook_catalog());
        assert!(
            items.iter().any(|item| {
                item.label == "c.CustomerId" && item.kind == CompletionKind::Column
            }),
            "fill-in-the-middle: should offer c.CustomerId from aliased table after cursor, got {:?}",
            labels(&items)
        );
    }

    #[test]
    fn nested_subquery_column_offered_at_correct_depth() {
        // Double-nested subquery: cursor at depth 2.
        let sql = "SELECT * FROM HR.Department \
                   WHERE DepartmentId IN \
                   (SELECT DepartmentId FROM HR.Employee \
                    WHERE EmployeeId IN (SELECT EmployeeId FROM HR.Employee WHERE Emp";
        let items = complete(sql, sql.len(), &cookbook_catalog());
        assert!(
            items
                .iter()
                .any(|item| { item.label == "EmployeeId" && item.kind == CompletionKind::Column }),
            "doubly-nested subquery should offer columns from innermost table, got {:?}",
            labels(&items)
        );
    }

    // ---- Bug repro: alias-dot in SELECT with schema-qualified table ----

    #[test]
    fn alias_dot_in_select_with_schema_qualified_table() {
        let sql = "SELECT\n\ttop 1000\n\tjl.\nFROM\n\tAccounting.JournalLine jl\nINNER JOIN Accounting.Journal j\n\tON j.JournalId = jl.JournalId";
        // Cursor right after "jl." on line 3
        let cursor = "SELECT\n\ttop 1000\n\tjl.".len();
        let items = complete(sql, cursor, &accounting_catalog());
        assert!(
            items.iter().any(|item| {
                item.label == "jl.JournalLineId" && item.kind == CompletionKind::Column
            }),
            "jl. should offer JournalLine columns, got {:?}",
            labels(&items)
        );
    }

    #[test]
    fn alias_dot_in_select_with_empty_columns() {
        // Simulate: table exists in catalog but columns not yet loaded (lazy).
        let mut catalog = accounting_catalog();
        for obj in &mut catalog.objects {
            obj.columns = Vec::new();
        }
        let sql = "SELECT\n\ttop 1000\n\tjl.\nFROM\n\tAccounting.JournalLine jl\nINNER JOIN Accounting.Journal j\n\tON j.JournalId = jl.JournalId";
        let cursor = "SELECT\n\ttop 1000\n\tjl.".len();
        let items = complete(sql, cursor, &catalog);
        // With no columns loaded, the engine produces nothing for alias-dot.
        // This is the root cause of the user's bug.
        assert!(
            !items.iter().any(|item| item.kind == CompletionKind::Column),
            "with empty columns, no column completions should be offered"
        );
    }
}
