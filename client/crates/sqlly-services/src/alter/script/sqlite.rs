//! SQLite ALTER and table-rebuild script generation.
//!
//! When `foreign_key_check` runs twice (step 1b and step 10), compare the
//! result grids in the editor — new violation rows after the rebuild were
//! caused by this change; pre-existing violations appear in both outputs.

use sqlly_lang::sqlmodel::DatabaseType;

use crate::alter::diff::{Change, ChangeSet};
use crate::alter::script::{
    AlterScript, RenderOptions, apply_view_wrap, header, qualified_table, render_comment_changes,
};
use crate::alter::snapshot::{ForeignKeyDef, IndexDef, TableDraft, TableSnapshot};

pub fn render(
    db: DatabaseType,
    snapshot: &TableSnapshot,
    draft: &TableDraft,
    set: &ChangeSet,
    _opts: &RenderOptions,
) -> Result<AlterScript, crate::alter::script::AlterError> {
    // SQLite has no server-side comments; a staged description change (the
    // editor hides the field, so this is only a guard) fails honestly rather
    // than vanishing from the script.
    let _ = render_comment_changes(db, snapshot, set, draft)?;
    let mut warnings = Vec::new();
    if !snapshot.referenced_by.is_empty() {
        warnings.push(
            "Other tables reference this one; DROP TABLE in Step 6 is safe only because foreign_keys is OFF for the transaction"
                .to_string(),
        );
    }

    if use_fast_path(set) {
        return render_fast_path(db, snapshot, draft, set, warnings);
    }
    render_rebuild(db, snapshot, draft, set, warnings)
}

fn use_fast_path(set: &ChangeSet) -> bool {
    set.changes.iter().all(|c| match c {
        Change::AddColumn(col) => col.nullable || col.default.is_some(),
        Change::RenameColumn { .. } => true,
        Change::DropColumn { .. } => true,
        Change::DropIndex { .. } | Change::AddIndex(_) => true,
        _ => false,
    })
}

fn render_fast_path(
    db: DatabaseType,
    snapshot: &TableSnapshot,
    _draft: &TableDraft,
    set: &ChangeSet,
    warnings: Vec<String>,
) -> Result<AlterScript, crate::alter::script::AlterError> {
    let table_q = qualified_table(db, set.table.schema.as_deref(), &set.table.table);
    let mut warnings = warnings;
    let mut manual_review = set.manual_review.clone();
    let (view_drop, view_create) =
        apply_view_wrap(db, snapshot, set, &mut warnings, &mut manual_review);
    let mut body = String::from("BEGIN;\n");
    for c in &set.changes {
        match c {
            Change::AddColumn(col) => {
                let mut line = format!(
                    "ALTER TABLE {table_q} ADD COLUMN {} {}",
                    db.quote_identifier(&col.name),
                    col.data_type
                );
                if !col.nullable {
                    line.push_str(" NOT NULL");
                }
                if let Some(def) = &col.default {
                    line.push_str(&format!(" DEFAULT {}", def));
                }
                body.push_str(&format!("{line};\n"));
            }
            Change::RenameColumn { from, to } => {
                body.push_str(&format!(
                    "ALTER TABLE {table_q} RENAME COLUMN {} TO {};\n",
                    db.quote_identifier(from),
                    db.quote_identifier(to)
                ));
            }
            Change::DropColumn { name } => {
                body.push_str(&format!(
                    "ALTER TABLE {table_q} DROP COLUMN {};\n",
                    db.quote_identifier(name)
                ));
            }
            Change::DropIndex { name } => {
                body.push_str(&format!("DROP INDEX {};\n", db.quote_identifier(name)));
            }
            Change::AddIndex(idx) => {
                body.push_str(&render_create_index(db, &table_q, idx, &mut warnings));
            }
            _ => {}
        }
    }
    body.push_str("COMMIT;\n");
    Ok(AlterScript {
        sql: header(
            set.table.schema.as_deref(),
            &set.table.table,
            &warnings,
            &manual_review,
        ) + &view_drop
            + &body
            + &view_create,
        warnings,
    })
}

