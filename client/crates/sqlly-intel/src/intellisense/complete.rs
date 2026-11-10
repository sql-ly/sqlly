//! Completion orchestration. `complete` decides which resolution path a
//! request takes — alias slot, dotted object path, or the general
//! keyword/object/column mix — and delegates to the focused submodules:
//!
//! - [`scan`]: lightweight SQL scanner (literals/comments skipped, paren
//!   depth tracked)
//! - [`context`]: clause-context detection + FROM/JOIN table-ref extraction
//! - [`dotted`]: `schema.table.column` dotted-path resolution
//! - [`query_local`]: aliases, in-query parameters, query-scoped columns
//! - [`filesystem`]: items from the filesystem object model
//! - [`text`]: cursor prefix / previous-word helpers

mod context;
mod dotted;
mod filesystem;
mod query_local;
pub(crate) mod scan;
mod text;

#[cfg(test)]
mod testutil;

use sqlly_core::ObjectKind;
use std::fmt::Write;

use sqlly_lang::sqlmodel::Dialect;

use crate::intellisense::builtins::builtins;
use crate::intellisense::catalog::SymbolSnapshot;
use crate::intellisense::score::{
    finalize_completion_items, push_match, push_scored, qualified_column_score,
    qualified_object_score, qualified_object_score_qualified, reset_push_cap_state,
};
use crate::intellisense::types::{
    CompletionCatalog, CompletionItem, CompletionKind, CompletionSource,
};

pub use context::{at_drop_table_name, join_awaiting_on, join_condition_without_on};
pub(crate) use context::{detect_clause_context, looks_like_keyword, query_table_refs};
use dotted::{DottedPrefix, add_dotted_items};
use filesystem::add_filesystem_items;
pub use query_local::query_aliases;
use query_local::{
    AliasContext, add_columns_from_query_tables, add_cte_source_items, add_join_key_items,
    add_query_local_items, add_script_table_drop_items, alias_context, has_matching_query_refs,
};
pub use text::current_prefix;
use text::previous_word;

