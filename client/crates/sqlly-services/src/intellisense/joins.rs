//! FK join candidate generation. When the cursor is in a table-source
//! context (intent `FromSource` or `JoinSource`), this module inspects the
//! catalog's `foreign_keys` metadata and the tables already referenced in
//! the query (from `CompletionContext.table_refs`) to offer ready-to-use
//! join suggestions — `Sales.Customers c ON c.CustomerId = o.CustomerId` —
//! mirroring the Swift `EditorCompletion.addForeignKeyJoinCandidates`
//! behavior: both FK directions are considered, pairs whose tables are all
//! already in the statement are skipped, the present side is referenced by
//! its alias (or qualified name), and the joined table gets a synthesized
//! collision-free alias.
//!
//! `JoinCondition` intent additionally yields bare `a.Col = b.Col` ON-pair
//! candidates, though the default policy currently filters `Join`-kind items
//! out of that intent (matching the Swift policy).
//!
//! Scoring: join candidates rank above generic table completions (base 80)
//! but below procedure snippets (110). We use 95 plus the fuzzy match score
//! of the typed prefix against the joined table's qualified name — the same
//! `base + match` shape the Swift builder uses.

use crate::intellisense::context::{CompletionContext, TableRef};
use crate::intellisense::rules::IntelliSenseIntent;
use sqlly_core::{DatabaseType, ObjectKind};
use sqlly_intel::intellisense::{
    CompletionCatalog, CompletionItem, CompletionKind, CompletionObject, CompletionSource,
    FkRelation, qualified_object_score, quote_path_if_needed,
};
use std::borrow::Cow;
use std::collections::HashSet;

/// Base score for FK join candidates. Above typical table completions (max
/// ~107), below procedure snippets (110).
const FK_JOIN_SCORE: i32 = 95;

/// Lowercased `schema.table` keys for tables related via FK to tables already
/// in the statement. Used to flag plain table completions as FK-related.
pub(crate) fn fk_related_table_keys(
    catalog: &CompletionCatalog,
    ctx: &CompletionContext,
) -> HashSet<String> {
    let mut keys = HashSet::new();
    if !matches!(
        ctx.intent,
        IntelliSenseIntent::FromSource | IntelliSenseIntent::JoinSource
    ) {
        return keys;
    }

    for table_ref in &ctx.table_refs {
        let Some(object) = resolve_ref(catalog, table_ref) else {
            continue;
        };
        let from_schema = object.schema.as_deref().unwrap_or_default().to_lowercase();
        let from_name = object.name.to_lowercase();

        for fk in &object.foreign_keys {
            let (to_schema, to_table) = resolve_fk_target(catalog, fk);
            keys.insert(format!(
                "{}.{}",
                to_schema.to_lowercase(),
                to_table.to_lowercase()
            ));
        }

        for obj in &catalog.objects {
            if obj.kind != ObjectKind::Table {
                continue;
            }
            for fk in &obj.foreign_keys {
                let (target_schema, target_table) = resolve_fk_target(catalog, fk);
                if target_schema.eq_ignore_ascii_case(&from_schema)
                    && target_table.eq_ignore_ascii_case(&from_name)
                {
                    let obj_schema = obj.schema.as_deref().unwrap_or_default().to_lowercase();
                    keys.insert(format!("{}.{}", obj_schema, obj.name.to_lowercase()));
                }
            }
        }
    }

    keys
}

/// Generate FK join candidate completions for the current context.
///
/// Returns `Vec<CompletionItem>` — possibly empty if no FK relationships
/// connect to the tables already in the query.
pub fn join_candidates(
    catalog: &CompletionCatalog,
    ctx: &CompletionContext,
) -> Vec<CompletionItem> {
    join_candidates_quoted(catalog, ctx, None)
}

/// [`join_candidates`] with every table, alias, and column in the inserted
/// text quoted as `database_type` requires (`JOIN dbo.[Order Details] od ON
/// od.[Order ID] = o.Id`). Labels keep the plain catalog names.
pub fn join_candidates_quoted(
    catalog: &CompletionCatalog,
    ctx: &CompletionContext,
    database_type: Option<DatabaseType>,
) -> Vec<CompletionItem> {
    let quoter = Quoter(database_type);
    match ctx.intent {
        IntelliSenseIntent::FromSource | IntelliSenseIntent::JoinSource => {
            join_source_candidates(catalog, ctx, quoter)
        }
        IntelliSenseIntent::JoinCondition => join_condition_candidates(catalog, ctx, quoter),
        _ => Vec::new(),
    }
}

/// Quotes identifier paths for the connection's engine, or passes them
/// through when the engine is unknown.
#[derive(Clone, Copy)]
struct Quoter(Option<DatabaseType>);

impl Quoter {
    fn path<'a>(&self, path: &'a str) -> Cow<'a, str> {
        match self.0 {
            Some(db) => quote_path_if_needed(db, path),
            None => Cow::Borrowed(path),
        }
    }

    fn part<'a>(&self, part: &'a str) -> Cow<'a, str> {
        match self.0 {
            Some(db) => db.quote_identifier_if_needed(part),
            None => Cow::Borrowed(part),
        }
    }
}

/// A table already present in the statement, resolved against the catalog,
/// with the token used to refer to it in an ON clause (alias or qualified
/// name). Port of the Swift `PresentTable`.
struct PresentTable {
    /// Lowercased resolved schema name (empty when the object has none).
    schema: String,
    /// Lowercased resolved table name.
    name: String,
    /// Token used to refer to this table in an ON clause.
    reference: String,
}

impl PresentTable {
    fn key(&self) -> String {
        format!("{}.{}", self.schema, self.name)
    }
}

