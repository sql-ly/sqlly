//! Driver conformance matrix for SQLite (the always-runnable backend). The
//! same shapes run env-gated against Postgres/MySQL in their crates'
//! `tests/conformance.rs`, all through `sqlly_testkit::conformance`.

use sqlly_core::{CellValue, ConnectionProfile, SecretProvider, SecretRef, SqlParam};
use sqlly_engines::sqlite::{
    execute_on_client, execute_params_on_client, open_client, query_rows_on_client,
};
use sqlly_testkit::conformance::{all_param_variants, assert_stream_shape, run_streaming};

fn memory_profile() -> ConnectionProfile {
    let mut p = ConnectionProfile::sql_login(
        "sqlite-conformance",
        ":memory:",
        "",
        SecretRef {
            provider: SecretProvider::InMemory,
            key: String::new(),
        },
    );
    p.database_type = sqlly_core::DatabaseType::Sqlite;
    p
}

#[tokio::test]
async fn connects_and_streams_multi_result_batch() {
    let profile = memory_profile();
    let client = open_client(&profile).await.expect("open");
    let (result, outcome) = run_streaming(|bus| {
        let client = &client;
        let profile = &profile;
        async move {
            execute_on_client(
                client,
                profile,
                "SELECT 1 AS a; SELECT 2 AS b, 3 AS c UNION ALL SELECT 4, 5;",
                &bus,
                None,
            )
            .await
        }
    })
    .await;
    result.expect("batch runs");
    assert_stream_shape(&outcome, &[(&["a"], 1), (&["b", "c"], 2)]);
}

#[tokio::test]
async fn reports_affected_rows_for_dml() {
    let profile = memory_profile();
    let client = open_client(&profile).await.expect("open");
    let (result, outcome) = run_streaming(|bus| {
        let client = &client;
        let profile = &profile;
        async move {
            execute_on_client(
                client,
                profile,
                "CREATE TABLE t (x INTEGER); INSERT INTO t VALUES (1); INSERT INTO t VALUES (2), (3);",
                &bus,
                None,
            )
            .await
        }
    })
    .await;
    result.expect("dml runs");
    assert!(outcome.completed);
    assert!(
        outcome.row_counts.contains(&1) && outcome.row_counts.contains(&2),
        "affected counts surface: {outcome:?}"
    );
}

#[tokio::test]
async fn errors_surface_without_wedging_the_client() {
    let profile = memory_profile();
    let client = open_client(&profile).await.expect("open");
    let (result, outcome) = run_streaming(|bus| {
        let client = &client;
        let profile = &profile;
        async move {
            execute_on_client(client, profile, "SELECT * FROM missing_table", &bus, None).await
        }
    })
    .await;
    assert!(result.is_err(), "bad SQL must error");
    assert!(!outcome.completed, "no completion after failure");

    // The same client keeps working afterwards.
    let sets = query_rows_on_client(&client, "SELECT 42", &[])
        .await
        .expect("client survives an error");
    assert_eq!(sets[0].rows[0][0], CellValue::I64(42));
}

#[tokio::test]
async fn every_param_variant_round_trips() {
    let profile = memory_profile();
    let client = open_client(&profile).await.expect("open");
    for param in all_param_variants() {
        let sets = query_rows_on_client(&client, "SELECT ? AS v", std::slice::from_ref(&param))
            .await
            .unwrap_or_else(|e| panic!("param {param:?} must bind: {e}"));
        let cell = &sets[0].rows[0][0];
        match &param {
            SqlParam::Null => assert_eq!(cell, &CellValue::Null),
            // SQLite has no native bool: it round-trips as 0/1.
            SqlParam::Bool(true) => assert!(
                matches!(cell, CellValue::I64(1) | CellValue::Bool(true)),
                "{cell:?}"
            ),
            SqlParam::Bool(false) => assert!(
                matches!(cell, CellValue::I64(0) | CellValue::Bool(false)),
                "{cell:?}"
            ),
            SqlParam::I64(v) => assert_eq!(cell, &CellValue::I64(*v)),
            SqlParam::F64(v) => assert!(matches!(cell, CellValue::F64(f) if f == v), "{cell:?}"),
            SqlParam::String(v) => {
                assert!(matches!(cell, CellValue::String(s) if s == v), "{cell:?}")
            }
            SqlParam::Bytes(v) => {
                assert!(matches!(cell, CellValue::Bytes(b) if b == v), "{cell:?}")
            }
        }
    }
}

#[tokio::test]
async fn parameterized_dml_reports_affected_count() {
    let profile = memory_profile();
    let client = open_client(&profile).await.expect("open");
    execute_params_on_client(&client, "CREATE TABLE t (x INTEGER, y TEXT)", &[])
        .await
        .expect("create");
    let affected = execute_params_on_client(
        &client,
        "INSERT INTO t (x, y) VALUES (?, ?)",
        &[SqlParam::I64(7), SqlParam::String("seven".into())],
    )
    .await
    .expect("insert");
    assert_eq!(affected, 1);
    let sets = query_rows_on_client(&client, "SELECT x, y FROM t", &[])
        .await
        .expect("read back");
    assert_eq!(sets[0].rows.len(), 1);
}

/// Server-side cancellation: interrupting the connection stops an
/// effectively-infinite statement mid-execution and surfaces it as an error,
/// promptly — the query does not keep burning CPU until completion.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_stops_a_running_statement() {
    let profile = memory_profile();
    let client = open_client(&profile).await.expect("open");
    let interrupt = sqlly_engines::sqlite::interrupt_handle(&client);

    let run = tokio::spawn(async move {
        run_streaming(|bus| {
            let client = &client;
            let profile = &profile;
            async move {
                execute_on_client(
                    client,
                    profile,
                    // Infinite aggregate: yields no rows until it finishes,
                    // so only an interrupt can end it.
                    "WITH RECURSIVE c(x) AS (VALUES(1) UNION ALL SELECT x + 1 FROM c) \
                     SELECT count(*) FROM c",
                    &bus,
                    None,
                )
                .await
            }
        })
        .await
    });

    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    interrupt.interrupt();

    let (result, outcome) = tokio::time::timeout(std::time::Duration::from_secs(5), run)
        .await
        .expect("interrupt must end the statement promptly")
        .expect("task join");
    assert!(
        result.is_err(),
        "interrupted statement surfaces as an error"
    );
    assert!(!outcome.completed);
}
