//! Live PostgreSQL Procedure Mode proof: the pure `pg_procedure` builders
//! deploy with `CREATE OR REPLACE`, invoke with `CALL` returning INOUT
//! values as a result row, and the rollback batch really leaves nothing
//! behind. Env-gated on `SQLLY_PG_*`; skips silently otherwise.

#![cfg(not(target_arch = "wasm32"))]

use sqlly_core::{CellValue, ConnectionProfile, DatabaseType, SecretProvider, SecretRef};
use sqlly_intel::query::pg_procedure;
use sqlly_intel::query::procedure::ParamArg;
use sqlly_services::query::{DefaultQueryExecutor, QueryEvent, QueryExecutor};

fn pg_profile() -> Option<ConnectionProfile> {
    let server = std::env::var("SQLLY_PG_SERVER").ok()?;
    let user = std::env::var("SQLLY_PG_USER").ok()?;
    let mut p = ConnectionProfile::sql_login(
        "pg-proc",
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

/// Run `sql`, collecting every row and panicking on server errors unless
/// `allow_error` is set (the rollback batch intentionally has no errors, but
/// cleanup is best-effort).
async fn run(
    executor: &DefaultQueryExecutor,
    profile: &ConnectionProfile,
    sql: &str,
) -> Vec<Vec<CellValue>> {
    let handle = executor.execute(profile, sql, 64).await.expect("execute");
    let mut events = handle.events;
    let mut rows = Vec::new();
    while let Some(event) = events.recv().await {
        match event {
            QueryEvent::Rows { rows: batch, .. } => rows.extend(batch),
            QueryEvent::Error { message } => panic!("query failed: {message}\nsql: {sql}"),
            QueryEvent::Completed { .. } | QueryEvent::Cancelled => break,
            _ => {}
        }
    }
    rows
}

/// PG's simple protocol returns every cell as text; typed cells appear when
/// drivers upgrade — accept both shapes.
fn is_seventeen(cell: &CellValue) -> bool {
    matches!(cell, CellValue::I64(17)) || matches!(cell, CellValue::String(s) if s == "17")
}

fn is_truthy(cell: &CellValue) -> bool {
    matches!(cell, CellValue::I64(1) | CellValue::Bool(true))
        || matches!(cell, CellValue::String(s) if s == "1" || s == "t" || s == "true")
}

const PROC_SCRIPT: &str = "CREATE OR REPLACE PROCEDURE public.sqlly_live_tally(IN uid integer, INOUT total integer)\nLANGUAGE plpgsql\nAS $$\nBEGIN\n  total := total + uid;\nEND\n$$;";

#[tokio::test]
async fn pg_procedure_deploy_invoke_and_rollback_live() {
    let Some(profile) = pg_profile() else {
        eprintln!("skipping: SQLLY_PG_* env vars not set");
        return;
    };
    let executor = DefaultQueryExecutor::new();
    let header = pg_procedure::parse_routine_header(PROC_SCRIPT).expect("header parses");
    assert_eq!(header.output_param_names(), vec!["total".to_string()]);

    // Make sure a previous run's artifact is gone before the rollback proof.
    run(
        &executor,
        &profile,
        "DROP PROCEDURE IF EXISTS public.sqlly_live_tally(integer, integer)",
    )
    .await;

    let args = vec![
        ParamArg {
            name: "uid".into(),
            value: Some("7".into()),
        },
        ParamArg {
            name: "total".into(),
            value: Some("10".into()),
        },
    ];
    let invoke = pg_procedure::build_invoke_sql(&header, &args).expect("invoke builds");
    let deploy = pg_procedure::rewrite_deploy(PROC_SCRIPT, &header);

    // 1) Rollback sandbox: deploy + invoke inside BEGIN…ROLLBACK. The INOUT
    //    value comes back as a result row, and nothing persists afterwards.
    let composite = pg_procedure::build_rollback_batch(&deploy, &invoke);
    let rows = run(&executor, &profile, &composite).await;
    assert!(
        rows.iter().any(|row| row.iter().any(is_seventeen)),
        "INOUT total 10 + 7 = 17 returned from CALL: {rows:?}"
    );
    let probe = run(
        &executor,
        &profile,
        &pg_procedure::object_probe_sql(&header),
    )
    .await;
    let exists = probe
        .first()
        .and_then(|row| row.first())
        .is_some_and(is_truthy);
    assert!(!exists, "rollback left the procedure deployed: {probe:?}");

    // 2) Real deploy: probe flips to existing, CALL works standalone.
    run(&executor, &profile, &deploy).await;
    let probe = run(
        &executor,
        &profile,
        &pg_procedure::object_probe_sql(&header),
    )
    .await;
    let exists = probe
        .first()
        .and_then(|row| row.first())
        .is_some_and(is_truthy);
    assert!(exists, "deploy did not persist: {probe:?}");
    let rows = run(&executor, &profile, &invoke).await;
    assert!(
        rows.iter().any(|row| row.iter().any(is_seventeen)),
        "standalone CALL returns the INOUT value: {rows:?}"
    );

    // Cleanup.
    run(
        &executor,
        &profile,
        "DROP PROCEDURE IF EXISTS public.sqlly_live_tally(integer, integer)",
    )
    .await;
}
