//! Shared remote engine handle backed by a **background demultiplexer task**,
//! plus the request-body encoders and response decoders that MUST match
//! `apps/cli/src/engine.rs` byte-for-byte.
//!
//! Every remote-backed port ([`crate::remote::query`], [`crate::remote::catalog`],
//! [`crate::remote::connection`]) holds a clone of the same [`RemoteEngine`] so they all
//! multiplex over one transport. A single background task owns the transport's
//! read side: it reads every inbound frame and routes it — by the request id
//! carried in the frame body — to a per-request channel. Senders never hold the
//! transport across a read; they register a channel and hand the outbound frame
//! to the task over a lock-free command queue. This lets many overlapping
//! requests (a streaming query + a catalog sync + a metadata probe) interleave
//! over one session instead of serializing behind whichever request currently
//! holds the transport.
//!
//! Because a `FramedTransport` (a `sqlly-remote` `PeerSession` in production) is
//! a single duplex stream that cannot be split into independent read/write
//! halves, the task drives both directions with one biased `select!` that
//! reduces to an event and acts *after* the select completes — the same pattern
//! the on-prem bridge pump uses. Report-style requests (catalog ops) are
//! single-shot: the engine answers with exactly one report frame (or a
//! `request_failed`), which is NOT one of the three terminal tags, so those
//! callers read a single frame off their channel rather than draining to a
//! terminal.

use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use sqlly_core::{AuthenticationType, ConnectionProfile, StudioError, StudioResult};
use sqlly_lang::sqlmodel::Dialect;
use sqlly_remote::engine_protocol::{
    BinaryReader, BinaryWriter, EngineCapabilities, EngineClient, FRAME_TAG_PONG,
    FRAME_TAG_REQUEST_FAILED, FramedTransport, REQUEST_TAG_PING, RawFrame,
};
use tokio::sync::mpsc;

/// The per-request sink the demux task routes inbound frames into. Bounded so
/// a peer that ignores the engine's `row_buffer` flow control cannot grow a
/// route's queue without limit — but delivered with `try_send`, so the reader
/// NEVER blocks routing one request behind another's slow consumer. A full
/// route is a protocol violation: that route is closed (its consumer sees the
/// stream end and reports a terminal transport error) while every other route
/// keeps streaming.
type RouteSender = mpsc::Sender<RawFrame>;

/// Frames a single route may hold undelivered before it is declared hostile.
/// A well-behaved engine honors flow control and stays far below this; a
/// flooder hits it quickly and only its own request dies.
const MAX_ROUTE_BUFFERED_FRAMES: usize = 4096;

/// Outbound request frames the demux task may hold unwritten. Requests are
/// small control frames; thousands queued means the transport is wedged, and
/// failing the new request fast beats buffering without bound.
const MAX_OUTBOUND_QUEUED_FRAMES: usize = 4096;

/// How long a `ping` may wait for its pong before the engine is declared
/// unresponsive.
const PING_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);

/// How long a report request may sit with NO frame arriving before the wait is
/// abandoned. Generous on purpose: report operations can be LLM-bound
/// (ask-schema, AI-cache builds) and legitimately silent for minutes; this
/// only fires when the engine has truly stopped responding.
const REPORT_IDLE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(600);

/// An outbound engine frame queued for the demux task to write. Only the task
/// touches the transport, so writes are serialized without a transport lock.
struct OutFrame {
    tag: u8,
    body: Vec<u8>,
}

/// Shared state between the [`RemoteEngine`] handles and their one demux task.
struct Demux {
    /// Bounded queue of frames to write. Dropping every [`RemoteEngine`]
    /// clone drops this sender, which closes the task's receiver and tears the
    /// session down cleanly (closing the transport). Queued with `try_send` so
    /// a wedged transport fails new requests fast instead of buffering them
    /// without bound.
    cmd_tx: mpsc::Sender<OutFrame>,
    /// Request id → response sink. Registered before the request frame is
    /// queued (so a response can never race ahead of its route) and removed by
    /// the caller when its response stream ends.
    routes: Arc<StdMutex<HashMap<String, RouteSender>>>,
    /// Monotonic request-id counter, `req-N`.
    seq: AtomicU64,
    /// Set once the transport closes; new requests then fail fast instead of
    /// registering a route no task will ever service.
    closed: Arc<AtomicBool>,
    /// The transport error that closed the session, when it closed on an
    /// error rather than clean EOF. Preserves the typed relay code a
    /// `PeerSession` flattens into its `ProtocolError` text, so consumers that
    /// observe the closed session surface the relay's copy instead of a
    /// generic "engine closed".
    close_reason: Arc<StdMutex<Option<String>>>,
    /// Observed side of the session-closed signal ([`RemoteEngine::closed_signal`]).
    closed_rx: tokio::sync::watch::Receiver<bool>,
    /// What the engine advertised in its `Ready` frame: `None` until that
    /// frame arrives, then the decoded set (empty for an engine that predates
    /// capability negotiation, or whose list was malformed).
    ready_rx: tokio::sync::watch::Receiver<Option<EngineCapabilities>>,
}

/// A shared, transport-agnostic engine handle. Cloning shares the same demux
/// task (and therefore the same transport + request-id sequence), so all ports
/// built from one handle interleave over one session.
pub struct RemoteEngine<T> {
    inner: Arc<Demux>,
    _marker: PhantomData<fn() -> T>,
}

impl<T> Clone for RemoteEngine<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            _marker: PhantomData,
        }
    }
}

