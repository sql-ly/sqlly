//! Cell previews over a remote (relay) session, for every client/engine age
//! pairing:
//!
//! * new client ↔ OLD engine: the old engine rejects trailing bytes on
//!   `execute_query`, so the client must send the legacy body byte for byte
//!   and take every cell whole — grid runs and exports alike;
//! * new client ↔ NEW engine: a grid run asks for previews and gets 8 KiB
//!   previews plus a `truncated_cells` frame, surfaced as
//!   `QueryEvent::TruncatedCells` (so hydration and export warnings work
//!   remotely); an export run asks for nothing and gets every cell whole.
//!
//! (OLD client ↔ new engine is covered where the pieces are defined: a
//! legacy body decodes with no options — `sqlly-remote` layout tests — and
//! the engine sends cells whole without the option — the CLI engine's
//! `consume_query_stream_previews_cells_only_when_asked`.)
//!
//! The scripted transport plays the engine: it opens with the scenario's
//! `Ready` frame, records every request, and answers an `execute_query` the
//! way that engine generation would.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use sqlly_core::{CellValue, ConnectionProfile, SecretProvider, SecretRef};
use sqlly_remote::engine_protocol::layouts::{
    TRUNCATED_KIND_TEXT, TruncatedCellWire, WireCell, decode_execute_query, encode_execute_query,
    encode_rows_batch, encode_truncated_cells,
};
use sqlly_remote::engine_protocol::{
    BinaryWriter, CAPABILITY_QUERY_CELL_PREVIEWS, EXECUTE_QUERY_OPTION_CELL_PREVIEWS,
    FRAME_TAG_READY, FRAME_TAG_REQUEST_COMPLETED, FRAME_TAG_REQUEST_FAILED, FRAME_TAG_ROWS_BATCH,
    FRAME_TAG_TRUNCATED_CELLS, FramedTransport, PROTOCOL_VERSION, ProtocolError,
    REQUEST_TAG_EXECUTE_QUERY, RawFrame, encode_ready_body,
};
use sqlly_services::QueryEvent;
use sqlly_services::remote::{RemoteEngine, RemoteServices};
use tokio::sync::mpsc;

const FULL_LEN: usize = 20 * 1024;
const PREVIEW_LEN: usize = 8 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Generation {
    /// Predates capabilities and the options word.
    Old,
    /// Advertises `query.cell_previews`.
    New,
}

struct EngineStub {
    generation: Generation,
    queue: VecDeque<RawFrame>,
    sent: Arc<Mutex<Vec<RawFrame>>>,
    wake_tx: mpsc::UnboundedSender<()>,
    wake_rx: mpsc::UnboundedReceiver<()>,
}

fn ready_for(generation: Generation) -> RawFrame {
    match generation {
        Generation::Old => RawFrame::new(FRAME_TAG_READY, PROTOCOL_VERSION.to_le_bytes().to_vec()),
        Generation::New => RawFrame::new(
            FRAME_TAG_READY,
            encode_ready_body(PROTOCOL_VERSION, &[CAPABILITY_QUERY_CELL_PREVIEWS]).unwrap(),
        ),
    }
}

fn id_only(id: &str) -> Vec<u8> {
    BinaryWriter::new().string(id).done()
}

#[async_trait]
impl FramedTransport for EngineStub {
    async fn recv(&mut self) -> Result<Option<RawFrame>, ProtocolError> {
        loop {
            if let Some(frame) = self.queue.pop_front() {
                return Ok(Some(frame));
            }
            if self.wake_rx.recv().await.is_none() {
                return Ok(None);
            }
        }
    }

    async fn send(&mut self, frame: &RawFrame) -> Result<(), ProtocolError> {
        self.sent.lock().unwrap().push(frame.clone());
        assert_eq!(frame.tag, REQUEST_TAG_EXECUTE_QUERY);
        let wire = decode_execute_query(&frame.body).expect("a body the new decoder reads");
        let id = wire.id.clone();
        // An old engine's decoder ends at the connection key and rejects any
        // trailing byte.
        let legacy = encode_execute_query(
            &wire.id,
            &wire.connection_json,
            &wire.sql,
            wire.row_buffer,
            wire.connection_key.as_deref(),
        );
        if self.generation == Generation::Old && frame.body != legacy {
            let body = BinaryWriter::new()
                .string(&id)
                .string("decode execute_query body: trailing byte(s)")
                .done();
            self.queue
                .push_back(RawFrame::new(FRAME_TAG_REQUEST_FAILED, body));
        } else {
            let previews = self.generation == Generation::New
                && wire.options & EXECUTE_QUERY_OPTION_CELL_PREVIEWS != 0;
            let cell_len = if previews { PREVIEW_LEN } else { FULL_LEN };
            let rows = vec![
                vec![WireCell::I64(1), WireCell::Text("short".into())],
                vec![WireCell::I64(2), WireCell::Text("x".repeat(cell_len))],
            ];
            self.queue.push_back(RawFrame::new(
                FRAME_TAG_ROWS_BATCH,
                encode_rows_batch(&id, 0, &rows),
            ));
            if previews {
                self.queue.push_back(RawFrame::new(
                    FRAME_TAG_TRUNCATED_CELLS,
                    encode_truncated_cells(
                        &id,
                        0,
                        &[TruncatedCellWire {
                            row: 1,
                            column: 1,
                            total_len: FULL_LEN as u64,
                            kind: TRUNCATED_KIND_TEXT,
                        }],
                    ),
                ));
            }
            self.queue
                .push_back(RawFrame::new(FRAME_TAG_REQUEST_COMPLETED, id_only(&id)));
        }
        let _ = self.wake_tx.send(());
        Ok(())
    }
}