fn render_rebuild(
    db: DatabaseType,
    snapshot: &TableSnapshot,
    draft: &TableDraft,
    set: &ChangeSet,
    mut warnings: Vec<String>,
) -> Result<AlterScript, crate::alter::script::AlterError> {
    for col in &draft.columns {
        if col.origin.is_none() && !col.def.nullable && col.def.default.is_none() {
            warnings.push(format!(
                "Step 5 will fail for existing rows on column {}; add a default",
                col.def.name
            ));
        }
    }

    let table = &draft.table;
    let table_q = qualified_table(db, draft.schema.as_deref(), table);
    let new_name = format!("new_{}", table);
    let new_q = db.quote_identifier(&new_name);

    let mut manual_review = set.manual_review.clone();
    let (view_drop, view_create) =
        apply_view_wrap(db, snapshot, set, &mut warnings, &mut manual_review);
    let mut body = String::new();
    if snapshot.sqlite_foreign_keys_on {
        body.push_str("-- Step 1: PRAGMA foreign_keys=OFF;\nPRAGMA foreign_keys=OFF;\n");
        body.push_str(
            "-- Step 1b: baseline violations (compare with Step 10 output; new rows there are caused by this change)\nPRAGMA foreign_key_check;\n",
        );
    }
    body.push_str("-- Step 2: BEGIN TRANSACTION;\nBEGIN TRANSACTION;\n");

    body.push_str("-- Step 3: remember indexes/triggers:\n");
    for ddl in &snapshot.stored_index_ddl {
        body.push_str(&format!("--   index DDL captured\n--   {ddl}\n"));
    }
    for ddl in &snapshot.stored_trigger_ddl {
        body.push_str(&format!("--   trigger DDL captured\n--   {ddl}\n"));
    }

    body.push_str("-- Step 4: CREATE TABLE new_<t> (...);\n");
    body.push_str(&format!(
        "CREATE TABLE {new_q} (\n{}\n);\n",
        render_create_columns(db, draft)
    ));

    body.push_str("-- Step 5: INSERT INTO new_<t> SELECT ... FROM <t>;\n");
    let (insert_cols, select_cols) = insert_mapping(draft, db);
    if !insert_cols.is_empty() {
        body.push_str(&format!(
            "INSERT INTO {new_q} ({insert_cols}) SELECT {select_cols} FROM {table_q};\n",
            insert_cols = insert_cols,
            select_cols = select_cols
        ));
    }

    body.push_str("-- Step 6: DROP TABLE <t>;\n");
    body.push_str(&format!("DROP TABLE {table_q};\n"));

    body.push_str("-- Step 7: ALTER TABLE new_<t> RENAME TO <t>;\n");
    body.push_str(&format!(
        "ALTER TABLE {new_q} RENAME TO {};\n",
        db.quote_identifier(table)
    ));

    body.push_str("-- Step 8: replay indexes and triggers;\n");
    for idx in &draft.indexes {
        if snapshot
            .stored_index_ddl
            .iter()
            .any(|ddl| ddl.contains(&idx.name))
        {
            if let Some(ddl) = snapshot
                .stored_index_ddl
                .iter()
                .find(|d| d.contains(&idx.name))
            {
                body.push_str(ddl);
                if !ddl.ends_with(';') {
                    body.push(';');
                }
                body.push('\n');
                warn_column_refs(ddl, &idx.name, draft, &mut warnings);
            }
        } else {
            body.push_str(&render_create_index(db, &table_q, idx, &mut warnings));
        }
    }
    for ddl in &snapshot.stored_trigger_ddl {
        body.push_str(ddl);
        if !ddl.ends_with(';') {
            body.push(';');
        }
        body.push('\n');
    }

    if view_create.is_empty() {
        body.push_str(
            "-- Step 9: views referencing this table are not modified; review them manually.\n",
        );
    } else {
        body.push_str("-- Step 9: recreate dependent views;\n");
        body.push_str(&view_create);
    }

    if snapshot.sqlite_foreign_keys_on {
        body.push_str("-- Step 10: PRAGMA foreign_key_check;\nPRAGMA foreign_key_check;\n");
    }
    body.push_str("-- Step 11: COMMIT;\nCOMMIT;\n");
    if snapshot.sqlite_foreign_keys_on {
        body.push_str("-- Step 12: PRAGMA foreign_keys=ON;\nPRAGMA foreign_keys=ON;\n");
    }

    Ok(AlterScript {
        sql: header(
            set.table.schema.as_deref(),
            &set.table.table,
            &warnings,
            &manual_review,
        ) + &view_drop
            + &body,
        warnings,
    })
}