pub fn complete(sql: &str, cursor: usize, catalog: &CompletionCatalog) -> Vec<CompletionItem> {
    reset_push_cap_state();
    // Preserve original case: the fuzzy ranker uses interior capitals
    // (camelCase chunks) to detect explicit camel intent (`SaOr`, `SSO`).
    // Lowercasing here would erase that signal.
    let prefix = current_prefix(sql, cursor).to_string();
    if let Some(items) = at_at_items(Dialect::SqlServer, sql, cursor) {
        return items;
    }
    // After a session-settings head (`SET ▏`) only the settable names apply;
    // callers that know the connection's dialect swap in its own list (see
    // feedback ifb_0fbc).
    if let Some(items) = crate::intellisense::set_statement_items(Dialect::SqlServer, sql, cursor) {
        return items;
    }
    let previous = previous_word(sql, cursor).to_ascii_uppercase();
    let ctx = clause_context_through_calls(sql, cursor);
    let query_refs = query_table_refs(sql, cursor);
    let strict_scope = ctx.is_column_expecting() && has_matching_query_refs(&query_refs, catalog);
    let mut items = Vec::new();

    // Right after a finished SELECT list with no FROM yet (`SELECT * f▏`)
    // only FROM / INTO can follow: offer those, then the tables and views with
    // inserts that write the FROM clause, and nothing else — above all no
    // columns. See feedback ifb_9f91.
    if !prefix.contains('.')
        && let Some(keywords) = crate::intellisense::after_select_list_keywords(sql, cursor)
    {
        add_after_select_list_items(&mut items, sql, cursor, catalog, &prefix, &keywords);
        finalize_completion_items(&mut items);
        return items;
    }

    // Just after a complete WHERE / HAVING / ON test: only AND / OR and the
    // following clauses once a separator is typed, and nothing while the caret
    // still touches the test. See feedback ifb_6f67.
    if !prefix.contains('.')
        && let Some(keywords) =
            crate::modelengine::intellisense::after_predicate_keywords(sql, cursor)
    {
        for (rank, keyword) in keywords.iter().enumerate() {
            push_match(
                &mut items,
                keyword,
                keyword,
                CompletionKind::Keyword,
                CompletionSource::Keyword,
                100 - rank as i32,
                &prefix,
            );
        }
        finalize_completion_items(&mut items);
        return items;
    }

    // Right after a FROM source (`FROM t⏎wh▏`): WHERE leads the clause
    // keywords, then the in-scope columns with inserts that write the WHERE
    // clause — no tables, views or routines. A word on the table's own line
    // with no alias yet is left to the alias slot below. See feedback ifb_92d9.
    if !prefix.contains('.')
        && let Some(clause) = crate::modelengine::intellisense::post_from_clause(sql, cursor)
        && clause.offer_columns
    {
        add_post_from_items(&mut items, sql, cursor, catalog, &prefix, clause);
        finalize_completion_items(&mut items);
        return items;
    }

    // A table reference can be followed by an alias (`FROM Schema.Table a`).
    // While the alias name is being typed there is nothing to suggest, and once
    // it is complete the only sensible continuations are the clauses that may
    // follow a table reference (the JOIN family, then WHERE / GROUP BY /
    // ORDER BY). Handle that state before any other resolution so the alias
    // token is not mistaken for an object or column prefix.
    match alias_context(sql, cursor) {
        AliasContext::Defining => return items,
        AliasContext::After => {
            for (keyword, score) in POST_TABLE_KEYWORDS {
                push_match(
                    &mut items,
                    keyword,
                    keyword,
                    CompletionKind::Keyword,
                    CompletionSource::Keyword,
                    *score,
                    &prefix,
                );
            }
            items.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.label.cmp(&b.label)));
            return items;
        }
        AliasContext::None => {}
    }

    // Dotted, multi-part identifiers (`schema.`, `schema.table.`,
    // `db.schema.table.`, or the lateral `table.column` short form) have a
    // precise meaning: each dot narrows the search to a known parent. We
    // resolve them on a dedicated path and skip the generic keyword/snippet
    // noise that would otherwise drown the relevant suggestions. Parameter
    // member access (`@x.`) is not an object path, so it is excluded here.
    if let Some(dotted) = DottedPrefix::parse(&prefix) {
        add_query_local_items(&mut items, sql, cursor, catalog, &prefix);
        add_dotted_items(&mut items, catalog, &dotted);
        finalize_completion_items(&mut items);
        return items;
    }

    add_query_local_items(&mut items, sql, cursor, catalog, &prefix);
    // A statement's own CTEs are FROM / JOIN sources (feedback ifb_b360),
    // and after `DROP TABLE` the tables the script creates are what gets
    // dropped (feedback ifb_416b).
    if ctx.is_table_expecting() {
        add_cte_source_items(&mut items, sql, cursor, &prefix);
    }
    let drop_table = at_drop_table_name(sql, cursor);
    if drop_table {
        add_script_table_drop_items(&mut items, sql, cursor, &prefix);
    }
    if ctx == context::ClauseContext::JoinOn {
        add_join_key_items(&mut items, sql, cursor, &query_refs, catalog, &prefix);
    }
    if ctx.is_column_expecting() {
        add_columns_from_query_tables(&mut items, &query_refs, catalog, &prefix);
        for keyword in EXPR_KEYWORDS {
            push_match(
                &mut items,
                keyword,
                keyword,
                CompletionKind::Keyword,
                CompletionSource::Keyword,
                12,
                &prefix,
            );
        }
    }

    for keyword in KEYWORDS {
        push_match(
            &mut items,
            keyword,
            keyword,
            CompletionKind::Keyword,
            CompletionSource::Keyword,
            10,
            &prefix,
        );
    }
    // A statement can also open with a block, a transaction, a declaration,
    // a CTE or DDL; offered only where a statement starts, so clauses stay
    // uncluttered (DbGate #1223).
    if at_statement_head(sql, cursor, &prefix) {
        for keyword in STATEMENT_HEAD_KEYWORDS {
            push_match(
                &mut items,
                keyword,
                keyword,
                CompletionKind::Keyword,
                CompletionSource::Keyword,
                10,
                &prefix,
            );
        }
    }
    // `INSERT ▏` continues with INTO (a fresh statement, see feedback
    // ifb_3798); the general keyword list does not carry it.
    if previous == "INSERT" {
        push_match(
            &mut items,
            "INTO",
            "INTO",
            CompletionKind::Keyword,
            CompletionSource::Keyword,
            100,
            &prefix,
        );
    }
    add_builtin_function_items(&mut items, Dialect::SqlServer, &prefix);
    for database in &catalog.databases {
        push_match(
            &mut items,
            database,
            database,
            CompletionKind::Database,
            CompletionSource::LiveMetadata,
            25,
            &prefix,
        );
    }
    for schema in &catalog.schemas {
        push_match(
            &mut items,
            schema,
            schema,
            CompletionKind::Schema,
            CompletionSource::LiveMetadata,
            30,
            &prefix,
        );
    }
    let prefix_has_dot = prefix.contains('.');
    let mut object_label_buf = String::new();
    let mut qualified_column_buf = String::new();
    for object in &catalog.objects {
        let kind = match object.kind {
            ObjectKind::Table | ObjectKind::View => CompletionKind::Table,
            ObjectKind::StoredProcedure => CompletionKind::Procedure,
            ObjectKind::ScalarFunction
            | ObjectKind::TableFunction
            | ObjectKind::AggregateFunction => CompletionKind::Function,
            ObjectKind::Type => CompletionKind::Type,
            _ => continue,
        };
        let base = if previous == "FROM"
            || previous.ends_with("JOIN")
            || ctx.is_table_expecting()
            || drop_table
            || matches!(
                previous.as_str(),
                "UPDATE" | "INTO" | "DELETE" | "MERGE" | "USING"
            ) {
            80
        } else {
            40
        };
        if let Some(fs) = qualified_object_score_qualified(
            &prefix,
            object.schema.as_deref(),
            &object.name,
            &mut object_label_buf,
        ) {
            let label = object
                .schema
                .as_ref()
                .map(|_| object_label_buf.as_str())
                .unwrap_or(object.name.as_str());
            push_scored(
                &mut items,
                label,
                label,
                kind,
                CompletionSource::LiveMetadata,
                base + fs,
            );
        }
        if (previous == "SELECT" || prefix_has_dot || ctx.is_column_expecting()) && !strict_scope {
            let dotted_parts =
                prefix_has_dot.then(|| prefix.rsplit_once('.').unwrap_or((&prefix, "")));
            for column in &object.columns {
                if let Some((parent_prefix, tail)) = dotted_parts {
                    if !tail.is_empty() {
                        let object_qualified = match object.schema.as_deref() {
                            Some(schema) => {
                                object_label_buf.clear();
                                let _ = write!(object_label_buf, "{schema}.{}", object.name);
                                object_label_buf.as_str()
                            }
                            None => object.name.as_str(),
                        };
                        let tail_might_match = qualified_object_score(tail, column).is_some()
                            || qualified_object_score(parent_prefix, object_qualified).is_some();
                        if !tail_might_match {
                            continue;
                        }
                    } else if qualified_object_score_qualified(
                        &prefix,
                        object.schema.as_deref(),
                        &object.name,
                        &mut object_label_buf,
                    )
                    .is_none()
                    {
                        continue;
                    }
                }
                let fs = if prefix_has_dot {
                    qualified_column_score(
                        &prefix,
                        object.schema.as_deref(),
                        &object.name,
                        column,
                        &mut qualified_column_buf,
                    )
                } else {
                    qualified_object_score(&prefix, column)
                };
                if let Some(fs) = fs {
                    let label = if prefix_has_dot {
                        qualified_column_buf.as_str()
                    } else {
                        column
                    };
                    push_scored(
                        &mut items,
                        label,
                        column,
                        CompletionKind::Column,
                        CompletionSource::LiveMetadata,
                        70 + fs,
                    );
                }
            }
        }
        if prefix.starts_with('@') {
            for parameter in &object.parameters {
                push_match(
                    &mut items,
                    parameter,
                    parameter,
                    CompletionKind::Parameter,
                    CompletionSource::LiveMetadata,
                    72,
                    &prefix,
                );
            }
        }
    }
    add_filesystem_items(&mut items, catalog, &prefix, &previous, ctx, strict_scope);
    add_required_group_by_items(&mut items, sql, cursor, &prefix);
    for (label, body) in &catalog.snippets {
        push_match(
            &mut items,
            label,
            body,
            CompletionKind::Snippet,
            CompletionSource::Snippet,
            20,
            &prefix,
        );
    }

    finalize_completion_items(&mut items);
    items
}

