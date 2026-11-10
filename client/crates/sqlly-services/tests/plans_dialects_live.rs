//! Live end-to-end plan capture per dialect: run the dialect's capture SQL
//! through the real executor, assemble the returned document exactly as the
//! plan pane does, and parse it into the shared model. SQLite runs
//! hermetically; PostgreSQL and MySQL are env-gated and skip silently.

#![cfg(not(target_arch = "wasm32"))]

use sqlly_core::{CellValue, ConnectionProfile, DatabaseType, SecretProvider, SecretRef};
use sqlly_services::plans::{PlanDialect, explain_capture_sql, parse_captured_plan};
use sqlly_services::query::{DefaultQueryExecutor, QueryEvent, QueryExecutor};

/// Collect a plan document from the query events: for JSON dialects the
/// first column of every row joined by newlines; for SQLite the
/// `id|parent|detail` rendering of each 4-column row.
async fn capture_document(
    executor: &DefaultQueryExecutor,
    profile: &ConnectionProfile,
    dialect: PlanDialect,
    sql: &str,
) -> String {
    let capture = explain_capture_sql(dialect, sql);
    let handle = executor
        .execute(profile, &capture, 100)
        .await
        .expect("capture execute");
    let mut events = handle.events;
    let mut lines = Vec::new();
    while let Some(event) = events.recv().await {
        match event {
            QueryEvent::Rows { rows, .. } => {
                for row in rows {
                    match dialect {
                        PlanDialect::Sqlite => {
                            let cell = |i: usize| match row.get(i) {
                                Some(CellValue::I64(n)) => n.to_string(),
                                Some(CellValue::String(s)) => s.clone(),
                                other => format!("{other:?}"),
                            };
                            lines.push(format!("{}|{}|{}", cell(0), cell(1), cell(3)));
                        }
                        _ => {
                            if let Some(CellValue::String(s)) = row.first() {
                                lines.push(s.clone());
                            }
                        }
                    }
                }
            }
            QueryEvent::Error { message } => panic!("capture failed: {message}"),
            QueryEvent::Completed { .. } | QueryEvent::Cancelled => break,
            _ => {}
        }
    }
    lines.join("\n")
}

