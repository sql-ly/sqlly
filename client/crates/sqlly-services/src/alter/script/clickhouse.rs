//! ClickHouse ALTER script generation.
//!
//! ClickHouse alters MergeTree tables column by column (`ADD COLUMN`, `DROP
//! COLUMN`, `RENAME COLUMN`, `MODIFY COLUMN type` / `DEFAULT expr` / `REMOVE
//! DEFAULT`, `ADD INDEX … TYPE … GRANULARITY …`, `DROP INDEX`), with no
//! transactions to wrap them in. What it cannot do — change the primary key
//! / ORDER BY, add foreign keys or unique indexes, retype a key column —
//! comes out as MANUAL REVIEW lines rather than a statement that would fail.

use sqlly_lang::sqlmodel::DatabaseType;

use crate::alter::diff::{Change, ChangeSet};
use crate::alter::script::{
    AlterScript, RenderOptions, header, qualified_table, render_comment_changes,
};
use crate::alter::snapshot::{ColumnDef, IndexDef, TableDraft, TableSnapshot};

/// `Nullable(T)` for a nullable column whose declared type is not already
/// wrapped; ClickHouse columns are non-nullable by default.
fn column_type(def: &ColumnDef) -> String {
    let ty = def.data_type.trim();
    if def.nullable && !ty.starts_with("Nullable(") && !ty.starts_with("LowCardinality(Nullable(") {
        format!("Nullable({ty})")
    } else {
        ty.to_string()
    }
}

fn add_column_sql(db: DatabaseType, table_q: &str, col: &ColumnDef) -> String {
    let mut line = format!(
        "ALTER TABLE {table_q} ADD COLUMN {} {}",
        db.quote_identifier(&col.name),
        column_type(col)
    );
    if let Some(def) = &col.default {
        line.push_str(&format!(" DEFAULT {def}"));
    }
    format!("{line};\n")
}

/// `ADD INDEX name (columns) TYPE kind GRANULARITY 1` plus a `MATERIALIZE
/// INDEX` so existing parts get the index too. `method` carries the
/// skipping-index kind when the user set one; `minmax` otherwise.
fn add_index_sql(
    db: DatabaseType,
    table_q: &str,
    idx: &IndexDef,
    warnings: &mut Vec<String>,
) -> String {
    let columns = idx
        .columns
        .iter()
        .map(|c| db.quote_identifier(c))
        .collect::<Vec<_>>()
        .join(", ");
    let kind = idx
        .method
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .unwrap_or("minmax")
        .to_string();
    if idx.unique {
        warnings.push(format!(
            "Index {} was marked unique; ClickHouse data-skipping indexes cannot enforce uniqueness",
            idx.name
        ));
    }
    if !idx.include.is_empty() || idx.filter.is_some() {
        warnings.push(format!(
            "Index {}: included columns and filters are not supported by ClickHouse data-skipping indexes and were dropped",
            idx.name
        ));
    }
    let name = db.quote_identifier(&idx.name);
    format!(
        "ALTER TABLE {table_q} ADD INDEX {name} ({columns}) TYPE {kind} GRANULARITY 1;\nALTER TABLE {table_q} MATERIALIZE INDEX {name};\n"
    )
}

