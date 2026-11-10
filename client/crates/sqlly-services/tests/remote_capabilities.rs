//! Engine capability negotiation over a remote (relay) session, for every
//! client/engine age pairing the field will see:
//!
//! * new client ↔ OLD engine: the `Ready` frame is the bare `[u32 version]`,
//!   so the client must treat every capability as absent and refuse, before
//!   sending, anything that engine would reject with "unknown variant";
//! * new client ↔ NEW engine: the advertised list unlocks those writes;
//! * a malformed list, or no `Ready` yet, reads as "nothing advertised".
//!
//! (OLD client ↔ new engine is covered where the body is defined: the new
//! `Ready` body keeps the legacy `u32` prefix byte for byte.)
//!
//! The scripted transport plays the engine: it opens with the scenario's
//! `Ready` frame, records every request the client actually sends, and
//! answers each with a `request_failed` so no call hangs.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use sqlly_core::{CellValue, ConnectionProfile, SecretProvider, SecretRef, StudioError};
use sqlly_remote::engine_protocol::{
    BinaryWriter, CAPABILITY_ROW_EDIT_APPLY_BATCH, CAPABILITY_ROW_EDIT_FK_LABELS,
    CAPABILITY_ROW_EDIT_SERVER_EXPRESSIONS, FRAME_TAG_READY, FRAME_TAG_REQUEST_FAILED,
    FramedTransport, PROTOCOL_VERSION, ProtocolError, REQUEST_TAG_ROW_EDIT, RawFrame,
    encode_ready_body,
};
use sqlly_services::remote::{RemoteEngine, RemoteServices};
use sqlly_services::row_editing::{
    BatchRequest, ColumnExpression, FkLabelRequest, InsertRowRequest, StagedBatch, StagedOp,
    TableTarget, UpdateRowRequest,
};
use tokio::sync::mpsc;

struct EngineStub {
    queue: VecDeque<RawFrame>,
    sent: Arc<Mutex<Vec<RawFrame>>>,
    wake_tx: mpsc::UnboundedSender<()>,
    wake_rx: mpsc::UnboundedReceiver<()>,
}

impl EngineStub {
    /// `ready` is the engine's opening frame, or `None` for an engine that has
    /// not announced itself yet.
    fn new(ready: Option<RawFrame>) -> (Self, Arc<Mutex<Vec<RawFrame>>>) {
        let (wake_tx, wake_rx) = mpsc::unbounded_channel();
        let sent = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                queue: ready.into_iter().collect(),
                sent: Arc::clone(&sent),
                wake_tx,
                wake_rx,
            },
            sent,
        )
    }
}

/// The request id in a `[u16 layout][u32 len][id]…` request body.
fn request_id_of(frame: &RawFrame) -> String {
    let body = &frame.body[2..];
    let len = u32::from_le_bytes(body[..4].try_into().expect("id length")) as usize;
    String::from_utf8(body[4..4 + len].to_vec()).expect("utf8 id")
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
        let id = request_id_of(frame);
        let body = BinaryWriter::new()
            .string(&id)
            .string("stub engine: request reached the wire")
            .done();
        self.queue
            .push_back(RawFrame::new(FRAME_TAG_REQUEST_FAILED, body));
        let _ = self.wake_tx.send(());
        Ok(())
    }
}

fn legacy_ready() -> RawFrame {
    RawFrame::new(FRAME_TAG_READY, PROTOCOL_VERSION.to_le_bytes().to_vec())
}

fn current_ready() -> RawFrame {
    RawFrame::new(
        FRAME_TAG_READY,
        encode_ready_body(
            PROTOCOL_VERSION,
            &[
                CAPABILITY_ROW_EDIT_SERVER_EXPRESSIONS,
                CAPABILITY_ROW_EDIT_FK_LABELS,
                CAPABILITY_ROW_EDIT_APPLY_BATCH,
            ],
        )
        .unwrap(),
    )
}

fn batch_of(ops: Vec<StagedOp>) -> BatchRequest {
    BatchRequest {
        batch: StagedBatch {
            target: Some(target()),
            key_columns: vec!["id".to_string()],
            ops,
        },
    }
}

