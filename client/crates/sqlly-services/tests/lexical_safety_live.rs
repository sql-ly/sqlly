//! Live proof, through the real drivers, that the connection safety gates
//! judge SQL the way each server reads it (the per-engine security lexer):
//!
//! - quoting tricks that hid a statement from the old dialect-free tokenizer
//!   are refused under SELECT-only, and the hidden statement never runs;
//! - the rollback wrap really rolls back an unterminated last statement
//!   (`DELETE FROM t` + `ROLLBACK` used to parse as a table alias on MySQL and
//!   PostgreSQL, deleting for good);
//! - on MySQL a failed wrapped run cannot leave work for the next run's
//!   `START TRANSACTION` to commit.
//!
//! Gated on the usual `SQLLY_PG_*` / `SQLLY_MYSQL_*` / `SQLLY_SQL_*` and
//! `SQLLY_CLICKHOUSE_URL` env vars; every test skips without a live server.
//! Each test owns a `sqlly_lex_probe` table it recreates.

use sqlly_core::{CellValue, ConnectionProfile, DatabaseType, SecretProvider, SecretRef};
use sqlly_services::{QueryEvent, Services};

fn env_profile(prefix: &str, db: DatabaseType, default_port: u16) -> Option<ConnectionProfile> {
    let server = std::env::var(format!("{prefix}_SERVER")).ok()?;
    let user = std::env::var(format!("{prefix}_USER"))
        .or_else(|_| std::env::var(format!("{prefix}_USERNAME")))
        .ok()?;
    let mut p = ConnectionProfile::sql_login(
        "lexical-safety-live",
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

/// Run `sql` and collect every result set's rows (flattened); `Err` carries
/// the terminal error message.
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
            QueryEvent::Rows { rows: batch, .. } => rows.extend(batch),
            QueryEvent::Error { message } => error = Some(message),
            _ => {}
        }
    }
    match error {
        Some(message) => Err(message),
        None => Ok(rows),
    }
}

async fn count(services: &Services, profile: &ConnectionProfile) -> i64 {
    let rows = run(services, profile, "SELECT COUNT(*) FROM sqlly_lex_probe")
        .await
        .expect("count query");
    match rows.first().and_then(|r| r.first()) {
        Some(CellValue::I64(n)) => *n,
        Some(CellValue::String(s)) => s.parse().expect("numeric count"),
        Some(CellValue::Bytes(b)) => String::from_utf8_lossy(b).parse().expect("numeric count"),
        other => panic!("unexpected count cell {other:?}"),
    }
}

async fn recreate_probe(services: &Services, profile: &ConnectionProfile, create: &str) {
    let _ = run(services, profile, "DROP TABLE sqlly_lex_probe").await;
    run(services, profile, create).await.expect("create probe");
    run(services, profile, "INSERT INTO sqlly_lex_probe VALUES (1)")
        .await
        .expect("seed probe");
}

