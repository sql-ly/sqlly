//! MySQL / MariaDB ALTER script generation.

use std::collections::BTreeSet;

use sqlly_lang::sqlmodel::DatabaseType;

use crate::alter::diff::{Change, ChangeSet};
use crate::alter::script::{
    AlterScript, MysqlAlgorithm, MysqlLock, RenderOptions, apply_view_wrap, collect_changes,
    comments, find_draft_column, header, is_add_col, is_add_fk, is_add_index, is_drop_col,
    is_drop_fk, is_drop_index, is_replace_pk, qualified_table, render_comment_changes,
};
use crate::alter::snapshot::{
    ColumnDef, ColumnExtras, ForeignKeyDef, IndexDef, TableDraft, TableSnapshot,
};

pub fn render(
    db: DatabaseType,
    snapshot: &TableSnapshot,
    draft: &TableDraft,
    set: &ChangeSet,
    opts: &RenderOptions,
) -> Result<AlterScript, crate::alter::script::AlterError> {
    let mut warnings = vec![
        "Each ALTER TABLE auto-commits on MySQL/MariaDB; the script cannot be rolled back as a unit"
            .to_string(),
    ];
    let mut manual = set.manual_review.clone();
    for idx_change in collect_changes(set, is_add_index) {
        if let Change::AddIndex(idx) = idx_change {
            if !idx.include.is_empty() || idx.filter.is_some() {
                manual.push(format!(
                    "Index {} uses INCLUDE or filter syntax not supported on MySQL/MariaDB",
                    idx.name
                ));
            }
            if idx.fillfactor.is_some() {
                warnings.push(format!(
                    "Index {}: fillfactor is not supported on MySQL/MariaDB; omitted",
                    idx.name
                ));
            }
        }
    }

    let table_q = qualified_table(db, set.table.schema.as_deref(), &set.table.table);
    let mut clauses: Vec<String> = Vec::new();

    for c in collect_changes(set, is_drop_fk) {
        if let Change::DropForeignKey { name } = c {
            clauses.push(format!("DROP FOREIGN KEY {}", db.quote_identifier(name)));
        }
    }
    for c in collect_changes(set, is_drop_index) {
        if let Change::DropIndex { name } = c {
            clauses.push(format!("DROP INDEX {}", db.quote_identifier(name)));
        }
    }
    for c in collect_changes(set, is_replace_pk) {
        if let Change::ReplacePrimaryKey { from, .. } = c
            && !from.is_empty()
        {
            clauses.push("DROP PRIMARY KEY".to_string());
        }
    }
    // One clause per rebuilt column: CHANGE when renamed, MODIFY otherwise.
    // Each restates the draft's whole definition, so a rename + retype +
    // default edit on one column is a single clause (MySQL rejects a MODIFY
    // of a name the same statement just CHANGEd away), and the column's
    // description rides in that clause — a separate comment restatement
    // after it would replay the PRE-edit definition and revert the change.
    let mut rebuilt: Vec<(String, Option<String>, Option<ColumnDef>)> = Vec::new();
    for c in &set.changes {
        let (name, renamed_from, fallback) = match c {
            Change::RenameColumn { from, to } => (to, Some(from.clone()), None),
            Change::AlterColumnType { name, to, .. } => (name, None, Some(to.clone())),
            Change::SetDefault { name, .. } => (name, None, None),
            _ => continue,
        };
        match rebuilt.iter_mut().find(|(n, _, _)| n == name) {
            Some(entry) => {
                if renamed_from.is_some() {
                    entry.1 = renamed_from;
                }
                if entry.2.is_none() {
                    entry.2 = fallback;
                }
            }
            None => rebuilt.push((name.clone(), renamed_from, fallback)),
        }
    }
    let mut folded: BTreeSet<String> = BTreeSet::new();
    for (name, renamed_from, fallback) in &rebuilt {
        let Some(def) = find_draft_column(draft, name).cloned().or(fallback.clone()) else {
            manual.push(format!(
                "Column {name}: its new definition could not be resolved; alter it by hand"
            ));
            continue;
        };
        let origin = renamed_from.as_deref().unwrap_or(name);
        let source_line = draft
            .source_ddl
            .as_deref()
            .and_then(|ddl| comments::mysql_column_definition_line(ddl, origin));
        let extras = snapshot.column_extras.get(origin);
        let comment = rebuilt_comment(
            snapshot,
            draft,
            name,
            source_line.as_deref(),
            db,
            &mut warnings,
        );
        let on_update = carried_on_update(&def, extras, source_line.as_deref());
        if let Some(line) = &source_line {
            warn_dropped_attributes(&def, line, &mut warnings);
        }
        let definition = column_definition(db, &def, comment.as_deref(), on_update.as_deref());
        clauses.push(match renamed_from {
            Some(from) => format!(
                "CHANGE COLUMN {} {} {definition}",
                db.quote_identifier(from),
                db.quote_identifier(name)
            ),
            None => format!("MODIFY COLUMN {} {definition}", db.quote_identifier(name)),
        });
        folded.insert(name.clone());
    }
    for c in collect_changes(set, is_drop_col) {
        if let Change::DropColumn { name } = c {
            clauses.push(format!("DROP COLUMN {}", db.quote_identifier(name)));
        }
    }
    for c in collect_changes(set, is_add_col) {
        if let Change::AddColumn(col) = c {
            let comment = draft
                .column_comments
                .get(&col.name)
                .filter(|text| !text.is_empty())
                .map(|text| format!("COMMENT {}", comments::string_literal(db, text)));
            clauses.push(format!(
                "ADD COLUMN {} {}",
                db.quote_identifier(&col.name),
                column_definition(db, col, comment.as_deref(), None)
            ));
            folded.insert(col.name.clone());
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
            clauses.push(format!("ADD PRIMARY KEY ({cols})"));
        }
    }
    for c in collect_changes(set, is_add_index) {
        if let Change::AddIndex(idx) = c {
            clauses.push(render_add_index(db, idx, &mut warnings));
        }
    }
    for c in collect_changes(set, is_add_fk) {
        if let Change::AddForeignKey(fk) = c {
            clauses.push(render_add_fk(db, fk));
        }
    }

    let mut body = String::new();
    if !clauses.is_empty() {
        let suffix = mysql_suffix(opts);
        body.push_str(&format!(
            "ALTER TABLE {table_q}\n    {clauses}{suffix};\n",
            clauses = clauses.join(",\n    "),
            suffix = suffix
        ));
    }

    // Comment-only changes restate the engine's own definition; columns
    // rebuilt or added above already carry their COMMENT.
    let mut comment_set = set.clone();
    comment_set.changes.retain(
        |c| !matches!(c, Change::SetComment { column: Some(col), .. } if folded.contains(col)),
    );
    body.push_str(&render_comment_changes(db, snapshot, &comment_set, draft)?);

    let (view_drop, view_create) = apply_view_wrap(db, snapshot, set, &mut warnings, &mut manual);

    Ok(AlterScript {
        sql: header(
            set.table.schema.as_deref(),
            &set.table.table,
            &warnings,
            &manual,
        ) + &view_drop
            + &body
            + &view_create,
        warnings,
    })
}

