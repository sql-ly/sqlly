//! SQL Server / Azure SQL ALTER script generation.

use sqlly_lang::sqlmodel::DatabaseType;

use std::collections::BTreeSet;

use crate::alter::diff::{Change, ChangeSet};
use crate::alter::script::{
    AlterScript, RenderOptions, apply_view_wrap, collect_changes, find_draft_column, header,
    is_add_col, is_add_fk, is_add_index, is_alter_type, is_drop_col, is_drop_fk, is_drop_index,
    is_rename, is_replace_pk, qualified_table, render_comment_changes,
};
use crate::alter::snapshot::{ColumnDef, ForeignKeyDef, IndexDef, TableDraft, TableSnapshot};

pub fn render(
    db: DatabaseType,
    snapshot: &TableSnapshot,
    draft: &TableDraft,
    set: &ChangeSet,
    _opts: &RenderOptions,
) -> Result<AlterScript, crate::alter::script::AlterError> {
    let mut warnings = Vec::new();
    let mut manual_review = set.manual_review.clone();
    for change in &set.changes {
        if let Change::AlterColumnType { to, .. } = change
            && !to.nullable
            && to.default.is_none()
        {
            warnings.push(format!(
                    "Existing NULL rows will make NOT NULL on {} fail; add a default or update rows first",
                    to.name
                ));
        }
        if let Change::AddColumn(col) = change
            && !col.nullable
            && col.default.is_none()
        {
            warnings.push(format!(
                    "Existing NULL rows will make NOT NULL on {} fail; add a default or update rows first",
                    col.name
                ));
        }
    }

    let mut body = String::new();
    body.push_str("SET XACT_ABORT ON;\nBEGIN TRANSACTION;\n");

    let table_q = qualified_table(db, set.table.schema.as_deref(), &set.table.table);

    for c in collect_changes(set, is_drop_fk) {
        if let Change::DropForeignKey { name } = c {
            body.push_str(&format!(
                "ALTER TABLE {table_q} DROP CONSTRAINT {};\n",
                db.quote_identifier(name)
            ));
        }
    }
    for c in collect_changes(set, is_drop_index) {
        if let Change::DropIndex { name } = c {
            body.push_str(&format!(
                "DROP INDEX {} ON {table_q};\n",
                db.quote_identifier(name)
            ));
        }
    }
    for c in collect_changes(set, is_replace_pk) {
        if let Change::ReplacePrimaryKey {
            constraint_name, ..
        } = c
        {
            let pk_name = constraint_name
                .clone()
                .unwrap_or_else(|| format!("PK_{}", set.table.table));
            body.push_str(&format!(
                "ALTER TABLE {table_q} DROP CONSTRAINT {};\n",
                db.quote_identifier(&pk_name)
            ));
        }
    }

    // Default constraints. SQL Server refuses to rename, retype, or drop a
    // column while a default constraint is bound to it, so every default that
    // is in the way (or is itself being changed) is dropped exactly once up
    // front, and the draft's default is re-added exactly once after the
    // column work — never dropped or added twice for one column.
    let mut defaults_dropped: BTreeSet<String> = BTreeSet::new();
    let mut default_readds: BTreeSet<String> = BTreeSet::new();
    for change in &set.changes {
        let snapshot_name = match change {
            Change::RenameColumn { from, .. } => Some(from.clone()),
            Change::AlterColumnType { from, .. } => Some(from.name.clone()),
            Change::DropColumn { name } => Some(name.clone()),
            Change::SetDefault { name, to, .. } => {
                if to.is_some() {
                    default_readds.insert(name.clone());
                }
                Some(draft_origin(draft, name).unwrap_or(name).to_string())
            }
            _ => None,
        };
        if let Some(snapshot_name) = snapshot_name
            && snapshot_column(snapshot, &snapshot_name).is_some_and(|c| c.default.is_some())
            && defaults_dropped.insert(snapshot_name.clone())
        {
            drop_default_before_column(snapshot, db, &table_q, &snapshot_name, &mut body);
        }
    }
    // Every surviving column whose default was dropped gets the draft's back.
    for column in &draft.columns {
        if let Some(origin) = &column.origin
            && defaults_dropped.contains(origin)
            && column.def.default.is_some()
        {
            default_readds.insert(column.def.name.clone());
        }
    }

    for c in collect_changes(set, is_rename) {
        if let Change::RenameColumn { from, to } = c {
            let old_q = column_object_name(db, set.table.schema.as_deref(), &set.table.table, from);
            body.push_str(&format!(
                "EXEC sp_rename N'{old_q}', N'{to}', N'COLUMN';\n",
                old_q = old_q.replace('\'', "''"),
                to = to.replace('\'', "''")
            ));
        }
    }

    for c in collect_changes(set, is_alter_type) {
        if let Change::AlterColumnType { name, from, to } = c {
            let collation = match kept_collation(snapshot, from, &to.data_type) {
                Some(collation) if is_collation_name(collation) => {
                    format!(" COLLATE {collation}")
                }
                Some(collation) => {
                    warnings.push(format!(
                        "Column {name}: collation {collation:?} is not a plain collation name; the column takes the database default"
                    ));
                    String::new()
                }
                None => String::new(),
            };
            body.push_str(&format!(
                "ALTER TABLE {table_q} ALTER COLUMN {} {}{collation} {};\n",
                db.quote_identifier(name),
                to.data_type,
                if to.nullable { "NULL" } else { "NOT NULL" }
            ));
        }
    }

    for name in &default_readds {
        if let Some(col) = find_draft_column(draft, name) {
            add_default_constraint(db, &table_q, &set.table.table, name, col, &mut body);
        }
    }

    for c in collect_changes(set, is_drop_col) {
        if let Change::DropColumn { name } = c {
            body.push_str(&format!(
                "ALTER TABLE {table_q} DROP COLUMN {};\n",
                db.quote_identifier(name)
            ));
        }
    }

    for c in collect_changes(set, is_add_col) {
        if let Change::AddColumn(col) = c {
            body.push_str(&format!(
                "ALTER TABLE {table_q} ADD {} {} {}",
                db.quote_identifier(&col.name),
                col.data_type,
                if col.nullable { "NULL" } else { "NOT NULL" }
            ));
            if let Some(def) = &col.default {
                let cn = format!("DF_{}_{}", set.table.table, col.name);
                body.push_str(&format!(
                    " CONSTRAINT {} DEFAULT ({})",
                    db.quote_identifier(&cn),
                    def
                ));
            }
            body.push_str(";\n");
        }
    }

    for c in collect_changes(set, is_replace_pk) {
        if let Change::ReplacePrimaryKey { to, .. } = c
            && !to.is_empty()
        {
            let cols = to
                .iter()
                .map(|c| db.quote_identifier(c))
                .collect::<Vec<_>>()
                .join(", ");
            body.push_str(&format!(
                "ALTER TABLE {table_q} ADD CONSTRAINT {} PRIMARY KEY ({});\n",
                db.quote_identifier(&format!("PK_{}", set.table.table)),
                cols
            ));
        }
    }

    for c in collect_changes(set, is_add_index) {
        if let Change::AddIndex(idx) = c {
            // A recreated index keeps the compression it has on the server.
            let compression = snapshot
                .storage
                .index_compression
                .get(&idx.name)
                .map(String::as_str);
            body.push_str(&render_create_index(
                db,
                &table_q,
                idx,
                compression,
                &mut warnings,
            ));
        }
    }

    for c in collect_changes(set, is_add_fk) {
        if let Change::AddForeignKey(fk) = c {
            body.push_str(&render_add_fk(db, &table_q, fk));
        }
    }

    body.push_str("COMMIT TRANSACTION;\n");

    let comments = render_comment_changes(db, snapshot, set, draft)?;

    let (view_drop, view_create) =
        apply_view_wrap(db, snapshot, set, &mut warnings, &mut manual_review);

    Ok(AlterScript {
        sql: header(
            set.table.schema.as_deref(),
            &set.table.table,
            &warnings,
            &manual_review,
        ) + &view_drop
            + &body
            + &view_create
            + &comments,
        warnings,
    })
}