async fn connect(
    generation: Generation,
) -> (RemoteServices<EngineStub>, Arc<Mutex<Vec<RawFrame>>>) {
    let (wake_tx, wake_rx) = mpsc::unbounded_channel();
    let sent = Arc::new(Mutex::new(Vec::new()));
    let stub = EngineStub {
        generation,
        queue: VecDeque::from([ready_for(generation)]),
        sent: Arc::clone(&sent),
        wake_tx,
        wake_rx,
    };
    let engine = RemoteEngine::new(stub);
    engine
        .wait_for_capabilities(Duration::from_secs(5))
        .await
        .expect("Ready arrives");
    (RemoteServices::from_engine(engine), sent)
}

fn profile() -> ConnectionProfile {
    ConnectionProfile::sql_login(
        "previews",
        "localhost",
        "sa",
        SecretRef {
            provider: SecretProvider::InMemory,
            key: "pw".to_string(),
        },
    )
}

/// Every `(long cell length, truncation reports)` a run produced.
async fn drain(mut handle: sqlly_services::QueryHandle) -> (usize, Vec<(usize, usize, u64)>) {
    let mut long = 0usize;
    let mut cut = Vec::new();
    while let Some(event) = handle.events.recv().await {
        match event {
            QueryEvent::Rows { rows, .. } => {
                for row in rows {
                    if let Some(CellValue::String(s)) = row.get(1)
                        && s.len() > long
                    {
                        long = s.len();
                    }
                }
            }
            QueryEvent::TruncatedCells { cells, .. } => {
                cut.extend(cells.iter().map(|c| (c.row, c.column, c.total_len)));
            }
            QueryEvent::Error { message } => panic!("query failed: {message}"),
            QueryEvent::Completed { .. } => break,
            _ => {}
        }
    }
    (long, cut)
}

fn sent_options(sent: &Arc<Mutex<Vec<RawFrame>>>) -> Vec<u32> {
    sent.lock()
        .unwrap()
        .iter()
        .map(|f| decode_execute_query(&f.body).unwrap().options)
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn old_engine_gets_legacy_bodies_and_sends_cells_whole() {
    let (remote, sent) = connect(Generation::Old).await;
    let queries = &remote.services().queries;
    let grid = queries
        .execute_in_session(&profile(), Some("tab"), "SELECT 1", 8)
        .await
        .unwrap();
    assert_eq!(drain(grid).await, (FULL_LEN, Vec::new()));
    let export = queries
        .execute_full_values(&profile(), None, "SELECT 1", 8)
        .await
        .unwrap();
    assert_eq!(drain(export).await, (FULL_LEN, Vec::new()));
    assert_eq!(
        sent_options(&sent),
        vec![0, 0],
        "no options word ever reaches an old engine"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn new_engine_previews_grid_runs_and_sends_exports_whole() {
    let (remote, sent) = connect(Generation::New).await;
    let queries = &remote.services().queries;
    let grid = queries
        .execute_in_session(&profile(), Some("tab"), "SELECT 1", 8)
        .await
        .unwrap();
    assert_eq!(
        drain(grid).await,
        (PREVIEW_LEN, vec![(1, 1, FULL_LEN as u64)]),
        "the grid gets the preview and learns what was cut"
    );
    let export = queries
        .execute_full_values(&profile(), None, "SELECT 1", 8)
        .await
        .unwrap();
    assert_eq!(
        drain(export).await,
        (FULL_LEN, Vec::new()),
        "exports get every cell whole"
    );
    assert_eq!(
        sent_options(&sent),
        vec![EXECUTE_QUERY_OPTION_CELL_PREVIEWS, 0]
    );
}