/// Score for the clause keywords that lead a clause position (after a finished
/// SELECT list, after a FROM source) — far above any object or column score,
/// so the keyword stays first through every later rank boost.
const AFTER_SELECT_LIST_KEYWORD_SCORE: i32 = 10_000;

fn add_after_select_list_items(
    items: &mut Vec<CompletionItem>,
    sql: &str,
    cursor: usize,
    catalog: &CompletionCatalog,
    prefix: &str,
    keywords: &[&'static str],
) {
    for (rank, keyword) in keywords.iter().enumerate() {
        push_match(
            items,
            keyword,
            keyword,
            CompletionKind::Keyword,
            CompletionSource::Keyword,
            AFTER_SELECT_LIST_KEYWORD_SCORE - rank as i32,
            prefix,
        );
    }
    let replace_start = cursor.saturating_sub(prefix.len());
    let unit = crate::intellisense::infer_indent_unit(sql);
    let mut label_buf = String::new();
    for object in &catalog.objects {
        if !matches!(object.kind, ObjectKind::Table | ObjectKind::View) {
            continue;
        }
        // The same matching as a FROM position, so the typed text filters
        // tables and schemas exactly as it would after `FROM `.
        let Some(fs) = qualified_object_score_qualified(
            prefix,
            object.schema.as_deref(),
            &object.name,
            &mut label_buf,
        ) else {
            continue;
        };
        let label = if object.schema.is_some() {
            label_buf.clone()
        } else {
            object.name.clone()
        };
        let insert_text =
            crate::intellisense::clause_insert_text(sql, replace_start, "FROM", &label, &unit);
        items.push(CompletionItem {
            label,
            insert_text,
            kind: CompletionKind::Table,
            source: CompletionSource::LiveMetadata,
            score: 80 + fs,
            detail: crate::intellisense::FROM_CLAUSE_DETAIL.to_string(),
            replace_span: None,
        });
    }
}

fn add_post_from_items(
    items: &mut Vec<CompletionItem>,
    sql: &str,
    cursor: usize,
    catalog: &CompletionCatalog,
    prefix: &str,
    clause: crate::modelengine::intellisense::PostFromClause,
) {
    for (keyword, score) in POST_TABLE_KEYWORDS {
        if *keyword == "ORDER BY" && !clause.order_by_allowed {
            continue;
        }
        let boost = match *keyword {
            "WHERE" => 2000,
            "GROUP BY" if clause.has_aggregate => 1000,
            _ => 0,
        };
        push_match(
            items,
            keyword,
            keyword,
            CompletionKind::Keyword,
            CompletionSource::Keyword,
            AFTER_SELECT_LIST_KEYWORD_SCORE + score + boost,
            prefix,
        );
    }
    // The typed word is blanked before reading the query's sources, or it
    // would parse as the table's alias.
    let replace_start = cursor.saturating_sub(prefix.len());
    let blanked = format!(
        "{}{}{}",
        &sql[..replace_start],
        " ".repeat(prefix.len()),
        &sql[cursor..]
    );
    let unit = crate::intellisense::infer_indent_unit(sql);
    let mut seen = std::collections::HashSet::new();
    for table_ref in query_table_refs(&blanked, cursor) {
        let Some(object) = query_local::find_object(catalog, &table_ref.name) else {
            continue;
        };
        for column in &object.columns {
            // An aliased source's columns carry the alias, as they must once
            // the WHERE clause names them.
            let label = match &table_ref.alias {
                Some(alias) => format!("{alias}.{column}"),
                None => column.clone(),
            };
            if !seen.insert(label.to_ascii_lowercase()) {
                continue;
            }
            let Some(fs) = qualified_object_score(prefix, column)
                .or_else(|| qualified_object_score(prefix, &label))
            else {
                continue;
            };
            let insert_text =
                crate::intellisense::clause_insert_text(sql, replace_start, "WHERE", &label, &unit);
            items.push(CompletionItem {
                label,
                insert_text,
                kind: CompletionKind::Column,
                source: CompletionSource::Inferred,
                score: 85 + fs,
                detail: crate::intellisense::WHERE_CLAUSE_DETAIL.to_string(),
                replace_span: None,
            });
        }
    }
}

/// Inside a GROUP BY list, the select-list columns it still needs lead the
/// list, marked required (feedback ifb_75bd). An existing item with the same
/// label is replaced so the column appears once.
fn add_required_group_by_items(
    items: &mut Vec<CompletionItem>,
    sql: &str,
    cursor: usize,
    prefix: &str,
) {
    for label in crate::intellisense::required_group_by_labels(sql, cursor, prefix) {
        items.retain(|item| !item.label.eq_ignore_ascii_case(&label));
        items.push(CompletionItem {
            insert_text: label.clone(),
            label,
            kind: CompletionKind::Column,
            source: CompletionSource::Inferred,
            score: crate::intellisense::clause_insert::REQUIRED_GROUP_BY_SCORE,
            detail: crate::intellisense::REQUIRED_GROUP_BY_DETAIL.to_string(),
            replace_span: None,
        });
    }
}

pub fn complete_from_snapshot(
    sql: &str,
    cursor: usize,
    snapshot: &SymbolSnapshot,
) -> Vec<CompletionItem> {
    complete(sql, cursor, &snapshot.catalog())
}

pub fn default_snippets() -> Vec<(String, String)> {
    vec![
        (
            "select-top".to_string(),
            "SELECT TOP 100 * FROM ".to_string(),
        ),
        (
            "cte".to_string(),
            "WITH cte AS (\n    SELECT \n)\nSELECT * FROM cte;".to_string(),
        ),
    ]
}

/// Clauses that may legally follow a completed `FROM`/`JOIN <object> <alias>`
/// table reference, paired with a base score. The JOIN family ranks ahead of
/// the filtering/grouping clauses so the most common continuation surfaces
/// first.
const POST_TABLE_KEYWORDS: &[(&str, i32)] = &[
    ("INNER JOIN", 60),
    ("LEFT JOIN", 59),
    ("RIGHT JOIN", 58),
    ("OUTER JOIN", 57),
    ("JOIN", 56),
    ("WHERE", 50),
    ("GROUP BY", 49),
    ("ORDER BY", 48),
];

const KEYWORDS: &[&str] = &[
    "SELECT",
    "FROM",
    "WHERE",
    "JOIN",
    "GROUP BY",
    "ORDER BY",
    "INSERT",
    "UPDATE",
    "DELETE",
    "CREATE TABLE",
    "CREATE VIEW",
    "EXEC",
];

/// Statement heads beyond [`KEYWORDS`], offered only at a statement start.
const STATEMENT_HEAD_KEYWORDS: &[&str] = &[
    "BEGIN",
    "DECLARE",
    "WITH",
    "MERGE",
    "ALTER",
    "DROP",
    "TRUNCATE TABLE",
    "COMMIT",
    "ROLLBACK",
    "IF",
    "WHILE",
];

/// Whether the caret starts a statement: nothing but the word being typed
/// precedes it in its statement, it follows a block opener, or it opens a
/// line after one that reads as finished — T-SQL needs no `;` between
/// statements.
fn at_statement_head(sql: &str, cursor: usize, prefix: &str) -> bool {
    if prefix.contains('.') || prefix.starts_with('@') {
        return false;
    }
    let words = context::depth_words(sql, cursor, true);
    let before = if prefix.is_empty() {
        &words[..]
    } else {
        &words[..words.len().saturating_sub(1)]
    };
    let Some(last) = before.last() else {
        return true;
    };
    if ["BEGIN", "ELSE", "TRY", "CATCH"]
        .iter()
        .any(|w| last.eq_ignore_ascii_case(w))
    {
        return true;
    }
    let cursor = cursor.min(sql.len());
    let word_start = cursor - prefix.len().min(cursor);
    let line_start = sql[..word_start].rfind('\n').map_or(0, |i| i + 1);
    if !sql[line_start..word_start].trim().is_empty() || line_start == 0 {
        return false;
    }
    let previous_line = sql[..line_start].trim_end();
    let ends_open = previous_line.ends_with([',', '(', '=', '<', '>', '+', '-', '*', '/', '%']);
    let last_word = previous_line
        .rsplit(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .next()
        .unwrap_or("");
    let continues = [
        "SELECT", "FROM", "WHERE", "AND", "OR", "NOT", "ON", "JOIN", "BY", "SET", "INTO", "VALUES",
        "AS", "IN", "IS", "LIKE", "BETWEEN", "THEN", "WHEN", "CASE", "HAVING", "UNION", "ALL",
        "DISTINCT", "TOP", "IF", "WHILE", "EXEC", "EXECUTE", "DECLARE", "UPDATE", "DELETE",
        "INSERT", "TABLE", "WITH",
    ]
    .iter()
    .any(|w| last_word.eq_ignore_ascii_case(w));
    !previous_line.is_empty() && !ends_open && !continues
}

/// Expression-level keywords offered inside column-expecting clauses
/// (WHERE, GROUP BY, HAVING, ORDER BY, JOIN ON, SELECT list).
const EXPR_KEYWORDS: &[&str] = &[
    "AND", "OR", "NOT", "IN", "LIKE", "BETWEEN", "IS", "NULL", "AS", "DESC", "ASC",
];

/// The clause the cursor is in, looking through function-call parentheses:
/// an argument of `COUNT(` in a SELECT list sits in the SELECT list, so its
/// columns are in scope. Clause detection alone only sees words at the
/// cursor's own paren depth, and inside a call there are none.
pub(crate) fn clause_context_through_calls(sql: &str, cursor: usize) -> context::ClauseContext {
    let mut at = cursor;
    // Nested calls (`ROUND(SUM(|`) peel one paren per step.
    for _ in 0..16 {
        let ctx = detect_clause_context(sql, at);
        if ctx != context::ClauseContext::Unknown {
            return ctx;
        }
        match crate::intellisense::signature::call_context_at(sql, at) {
            Some(call) if call.open_paren < at => at = call.open_paren,
            _ => return ctx,
        }
    }
    detect_clause_context(sql, at)
}

/// Once `@@` is typed, the completions are the dialect's own `@@` names —
/// SQL Server's system functions (`@@ROWCOUNT`), MySQL/MariaDB's system
/// variables (`@@sql_mode`) — matched on what follows the `@@` by the usual
/// fuzzy rules. `None` when the word at the cursor does not start with `@@`
/// (a single `@` is a user variable or parameter, never a system name); an
/// empty list when the dialect has no `@@` names at all.
pub fn at_at_items(dialect: Dialect, sql: &str, cursor: usize) -> Option<Vec<CompletionItem>> {
    let prefix = current_prefix(sql, cursor);
    let typed = prefix.strip_prefix("@@")?;
    let catalog = builtins(dialect);
    let mut items = Vec::new();
    for (name, doc) in catalog.at_at_names() {
        let Some(fs) = qualified_object_score(typed, &name[2..]) else {
            continue;
        };
        let is_function = catalog.function(name).is_some();
        let detail = doc.split(". ").next().unwrap_or(doc).trim_end_matches('.');
        items.push(CompletionItem {
            label: name.to_string(),
            insert_text: name.to_string(),
            kind: if is_function {
                CompletionKind::Function
            } else {
                CompletionKind::Parameter
            },
            source: CompletionSource::Builtin,
            score: 100 + fs,
            detail: detail.to_string(),
            replace_span: None,
        });
    }
    items.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.label.cmp(&b.label)));
    Some(items)
}