impl<T: FramedTransport + 'static> RemoteEngine<T> {
    /// Wrap a connected [`FramedTransport`] (a `sqlly-remote` `PeerSession` in
    /// production, a `sqlly-cli engine` stdio transport in tests) and spawn its
    /// demux task.
    pub fn new(transport: T) -> Self {
        Self::from_client(EngineClient::new(transport))
    }

    /// Wrap an already-constructed [`EngineClient`] and spawn its demux task.
    pub fn from_client(client: EngineClient<T>) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel::<OutFrame>(MAX_OUTBOUND_QUEUED_FRAMES);
        let routes: Arc<StdMutex<HashMap<String, RouteSender>>> =
            Arc::new(StdMutex::new(HashMap::new()));
        let closed = Arc::new(AtomicBool::new(false));
        let close_reason: Arc<StdMutex<Option<String>>> = Arc::new(StdMutex::new(None));
        let (closed_tx, closed_rx) = tokio::sync::watch::channel(false);
        let (ready_tx, ready_rx) = tokio::sync::watch::channel(None);

        // The task deliberately does NOT hold `cmd_tx`: keeping the writer solely
        // in the handles means the last handle drop closes `cmd_rx` and stops the
        // task, closing the transport.
        crate::remote::rt::spawn(run_demux(
            client,
            cmd_rx,
            Arc::clone(&routes),
            Arc::clone(&closed),
            Arc::clone(&close_reason),
            closed_tx,
            ready_tx,
        ));

        Self {
            inner: Arc::new(Demux {
                cmd_tx,
                routes,
                seq: AtomicU64::new(0),
                closed,
                close_reason,
                closed_rx,
                ready_rx,
            }),
            _marker: PhantomData,
        }
    }

    /// A watch on the session lifecycle: flips to `true` the moment the demux
    /// task ends (transport EOF, transport error, or last-handle teardown).
    /// Lets a host observe connection loss the instant it happens instead of
    /// discovering it on the next failed request. The sender lives in the
    /// demux task, so a dropped sender also means the session is over.
    pub fn closed_signal(&self) -> tokio::sync::watch::Receiver<bool> {
        self.inner.closed_rx.clone()
    }

    /// The capabilities the engine advertised, or `None` while its `Ready`
    /// frame has not arrived yet. An engine that predates negotiation yields
    /// `Some(empty)`: callers gate newer wire features on
    /// [`EngineCapabilities::supports`] and treat "not yet known" the same as
    /// "not supported".
    pub fn capabilities(&self) -> Option<EngineCapabilities> {
        self.inner.ready_rx.borrow().clone()
    }

    /// Whether the engine advertised `capability`. False until `Ready`
    /// arrives, and always false for an engine that predates negotiation.
    pub fn supports(&self, capability: &str) -> bool {
        self.inner
            .ready_rx
            .borrow()
            .as_ref()
            .is_some_and(|caps| caps.supports(capability))
    }

    /// Wait up to `timeout` for the engine's `Ready` frame and return what it
    /// advertised. `None` on timeout or when the session closes first; the
    /// session itself stays usable either way (capabilities are then treated
    /// as absent until `Ready` shows up).
    pub async fn wait_for_capabilities(
        &self,
        timeout: std::time::Duration,
    ) -> Option<EngineCapabilities> {
        let mut rx = self.inner.ready_rx.clone();
        let waited = crate::remote::rt::timeout(timeout, async {
            loop {
                if let Some(caps) = rx.borrow_and_update().clone() {
                    return Some(caps);
                }
                if rx.changed().await.is_err() {
                    return None;
                }
            }
        })
        .await;
        waited.ok().flatten()
    }

    /// Allocate a fresh, session-unique request id of the form `req-N`.
    pub(crate) fn next_request_id(&self) -> String {
        let n = self.inner.seq.fetch_add(1, Ordering::Relaxed) + 1;
        format!("req-{n}")
    }

    /// The user-facing message for the transport error that closed the
    /// session, if it closed on an error (relay codes mapped to their copy).
    /// `None` while the session is live or after a clean EOF.
    pub(crate) fn close_reason_message(&self) -> Option<String> {
        self.inner
            .close_reason
            .lock()
            .expect("close reason poisoned")
            .as_deref()
            .map(transport_message)
    }

    /// The error a caller observing the closed session should surface: the
    /// mapped close reason when the transport died on an error, otherwise
    /// `fallback`.
    fn closed_error(&self, fallback: &str) -> StudioError {
        StudioError::Connection(
            self.close_reason_message()
                .unwrap_or_else(|| fallback.to_string()),
        )
    }

    /// Register a response channel for `id`, then queue the request frame for the
    /// demux task to write. The route is inserted *before* the frame is queued so
    /// the response can never arrive before its sink exists. Returns the receiver
    /// the caller drains; the caller must [`unregister`](Self::unregister) `id`
    /// when its stream ends.
    pub(crate) fn open(
        &self,
        req_tag: u8,
        id: &str,
        body: Vec<u8>,
    ) -> StudioResult<mpsc::Receiver<RawFrame>> {
        if self.inner.closed.load(Ordering::SeqCst) {
            return Err(self.closed_error("engine transport is closed"));
        }
        let (tx, rx) = mpsc::channel::<RawFrame>(MAX_ROUTE_BUFFERED_FRAMES);
        self.inner
            .routes
            .lock()
            .expect("routes poisoned")
            .insert(id.to_string(), tx);
        if let Err(error) = self.inner.cmd_tx.try_send(OutFrame { tag: req_tag, body }) {
            self.unregister(id);
            return Err(match error {
                mpsc::error::TrySendError::Full(_) => StudioError::Connection(
                    "engine transport is backed up; request not sent".to_string(),
                ),
                mpsc::error::TrySendError::Closed(_) => {
                    self.closed_error("engine transport is closed")
                }
            });
        }
        Ok(rx)
    }

    /// Queue a fire-and-forget frame (e.g. a `cancel`) whose response, if any,
    /// arrives on an already-registered route. No new route is created.
    pub(crate) fn send_raw(&self, req_tag: u8, body: Vec<u8>) -> StudioResult<()> {
        self.inner
            .cmd_tx
            .try_send(OutFrame { tag: req_tag, body })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => StudioError::Connection(
                    "engine transport is backed up; request not sent".to_string(),
                ),
                mpsc::error::TrySendError::Closed(_) => {
                    self.closed_error("engine transport is closed")
                }
            })
    }

    /// Drop the response route for `id`; the demux task will discard any further
    /// frames it carries.
    pub(crate) fn unregister(&self, id: &str) {
        self.inner
            .routes
            .lock()
            .expect("routes poisoned")
            .remove(id);
    }

    /// Engine-liveness probe: `ping` → `pong`. Never touches the target
    /// database, so it is a pure transport/engine health check.
    pub async fn ping(&self) -> StudioResult<()> {
        let id = self.next_request_id();
        let body = sqlly_remote::engine_protocol::layouts::encode_header_only(&id);
        let mut rx = self.open(REQUEST_TAG_PING, &id, body)?;
        // Ping is answered by exactly one frame (the engine sends a bare
        // `pong`, never `request_started`), so a single deadline-bounded read
        // decides it.
        let received = match crate::remote::rt::timeout(PING_DEADLINE, rx.recv()).await {
            Ok(received) => received,
            Err(_) => {
                self.unregister(&id);
                return Err(StudioError::Connection(format!(
                    "engine did not answer ping within {}s",
                    PING_DEADLINE.as_secs()
                )));
            }
        };
        let result = match received {
            Some(frame) if frame.tag == FRAME_TAG_PONG => Ok(()),
            Some(frame) if frame.tag == FRAME_TAG_REQUEST_FAILED => {
                Err(decode_failed_studio_error(&frame.body))
            }
            Some(frame) => Err(StudioError::Connection(format!(
                "unexpected frame 0x{:02x} answering ping",
                frame.tag
            ))),
            None => Err(self.closed_error("engine closed before answering ping")),
        };
        self.unregister(&id);
        result
    }

    /// Send a single-shot request whose id is embedded by `build_body`, then
    /// return the body of the first response frame carrying `expected_tag`. A
    /// `request_failed` frame is mapped to [`StudioError::Connection`]. Stray
    /// non-terminal frames (e.g. `request_started`) are skipped.
    pub(crate) async fn report_request<F>(
        &self,
        req_tag: u8,
        expected_tag: u8,
        build_body: F,
    ) -> StudioResult<Vec<u8>>
    where
        F: FnOnce(&str) -> Vec<u8>,
    {
        let id = self.next_request_id();
        let body = build_body(&id);
        let mut rx = self.open(req_tag, &id, body)?;
        let result = loop {
            // Idle deadline, not total: each arriving frame (request_started,
            // interim progress) re-arms it, so only a truly silent engine
            // trips it.
            let received = match crate::remote::rt::timeout(REPORT_IDLE_DEADLINE, rx.recv()).await {
                Ok(received) => received,
                Err(_) => {
                    break Err(StudioError::Connection(format!(
                        "engine went silent for {}s before returning a report",
                        REPORT_IDLE_DEADLINE.as_secs()
                    )));
                }
            };
            match received {
                Some(frame) if frame.tag == expected_tag => break Ok(frame.body),
                Some(frame) if frame.tag == FRAME_TAG_REQUEST_FAILED => {
                    break Err(decode_failed_studio_error(&frame.body));
                }
                // request_started and any other non-matching, non-terminal frame
                // for this id: keep waiting for the report.
                Some(_) => continue,
                None => {
                    break Err(self.closed_error("engine closed before returning a report"));
                }
            }
        };
        self.unregister(&id);
        result
    }
}

