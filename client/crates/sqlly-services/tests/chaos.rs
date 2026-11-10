//! Chaos suite for the remote-engine frame path: malformed frames, frames for
//! unknown requests, duplicate terminal frames, cancel/terminal races, and
//! byte-level partial frames. Every scenario asserts the same three
//! invariants — no panic, no hang, and exactly one terminal event per query —
//! because a request stream that ends twice, never, or by tearing the session
//! down is exactly the class of bug the remote cancel hang was.
//!
//! The scripted transport plays the engine's side of the wire in-process: it
//! parses each outgoing request's id and answers with whatever frame sequence
//! the scenario dictates, however hostile.

use std::collections::VecDeque;
use std::time::Duration;

use async_trait::async_trait;
use sqlly_core::{ConnectionProfile, SecretProvider, SecretRef};
use sqlly_remote::engine_protocol::{
    AsyncFramedTransport, FRAME_TAG_READY, FRAME_TAG_REQUEST_CANCELLED,
    FRAME_TAG_REQUEST_COMPLETED, FRAME_TAG_REQUEST_STARTED, FRAME_TAG_ROWS_BATCH, FramedTransport,
    MAX_FRAME_BYTES, PROTOCOL_VERSION, ProtocolError, REQUEST_TAG_CANCEL, RawFrame,
    encode_id_prefix,
};
use sqlly_services::remote::{RemoteEngine, RemoteQueryExecutor};
use sqlly_services::{QueryEvent, QueryExecutor};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

fn profile() -> ConnectionProfile {
    ConnectionProfile::sql_login(
        "chaos",
        "localhost",
        "sa",
        SecretRef {
            provider: SecretProvider::InMemory,
            key: "pw".to_string(),
        },
    )
}

/// Parse the request id out of an outgoing request frame:
/// `[u16 layout][u32 id_len][id]` for queries, and the same shape for cancels
/// (whose id is the cancel's own id; the target follows).
fn request_id_of(frame: &RawFrame) -> String {
    let body = &frame.body[2..];
    let len = u32::from_le_bytes(body[..4].try_into().expect("id length")) as usize;
    String::from_utf8(body[4..4 + len].to_vec()).expect("utf8 id")
}

/// The cancel target id: second `[u32 len][utf8]` string after the layout
/// version and the cancel's own id.
fn cancel_target_of(frame: &RawFrame) -> String {
    let body = &frame.body[2..];
    let first = u32::from_le_bytes(body[..4].try_into().expect("len")) as usize;
    let rest = &body[4 + first..];
    let second = u32::from_le_bytes(rest[..4].try_into().expect("len")) as usize;
    String::from_utf8(rest[4..4 + second].to_vec()).expect("utf8 target")
}

fn id_frame(tag: u8, id: &str) -> RawFrame {
    RawFrame::new(tag, encode_id_prefix(id).expect("id prefix"))
}

fn completed(id: &str) -> RawFrame {
    id_frame(FRAME_TAG_REQUEST_COMPLETED, id)
}

/// A `request_cancelled` ack: body is `[cancel_id][target_id]`; the router
/// must deliver it to the TARGET's stream.
fn cancelled(cancel_id: &str, target_id: &str) -> RawFrame {
    let mut body = encode_id_prefix(cancel_id).expect("cancel id");
    body.extend_from_slice(&encode_id_prefix(target_id).expect("target id"));
    RawFrame::new(FRAME_TAG_REQUEST_CANCELLED, body)
}

/// A rows-batch frame whose payload after the id is garbage the decoder must
/// reject without panicking.
fn garbage_rows(id: &str) -> RawFrame {
    let mut body = encode_id_prefix(id).expect("id prefix");
    body.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef, 0xff, 0x01]);
    RawFrame::new(FRAME_TAG_ROWS_BATCH, body)
}

/// A frame whose body is too short to carry an id at all: the router cannot
/// address it and must drop it without disturbing the session.
fn unaddressable(tag: u8) -> RawFrame {
    RawFrame::new(tag, vec![0xff, 0xff])
}

/// The engine's side of a scenario: outgoing frame in, reply frames out.
type Script = Box<dyn FnMut(&RawFrame) -> Vec<RawFrame> + Send>;

/// Plays the engine's side: each outgoing frame is answered with the frames
/// the scenario's script returns for it.
struct ScriptedTransport {
    script: Script,
    queue: VecDeque<RawFrame>,
    wake_tx: mpsc::UnboundedSender<()>,
    wake_rx: mpsc::UnboundedReceiver<()>,
}