pub fn render(
    db: DatabaseType,
    snapshot: &TableSnapshot,
    draft: &TableDraft,
    set: &ChangeSet,
    _opts: &RenderOptions,
) -> Result<AlterScript, crate::alter::script::AlterError> {
    let table_q = qualified_table(db, set.table.schema.as_deref(), &set.table.table);
    let mut warnings = Vec::new();
    let mut manual_review = set.manual_review.clone();
    let mut body = String::new();

    let touches_columns = set.changes.iter().any(|c| {
        matches!(
            c,
            Change::DropColumn { .. }
                | Change::RenameColumn { .. }
                | Change::AlterColumnType { .. }
        )
    });
    if touches_columns && !snapshot.dependent_views.is_empty() {
        let names = snapshot
            .dependent_views
            .iter()
            .map(|v| qualified_table(db, v.schema.as_deref(), &v.name))
            .collect::<Vec<_>>()
            .join(", ");
        warnings.push(format!(
            "Views that select from this table ({names}) are not recreated; dropped, renamed or retyped columns break them until you alter their SELECT"
        ));
    }

    for c in &set.changes {
        match c {
            Change::AddColumn(col) => body.push_str(&add_column_sql(db, &table_q, col)),
            Change::DropColumn { name } => {
                if snapshot.primary_key.iter().any(|k| k == name) {
                    manual_review.push(format!(
                        "Column {name} is part of the primary key (ORDER BY) and cannot be dropped; create a new table with the wanted key and INSERT … SELECT"
                    ));
                } else {
                    body.push_str(&format!(
                        "ALTER TABLE {table_q} DROP COLUMN {};\n",
                        db.quote_identifier(name)
                    ));
                }
            }
            Change::RenameColumn { from, to } => body.push_str(&format!(
                "ALTER TABLE {table_q} RENAME COLUMN {} TO {};\n",
                db.quote_identifier(from),
                db.quote_identifier(to)
            )),
            Change::AlterColumnType { name, to, .. } => {
                if snapshot.primary_key.iter().any(|k| k == name) {
                    manual_review.push(format!(
                        "Column {name} is part of the primary key (ORDER BY); its type cannot be modified in place — create a new table and INSERT … SELECT"
                    ));
                } else {
                    body.push_str(&format!(
                        "ALTER TABLE {table_q} MODIFY COLUMN {} {};\n",
                        db.quote_identifier(name),
                        column_type(to)
                    ));
                }
            }
            Change::SetDefault { name, to, .. } => match to {
                Some(expr) => body.push_str(&format!(
                    "ALTER TABLE {table_q} MODIFY COLUMN {} DEFAULT {expr};\n",
                    db.quote_identifier(name)
                )),
                None => body.push_str(&format!(
                    "ALTER TABLE {table_q} MODIFY COLUMN {} REMOVE DEFAULT;\n",
                    db.quote_identifier(name)
                )),
            },
            Change::DropIndex { name } => body.push_str(&format!(
                "ALTER TABLE {table_q} DROP INDEX {};\n",
                db.quote_identifier(name)
            )),
            Change::AddIndex(idx) => {
                body.push_str(&add_index_sql(db, &table_q, idx, &mut warnings));
            }
            Change::DropForeignKey { name } => manual_review.push(format!(
                "Foreign key {name}: ClickHouse has no foreign keys; nothing to drop"
            )),
            Change::AddForeignKey(fk) => manual_review.push(format!(
                "Foreign key {} was not created: ClickHouse has no foreign keys (join on the columns instead)",
                fk.name
            )),
            Change::ReplacePrimaryKey { to, .. } => manual_review.push(format!(
                "Primary key change to ({}) needs a new table: the ORDER BY of a MergeTree table cannot be altered — CREATE TABLE … ORDER BY (…), INSERT … SELECT, then swap the names with EXCHANGE TABLES",
                to.join(", ")
            )),
            // Descriptions are appended below in ClickHouse's own COMMENT forms.
            Change::SetComment { .. } => {}
        }
    }

    body.push_str(&render_comment_changes(db, snapshot, set, draft)?);

    if body.is_empty() && manual_review.is_empty() && warnings.is_empty() {
        return Ok(AlterScript {
            sql: header(set.table.schema.as_deref(), &set.table.table, &[], &[]),
            warnings,
        });
    }
    let sql = header(
        set.table.schema.as_deref(),
        &set.table.table,
        &warnings,
        &manual_review,
    ) + "-- ClickHouse has no transactions: each ALTER below is applied on its own.\n"
        + &body;
    Ok(AlterScript { sql, warnings })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alter::diff::TableRef;
    use crate::alter::snapshot::{DependentView, ForeignKeyDef};

    fn col(name: &str, ty: &str, nullable: bool) -> ColumnDef {
        ColumnDef {
            name: name.into(),
            data_type: ty.into(),
            nullable,
            identity: false,
            default: None,
            default_constraint_name: None,
        }
    }

    fn snapshot() -> TableSnapshot {
        TableSnapshot {
            schema: Some("shop".into()),
            table: "orders".into(),
            columns: vec![col("id", "UInt32", false), col("note", "String", false)],
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

    fn draft() -> TableDraft {
        TableDraft {
            schema: Some("shop".into()),
            table: "orders".into(),
            columns: vec![],
            primary_key: vec!["id".into()],
            pk_constraint_name: None,
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
                schema: Some("shop".into()),
                table: "orders".into(),
            },
            changes,
            manual_review: vec![],
        }
    }

    fn render_sql(changes: Vec<Change>) -> AlterScript {
        render(
            DatabaseType::ClickHouse,
            &snapshot(),
            &draft(),
            &set(changes),
            &RenderOptions::default(),
        )
        .unwrap()
    }

    #[test]
    fn column_changes_render_clickhouse_alter_forms() {
        let mut with_default = col("status", "String", true);
        with_default.default = Some("'new'".into());
        let script = render_sql(vec![
            Change::AddColumn(with_default),
            Change::AddColumn(col("amount", "Decimal(12, 2)", false)),
            Change::RenameColumn {
                from: "note".into(),
                to: "comment".into(),
            },
            Change::AlterColumnType {
                name: "note".into(),
                from: col("note", "String", false),
                to: col("note", "String", true),
            },
            Change::SetDefault {
                name: "note".into(),
                from: None,
                to: Some("''".into()),
                constraint_name: None,
            },
            Change::SetDefault {
                name: "note".into(),
                from: Some("''".into()),
                to: None,
                constraint_name: None,
            },
            Change::DropColumn {
                name: "note".into(),
            },
        ]);
        let sql = script.sql;
        assert!(sql.contains("-- ClickHouse has no transactions"), "{sql}");
        assert!(
            sql.contains(
                "ALTER TABLE `shop`.`orders` ADD COLUMN `status` Nullable(String) DEFAULT 'new';"
            ),
            "{sql}"
        );
        assert!(
            sql.contains("ALTER TABLE `shop`.`orders` ADD COLUMN `amount` Decimal(12, 2);"),
            "{sql}"
        );
        assert!(
            sql.contains("ALTER TABLE `shop`.`orders` RENAME COLUMN `note` TO `comment`;"),
            "{sql}"
        );
        assert!(
            sql.contains("ALTER TABLE `shop`.`orders` MODIFY COLUMN `note` Nullable(String);"),
            "{sql}"
        );
        assert!(
            sql.contains("ALTER TABLE `shop`.`orders` MODIFY COLUMN `note` DEFAULT '';"),
            "{sql}"
        );
        assert!(
            sql.contains("ALTER TABLE `shop`.`orders` MODIFY COLUMN `note` REMOVE DEFAULT;"),
            "{sql}"
        );
        assert!(
            sql.contains("ALTER TABLE `shop`.`orders` DROP COLUMN `note`;"),
            "{sql}"
        );
        assert!(!sql.contains("BEGIN"), "{sql}");
        assert!(!sql.contains("NOT NULL"), "{sql}");
    }

    #[test]
    fn key_columns_keys_and_foreign_keys_go_to_manual_review() {
        let script = render_sql(vec![
            Change::DropColumn { name: "id".into() },
            Change::AlterColumnType {
                name: "id".into(),
                from: col("id", "UInt32", false),
                to: col("id", "UInt64", false),
            },
            Change::ReplacePrimaryKey {
                from: vec!["id".into()],
                to: vec!["id".into(), "note".into()],
                constraint_name: None,
            },
            Change::AddForeignKey(ForeignKeyDef {
                name: "fk_customer".into(),
                columns: vec!["customer_id".into()],
                ref_schema: Some("shop".into()),
                ref_table: "customers".into(),
                ref_columns: vec!["id".into()],
                on_delete: None,
                on_update: None,
            }),
            Change::DropForeignKey {
                name: "fk_old".into(),
            },
        ]);
        let sql = script.sql;
        assert!(!sql.contains("DROP COLUMN `id`"), "{sql}");
        assert!(!sql.contains("MODIFY COLUMN `id`"), "{sql}");
        assert!(
            sql.contains("-- MANUAL REVIEW: Column id is part of the primary key (ORDER BY) and cannot be dropped"),
            "{sql}"
        );
        assert!(
            sql.contains("-- MANUAL REVIEW: Column id is part of the primary key (ORDER BY); its type cannot be modified"),
            "{sql}"
        );
        assert!(
            sql.contains("-- MANUAL REVIEW: Primary key change to (id, note) needs a new table"),
            "{sql}"
        );
        assert!(
            sql.contains("-- MANUAL REVIEW: Foreign key fk_customer was not created: ClickHouse has no foreign keys"),
            "{sql}"
        );
        assert!(
            sql.contains("-- MANUAL REVIEW: Foreign key fk_old: ClickHouse has no foreign keys"),
            "{sql}"
        );
    }

    #[test]
    fn indexes_become_data_skipping_indexes_with_materialize() {
        let script = render_sql(vec![
            Change::AddIndex(IndexDef {
                name: "idx_note".into(),
                columns: vec!["note".into()],
                unique: true,
                include: vec![],
                filter: None,
                method: Some("bloom_filter".into()),
                fillfactor: None,
            }),
            Change::AddIndex(IndexDef {
                name: "idx_amount".into(),
                columns: vec!["amount".into()],
                unique: false,
                include: vec!["note".into()],
                filter: None,
                method: None,
                fillfactor: None,
            }),
            Change::DropIndex {
                name: "idx_old".into(),
            },
        ]);
        let sql = script.sql;
        assert!(
            sql.contains(
                "ALTER TABLE `shop`.`orders` ADD INDEX `idx_note` (`note`) TYPE bloom_filter GRANULARITY 1;\nALTER TABLE `shop`.`orders` MATERIALIZE INDEX `idx_note`;"
            ),
            "{sql}"
        );
        assert!(
            sql.contains(
                "ALTER TABLE `shop`.`orders` ADD INDEX `idx_amount` (`amount`) TYPE minmax GRANULARITY 1;"
            ),
            "{sql}"
        );
        assert!(
            sql.contains("ALTER TABLE `shop`.`orders` DROP INDEX `idx_old`;"),
            "{sql}"
        );
        assert!(
            script
                .warnings
                .iter()
                .any(|w| w.contains("cannot enforce uniqueness"))
        );
        assert!(
            script
                .warnings
                .iter()
                .any(|w| w.contains("included columns and filters"))
        );
    }

    #[test]
    fn dependent_views_are_warned_about_not_recreated() {
        let mut snapshot = snapshot();
        snapshot.dependent_views.push(DependentView {
            schema: Some("shop".into()),
            name: "big_orders".into(),
            materialized: true,
            definition: Some("CREATE MATERIALIZED VIEW shop.big_orders …".into()),
        });
        let script = render(
            DatabaseType::ClickHouse,
            &snapshot,
            &draft(),
            &set(vec![Change::RenameColumn {
                from: "note".into(),
                to: "n".into(),
            }]),
            &RenderOptions::default(),
        )
        .unwrap();
        assert!(!script.sql.contains("DROP VIEW"), "{}", script.sql);
        assert!(
            !script.sql.contains("CREATE MATERIALIZED VIEW"),
            "{}",
            script.sql
        );
        assert!(
            script.sql.contains("-- WARNING: Views that select from this table (`shop`.`big_orders`) are not recreated"),
            "{}",
            script.sql
        );
    }

    #[test]
    fn dispatcher_routes_clickhouse_to_this_renderer_not_mysql() {
        let script = crate::alter::script::render(
            DatabaseType::ClickHouse,
            &snapshot(),
            &draft(),
            &set(vec![Change::AddColumn(col("x", "UInt8", false))]),
            &RenderOptions::default(),
        )
        .unwrap();
        assert!(
            script
                .sql
                .contains("ALTER TABLE `shop`.`orders` ADD COLUMN `x` UInt8;"),
            "{}",
            script.sql
        );
        assert!(!script.sql.contains("ALGORITHM"), "{}", script.sql);
    }
}
