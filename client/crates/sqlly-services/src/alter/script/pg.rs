//! PostgreSQL ALTER script generation.

use sqlly_lang::sqlmodel::DatabaseType;

use crate::alter::diff::{Change, ChangeSet};
use crate::alter::script::{
    AlterScript, RenderOptions, apply_view_wrap, collect_changes, header, is_add_col, is_add_fk,
    is_add_index, is_alter_type, is_drop_col, is_drop_fk, is_drop_index, is_replace_pk,
    is_set_default, qualified_table, render_comment_changes,
};
use crate::alter::snapshot::{ForeignKeyDef, IndexDef, TableDraft, TableSnapshot};

pub fn render(
    db: DatabaseType,
    snapshot: &TableSnapshot,
    draft: &TableDraft,
    set: &ChangeSet,
    opts: &RenderOptions,
) -> Result<AlterScript, crate::alter::script::AlterError> {
    let mut warnings = Vec::new();
    let mut manual_review = set.manual_review.clone();
    for change in &set.changes {
        if let Change::AlterColumnType { name, to, .. } = change
            && !opts.pg_using.contains_key(name)
        {
            warnings.push(format!(
                    "PostgreSQL will cast {} with ::{}; supply a USING expression if the cast is not implicit",
                    name, to.data_type
                ));
        }
    }

    let table_q = qualified_table(db, set.table.schema.as_deref(), &set.table.table);
    let mut body = String::new();

    for c in &set.changes {
        if let Change::RenameColumn { from, to } = c {
            body.push_str(&format!(
                "ALTER TABLE {table_q} RENAME COLUMN {} TO {};\n",
                db.quote_identifier(from),
                db.quote_identifier(to)
            ));
        }
    }

    for c in collect_changes(set, is_drop_index) {
        if let Change::DropIndex { name } = c {
            let idx_q = match set.table.schema.as_deref().filter(|s| !s.is_empty()) {
                Some(s) => format!("{}.{}", db.quote_identifier(s), db.quote_identifier(name)),
                None => db.quote_identifier(name),
            };
            body.push_str(&format!("DROP INDEX {idx_q};\n"));
        }
    }

    let mut alter_clauses: Vec<String> = Vec::new();

    for c in collect_changes(set, is_drop_fk) {
        if let Change::DropForeignKey { name } = c {
            alter_clauses.push(format!("DROP CONSTRAINT {}", db.quote_identifier(name)));
        }
    }
    for c in collect_changes(set, is_replace_pk) {
        if let Change::ReplacePrimaryKey {
            constraint_name, ..
        } = c
            && let Some(cn) = constraint_name
        {
            alter_clauses.push(format!("DROP CONSTRAINT {}", db.quote_identifier(cn)));
        }
    }
    for c in collect_changes(set, is_set_default) {
        if let Change::SetDefault { name, from, .. } = c
            && from.is_some()
        {
            alter_clauses.push(format!(
                "ALTER COLUMN {} DROP DEFAULT",
                db.quote_identifier(name)
            ));
        }
    }
    for c in collect_changes(set, is_alter_type) {
        if let Change::AlterColumnType { name, to, .. } = c {
            let using = opts
                .pg_using
                .get(name)
                .cloned()
                .unwrap_or_else(|| format!("{}::{}", db.quote_identifier(name), to.data_type));
            alter_clauses.push(format!(
                "ALTER COLUMN {} TYPE {} USING {}",
                db.quote_identifier(name),
                to.data_type,
                using
            ));
        }
    }
    for c in collect_changes(set, is_alter_type) {
        if let Change::AlterColumnType { name, to, .. } = c {
            alter_clauses.push(format!(
                "ALTER COLUMN {} {}",
                db.quote_identifier(name),
                if to.nullable {
                    "DROP NOT NULL"
                } else {
                    "SET NOT NULL"
                }
            ));
        }
    }
    for c in collect_changes(set, is_set_default) {
        if let Change::SetDefault { name, to, .. } = c
            && let Some(def) = to
        {
            alter_clauses.push(format!(
                "ALTER COLUMN {} SET DEFAULT {}",
                db.quote_identifier(name),
                def
            ));
        }
    }
    for c in collect_changes(set, is_drop_col) {
        if let Change::DropColumn { name } = c {
            alter_clauses.push(format!("DROP COLUMN {}", db.quote_identifier(name)));
        }
    }
    for c in collect_changes(set, is_add_col) {
        if let Change::AddColumn(col) = c {
            let mut clause = format!(
                "ADD COLUMN {} {} {}",
                db.quote_identifier(&col.name),
                col.data_type,
                if col.nullable { "NULL" } else { "NOT NULL" }
            );
            if let Some(def) = &col.default {
                clause.push_str(&format!(" DEFAULT {}", def));
            }
            alter_clauses.push(clause);
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
            alter_clauses.push(format!(
                "ADD CONSTRAINT {} PRIMARY KEY ({})",
                db.quote_identifier(&format!("PK_{}", set.table.table)),
                cols
            ));
        }
    }
    for c in collect_changes(set, is_add_fk) {
        if let Change::AddForeignKey(fk) = c {
            alter_clauses.push(render_fk_clause(db, fk));
        }
    }

    body.push_str("BEGIN;\n");
    if !alter_clauses.is_empty() {
        body.push_str(&format!(
            "ALTER TABLE {table_q}\n    {}\n;\n",
            alter_clauses.join(",\n    ")
        ));
    }

    for c in collect_changes(set, is_add_index) {
        if let Change::AddIndex(idx) = c {
            body.push_str(&render_create_index(db, &table_q, idx, &mut warnings));
        }
    }

    body.push_str("COMMIT;\n");

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

pub(crate) fn render_create_index(
    db: DatabaseType,
    table_q: &str,
    idx: &IndexDef,
    warnings: &mut Vec<String>,
) -> String {
    let unique = if idx.unique { "UNIQUE " } else { "" };
    let using = match idx.method.as_deref() {
        Some(m) if !m.eq_ignore_ascii_case("btree") => format!(" USING {m}"),
        _ => String::new(),
    };
    let cols = idx
        .columns
        .iter()
        .map(|c| db.quote_identifier(c))
        .collect::<Vec<_>>()
        .join(", ");
    let mut sql = format!(
        "CREATE {unique}INDEX {} ON {table_q}{using} ({cols})",
        db.quote_identifier(&idx.name),
        unique = unique,
        using = using,
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
    if let Some(ff) = idx.fillfactor {
        let method = idx.method.as_deref().unwrap_or("btree");
        if matches!(
            method.to_ascii_lowercase().as_str(),
            "btree" | "hash" | "gist" | "spgist"
        ) {
            sql.push_str(&format!(" WITH (fillfactor = {ff})"));
        } else {
            warnings.push(format!(
                "Index {}: fillfactor is not supported on PostgreSQL for access method {method}; omitted",
                idx.name
            ));
        }
    }
    if let Some(filter) = &idx.filter {
        sql.push_str(&format!(" WHERE {filter}"));
    }
    sql.push_str(";\n");
    sql
}

fn render_fk_clause(db: DatabaseType, fk: &ForeignKeyDef) -> String {
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
        "ADD CONSTRAINT {} FOREIGN KEY ({cols}) REFERENCES {ref_table} ({ref_cols})",
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
    sql
}
