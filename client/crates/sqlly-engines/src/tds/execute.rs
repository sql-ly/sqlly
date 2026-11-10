//! The streaming execution engine: runs SQL batches on a warm [`TdsClient`],
//! publishing lifecycle events and rows exactly like SSMS's
//! "continue after error" execution model, plus the small parameterized
//! query/execute helpers used for targeted reads and writes.

use futures::TryStreamExt;
use sqlly_core::{
    CellValue, ConnectionProfile, EventBus, EventPayload, ResultColumn, RowBatch, RowSender,
    SqlParam, StudioError, StudioResult,
};
use sqlly_lang::sqllex::split_batches;
use sqlly_lang::sqlmodel::Dialect;
use std::time::Instant;
use tiberius::{ColumnData, DoneKind, QueryItem};

use crate::sql_text::{may_change_database, may_open_transaction};
use crate::tds::client::TdsClient;
use crate::tds::convert::{column_data_to_cell, column_type_to_sql_name};
use crate::tds::error::{format_server_error, is_recoverable_server_error, to_query_error};

// Batch coalescing size and partial-batch flush cadence come from the shared
// driver contract — one source of truth so the streaming cadence can never
// silently diverge between the TDS fast path and `QueryStreamer`.
use crate::driver::{ROW_BATCH_TARGET, ROW_FLUSH_INTERVAL};

/// Whether a partially-filled batch of `pending_len` rows has reached the
/// size threshold and should be flushed to the row channel now. The time-based
/// flush is handled separately by the loop's timer branch; this covers only
/// the "buffer is full" trigger so the decision is a pure, unit-testable
/// function. `target` is normally [`ROW_BATCH_TARGET`].
#[must_use]
fn size_flush_ready(pending_len: usize, target: usize) -> bool {
    pending_len >= target
}

/// SSMS's per-statement count line, e.g. `(1 row affected)` or
/// `(3 rows affected)`.
#[must_use]
fn rows_affected_text(rows: u64) -> String {
    if rows == 1 {
        "(1 row affected)".to_string()
    } else {
        format!("({rows} rows affected)")
    }
}

/// What a DONE-family token contributes to the run, decided purely from the
/// token and whether the statement it ends opened a result set.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DoneEffect {
    /// The token ends a statement that returned a result set: close that set
    /// with its received-row count now, so later sets/messages follow it.
    close_result_set: bool,
    /// Rows changed by a statement without a result set (INSERT/UPDATE/DELETE),
    /// reported as a rows-affected count exactly like the other engines.
    statement_rows: Option<u64>,
    /// SSMS's `(N rows affected)` line for the Messages pane.
    message: Option<String>,
}

/// Decide a DONE token's effect. The server sets `DONE_COUNT` only when the
/// statement has a count and `SET NOCOUNT` is off, so `rows == None` reports
/// nothing. Like SSMS, every counted statement inside a procedure
/// (`DONEINPROC`) is reported; the procedure's own closing `DONEPROC` is not,
/// so its total is never counted twice. A statement that ended in an error
/// reports no count, as in SSMS.
#[must_use]
fn done_effect(
    kind: DoneKind,
    rows: Option<u64>,
    is_error: bool,
    result_set_open: bool,
) -> DoneEffect {
    let counted = match (kind, is_error) {
        (DoneKind::DoneProc, _) | (_, true) => None,
        _ => rows,
    };
    DoneEffect {
        close_result_set: result_set_open,
        statement_rows: if result_set_open { None } else { counted },
        message: counted.map(rows_affected_text),
    }
}