/// The built-in functions of `dialect` that match `prefix`, as completions
/// whose detail is the function's first signature (`DATEADD(datepart,
/// number, date)`). [`complete`] offers SQL Server's; callers that know the
/// connection's dialect swap in its own with
/// [`replace_builtin_function_items`].
pub fn add_builtin_function_items(items: &mut Vec<CompletionItem>, dialect: Dialect, prefix: &str) {
    // `@@` names are offered only once `@@` is typed (`at_at_items`).
    for function in builtins(dialect)
        .functions()
        .iter()
        .filter(|f| !f.name.starts_with("@@"))
    {
        let before = items.len();
        push_match(
            items,
            &function.name,
            &function.name,
            CompletionKind::Function,
            CompletionSource::Builtin,
            15,
            prefix,
        );
        if items.len() > before
            && let Some(item) = items.last_mut()
            && item.label == function.name
        {
            item.detail = function.label(0).0;
        }
    }
}

/// Swap the built-in function completions [`complete`] produced (SQL
/// Server's) for `dialect`'s.
pub fn replace_builtin_function_items(
    items: &mut Vec<CompletionItem>,
    dialect: Dialect,
    prefix: &str,
) {
    if dialect == Dialect::SqlServer {
        return;
    }
    items.retain(|item| {
        !(item.kind == CompletionKind::Function && item.source == CompletionSource::Builtin)
    });
    add_builtin_function_items(items, dialect, prefix);
}