fn profile() -> ConnectionProfile {
    ConnectionProfile::sql_login(
        "caps",
        "localhost",
        "sa",
        SecretRef {
            provider: SecretProvider::InMemory,
            key: "pw".to_string(),
        },
    )
}

fn target() -> TableTarget {
    TableTarget {
        schema: "dbo".to_string(),
        table: "t".to_string(),
    }
}

fn update_with(expr: ColumnExpression) -> UpdateRowRequest {
    UpdateRowRequest {
        target: target(),
        key: vec![("id".to_string(), CellValue::I64(1))],
        rowversion: None,
        changes: Vec::new(),
        original_values: Vec::new(),
        expressions: vec![("stamp".to_string(), expr)],
    }
}

fn insert_with(expr: ColumnExpression) -> InsertRowRequest {
    InsertRowRequest {
        target: target(),
        values: vec![("id".to_string(), CellValue::I64(1))],
        expressions: vec![("owner".to_string(), expr)],
    }
}

fn row_edit_frames(sent: &Arc<Mutex<Vec<RawFrame>>>) -> usize {
    sent.lock()
        .unwrap()
        .iter()
        .filter(|f| f.tag == REQUEST_TAG_ROW_EDIT)
        .count()
}

async fn connect(
    ready: Option<RawFrame>,
) -> (
    RemoteServices<EngineStub>,
    Arc<Mutex<Vec<RawFrame>>>,
    Option<sqlly_remote::engine_protocol::EngineCapabilities>,
) {
    let (stub, sent) = EngineStub::new(ready);
    let engine = RemoteEngine::new(stub);
    let caps = engine.wait_for_capabilities(Duration::from_secs(5)).await;
    (RemoteServices::from_engine(engine), sent, caps)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn old_engine_advertises_nothing_and_new_expressions_never_reach_it() {
    let (remote, sent, caps) = connect(Some(legacy_ready())).await;
    let caps = caps.expect("a legacy Ready still resolves the wait");
    assert!(caps.is_empty());
    let row_editing = &remote.services().row_editing;
    assert!(!row_editing.server_expressions_supported());

    for expr in [
        ColumnExpression::ServerDate,
        ColumnExpression::ServerUuid,
        ColumnExpression::CurrentUser,
    ] {
        let err = row_editing
            .update_row(&profile(), update_with(expr))
            .await
            .expect_err("an old bridge must not receive the new variant");
        assert!(
            matches!(&err, StudioError::CapabilityNotSupportedMessage(m) if m.fallback().contains("older version") && m.fallback().contains("stamp")),
            "{err}"
        );
        let err = row_editing
            .insert_row(&profile(), insert_with(expr))
            .await
            .expect_err("inserts are gated the same way");
        assert!(matches!(err, StudioError::CapabilityNotSupportedMessage(_)));
    }
    let err = row_editing
        .fk_labels(
            &profile(),
            FkLabelRequest {
                referenced: target(),
                key_column: "id".to_string(),
                display_column: "name".to_string(),
                keys: vec![CellValue::I64(1)],
            },
        )
        .await
        .expect_err("an old bridge has no fk_labels op");
    assert!(matches!(err, StudioError::CapabilityNotSupported(_)));
    assert_eq!(row_edit_frames(&sent), 0, "refusals happen before the wire");

    // What the old bridge DOES understand still goes out.
    let err = row_editing
        .update_row(&profile(), update_with(ColumnExpression::ServerTime))
        .await
        .expect_err("the stub answers request_failed");
    assert!(err.to_string().contains("reached the wire"), "{err}");
    assert_eq!(row_edit_frames(&sent), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn new_engine_unlocks_server_expressions_and_fk_labels() {
    let (remote, sent, caps) = connect(Some(current_ready())).await;
    let caps = caps.expect("ready");
    assert!(caps.supports(CAPABILITY_ROW_EDIT_SERVER_EXPRESSIONS));
    let row_editing = &remote.services().row_editing;
    assert!(row_editing.server_expressions_supported());

    let err = row_editing
        .update_row(&profile(), update_with(ColumnExpression::ServerUuid))
        .await
        .expect_err("the stub answers request_failed");
    assert!(err.to_string().contains("reached the wire"), "{err}");
    let err = row_editing
        .fk_labels(
            &profile(),
            FkLabelRequest {
                referenced: target(),
                key_column: "id".to_string(),
                display_column: "name".to_string(),
                keys: vec![CellValue::I64(1)],
            },
        )
        .await
        .expect_err("the stub answers request_failed");
    assert!(err.to_string().contains("reached the wire"), "{err}");
    assert_eq!(row_edit_frames(&sent), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_capability_list_counts_as_none() {
    let mut body =
        encode_ready_body(PROTOCOL_VERSION, &[CAPABILITY_ROW_EDIT_SERVER_EXPRESSIONS]).unwrap();
    body.truncate(body.len() - 3);
    let (remote, _sent, caps) = connect(Some(RawFrame::new(FRAME_TAG_READY, body))).await;
    assert!(
        caps.expect("still a Ready").is_empty(),
        "a damaged list must never read as a partial one"
    );
    assert!(!remote.services().row_editing.server_expressions_supported());
    // The session survives: the version prefix was fine.
    assert!(!*remote.engine().closed_signal().borrow());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn before_ready_nothing_is_supported() {
    let (remote, _sent, caps) = connect(None).await;
    assert!(caps.is_none(), "the wait times out without a Ready");
    assert!(remote.engine().capabilities().is_none());
    assert!(!remote.services().row_editing.server_expressions_supported());
}

/// Staged grid saves (`apply_batch`) follow the same rule: an old bridge is
/// refused before anything is sent, a current one receives the batch, and a
/// bridge that has the batch op but not the newer expressions still never
/// sees those expressions.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn staged_batch_save_is_gated_on_the_bridge() {
    // Old bridge.
    let (remote, sent, _) = connect(Some(legacy_ready())).await;
    let row_editing = &remote.services().row_editing;
    assert!(!row_editing.batch_commit_supported());
    let err = row_editing
        .apply_batch(
            &profile(),
            batch_of(vec![StagedOp::Update(update_with(
                ColumnExpression::Default,
            ))]),
        )
        .await
        .expect_err("an old bridge has no apply_batch op");
    assert!(
        matches!(&err, StudioError::CapabilityNotSupportedMessage(m) if m.fallback().contains("update the bridge")),
        "{err}"
    );
    assert_eq!(row_edit_frames(&sent), 0);

    // Current bridge.
    let (remote, sent, _) = connect(Some(current_ready())).await;
    let row_editing = &remote.services().row_editing;
    assert!(row_editing.batch_commit_supported());
    let err = row_editing
        .apply_batch(
            &profile(),
            batch_of(vec![
                StagedOp::Update(update_with(ColumnExpression::ServerUuid)),
                StagedOp::Insert(insert_with(ColumnExpression::CurrentUser)),
            ]),
        )
        .await
        .expect_err("the stub answers request_failed");
    assert!(err.to_string().contains("reached the wire"), "{err}");
    assert_eq!(row_edit_frames(&sent), 1);

    // A bridge with the batch op but without the newer expressions.
    let ready = RawFrame::new(
        FRAME_TAG_READY,
        encode_ready_body(PROTOCOL_VERSION, &[CAPABILITY_ROW_EDIT_APPLY_BATCH]).unwrap(),
    );
    let (remote, sent, _) = connect(Some(ready)).await;
    let err = remote
        .services()
        .row_editing
        .apply_batch(
            &profile(),
            batch_of(vec![StagedOp::Update(update_with(
                ColumnExpression::ServerDate,
            ))]),
        )
        .await
        .expect_err("the expression is refused before the wire");
    assert!(
        matches!(err, StudioError::CapabilityNotSupportedMessage(_)),
        "{err}"
    );
    assert_eq!(row_edit_frames(&sent), 0);
}