fn render_create_columns(db: DatabaseType, draft: &TableDraft) -> String {
    let mut lines: Vec<String> = Vec::new();
    let pk_cols = &draft.primary_key;
    let single_int_identity_pk = pk_cols.len() == 1
        && draft
            .columns
            .iter()
            .any(|c| c.def.name == pk_cols[0] && c.def.identity);

    for col in &draft.columns {
        let mut line = format!(
            "    {} {}",
            db.quote_identifier(&col.def.name),
            col.def.data_type
        );
        if !col.def.nullable {
            line.push_str(" NOT NULL");
        }
        if let Some(def) = &col.def.default {
            line.push_str(&format!(" DEFAULT {}", def));
        }
        if single_int_identity_pk && col.def.name == pk_cols[0] && col.def.identity {
            line.push_str(" PRIMARY KEY AUTOINCREMENT");
        }
        lines.push(line);
    }
    if !pk_cols.is_empty() && !single_int_identity_pk {
        let cols = pk_cols
            .iter()
            .map(|c| db.quote_identifier(c))
            .collect::<Vec<_>>()
            .join(", ");
        lines.push(format!("    PRIMARY KEY ({cols})"));
    }
    for fk in &draft.foreign_keys {
        lines.push(format!("    {}", render_table_fk(db, fk)));
    }
    lines.join(",\n")
}

fn render_table_fk(db: DatabaseType, fk: &ForeignKeyDef) -> String {
    let cols = fk
        .columns
        .iter()
        .map(|c| db.quote_identifier(c))
        .collect::<Vec<_>>()
        .join(", ");
    let ref_table = qualified_table(db, fk.ref_schema.as_deref(), &fk.ref_table);
    let ref_cols = fk
        .ref_columns
        .iter()
        .map(|c| db.quote_identifier(c))
        .collect::<Vec<_>>()
        .join(", ");
    let mut sql = format!(
        "FOREIGN KEY ({cols}) REFERENCES {ref_table} ({ref_cols})",
        cols = cols,
        ref_table = ref_table,
        ref_cols = ref_cols
    );
    if let Some(action) = &fk.on_delete {
        sql.push_str(&format!(" ON DELETE {action}"));
    }
    if let Some(action) = &fk.on_update {
        sql.push_str(&format!(" ON UPDATE {action}"));
    }
    sql
}

fn insert_mapping(draft: &TableDraft, db: DatabaseType) -> (String, String) {
    let mut insert: Vec<String> = Vec::new();
    let mut select: Vec<String> = Vec::new();
    for col in &draft.columns {
        if let Some(origin) = &col.origin {
            insert.push(db.quote_identifier(&col.def.name));
            select.push(db.quote_identifier(origin));
        }
    }
    (insert.join(", "), select.join(", "))
}

fn render_create_index(
    db: DatabaseType,
    table_q: &str,
    idx: &IndexDef,
    warnings: &mut Vec<String>,
) -> String {
    let unique = if idx.unique { "UNIQUE " } else { "" };
    let cols = idx
        .columns
        .iter()
        .map(|c| db.quote_identifier(c))
        .collect::<Vec<_>>()
        .join(", ");
    let mut sql = format!(
        "CREATE {unique}INDEX {} ON {table_q} ({cols})",
        db.quote_identifier(&idx.name),
        unique = unique,
        cols = cols
    );
    if let Some(filter) = &idx.filter {
        sql.push_str(&format!(" WHERE {filter}"));
    }
    if idx.method.is_some() {
        warnings.push(format!(
            "Index {}: method is not supported on SQLite; omitted",
            idx.name
        ));
    }
    if idx.fillfactor.is_some() {
        warnings.push(format!(
            "Index {}: fillfactor is not supported on SQLite; omitted",
            idx.name
        ));
    }
    if !idx.include.is_empty() {
        warnings.push(format!(
            "Index {}: INCLUDE is not supported on SQLite; omitted",
            idx.name
        ));
    }
    sql.push_str(";\n");
    sql
}

