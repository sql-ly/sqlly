//! Symbol-mode completion: a pure snapshot lookup (no I/O, per-keystroke
//! safe). Dispatches on the detected [`CursorContext`] to build, score, and
//! rank candidate items.

use super::context::{CursorContext, POST_FROM_JOIN_KEYWORDS, cursor_context, parse_object_ref};
use super::fuzzy;
use super::joins::fk_join_candidates;
use super::scoring::qualified_object_score;
use super::set_options;
use super::types::{CompletionItem, CompletionItemKind, CompletionRequest, Completions};
use sqlly_lang::sqlmodel::{Dialect, ModelSnapshot, ObjectKind, ObjectRef, Span, SqlName};
use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::HashSet;

const SYMBOL_LIMIT: usize = 200;

/// Lifts the clause keywords above any object or column match score where
/// both are offered — after a finished SELECT list (`FROM` leads) and after a
/// FROM source (`WHERE` leads).
const CLAUSE_KEYWORD_BOOST: i32 = 10_000;

pub fn complete_symbols(req: &CompletionRequest, snapshot: &ModelSnapshot) -> Completions {
    let ctx = cursor_context(&req.text, req.cursor);
    // (item, match score) pairs; sorted by score then label at the end.
    let mut items: Vec<(CompletionItem, i32)> = Vec::new();

    let match_score = |name: &SqlName, prefix: &str| fuzzy::score(prefix, name.spelling());

    match &ctx {
        CursorContext::AliasDot {
            alias,
            prefix,
            replace,
        } => {
            // Find the table the alias maps to via slot/source analysis, scoped
            // to the cursor's own statement so a prior statement's alias of the
            // same name does not resolve here.
            let tree = sqlly_lang::sqlparse::parse(&req.text, req.dialect);
            let cursor_stmt =
                super::scope::cursor_statement_index(&tree, &req.text, req.cursor, req.dialect);
            let source = tree
                .references
                .iter()
                .filter(|u| super::scope::use_in_scope(u.statement_index, cursor_stmt))
                // Only a table source *defines* an alias. A qualified column use
                // (`jl.JournalId` in the select list) carries the same
                // `via_alias` but its `reference` is the column, not the table —
                // matching it here would resolve nothing table-like and blank
                // the completion. See feedback ifb_5e41.
                .filter(|u| u.use_kind == sqlly_lang::sqlparse::UseKind::TableSource)
                .find(|u| {
                    u.via_alias
                        .as_deref()
                        .map(|a| a.eq_ignore_ascii_case(alias))
                        .unwrap_or(false)
                })
                .map(|u| u.reference.clone())
                // Alias may actually be a table name used as qualifier.
                .or_else(|| {
                    Some(ObjectRef::bare(SqlName::new(
                        alias.clone(),
                        req.dialect,
                        false,
                    )))
                });
            if let Some(reference) = source
                && let Some(entry) = snapshot
                    .resolve(
                        &reference,
                        req.default_catalog.as_ref(),
                        req.default_namespace.as_ref(),
                    )
                    .into_iter()
                    .find(|e| matches!(e.kind, ObjectKind::Table | ObjectKind::View))
            {
                let tentative = !entry.fidelity.is_exact();
                // Views carry columns too; both bodies expose ColumnDefs.
                let columns = match &entry.body {
                    sqlly_lang::sqlmodel::ObjectBody::Table(t) => Some(&t.columns),
                    sqlly_lang::sqlmodel::ObjectBody::View(v) => Some(&v.columns),
                    _ => None,
                };
                if let Some(columns) = columns {
                    let parent_name = entry.reference.name.spelling().to_string();
                    let parent_kind = kind_to_item(entry.kind);
                    let parent_schema = entry
                        .reference
                        .namespace
                        .as_ref()
                        .map(|n| n.spelling().to_string());
                    for col in columns {
                        if let Some(score) = match_score(&col.name, prefix) {
                            let mut item = CompletionItem::symbol(
                                col.name.spelling().to_string(),
                                col.name.spelling().to_string(),
                                CompletionItemKind::Column,
                            );
                            item.detail = col.ty.as_ref().map(|t| t.native.clone());
                            item.replace_span = Some(*replace);
                            item.tentative = tentative;
                            item.r#type = col.ty.as_ref().map(|t| t.native.clone());
                            item.parent = Some(parent_name.clone());
                            item.parent_type = Some(parent_kind);
                            item.schema = parent_schema.clone();
                            item.alias = Some(alias.clone());
                            item.why = Some(Cow::Borrowed("alias"));
                            items.push((item, score));
                        }
                    }
                }
            }
        }
        CursorContext::SchemaDot {
            schema,
            kinds,
            prefix,
            replace,
        } => {
            // Schema-qualified object names (`FROM HR.<prefix>`): only objects
            // in `schema`, matched against the *bare* name (the schema is already
            // committed by the explicit `schema.` qualifier). Every fuzzy tier
            // applies, so the list is a contains search ranked prefix, then
            // camel/word-start, then plain substring.
            for entry in snapshot.objects() {
                if !kinds.contains(&entry.kind) {
                    continue;
                }
                let in_schema = entry
                    .reference
                    .namespace
                    .as_ref()
                    .map(|ns| ns.spelling().eq_ignore_ascii_case(schema))
                    .unwrap_or(false);
                if !in_schema {
                    continue;
                }
                let name = entry.reference.name.spelling();
                let score = if prefix.is_empty() {
                    Some(fuzzy::CONTAINS_SCORE)
                } else {
                    // Every tier down to contains: `dbo.ev` offers everything
                    // with `ev` in its name, prefix and word-start hits first
                    // (feedback ifb_2ff4).
                    fuzzy::score(prefix, name)
                };
                let Some(score) = score else { continue };
                // The schema (`Sales.`) is already in the buffer, so the label
                // shows the fully-qualified name for clarity while the insert is
                // the bare object name; with the name-only `replace` span this
                // yields `Sales.SalesOrder` without duplicating the schema.
                let mut item = CompletionItem::symbol(
                    entry.reference.display(),
                    name.to_string(),
                    kind_to_item(entry.kind),
                );
                item.detail = Some(format!("{:?}", entry.kind));
                item.replace_span = Some(*replace);
                item.tentative = !entry.fidelity.is_exact();
                item.name = name.to_string();
                item.schema = Some(schema.clone());
                items.push((item, score));
            }
        }
        CursorContext::ObjectName {
            kinds,
            prefix,
            replace,
        } => {
            // FK-based join candidates, boosted above plain table names.
            items.extend(fk_join_candidates(req, snapshot, prefix, *replace));

            for entry in snapshot.objects() {
                // Unqualified object names are matched against the *qualified*
                // display (`Sales.SalesOrder`) so a schema-spanning acronym like
                // `SSO` (S-ales, S-ales, O-rder) resolves, while still excluding
                // unrelated objects whose word-starts don't line up.
                if kinds.contains(&entry.kind)
                    && let Some(score) = qualified_object_score(prefix, &entry.reference.display())
                {
                    let mut item = CompletionItem::symbol(
                        entry.reference.display(),
                        entry.reference.display(),
                        kind_to_item(entry.kind),
                    );
                    item.detail = Some(format!("{:?}", entry.kind));
                    item.replace_span = Some(*replace);
                    item.tentative = !entry.fidelity.is_exact();
                    item.name = entry.reference.name.spelling().to_string();
                    item.schema = entry
                        .reference
                        .namespace
                        .as_ref()
                        .map(|n| n.spelling().to_string());
                    items.push((item, score));
                }
            }
        }
        CursorContext::SetOption {
            head,
            head_span,
            prefix,
            replace,
        } => {
            items.extend(set_option_items(
                req.dialect,
                *head,
                *head_span,
                prefix,
                *replace,
            ));
        }
        CursorContext::AfterPredicate {
            keywords,
            prefix,
            replace,
        } => {
            // Only a connector or the next clause can follow a finished
            // test, in their listed order (see feedback ifb_6f67).
            for (rank, kw) in keywords.iter().enumerate() {
                if let Some(score) = fuzzy::score(prefix, kw) {
                    let mut item = CompletionItem::symbol(
                        (*kw).to_string(),
                        (*kw).to_string(),
                        CompletionItemKind::Keyword,
                    );
                    item.detail = Some("keyword".into());
                    item.replace_span = Some(*replace);
                    items.push((item, CLAUSE_KEYWORD_BOOST - rank as i32 + score));
                }
            }
        }
        CursorContext::AfterSelectList {
            keywords,
            prefix,
            replace,
        } => {
            // The clause keywords lead, in their listed order (FROM, then
            // INTO), ahead of every table.
            for (rank, kw) in keywords.iter().enumerate() {
                if let Some(score) = fuzzy::score(prefix, kw) {
                    let mut item = CompletionItem::symbol(
                        (*kw).to_string(),
                        (*kw).to_string(),
                        CompletionItemKind::Keyword,
                    );
                    item.detail = Some("keyword".into());
                    item.replace_span = Some(*replace);
                    items.push((item, CLAUSE_KEYWORD_BOOST - rank as i32 + score));
                }
            }
            // Then the table-like objects, filtered exactly as after `FROM`,
            // each inserting the whole `FROM⏎⇥<name>` clause.
            let unit = crate::intellisense::infer_indent_unit(&req.text);
            for entry in snapshot.objects() {
                if !matches!(entry.kind, ObjectKind::Table | ObjectKind::View) {
                    continue;
                }
                let display = entry.reference.display();
                let Some(score) = qualified_object_score(prefix, &display) else {
                    continue;
                };
                let insert = crate::intellisense::clause_insert_text(
                    &req.text,
                    replace.start,
                    "FROM",
                    &display,
                    &unit,
                );
                let mut item =
                    CompletionItem::symbol(display.clone(), insert, kind_to_item(entry.kind));
                item.detail = Some(crate::intellisense::FROM_CLAUSE_DETAIL.into());
                item.replace_span = Some(*replace);
                item.tentative = !entry.fidelity.is_exact();
                item.name = entry.reference.name.spelling().to_string();
                item.schema = entry
                    .reference
                    .namespace
                    .as_ref()
                    .map(|n| n.spelling().to_string());
                items.push((item, score));
            }
        }
        CursorContext::PostFromTable {
            has_aggregate,
            order_by_allowed,
            offer_columns,
            prefix,
            replace,
        } => {
            // JOIN keywords are only meaningful when the present table(s)
            // participate in a foreign-key relationship. Probe with an empty
            // prefix so the FK check is independent of the keyword being typed.
            let has_fks = !fk_join_candidates(req, snapshot, "", *replace).is_empty();
            // WHERE always leads (feedback ifb_92d9). GROUP BY follows it when
            // the select list aggregates, or when there are no JOIN keywords
            // to surface ahead of it.
            let group_by_first = *has_aggregate || !has_fks;

            let mut kws: Vec<&str> = Vec::new();
            if has_fks {
                kws.extend_from_slice(POST_FROM_JOIN_KEYWORDS);
            }
            kws.push("WHERE");
            kws.push("GROUP BY");
            if *order_by_allowed {
                kws.push("ORDER BY");
            }

            for kw in kws {
                if let Some(base) = fuzzy::score(prefix, kw) {
                    let boost = match kw {
                        "WHERE" => 2000,
                        "GROUP BY" if group_by_first => 1000,
                        _ => 0,
                    };
                    let mut item = CompletionItem::symbol(
                        kw.to_string(),
                        kw.to_string(),
                        CompletionItemKind::Keyword,
                    );
                    item.detail = Some("keyword".into());
                    item.replace_span = Some(*replace);
                    items.push((item, CLAUSE_KEYWORD_BOOST + base + boost));
                }
            }

            // Then the in-scope columns, each inserting `WHERE⏎⇥<column>` so
            // accepting one starts the filter. The typed prefix is blanked for
            // the parse so it is not read as the table's alias.
            if *offer_columns {
                let parse_text = format!(
                    "{}{}{}",
                    &req.text[..replace.start],
                    " ".repeat(replace.end - replace.start),
                    &req.text[replace.end..]
                );
                let unit = crate::intellisense::infer_indent_unit(&req.text);
                for (mut item, score) in scope_columns(req, snapshot, &parse_text, prefix, *replace)
                {
                    item.insert_text = crate::intellisense::clause_insert_text(
                        &req.text,
                        replace.start,
                        "WHERE",
                        &item.label,
                        &unit,
                    );
                    item.detail = Some(crate::intellisense::WHERE_CLAUSE_DETAIL.into());
                    items.push((item, score));
                }
            }
        }
        CursorContext::General {
            prefix,
            replace,
            columns_only,
        } => {
            // In an ORDER BY / GROUP BY item list no object name is legal, so
            // the catalog half of this context is skipped entirely and only
            // the in-scope columns below are offered.
            for entry in snapshot.objects().filter(|_| !*columns_only) {
                if let Some(score) = match_score(&entry.reference.name, prefix) {
                    let mut item = CompletionItem::symbol(
                        entry.reference.display(),
                        entry.reference.display(),
                        kind_to_item(entry.kind),
                    );
                    item.detail = Some(format!("{:?}", entry.kind));
                    item.replace_span = Some(*replace);
                    item.tentative = !entry.fidelity.is_exact();
                    item.name = entry.reference.name.spelling().to_string();
                    item.schema = entry
                        .reference
                        .namespace
                        .as_ref()
                        .map(|n| n.spelling().to_string());
                    items.push((item, score));
                }
            }

            items.extend(scope_columns(req, snapshot, &req.text, prefix, *replace));

            // Inside a GROUP BY list the select-list columns it still needs
            // lead, marked required (feedback ifb_75bd).
            for label in
                crate::intellisense::required_group_by_labels(&req.text, req.cursor, prefix)
            {
                items.retain(|(item, _)| !item.label.eq_ignore_ascii_case(&label));
                let mut item =
                    CompletionItem::symbol(label.clone(), label, CompletionItemKind::Column);
                item.detail = Some(crate::intellisense::REQUIRED_GROUP_BY_DETAIL.into());
                item.replace_span = Some(*replace);
                item.why = Some(Cow::Borrowed("required"));
                items.push((
                    item,
                    crate::intellisense::clause_insert::REQUIRED_GROUP_BY_SCORE,
                ));
            }
        }
        CursorContext::ExecArg {
            proc,
            prefix,
            replace,
        } => {
            // Complete the declared parameters of the routine being EXEC'd.
            let reference = parse_object_ref(proc, req.dialect);
            if let Some(entry) = snapshot
                .resolve(
                    &reference,
                    req.default_catalog.as_ref(),
                    req.default_namespace.as_ref(),
                )
                .into_iter()
                .find(|e| {
                    matches!(
                        e.kind,
                        ObjectKind::Procedure
                            | ObjectKind::Function
                            | ObjectKind::ScalarFunction
                            | ObjectKind::TableFunction
                            | ObjectKind::AggregateFunction
                    )
                })
                && let sqlly_lang::sqlmodel::ObjectBody::Routine(routine) = &entry.body
            {
                let tentative = !entry.fidelity.is_exact();
                let parent_name = entry.reference.name.spelling().to_string();
                let parent_kind = kind_to_item(entry.kind);
                let parent_schema = entry
                    .reference
                    .namespace
                    .as_ref()
                    .map(|n| n.spelling().to_string());
                for param in &routine.params {
                    let pname = param.name.spelling();
                    if let Some(score) = fuzzy::score(prefix, pname) {
                        let mut item = CompletionItem::symbol(
                            pname.to_string(),
                            pname.to_string(),
                            CompletionItemKind::Parameter,
                        );
                        item.detail = param.ty.as_ref().map(|t| t.native.clone());
                        item.replace_span = Some(*replace);
                        item.tentative = tentative;
                        item.name = pname.to_string();
                        item.r#type = param.ty.as_ref().map(|t| t.native.clone());
                        item.parent = Some(parent_name.clone());
                        item.parent_type = Some(parent_kind);
                        item.schema = parent_schema.clone();
                        item.why = Some(Cow::Borrowed("parameter"));
                        items.push((item, score));
                    }
                }
            }
        }
    }

    // Best match first; ties alphabetical, then insertion order (the final
    // tie-break makes the order total, reproducing the previous stable sort
    // exactly). Only the returned page is fully sorted: on large catalogs the
    // top SYMBOL_LIMIT is partitioned out in O(n) first instead of sorting
    // every candidate.
    let mut indexed: Vec<(usize, (CompletionItem, i32))> = items.into_iter().enumerate().collect();
    fn cmp(a: &(usize, (CompletionItem, i32)), b: &(usize, (CompletionItem, i32))) -> Ordering {
        b.1.1
            .cmp(&a.1.1)
            .then_with(|| a.1.0.label.cmp(&b.1.0.label))
            .then_with(|| a.0.cmp(&b.0))
    }
    let truncated = indexed.len() > SYMBOL_LIMIT;
    if truncated {
        indexed.select_nth_unstable_by(SYMBOL_LIMIT, cmp);
        indexed.truncate(SYMBOL_LIMIT);
    }
    indexed.sort_by(cmp);
    Completions {
        items: indexed.into_iter().map(|(_, (item, _))| item).collect(),
        truncated,
        snapshot_generation: snapshot.generation,
        slot_note: None,
    }
}