/// `type [NOT] NULL [DEFAULT …] [ON UPDATE …] [AUTO_INCREMENT] [COMMENT '…']`.
/// `comment` is the whole `COMMENT '…'` clause, already rendered.
fn column_definition(
    db: DatabaseType,
    col: &ColumnDef,
    comment: Option<&str>,
    on_update: Option<&str>,
) -> String {
    let _ = db;
    let mut s = col.data_type.clone();
    if col.nullable {
        s.push_str(" NULL");
    } else {
        s.push_str(" NOT NULL");
    }
    if let Some(def) = &col.default {
        s.push_str(&format!(" DEFAULT {}", def));
    }
    if let Some(expr) = on_update {
        s.push_str(&format!(" ON UPDATE {expr}"));
    }
    if col.identity {
        s.push_str(" AUTO_INCREMENT");
    }
    if let Some(clause) = comment {
        s.push(' ');
        s.push_str(clause);
    }
    s
}

/// The `COMMENT '…'` clause a rebuilt column must carry. Normally the
/// draft's description (the user may have edited it in the same change).
/// When the descriptions could not be read, the draft knows nothing, so the
/// clause is copied verbatim from the engine's own definition — restating
/// no COMMENT would silently clear it.
fn rebuilt_comment(
    snapshot: &TableSnapshot,
    draft: &TableDraft,
    name: &str,
    source_line: Option<&str>,
    db: DatabaseType,
    warnings: &mut Vec<String>,
) -> Option<String> {
    if snapshot.comment_read_error.is_none() {
        return draft
            .column_comments
            .get(name)
            .filter(|text| !text.is_empty())
            .map(|text| format!("COMMENT {}", comments::string_literal(db, text)));
    }
    match source_line {
        Some(line) => comments::mysql_comment_clause(line).map(str::to_string),
        None => {
            warnings.push(format!(
                "Column {name}: its description could not be read, so the rebuilt definition may clear it"
            ));
            None
        }
    }
}

