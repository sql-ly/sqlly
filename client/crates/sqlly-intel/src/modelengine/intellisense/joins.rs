//! FK-based join suggestions: for tables already present in the query's
//! FROM/JOIN sources, suggest foreign-key-connected tables with a ready-to-use
//! ON clause and a collision-free alias.

use super::fuzzy;
use super::types::{CompletionItem, CompletionItemKind, CompletionRequest};
use sqlly_lang::sqlmodel::{ModelSnapshot, ObjectKind, Span};
use sqlly_lang::sqlparse::UseKind;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

/// Score boost added to FK-based join suggestions so they rank above plain
/// table-name completions in the list. The Swift re-scorer adds a further
/// boost for `detail == "fk"`, so this only affects ordering within the Rust
/// response before it crosses the wire.
const FK_JOIN_BOOST: i32 = 100;

/// Generate a short, collision-free alias for a candidate table: the first
/// letter (lowercased), optionally suffixed with a counter.
fn join_alias(table_name: &str, used: &HashSet<String>) -> String {
    let base = table_name
        .chars()
        .next()
        .map(|c| c.to_ascii_lowercase().to_string())
        .unwrap_or_else(|| "t".to_string());
    if !used.contains(&base) {
        return base;
    }
    let mut n = 2;
    loop {
        let candidate = format!("{base}{n}");
        if !used.contains(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

/// Render the equality pairs for a join ON clause. When `new_qualifier` is
/// empty the left-hand columns are bare (used for display labels).
fn join_conditions(
    new_qualifier: &str,
    new_cols: &[&str],
    existing_qualifier: &str,
    existing_cols: &[&str],
) -> String {
    new_cols
        .iter()
        .zip(existing_cols)
        .map(|(nc, ec)| {
            if new_qualifier.is_empty() {
                format!("{nc} = {existing_qualifier}.{ec}")
            } else {
                format!("{new_qualifier}.{nc} = {existing_qualifier}.{ec}")
            }
        })
        .collect::<Vec<_>>()
        .join(" AND ")
}

/// FK-based join candidates: for each table already present in the query's
/// FROM/JOIN sources, suggest tables connected by a foreign key — outgoing
/// (the present table has an FK to the candidate) and incoming (the candidate
/// has an FK to the present table). Each suggestion includes a ready-to-use
/// ON clause and a collision-free alias. Items carry `detail == "fk"` so the
/// client can boost them above plain table names.
pub(crate) fn fk_join_candidates(
    req: &CompletionRequest,
    snapshot: &ModelSnapshot,
    prefix: &str,
    replace: Span,
) -> Vec<(CompletionItem, i32)> {
    let tree = sqlly_lang::sqlparse::parse(&req.text, req.dialect);
    // Only the cursor's own statement contributes join sources: a fresh
    // statement (after `;` or `GO`) is a fresh FROM context.
    let cursor_stmt =
        super::scope::cursor_statement_index(&tree, &req.text, req.cursor, req.dialect);

    // Collect present tables: fold_key → qualifier (alias or display name).
    let mut qualifiers: HashMap<String, String> = HashMap::new();
    let mut present_keys: HashSet<String> = HashSet::new();
    let mut used_aliases: HashSet<String> = HashSet::new();

    for u in &tree.references {
        if u.use_kind != UseKind::TableSource {
            continue;
        }
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
            .find(|e| e.kind == ObjectKind::Table)
        else {
            continue;
        };
        let fold_key = entry.reference.fold_key();
        present_keys.insert(fold_key.clone());
        let qualifier = u
            .via_alias
            .as_deref()
            .filter(|a| !a.is_empty())
            .map(|a| a.to_string())
            .unwrap_or_else(|| entry.reference.display());
        qualifiers.insert(fold_key, qualifier);
        if let Some(a) = u.via_alias.as_deref().filter(|a| !a.is_empty()) {
            used_aliases.insert(a.to_ascii_lowercase());
        }
    }

    if present_keys.is_empty() {
        return Vec::new();
    }

    let mut seen_targets: HashSet<String> = HashSet::new();
    let mut candidates: Vec<(CompletionItem, i32)> = Vec::new();

    for entry in snapshot.objects() {
        if entry.kind != ObjectKind::Table {
            continue;
        }
        let Some(table) = entry.table() else {
            continue;
        };
        let entry_key = entry.reference.fold_key();
        let is_present = present_keys.contains(&entry_key);

        // --- Outgoing FKs: this present table → candidate target. ---
        if is_present {
            let Some(pt_qualifier) = qualifiers.get(&entry_key) else {
                continue;
            };
            for fk in &table.foreign_keys {
                let target_key = fk.target.fold_key();
                if present_keys.contains(&target_key) || !seen_targets.insert(target_key) {
                    continue;
                }
                let Some(target_entry) = snapshot
                    .resolve(
                        &fk.target,
                        req.default_catalog.as_ref(),
                        req.default_namespace.as_ref(),
                    )
                    .into_iter()
                    .find(|e| e.kind == ObjectKind::Table)
                else {
                    continue;
                };
                let target_display = target_entry.reference.display();
                let Some(score) = fuzzy::score(prefix, target_entry.reference.name.spelling())
                else {
                    continue;
                };
                let alias = join_alias(target_entry.reference.name.spelling(), &used_aliases);
                used_aliases.insert(alias.clone());
                let new_cols: Vec<&str> = fk.target_columns.iter().map(|c| c.spelling()).collect();
                let existing_cols: Vec<&str> = fk.columns.iter().map(|c| c.spelling()).collect();
                let conds = join_conditions(&alias, &new_cols, pt_qualifier, &existing_cols);
                let label_conds = join_conditions("", &new_cols, pt_qualifier, &existing_cols);
                candidates.push((
                    {
                        let mut item = CompletionItem::symbol(
                            format!("{target_display} ON {label_conds}"),
                            format!("{target_display} {alias} ON {conds}"),
                            CompletionItemKind::Table,
                        );
                        item.detail = Some("fk".into());
                        item.replace_span = Some(replace);
                        item.tentative = !target_entry.fidelity.is_exact();
                        item.name = target_entry.reference.name.spelling().to_string();
                        item.schema = target_entry
                            .reference
                            .namespace
                            .as_ref()
                            .map(|n| n.spelling().to_string());
                        item.alias = Some(alias.clone());
                        item.why = Some(Cow::Borrowed("fk"));
                        item.match_text = Some(target_display);
                        item
                    },
                    score + FK_JOIN_BOOST,
                ));
            }
        }

        // --- Incoming FKs: this candidate → a present table. ---
        if !is_present {
            for fk in &table.foreign_keys {
                let target_key = fk.target.fold_key();
                if !present_keys.contains(&target_key) {
                    continue;
                }
                if !seen_targets.insert(entry_key.clone()) {
                    continue;
                }
                let Some(pt_qualifier) = qualifiers.get(&target_key) else {
                    continue;
                };
                let owner_display = entry.reference.display();
                let Some(score) = fuzzy::score(prefix, entry.reference.name.spelling()) else {
                    continue;
                };
                let alias = join_alias(entry.reference.name.spelling(), &used_aliases);
                used_aliases.insert(alias.clone());
                let new_cols: Vec<&str> = fk.columns.iter().map(|c| c.spelling()).collect();
                let existing_cols: Vec<&str> =
                    fk.target_columns.iter().map(|c| c.spelling()).collect();
                let conds = join_conditions(&alias, &new_cols, pt_qualifier, &existing_cols);
                let label_conds = join_conditions("", &new_cols, pt_qualifier, &existing_cols);
                candidates.push((
                    {
                        let mut item = CompletionItem::symbol(
                            format!("{owner_display} ON {label_conds}"),
                            format!("{owner_display} {alias} ON {conds}"),
                            CompletionItemKind::Table,
                        );
                        item.detail = Some("fk".into());
                        item.replace_span = Some(replace);
                        item.tentative = !entry.fidelity.is_exact();
                        item.name = entry.reference.name.spelling().to_string();
                        item.schema = entry
                            .reference
                            .namespace
                            .as_ref()
                            .map(|n| n.spelling().to_string());
                        item.alias = Some(alias.clone());
                        item.why = Some(Cow::Borrowed("fk"));
                        item.match_text = Some(owner_display);
                        item
                    },
                    score + FK_JOIN_BOOST,
                ));
                break;
            }
        }
    }

    candidates
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modelengine::intellisense::{CompletionMode, complete_symbols};
    use sqlly_lang::sqlmodel::{
        ColumnDef, Dialect, Fidelity, ForeignKeyDef, IncompleteRegions, ObjectBody, ObjectFragment,
        ObjectKind, ObjectRef, Provenance, SqlName, TableBody,
    };
    use std::collections::BTreeSet;

    fn name(text: &str) -> SqlName {
        SqlName::new(text, Dialect::SqlServer, false)
    }

    fn ref_of(schema: &str, table: &str) -> ObjectRef {
        ObjectRef::new(None, Some(name(schema)), name(table))
    }

    fn column(text: &str) -> ColumnDef {
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

    fn fk(cols: &[&str], tgt_schema: &str, tgt_table: &str, tgt_cols: &[&str]) -> ForeignKeyDef {
        ForeignKeyDef {
            name: None,
            columns: cols.iter().map(|c| name(c)).collect(),
            target: ref_of(tgt_schema, tgt_table),
            target_columns: tgt_cols.iter().map(|c| name(c)).collect(),
        }
    }

    fn table_fragment(schema: &str, table: &str, fks: Vec<ForeignKeyDef>) -> ObjectFragment {
        ObjectFragment {
            reference: ref_of(schema, table),
            kind: ObjectKind::Table,
            dialect: Dialect::SqlServer,
            body: ObjectBody::Table(TableBody {
                columns: vec![column("Id")],
                foreign_keys: fks,
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

    fn fk_items(items: &[CompletionItem]) -> Vec<&CompletionItem> {
        items
            .iter()
            .filter(|i| i.detail.as_deref() == Some("fk"))
            .collect()
    }

    // --- outgoing FK ------------------------------------------------------

    #[test]
    fn outgoing_fk_suggests_target_with_on_clause() {
        let snap = snapshot(vec![
            table_fragment(
                "dbo",
                "JournalLine",
                vec![fk(&["BatchId"], "dbo", "Batch", &["Id"])],
            ),
            table_fragment("dbo", "Batch", vec![]),
        ]);
        let items = complete("SELECT * FROM dbo.JournalLine jl INNER JOIN ", &snap);
        let fks = fk_items(&items);
        assert_eq!(fks.len(), 1, "exactly one FK candidate");
        let item = fks[0];
        assert!(
            item.insert_text.contains("dbo.Batch"),
            "insert should name target table: {}",
            item.insert_text
        );
        assert!(
            item.insert_text.contains("ON b.Id = jl.BatchId"),
            "insert should have ON clause with alias + existing qualifier: {}",
            item.insert_text
        );
        assert_eq!(item.detail.as_deref(), Some("fk"));
        assert_eq!(item.match_text.as_deref(), Some("dbo.Batch"));
    }

    #[test]
    fn outgoing_fk_without_alias_uses_qualified_name() {
        let snap = snapshot(vec![
            table_fragment(
                "dbo",
                "JournalLine",
                vec![fk(&["BatchId"], "dbo", "Batch", &["Id"])],
            ),
            table_fragment("dbo", "Batch", vec![]),
        ]);
        let items = complete("SELECT * FROM dbo.JournalLine INNER JOIN ", &snap);
        let fks = fk_items(&items);
        assert_eq!(fks.len(), 1);
        assert!(
            fks[0]
                .insert_text
                .contains("ON b.Id = dbo.JournalLine.BatchId"),
            "no alias → qualified name on RHS: {}",
            fks[0].insert_text
        );
    }

    // --- incoming FK ------------------------------------------------------

    #[test]
    fn incoming_fk_suggests_child_table() {
        let snap = snapshot(vec![
            table_fragment("dbo", "JournalLine", vec![]),
            table_fragment(
                "dbo",
                "JournalDetail",
                vec![fk(&["JournalLineId"], "dbo", "JournalLine", &["Id"])],
            ),
        ]);
        let items = complete("SELECT * FROM dbo.JournalLine jl INNER JOIN ", &snap);
        let fks = fk_items(&items);
        assert_eq!(fks.len(), 1, "exactly one incoming FK candidate");
        let item = fks[0];
        assert!(
            item.insert_text.contains("dbo.JournalDetail"),
            "should suggest child table: {}",
            item.insert_text
        );
        assert!(
            item.insert_text.contains("ON j.JournalLineId = jl.Id"),
            "ON clause should join child FK col to parent PK: {}",
            item.insert_text
        );
    }

    // --- both directions --------------------------------------------------

    #[test]
    fn both_outgoing_and_incoming_in_one_list() {
        let snap = snapshot(vec![
            table_fragment(
                "dbo",
                "Middle",
                vec![fk(&["ParentId"], "dbo", "Parent", &["Id"])],
            ),
            table_fragment("dbo", "Parent", vec![]),
            table_fragment(
                "dbo",
                "Child",
                vec![fk(&["MiddleId"], "dbo", "Middle", &["Id"])],
            ),
        ]);
        let items = complete("SELECT * FROM dbo.Middle m INNER JOIN ", &snap);
        let fks = fk_items(&items);
        assert_eq!(fks.len(), 2, "one outgoing + one incoming");
        let labels: Vec<&str> = fks.iter().map(|i| i.label.as_str()).collect();
        assert!(
            labels.iter().any(|l| l.starts_with("dbo.Parent")),
            "outgoing: {labels:?}"
        );
        assert!(
            labels.iter().any(|l| l.starts_with("dbo.Child")),
            "incoming: {labels:?}"
        );
    }

    // --- dedup / skip -----------------------------------------------------

    #[test]
    fn skips_table_already_in_query() {
        let snap = snapshot(vec![
            table_fragment("dbo", "A", vec![fk(&["BId"], "dbo", "B", &["Id"])]),
            table_fragment("dbo", "B", vec![fk(&["AId"], "dbo", "A", &["Id"])]),
        ]);
        // Both A and B are already in the query → no FK candidates.
        let items = complete(
            "SELECT * FROM dbo.A a INNER JOIN dbo.B b ON b.Id = a.BId INNER JOIN ",
            &snap,
        );
        assert!(
            fk_items(&items).is_empty(),
            "no candidates when targets already present"
        );
    }

    #[test]
    fn dedups_multiple_fks_to_same_target() {
        let snap = snapshot(vec![
            table_fragment(
                "dbo",
                "A",
                vec![
                    fk(&["BId1"], "dbo", "B", &["Id1"]),
                    fk(&["BId2"], "dbo", "B", &["Id2"]),
                ],
            ),
            table_fragment("dbo", "B", vec![]),
        ]);
        let items = complete("SELECT * FROM dbo.A a INNER JOIN ", &snap);
        assert_eq!(fk_items(&items).len(), 1, "one suggestion per target table");
    }

    // --- prefix filter ----------------------------------------------------

    #[test]
    fn prefix_filter_excludes_non_matching() {
        let snap = snapshot(vec![
            table_fragment("dbo", "A", vec![fk(&["BId"], "dbo", "B", &["Id"])]),
            table_fragment("dbo", "B", vec![]),
        ]);
        let items = complete("SELECT * FROM dbo.A a INNER JOIN Z", &snap);
        assert!(fk_items(&items).is_empty(), "prefix 'Z' should exclude 'B'");
    }

    #[test]
    fn empty_prefix_matches_all_fk_candidates() {
        let snap = snapshot(vec![
            table_fragment("dbo", "A", vec![fk(&["BId"], "dbo", "B", &["Id"])]),
            table_fragment("dbo", "B", vec![]),
        ]);
        let items = complete("SELECT * FROM dbo.A a INNER JOIN ", &snap);
        assert_eq!(fk_items(&items).len(), 1);
    }

    // --- guard: no present tables ----------------------------------------

    #[test]
    fn no_candidates_without_join_context() {
        let snap = snapshot(vec![
            table_fragment("dbo", "A", vec![fk(&["BId"], "dbo", "B", &["Id"])]),
            table_fragment("dbo", "B", vec![]),
        ]);
        // Bare FROM with single table → no JOIN yet → still ObjectName context,
        // but only one table present so FK target B is a valid join candidate.
        // A query with no table sources at all:
        let items = complete("SELECT * FROM ", &snap);
        let fks = fk_items(&items);
        assert!(
            fks.is_empty(),
            "no FK candidates when no tables are present yet"
        );
    }

    // --- alias collision --------------------------------------------------

    #[test]
    fn alias_collision_appends_counter() {
        let snap = snapshot(vec![
            table_fragment(
                "dbo",
                "Order",
                vec![
                    fk(&["CustomerId"], "dbo", "Customer", &["Id"]),
                    fk(&["CompanyId"], "dbo", "Company", &["Id"]),
                ],
            ),
            table_fragment("dbo", "Customer", vec![]),
            table_fragment("dbo", "Company", vec![]),
        ]);
        let items = complete("SELECT * FROM dbo.[Order] o INNER JOIN ", &snap);
        let fks = fk_items(&items);
        assert_eq!(fks.len(), 2);
        // Both Customer and Company start with 'C' → second alias should be 'c2'.
        let aliases: HashSet<String> = fks
            .iter()
            .filter_map(|i| {
                let parts: Vec<&str> = i.insert_text.split_whitespace().collect();
                parts.get(1).map(|s| s.to_string())
            })
            .collect();
        assert!(
            aliases.contains("c") && aliases.contains("c2"),
            "collision-safe aliases: {aliases:?}"
        );
    }

    // --- multi-column FK --------------------------------------------------

    #[test]
    fn multi_column_fk_generates_and_clause() {
        let snap = snapshot(vec![
            table_fragment("dbo", "A", vec![fk(&["X", "Y"], "dbo", "B", &["P", "Q"])]),
            table_fragment("dbo", "B", vec![]),
        ]);
        let items = complete("SELECT * FROM dbo.A a INNER JOIN ", &snap);
        let fks = fk_items(&items);
        assert_eq!(fks.len(), 1);
        assert!(
            fks[0].insert_text.contains("AND"),
            "multi-column FK should produce AND: {}",
            fks[0].insert_text
        );
        assert!(
            fks[0].insert_text.contains("b.P = a.X") && fks[0].insert_text.contains("b.Q = a.Y"),
            "both pairs present: {}",
            fks[0].insert_text
        );
    }
}
