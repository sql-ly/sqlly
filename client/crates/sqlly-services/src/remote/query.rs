//! Remote [`QueryExecutor`]: streams `REQUEST_TAG_EXECUTE_QUERY` over the shared
//! engine transport, maps the `event` / `rows_batch` / terminal frames back onto
//! the native [`QueryEvent`] the GPUI grid and the metadata service consume, and
//! implements `cancel` via `REQUEST_TAG_CANCEL`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};

use crate::{QueryEvent, QueryExecutor, QueryHandle};
use async_trait::async_trait;
use sqlly_core::{CellValue, ConnectionProfile, Event, EventPayload, StudioResult};
use sqlly_remote::engine_protocol::{
    BinaryReader, BinaryWriter, FRAME_TAG_EVENT, FRAME_TAG_PONG, FRAME_TAG_REQUEST_CANCELLED,
    FRAME_TAG_REQUEST_COMPLETED, FRAME_TAG_REQUEST_FAILED, FRAME_TAG_REQUEST_STARTED,
    FRAME_TAG_ROWS_BATCH, FramedTransport, REQUEST_TAG_CANCEL, RawFrame, is_terminal_frame_tag,
};
use tokio::sync::{Notify, mpsc};

use crate::remote::engine::{RemoteEngine, decode_failed_error, engine_connection_json};

/// Remote query executor bound to one [`RemoteEngine`] session.
pub struct RemoteQueryExecutor<T> {
    engine: RemoteEngine<T>,
    /// Cancel signals keyed by `task_id` (the engine request id). A running
    /// query's drain task selects on its `Notify`; [`cancel`](QueryExecutor::cancel)
    /// fires it without needing the transport lock.
    cancels: Arc<StdMutex<HashMap<String, Arc<Notify>>>>,
}

impl<T> Clone for RemoteQueryExecutor<T> {
    fn clone(&self) -> Self {
        Self {
            engine: self.engine.clone(),
            cancels: Arc::clone(&self.cancels),
        }
    }
}

impl<T: FramedTransport + 'static> RemoteQueryExecutor<T> {
    pub fn new(engine: RemoteEngine<T>) -> Self {
        Self {
            engine,
            cancels: Arc::new(StdMutex::new(HashMap::new())),
        }
    }

    /// Send one `execute_query` with `options` (non-zero only when the engine
    /// advertised the matching capability) and drain its frames.
    fn start(
        &self,
        profile: &ConnectionProfile,
        session: Option<&str>,
        sql: &str,
        row_buffer: usize,
        options: u32,
    ) -> StudioResult<QueryHandle> {
        let buffer = row_buffer.max(1);
        let conn_json = engine_connection_json(profile)?;
        if profile.enforce_read_only {
            // The wrap itself runs on the remote engine's drivers; the flag
            // rides the connection payload. An engine older than the flag
            // ignores it and applies the SELECT-only parser gate alone.
            tracing::debug!(
                "read-only enforcement forwarded to the remote engine as a connection flag; \
                 engines that predate it fall back to the parser gate"
            );
        }
        let (event_tx, event_rx) = mpsc::channel::<QueryEvent>(buffer);

        // Allocate the id, then register + queue the request through the demux.
        // The drain task and cancel() both address this same id.
        let id = self.engine.next_request_id();
        let body = sqlly_remote::engine_protocol::layouts::encode_execute_query_with_options(
            &id,
            &conn_json,
            sql,
            Some(buffer as u32),
            session,
            options,
        );
        let frames = self.engine.open(
            sqlly_remote::engine_protocol::REQUEST_TAG_EXECUTE_QUERY,
            &id,
            body,
        )?;

        let notify = Arc::new(Notify::new());
        self.cancels
            .lock()
            .expect("cancel map poisoned")
            .insert(id.clone(), Arc::clone(&notify));

        let cancels = Arc::clone(&self.cancels);
        let engine = self.engine.clone();
        let drain_id = id.clone();
        crate::remote::rt::spawn(async move {
            drain_query(engine, drain_id.clone(), frames, event_tx, notify).await;
            cancels
                .lock()
                .expect("cancel map poisoned")
                .remove(&drain_id);
        });

        Ok(QueryHandle {
            task_id: id,
            events: event_rx,
        })
    }
}