/// For every foreign key (outbound or inbound) touching a table already
/// present in the statement whose other side is *not* present, offer a
/// ready-to-use `Schema.Other o ON o.Col = existing.Col` join suggestion.
/// Port of Swift `addForeignKeyJoinCandidates`.
/// How the typed prefix shapes a FK join suggestion.
#[derive(Debug, Clone, Copy)]
enum JoinHint {
    /// `FromSource`: the `JOIN` keyword hasn't been typed, so synthesize it from
    /// what the user is typing (`i` → `INNER JOIN`, `r` → `RIGHT JOIN`, empty →
    /// `JOIN`) and show every FK join prefixed with it (no table-name filter).
    Keyword(&'static str),
    /// `FromSource` with a table-name-ish prefix: filter by table name and lead
    /// with a plain `JOIN` so the inserted text is valid on its own.
    TableFilter,
    /// `JoinSource`: the keyword is already in the buffer, so emit no keyword and
    /// filter by the joined table's name.
    None,
}

/// Map the intent + typed prefix onto a [`JoinHint`]. Single letters and
/// keyword-prefixes (`i`/`in`/`inner`, `l`/`left`, `r`/`right`, `o`/`outer`,
/// `f`/`full`, `j`/`join`) select a join type; anything else is treated as a
/// table name.
fn join_hint(intent: IntelliSenseIntent, prefix: &str) -> JoinHint {
    if intent != IntelliSenseIntent::FromSource {
        return JoinHint::None;
    }
    let p = prefix.to_lowercase();
    if p.is_empty() {
        return JoinHint::Keyword("JOIN");
    }
    if "inner".starts_with(&p) {
        JoinHint::Keyword("INNER JOIN")
    } else if "left".starts_with(&p) {
        JoinHint::Keyword("LEFT JOIN")
    } else if "right".starts_with(&p) {
        JoinHint::Keyword("RIGHT JOIN")
    } else if "outer".starts_with(&p) || "full".starts_with(&p) {
        JoinHint::Keyword("FULL OUTER JOIN")
    } else if "join".starts_with(&p) {
        JoinHint::Keyword("JOIN")
    } else {
        JoinHint::TableFilter
    }
}

fn join_source_candidates(
    catalog: &CompletionCatalog,
    ctx: &CompletionContext,
    quoter: Quoter,
) -> Vec<CompletionItem> {
    let prefix = &ctx.prefix;
    let hint = join_hint(ctx.intent, prefix);
    let mut items = Vec::new();

    let mut present: Vec<PresentTable> = Vec::new();
    let mut used_aliases: HashSet<String> = HashSet::new();
    for table_ref in &ctx.table_refs {
        let Some(object) = resolve_ref(catalog, table_ref) else {
            continue;
        };
        let reference = table_ref
            .alias
            .clone()
            .unwrap_or_else(|| qualified_name(object));
        present.push(PresentTable {
            schema: object.schema.as_deref().unwrap_or_default().to_lowercase(),
            name: object.name.to_lowercase(),
            reference,
        });
        if let Some(alias) = &table_ref.alias {
            used_aliases.insert(alias.to_lowercase());
        }
    }
    if present.is_empty() {
        return items;
    }

    let present_keys: HashSet<String> = present.iter().map(|p| p.key()).collect();

    for obj in &catalog.objects {
        if obj.kind != ObjectKind::Table {
            continue;
        }
        let from_schema = obj.schema.as_deref().unwrap_or_default().to_lowercase();
        let from_name = obj.name.to_lowercase();
        let from_key = format!("{from_schema}.{from_name}");
        let from_qualified = qualified_name(obj);

        // A composite FK is one suggestion joining every column pair with
        // AND (in key-column order), like the model engine's join builder.
        for key in FkRelation::group(&obj.foreign_keys) {
            let (to_schema, to_table) = resolve_fk_target(catalog, key[0]);
            let to_key = format!("{}.{}", to_schema.to_lowercase(), to_table.to_lowercase());
            let to_qualified = if to_schema.is_empty() {
                to_table.clone()
            } else {
                format!("{to_schema}.{to_table}")
            };

            // Determine which side is already present and which is the new
            // table. new_* = table to JOIN in; existing_* = side in statement.
            // `pairs` is (new-table column, present-table column) per key column.
            let (new_qualified, new_table, pairs, present_side): (_, _, Vec<(&str, &str)>, _) =
                if let Some(p) = present
                    .iter()
                    .find(|p| p.schema == from_schema && p.name == from_name)
                    .filter(|_| !present_keys.contains(&to_key))
                {
                    // Outbound: statement has the child table, suggest the parent.
                    (
                        to_qualified.clone(),
                        to_table.as_str(),
                        key.iter()
                            .map(|fk| (fk.target_column.as_str(), fk.column.as_str()))
                            .collect(),
                        p,
                    )
                } else if let Some(p) = present
                    .iter()
                    .find(|p| format!("{}.{}", p.schema, p.name) == to_key)
                    .filter(|_| !present_keys.contains(&from_key))
                {
                    // Inbound: statement has the parent table, suggest the child.
                    (
                        from_qualified.clone(),
                        obj.name.as_str(),
                        key.iter()
                            .map(|fk| (fk.column.as_str(), fk.target_column.as_str()))
                            .collect(),
                        p,
                    )
                } else {
                    continue;
                };

            // Decide the score, and whether to lead with a synthesized join
            // keyword, based on the typed prefix. A join-type hint shows every
            // FK join prefixed with that keyword; a table-name prefix filters by
            // the joined table's qualified name (the Swift `matchText`).
            let (score, keyword) = match hint {
                JoinHint::Keyword(kw) => (FK_JOIN_SCORE, Some(kw)),
                JoinHint::TableFilter => match score_for_prefix(prefix, &new_qualified) {
                    Some(score) => (score, Some("JOIN")),
                    None => continue,
                },
                JoinHint::None => match score_for_prefix(prefix, &new_qualified) {
                    Some(score) => (score, None),
                    None => continue,
                },
            };

            let alias = make_alias(new_table, &mut used_aliases);
            let conditions = pairs
                .iter()
                .map(|(new_column, existing_column)| {
                    format!(
                        "{alias}.{} = {}.{}",
                        quoter.part(new_column),
                        quoter.path(&present_side.reference),
                        quoter.part(existing_column),
                    )
                })
                .collect::<Vec<_>>()
                .join(" AND ");
            let label_conditions = pairs
                .iter()
                .map(|(new_column, existing_column)| {
                    format!(
                        "{new_column} = {}.{existing_column}",
                        present_side.reference
                    )
                })
                .collect::<Vec<_>>()
                .join(" AND ");
            let target = format!("{} {alias} ON {conditions}", quoter.path(&new_qualified));
            let (insert_text, label) = match keyword {
                Some(kw) => (
                    format!("{kw} {target}"),
                    format!("{kw} {new_qualified} ON {label_conditions}"),
                ),
                None => (target, format!("{new_qualified} ON {label_conditions}")),
            };
            items.push(CompletionItem {
                label,
                insert_text,
                kind: CompletionKind::Join,
                source: CompletionSource::Inferred,
                score,
                detail: "fk".to_string(),
                replace_span: None,
            });
        }
    }

    items
}

/// Resolves a statement table ref against the catalog: name must match, and
/// when the ref is schema-qualified the schema must match too. Port of the
/// Swift `resolve(_:in:)`.
fn resolve_ref<'a>(
    catalog: &'a CompletionCatalog,
    table_ref: &TableRef,
) -> Option<&'a CompletionObject> {
    catalog.objects.iter().find(|object| {
        object.name.eq_ignore_ascii_case(&table_ref.name)
            && match (&table_ref.schema, &object.schema) {
                (Some(ref_schema), Some(obj_schema)) => obj_schema.eq_ignore_ascii_case(ref_schema),
                (Some(_), None) => false,
                (None, _) => true,
            }
    })
}

