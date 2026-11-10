//! Oracle Database ALTER script generation.
//!
//! Oracle has no transactional DDL (every statement commits on its own) and
//! no combined `ALTER TABLE … ADD …, DROP …` form, so each change family is
//! its own statement: `RENAME COLUMN`, `MODIFY (…)`, `DROP (…)`, `ADD (…)`,
//! `ADD/DROP CONSTRAINT`, and standalone `CREATE INDEX` / `DROP INDEX`.
//! Dependent views are never dropped: Oracle invalidates them and recompiles
//! them on next use, so the script only notes that they exist.

use sqlly_lang::sqlmodel::DatabaseType;

use crate::alter::diff::{Change, ChangeSet, touches_columns};
use crate::alter::script::{
    AlterScript, RenderOptions, collect_changes, header, is_add_col, is_add_fk, is_add_index,
    is_alter_type, is_drop_col, is_drop_fk, is_drop_index, is_rename, is_replace_pk,
    is_set_default, qualified_table, render_comment_changes,
};
use crate::alter::snapshot::{ColumnDef, ForeignKeyDef, IndexDef, TableDraft, TableSnapshot};

pub fn render(
    db: DatabaseType,
    snapshot: &TableSnapshot,
    draft: &TableDraft,
    set: &ChangeSet,
    _opts: &RenderOptions,
) -> Result<AlterScript, crate::alter::script::AlterError> {
    let mut warnings = vec![
        "Each DDL statement commits on its own on Oracle; the script cannot be rolled back as a unit"
            .to_string(),
    ];
    let mut manual = set.manual_review.clone();

    if touches_columns(set) && !snapshot.dependent_views.is_empty() {
        let names = snapshot
            .dependent_views
            .iter()
            .map(|v| qualified_table(db, v.schema.as_deref(), &v.name))
            .collect::<Vec<_>>()
            .join(", ");
        warnings.push(format!(
            "Dependent views ({names}) are invalidated by this change; Oracle recompiles them on next use — run them afterwards to confirm they still compile"
        ));
    }

    for c in collect_changes(set, is_add_index) {
        if let Change::AddIndex(idx) = c {
            if !idx.include.is_empty() {
                manual.push(format!(
                    "Index {} uses INCLUDE columns, which Oracle does not support; add them as key columns if needed",
                    idx.name
                ));
            }
            if idx.filter.is_some() {
                manual.push(format!(
                    "Index {} has a filter; Oracle has no partial indexes (use a function-based index instead)",
                    idx.name
                ));
            }
            if idx.fillfactor.is_some() {
                warnings.push(format!(
                    "Index {}: fillfactor is not supported on Oracle (use PCTFREE); omitted",
                    idx.name
                ));
            }
        }
    }
    for c in collect_changes(set, is_add_fk) {
        if let Change::AddForeignKey(fk) = c
            && fk.on_update.is_some()
        {
            warnings.push(format!(
                "Foreign key {}: Oracle has no ON UPDATE action; omitted",
                fk.name
            ));
        }
    }

    let table_q = qualified_table(db, set.table.schema.as_deref(), &set.table.table);
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
            body.push_str(&format!(
                "DROP INDEX {};\n",
                qualified_index(db, set.table.schema.as_deref(), name)
            ));
        }
    }

    for c in collect_changes(set, is_drop_fk) {
        if let Change::DropForeignKey { name } = c {
            body.push_str(&format!(
                "ALTER TABLE {table_q} DROP CONSTRAINT {};\n",
                db.quote_identifier(name)
            ));
        }
    }
    for c in collect_changes(set, is_replace_pk) {
        if let Change::ReplacePrimaryKey {
            from,
            constraint_name,
            ..
        } = c
            && !from.is_empty()
        {
            match constraint_name {
                Some(cn) => body.push_str(&format!(
                    "ALTER TABLE {table_q} DROP CONSTRAINT {};\n",
                    db.quote_identifier(cn)
                )),
                None => body.push_str(&format!("ALTER TABLE {table_q} DROP PRIMARY KEY;\n")),
            }
        }
    }

    for c in collect_changes(set, is_alter_type) {
        if let Change::AlterColumnType { name, from, to } = c {
            body.push_str(&format!(
                "ALTER TABLE {table_q} MODIFY ({});\n",
                modify_clause(db, name, from, to)
            ));
        }
    }
    for c in collect_changes(set, is_set_default) {
        if let Change::SetDefault { name, to, .. } = c {
            body.push_str(&format!(
                "ALTER TABLE {table_q} MODIFY ({} DEFAULT {});\n",
                db.quote_identifier(name),
                to.as_deref().unwrap_or("NULL")
            ));
        }
    }

    let dropped: Vec<String> = collect_changes(set, is_drop_col)
        .into_iter()
        .filter_map(|c| match c {
            Change::DropColumn { name } => Some(db.quote_identifier(name)),
            _ => None,
        })
        .collect();
    if !dropped.is_empty() {
        body.push_str(&format!(
            "ALTER TABLE {table_q} DROP ({});\n",
            dropped.join(", ")
        ));
    }

    let added: Vec<String> = collect_changes(set, is_add_col)
        .into_iter()
        .filter_map(|c| match c {
            Change::AddColumn(col) => Some(column_definition(db, col)),
            _ => None,
        })
        .collect();
    if !added.is_empty() {
        body.push_str(&format!(
            "ALTER TABLE {table_q} ADD ({});\n",
            added.join(", ")
        ));
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
                "ALTER TABLE {table_q} ADD CONSTRAINT {} PRIMARY KEY ({cols});\n",
                db.quote_identifier(&format!("PK_{}", set.table.table))
            ));
        }
    }
    for c in collect_changes(set, is_add_fk) {
        if let Change::AddForeignKey(fk) = c {
            body.push_str(&format!(
                "ALTER TABLE {table_q} {};\n",
                render_fk_clause(db, fk)
            ));
        }
    }
    for c in collect_changes(set, is_add_index) {
        if let Change::AddIndex(idx) = c {
            body.push_str(&render_create_index(db, &table_q, idx));
        }
    }

    body.push_str(&render_comment_changes(db, snapshot, set, draft)?);

    Ok(AlterScript {
        sql: header(
            set.table.schema.as_deref(),
            &set.table.table,
            &warnings,
            &manual,
        ) + &body,
        warnings,
    })
}

