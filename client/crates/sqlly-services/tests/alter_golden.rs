//! Whole-script golden tests for the structure editor generators.

use sqlly_lang::sqlmodel::DatabaseType;
use sqlly_services::alter::{
    ColumnDef, ColumnExtras, DependentView, DraftColumn, ForeignKeyDef, IndexDef, MysqlAlgorithm,
    RenderOptions, TableDraft, TableSnapshot, diff, render, touches_columns,
};

/// Compare generated SQL against a golden fixture, ignoring line-ending style.
/// The generators emit `\n`, but `include_str!` embeds whatever git checked the
/// fixture out as — CRLF on a Windows agent whose autocrlf rewrote it — so a
/// raw `assert_eq!` would fail purely on `\r`. Normalize both sides to `\n`.
fn assert_golden_eq(actual: &str, expected: &str) {
    assert_eq!(actual.replace("\r\n", "\n"), expected.replace("\r\n", "\n"));
}

fn people_snapshot() -> TableSnapshot {
    TableSnapshot {
        schema: Some("dbo".into()),
        table: "people".into(),
        columns: vec![
            ColumnDef {
                name: "id".into(),
                data_type: "int".into(),
                nullable: false,
                identity: true,
                default: None,
                default_constraint_name: None,
            },
            ColumnDef {
                name: "name".into(),
                data_type: "nvarchar(50)".into(),
                nullable: false,
                identity: false,
                default: None,
                default_constraint_name: None,
            },
            ColumnDef {
                name: "email".into(),
                data_type: "nvarchar(100)".into(),
                nullable: true,
                identity: false,
                default: Some("('')".into()),
                default_constraint_name: Some("DF_people_email".into()),
            },
            ColumnDef {
                name: "org_id".into(),
                data_type: "int".into(),
                nullable: true,
                identity: false,
                default: None,
                default_constraint_name: None,
            },
        ],
        primary_key: vec!["id".into()],
        pk_constraint_name: Some("PK_people".into()),
        indexes: vec![index_def(
            "IX_people_name",
            vec!["name"],
            false,
            None,
            None,
            None,
        )],
        foreign_keys: vec![ForeignKeyDef {
            name: "FK_people_org".into(),
            columns: vec!["org_id".into()],
            ref_schema: Some("dbo".into()),
            ref_table: "orgs".into(),
            ref_columns: vec!["id".into()],
            on_delete: None,
            on_update: None,
        }],
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

fn index_def(
    name: &str,
    columns: Vec<&str>,
    unique: bool,
    filter: Option<&str>,
    method: Option<&str>,
    fillfactor: Option<u8>,
) -> IndexDef {
    IndexDef {
        name: name.into(),
        columns: columns.into_iter().map(str::to_string).collect(),
        unique,
        include: vec![],
        filter: filter.map(str::to_string),
        method: method.map(str::to_string),
        fillfactor,
    }
}

fn people_draft(snapshot: &TableSnapshot) -> TableDraft {
    TableDraft {
        schema: snapshot.schema.clone(),
        table: snapshot.table.clone(),
        columns: vec![
            DraftColumn {
                origin: Some("id".into()),
                def: snapshot.columns[0].clone(),
            },
            DraftColumn {
                origin: Some("name".into()),
                def: ColumnDef {
                    name: "name".into(),
                    data_type: "nvarchar(120)".into(),
                    nullable: false,
                    identity: false,
                    default: None,
                    default_constraint_name: None,
                },
            },
            DraftColumn {
                origin: Some("email".into()),
                def: ColumnDef {
                    name: "email_address".into(),
                    data_type: "nvarchar(100)".into(),
                    nullable: true,
                    identity: false,
                    default: Some("('')".into()),
                    default_constraint_name: Some("DF_people_email".into()),
                },
            },
            DraftColumn {
                origin: Some("org_id".into()),
                def: snapshot.columns[3].clone(),
            },
            DraftColumn {
                origin: None,
                def: ColumnDef {
                    name: "created_at".into(),
                    data_type: "datetime".into(),
                    nullable: false,
                    identity: false,
                    default: Some("CURRENT_TIMESTAMP".into()),
                    default_constraint_name: None,
                },
            },
        ],
        primary_key: snapshot.primary_key.clone(),
        pk_constraint_name: snapshot.pk_constraint_name.clone(),
        indexes: vec![
            index_def(
                "IX_people_email",
                vec!["email_address"],
                true,
                None,
                None,
                None,
            ),
            index_def(
                "IX_people_active_email",
                vec!["email_address"],
                false,
                Some("active = 1"),
                None,
                None,
            ),
        ],
        foreign_keys: snapshot.foreign_keys.clone(),
        table_comment: snapshot.table_comment.clone(),
        column_comments: snapshot.column_comments.clone(),
        source_ddl: snapshot.stored_table_ddl.clone(),
    }
}

fn people_change_set(
    snapshot: &TableSnapshot,
    draft: &TableDraft,
) -> sqlly_services::alter::ChangeSet {
    diff(snapshot, draft)
}

#[test]
fn golden_sql_server_people() {
    let snapshot = people_snapshot();
    let draft = people_draft(&snapshot);
    let set = people_change_set(&snapshot, &draft);
    let script = render(
        DatabaseType::SqlServer,
        &snapshot,
        &draft,
        &set,
        &RenderOptions::default(),
    )
    .expect("render");
    let sql = &script.sql;
    assert!(sql.contains("SET XACT_ABORT ON;"));
    assert!(sql.contains("DROP CONSTRAINT [DF_people_email]"));
    assert!(
        sql.find("DROP CONSTRAINT [DF_people_email]").unwrap() < sql.find("sp_rename").unwrap()
    );
    assert!(sql.contains("ADD CONSTRAINT [DF_people_email] DEFAULT (('')) FOR [email_address]"));
    assert_golden_eq(sql, EXPECTED_TDS);
}

#[test]
fn golden_postgres_people() {
    let snapshot = people_snapshot();
    let draft = people_draft(&snapshot);
    let set = people_change_set(&snapshot, &draft);
    let mut opts = RenderOptions::default();
    opts.pg_using
        .insert("name".into(), "\"name\"::nvarchar(120)".into());
    let script = render(DatabaseType::Postgres, &snapshot, &draft, &set, &opts).expect("render");
    let sql = &script.sql;
    assert!(sql.contains("RENAME COLUMN"));
    let combined_count = sql.matches("ALTER TABLE \"dbo\".\"people\"\n").count();
    assert_eq!(combined_count, 1);
    assert!(sql.contains("TYPE nvarchar(120) USING \"name\"::nvarchar(120)"));
    assert_golden_eq(sql, EXPECTED_PG);
}

#[test]
fn golden_mysql_people() {
    let snapshot = people_snapshot();
    let draft = people_draft(&snapshot);
    let set = people_change_set(&snapshot, &draft);
    let script = render(
        DatabaseType::MySql,
        &snapshot,
        &draft,
        &set,
        &RenderOptions::default(),
    )
    .expect("render");
    let sql = &script.sql;
    assert!(sql.contains("CHANGE COLUMN `email` `email_address`"));
    assert_golden_eq(sql, EXPECTED_MYSQL);
}

#[test]
fn golden_mysql_people_instant_algorithm() {
    let snapshot = people_snapshot();
    let draft = people_draft(&snapshot);
    let set = people_change_set(&snapshot, &draft);
    let opts = RenderOptions {
        mysql_algorithm: Some(MysqlAlgorithm::Instant),
        mysql_lock: None,
        pg_using: Default::default(),
    };
    let script = render(DatabaseType::MySql, &snapshot, &draft, &set, &opts).expect("render");
    assert!(script.sql.contains("ALGORITHM=INSTANT"));
    assert_golden_eq(&script.sql, EXPECTED_MYSQL_INSTANT);
}

#[test]
fn golden_sqlite_people_rebuild() {
    let snapshot = people_snapshot();
    let draft = people_draft(&snapshot);
    let set = people_change_set(&snapshot, &draft);
    let script = render(
        DatabaseType::Sqlite,
        &snapshot,
        &draft,
        &set,
        &RenderOptions::default(),
    )
    .expect("render");
    let sql = &script.sql;
    assert!(sql.contains("-- Step 1:"));
    assert!(sql.contains("-- Step 1b:"));
    assert!(sql.contains(
        "INSERT INTO \"new_people\" (\"id\", \"name\", \"email_address\", \"org_id\") SELECT \"id\", \"name\", \"email\", \"org_id\" FROM \"dbo\".\"people\";"
    ));
    assert_golden_eq(sql, EXPECTED_SQLITE);
}

fn people_index_options_draft(snapshot: &TableSnapshot) -> TableDraft {
    let mut draft = people_draft(snapshot);
    draft.indexes.push(index_def(
        "IX_people_hash",
        vec!["email_address"],
        false,
        None,
        Some("hash"),
        Some(80),
    ));
    draft
}

#[test]
fn golden_index_options_pg() {
    let snapshot = people_snapshot();
    let draft = people_index_options_draft(&snapshot);
    let set = people_change_set(&snapshot, &draft);
    let mut opts = RenderOptions::default();
    opts.pg_using
        .insert("name".into(), "\"name\"::nvarchar(120)".into());
    let script = render(DatabaseType::Postgres, &snapshot, &draft, &set, &opts).expect("render");
    assert_golden_eq(&script.sql, EXPECTED_INDEX_OPTIONS_PG);
}

#[test]
fn golden_index_options_tds() {
    let snapshot = people_snapshot();
    let mut draft = people_draft(&snapshot);
    draft.indexes.push(index_def(
        "IX_people_clustered",
        vec!["email_address"],
        false,
        None,
        Some("CLUSTERED"),
        Some(80),
    ));
    let set = people_change_set(&snapshot, &draft);
    let script = render(
        DatabaseType::SqlServer,
        &snapshot,
        &draft,
        &set,
        &RenderOptions::default(),
    )
    .expect("render");
    assert_golden_eq(&script.sql, EXPECTED_INDEX_OPTIONS_TDS);
}

#[test]
fn golden_index_options_mysql() {
    let snapshot = people_snapshot();
    let mut draft = people_draft(&snapshot);
    draft.indexes.push(index_def(
        "IX_people_hash",
        vec!["email_address"],
        false,
        None,
        Some("HASH"),
        Some(80),
    ));
    let set = people_change_set(&snapshot, &draft);
    let script = render(
        DatabaseType::MySql,
        &snapshot,
        &draft,
        &set,
        &RenderOptions::default(),
    )
    .expect("render");
    assert_golden_eq(&script.sql, EXPECTED_INDEX_OPTIONS_MYSQL);
}

#[test]
fn golden_index_options_sqlite() {
    let snapshot = people_snapshot();
    let mut draft = people_draft(&snapshot);
    draft.indexes.push(index_def(
        "IX_people_hash",
        vec!["email_address"],
        false,
        None,
        Some("hash"),
        Some(80),
    ));
    let set = people_change_set(&snapshot, &draft);
    let script = render(
        DatabaseType::Sqlite,
        &snapshot,
        &draft,
        &set,
        &RenderOptions::default(),
    )
    .expect("render");
    assert_golden_eq(&script.sql, EXPECTED_INDEX_OPTIONS_SQLITE);
}

fn people_views_snapshot() -> TableSnapshot {
    let mut snapshot = people_snapshot();
    snapshot.dependent_views = vec![
        DependentView {
            schema: Some("dbo".into()),
            name: "v_people".into(),
            materialized: false,
            definition: Some(
                "CREATE VIEW \"dbo\".\"v_people\" AS SELECT id, name FROM \"dbo\".\"people\""
                    .into(),
            ),
        },
        DependentView {
            schema: Some("dbo".into()),
            name: "v_broken".into(),
            materialized: false,
            definition: None,
        },
    ];
    snapshot
}

#[test]
fn golden_dependent_views_pg() {
    let snapshot = people_views_snapshot();
    let draft = people_draft(&snapshot);
    let set = people_change_set(&snapshot, &draft);
    let mut opts = RenderOptions::default();
    opts.pg_using
        .insert("name".into(), "\"name\"::nvarchar(120)".into());
    let script = render(DatabaseType::Postgres, &snapshot, &draft, &set, &opts).expect("render");
    assert_golden_eq(&script.sql, EXPECTED_VIEWS_PG);
}

#[test]
fn golden_dependent_views_tds() {
    let snapshot = people_views_snapshot();
    let draft = people_draft(&snapshot);
    let set = people_change_set(&snapshot, &draft);
    let script = render(
        DatabaseType::SqlServer,
        &snapshot,
        &draft,
        &set,
        &RenderOptions::default(),
    )
    .expect("render");
    assert_golden_eq(&script.sql, EXPECTED_VIEWS_TDS);
}

#[test]
fn golden_dependent_views_mysql() {
    let snapshot = people_views_snapshot();
    let draft = people_draft(&snapshot);
    let set = people_change_set(&snapshot, &draft);
    let script = render(
        DatabaseType::MySql,
        &snapshot,
        &draft,
        &set,
        &RenderOptions::default(),
    )
    .expect("render");
    assert_golden_eq(&script.sql, EXPECTED_VIEWS_MYSQL);
}

#[test]
fn golden_dependent_views_sqlite() {
    let snapshot = people_views_snapshot();
    let draft = people_draft(&snapshot);
    let set = people_change_set(&snapshot, &draft);
    let script = render(
        DatabaseType::Sqlite,
        &snapshot,
        &draft,
        &set,
        &RenderOptions::default(),
    )
    .expect("render");
    assert_golden_eq(&script.sql, EXPECTED_VIEWS_SQLITE);
}

#[test]
fn golden_oracle_people() {
    let snapshot = people_snapshot();
    let draft = people_draft(&snapshot);
    let set = people_change_set(&snapshot, &draft);
    let script = render(
        DatabaseType::Oracle,
        &snapshot,
        &draft,
        &set,
        &RenderOptions::default(),
    )
    .expect("render");
    let sql = &script.sql;
    assert!(sql.contains("RENAME COLUMN \"email\" TO \"email_address\""));
    assert!(sql.contains("MODIFY (\"name\" nvarchar(120))"));
    assert!(!sql.contains("BEGIN;"));
    assert!(!sql.contains("COMMIT;"));
    // The partial index cannot be expressed; it goes to manual review.
    assert!(sql.contains("MANUAL REVIEW: Index IX_people_active_email has a filter"));
    assert_golden_eq(sql, EXPECTED_ORACLE);
}

#[test]
fn golden_index_options_oracle() {
    let snapshot = people_snapshot();
    let mut draft = people_draft(&snapshot);
    draft.indexes.push(index_def(
        "IX_people_bitmap",
        vec!["email_address"],
        false,
        None,
        Some("BITMAP"),
        Some(80),
    ));
    let set = people_change_set(&snapshot, &draft);
    let script = render(
        DatabaseType::Oracle,
        &snapshot,
        &draft,
        &set,
        &RenderOptions::default(),
    )
    .expect("render");
    assert!(
        script
            .sql
            .contains("CREATE BITMAP INDEX \"IX_people_bitmap\"")
    );
    assert_golden_eq(&script.sql, EXPECTED_INDEX_OPTIONS_ORACLE);
}

/// Oracle never drops or recreates dependent views: they are invalidated
/// and recompiled on next use, and the script says so.
#[test]
fn golden_dependent_views_oracle() {
    let snapshot = people_views_snapshot();
    let draft = people_draft(&snapshot);
    let set = people_change_set(&snapshot, &draft);
    let script = render(
        DatabaseType::Oracle,
        &snapshot,
        &draft,
        &set,
        &RenderOptions::default(),
    )
    .expect("render");
    assert!(!script.sql.contains("DROP VIEW"));
    assert!(!script.sql.contains("CREATE VIEW"));
    assert!(script.sql.contains("invalidated by this change"));
    assert_golden_eq(&script.sql, EXPECTED_VIEWS_ORACLE);
}

#[test]
fn add_column_does_not_touch_views() {
    let snapshot = people_views_snapshot();
    let mut draft = TableDraft {
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
    };
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
    let set = people_change_set(&snapshot, &draft);
    assert!(!touches_columns(&set));
    let script = render(
        DatabaseType::Postgres,
        &snapshot,
        &draft,
        &set,
        &RenderOptions::default(),
    )
    .expect("render");
    assert!(!script.sql.contains("DROP VIEW"));
}

// ---------------------------------------------------------------------------
// MySQL/MariaDB: a description edit folded into the same column's structural
// clause. Restating the column from the PRE-edit `SHOW CREATE TABLE` after a
// MODIFY would silently revert the type/default/nullability change.
// ---------------------------------------------------------------------------

const ORDERS_DDL: &str = "CREATE TABLE `orders` (\n  `id` int NOT NULL AUTO_INCREMENT,\n  `status` varchar(20) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin NOT NULL DEFAULT 'new' COMMENT 'x',\n  `created` timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,\n  `qty` int NOT NULL COMMENT 'units',\n  PRIMARY KEY (`id`)\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4\n";

fn mysql_col(name: &str, ty: &str, nullable: bool, default: Option<&str>) -> ColumnDef {
    ColumnDef {
        name: name.into(),
        data_type: ty.into(),
        nullable,
        identity: false,
        default: default.map(str::to_string),
        default_constraint_name: None,
    }
}

fn orders_snapshot() -> TableSnapshot {
    let mut id = mysql_col("id", "int", false, None);
    id.identity = true;
    let mut column_comments = std::collections::BTreeMap::new();
    column_comments.insert("status".to_string(), "x".to_string());
    column_comments.insert("qty".to_string(), "units".to_string());
    TableSnapshot {
        schema: Some("shop".into()),
        table: "orders".into(),
        columns: vec![
            id,
            mysql_col("status", "varchar(20)", false, Some("'new'")),
            mysql_col("created", "timestamp", false, Some("CURRENT_TIMESTAMP")),
            mysql_col("qty", "int", false, None),
        ],
        primary_key: vec!["id".into()],
        column_comments,
        stored_table_ddl: Some(ORDERS_DDL.into()),
        ..Default::default()
    }
}

fn orders_draft(snapshot: &TableSnapshot) -> TableDraft {
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
        table_comment: None,
        column_comments: snapshot.column_comments.clone(),
        source_ddl: snapshot.stored_table_ddl.clone(),
    }
}

fn render_mysql(snapshot: &TableSnapshot, draft: &TableDraft) -> String {
    let set = diff(snapshot, draft);
    render(
        DatabaseType::MySql,
        snapshot,
        draft,
        &set,
        &RenderOptions::default(),
    )
    .expect("render")
    .sql
    .replace("\r\n", "\n")
}

/// The ALTER statements of a script, header comments stripped.
fn statements(sql: &str) -> String {
    sql.lines()
        .filter(|l| !l.starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn mysql_type_change_plus_description_is_one_clause() {
    let snapshot = orders_snapshot();
    let mut draft = orders_draft(&snapshot);
    draft.columns[3].def.data_type = "bigint".into();
    draft
        .column_comments
        .insert("qty".into(), "units sold".into());
    let sql = render_mysql(&snapshot, &draft);
    assert_eq!(
        statements(&sql),
        "ALTER TABLE `shop`.`orders`\n    MODIFY COLUMN `qty` bigint NOT NULL COMMENT 'units sold';"
    );
}

#[test]
fn mysql_default_change_plus_description_is_one_clause() {
    let snapshot = orders_snapshot();
    let mut draft = orders_draft(&snapshot);
    draft.columns[1].def.default = Some("'open'".into());
    draft
        .column_comments
        .insert("status".into(), "lifecycle".into());
    let sql = render_mysql(&snapshot, &draft);
    assert_eq!(
        statements(&sql),
        "ALTER TABLE `shop`.`orders`\n    MODIFY COLUMN `status` varchar(20) NOT NULL DEFAULT 'open' COMMENT 'lifecycle';"
    );
    // The editor cannot restate the charset/collation it does not model;
    // the script says so instead of dropping them silently.
    assert!(
        sql.contains("-- WARNING: Column status: the rebuilt definition does not restate CHARACTER SET, COLLATE"),
        "{sql}"
    );
}

#[test]
fn mysql_nullability_change_plus_cleared_description() {
    let snapshot = orders_snapshot();
    let mut draft = orders_draft(&snapshot);
    draft.columns[3].def.nullable = true;
    draft.column_comments.remove("qty");
    let sql = render_mysql(&snapshot, &draft);
    assert_eq!(
        statements(&sql),
        "ALTER TABLE `shop`.`orders`\n    MODIFY COLUMN `qty` int NULL;"
    );
}

#[test]
fn mysql_rename_plus_description_is_one_change_clause() {
    let snapshot = orders_snapshot();
    let mut draft = orders_draft(&snapshot);
    draft.columns[3].def.name = "quantity".into();
    draft.column_comments.remove("qty");
    draft
        .column_comments
        .insert("quantity".into(), "units ordered".into());
    let sql = render_mysql(&snapshot, &draft);
    assert_eq!(
        statements(&sql),
        "ALTER TABLE `shop`.`orders`\n    CHANGE COLUMN `qty` `quantity` int NOT NULL COMMENT 'units ordered';"
    );
}

#[test]
fn mysql_rename_retype_and_default_on_one_column_is_one_clause() {
    let snapshot = orders_snapshot();
    let mut draft = orders_draft(&snapshot);
    draft.columns[3].def.name = "quantity".into();
    draft.columns[3].def.data_type = "bigint".into();
    draft.columns[3].def.default = Some("0".into());
    draft.column_comments.remove("qty");
    draft
        .column_comments
        .insert("quantity".into(), "units".into());
    let sql = render_mysql(&snapshot, &draft);
    assert_eq!(
        statements(&sql),
        "ALTER TABLE `shop`.`orders`\n    CHANGE COLUMN `qty` `quantity` bigint NOT NULL DEFAULT 0 COMMENT 'units';"
    );
}

#[test]
fn mysql_add_column_with_description() {
    let snapshot = orders_snapshot();
    let mut draft = orders_draft(&snapshot);
    draft.columns.push(DraftColumn {
        origin: None,
        def: mysql_col("note", "text", true, None),
    });
    draft
        .column_comments
        .insert("note".into(), "it's free text".into());
    let sql = render_mysql(&snapshot, &draft);
    assert_eq!(
        statements(&sql),
        "ALTER TABLE `shop`.`orders`\n    ADD COLUMN `note` text NULL COMMENT 'it''s free text';"
    );
}

#[test]
fn mysql_retype_without_description_edit_keeps_the_comment() {
    let snapshot = orders_snapshot();
    let mut draft = orders_draft(&snapshot);
    draft.columns[3].def.data_type = "bigint".into();
    let sql = render_mysql(&snapshot, &draft);
    assert_eq!(
        statements(&sql),
        "ALTER TABLE `shop`.`orders`\n    MODIFY COLUMN `qty` bigint NOT NULL COMMENT 'units';"
    );
    // Renaming alone keeps it too.
    let mut renamed = orders_draft(&snapshot);
    renamed.columns[3].def.name = "quantity".into();
    renamed.column_comments.remove("qty");
    renamed
        .column_comments
        .insert("quantity".into(), "units".into());
    let sql = render_mysql(&snapshot, &renamed);
    assert!(
        sql.contains("CHANGE COLUMN `qty` `quantity` int NOT NULL COMMENT 'units';"),
        "{sql}"
    );
}

#[test]
fn mysql_retyped_timestamp_keeps_on_update() {
    let snapshot = orders_snapshot();
    let mut draft = orders_draft(&snapshot);
    draft.columns[2].def.data_type = "timestamp(3)".into();
    draft.columns[2].def.default = Some("CURRENT_TIMESTAMP(3)".into());
    let sql = render_mysql(&snapshot, &draft);
    assert!(
        sql.contains(
            "MODIFY COLUMN `created` timestamp(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3) ON UPDATE CURRENT_TIMESTAMP;"
        ),
        "{sql}"
    );
}

#[test]
fn mysql_comment_only_change_still_restates_the_engine_definition() {
    let snapshot = orders_snapshot();
    let mut draft = orders_draft(&snapshot);
    draft
        .column_comments
        .insert("status".into(), "lifecycle".into());
    let sql = render_mysql(&snapshot, &draft);
    assert_eq!(
        statements(&sql),
        "ALTER TABLE `shop`.`orders` MODIFY COLUMN `status` varchar(20) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin NOT NULL DEFAULT 'new' COMMENT 'lifecycle';"
    );
}

#[test]
fn mysql_comment_only_render_cannot_bypass_generated_guard() {
    let mut snapshot = orders_snapshot();
    snapshot
        .columns
        .push(mysql_col("total", "decimal(10,2)", false, None));
    snapshot.stored_table_ddl = Some(
        "CREATE TABLE `orders` (\n  `id` int NOT NULL AUTO_INCREMENT,\n  `status` varchar(20) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin NOT NULL DEFAULT 'new' COMMENT 'x',\n  `created` timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,\n  `qty` int NOT NULL COMMENT 'units',\n  `total` decimal(10,2) GENERATED ALWAYS AS ((`price` * `qty`)) STORED,\n  PRIMARY KEY (`id`)\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4\n".into(),
    );
    let mut draft = orders_draft(&snapshot);
    draft
        .column_comments
        .insert("total".into(), "total amount".into());

    let sql = render_mysql(&snapshot, &draft);
    assert_eq!(
        statements(&sql),
        "ALTER TABLE `shop`.`orders` MODIFY COLUMN `total` decimal(10,2) GENERATED ALWAYS AS ((`price` * `qty`)) STORED COMMENT 'total amount', ALGORITHM=INPLACE;"
    );
    assert_eq!(sql.matches("MODIFY COLUMN").count(), 1);
}

#[test]
fn mariadb_native_comment_only_render_uses_instant() {
    let mut snapshot = orders_snapshot();
    snapshot
        .columns
        .push(mysql_col("total", "decimal(10,2)", false, None));
    snapshot.stored_table_ddl = Some(
        "CREATE TABLE `orders` (\n  `id` int NOT NULL AUTO_INCREMENT,\n  `status` varchar(20) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin NOT NULL DEFAULT 'new' COMMENT 'x',\n  `created` timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,\n  `qty` int NOT NULL COMMENT 'units',\n  `total` decimal(10,2) AS ((`price` * `qty`)) PERSISTENT,\n  PRIMARY KEY (`id`)\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4\n".into(),
    );
    let mut draft = orders_draft(&snapshot);
    draft
        .column_comments
        .insert("total".into(), "total amount".into());
    let set = diff(&snapshot, &draft);

    let sql = render(
        DatabaseType::MariaDb,
        &snapshot,
        &draft,
        &set,
        &RenderOptions::default(),
    )
    .expect("render")
    .sql
    .replace("\r\n", "\n");
    assert_eq!(
        statements(&sql),
        "ALTER TABLE `shop`.`orders` MODIFY COLUMN `total` decimal(10,2) AS ((`price` * `qty`)) PERSISTENT COMMENT 'total amount', ALGORITHM=INSTANT;"
    );
    assert_eq!(sql.matches("MODIFY COLUMN").count(), 1);
}

#[test]
fn unreadable_descriptions_refuse_edits_and_rebuilds_keep_the_engine_comment() {
    let mut snapshot = orders_snapshot();
    snapshot.column_comments.clear();
    snapshot.comment_read_error = Some("permission denied".into());
    // A description edit is refused: the current value is unknown.
    let mut draft = orders_draft(&snapshot);
    draft.table_comment = Some("orders".into());
    let set = diff(&snapshot, &draft);
    let err = render(
        DatabaseType::MySql,
        &snapshot,
        &draft,
        &set,
        &RenderOptions::default(),
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("could not be read"), "{err}");
    // A retype keeps the comment the engine definition carries.
    let mut draft = orders_draft(&snapshot);
    draft.columns[3].def.data_type = "bigint".into();
    let sql = render_mysql(&snapshot, &draft);
    assert!(
        sql.contains("MODIFY COLUMN `qty` bigint NOT NULL COMMENT 'units';"),
        "{sql}"
    );
}

#[test]
fn sql_server_description_edit_refused_when_descriptions_unreadable() {
    let mut snapshot = people_snapshot();
    snapshot.comment_read_error = Some("permission denied".into());
    let mut draft = people_draft(&snapshot);
    draft
        .column_comments
        .insert("name".into(), "full name".into());
    let set = diff(&snapshot, &draft);
    let err = render(
        DatabaseType::SqlServer,
        &snapshot,
        &draft,
        &set,
        &RenderOptions::default(),
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("could not be read"), "{err}");
}

#[test]
fn header_comments_cannot_be_escaped_by_a_newline_in_a_name() {
    let snapshot = orders_snapshot();
    let mut draft = orders_draft(&snapshot);
    draft.columns[3].def.name = "evil\nDROP TABLE orders;".into();
    draft.columns[3].def.identity = true;
    let sql = render_mysql(&snapshot, &draft);
    // The manual-review note names the column; its newline is flattened so
    // the note stays one comment line (inside the ALTER the name is a
    // backtick-quoted identifier, where a newline is legal).
    assert!(
        sql.contains(
            "-- MANUAL REVIEW: Identity/AUTO_INCREMENT cannot be changed in place on column evil DROP TABLE orders;\n"
        ),
        "{sql}"
    );
    assert!(!sql.lines().any(|l| l == "DROP TABLE orders;"), "{sql}");
}

const EXPECTED_TDS: &str = include_str!("alter_golden/people_tds.sql");
const EXPECTED_PG: &str = include_str!("alter_golden/people_pg.sql");
const EXPECTED_MYSQL: &str = include_str!("alter_golden/people_mysql.sql");
const EXPECTED_MYSQL_INSTANT: &str = include_str!("alter_golden/people_mysql_instant.sql");
const EXPECTED_SQLITE: &str = include_str!("alter_golden/people_sqlite.sql");
const EXPECTED_INDEX_OPTIONS_PG: &str = include_str!("alter_golden/people_index_options_pg.sql");
const EXPECTED_INDEX_OPTIONS_TDS: &str = include_str!("alter_golden/people_index_options_tds.sql");
const EXPECTED_INDEX_OPTIONS_MYSQL: &str =
    include_str!("alter_golden/people_index_options_mysql.sql");
const EXPECTED_INDEX_OPTIONS_SQLITE: &str =
    include_str!("alter_golden/people_index_options_sqlite.sql");
const EXPECTED_VIEWS_PG: &str = include_str!("alter_golden/people_views_pg.sql");
const EXPECTED_VIEWS_TDS: &str = include_str!("alter_golden/people_views_tds.sql");
const EXPECTED_VIEWS_MYSQL: &str = include_str!("alter_golden/people_views_mysql.sql");
const EXPECTED_VIEWS_SQLITE: &str = include_str!("alter_golden/people_views_sqlite.sql");
const EXPECTED_ORACLE: &str = include_str!("alter_golden/people_oracle.sql");
const EXPECTED_INDEX_OPTIONS_ORACLE: &str =
    include_str!("alter_golden/people_index_options_oracle.sql");
const EXPECTED_VIEWS_ORACLE: &str = include_str!("alter_golden/people_views_oracle.sql");

/// Regenerate golden SQL files when generator output changes intentionally.
#[test]
fn write_golden_fixtures() {
    if std::env::var("SQLLY_WRITE_GOLDENS").is_err() {
        return;
    }
    let snapshot = people_snapshot();
    let draft = people_draft(&snapshot);
    let set = people_change_set(&snapshot, &draft);
    let tds = render(
        DatabaseType::SqlServer,
        &snapshot,
        &draft,
        &set,
        &RenderOptions::default(),
    )
    .expect("tds");
    std::fs::write("tests/alter_golden/people_tds.sql", tds.sql).expect("write tds");
    let mut pg_opts = RenderOptions::default();
    pg_opts
        .pg_using
        .insert("name".into(), "\"name\"::nvarchar(120)".into());
    let pg = render(DatabaseType::Postgres, &snapshot, &draft, &set, &pg_opts).expect("pg");
    std::fs::write("tests/alter_golden/people_pg.sql", pg.sql).expect("write pg");
    let mysql = render(
        DatabaseType::MySql,
        &snapshot,
        &draft,
        &set,
        &RenderOptions::default(),
    )
    .expect("mysql");
    std::fs::write("tests/alter_golden/people_mysql.sql", mysql.sql).expect("write mysql");
    let mysql_instant = render(
        DatabaseType::MySql,
        &snapshot,
        &draft,
        &set,
        &RenderOptions {
            mysql_algorithm: Some(MysqlAlgorithm::Instant),
            mysql_lock: None,
            pg_using: Default::default(),
        },
    )
    .expect("mysql instant");
    std::fs::write(
        "tests/alter_golden/people_mysql_instant.sql",
        mysql_instant.sql,
    )
    .expect("write mysql instant");
    let sqlite = render(
        DatabaseType::Sqlite,
        &snapshot,
        &draft,
        &set,
        &RenderOptions::default(),
    )
    .expect("sqlite");
    std::fs::write("tests/alter_golden/people_sqlite.sql", sqlite.sql).expect("write sqlite");

    let index_opts_draft = people_index_options_draft(&snapshot);
    let index_opts_set = people_change_set(&snapshot, &index_opts_draft);
    let mut pg_opts = RenderOptions::default();
    pg_opts
        .pg_using
        .insert("name".into(), "\"name\"::nvarchar(120)".into());
    let index_pg = render(
        DatabaseType::Postgres,
        &snapshot,
        &index_opts_draft,
        &index_opts_set,
        &pg_opts,
    )
    .expect("index pg");
    std::fs::write(
        "tests/alter_golden/people_index_options_pg.sql",
        index_pg.sql,
    )
    .expect("write index pg");

    let mut tds_draft = people_draft(&snapshot);
    tds_draft.indexes.push(index_def(
        "IX_people_clustered",
        vec!["email_address"],
        false,
        None,
        Some("CLUSTERED"),
        Some(80),
    ));
    let tds_set = people_change_set(&snapshot, &tds_draft);
    let index_tds = render(
        DatabaseType::SqlServer,
        &snapshot,
        &tds_draft,
        &tds_set,
        &RenderOptions::default(),
    )
    .expect("index tds");
    std::fs::write(
        "tests/alter_golden/people_index_options_tds.sql",
        index_tds.sql,
    )
    .expect("write index tds");

    let mut mysql_draft = people_draft(&snapshot);
    mysql_draft.indexes.push(index_def(
        "IX_people_hash",
        vec!["email_address"],
        false,
        None,
        Some("HASH"),
        Some(80),
    ));
    let mysql_set = people_change_set(&snapshot, &mysql_draft);
    let index_mysql = render(
        DatabaseType::MySql,
        &snapshot,
        &mysql_draft,
        &mysql_set,
        &RenderOptions::default(),
    )
    .expect("index mysql");
    std::fs::write(
        "tests/alter_golden/people_index_options_mysql.sql",
        index_mysql.sql,
    )
    .expect("write index mysql");

    let mut sqlite_draft = people_draft(&snapshot);
    sqlite_draft.indexes.push(index_def(
        "IX_people_hash",
        vec!["email_address"],
        false,
        None,
        Some("hash"),
        Some(80),
    ));
    let sqlite_set = people_change_set(&snapshot, &sqlite_draft);
    let index_sqlite = render(
        DatabaseType::Sqlite,
        &snapshot,
        &sqlite_draft,
        &sqlite_set,
        &RenderOptions::default(),
    )
    .expect("index sqlite");
    std::fs::write(
        "tests/alter_golden/people_index_options_sqlite.sql",
        index_sqlite.sql,
    )
    .expect("write index sqlite");

    let views_snapshot = people_views_snapshot();
    let views_draft = people_draft(&views_snapshot);
    let views_set = people_change_set(&views_snapshot, &views_draft);
    let views_pg = render(
        DatabaseType::Postgres,
        &views_snapshot,
        &views_draft,
        &views_set,
        &pg_opts,
    )
    .expect("views pg");
    std::fs::write("tests/alter_golden/people_views_pg.sql", views_pg.sql).expect("write views pg");
    let views_tds = render(
        DatabaseType::SqlServer,
        &views_snapshot,
        &views_draft,
        &views_set,
        &RenderOptions::default(),
    )
    .expect("views tds");
    std::fs::write("tests/alter_golden/people_views_tds.sql", views_tds.sql)
        .expect("write views tds");
    let views_mysql = render(
        DatabaseType::MySql,
        &views_snapshot,
        &views_draft,
        &views_set,
        &RenderOptions::default(),
    )
    .expect("views mysql");
    std::fs::write("tests/alter_golden/people_views_mysql.sql", views_mysql.sql)
        .expect("write views mysql");
    let views_sqlite = render(
        DatabaseType::Sqlite,
        &views_snapshot,
        &views_draft,
        &views_set,
        &RenderOptions::default(),
    )
    .expect("views sqlite");
    std::fs::write(
        "tests/alter_golden/people_views_sqlite.sql",
        views_sqlite.sql,
    )
    .expect("write views sqlite");

    let oracle = render(
        DatabaseType::Oracle,
        &snapshot,
        &draft,
        &set,
        &RenderOptions::default(),
    )
    .expect("oracle");
    std::fs::write("tests/alter_golden/people_oracle.sql", oracle.sql).expect("write oracle");
    let mut oracle_draft = people_draft(&snapshot);
    oracle_draft.indexes.push(index_def(
        "IX_people_bitmap",
        vec!["email_address"],
        false,
        None,
        Some("BITMAP"),
        Some(80),
    ));
    let oracle_set = people_change_set(&snapshot, &oracle_draft);
    let index_oracle = render(
        DatabaseType::Oracle,
        &snapshot,
        &oracle_draft,
        &oracle_set,
        &RenderOptions::default(),
    )
    .expect("index oracle");
    std::fs::write(
        "tests/alter_golden/people_index_options_oracle.sql",
        index_oracle.sql,
    )
    .expect("write index oracle");
    let views_oracle = render(
        DatabaseType::Oracle,
        &views_snapshot,
        &views_draft,
        &views_set,
        &RenderOptions::default(),
    )
    .expect("views oracle");
    std::fs::write(
        "tests/alter_golden/people_views_oracle.sql",
        views_oracle.sql,
    )
    .expect("write views oracle");
}

/// Compare against a golden under `tests/alter_golden/` with the shared
/// CRLF-normalising harness (`SQLLY_UPDATE_GOLDEN=1` rewrites it).
fn assert_alter_golden(actual: &str, file: &str) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("alter_golden")
        .join(file);
    let matches = sqlly_testkit::GoldenTest::compare(actual, &path).expect("read golden");
    assert!(matches, "{file} differs from:\n{actual}");
}

/// SQL Server: `ALTER COLUMN` without `COLLATE` resets a column to the
/// database default, so a retyped character column restates its own
/// collation (a column turned numeric drops it). A column renamed and
/// retyped with a bound default has that default dropped once before the
/// rename and re-added once after the retype — never bound across the
/// ALTER COLUMN (Msg 5074) or added twice.
#[test]
fn golden_tds_retype_keeps_collation_and_moves_defaults_once() {
    let mut snapshot = people_snapshot();
    for (column, collation) in [
        ("name", "Latin1_General_CS_AS"),
        ("email", "Latin1_General_BIN2"),
    ] {
        snapshot.column_extras.insert(
            column.into(),
            ColumnExtras {
                collation: Some(collation.into()),
                ..ColumnExtras::default()
            },
        );
    }
    let mut draft = people_draft(&snapshot);
    // email → email_address, widened, default kept.
    draft.columns[2].def.data_type = "nvarchar(200)".into();
    // org_id int → bigint: no collation on a numeric column.
    draft.columns[3].def.data_type = "bigint".into();
    let set = diff(&snapshot, &draft);
    let sql = render(
        DatabaseType::SqlServer,
        &snapshot,
        &draft,
        &set,
        &RenderOptions::default(),
    )
    .expect("render")
    .sql;
    assert!(
        sql.contains("ALTER COLUMN [name] nvarchar(120) COLLATE Latin1_General_CS_AS NOT NULL;"),
        "{sql}"
    );
    assert!(
        sql.contains(
            "ALTER COLUMN [email_address] nvarchar(200) COLLATE Latin1_General_BIN2 NULL;"
        ),
        "{sql}"
    );
    assert!(sql.contains("ALTER COLUMN [org_id] bigint NULL;"), "{sql}");
    assert_eq!(
        sql.matches("DROP CONSTRAINT [DF_people_email]").count(),
        1,
        "{sql}"
    );
    assert_eq!(
        sql.matches("ADD CONSTRAINT [DF_people_email]").count(),
        1,
        "{sql}"
    );
    assert!(
        sql.find("ALTER COLUMN [email_address]").unwrap()
            < sql.find("ADD CONSTRAINT [DF_people_email]").unwrap(),
        "{sql}"
    );
    assert_alter_golden(&sql, "people_collation_tds.sql");
}

/// SQL Server: changing a column's type and its default together drops and
/// re-adds the default constraint once each (it used to do both twice).
#[test]
fn golden_tds_retype_with_new_default_is_one_drop_and_one_add() {
    let snapshot = people_snapshot();
    let mut draft = people_draft(&snapshot);
    draft.columns[2].def.name = "email".into();
    draft.columns[2].def.data_type = "nvarchar(200)".into();
    draft.columns[2].def.default = Some("('none')".into());
    draft.indexes.clear();
    let set = diff(&snapshot, &draft);
    let sql = render(
        DatabaseType::SqlServer,
        &snapshot,
        &draft,
        &set,
        &RenderOptions::default(),
    )
    .expect("render")
    .sql;
    assert_eq!(
        sql.matches("DROP CONSTRAINT [DF_people_email]").count(),
        1,
        "{sql}"
    );
    assert_eq!(
        sql.matches("ADD CONSTRAINT [DF_people_email] DEFAULT (('none')) FOR [email]")
            .count(),
        1,
        "{sql}"
    );
    assert_alter_golden(&sql, "people_retype_default_tds.sql");
}