/// Score for a word the argument slot under the cursor accepts: above
/// columns and every other expression suggestion, since the slot takes
/// nothing else.
pub const SLOT_VALUE_SCORE: i32 = 400;

/// When the cursor is in an argument that takes fixed words (`DATEADD(|` →
/// `year`, `month`, `day`, …), those words as completions, matching the
/// prefix typed so far. Empty everywhere else.
pub fn slot_value_items(dialect: Dialect, sql: &str, cursor: usize) -> Vec<CompletionItem> {
    let Some(ctx) = crate::intellisense::signature::call_context_at(sql, cursor) else {
        return Vec::new();
    };
    if ctx.callee.contains('.') {
        return Vec::new();
    }
    let catalog = builtins(dialect);
    let Some(function) = catalog.function(&ctx.callee) else {
        return Vec::new();
    };
    let prefix = current_prefix(sql, cursor);
    let mut items = Vec::new();
    for value in catalog.slot_values(&ctx.callee, ctx.active_parameter) {
        if let Some(fs) = qualified_object_score(prefix, value) {
            let param = catalog
                .slot_param(&ctx.callee, ctx.active_parameter, value)
                .map(|p| p.name.as_str())
                .unwrap_or("argument");
            items.push(CompletionItem {
                label: value.to_string(),
                insert_text: value.to_string(),
                kind: CompletionKind::Keyword,
                source: CompletionSource::Builtin,
                score: SLOT_VALUE_SCORE + fs,
                detail: format!("{param} for {}", function.name),
                replace_span: None,
            });
        }
    }
    items
}

#[cfg(test)]
mod tests {
    use crate::intellisense::catalog::{LiveMetadataSnapshot, SymbolSnapshot};
    #[cfg(not(target_arch = "wasm32"))]
    use std::time::SystemTime;
    #[cfg(target_arch = "wasm32")]
    use web_time::SystemTime;

    use super::testutil::{catalog, cookbook_catalog, labels};
    use super::*;
    use crate::intellisense::types::CompletionObject;
    #[cfg(not(target_arch = "wasm32"))]
    use std::time::Instant;
    #[cfg(target_arch = "wasm32")]
    use web_time::Instant;

    #[test]
    fn ranks_tables_after_from() {
        let items = complete("SELECT * FROM ord", 17, &catalog());
        assert_eq!(items[0].label, "sales.Orders");
    }

    #[test]
    fn offers_columns_after_select() {
        let items = complete("SELECT ord", 10, &catalog());
        assert!(items.iter().any(|item| item.kind == CompletionKind::Column));
    }

    #[test]
    fn offers_qualified_columns_after_dot_prefix() {
        let items = complete("SELECT sales.Orders.ord", 23, &catalog());
        assert!(
            items
                .iter()
                .any(|item| item.label == "sales.Orders.OrderId")
        );
    }

    #[test]
    fn offers_snippets() {
        let items = complete("sel", 3, &catalog());
        assert!(
            items
                .iter()
                .any(|item| item.kind == CompletionKind::Snippet)
        );
    }

    #[test]
    fn slot_values_lead_inside_a_date_part_argument() {
        let items = slot_value_items(Dialect::SqlServer, "SELECT DATEADD(", 15);
        assert!(items.iter().any(|i| i.label.eq_ignore_ascii_case("month")));
        assert!(items.iter().all(|i| i.score >= SLOT_VALUE_SCORE));
        let items = slot_value_items(Dialect::SqlServer, "SELECT DATEADD(mon", 18);
        assert!(items.iter().any(|i| i.label.eq_ignore_ascii_case("month")));
        assert!(!items.iter().any(|i| i.label.eq_ignore_ascii_case("year")));
        // The second argument takes an expression, not a fixed word.
        assert!(slot_value_items(Dialect::SqlServer, "SELECT DATEADD(day, ", 20).is_empty());
    }

    #[test]
    fn a_function_argument_offers_the_query_columns_first() {
        // The user's report: `COUNT(` in a SELECT list offered only `@@`
        // system functions.
        let sql = "SELECT\n    o.OrderId,\n    COUNT(\nFROM\n    sales.Orders o\n";
        let cursor = sql.find("COUNT(").unwrap() + 6;
        let items = complete(sql, cursor, &catalog());
        assert_eq!(
            items[0].kind,
            CompletionKind::Column,
            "{:?}",
            labels(&items)
        );
        assert!(items.iter().any(|i| i.label == "o.CustomerId"));
        // Nested calls look through every paren.
        let sql = "SELECT ROUND(SUM( FROM sales.Orders o";
        let cursor = sql.find("SUM(").unwrap() + 4;
        assert_eq!(
            complete(sql, cursor, &catalog())[0].kind,
            CompletionKind::Column
        );
        // `@@` functions are not in an ordinary list at all.
        assert!(!items.iter().any(|i| i.label.starts_with("@@")));
    }