/// Result of a completed [`execute_on_client`] run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecuteOutcome {
    /// The connection's current database changed during the run (a `USE` took
    /// effect). The warm client's live database no longer matches its pool key,
    /// so callers MUST discard it rather than return it to the pool.
    pub database_changed: bool,
    /// At least one batch reported a server-side SQL error (or a client-side
    /// policy refusal). Every batch still ran (SSMS "continue after error"), but
    /// the query as a whole did not fully succeed — the caller reports this as a
    /// failed request even though the healthy connection is still reusable.
    pub had_error: bool,
    /// The first meaningful error message from the run (the same text published
    /// as [`EventPayload::Error`]), so the caller can report the *actual* reason
    /// — `Msg 208 … Invalid object name 'foo'.` — instead of a generic
    /// "completed with errors". `Some` iff `had_error`.
    pub first_error: Option<String>,
    /// The post-run `DB_NAME()` confirmation could not be read (the results were
    /// already delivered, so this is not a hard failure). The client's live
    /// database is now unknown, so it MUST be discarded rather than pooled.
    pub database_confirmation_failed: bool,
    /// The session could not be returned to a clean state before check-in — an
    /// open transaction could not be rolled back, or `@@TRANCOUNT` could not be
    /// read. The client MUST be discarded so its leftover transaction/locks
    /// never enroll the next unrelated pooled query.
    pub session_dirty: bool,
}

impl ExecuteOutcome {
    /// Whether the warm client must be dropped instead of returned to the pool:
    /// a `USE` moved it off its pool key, we couldn't confirm its database, or
    /// its session state could not be cleaned (leftover open transaction).
    pub fn should_discard_client(&self) -> bool {
        self.database_changed || self.database_confirmation_failed || self.session_dirty
    }
}

/// Run a query on an already-connected client, streaming results exactly as
/// [`crate::tds::TiberiusTransport::execute_query`] does. Splitting this out lets
/// callers reuse a warm client (see [`crate::tds::open_client`]).
///
/// IMPORTANT: if this returns `Err`, or if the awaiting task is cancelled
/// mid-stream, the client's TDS protocol stream is left in an indeterminate
/// state and the client MUST be discarded rather than reused.
pub async fn execute_on_client(
    client: &mut TdsClient,
    profile: &ConnectionProfile,
    sql: &str,
    events: &EventBus,
    rows_tx: Option<&RowSender>,
) -> StudioResult<ExecuteOutcome> {
    execute_on_client_with(client, profile, sql, events, rows_tx, false).await
}

/// Like [`execute_on_client`], but leaves any transaction the batch opened
/// intact — used when the client is pinned to a tab in manual commit mode.
pub async fn execute_on_client_keep_transaction(
    client: &mut TdsClient,
    profile: &ConnectionProfile,
    sql: &str,
    events: &EventBus,
    rows_tx: Option<&RowSender>,
) -> StudioResult<ExecuteOutcome> {
    let keep_open_transaction = true;
    execute_on_client_with(client, profile, sql, events, rows_tx, keep_open_transaction).await
}