/// The completions after a session-settings statement head (feedback
/// ifb_978b, ifb_0fbc): when the typed head is not the engine's form, one
/// steering item that rewrites the head to the engine's form (`SET` →
/// `ALTER SESSION SET` on Oracle, → `PRAGMA` on SQLite, and back again on
/// the engines that use `SET`), then the names that head can set on the
/// engine — nothing else, never a function or a table.
fn set_option_items(
    dialect: Dialect,
    head: set_options::SetHead,
    head_span: Span,
    prefix: &str,
    replace: Span,
) -> Vec<(CompletionItem, i32)> {
    let mut items: Vec<(CompletionItem, i32)> = Vec::new();
    let native = set_options::native_set_head(dialect);
    if head != native {
        let keyword = native.keyword();
        let mut item = CompletionItem::symbol(
            keyword.to_string(),
            format!("{keyword} {prefix}"),
            CompletionItemKind::Keyword,
        );
        item.detail = Some(set_options::set_head_note(dialect).to_string());
        item.replace_span = Some(Span::new(head_span.start, replace.end));
        item.name = keyword.to_string();
        item.why = Some(Cow::Borrowed("statement form"));
        items.push((item, CLAUSE_KEYWORD_BOOST));
    }
    // Names are matched by the same fuzzy scorer as keywords, which
    // case-folds, so `iden`, `IDEN` and `Iden` all reach IDENTITY_INSERT.
    for name in set_options::settable_names(dialect, head) {
        if let Some(score) = fuzzy::score(prefix, name) {
            let mut item = CompletionItem::symbol(
                (*name).to_string(),
                (*name).to_string(),
                CompletionItemKind::Keyword,
            );
            item.detail = Some(head.detail().into());
            item.replace_span = Some(replace);
            item.name = (*name).to_string();
            items.push((item, score));
        }
    }
    items
}

