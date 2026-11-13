//! Streaming query execution port — the direct-wire path the GPUI results grid
//! and LSP adapter both consume.
//!
//! The default impl drives `sqlly_engines::tds::execute_on_client` exactly like the CLI
//! engine does, but instead of framing `EngineFrame`s over stdout it maps the
//! `EventBus` lifecycle events + bounded `RowChannel` batches onto the native
//! [`QueryEvent`] enum on an `mpsc` channel. No serialization is involved on
//! this path.
//!
//! Module layout: [`spill`] owns the spill-to-disk machinery (manifest types,
//! thresholds, tracker), [`producer`] owns driver dispatch + server-side
//! cancel; this module orchestrates them (executor, drain loop) and re-exports
//! the public surface so `crate::query::X` paths are stable.

mod large_cells;
#[cfg(not(target_arch = "wasm32"))]
mod manual_tx;
pub mod multi_target;
#[cfg(not(target_arch = "wasm32"))]
mod producer;
mod spill;
// Used by the browser build; compiled natively for its tests.
#[cfg(any(target_arch = "wasm32", test))]
pub(crate) mod spill_stage;

#[cfg(not(target_arch = "wasm32"))]
pub use manual_tx::{ManualTxState, TransactionEnd};

pub use large_cells::{CellLimits, TruncatedCell, TruncatedKind, truncate_rows};
#[cfg(not(target_arch = "wasm32"))]
pub use producer::{
    CancelHook, CancelSlot, register_duckdb_cancel, register_libsql_cancel, register_mysql_cancel,
    register_oracle_cancel, register_pg_cancel, register_sqlite_cancel, register_tds_cancel,
    run_query_producer,
};
#[cfg(any(target_arch = "wasm32", test))]
pub(crate) use spill::estimate_row_bytes;
pub use spill::{
    RESULT_MEMORY_BUDGET_BYTES, SPILL_ROW_THRESHOLD, SpillManifestData, SpillResultSetData,
    result_memory_budget_bytes, set_result_memory_budget_bytes, set_spill_row_threshold,
    spill_row_threshold, take_spilled_rows,
};

#[cfg(not(target_arch = "wasm32"))]
use std::collections::HashMap;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use sqlly_core::{CellValue, ConnectionProfile, ResultColumn, StudioError, StudioResult};
#[cfg(not(target_arch = "wasm32"))]
use sqlly_core::{EventBus, EventPayload, EventSubscription, RowReceiver, row_channel};
use tokio::sync::mpsc;
#[cfg(not(target_arch = "wasm32"))]
use tokio::task::JoinHandle;

#[cfg(not(target_arch = "wasm32"))]
use spill::SpillTracker;

/// Native, zero-copy-ish streaming contract for query results. This is the
/// canonical event type both the GPUI results grid and the LSP adapter consume;
/// no `serde` is involved on the GPUI path.
#[derive(Debug, Clone)]
pub enum QueryEvent {
    /// Emitted once when the server starts processing the query.
    Started,
    /// Metadata for a result set; precedes any [`QueryEvent::Rows`] for this index.
    ResultSet {
        index: usize,
        columns: Vec<ResultColumn>,
    },
    /// A batch of rows for a result set. `index` matches the preceding `ResultSet`.
    Rows {
        index: usize,
        rows: Vec<Vec<CellValue>>,
    },
    /// Cells in the preceding [`QueryEvent::Rows`] batches that were cut to a
    /// preview at fetch time. Additive: consumers that ignore it just see
    /// shorter strings. Never crosses the engine wire (no frame tag).
    TruncatedCells {
        index: usize,
        cells: Vec<large_cells::TruncatedCell>,
    },
    /// Rows-affected count for the preceding result set.
    RowCount { index: usize, rows_affected: u64 },
    /// Count-only live progress for a result set whose row payloads are no
    /// longer being forwarded (row threshold or byte budget reached).
    /// `rows_seen` is the running total of rows observed by the executor so
    /// far — distinct from [`QueryEvent::RowCount`], which carries the
    /// server-reported rows-affected count. Fallback for suppressed sets
    /// whose spill file could not be flushed — the common suppressed path
    /// emits [`QueryEvent::SpillWindow`] instead, which carries the count.
    RowsProgress { index: usize, rows_seen: usize },
    /// Live paging window over a suppressed result set's GROWING spill file:
    /// rows `0..rows_available` are durably flushed to `spill_file` and safe
    /// to page mid-query. Emitted only for suppressed sets, at the progress
    /// cadence, strictly after a successful flush; `rows_available` is
    /// monotonic per set. The path is a BORROW — the executor still owns the
    /// file, and the final [`QueryEvent::SpillManifest`] (authoritative; it
    /// supersedes every `SpillWindow`) transfers deletion ownership to the
    /// consumer as usual. A consumer must never read at/after
    /// `rows_available`: the on-disk index can run ahead of the flushed data
    /// between ticks.
    SpillWindow {
        index: usize,
        spill_file: String,
        rows_available: usize,
    },
    /// Non-fatal info message from the server.
    Message { text: String },
    /// The connection's current database changed (a `USE` took effect).
    /// Carries the new database name so the UI can retarget the tab's session.
    DatabaseChanged { name: String },
    /// Fatal error — the stream ends after this.
    Error { message: String },
    /// Normal completion. Always the terminal event for a successful query.
    Completed { elapsed_ms: u128 },
    /// The query was cancelled via [`QueryExecutor::cancel`]. Terminal.
    Cancelled,
    /// Spill manifest emitted when a result set exceeds the in-memory
    /// threshold and rows are written to disk files instead of (or in
    /// addition to) being streamed via [`QueryEvent::Rows`]. The GPUI app
    /// loads rows on demand from the spill files via `SpillRowPager`.
    SpillManifest { manifest: SpillManifestData },
}

/// The trait default for executors without manual-commit support.
#[cfg(not(target_arch = "wasm32"))]
fn manual_commit_unavailable() -> StudioError {
    StudioError::CapabilityNotSupportedMessage(sqlly_core::DisplayMessage::new(
        "services.query.errors.manual_commit_unavailable",
        "Manual commit mode is not available for this query service",
    ))
}

/// Handle returned by [`QueryExecutor::execute`]. The caller drains `events`
/// for the result stream and uses `task_id` to cancel the query.
pub struct QueryHandle {
    pub task_id: String,
    pub events: mpsc::Receiver<QueryEvent>,
}