    /// Feedback ifb_93e5: `@@` offers the dialect's system names, filtered by
    /// what follows; a single `@` never does.
    #[test]
    fn at_at_offers_the_dialects_system_names() {
        let sql = "declare @Test bigint = 10;\nSELECT\n\t@@\nFROM t";
        let cursor = sql.find("@@").unwrap() + 2;
        let items = complete(sql, cursor, &catalog());
        assert!(items.len() > 10, "{:?}", labels(&items));
        assert!(items.iter().all(|i| i.label.starts_with("@@")));
        assert!(
            items
                .iter()
                .any(|i| i.label == "@@ROWCOUNT" && !i.detail.is_empty())
        );
        // Typing after `@@` filters by the usual rules.
        let sql = "SELECT @@rowc";
        let items = complete(sql, sql.len(), &catalog());
        assert_eq!(items.first().map(|i| i.label.as_str()), Some("@@ROWCOUNT"));
        let sql = "SELECT @@tran";
        let items = complete(sql, sql.len(), &catalog());
        assert_eq!(
            items.first().map(|i| i.label.as_str()),
            Some("@@TRANCOUNT"),
            "{:?}",
            labels(&items)
        );
        // One `@` is a user variable: no system names.
        let sql = "declare @Test bigint = 10;\nSELECT @";
        let items = complete(sql, sql.len(), &catalog());
        assert!(
            !items.iter().any(|i| i.label.starts_with("@@")),
            "{:?}",
            labels(&items)
        );
        // Each dialect offers its own: MySQL variables, nothing for Postgres.
        let sql = "SELECT @@sql";
        let mysql = at_at_items(Dialect::MySql, sql, sql.len()).unwrap();
        assert_eq!(mysql.first().map(|i| i.label.as_str()), Some("@@sql_mode"));
        assert!(!mysql.iter().any(|i| i.label == "@@ROWCOUNT"));
        assert!(
            at_at_items(Dialect::Postgres, sql, sql.len())
                .unwrap()
                .is_empty()
        );
        assert!(at_at_items(Dialect::SqlServer, "SELECT @x", 9).is_none());
    }

    #[test]
    fn builtin_function_completions_follow_the_dialect() {
        let mut items = complete("SELECT date_tr", 14, &catalog());
        replace_builtin_function_items(&mut items, Dialect::Postgres, "date_tr");
        let item = items
            .iter()
            .find(|i| i.label == "date_trunc")
            .expect("postgres date_trunc");
        assert!(item.detail.starts_with("date_trunc("), "{}", item.detail);
        let mut items = complete("SELECT getd", 11, &catalog());
        replace_builtin_function_items(&mut items, Dialect::Postgres, "getd");
        assert!(
            !items.iter().any(|i| i.label == "GETDATE"),
            "GETDATE is SQL Server's"
        );
    }

    #[test]
    fn offers_builtin_functions_by_prefix() {
        let items = complete("SELECT getd", 11, &catalog());
        assert!(
            items.iter().any(|item| item.label == "GETDATE"
                && item.kind == CompletionKind::Function
                && item.source == CompletionSource::Builtin),
            "expected GETDATE builtin for prefix 'getd'"
        );
        let coalesce = complete("SELECT coa", 10, &catalog());
        assert!(
            coalesce
                .iter()
                .any(|item| item.label == "COALESCE" && item.source == CompletionSource::Builtin),
            "expected COALESCE builtin for prefix 'coa'"
        );
        // Builtins are prefix-filtered like everything else.
        let none = complete("SELECT zzz", 10, &catalog());
        assert!(
            !none
                .iter()
                .any(|item| item.source == CompletionSource::Builtin),
            "no builtin should match prefix 'zzz'"
        );
    }

    #[test]
    fn builds_catalog_from_symbol_snapshot() {
        let snapshot = SymbolSnapshot {
            live: LiveMetadataSnapshot {
                captured_at: Some(SystemTime::now()),
                active_database: Some("appdb".to_string()),
                databases: vec!["appdb".to_string()],
                schemas: vec!["sales".to_string()],
                objects: vec![CompletionObject::table(
                    "sales",
                    "Invoices",
                    vec!["InvoiceId".to_string()],
                )],
            },
            filesystem_schemas: Vec::new(),
        };

        let items = complete_from_snapshot("SELECT * FROM inv", 17, &snapshot);
        assert_eq!(items[0].label, "sales.Invoices");
        assert_eq!(items[0].source, CompletionSource::LiveMetadata);
    }

    #[test]
    fn cached_completion_stays_under_latency_budget() {
        let mut catalog = catalog();
        for idx in 0..1_000 {
            catalog.objects.push(CompletionObject::table(
                "dbo",
                format!("Table{idx}"),
                vec!["Id".to_string(), "Name".to_string()],
            ));
        }
        let started = Instant::now();
        let items = complete("SELECT * FROM table99", 21, &catalog);
        assert!(!items.is_empty());
        assert!(started.elapsed() < std::time::Duration::from_millis(50));
    }

    // ---- Fuzzy matching (mirrors the sqlly-modelengine cookbook tests) -----

    fn has_label(items: &[CompletionItem], expected: &str) -> bool {
        items.iter().any(|item| item.label == expected)
    }

    fn missing_label(items: &[CompletionItem], unexpected: &str) -> bool {
        !items.iter().any(|item| item.label == unexpected)
    }

    #[test]
    fn from_partial_dep_matches_hr_department() {
        let items = complete("SELECT * FROM Dep", 17, &cookbook_catalog());
        assert!(has_label(&items, "HR.Department"));
    }

    #[test]
    fn from_partial_dep_does_not_match_unrelated_tables() {
        let items = complete("SELECT * FROM Dep", 17, &cookbook_catalog());
        assert!(has_label(&items, "HR.Department"));
        assert!(missing_label(&items, "Sales.Customer"));
        assert!(missing_label(&items, "Sales.SalesOrder"));
        assert!(missing_label(&items, "HR.Employee"));
        assert!(missing_label(&items, "Archive.ArchivedOrder"));
        assert!(missing_label(&items, "Staging.RawCustomerImport"));
    }

    #[test]
    fn from_partial_dep_is_case_insensitive() {
        let items = complete("SELECT * FROM dep", 17, &cookbook_catalog());
        assert!(has_label(&items, "HR.Department"));
    }

    #[test]
    fn from_acronym_sso_matches_sales_sales_order() {
        let items = complete("SELECT * FROM SSO", 17, &cookbook_catalog());
        assert!(has_label(&items, "Sales.SalesOrder"));
    }

