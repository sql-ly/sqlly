//! Driver conformance matrix for SQL Server, mirroring
//! `sqlly-pg/tests/conformance.rs` / `sqlly-mysql/tests/conformance.rs` and
//! closing the gap where the shared-harness docs named `sqlly-tds` without a
//! conformance test existing. Gated the same way as the other live TDS tests:
//! a `.sqlly-local-sql.json` file or `SQLLY_SQL_*` env vars; each test skips
//! silently when no live server is configured.

use sqlly_core::{CellValue, ConnectionProfile, SqlParam};
use sqlly_engines::tds::{
    execute_on_client, execute_params_on_client, open_client, query_rows_on_client,
};
use sqlly_testkit::LiveSqlConfig;
use sqlly_testkit::conformance::{all_param_variants, assert_stream_shape, run_streaming};

fn live_profile() -> Option<ConnectionProfile> {
    let cfg = LiveSqlConfig::load()?;
    Some(cfg.to_profile("tds-conformance"))
}

macro_rules! live_or_skip {
    () => {
        match live_profile() {
            Some(p) => p,
            None => {
                eprintln!(
                    "skipping: no {} file and no SQLLY_SQL_* env vars",
                    sqlly_testkit::LOCAL_SQL_CONFIG_FILE
                );
                return;
            }
        }
    };
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connects_and_streams_multi_result_batch() {
    let profile = live_or_skip!();
    let mut client = open_client(&profile).await.expect("open");
    let (result, outcome) = run_streaming(|bus| {
        let client = &mut client;
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
            .map(|_| ())
        }
    })
    .await;
    result.expect("batch runs");
    assert_stream_shape(&outcome, &[(&["a"], 1), (&["b", "c"], 2)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn errors_surface_without_wedging_the_client() {
    let profile = live_or_skip!();
    let mut client = open_client(&profile).await.expect("open");
    let (result, outcome) = run_streaming(|bus| {
        let client = &mut client;
        let profile = &profile;
        async move {
            execute_on_client(client, profile, "SELECT * FROM missing_table", &bus, None)
                .await
                .map(|_| ())
        }
    })
    .await;
    // TDS batches run SSMS-style "continue after error": the run either
    // errors outright or reports the failed batch via the outcome — a silent
    // clean success would hide the failure.
    assert!(
        result.is_err() || !outcome.completed || !outcome.errors.is_empty(),
        "bad SQL must surface an error"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_param_variant_round_trips() {
    let profile = live_or_skip!();
    let mut client = open_client(&profile).await.expect("open");
    for param in all_param_variants() {
        // Cast so SQL Server infers a type for the bare placeholder.
        let sql = match &param {
            SqlParam::Null => "SELECT CAST(@P1 AS nvarchar(100)) AS v",
            SqlParam::Bool(_) => "SELECT CAST(@P1 AS bit) AS v",
            SqlParam::I64(_) => "SELECT CAST(@P1 AS bigint) AS v",
            SqlParam::F64(_) => "SELECT CAST(@P1 AS float) AS v",
            SqlParam::String(_) => "SELECT CAST(@P1 AS nvarchar(100)) AS v",
            SqlParam::Bytes(_) => "SELECT CAST(@P1 AS varbinary(100)) AS v",
        };
        let sets = query_rows_on_client(&mut client, sql, std::slice::from_ref(&param))
            .await
            .unwrap_or_else(|e| panic!("param {param:?} must bind: {e}"));
        let cell = &sets[0].rows[0][0];
        match &param {
            SqlParam::Null => assert_eq!(cell, &CellValue::Null),
            SqlParam::Bool(v) => {
                assert!(matches!(cell, CellValue::Bool(b) if b == v), "{cell:?}")
            }
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parameterized_dml_reports_affected_count() {
    let profile = live_or_skip!();
    let mut client = open_client(&profile).await.expect("open");
    // Behavioral difference the matrix exists to pin: parameterized batches
    // run through sp_executesql, and a local `#temp` created there is dropped
    // when that scope ends — unlike PG/MySQL session temp tables. A global
    // `##temp` survives to the next statement and still dies with the session.
    let table = format!("##sqlly_conformance_{}", std::process::id());
    execute_params_on_client(
        &mut client,
        &format!("CREATE TABLE {table} (x bigint, y nvarchar(100))"),
        &[],
    )
    .await
    .expect("create temp table");
    let affected = execute_params_on_client(
        &mut client,
        &format!("INSERT INTO {table} (x, y) VALUES (@P1, @P2)"),
        &[SqlParam::I64(7), SqlParam::String("seven".into())],
    )
    .await
    .expect("insert");
    assert_eq!(affected, 1);
    execute_params_on_client(&mut client, &format!("DROP TABLE {table}"), &[])
        .await
        .expect("drop temp table");
}

/// Run `sql` through the streaming path and return what it published.
async fn stream_batch(
    client: &mut sqlly_engines::tds::TdsClient,
    profile: &ConnectionProfile,
    sql: &str,
) -> (
    sqlly_core::StudioResult<()>,
    sqlly_testkit::conformance::StreamOutcome,
) {
    run_streaming(|bus| async move {
        execute_on_client(client, profile, sql, &bus, None)
            .await
            .map(|_| ())
    })
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn print_and_informational_raiserror_reach_messages_in_order() {
    let profile = live_or_skip!();
    let mut client = open_client(&profile).await.expect("open");
    let (result, outcome) = stream_batch(
        &mut client,
        &profile,
        "SET NOCOUNT OFF; PRINT 'a'; RAISERROR('b',10,1); SELECT 1 AS x; PRINT 'c'",
    )
    .await;
    eprintln!("outcome: {outcome:?}");
    result.expect("batch runs");
    assert!(outcome.completed, "informational messages are not errors");
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    // The result set is unchanged by the messages around it.
    assert_stream_shape(&outcome, &[(&["x"], 1)]);
    assert_eq!(outcome.result_sets[0].1, vec![vec![CellValue::I64(1)]]);
    // Server text as-is, in server order; SSMS's count line for the SELECT
    // sits between the messages that surround it.
    assert_eq!(outcome.messages, vec!["a", "b", "(1 row affected)", "c"]);

    // Severity 11 and above is still an error, exactly as before.
    let (result, outcome) = stream_batch(
        &mut client,
        &profile,
        "PRINT 'before'; RAISERROR('boom',16,1)",
    )
    .await;
    eprintln!("outcome: {outcome:?}");
    assert!(
        result.is_err() || !outcome.errors.is_empty(),
        "severity 16 must fail the run"
    );
    assert!(!outcome.completed);
    assert!(
        outcome
            .messages
            .iter()
            .any(|m| m.contains("Msg 50000") && m.contains("boom")),
        "{:?}",
        outcome.messages
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dml_reports_rows_affected_per_statement_and_respects_nocount() {
    let profile = live_or_skip!();
    let mut client = open_client(&profile).await.expect("open");
    // Session-local temp table only: safe on a shared server.
    let (result, outcome) = stream_batch(
        &mut client,
        &profile,
        "SET NOCOUNT OFF; CREATE TABLE #t(i int); INSERT #t VALUES (1),(2); PRINT 'mid'; \
         UPDATE #t SET i = i + 1; DELETE #t WHERE i = 3; SELECT * FROM #t; \
         SET NOCOUNT ON; INSERT #t VALUES (9); UPDATE #t SET i = i; SELECT i FROM #t ORDER BY i; \
         DROP TABLE #t",
    )
    .await;
    eprintln!("outcome: {outcome:?}");
    result.expect("batch runs");
    assert!(
        outcome.completed && outcome.errors.is_empty(),
        "{outcome:?}"
    );
    assert_stream_shape(&outcome, &[(&["i"], 1), (&["i"], 2)]);
    // INSERT 2, UPDATE 2, DELETE 1, SELECT 1 — then NOCOUNT ON silences the
    // INSERT/UPDATE counts; the final SELECT still reports its own 2 rows.
    assert_eq!(
        outcome.messages,
        vec![
            "(2 rows affected)",
            "mid",
            "(2 rows affected)",
            "(1 row affected)",
            "(1 row affected)",
        ]
    );
    assert_eq!(outcome.row_counts, vec![2, 2, 1, 1, 2]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn procedure_statements_report_counts_once() {
    let profile = live_or_skip!();
    let mut client = open_client(&profile).await.expect("open");
    // A session-local temp procedure and temp table: safe on a shared server.
    let (result, outcome) = stream_batch(
        &mut client,
        &profile,
        "SET NOCOUNT OFF\nGO\n\
         CREATE PROCEDURE #sqlly_counts AS BEGIN \
         CREATE TABLE #u(i int); INSERT #u VALUES (1),(2),(3); PRINT 'inside'; \
         UPDATE #u SET i = i; SELECT COUNT(*) AS n FROM #u; END\nGO\n\
         EXEC #sqlly_counts\nGO\n\
         DROP PROCEDURE #sqlly_counts",
    )
    .await;
    eprintln!("outcome: {outcome:?}");
    result.expect("batch runs");
    assert!(
        outcome.completed && outcome.errors.is_empty(),
        "{outcome:?}"
    );
    assert_stream_shape(&outcome, &[(&["n"], 1)]);
    // One line per counted statement inside the procedure, like SSMS; the
    // procedure's own completion is not counted a second time.
    assert_eq!(
        outcome.messages,
        vec![
            "(3 rows affected)",
            "inside",
            "(3 rows affected)",
            "(1 row affected)"
        ]
    );
    assert_eq!(outcome.row_counts, vec![3, 3, 1]);
}
