//! Live proof of MySQL warm pooling: sequential queries in the same session
//! lane must reuse the server connection (same `CONNECTION_ID()`), and a
//! different session lane must not share it mid-flight. Gated on
//! `SQLLY_MYSQL_*` env vars; skips silently when absent.

#![cfg(not(target_arch = "wasm32"))]

use sqlly_core::{CellValue, ConnectionProfile, DatabaseType, SecretProvider, SecretRef};
use sqlly_services::query::{DefaultQueryExecutor, QueryEvent, QueryExecutor};

fn mysql_profile() -> Option<ConnectionProfile> {
    let server = std::env::var("SQLLY_MYSQL_SERVER").ok()?;
    let user = std::env::var("SQLLY_MYSQL_USER").ok()?;
    let mut p = ConnectionProfile::sql_login(
        "mysql-pool-live",
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
    p.database = std::env::var("SQLLY_MYSQL_DATABASE").ok();
    p.database_type = DatabaseType::MySql;
    p.encrypt = false;
    Some(p)
}

async fn scalar(
    executor: &DefaultQueryExecutor,
    profile: &ConnectionProfile,
    session: Option<&str>,
    sql: &str,
) -> String {
    let handle = executor
        .execute_in_session(profile, session, sql, 100)
        .await
        .expect("execute");
    let mut events = handle.events;
    let mut value = None;
    while let Some(event) = events.recv().await {
        match event {
            QueryEvent::Rows { rows, .. } => {
                if let Some(cell) = rows.first().and_then(|r| r.first()) {
                    value = Some(match cell {
                        CellValue::String(s) => s.clone(),
                        CellValue::I64(n) => n.to_string(),
                        other => format!("{other:?}"),
                    });
                }
            }
            QueryEvent::Error { message } => panic!("query failed: {message}"),
            QueryEvent::Completed { .. } | QueryEvent::Cancelled => break,
            _ => {}
        }
    }
    value.expect("scalar result")
}

#[tokio::test]
async fn warm_pool_reuses_the_same_server_connection_per_session() {
    let Some(profile) = mysql_profile() else {
        eprintln!("skipping: SQLLY_MYSQL_* env vars not set");
        return;
    };
    let executor = DefaultQueryExecutor::new();

    let first = scalar(&executor, &profile, Some("tab-1"), "SELECT CONNECTION_ID()").await;
    let second = scalar(&executor, &profile, Some("tab-1"), "SELECT CONNECTION_ID()").await;
    assert_eq!(
        first, second,
        "sequential queries in one session lane must reuse the warm connection"
    );

    let other = scalar(&executor, &profile, Some("tab-2"), "SELECT CONNECTION_ID()").await;
    assert_ne!(
        first, other,
        "a different session lane gets its own connection"
    );
}