async fn execute_on_client_with(
    client: &mut TdsClient,
    profile: &ConnectionProfile,
    sql: &str,
    events: &EventBus,
    rows_tx: Option<&RowSender>,
    keep_open_transaction: bool,
) -> StudioResult<ExecuteOutcome> {
    if sql.trim().is_empty() {
        return Err(StudioError::InvalidInputMessage(
            sqlly_core::DisplayMessage::new("services.tds.errors.empty_query", "Empty query"),
        ));
    }

    let session_id = sqlly_core::SessionId::new();
    let started = Instant::now();

    // `QueryStarted` is published lazily — only once the connection has actually
    // responded (see `run_one_batch`). A warm client that went stale while idle
    // must fail its first `simple_query` having published ZERO events, so the
    // pool's stale-warm probe (`run_producer`) can tell "nothing reached the
    // server" apart from a genuine mid-query failure and retry on a fresh
    // connection.
    let mut query_started = false;

    // Split on `GO` batch separators (semicolons within a batch are preserved)
    // and run each batch on this same warm client so result-set numbering stays
    // continuous and session state (`USE`, temp tables, SET options) carries
    // across the whole editor buffer — exactly like SSMS.
    let batches = split_batches(sql, Dialect::SqlServer);
    let mut result_set_index = 0usize;
    // The first error text seen (server SQL error or policy refusal). Batches
    // keep running after it; it becomes the query's terminal error so the UI
    // reports failure while the specifics stay in the Messages stream.
    let mut first_error: Option<String> = None;

    'batches: for batch in &batches {
        // Apply the connection policy per batch so a rolled-back-transaction
        // wrap never straddles a `GO`. A client-side refusal (SELECT-only) is
        // reported inline and the run continues to the next batch.
        let policied =
            match sqlly_intel::query::safety::apply_connection_policy(profile, batch.text) {
                Ok(sql) => sql,
                Err(err) => {
                    let text = err.to_string();
                    ensure_query_started(events, &session_id, &mut query_started)?;
                    events.publish(EventPayload::Message {
                        session_id: session_id.clone(),
                        text: text.clone(),
                    })?;
                    first_error.get_or_insert(text);
                    continue 'batches;
                }
            };

        let line_shift =
            crate::tds::error::batch_line_shift(sql, batch.span.start, batch.text, &policied);
        let progress = run_one_batch(
            client,
            policied.as_str(),
            events,
            rows_tx,
            &session_id,
            result_set_index,
            &mut query_started,
            line_shift,
        )
        .await?;
        result_set_index = progress.next_rsi;
        if let Some(err) = progress.first_error {
            first_error.get_or_insert(err);
        }
        if progress.consumer_gone {
            // The row consumer went away (tab closed / torn down); stop before
            // running further batches, which for a GO script could have side
            // effects nobody is waiting on.
            break 'batches;
        }
    }

    // `USE` detection: pay for a `DB_NAME()` round-trip only when the buffer
    // *might* change the database (a plain `USE`, or a `USE` hidden inside a
    // dynamic `EXEC`/`sp_executesql` string). The server's answer is
    // authoritative regardless of quoting or conditional/dynamic USE — tiberius
    // does not surface the ENVCHANGE token to us.
    let mut database_changed = false;
    let mut database_confirmation_failed = false;
    if may_change_database(sql, Dialect::SqlServer) {
        match current_database(client).await {
            Ok(Some(current)) => {
                let prior = profile.database.as_deref().unwrap_or("");
                if !current.eq_ignore_ascii_case(prior) {
                    database_changed = true;
                    ensure_query_started(events, &session_id, &mut query_started)?;
                    events.publish(EventPayload::DatabaseChanged {
                        session_id: session_id.clone(),
                        name: current,
                    })?;
                }
            }
            Ok(None) => {}
            Err(_) => {
                // The batches already streamed their results, so a failed
                // confirmation must NOT turn a delivered run into a hard error.
                // Warn and mark the client for discard: its live database is now
                // unknown and must not be reused under a stale pool key.
                ensure_query_started(events, &session_id, &mut query_started)?;
                events.publish(EventPayload::Message {
                    session_id: session_id.clone(),
                    text: "Could not confirm the current database after execution; \
                           reconnecting before the next query."
                        .to_string(),
                })?;
                database_confirmation_failed = true;
            }
        }
    }

    // A run that produced no batch activity at all (e.g. only whitespace-noise
    // batches) still needs its `QueryStarted` before a terminal event.
    ensure_query_started(events, &session_id, &mut query_started)?;

    let had_error = first_error.is_some();

    // Before the terminal event and any re-pool: if a batch left a transaction
    // open — the classic case is a mid-batch runtime error inside
    // `BEGIN TRANSACTION … ` — roll it back so this warm client is not returned
    // to the pool mid-transaction, holding locks and silently enrolling the next
    // unrelated query. If the session can't be cleaned, flag it for discard. Skip
    // when the client is already being discarded (its stream is unknown anyway).
    let mut session_dirty = false;
    if !keep_open_transaction
        && !database_confirmation_failed
        && (had_error || may_open_transaction(sql))
    {
        session_dirty = !reset_open_transaction(client, events, &session_id).await;
    }

    // Terminal event: a run where any batch errored ends in `Error` (so the UI
    // shows a failed status and the per-batch `Msg …` lines already in the
    // stream stay as the detail), otherwise a clean `QueryCompleted`.
    let first_error_for_outcome = first_error.clone();
    match first_error {
        Some(error) => {
            events.publish(EventPayload::Error { error })?;
        }
        None => {
            events.publish(EventPayload::QueryCompleted {
                session_id,
                elapsed_ms: started.elapsed().as_millis(),
            })?;
        }
    }
    Ok(ExecuteOutcome {
        database_changed,
        had_error,
        first_error: first_error_for_outcome,
        database_confirmation_failed,
        session_dirty,
    })
}