/// `"schema"."index"` (or just `"index"` without a schema).
fn qualified_index(db: DatabaseType, schema: Option<&str>, name: &str) -> String {
    match schema.filter(|s| !s.is_empty()) {
        Some(s) => format!("{}.{}", db.quote_identifier(s), db.quote_identifier(name)),
        None => db.quote_identifier(name),
    }
}

/// `"col" type [DEFAULT x] [NOT NULL]` for `ADD (…)`.
fn column_definition(db: DatabaseType, col: &ColumnDef) -> String {
    let mut out = format!("{} {}", db.quote_identifier(&col.name), col.data_type);
    if let Some(def) = &col.default {
        out.push_str(&format!(" DEFAULT {def}"));
    }
    if !col.nullable {
        out.push_str(" NOT NULL");
    }
    out
}

/// The `MODIFY (…)` body for a type and/or nullability change. Oracle rejects
/// `MODIFY (c NOT NULL)` when the column already is (ORA-01442), so the
/// null clause appears only when nullability actually changed, and the type
/// only when it did.
fn modify_clause(db: DatabaseType, name: &str, from: &ColumnDef, to: &ColumnDef) -> String {
    let mut out = db.quote_identifier(name);
    if !from.data_type.eq_ignore_ascii_case(&to.data_type) {
        out.push(' ');
        out.push_str(&to.data_type);
    }
    if from.nullable != to.nullable {
        out.push_str(if to.nullable { " NULL" } else { " NOT NULL" });
    }
    out
}