/// One decided event from the demux `select!`. Reducing the select to a value
/// keeps the transport borrowed by at most one future at a time (only the
/// `recv` arm borrows it), so the follow-up write below is legal — mirroring the
/// bridge pump.
enum DemuxEvent {
    /// A frame to write to the transport.
    Out(OutFrame),
    /// An inbound frame to route to its request.
    Frame(RawFrame),
    /// The command queue or transport closed; stop the task.
    Stop,
}

/// The background demultiplexer: owns the transport, writes queued request
/// frames, reads every inbound frame, and routes each to the channel registered
/// for the request id in its body. Runs until every [`RemoteEngine`] handle is
/// dropped (command queue closes) or the transport closes.
async fn run_demux<T: FramedTransport>(
    mut client: EngineClient<T>,
    mut cmd_rx: mpsc::Receiver<OutFrame>,
    routes: Arc<StdMutex<HashMap<String, RouteSender>>>,
    closed: Arc<AtomicBool>,
    close_reason: Arc<StdMutex<Option<String>>>,
    closed_tx: tokio::sync::watch::Sender<bool>,
    ready_tx: tokio::sync::watch::Sender<Option<EngineCapabilities>>,
) {
    // Record the transport error that stops the task (first one wins), so
    // closed-session consumers can surface its typed relay code rather than a
    // generic "closed" message. A clean EOF records nothing.
    let record = |err: String| {
        close_reason
            .lock()
            .expect("close reason poisoned")
            .get_or_insert(err);
    };
    loop {
        let event = tokio::select! {
            biased;

            out = cmd_rx.recv() => match out {
                Some(frame) => DemuxEvent::Out(frame),
                None => DemuxEvent::Stop,
            },

            // Only this arm borrows `client`, and that borrow ends when the
            // `select!` completes — so the `send_request` below is legal.
            inbound = client.transport_mut().recv() => match inbound {
                Ok(Some(frame)) => DemuxEvent::Frame(frame),
                Ok(None) => DemuxEvent::Stop,
                Err(e) => {
                    record(e.to_string());
                    DemuxEvent::Stop
                }
            },
        };

        match event {
            DemuxEvent::Out(frame) => {
                if let Err(e) = client.send_request(frame.tag, frame.body).await {
                    record(e.to_string());
                    break;
                }
            }
            DemuxEvent::Frame(frame) => {
                // The engine's Ready announcement is the one idless frame with
                // content worth checking: a protocol-version mismatch means
                // every layout assumption downstream is suspect, so close the
                // session with a diagnostic naming BOTH versions instead of
                // failing later with baffling decode errors.
                if frame.tag == sqlly_remote::engine_protocol::FRAME_TAG_READY {
                    let mut r = BinaryReader::new(&frame.body);
                    if let Ok(remote_version) = r.read_u32_le("protocol_version")
                        && remote_version != sqlly_remote::engine_protocol::PROTOCOL_VERSION
                    {
                        record(format!(
                            "engine protocol version mismatch: the engine speaks v{remote_version}, this app speaks v{}; update the older side",
                            sqlly_remote::engine_protocol::PROTOCOL_VERSION
                        ));
                        break;
                    }
                    // Newer engines append a capability list; older ones send
                    // the bare version. A malformed list is logged and
                    // treated as "advertises nothing", never as a partial set.
                    let capabilities = match sqlly_remote::engine_protocol::decode_ready_body(
                        &frame.body,
                    ) {
                        Ok(info) => info.capabilities,
                        Err(error) => {
                            tracing::warn!(
                                %error,
                                "engine ready frame carried a malformed capability list; treating it as advertising none"
                            );
                            EngineCapabilities::default()
                        }
                    };
                    ready_tx.send_replace(Some(capabilities));
                    continue;
                }
                if let Some(id) = frame.id() {
                    // Clone the sender out of the lock so we never hold the mutex
                    // across the (non-blocking) try_send.
                    let route = routes.lock().expect("routes poisoned").get(&id).cloned();
                    if let Some(tx) = route {
                        match tx.try_send(frame) {
                            Ok(()) => {}
                            Err(mpsc::error::TrySendError::Full(_)) => {
                                // The peer outran the flow-control window by
                                // thousands of frames: hostile or broken.
                                // Close just this route — its consumer sees
                                // the stream end and reports one terminal
                                // error; other requests keep streaming.
                                tracing::warn!(
                                    id = %id,
                                    limit = MAX_ROUTE_BUFFERED_FRAMES,
                                    "route buffer overflow; closing the request's stream"
                                );
                                routes.lock().expect("routes poisoned").remove(&id);
                            }
                            Err(mpsc::error::TrySendError::Closed(_)) => {
                                routes.lock().expect("routes poisoned").remove(&id);
                            }
                        }
                    }
                    // A frame for an unregistered id (already-completed request,
                    // or a stray cancel response) is dropped.
                }
                // An idless frame (e.g. `Ready`) is not attributable to a
                // request and is dropped from the per-id demux.
            }
            DemuxEvent::Stop => break,
        }
    }

    // Fail fast for any request opened during shutdown, and drop every route so
    // in-flight consumers observe their channel closing.
    closed.store(true, Ordering::SeqCst);
    routes.lock().expect("routes poisoned").clear();
    // Announce the loss to anyone watching the session lifecycle.
    let _ = closed_tx.send(true);
}