#[async_trait]
impl<T: FramedTransport + 'static> QueryExecutor for RemoteQueryExecutor<T> {
    async fn execute(
        &self,
        profile: &ConnectionProfile,
        sql: &str,
        row_buffer: usize,
    ) -> StudioResult<QueryHandle> {
        self.execute_in_session(profile, None, sql, row_buffer)
            .await
    }

    /// Grid runs ask an engine that advertises
    /// `CAPABILITY_QUERY_CELL_PREVIEWS` for 8 KiB previews plus
    /// `TruncatedCells` reports, the same as a direct run; an older engine
    /// sends every cell whole, as it always has.
    async fn execute_in_session(
        &self,
        profile: &ConnectionProfile,
        session: Option<&str>,
        sql: &str,
        row_buffer: usize,
    ) -> StudioResult<QueryHandle> {
        let options = if self
            .engine
            .supports(sqlly_remote::engine_protocol::CAPABILITY_QUERY_CELL_PREVIEWS)
        {
            sqlly_remote::engine_protocol::EXECUTE_QUERY_OPTION_CELL_PREVIEWS
        } else {
            0
        };
        self.start(profile, session, sql, row_buffer, options)
    }

    /// Exports: no options, so every engine — old or new — sends every cell
    /// whole. (Rows still stream as they arrive; a single row larger than
    /// the 16 MiB frame limit fails the query, as it always has.)
    async fn execute_full_values(
        &self,
        profile: &ConnectionProfile,
        session: Option<&str>,
        sql: &str,
        row_buffer: usize,
    ) -> StudioResult<QueryHandle> {
        self.start(profile, session, sql, row_buffer, 0)
    }

    async fn cancel(&self, task_id: &str) -> StudioResult<()> {
        // Just wake the drain task; it issues the REQUEST_TAG_CANCEL itself so
        // the cancel is sequenced relative to the frames it is already draining.
        if let Some(notify) = self
            .cancels
            .lock()
            .expect("cancel map poisoned")
            .get(task_id)
            .cloned()
        {
            notify.notify_one();
        }
        Ok(())
    }
}