pub(crate) fn render_create_index(db: DatabaseType, table_q: &str, idx: &IndexDef) -> String {
    let unique = if idx.unique { "UNIQUE " } else { "" };
    let cols = idx
        .columns
        .iter()
        .map(|c| db.quote_identifier(c))
        .collect::<Vec<_>>()
        .join(", ");
    let bitmap = matches!(idx.method.as_deref(), Some(m) if m.eq_ignore_ascii_case("bitmap"));
    let kind = if bitmap { "BITMAP " } else { unique };
    format!(
        "CREATE {kind}INDEX {} ON {table_q} ({cols});\n",
        db.quote_identifier(&idx.name)
    )
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
        db.quote_identifier(&fk.name)
    );
    // Oracle supports only ON DELETE CASCADE / SET NULL; NO ACTION is the
    // default and cannot be spelled out.
    if let Some(action) = fk.on_delete.as_deref()
        && matches!(action.to_ascii_uppercase().as_str(), "CASCADE" | "SET NULL")
    {
        sql.push_str(&format!(" ON DELETE {}", action.to_ascii_uppercase()));
    }
    sql
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alter::diff::TableRef;

    fn col(name: &str, data_type: &str, nullable: bool, default: Option<&str>) -> ColumnDef {
        ColumnDef {
            name: name.into(),
            data_type: data_type.into(),
            nullable,
            identity: false,
            default: default.map(str::to_string),
            default_constraint_name: None,
        }
    }

    fn snapshot() -> TableSnapshot {
        TableSnapshot {
            schema: Some("HR".into()),
            table: "EMP".into(),
            columns: vec![col("ID", "NUMBER(10)", false, None)],
            primary_key: vec!["ID".into()],
            pk_constraint_name: Some("EMP_PK".into()),
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

    fn draft(snapshot: &TableSnapshot) -> TableDraft {
        TableDraft {
            schema: snapshot.schema.clone(),
            table: snapshot.table.clone(),
            columns: vec![],
            primary_key: snapshot.primary_key.clone(),
            pk_constraint_name: snapshot.pk_constraint_name.clone(),
            indexes: vec![],
            foreign_keys: vec![],
            table_comment: None,
            column_comments: Default::default(),
            source_ddl: None,
        }
    }

    fn set(changes: Vec<Change>) -> ChangeSet {
        ChangeSet {
            table: TableRef {
                schema: Some("HR".into()),
                table: "EMP".into(),
            },
            changes,
            manual_review: vec![],
        }
    }

    fn render_sql(changes: Vec<Change>) -> String {
        let s = snapshot();
        let d = draft(&s);
        render(
            DatabaseType::Oracle,
            &s,
            &d,
            &set(changes),
            &RenderOptions::default(),
        )
        .unwrap()
        .sql
    }

    #[test]
    fn add_drop_rename_and_modify_are_separate_statements() {
        let sql = render_sql(vec![
            Change::RenameColumn {
                from: "OLD".into(),
                to: "NEW".into(),
            },
            Change::AlterColumnType {
                name: "NEW".into(),
                from: col("NEW", "VARCHAR2(20)", true, None),
                to: col("NEW", "VARCHAR2(50)", false, None),
            },
            Change::SetDefault {
                name: "NEW".into(),
                from: None,
                to: Some("'x'".into()),
                constraint_name: None,
            },
            Change::DropColumn {
                name: "GONE".into(),
            },
            Change::DropColumn {
                name: "GONE2".into(),
            },
            Change::AddColumn(col("NOTE", "CLOB", true, None)),
            Change::AddColumn(col("CREATED", "TIMESTAMP", false, Some("SYSTIMESTAMP"))),
        ]);
        assert!(sql.contains("ALTER TABLE \"HR\".\"EMP\" RENAME COLUMN \"OLD\" TO \"NEW\";\n"));
        assert!(
            sql.contains("ALTER TABLE \"HR\".\"EMP\" MODIFY (\"NEW\" VARCHAR2(50) NOT NULL);\n")
        );
        assert!(sql.contains("ALTER TABLE \"HR\".\"EMP\" MODIFY (\"NEW\" DEFAULT 'x');\n"));
        assert!(sql.contains("ALTER TABLE \"HR\".\"EMP\" DROP (\"GONE\", \"GONE2\");\n"));
        assert!(sql.contains(
            "ALTER TABLE \"HR\".\"EMP\" ADD (\"NOTE\" CLOB, \"CREATED\" TIMESTAMP DEFAULT SYSTIMESTAMP NOT NULL);\n"
        ));
        assert!(!sql.contains("BEGIN;"));
        assert!(sql.contains("-- WARNING: Each DDL statement commits on its own on Oracle"));
    }

    #[test]
    fn modify_only_spells_what_changed() {
        assert_eq!(
            modify_clause(
                DatabaseType::Oracle,
                "C",
                &col("C", "NUMBER", true, None),
                &col("C", "NUMBER", false, None)
            ),
            "\"C\" NOT NULL"
        );
        assert_eq!(
            modify_clause(
                DatabaseType::Oracle,
                "C",
                &col("C", "NUMBER", false, None),
                &col("C", "VARCHAR2(10)", false, None)
            ),
            "\"C\" VARCHAR2(10)"
        );
        assert_eq!(
            modify_clause(
                DatabaseType::Oracle,
                "C",
                &col("C", "NUMBER", false, None),
                &col("C", "VARCHAR2(10)", true, None)
            ),
            "\"C\" VARCHAR2(10) NULL"
        );
    }

    #[test]
    fn dropping_a_default_sets_default_null() {
        let sql = render_sql(vec![Change::SetDefault {
            name: "C".into(),
            from: Some("1".into()),
            to: None,
            constraint_name: None,
        }]);
        assert!(sql.contains("MODIFY (\"C\" DEFAULT NULL);"));
    }

    #[test]
    fn constraints_and_indexes_render_oracle_syntax() {
        let sql = render_sql(vec![
            Change::DropForeignKey {
                name: "EMP_DEPT_FK".into(),
            },
            Change::DropIndex {
                name: "EMP_NAME_IX".into(),
            },
            Change::ReplacePrimaryKey {
                from: vec!["ID".into()],
                to: vec!["ID".into(), "DEPT".into()],
                constraint_name: Some("EMP_PK".into()),
            },
            Change::AddForeignKey(ForeignKeyDef {
                name: "EMP_DEPT_FK".into(),
                columns: vec!["DEPT".into()],
                ref_schema: Some("HR".into()),
                ref_table: "DEPT".into(),
                ref_columns: vec!["ID".into()],
                on_delete: Some("cascade".into()),
                on_update: Some("CASCADE".into()),
            }),
            Change::AddIndex(IndexDef {
                name: "EMP_NAME_IX".into(),
                columns: vec!["NAME".into()],
                unique: true,
                include: vec![],
                filter: None,
                method: None,
                fillfactor: Some(70),
            }),
            Change::AddIndex(IndexDef {
                name: "EMP_FLAG_BX".into(),
                columns: vec!["FLAG".into()],
                unique: false,
                include: vec!["X".into()],
                filter: Some("FLAG = 1".into()),
                method: Some("BITMAP".into()),
                fillfactor: None,
            }),
        ]);
        assert!(sql.contains("ALTER TABLE \"HR\".\"EMP\" DROP CONSTRAINT \"EMP_DEPT_FK\";\n"));
        assert!(sql.contains("DROP INDEX \"HR\".\"EMP_NAME_IX\";\n"));
        assert!(sql.contains("ALTER TABLE \"HR\".\"EMP\" DROP CONSTRAINT \"EMP_PK\";\n"));
        assert!(sql.contains(
            "ALTER TABLE \"HR\".\"EMP\" ADD CONSTRAINT \"PK_EMP\" PRIMARY KEY (\"ID\", \"DEPT\");\n"
        ));
        assert!(sql.contains(
            "ADD CONSTRAINT \"EMP_DEPT_FK\" FOREIGN KEY (\"DEPT\") REFERENCES \"HR\".\"DEPT\" (\"ID\") ON DELETE CASCADE;\n"
        ));
        assert!(!sql.contains("ON UPDATE CASCADE"));
        assert!(
            sql.contains("-- WARNING: Foreign key EMP_DEPT_FK: Oracle has no ON UPDATE action")
        );
        assert!(
            sql.contains("CREATE UNIQUE INDEX \"EMP_NAME_IX\" ON \"HR\".\"EMP\" (\"NAME\");\n")
        );
        assert!(
            sql.contains("-- WARNING: Index EMP_NAME_IX: fillfactor is not supported on Oracle")
        );
        assert!(
            sql.contains("CREATE BITMAP INDEX \"EMP_FLAG_BX\" ON \"HR\".\"EMP\" (\"FLAG\");\n")
        );
        assert!(sql.contains("-- MANUAL REVIEW: Index EMP_FLAG_BX uses INCLUDE columns"));
        assert!(sql.contains("-- MANUAL REVIEW: Index EMP_FLAG_BX has a filter"));
        // Drops come before adds so the PK can be replaced in place.
        assert!(
            sql.find("DROP CONSTRAINT \"EMP_PK\"").unwrap()
                < sql.find("ADD CONSTRAINT \"PK_EMP\"").unwrap()
        );
    }

    #[test]
    fn dependent_views_are_noted_not_dropped() {
        let mut s = snapshot();
        s.dependent_views
            .push(crate::alter::snapshot::DependentView {
                schema: Some("HR".into()),
                name: "EMP_V".into(),
                materialized: false,
                definition: Some("CREATE VIEW HR.EMP_V AS SELECT * FROM HR.EMP".into()),
            });
        let d = draft(&s);
        let sql = render(
            DatabaseType::Oracle,
            &s,
            &d,
            &set(vec![Change::DropColumn { name: "X".into() }]),
            &RenderOptions::default(),
        )
        .unwrap()
        .sql;
        assert!(!sql.contains("DROP VIEW"));
        assert!(sql.contains("-- WARNING: Dependent views (\"HR\".\"EMP_V\") are invalidated"));
    }
}
