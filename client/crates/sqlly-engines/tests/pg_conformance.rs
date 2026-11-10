//! Driver conformance matrix for PostgreSQL, mirroring
//! `sqlly-sqlite/tests/conformance.rs`. Gated on `SQLLY_PG_*` env vars: each
//! test skips silently when no live server is configured, so the suite is
//! safe in CI and meaningful on a developer machine with Postgres running.

use sqlly_core::{CellValue, ConnectionProfile, DatabaseType, SecretProvider, SecretRef, SqlParam};
use sqlly_engines::pg::{
    execute_on_client, execute_params_on_client, open_client, query_rows_on_client,
};
use sqlly_testkit::conformance::{all_param_variants, assert_stream_shape, run_streaming};

fn live_profile() -> Option<ConnectionProfile> {
    let server = std::env::var("SQLLY_PG_SERVER").ok()?;
    let user = std::env::var("SQLLY_PG_USER").ok()?;
    let mut p = ConnectionProfile::sql_login(
        "pg-conformance",
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
    p.database = std::env::var("SQLLY_PG_DATABASE").ok();
    p.database_type = DatabaseType::Postgres;
    if let Ok(engine) = std::env::var("SQLLY_PG_ENGINE")
        && engine.eq_ignore_ascii_case("cockroachdb")
    {
        p.database_type = DatabaseType::CockroachDb;
    }
    p.encrypt = false;
    Some(p)
}

macro_rules! live_or_skip {
    () => {
        match live_profile() {
            Some(p) => p,
            None => {
                eprintln!("skipping: SQLLY_PG_* env vars not set");
                return;
            }
        }
    };
}

#[tokio::test]
async fn connects_and_streams_multi_result_batch() {
    let profile = live_or_skip!();
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
async fn simple_query_reports_declared_types_and_typed_scalars() {
    // The simple-query protocol only names columns; the driver prepares the
    // batch first so the grid and the exports see `int4`/`numeric`/`bool`
    // and typed integer, float and boolean cells. Exact decimals stay text.
    let profile = live_or_skip!();
    let client = open_client(&profile).await.expect("open");
    let (result, outcome) = run_streaming(|bus| {
        let client = &client;
        let profile = &profile;
        async move {
            execute_on_client(
                client,
                profile,
                "SELECT 42::int AS i, 12.50::numeric(10,2) AS n, 1.5::float8 AS f, \
                 true AS b, 'x'::text AS t, 9000000000::bigint AS big",
                &bus,
                None,
            )
            .await
        }
    })
    .await;
    result.expect("select runs");
    let (columns, rows) = &outcome.result_sets[0];
    let types: Vec<&str> = columns.iter().map(|c| c.data_type.as_str()).collect();
    assert_eq!(types, ["int4", "numeric", "float8", "bool", "text", "int8"]);
    assert_eq!(
        rows[0],
        vec![
            CellValue::I64(42),
            CellValue::String("12.50".into()),
            CellValue::F64(1.5),
            CellValue::Bool(true),
            CellValue::String("x".into()),
            CellValue::I64(9_000_000_000),
        ]
    );

    // A multi-statement batch cannot be prepared: it still streams, with
    // the text-only shape.
    let (result, outcome) = run_streaming(|bus| {
        let client = &client;
        let profile = &profile;
        async move {
            execute_on_client(client, profile, "SELECT 1 AS a; SELECT 2 AS b;", &bus, None).await
        }
    })
    .await;
    result.expect("batch runs");
    assert_stream_shape(&outcome, &[(&["a"], 1), (&["b"], 1)]);
    assert_eq!(outcome.result_sets[0].0[0].data_type, "text");
    assert_eq!(
        outcome.result_sets[0].1[0][0],
        CellValue::String("1".into())
    );
}

#[tokio::test]
async fn errors_surface_without_wedging_the_client() {
    let profile = live_or_skip!();
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
}

#[tokio::test]
async fn every_param_variant_round_trips() {
    let profile = live_or_skip!();
    let client = open_client(&profile).await.expect("open");
    for param in all_param_variants() {
        // Cast so Postgres can infer a type for the bare placeholder.
        let sql = match &param {
            SqlParam::Null => "SELECT $1::text AS v",
            SqlParam::Bool(_) => "SELECT $1::bool AS v",
            SqlParam::I64(_) => "SELECT $1::bigint AS v",
            SqlParam::F64(_) => "SELECT $1::float8 AS v",
            SqlParam::String(_) => "SELECT $1::text AS v",
            SqlParam::Bytes(_) => "SELECT $1::bytea AS v",
        };
        let sets = query_rows_on_client(&client, sql, std::slice::from_ref(&param))
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

#[tokio::test]
async fn parameterized_dml_reports_affected_count() {
    let profile = live_or_skip!();
    let client = open_client(&profile).await.expect("open");
    execute_params_on_client(
        &client,
        "CREATE TEMP TABLE sqlly_conformance (x bigint, y text)",
        &[],
    )
    .await
    .expect("create temp table");
    let affected = execute_params_on_client(
        &client,
        "INSERT INTO sqlly_conformance (x, y) VALUES ($1, $2)",
        &[SqlParam::I64(7), SqlParam::String("seven".into())],
    )
    .await
    .expect("insert");
    assert_eq!(affected, 1);
}

/// Server-side cancellation: the cancel handle stops a `pg_sleep` on the
/// server promptly instead of letting it run to completion.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_handle_stops_a_running_statement() {
    let profile = live_or_skip!();
    let client = open_client(&profile).await.expect("open");
    let cancel = sqlly_engines::pg::cancel_handle(&client);

    let started = std::time::Instant::now();
    let run = tokio::spawn(async move {
        run_streaming(|bus| {
            let client = &client;
            let profile = &profile;
            async move {
                execute_on_client(client, profile, "SELECT pg_sleep(30)", &bus, None).await
            }
        })
        .await
    });

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    cancel.cancel().await.expect("cancel request sends");

    let (result, _outcome) = tokio::time::timeout(std::time::Duration::from_secs(10), run)
        .await
        .expect("cancel must end the statement promptly")
        .expect("task join");
    assert!(result.is_err(), "cancelled statement surfaces as an error");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "must not wait out the full sleep"
    );
}

#[tokio::test]
async fn raise_notice_reaches_messages_in_order() {
    // RAISE NOTICE / INFO / WARNING arrive as asynchronous NoticeResponse
    // messages on the connection, not in the query stream; they must still
    // land in the Messages pane, in order, before the run completes.
    let profile = live_or_skip!();
    let client = open_client(&profile).await.expect("open");
    let (result, outcome) = run_streaming(|bus| {
        let client = &client;
        let profile = &profile;
        async move {
            execute_on_client(
                client,
                profile,
                "DO $$ BEGIN RAISE NOTICE 'hello %', 1; END $$; \
                 SELECT 1 AS a; \
                 DO $$ BEGIN RAISE WARNING 'careful'; RAISE INFO 'fyi'; END $$;",
                &bus,
                None,
            )
            .await
        }
    })
    .await;
    result.expect("batch runs");
    assert_stream_shape(&outcome, &[(&["a"], 1)]);
    assert_eq!(
        outcome.messages,
        vec![
            "NOTICE: hello 1".to_string(),
            "WARNING: careful".to_string(),
            "INFO: fyi".to_string(),
        ],
        "notices in order: {outcome:?}"
    );

    // A notice raised by a later run never leaks into the earlier one, and a
    // notice-free run reports none.
    let (result, outcome) = run_streaming(|bus| {
        let client = &client;
        let profile = &profile;
        async move { execute_on_client(client, profile, "SELECT 2 AS b", &bus, None).await }
    })
    .await;
    result.expect("second batch runs");
    assert!(outcome.messages.is_empty(), "no stale notices: {outcome:?}");

    // Notices raised before a failure still reach the Messages pane.
    let (result, outcome) = run_streaming(|bus| {
        let client = &client;
        let profile = &profile;
        async move {
            execute_on_client(
                client,
                profile,
                "DO $$ BEGIN RAISE NOTICE 'before'; RAISE EXCEPTION 'boom'; END $$;",
                &bus,
                None,
            )
            .await
        }
    })
    .await;
    assert!(result.is_err(), "the exception fails the run");
    assert_eq!(outcome.messages, vec!["NOTICE: before".to_string()]);
}