impl ScriptedTransport {
    fn new(script: impl FnMut(&RawFrame) -> Vec<RawFrame> + Send + 'static) -> Self {
        let (wake_tx, wake_rx) = mpsc::unbounded_channel();
        Self {
            script: Box::new(script),
            queue: VecDeque::new(),
            wake_tx,
            wake_rx,
        }
    }
}

#[async_trait]
impl FramedTransport for ScriptedTransport {
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
        let replies = (self.script)(frame);
        self.queue.extend(replies);
        let _ = self.wake_tx.send(());
        Ok(())
    }
}

/// Drain a handle's events to completion, with a hang guard. Returns the
/// full event list.
async fn drain_all(mut events: mpsc::Receiver<QueryEvent>) -> Vec<QueryEvent> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut out = Vec::new();
        while let Some(ev) = events.recv().await {
            out.push(ev);
        }
        out
    })
    .await
    .expect("the stream must terminate — a hang is the failure this suite exists to catch")
}

fn terminal_count(events: &[QueryEvent]) -> usize {
    events
        .iter()
        .filter(|ev| {
            matches!(
                ev,
                QueryEvent::Completed { .. } | QueryEvent::Error { .. } | QueryEvent::Cancelled
            )
        })
        .count()
}

/// Malformed frames on a live stream — garbage rows payload, an unaddressable
/// body, junk for an id that was never issued — must all be swallowed, and the
/// stream still ends on its real terminal.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_and_misaddressed_frames_do_not_break_the_stream() {
    let transport = ScriptedTransport::new(|frame| {
        let id = request_id_of(frame);
        vec![
            id_frame(FRAME_TAG_REQUEST_STARTED, &id),
            garbage_rows(&id),
            unaddressable(FRAME_TAG_ROWS_BATCH),
            garbage_rows("req-999"),
            completed(&id),
        ]
    });
    let executor = RemoteQueryExecutor::new(RemoteEngine::new(transport));
    let handle = executor
        .execute(&profile(), "SELECT 1", 8)
        .await
        .expect("execute");
    let events = drain_all(handle.events).await;
    assert_eq!(terminal_count(&events), 1, "{events:?}");
    assert!(
        matches!(events.last(), Some(QueryEvent::Completed { .. })),
        "{events:?}"
    );
}

/// Duplicate terminal frames (and rows after the terminal): the first
/// terminal ends the stream exactly once; the leftovers are discarded and the
/// session stays healthy for the next request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_terminal_frames_end_the_stream_once_and_spare_the_session() {
    let transport = ScriptedTransport::new(|frame| {
        let id = request_id_of(frame);
        vec![
            completed(&id),
            completed(&id),
            garbage_rows(&id),
            completed(&id),
        ]
    });
    let executor = RemoteQueryExecutor::new(RemoteEngine::new(transport));

    let first = executor
        .execute(&profile(), "SELECT 1", 8)
        .await
        .expect("first execute");
    let events = drain_all(first.events).await;
    assert_eq!(terminal_count(&events), 1, "{events:?}");

    // The duplicate terminals for the finished id must not have poisoned the
    // demux: a second request on the same session still completes.
    let second = executor
        .execute(&profile(), "SELECT 2", 8)
        .await
        .expect("second execute");
    let events = drain_all(second.events).await;
    assert_eq!(terminal_count(&events), 1, "{events:?}");
}

/// Cancel/terminal race, completion first: the engine finishes the query just
/// before it processes the cancel, so the stream gets `request_completed`
/// followed by a late `request_cancelled` for the same id. The stream must
/// report Completed exactly once and discard the late ack.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_losing_the_race_to_completion_is_discarded() {
    let transport = ScriptedTransport::new(|frame| {
        if frame.tag == REQUEST_TAG_CANCEL {
            let cancel_id = request_id_of(frame);
            let target = cancel_target_of(frame);
            // Completion was already on the wire; the cancel ack trails it.
            vec![completed(&target), cancelled(&cancel_id, &target)]
        } else {
            // Hold the query open until the cancel arrives.
            let id = request_id_of(frame);
            vec![id_frame(FRAME_TAG_REQUEST_STARTED, &id)]
        }
    });
    let executor = RemoteQueryExecutor::new(RemoteEngine::new(transport));
    let handle = executor
        .execute(&profile(), "SELECT 1", 8)
        .await
        .expect("execute");
    executor.cancel(&handle.task_id).await.expect("cancel");

    let events = drain_all(handle.events).await;
    assert_eq!(terminal_count(&events), 1, "{events:?}");
    assert!(
        matches!(events.last(), Some(QueryEvent::Completed { .. })),
        "completion won the race, so the terminal must be Completed: {events:?}"
    );
}