// ---------------------------------------------------------------------------
// Response body decoders (must mirror engine.rs encoders)
// ---------------------------------------------------------------------------

/// Decode a `request_failed` body (`[id][error]`, newer engines append a
/// trailing `[code]`) into its error message. Falls back to a generic message
/// when the body is malformed.
pub(crate) fn decode_failed_error(body: &[u8]) -> String {
    let mut r = BinaryReader::new(body);
    let _id = r.read_string("id");
    r.read_string("error")
        .unwrap_or_else(|_| "engine reported an unspecified failure".to_string())
}

/// Decode a `request_failed` body into a typed [`StudioError`], using the
/// machine-readable code newer engines append after the message. Older
/// engines send no code; their failures stay generic connection errors.
pub(crate) fn decode_failed_studio_error(body: &[u8]) -> StudioError {
    let mut r = BinaryReader::new(body);
    let _id = r.read_string("id");
    let message = r
        .read_string("error")
        .unwrap_or_else(|_| "engine reported an unspecified failure".to_string());
    match r.read_string("code").ok().as_deref() {
        Some("unsupported") => StudioError::CapabilityNotSupported(message),
        _ => StudioError::Connection(message),
    }
}

/// Decode a `[id][json_len][json]` report body's JSON payload as a UTF-8
/// string. The engine encodes the JSON as a length-prefixed string, so this is
/// exactly two `read_string`s.
pub(crate) fn decode_report_json(body: &[u8]) -> StudioResult<String> {
    let mut r = BinaryReader::new(body);
    r.read_string("id").map_err(map_proto)?;
    r.read_string("payload").map_err(map_proto)
}