/// The completions at `cursor` when it sits after a session-settings
/// statement head (see [`set_option_items`]), best first, or `None` anywhere
/// else. Needs no snapshot: the engine's settable names are static.
pub(crate) fn set_statement_items(
    text: &str,
    cursor: usize,
    dialect: Dialect,
) -> Option<Vec<CompletionItem>> {
    match cursor_context(text, cursor) {
        CursorContext::SetOption {
            head,
            head_span,
            prefix,
            replace,
        } => {
            let mut items = set_option_items(dialect, head, head_span, &prefix, replace);
            items.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.label.cmp(&b.0.label)));
            Some(items.into_iter().map(|(item, _)| item).collect())
        }
        _ => None,
    }
}

/// The columns in scope at the cursor, from the table references of the
/// cursor's own statement, matched against `prefix`. `parse_text` is the text
/// parsed for those references — normally the request text, but with the typed
/// prefix blanked where it would otherwise parse as part of the query (a word
/// after `FROM t` reads as `t`'s alias).
fn scope_columns(
    req: &CompletionRequest,
    snapshot: &ModelSnapshot,
    parse_text: &str,
    prefix: &str,
    replace: Span,
) -> Vec<(CompletionItem, i32)> {
    let mut items: Vec<(CompletionItem, i32)> = Vec::new();
    // Columns in scope from the query's table references
    // (WHERE / SELECT-list / GROUP BY / ORDER BY / HAVING / ON / SET).
    // An aliased table is qualified with its alias so accepting inserts
    // e.g. `pm.Description`; an unaliased table emits bare column names.
    // `match_text` carries the bare name so the client scores the column
    // on a prefix hit rather than an acronym hit on the qualified label.
    let tree = sqlly_lang::sqlparse::parse(parse_text, req.dialect);
    // Columns come only from the cursor's own statement — a previous
    // statement's FROM tables are out of scope.
    let cursor_stmt =
        super::scope::cursor_statement_index(&tree, parse_text, req.cursor, req.dialect);
    let mut seen: HashSet<String> = HashSet::new();
    for u in &tree.references {
        if !super::scope::use_in_scope(u.statement_index, cursor_stmt) {
            continue;
        }
        let Some(entry) = snapshot
            .resolve(
                &u.reference,
                req.default_catalog.as_ref(),
                req.default_namespace.as_ref(),
            )
            .into_iter()
            .find(|e| matches!(e.kind, ObjectKind::Table | ObjectKind::View))
        else {
            continue;
        };
        let tentative = !entry.fidelity.is_exact();
        let columns = match &entry.body {
            sqlly_lang::sqlmodel::ObjectBody::Table(t) => Some(&t.columns),
            sqlly_lang::sqlmodel::ObjectBody::View(v) => Some(&v.columns),
            _ => None,
        };
        let Some(columns) = columns else { continue };
        let alias = u.via_alias.as_deref().filter(|a| !a.is_empty());
        let parent_name = entry.reference.name.spelling().to_string();
        let parent_kind = kind_to_item(entry.kind);
        let parent_schema = entry
            .reference
            .namespace
            .as_ref()
            .map(|n| n.spelling().to_string());
        for col in columns {
            let bare = col.name.spelling().to_string();
            let dedupe_key = match alias {
                Some(a) => {
                    format!("{}@{}", a.to_ascii_lowercase(), bare.to_ascii_lowercase())
                }
                None => bare.to_ascii_lowercase(),
            };
            if !seen.insert(dedupe_key) {
                continue;
            }
            let Some(score) = fuzzy::score(prefix, col.name.spelling()) else {
                continue;
            };
            let (label, insert_text, match_text) = match alias {
                Some(a) => (
                    format!("{}.{}", a, bare),
                    format!("{}.{}", a, bare),
                    Some(bare.clone()),
                ),
                None => (bare.clone(), bare.clone(), None),
            };
            let mut item = CompletionItem::symbol(label, insert_text, CompletionItemKind::Column);
            item.detail = col.ty.as_ref().map(|t| t.native.clone());
            item.replace_span = Some(replace);
            item.tentative = tentative;
            item.match_text = match_text;
            item.name = bare;
            item.r#type = col.ty.as_ref().map(|t| t.native.clone());
            item.parent = Some(parent_name.clone());
            item.parent_type = Some(parent_kind);
            item.schema = parent_schema.clone();
            item.alias = alias.map(|a| a.to_string());
            item.why = Some(Cow::Borrowed(if alias.is_some() {
                "alias"
            } else {
                "scope"
            }));
            items.push((item, score));
        }
    }
    items
}