fn with(profile: &ConnectionProfile, f: impl FnOnce(&mut ConnectionProfile)) -> ConnectionProfile {
    let mut p = profile.clone();
    f(&mut p);
    p
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mysql_gates_read_sql_the_way_mysql_does() {
    let Some(plain) = env_profile("SQLLY_MYSQL", DatabaseType::MySql, 3306) else {
        eprintln!("skipping: SQLLY_MYSQL_* env vars not set");
        return;
    };
    let services = Services::new();
    recreate_probe(
        &services,
        &plain,
        "CREATE TABLE sqlly_lex_probe (a int) ENGINE=InnoDB",
    )
    .await;

    // SELECT-only: the backtick repro and a mixed-version executable comment
    // are refused, and the DELETE between them never runs.
    let select_only = with(&plain, |p| p.block_all_but_select = true);
    for sql in [
        "SELECT 1 AS `'`; DELETE FROM sqlly_lex_probe; SELECT 1 AS `'`",
        "SELECT 1 /*!10000 , \"*/\"*/ /*!99999 ' */ ; DELETE FROM sqlly_lex_probe; -- \"'",
        "SELECT 1 --1; DELETE FROM sqlly_lex_probe",
    ] {
        let err = run(&services, &select_only, sql).await.unwrap_err();
        assert!(err.contains("SELECT statements only"), "{sql}: {err}");
    }
    assert_eq!(count(&services, &plain).await, 1);

    // Rollback wrap: an unterminated DELETE is rolled back (not aliased).
    let wrapped = with(&plain, |p| p.auto_wrap_rollback = true);
    run(&services, &wrapped, "DELETE FROM sqlly_lex_probe")
        .await
        .expect("wrapped delete runs");
    assert_eq!(
        count(&services, &plain).await,
        1,
        "wrapped DELETE must roll back"
    );

    // A statement error mid-batch cannot leave the DELETE for a later
    // START TRANSACTION to commit.
    let err = run(
        &services,
        &wrapped,
        "DELETE FROM sqlly_lex_probe; SELECT * FROM sqlly_lex_missing_table",
    )
    .await
    .unwrap_err();
    assert!(err.contains("sqlly_lex_missing_table"), "{err}");
    run(&services, &wrapped, "SELECT 1")
        .await
        .expect("next wrapped run");
    assert_eq!(
        count(&services, &plain).await,
        1,
        "failed run must not commit later"
    );

    // Transaction control inside the wrap is refused before it runs.
    let err = run(&services, &wrapped, "ROLLBACK; DELETE FROM sqlly_lex_probe")
        .await
        .unwrap_err();
    assert!(err.contains("ROLLBACK"), "{err}");
    assert_eq!(count(&services, &plain).await, 1);
    let _ = run(&services, &plain, "DROP TABLE sqlly_lex_probe").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_gates_read_sql_the_way_postgres_does() {
    let Some(plain) = env_profile("SQLLY_PG", DatabaseType::Postgres, 5432) else {
        eprintln!("skipping: SQLLY_PG_* env vars not set");
        return;
    };
    let services = Services::new();
    recreate_probe(&services, &plain, "CREATE TABLE sqlly_lex_probe (a int)").await;

    let select_only = with(&plain, |p| p.block_all_but_select = true);
    for sql in [
        "SELECT 1 AS \"'\"; DELETE FROM sqlly_lex_probe; SELECT 1 AS \"'\"",
        "SELECT $$'$$; DELETE FROM sqlly_lex_probe; SELECT $$'$$",
        "SELECT 1; -- '\rDELETE FROM sqlly_lex_probe; -- '",
    ] {
        let err = run(&services, &select_only, sql).await.unwrap_err();
        assert!(err.contains("SELECT statements only"), "{sql:?}: {err}");
    }
    assert_eq!(count(&services, &plain).await, 1);

    let wrapped = with(&plain, |p| p.auto_wrap_rollback = true);
    run(&services, &wrapped, "DELETE FROM sqlly_lex_probe")
        .await
        .expect("wrapped delete runs");
    assert_eq!(
        count(&services, &plain).await,
        1,
        "wrapped DELETE must roll back"
    );
    let err = run(&services, &wrapped, "COMMIT; DELETE FROM sqlly_lex_probe")
        .await
        .unwrap_err();
    assert!(err.contains("COMMIT"), "{err}");
    assert_eq!(count(&services, &plain).await, 1);
    let _ = run(&services, &plain, "DROP TABLE sqlly_lex_probe").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sql_server_gates_read_sql_the_way_sql_server_does() {
    let Some(plain) = env_profile("SQLLY_SQL", DatabaseType::SqlServer, 1433) else {
        eprintln!("skipping: SQLLY_SQL_* env vars not set");
        return;
    };
    let services = Services::new();
    recreate_probe(&services, &plain, "CREATE TABLE sqlly_lex_probe (a int)").await;

    let select_only = with(&plain, |p| p.block_all_but_select = true);
    for sql in [
        "SELECT 1 AS \"'\"; DELETE FROM sqlly_lex_probe; SELECT 1 AS \"'\"",
        "SELECT 1 AS [']; DELETE FROM sqlly_lex_probe; SELECT 1 AS [']",
        "SELECT 'A' -- '\rDELETE FROM sqlly_lex_probe -- '",
        "SELECT COUNT(*) FROM sqlly_lex_probe\nDELETE FROM sqlly_lex_probe",
    ] {
        let err = run(&services, &select_only, sql).await.unwrap_err();
        assert!(err.contains("SELECT statements only"), "{sql:?}: {err}");
    }
    assert_eq!(count(&services, &plain).await, 1);

    let wrapped = with(&plain, |p| p.auto_wrap_rollback = true);
    let err = run(&services, &wrapped, "DELETE FROM sqlly_lex_probe COMMIT")
        .await
        .unwrap_err();
    assert!(err.contains("COMMIT"), "{err}");
    assert_eq!(count(&services, &plain).await, 1);
    let _ = run(&services, &plain, "DROP TABLE sqlly_lex_probe").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clickhouse_splits_and_gates_by_its_own_rules() {
    let Some(plain) = sqlly_engines::clickhouse::live_profile_from_env("lexical-safety-live")
    else {
        eprintln!("skipping: SQLLY_CLICKHOUSE_URL not set");
        return;
    };
    let services = Services::new();
    let _ = run(&services, &plain, "DROP TABLE IF EXISTS sqlly_lex_probe").await;
    run(
        &services,
        &plain,
        "CREATE TABLE sqlly_lex_probe (a Int32) ENGINE = MergeTree ORDER BY a",
    )
    .await
    .expect("create probe");
    run(&services, &plain, "INSERT INTO sqlly_lex_probe VALUES (1)")
        .await
        .expect("seed probe");

    let select_only = with(&plain, |p| p.block_all_but_select = true);
    for sql in [
        // Nested comments: the flat reading sees the statement.
        "SELECT 1 /* /* */ ; TRUNCATE TABLE sqlly_lex_probe; -- */",
        "SELECT 1 AS `'`; TRUNCATE TABLE sqlly_lex_probe; SELECT 1 AS `'`",
    ] {
        let err = run(&services, &select_only, sql).await.unwrap_err();
        assert!(err.contains("SELECT statements only"), "{sql}: {err}");
    }
    // A backslash-escaped backtick keeps the `;` inside the name: one
    // statement, so nothing after it runs.
    run(
        &services,
        &select_only,
        "SELECT 1 AS `a\\`; TRUNCATE TABLE sqlly_lex_probe; --`",
    )
    .await
    .expect("one SELECT");
    assert_eq!(count(&services, &plain).await, 1);
    let _ = run(&services, &plain, "DROP TABLE sqlly_lex_probe").await;
}

fn oracle_profile() -> Option<ConnectionProfile> {
    let server = std::env::var("SQLLY_ORACLE_SERVER").ok()?;
    let user = std::env::var("SQLLY_ORACLE_USER").ok()?;
    let mut p = ConnectionProfile::sql_login(
        "lexical-safety-live",
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oracle_gates_read_sql_the_way_oracle_does() {
    let Some(plain) = oracle_profile() else {
        eprintln!("skipping: SQLLY_ORACLE_* env vars not set");
        return;
    };
    let services = Services::new();
    recreate_probe(&services, &plain, "CREATE TABLE sqlly_lex_probe (a NUMBER)").await;

    let select_only = with(&plain, |p| p.block_all_but_select = true);
    let err = run(
        &services,
        &select_only,
        "SELECT q'[']' FROM dual; DELETE FROM sqlly_lex_probe; SELECT 'x' FROM dual",
    )
    .await
    .unwrap_err();
    assert!(err.contains("SELECT statements only"), "{err}");
    assert_eq!(count(&services, &plain).await, 1);

    // A trailing line comment used to swallow the `;` the wrap appends,
    // gluing ROLLBACK onto the DELETE as a table alias.
    let wrapped = with(&plain, |p| p.auto_wrap_rollback = true);
    run(&services, &wrapped, "DELETE FROM sqlly_lex_probe -- note")
        .await
        .expect("wrapped delete runs");
    assert_eq!(
        count(&services, &plain).await,
        1,
        "wrapped DELETE must roll back"
    );
    let err = run(
        &services,
        &wrapped,
        "BEGIN DELETE FROM sqlly_lex_probe; COMMIT; END;",
    )
    .await
    .unwrap_err();
    assert!(err.contains("COMMIT"), "{err}");
    assert_eq!(count(&services, &plain).await, 1);
    let _ = run(&services, &plain, "DROP TABLE sqlly_lex_probe").await;
}
