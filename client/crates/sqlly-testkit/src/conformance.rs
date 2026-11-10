//! Driver conformance harness: one behavior matrix that every dialect driver
//! (`sqlly-tds`, `sqlly-pg`, `sqlly-mysql`, `sqlly-sqlite`) exercises through
//! its public `open_client` / `execute_on_client` / `execute_params_on_client`
//! surface. Each driver crate hosts a `tests/conformance.rs` that runs the
//! same shapes — streaming order, multi-result batches, parameter round-trips,
//! error surfacing — against this collector, so behavioral drift between
//! backends shows up as a failing conformance test, not a bug report.
//!
//! SQLite runs deterministically everywhere; the network dialects gate on
//! their `SQLLY_{PG,MYSQL}_*` / live SQL Server env config and skip silently
//! when absent.

use std::future::Future;

use sqlly_core::{CellValue, EventBus, EventPayload, ResultColumn, SqlParam, StudioResult};

/// Everything a streaming execution produced, reassembled from the event bus.
#[derive(Debug, Default)]
pub struct StreamOutcome {
    /// One entry per result set, in arrival order: columns + rows.
    pub result_sets: Vec<(Vec<ResultColumn>, Vec<Vec<CellValue>>)>,
    /// `RowCount` events (rows-affected notifications), in order.
    pub row_counts: Vec<u64>,
    /// Whether a terminal `QueryCompleted` arrived.
    pub completed: bool,
    /// `Message` texts, in order.
    pub messages: Vec<String>,
    /// `Error` payloads, in order.
    pub errors: Vec<String>,
    /// `DatabaseChanged` names, in order: the engine confirmed the session moved
    /// to a database other than the profile's.
    pub database_changes: Vec<String>,
}

/// Run a streaming execution (rows delivered as events — pass `None` for the
/// row sender) and reassemble what it published. The closure receives the bus
/// to hand to `execute_on_client`.
pub async fn run_streaming<T, Fut>(
    f: impl FnOnce(EventBus) -> Fut,
) -> (StudioResult<T>, StreamOutcome)
where
    Fut: Future<Output = StudioResult<T>>,
{
    let bus = EventBus::new(8192);
    let mut sub = bus.subscribe();
    let result = f(bus.clone()).await;
    drop(bus);

    let mut outcome = StreamOutcome::default();
    while let Ok(event) = sub.recv().await {
        match event.payload {
            EventPayload::DatabaseChanged { name, .. } => {
                outcome.database_changes.push(name);
            }
            EventPayload::ResultSetStarted { columns, .. } => {
                outcome.result_sets.push((columns, Vec::new()));
            }
            EventPayload::Rows {
                result_set_index,
                rows,
                ..
            } => {
                // Rows always follow their ResultSetStarted; index-addressed
                // so out-of-order coalescing would be caught here.
                if let Some(set) = outcome.result_sets.get_mut(result_set_index) {
                    set.1.extend(rows);
                }
            }
            EventPayload::RowCount { rows_affected, .. } => {
                outcome.row_counts.push(rows_affected);
            }
            EventPayload::QueryCompleted { .. } => outcome.completed = true,
            EventPayload::Message { text, .. } => outcome.messages.push(text),
            EventPayload::Error { error } => outcome.errors.push(error),
            _ => {}
        }
    }
    (result, outcome)
}

/// Every `SqlParam` variant, with representative values, for round-trip
/// coverage. Drivers map types differently on the way back (e.g. SQLite has
/// no native bool), so per-driver tests assert the mapped expectations — but
/// every driver must accept the full set without erroring.
pub fn all_param_variants() -> Vec<SqlParam> {
    vec![
        SqlParam::Null,
        SqlParam::Bool(true),
        SqlParam::I64(-42),
        SqlParam::F64(2.5),
        SqlParam::String("it's".to_string()),
        SqlParam::Bytes(vec![0x00, 0xFF, 0x10]),
    ]
}

/// Assert the standard shape of a successful streaming run: N result sets
/// with the expected column names and row counts, and a terminal completion.
pub fn assert_stream_shape(outcome: &StreamOutcome, expected: &[(&[&str], usize)]) {
    assert_eq!(
        outcome.result_sets.len(),
        expected.len(),
        "result-set count mismatch: {outcome:?}"
    );
    for (i, ((columns, rows), (want_cols, want_rows))) in
        outcome.result_sets.iter().zip(expected).enumerate()
    {
        let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(&names, want_cols, "result set {i} column names");
        assert_eq!(rows.len(), *want_rows, "result set {i} row count");
    }
    assert!(outcome.completed, "missing QueryCompleted: {outcome:?}");
    assert!(outcome.errors.is_empty(), "unexpected errors: {outcome:?}");
}
