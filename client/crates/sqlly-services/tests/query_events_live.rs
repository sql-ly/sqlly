//! Live-server check of the `QueryEvent` stream contract: the terminal event
//! (`Completed` / `Error` / `Cancelled`) is always the LAST event, after all
//! row batches. Collect-until-Completed consumers (metadata service → schema
//! explorer + intellisense catalog, value lookup, showplan) rely on this;
//! before the ordering fix a fast query's `Completed` could overtake its
//! rows and every one of those consumers silently received zero rows.
//!
//! Self-skips when no live SQL config is present.

use sqlly_services::{DefaultQueryExecutor, QueryEvent, QueryExecutor};
use sqlly_testkit::LiveSqlConfig;

#[tokio::test(flavor = "multi_thread")]
async fn terminal_event_is_last_and_rows_precede_it() {
    let Some(cfg) = LiveSqlConfig::load() else {
        eprintln!("skipping: no live SQL config");
        return;
    };
    let profile = cfg.to_profile("live-events");
    let queries = DefaultQueryExecutor::new();

    for sql in [
        "SELECT name FROM sys.schemas WHERE principal_id IS NOT NULL ORDER BY name",
        "SELECT 1 AS one",
    ] {
        let mut handle = queries
            .execute(&profile, sql, 1000)
            .await
            .expect("execute against live server");
        let mut kinds: Vec<&'static str> = Vec::new();
        let mut rows_total = 0usize;
        while let Some(event) = handle.events.recv().await {
            kinds.push(match &event {
                QueryEvent::Started => "Started",
                QueryEvent::ResultSet { .. } => "ResultSet",
                QueryEvent::Rows { rows, .. } => {
                    rows_total += rows.len();
                    "Rows"
                }
                QueryEvent::TruncatedCells { .. } => "TruncatedCells",
                QueryEvent::RowCount { .. } => "RowCount",
                QueryEvent::RowsProgress { .. } => "RowsProgress",
                QueryEvent::SpillWindow { .. } => "SpillWindow",
                QueryEvent::Message { .. } => "Message",
                QueryEvent::DatabaseChanged { .. } => "DatabaseChanged",
                QueryEvent::Error { .. } => "Error",
                QueryEvent::Completed { .. } => "Completed",
                QueryEvent::Cancelled => "Cancelled",
                QueryEvent::SpillManifest { .. } => "SpillManifest",
            });
        }
        eprintln!("{sql}: {kinds:?} ({rows_total} rows)");

        assert!(rows_total > 0, "query must yield rows: {sql}");
        let terminal_pos = kinds
            .iter()
            .position(|k| matches!(*k, "Completed" | "Error" | "Cancelled"))
            .expect("stream must carry a terminal event");
        assert_eq!(
            terminal_pos,
            kinds.len() - 1,
            "terminal event must be last, got {kinds:?}"
        );
        let last_rows = kinds.iter().rposition(|k| *k == "Rows").unwrap();
        assert!(
            last_rows < terminal_pos,
            "all rows must precede the terminal event, got {kinds:?}"
        );
    }
}