fn warn_column_refs(ddl: &str, name: &str, draft: &TableDraft, warnings: &mut Vec<String>) {
    let ddl_lower = ddl.to_ascii_lowercase();
    for col in &draft.columns {
        if let Some(origin) = &col.origin
            && origin != &col.def.name
            && ddl_lower.contains(&origin.to_ascii_lowercase())
        {
            warnings.push(format!(
                "Index/trigger {name} references a renamed/dropped column; edit before running"
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alter::diff::diff;
    use crate::alter::snapshot::{ColumnDef, DraftColumn, TableDraft, TableSnapshot};

    fn snap() -> TableSnapshot {
        TableSnapshot {
            schema: None,
            table: "t".into(),
            columns: vec![ColumnDef {
                name: "id".into(),
                data_type: "INTEGER".into(),
                nullable: false,
                identity: false,
                default: None,
                default_constraint_name: None,
            }],
            primary_key: vec!["id".into()],
            pk_constraint_name: None,
            indexes: vec![],
            foreign_keys: vec![],
            stored_index_ddl: vec![],
            stored_trigger_ddl: vec![],
            sqlite_foreign_keys_on: true,
            referenced_by: vec![],
            dependent_views: vec![],
            table_comment: None,
            column_comments: Default::default(),
            stored_table_ddl: None,
            ..Default::default()
        }
    }

    #[test]
    fn rebuild_step_markers_with_fks_on() {
        let snapshot = snap();
        let draft = TableDraft {
            schema: None,
            table: "t".into(),
            columns: vec![DraftColumn {
                origin: Some("id".into()),
                def: ColumnDef {
                    name: "id".into(),
                    data_type: "TEXT".into(),
                    nullable: false,
                    identity: false,
                    default: None,
                    default_constraint_name: None,
                },
            }],
            primary_key: vec!["id".into()],
            pk_constraint_name: None,
            indexes: vec![],
            foreign_keys: vec![],
            table_comment: None,
            column_comments: Default::default(),
            source_ddl: None,
        };
        let set = diff(&snapshot, &draft);
        let script = render(
            sqlly_lang::sqlmodel::DatabaseType::Sqlite,
            &snapshot,
            &draft,
            &set,
            &RenderOptions::default(),
        )
        .unwrap();
        let sql = script.sql;
        assert!(sql.contains("-- Step 1:"));
        assert!(sql.contains("-- Step 1b:"));
        assert!(sql.contains("-- Step 10:"));
        assert!(sql.contains("-- Step 12:"));
    }

    #[test]
    fn rebuild_omits_fk_pragmas_when_off() {
        let mut snapshot = snap();
        snapshot.sqlite_foreign_keys_on = false;
        let draft = TableDraft {
            schema: None,
            table: "t".into(),
            columns: vec![DraftColumn {
                origin: Some("id".into()),
                def: ColumnDef {
                    name: "id".into(),
                    data_type: "TEXT".into(),
                    nullable: false,
                    identity: false,
                    default: None,
                    default_constraint_name: None,
                },
            }],
            primary_key: vec!["id".into()],
            pk_constraint_name: None,
            indexes: vec![],
            foreign_keys: vec![],
            table_comment: None,
            column_comments: Default::default(),
            source_ddl: None,
        };
        let set = diff(&snapshot, &draft);
        let script = render(
            sqlly_lang::sqlmodel::DatabaseType::Sqlite,
            &snapshot,
            &draft,
            &set,
            &RenderOptions::default(),
        )
        .unwrap();
        let sql = script.sql;
        assert!(!sql.contains("PRAGMA foreign_keys=OFF"));
        assert!(!sql.contains("-- Step 1b:"));
        assert!(!sql.contains("-- Step 10:"));
        assert!(!sql.contains("PRAGMA foreign_keys=ON"));
    }

    #[test]
    fn insert_maps_origin_for_renamed_columns() {
        let snapshot = TableSnapshot {
            schema: None,
            table: "people".into(),
            columns: vec![
                ColumnDef {
                    name: "id".into(),
                    data_type: "INTEGER".into(),
                    nullable: false,
                    identity: false,
                    default: None,
                    default_constraint_name: None,
                },
                ColumnDef {
                    name: "email".into(),
                    data_type: "TEXT".into(),
                    nullable: true,
                    identity: false,
                    default: None,
                    default_constraint_name: None,
                },
            ],
            primary_key: vec!["id".into()],
            pk_constraint_name: None,
            indexes: vec![],
            foreign_keys: vec![],
            stored_index_ddl: vec![],
            stored_trigger_ddl: vec![],
            sqlite_foreign_keys_on: true,
            referenced_by: vec![],
            dependent_views: vec![],
            table_comment: None,
            column_comments: Default::default(),
            stored_table_ddl: None,
            ..Default::default()
        };
        let draft = TableDraft {
            schema: None,
            table: "people".into(),
            columns: vec![
                DraftColumn {
                    origin: Some("id".into()),
                    def: snapshot.columns[0].clone(),
                },
                DraftColumn {
                    origin: Some("email".into()),
                    def: ColumnDef {
                        name: "email_address".into(),
                        data_type: "TEXT NOT NULL".into(),
                        nullable: false,
                        identity: false,
                        default: None,
                        default_constraint_name: None,
                    },
                },
            ],
            primary_key: vec!["id".into()],
            pk_constraint_name: None,
            indexes: vec![],
            foreign_keys: vec![],
            table_comment: None,
            column_comments: Default::default(),
            source_ddl: None,
        };
        let set = diff(&snapshot, &draft);
        let script = render(
            sqlly_lang::sqlmodel::DatabaseType::Sqlite,
            &snapshot,
            &draft,
            &set,
            &RenderOptions::default(),
        )
        .unwrap();
        assert!(
            script
                .sql
                .contains("INSERT INTO \"new_people\" (\"id\", \"email_address\") SELECT \"id\", \"email\" FROM \"people\"")
        );
    }
}
