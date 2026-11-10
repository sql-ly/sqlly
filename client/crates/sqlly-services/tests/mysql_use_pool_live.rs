//! Live proof that a MySQL connection moved by `USE` never goes back into the
//! warm pool under its old key. Without the discard, the next query on the
//! profile's database checks out that connection and runs in whatever database
//! the `USE` left it on — silently.
//!
//! Gated on the usual `SQLLY_MYSQL_*` env vars (`SQLLY_MYSQL_DATABASE`
//! required); skips silently without a live server.

use sqlly_core::{CellValue, ConnectionProfile, DatabaseType, SecretProvider, SecretRef};
use sqlly_services::query::ManualTxState;
use sqlly_services::{DefaultQueryExecutor, QueryEvent, QueryExecutor};

fn env_profile() -> Option<ConnectionProfile> {
    let server = std::env::var("SQLLY_MYSQL_SERVER").ok()?;
    let user = std::env::var("SQLLY_MYSQL_USER").ok()?;
    let mut p = ConnectionProfile::sql_login(
        "use-pool-live",
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
    p.database = Some(std::env::var("SQLLY_MYSQL_DATABASE").ok()?);
    p.database_type = DatabaseType::MySql;
    p.encrypt = false;
    Some(p)
}

/// Run `sql` to completion and return the first cell of the first row, if any.
async fn first_cell(
    executor: &DefaultQueryExecutor,
    profile: &ConnectionProfile,
    sql: &str,
) -> Option<CellValue> {
    let mut handle = executor.execute(profile, sql, 32).await.expect("execute");
    let mut first = None;
    while let Some(event) = handle.events.recv().await {
        match event {
            QueryEvent::Rows { rows, .. } if first.is_none() => {
                first = rows.into_iter().next().and_then(|r| r.into_iter().next());
            }
            QueryEvent::Error { message } => panic!("{sql}: {message}"),
            _ => {}
        }
    }
    first
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_moved_by_use_is_not_reused_for_the_profile_database() {
    let Some(profile) = env_profile() else {
        eprintln!("skipping: SQLLY_MYSQL_* env vars not set");
        return;
    };
    let home = profile.database.clone().expect("database");
    let executor = DefaultQueryExecutor::new();

    // Warm the pool on the profile's database, then move that client away.
    first_cell(&executor, &profile, "SELECT 1").await;
    first_cell(&executor, &profile, "USE information_schema").await;

    // Same profile, same pool key: this must run where the profile says.
    let current = first_cell(&executor, &profile, "SELECT DATABASE()").await;
    assert_eq!(
        current,
        Some(CellValue::String(home.clone())),
        "the next query on `{home}` ran on a client a USE had moved"
    );
}

/// A manual-transaction tab keeps its client after a failed statement so the
/// open transaction survives. But in `USE information_schema; SELECT * FROM
/// missing` the USE has moved the session before the SELECT fails; keeping
/// that client would run the tab's later statements in `information_schema`
/// while the tab still names the profile's database. The session is lost
/// instead.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_manual_transaction_moved_by_use_before_an_error_is_not_kept() {
    let Some(profile) = env_profile() else {
        eprintln!("skipping: SQLLY_MYSQL_* env vars not set");
        return;
    };
    let executor = DefaultQueryExecutor::new();
    let tab = "use-then-error-tab";
    executor
        .begin_manual_transaction(&profile, tab)
        .await
        .expect("begin");

    let mut handle = executor
        .execute_in_session(
            &profile,
            Some(tab),
            "USE information_schema; SELECT * FROM sqlly_no_such_table;",
            32,
        )
        .await
        .expect("execute");
    let mut failed = false;
    while let Some(event) = handle.events.recv().await {
        failed |= matches!(event, QueryEvent::Error { .. });
    }
    assert!(failed, "the SELECT must fail");

    assert_eq!(
        executor.manual_transaction_state(&profile, tab),
        Some(ManualTxState::Lost),
        "a session a USE moved must not be kept for the next statement"
    );
}