/// Map a framing/decode [`ProtocolError`] into a [`StudioError`].
fn map_proto(err: sqlly_remote::engine_protocol::ProtocolError) -> StudioError {
    StudioError::Connection(transport_message(&format!("engine transport: {err}")))
}

/// The user-facing text for a raw transport-layer error message: a flattened
/// relay error code (see `sqlly_remote::transport::RemoteError::relay_code_in`) maps to
/// the same copy session establishment shows; anything else passes through.
pub(crate) fn transport_message(raw: &str) -> String {
    #[cfg(not(target_arch = "wasm32"))]
    {
        match sqlly_remote::transport::RemoteError::relay_code_in(raw) {
            Some(code) => {
                sqlly_remote::transport::RemoteError::relay_user_message(code).to_string()
            }
            None => raw.to_string(),
        }
    }
    // The browser rides a relay proxy socket, never the relay transport, so
    // there is no relay code to translate.
    #[cfg(target_arch = "wasm32")]
    {
        raw.to_string()
    }
}

// ---------------------------------------------------------------------------
// Request body encoders (must mirror engine.rs decoders)
// ---------------------------------------------------------------------------

/// The connection layer of a binary request body: a length-prefixed JSON
/// `EngineConnection`. Secrets resolve through the shared
/// [`sqlly_core::secret_resolver::resolve_secret`] boundary so the payload the
/// engine deserializes matches a direct connect, and a missing local secret
/// fails HERE with an authentication error naming the key or account — not as a
/// baffling server-side login failure with an empty password.
pub(crate) fn engine_connection_json(profile: &ConnectionProfile) -> StudioResult<String> {
    // The engine payload has no Sentinel fields: the far side would dial the
    // first sentinel as if it were the data node. Refuse rather than misroute.
    if profile.redis_sentinel.is_some() {
        return Err(StudioError::CapabilityNotSupported(
            "Redis Sentinel connections are not available through a relay yet; connect directly"
                .to_string(),
        ));
    }
    // A connection listed by a relay proxy: the relay's engine opens its own
    // catalog entry by id and ignores everything else, and this side holds
    // no secret to send. Without the id the engine refuses every request.
    if profile.provenance == sqlly_core::ConnectionProvenance::Proxied {
        let value = serde_json::json!({
            "proxy_profile": profile.id.0,
            "server": profile.server,
            "port": profile.port,
            "database": profile.database,
            "database_type": profile.database_type,
            "statement_timeout_seconds": profile.statement_timeout_seconds,
            "auth": "sql",
            "username": "",
            "password": "",
            "encrypt": profile.encrypt,
            "trust_certificate": profile.trust_server_certificate,
            "timeout_seconds": profile.timeout_seconds,
            "block_all_but_select": profile.block_all_but_select,
            "enforce_read_only": profile.enforce_read_only,
            "auto_wrap_rollback": profile.auto_wrap_rollback,
            "confirm_data_changes": profile.confirm_data_changes,
        });
        return Ok(value.to_string());
    }
    let (auth, username, password, access_token) = match &profile.authentication {
        AuthenticationType::SqlLogin {
            username,
            secret_ref,
        } => ("sql", username.clone(), secret_ref.resolve()?, None),
        AuthenticationType::EntraBrowser => (
            "entra-browser",
            String::new(),
            String::new(),
            profile.runtime_entra_access_token.clone(),
        ),
        AuthenticationType::EntraDeviceCode => (
            "entra-device-code",
            String::new(),
            String::new(),
            profile.runtime_entra_access_token.clone(),
        ),
        AuthenticationType::Integrated => ("integrated", String::new(), String::new(), None),
        AuthenticationType::AwsRdsIam { username, .. } => (
            "aws-rds-iam",
            username.clone(),
            String::new(),
            profile.runtime_cloud_iam_token.clone(),
        ),
        AuthenticationType::GcpCloudSqlIam { username } => (
            "gcp-cloud-sql-iam",
            username.clone(),
            String::new(),
            profile.runtime_cloud_iam_token.clone(),
        ),
        // The token rides in the password slot; the engine rebuilds an
        // AuthToken profile from the `auth` tag.
        AuthenticationType::AuthToken { secret_ref } => {
            ("auth-token", String::new(), secret_ref.resolve()?, None)
        }
    };

    let value = serde_json::json!({
        "server": profile.server,
        "port": profile.port,
        "database": profile.database,
        // Engine family for the engine's multi-engine dispatch. Old engines
        // ignore the extra field and keep treating the profile as SQL Server.
        "database_type": profile.database_type,
        "statement_timeout_seconds": profile.statement_timeout_seconds,
        "auth": auth,
        "username": username,
        "password": password,
        "access_token": access_token,
        "encrypt": profile.encrypt,
        "trust_certificate": profile.trust_server_certificate,
        "timeout_seconds": profile.timeout_seconds,
        "block_all_but_select": profile.block_all_but_select,
        // Database-enforced read-only mode for AI/agent/MCP-composed SQL.
        // Old engines ignore the extra field and apply the parser gate only.
        "enforce_read_only": profile.enforce_read_only,
        "auto_wrap_rollback": profile.auto_wrap_rollback,
        // Opts the engine's validate_sql into unconstrained-mutation warnings
        // (off by default). Old engines ignore the extra field.
        "confirm_data_changes": profile.confirm_data_changes,
    });
    Ok(value.to_string())
}

