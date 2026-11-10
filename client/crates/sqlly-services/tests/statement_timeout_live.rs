//! Live per-engine proof of the per-connection statement timeout: a
//! deliberately slow statement (server-side sleep) must be stopped by the
//! engine's native cancel mechanism and reported as a timeout, promptly.
//! Gated on the usual `SQLLY_PG_*` / `SQLLY_MYSQL_*` / `SQLLY_SQL_*` env
//! vars — each test skips silently without a live server, so the suite is
//! safe in CI and meaningful on a developer machine with containers running.
//! (The hermetic SQLite counterpart lives in `sqlly-services`' unit tests:
//! `statement_timeout_cancels_and_reports_timeout`.)

use sqlly_core::{ConnectionProfile, DatabaseType, SecretProvider, SecretRef};
use sqlly_services::{DefaultQueryExecutor, QueryEvent, QueryExecutor};

fn env_profile(prefix: &str, db: DatabaseType, default_port: u16) -> Option<ConnectionProfile> {
    let server = std::env::var(format!("{prefix}_SERVER")).ok()?;
    let user = std::env::var(format!("{prefix}_USER"))
        .or_else(|_| std::env::var(format!("{prefix}_USERNAME")))
        .ok()?;
    let mut p = ConnectionProfile::sql_login(
        "timeout-live",
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
    p.statement_timeout_seconds = Some(1);
    Some(p)
}

/// Run `slow_sql` under a 1-second statement timeout and assert the stream
/// terminates promptly with an error that reads as a timeout.
async fn assert_times_out(profile: ConnectionProfile, slow_sql: &str) {
    let executor = DefaultQueryExecutor::new();
    let started = std::time::Instant::now();
    let mut handle = executor
        .execute(&profile, slow_sql, 32)
        .await
        .expect("execute");
    let mut error_message = None;
    while let Some(event) = handle.events.recv().await {
        if let QueryEvent::Error { message } = event {
            error_message = Some(message);
        }
    }
    let elapsed = started.elapsed();
    let message = error_message.expect("the slow statement must fail, not complete");
    assert!(
        message.to_ascii_lowercase().contains("timeout"),
        "expected a timeout error, got: {message}"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(15),
        "the stream must settle promptly after the 1s ceiling, took {elapsed:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_statement_timeout_stops_a_server_side_sleep() {
    let Some(profile) = env_profile("SQLLY_PG", DatabaseType::Postgres, 5432) else {
        eprintln!("skipping: SQLLY_PG_* env vars not set");
        return;
    };
    assert_times_out(profile, "SELECT pg_sleep(30)").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mysql_statement_timeout_stops_a_server_side_sleep() {
    let Some(profile) = env_profile("SQLLY_MYSQL", DatabaseType::MySql, 3306) else {
        eprintln!("skipping: SQLLY_MYSQL_* env vars not set");
        return;
    };
    assert_times_out(profile, "SELECT SLEEP(30)").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlserver_statement_timeout_stops_a_waitfor_delay() {
    let Some(profile) = env_profile("SQLLY_SQL", DatabaseType::SqlServer, 1433) else {
        eprintln!("skipping: SQLLY_SQL_* env vars not set");
        return;
    };
    assert_times_out(profile, "WAITFOR DELAY '00:00:30'; SELECT 1").await;
}