/// Roll back any transaction the batches left open, so the warm client can be
/// safely re-pooled. Returns whether the session is clean and reusable; a
/// `false` return means the caller MUST discard the client. Emits an
/// informational message when it actually rolls something back so the leftover
/// transaction isn't silently discarded behind the user's back.
async fn reset_open_transaction(
    client: &mut TdsClient,
    events: &EventBus,
    session_id: &sqlly_core::SessionId,
) -> bool {
    let count = match query_rows_on_client(client, "SELECT @@TRANCOUNT", &[]).await {
        Ok(sets) => sets
            .first()
            .and_then(|s| s.rows.first())
            .and_then(|r| r.first())
            .and_then(|c| match c {
                CellValue::I64(n) => Some(*n),
                _ => None,
            })
            .unwrap_or(0),
        // Can't tell whether a transaction is open: discard to be safe.
        Err(_) => return false,
    };
    if count <= 0 {
        return true;
    }
    match query_rows_on_client(client, "IF @@TRANCOUNT > 0 ROLLBACK", &[]).await {
        Ok(_) => {
            let _ = events.publish(EventPayload::Message {
                session_id: session_id.clone(),
                text: "Rolled back a transaction the batch left open before \
                       reusing the connection."
                    .to_string(),
            });
            true
        }
        Err(_) => false,
    }
}

/// Publishes [`EventPayload::QueryStarted`] exactly once per execution, the
/// first time the connection is known to be responding. Deferring it this way
/// keeps the "zero events published ⟺ stale first contact" invariant the pool's
/// warm-client retry relies on.
fn ensure_query_started(
    events: &EventBus,
    session_id: &sqlly_core::SessionId,
    query_started: &mut bool,
) -> StudioResult<()> {
    if !*query_started {
        events.publish(EventPayload::QueryStarted {
            session_id: session_id.clone(),
        })?;
        *query_started = true;
    }
    Ok(())
}

/// Progress carried out of a single batch's stream.
struct BatchProgress {
    /// Next result-set index to assign (continues across batches).
    next_rsi: usize,
    /// The row consumer dropped its receiver mid-stream.
    consumer_gone: bool,
    /// Text of the first server error reported while running this batch, if any.
    first_error: Option<String>,
}