    #[test]
    fn from_acronym_sso_does_not_match_unrelated_objects() {
        let items = complete("SELECT * FROM SSO", 17, &cookbook_catalog());
        assert!(has_label(&items, "Sales.SalesOrder"));
        assert!(missing_label(&items, "Sales.Customer"));
        assert!(missing_label(&items, "HR.Department"));
        assert!(missing_label(&items, "HR.Employee"));
        assert!(missing_label(&items, "Archive.ArchivedOrder"));
        assert!(missing_label(&items, "Staging.RawCustomerImport"));
    }

    #[test]
    fn from_partial_saor_matches_sales_sales_order() {
        let items = complete("SELECT * FROM SaOr", 18, &cookbook_catalog());
        assert!(has_label(&items, "Sales.SalesOrder"));
    }

    #[test]
    fn from_mixed_schema_and_acronym_sasso_matches_sales_sales_order() {
        let items = complete("SELECT * FROM SaSSO", 19, &cookbook_catalog());
        assert!(has_label(&items, "Sales.SalesOrder"));
    }

    #[test]
    fn from_mixed_schema_and_name_ssaor_matches_sales_sales_order() {
        let items = complete("SELECT * FROM SSaOr", 19, &cookbook_catalog());
        assert!(has_label(&items, "Sales.SalesOrder"));
    }

    #[test]
    fn fuzzy_matching_stays_under_latency_budget() {
        let mut catalog = cookbook_catalog();
        for idx in 0..1_000 {
            catalog.objects.push(CompletionObject {
                database: Some("AppDb".to_string()),
                schema: Some("Sales".to_string()),
                name: format!("Table{idx}"),
                kind: ObjectKind::Table,
                columns: vec!["Id".to_string(), "Name".to_string()],
                parameters: Vec::new(),
                foreign_keys: Vec::new(),
            });
        }
        let started = Instant::now();
        let items = complete("SELECT * FROM SSO", 17, &catalog);
        assert!(!items.is_empty());
        assert!(started.elapsed() < std::time::Duration::from_millis(50));
    }

    // ---- Tier 2B: Fallback-path latency ----------------------------------

    #[test]
    fn fallback_heavy_query_stays_under_latency_budget() {
        // `SaSSO` exercises the last_run_word_anchored fallback (the most
        // expensive matching path) against every object in the catalog.
        let mut catalog = cookbook_catalog();
        for idx in 0..1_000 {
            catalog.objects.push(CompletionObject {
                database: Some("AppDb".to_string()),
                schema: Some("Sales".to_string()),
                name: format!("Table{idx}"),
                kind: ObjectKind::Table,
                columns: vec!["Id".to_string(), "Name".to_string()],
                parameters: Vec::new(),
                foreign_keys: Vec::new(),
            });
        }
        let started = Instant::now();
        let items = complete("SELECT * FROM SaSSO", 19, &catalog);
        assert!(!items.is_empty());
        assert!(started.elapsed() < std::time::Duration::from_millis(50));
    }

    // ---- General context tests ------------------------------------------

    #[test]
    fn prefix_filters_out_unused_schema_objects() {
        // `xyz` should not match any objects.
        let items = complete("SELECT * FROM xyz", 17, &cookbook_catalog());
        assert!(
            !items.iter().any(|item| item.kind == CompletionKind::Table),
            "prefix 'xyz' should not match any tables"
        );
    }

    #[test]
    fn general_context_offers_in_scope_columns() {
        // After SELECT, columns from catalog objects should be offered.
        let items = complete("SELECT CustomerId", 17, &cookbook_catalog());
        assert!(
            items.iter().any(|item| item.kind == CompletionKind::Column),
            "SELECT context should offer columns"
        );
    }

    // ---- Column completion in SELECT list with commas -----------------

    #[test]
    fn offers_columns_in_select_list_after_comma() {
        let sql = "SELECT CustomerId, Order";
        let items = complete(sql, sql.len(), &cookbook_catalog());
        assert!(
            items
                .iter()
                .any(|item| { item.kind == CompletionKind::Column }),
            "SELECT list after comma should offer columns, got {:?}",
            labels(&items)
        );
    }

    // ---- Expression keywords in expression contexts -------------------

    #[test]
    fn offers_expr_keywords_in_where_clause() {
        let sql = "SELECT * FROM Sales.SalesOrder WHERE OrderId = 1 AN";
        let items = complete(sql, sql.len(), &cookbook_catalog());
        assert!(
            items
                .iter()
                .any(|item| { item.label == "AND" && item.kind == CompletionKind::Keyword }),
            "WHERE clause should offer AND keyword, got {:?}",
            labels(&items)
        );
    }

    #[test]
    fn offers_expr_keywords_in_order_by() {
        let sql = "SELECT * FROM Sales.SalesOrder ORDER BY OrderId DE";
        let items = complete(sql, sql.len(), &cookbook_catalog());
        assert!(
            items
                .iter()
                .any(|item| { item.label == "DESC" && item.kind == CompletionKind::Keyword }),
            "ORDER BY should offer DESC keyword, got {:?}",
            labels(&items)
        );
    }

    // ---- Latency budget with clause context ---------------------------

    #[test]
    fn clause_aware_completion_stays_under_latency_budget() {
        let mut catalog = cookbook_catalog();
        for idx in 0..1_000 {
            catalog.objects.push(CompletionObject {
                database: Some("AppDb".to_string()),
                schema: Some("Sales".to_string()),
                name: format!("Table{idx}"),
                kind: ObjectKind::Table,
                columns: vec!["Id".to_string(), "Name".to_string()],
                parameters: Vec::new(),
                foreign_keys: Vec::new(),
            });
        }
        let sql = "SELECT * FROM Sales.Customer c JOIN Sales.SalesOrder o ON c.CustomerId = o.CustomerId WHERE Cust";
        let started = Instant::now();
        let items = complete(sql, sql.len(), &catalog);
        assert!(!items.is_empty());
        assert!(started.elapsed() < std::time::Duration::from_millis(50));
    }

    // ---- Scanner integration: literals, comments, brackets ------------

    #[test]
    fn string_literal_parens_ignored() {
        // Parens inside string literals should not affect depth tracking.
        let sql = "SELECT * FROM Sales.Customer WHERE Name = '(test)' AND Cust";
        let items = complete(sql, sql.len(), &cookbook_catalog());
        assert!(
            items
                .iter()
                .any(|item| { item.label == "CustomerId" && item.kind == CompletionKind::Column }),
            "string literal parens should not affect depth, got {:?}",
            labels(&items)
        );
    }

