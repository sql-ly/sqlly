//! Live-server checks of "Script as CREATE" fidelity: the catalog snapshot
//! must carry what the script restates (SQL Server column collations and
//! table storage options; Oracle CHAR/BYTE length semantics and INTEGER),
//! and the rendered CREATE must run and reproduce those facts on a copy.
//!
//! Self-skips when no live SQL Server config / `SQLLY_ORACLE_*` is present.

use std::collections::HashSet;

use sqlly_core::{CellValue, ConnectionProfile, DatabaseType, SecretProvider, SecretRef};
use sqlly_services::alter::render_create_tables;
use sqlly_services::{DefaultQueryExecutor, QueryEvent, QueryExecutor, load_table_snapshot};
use sqlly_testkit::LiveSqlConfig;

async fn run(queries: &DefaultQueryExecutor, profile: &ConnectionProfile, sql: &str) {
    let mut handle = queries
        .execute(profile, sql, 100)
        .await
        .expect("execute against live server");
    while let Some(event) = handle.events.recv().await {
        if let QueryEvent::Error { message } = event {
            panic!("{sql}: {message}");
        }
    }
}

/// Run `sql`, ignoring a server error (cleanup of objects that may not exist).
async fn run_quiet(queries: &DefaultQueryExecutor, profile: &ConnectionProfile, sql: &str) {
    if let Ok(mut handle) = queries.execute(profile, sql, 100).await {
        while handle.events.recv().await.is_some() {}
    }
}

async fn rows(
    queries: &DefaultQueryExecutor,
    profile: &ConnectionProfile,
    sql: &str,
) -> Vec<Vec<CellValue>> {
    let mut handle = queries.execute(profile, sql, 100).await.expect("execute");
    let mut out = Vec::new();
    while let Some(event) = handle.events.recv().await {
        match event {
            QueryEvent::Rows { rows, .. } => out.extend(rows),
            QueryEvent::Error { message } => panic!("{sql}: {message}"),
            _ => {}
        }
    }
    out
}

fn text(cell: &CellValue) -> String {
    match cell {
        CellValue::String(s) => s.trim().to_string(),
        CellValue::Null => String::new(),
        CellValue::I64(n) => n.to_string(),
        CellValue::F64(n) => n.to_string(),
        other => format!("{other:?}"),
    }
}

/// Run `sql`, reporting the server's error instead of panicking.
async fn try_run(
    queries: &DefaultQueryExecutor,
    profile: &ConnectionProfile,
    sql: &str,
) -> Result<(), String> {
    let mut handle = queries
        .execute(profile, sql, 100)
        .await
        .map_err(|e| e.to_string())?;
    while let Some(event) = handle.events.recv().await {
        if let QueryEvent::Error { message } = event {
            return Err(message);
        }
    }
    Ok(())
}