/// Streams one SQL batch on a warm client, publishing result-set metadata,
/// rows, and row counts. Server-side SQL errors (syntax, constraint,
/// permission) are reported inline as [`EventPayload::Message`] and the stream
/// keeps draining so subsequent statements/result sets in the batch still
/// surface — matching SSMS's "continue after error" behavior. Only a
/// transport/protocol failure returns `Err`, in which case the client is
/// unusable and must be discarded by the caller.
#[allow(clippy::too_many_arguments)]
async fn run_one_batch(
    client: &mut TdsClient,
    sql: &str,
    events: &EventBus,
    rows_tx: Option<&RowSender>,
    session_id: &sqlly_core::SessionId,
    rsi_start: usize,
    query_started: &mut bool,
    line_shift: i64,
) -> StudioResult<BatchProgress> {
    // `simple_query_with_server_messages().await` blocks until the server
    // begins responding, so a stale warm client fails HERE with no events yet
    // published — leave `query_started` false in that case so the pool can
    // retry fresh. Once the server responds (a stream, or even a server-side
    // error), the connection is alive and we publish `QueryStarted`. The
    // stream also yields PRINT / RAISERROR (severity <= 10) messages and each
    // statement's DONE row count (the vendored tiberius patch).
    let stream = match client.simple_query_with_server_messages(sql).await {
        Ok(stream) => {
            ensure_query_started(events, session_id, query_started)?;
            stream
        }
        // A server error before any row (e.g. a syntax error) — report and let
        // the caller move on to the next batch.
        Err(err) if is_recoverable_server_error(&err) => {
            ensure_query_started(events, session_id, query_started)?;
            let text = format_server_error(&err, line_shift);
            events.publish(EventPayload::Message {
                session_id: session_id.clone(),
                text: text.clone(),
            })?;
            return Ok(BatchProgress {
                next_rsi: rsi_start,
                consumer_gone: false,
                first_error: Some(text),
            });
        }
        Err(err) => return Err(to_query_error(err)),
    };

    let mut result_set_index = rsi_start;
    let mut current_result_set: Option<usize> = None;
    let mut current_row_count = 0u64;
    let mut pending_rows: Vec<Vec<CellValue>> = Vec::with_capacity(ROW_BATCH_TARGET);
    // The result set the buffered `pending_rows` belong to. Tracked separately
    // from `current_result_set` so the flush timer can attribute a partial
    // batch to the correct set even across a metadata boundary.
    let mut pending_rsi: Option<usize> = None;
    let mut consumer_gone = false;
    let mut first_error: Option<String> = None;
    // Whether the statement the next DONE token ends opened a result set: its
    // count is then the SELECT's row count, not a DML "rows affected".
    let mut statement_has_result_set = false;

    // The row stream is polled inside a `select!` against a flush timer, so it
    // must be pinned.
    tokio::pin!(stream);

    'items: loop {
        // Arm the flush timer only when there is a partial batch waiting;
        // otherwise there is nothing to time out and we simply block on the
        // stream. Racing the timer against the next stream item is what lets a
        // slowly-trickling result set surface rows on a steady cadence instead
        // of stalling in `pending_rows` until end-of-stream.
        let flush_timer = async {
            if pending_rows.is_empty() {
                // Never resolves: leaves the stream as the only ready branch.
                std::future::pending::<()>().await
            } else {
                tokio::time::sleep(ROW_FLUSH_INTERVAL).await
            }
        };

        let item = tokio::select! {
            biased;
            // Flush a partial batch that has aged past the interval.
            _ = flush_timer => {
                if let (Some(tx), Some(rsi)) = (rows_tx, pending_rsi)
                    && !pending_rows.is_empty()
                {
                    let rows = std::mem::take(&mut pending_rows);
                    if tx
                        .send(RowBatch::new(session_id.clone(), rsi, rows))
                        .await
                        .is_err()
                    {
                        consumer_gone = true;
                        break 'items;
                    }
                }
                continue 'items;
            }
            next = stream.try_next() => match next {
                Ok(item) => item,
                // Server SQL error: report it and KEEP draining — with error
                // handling off (the default), the server continues the batch
                // and may still emit later result sets.
                Err(err) if is_recoverable_server_error(&err) => {
                    let text = format_server_error(&err, line_shift);
                    events.publish(EventPayload::Message {
                        session_id: session_id.clone(),
                        text: text.clone(),
                    })?;
                    first_error.get_or_insert(text);
                    continue 'items;
                }
                Err(err) => return Err(to_query_error(err)),
            },
        };

        let Some(item) = item else {
            break 'items;
        };

        match item {
            QueryItem::Metadata(metadata) => {
                if let (Some(tx), Some(rsi)) = (rows_tx, pending_rsi)
                    && !pending_rows.is_empty()
                {
                    let rows = std::mem::take(&mut pending_rows);
                    if tx
                        .send(RowBatch::new(session_id.clone(), rsi, rows))
                        .await
                        .is_err()
                    {
                        consumer_gone = true;
                        break 'items;
                    }
                }
                pending_rsi = None;
                statement_has_result_set = true;
                if current_result_set.is_some() {
                    events.publish(EventPayload::RowCount {
                        session_id: session_id.clone(),
                        rows_affected: current_row_count,
                    })?;
                }
                let columns = metadata
                    .columns()
                    .iter()
                    .map(|column| ResultColumn {
                        name: column.name().to_string(),
                        data_type: column_type_to_sql_name(column.column_type()).to_string(),
                        nullable: true,
                    })
                    .collect();
                events.publish(EventPayload::ResultSetStarted {
                    session_id: session_id.clone(),
                    result_set_index,
                    columns,
                })?;
                current_result_set = Some(result_set_index);
                result_set_index += 1;
                current_row_count = 0;
            }
            QueryItem::Row(row) => {
                current_row_count += 1;
                let cells = row.into_iter().map(column_data_to_cell).collect::<Vec<_>>();
                let rsi = current_result_set.unwrap_or(0);
                match rows_tx {
                    // Bounded mpsc path: `send` awaits when the buffer is full,
                    // giving the server read loop real backpressure so rows are
                    // never dropped. If the consumer is gone, stop pulling.
                    Some(tx) => {
                        pending_rsi = Some(rsi);
                        pending_rows.push(cells);
                        if size_flush_ready(pending_rows.len(), ROW_BATCH_TARGET) {
                            let rows = std::mem::take(&mut pending_rows);
                            if tx
                                .send(RowBatch::new(session_id.clone(), rsi, rows))
                                .await
                                .is_err()
                            {
                                consumer_gone = true;
                                break 'items;
                            }
                        }
                    }
                    // Fallback for callers that did not provide a row channel:
                    // publish on the broadcast bus as before.
                    None => {
                        events.publish(EventPayload::Rows {
                            session_id: session_id.clone(),
                            result_set_index: rsi,
                            rows: vec![cells],
                        })?;
                    }
                }
            }
            // PRINT / RAISERROR (severity <= 10): the server's text as-is, the
            // way SSMS shows it, in stream order.
            QueryItem::Info(info) => {
                events.publish(EventPayload::Message {
                    session_id: session_id.clone(),
                    text: info.message().to_string(),
                })?;
            }
            QueryItem::Done(done) => {
                let effect = done_effect(
                    done.kind(),
                    done.rows(),
                    done.is_error(),
                    statement_has_result_set,
                );
                statement_has_result_set = false;
                if effect.close_result_set {
                    if let (Some(tx), Some(rsi)) = (rows_tx, pending_rsi)
                        && !pending_rows.is_empty()
                    {
                        let rows = std::mem::take(&mut pending_rows);
                        if tx
                            .send(RowBatch::new(session_id.clone(), rsi, rows))
                            .await
                            .is_err()
                        {
                            consumer_gone = true;
                            break 'items;
                        }
                    }
                    pending_rsi = None;
                    if current_result_set.take().is_some() {
                        events.publish(EventPayload::RowCount {
                            session_id: session_id.clone(),
                            rows_affected: current_row_count,
                        })?;
                    }
                }
                if let Some(rows_affected) = effect.statement_rows {
                    events.publish(EventPayload::RowCount {
                        session_id: session_id.clone(),
                        rows_affected,
                    })?;
                }
                if let Some(text) = effect.message {
                    events.publish(EventPayload::Message {
                        session_id: session_id.clone(),
                        text,
                    })?;
                }
            }
        }
    }

    if let (Some(tx), Some(rsi)) = (rows_tx, pending_rsi)
        && !pending_rows.is_empty()
        && tx
            .send(RowBatch::new(session_id.clone(), rsi, pending_rows))
            .await
            .is_err()
    {
        consumer_gone = true;
    }

    if current_result_set.is_some() {
        events.publish(EventPayload::RowCount {
            session_id: session_id.clone(),
            rows_affected: current_row_count,
        })?;
    }

    Ok(BatchProgress {
        next_rsi: result_set_index,
        consumer_gone,
        first_error,
    })
}