#[tokio::test]
async fn sqlite_plan_capture_end_to_end_hermetic() {
    let dir = std::env::temp_dir().join(format!("sqlly-plan-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("plan.db");
    let mut profile = ConnectionProfile::sql_login(
        "sqlite-plan",
        db.to_string_lossy().as_ref(),
        "",
        SecretRef {
            provider: SecretProvider::InMemory,
            key: String::new(),
        },
    );
    profile.database_type = DatabaseType::Sqlite;
    let executor = DefaultQueryExecutor::new();

    // Seed a table without an index on `name`, so the plan is a SCAN.
    let seed = executor
        .execute(
            &profile,
            "CREATE TABLE plan_demo (id INTEGER PRIMARY KEY, name TEXT)",
            10,
        )
        .await
        .unwrap();
    let mut events = seed.events;
    while let Some(event) = events.recv().await {
        if matches!(
            event,
            QueryEvent::Completed { .. } | QueryEvent::Error { .. }
        ) {
            break;
        }
    }

    let doc = capture_document(
        &executor,
        &profile,
        PlanDialect::Sqlite,
        "SELECT * FROM plan_demo WHERE name = 'x'",
    )
    .await;
    let plans = parse_captured_plan(PlanDialect::Sqlite, &doc).unwrap();
    assert_eq!(plans.len(), 1);
    assert!(!plans[0].nodes.is_empty(), "{doc}");
    let findings = sqlly_services::plans::findings_for_plan(&plans[0]);
    assert!(
        findings.iter().any(|f| f.summary.contains("plan_demo")),
        "SCAN finding expected: {doc}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn postgres_plan_capture_end_to_end_live() {
    let Some(profile) = pg_profile() else {
        eprintln!("skipping: SQLLY_PG_* env vars not set");
        return;
    };
    let executor = DefaultQueryExecutor::new();
    let doc = capture_document(
        &executor,
        &profile,
        PlanDialect::Postgres,
        "SELECT n.nspname FROM pg_catalog.pg_namespace n",
    )
    .await;
    let plans = parse_captured_plan(PlanDialect::Postgres, &doc).unwrap();
    assert_eq!(plans.len(), 1);
    assert!(!plans[0].nodes.is_empty(), "{doc}");
    assert!(plans[0].total_subtree_cost > 0.0, "{doc}");
    assert!(
        plans[0].nodes[0].actual_rows.is_some(),
        "ANALYZE captures actual rows: {doc}"
    );
}

#[tokio::test]
async fn mysql_plan_capture_end_to_end_live() {
    let Some(profile) = mysql_profile() else {
        eprintln!("skipping: SQLLY_MYSQL_* env vars not set");
        return;
    };
    let executor = DefaultQueryExecutor::new();
    let doc = capture_document(
        &executor,
        &profile,
        PlanDialect::MySql,
        "SELECT table_name FROM information_schema.tables",
    )
    .await;
    let plans = parse_captured_plan(PlanDialect::MySql, &doc).unwrap();
    assert_eq!(plans.len(), 1);
    assert!(plans[0].nodes.len() > 1, "{doc}");
    // MySQL 8 actual plans run the statement: runtime row counts arrive.
    assert!(
        plans[0].nodes.iter().any(|node| node.actual_rows.is_some()),
        "EXPLAIN ANALYZE captures actual rows: {doc}"
    );
}

#[tokio::test]
async fn mariadb_plan_capture_end_to_end_live() {
    let Some(profile) = mariadb_profile() else {
        eprintln!("skipping: SQLLY_MARIADB_* env vars not set");
        return;
    };
    let executor = DefaultQueryExecutor::new();
    let doc = capture_document(
        &executor,
        &profile,
        PlanDialect::MariaDb,
        "SELECT table_name FROM information_schema.tables",
    )
    .await;
    let plans = parse_captured_plan(PlanDialect::MariaDb, &doc).unwrap();
    assert_eq!(plans.len(), 1);
    assert!(!plans[0].nodes.is_empty(), "{doc}");
    // ANALYZE FORMAT=JSON runs the statement: r_rows land as actual rows.
    assert!(
        plans[0].nodes.iter().any(|node| node.actual_rows.is_some()),
        "ANALYZE captures actual rows: {doc}"
    );
}

fn pg_profile() -> Option<ConnectionProfile> {
    let server = std::env::var("SQLLY_PG_SERVER").ok()?;
    let user = std::env::var("SQLLY_PG_USER").ok()?;
    let mut p = ConnectionProfile::sql_login(
        "pg-plan",
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

fn mariadb_profile() -> Option<ConnectionProfile> {
    let server = std::env::var("SQLLY_MARIADB_SERVER").ok()?;
    let user = std::env::var("SQLLY_MARIADB_USER").ok()?;
    let mut p = ConnectionProfile::sql_login(
        "mariadb-plan",
        server,
        user,
        SecretRef {
            provider: SecretProvider::Environment,
            key: "SQLLY_MARIADB_PASSWORD".into(),
        },
    );
    p.port = std::env::var("SQLLY_MARIADB_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3306);
    p.database = std::env::var("SQLLY_MARIADB_DATABASE").ok();
    p.database_type = DatabaseType::MariaDb;
    p.encrypt = false;
    Some(p)
}

fn mysql_profile() -> Option<ConnectionProfile> {
    let server = std::env::var("SQLLY_MYSQL_SERVER").ok()?;
    let user = std::env::var("SQLLY_MYSQL_USER").ok()?;
    let mut p = ConnectionProfile::sql_login(
        "mysql-plan",
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