/// The display side of an FK target. DDL-derived FKs may carry an empty
/// target schema; resolve it through the catalog so present-table matching
/// and the suggestion label both use the real qualified name.
fn resolve_fk_target(catalog: &CompletionCatalog, fk: &FkRelation) -> (String, String) {
    if !fk.target_schema.is_empty() {
        return (fk.target_schema.clone(), fk.target_table.clone());
    }
    if let Some(object) = catalog
        .objects
        .iter()
        .find(|o| o.kind == ObjectKind::Table && o.name.eq_ignore_ascii_case(&fk.target_table))
    {
        return (
            object.schema.clone().unwrap_or_default(),
            object.name.clone(),
        );
    }
    (String::new(), fk.target_table.clone())
}

/// Stable, collision-free short alias from a table name: the first letter,
/// lowercased, with a numeric suffix on collision (`c`, `c2`, `c3`, …).
/// Registers the chosen alias in `used` so later suggestions avoid it too.
/// Port of the Swift `makeAlias(for:)`.
fn make_alias(table: &str, used: &mut HashSet<String>) -> String {
    let base = table
        .chars()
        .find(|ch| ch.is_alphabetic())
        .map(|ch| ch.to_lowercase().to_string())
        .unwrap_or_else(|| "t".to_string());
    let mut alias = base.clone();
    if used.contains(&alias) {
        let mut n = 2;
        while used.contains(&format!("{base}{n}")) {
            n += 1;
        }
        alias = format!("{base}{n}");
    }
    used.insert(alias.clone());
    alias
}

/// `schema.name`, or the bare name when the object has no schema.
fn qualified_name(object: &CompletionObject) -> String {
    match &object.schema {
        Some(schema) if !schema.is_empty() => format!("{schema}.{}", object.name),
        _ => object.name.clone(),
    }
}

/// Suggest `ON` clause column pairs based on FK relationships.
fn join_condition_candidates(
    catalog: &CompletionCatalog,
    ctx: &CompletionContext,
    quoter: Quoter,
) -> Vec<CompletionItem> {
    let prefix = &ctx.prefix;
    let mut items = Vec::new();

    if ctx.table_refs.len() < 2 {
        return items;
    }

    // The table being joined pairs with every source before it, nearest
    // first, so each foreign key it shares with any of them is offered
    // (feedback ifb_e275).
    let right = &ctx.table_refs[ctx.table_refs.len() - 1];
    let right_obj = find_table(catalog, right);
    let right_alias = right.alias.as_deref().unwrap_or(&right.name);
    // Without an `ON` in the buffer yet, each condition writes it.
    let on = if ctx.join_on_missing { "ON " } else { "" };

    // Each FK — single-column or composite — is one `a.X = b.X [AND …]`
    // suggestion, its pairs in key-column order. The joined table's alias is
    // always written first (`fak.AccountId = a.AccountId`) whichever side owns
    // the key (feedback ifb_13aa); `distance` ranks nearer sources first.
    let mut push_fk_conditions = |owner: &CompletionObject,
                                  owner_alias: &str,
                                  other: &CompletionObject,
                                  other_alias: &str,
                                  owner_is_joined: bool,
                                  distance: i32| {
        for key in FkRelation::group(&owner.foreign_keys) {
            let fk = key[0];
            let targets_other = other
                .schema
                .as_deref()
                .is_some_and(|s| s.eq_ignore_ascii_case(&fk.target_schema))
                && other.name.eq_ignore_ascii_case(&fk.target_table);
            if !targets_other {
                continue;
            }
            let join = |render: &dyn Fn(&FkRelation) -> String| {
                key.iter()
                    .map(|fk| render(fk))
                    .collect::<Vec<_>>()
                    .join(" AND ")
            };
            // (alias, column) pairs: joined side first.
            fn sides<'a>(
                fk: &'a FkRelation,
                owner_alias: &'a str,
                other_alias: &'a str,
                owner_is_joined: bool,
            ) -> ((&'a str, &'a str), (&'a str, &'a str)) {
                let owner_side = (owner_alias, fk.column.as_str());
                let other_side = (other_alias, fk.target_column.as_str());
                if owner_is_joined {
                    (owner_side, other_side)
                } else {
                    (other_side, owner_side)
                }
            }
            let label = join(&|fk| {
                let ((la, lc), (ra, rc)) = sides(fk, owner_alias, other_alias, owner_is_joined);
                format!("{la}.{lc} = {ra}.{rc}")
            });
            let insert = on.to_string()
                + &join(&|fk| {
                    let ((la, lc), (ra, rc)) = sides(fk, owner_alias, other_alias, owner_is_joined);
                    format!(
                        "{}.{} = {}.{}",
                        quoter.part(la),
                        quoter.part(lc),
                        quoter.part(ra),
                        quoter.part(rc)
                    )
                });
            let detail = format!(
                "FK: {} -> {}",
                key.iter()
                    .map(|fk| format!("{owner_alias}.{}", fk.column))
                    .collect::<Vec<_>>()
                    .join(", "),
                key.iter()
                    .map(|fk| format!("{other_alias}.{}", fk.target_column))
                    .collect::<Vec<_>>()
                    .join(", "),
            );
            push_join_condition(&mut items, prefix, &label, insert, detail, distance);
        }
    };

    if let Some(ro) = right_obj {
        let earlier = &ctx.table_refs[..ctx.table_refs.len() - 1];
        for (distance, left) in earlier.iter().rev().enumerate() {
            let Some(lo) = find_table(catalog, left) else {
                continue;
            };
            let left_alias = left.alias.as_deref().unwrap_or(&left.name);
            let distance = distance as i32;
            push_fk_conditions(ro, right_alias, lo, left_alias, true, distance);
            push_fk_conditions(lo, left_alias, ro, right_alias, false, distance);
        }
    }

    items
}

fn push_join_condition(
    items: &mut Vec<CompletionItem>,
    prefix: &str,
    label: &str,
    insert_text: String,
    detail: String,
    distance: i32,
) {
    let score = if prefix.is_empty() {
        FK_JOIN_SCORE
    } else if label.to_lowercase().starts_with(&prefix.to_lowercase()) {
        FK_JOIN_SCORE + 5
    } else {
        return;
    } - distance.min(FK_JOIN_SCORE / 2);
    items.push(CompletionItem {
        label: label.to_string(),
        insert_text,
        kind: CompletionKind::Join,
        source: CompletionSource::Inferred,
        score,
        detail,
        replace_span: None,
    });
}