/// Reads the connection's current database via `SELECT DB_NAME()`.
async fn current_database(client: &mut TdsClient) -> StudioResult<Option<String>> {
    let sets = query_rows_on_client(client, "SELECT DB_NAME()", &[]).await?;
    Ok(sets
        .first()
        .and_then(|set| set.rows.first())
        .and_then(|row| row.first())
        .and_then(|cell| match cell {
            CellValue::String(name) if !name.is_empty() => Some(name.clone()),
            _ => None,
        }))
}

/// Orphan-rule bridge: `SqlParam` lives in `sqlly-core` and `ToSql` in
/// tiberius, so the binding goes through this local newtype.
struct TdsParam<'a>(&'a SqlParam);

impl tiberius::ToSql for TdsParam<'_> {
    fn to_sql(&self) -> ColumnData<'_> {
        match self.0 {
            SqlParam::Null => ColumnData::String(None),
            SqlParam::Bool(b) => ColumnData::Bit(Some(*b)),
            SqlParam::I64(n) => ColumnData::I64(Some(*n)),
            SqlParam::F64(f) => ColumnData::F64(Some(*f)),
            SqlParam::String(s) => ColumnData::String(Some(s.as_str().into())),
            SqlParam::Bytes(b) => ColumnData::Binary(Some(b.as_slice().into())),
        }
    }
}

