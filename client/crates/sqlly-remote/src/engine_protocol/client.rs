//! `EngineClient<T>`: a transport-agnostic engine *client* that drives the
//! request/response protocol over any framed transport.
//!
//! This is the type Phase 2's remote client and the bridge will build on. It
//! deliberately knows nothing about engine request *dispatch* or the concrete
//! [`RawFrame`] body layouts — those stay in `sqlly-cli`. Its job is the
//! transport-level lifecycle:
//!
//! * send request frames (the caller supplies the tag + already-encoded body,
//!   which embeds the request id),
//! * demultiplex the inbound frame stream back to the requesting id,
//! * recognize terminal frames so a response stream has a definite end,
//! * enforce deadlines and support cancellation.

use std::collections::{HashMap, HashSet, VecDeque};
#[cfg(not(target_arch = "wasm32"))]
use std::future::Future;

use async_trait::async_trait;
#[cfg(not(target_arch = "wasm32"))]
use tokio::io::{AsyncRead, AsyncWrite};
#[cfg(not(target_arch = "wasm32"))]
use tokio::time::Instant;

#[cfg(not(target_arch = "wasm32"))]
use crate::engine_protocol::constants::{
    FRAME_TAG_PONG, FRAME_TAG_SHUTDOWN_ACK, is_terminal_frame_tag,
};
use crate::engine_protocol::error::ProtocolError;
use crate::engine_protocol::framing::RawFrame;
#[cfg(not(target_arch = "wasm32"))]
use crate::engine_protocol::framing::{read_length_prefixed, write_length_prefixed};

/// Errors surfaced while driving [`EngineClient`].
#[derive(Debug, thiserror::Error)]
pub enum EngineClientError {
    /// A framing/transport codec error.
    #[error(transparent)]
    Protocol(#[from] ProtocolError),

    /// The transport reached EOF before the in-flight request produced a
    /// terminal frame.
    #[error("engine transport closed before response for request '{0}' completed")]
    TransportClosed(String),

    /// The per-response deadline elapsed.
    #[error("timed out waiting for response to request '{0}'")]
    Timeout(String),

    /// The caller's cancellation future fired while awaiting a response.
    #[error("wait for response to request '{0}' was cancelled")]
    Cancelled(String),
}

/// An ordered, framed, bidirectional transport of [`RawFrame`]s.
///
/// Implementors handle their own length framing; a `recv` returns the next
/// decoded frame (or `None` at EOF), and `send` writes one frame.
#[async_trait]
pub trait FramedTransport: Send {
    async fn recv(&mut self) -> Result<Option<RawFrame>, ProtocolError>;
    async fn send(&mut self, frame: &RawFrame) -> Result<(), ProtocolError>;
}

/// A [`FramedTransport`] over any duplex byte stream (e.g. a TCP/TLS socket, or
/// a `tokio::io::DuplexStream` in tests) using the length-prefixed envelope.
#[cfg(not(target_arch = "wasm32"))]
pub struct AsyncFramedTransport<S> {
    stream: S,
}

#[cfg(not(target_arch = "wasm32"))]
impl<S> AsyncFramedTransport<S> {
    pub fn new(stream: S) -> Self {
        Self { stream }
    }