/// Cancel/terminal race, cancel first: `request_cancelled` lands before the
/// (already in flight) completion. The stream must report Cancelled exactly
/// once and discard the late completion.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completion_losing_the_race_to_cancel_is_discarded() {
    let transport = ScriptedTransport::new(|frame| {
        if frame.tag == REQUEST_TAG_CANCEL {
            let cancel_id = request_id_of(frame);
            let target = cancel_target_of(frame);
            vec![cancelled(&cancel_id, &target), completed(&target)]
        } else {
            let id = request_id_of(frame);
            vec![id_frame(FRAME_TAG_REQUEST_STARTED, &id)]
        }
    });
    let executor = RemoteQueryExecutor::new(RemoteEngine::new(transport));
    let handle = executor
        .execute(&profile(), "SELECT 1", 8)
        .await
        .expect("execute");
    executor.cancel(&handle.task_id).await.expect("cancel");

    let events = drain_all(handle.events).await;
    assert_eq!(terminal_count(&events), 1, "{events:?}");
    assert!(
        matches!(events.last(), Some(QueryEvent::Cancelled)),
        "the cancel ack arrived first, so the terminal must be Cancelled: {events:?}"
    );
}

/// A peer that dies mid-frame (length prefix promised more bytes than were
/// written) must surface an error — never a clean EOF and never a hang.
#[tokio::test]
async fn partial_frame_at_disconnect_is_an_error_not_a_clean_eof() {
    let (mut chaos_side, engine_side) = tokio::io::duplex(256);
    let mut transport = AsyncFramedTransport::new(engine_side);

    // Promise a 10-byte payload, deliver 3, hang up.
    chaos_side
        .write_all(&10u32.to_le_bytes())
        .await
        .expect("length prefix");
    chaos_side
        .write_all(&[0x01, 0x02, 0x03])
        .await
        .expect("partial body");
    drop(chaos_side);

    let result = tokio::time::timeout(Duration::from_secs(5), transport.recv())
        .await
        .expect("truncated frame must resolve promptly");
    assert!(
        result.is_err(),
        "a mid-frame disconnect is corruption, not a clean end-of-stream: {result:?}"
    );
}

/// A length prefix past the protocol cap must be rejected before any body
/// allocation, poisoning only that session — not the process.
#[tokio::test]
async fn oversized_length_prefix_is_rejected() {
    let (mut chaos_side, engine_side) = tokio::io::duplex(256);
    let mut transport = AsyncFramedTransport::new(engine_side);

    chaos_side
        .write_all(&(MAX_FRAME_BYTES + 1).to_le_bytes())
        .await
        .expect("oversized prefix");

    let result = tokio::time::timeout(Duration::from_secs(5), transport.recv())
        .await
        .expect("oversized frame must resolve promptly");
    assert!(
        matches!(result, Err(ProtocolError::FrameTooLarge(_))),
        "{result:?}"
    );
}

/// A Ready frame announcing a different protocol version must close the
/// session with a diagnostic naming BOTH versions — never limp along into
/// baffling per-frame decode failures.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn protocol_version_mismatch_names_both_versions_and_closes_the_session() {
    let transport = ScriptedTransport::new(|_frame| {
        vec![RawFrame::new(
            FRAME_TAG_READY,
            9999u32.to_le_bytes().to_vec(),
        )]
    });
    let engine = RemoteEngine::new(transport);
    let error = tokio::time::timeout(Duration::from_secs(5), engine.ping())
        .await
        .expect("mismatch must resolve the ping promptly")
        .expect_err("mismatch must fail the ping");
    let message = error.to_string();
    assert!(message.contains("protocol version mismatch"), "{message}");
    assert!(message.contains("v9999"), "{message}");
    assert!(
        message.contains(&format!("v{PROTOCOL_VERSION}")),
        "{message}"
    );
}