/// Run a statement batch as a PLAIN batch (no sp_executesql wrapper).
/// Transaction-control statements (`BEGIN TRANSACTION` / `COMMIT` /
/// `ROLLBACK`) must use this: opening a transaction inside an RPC batch and
/// letting it survive the RPC raises SQL Server error 266 ("Transaction count
/// after EXECUTE indicates a mismatching number of BEGIN and COMMIT"). No
/// safety policy is applied; no result rows are returned.
pub async fn execute_batch_on_client(client: &mut TdsClient, sql: &str) -> StudioResult<()> {
    client
        .simple_query(sql)
        .await
        .map_err(to_query_error)?
        .into_results()
        .await
        .map_err(to_query_error)?;
    Ok(())
}

/// Runs a parameterized query (`@P1`, `@P2`, …) on a warm client and
/// materializes every result set. Intended for small, targeted reads
/// (metadata lookups, single-row fetches) — not for streaming user queries,
/// which go through [`execute_on_client`].
///
/// This is a low-level primitive: it does NOT apply the connection safety
/// policy (`apply_connection_policy`). Callers own policy enforcement.
pub async fn query_rows_on_client(
    client: &mut TdsClient,
    sql: &str,
    params: &[SqlParam],
) -> StudioResult<Vec<sqlly_core::MaterializedResultSet>> {
    let wrapped: Vec<TdsParam<'_>> = params.iter().map(TdsParam).collect();
    let refs: Vec<&dyn tiberius::ToSql> =
        wrapped.iter().map(|p| p as &dyn tiberius::ToSql).collect();
    let mut stream = client.query(sql, &refs).await.map_err(to_query_error)?;
    let mut sets: Vec<sqlly_core::MaterializedResultSet> = Vec::new();
    while let Some(item) = stream.try_next().await.map_err(to_query_error)? {
        match item {
            QueryItem::Metadata(metadata) => {
                sets.push(sqlly_core::MaterializedResultSet {
                    columns: metadata
                        .columns()
                        .iter()
                        .map(|column| ResultColumn {
                            name: column.name().to_string(),
                            data_type: column_type_to_sql_name(column.column_type()).to_string(),
                            nullable: true,
                        })
                        .collect(),
                    rows: Vec::new(),
                });
            }
            QueryItem::Row(row) => {
                let cells = row.into_iter().map(column_data_to_cell).collect::<Vec<_>>();
                match sets.last_mut() {
                    Some(set) => set.rows.push(cells),
                    // Defensive: a row before any metadata frame should not
                    // happen; give it a set so it is not silently dropped.
                    None => sets.push(sqlly_core::MaterializedResultSet {
                        columns: Vec::new(),
                        rows: vec![cells],
                    }),
                }
            }
            // Only yielded by opt-in streams; `query()` streams never carry them.
            QueryItem::Info(_) | QueryItem::Done(_) => {}
        }
    }
    Ok(sets)
}