fn column_object_name(db: DatabaseType, schema: Option<&str>, table: &str, column: &str) -> String {
    match schema.filter(|s| !s.is_empty()) {
        Some(s) => format!(
            "{}.{}.{}",
            db.quote_identifier(s),
            db.quote_identifier(table),
            db.quote_identifier(column)
        ),
        None => format!(
            "{}.{}",
            db.quote_identifier(table),
            db.quote_identifier(column)
        ),
    }
}

fn snapshot_column<'a>(snapshot: &'a TableSnapshot, name: &str) -> Option<&'a ColumnDef> {
    snapshot.columns.iter().find(|c| c.name == name)
}

fn drop_default_before_column(
    snapshot: &TableSnapshot,
    db: DatabaseType,
    table_q: &str,
    name: &str,
    body: &mut String,
) {
    if let Some(col) = snapshot_column(snapshot, name)
        && col.default.is_some()
    {
        let cn = col
            .default_constraint_name
            .clone()
            .unwrap_or_else(|| format!("DF_{}_{}", snapshot.table, name));
        body.push_str(&format!(
            "ALTER TABLE {table_q} DROP CONSTRAINT {};\n",
            db.quote_identifier(&cn)
        ));
    }
}

fn add_default_constraint(
    db: DatabaseType,
    table_q: &str,
    table: &str,
    name: &str,
    col: &ColumnDef,
    body: &mut String,
) {
    if let Some(def) = &col.default {
        let cn = col
            .default_constraint_name
            .clone()
            .unwrap_or_else(|| format!("DF_{}_{}", table, name));
        body.push_str(&format!(
            "ALTER TABLE {table_q} ADD CONSTRAINT {} DEFAULT ({}) FOR {};\n",
            db.quote_identifier(&cn),
            def,
            db.quote_identifier(name)
        ));
    }
}

