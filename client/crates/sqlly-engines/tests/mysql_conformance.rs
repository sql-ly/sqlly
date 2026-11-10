//! Driver conformance matrix for MySQL, mirroring
//! `sqlly-sqlite/tests/conformance.rs`. Gated on `SQLLY_MYSQL_*` env vars: each
//! test skips silently when no live server is configured, so the suite is
//! safe in CI and meaningful on a developer machine with Postgres running.

use sqlly_core::{CellValue, ConnectionProfile, DatabaseType, SecretProvider, SecretRef, SqlParam};
use sqlly_engines::mysql::{
    execute_on_client, execute_params_on_client, open_client, query_rows_on_client,
};
use sqlly_testkit::conformance::{all_param_variants, assert_stream_shape, run_streaming};

fn live_profile() -> Option<ConnectionProfile> {
    let server = std::env::var("SQLLY_MYSQL_SERVER").ok()?;
    let user = std::env::var("SQLLY_MYSQL_USER").ok()?;
    let mut p = ConnectionProfile::sql_login(
        "mysql-conformance",
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
    if let Ok(engine) = std::env::var("SQLLY_MYSQL_ENGINE")
        && engine.eq_ignore_ascii_case("tidb")
    {
        p.database_type = DatabaseType::TiDb;
    }
    p.encrypt = false;
    Some(p)
}

macro_rules! live_or_skip {
    () => {
        match live_profile() {
            Some(p) => p,
            None => {
                eprintln!("skipping: SQLLY_MYSQL_* env vars not set");
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
        let sets = query_rows_on_client(&client, "SELECT ? AS v", std::slice::from_ref(&param))
            .await
            .unwrap_or_else(|e| panic!("param {param:?} must bind: {e}"));
        let cell = &sets[0].rows[0][0];
        match &param {
            SqlParam::Null => assert_eq!(cell, &CellValue::Null),
            // MySQL has no bool type: parameters bind as 0/1 integers.
            SqlParam::Bool(v) => assert!(
                matches!(cell, CellValue::Bool(b) if b == v)
                    || matches!(cell, CellValue::I64(n) if *n == i64::from(*v)),
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

/// Server-side cancellation: `KILL QUERY` from a side connection stops a
/// `SLEEP` on the server promptly instead of letting it run to completion.
/// (An interrupted `SLEEP` returns 1 rather than erroring, so the assertion
/// is on promptness, not on an error result.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_handle_stops_a_running_statement() {
    let profile = live_or_skip!();
    let client = open_client(&profile).await.expect("open");
    let cancel = sqlly_engines::mysql::cancel_handle(&client, &profile)
        .await
        .expect("cancel handle");

    let started = std::time::Instant::now();
    let run = tokio::spawn(async move {
        run_streaming(|bus| {
            let client = &client;
            let profile = &profile;
            async move { execute_on_client(client, profile, "SELECT SLEEP(30)", &bus, None).await }
        })
        .await
    });

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    cancel.cancel().await.expect("cancel request sends");

    let (_result, _outcome) = tokio::time::timeout(std::time::Duration::from_secs(10), run)
        .await
        .expect("cancel must end the statement promptly")
        .expect("task join");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "must not wait out the full sleep"
    );
}

#[tokio::test]
async fn parameterized_dml_reports_affected_count() {
    let profile = live_or_skip!();
    let client = open_client(&profile).await.expect("open");
    execute_params_on_client(
        &client,
        "CREATE TEMPORARY TABLE sqlly_conformance (x BIGINT, y TEXT)",
        &[],
    )
    .await
    .expect("create temp table");
    let affected = execute_params_on_client(
        &client,
        "INSERT INTO sqlly_conformance (x, y) VALUES (?, ?)",
        &[SqlParam::I64(7), SqlParam::String("seven".into())],
    )
    .await
    .expect("insert");
    assert_eq!(affected, 1);
}

/// `USE other_db` retargets the MySQL session silently: the profile (and the
/// warm-pool key derived from it) still names the old database, so without a
/// post-run confirmation the app keeps showing one database while every later
/// statement runs against another. SQL Server confirms the same way; this is
/// the MySQL half of that parity.
#[tokio::test]
async fn use_statement_announces_the_new_database() {
    let profile = live_or_skip!();
    let client = open_client(&profile).await.expect("open");

    // A database that exists on every MySQL server and is never the profile's.
    let (result, outcome) =
        run_streaming(|bus| {
            let client = &client;
            let profile = &profile;
            async move {
                execute_on_client(client, profile, "USE information_schema;", &bus, None).await
            }
        })
        .await;
    let run = result.expect("USE runs");
    assert_eq!(
        outcome.database_changes,
        vec!["information_schema".to_string()],
        "a USE that moves the session must be announced, got {:?}",
        outcome.database_changes
    );
    // The client now sits on a database other than the one its pool key names;
    // returning it to the pool would hand the next query on the profile's
    // database a session that runs somewhere else.
    assert!(
        run.should_discard_client(),
        "a client moved by USE must not be re-pooled: {run:?}"
    );

    // A batch that never leaves the profile's database announces nothing, so the
    // confirmation cannot chatter on ordinary statements.
    let (result, outcome) = run_streaming(|bus| {
        let client = &client;
        let profile = &profile;
        async move { execute_on_client(client, profile, "SELECT 1 AS a;", &bus, None).await }
    })
    .await;
    let run = result.expect("plain select runs");
    assert!(
        !run.should_discard_client(),
        "an ordinary statement keeps the client poolable: {run:?}"
    );
    assert!(
        outcome.database_changes.is_empty(),
        "a statement with no USE must not announce a change, got {:?}",
        outcome.database_changes
    );
}

/// MySQL's text protocol hands every value over as bytes, so the column
/// metadata decides how each one is read back. `BINARY_FLAG` is not that
/// decision: MySQL sets it on everything that is not a character column, which
/// includes temporal columns and every computed expression. Reading it as
/// "this is binary" rendered `SELECT COUNT(*)` and any DATETIME column as hex.
///
/// `assert_stream_shape` cannot catch this — it checks column names and row
/// counts, never values — so this test reads the cells.
#[tokio::test]
async fn values_read_back_as_values_not_as_hex() {
    let profile = live_or_skip!();
    let client = open_client(&profile).await.expect("open");

    let (result, outcome) = run_streaming(|bus| {
        let client = &client;
        let profile = &profile;
        async move {
            execute_on_client(
                client,
                profile,
                "SELECT COUNT(*) AS counted, 42 AS literal, \
                 CAST('2026-09-23 14:43:48' AS DATETIME) AS moment, \
                 UNHEX('DEADBEEF') AS blob_value \
                 FROM (SELECT 1 UNION ALL SELECT 2) AS two_rows;",
                &bus,
                None,
            )
            .await
        }
    })
    .await;
    result.expect("batch runs");

    let row = &outcome.result_sets[0].1[0];

    // A computed count is a number the reader can read, not a byte dump.
    assert_eq!(row[0], CellValue::String("2".into()), "COUNT(*): {row:?}");
    assert_eq!(row[1], CellValue::String("42".into()), "literal: {row:?}");

    // Temporal columns carry the binary flag too and are still text.
    assert_eq!(
        row[2],
        CellValue::String("2026-09-23 14:43:48".into()),
        "DATETIME: {row:?}"
    );

    // A genuinely binary value must still arrive as bytes.
    assert_eq!(
        row[3],
        CellValue::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF]),
        "binary stays binary: {row:?}"
    );
}

/// The text protocol renders every value as text, so the column's declared
/// type is what tells the grid and the exports that a `"42"` is a number.
/// The driver must therefore report SQL spellings (`bigint`, `decimal`),
/// not the wire enum's `LONGLONG`/`NEWDECIMAL`.
#[tokio::test]
async fn text_protocol_columns_carry_sql_type_names() {
    let profile = live_or_skip!();
    let client = open_client(&profile).await.expect("open");

    let (result, outcome) = run_streaming(|bus| {
        let client = &client;
        let profile = &profile;
        async move {
            execute_on_client(
                client,
                profile,
                "SELECT COUNT(*) AS counted, CAST(42 AS SIGNED) AS i, \
                 CAST(12.50 AS DECIMAL(10,2)) AS d, 1.5e0 AS f, \
                 CAST(7 AS UNSIGNED) AS u, 'x' AS t, \
                 CAST('2026-09-23' AS DATE) AS day, UNHEX('FF') AS raw \
                 FROM (SELECT 1 UNION ALL SELECT 2) AS two_rows;",
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
    assert_eq!(
        types,
        [
            "bigint",
            "bigint",
            "decimal",
            "double",
            "bigint unsigned",
            "varchar",
            "date",
            "varbinary",
        ],
        "{columns:?}"
    );
    assert_eq!(rows[0][0], CellValue::String("2".into()));
    assert_eq!(rows[0][2], CellValue::String("12.50".into()));
    assert_eq!(rows[0][7], CellValue::Bytes(vec![0xFF]));
}

/// `BIT` columns arrive as raw bytes with the binary charset, like a blob.
/// Reading them as text turned `b'00000001'` into an invisible control
/// character instead of `0x01`.
#[tokio::test]
async fn bit_columns_stay_binary() {
    let profile = live_or_skip!();
    let client = open_client(&profile).await.expect("open");

    let (result, outcome) = run_streaming(|bus| {
        let client = &client;
        let profile = &profile;
        async move {
            execute_on_client(
                client,
                profile,
                "CREATE TEMPORARY TABLE sqlly_bits (b BIT(8)); \
                 INSERT INTO sqlly_bits VALUES (b'00000001'); \
                 SELECT b FROM sqlly_bits;",
                &bus,
                None,
            )
            .await
        }
    })
    .await;
    result.expect("batch runs");

    let (_, rows) = outcome.result_sets.last().expect("a result set");
    assert_eq!(rows[0][0], CellValue::Bytes(vec![0x01]), "BIT(8): {rows:?}");
}

/// In `USE other; SELECT * FROM missing` the USE has moved the session before
/// the SELECT fails. The failure must not skip the confirmation: the move is
/// still announced, and the client reports it so a caller that keeps clients
/// after errors (a pinned manual-transaction session) can drop it.
#[tokio::test]
async fn a_failure_after_use_still_confirms_the_database() {
    let profile = live_or_skip!();
    let client = open_client(&profile).await.expect("open");

    let (result, outcome) = run_streaming(|bus| {
        let client = &client;
        let profile = &profile;
        async move {
            execute_on_client(
                client,
                profile,
                "USE information_schema; SELECT * FROM sqlly_no_such_table;",
                &bus,
                None,
            )
            .await
        }
    })
    .await;
    assert!(result.is_err(), "the SELECT must fail");
    assert_eq!(
        outcome.database_changes,
        vec!["information_schema".to_string()],
        "the USE before the failure must still be announced"
    );
    assert!(
        client.left_profile_database(),
        "the client must report that it left the profile's database"
    );
}

/// MySQL database names are case-sensitive on Linux servers
/// (`lower_case_table_names = 0`), so a session on `sales` has left a profile
/// that names `SALES`. A case-insensitive compare missed that and re-pooled
/// the client under the wrong key.
#[tokio::test]
async fn a_database_differing_only_in_case_is_a_different_database() {
    let profile = live_or_skip!();
    let Some(actual) = profile.database.clone() else {
        eprintln!("skipping: SQLLY_MYSQL_DATABASE not set");
        return;
    };
    let client = open_client(&profile).await.expect("open");

    // The same client, described by a profile naming the case-flipped database.
    let mut shouting = profile.clone();
    let flipped = if actual.to_uppercase() != actual {
        actual.to_uppercase()
    } else {
        actual.to_lowercase()
    };
    assert_ne!(flipped, actual, "database name has no letters to flip");
    shouting.database = Some(flipped);

    let use_sql = format!("USE `{actual}`;");
    let (result, outcome) = run_streaming(|bus| {
        let client = &client;
        let shouting = &shouting;
        let use_sql = &use_sql;
        async move { execute_on_client(client, shouting, use_sql, &bus, None).await }
    })
    .await;
    let run = result.expect("USE runs");
    assert!(
        run.should_discard_client(),
        "case differs, so it moved: {run:?}"
    );
    assert_eq!(outcome.database_changes, vec![actual]);
}

/// `USE scratch; DROP DATABASE scratch` leaves the session with no database at
/// all. The profile still names one, so the client has left it and must not
/// go back to the pool, where the next query would fail with "No database
/// selected".
#[tokio::test]
async fn dropping_the_current_database_leaves_the_profile_database() {
    let profile = live_or_skip!();
    let client = open_client(&profile).await.expect("open");

    let (result, outcome) = run_streaming(|bus| {
        let client = &client;
        let profile = &profile;
        async move {
            execute_on_client(
                client,
                profile,
                "CREATE DATABASE IF NOT EXISTS sqlly_scratch_drop; \
                 USE sqlly_scratch_drop; \
                 DROP DATABASE sqlly_scratch_drop;",
                &bus,
                None,
            )
            .await
        }
    })
    .await;
    let run = result.expect("batch runs");
    assert!(
        run.should_discard_client(),
        "no database selected is not the profile's database: {run:?}"
    );
    // There is no name to announce.
    assert!(outcome.database_changes.is_empty());
}