/// The first statement (the CREATE TABLE) of a rendered script, without its
/// terminator.
fn create_statement(script: &str) -> String {
    let at = script.find("CREATE TABLE").expect("a CREATE TABLE");
    let rest = &script[at..];
    let end = rest.find(";\n").expect("terminated CREATE");
    rest[..end].to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn sql_server_create_script_keeps_collations_and_storage_options() {
    let Some(cfg) = LiveSqlConfig::load() else {
        eprintln!("skipping: no live SQL config");
        return;
    };
    const DB: &str = "tempdb";
    const TABLE: &str = "sqlly_create_fidelity";
    const COPY: &str = "sqlly_create_fidelity_copy";
    const TEMPORAL: &str = "sqlly_create_temporal";
    let mut in_db = cfg.to_profile("live-create-fidelity");
    in_db.database = Some(DB.to_string());
    let queries = DefaultQueryExecutor::new();

    for t in [COPY, TABLE] {
        run_quiet(&queries, &in_db, &format!("DROP TABLE IF EXISTS dbo.{t}")).await;
    }
    run_quiet(
        &queries,
        &in_db,
        &format!(
            "IF OBJECT_ID('dbo.{TEMPORAL}') IS NOT NULL ALTER TABLE dbo.{TEMPORAL} SET (SYSTEM_VERSIONING = OFF); DROP TABLE IF EXISTS dbo.{TEMPORAL}; DROP TABLE IF EXISTS dbo.{TEMPORAL}_history"
        ),
    )
    .await;
    run(
        &queries,
        &in_db,
        &format!(
            "CREATE TABLE dbo.{TABLE} (\
             id int NOT NULL CONSTRAINT pk_{TABLE} PRIMARY KEY, \
             code varchar(20) COLLATE Latin1_General_CS_AS NOT NULL, \
             plain nvarchar(40) NULL) WITH (DATA_COMPRESSION = PAGE); \
             CREATE INDEX ix_{TABLE}_code ON dbo.{TABLE} (code) WITH (DATA_COMPRESSION = ROW)"
        ),
    )
    .await;
    run(
        &queries,
        &in_db,
        &format!(
            "CREATE TABLE dbo.{TEMPORAL} (\
             id int NOT NULL PRIMARY KEY, \
             valid_from datetime2 GENERATED ALWAYS AS ROW START NOT NULL, \
             valid_to datetime2 GENERATED ALWAYS AS ROW END NOT NULL, \
             PERIOD FOR SYSTEM_TIME (valid_from, valid_to)) \
             WITH (SYSTEM_VERSIONING = ON (HISTORY_TABLE = dbo.{TEMPORAL}_history))"
        ),
    )
    .await;

    let profile = cfg.to_profile("live-create-fidelity");
    let snapshot = load_table_snapshot(&queries, &profile, DB, Some("dbo"), TABLE)
        .await
        .expect("snapshot");
    let temporal = load_table_snapshot(&queries, &profile, DB, Some("dbo"), TEMPORAL)
        .await
        .expect("temporal snapshot");

    let code = snapshot
        .column_extras
        .get("code")
        .and_then(|e| e.collation.clone());
    assert_eq!(code.as_deref(), Some("Latin1_General_CS_AS"));
    assert!(
        snapshot
            .column_extras
            .get("plain")
            .and_then(|e| e.collation.as_ref())
            .is_none(),
        "a database-default collation is implied, not restated"
    );
    assert_eq!(snapshot.storage.data_compression.as_deref(), Some("PAGE"));
    assert_eq!(
        snapshot
            .storage
            .index_compression
            .get(&format!("ix_{TABLE}_code"))
            .map(String::as_str),
        Some("ROW")
    );
    assert!(
        snapshot.storage.filegroup.is_none(),
        "PRIMARY is the default filegroup"
    );
    assert!(
        temporal
            .storage
            .unscripted
            .iter()
            .any(|w| w.contains("system-versioned")),
        "{:?}",
        temporal.storage.unscripted
    );

    let rendered = render_create_tables(
        DatabaseType::SqlServer,
        std::slice::from_ref(&snapshot),
        &HashSet::new(),
    );
    let create = create_statement(&rendered.sql);
    assert!(
        create.contains("[code] varchar(20) COLLATE Latin1_General_CS_AS NOT NULL"),
        "{create}"
    );
    assert!(
        create.ends_with(") WITH (DATA_COMPRESSION = PAGE)"),
        "{create}"
    );
    assert!(
        rendered.sql.contains(&format!(
            "CREATE INDEX [ix_{TABLE}_code] ON [dbo].[{TABLE}] ([code]) WITH (DATA_COMPRESSION = ROW);"
        )),
        "{}",
        rendered.sql
    );
    let temporal_script = render_create_tables(
        DatabaseType::SqlServer,
        std::slice::from_ref(&temporal),
        &HashSet::new(),
    );
    assert!(
        temporal_script
            .warnings
            .iter()
            .any(|w| w.contains("SYSTEM_VERSIONING")),
        "{:?}",
        temporal_script.warnings
    );

    // The script runs, and the copy really has the collation and compression.
    let copy_sql = create
        .replace(&format!("[{TABLE}]"), &format!("[{COPY}]"))
        .replace(&format!("[pk_{TABLE}]"), &format!("[pk_{COPY}]"));
    run(&queries, &in_db, &copy_sql).await;
    let facts = rows(
        &queries,
        &in_db,
        &format!(
            "SELECT (SELECT collation_name FROM sys.columns WHERE object_id = OBJECT_ID('dbo.{COPY}') AND name = 'code'), \
             (SELECT TOP 1 data_compression_desc FROM sys.partitions WHERE object_id = OBJECT_ID('dbo.{COPY}') AND index_id IN (0, 1))"
        ),
    )
    .await;

    run_quiet(
        &queries,
        &in_db,
        &format!("DROP TABLE IF EXISTS dbo.{COPY}; DROP TABLE IF EXISTS dbo.{TABLE}"),
    )
    .await;
    run_quiet(
        &queries,
        &in_db,
        &format!(
            "ALTER TABLE dbo.{TEMPORAL} SET (SYSTEM_VERSIONING = OFF); DROP TABLE IF EXISTS dbo.{TEMPORAL}; DROP TABLE IF EXISTS dbo.{TEMPORAL}_history"
        ),
    )
    .await;

    let row = facts.first().expect("copy facts");
    assert_eq!(text(&row[0]), "Latin1_General_CS_AS");
    assert_eq!(text(&row[1]), "PAGE");
}

fn oracle_profile() -> Option<ConnectionProfile> {
    let server = std::env::var("SQLLY_ORACLE_SERVER").ok()?;
    let user = std::env::var("SQLLY_ORACLE_USER").ok()?;
    let mut p = ConnectionProfile::sql_login(
        "oracle-create-fidelity",
        server,
        user,
        SecretRef {
            provider: SecretProvider::Environment,
            key: "SQLLY_ORACLE_PASSWORD".into(),
        },
    );
    p.port = std::env::var("SQLLY_ORACLE_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1521);
    p.database = Some(std::env::var("SQLLY_ORACLE_SERVICE").unwrap_or_else(|_| "FREEPDB1".into()));
    p.database_type = DatabaseType::Oracle;
    p.encrypt = false;
    Some(p)
}

#[tokio::test(flavor = "multi_thread")]
async fn oracle_create_script_keeps_length_semantics_and_integer() {
    let Some(profile) = oracle_profile() else {
        eprintln!("skipping: no SQLLY_ORACLE_* config");
        return;
    };
    const TABLE: &str = "SQLLY_CREATE_FIDELITY";
    const COPY: &str = "SQLLY_CREATE_FIDELITY_COPY";
    let owner = std::env::var("SQLLY_ORACLE_USER")
        .unwrap_or_default()
        .to_ascii_uppercase();
    let database = profile.database.clone().unwrap_or_default();
    let queries = DefaultQueryExecutor::new();
    for t in [COPY, TABLE] {
        run_quiet(&queries, &profile, &format!("DROP TABLE {t} PURGE")).await;
    }
    run(
        &queries,
        &profile,
        &format!(
            "CREATE TABLE {TABLE} (\
             ID INTEGER NOT NULL, \
             CHARS VARCHAR2(10 CHAR), \
             BYTES VARCHAR2(10 BYTE), \
             FIXED CHAR(3 CHAR), \
             NATIONAL NVARCHAR2(5), \
             ANY_NUMBER NUMBER, \
             MONEY NUMBER(10,2), \
             CONSTRAINT PK_{TABLE} PRIMARY KEY (ID))"
        ),
    )
    .await;

    let snapshot = load_table_snapshot(&queries, &profile, &database, Some(&owner), TABLE).await;
    let snapshot = match snapshot {
        Ok(s) => s,
        Err(err) => {
            run_quiet(&queries, &profile, &format!("DROP TABLE {TABLE} PURGE")).await;
            panic!("snapshot: {err}");
        }
    };
    let ty = |n: &str| {
        snapshot
            .columns
            .iter()
            .find(|c| c.name == n)
            .map(|c| c.data_type.clone())
            .unwrap_or_default()
    };
    let types = [
        ty("ID"),
        ty("CHARS"),
        ty("BYTES"),
        ty("FIXED"),
        ty("NATIONAL"),
        ty("ANY_NUMBER"),
        ty("MONEY"),
    ];

    let rendered = render_create_tables(
        DatabaseType::Oracle,
        std::slice::from_ref(&snapshot),
        &HashSet::new(),
    );
    let copy_sql = create_statement(&rendered.sql)
        .replace(&format!("\"{TABLE}\""), &format!("\"{COPY}\""))
        .replace(&format!("\"PK_{TABLE}\""), &format!("\"PK_{COPY}\""));
    let copy_result = try_run(&queries, &profile, &copy_sql).await;
    let mut copy_types = Vec::new();
    if copy_result.is_ok() {
        copy_types = rows(
            &queries,
            &profile,
            &format!(
                "SELECT column_name, data_type, char_used, data_precision, data_scale FROM user_tab_cols WHERE table_name = '{COPY}' ORDER BY column_id"
            ),
        )
        .await;
    }
    for t in [COPY, TABLE] {
        run_quiet(&queries, &profile, &format!("DROP TABLE {t} PURGE")).await;
    }

    assert_eq!(
        types,
        [
            "INTEGER",
            "VARCHAR2(10 CHAR)",
            "VARCHAR2(10 BYTE)",
            "CHAR(3 CHAR)",
            "NVARCHAR2(5)",
            "NUMBER",
            "NUMBER(10,2)"
        ]
        .map(str::to_string)
    );
    assert!(
        copy_result.is_ok(),
        "rendered CREATE did not run: {copy_result:?}\n{copy_sql}"
    );
    assert_eq!(copy_types.len(), 7, "{copy_types:?}");
    let used: Vec<String> = copy_types.iter().map(|r| text(&r[2])).collect();
    assert_eq!(used[1], "C", "{copy_types:?}");
    assert_eq!(used[2], "B", "{copy_types:?}");
    // INTEGER round-trips as NUMBER(*,0): precision NULL, scale 0.
    assert_eq!(text(&copy_types[0][3]), "");
    assert_eq!(text(&copy_types[0][4]), "0");
}

/// The structure editor's ALTER on SQL Server keeps a retyped column's
/// collation and moves its bound default around the rename and retype —
/// the script runs, and the column ends up with the same collation and
/// default.
#[tokio::test(flavor = "multi_thread")]
async fn sql_server_alter_script_keeps_collation_and_default() {
    use sqlly_services::alter::{DraftColumn, RenderOptions, TableDraft, diff, render};
    let Some(cfg) = LiveSqlConfig::load() else {
        eprintln!("skipping: no live SQL config");
        return;
    };
    const DB: &str = "tempdb";
    const TABLE: &str = "sqlly_alter_fidelity";
    let mut in_db = cfg.to_profile("live-alter-fidelity");
    in_db.database = Some(DB.to_string());
    let queries = DefaultQueryExecutor::new();
    run_quiet(
        &queries,
        &in_db,
        &format!("DROP TABLE IF EXISTS dbo.{TABLE}"),
    )
    .await;
    run(
        &queries,
        &in_db,
        &format!(
            "CREATE TABLE dbo.{TABLE} (\
             id int NOT NULL PRIMARY KEY, \
             code varchar(20) COLLATE Latin1_General_CS_AS NOT NULL CONSTRAINT df_{TABLE}_code DEFAULT ('x'))"
        ),
    )
    .await;
    let profile = cfg.to_profile("live-alter-fidelity");
    let snapshot = load_table_snapshot(&queries, &profile, DB, Some("dbo"), TABLE)
        .await
        .expect("snapshot");
    let draft = TableDraft {
        schema: snapshot.schema.clone(),
        table: snapshot.table.clone(),
        columns: snapshot
            .columns
            .iter()
            .map(|c| {
                let mut def = c.clone();
                if def.name == "code" {
                    def.name = "code_ref".into();
                    def.data_type = "varchar(40)".into();
                }
                DraftColumn {
                    origin: Some(c.name.clone()),
                    def,
                }
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
    let set = diff(&snapshot, &draft);
    let script = render(
        DatabaseType::SqlServer,
        &snapshot,
        &draft,
        &set,
        &RenderOptions::default(),
    )
    .expect("render")
    .sql;
    let result = try_run(&queries, &in_db, &script).await;
    let facts = if result.is_ok() {
        rows(
            &queries,
            &in_db,
            &format!(
                "SELECT c.collation_name, c.max_length, dc.definition FROM sys.columns c \
                 LEFT JOIN sys.default_constraints dc ON dc.parent_object_id = c.object_id AND dc.parent_column_id = c.column_id \
                 WHERE c.object_id = OBJECT_ID('dbo.{TABLE}') AND c.name = 'code_ref'"
            ),
        )
        .await
    } else {
        Vec::new()
    };
    run_quiet(
        &queries,
        &in_db,
        &format!("DROP TABLE IF EXISTS dbo.{TABLE}"),
    )
    .await;

    assert!(result.is_ok(), "ALTER script failed: {result:?}\n{script}");
    let row = facts.first().expect("renamed column exists");
    assert_eq!(text(&row[0]), "Latin1_General_CS_AS", "{script}");
    assert_eq!(text(&row[1]), "40");
    assert_eq!(text(&row[2]), "('x')");
}