/// Score the typed prefix against the joined table's qualified name: fuzzy
/// first (camel/prefix/acronym/contains), with an explicit prefix bonus.
/// An empty prefix matches everything at the base score, mirroring the
/// Swift matcher.
fn score_for_prefix(prefix: &str, match_text: &str) -> Option<i32> {
    if prefix.is_empty() {
        return Some(FK_JOIN_SCORE);
    }
    if match_text
        .to_lowercase()
        .starts_with(&prefix.to_lowercase())
    {
        return Some(FK_JOIN_SCORE + 5);
    }
    qualified_object_score(prefix, match_text).map(|fs| FK_JOIN_SCORE + fs)
}

fn find_table<'a>(
    catalog: &'a CompletionCatalog,
    qtable: &TableRef,
) -> Option<&'a CompletionObject> {
    catalog.objects.iter().find(|o| {
        o.kind == ObjectKind::Table
            && o.name.eq_ignore_ascii_case(&qtable.name)
            && o.schema.as_deref().is_some_and(|s| {
                qtable
                    .schema
                    .as_deref()
                    .is_some_and(|qs| s.eq_ignore_ascii_case(qs))
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intellisense::context::CompletionContext;
    use crate::intellisense::rules::{IntelliSenseClause, IntelliSenseTriggerKind};
    use sqlly_core::ObjectKind;
    use sqlly_intel::intellisense::{CompletionCatalog, CompletionObject, FkRelation};

    fn catalog_with_fks() -> CompletionCatalog {
        let orders = CompletionObject {
            database: None,
            schema: Some("dbo".to_string()),
            name: "Orders".to_string(),
            kind: ObjectKind::Table,
            columns: vec!["OrderId".into(), "CustomerId".into()],
            parameters: Vec::new(),
            foreign_keys: vec![FkRelation {
                column: "CustomerId".to_string(),
                target_schema: "dbo".to_string(),
                target_table: "Customers".to_string(),
                target_column: "Id".to_string(),
                constraint: None,
            }],
        };
        let customers = CompletionObject {
            database: None,
            schema: Some("dbo".to_string()),
            name: "Customers".to_string(),
            kind: ObjectKind::Table,
            columns: vec!["Id".into(), "Name".into()],
            parameters: Vec::new(),
            foreign_keys: Vec::new(),
        };
        let products = CompletionObject {
            database: None,
            schema: Some("dbo".to_string()),
            name: "Products".to_string(),
            kind: ObjectKind::Table,
            columns: vec!["ProductId".into(), "Name".into()],
            parameters: Vec::new(),
            foreign_keys: Vec::new(),
        };
        CompletionCatalog {
            databases: vec!["appdb".to_string()],
            schemas: vec!["dbo".to_string()],
            objects: vec![orders, customers, products],
            filesystem_schemas: Vec::new(),
            snippets: Vec::new(),
        }
    }

    fn context_with_intent(
        sql: &str,
        cursor: usize,
        intent: IntelliSenseIntent,
        clause: IntelliSenseClause,
        table_refs: Vec<TableRef>,
    ) -> CompletionContext {
        CompletionContext {
            prefix: sqlly_intel::intellisense::current_prefix(sql, cursor).to_string(),
            replace_text: String::new(),
            replace_start: cursor,
            replace_end: cursor,
            trigger: IntelliSenseTriggerKind::AutomaticTyping,
            manual: false,
            clause,
            intent,
            is_qualified_access: false,
            qualifier: None,
            has_aliases: !table_refs.is_empty(),
            alias_count: table_refs.iter().filter(|t| t.alias.is_some()).count() as i32,
            table_count: table_refs.len() as i32,
            table_refs,
            aliases: Vec::new(),
            parameters: Vec::new(),
            value_context: None,
            statement_lower: sql.to_lowercase(),
            join_on_missing: false,
        }
    }

    /// Port of the Swift `EditorCompletionForeignKeyTests` sample schema:
    /// Sales.Customers is the parent; Sales.Orders is the child with an FK
    /// Orders.CustomerId -> Customers.CustomerId; Sales.OrderLines has an FK
    /// OrderLines.OrderId -> Orders.OrderId.
    fn sales_catalog() -> CompletionCatalog {
        let customers = CompletionObject {
            database: None,
            schema: Some("Sales".to_string()),
            name: "Customers".to_string(),
            kind: ObjectKind::Table,
            columns: vec!["CustomerId".into(), "Name".into()],
            parameters: Vec::new(),
            foreign_keys: Vec::new(),
        };
        let orders = CompletionObject {
            database: None,
            schema: Some("Sales".to_string()),
            name: "Orders".to_string(),
            kind: ObjectKind::Table,
            columns: vec!["OrderId".into(), "CustomerId".into(), "Total".into()],
            parameters: Vec::new(),
            foreign_keys: vec![FkRelation {
                column: "CustomerId".to_string(),
                target_schema: "Sales".to_string(),
                target_table: "Customers".to_string(),
                target_column: "CustomerId".to_string(),
                constraint: None,
            }],
        };
        let order_lines = CompletionObject {
            database: None,
            schema: Some("Sales".to_string()),
            name: "OrderLines".to_string(),
            kind: ObjectKind::Table,
            columns: vec!["OrderLineId".into(), "OrderId".into(), "ProductId".into()],
            parameters: Vec::new(),
            foreign_keys: vec![FkRelation {
                column: "OrderId".to_string(),
                target_schema: "Sales".to_string(),
                target_table: "Orders".to_string(),
                target_column: "OrderId".to_string(),
                constraint: None,
            }],
        };
        CompletionCatalog {
            databases: Vec::new(),
            schemas: vec!["Sales".to_string()],
            objects: vec![customers, orders, order_lines],
            filesystem_schemas: Vec::new(),
            snippets: Vec::new(),
        }
    }

    fn table_ref(schema: &str, name: &str, alias: Option<&str>) -> TableRef {
        TableRef {
            schema: Some(schema.to_string()),
            name: name.to_string(),
            alias: alias.map(|a| a.to_string()),
        }
    }

    fn join_source_ctx(sql: &str, table_refs: Vec<TableRef>) -> CompletionContext {
        context_with_intent(
            sql,
            sql.len(),
            IntelliSenseIntent::JoinSource,
            IntelliSenseClause::Join,
            table_refs,
        )
    }

    #[test]
    fn join_source_suggests_ready_to_use_outbound_join() {
        // Orders is present (child); we should be offered a full JOIN onto
        // Customers (parent), mirroring Swift: insert text carries the
        // synthesized alias and the complete ON clause.
        let catalog = catalog_with_fks();
        let ctx = join_source_ctx(
            "SELECT * FROM dbo.Orders o JOIN ",
            vec![table_ref("dbo", "Orders", Some("o"))],
        );
        let items = join_candidates(&catalog, &ctx);
        assert!(
            items
                .iter()
                .any(|i| i.label == "dbo.Customers ON Id = o.CustomerId"
                    && i.insert_text == "dbo.Customers c ON c.Id = o.CustomerId"
                    && i.kind == CompletionKind::Join
                    && i.detail == "fk"),
            "expected ready-to-use Customers join candidate, got: {:?}",
            items
                .iter()
                .map(|i| (&i.label, &i.insert_text))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn join_source_does_not_suggest_unrelated_table() {
        let catalog = catalog_with_fks();
        let ctx = join_source_ctx(
            "SELECT * FROM dbo.Orders o JOIN ",
            vec![table_ref("dbo", "Orders", Some("o"))],
        );
        let items = join_candidates(&catalog, &ctx);
        assert!(
            !items.iter().any(|i| i.label.starts_with("dbo.Products")),
            "Products has no FK relationship to Orders"
        );
    }

    fn from_source_ctx(sql: &str, table_refs: Vec<TableRef>) -> CompletionContext {
        context_with_intent(
            sql,
            sql.len(),
            IntelliSenseIntent::FromSource,
            IntelliSenseClause::From,
            table_refs,
        )
    }

    #[test]
    fn from_source_synthesizes_join_keyword_from_prefix() {
        let catalog = catalog_with_fks();
        // "i" after the table → every FK join led with INNER JOIN.
        let items = join_candidates(
            &catalog,
            &from_source_ctx(
                "SELECT * FROM dbo.Orders o i",
                vec![table_ref("dbo", "Orders", Some("o"))],
            ),
        );
        assert!(
            items
                .iter()
                .any(|i| i.insert_text == "INNER JOIN dbo.Customers c ON c.Id = o.CustomerId"),
            "expected INNER JOIN, got: {:?}",
            items.iter().map(|i| &i.insert_text).collect::<Vec<_>>()
        );

        // "r" → RIGHT JOIN.
        let items = join_candidates(
            &catalog,
            &from_source_ctx(
                "SELECT * FROM dbo.Orders o r",
                vec![table_ref("dbo", "Orders", Some("o"))],
            ),
        );
        assert!(
            items
                .iter()
                .any(|i| i.insert_text.starts_with("RIGHT JOIN dbo.Customers")),
            "expected RIGHT JOIN, got: {:?}",
            items.iter().map(|i| &i.insert_text).collect::<Vec<_>>()
        );

        // No text → a plain JOIN.
        let items = join_candidates(
            &catalog,
            &from_source_ctx(
                "SELECT * FROM dbo.Orders o ",
                vec![table_ref("dbo", "Orders", Some("o"))],
            ),
        );
        assert!(
            items
                .iter()
                .any(|i| i.insert_text == "JOIN dbo.Customers c ON c.Id = o.CustomerId"),
            "expected plain JOIN, got: {:?}",
            items.iter().map(|i| &i.insert_text).collect::<Vec<_>>()
        );
    }

    #[test]
    fn join_source_reverse_fk_suggests_referencing_table() {
        let catalog = catalog_with_fks();
        let ctx = join_source_ctx(
            "SELECT * FROM dbo.Customers c JOIN ",
            vec![table_ref("dbo", "Customers", Some("c"))],
        );
        let items = join_candidates(&catalog, &ctx);
        assert!(
            items
                .iter()
                .any(|i| i.insert_text == "dbo.Orders o ON o.CustomerId = c.Id"),
            "Orders has FK to Customers, should be suggested as reverse join, got: {:?}",
            items.iter().map(|i| &i.insert_text).collect::<Vec<_>>()
        );
    }

    #[test]
    fn join_source_no_candidates_when_no_fk() {
        let catalog = catalog_with_fks();
        let ctx = join_source_ctx(
            "SELECT * FROM dbo.Products p JOIN ",
            vec![table_ref("dbo", "Products", Some("p"))],
        );
        let items = join_candidates(&catalog, &ctx);
        assert!(items.is_empty(), "Products has no FK relationships");
    }

    #[test]
    fn join_source_does_not_suggest_self_join() {
        let catalog = catalog_with_fks();
        let ctx = join_source_ctx(
            "SELECT * FROM dbo.Orders o JOIN ",
            vec![table_ref("dbo", "Orders", Some("o"))],
        );
        let items = join_candidates(&catalog, &ctx);
        assert!(
            !items.iter().any(|i| i.label.starts_with("dbo.Orders ")),
            "should not suggest self-join"
        );
    }

    // ---- Ports of EditorCompletionForeignKeyTests.swift ----------------

    #[test]
    fn outbound_join_reuses_existing_alias() {
        // The existing Orders alias "o" must be used on the present side of
        // the ON clause, and the parent column on the new side.
        let catalog = sales_catalog();
        let ctx = join_source_ctx(
            "SELECT * FROM Sales.Orders o JOIN ",
            vec![table_ref("Sales", "Orders", Some("o"))],
        );
        let items = join_candidates(&catalog, &ctx);
        let customer_join = items
            .iter()
            .find(|i| i.insert_text.contains("Sales.Customers"))
            .expect("expected an FK join suggestion onto Sales.Customers");
        assert!(
            customer_join.insert_text.contains("= o.CustomerId"),
            "expected ON clause to reference existing alias o, got: {}",
            customer_join.insert_text
        );
        assert!(
            customer_join.insert_text.contains(".CustomerId ="),
            "expected the parent column on the new side, got: {}",
            customer_join.insert_text
        );
    }

    #[test]
    fn inbound_foreign_key_join_suggested() {
        // Customers is present (parent); we should be offered a JOIN onto
        // Orders (child).
        let catalog = sales_catalog();
        let ctx = join_source_ctx(
            "SELECT * FROM Sales.Customers c JOIN ",
            vec![table_ref("Sales", "Customers", Some("c"))],
        );
        let items = join_candidates(&catalog, &ctx);
        assert!(
            items.iter().any(|i| i.insert_text.contains("Sales.Orders")),
            "expected an inbound FK join onto Sales.Orders, got: {:?}",
            items.iter().map(|i| &i.insert_text).collect::<Vec<_>>()
        );
    }

    #[test]
    fn no_suggestion_when_both_tables_already_present() {
        // Both Orders and Customers are already joined; no FK suggestion
        // between them (the OrderLines suggestion is still expected).
        let catalog = sales_catalog();
        let ctx = join_source_ctx(
            "SELECT * FROM Sales.Orders o JOIN Sales.Customers c ON c.CustomerId = o.CustomerId JOIN ",
            vec![
                table_ref("Sales", "Orders", Some("o")),
                table_ref("Sales", "Customers", Some("c")),
            ],
        );
        let items = join_candidates(&catalog, &ctx);
        assert!(
            !items.iter().any(|i| {
                i.insert_text.contains("Sales.Customers") && i.insert_text.contains("o.CustomerId")
            }),
            "should not re-suggest a join between two tables already present, got: {:?}",
            items.iter().map(|i| &i.insert_text).collect::<Vec<_>>()
        );
        assert!(
            items
                .iter()
                .any(|i| i.insert_text.contains("Sales.OrderLines")),
            "the OrderLines child join should still be offered"
        );
    }

    #[test]
    fn synthesized_alias_avoids_collision_with_existing() {
        // Existing alias "c" for Customers; the inbound Orders suggestion
        // must not reuse "c" (it gets "o"), and a suggestion whose table
        // also starts with "c" must fall back to a numbered alias.
        let mut catalog = sales_catalog();
        catalog.objects.push(CompletionObject {
            database: None,
            schema: Some("Sales".to_string()),
            name: "Coupons".to_string(),
            kind: ObjectKind::Table,
            columns: vec!["CouponId".into(), "CustomerId".into()],
            parameters: Vec::new(),
            foreign_keys: vec![FkRelation {
                column: "CustomerId".to_string(),
                target_schema: "Sales".to_string(),
                target_table: "Customers".to_string(),
                target_column: "CustomerId".to_string(),
                constraint: None,
            }],
        });
        let ctx = join_source_ctx(
            "SELECT * FROM Sales.Customers c JOIN ",
            vec![table_ref("Sales", "Customers", Some("c"))],
        );
        let items = join_candidates(&catalog, &ctx);
        let order_join = items
            .iter()
            .find(|i| i.insert_text.contains("Sales.Orders"))
            .expect("expected the inbound Orders suggestion");
        assert!(
            !order_join.insert_text.contains("Sales.Orders c "),
            "synthesized alias collided with existing alias c: {}",
            order_join.insert_text
        );
        let coupon_join = items
            .iter()
            .find(|i| i.insert_text.contains("Sales.Coupons"))
            .expect("expected the inbound Coupons suggestion");
        assert!(
            coupon_join
                .insert_text
                .starts_with("Sales.Coupons c2 ON c2."),
            "expected the colliding alias to get a numeric suffix, got: {}",
            coupon_join.insert_text
        );
    }

    #[test]
    fn unqualified_present_ref_resolves_and_uses_qualified_reference() {
        // `FROM Orders` (no schema, no alias): the ref resolves against the
        // catalog and the ON clause references the qualified name, mirroring
        // Swift's `ref.alias ?? object.qualifiedName`.
        let catalog = sales_catalog();
        let ctx = join_source_ctx(
            "SELECT * FROM Orders JOIN ",
            vec![TableRef {
                schema: None,
                name: "Orders".to_string(),
                alias: None,
            }],
        );
        let items = join_candidates(&catalog, &ctx);
        assert!(
            items
                .iter()
                .any(|i| i.insert_text
                    == "Sales.Customers c ON c.CustomerId = Sales.Orders.CustomerId"),
            "expected the present side to be referenced by qualified name, got: {:?}",
            items.iter().map(|i| &i.insert_text).collect::<Vec<_>>()
        );
    }

    #[test]
    fn join_source_also_offered_in_from_clause_intent() {
        // Swift adds FK joins for any table context; the FromSource intent
        // (whose policy already allows Join items) must yield them too.
        let catalog = sales_catalog();
        let sql = "SELECT * FROM Sales.Orders o, ";
        let ctx = context_with_intent(
            sql,
            sql.len(),
            IntelliSenseIntent::FromSource,
            IntelliSenseClause::From,
            vec![table_ref("Sales", "Orders", Some("o"))],
        );
        let items = join_candidates(&catalog, &ctx);
        assert!(
            items
                .iter()
                .any(|i| i.insert_text.contains("Sales.Customers")),
            "FK joins should be offered in FROM-source contexts too"
        );
    }

    #[test]
    fn join_source_scores_above_plain_table_base() {
        // FK joins rank above plain table candidates (base 80).
        let catalog = sales_catalog();
        let ctx = join_source_ctx(
            "SELECT * FROM Sales.Orders o JOIN ",
            vec![table_ref("Sales", "Orders", Some("o"))],
        );
        let items = join_candidates(&catalog, &ctx);
        assert!(!items.is_empty());
        assert!(
            items.iter().all(|i| i.score >= 95),
            "FK join scores must sit above the plain-table base, got: {:?}",
            items
                .iter()
                .map(|i| (&i.label, i.score))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn join_source_empty_target_schema_resolves_via_catalog() {
        // DDL-derived FKs can carry an empty target schema; the target must
        // resolve through the catalog so the suggestion is fully qualified.
        let mut catalog = sales_catalog();
        if let Some(orders) = catalog.objects.iter_mut().find(|o| o.name == "Orders") {
            orders.foreign_keys[0].target_schema = String::new();
        }
        let ctx = join_source_ctx(
            "SELECT * FROM Sales.Orders o JOIN ",
            vec![table_ref("Sales", "Orders", Some("o"))],
        );
        let items = join_candidates(&catalog, &ctx);
        assert!(
            items
                .iter()
                .any(|i| i.insert_text == "Sales.Customers c ON c.CustomerId = o.CustomerId"),
            "expected the unqualified FK target to resolve to Sales.Customers, got: {:?}",
            items.iter().map(|i| &i.insert_text).collect::<Vec<_>>()
        );
    }

    #[test]
    fn join_condition_suggests_on_clause_pair() {
        let catalog = catalog_with_fks();
        let ctx = context_with_intent(
            "SELECT * FROM dbo.Orders o JOIN dbo.Customers c ON ",
            52,
            IntelliSenseIntent::JoinCondition,
            IntelliSenseClause::On,
            vec![
                TableRef {
                    schema: Some("dbo".into()),
                    name: "Orders".into(),
                    alias: Some("o".into()),
                },
                TableRef {
                    schema: Some("dbo".into()),
                    name: "Customers".into(),
                    alias: Some("c".into()),
                },
            ],
        );
        let items = join_candidates(&catalog, &ctx);
        assert!(
            items
                .iter()
                // The joined table's alias leads (feedback ifb_13aa).
                .any(|i| i.label == "c.Id = o.CustomerId" && i.kind == CompletionKind::Join),
            "expected ON clause join condition, got: {:?}",
            items.iter().map(|i| &i.label).collect::<Vec<_>>()
        );
    }

    #[test]
    fn join_condition_no_candidates_for_unrelated_tables() {
        let catalog = catalog_with_fks();
        let ctx = context_with_intent(
            "SELECT * FROM dbo.Products p JOIN dbo.Customers c ON ",
            52,
            IntelliSenseIntent::JoinCondition,
            IntelliSenseClause::On,
            vec![
                TableRef {
                    schema: Some("dbo".into()),
                    name: "Products".into(),
                    alias: Some("p".into()),
                },
                TableRef {
                    schema: Some("dbo".into()),
                    name: "Customers".into(),
                    alias: Some("c".into()),
                },
            ],
        );
        let items = join_candidates(&catalog, &ctx);
        assert!(
            items.is_empty(),
            "no FK between Products and Customers, expected no join conditions"
        );
    }

    #[test]
    fn join_source_filters_by_prefix() {
        let catalog = catalog_with_fks();
        let ctx = context_with_intent(
            "SELECT * FROM dbo.Orders o JOIN C",
            34,
            IntelliSenseIntent::JoinSource,
            IntelliSenseClause::Join,
            vec![TableRef {
                schema: Some("dbo".into()),
                name: "Orders".into(),
                alias: Some("o".into()),
            }],
        );
        let items = join_candidates(&catalog, &ctx);
        assert!(items.iter().all(|i| i.label.to_lowercase().contains("c")));
        assert!(
            items
                .iter()
                .any(|i| i.label.starts_with("dbo.Customers ON")),
            "prefix C should match the Customers join via its qualified name, got: {:?}",
            items.iter().map(|i| &i.label).collect::<Vec<_>>()
        );
    }

    #[test]
    fn join_source_no_candidates_when_no_tables_in_query() {
        let catalog = catalog_with_fks();
        let ctx = context_with_intent(
            "JOIN ",
            5,
            IntelliSenseIntent::JoinSource,
            IntelliSenseClause::Join,
            vec![],
        );
        let items = join_candidates(&catalog, &ctx);
        assert!(items.is_empty(), "no tables in query, no FK candidates");
    }

    #[test]
    fn join_condition_no_candidates_with_single_table() {
        let catalog = catalog_with_fks();
        let ctx = context_with_intent(
            "SELECT * FROM dbo.Orders o ON ",
            30,
            IntelliSenseIntent::JoinCondition,
            IntelliSenseClause::On,
            vec![TableRef {
                schema: Some("dbo".into()),
                name: "Orders".into(),
                alias: Some("o".into()),
            }],
        );
        let items = join_candidates(&catalog, &ctx);
        assert!(items.is_empty(), "need 2+ tables for join condition");
    }

    #[test]
    fn non_join_intent_returns_empty() {
        let catalog = catalog_with_fks();
        let ctx = context_with_intent(
            "SELECT * FROM dbo.Orders o WHERE ",
            31,
            IntelliSenseIntent::WherePredicate,
            IntelliSenseClause::Where,
            vec![TableRef {
                schema: Some("dbo".into()),
                name: "Orders".into(),
                alias: Some("o".into()),
            }],
        );
        let items = join_candidates(&catalog, &ctx);
        assert!(items.is_empty(), "non-join intent should return empty");
    }

    // ---- Composite foreign keys ------------------------------------------

    /// dbo.OrderLines has a composite FK (TenantId, OrderNo) ->
    /// dbo.Orders (TenantId, OrderNo), declared with the key columns in that
    /// order, plus a separate single-column FK ProductId -> dbo.Products.Id.
    fn composite_catalog() -> CompletionCatalog {
        let pair = |column: &str, table: &str, target: &str, constraint: &str| FkRelation {
            column: column.to_string(),
            target_schema: "dbo".to_string(),
            target_table: table.to_string(),
            target_column: target.to_string(),
            constraint: Some(constraint.to_string()),
        };
        let mut lines = CompletionObject::table(
            "dbo",
            "OrderLines",
            vec![
                "TenantId".into(),
                "OrderNo".into(),
                "LineNo".into(),
                "ProductId".into(),
            ],
        );
        lines.foreign_keys = vec![
            pair("TenantId", "Orders", "TenantId", "FK_Lines_Orders"),
            pair("OrderNo", "Orders", "OrderNo", "FK_Lines_Orders"),
            pair("ProductId", "Products", "Id", "FK_Lines_Products"),
        ];
        CompletionCatalog {
            databases: Vec::new(),
            schemas: vec!["dbo".to_string()],
            objects: vec![
                lines,
                CompletionObject::table("dbo", "Orders", vec!["TenantId".into(), "OrderNo".into()]),
                CompletionObject::table("dbo", "Products", vec!["Id".into()]),
            ],
            filesystem_schemas: Vec::new(),
            snippets: Vec::new(),
        }
    }

    #[test]
    fn composite_fk_outbound_is_one_suggestion_joined_with_and() {
        let catalog = composite_catalog();
        let sql = "SELECT * FROM dbo.OrderLines ol JOIN ";
        let ctx = join_source_ctx(sql, vec![table_ref("dbo", "OrderLines", Some("ol"))]);
        let items = join_candidates(&catalog, &ctx);
        let orders: Vec<_> = items
            .iter()
            .filter(|i| i.label.starts_with("dbo.Orders"))
            .collect();
        assert_eq!(orders.len(), 1, "one suggestion per FK, got {items:#?}");
        assert_eq!(
            orders[0].insert_text,
            "dbo.Orders o ON o.TenantId = ol.TenantId AND o.OrderNo = ol.OrderNo"
        );
        assert_eq!(
            orders[0].label,
            "dbo.Orders ON TenantId = ol.TenantId AND OrderNo = ol.OrderNo"
        );
        // The single-column FK on the same table is unchanged.
        let products: Vec<_> = items
            .iter()
            .filter(|i| i.label.starts_with("dbo.Products"))
            .collect();
        assert_eq!(products.len(), 1);
        assert_eq!(
            products[0].insert_text,
            "dbo.Products p ON p.Id = ol.ProductId"
        );
    }

    #[test]
    fn composite_fk_inbound_is_one_suggestion_joined_with_and() {
        let catalog = composite_catalog();
        let sql = "SELECT * FROM dbo.Orders o JOIN ";
        let ctx = join_source_ctx(sql, vec![table_ref("dbo", "Orders", Some("o"))]);
        let items = join_candidates(&catalog, &ctx);
        assert_eq!(items.len(), 1, "got {items:#?}");
        assert_eq!(
            items[0].insert_text,
            "dbo.OrderLines o2 ON o2.TenantId = o.TenantId AND o2.OrderNo = o.OrderNo"
        );
    }

    #[test]
    fn composite_fk_pairs_follow_key_column_order() {
        // Declared (OrderNo, TenantId): the ON clause keeps that order.
        let mut catalog = composite_catalog();
        let lines = &mut catalog.objects[0];
        lines.foreign_keys.swap(0, 1);
        let sql = "SELECT * FROM dbo.OrderLines ol JOIN ";
        let ctx = join_source_ctx(sql, vec![table_ref("dbo", "OrderLines", Some("ol"))]);
        let items = join_candidates(&catalog, &ctx);
        let orders = items
            .iter()
            .find(|i| i.label.starts_with("dbo.Orders"))
            .expect("orders join");
        assert_eq!(
            orders.insert_text,
            "dbo.Orders o ON o.OrderNo = ol.OrderNo AND o.TenantId = ol.TenantId"
        );
    }

    #[test]
    fn separate_single_column_fks_to_one_table_stay_separate() {
        // Billing and shipping address FKs both target Addresses.Id but are
        // two constraints: two suggestions, each on its own column.
        let fk = |column: &str, constraint: Option<&str>| FkRelation {
            column: column.to_string(),
            target_schema: "dbo".to_string(),
            target_table: "Addresses".to_string(),
            target_column: "Id".to_string(),
            constraint: constraint.map(str::to_string),
        };
        for (billing, shipping) in [(Some("FK_Billing"), Some("FK_Shipping")), (None, None)] {
            let mut orders = CompletionObject::table(
                "dbo",
                "Orders",
                vec!["BillingAddressId".into(), "ShippingAddressId".into()],
            );
            orders.foreign_keys = vec![
                fk("BillingAddressId", billing),
                fk("ShippingAddressId", shipping),
            ];
            let catalog = CompletionCatalog {
                databases: Vec::new(),
                schemas: vec!["dbo".to_string()],
                objects: vec![
                    orders,
                    CompletionObject::table("dbo", "Addresses", vec!["Id".into()]),
                ],
                filesystem_schemas: Vec::new(),
                snippets: Vec::new(),
            };
            let sql = "SELECT * FROM dbo.Orders o JOIN ";
            let ctx = join_source_ctx(sql, vec![table_ref("dbo", "Orders", Some("o"))]);
            let inserts: Vec<_> = join_candidates(&catalog, &ctx)
                .into_iter()
                .map(|i| i.insert_text)
                .collect();
            assert_eq!(
                inserts,
                vec![
                    "dbo.Addresses a ON a.Id = o.BillingAddressId",
                    "dbo.Addresses a2 ON a2.Id = o.ShippingAddressId",
                ]
            );
        }
    }

    #[test]
    fn composite_fk_join_condition_is_one_and_suggestion() {
        let catalog = composite_catalog();
        let sql = "SELECT * FROM dbo.OrderLines ol JOIN dbo.Orders o ON ";
        let ctx = context_with_intent(
            sql,
            sql.len(),
            IntelliSenseIntent::JoinCondition,
            IntelliSenseClause::On,
            vec![
                table_ref("dbo", "OrderLines", Some("ol")),
                table_ref("dbo", "Orders", Some("o")),
            ],
        );
        let items = join_candidates(&catalog, &ctx);
        assert_eq!(items.len(), 1, "got {items:#?}");
        // The joined table's alias leads (feedback ifb_13aa).
        assert_eq!(
            items[0].insert_text,
            "o.TenantId = ol.TenantId AND o.OrderNo = ol.OrderNo"
        );
        assert_eq!(
            items[0].detail,
            "FK: ol.TenantId, ol.OrderNo -> o.TenantId, o.OrderNo"
        );
    }

    #[test]
    fn composite_fk_from_model_snapshot_joins_all_columns() {
        // End to end through the real catalog bridge: an unnamed composite
        // FK parsed from DDL keeps its pairs together.
        use sqlly_lang::sqlmodel::{
            ColumnDef, Dialect, Fidelity, ForeignKeyDef, IncompleteRegions, ModelSnapshot,
            ObjectBody, ObjectFragment, ObjectKind as ModelKind, ObjectRef, Provenance, SqlName,
            TableBody,
        };
        use std::collections::BTreeSet;
        let name = |t: &str| SqlName::new(t, Dialect::SqlServer, false);
        let column = |t: &str| ColumnDef {
            name: name(t),
            ty: None,
            nullable: None,
            default: None,
            identity: false,
            generated: None,
            collation: None,
            ordinal: 0,
        };
        let table = |t: &str, fks: Vec<ForeignKeyDef>| ObjectFragment {
            reference: ObjectRef::new(None, Some(name("dbo")), name(t)),
            kind: ModelKind::Table,
            dialect: Dialect::SqlServer,
            body: ObjectBody::Table(TableBody {
                columns: vec![column("TenantId"), column("OrderNo")],
                foreign_keys: fks,
                ..TableBody::default()
            }),
            provenance: Provenance::Database {
                profile: "test".into(),
            },
            fidelity: Fidelity::Exact,
            references: Vec::new(),
        };
        let fk = ForeignKeyDef {
            name: None,
            columns: vec![name("TenantId"), name("OrderNo")],
            target: ObjectRef::new(None, Some(name("dbo")), name("Orders")),
            target_columns: vec![name("TenantId"), name("OrderNo")],
        };
        let (snapshot, _) = ModelSnapshot::empty().apply(
            1,
            vec![table("OrderLines", vec![fk]), table("Orders", Vec::new())],
            &BTreeSet::new(),
            IncompleteRegions::default(),
        );
        let catalog = CompletionCatalog::from_model_snapshot(&snapshot);
        let sql = "SELECT * FROM dbo.OrderLines ol JOIN ";
        let ctx = join_source_ctx(sql, vec![table_ref("dbo", "OrderLines", Some("ol"))]);
        let inserts: Vec<_> = join_candidates(&catalog, &ctx)
            .into_iter()
            .map(|i| i.insert_text)
            .collect();
        assert_eq!(
            inserts,
            vec!["dbo.Orders o ON o.TenantId = ol.TenantId AND o.OrderNo = ol.OrderNo"]
        );
    }
}