/// Keep a rebuilt temporal column's `ON UPDATE` (MySQL drops it on MODIFY
/// otherwise). Read from the catalog extras, else the engine's definition.
fn carried_on_update(
    def: &ColumnDef,
    extras: Option<&ColumnExtras>,
    source_line: Option<&str>,
) -> Option<String> {
    let ty = def.data_type.trim().to_ascii_lowercase();
    if !(ty.starts_with("timestamp") || ty.starts_with("datetime")) {
        return None;
    }
    if let Some(expr) = extras.and_then(|e| e.on_update.clone()) {
        return Some(expr);
    }
    let line = source_line?;
    let at = comments::mysql_top_level_word(line, "ON UPDATE")?;
    line[at + "ON UPDATE ".len()..]
        .split_whitespace()
        .next()
        .map(|token| token.trim_end_matches(',').to_string())
}

/// Warn when the engine's current definition carries attributes the
/// editor's rebuilt definition does not restate.
fn warn_dropped_attributes(def: &ColumnDef, line: &str, warnings: &mut Vec<String>) {
    let typed = def.data_type.to_ascii_uppercase();
    let dropped: Vec<&str> = [
        ("CHARACTER SET", "CHARACTER SET"),
        ("COLLATE", "COLLATE"),
        ("GENERATED", "a generated expression"),
        ("INVISIBLE", "INVISIBLE"),
        ("SRID", "SRID"),
        ("CHECK", "a column CHECK"),
    ]
    .into_iter()
    .filter(|(word, _)| {
        comments::mysql_top_level_word(line, word).is_some() && !typed.contains(word)
    })
    .map(|(_, label)| label)
    .collect();
    if !dropped.is_empty() {
        warnings.push(format!(
            "Column {}: the rebuilt definition does not restate {} from the current definition; add it to the type if it must stay",
            def.name,
            dropped.join(", ")
        ));
    }
}

fn mysql_suffix(opts: &RenderOptions) -> String {
    let mut parts = Vec::new();
    if let Some(alg) = opts.mysql_algorithm {
        parts.push(match alg {
            MysqlAlgorithm::Instant => "ALGORITHM=INSTANT",
            MysqlAlgorithm::Inplace => "ALGORITHM=INPLACE",
            MysqlAlgorithm::Copy => "ALGORITHM=COPY",
        });
    }
    if let Some(lock) = opts.mysql_lock {
        parts.push(match lock {
            MysqlLock::None => "LOCK=NONE",
            MysqlLock::Shared => "LOCK=SHARED",
            MysqlLock::Exclusive => "LOCK=EXCLUSIVE",
        });
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(", {}", parts.join(", "))
    }
}

pub(crate) fn render_add_index(
    db: DatabaseType,
    idx: &IndexDef,
    warnings: &mut Vec<String>,
) -> String {
    let unique = if idx.unique { "UNIQUE " } else { "" };
    let using = match idx.method.as_deref() {
        Some(m) if !m.eq_ignore_ascii_case("BTREE") => {
            if m.eq_ignore_ascii_case("HASH") {
                format!(" USING {m}")
            } else {
                warnings.push(format!(
                    "Index {}: method is not supported on MySQL/MariaDB; omitted",
                    idx.name
                ));
                String::new()
            }
        }
        _ => String::new(),
    };
    let cols = idx
        .columns
        .iter()
        .map(|c| db.quote_identifier(c))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "ADD {unique}INDEX {} ({cols}){using}",
        db.quote_identifier(&idx.name),
        unique = unique,
        cols = cols,
        using = using
    )
}

fn render_add_fk(db: DatabaseType, fk: &ForeignKeyDef) -> String {
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
