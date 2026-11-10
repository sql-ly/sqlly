//! Draft-vs-snapshot diff engine.

use std::collections::{BTreeMap, BTreeSet};

use crate::alter::snapshot::{ColumnDef, ForeignKeyDef, IndexDef, TableDraft, TableSnapshot};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRef {
    pub schema: Option<String>,
    pub table: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeSet {
    pub table: TableRef,
    pub changes: Vec<Change>,
    pub manual_review: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    AddColumn(ColumnDef),
    DropColumn {
        name: String,
    },
    RenameColumn {
        from: String,
        to: String,
    },
    AlterColumnType {
        name: String,
        from: ColumnDef,
        to: ColumnDef,
    },
    SetDefault {
        name: String,
        from: Option<String>,
        to: Option<String>,
        constraint_name: Option<String>,
    },
    DropIndex {
        name: String,
    },
    AddIndex(IndexDef),
    DropForeignKey {
        name: String,
    },
    AddForeignKey(ForeignKeyDef),
    ReplacePrimaryKey {
        from: Vec<String>,
        to: Vec<String>,
        constraint_name: Option<String>,
    },
    /// A description (comment) change: `column: None` addresses the table
    /// itself. `from` is the live value (`None` = unset), `to` the staged one
    /// (`None` = clear).
    SetComment {
        column: Option<String>,
        from: Option<String>,
        to: Option<String>,
    },
}

pub fn diff(snapshot: &TableSnapshot, draft: &TableDraft) -> ChangeSet {
    let mut changes = Vec::new();
    let mut manual_review = Vec::new();

    let snapshot_by_name: BTreeMap<&str, &ColumnDef> = snapshot
        .columns
        .iter()
        .map(|c| (c.name.as_str(), c))
        .collect();

    let mut referenced_origins: BTreeSet<String> = BTreeSet::new();
    for col in &draft.columns {
        if let Some(origin) = col.origin.as_deref() {
            referenced_origins.insert(origin.to_string());
        }
    }

    for snap_col in &snapshot.columns {
        if !referenced_origins.contains(&snap_col.name) {
            changes.push(Change::DropColumn {
                name: snap_col.name.clone(),
            });
        }
    }

    for draft_col in &draft.columns {
        match draft_col.origin.as_deref() {
            None => {
                changes.push(Change::AddColumn(draft_col.def.clone()));
            }
            Some(origin) => {
                let snap = snapshot_by_name.get(origin).cloned();
                if snap.is_none() {
                    changes.push(Change::AddColumn(draft_col.def.clone()));
                    continue;
                }
                let from = snap.unwrap().clone();
                let to = draft_col.def.clone();

                if from.name != to.name {
                    changes.push(Change::RenameColumn {
                        from: from.name.clone(),
                        to: to.name.clone(),
                    });
                }

                if from.identity != to.identity {
                    manual_review.push(format!(
                        "Identity/AUTO_INCREMENT cannot be changed in place on column {}",
                        to.name
                    ));
                }

                let type_changed = !types_equal(&from.data_type, &to.data_type);
                let null_changed = from.nullable != to.nullable;
                if type_changed || null_changed {
                    changes.push(Change::AlterColumnType {
                        name: to.name.clone(),
                        from: from.clone(),
                        to: to.clone(),
                    });
                }

                if from.default != to.default {
                    changes.push(Change::SetDefault {
                        name: to.name.clone(),
                        from: from.default.clone(),
                        to: to.default.clone(),
                        constraint_name: from.default_constraint_name.clone(),
                    });
                }
            }
        }
    }

    diff_indexes(snapshot, draft, &mut changes);
    diff_foreign_keys(snapshot, draft, &mut changes);
    diff_primary_key(snapshot, draft, &mut changes);
    diff_comments(snapshot, draft, &mut changes);

    changes.sort_by_key(order_key);
    ChangeSet {
        table: TableRef {
            schema: draft.schema.clone(),
            table: draft.table.clone(),
        },
        changes,
        manual_review,
    }
}

fn diff_indexes(snapshot: &TableSnapshot, draft: &TableDraft, changes: &mut Vec<Change>) {
    let snap_map: BTreeMap<&str, &IndexDef> = snapshot
        .indexes
        .iter()
        .map(|i| (i.name.as_str(), i))
        .collect();
    let draft_map: BTreeMap<&str, &IndexDef> =
        draft.indexes.iter().map(|i| (i.name.as_str(), i)).collect();

    for (name, snap) in &snap_map {
        match draft_map.get(name) {
            Some(draft_idx) if draft_idx == snap => {}
            Some(_) | None => changes.push(Change::DropIndex {
                name: name.to_string(),
            }),
        }
    }
    for (name, draft_idx) in &draft_map {
        match snap_map.get(name) {
            Some(snap) if snap == draft_idx => {}
            Some(_) | None => changes.push(Change::AddIndex((*draft_idx).clone())),
        }
    }
}

fn diff_foreign_keys(snapshot: &TableSnapshot, draft: &TableDraft, changes: &mut Vec<Change>) {
    let snap_map: BTreeMap<&str, &ForeignKeyDef> = snapshot
        .foreign_keys
        .iter()
        .map(|f| (f.name.as_str(), f))
        .collect();
    let draft_map: BTreeMap<&str, &ForeignKeyDef> = draft
        .foreign_keys
        .iter()
        .map(|f| (f.name.as_str(), f))
        .collect();

    for (name, snap) in &snap_map {
        match draft_map.get(name) {
            Some(draft_fk) if fk_same_for_diff(snap, draft_fk) => {}
            Some(_) | None => changes.push(Change::DropForeignKey {
                name: name.to_string(),
            }),
        }
    }
    for (name, draft_fk) in &draft_map {
        match snap_map.get(name) {
            Some(snap) if fk_same_for_diff(snap, draft_fk) => {}
            Some(_) | None => changes.push(Change::AddForeignKey((*draft_fk).clone())),
        }
    }
}

fn fk_same_for_diff(a: &ForeignKeyDef, b: &ForeignKeyDef) -> bool {
    a.columns == b.columns
        && a.ref_schema == b.ref_schema
        && a.ref_table == b.ref_table
        && a.ref_columns == b.ref_columns
        && (a.on_delete == b.on_delete || a.on_delete.is_none() || b.on_delete.is_none())
        && (a.on_update == b.on_update || a.on_update.is_none() || b.on_update.is_none())
}

fn diff_primary_key(snapshot: &TableSnapshot, draft: &TableDraft, changes: &mut Vec<Change>) {
    if snapshot.primary_key != draft.primary_key {
        changes.push(Change::ReplacePrimaryKey {
            from: snapshot.primary_key.clone(),
            to: draft.primary_key.clone(),
            constraint_name: snapshot.pk_constraint_name.clone(),
        });
    }
}

/// Description changes. Comments hang off the column's identity, not its
/// name, so a rename needs no statement — the diff keys each draft column's
/// comment on its origin and emits against the new name only when the text
/// actually changed. A dropped column takes its comment with it, so only
/// surviving draft columns are compared.
fn diff_comments(snapshot: &TableSnapshot, draft: &TableDraft, changes: &mut Vec<Change>) {
    // SQL Server can hold an MS_Description whose value is '' — the snapshot
    // keeps it as `Some("")` so a later edit updates (not re-adds) the
    // property, but it reads as "no description" for change detection.
    let same = |from: Option<&String>, to: Option<&String>| {
        from.filter(|t| !t.is_empty()) == to.filter(|t| !t.is_empty())
    };
    if !same(
        snapshot.table_comment.as_ref(),
        draft.table_comment.as_ref(),
    ) {
        changes.push(Change::SetComment {
            column: None,
            from: snapshot.table_comment.clone(),
            to: draft.table_comment.clone(),
        });
    }
    for dcol in &draft.columns {
        let from = dcol
            .origin
            .as_deref()
            .and_then(|origin| snapshot.column_comments.get(origin));
        let to = draft.column_comments.get(&dcol.def.name);
        if !same(from, to) {
            changes.push(Change::SetComment {
                column: Some(dcol.def.name.clone()),
                from: from.cloned(),
                to: to.cloned(),
            });
        }
    }
}

fn types_equal(a: &str, b: &str) -> bool {
    normalize_type(a) == normalize_type(b)
}

fn normalize_type(text: &str) -> String {
    text.to_ascii_lowercase()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect()
}

/// True when column structure changes require dependent views to be recreated.
pub fn touches_columns(set: &ChangeSet) -> bool {
    set.changes.iter().any(|c| {
        matches!(
            c,
            Change::DropColumn { .. }
                | Change::RenameColumn { .. }
                | Change::AlterColumnType { .. }
                | Change::ReplacePrimaryKey { .. }
        )
    })
}

pub fn order_key(change: &Change) -> u8 {
    match change {
        Change::DropForeignKey { .. } => 0,
        Change::DropIndex { .. } => 1,
        Change::ReplacePrimaryKey { .. } => 2,
        Change::RenameColumn { .. } => 3,
        Change::SetDefault { .. } => 4,
        Change::AlterColumnType { .. } => 5,
        Change::DropColumn { .. } => 6,
        Change::AddColumn { .. } => 7,
        Change::AddIndex { .. } => 8,
        Change::AddForeignKey { .. } => 9,
        Change::SetComment { .. } => 10,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alter::snapshot::DraftColumn;
    use crate::alter::snapshot::{TableDraft, TableSnapshot};

    fn base_snapshot() -> TableSnapshot {
        TableSnapshot {
            schema: Some("dbo".into()),
            table: "t".into(),
            columns: vec![ColumnDef {
                name: "id".into(),
                data_type: "int".into(),
                nullable: false,
                identity: false,
                default: None,
                default_constraint_name: None,
            }],
            primary_key: vec!["id".into()],
            pk_constraint_name: Some("PK_t".into()),
            indexes: vec![],
            foreign_keys: vec![],
            stored_index_ddl: vec![],
            stored_trigger_ddl: vec![],
            sqlite_foreign_keys_on: false,
            referenced_by: vec![],
            dependent_views: vec![],
            table_comment: None,
            column_comments: BTreeMap::new(),
            stored_table_ddl: None,
            ..Default::default()
        }
    }

    fn base_draft(snapshot: &TableSnapshot) -> TableDraft {
        TableDraft {
            schema: snapshot.schema.clone(),
            table: snapshot.table.clone(),
            columns: snapshot
                .columns
                .iter()
                .map(|c| DraftColumn {
                    origin: Some(c.name.clone()),
                    def: c.clone(),
                })
                .collect(),
            primary_key: snapshot.primary_key.clone(),
            pk_constraint_name: snapshot.pk_constraint_name.clone(),
            indexes: snapshot.indexes.clone(),
            foreign_keys: snapshot.foreign_keys.clone(),
            table_comment: snapshot.table_comment.clone(),
            column_comments: snapshot.column_comments.clone(),
            source_ddl: snapshot.stored_table_ddl.clone(),
        }
    }

    #[test]
    fn empty_diff() {
        let snap = base_snapshot();
        let draft = base_draft(&snap);
        let set = diff(&snap, &draft);
        assert!(set.changes.is_empty());
        assert!(set.manual_review.is_empty());
    }

    #[test]
    fn add_column() {
        let snap = base_snapshot();
        let mut draft = base_draft(&snap);
        draft.columns.push(DraftColumn {
            origin: None,
            def: ColumnDef {
                name: "note".into(),
                data_type: "nvarchar(50)".into(),
                nullable: true,
                identity: false,
                default: None,
                default_constraint_name: None,
            },
        });
        let set = diff(&snap, &draft);
        assert!(
            set.changes
                .iter()
                .any(|c| matches!(c, Change::AddColumn(_)))
        );
    }

    #[test]
    fn drop_column() {
        let snap = base_snapshot();
        let draft = base_draft(&snap);
        let set = diff(
            &snap,
            &TableDraft {
                columns: vec![],
                ..draft
            },
        );
        assert!(
            set.changes
                .iter()
                .any(|c| matches!(c, Change::DropColumn { name } if name == "id"))
        );
    }

    #[test]
    fn rename_column() {
        let snap = base_snapshot();
        let mut draft = base_draft(&snap);
        draft.columns[0].origin = Some("id".into());
        draft.columns[0].def.name = "new_id".into();
        let set = diff(&snap, &draft);
        assert!(set.changes.iter().any(
            |c| matches!(c, Change::RenameColumn { from, to } if from == "id" && to == "new_id")
        ));
    }

    #[test]
    fn rename_and_type_change() {
        let snap = base_snapshot();
        let mut draft = base_draft(&snap);
        draft.columns[0].origin = Some("id".into());
        draft.columns[0].def.name = "new_id".into();
        draft.columns[0].def.data_type = "bigint".into();
        let set = diff(&snap, &draft);
        assert!(
            set.changes
                .iter()
                .any(|c| matches!(c, Change::RenameColumn { .. }))
        );
        assert!(
            set.changes
                .iter()
                .any(|c| matches!(c, Change::AlterColumnType { name, .. } if name == "new_id"))
        );
    }

    #[test]
    fn type_only_change() {
        let snap = base_snapshot();
        let mut draft = base_draft(&snap);
        draft.columns[0].def.data_type = "bigint".into();
        let set = diff(&snap, &draft);
        assert!(
            set.changes
                .iter()
                .any(|c| matches!(c, Change::AlterColumnType { .. }))
        );
        assert!(
            !set.changes
                .iter()
                .any(|c| matches!(c, Change::RenameColumn { .. }))
        );
    }

    #[test]
    fn nullability_only_change() {
        let snap = base_snapshot();
        let mut draft = base_draft(&snap);
        draft.columns[0].def.nullable = true;
        let set = diff(&snap, &draft);
        assert!(
            set.changes
                .iter()
                .any(|c| matches!(c, Change::AlterColumnType { .. }))
        );
    }

    #[test]
    fn default_only_change() {
        let snap = base_snapshot();
        let mut draft = base_draft(&snap);
        draft.columns[0].def.default = Some("0".into());
        let set = diff(&snap, &draft);
        assert!(
            set.changes
                .iter()
                .any(|c| matches!(c, Change::SetDefault { .. }))
        );
    }

    #[test]
    fn index_edit_becomes_drop_add() {
        let mut snap = base_snapshot();
        snap.indexes.push(IndexDef {
            name: "IX_t".into(),
            columns: vec!["id".into()],
            unique: false,
            include: vec![],
            filter: None,
            method: None,
            fillfactor: None,
        });
        let mut draft = base_draft(&snap);
        draft.indexes[0].columns = vec!["id".into(), "note".into()];
        let set = diff(&snap, &draft);
        assert!(
            set.changes
                .iter()
                .any(|c| matches!(c, Change::DropIndex { name } if name == "IX_t"))
        );
        assert!(set.changes.iter().any(|c| matches!(c, Change::AddIndex(_))));
    }

    #[test]
    fn identity_toggle_is_manual_review() {
        let snap = base_snapshot();
        let mut draft = base_draft(&snap);
        draft.columns[0].def.identity = true;
        let set = diff(&snap, &draft);
        assert!(!set.manual_review.is_empty());
    }

    #[test]
    fn comment_changes_diff_at_table_and_column_level() {
        let mut snap = base_snapshot();
        snap.table_comment = Some("old table note".into());
        snap.column_comments
            .insert("id".into(), "identifier".into());
        let mut draft = base_draft(&snap);
        draft.table_comment = Some("new table note".into());
        draft
            .column_comments
            .insert("id".into(), "primary key".into());
        let set = diff(&snap, &draft);
        assert!(set.changes.iter().any(
            |c| matches!(c, Change::SetComment { column: None, from, to }
                if from.as_deref() == Some("old table note") && to.as_deref() == Some("new table note"))
        ));
        assert!(set.changes.iter().any(
            |c| matches!(c, Change::SetComment { column: Some(col), from, to }
                if col == "id" && from.as_deref() == Some("identifier") && to.as_deref() == Some("primary key"))
        ));
    }

    #[test]
    fn clearing_a_comment_diffs_to_none() {
        let mut snap = base_snapshot();
        snap.column_comments
            .insert("id".into(), "identifier".into());
        let draft = base_draft(&snap);
        let set = diff(
            &snap,
            &TableDraft {
                column_comments: BTreeMap::new(),
                ..draft
            },
        );
        assert!(set.changes.iter().any(
            |c| matches!(c, Change::SetComment { column: Some(col), from, to }
                if col == "id" && from.as_deref() == Some("identifier") && to.is_none())
        ));
    }

    #[test]
    fn renamed_column_keeps_its_comment_without_a_change() {
        let mut snap = base_snapshot();
        snap.column_comments
            .insert("id".into(), "identifier".into());
        let mut draft = base_draft(&snap);
        draft.columns[0].origin = Some("id".into());
        draft.columns[0].def.name = "new_id".into();
        draft
            .column_comments
            .insert("new_id".into(), "identifier".into());
        let set = diff(&snap, &draft);
        assert!(
            !set.changes
                .iter()
                .any(|c| matches!(c, Change::SetComment { .. }))
        );
        assert!(
            set.changes
                .iter()
                .any(|c| matches!(c, Change::RenameColumn { .. }))
        );
    }

    #[test]
    fn dropped_columns_comment_needs_no_statement() {
        let mut snap = base_snapshot();
        snap.column_comments
            .insert("id".into(), "identifier".into());
        let draft = base_draft(&snap);
        let set = diff(
            &snap,
            &TableDraft {
                columns: vec![],
                column_comments: BTreeMap::new(),
                ..draft
            },
        );
        assert!(
            !set.changes
                .iter()
                .any(|c| matches!(c, Change::SetComment { .. }))
        );
    }

    #[test]
    fn an_empty_stored_description_is_no_change_but_edits_keep_its_presence() {
        // SQL Server: MS_Description exists with value ''. The editor shows
        // an empty field; leaving it empty must not stage a phantom drop, and
        // typing text must carry `from: Some("")` so the renderer updates the
        // existing property instead of adding a duplicate.
        let mut snap = base_snapshot();
        snap.table_comment = Some(String::new());
        snap.column_comments.insert("id".into(), String::new());
        let mut draft = base_draft(&snap);
        draft.table_comment = None;
        draft.column_comments.clear();
        assert!(diff(&snap, &draft).changes.is_empty());

        draft.column_comments.insert("id".into(), "key".into());
        let set = diff(&snap, &draft);
        assert!(set.changes.iter().any(
            |c| matches!(c, Change::SetComment { column: Some(col), from, to }
                if col == "id" && from.as_deref() == Some("") && to.as_deref() == Some("key"))
        ));
    }

    #[test]
    fn comments_sort_after_structural_changes() {
        let mut snap = base_snapshot();
        snap.column_comments
            .insert("id".into(), "identifier".into());
        let mut draft = base_draft(&snap);
        draft.columns[0].def.data_type = "bigint".into();
        draft.column_comments.insert("id".into(), "pk".into());
        let set = diff(&snap, &draft);
        let keys: Vec<u8> = set.changes.iter().map(order_key).collect();
        for pair in keys.windows(2) {
            assert!(pair[0] <= pair[1]);
        }
        assert_eq!(
            keys.last(),
            Some(&order_key(&Change::SetComment {
                column: None,
                from: None,
                to: None,
            }))
        );
    }

    #[test]
    fn change_ordering_is_fixed() {
        let mut snap = base_snapshot();
        snap.columns.push(ColumnDef {
            name: "email".into(),
            data_type: "nvarchar(100)".into(),
            nullable: true,
            identity: false,
            default: Some("''".into()),
            default_constraint_name: Some("DF_t_email".into()),
        });
        snap.indexes.push(IndexDef {
            name: "IX_email".into(),
            columns: vec!["email".into()],
            unique: false,
            include: vec![],
            filter: None,
            method: None,
            fillfactor: None,
        });
        let mut draft = base_draft(&snap);
        draft.columns[1].origin = Some("email".into());
        draft.columns[1].def.name = "email_addr".into();
        draft.columns[1].def.data_type = "nvarchar(200)".into();
        draft.indexes.clear();
        let set = diff(&snap, &draft);
        let keys: Vec<u8> = set.changes.iter().map(order_key).collect();
        let expected_order = [
            order_key(&Change::DropIndex {
                name: String::new(),
            }),
            order_key(&Change::RenameColumn {
                from: String::new(),
                to: String::new(),
            }),
            order_key(&Change::SetDefault {
                name: String::new(),
                from: None,
                to: None,
                constraint_name: None,
            }),
            order_key(&Change::AlterColumnType {
                name: String::new(),
                from: ColumnDef {
                    name: String::new(),
                    data_type: String::new(),
                    nullable: false,
                    identity: false,
                    default: None,
                    default_constraint_name: None,
                },
                to: ColumnDef {
                    name: String::new(),
                    data_type: String::new(),
                    nullable: false,
                    identity: false,
                    default: None,
                    default_constraint_name: None,
                },
            }),
        ];
        for pair in keys.windows(2) {
            assert!(pair[0] <= pair[1]);
        }
        assert!(keys.contains(&expected_order[0]));
    }
}