/// Prefix shared by every binary request body: `[u16 layout_version][string
/// id]`. Owned by the protocol crate; re-exported here for the per-operation
/// encoders that append fields to it.
pub(crate) fn request_header(id: &str) -> BinaryWriter {
    sqlly_remote::engine_protocol::layouts::request_header(id)
}

/// The wire spelling of a [`Dialect`] accepted by the engine's `parse_dialect`.
pub(crate) fn dialect_str(dialect: Dialect) -> &'static str {
    match dialect {
        Dialect::SqlServer => "sqlserver",
        Dialect::Postgres => "postgres",
        Dialect::MySql => "mysql",
        Dialect::Oracle => "oracle",
        Dialect::Sqlite => "sqlite",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Duration;

    use async_trait::async_trait;
    use sqlly_core::{SecretProvider, SecretRef};
    use sqlly_remote::engine_protocol::{
        FRAME_TAG_CATALOG_SYNC_REPORT, ProtocolError, REQUEST_TAG_SYNC_CATALOG, encode_id_prefix,
    };

    /// Body helper mirroring the engine's `request_failed` encoder:
    /// `[id][message]` plus an optional trailing `[code]`.
    fn failed_body(message: &str, code: Option<&str>) -> Vec<u8> {
        let mut body = encode_id_prefix("req-1").expect("id");
        body.extend_from_slice(&encode_id_prefix(message).expect("message"));
        if let Some(code) = code {
            body.extend_from_slice(&encode_id_prefix(code).expect("code"));
        }
        body
    }

    /// The engine payload cannot carry Sentinel settings, so a Sentinel
    /// profile is refused instead of being dialed at its first sentinel.
    #[test]
    fn engine_connection_json_refuses_redis_sentinel_profiles() {
        let mut profile = ConnectionProfile::sql_login(
            "cache",
            "10.0.0.1",
            "",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: String::new(),
            },
        );
        profile.database_type = sqlly_core::DatabaseType::Redis;
        profile.redis_sentinel = Some(sqlly_core::RedisSentinelConfig {
            master_name: "mymaster".into(),
            sentinels: vec!["10.0.0.1:26379".into()],
            ..Default::default()
        });
        let error = engine_connection_json(&profile).unwrap_err();
        assert!(
            matches!(&error, StudioError::CapabilityNotSupported(t) if t.contains("Sentinel")),
            "{error:?}"
        );
    }

    /// A relay-proxied profile names its catalog entry and sends no secret —
    /// even when its (secret-free) auth would otherwise need resolving.
    #[test]
    fn engine_connection_json_names_the_proxied_connection() {
        let mut profile = ConnectionProfile::sql_login(
            "local",
            "localhost",
            "sa",
            SecretRef {
                provider: SecretProvider::MacOsKeychain,
                key: "not-on-this-machine".into(),
            },
        );
        profile.provenance = sqlly_core::ConnectionProvenance::Proxied;
        let json: serde_json::Value =
            serde_json::from_str(&engine_connection_json(&profile).unwrap()).unwrap();
        assert_eq!(json["proxy_profile"], profile.id.0.to_string());
        assert_eq!(json["password"], "");
        assert_eq!(json["database_type"], "SqlServer");

        profile.provenance = sqlly_core::ConnectionProvenance::Manual;
        assert!(
            engine_connection_json(&profile).is_err(),
            "normal profiles still resolve"
        );
    }

    #[test]
    fn engine_connection_json_fails_closed_on_missing_keychain_secret() {
        let account_id = uuid::Uuid::new_v4();
        let profile = ConnectionProfile::sql_login(
            "test",
            "localhost",
            "sa",
            SecretRef {
                provider: SecretProvider::MacOsKeychain,
                key: account_id.to_string(),
            },
        );
        let err = engine_connection_json(&profile).unwrap_err();
        assert!(matches!(err, StudioError::Authentication(_)));
    }

    #[test]
    fn engine_connection_json_in_memory_includes_password() {
        let profile = ConnectionProfile::sql_login(
            "test",
            "localhost",
            "sa",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: "pw".into(),
            },
        );
        let json = engine_connection_json(&profile).unwrap();
        assert!(json.contains("pw"));
    }

    #[test]
    fn engine_connection_json_carries_the_read_only_enforcement_flag() {
        let mut profile = ConnectionProfile::sql_login(
            "test",
            "localhost",
            "sa",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: "pw".into(),
            },
        );
        let json = engine_connection_json(&profile).unwrap();
        assert!(json.contains("\"enforce_read_only\":false"), "{json}");
        profile.enforce_read_only = true;
        let json = engine_connection_json(&profile).unwrap();
        assert!(json.contains("\"enforce_read_only\":true"), "{json}");
    }

    #[test]
    fn unsupported_code_maps_to_capability_error() {
        let error = decode_failed_studio_error(&failed_body(
            "no driver yet for DuckDB",
            Some("unsupported"),
        ));
        assert!(
            matches!(error, StudioError::CapabilityNotSupported(ref m) if m.contains("DuckDB")),
            "{error:?}"
        );
    }

    #[test]
    fn missing_or_other_codes_stay_connection_errors() {
        // Old engines: no trailing code at all.
        let legacy = decode_failed_studio_error(&failed_body("boom", None));
        assert!(
            matches!(legacy, StudioError::Connection(ref m) if m == "boom"),
            "{legacy:?}"
        );
        // Known-but-unmapped and unknown codes both stay generic.
        for code in ["query", "catalog", "something-future"] {
            let error = decode_failed_studio_error(&failed_body("boom", Some(code)));
            assert!(
                matches!(error, StudioError::Connection(ref m) if m == "boom"),
                "{code}: {error:?}"
            );
        }
    }

    /// A fake engine transport that **withholds every response until it has
    /// observed `gate` distinct requests**, then answers them all at once in
    /// reverse arrival order.
    ///
    /// This models an engine that cannot make progress on the first request
    /// until a *second, concurrent* request has also reached it. Under the
    /// previous design — one `EngineClient` behind an async mutex, where a
    /// caller holds the lock across its blocking read — the second request could
    /// never be sent (the first caller owns the lock while waiting), so `gate`
    /// was never reached and the two requests deadlocked. The demux task sends
    /// both requests without holding the transport across any read, so both
    /// complete.
    struct GatedEngine {
        gate: usize,
        seen: Vec<String>,
        resp_tx: mpsc::UnboundedSender<RawFrame>,
        resp_rx: mpsc::UnboundedReceiver<RawFrame>,
    }

    impl GatedEngine {
        fn new(gate: usize) -> Self {
            let (resp_tx, resp_rx) = mpsc::unbounded_channel();
            Self {
                gate,
                seen: Vec::new(),
                resp_tx,
                resp_rx,
            }
        }
    }

    #[async_trait]
    impl FramedTransport for GatedEngine {
        async fn recv(&mut self) -> Result<Option<RawFrame>, ProtocolError> {
            Ok(self.resp_rx.recv().await)
        }

        async fn send(&mut self, frame: &RawFrame) -> Result<(), ProtocolError> {
            if let Some(id) = frame.id() {
                self.seen.push(id);
                if self.seen.len() == self.gate {
                    // Answer every gated request, newest-first, to also prove the
                    // demux routes replies to the right id regardless of order.
                    for id in self.seen.iter().rev() {
                        let body = encode_id_prefix(id).expect("encode id");
                        let _ = self
                            .resp_tx
                            .send(RawFrame::new(FRAME_TAG_CATALOG_SYNC_REPORT, body));
                    }
                }
            }
            Ok(())
        }
    }

    /// Two overlapping requests must both complete even when the engine will not
    /// answer the first until the second has been sent — i.e. the shared session
    /// multiplexes them instead of serializing one behind the other.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn overlapping_requests_multiplex_instead_of_serializing() {
        let engine = RemoteEngine::new(GatedEngine::new(2));

        let a = engine.report_request(
            REQUEST_TAG_SYNC_CATALOG,
            FRAME_TAG_CATALOG_SYNC_REPORT,
            |id| encode_id_prefix(id).expect("encode id a"),
        );
        let b = engine.report_request(
            REQUEST_TAG_SYNC_CATALOG,
            FRAME_TAG_CATALOG_SYNC_REPORT,
            |id| encode_id_prefix(id).expect("encode id b"),
        );

        // Under the old single-lock model this join never resolves: the first
        // request holds the transport while blocked on its read, so the second
        // is never sent and the gate is never reached.
        let (ra, rb) = tokio::time::timeout(Duration::from_secs(5), async { tokio::join!(a, b) })
            .await
            .expect("overlapping requests must not deadlock");

        let ba = ra.expect("request A must complete");
        let bb = rb.expect("request B must complete");
        // Each reply is routed back to the request whose id it carries, so the
        // two bodies must carry two distinct ids.
        assert!(frame_id_of(&ba).is_some(), "reply A must carry an id");
        assert!(frame_id_of(&bb).is_some(), "reply B must carry an id");
        assert_ne!(
            frame_id_of(&ba),
            frame_id_of(&bb),
            "each request must receive its own distinct reply"
        );
    }

    /// Parse the leading id prefix from a report body (`[u32 len][id]`).
    fn frame_id_of(body: &[u8]) -> Option<String> {
        RawFrame::new(FRAME_TAG_CATALOG_SYNC_REPORT, body.to_vec()).id()
    }

    /// A peer that floods one request's route with unbounded non-terminal
    /// frames (ignoring all flow control), then serves a ping normally.
    struct FloodTransport {
        resp_tx: mpsc::UnboundedSender<RawFrame>,
        resp_rx: mpsc::UnboundedReceiver<RawFrame>,
        flooded: bool,
        flood_count: usize,
    }

    impl FloodTransport {
        fn new(flood_count: usize) -> Self {
            let (resp_tx, resp_rx) = mpsc::unbounded_channel();
            Self {
                resp_tx,
                resp_rx,
                flooded: false,
                flood_count,
            }
        }
    }

    #[async_trait]
    impl FramedTransport for FloodTransport {
        async fn recv(&mut self) -> Result<Option<RawFrame>, ProtocolError> {
            Ok(self.resp_rx.recv().await)
        }

        async fn send(&mut self, frame: &RawFrame) -> Result<(), ProtocolError> {
            if frame.tag == REQUEST_TAG_PING {
                // Request bodies open with the u16 layout version, then the
                // length-prefixed id.
                let body = &frame.body[2..];
                let len = u32::from_le_bytes(body[..4].try_into().expect("id len")) as usize;
                let id = String::from_utf8(body[4..4 + len].to_vec()).expect("ping id utf8");
                let _ = self.resp_tx.send(RawFrame::new(
                    FRAME_TAG_PONG,
                    encode_id_prefix(&id).expect("pong id"),
                ));
            } else if !self.flooded {
                self.flooded = true;
                let id = frame.id().expect("request carries an id");
                for _ in 0..self.flood_count {
                    let _ = self.resp_tx.send(RawFrame::new(
                        sqlly_remote::engine_protocol::FRAME_TAG_EVENT,
                        encode_id_prefix(&id).expect("flood id"),
                    ));
                }
            }
            Ok(())
        }
    }

    /// A route flooded past its bound is closed — its consumer's stream ends
    /// after exactly the bounded number of frames — while a second request on
    /// the same session still completes. The demux never blocks and never
    /// buffers beyond the cap.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn route_flood_closes_only_the_hostile_stream() {
        let flood = MAX_ROUTE_BUFFERED_FRAMES + 512;
        let engine = RemoteEngine::new(FloodTransport::new(flood));

        let id = engine.next_request_id();
        let mut rx = engine
            .open(
                REQUEST_TAG_SYNC_CATALOG,
                &id,
                encode_id_prefix(&id).expect("encode id"),
            )
            .expect("open flood route");

        // The rest of the session stays healthy while the flood pours in.
        tokio::time::timeout(Duration::from_secs(5), engine.ping())
            .await
            .expect("ping must not be starved by the flood")
            .expect("ping succeeds");

        // Drain the hostile route without any consumer backpressure games:
        // it yields at most the bound, then ends (route was closed).
        let mut received = 0usize;
        while tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("drain must not hang")
            .is_some()
        {
            received += 1;
        }
        assert!(
            received <= MAX_ROUTE_BUFFERED_FRAMES,
            "route must never buffer past its bound (got {received})"
        );
        assert!(
            received >= MAX_ROUTE_BUFFERED_FRAMES / 2,
            "sanity: the flood actually flowed (got {received})"
        );
    }

    /// A transport that answers every `recv` with the flattened form of a
    /// mid-session relay error — exactly what a `PeerSession` produces when
    /// the relay reports `DEST_OFFLINE` after the session was established.
    struct RelayDropTransport;

    #[async_trait]
    impl FramedTransport for RelayDropTransport {
        async fn recv(&mut self) -> Result<Option<RawFrame>, ProtocolError> {
            Err(ProtocolError::decode(
                "relay reported error: DestOffline".to_string(),
            ))
        }

        async fn send(&mut self, _frame: &RawFrame) -> Result<(), ProtocolError> {
            Ok(())
        }
    }

    /// A mid-session relay error must stay typed across the transport
    /// boundary: a request that observes the dead session gets the relay
    /// code's user copy, not a generic "engine closed" message.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mid_session_relay_error_surfaces_its_copy() {
        let engine = RemoteEngine::new(RelayDropTransport);

        let err = tokio::time::timeout(
            Duration::from_secs(5),
            engine.report_request(
                REQUEST_TAG_SYNC_CATALOG,
                FRAME_TAG_CATALOG_SYNC_REPORT,
                |id| encode_id_prefix(id).expect("encode id"),
            ),
        )
        .await
        .expect("request must resolve once the transport dies")
        .expect_err("a dead transport cannot return a report");

        let expected = sqlly_remote::transport::RemoteError::relay_user_message(
            sqlly_wire::RelayErrorCode::DestOffline,
        );
        match err {
            StudioError::Connection(message) => assert_eq!(message, expected),
            other => panic!("expected Connection error, got {other:?}"),
        }
    }

    /// A clean EOF (bridge shut down without an error frame) keeps the
    /// generic closed message — no relay copy is invented.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn clean_eof_keeps_the_generic_closed_message() {
        struct EofTransport;

        #[async_trait]
        impl FramedTransport for EofTransport {
            async fn recv(&mut self) -> Result<Option<RawFrame>, ProtocolError> {
                Ok(None)
            }
            async fn send(&mut self, _frame: &RawFrame) -> Result<(), ProtocolError> {
                Ok(())
            }
        }

        let engine = RemoteEngine::new(EofTransport);
        let err = tokio::time::timeout(
            Duration::from_secs(5),
            engine.report_request(
                REQUEST_TAG_SYNC_CATALOG,
                FRAME_TAG_CATALOG_SYNC_REPORT,
                |id| encode_id_prefix(id).expect("encode id"),
            ),
        )
        .await
        .expect("request must resolve on EOF")
        .expect_err("EOF cannot return a report");
        match err {
            StudioError::Connection(message) => {
                assert!(message.contains("closed"), "got: {message}");
                assert!(!message.contains("relay"), "got: {message}");
            }
            other => panic!("expected Connection error, got {other:?}"),
        }
    }

    /// The closed signal flips to `true` the moment the transport dies —
    /// without any request in flight — so a host can observe connection loss
    /// from the session lifecycle instead of the next failed call.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn closed_signal_fires_when_the_transport_dies() {
        let engine = RemoteEngine::new(RelayDropTransport);
        let mut closed = engine.closed_signal();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !*closed.borrow() {
                if closed.changed().await.is_err() {
                    break;
                }
            }
        })
        .await
        .expect("the closed signal must fire once the transport errors");
        assert!(*closed.borrow());
    }
}