/// Executes a parameterized non-query statement (UPDATE/INSERT/DELETE or a
/// control statement) on a warm client and returns the total affected-row
/// count.
///
/// Like [`query_rows_on_client`], this does NOT apply the connection safety
/// policy; callers own policy enforcement.
pub async fn execute_params_on_client(
    client: &mut TdsClient,
    sql: &str,
    params: &[SqlParam],
) -> StudioResult<u64> {
    let wrapped: Vec<TdsParam<'_>> = params.iter().map(TdsParam).collect();
    let refs: Vec<&dyn tiberius::ToSql> =
        wrapped.iter().map(|p| p as &dyn tiberius::ToSql).collect();
    let result = client.execute(sql, &refs).await.map_err(to_query_error)?;
    Ok(result.total())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_flush_triggers_only_at_or_above_target() {
        assert!(!size_flush_ready(0, 256));
        assert!(!size_flush_ready(255, 256));
        assert!(size_flush_ready(256, 256));
        assert!(size_flush_ready(257, 256));
        // A partial batch below target is never size-flushed; that case is the
        // job of the time-based flush timer.
        assert!(!size_flush_ready(1, 256));
    }

    #[test]
    fn rows_affected_text_matches_ssms_wording() {
        assert_eq!(rows_affected_text(0), "(0 rows affected)");
        assert_eq!(rows_affected_text(1), "(1 row affected)");
        assert_eq!(rows_affected_text(42), "(42 rows affected)");
    }

    #[test]
    fn dml_done_reports_rows_affected_and_message() {
        let effect = done_effect(DoneKind::Done, Some(2), false, false);
        assert_eq!(
            effect,
            DoneEffect {
                close_result_set: false,
                statement_rows: Some(2),
                message: Some("(2 rows affected)".to_string()),
            }
        );
        // A statement inside a procedure is reported the same way.
        assert_eq!(
            done_effect(DoneKind::DoneInProc, Some(1), false, false).statement_rows,
            Some(1)
        );
    }

    #[test]
    fn select_done_closes_its_result_set_without_a_dml_count() {
        let effect = done_effect(DoneKind::Done, Some(5), false, true);
        assert!(effect.close_result_set);
        assert_eq!(effect.statement_rows, None);
        assert_eq!(effect.message.as_deref(), Some("(5 rows affected)"));
    }

    #[test]
    fn nocount_error_and_doneproc_report_no_count() {
        // SET NOCOUNT ON: no DONE_COUNT bit, so no count and no message.
        assert_eq!(
            done_effect(DoneKind::Done, None, false, false),
            DoneEffect {
                close_result_set: false,
                statement_rows: None,
                message: None,
            }
        );
        // NOCOUNT still closes a SELECT's result set.
        assert!(done_effect(DoneKind::Done, None, false, true).close_result_set);
        // A failed statement reports no count.
        assert_eq!(
            done_effect(DoneKind::Done, Some(0), true, false).message,
            None
        );
        // The procedure's closing DONEPROC is not counted again.
        let proc_end = done_effect(DoneKind::DoneProc, Some(3), false, false);
        assert_eq!(proc_end.statement_rows, None);
        assert_eq!(proc_end.message, None);
    }

    #[test]
    fn flush_interval_is_short_enough_to_feel_live_but_coalesces() {
        // The interval must be small enough that a human perceives progressive
        // fill (well under ~100ms) yet large enough to coalesce bursts rather
        // than flushing per row.
        assert!(ROW_FLUSH_INTERVAL <= std::time::Duration::from_millis(100));
        assert!(ROW_FLUSH_INTERVAL >= std::time::Duration::from_millis(10));
    }
}
