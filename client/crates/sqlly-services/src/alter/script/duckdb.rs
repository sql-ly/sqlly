//! DuckDB ALTER script generation.
//!
//! DuckDB alters one thing per statement (no comma-separated clause list),
//! supports ADD / DROP / RENAME COLUMN, ALTER COLUMN … TYPE (with `USING`),
//! SET / DROP DEFAULT and SET / DROP NOT NULL, can add a primary key to a
//! table that has none, and creates and drops indexes — but cannot add or
//! drop foreign keys, unique constraints, or an existing primary key in
//! place (the table must be recreated), and refuses to add a column that
//! carries a constraint, so NOT NULL on a new column is applied in a second
//! statement. Those limits become MANUAL REVIEW lines in the script header
//! instead of statements that would fail. DDL is transactional, so the body
//! runs inside one transaction.

use sqlly_lang::sqlmodel::DatabaseType;

use crate::alter::diff::{Change, ChangeSet};
use crate::alter::script::{
    AlterScript, RenderOptions, apply_view_wrap, collect_changes, header, is_add_col, is_add_fk,
    is_add_index, is_alter_type, is_drop_col, is_drop_fk, is_drop_index, is_rename, is_replace_pk,
    is_set_default, qualified_table, render_comment_changes,
};
use crate::alter::snapshot::{IndexDef, TableDraft, TableSnapshot};

