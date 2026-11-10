//! Live integration fixture for system-schema metadata: verifies that the
//! queries `sqlly-metadata` generates for `sys` / `INFORMATION_SCHEMA` really
//! do enumerate objects, list columns, and count those schemas as non-empty on
//! an actual SQL Server — the behaviors the unit tests can only assert as SQL
//! text.
//!
//! Connects to the server described by `.sqlly-local-sql.json` (local dev) or
//! `SQLLY_SQL_*` env vars (CI); with neither present the test self-skips so
//! the default `cargo test` run stays hermetic. See `sqlserver_live.rs` for
//! the config contract.

use sqlly_core::{CellValue, EventBus};
use sqlly_engines::tds::{SqlServerTransport, TiberiusTransport};
use sqlly_intel::metadata::{MetadataQuery, sql_for};
use sqlly_testkit::LiveSqlConfig;

/// Run `sql` against the live server and return every row of the first result
/// set.
async fn fetch_rows(profile: &sqlly_core::ConnectionProfile, sql: &str) -> Vec<Vec<CellValue>> {
    let events = EventBus::default();
    // The lifecycle bus is a broadcast channel: with zero subscribers a send
    // fails and the transport surfaces it as an error, so keep one alive for
    // the duration of the query even though this test never reads it.
    let _subscription = events.subscribe();
    let (rows_tx, mut rows_rx) = sqlly_core::row_channel(64);
    let drain = tokio::spawn(async move {
        let mut rows = Vec::new();
        while let Some(batch) = rows_rx.recv().await {
            rows.extend(batch.rows);
        }
        rows
    });
    TiberiusTransport
        .execute_query(profile, sql, &events, Some(&rows_tx))
        .await
        .unwrap_or_else(|e| panic!("query should succeed: {e:?}\nsql: {sql}"));
    drop(rows_tx);
    drain.await.expect("row drain joins")
}

fn cell_text(cell: &CellValue) -> String {
    match cell {
        CellValue::String(s) => s.clone(),
        other => format!("{other:?}"),
    }
}

fn cell_i64(cell: &CellValue) -> i64 {
    match cell {
        CellValue::I64(n) => *n,
        CellValue::String(s) => s.parse().unwrap_or(-1),
        other => panic!("expected a number, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn system_schemas_enumerate_and_count_on_a_real_server() {
    let Some(cfg) = LiveSqlConfig::load() else {
        eprintln!(
            "skipping live system-schema test: no {} file and no SQLLY_SQL_* env vars",
            sqlly_testkit::LOCAL_SQL_CONFIG_FILE
        );
        return;
    };
    let profile = cfg.to_profile("system-schemas-live");

    // 1) `sys` and `INFORMATION_SCHEMA` views enumerate (the folders the
    //    explorer expands). Every SQL Server ships sys.objects and
    //    INFORMATION_SCHEMA.TABLES, so both listings must be non-empty.
    for schema in ["sys", "INFORMATION_SCHEMA"] {
        let sql = sql_for(&MetadataQuery::Views {
            schema: Some(schema.to_string()),
        });
        let rows = fetch_rows(&profile, &sql).await;
        assert!(
            !rows.is_empty(),
            "{schema} views should enumerate from sys.all_objects"
        );
    }

    // 2) Columns of a system view populate (sys.all_columns widening).
    let sql = sql_for(&MetadataQuery::Columns {
        schema: "INFORMATION_SCHEMA".to_string(),
        object: "TABLES".to_string(),
    });
    let columns = fetch_rows(&profile, &sql).await;
    let column_names: Vec<String> = columns.iter().map(|r| cell_text(&r[0])).collect();
    assert!(
        column_names.iter().any(|c| c == "TABLE_NAME"),
        "INFORMATION_SCHEMA.TABLES columns should include TABLE_NAME, got {column_names:?}"
    );

    // 3) The schema-count query counts the system schemas as non-empty, so
    //    the "Only If Not Empty" placement shows them. This is the live proof
    //    of the sys.objects → sys.all_objects count fix.
    let sql = sql_for(&MetadataQuery::SchemaObjectCounts);
    let counts = fetch_rows(&profile, &sql).await;
    for schema in ["sys", "INFORMATION_SCHEMA"] {
        let count = counts
            .iter()
            .find(|r| cell_text(&r[0]).eq_ignore_ascii_case(schema))
            .map(|r| cell_i64(&r[1]))
            .unwrap_or_else(|| panic!("{schema} missing from schema counts"));
        assert!(
            count > 0,
            "{schema} should count its system objects, got {count}"
        );
    }

    // 4) User schemas are not widened: dbo's table listing must not contain
    //    ms-shipped noise like sys.objects' own rows (spot check: every
    //    returned schema_name is dbo).
    let sql = sql_for(&MetadataQuery::Tables {
        schema: Some("dbo".to_string()),
    });
    let rows = fetch_rows(&profile, &sql).await;
    for row in &rows {
        assert_eq!(
            cell_text(&row[0]),
            "dbo",
            "dbo table listing must stay schema-scoped"
        );
    }
}