/// Eight interleaved queries on one session: every stream gets exactly its own
/// terminal, none cross-talk, none hang.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_mixed_requests_interleave_without_cross_talk() {
    let transport = ScriptedTransport::new(|frame| {
        let id = request_id_of(frame);
        vec![id_frame(FRAME_TAG_REQUEST_STARTED, &id), completed(&id)]
    });
    let executor = std::sync::Arc::new(RemoteQueryExecutor::new(RemoteEngine::new(transport)));
    let mut joins = Vec::new();
    for i in 0..8 {
        let executor = executor.clone();
        joins.push(tokio::spawn(async move {
            let handle = executor
                .execute(&profile(), &format!("SELECT {i}"), 8)
                .await
                .expect("execute");
            drain_all(handle.events).await
        }));
    }
    for join in joins {
        let events = join.await.expect("join");
        assert_eq!(terminal_count(&events), 1, "{events:?}");
        assert!(
            matches!(events.last(), Some(QueryEvent::Completed { .. })),
            "{events:?}"
        );
    }
}

/// A hostile engine answers query B with terminals for BOTH A and B. A's
/// stream must end exactly once — from its own terminal — and B's stream must
/// not end twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interleaved_duplicate_terminals_end_each_stream_exactly_once() {
    let ids = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let script_ids = ids.clone();
    let transport = ScriptedTransport::new(move |frame| {
        let id = request_id_of(frame);
        let mut seen = script_ids.lock().unwrap();
        seen.push(id.clone());
        if seen.len() == 2 {
            // Answer the SECOND request with terminals for both ids, twice.
            let first = seen[0].clone();
            vec![
                completed(&first),
                completed(&id),
                completed(&first),
                completed(&id),
            ]
        } else {
            // First request: only a started frame; its terminal arrives via
            // the second request's hostile batch.
            vec![id_frame(FRAME_TAG_REQUEST_STARTED, &id)]
        }
    });
    let executor = std::sync::Arc::new(RemoteQueryExecutor::new(RemoteEngine::new(transport)));
    let first = executor.execute(&profile(), "SELECT 1", 8).await.unwrap();
    let second = executor.execute(&profile(), "SELECT 2", 8).await.unwrap();
    let first_events = drain_all(first.events).await;
    let second_events = drain_all(second.events).await;
    assert_eq!(terminal_count(&first_events), 1, "{first_events:?}");
    assert_eq!(terminal_count(&second_events), 1, "{second_events:?}");
}

/// A cancellation storm: many in-flight queries cancelled at once. Every
/// stream settles with exactly one terminal and the session stays usable.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancellation_storm_settles_every_stream() {
    let transport = ScriptedTransport::new(|frame| {
        if frame.tag == REQUEST_TAG_CANCEL {
            let cancel_id = request_id_of(frame);
            let target = cancel_target_of(frame);
            return vec![cancelled(&cancel_id, &target)];
        }
        let id = request_id_of(frame);
        // Started only — the query "runs" until cancelled.
        vec![id_frame(FRAME_TAG_REQUEST_STARTED, &id)]
    });
    let executor = std::sync::Arc::new(RemoteQueryExecutor::new(RemoteEngine::new(transport)));
    let mut handles = Vec::new();
    for i in 0..8 {
        handles.push(
            executor
                .execute(&profile(), &format!("SELECT {i}"), 8)
                .await
                .expect("execute"),
        );
    }
    for handle in &handles {
        executor.cancel(&handle.task_id).await.expect("cancel");
    }
    for handle in handles {
        let events = drain_all(handle.events).await;
        assert_eq!(terminal_count(&events), 1, "{events:?}");
        assert!(
            events.iter().any(|e| matches!(e, QueryEvent::Cancelled)),
            "{events:?}"
        );
    }

    // The session survives the storm: a fresh query still completes.
    let transport_check = executor
        .execute(&profile(), "SELECT 99", 8)
        .await
        .expect("post-storm execute");
    // This query is never cancelled, so it hangs on "started" — cancel it to
    // close the stream and prove the cancel path still works too.
    executor
        .cancel(&transport_check.task_id)
        .await
        .expect("post-storm cancel");
    let events = drain_all(transport_check.events).await;
    assert_eq!(terminal_count(&events), 1, "{events:?}");
}