/// The snapshot (origin) name of a draft column, when it existed before.
fn draft_origin<'a>(draft: &'a TableDraft, name: &str) -> Option<&'a str> {
    draft
        .columns
        .iter()
        .find(|c| c.def.name == name)
        .and_then(|c| c.origin.as_deref())
}

/// A SQL Server collation name is a bare identifier (`Latin1_General_100_CI_AS_SC`);
/// anything else is refused rather than spliced into a script.
pub(crate) fn is_collation_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Character types, the only ones a collation applies to.
fn is_character_type(data_type: &str) -> bool {
    let base = data_type
        .split('(')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    matches!(
        base.as_str(),
        "char" | "varchar" | "nchar" | "nvarchar" | "text" | "ntext" | "sysname"
    )
}

/// The column's non-default collation, when the retyped column stays a
/// character type: `ALTER COLUMN` without `COLLATE` silently resets the column
/// to the database default.
fn kept_collation<'a>(
    snapshot: &'a TableSnapshot,
    from: &ColumnDef,
    to_type: &str,
) -> Option<&'a str> {
    let collation = snapshot
        .column_extras
        .get(&from.name)?
        .collation
        .as_deref()?;
    is_character_type(to_type).then_some(collation)
}

/// `CREATE [UNIQUE] [CLUSTERED] INDEX …` with its INCLUDE list, filter, and
/// `WITH (FILLFACTOR = n, DATA_COMPRESSION = ROW|PAGE)` options.
pub(crate) fn render_create_index(
    db: DatabaseType,
    table_q: &str,
    idx: &IndexDef,
    compression: Option<&str>,
    warnings: &mut Vec<String>,
) -> String {
    let unique = if idx.unique { "UNIQUE " } else { "" };
    let clustered = if idx
        .method
        .as_deref()
        .is_some_and(|m| m.eq_ignore_ascii_case("CLUSTERED"))
    {
        "CLUSTERED "
    } else {
        ""
    };
    let cols = idx
        .columns
        .iter()
        .map(|c| db.quote_identifier(c))
        .collect::<Vec<_>>()
        .join(", ");
    let mut sql = format!(
        "CREATE {unique}{clustered}INDEX {} ON {table_q} ({cols})",
        db.quote_identifier(&idx.name),
        unique = unique,
        clustered = clustered,
        cols = cols
    );
    if !idx.include.is_empty() {
        let inc = idx
            .include
            .iter()
            .map(|c| db.quote_identifier(c))
            .collect::<Vec<_>>()
            .join(", ");
        sql.push_str(&format!(" INCLUDE ({inc})"));
    }
    if let Some(filter) = &idx.filter {
        sql.push_str(&format!(" WHERE {filter}"));
    }
    let mut options = Vec::new();
    if let Some(ff) = idx.fillfactor {
        options.push(format!("FILLFACTOR = {ff}"));
    }
    if let Some(compression) = compression.filter(|c| matches!(*c, "ROW" | "PAGE")) {
        options.push(format!("DATA_COMPRESSION = {compression}"));
    }
    if !options.is_empty() {
        sql.push_str(&format!(" WITH ({})", options.join(", ")));
    }
    if idx.method.as_deref().is_some_and(|m| {
        !m.eq_ignore_ascii_case("CLUSTERED") && !m.eq_ignore_ascii_case("NONCLUSTERED")
    }) {
        warnings.push(format!(
            "Index {}: method is not supported on SQL Server; omitted",
            idx.name
        ));
    }
    sql.push_str(";\n");
    sql
}

fn render_add_fk(db: DatabaseType, table_q: &str, fk: &ForeignKeyDef) -> String {
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
        "ALTER TABLE {table_q} ADD CONSTRAINT {} FOREIGN KEY ({cols}) REFERENCES {ref_table} ({ref_cols})",
        db.quote_identifier(&fk.name),
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
    sql.push_str(";\n");
    sql
}