fn kind_to_item(kind: ObjectKind) -> CompletionItemKind {
    match kind {
        ObjectKind::Table => CompletionItemKind::Table,
        ObjectKind::View | ObjectKind::MaterializedView => CompletionItemKind::View,
        ObjectKind::Procedure => CompletionItemKind::Procedure,
        ObjectKind::Function
        | ObjectKind::ScalarFunction
        | ObjectKind::TableFunction
        | ObjectKind::AggregateFunction => CompletionItemKind::Function,
        _ => CompletionItemKind::Keyword,
    }
}

#[cfg(test)]
mod alias_dot_tests {
    use super::*;
    use crate::modelengine::intellisense::CompletionMode;
    use sqlly_lang::sqlmodel::{
        ColumnDef, Dialect, Fidelity, IncompleteRegions, ObjectBody, ObjectFragment, ObjectKind,
        ObjectRef, Provenance, TableBody,
    };
    use std::collections::BTreeSet;

    fn name(text: &str) -> SqlName {
        SqlName::new(text, Dialect::SqlServer, false)
    }

    fn col(text: &str) -> ColumnDef {
        ColumnDef {
            name: name(text),
            ty: None,
            nullable: None,
            default: None,
            identity: false,
            generated: None,
            collation: None,
            ordinal: 0,
        }
    }

