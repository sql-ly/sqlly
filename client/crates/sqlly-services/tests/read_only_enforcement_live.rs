//! Live per-engine proof that AI/agent/MCP queries run under a read-only mode
//! the DATABASE enforces, on top of the SELECT-only parser gate:
//!
//! - a plain SELECT works under the enforced profile;
//! - a write the parser cannot see (a function that inserts, a data-modifying
//!   CTE) is refused by the server itself, and the agent tool host reports it
//!   as "blocked: AI queries run read-only …" rather than a raw driver error;
//! - on SQL Server, which has no read-only transaction, a write that reaches
//!   the server inside the rollback wrap leaves no trace, and the statement
//!   screen refuses what a rollback could not undo.
//!
//! Gated on the usual `SQLLY_PG_*` / `SQLLY_MYSQL_*` / `SQLLY_SQL_*` env vars;
//! every test skips silently without a live server. Each test owns a
//! `sqlly_ro_probe` table (and a `sqlly_ro_write` function) it recreates.

use sqlly_ai::catalog::AgentToolHost;
use sqlly_core::{CellValue, ConnectionProfile, DatabaseType, SecretProvider, SecretRef};
use sqlly_intel::query::safety::{READ_ONLY_REFUSAL_PREFIX, is_read_only_refusal};
use sqlly_services::{QueryEvent, Services, ServicesToolHost};

fn env_profile(prefix: &str, db: DatabaseType, default_port: u16) -> Option<ConnectionProfile> {
    let server = std::env::var(format!("{prefix}_SERVER")).ok()?;
    let user = std::env::var(format!("{prefix}_USER"))
        .or_else(|_| std::env::var(format!("{prefix}_USERNAME")))
        .ok()?;
    let mut p = ConnectionProfile::sql_login(
        "read-only-live",
        server,
        user,
        SecretRef {
            provider: SecretProvider::Environment,
            key: format!("{prefix}_PASSWORD"),
        },
    );
    p.port = std::env::var(format!("{prefix}_PORT"))
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default_port);
    p.database = std::env::var(format!("{prefix}_DATABASE")).ok();
    p.database_type = db;
    p.encrypt = matches!(db, DatabaseType::SqlServer);
    p.trust_server_certificate = true;
    Some(p)
}

/// Run `sql` through the real executor and collect the first result set's
/// rows; `Err` carries the terminal error message.
async fn run(
    services: &Services,
    profile: &ConnectionProfile,
    sql: &str,
) -> Result<Vec<Vec<CellValue>>, String> {
    let mut handle = services
        .queries
        .execute(profile, sql, 64)
        .await
        .map_err(|e| e.to_string())?;
    let mut rows = Vec::new();
    let mut error = None;
    while let Some(event) = handle.events.recv().await {
        match event {
            QueryEvent::Rows {
                index: 0,
                rows: batch,
            } => rows.extend(batch),
            QueryEvent::Error { message } => error = Some(message),
            _ => {}
        }
    }
    match error {
        Some(message) => Err(message),
        None => Ok(rows),
    }
}

async fn count(services: &Services, profile: &ConnectionProfile, sql: &str) -> i64 {
    let rows = run(services, profile, sql).await.expect("count query");
    match rows.first().and_then(|r| r.first()) {
        Some(CellValue::I64(n)) => *n,
        Some(CellValue::String(s)) => s.parse().expect("numeric count"),
        // MySQL's text protocol hands COUNT(*) back as a binary-flagged blob.
        Some(CellValue::Bytes(b)) => String::from_utf8_lossy(b).parse().expect("numeric count"),
        other => panic!("unexpected count cell {other:?}"),
    }
}

/// The profile the drivers see for an AI-composed query WITHOUT the parser
/// gate, so the write reaches the server and the server's refusal is what
/// the test observes.
fn enforced_only(profile: &ConnectionProfile) -> ConnectionProfile {
    let mut p = profile.clone();
    p.enforce_read_only = true;
    p.block_all_but_select = false;
    p
}