/// Drain the per-id frame channel until the engine's terminal frame arrives,
/// mapping each frame to a [`QueryEvent`]. On the first cancel notification the
/// loop queues a `REQUEST_TAG_CANCEL` for `id` (the response arrives on this
/// same channel) and keeps draining until the terminal frame. Exactly one
/// terminal [`QueryEvent`] is emitted.
async fn drain_query<T: FramedTransport + 'static>(
    engine: RemoteEngine<T>,
    id: String,
    mut frames: mpsc::Receiver<RawFrame>,
    event_tx: mpsc::Sender<QueryEvent>,
    notify: Arc<Notify>,
) {
    let mut current_rs = 0usize;
    let mut terminal_sent = false;
    let mut cancel_requested = false;
    // An engine that predates ordered streaming can send a small result's
    // rows ahead of its `ResultSetStarted`, or `QueryCompleted` ahead of the
    // rows; the results view drops rows for a set it hasn't seen, which
    // showed as headers with zero rows. Hold early rows until their set
    // starts and the terminal event until the terminal frame.
    let mut started_sets: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let mut pending_rows: Vec<(usize, Vec<Vec<CellValue>>)> = Vec::new();
    let mut deferred_terminal: Option<QueryEvent> = None;

    'drain: loop {
        // Take the next frame for `id`. Until a cancel is requested, race the
        // channel against the cancel notification so cancel() is prompt.
        let frame = if cancel_requested {
            frames.recv().await
        } else {
            tokio::select! {
                biased;
                _ = notify.notified() => {
                    // Queue the cancel; its outcome (request_cancelled) returns
                    // on this same id channel.
                    let cancel_id = engine.next_request_id();
                    let body = BinaryWriter::new()
                        .u16_le(sqlly_remote::engine_protocol::BINARY_REQUEST_LAYOUT_VERSION)
                        .string(&cancel_id)
                        .string(&id)
                        .done();
                    let _ = engine.send_raw(REQUEST_TAG_CANCEL, body);
                    cancel_requested = true;
                    continue;
                }
                frame = frames.recv() => frame,
            }
        };

        let frame = match frame {
            Some(frame) => frame,
            // Channel closed: the demux task tore down (transport closed) before
            // a terminal frame. Fall through to the terminal-guarantee below.
            None => break,
        };

        // The response stream ends at the engine's terminal frame; stop after
        // handling it so the channel drain does not block forever.
        let is_terminal_frame = is_terminal_frame_tag(frame.tag);
        if is_terminal_frame {
            // Rows whose set never started (a producer that died in
            // between): deliver them rather than lose them, before the end.
            for (index, rows) in std::mem::take(&mut pending_rows) {
                if event_tx
                    .send(QueryEvent::Rows { index, rows })
                    .await
                    .is_err()
                {
                    break 'drain;
                }
            }
        }

        match frame.tag {
            FRAME_TAG_EVENT => {
                if let Some(events) = map_event_frame(&frame.body, &mut current_rs) {
                    for ev in events {
                        match ev {
                            QueryEvent::Completed { .. }
                            | QueryEvent::Error { .. }
                            | QueryEvent::Cancelled => {
                                if deferred_terminal.is_none() {
                                    deferred_terminal = Some(ev);
                                }
                            }
                            QueryEvent::ResultSet { index, .. } => {
                                started_sets.insert(index);
                                if event_tx.send(ev).await.is_err() {
                                    break 'drain;
                                }
                                let (ready, held): (Vec<_>, Vec<_>) =
                                    std::mem::take(&mut pending_rows)
                                        .into_iter()
                                        .partition(|(i, _)| *i == index);
                                pending_rows = held;
                                for (index, rows) in ready {
                                    if event_tx
                                        .send(QueryEvent::Rows { index, rows })
                                        .await
                                        .is_err()
                                    {
                                        break 'drain;
                                    }
                                }
                            }
                            QueryEvent::Rows { index, rows } if !started_sets.contains(&index) => {
                                pending_rows.push((index, rows));
                            }
                            ev => {
                                if event_tx.send(ev).await.is_err() {
                                    break 'drain;
                                }
                            }
                        }
                    }
                }
            }
            sqlly_remote::engine_protocol::FRAME_TAG_TRUNCATED_CELLS => {
                if let Some((index, cells)) = decode_truncated_cells(&frame.body)
                    && event_tx
                        .send(QueryEvent::TruncatedCells { index, cells })
                        .await
                        .is_err()
                {
                    break 'drain;
                }
            }
            FRAME_TAG_ROWS_BATCH => {
                if let Some((index, rows)) = decode_rows_batch(&frame.body) {
                    if !started_sets.contains(&index) {
                        pending_rows.push((index, rows));
                    } else if event_tx
                        .send(QueryEvent::Rows { index, rows })
                        .await
                        .is_err()
                    {
                        break 'drain;
                    }
                }
            }
            FRAME_TAG_REQUEST_COMPLETED => {
                if !terminal_sent {
                    let terminal = deferred_terminal
                        .take()
                        .unwrap_or(QueryEvent::Completed { elapsed_ms: 0 });
                    let _ = event_tx.send(terminal).await;
                    terminal_sent = true;
                }
            }
            FRAME_TAG_REQUEST_FAILED => {
                // A failure frame wins over a deferred `QueryCompleted`, but
                // an engine-reported error event carries the better message.
                if let Some(error @ QueryEvent::Error { .. }) = deferred_terminal.take() {
                    let _ = event_tx.send(error).await;
                    terminal_sent = true;
                }
                if !terminal_sent {
                    let _ = event_tx
                        .send(QueryEvent::Error {
                            message: decode_failed_error(&frame.body),
                        })
                        .await;
                    terminal_sent = true;
                }
            }
            FRAME_TAG_REQUEST_CANCELLED => {
                deferred_terminal = None;
                if !terminal_sent {
                    let _ = event_tx.send(QueryEvent::Cancelled).await;
                    terminal_sent = true;
                }
            }
            // request_started, pong, and anything else on this id carry no
            // QueryEvent.
            FRAME_TAG_REQUEST_STARTED | FRAME_TAG_PONG => {}
            _ => {}
        }

        if is_terminal_frame {
            break;
        }
    }

    // The transport closed before a terminal frame: an event already said
    // how the query ended, so report that rather than a transport error.
    if !terminal_sent && let Some(terminal) = deferred_terminal {
        for (index, rows) in std::mem::take(&mut pending_rows) {
            let _ = event_tx.send(QueryEvent::Rows { index, rows }).await;
        }
        let _ = event_tx.send(terminal).await;
        terminal_sent = true;
    }

    // Stop routing frames for this id, then guarantee exactly one terminal
    // event even if the channel closed before the engine sent one. Reaching
    // here without a terminal frame means the transport closed mid-query (a
    // real cancellation is acknowledged with `request_cancelled` above), so
    // surface the close reason — the typed relay copy when the session died on
    // an error, a generic closed message on a clean EOF — never a fabricated
    // cancellation.
    engine.unregister(&id);
    if !terminal_sent {
        let message = engine
            .close_reason_message()
            .unwrap_or_else(|| "the remote session closed before the query finished".to_string());
        let _ = event_tx.send(QueryEvent::Error { message }).await;
    }
}