    pub fn into_inner(self) -> S {
        self.stream
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[async_trait]
impl<S> FramedTransport for AsyncFramedTransport<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    async fn recv(&mut self) -> Result<Option<RawFrame>, ProtocolError> {
        match read_length_prefixed(&mut self.stream).await? {
            Some(payload) => Ok(Some(RawFrame::from_payload(&payload)?)),
            None => Ok(None),
        }
    }

    async fn send(&mut self, frame: &RawFrame) -> Result<(), ProtocolError> {
        write_length_prefixed(&mut self.stream, &frame.to_payload()).await
    }
}

/// Whether a frame ends the response stream for its request. Beyond the three
/// documented terminal tags, `Pong` answers a ping and `ShutdownAck` answers a
/// shutdown — both are single-shot and terminate their request too.
#[cfg(not(target_arch = "wasm32"))]
fn ends_response(tag: u8) -> bool {
    is_terminal_frame_tag(tag) || tag == FRAME_TAG_PONG || tag == FRAME_TAG_SHUTDOWN_ACK
}

/// Drives the engine request/response protocol over a [`FramedTransport`].
///
/// The client multiplexes many in-flight requests over a single transport: a
/// frame read while waiting for request A that actually belongs to request B is
/// buffered until B is awaited. Response streaming is per-id and ends at the
/// first frame for which `ends_response` is true.
///
/// On wasm32 only the transport-level half is available (`send_request`,
/// `send_frame`, `transport_mut`): the browser's remote engine demultiplexes
/// in its own task, and the deadline-driven readers below need a tokio timer.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub struct EngineClient<T> {
    transport: T,
    /// Frames read for an id that the caller has not yet consumed.
    buffered: HashMap<String, VecDeque<RawFrame>>,
    /// Ids in `buffered`, oldest first — the eviction order when the
    /// hostile-peer caps below are exceeded.
    buffered_order: VecDeque<String>,
    /// Total bytes across every buffered frame body.
    buffered_bytes: usize,
    /// Ids whose stream has already produced its terminal frame.
    closed: HashSet<String>,
    /// `closed` in insertion order, so it stays bounded.
    closed_order: VecDeque<String>,
    /// Monotonic counter backing [`next_request_id`](Self::next_request_id).
    seq: u64,
}

/// Hostile-peer memory bounds for cross-id buffering. A peer can address
/// frames to ids this client never issued; without caps that grows without
/// limit (~16 MiB per frame). When a cap is exceeded the STALEST buffered id
/// is discarded whole — deterministic, and it never starves the id currently
/// being awaited (frames for that id are returned, not buffered).
const MAX_BUFFERED_IDS: usize = 64;
/// Total byte cap across all buffered frame bodies.
const MAX_BUFFERED_BYTES: usize = 64 * 1024 * 1024;
/// Cap on remembered closed ids (late frames for an evicted id are simply
/// buffered again and age out through the caps above).
const MAX_CLOSED_IDS: usize = 1024;

impl<T: FramedTransport> EngineClient<T> {
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            buffered: HashMap::new(),
            buffered_order: VecDeque::new(),
            buffered_bytes: 0,
            closed: HashSet::new(),
            closed_order: VecDeque::new(),
            seq: 0,
        }
    }

    /// Allocate a fresh, process-unique request id of the form `req-N`.
    pub fn next_request_id(&mut self) -> String {
        self.seq += 1;
        format!("req-{}", self.seq)
    }

    /// Send a request frame. `body` must already encode the request id (via
    /// [`crate::engine_protocol::framing::encode_id_prefix`] or the request layout) so responses
    /// can be demultiplexed back.
    pub async fn send_request(&mut self, tag: u8, body: Vec<u8>) -> Result<(), ProtocolError> {
        self.transport.send(&RawFrame::new(tag, body)).await
    }

    /// Send a pre-built frame.
    pub async fn send_frame(&mut self, frame: &RawFrame) -> Result<(), ProtocolError> {
        self.transport.send(frame).await
    }

    /// Access the underlying transport (e.g. to close it).
    pub fn transport_mut(&mut self) -> &mut T {
        &mut self.transport
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl<T: FramedTransport> EngineClient<T> {
    /// Pull the next frame belonging to `id`, buffering unrelated frames for
    /// their own ids. Returns `Ok(None)` once the stream for `id` is closed
    /// (i.e. after its terminal frame has already been returned). An optional
    /// `deadline` bounds the wait.
    pub async fn next_frame(
        &mut self,
        id: &str,
        deadline: Option<Instant>,
    ) -> Result<Option<RawFrame>, EngineClientError> {
        if let Some(frame) = self.take_buffered(id) {
            return Ok(Some(frame));
        }
        if self.closed.contains(id) {
            return Ok(None);
        }
        loop {
            let frame = self.read_next(id, deadline).await?;
            match frame.id().as_deref() {
                Some(fid) if fid == id => {
                    if ends_response(frame.tag) {
                        self.mark_closed(id);
                    }
                    return Ok(Some(frame));
                }
                Some(other) => {
                    let other = other.to_string();
                    self.buffer_unrelated(other, frame);
                }
                // Idless frame (e.g. `Ready`): not attributable to a request,
                // so drop it from the per-id demux.
                None => {}
            }
        }
    }

    /// Collect every frame for `id` up to and including its terminal frame.
    pub async fn collect_response(
        &mut self,
        id: &str,
        deadline: Option<Instant>,
    ) -> Result<Vec<RawFrame>, EngineClientError> {
        let mut frames = Vec::new();
        while let Some(frame) = self.next_frame(id, deadline).await? {
            frames.push(frame);
        }
        Ok(frames)
    }

    /// Like [`next_frame`](Self::next_frame) but races the read against a
    /// caller-supplied cancellation future. If `cancel` resolves first, returns
    /// [`EngineClientError::Cancelled`]; the caller is then responsible for
    /// issuing a cancel request for `id`.
    pub async fn next_frame_cancellable<C>(
        &mut self,
        id: &str,
        deadline: Option<Instant>,
        cancel: C,
    ) -> Result<Option<RawFrame>, EngineClientError>
    where
        C: Future<Output = ()>,
    {
        if let Some(frame) = self.take_buffered(id) {
            return Ok(Some(frame));
        }
        if self.closed.contains(id) {
            return Ok(None);
        }
        tokio::pin!(cancel);
        loop {
            let frame = tokio::select! {
                biased;
                _ = &mut cancel => return Err(EngineClientError::Cancelled(id.to_string())),
                res = self.read_next(id, deadline) => res?,
            };
            match frame.id().as_deref() {
                Some(fid) if fid == id => {
                    if ends_response(frame.tag) {
                        self.mark_closed(id);
                    }
                    return Ok(Some(frame));
                }
                Some(other) => {
                    let other = other.to_string();
                    self.buffer_unrelated(other, frame);
                }
                None => {}
            }
        }
    }

    #[cfg(test)]
    fn buffer_stats(&self) -> (usize, usize, usize) {
        (self.buffered.len(), self.buffered_bytes, self.closed.len())
    }

    fn take_buffered(&mut self, id: &str) -> Option<RawFrame> {
        let frame = self.buffered.get_mut(id).and_then(VecDeque::pop_front)?;
        self.buffered_bytes = self.buffered_bytes.saturating_sub(frame.body.len());
        if self.buffered.get(id).is_some_and(VecDeque::is_empty) {
            self.buffered.remove(id);
            self.buffered_order.retain(|queued| queued != id);
        }
        if ends_response(frame.tag) {
            self.mark_closed(id);
        }
        Some(frame)
    }

    /// Buffer a frame addressed to an id other than the one being awaited,
    /// enforcing the hostile-peer caps by evicting stalest-first.
    fn buffer_unrelated(&mut self, id: String, frame: RawFrame) {
        self.buffered_bytes += frame.body.len();
        match self.buffered.entry(id.clone()) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                entry.get_mut().push_back(frame);
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(VecDeque::from([frame]));
                self.buffered_order.push_back(id);
            }
        }
        while self.buffered.len() > MAX_BUFFERED_IDS || self.buffered_bytes > MAX_BUFFERED_BYTES {
            let Some(oldest) = self.buffered_order.pop_front() else {
                break;
            };
            if let Some(dropped) = self.buffered.remove(&oldest) {
                let bytes: usize = dropped.iter().map(|f| f.body.len()).sum();
                self.buffered_bytes = self.buffered_bytes.saturating_sub(bytes);
            }
        }
    }

    /// Record a terminal frame for `id`, keeping the closed set bounded.
    fn mark_closed(&mut self, id: &str) {
        if self.closed.insert(id.to_string()) {
            self.closed_order.push_back(id.to_string());
            while self.closed_order.len() > MAX_CLOSED_IDS {
                if let Some(evicted) = self.closed_order.pop_front() {
                    self.closed.remove(&evicted);
                }
            }
        }
    }

    /// Read the next frame off the transport, honoring an optional deadline and
    /// mapping EOF to [`EngineClientError::TransportClosed`].
    async fn read_next(
        &mut self,
        id: &str,
        deadline: Option<Instant>,
    ) -> Result<RawFrame, EngineClientError> {
        let read = self.transport.recv();
        let frame = match deadline {
            Some(deadline) => match tokio::time::timeout_at(deadline, read).await {
                Ok(res) => res?,
                Err(_) => return Err(EngineClientError::Timeout(id.to_string())),
            },
            None => read.await?,
        };
        frame.ok_or_else(|| EngineClientError::TransportClosed(id.to_string()))
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use crate::engine_protocol::constants::{FRAME_TAG_EVENT, FRAME_TAG_REQUEST_COMPLETED};
    use crate::engine_protocol::framing::encode_id_prefix;
    use std::collections::VecDeque as ScriptQueue;
    use std::time::Duration;

    /// A transport that replays a scripted frame sequence.
    struct Scripted {
        frames: ScriptQueue<RawFrame>,
    }

    #[async_trait::async_trait]
    impl FramedTransport for Scripted {
        async fn recv(&mut self) -> Result<Option<RawFrame>, ProtocolError> {
            Ok(self.frames.pop_front())
        }
        async fn send(&mut self, _frame: &RawFrame) -> Result<(), ProtocolError> {
            Ok(())
        }
    }

    fn frame_for(id: &str, tag: u8, padding: usize) -> RawFrame {
        let mut body = encode_id_prefix(id).expect("id");
        body.extend(std::iter::repeat_n(0u8, padding));
        RawFrame::new(tag, body)
    }

    /// Transport whose `recv` never completes — drives timeout/cancel branches.
    struct NeverRecv;

    #[async_trait::async_trait]
    impl FramedTransport for NeverRecv {
        async fn recv(&mut self) -> Result<Option<RawFrame>, ProtocolError> {
            std::future::pending().await
        }
        async fn send(&mut self, _frame: &RawFrame) -> Result<(), ProtocolError> {
            Ok(())
        }
    }

    /// A past deadline while the transport recv is pending yields Timeout.
    #[tokio::test]
    async fn past_deadline_yields_timeout() {
        let mut client = EngineClient::new(NeverRecv);
        let past = Instant::now() - Duration::from_secs(1);
        let err = client
            .next_frame("slow", Some(past))
            .await
            .expect_err("must time out");
        assert!(
            matches!(err, EngineClientError::Timeout(ref id) if id == "slow"),
            "{err:?}"
        );
    }

    /// A ready cancel future wins over a pending recv.
    #[tokio::test]
    async fn cancel_future_ready_yields_cancelled() {
        let mut client = EngineClient::new(NeverRecv);
        let err = client
            .next_frame_cancellable("q1", None, std::future::ready(()))
            .await
            .expect_err("must cancel");
        assert!(
            matches!(err, EngineClientError::Cancelled(ref id) if id == "q1"),
            "{err:?}"
        );
    }

    /// A peer addressing frames to ids this client never issued cannot grow
    /// the cross-id buffer past its caps: stalest ids are evicted whole, the
    /// awaited id still gets its frame, and the byte/id counters stay bounded.
    #[tokio::test]
    async fn unknown_id_flood_is_bounded_by_id_and_byte_caps() {
        let mut frames = ScriptQueue::new();
        // 3x the id cap in unknown ids, each with a 64 KiB frame.
        for i in 0..(MAX_BUFFERED_IDS * 3) {
            frames.push_back(frame_for(&format!("evil-{i}"), FRAME_TAG_EVENT, 64 * 1024));
        }
        frames.push_back(frame_for("want", FRAME_TAG_REQUEST_COMPLETED, 0));

        let mut client = EngineClient::new(Scripted { frames });
        let frame = client
            .next_frame("want", None)
            .await
            .expect("read")
            .expect("wanted frame arrives despite the flood");
        assert_eq!(frame.tag, FRAME_TAG_REQUEST_COMPLETED);

        let (ids, bytes, _) = client.buffer_stats();
        assert!(ids <= MAX_BUFFERED_IDS, "{ids} buffered ids");
        assert!(bytes <= MAX_BUFFERED_BYTES, "{bytes} buffered bytes");
    }

    /// The byte cap evicts stalest-first even when the id count stays small.
    #[tokio::test]
    async fn byte_cap_evicts_stalest_ids_first() {
        let mut frames = ScriptQueue::new();
        // 5 ids x 16 MiB-ish frames = ~80 MiB, over the 64 MiB cap.
        for i in 0..5 {
            frames.push_back(frame_for(
                &format!("big-{i}"),
                FRAME_TAG_EVENT,
                15 * 1024 * 1024,
            ));
        }
        frames.push_back(frame_for("want", FRAME_TAG_REQUEST_COMPLETED, 0));

        let mut client = EngineClient::new(Scripted { frames });
        client
            .next_frame("want", None)
            .await
            .expect("read")
            .unwrap();

        let (ids, bytes, _) = client.buffer_stats();
        assert!(bytes <= MAX_BUFFERED_BYTES, "{bytes} buffered bytes");
        // The newest ids survive; the stalest were evicted.
        assert!(client.take_buffered("big-0").is_none(), "stalest evicted");
        assert!(client.take_buffered("big-4").is_some(), "newest kept");
        let _ = ids;
    }

    /// The closed-id memory is bounded: finishing far more requests than the
    /// cap keeps the set at the cap instead of growing forever.
    #[tokio::test]
    async fn closed_set_stays_bounded() {
        let mut frames = ScriptQueue::new();
        let total = MAX_CLOSED_IDS + 100;
        for i in 0..total {
            frames.push_back(frame_for(
                &format!("req-{i}"),
                FRAME_TAG_REQUEST_COMPLETED,
                0,
            ));
        }
        let mut client = EngineClient::new(Scripted { frames });
        for i in 0..total {
            let id = format!("req-{i}");
            // Drain to the terminal frame, closing the id.
            while client.next_frame(&id, None).await.expect("read").is_some() {}
        }
        let (_, _, closed) = client.buffer_stats();
        assert!(closed <= MAX_CLOSED_IDS, "{closed} closed ids retained");
    }
}