/// Port: streaming query execution with cancellation.
///
/// `execute` opens a bounded channel the caller drains while the engine pulls
/// rows from the server; `cancel` aborts a running task by id. The default impl
/// is wired to `sqlly_engines::tds::execute_on_client` + `EventBus` + `RowChannel`,
/// mirroring the CLI engine path but emitting native [`QueryEvent`]s.
#[async_trait]
pub trait QueryExecutor: Send + Sync {
    async fn execute(
        &self,
        profile: &ConnectionProfile,
        sql: &str,
        row_buffer: usize,
    ) -> StudioResult<QueryHandle>;

    /// Execute in a named warm-connection lane (e.g. a query tab's id). Two
    /// executions with the same session key share a warm client; distinct
    /// keys never do. `None` uses the shared default lane. The default body
    /// ignores the session (ports without pooling — browser, test fakes).
    async fn execute_in_session(
        &self,
        profile: &ConnectionProfile,
        session: Option<&str>,
        sql: &str,
        row_buffer: usize,
    ) -> StudioResult<QueryHandle> {
        let _ = session;
        self.execute(profile, sql, row_buffer).await
    }

    /// Like [`Self::execute_in_session`], but oversized cells arrive whole:
    /// no 8 KiB preview cut and no [`QueryEvent::TruncatedCells`]. For
    /// consumers that write every value out — exports, dumps, transfers, the
    /// CLI — rather than paint a grid. Every row is forwarded as
    /// [`QueryEvent::Rows`] (nothing is withheld into a spill file), so the
    /// consumer must write rows as they arrive: memory stays bounded by the
    /// bounded event channel, whose backpressure stops the drain pulling
    /// rows until the consumer catches up.
    ///
    /// The default body falls back to [`Self::execute_in_session`]: a port
    /// that cannot deliver full values (the browser preview, a remote
    /// engine behind a relay) still cuts previews and may still withhold
    /// rows into a spill file, so callers must keep handling
    /// `TruncatedCells` and `SpillManifest`.
    async fn execute_full_values(
        &self,
        profile: &ConnectionProfile,
        session: Option<&str>,
        sql: &str,
        row_buffer: usize,
    ) -> StudioResult<QueryHandle> {
        self.execute_in_session(profile, session, sql, row_buffer)
            .await
    }

    /// A results-grid run: [`Self::execute_in_session`] for a consumer that
    /// pages a large result from its spill file (the results view) rather
    /// than collecting every row. The browser build overrides it to keep
    /// rows past the spill threshold in browser storage; everywhere else it
    /// is `execute_in_session`, which already spills.
    async fn execute_for_grid(
        &self,
        profile: &ConnectionProfile,
        session: Option<&str>,
        sql: &str,
        row_buffer: usize,
    ) -> StudioResult<QueryHandle> {
        self.execute_in_session(profile, session, sql, row_buffer)
            .await
    }

    /// Drop every warm connection held for this profile (any lane). Hosts
    /// call this when the profile is edited, disconnected, or its Entra
    /// account changes, so stale-credential sessions never linger until the
    /// idle TTL. Default no-op for ports without pooling.
    fn evict_profile(&self, profile: &ConnectionProfile) {
        let _ = profile;
    }

    /// Open a manual transaction on a dedicated client for `session` (a tab id).
    /// Idempotent: a second call while one is open returns the current state.
    /// Manual commit mode is a desktop-only feature; the types live behind the
    /// same `not(wasm32)` gate as the `manual_tx` module, so gate the method too.
    #[cfg(not(target_arch = "wasm32"))]
    async fn begin_manual_transaction(
        &self,
        profile: &ConnectionProfile,
        session: &str,
    ) -> StudioResult<ManualTxState> {
        let _ = (profile, session);
        Err(manual_commit_unavailable())
    }

    /// COMMIT or ROLLBACK the pinned transaction and release the client back to
    /// the tab's warm lane (clean) or drop it (any error).
    #[cfg(not(target_arch = "wasm32"))]
    async fn end_manual_transaction(
        &self,
        profile: &ConnectionProfile,
        session: &str,
        end: TransactionEnd,
    ) -> StudioResult<()> {
        let _ = (profile, session, end);
        Err(manual_commit_unavailable())
    }

    /// Current pinned-session state for `session`, if any. Pure lookup.
    #[cfg(not(target_arch = "wasm32"))]
    fn manual_transaction_state(
        &self,
        profile: &ConnectionProfile,
        session: &str,
    ) -> Option<ManualTxState> {
        let _ = (profile, session);
        None
    }

    async fn cancel(&self, task_id: &str) -> StudioResult<()>;
}

#[cfg(not(target_arch = "wasm32"))]
struct RunningQuery {
    producer: JoinHandle<()>,
    cancel: CancelSlot,
}

/// Real query executor backed by `sqlly-tds`.
///
/// Owns a task table of in-flight query producers so [`cancel`] can abort
/// them. The drain task is detached — it ends naturally when both the
/// `EventBus` subscription and the `RowChannel` close (producer dropped or
/// aborted, or the caller dropped the receiver).
///
/// TDS queries run on warm pooled clients (`tds_pool`): checkout removes the
/// lane's idle client (concurrent queries on the same lane open parallel
/// fresh connections — no head-of-line blocking), a clean completion checks
/// the client back in, and any error/cancellation drops it. This is the
/// primary latency win for Microsoft Entra profiles: every fresh Entra
/// connection pays AAD token validation at the Azure SQL gateway, which
/// dominates small-query latency. A warm client whose query fails before any
/// event was published (provably pre-execution, e.g. a socket the server
/// dropped while idle) is retried once on a fresh connection; after any
/// event has been emitted there is no retry, so side effects and streamed
/// rows are never duplicated.
///
/// [`cancel`]: QueryExecutor::cancel
#[cfg(not(target_arch = "wasm32"))]
pub struct DefaultQueryExecutor {
    tasks: Arc<Mutex<HashMap<String, RunningQuery>>>,
    pool: crate::tds_pool::TdsPool,
    pg_pool: crate::tds_pool::PgPool,
    mysql_pool: crate::tds_pool::MySqlPool,
    pinned: manual_tx::PinnedSessions,
}

#[cfg(not(target_arch = "wasm32"))]
impl DefaultQueryExecutor {
    pub fn new() -> Self {
        Self::with_pool(crate::tds_pool::TdsPool::new())
    }

    /// Share one pool across services (query executor + row editing) so the
    /// reaper and profile eviction cover every warm client the app holds.
    pub fn with_pool(pool: crate::tds_pool::TdsPool) -> Self {
        pool.spawn_reaper();
        let pg_pool = crate::tds_pool::PgPool::new();
        pg_pool.spawn_reaper();
        let mysql_pool = crate::tds_pool::MySqlPool::new();
        mysql_pool.spawn_reaper();
        Self {
            tasks: Arc::new(Mutex::new(HashMap::new())),
            pool,
            pg_pool,
            mysql_pool,
            pinned: manual_tx::PinnedSessions::default(),
        }
    }