/// Decode an `event` frame body (`[id][json_len][json]`) and map the
/// `sqlly_core::Event` payload onto zero or more [`QueryEvent`]s, tracking the
/// current result-set index the way the in-process drain does.
fn map_event_frame(body: &[u8], current_rs: &mut usize) -> Option<Vec<QueryEvent>> {
    let mut r = BinaryReader::new(body);
    r.read_string("id").ok()?;
    let json = r.read_string("payload").ok()?;
    let event: Event = serde_json::from_str(&json).ok()?;
    let out = match event.payload {
        EventPayload::QueryStarted { .. } => vec![QueryEvent::Started],
        EventPayload::ResultSetStarted {
            result_set_index,
            columns,
            ..
        } => {
            *current_rs = result_set_index;
            vec![QueryEvent::ResultSet {
                index: result_set_index,
                columns,
            }]
        }
        EventPayload::Rows {
            result_set_index,
            rows,
            ..
        } => vec![QueryEvent::Rows {
            index: result_set_index,
            rows,
        }],
        EventPayload::RowCount { rows_affected, .. } => vec![QueryEvent::RowCount {
            index: *current_rs,
            rows_affected,
        }],
        EventPayload::Message { text, .. } => vec![QueryEvent::Message { text }],
        EventPayload::DatabaseChanged { name, .. } => {
            vec![QueryEvent::DatabaseChanged { name }]
        }
        EventPayload::QueryCompleted { elapsed_ms, .. } => {
            vec![QueryEvent::Completed { elapsed_ms }]
        }
        EventPayload::QueryCancelled { .. } => vec![QueryEvent::Cancelled],
        EventPayload::Error { error } => vec![QueryEvent::Error { message: error }],
        // Connection/explorer lifecycle events never appear on the query path.
        _ => Vec::new(),
    };
    Some(out)
}

/// Decode a `truncated_cells` body into the native preview report. A
/// malformed body is dropped (the rows it describes still arrived).
fn decode_truncated_cells(body: &[u8]) -> Option<(usize, Vec<crate::query::TruncatedCell>)> {
    use sqlly_remote::engine_protocol::layouts::{
        TRUNCATED_KIND_BYTES, decode_truncated_cells as decode_wire,
    };
    let wire = decode_wire(body).ok()?;
    let cells = wire
        .cells
        .into_iter()
        .map(|cell| crate::query::TruncatedCell {
            row: cell.row as usize,
            column: cell.column as usize,
            total_len: cell.total_len,
            kind: if cell.kind == TRUNCATED_KIND_BYTES {
                crate::query::TruncatedKind::Bytes
            } else {
                crate::query::TruncatedKind::Text
            },
        })
        .collect();
    Some((wire.result_set_index as usize, cells))
}

