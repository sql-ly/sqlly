//! A plain value typed as a text, ENUM or SET column's default is written as
//! a string literal by every structure-editor generator (Beekeeper Studio
//! #5059: `DEFAULT a` instead of `DEFAULT 'a'`), while expressions stay as
//! typed.

use sqlly_lang::sqlmodel::DatabaseType;
use sqlly_services::alter::{
    ColumnDef, DraftColumn, RenderOptions, TableDraft, TableSnapshot, diff, render,
};

fn col(name: &str, ty: &str, default: Option<&str>) -> ColumnDef {
    ColumnDef {
        name: name.into(),
        data_type: ty.into(),
        nullable: true,
        identity: false,
        default: default.map(str::to_string),
        default_constraint_name: None,
    }
}

fn snapshot(schema: Option<&str>) -> TableSnapshot {
    TableSnapshot {
        schema: schema.map(str::to_string),
        table: "t".into(),
        columns: vec![
            col("id", "int", None),
            col("kind", "enum('a','b')", None),
            col("created", "timestamp", None),
        ],
        primary_key: vec!["id".into()],
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
        table_comment: None,
        column_comments: snapshot.column_comments.clone(),
        source_ddl: snapshot.stored_table_ddl.clone(),
    }
}

fn script(db: DatabaseType, snapshot: &TableSnapshot, draft: &TableDraft) -> String {
    let set = diff(snapshot, draft);
    render(db, snapshot, draft, &set, &RenderOptions::default())
        .expect("render")
        .sql
        .replace("\r\n", "\n")
}

/// Retype `kind` to `ty`, give it the plain default `a`, set `created` to
/// `CURRENT_TIMESTAMP`, and add a `note` text column defaulting to `it's`.
fn edited(db_ty: &str) -> (TableSnapshot, TableDraft) {
    let mut snap = snapshot(None);
    snap.columns[1].data_type = db_ty.into();
    let mut draft = draft_of(&snap);
    draft.columns[1].def.default = Some("a".into());
    draft.columns[2].def.default = Some("CURRENT_TIMESTAMP".into());
    draft.columns.push(DraftColumn {
        origin: None,
        def: col("note", "varchar(20)", Some("it's")),
    });
    (snap, draft)
}

#[test]
fn mysql_enum_and_text_defaults_are_quoted() {
    for db in [DatabaseType::MySql, DatabaseType::MariaDb] {
        let (snap, draft) = edited("enum('a','b')");
        let sql = script(db, &snap, &draft);
        assert!(sql.contains("enum('a','b') NULL DEFAULT 'a'"), "{sql}");
        assert!(sql.contains("varchar(20) NULL DEFAULT 'it''s'"), "{sql}");
        assert!(sql.contains("DEFAULT CURRENT_TIMESTAMP"), "{sql}");
        assert!(!sql.contains("DEFAULT a"), "{sql}");
    }
}

#[test]
fn mysql_set_default_is_quoted() {
    let (snap, draft) = edited("set('a','b')");
    let sql = script(DatabaseType::MySql, &snap, &draft);
    assert!(sql.contains("set('a','b') NULL DEFAULT 'a'"), "{sql}");
}

#[test]
fn mysql_expression_defaults_are_kept() {
    let (snap, mut draft) = edited("varchar(36)");
    draft.columns[1].def.default = Some("(UUID())".into());
    draft.columns[3].def.default = Some("''".into());
    let sql = script(DatabaseType::MySql, &snap, &draft);
    assert!(sql.contains("varchar(36) NULL DEFAULT (UUID())"), "{sql}");
    assert!(sql.contains("varchar(20) NULL DEFAULT ''"), "{sql}");
}

#[test]
fn postgres_text_defaults_are_quoted() {
    let (snap, draft) = edited("character varying(10)");
    let sql = script(DatabaseType::Postgres, &snap, &draft);
    assert!(sql.contains("SET DEFAULT 'a'"), "{sql}");
    assert!(sql.contains("DEFAULT 'it''s'"), "{sql}");
    assert!(sql.contains("SET DEFAULT CURRENT_TIMESTAMP"), "{sql}");
    assert!(!sql.contains("DEFAULT a"), "{sql}");
}

#[test]
fn sql_server_text_defaults_are_quoted() {
    let (snap, draft) = edited("nvarchar(10)");
    let sql = script(DatabaseType::SqlServer, &snap, &draft);
    assert!(sql.contains("DEFAULT (N'a') FOR"), "{sql}");
    assert!(sql.contains("DEFAULT ('it''s')"), "{sql}");
    assert!(sql.contains("DEFAULT (CURRENT_TIMESTAMP)"), "{sql}");
    assert!(!sql.contains("DEFAULT (a)"), "{sql}");
}

#[test]
fn sqlite_text_defaults_are_quoted() {
    let (snap, draft) = edited("TEXT");
    let sql = script(DatabaseType::Sqlite, &snap, &draft);
    assert!(sql.contains("DEFAULT 'a'"), "{sql}");
    assert!(sql.contains("DEFAULT 'it''s'"), "{sql}");
    assert!(!sql.contains("DEFAULT a"), "{sql}");
}

#[test]
fn quoted_catalog_defaults_round_trip_unchanged() {
    let mut snap = snapshot(None);
    snap.columns[1].default = Some("'a'".into());
    let draft = draft_of(&snap);
    assert!(diff(&snap, &draft).changes.is_empty());
}