    /// The shared warm-client pool (row editing uses a dedicated lane on it).
    pub fn pool(&self) -> &crate::tds_pool::TdsPool {
        &self.pool
    }

    async fn checkout_manual_client(
        &self,
        profile: &ConnectionProfile,
        session: &str,
        kind: sqlly_core::ConnectionKind,
    ) -> StudioResult<manual_tx::PinnedClient> {
        use sqlly_core::ConnectionKind;
        match kind {
            ConnectionKind::Tds => {
                let key = crate::tds_pool::TdsPool::pool_key(Some(session), profile);
                if let Some(client) = self.pool.checkout(&key) {
                    return Ok(manual_tx::PinnedClient::Tds(client));
                }
                sqlly_engines::tds::open_client(profile)
                    .await
                    .map(manual_tx::PinnedClient::Tds)
            }
            ConnectionKind::PostgresWire => {
                let key = crate::tds_pool::PgPool::pool_key(Some(session), profile);
                if let Some(client) = self.pg_pool.checkout(&key) {
                    return Ok(manual_tx::PinnedClient::Pg(client));
                }
                sqlly_engines::pg::open_client(profile)
                    .await
                    .map(manual_tx::PinnedClient::Pg)
            }
            ConnectionKind::MySqlWire => {
                let key = crate::tds_pool::MySqlPool::pool_key(Some(session), profile);
                if let Some(client) = self.mysql_pool.checkout(&key) {
                    return Ok(manual_tx::PinnedClient::MySql(client));
                }
                sqlly_engines::mysql::open_client(profile)
                    .await
                    .map(manual_tx::PinnedClient::MySql)
            }
            other => Err(StudioError::CapabilityNotSupportedMessage(
                sqlly_core::DisplayMessage::new(
                    "services.query.errors.manual_commit_connection_kind_unsupported",
                    format!("Manual commit mode is not supported on {other:?} connections"),
                )
                .with_arg("connection_kind", format!("{other:?}")),
            )),
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Default for DefaultQueryExecutor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl DefaultQueryExecutor {
    /// Spawn the producer + drain for one execution. `full_values` picks the
    /// streaming policy (every row forwarded, every cell whole) for
    /// consumers that write rows out; otherwise the grid policy (previews
    /// and spill withholding) applies.
    fn start_query(
        &self,
        profile: &ConnectionProfile,
        session: Option<&str>,
        sql: &str,
        row_buffer: usize,
        full_values: bool,
    ) -> StudioResult<QueryHandle> {
        let task_id = uuid::Uuid::new_v4().to_string();
        let buffer = row_buffer.max(1);

        // QueryEvent channel the caller drains.
        let (event_tx, event_rx) = mpsc::channel::<QueryEvent>(buffer);

        // Lifecycle events flow on the broadcast EventBus; rows flow on the
        // bounded RowChannel for real backpressure. Both mirror the engine.
        let events = EventBus::new(1024);
        let subscription = events.subscribe();
        let (rows_tx, rows_rx) = row_channel(buffer);
        let producer_events = events.clone();
        drop(events); // only the producer + the subscription hold refs now

        let producer_profile = profile.clone();
        let producer_sql = sql.to_string();
        let producer_pool = self.pool.clone();
        let producer_pg_pool = self.pg_pool.clone();
        let producer_mysql_pool = self.mysql_pool.clone();
        let producer_session = session.map(str::to_string);
        let producer_pinned = self.pinned.clone();
        let cancel_slot: CancelSlot = Arc::new(Mutex::new(None));
        let producer_cancel_slot = cancel_slot.clone();

        // Producer: checks out (or opens) a client for the profile's engine
        // and streams the query to completion. On any error (including a
        // cancelled/aborted task) the client is dropped — its protocol stream
        // is in an unknown state and must not be reused. On clean completion
        // a TDS client is checked back into its lane for reuse. Rows +
        // lifecycle events are published as they arrive.
        let producer = tokio::spawn(async move {
            if let Err(error) = producer::run_query_producer_with_pinned(
                &producer_profile,
                &producer_sql,
                &producer_events,
                &rows_tx,
                &producer_pool,
                &producer_pg_pool,
                &producer_mysql_pool,
                producer_session.as_deref(),
                &producer_pinned,
                &producer_cancel_slot,
            )
            .await
            {
                // A driver may return Err without publishing an
                // EventPayload::Error (e.g. when a SQL syntax error surfaces
                // before any events are emitted). Publish the error here so
                // the drain sees a terminal event instead of synthesizing
                // Cancelled.
                let _ = producer_events.publish(EventPayload::Error {
                    error: error.to_string(),
                });
            }
            // rows_tx + producer_events dropped here → the drain sees EOF on
            // both channels and emits the terminal event.
        });

        // Track the producer so cancel() can stop it (server-side hook
        // first, then task abort).
        self.tasks.lock().expect("task table poisoned").insert(
            task_id.clone(),
            RunningQuery {
                producer,
                cancel: cancel_slot,
            },
        );

        // Drain: maps EventBus + RowChannel → QueryEvent until both close,
        // then removes the producer from the task table. Detached — it ends
        // naturally on stream completion, cancellation, or receiver drop.
        let drain_tasks = self.tasks.clone();
        let drain_task_id = task_id.clone();
        tokio::spawn(async move {
            let spill = if full_values {
                SpillTracker::streaming_full_values()
            } else {
                SpillTracker::new(spill_row_threshold(), result_memory_budget_bytes())
            };
            drain_query_stream_with(subscription, rows_rx, event_tx, spill).await;
            drain_tasks
                .lock()
                .expect("task table poisoned")
                .remove(&drain_task_id);
        });

        Ok(QueryHandle {
            task_id,
            events: event_rx,
        })
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[async_trait]
impl QueryExecutor for DefaultQueryExecutor {
    async fn execute(
        &self,
        profile: &ConnectionProfile,
        sql: &str,
        row_buffer: usize,
    ) -> StudioResult<QueryHandle> {
        self.execute_in_session(profile, None, sql, row_buffer)
            .await
    }

    async fn execute_in_session(
        &self,
        profile: &ConnectionProfile,
        session: Option<&str>,
        sql: &str,
        row_buffer: usize,
    ) -> StudioResult<QueryHandle> {
        self.start_query(profile, session, sql, row_buffer, false)
    }

    async fn execute_full_values(
        &self,
        profile: &ConnectionProfile,
        session: Option<&str>,
        sql: &str,
        row_buffer: usize,
    ) -> StudioResult<QueryHandle> {
        self.start_query(profile, session, sql, row_buffer, true)
    }

    fn evict_profile(&self, profile: &ConnectionProfile) {
        let identity = profile.pool_identity();
        let evicted = self.pool.evict_identity(&identity)
            + self.pg_pool.evict_identity(&identity)
            + self.mysql_pool.evict_identity(&identity);
        if evicted > 0 {
            tracing::debug!(evicted, "warm pool: evicted clients for profile");
        }
        let kind = profile.database_type.connection_kind();
        let rollback_sql = manual_tx::end_sql(kind, manual_tx::TransactionEnd::Rollback);
        for (_key, client) in self.pinned.drain_identity_clients(&identity) {
            if let Some(mut client) = client {
                let sql = rollback_sql;
                tokio::spawn(async move {
                    let _ = match &mut client {
                        manual_tx::PinnedClient::Tds(c) => {
                            sqlly_engines::tds::execute_batch_on_client(c, sql).await
                        }
                        manual_tx::PinnedClient::Pg(c) => {
                            sqlly_engines::pg::execute_batch_on_client(c, sql).await
                        }
                        manual_tx::PinnedClient::MySql(c) => {
                            sqlly_engines::mysql::execute_batch_on_client(c, sql).await
                        }
                    };
                });
            }
        }
    }

    async fn begin_manual_transaction(
        &self,
        profile: &ConnectionProfile,
        session: &str,
    ) -> StudioResult<ManualTxState> {
        use sqlly_core::ConnectionKind;
        let kind = profile.database_type.connection_kind();
        match kind {
            ConnectionKind::Tds | ConnectionKind::PostgresWire | ConnectionKind::MySqlWire => {}
            _ => {
                return Err(StudioError::CapabilityNotSupportedMessage(
                    sqlly_core::DisplayMessage::new(
                        "services.query.errors.manual_commit_engine_unsupported",
                        format!(
                            "Manual commit mode is not supported on {}",
                            profile.database_type.display_name()
                        ),
                    )
                    .with_arg("engine_name", profile.database_type.display_name()),
                ));
            }
        }
        let key = crate::tds_pool::TdsPool::pool_key(Some(session), profile);
        if let Some(state) = self.pinned.state(&key) {
            if state != ManualTxState::Lost {
                return Ok(state);
            }
            self.pinned.close(&key);
        }
        let mut client = self.checkout_manual_client(profile, session, kind).await?;
        let begin = manual_tx::begin_sql(kind);
        match &mut client {
            manual_tx::PinnedClient::Tds(c) => {
                sqlly_engines::tds::execute_batch_on_client(c, begin).await?
            }
            manual_tx::PinnedClient::Pg(c) => {
                sqlly_engines::pg::execute_batch_on_client(c, begin).await?
            }
            manual_tx::PinnedClient::MySql(c) => {
                sqlly_engines::mysql::execute_batch_on_client(c, begin).await?
            }
        }
        self.pinned.open(key.clone(), client);
        Ok(self
            .pinned
            .state(&key)
            .unwrap_or(ManualTxState::Open { statements: 0 }))
    }

    async fn end_manual_transaction(
        &self,
        profile: &ConnectionProfile,
        session: &str,
        end: TransactionEnd,
    ) -> StudioResult<()> {
        let kind = profile.database_type.connection_kind();
        let key = crate::tds_pool::TdsPool::pool_key(Some(session), profile);
        let Some(mut client) = self.pinned.take(&key)? else {
            self.pinned.close(&key);
            return Ok(());
        };
        let sql = manual_tx::end_sql(kind, end);
        let result = match &mut client {
            manual_tx::PinnedClient::Tds(c) => {
                sqlly_engines::tds::execute_batch_on_client(c, sql).await
            }
            manual_tx::PinnedClient::Pg(c) => {
                sqlly_engines::pg::execute_batch_on_client(c, sql).await
            }
            manual_tx::PinnedClient::MySql(c) => {
                sqlly_engines::mysql::execute_batch_on_client(c, sql).await
            }
        };
        self.pinned.close(&key);
        if result.is_ok() {
            match client {
                manual_tx::PinnedClient::Tds(c) => self.pool.checkin(key, c),
                manual_tx::PinnedClient::Pg(c) => self.pg_pool.checkin(key, c),
                manual_tx::PinnedClient::MySql(c) => self.mysql_pool.checkin(key, c),
            }
        }
        result
    }

    fn manual_transaction_state(
        &self,
        profile: &ConnectionProfile,
        session: &str,
    ) -> Option<ManualTxState> {
        let key = crate::tds_pool::TdsPool::pool_key(Some(session), profile);
        self.pinned.state(&key)
    }

    async fn cancel(&self, task_id: &str) -> StudioResult<()> {
        if let Some(running) = self
            .tasks
            .lock()
            .expect("task table poisoned")
            .remove(task_id)
        {
            // Server-side first: stop the statement on the server, then
            // abort the local producer. Order matters — aborting first drops
            // the client, after which some backends have nothing to address
            // the cancel request at.
            if let Some(hook) = running.cancel.lock().expect("cancel slot poisoned").take() {
                hook();
            }
            running.producer.abort();
        }
        Ok(())
    }
}

/// Drives the lifecycle-event subscription and the bounded row receiver until
/// both close, mapping every item onto a [`QueryEvent`] sent to `event_tx`.
/// When the streams end without a terminal event (`Completed` / `Error`) —
/// i.e. the producer was aborted — a synthetic [`QueryEvent::Cancelled`] is
/// emitted so the consumer always observes a terminal frame.
///
/// **Terminal events are always last.** Lifecycle events and row batches
/// travel on separate channels, so a fast query's `QueryCompleted` can reach
/// the drain while its row batches are still queued on the row channel.
/// Forwarding the terminal immediately would break the documented contract
/// ("`Completed` — always the terminal event") and make every
/// collect-until-Completed consumer (metadata service, value lookup,
/// showplan) silently lose the rows. The drain therefore *defers* the
/// terminal event (`Completed` / `Error` / `Cancelled`) until both channels
/// have closed and every buffered row batch has been forwarded.
///
/// If the consumer drops `event_rx`, `event_tx.send` returns `Err` and the
/// drain stops immediately; the producer then learns the consumer is gone via
/// `RowChannelClosed` on its next `send` and stops pulling from the server.
#[cfg(not(target_arch = "wasm32"))]
#[cfg(test)]
async fn drain_query_stream(
    subscription: EventSubscription,
    rows_rx: RowReceiver,
    event_tx: mpsc::Sender<QueryEvent>,
) {
    drain_query_stream_with(
        subscription,
        rows_rx,
        event_tx,
        SpillTracker::new(spill_row_threshold(), result_memory_budget_bytes()),
    )
    .await;
}

/// The drain documented above (`drain_query_stream` is its grid-mode test
/// wrapper) with the row/cell policy given by `spill`: the grid tracker cuts
/// oversized cells to previews and withholds rows past the threshold into a
/// spill file; [`SpillTracker::streaming_full_values`] forwards everything
/// whole.
#[cfg(not(target_arch = "wasm32"))]
async fn drain_query_stream_with(
    mut subscription: EventSubscription,
    mut rows_rx: RowReceiver,
    event_tx: mpsc::Sender<QueryEvent>,
    mut spill: SpillTracker,
) {
    use tokio::sync::broadcast::error::RecvError;

    let mut events_done = false;
    let mut rows_done = false;
    let mut current_result_set = 0usize;
    // The terminal event, held back until all rows have been forwarded. The
    // producer drops both channels right after publishing it, so deferral
    // adds no latency.
    let mut deferred_terminal: Option<QueryEvent> = None;
    // Row batches that arrived before their `ResultSetStarted` lifecycle
    // event was consumed. Lifecycle events and rows travel on separate
    // channels, and consumers (the results view) drop `Rows` for an index
    // they have not seen a `ResultSet` for — so out-of-order batches are
    // stashed here and flushed right after their `ResultSet` is forwarded.
    let mut pending_rows: HashMap<usize, Vec<Vec<Vec<CellValue>>>> = HashMap::new();

    loop {
        // `biased` polls the lifecycle branch first. The producer always
        // publishes `ResultSetStarted` before sending that set's rows, so
        // draining lifecycle events preferentially preserves the
        // ResultSet-before-Rows ordering that a fair (random) select breaks
        // roughly half the time.
        tokio::select! {
            biased;
            event = subscription.recv(), if !events_done => match event {
                Ok(event) => {
                    let outgoing = match event.payload {
                        EventPayload::QueryStarted { .. } => Some(QueryEvent::Started),
                        EventPayload::ResultSetStarted { result_set_index, columns, .. } => {
                            current_result_set = result_set_index;
                            spill.record_columns(result_set_index, columns.clone());
                            Some(QueryEvent::ResultSet { index: result_set_index, columns })
                        }
                        EventPayload::RowCount { rows_affected, .. } => {
                            Some(QueryEvent::RowCount { index: current_result_set, rows_affected })
                        }
                        EventPayload::Message { text, .. } => {
                            spill.messages.push(text.clone());
                            Some(QueryEvent::Message { text })
                        }
                        EventPayload::DatabaseChanged { name, .. } => {
                            Some(QueryEvent::DatabaseChanged { name })
                        }
                        EventPayload::Error { error } => {
                            spill.error = Some(error.clone());
                            deferred_terminal = Some(QueryEvent::Error { message: error });
                            None
                        }
                        EventPayload::QueryCompleted { elapsed_ms, .. } => {
                            spill.elapsed_ms = Some(elapsed_ms as i64);
                            deferred_terminal = Some(QueryEvent::Completed { elapsed_ms });
                            None
                        }
                        EventPayload::QueryCancelled { .. } => {
                            deferred_terminal = Some(QueryEvent::Cancelled);
                            None
                        }
                        // Connection/explorer events are irrelevant on the query path.
                        _ => None,
                    };
                    let flush_for = match &outgoing {
                        Some(QueryEvent::ResultSet { index, .. }) => Some(*index),
                        _ => None,
                    };
                    if let Some(ev) = outgoing
                        && event_tx.send(ev).await.is_err() {
                            return;
                    }
                    // Flush any row batches that raced ahead of this
                    // result set's lifecycle event.
                    if let Some(index) = flush_for
                        && let Some(batches) = pending_rows.remove(&index)
                    {
                        for rows in batches {
                            if spill.handle_rows(index, rows, &event_tx).await.is_err() {
                                return;
                            }
                        }
                    }
                }
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => events_done = true,
            },
            batch = rows_rx.recv(), if !rows_done => match batch {
                Some(batch) => {
                    let index = batch.result_set_index;
                    if spill.columns_by_index.contains_key(&index) {
                        if spill.handle_rows(index, batch.rows, &event_tx).await.is_err() {
                            return;
                        }
                    } else {
                        // ResultSetStarted not seen yet — hold the batch so
                        // the consumer never sees Rows before its ResultSet.
                        pending_rows.entry(index).or_default().push(batch.rows);
                    }
                }
                None => rows_done = true,
            },
            else => break,
        }

        if events_done && rows_done {
            break;
        }
    }

    // Flush anything still pending (a producer that died between publishing
    // rows and its lifecycle event) so data is at least not lost silently.
    for (index, batches) in pending_rows {
        for rows in batches {
            if spill.handle_rows(index, rows, &event_tx).await.is_err() {
                return;
            }
        }
    }

    if let Some(manifest) = spill.into_manifest() {
        let _ = event_tx.send(QueryEvent::SpillManifest { manifest }).await;
    }

    // Terminal event last, always — the deferred one from the producer, or a
    // synthetic Cancelled when the producer died without publishing one.
    let terminal = deferred_terminal.unwrap_or(QueryEvent::Cancelled);
    let _ = event_tx.send(terminal).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::{EventBus, EventPayload, SessionId, row_channel};

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn manual_commit_refusal_displays_the_capability_prefix_once() {
        let error = manual_commit_unavailable();
        assert_eq!(
            error.to_string(),
            "Server capability not supported: Manual commit mode is not available for this query service"
        );
        assert_eq!(
            error.message().fallback(),
            "Manual commit mode is not available for this query service"
        );
    }

    fn sqlite_memory_profile() -> ConnectionProfile {
        let mut profile = ConnectionProfile::sql_login(
            "sqlite-test",
            ":memory:",
            "",
            sqlly_core::SecretRef {
                provider: sqlly_core::SecretProvider::InMemory,
                key: String::new(),
            },
        );
        profile.database_type = sqlly_core::DatabaseType::Sqlite;
        profile
    }

    /// Collect every `Rows` cell and every `TruncatedCells` entry of a run.
    async fn drain_cells(mut handle: QueryHandle) -> (Vec<CellValue>, usize) {
        let mut cells = Vec::new();
        let mut cut = 0usize;
        while let Some(event) = handle.events.recv().await {
            match event {
                QueryEvent::Rows { rows, .. } => cells.extend(rows.into_iter().flatten()),
                QueryEvent::TruncatedCells { cells: c, .. } => cut += c.len(),
                QueryEvent::Completed { .. } => break,
                QueryEvent::Error { message } => panic!("query failed: {message}"),
                _ => {}
            }
        }
        (cells, cut)
    }

    /// Grid runs cut a 20 KiB cell to its 8 KiB preview and say so; a
    /// full-values run (exports, CLI) delivers the same cell whole.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn full_values_run_delivers_a_20_kib_cell_whole() {
        let sql = "SELECT replace(hex(zeroblob(10240)), '00', 'ab') AS body";
        let executor = DefaultQueryExecutor::new();
        let profile = sqlite_memory_profile();

        let preview = executor.execute(&profile, sql, 8).await.unwrap();
        let (cells, cut) = drain_cells(preview).await;
        assert_eq!(cut, 1, "the grid path reports the cut");
        let CellValue::String(text) = &cells[0] else {
            panic!("expected text, got {:?}", cells[0]);
        };
        assert_eq!(text.len(), 8 * 1024);

        let full = executor
            .execute_full_values(&profile, None, sql, 8)
            .await
            .unwrap();
        let (cells, cut) = drain_cells(full).await;
        assert_eq!(cut, 0, "nothing is cut for an export");
        assert_eq!(cells, vec![CellValue::String("ab".repeat(10 * 1024))]);
    }

    /// A full-values run forwards every row even past the spill threshold, so
    /// streaming consumers (dumps, transfers) that never read a spill file
    /// cannot lose the tail.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn full_values_run_forwards_rows_past_the_spill_threshold() {
        let total = SPILL_ROW_THRESHOLD + 5_000;
        let sql = format!(
            "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < {total}) SELECT x FROM c"
        );
        let executor = DefaultQueryExecutor::new();
        let mut handle = executor
            .execute_full_values(&sqlite_memory_profile(), None, &sql, 16)
            .await
            .unwrap();
        let mut rows = 0usize;
        while let Some(event) = handle.events.recv().await {
            match event {
                QueryEvent::Rows { rows: batch, .. } => rows += batch.len(),
                QueryEvent::SpillManifest { .. } => panic!("nothing may be withheld"),
                QueryEvent::Completed { .. } => break,
                QueryEvent::Error { message } => panic!("query failed: {message}"),
                _ => {}
            }
        }
        assert_eq!(rows, total);
    }

    /// A statement that outruns the profile's statement timeout is cancelled
    /// server-side (SQLite interrupt here) and reported as a timeout — the
    /// stream terminates promptly instead of running the full computation.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn statement_timeout_cancels_and_reports_timeout() {
        let mut profile = sqlite_memory_profile();
        profile.statement_timeout_seconds = Some(1);
        let executor = DefaultQueryExecutor::new();
        let started = std::time::Instant::now();
        let mut handle = executor
            .execute(
                &profile,
                // CPU-bound recursive CTE that would run far longer than the
                // 1s ceiling if left alone.
                "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 500000000) SELECT count(*) FROM c;",
                32,
            )
            .await
            .unwrap();

        let mut error_message = None;
        while let Some(event) = handle.events.recv().await {
            if let QueryEvent::Error { message } = event {
                error_message = Some(message);
            }
        }
        let message = error_message.expect("the timed-out query must report an error");
        assert!(
            message.to_ascii_lowercase().contains("timeout"),
            "expected a timeout error, got: {message}"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "the stream must end promptly after the ceiling, took {:?}",
            started.elapsed()
        );
    }

    /// The whole native pipeline, no mocks: engine dispatch picks the SQLite
    /// driver, the driver streams over EventBus + RowChannel, the drain maps
    /// onto QueryEvents — exactly what the GPUI grid consumes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn executor_runs_sqlite_end_to_end() {
        let executor = DefaultQueryExecutor::new();
        let mut handle = executor
            .execute(
                &sqlite_memory_profile(),
                "CREATE TABLE t (id INTEGER, name TEXT);\n\
                 INSERT INTO t VALUES (1, 'a'), (2, 'b');\n\
                 SELECT id, name FROM t ORDER BY id;",
                32,
            )
            .await
            .unwrap();

        let mut row_count = 0usize;
        let mut result_set_columns: Option<Vec<ResultColumn>> = None;
        let mut completed = false;
        while let Some(event) = handle.events.recv().await {
            match event {
                QueryEvent::ResultSet { columns, .. } => result_set_columns = Some(columns),
                QueryEvent::Rows { rows, .. } => row_count += rows.len(),
                QueryEvent::Completed { .. } => completed = true,
                QueryEvent::Error { message } => panic!("unexpected error: {message}"),
                _ => {}
            }
        }
        let columns = result_set_columns.expect("result set metadata");
        assert_eq!(columns[0].name, "id");
        assert_eq!(row_count, 2);
        assert!(completed, "terminal Completed event must arrive");
    }

    /// Cancel fires the backend's server-side hook (SQLite interrupt) and
    /// aborts the producer: the caller sees a terminal event promptly even
    /// though the statement would otherwise run forever.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_interrupts_a_running_sqlite_statement() {
        let executor = DefaultQueryExecutor::new();
        let mut handle = executor
            .execute(
                &sqlite_memory_profile(),
                "WITH RECURSIVE c(x) AS (VALUES(1) UNION ALL SELECT x + 1 FROM c) \
                 SELECT count(*) FROM c",
                32,
            )
            .await
            .unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while let Some(event) = handle.events.recv().await {
                if matches!(event, QueryEvent::Started) {
                    return;
                }
            }
            panic!("event stream closed before query started");
        })
        .await
        .expect("query must start within 1s");
        executor.cancel(&handle.task_id).await.unwrap();

        let terminal = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while let Some(event) = handle.events.recv().await {
                match event {
                    QueryEvent::Cancelled | QueryEvent::Error { .. } => return Some(event),
                    QueryEvent::Completed { .. } => panic!("must not complete"),
                    _ => {}
                }
            }
            None
        })
        .await
        .expect("terminal event must arrive promptly after cancel");
        assert!(terminal.is_some(), "stream ended without a terminal event");
    }

    /// Engines without a native driver fail with a capability error, not a
    /// protocol mismatch or a hang.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn executor_reports_missing_driver_as_error_event() {
        let executor = DefaultQueryExecutor::new();
        let mut profile = sqlite_memory_profile();
        profile.database_type = sqlly_core::DatabaseType::Db2;
        let mut handle = executor.execute(&profile, "SELECT 1", 8).await.unwrap();

        let mut error_message = None;
        while let Some(event) = handle.events.recv().await {
            if let QueryEvent::Error { message } = event {
                error_message = Some(message);
            }
        }
        let message = error_message.expect("error event");
        assert!(message.contains("no driver yet"), "got: {message}");
        assert!(message.contains("Db2"), "got: {message}");
    }

    /// Publishes a synthetic multi-result-set query onto an EventBus + RowChannel
    /// and asserts the drain maps it onto the expected QueryEvent sequence
    /// (Started → ResultSet → Rows → RowCount → Completed).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drain_maps_event_bus_and_rows_to_query_events() {
        let events = EventBus::new(64);
        let subscription = events.subscribe();
        let (rows_tx, rows_rx) = row_channel(8);
        let (event_tx, mut event_rx) = mpsc::channel::<QueryEvent>(32);

        let session = SessionId::new();
        let producer = tokio::spawn(async move {
            events
                .publish(EventPayload::QueryStarted {
                    session_id: session.clone(),
                })
                .unwrap();
            events
                .publish(EventPayload::ResultSetStarted {
                    session_id: session.clone(),
                    result_set_index: 0,
                    columns: vec![ResultColumn {
                        name: "id".into(),
                        data_type: "int".into(),
                        nullable: false,
                    }],
                })
                .unwrap();
            rows_tx
                .send(sqlly_core::RowBatch::new(
                    session.clone(),
                    0,
                    vec![vec![CellValue::I64(1)], vec![CellValue::I64(2)]],
                ))
                .await
                .unwrap();
            drop(rows_tx);
            events
                .publish(EventPayload::RowCount {
                    session_id: session.clone(),
                    rows_affected: 2,
                })
                .unwrap();
            events
                .publish(EventPayload::QueryCompleted {
                    session_id: session,
                    elapsed_ms: 42,
                })
                .unwrap();
        });

        drain_query_stream(subscription, rows_rx, event_tx).await;
        producer.await.unwrap();

        let collected: Vec<QueryEvent> =
            std::iter::from_fn(|| futures::executor::block_on(async { event_rx.recv().await }))
                .take(10)
                .collect();

        let kinds: Vec<&str> = collected.iter().map(kind_of).map(Option::unwrap).collect();

        // Started must be first (it's the first event published).
        assert_eq!(kinds.first(), Some(&"Started"), "Started must be first");

        // ResultSet must come before both Rows and RowCount (same result set).
        let rs_pos = kinds
            .iter()
            .position(|k| *k == "ResultSet")
            .expect("ResultSet must be present");
        let rows_pos = kinds
            .iter()
            .position(|k| *k == "Rows")
            .expect("Rows must be present");
        let rowcount_pos = kinds
            .iter()
            .position(|k| *k == "RowCount")
            .expect("RowCount must be present");
        assert!(
            rs_pos < rows_pos && rs_pos < rowcount_pos,
            "ResultSet must precede Rows and RowCount"
        );

        // All five events must be present, and Completed must be the LAST
        // event — the drain defers the terminal until every row batch has
        // been flushed, so collect-until-Completed consumers never lose rows.
        assert!(kinds.contains(&"Started"));
        assert!(kinds.contains(&"ResultSet"));
        assert!(kinds.contains(&"Rows"));
        assert!(kinds.contains(&"RowCount"));
        assert_eq!(
            kinds.last(),
            Some(&"Completed"),
            "Completed must be terminal, got {kinds:?}"
        );
    }

    /// Regression: a fast query queues `ResultSetStarted` / `RowCount` /
    /// `QueryCompleted` on the lifecycle bus while its row batch is still
    /// sitting in the row channel. The biased drain loop consumes lifecycle
    /// events first, so without terminal deferral it forwarded `Completed`
    /// before `Rows` — and every collect-until-Completed consumer (metadata
    /// lists → schema explorer + intellisense catalog, value lookup,
    /// showplan) silently received ZERO rows. Everything is queued before
    /// the drain starts, making the inversion deterministic.
    #[tokio::test]
    async fn drain_defers_terminal_until_rows_are_flushed() {
        let events = EventBus::new(64);
        let subscription = events.subscribe();
        let (rows_tx, rows_rx) = row_channel(8);
        let (event_tx, mut event_rx) = mpsc::channel::<QueryEvent>(32);

        let session = SessionId::new();
        events
            .publish(EventPayload::QueryStarted {
                session_id: session.clone(),
            })
            .unwrap();
        events
            .publish(EventPayload::ResultSetStarted {
                session_id: session.clone(),
                result_set_index: 0,
                columns: vec![ResultColumn {
                    name: "name".into(),
                    data_type: "nvarchar".into(),
                    nullable: true,
                }],
            })
            .unwrap();
        rows_tx
            .send(sqlly_core::RowBatch::new(
                session.clone(),
                0,
                vec![vec![CellValue::String("dbo".into())]],
            ))
            .await
            .unwrap();
        events
            .publish(EventPayload::RowCount {
                session_id: session.clone(),
                rows_affected: 1,
            })
            .unwrap();
        events
            .publish(EventPayload::QueryCompleted {
                session_id: session,
                elapsed_ms: 0,
            })
            .unwrap();
        drop(rows_tx);
        drop(events);

        drain_query_stream(subscription, rows_rx, event_tx).await;

        let mut collected = Vec::new();
        while let Some(ev) = event_rx.recv().await {
            collected.push(ev);
        }
        let kinds: Vec<&str> = collected.iter().map(kind_of).map(Option::unwrap).collect();

        assert_eq!(
            kinds.last(),
            Some(&"Completed"),
            "terminal must be last, got {kinds:?}"
        );
        let rows_pos = kinds
            .iter()
            .position(|k| *k == "Rows")
            .expect("rows must be forwarded");
        let completed_pos = kinds.iter().position(|k| *k == "Completed").unwrap();
        assert!(
            rows_pos < completed_pos,
            "rows must precede Completed, got {kinds:?}"
        );

        // The exact consumer pattern this guards: stop at Completed and keep
        // whatever rows arrived — which must be all of them.
        let mut rows_seen = 0usize;
        for ev in &collected {
            match ev {
                QueryEvent::Rows { rows, .. } => rows_seen += rows.len(),
                QueryEvent::Completed { .. } => break,
                _ => {}
            }
        }
        assert_eq!(
            rows_seen, 1,
            "a collect-until-Completed consumer must see the rows"
        );
    }

    /// Regression: a row batch that reaches the drain BEFORE its
    /// `ResultSetStarted` lifecycle event must still be forwarded AFTER the
    /// `ResultSet` event. Consumers drop `Rows` for an unknown result-set
    /// index, so forwarding out of order silently lost the entire result
    /// body ("query completes but no rows render").
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drain_holds_rows_that_arrive_before_result_set_event() {
        let events = EventBus::new(64);
        let subscription = events.subscribe();
        let (rows_tx, rows_rx) = row_channel(8);
        let (event_tx, mut event_rx) = mpsc::channel::<QueryEvent>(32);

        let session = SessionId::new();
        let producer = tokio::spawn(async move {
            // Rows first — the drain has nothing on the lifecycle bus yet, so
            // it must stash this batch instead of forwarding it.
            rows_tx
                .send(sqlly_core::RowBatch::new(
                    session.clone(),
                    0,
                    vec![vec![CellValue::I64(1)], vec![CellValue::I64(2)]],
                ))
                .await
                .unwrap();
            // Give the drain time to consume the batch before the lifecycle
            // event exists at all.
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            events
                .publish(EventPayload::ResultSetStarted {
                    session_id: session.clone(),
                    result_set_index: 0,
                    columns: vec![ResultColumn {
                        name: "id".into(),
                        data_type: "int".into(),
                        nullable: false,
                    }],
                })
                .unwrap();
            events
                .publish(EventPayload::QueryCompleted {
                    session_id: session,
                    elapsed_ms: 1,
                })
                .unwrap();
            drop(rows_tx);
        });

        drain_query_stream(subscription, rows_rx, event_tx).await;
        producer.await.unwrap();

        let mut collected = Vec::new();
        while let Some(ev) = event_rx.recv().await {
            collected.push(ev);
        }
        let kinds: Vec<&str> = collected.iter().map(kind_of).map(Option::unwrap).collect();

        let rs_pos = kinds
            .iter()
            .position(|k| *k == "ResultSet")
            .expect("ResultSet must be present");
        let rows_pos = kinds
            .iter()
            .position(|k| *k == "Rows")
            .expect("Rows must be forwarded, not dropped");
        assert!(
            rs_pos < rows_pos,
            "ResultSet must precede Rows even when the batch arrives first (got {kinds:?})"
        );
        // The forwarded batch must carry the data.
        let rows_ok = collected
            .iter()
            .any(|ev| matches!(ev, QueryEvent::Rows { index: 0, rows } if rows.len() == 2));
        assert!(rows_ok, "the stashed batch must be flushed intact");
    }

    /// When the producer is dropped without publishing a terminal event, the
    /// drain must emit a synthetic `Cancelled` so the consumer always sees a
    /// terminal frame.
    #[tokio::test]
    async fn drain_emits_cancelled_when_no_terminal_event() {
        let events = EventBus::new(64);
        let subscription = events.subscribe();
        let (rows_tx, rows_rx) = row_channel(8);
        let (event_tx, mut event_rx) = mpsc::channel::<QueryEvent>(32);

        // Publish a start but never a completion, then drop everything.
        events
            .publish(EventPayload::QueryStarted {
                session_id: SessionId::new(),
            })
            .unwrap();
        drop(rows_tx);
        drop(events);

        drain_query_stream(subscription, rows_rx, event_tx).await;

        let mut collected = Vec::new();
        while let Some(ev) = event_rx.recv().await {
            collected.push(ev);
        }
        let kinds: Vec<&str> = collected.iter().map(kind_of).map(Option::unwrap).collect();
        assert_eq!(
            kinds,
            vec!["Started", "Cancelled"],
            "missing terminal must synthesize Cancelled"
        );
    }

    /// When the consumer drops the receiver mid-stream, the drain stops and
    /// does not panic.
    #[tokio::test]
    async fn drain_stops_cleanly_when_consumer_drops_receiver() {
        let events = EventBus::new(64);
        let subscription = events.subscribe();
        let (rows_tx, rows_rx) = row_channel(8);
        let (event_tx, event_rx) = mpsc::channel::<QueryEvent>(2);
        drop(event_rx);

        // Publish a start event so the drain tries to send and discovers the
        // closed receiver.
        events
            .publish(EventPayload::QueryStarted {
                session_id: SessionId::new(),
            })
            .unwrap();
        drop(rows_tx);
        drop(events);

        // Should complete without hanging or panicking.
        drain_query_stream(subscription, rows_rx, event_tx).await;
    }

    /// When the producer publishes an `Error` event (as it now does when
    /// `execute_on_client` returns `Err` without publishing events itself —
    /// e.g. tiberius' `simple_query` failing at `forward_to_metadata` for a
    /// SQL syntax error), the drain must emit `QueryEvent::Error`, NOT
    /// synthesize `QueryEvent::Cancelled`.
    #[tokio::test]
    async fn drain_emits_error_not_cancelled_when_error_published() {
        let events = EventBus::new(64);
        let subscription = events.subscribe();
        let (rows_tx, rows_rx) = row_channel(8);
        let (event_tx, mut event_rx) = mpsc::channel::<QueryEvent>(32);

        // Simulate the producer: no QueryStarted, just an Error published
        // (mirroring the fix path where execute_on_client returns Err and
        // the producer publishes EventPayload::Error).
        events
            .publish(EventPayload::Error {
                error: "syntax error near 'SELET'".to_string(),
            })
            .unwrap();
        drop(rows_tx);
        drop(events);

        drain_query_stream(subscription, rows_rx, event_tx).await;

        let mut collected = Vec::new();
        while let Some(ev) = event_rx.recv().await {
            collected.push(ev);
        }
        let kinds: Vec<&str> = collected.iter().map(kind_of).map(Option::unwrap).collect();
        assert_eq!(
            kinds,
            vec!["Error"],
            "Error event must produce QueryEvent::Error, not Cancelled"
        );
        if let QueryEvent::Error { message } = &collected[0] {
            assert!(
                message.contains("syntax error"),
                "error message must survive"
            );
        } else {
            panic!("first event must be Error");
        }
    }

    fn kind_of(ev: &QueryEvent) -> Option<&'static str> {
        match ev {
            QueryEvent::Started => Some("Started"),
            QueryEvent::ResultSet { .. } => Some("ResultSet"),
            QueryEvent::Rows { .. } => Some("Rows"),
            QueryEvent::TruncatedCells { .. } => Some("TruncatedCells"),
            QueryEvent::RowCount { .. } => Some("RowCount"),
            QueryEvent::RowsProgress { .. } => Some("RowsProgress"),
            QueryEvent::SpillWindow { .. } => Some("SpillWindow"),
            QueryEvent::Message { .. } => Some("Message"),
            QueryEvent::DatabaseChanged { .. } => Some("DatabaseChanged"),
            QueryEvent::Error { .. } => Some("Error"),
            QueryEvent::Completed { .. } => Some("Completed"),
            QueryEvent::Cancelled => Some("Cancelled"),
            QueryEvent::SpillManifest { .. } => Some("SpillManifest"),
        }
    }
}