/// Decode a `rows_batch` body into `(result_set_index, rows)`. The byte
/// layout is protocol-owned; this maps its wire cells onto `CellValue`.
fn decode_rows_batch(body: &[u8]) -> Option<(usize, Vec<Vec<CellValue>>)> {
    use sqlly_remote::engine_protocol::layouts::{WireCell, decode_rows_batch as decode_wire};
    let wire = decode_wire(body).ok()?;
    let rows = wire
        .rows
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|cell| match cell {
                    WireCell::Null => CellValue::Null,
                    WireCell::Bool(b) => CellValue::Bool(b),
                    WireCell::I64(v) => CellValue::I64(v),
                    WireCell::F64(v) => CellValue::F64(v),
                    WireCell::Text(s) => CellValue::String(s),
                    WireCell::Bytes(b) => CellValue::Bytes(b),
                })
                .collect()
        })
        .collect();
    Some((wire.result_set_index as usize, rows))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use async_trait::async_trait;
    use sqlly_core::{SecretProvider, SecretRef};
    use sqlly_remote::engine_protocol::ProtocolError;

    fn profile() -> ConnectionProfile {
        ConnectionProfile::sql_login(
            "p",
            "localhost",
            "sa",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: "pw".to_string(),
            },
        )
    }

    /// A transport whose read side dies with a flattened mid-session relay
    /// error — but only after the query request has been written, so the
    /// failure lands on the in-flight drain rather than at `execute`.
    struct RelayDropTransport {
        sent_tx: mpsc::UnboundedSender<()>,
        sent_rx: mpsc::UnboundedReceiver<()>,
    }

    impl RelayDropTransport {
        fn new() -> Self {
            let (sent_tx, sent_rx) = mpsc::unbounded_channel();
            Self { sent_tx, sent_rx }
        }
    }

    #[async_trait]
    impl FramedTransport for RelayDropTransport {
        async fn recv(&mut self) -> Result<Option<RawFrame>, ProtocolError> {
            let _ = self.sent_rx.recv().await;
            Err(ProtocolError::decode(
                "relay reported error: DestOffline".to_string(),
            ))
        }
        async fn send(&mut self, _frame: &RawFrame) -> Result<(), ProtocolError> {
            let _ = self.sent_tx.send(());
            Ok(())
        }
    }

    /// A query whose relay session dies mid-stream must terminate with an
    /// `Error` event carrying the relay code's copy — not read as `Cancelled`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn relay_drop_mid_query_terminates_with_the_relay_copy() {
        let executor = RemoteQueryExecutor::new(RemoteEngine::new(RelayDropTransport::new()));
        let handle = executor
            .execute(&profile(), "SELECT 1", 8)
            .await
            .expect("the request is queued before the transport dies");

        let mut events = handle.events;
        let expected = sqlly_remote::transport::RemoteError::relay_user_message(
            sqlly_wire::RelayErrorCode::DestOffline,
        );
        let terminal = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match events.recv().await {
                    Some(QueryEvent::Error { message }) => break Some(message),
                    Some(_) => continue,
                    None => break None,
                }
            }
        })
        .await
        .expect("the drain must emit a terminal event");
        assert_eq!(terminal.as_deref(), Some(expected));
    }

    /// A transport that FIN-closes cleanly after the query request is written:
    /// no error, just `Ok(None)` at a frame boundary.
    struct CleanEofTransport {
        sent_tx: mpsc::UnboundedSender<()>,
        sent_rx: mpsc::UnboundedReceiver<()>,
    }

    impl CleanEofTransport {
        fn new() -> Self {
            let (sent_tx, sent_rx) = mpsc::unbounded_channel();
            Self { sent_tx, sent_rx }
        }
    }

    #[async_trait]
    impl FramedTransport for CleanEofTransport {
        async fn recv(&mut self) -> Result<Option<RawFrame>, ProtocolError> {
            let _ = self.sent_rx.recv().await;
            Ok(None)
        }
        async fn send(&mut self, _frame: &RawFrame) -> Result<(), ProtocolError> {
            let _ = self.sent_tx.send(());
            Ok(())
        }
    }

    /// A session that closes cleanly mid-query (bridge shut down without an
    /// error frame) must terminate with a session-closed `Error` — the user's
    /// query did not finish and they never cancelled it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn clean_eof_mid_query_terminates_with_session_closed_not_cancelled() {
        let executor = RemoteQueryExecutor::new(RemoteEngine::new(CleanEofTransport::new()));
        let handle = executor
            .execute(&profile(), "SELECT 1", 8)
            .await
            .expect("the request is queued before the transport closes");

        let mut events = handle.events;
        let terminal = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match events.recv().await {
                    Some(QueryEvent::Error { message }) => break Ok(message),
                    Some(QueryEvent::Cancelled) => break Err("misreported as Cancelled"),
                    Some(_) => continue,
                    None => break Err("stream ended without a terminal event"),
                }
            }
        })
        .await
        .expect("the drain must emit a terminal event")
        .expect("clean EOF must surface an error, not a cancellation");
        assert!(terminal.contains("closed"), "got: {terminal}");
        assert!(!terminal.contains("relay"), "got: {terminal}");
    }

    /// Replays the frames an engine without ordered streaming could send for
    /// a one-batch result: the rows and `QueryCompleted` ahead of the
    /// `ResultSetStarted`. The results view drops rows for a set it hasn't
    /// seen, so the drain must hand them over after the header and finish
    /// with the terminal event (the browser's "headers, zero rows" re-runs).
    struct OutOfOrderTransport {
        resp_rx: mpsc::UnboundedReceiver<RawFrame>,
        resp_tx: Option<mpsc::UnboundedSender<RawFrame>>,
    }

    impl OutOfOrderTransport {
        fn new() -> Self {
            let (resp_tx, resp_rx) = mpsc::unbounded_channel();
            Self {
                resp_rx,
                resp_tx: Some(resp_tx),
            }
        }
    }

    fn event_frame(id: &str, payload: sqlly_core::EventPayload) -> RawFrame {
        let json = serde_json::to_string(&Event::new(payload)).unwrap();
        RawFrame::new(
            FRAME_TAG_EVENT,
            BinaryWriter::new().string(id).string(&json).done(),
        )
    }

    #[async_trait]
    impl FramedTransport for OutOfOrderTransport {
        async fn recv(&mut self) -> Result<Option<RawFrame>, ProtocolError> {
            Ok(self.resp_rx.recv().await)
        }
        async fn send(&mut self, frame: &RawFrame) -> Result<(), ProtocolError> {
            let Some(tx) = self.resp_tx.take() else {
                return Ok(());
            };
            let id = "req-1";
            assert_eq!(
                frame.tag,
                sqlly_remote::engine_protocol::REQUEST_TAG_EXECUTE_QUERY
            );
            let session = sqlly_core::SessionId::new();
            let header = || BinaryWriter::new().string(id).done();
            for frame in [
                RawFrame::new(FRAME_TAG_REQUEST_STARTED, header()),
                RawFrame::new(
                    FRAME_TAG_ROWS_BATCH,
                    sqlly_remote::engine_protocol::layouts::encode_rows_batch(
                        id,
                        0,
                        &[vec![sqlly_remote::engine_protocol::layouts::WireCell::I64(
                            7,
                        )]],
                    ),
                ),
                event_frame(
                    id,
                    EventPayload::QueryCompleted {
                        session_id: session.clone(),
                        elapsed_ms: 3,
                    },
                ),
                event_frame(
                    id,
                    EventPayload::ResultSetStarted {
                        session_id: session.clone(),
                        result_set_index: 0,
                        columns: vec![sqlly_core::ResultColumn {
                            name: "id".into(),
                            data_type: "int".into(),
                            nullable: false,
                        }],
                    },
                ),
                RawFrame::new(FRAME_TAG_REQUEST_COMPLETED, header()),
            ] {
                let _ = tx.send(frame);
            }
            Ok(())
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rows_racing_ahead_of_their_result_set_still_arrive() {
        let executor = RemoteQueryExecutor::new(RemoteEngine::new(OutOfOrderTransport::new()));
        let handle = executor
            .execute(&profile(), "SELECT 7 AS id", 8)
            .await
            .expect("queued");
        let mut events = handle.events;
        let mut seen = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(event) = events.recv().await {
                seen.push(match event {
                    QueryEvent::ResultSet { index, .. } => format!("set {index}"),
                    QueryEvent::Rows { index, rows } => format!("rows {index}x{}", rows.len()),
                    QueryEvent::Completed { elapsed_ms } => format!("completed {elapsed_ms}"),
                    other => format!("{other:?}"),
                });
            }
        })
        .await
        .expect("the drain finishes");
        assert_eq!(seen, ["set 0", "rows 0x1", "completed 3"]);
    }
}
