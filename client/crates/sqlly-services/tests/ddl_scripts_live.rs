//! Live checks that generated DDL scripts run back on a real server.
//!
//! PostgreSQL and MySQL/MariaDB cases are gated on the `SQLLY_PG_*` /
//! `SQLLY_MYSQL_*` environment variables (the same ones as
//! `metadata_dialects_live.rs`) and skip silently without them. Each case
//! works inside its own scratch schema or database and drops it afterwards.

#![cfg(not(target_arch = "wasm32"))]

use sqlly_core::{
    CellValue, ConnectionProfile, DatabaseType, ObjectKind, SecretProvider, SecretRef,
};
use sqlly_lang::sqllex::split_statements;
use sqlly_lang::sqlmodel::Dialect;
use sqlly_services::query::QueryEvent;
use sqlly_services::{ScriptSetObject, Services, render_create_script};

fn pg_profile() -> Option<ConnectionProfile> {
    let server = std::env::var("SQLLY_PG_SERVER").ok()?;
    let user = std::env::var("SQLLY_PG_USER").ok()?;
    let mut p = ConnectionProfile::sql_login(
        "pg-ddl",
        server,
        user,
        SecretRef {
            provider: SecretProvider::Environment,
            key: "SQLLY_PG_PASSWORD".into(),
        },
    );
    p.port = std::env::var("SQLLY_PG_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5432);
    p.database = std::env::var("SQLLY_PG_DATABASE")
        .ok()
        .or(Some("postgres".into()));
    p.database_type = DatabaseType::Postgres;
    p.encrypt = false;
    Some(p)
}

fn mysql_profile() -> Option<ConnectionProfile> {
    let server = std::env::var("SQLLY_MYSQL_SERVER").ok()?;
    let user = std::env::var("SQLLY_MYSQL_USER").ok()?;
    let mut p = ConnectionProfile::sql_login(
        "mysql-ddl",
        server,
        user,
        SecretRef {
            provider: SecretProvider::Environment,
            key: "SQLLY_MYSQL_PASSWORD".into(),
        },
    );
    p.port = std::env::var("SQLLY_MYSQL_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3306);
    p.database_type = DatabaseType::MySql;
    p.encrypt = false;
    Some(p)
}

/// Run `sql`, returning the server's error text instead of panicking.
async fn try_run(
    services: &Services,
    profile: &ConnectionProfile,
    sql: &str,
) -> Result<(), String> {
    let mut handle = services
        .queries
        .execute(profile, sql, 100)
        .await
        .map_err(|e| e.to_string())?;
    while let Some(event) = handle.events.recv().await {
        match event {
            QueryEvent::Error { message } => return Err(message),
            QueryEvent::Completed { .. } | QueryEvent::Cancelled => break,
            _ => {}
        }
    }
    Ok(())
}

async fn run(services: &Services, profile: &ConnectionProfile, sql: &str) {
    if let Err(e) = try_run(services, profile, sql).await {
        panic!("{sql}: {e}");
    }
}

async fn rows(services: &Services, profile: &ConnectionProfile, sql: &str) -> Vec<Vec<CellValue>> {
    let mut handle = services
        .queries
        .execute(profile, sql, 100)
        .await
        .expect("execute");
    let mut out = Vec::new();
    while let Some(event) = handle.events.recv().await {
        match event {
            QueryEvent::Rows { rows, .. } => out.extend(rows),
            QueryEvent::Error { message } => panic!("{sql}: {message}"),
            QueryEvent::Completed { .. } | QueryEvent::Cancelled => break,
            _ => {}
        }
    }
    out
}

fn text(cell: &CellValue) -> String {
    match cell {
        CellValue::String(s) => s.clone(),
        CellValue::Null => String::new(),
        CellValue::I64(n) => n.to_string(),
        other => format!("{other:?}"),
    }
}

/// Two PostgreSQL functions scripted together must run back as two
/// statements: `pg_get_functiondef` has no trailing `;`.
#[tokio::test]
async fn pg_function_script_runs_back() {
    let Some(profile) = pg_profile() else {
        eprintln!("skipping: SQLLY_PG_* env vars not set");
        return;
    };
    let database = profile.database.clone().unwrap();
    let services = Services::new();
    run(
        &services,
        &profile,
        "DROP SCHEMA IF EXISTS sqlly_ddl_fn CASCADE",
    )
    .await;
    run(&services, &profile, "CREATE SCHEMA sqlly_ddl_fn").await;
    run(
        &services,
        &profile,
        "CREATE FUNCTION sqlly_ddl_fn.one() RETURNS int LANGUAGE sql AS $$ SELECT 1; $$",
    )
    .await;
    run(
        &services,
        &profile,
        "CREATE FUNCTION sqlly_ddl_fn.two() RETURNS int LANGUAGE plpgsql AS $$ BEGIN RETURN 2; END $$",
    )
    .await;
    let objects = vec![
        ScriptSetObject::new("sqlly_ddl_fn", "one", ObjectKind::ScalarFunction),
        ScriptSetObject::new("sqlly_ddl_fn", "two", ObjectKind::ScalarFunction),
    ];
    let report = render_create_script(&services, &profile, &database, &objects, true)
        .await
        .unwrap();
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    let result = try_run(&services, &profile, &report.sql).await;
    let found = rows(
        &services,
        &profile,
        "SELECT sqlly_ddl_fn.one() + sqlly_ddl_fn.two()",
    )
    .await;
    run(&services, &profile, "DROP SCHEMA sqlly_ddl_fn CASCADE").await;
    result.unwrap_or_else(|e| panic!("script failed: {e}\n{}", report.sql));
    assert_eq!(text(&found[0][0]), "3");
}

/// A MySQL procedure whose body holds `;` must be scripted inside a
/// `DELIMITER` block so it runs back whole.
#[tokio::test]
async fn mysql_procedure_script_runs_back() {
    let Some(mut profile) = mysql_profile() else {
        eprintln!("skipping: SQLLY_MYSQL_* env vars not set");
        return;
    };
    let services = Services::new();
    run(
        &services,
        &profile,
        "DROP DATABASE IF EXISTS sqlly_ddl_proc",
    )
    .await;
    run(&services, &profile, "CREATE DATABASE sqlly_ddl_proc").await;
    profile.database = Some("sqlly_ddl_proc".into());
    run(
        &services,
        &profile,
        "CREATE TABLE t (x int); CREATE PROCEDURE touch() BEGIN INSERT INTO t VALUES (1); INSERT INTO t VALUES (2); END",
    )
    .await;
    let objects = vec![ScriptSetObject::new(
        "sqlly_ddl_proc",
        "touch",
        ObjectKind::StoredProcedure,
    )];
    let report = render_create_script(&services, &profile, "sqlly_ddl_proc", &objects, true)
        .await
        .unwrap();
    assert!(report.sql.contains("DELIMITER $$"), "{}", report.sql);
    // Run it the way a script file is run: split client-side (honoring
    // DELIMITER), one statement at a time.
    let mut result = Ok(());
    for statement in split_statements(&report.sql, Dialect::MySql) {
        if statement.text.trim().is_empty() {
            continue;
        }
        result = try_run(&services, &profile, statement.text).await;
        if result.is_err() {
            break;
        }
    }
    let call = try_run(&services, &profile, "CALL touch()").await;
    let count = rows(&services, &profile, "SELECT COUNT(*) FROM t").await;
    profile.database = None;
    run(&services, &profile, "DROP DATABASE sqlly_ddl_proc").await;
    result.unwrap_or_else(|e| panic!("script failed: {e}\n{}", report.sql));
    call.unwrap();
    assert_eq!(text(&count[0][0]), "2");
}

/// Run SQL File must hand SQL Server each batch with the comments written
/// above it: the server stores a view's source from the start of its batch,
/// so a stripped header comment was lost from the saved view.
#[tokio::test(flavor = "multi_thread")]
async fn sql_server_run_file_keeps_view_header_comment() {
    let Some(cfg) = sqlly_testkit::LiveSqlConfig::load() else {
        eprintln!("skipping: no live SQL config");
        return;
    };
    let mut profile = cfg.to_profile("live-run-file-comments");
    profile.database = Some("tempdb".into());
    let services = Services::new();
    run(
        &services,
        &profile,
        "DROP VIEW IF EXISTS dbo.sqlly_header_view",
    )
    .await;
    let script = "-- Orders summary: keep this note\n/* owner: data team */\nCREATE VIEW dbo.sqlly_header_view AS SELECT 1 AS x\nGO\n";
    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    let outcome = sqlly_services::run_script(
        services.queries.as_ref(),
        &profile,
        sqlly_services::ChunkReader::new(std::io::Cursor::new(script.as_bytes().to_vec())),
        script.len() as u64,
        &sqlly_services::ScriptRunOptions {
            dialect: Dialect::SqlServer,
            on_error: sqlly_services::OnError::Stop,
            transaction: sqlly_services::TransactionMode::None,
            coalesce_inserts: false,
            session_key: "run-file:live-comments".into(),
            coalesce_max_rows: 500,
            coalesce_max_bytes: 1024 * 1024,
            definer: None,
        },
        tx,
        sqlly_services::ScriptCancel::new(),
        |_| Box::pin(async { sqlly_services::ErrorDecision::Stop }),
    )
    .await
    .unwrap();
    let definition = rows(
        &services,
        &profile,
        "SELECT OBJECT_DEFINITION(OBJECT_ID('dbo.sqlly_header_view'))",
    )
    .await;
    run(
        &services,
        &profile,
        "DROP VIEW IF EXISTS dbo.sqlly_header_view",
    )
    .await;
    assert_eq!(outcome.units_failed, 0, "{:?}", outcome.failures);
    let definition = text(&definition[0][0]);
    assert!(
        definition
            .starts_with("-- Orders summary: keep this note\n/* owner: data team */\nCREATE VIEW"),
        "{definition}"
    );
}

/// Schema Compare's deploy step for a changed view must run on SQL Server
/// and keep the source view's header comment in the deployed definition
/// (it used to come out as `CREATE OR ALTER dbo.v AS …`, dropping both the
/// comment and the VIEW keyword).
#[tokio::test(flavor = "multi_thread")]
async fn sql_server_schema_compare_deploy_keeps_view_header() {
    use sqlly_services::admin::advanced_tools::SchemaObject;
    use sqlly_services::admin::schema_plan::{DeployOptions, compare_objects, plan_deploy};

    let Some(cfg) = sqlly_testkit::LiveSqlConfig::load() else {
        eprintln!("skipping: no live SQL config");
        return;
    };
    let mut profile = cfg.to_profile("live-schema-compare-comments");
    profile.database = Some("tempdb".into());
    let services = Services::new();
    let view = |definition: &str| SchemaObject {
        schema: "dbo".into(),
        name: "sqlly_compare_view".into(),
        kind: "VIEW".into(),
        definition: definition.into(),
    };
    let source =
        "-- Revenue view: reviewed by finance\nCREATE VIEW dbo.sqlly_compare_view AS SELECT 2 AS x";
    let target = "CREATE VIEW dbo.sqlly_compare_view AS SELECT 1 AS x";
    run(
        &services,
        &profile,
        "DROP VIEW IF EXISTS dbo.sqlly_compare_view",
    )
    .await;
    run(&services, &profile, target).await;
    // Deploy the changed view, then deploy what the server stored back over
    // itself: the second round must not change the stored text (SQL Server
    // blanks out OR ALTER, so a kept gap would grow by two spaces a round).
    let mut stored = Vec::new();
    let mut outcome = Ok(());
    let mut next_source = source.to_string();
    let mut current_target = target.to_string();
    for _ in 0..2 {
        let differences = compare_objects(
            &[view(&next_source)],
            &[view(&current_target)],
            DatabaseType::SqlServer,
            DatabaseType::SqlServer,
        );
        let plan = plan_deploy(
            &differences,
            DatabaseType::SqlServer,
            DatabaseType::SqlServer,
            DeployOptions::default(),
        );
        let Some(step) = plan
            .steps
            .iter()
            .find(|s| s.identity.contains("sqlly_compare_view"))
        else {
            break;
        };
        outcome = try_run(&services, &profile, &step.sql)
            .await
            .map_err(|e| format!("{e}\n{}", step.sql));
        if outcome.is_err() {
            break;
        }
        let definition = rows(
            &services,
            &profile,
            "SELECT OBJECT_DEFINITION(OBJECT_ID('dbo.sqlly_compare_view'))",
        )
        .await;
        let definition = text(&definition[0][0]);
        current_target = definition.clone();
        // Force a difference so the second round deploys too.
        next_source = definition.replace("SELECT 2 AS x", "SELECT 3 AS x");
        stored.push(definition);
    }
    run(
        &services,
        &profile,
        "DROP VIEW IF EXISTS dbo.sqlly_compare_view",
    )
    .await;
    outcome.unwrap_or_else(|e| panic!("deploy step failed: {e}"));
    assert_eq!(stored.len(), 2, "{stored:?}");
    assert!(
        stored[0].starts_with("-- Revenue view: reviewed by finance\nCREATE "),
        "{}",
        stored[0]
    );
    assert!(
        stored[0].contains(" VIEW dbo.sqlly_compare_view"),
        "{}",
        stored[0]
    );
    let head = |d: &str| d[..d.find("VIEW").unwrap()].to_string();
    assert_eq!(head(&stored[0]), head(&stored[1]), "{stored:#?}");
}

/// PostgreSQL functional indexes: the index-column metadata lists each
/// expression key (it used to drop them), INCLUDE and DESC are reported, and
/// a CREATE script built from the table snapshot recreates the functional
/// indexes exactly.
#[tokio::test]
async fn pg_functional_indexes_survive_metadata_and_create_script() {
    use sqlly_intel::metadata::MetadataQuery;
    use sqlly_intel::metadata::dialect::sql_for_engine;
    use sqlly_services::alter::render_create_tables;
    use sqlly_services::load_table_snapshot;

    let Some(profile) = pg_profile() else {
        eprintln!("skipping: SQLLY_PG_* env vars not set");
        return;
    };
    let database = profile.database.clone().unwrap();
    let services = Services::new();
    let indexdefs = "SELECT indexname || ': ' || indexdef FROM pg_indexes WHERE schemaname = 'sqlly_ix_live' ORDER BY indexname";
    run(
        &services,
        &profile,
        "DROP SCHEMA IF EXISTS sqlly_ix_live CASCADE",
    )
    .await;
    run(
        &services,
        &profile,
        "CREATE SCHEMA sqlly_ix_live; \
         CREATE TABLE sqlly_ix_live.feeds (id int PRIMARY KEY, feed uuid NOT NULL, email text, name text, body xml); \
         CREATE INDEX ix_lower ON sqlly_ix_live.feeds (lower(email)); \
         CREATE INDEX ix_mixed ON sqlly_ix_live.feeds (name DESC, lower(email)) INCLUDE (id); \
         CREATE UNIQUE INDEX ux_feed_body ON sqlly_ix_live.feeds (feed, md5(body::text)); \
         CREATE INDEX ix_plain ON sqlly_ix_live.feeds (name, email)",
    )
    .await;
    let before: Vec<String> = rows(&services, &profile, indexdefs)
        .await
        .iter()
        .map(|r| text(&r[0]))
        .collect();

    let sql = sql_for_engine(
        &MetadataQuery::IndexColumns {
            schema: "sqlly_ix_live".into(),
            table: "feeds".into(),
            index: "ix_mixed".into(),
        },
        DatabaseType::Postgres,
    )
    .unwrap();
    let mixed: Vec<(String, String, String)> = rows(&services, &profile, &sql)
        .await
        .iter()
        .map(|r| (text(&r[0]), text(&r[2]), text(&r[3])))
        .collect();
    let snapshot = load_table_snapshot(
        services.queries.as_ref(),
        &profile,
        &database,
        Some("sqlly_ix_live"),
        "feeds",
    )
    .await;
    let script = snapshot.as_ref().map(|snap| {
        render_create_tables(
            DatabaseType::Postgres,
            std::slice::from_ref(snap),
            &Default::default(),
        )
        .sql
    });
    let replay = match &script {
        Ok(script) => {
            run(&services, &profile, "DROP TABLE sqlly_ix_live.feeds").await;
            try_run(&services, &profile, script).await
        }
        Err(e) => Err(e.to_string()),
    };
    let after: Vec<String> = rows(&services, &profile, indexdefs)
        .await
        .iter()
        .map(|r| text(&r[0]))
        .collect();
    run(&services, &profile, "DROP SCHEMA sqlly_ix_live CASCADE").await;

    assert_eq!(
        mixed,
        vec![
            ("name".to_string(), "1".to_string(), "0".to_string()),
            ("lower(email)".to_string(), "0".to_string(), "0".to_string()),
            ("id".to_string(), "0".to_string(), "1".to_string()),
        ]
    );
    let script = script.unwrap();
    let snapshot = snapshot.unwrap();
    let names: Vec<&str> = snapshot.indexes.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(names, vec!["ix_plain"], "{snapshot:#?}");
    assert_eq!(snapshot.opaque_index_ddl.len(), 3, "{snapshot:#?}");
    replay.unwrap_or_else(|e| panic!("replay failed: {e}\n{script}"));
    assert_eq!(after, before, "{script}");
}

/// The generated CREATE TABLE for a PostgreSQL table keeps a default drawn
/// from a shared sequence (it used to turn into an identity), the serial,
/// the primary key and the foreign key, and recreates the same table.
#[tokio::test]
async fn pg_table_ddl_keeps_shared_sequence_default_and_keys() {
    use sqlly_intel::intellisense::{ClickedRole, NavigationTarget};
    let Some(profile) = pg_profile() else {
        eprintln!("skipping: SQLLY_PG_* env vars not set");
        return;
    };
    let database = profile.database.clone().unwrap();
    let services = Services::new();
    let shape = "SELECT c.column_name || ' ' || c.data_type || ' ' || c.is_nullable || ' ' || COALESCE(c.column_default, '-') || ' ' || c.is_identity FROM information_schema.columns c WHERE c.table_schema = 'sqlly_seq_live' AND c.table_name = 'orders' ORDER BY c.ordinal_position";
    let keys = "SELECT conname || ' ' || pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid = 'sqlly_seq_live.orders'::regclass ORDER BY conname";
    run(
        &services,
        &profile,
        "DROP SCHEMA IF EXISTS sqlly_seq_live CASCADE",
    )
    .await;
    run(
        &services,
        &profile,
        "CREATE SCHEMA sqlly_seq_live; \
         CREATE SEQUENCE sqlly_seq_live.shared_ticket_seq; \
         CREATE TABLE sqlly_seq_live.customers (id int PRIMARY KEY); \
         CREATE TABLE sqlly_seq_live.orders (id serial PRIMARY KEY, ticket bigint NOT NULL DEFAULT nextval('sqlly_seq_live.shared_ticket_seq'), customer_id int REFERENCES sqlly_seq_live.customers(id), status text NOT NULL DEFAULT 'new')",
    )
    .await;
    let read = |sql: &'static str| {
        let services = &services;
        let profile = &profile;
        async move {
            rows(services, profile, sql)
                .await
                .iter()
                .map(|r| text(&r[0]))
                .collect::<Vec<_>>()
        }
    };
    let shape_before = read(shape).await;
    let keys_before = read(keys).await;
    let doc = sqlly_services::fetch_ddl_document(
        services.queries.as_ref(),
        &profile,
        &database,
        &NavigationTarget::Table {
            catalog: None,
            schema: "sqlly_seq_live".into(),
            name: "orders".into(),
            clicked: ClickedRole::Table,
        },
    )
    .await
    .unwrap();
    let body = doc
        .primary_source()
        .map(|s| s.body.clone())
        .unwrap_or_default();
    run(&services, &profile, "DROP TABLE sqlly_seq_live.orders").await;
    let replay = try_run(&services, &profile, &body).await;
    let shape_after = if replay.is_ok() {
        read(shape).await
    } else {
        vec![]
    };
    let keys_after = if replay.is_ok() {
        read(keys).await
    } else {
        vec![]
    };
    run(&services, &profile, "DROP SCHEMA sqlly_seq_live CASCADE").await;

    assert!(doc.error.is_none(), "{:?}", doc.error);
    assert!(
        body.contains("DEFAULT nextval('sqlly_seq_live.shared_ticket_seq'::regclass)"),
        "{body}"
    );
    replay.unwrap_or_else(|e| panic!("replay failed: {e}\n{body}"));
    // The serial comes back as an identity column (its sequence is the
    // table's own); everything else must match exactly.
    let without_id = |v: &[String]| -> Vec<String> {
        v.iter()
            .filter(|l| !l.starts_with("id "))
            .cloned()
            .collect()
    };
    assert_eq!(
        without_id(&shape_after),
        without_id(&shape_before),
        "{body}"
    );
    assert_eq!(keys_after, keys_before, "{body}");
}

/// The generated CREATE TABLE for a MySQL/MariaDB table is the server's own:
/// `unsigned`, every default (MariaDB's `nextval(seq)` included), the
/// primary key and the engine survive, and it recreates the same table.
#[tokio::test]
async fn mysql_table_ddl_keeps_unsigned_defaults_and_primary_key() {
    use sqlly_intel::intellisense::{ClickedRole, NavigationTarget};
    let Some(mut profile) = mysql_profile() else {
        eprintln!("skipping: SQLLY_MYSQL_* env vars not set");
        return;
    };
    let services = Services::new();
    run(&services, &profile, "DROP DATABASE IF EXISTS sqlly_ddl_tbl").await;
    run(&services, &profile, "CREATE DATABASE sqlly_ddl_tbl").await;
    profile.database = Some("sqlly_ddl_tbl".into());
    let mariadb = text(&rows(&services, &profile, "SELECT VERSION()").await[0][0])
        .to_ascii_lowercase()
        .contains("mariadb");
    let ticket = if mariadb {
        run(&services, &profile, "CREATE SEQUENCE ticket_seq").await;
        "ticket bigint NOT NULL DEFAULT nextval(ticket_seq),"
    } else {
        "ticket bigint NOT NULL DEFAULT 5,"
    };
    run(
        &services,
        &profile,
        &format!(
            "CREATE TABLE orders (id int unsigned NOT NULL AUTO_INCREMENT, {ticket} status varchar(20) NOT NULL DEFAULT 'new', PRIMARY KEY (id)) ENGINE=InnoDB"
        ),
    )
    .await;
    run(
        &services,
        &profile,
        "INSERT INTO orders (status) VALUES ('a')",
    )
    .await;
    let before = text(&rows(&services, &profile, "SHOW CREATE TABLE orders").await[0][1]);
    let doc = sqlly_services::fetch_ddl_document(
        services.queries.as_ref(),
        &profile,
        "sqlly_ddl_tbl",
        &NavigationTarget::Table {
            catalog: None,
            schema: "sqlly_ddl_tbl".into(),
            name: "orders".into(),
            clicked: ClickedRole::Table,
        },
    )
    .await
    .unwrap();
    let body = doc
        .primary_source()
        .map(|s| s.body.clone())
        .unwrap_or_default();
    run(&services, &profile, "DROP TABLE orders").await;
    let replay = try_run(&services, &profile, &body).await;
    let after = if replay.is_ok() {
        text(&rows(&services, &profile, "SHOW CREATE TABLE orders").await[0][1])
    } else {
        String::new()
    };
    profile.database = None;
    run(&services, &profile, "DROP DATABASE sqlly_ddl_tbl").await;

    assert!(doc.error.is_none(), "{:?}", doc.error);
    assert!(body.contains("unsigned"), "{body}");
    assert!(body.contains("PRIMARY KEY (`id`)"), "{body}");
    assert!(body.contains("ENGINE=InnoDB"), "{body}");
    assert!(body.contains("DEFAULT 'new'"), "{body}");
    if mariadb {
        assert!(
            body.contains("nextval(`sqlly_ddl_tbl`.`ticket_seq`)"),
            "{body}"
        );
    }
    replay.unwrap_or_else(|e| panic!("replay failed: {e}\n{body}"));
    // The live AUTO_INCREMENT counter is the only difference.
    let strip = |s: &str| {
        s.split_whitespace()
            .filter(|w| !w.starts_with("AUTO_INCREMENT="))
            .collect::<Vec<_>>()
            .join(" ")
    };
    assert_eq!(strip(&after), strip(&before), "{body}");
}

/// MySQL/MariaDB triggers sharing a table, timing and event fire in
/// ACTION_ORDER. A structure dump must restore them in the same order: each
/// definition names the trigger it FOLLOWS, and the dump creates them in
/// firing order (alphabetical order reversed them).
#[tokio::test]
async fn mysql_dump_restores_trigger_firing_order() {
    use sqlly_services::{DumpContent, DumpOptions, run_database_dump};
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    let Some(mut profile) = mysql_profile() else {
        eprintln!("skipping: SQLLY_MYSQL_* env vars not set");
        return;
    };
    let services = Services::new();
    let reset = |profile: &ConnectionProfile| {
        let mut p = profile.clone();
        p.database = None;
        p
    };
    run(
        &services,
        &reset(&profile),
        "DROP DATABASE IF EXISTS sqlly_trg_live",
    )
    .await;
    run(
        &services,
        &reset(&profile),
        "CREATE DATABASE sqlly_trg_live",
    )
    .await;
    profile.database = Some("sqlly_trg_live".into());
    run(
        &services,
        &profile,
        "CREATE TABLE orders (id int PRIMARY KEY, note varchar(20) NOT NULL DEFAULT '')",
    )
    .await;
    // Created z, a, then q placed before a: firing order z, q, a — neither
    // creation order nor alphabetical.
    run(&services, &profile, "CREATE TRIGGER z_first BEFORE INSERT ON orders FOR EACH ROW SET NEW.note = CONCAT(NEW.note, 'z')").await;
    run(&services, &profile, "CREATE TRIGGER a_second BEFORE INSERT ON orders FOR EACH ROW SET NEW.note = CONCAT(NEW.note, 'a')").await;
    run(&services, &profile, "CREATE TRIGGER q_middle BEFORE INSERT ON orders FOR EACH ROW PRECEDES a_second SET NEW.note = CONCAT(NEW.note, 'q')").await;
    let order_sql = "SELECT GROUP_CONCAT(trigger_name ORDER BY action_order SEPARATOR ',') FROM information_schema.triggers WHERE trigger_schema = 'sqlly_trg_live'";
    let before = text(&rows(&services, &profile, order_sql).await[0][0]);

    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("dump.sql");
    let dumped = run_database_dump(
        &services,
        &profile,
        "sqlly_trg_live",
        &output,
        &DumpOptions {
            content: DumpContent::StructureOnly,
            ..DumpOptions::default()
        },
        Arc::new(AtomicBool::new(false)),
        &mut |_| {},
    )
    .await;
    let script = std::fs::read_to_string(&output).unwrap_or_default();
    run(&services, &reset(&profile), "DROP DATABASE sqlly_trg_live").await;
    run(
        &services,
        &reset(&profile),
        "CREATE DATABASE sqlly_trg_live",
    )
    .await;
    let mut restore = Ok(());
    for statement in split_statements(&script, Dialect::MySql) {
        if sqlly_lang::sqllex::strip_leading_comments(statement.text).is_empty() {
            continue;
        }
        restore = try_run(&services, &profile, statement.text).await;
        if restore.is_err() {
            break;
        }
    }
    let after = if restore.is_ok() {
        text(&rows(&services, &profile, order_sql).await[0][0])
    } else {
        String::new()
    };
    let fired = if restore.is_ok() {
        run(&services, &profile, "INSERT INTO orders (id) VALUES (1)").await;
        text(&rows(&services, &profile, "SELECT note FROM orders").await[0][0])
    } else {
        String::new()
    };
    run(&services, &reset(&profile), "DROP DATABASE sqlly_trg_live").await;

    dumped.unwrap();
    assert_eq!(before, "z_first,q_middle,a_second");
    restore.unwrap_or_else(|e| panic!("restore failed: {e}\n{script}"));
    assert_eq!(after, before, "{script}");
    assert_eq!(fired, "zqa", "{script}");
}

/// A real role named "PUBLIC" is not the PUBLIC pseudo-role: the Security
/// dialog's table grants for it must not show what was granted to PUBLIC
/// (information_schema spells both 'PUBLIC'), only its own grants.
#[tokio::test]
async fn pg_role_named_public_does_not_get_public_grants() {
    use sqlly_services::admin::security_sql::postgres::table_grants_query;

    let Some(profile) = pg_profile() else {
        eprintln!("skipping: SQLLY_PG_* env vars not set");
        return;
    };
    let services = Services::new();
    let exists = rows(
        &services,
        &profile,
        "SELECT count(*) FROM pg_catalog.pg_roles WHERE rolname = 'PUBLIC'",
    )
    .await;
    if text(&exists[0][0]) != "0" {
        eprintln!("skipping: a role named \"PUBLIC\" already exists on this server");
        return;
    }
    run(
        &services,
        &profile,
        "DROP SCHEMA IF EXISTS sqlly_public_live CASCADE",
    )
    .await;
    run(
        &services,
        &profile,
        "CREATE SCHEMA sqlly_public_live; CREATE TABLE sqlly_public_live.t (id int); \
         CREATE ROLE \"PUBLIC\" NOLOGIN; GRANT USAGE ON SCHEMA sqlly_public_live TO PUBLIC; \
         GRANT SELECT ON sqlly_public_live.t TO PUBLIC",
    )
    .await;
    let grants = |services: &Services| {
        let sql = table_grants_query("PUBLIC");
        let services = services.queries.clone();
        let profile = profile.clone();
        async move {
            let mut handle = services.execute(&profile, &sql, 100).await.unwrap();
            let mut out = Vec::new();
            while let Some(event) = handle.events.recv().await {
                match event {
                    QueryEvent::Rows { rows, .. } => out.extend(
                        rows.iter()
                            .filter(|r| text(&r[0]) == "sqlly_public_live")
                            .map(|r| text(&r[2])),
                    ),
                    QueryEvent::Error { message } => panic!("{message}"),
                    QueryEvent::Completed { .. } | QueryEvent::Cancelled => break,
                    _ => {}
                }
            }
            out
        }
    };
    let only_public = grants(&services).await;
    run(
        &services,
        &profile,
        "GRANT INSERT ON sqlly_public_live.t TO \"PUBLIC\"",
    )
    .await;
    let own = grants(&services).await;
    run(&services, &profile, "DROP SCHEMA sqlly_public_live CASCADE").await;
    run(&services, &profile, "DROP OWNED BY \"PUBLIC\"").await;
    run(&services, &profile, "DROP ROLE \"PUBLIC\"").await;

    assert!(only_public.is_empty(), "{only_public:?}");
    assert_eq!(own, vec!["INSERT".to_string()]);
}

/// Fail the enclosing `Result`-returning block with a message instead of
/// panicking, so a live test's cleanup still runs after a failed check.
macro_rules! check {
    ($cond:expr, $($arg:tt)+) => {
        if !$cond {
            return Err(format!($($arg)+));
        }
    };
}

/// Like [`rows`] but returning the server's error text instead of panicking.
async fn try_rows(
    services: &Services,
    profile: &ConnectionProfile,
    sql: &str,
) -> Result<Vec<Vec<CellValue>>, String> {
    let mut handle = services
        .queries
        .execute(profile, sql, 100)
        .await
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    while let Some(event) = handle.events.recv().await {
        match event {
            QueryEvent::Rows { rows, .. } => out.extend(rows),
            QueryEvent::Error { message } => return Err(format!("{sql}: {message}")),
            QueryEvent::Completed { .. } | QueryEvent::Cancelled => break,
            _ => {}
        }
    }
    Ok(out)
}

/// PostgreSQL Script as CREATE with privileges: a function whose implicit
/// PUBLIC EXECUTE was revoked and a table with grants (one WITH GRANT
/// OPTION, plus a privilege revoked from its owner) are dropped and
/// recreated from the generated DDL, and their ACLs come back identical.
/// Without the privileges section the function is executable by PUBLIC
/// again and the table has no grants.
///
/// One-sided ACLs, where the diff against the defaults is empty on one
/// side (a grant-only table, a function with only its PUBLIC EXECUTE
/// revoked, a sequence with one grant) and an ACL that was set but equals
/// the defaults used to fail outright with "ACL arrays must be
/// one-dimensional", leaving `-- Privileges unavailable` in the script.
/// Column grants and a grant made by a role other than the owner (replayed
/// inside SET SESSION AUTHORIZATION) round-trip too.
///
/// The scratch schema and roles are dropped on every path, including a
/// failed check: the body runs as a `Result` and cleanup follows it.
#[tokio::test]
async fn pg_ddl_with_privileges_recreates_the_same_acl() {
    let Some(profile) = pg_profile() else {
        eprintln!("skipping: SQLLY_PG_* env vars not set");
        return;
    };
    let database = profile.database.clone().unwrap();
    let services = Services::new();
    let outcome = pg_privileges_round_trip(&services, &profile, &database).await;
    let mut cleanup = Vec::new();
    for sql in [
        "DROP SCHEMA IF EXISTS sqlly_priv_live CASCADE",
        "DROP ROLE IF EXISTS sqlly_priv_reader",
        "DROP ROLE IF EXISTS sqlly_priv_lead",
    ] {
        if let Err(e) = try_run(&services, &profile, sql).await {
            cleanup.push(format!("{sql}: {e}"));
        }
    }
    outcome.unwrap_or_else(|e| panic!("{e}"));
    assert!(cleanup.is_empty(), "cleanup failed: {cleanup:?}");
}

async fn pg_privileges_round_trip(
    services: &Services,
    profile: &ConnectionProfile,
    database: &str,
) -> Result<(), String> {
    use sqlly_intel::intellisense::{ClickedRole, NavigationTarget, RoutineKind};
    use sqlly_services::pg_privileges::{PgAclObject, fetch_privilege_statements};

    try_run(
        services,
        profile,
        "DROP SCHEMA IF EXISTS sqlly_priv_live CASCADE",
    )
    .await?;
    try_run(
        services,
        profile,
        "DO $$ BEGIN \
           IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'sqlly_priv_reader') THEN CREATE ROLE sqlly_priv_reader NOLOGIN; END IF; \
           IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'sqlly_priv_lead') THEN CREATE ROLE sqlly_priv_lead NOLOGIN; END IF; \
         END $$",
    )
    .await?;
    try_run(
        services,
        profile,
        "CREATE SCHEMA sqlly_priv_live; \
         CREATE FUNCTION sqlly_priv_live.acltest(n int) RETURNS int LANGUAGE sql AS $f$ SELECT n $f$; \
         REVOKE ALL ON FUNCTION sqlly_priv_live.acltest(int) FROM PUBLIC; \
         GRANT EXECUTE ON FUNCTION sqlly_priv_live.acltest(int) TO sqlly_priv_reader; \
         CREATE TABLE sqlly_priv_live.orders (id int PRIMARY KEY); \
         GRANT SELECT, INSERT ON sqlly_priv_live.orders TO sqlly_priv_reader; \
         GRANT UPDATE ON sqlly_priv_live.orders TO sqlly_priv_reader WITH GRANT OPTION; \
         REVOKE TRUNCATE ON sqlly_priv_live.orders FROM CURRENT_USER; \
         CREATE TABLE sqlly_priv_live.grant_only (id int); \
         GRANT SELECT ON sqlly_priv_live.grant_only TO sqlly_priv_reader; \
         CREATE FUNCTION sqlly_priv_live.revoke_only() RETURNS int LANGUAGE sql AS $f$ SELECT 1 $f$; \
         REVOKE ALL ON FUNCTION sqlly_priv_live.revoke_only() FROM PUBLIC; \
         CREATE TABLE sqlly_priv_live.explicit_default (id int); \
         GRANT ALL ON sqlly_priv_live.explicit_default TO CURRENT_USER; \
         CREATE SEQUENCE sqlly_priv_live.ticket_seq; \
         GRANT USAGE ON SEQUENCE sqlly_priv_live.ticket_seq TO sqlly_priv_reader; \
         CREATE TABLE sqlly_priv_live.people (id int PRIMARY KEY, secret text); \
         GRANT SELECT (secret) ON sqlly_priv_live.people TO sqlly_priv_reader; \
         GRANT USAGE ON SCHEMA sqlly_priv_live TO sqlly_priv_lead; \
         GRANT SELECT ON sqlly_priv_live.people TO sqlly_priv_lead WITH GRANT OPTION; \
         SET SESSION AUTHORIZATION sqlly_priv_lead; \
         GRANT SELECT ON sqlly_priv_live.people TO sqlly_priv_reader; \
         RESET SESSION AUTHORIZATION",
    )
    .await?;
    let acl_sql = "SELECT (SELECT proacl::text FROM pg_proc WHERE oid = 'sqlly_priv_live.acltest(int)'::regprocedure), \
                          (SELECT relacl::text FROM pg_class WHERE oid = 'sqlly_priv_live.orders'::regclass), \
                          (SELECT relacl::text FROM pg_class WHERE oid = 'sqlly_priv_live.people'::regclass), \
                          (SELECT attacl::text FROM pg_attribute WHERE attrelid = 'sqlly_priv_live.people'::regclass AND attname = 'secret'), \
                          (SELECT (relacl IS NOT NULL)::text FROM pg_class WHERE oid = 'sqlly_priv_live.explicit_default'::regclass)";
    let acls = |r: Vec<Vec<CellValue>>| -> Vec<String> { r[0].iter().map(text).collect() };
    let before = acls(try_rows(services, profile, acl_sql).await?);
    check!(
        before[4] == "true",
        "explicit_default should have an explicit ACL: {before:?}"
    );

    // One-sided diffs through the module itself.
    let relation = |name: &str| PgAclObject::Relation {
        schema: "sqlly_priv_live".into(),
        name: name.into(),
    };
    let routine = |name: &str| PgAclObject::Routine {
        schema: "sqlly_priv_live".into(),
        name: name.into(),
    };
    let expected: [(PgAclObject, Vec<&str>); 5] = [
        (
            relation("grant_only"),
            vec!["GRANT SELECT ON TABLE sqlly_priv_live.grant_only TO sqlly_priv_reader;"],
        ),
        (
            routine("revoke_only"),
            vec!["REVOKE EXECUTE ON FUNCTION sqlly_priv_live.revoke_only() FROM PUBLIC;"],
        ),
        (relation("explicit_default"), vec![]),
        (
            relation("ticket_seq"),
            vec!["GRANT USAGE ON SEQUENCE sqlly_priv_live.ticket_seq TO sqlly_priv_reader;"],
        ),
        (
            relation("people"),
            vec![
                "GRANT SELECT ON TABLE sqlly_priv_live.people TO sqlly_priv_lead WITH GRANT OPTION;",
                "GRANT SELECT(secret) ON TABLE sqlly_priv_live.people TO sqlly_priv_reader;",
                "SET SESSION AUTHORIZATION sqlly_priv_lead;",
                "GRANT SELECT ON TABLE sqlly_priv_live.people TO sqlly_priv_reader;",
                "RESET SESSION AUTHORIZATION;",
            ],
        ),
    ];
    for (object, statements) in &expected {
        let got = fetch_privilege_statements(services.queries.as_ref(), profile, database, object)
            .await
            .map_err(|e| format!("{object:?}: {e}"))?;
        check!(&got == statements, "{object:?}: {got:#?}");
    }

    let routine = NavigationTarget::Routine {
        catalog: None,
        schema: "sqlly_priv_live".into(),
        name: "acltest".into(),
        kind: RoutineKind::Function,
        clicked: ClickedRole::Function,
    };
    let table = |name: &str| NavigationTarget::Table {
        catalog: None,
        schema: "sqlly_priv_live".into(),
        name: name.into(),
        clicked: ClickedRole::Table,
    };
    let options = sqlly_services::DdlOptions {
        include_privileges: true,
    };
    let mut bodies = Vec::new();
    for target in [&table("orders"), &table("people"), &routine] {
        let doc = sqlly_services::fetch_ddl_document_with(
            services.queries.as_ref(),
            profile,
            database,
            target,
            options,
        )
        .await
        .map_err(|e| e.to_string())?;
        check!(doc.error.is_none(), "{:?}", doc.error);
        bodies.push(
            doc.primary_source()
                .map(|s| s.body.clone())
                .unwrap_or_default(),
        );
    }
    // A structure dump with Include privileges carries the same statements.
    let dump_dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let dump_file = dump_dir.path().join("dump.sql");
    sqlly_services::run_database_dump(
        services,
        profile,
        database,
        &dump_file,
        &sqlly_services::DumpOptions {
            content: sqlly_services::DumpContent::StructureOnly,
            schemas: Some(vec!["sqlly_priv_live".into()]),
            include_privileges: true,
            ..Default::default()
        },
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        &mut |_| {},
    )
    .await
    .map_err(|e| e.to_string())?;
    let dump = std::fs::read_to_string(&dump_file).map_err(|e| e.to_string())?;
    try_run(
        services,
        profile,
        "DROP FUNCTION sqlly_priv_live.acltest(int); DROP TABLE sqlly_priv_live.orders; DROP TABLE sqlly_priv_live.people",
    )
    .await?;
    let script = bodies.join("\n\n");
    for body in &bodies {
        try_run(services, profile, body)
            .await
            .map_err(|e| format!("replay failed: {e}\n{script}"))?;
    }
    let after = acls(try_rows(services, profile, acl_sql).await?);

    check!(
        script
            .contains("REVOKE EXECUTE ON FUNCTION sqlly_priv_live.acltest(n integer) FROM PUBLIC;"),
        "{script}"
    );
    check!(
        script
            .contains("GRANT SELECT(secret) ON TABLE sqlly_priv_live.people TO sqlly_priv_reader;"),
        "{script}"
    );
    check!(!script.contains("Privileges unavailable"), "{script}");
    check!(
        after == before,
        "before {before:?}\nafter {after:?}\n{script}"
    );
    check!(
        dump.contains("REVOKE EXECUTE ON FUNCTION sqlly_priv_live.acltest(n integer) FROM PUBLIC;"),
        "{dump}"
    );
    check!(
        dump.contains(
            "GRANT UPDATE ON TABLE sqlly_priv_live.orders TO sqlly_priv_reader WITH GRANT OPTION;"
        ),
        "{dump}"
    );
    check!(
        dump.contains("GRANT SELECT ON TABLE sqlly_priv_live.grant_only TO sqlly_priv_reader;"),
        "{dump}"
    );
    check!(
        dump.contains("REVOKE EXECUTE ON FUNCTION sqlly_priv_live.revoke_only() FROM PUBLIC;"),
        "{dump}"
    );
    check!(!dump.contains("Privileges unavailable"), "{dump}");
    Ok(())
}