    fn table_with_cols(schema: &str, table: &str, cols: &[&str]) -> ObjectFragment {
        ObjectFragment {
            reference: ObjectRef::new(None, Some(name(schema)), name(table)),
            kind: ObjectKind::Table,
            dialect: Dialect::SqlServer,
            body: ObjectBody::Table(TableBody {
                columns: cols.iter().map(|c| col(c)).collect(),
                ..TableBody::default()
            }),
            provenance: Provenance::Database {
                profile: "test".into(),
            },
            fidelity: Fidelity::Exact,
            references: Vec::new(),
        }
    }

    fn snapshot(fragments: Vec<ObjectFragment>) -> ModelSnapshot {
        let (snap, _) = ModelSnapshot::empty().apply(
            1,
            fragments,
            &BTreeSet::new(),
            IncompleteRegions::default(),
        );
        snap
    }

    fn complete(text: &str, snap: &ModelSnapshot) -> Vec<CompletionItem> {
        let req = CompletionRequest {
            text: text.to_string(),
            cursor: text.len(),
            dialect: Dialect::SqlServer,
            default_catalog: None,
            default_namespace: Some(name("dbo")),
            mode: CompletionMode::Symbol,
        };
        complete_symbols(&req, snap).items
    }

    /// The reported case: inside an `ALTER PROCEDURE` body, with the cursor at
    /// `t.` in a JOIN ON clause, completion must offer the columns of the table
    /// `t` is aliased to (`[Shared].[Contact]`). Before the ALTER-body fix the
    /// statement parsed as Opaque, no table source was harvested, and the
    /// AliasDot path resolved nothing — so this returned an empty list.
    #[test]
    fn alias_dot_in_alter_procedure_join_on_clause_completes_aliased_table_columns() {
        let snap = snapshot(vec![
            table_with_cols(
                "Shared",
                "Contact",
                &["ContactId", "FirstName", "LastName", "IsActive"],
            ),
            table_with_cols("Shared", "ContactPostalAddress", &["ContactId"]),
        ]);
        let sql = "ALTER PROCEDURE [Shared].[pContact_ReferenceList] (@TenantId smallint) AS BEGIN \
            SELECT t.ContactId FROM [Shared].[Contact] t \
            INNER JOIN Shared.ContactPostalAddress cpa ON cpa.ContactId = t.";
        let items = complete(sql, &snap);
        let labels: Vec<&str> = items
            .iter()
            .filter(|i| i.kind == CompletionItemKind::Column)
            .map(|i| i.label.as_str())
            .collect();
        assert!(
            labels.contains(&"ContactId")
                && labels.contains(&"FirstName")
                && labels.contains(&"IsActive"),
            "AliasDot should complete Shared.Contact columns, got: {labels:?}"
        );
    }
}