fn tool_host(services: &Services, profile: &ConnectionProfile) -> ServicesToolHost {
    ServicesToolHost::new(
        profile.clone(),
        services.metadata.clone(),
        services.queries.clone(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_server_refuses_hidden_writes_under_read_only() {
    let Some(plain) = env_profile("SQLLY_PG", DatabaseType::Postgres, 5432) else {
        eprintln!("skipping: SQLLY_PG_* env vars not set");
        return;
    };
    let services = Services::new();
    run(
        &services,
        &plain,
        "DROP FUNCTION IF EXISTS sqlly_ro_write(); \
         DROP TABLE IF EXISTS sqlly_ro_probe; \
         CREATE TABLE sqlly_ro_probe (id int); \
         INSERT INTO sqlly_ro_probe VALUES (1); \
         CREATE FUNCTION sqlly_ro_write() RETURNS int LANGUAGE plpgsql AS $$ \
         BEGIN INSERT INTO sqlly_ro_probe VALUES (2); RETURN 1; END $$;",
    )
    .await
    .expect("setup");

    let enforced = enforced_only(&plain);
    assert_eq!(
        count(&services, &enforced, "SELECT count(*) FROM sqlly_ro_probe").await,
        1,
        "a SELECT works under the read-only transaction"
    );
    let err = run(
        &services,
        &enforced,
        "WITH x AS (INSERT INTO sqlly_ro_probe VALUES (2) RETURNING *) SELECT * FROM x",
    )
    .await
    .unwrap_err();
    assert!(
        is_read_only_refusal(DatabaseType::Postgres, &err),
        "the server must refuse the data-modifying CTE: {err}"
    );

    // Through the agent tool host: the writing function passes the parser
    // gate (it is a SELECT), the server refuses it, and the tool result says
    // why in plain words.
    let host = tool_host(&services, &plain);
    let ok = host
        .run_readonly_query("SELECT count(*) AS n FROM sqlly_ro_probe", 10)
        .await
        .unwrap();
    assert!(ok.contains("1 rows shown"), "{ok}");
    let err = host
        .run_readonly_query("SELECT sqlly_ro_write()", 10)
        .await
        .unwrap_err();
    assert!(err.starts_with(READ_ONLY_REFUSAL_PREFIX), "{err}");

    assert_eq!(
        count(&services, &plain, "SELECT count(*) FROM sqlly_ro_probe").await,
        1,
        "nothing was written"
    );
    run(
        &services,
        &plain,
        "DROP FUNCTION sqlly_ro_write(); DROP TABLE sqlly_ro_probe;",
    )
    .await
    .expect("teardown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mysql_server_refuses_hidden_writes_under_read_only() {
    let Some(plain) = env_profile("SQLLY_MYSQL", DatabaseType::MySql, 3306) else {
        eprintln!("skipping: SQLLY_MYSQL_* env vars not set");
        return;
    };
    let services = Services::new();
    run(
        &services,
        &plain,
        "DROP FUNCTION IF EXISTS sqlly_ro_write; \
         DROP TABLE IF EXISTS sqlly_ro_probe; \
         CREATE TABLE sqlly_ro_probe (id int); \
         INSERT INTO sqlly_ro_probe VALUES (1);",
    )
    .await
    .expect("setup");
    // Sent alone so the routine body's semicolons are not split client-side.
    run(
        &services,
        &plain,
        "CREATE FUNCTION sqlly_ro_write() RETURNS INT DETERMINISTIC MODIFIES SQL DATA \
         BEGIN INSERT INTO sqlly_ro_probe VALUES (2); RETURN 1; END",
    )
    .await
    .expect("create function");

    let enforced = enforced_only(&plain);
    assert_eq!(
        count(&services, &enforced, "SELECT COUNT(*) FROM sqlly_ro_probe").await,
        1,
        "a SELECT works under START TRANSACTION READ ONLY"
    );
    let err = run(&services, &enforced, "SELECT sqlly_ro_write()")
        .await
        .unwrap_err();
    assert!(
        is_read_only_refusal(DatabaseType::MySql, &err),
        "the server must refuse the writing function: {err}"
    );

    let host = tool_host(&services, &plain);
    let err = host
        .run_readonly_query("SELECT sqlly_ro_write()", 10)
        .await
        .unwrap_err();
    assert!(err.starts_with(READ_ONLY_REFUSAL_PREFIX), "{err}");

    assert_eq!(
        count(&services, &plain, "SELECT COUNT(*) FROM sqlly_ro_probe").await,
        1,
        "nothing was written"
    );
    run(
        &services,
        &plain,
        "DROP FUNCTION sqlly_ro_write; DROP TABLE sqlly_ro_probe;",
    )
    .await
    .expect("teardown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlserver_rollback_wrap_and_statement_screen_hold() {
    let Some(plain) = env_profile("SQLLY_SQL", DatabaseType::SqlServer, 1433) else {
        eprintln!("skipping: SQLLY_SQL_* env vars not set");
        return;
    };
    let services = Services::new();
    run(
        &services,
        &plain,
        "DROP TABLE IF EXISTS dbo.sqlly_ro_probe; \
         CREATE TABLE dbo.sqlly_ro_probe (id int); \
         INSERT INTO dbo.sqlly_ro_probe VALUES (1);",
    )
    .await
    .expect("setup");

    // No server-side read-only mode: a write that reaches the server runs
    // inside the wrap and is visible there, but the wrap always rolls back.
    let enforced = enforced_only(&plain);
    assert_eq!(
        count(
            &services,
            &enforced,
            "INSERT INTO dbo.sqlly_ro_probe VALUES (2); SELECT COUNT(*) FROM dbo.sqlly_ro_probe"
        )
        .await,
        2,
        "inside the transaction the write is visible"
    );
    assert_eq!(
        count(&services, &plain, "SELECT COUNT(*) FROM dbo.sqlly_ro_probe").await,
        1,
        "the rollback wrap undid it"
    );

    // Through the agent tool host: reads work, and the statement screen
    // refuses what a rollback could not undo, before anything is sent.
    let host = tool_host(&services, &plain);
    let ok = host
        .run_readonly_query("SELECT COUNT(*) AS n FROM dbo.sqlly_ro_probe", 10)
        .await
        .unwrap();
    assert!(ok.contains("1 rows shown"), "{ok}");
    for sql in [
        "SELECT * FROM OPENQUERY(linked, 'SELECT 1')",
        "SELECT * INTO dbo.sqlly_ro_copy FROM dbo.sqlly_ro_probe",
        "EXEC sp_executesql N'DELETE FROM dbo.sqlly_ro_probe'",
    ] {
        let err = host.run_readonly_query(sql, 10).await.unwrap_err();
        assert!(
            err.contains("read-only") || err.contains("refused"),
            "{sql}: {err}"
        );
    }
    assert_eq!(
        count(&services, &plain, "SELECT COUNT(*) FROM dbo.sqlly_ro_probe").await,
        1
    );
    run(&services, &plain, "DROP TABLE dbo.sqlly_ro_probe;")
        .await
        .expect("teardown");
}