pub fn render(
    db: DatabaseType,
    snapshot: &TableSnapshot,
    draft: &TableDraft,
    set: &ChangeSet,
    opts: &RenderOptions,
) -> Result<AlterScript, crate::alter::script::AlterError> {
    let mut warnings = Vec::new();
    let mut manual_review = set.manual_review.clone();
    let table_q = qualified_table(db, set.table.schema.as_deref(), &set.table.table);
    let index_q = |name: &str| match set.table.schema.as_deref().filter(|s| !s.is_empty()) {
        Some(s) => format!("{}.{}", db.quote_identifier(s), db.quote_identifier(name)),
        None => db.quote_identifier(name),
    };

    let touches_indexed_columns = !snapshot.indexes.is_empty()
        && set.changes.iter().any(|c| {
            matches!(
                c,
                Change::RenameColumn { .. }
                    | Change::AlterColumnType { .. }
                    | Change::DropColumn { .. }
            )
        });
    if touches_indexed_columns {
        warnings.push(
            "DuckDB refuses to rename, retype, or drop a column that an index depends on; drop that index first and recreate it afterwards"
                .to_string(),
        );
    }

    let mut body = String::new();

    for c in collect_changes(set, is_rename) {
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
            body.push_str(&format!("DROP INDEX {};\n", index_q(name)));
        }
    }

    for c in collect_changes(set, is_drop_fk) {
        if let Change::DropForeignKey { name } = c {
            manual_review.push(format!(
                "DuckDB cannot drop foreign key {name} in place; recreate the table without it"
            ));
        }
    }

    for c in collect_changes(set, is_set_default) {
        if let Change::SetDefault { name, from, to, .. } = c {
            if from.is_some() && to.is_none() {
                body.push_str(&format!(
                    "ALTER TABLE {table_q} ALTER COLUMN {} DROP DEFAULT;\n",
                    db.quote_identifier(name)
                ));
            }
            if let Some(def) = to {
                body.push_str(&format!(
                    "ALTER TABLE {table_q} ALTER COLUMN {} SET DEFAULT {def};\n",
                    db.quote_identifier(name)
                ));
            }
        }
    }

    for c in collect_changes(set, is_alter_type) {
        if let Change::AlterColumnType { name, from, to } = c {
            if from.data_type != to.data_type {
                let column_q = db.quote_identifier(name);
                match opts.pg_using.get(name) {
                    Some(using) => body.push_str(&format!(
                        "ALTER TABLE {table_q} ALTER COLUMN {column_q} TYPE {} USING {using};\n",
                        to.data_type
                    )),
                    None => body.push_str(&format!(
                        "ALTER TABLE {table_q} ALTER COLUMN {column_q} TYPE {};\n",
                        to.data_type
                    )),
                }
            }
            if from.nullable != to.nullable {
                body.push_str(&format!(
                    "ALTER TABLE {table_q} ALTER COLUMN {} {};\n",
                    db.quote_identifier(name),
                    if to.nullable {
                        "DROP NOT NULL"
                    } else {
                        "SET NOT NULL"
                    }
                ));
            }
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
            let column_q = db.quote_identifier(&col.name);
            let mut clause = format!(
                "ALTER TABLE {table_q} ADD COLUMN {column_q} {}",
                col.data_type
            );
            if let Some(def) = &col.default {
                clause.push_str(&format!(" DEFAULT {def}"));
            }
            clause.push_str(";\n");
            body.push_str(&clause);
            if !col.nullable {
                // A new column cannot carry a constraint in the ADD itself.
                body.push_str(&format!(
                    "ALTER TABLE {table_q} ALTER COLUMN {column_q} SET NOT NULL;\n"
                ));
                if col.default.is_none() {
                    warnings.push(format!(
                        "Column {}: SET NOT NULL fails if the table already has rows, because the new column has no default",
                        col.name
                    ));
                }
            }
        }
    }

    for c in collect_changes(set, is_replace_pk) {
        if let Change::ReplacePrimaryKey { from, to, .. } = c {
            if from.is_empty() && !to.is_empty() {
                let cols = to
                    .iter()
                    .map(|c| db.quote_identifier(c))
                    .collect::<Vec<_>>()
                    .join(", ");
                body.push_str(&format!(
                    "ALTER TABLE {table_q} ADD PRIMARY KEY ({cols});\n"
                ));
            } else {
                manual_review.push(format!(
                    "DuckDB cannot drop or replace an existing primary key in place; recreate {} with PRIMARY KEY ({})",
                    set.table.table,
                    to.join(", ")
                ));
            }
        }
    }

    for c in collect_changes(set, is_add_fk) {
        if let Change::AddForeignKey(fk) = c {
            manual_review.push(format!(
                "DuckDB cannot add foreign key {} in place; recreate the table with FOREIGN KEY ({}) REFERENCES {} ({})",
                fk.name,
                fk.columns.join(", "),
                qualified_table(db, fk.ref_schema.as_deref(), &fk.ref_table),
                fk.ref_columns.join(", ")
            ));
        }
    }

    for c in collect_changes(set, is_add_index) {
        if let Change::AddIndex(idx) = c {
            body.push_str(&render_create_index(db, &table_q, idx, &mut warnings));
        }
    }

    body.push_str(&render_comment_changes(db, snapshot, set, draft)?);

    let (view_drop, view_create) =
        apply_view_wrap(db, snapshot, set, &mut warnings, &mut manual_review);

    let mut sql = header(
        set.table.schema.as_deref(),
        &set.table.table,
        &warnings,
        &manual_review,
    );
    sql.push_str("BEGIN TRANSACTION;\n");
    sql.push_str(&view_drop);
    sql.push_str(&body);
    sql.push_str(&view_create);
    sql.push_str("COMMIT;\n");

    Ok(AlterScript { sql, warnings })
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
    if !idx.include.is_empty() {
        warnings.push(format!(
            "Index {}: DuckDB indexes have no INCLUDE columns; omitted",
            idx.name
        ));
    }
    if idx.filter.is_some() {
        warnings.push(format!(
            "Index {}: DuckDB has no partial (filtered) indexes; the filter was omitted",
            idx.name
        ));
    }
    if idx.fillfactor.is_some() {
        warnings.push(format!(
            "Index {}: fill factor does not apply to DuckDB indexes; omitted",
            idx.name
        ));
    }
    format!(
        "CREATE {unique}INDEX {} ON {table_q} ({cols});\n",
        db.quote_identifier(&idx.name)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alter::diff::diff;
    use crate::alter::snapshot::{ColumnDef, DraftColumn, ForeignKeyDef};

    fn column(name: &str, data_type: &str, nullable: bool) -> ColumnDef {
        ColumnDef {
            name: name.into(),
            data_type: data_type.into(),
            nullable,
            identity: false,
            default: None,
            default_constraint_name: None,
        }
    }

    fn snapshot() -> TableSnapshot {
        TableSnapshot {
            schema: Some("main".into()),
            table: "person".into(),
            columns: vec![
                column("id", "INTEGER", false),
                column("name", "VARCHAR", true),
                column("age", "INTEGER", true),
            ],
            primary_key: vec!["id".into()],
            pk_constraint_name: None,
            indexes: vec![],
            foreign_keys: vec![],
            stored_index_ddl: vec![],
            stored_trigger_ddl: vec![],
            sqlite_foreign_keys_on: false,
            referenced_by: vec![],
            dependent_views: vec![],
            table_comment: None,
            column_comments: Default::default(),
            stored_table_ddl: None,
            ..Default::default()
        }
    }

    fn draft_of(snapshot: &TableSnapshot) -> TableDraft {
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
            pk_constraint_name: None,
            indexes: vec![],
            foreign_keys: vec![],
            table_comment: snapshot.table_comment.clone(),
            column_comments: snapshot.column_comments.clone(),
            source_ddl: snapshot.stored_table_ddl.clone(),
        }
    }

    fn render_duckdb(snapshot: &TableSnapshot, draft: &TableDraft) -> AlterScript {
        let set = diff(snapshot, draft);
        crate::alter::script::render(
            DatabaseType::DuckDb,
            snapshot,
            draft,
            &set,
            &RenderOptions::default(),
        )
        .unwrap()
    }

    #[test]
    fn column_changes_render_one_statement_each_inside_a_transaction() {
        let snapshot = snapshot();
        let mut draft = draft_of(&snapshot);
        // Rename age → years, retype it, make name NOT NULL with a default,
        // drop nothing, add a nullable note and a NOT NULL flag with default.
        draft.columns[2].def.name = "years".into();
        draft.columns[2].def.data_type = "BIGINT".into();
        draft.columns[1].def.nullable = false;
        draft.columns[1].def.default = Some("'anonymous'".into());
        draft.columns.push(DraftColumn {
            origin: None,
            def: column("note", "VARCHAR", true),
        });
        draft.columns.push(DraftColumn {
            origin: None,
            def: ColumnDef {
                default: Some("true".into()),
                ..column("active", "BOOLEAN", false)
            },
        });
        let script = render_duckdb(&snapshot, &draft);
        let sql = script.sql;
        let lines: Vec<&str> = sql.lines().filter(|l| !l.starts_with("--")).collect();
        assert_eq!(
            lines,
            vec![
                "BEGIN TRANSACTION;",
                "ALTER TABLE \"main\".\"person\" RENAME COLUMN \"age\" TO \"years\";",
                "ALTER TABLE \"main\".\"person\" ALTER COLUMN \"name\" SET DEFAULT 'anonymous';",
                "ALTER TABLE \"main\".\"person\" ALTER COLUMN \"name\" SET NOT NULL;",
                "ALTER TABLE \"main\".\"person\" ALTER COLUMN \"years\" TYPE BIGINT;",
                "ALTER TABLE \"main\".\"person\" ADD COLUMN \"note\" VARCHAR;",
                "ALTER TABLE \"main\".\"person\" ADD COLUMN \"active\" BOOLEAN DEFAULT true;",
                "ALTER TABLE \"main\".\"person\" ALTER COLUMN \"active\" SET NOT NULL;",
                "COMMIT;",
            ],
            "{sql}"
        );
        assert!(script.warnings.is_empty(), "{:?}", script.warnings);
    }

    #[test]
    fn using_expression_and_indexed_column_warning() {
        let mut snapshot = snapshot();
        snapshot.indexes.push(IndexDef {
            name: "ix_age".into(),
            columns: vec!["age".into()],
            unique: false,
            include: vec![],
            filter: None,
            method: None,
            fillfactor: None,
        });
        let mut draft = draft_of(&snapshot);
        draft.indexes = snapshot.indexes.clone();
        draft.columns[2].def.data_type = "VARCHAR".into();
        let set = diff(&snapshot, &draft);
        let mut opts = RenderOptions::default();
        opts.pg_using
            .insert("age".into(), "CAST(\"age\" AS VARCHAR)".into());
        let script = render(DatabaseType::DuckDb, &snapshot, &draft, &set, &opts).unwrap();
        assert!(
            script.sql.contains(
                "ALTER TABLE \"main\".\"person\" ALTER COLUMN \"age\" TYPE VARCHAR USING CAST(\"age\" AS VARCHAR);"
            ),
            "{}",
            script.sql
        );
        assert!(
            script
                .warnings
                .iter()
                .any(|w| w.contains("index depends on")),
            "{:?}",
            script.warnings
        );
    }

    #[test]
    fn constraints_duckdb_cannot_alter_become_manual_review() {
        let snapshot = snapshot();
        let mut draft = draft_of(&snapshot);
        draft.primary_key = vec!["id".into(), "name".into()];
        draft.foreign_keys.push(ForeignKeyDef {
            name: "fk_person_team".into(),
            columns: vec!["age".into()],
            ref_schema: Some("main".into()),
            ref_table: "team".into(),
            ref_columns: vec!["id".into()],
            on_delete: None,
            on_update: None,
        });
        draft.indexes.push(IndexDef {
            name: "ux_name".into(),
            columns: vec!["name".into()],
            unique: true,
            include: vec!["age".into()],
            filter: Some("name IS NOT NULL".into()),
            method: None,
            fillfactor: Some(80),
        });
        let script = render_duckdb(&snapshot, &draft);
        assert!(
            script.sql.contains(
                "-- MANUAL REVIEW: DuckDB cannot drop or replace an existing primary key"
            ),
            "{}",
            script.sql
        );
        assert!(
            script.sql.contains("-- MANUAL REVIEW: DuckDB cannot add foreign key fk_person_team in place; recreate the table with FOREIGN KEY (age) REFERENCES \"main\".\"team\" (id)"),
            "{}",
            script.sql
        );
        let statements: Vec<&str> = script
            .sql
            .lines()
            .filter(|l| !l.starts_with("--"))
            .collect();
        assert_eq!(
            statements,
            vec![
                "BEGIN TRANSACTION;",
                "CREATE UNIQUE INDEX \"ux_name\" ON \"main\".\"person\" (\"name\");",
                "COMMIT;",
            ],
            "{}",
            script.sql
        );
        assert_eq!(script.warnings.len(), 3, "{:?}", script.warnings);
    }

    #[test]
    fn adding_a_first_primary_key_and_dropping_columns_and_indexes() {
        let mut snapshot = snapshot();
        snapshot.primary_key = vec![];
        snapshot.indexes.push(IndexDef {
            name: "ix_name".into(),
            columns: vec!["name".into()],
            unique: false,
            include: vec![],
            filter: None,
            method: None,
            fillfactor: None,
        });
        let mut draft = draft_of(&snapshot);
        draft.primary_key = vec!["id".into()];
        draft.columns.retain(|c| c.def.name != "age");
        let script = render_duckdb(&snapshot, &draft);
        let lines: Vec<&str> = script
            .sql
            .lines()
            .filter(|l| !l.starts_with("--"))
            .collect();
        assert_eq!(
            lines,
            vec![
                "BEGIN TRANSACTION;",
                "DROP INDEX \"main\".\"ix_name\";",
                "ALTER TABLE \"main\".\"person\" DROP COLUMN \"age\";",
                "ALTER TABLE \"main\".\"person\" ADD PRIMARY KEY (\"id\");",
                "COMMIT;",
            ],
            "{}",
            script.sql
        );
    }
}