/// The remote row-editing service round-trips hermetically: the scripted
/// engine answers an open_session with a NotEditable report, which must decode
/// into the typed outcome (no live database anywhere).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_row_edit_open_session_round_trips_hermetically() {
    use sqlly_services::remote::RemoteRowEditingService;
    use sqlly_services::{RowEditOpenOutcome, RowEditOpenRequest, RowEditingService};

    let transport = ScriptedTransport::new(|frame| {
        let id = request_id_of(frame);
        let report = serde_json::to_string(&RowEditOpenOutcome::NotEditable {
            reason: "scripted".to_string(),
            message: Some(sqlly_core::StoredMessage {
                key: "services.row_editing.not_editable.row_not_found".to_string(),
                arguments: Vec::new(),
            }),
        })
        .unwrap();
        let mut body = encode_id_prefix(&id).expect("id");
        body.extend_from_slice(&encode_id_prefix(&report).expect("payload"));
        vec![RawFrame::new(
            0x14, /* FRAME_TAG_ROW_EDIT_REPORT */
            body,
        )]
    });
    let service = RemoteRowEditingService::new(RemoteEngine::new(transport));
    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        service.open_session(
            &profile(),
            RowEditOpenRequest {
                sql: "SELECT * FROM t".to_string(),
                result_set_index: 0,
                columns: Vec::new(),
                row: Vec::new(),
            },
        ),
    )
    .await
    .expect("must answer promptly")
    .expect("open_session");
    match outcome {
        RowEditOpenOutcome::NotEditable { reason, message } => {
            assert_eq!(reason, "scripted");
            assert_eq!(
                message.map(|message| message.key),
                Some("services.row_editing.not_editable.row_not_found".to_string())
            );
        }
        other => panic!("expected NotEditable, got {other:?}"),
    }
}

/// The three AI endpoints round-trip hermetically over the remote (bridge)
/// path: a scripted engine answers provider_status, review_sql, and
/// summarize_query with valid report frames, which must decode into the
/// typed shapes — proving the bridged dispatch, not just the local CLI.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_ai_endpoints_round_trip_hermetically() {
    use sqlly_services::catalog::CatalogService;
    use sqlly_services::remote::RemoteCatalogService;

    let transport = ScriptedTransport::new(|frame| {
        let id = request_id_of(frame);
        let (tag, json) = match frame.tag {
            0x14 /* REQUEST_TAG_PROVIDER_STATUS */ => (
                0x16u8, /* FRAME_TAG_PROVIDER_STATUS_REPORT */
                serde_json::json!({
                    "llm_requested": null,
                    "llm_active": "stub",
                    "llm_fallback_reason": null,
                    "embedding_requested": null,
                    "embedding_active": "stub",
                    "embedding_fallback_reason": null,
                    "runtime_reachable": null,
                    "runtime_version": null,
                    "installed_models": [],
                    "runtime_detail": null
                }),
            ),
            0x15 /* REQUEST_TAG_REVIEW_SQL */ => (
                0x17u8, /* FRAME_TAG_SQL_REVIEW_REPORT */
                serde_json::json!({
                    "narrative": "scripted review",
                    "validation": {
                        "is_valid": true,
                        "requires_confirmation": false,
                        "detected_intent": "select",
                        "issues": [],
                        "referenced_objects": [],
                        "referenced_columns": []
                    },
                    "meta": {"grounded_in": []}
                }),
            ),
            0x16 /* REQUEST_TAG_SUMMARIZE_QUERY */ => (
                0x18u8, /* FRAME_TAG_QUERY_SUMMARY_REPORT */
                serde_json::json!({
                    "summary": "scripted summary",
                    "meta": {"grounded_in": []}
                }),
            ),
            _ => return vec![],
        };
        let mut body = encode_id_prefix(&id).expect("id");
        body.extend_from_slice(&encode_id_prefix(&json.to_string()).expect("payload"));
        vec![RawFrame::new(tag, body)]
    });

    let service = RemoteCatalogService::new(RemoteEngine::new(transport));

    let status = tokio::time::timeout(Duration::from_secs(5), service.provider_status())
        .await
        .expect("prompt")
        .expect("provider_status");
    assert_eq!(status.llm_active, "stub");

    let review = tokio::time::timeout(
        Duration::from_secs(5),
        service.review_sql(&profile(), "SELECT 1".to_string(), None),
    )
    .await
    .expect("prompt")
    .expect("review_sql");
    assert_eq!(review.narrative, "scripted review");
    assert!(review.validation.is_valid);

    let summary = tokio::time::timeout(
        Duration::from_secs(5),
        service.summarize_query(&profile(), "SELECT 1".to_string(), vec![], None, None),
    )
    .await
    .expect("prompt")
    .expect("summarize_query");
    assert_eq!(summary.summary, "scripted summary");
}