    #[test]
    fn line_comment_does_not_affect_clause_detection() {
        let sql = "SELECT * FROM Sales.Customer -- this is a comment\n WHERE Cust";
        let items = complete(sql, sql.len(), &cookbook_catalog());
        assert!(
            items
                .iter()
                .any(|item| { item.label == "CustomerId" && item.kind == CompletionKind::Column }),
            "line comment should not affect clause detection"
        );
    }

    #[test]
    fn block_comment_does_not_affect_clause_detection() {
        let sql = "SELECT * FROM /* comment */ Sales.Customer WHERE Cust";
        let items = complete(sql, sql.len(), &cookbook_catalog());
        assert!(
            items
                .iter()
                .any(|item| { item.label == "CustomerId" && item.kind == CompletionKind::Column }),
            "block comment should not affect clause detection"
        );
    }

    #[test]
    fn bracket_identifier_in_from_clause() {
        // Bracketed identifiers should be handled by the scanner.
        let sql = "SELECT * FROM [Sales].[Customer] WHERE Cust";
        let items = complete(sql, sql.len(), &cookbook_catalog());
        assert!(
            items
                .iter()
                .any(|item| { item.label == "CustomerId" && item.kind == CompletionKind::Column }),
            "bracketed identifiers in FROM should resolve, got {:?}",
            labels(&items)
        );
    }

    /// Feedback ifb_3798: `update` typed after a finished `SELECT … FROM t
    /// AS x` opens a new statement. It is neither the SELECT's post-FROM
    /// position (no WHERE / JOIN family) nor, without the alias, the alias of
    /// `t`; it offers the tables as UPDATE targets and none of the previous
    /// statement's columns.
    #[test]
    fn a_statement_keyword_after_a_finished_select_starts_a_new_statement() {
        let catalog = catalog();
        for sql in [
            "SELECT *\nFROM sales.Orders AS o\n\nupdate ",
            "SELECT *\nFROM sales.Orders\n\nupdate ",
            "SELECT *\nFROM sales.Orders o WHERE o.OrderId = \n\nupdate ",
            "SELECT * FROM sales.Orders o;\ndelete ",
            "SELECT * FROM sales.Orders o\nGO\nmerge ",
        ] {
            let items = complete(sql, sql.len(), &catalog);
            let got = labels(&items);
            assert_eq!(
                got.first().map(String::as_str),
                Some("sales.Orders"),
                "{sql:?}: {got:?}"
            );
            // The post-FROM clause keywords (the general keyword list still
            // carries bare JOIN / WHERE at its floor score) and the previous
            // statement's columns are gone.
            for bad in ["INNER JOIN", "LEFT JOIN", "o.OrderId", "OrderId"] {
                assert!(
                    !got.iter().any(|l| l == bad),
                    "{sql:?} offered {bad}: {got:?}"
                );
            }
        }

        // A new SELECT does not inherit the previous statement's alias.
        let sql = "SELECT *\nFROM sales.Orders AS o\n\nselect ";
        let got = labels(&complete(sql, sql.len(), &catalog));
        assert!(!got.iter().any(|l| l == "o.OrderId"), "{got:?}");

        // A fresh INSERT leads with INTO.
        let sql = "SELECT 1\n\ninsert ";
        let got = labels(&complete(sql, sql.len(), &catalog));
        assert_eq!(got.first().map(String::as_str), Some("INTO"), "{got:?}");

        // Keyword look-alikes inside the statement do not end it: the alias
        // columns are still in scope after them.
        let sql = "SELECT 'update', [delete], o.OrderId FROM sales.Orders o WHERE o.";
        let got = labels(&complete(sql, sql.len(), &catalog));
        assert!(got.iter().any(|l| l == "o.CustomerId"), "{got:?}");
    }

    /// Feedback ifb_0fbc: `SET ▏` offers the engine's settable options and
    /// nothing else — no built-in or user functions, no tables — while
    /// `UPDATE t SET ▏` keeps offering the table's columns.
    #[test]
    fn a_settings_statement_offers_only_settable_names() {
        let catalog = cookbook_catalog();
        let sql = "SET ";
        let items = complete(sql, sql.len(), &catalog);
        let got = labels(&items);
        assert!(got.iter().any(|l| l == "NOCOUNT"), "{got:?}");
        assert!(
            items.iter().all(|i| i.kind == CompletionKind::Keyword),
            "only keywords after SET: {got:?}"
        );
        for bad in ["GETDATE", "ABS", "Sales.Customer", "HR.Department"] {
            assert!(!got.iter().any(|l| l == bad), "SET offered {bad}: {got:?}");
        }

        let sql = "UPDATE Sales.Customer SET ";
        let got = labels(&complete(sql, sql.len(), &catalog));
        assert!(got.iter().any(|l| l == "FullName"), "{got:?}");
        assert!(!got.iter().any(|l| l == "NOCOUNT"), "{got:?}");
    }

    #[test]
    fn scan_aware_latency_budget() {
        let mut catalog = cookbook_catalog();
        for idx in 0..1_000 {
            catalog.objects.push(CompletionObject {
                database: Some("AppDb".to_string()),
                schema: Some("Sales".to_string()),
                name: format!("Table{idx}"),
                kind: ObjectKind::Table,
                columns: vec!["Id".to_string(), "Name".to_string()],
                parameters: Vec::new(),
                foreign_keys: Vec::new(),
            });
        }
        // Complex query with comments, string literals, subquery, and CTE.
        let sql = "WITH cte AS (SELECT a FROM t1) \
                   SELECT * FROM cte /* comment */ \
                   WHERE Name = 'test' AND Id IN \
                   (SELECT Id FROM HR.Department WHERE Dep)";
        let started = Instant::now();
        let items = complete(sql, sql.len(), &catalog);
        assert!(!items.is_empty());
        assert!(
            started.elapsed() < std::time::Duration::from_millis(50),
            "scan-based completion should stay under 50ms, took {:?}",
            started.elapsed()
        );
    }
}
