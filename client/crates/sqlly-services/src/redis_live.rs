//! Redis live tools — the Profiler (MONITOR) and the Pub/Sub viewer.
//!
//! Each tool runs a [`LiveCapture`]: one tokio task holding a DEDICATED
//! Redis connection (the engine's `redis::live` lane — never the editor's
//! request/response lane, which refuses MONITOR/SUBSCRIBE) and pumping
//! parsed items into a bounded buffer. Stop is deterministic: cancelling
//! the token wakes the task immediately and dropping the task handle aborts
//! it; either way the connection inside the task is dropped and the socket
//! closes. Dropping the [`LiveCapture`] does both, so a closed view can
//! never leak a MONITOR or a subscriber connection.
//!
//! Pausing never stops the pump — the stream keeps draining so a busy
//! server's output cannot pile up in socket buffers; items are simply not
//! recorded while paused. Overflow drops the OLDEST entries and counts
//! them (`dropped`), which is exactly what the Profiler banner reports.

use std::sync::{Arc, Mutex, Weak};

use sqlly_core::{ConnectionProfile, DisplayMessage, StudioError, StudioResult};
use tokio_util::sync::CancellationToken;

use sqlly_engines::redis::live::{self, BoundedBuffer, MonitorFrame, StoredSize};
pub use sqlly_engines::redis::live::{
    MAX_STORED_MONITOR_TEXT_BYTES, MAX_STORED_PAYLOAD_BYTES, MonitorEntry, ProfilerFilter,
    PubSubMessage, PubSubPayload, SubscribeTarget, key_events_preset, keyspace_notification,
    parse_subscribe_targets,
};

/// Default ring capacity for the Profiler: 10,000 commands.
pub const DEFAULT_PROFILER_CAP: usize = 10_000;
/// Default ring capacity for the Pub/Sub viewer: 1,000 messages.
pub const DEFAULT_PUBSUB_CAP: usize = 1_000;
/// Memory ceiling for one capture's ring, whatever its entry cap: past it
/// the oldest entries are evicted (and counted as dropped). Each entry is
/// itself capped ([`MAX_STORED_PAYLOAD_BYTES`],
/// [`MAX_STORED_MONITOR_TEXT_BYTES`]), so even a 100,000-entry ring of
/// large messages stays within this.
pub const LIVE_RING_BYTE_BUDGET: usize = 64 * 1024 * 1024;
/// Default Profiler time box: MONITOR costs the server throughput on every
/// command, so a capture stops itself after five minutes unless the user
/// picked another limit.
pub const DEFAULT_PROFILER_TIME_BOX_SECS: u64 = 300;
/// Longest time box the Profiler accepts (one hour).
pub const MAX_PROFILER_TIME_BOX_SECS: u64 = 3_600;

pub fn time_box_reason_message(limit: std::time::Duration) -> DisplayMessage {
    let secs = limit.as_secs();
    if secs >= 60 && secs.is_multiple_of(60) {
        DisplayMessage::new(
            "services.redis.live.time_box_minutes",
            format!(
                "time box of {} min reached — the capture connection is closed",
                secs / 60
            ),
        )
        .with_arg("minutes", (secs / 60).to_string())
    } else {
        DisplayMessage::new(
            "services.redis.live.time_box_seconds",
            format!("time box of {secs} s reached — the capture connection is closed"),
        )
        .with_arg("seconds", secs.to_string())
    }
}

/// Legacy English fallback; localized callers use [`time_box_reason_message`].
pub fn time_box_reason(limit: std::time::Duration) -> String {
    time_box_reason_message(limit).into_fallback()
}

pub const STOPPED_REASON: &str = "stopped — the capture connection is closed";
pub const DISCONNECTED_REASON: &str =
    "the connection was disconnected — the capture connection is closed";

pub fn stopped_reason() -> DisplayMessage {
    DisplayMessage::new("services.redis.live.stopped", STOPPED_REASON)
}

pub fn disconnected_reason() -> DisplayMessage {
    DisplayMessage::new("services.redis.live.disconnected", DISCONNECTED_REASON)
}

/// Where a capture is in its lifecycle. `Ended` carries translation-neutral
/// presentation copy; resolve it at the UI render boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LiveStatus {
    Connecting,
    Live,
    Ended(DisplayMessage),
}

struct LiveState<T> {
    /// Entries are `Arc`s so a snapshot clones pointers under the lock and
    /// does its filtering after releasing it — the pump is never held up by
    /// the view.
    buffer: BoundedBuffer<Arc<T>>,
    status: LiveStatus,
    paused: bool,
    /// Items the pump recorded or discarded while paused (lines that do not
    /// parse are not counted).
    received: u64,
    /// Items ever pushed into the ring. The ring evicts oldest-first, so the
    /// entry at ring index `i` has sequence number `pushed - len + i` — a
    /// stable identity the view uses to keep its selection on the same
    /// entry while new ones arrive.
    pushed: u64,
}

impl<T> LiveState<T> {
    fn push(&mut self, item: T) {
        self.buffer.push(Arc::new(item));
        self.pushed += 1;
    }

    fn first_seq(&self) -> u64 {
        self.pushed - self.buffer.len() as u64
    }

    /// End the capture unless it already ended: the first reason wins, so
    /// a late stream-end or time box never overwrites "stopped".
    fn end(&mut self, reason: impl Into<DisplayMessage>) {
        if !matches!(self.status, LiveStatus::Ended(_)) {
            self.status = LiveStatus::Ended(reason.into());
        }
    }
}

/// A point-in-time read of a capture: up to `max` newest entries (oldest
/// first, each with its sequence number) plus the counters and status the
/// toolbar reports.
#[derive(Clone, Debug)]
pub struct LiveSnapshot<T> {
    pub entries: Vec<(u64, Arc<T>)>,
    pub status: LiveStatus,
    pub paused: bool,
    pub received: u64,
    /// Entries dropped-oldest on overflow since the capture started.
    pub dropped: u64,
    /// Entries currently buffered (before any display-side filtering).
    pub buffered: usize,
    pub cap: usize,
}

/// A running live capture. Clone-free by design: the view owns it, and
/// `Drop` stops the capture so every path — stop button, tab switch,
/// dialog close, app shutdown — closes the dedicated connection.
pub struct LiveCapture<T> {
    state: Arc<Mutex<LiveState<T>>>,
    cancel: CancellationToken,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl<T> LiveCapture<T> {
    /// Stop the capture now: cancel wakes the pump's `select!` at once and
    /// aborting the handle drops the connection even mid-read. Idempotent.
    /// Marks the status ended (without overwriting a real stream-end
    /// reason) so a stopped capture never still reads "live".
    pub fn stop(&self) {
        self.stop_with(stopped_reason());
    }

    fn stop_with(&self, reason: DisplayMessage) {
        // Status first: the pump checks it before going live, so a stop that
        // lands while the connection is opening can never be overwritten.
        lock(&self.state).end(reason);
        self.cancel.cancel();
        if let Some(task) = self
            .task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            task.abort();
        }
    }

    pub fn set_paused(&self, paused: bool) {
        lock(&self.state).paused = paused;
    }

    pub fn is_paused(&self) -> bool {
        lock(&self.state).paused
    }

    pub fn clear(&self) {
        lock(&self.state).buffer.clear();
    }

    /// Re-cap the ring; overflow beyond the new cap is evicted oldest-first.
    pub fn set_cap(&self, cap: usize) {
        lock(&self.state).buffer.set_cap(cap);
    }

    pub fn status(&self) -> LiveStatus {
        lock(&self.state).status.clone()
    }

    /// The cheap counters — (received, dropped, buffered) — for the status
    /// line; taking them never clones the ring.
    pub fn counts(&self) -> (u64, u64, usize) {
        let state = lock(&self.state);
        (state.received, state.buffer.dropped(), state.buffer.len())
    }

    /// The view's change stamp: (sequence number of the next entry,
    /// buffered count). It moves on every push, eviction and clear — even
    /// when a full ring's length stays put.
    pub fn change_stamp(&self) -> (u64, usize) {
        let state = lock(&self.state);
        (state.pushed, state.buffer.len())
    }

    /// The ring's current capacity.
    pub fn cap(&self) -> usize {
        lock(&self.state).buffer.cap()
    }

    /// Clone the whole ring's `Arc`s (cheap) plus the counters under the
    /// lock; callers filter and trim after it is released.
    fn snapshot_all(&self) -> LiveSnapshot<T> {
        let state = lock(&self.state);
        let first = state.first_seq();
        LiveSnapshot {
            entries: state
                .buffer
                .iter()
                .enumerate()
                .map(|(i, entry)| (first + i as u64, entry.clone()))
                .collect(),
            status: state.status.clone(),
            paused: state.paused,
            received: state.received,
            dropped: state.buffer.dropped(),
            buffered: state.buffer.len(),
            cap: state.buffer.cap(),
        }
    }

    /// The newest `max` buffered entries, oldest first.
    pub fn snapshot_tail(&self, max: usize) -> LiveSnapshot<T> {
        let mut snapshot = self.snapshot_all();
        let skip = snapshot.entries.len().saturating_sub(max);
        snapshot.entries.drain(..skip);
        snapshot
    }
}

impl LiveCapture<MonitorEntry> {
    /// The newest `max` entries passing the Profiler filter, oldest first.
    /// Filtering reads the whole ring, so a narrow filter still finds older
    /// matches instead of only sifting the display tail — but it runs after
    /// the lock is released, newest first, and stops at `max` matches.
    pub fn snapshot_filtered(
        &self,
        max: usize,
        filter: &ProfilerFilter,
    ) -> LiveSnapshot<MonitorEntry> {
        let mut snapshot = self.snapshot_all();
        let mut matching: Vec<(u64, Arc<MonitorEntry>)> = snapshot
            .entries
            .drain(..)
            .rev()
            .filter(|(_, entry)| live::monitor_entry_matches(entry, filter))
            .take(max)
            .collect();
        matching.reverse();
        snapshot.entries = matching;
        snapshot
    }
}

impl<T> Drop for LiveCapture<T> {
    fn drop(&mut self) {
        self.stop();
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The Profiler's capture type.
pub type ProfilerCapture = LiveCapture<MonitorEntry>;
/// The Pub/Sub viewer's capture type.
pub type PubSubCapture = LiveCapture<PubSubMessage>;

// --- Per-connection registry -------------------------------------------------

/// What the registry can do to a capture without knowing its item type.
trait EndCapture: Send + Sync {
    fn end_with(&self, reason: DisplayMessage);
}

impl<T: Send + Sync> EndCapture for Mutex<LiveState<T>> {
    fn end_with(&self, reason: DisplayMessage) {
        lock(self).end(reason);
    }
}

struct Registered {
    profile: uuid::Uuid,
    state: Weak<dyn EndCapture>,
    cancel: CancellationToken,
}

/// Every capture started in this process, by connection profile, so a
/// disconnect or a deleted connection can stop them wherever their view
/// lives — the docked Activity Monitor or a popped-out window. Holds only
/// weak state and a cancel token: it never keeps a capture (or its socket)
/// alive.
static REGISTRY: Mutex<Vec<Registered>> = Mutex::new(Vec::new());

fn register<T: Send + Sync + 'static>(
    profile: uuid::Uuid,
    state: &Arc<Mutex<LiveState<T>>>,
    cancel: &CancellationToken,
) {
    let state: Arc<dyn EndCapture> = state.clone();
    let mut registry = lock(&REGISTRY);
    registry.retain(|entry| entry.state.strong_count() > 0 && !entry.cancel.is_cancelled());
    registry.push(Registered {
        profile,
        state: Arc::downgrade(&state),
        cancel: cancel.clone(),
    });
}

/// Stop every running capture on `profile` — the connection was
/// disconnected or deleted, so no MONITOR or subscriber socket may outlive
/// it. Each stopped capture ends with `reason`; captures that already ended
/// keep their own reason. Returns how many running captures were stopped.
pub fn stop_captures_for_profile(profile: uuid::Uuid, reason: &str) -> usize {
    stop_captures_for_profile_message(profile, DisplayMessage::raw(reason))
}

pub fn stop_captures_for_profile_message(profile: uuid::Uuid, reason: DisplayMessage) -> usize {
    let mut registry = lock(&REGISTRY);
    let mut stopped = 0;
    registry.retain(|entry| {
        if entry.profile != profile {
            return entry.state.strong_count() > 0;
        }
        if let Some(state) = entry.state.upgrade()
            && !entry.cancel.is_cancelled()
        {
            state.end_with(reason.clone());
            entry.cancel.cancel();
            stopped += 1;
        }
        false
    });
    stopped
}

// --- Starting captures -------------------------------------------------------

fn spawn<T>(
    state: &Arc<Mutex<LiveState<T>>>,
    cancel: &CancellationToken,
    handle: &tokio::runtime::Handle,
    pump: impl std::future::Future<Output = ()> + Send + 'static,
) -> LiveCapture<T> {
    let _enter = handle.enter();
    let task = tokio::spawn(pump);
    LiveCapture {
        state: state.clone(),
        cancel: cancel.clone(),
        task: Mutex::new(Some(task)),
    }
}

fn fresh_state<T: StoredSize>(cap: usize) -> Arc<Mutex<LiveState<T>>> {
    Arc::new(Mutex::new(LiveState {
        buffer: BoundedBuffer::with_byte_budget(cap, LIVE_RING_BYTE_BUDGET),
        status: LiveStatus::Connecting,
        paused: false,
        received: 0,
        pushed: 0,
    }))
}

/// Start a MONITOR capture from the profile on a dedicated connection.
/// Always returns immediately: connecting happens inside the pump and a
/// failure surfaces as [`LiveStatus::Ended`] rather than a blocking error.
///
/// `time_box` is the hard stop: when it elapses the pump future is dropped
/// (closing the MONITOR socket) and the status ends with
/// [`time_box_reason`]. The clock starts once the connection is live.
pub fn start_profiler(
    handle: &tokio::runtime::Handle,
    profile: ConnectionProfile,
    cap: usize,
    time_box: Option<std::time::Duration>,
) -> ProfilerCapture {
    let state = fresh_state(cap);
    let cancel = CancellationToken::new();
    register(profile.id.0, &state, &cancel);
    let pump = {
        let state = state.clone();
        let cancel = cancel.clone();
        async move {
            run_capture(
                &state,
                &cancel,
                live::open_monitor(&profile),
                |monitor| Box::pin(monitor.next_frame()),
                DisplayMessage::new(
                    "services.redis.live.monitor_ended",
                    "the MONITOR stream ended (the server closed the connection, the network dropped, or a reply could not be read) — the capture connection is closed",
                ),
                |frame| match frame {
                    MonitorFrame::Line(line) => match live::parse_monitor_line(&line) {
                        Some(entry) => PumpStep::Record(entry),
                        None => PumpStep::Skip,
                    },
                    // A cluster master's line: the same entry, tagged.
                    MonitorFrame::NodeLine { node, line } => {
                        match live::parse_monitor_line(&line) {
                            Some(entry) => PumpStep::Record(MonitorEntry {
                                node: Some(node),
                                ..entry
                            }),
                            None => PumpStep::Skip,
                        }
                    }
                    MonitorFrame::ServerError(error) => PumpStep::End(
                        DisplayMessage::new(
                            "services.redis.live.monitor_server_error",
                            format!("the server sent an error on the MONITOR connection: {error} — the capture connection is closed"),
                        )
                        .with_arg("detail", error),
                    ),
                    MonitorFrame::Other => PumpStep::Skip,
                },
                time_box,
            )
            .await;
        }
    };
    spawn(&state, &cancel, handle, pump)
}

/// Start a Pub/Sub subscription from the profile on a dedicated
/// connection. Refuses an empty target list up front; connect/subscribe
/// failures surface as [`LiveStatus::Ended`]. There is no time box: a
/// subscription costs the server nothing per command, and it ends on Stop,
/// on leaving the tab, on closing the monitor, or on disconnecting.
pub fn start_pubsub(
    handle: &tokio::runtime::Handle,
    profile: ConnectionProfile,
    targets: Vec<SubscribeTarget>,
    cap: usize,
) -> StudioResult<PubSubCapture> {
    if targets.is_empty() {
        return Err(StudioError::InvalidInputMessage(DisplayMessage::new(
            "services.redis.live.no_subscription_targets",
            "no channels or patterns to subscribe to",
        )));
    }
    let state = fresh_state(cap);
    let cancel = CancellationToken::new();
    register(profile.id.0, &state, &cancel);
    let pump = {
        let state = state.clone();
        let cancel = cancel.clone();
        async move {
            run_capture(
                &state,
                &cancel,
                live::open_pubsub(&profile, &targets),
                |listener| Box::pin(listener.next_message()),
                DisplayMessage::new(
                    "services.redis.live.subscription_ended",
                    "the subscription ended (the server closed the connection or the network dropped) — the capture connection is closed",
                ),
                PumpStep::Record,
                None,
            )
            .await;
        }
    };
    Ok(spawn(&state, &cancel, handle, pump))
}

/// What the pump does with one raw item.
enum PumpStep<T> {
    /// Record it (unless paused) and count it.
    Record(T),
    /// Not an entry (an unparsable MONITOR line); ignored, not counted.
    Skip,
    /// End the capture with this reason.
    End(DisplayMessage),
}

/// Open the source (racing the cancel token), go live — unless a stop
/// landed while the connection was opening — then pump under the optional
/// time box.
#[allow(clippy::too_many_arguments)]
async fn run_capture<T, S, R, N, M, E>(
    state: &Arc<Mutex<LiveState<T>>>,
    cancel: &CancellationToken,
    open: impl std::future::Future<Output = StudioResult<S>>,
    next: N,
    ended_reason: E,
    map: M,
    time_box: Option<std::time::Duration>,
) where
    N: for<'a> FnMut(&'a mut S) -> NextFut<'a, R>,
    M: Fn(R) -> PumpStep<T>,
    E: Into<DisplayMessage>,
{
    let opened = tokio::select! {
        biased;
        _ = cancel.cancelled() => return,
        opened = open => opened,
    };
    let source = match opened {
        Ok(source) => source,
        Err(error) => {
            lock(state).end(
                DisplayMessage::new(
                    "services.redis.live.start_failed",
                    format!("could not start the capture: {error}"),
                )
                .with_arg("detail", error.to_string()),
            );
            return;
        }
    };
    if !mark_live(state) {
        // Stopped while connecting: dropping `source` closes the socket.
        return;
    }
    with_time_box(
        state,
        time_box,
        run_pump(state, cancel, source, next, ended_reason, map),
    )
    .await;
}

/// `Connecting` → `Live`. Refuses (returns false) when the capture already
/// ended — a stop that raced the connect must win.
fn mark_live<T>(state: &Arc<Mutex<LiveState<T>>>) -> bool {
    let mut state = lock(state);
    if state.status == LiveStatus::Connecting {
        state.status = LiveStatus::Live;
        true
    } else {
        false
    }
}

/// Run `pump` under an optional hard time limit. When the limit wins the
/// pump future — and the connection it owns — is dropped, and the status
/// ends with [`time_box_reason`].
async fn with_time_box<T>(
    state: &Arc<Mutex<LiveState<T>>>,
    time_box: Option<std::time::Duration>,
    pump: impl std::future::Future<Output = ()>,
) {
    let Some(limit) = time_box else {
        pump.await;
        return;
    };
    tokio::select! {
        _ = pump => {}
        _ = tokio::time::sleep(limit) => lock(state).end(time_box_reason_message(limit)),
    }
}

/// The future a pump source yields per item, borrowing the source for its
/// lifetime — the shape that lets `next` be an `FnMut(&mut S)` without the
/// borrow escaping the closure body.
type NextFut<'a, R> = std::pin::Pin<Box<dyn std::future::Future<Output = Option<R>> + Send + 'a>>;

/// The shared pump loop: pull items, count them, record unless paused.
/// `next` yields `None` when the stream ends; `map` decides per raw item:
/// [`PumpStep::Record`] counts it in `received` (and buffers it unless
/// paused), [`PumpStep::Skip`] drops it without counting it (a MONITOR
/// line that does not parse is not a command), and [`PumpStep::End`] ends
/// the capture with its reason. Cancel breaks the loop without touching the
/// status — [`LiveCapture::stop`] sets the stopped reason itself.
async fn run_pump<T, S, R, N, M, E>(
    state: &Arc<Mutex<LiveState<T>>>,
    cancel: &CancellationToken,
    mut source: S,
    mut next: N,
    ended_reason: E,
    map: M,
) where
    N: for<'a> FnMut(&'a mut S) -> NextFut<'a, R>,
    M: Fn(R) -> PumpStep<T>,
    E: Into<DisplayMessage>,
{
    loop {
        let item = tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            item = next(&mut source) => item,
        };
        let Some(raw) = item else {
            lock(state).end(ended_reason);
            break;
        };
        match map(raw) {
            PumpStep::Record(item) => {
                let mut state = lock(state);
                state.received += 1;
                if !state.paused {
                    state.push(item);
                }
            }
            PumpStep::Skip => {}
            PumpStep::End(reason) => {
                lock(state).end(reason);
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    fn entry(command: &str, client: &str, text: &str) -> MonitorEntry {
        MonitorEntry {
            server_seconds: 1.0,
            database: Some(0),
            client: client.to_string(),
            command: command.to_string(),
            text: text.to_string(),
            node: None,
        }
    }

    fn state_with<T>(status: LiveStatus) -> Arc<Mutex<LiveState<T>>> {
        Arc::new(Mutex::new(LiveState {
            buffer: BoundedBuffer::new(100),
            status,
            paused: false,
            received: 0,
            pushed: 0,
        }))
    }

    fn harness<T>() -> (Arc<Mutex<LiveState<T>>>, CancellationToken) {
        (state_with(LiveStatus::Live), CancellationToken::new())
    }

    fn buffered<T: Copy>(state: &Arc<Mutex<LiveState<T>>>) -> Vec<T> {
        lock(state).buffer.iter().map(|item| **item).collect()
    }

    /// Drive the pump from a scripted list of items (the queue emptying
    /// ends the "stream") — the state machine under test needs no sockets.
    async fn pump_script<T: Clone + Send>(
        state: &Arc<Mutex<LiveState<T>>>,
        cancel: &CancellationToken,
        script: Vec<Option<T>>,
    ) {
        run_pump(
            state,
            cancel,
            VecDeque::from(script),
            |items| Box::pin(std::future::ready(items.pop_front().flatten())),
            "ended",
            PumpStep::Record,
        )
        .await;
    }

    #[tokio::test]
    async fn time_box_ends_a_quiet_capture_and_drops_the_pump() {
        let (state, cancel) = harness::<i32>();
        let pump = run_pump(
            &state,
            &cancel,
            (),
            |_| Box::pin(std::future::pending::<Option<i32>>()),
            "ended",
            PumpStep::Record,
        );
        with_time_box(&state, Some(std::time::Duration::from_millis(20)), pump).await;
        let status = lock(&state).status.clone();
        match status {
            LiveStatus::Ended(reason) => assert!(reason.contains("time box of 0 s"), "{reason}"),
            other => panic!("expected ended, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn no_time_box_lets_the_stream_end_on_its_own() {
        let (state, cancel) = harness::<i32>();
        let pump = run_pump(
            &state,
            &cancel,
            VecDeque::from(vec![Some(1), None]),
            |items| Box::pin(std::future::ready(items.pop_front().flatten())),
            "ended",
            PumpStep::Record,
        );
        with_time_box(&state, None, pump).await;
        assert_eq!(lock(&state).status, LiveStatus::Ended("ended".into()));
    }

    #[test]
    fn time_box_reason_reads_minutes_or_seconds() {
        use std::time::Duration;
        assert!(time_box_reason(Duration::from_secs(300)).starts_with("time box of 5 min reached"));
        assert!(time_box_reason(Duration::from_secs(90)).starts_with("time box of 90 s reached"));
    }

    #[tokio::test]
    async fn pump_records_until_the_stream_ends() {
        let (state, cancel) = harness();
        pump_script(&state, &cancel, vec![Some(1), Some(2), None]).await;
        assert_eq!(buffered(&state), vec![1, 2]);
        let state = lock(&state);
        assert_eq!(state.received, 2);
        assert_eq!(state.status, LiveStatus::Ended("ended".into()));
    }

    #[tokio::test]
    async fn pump_discards_while_paused_but_still_counts() {
        let (state, cancel) = harness();
        lock(&state).paused = true;
        pump_script(&state, &cancel, vec![Some(1), Some(2), None]).await;
        let state = lock(&state);
        assert!(state.buffer.is_empty());
        assert_eq!(state.received, 2);
    }

    /// A flood of large messages under a generous entry cap stays inside
    /// the ring's byte budget: each payload keeps only its capped prefix,
    /// and past the budget the oldest are evicted and counted.
    #[tokio::test]
    async fn pubsub_ring_holds_to_its_byte_budget() {
        let state = fresh_state::<PubSubMessage>(100_000);
        lock(&state).status = LiveStatus::Live;
        let cancel = CancellationToken::new();
        let big = vec![b'x'; 1024 * 1024];
        let per_message = MAX_STORED_PAYLOAD_BYTES;
        let count = LIVE_RING_BYTE_BUDGET / per_message + 100;
        let script: Vec<Option<PubSubMessage>> = (0..count)
            .map(|i| Some(live::shape_pubsub_message(b"c", None, &big, i as i64)))
            .chain(std::iter::once(None))
            .collect();
        pump_script(&state, &cancel, script).await;
        let state = lock(&state);
        assert!(state.buffer.stored_bytes() <= LIVE_RING_BYTE_BUDGET);
        assert!(state.buffer.len() < count);
        assert!(state.buffer.dropped() >= 100);
        assert_eq!(state.received, count as u64);
        let newest = state.buffer.iter().last().expect("newest kept");
        assert!(newest.payload_truncated());
        assert_eq!(newest.payload_len, big.len());
    }

    #[tokio::test]
    async fn pump_overflow_drops_oldest_and_counts() {
        let (state, cancel) = harness::<i32>();
        lock(&state).buffer.set_cap(3);
        pump_script(
            &state,
            &cancel,
            (0..5).map(Some).chain(std::iter::once(None)).collect(),
        )
        .await;
        assert_eq!(buffered(&state), vec![2, 3, 4]);
        assert_eq!(lock(&state).buffer.dropped(), 2);
    }

    #[tokio::test]
    async fn pump_cancel_stops_without_marking_ended() {
        let (state, cancel) = harness::<i32>();
        cancel.cancel();
        pump_script(&state, &cancel, vec![Some(1), Some(2), None]).await;
        let state = lock(&state);
        assert!(state.buffer.is_empty());
        assert_eq!(state.status, LiveStatus::Live);
    }

    #[tokio::test]
    async fn pump_skipped_items_are_dropped_and_not_counted_as_received() {
        let (state, cancel) = harness::<i32>();
        let script = VecDeque::from(vec![1, 2, 3]);
        // The MONITOR path's map: even numbers "parse", odd ones don't.
        run_pump(
            &state,
            &cancel,
            script,
            |items| Box::pin(std::future::ready(items.pop_front())),
            "ended",
            |n| {
                if n % 2 == 0 {
                    PumpStep::Record(n)
                } else {
                    PumpStep::Skip
                }
            },
        )
        .await;
        assert_eq!(buffered(&state), vec![2]);
        assert_eq!(lock(&state).received, 1);
    }

    #[tokio::test]
    async fn pump_end_step_ends_with_the_server_reason() {
        let (state, cancel) = harness::<i32>();
        run_pump(
            &state,
            &cancel,
            VecDeque::from(vec![1, -1, 3]),
            |items| Box::pin(std::future::ready(items.pop_front())),
            "stream ended",
            |n| {
                if n < 0 {
                    PumpStep::End("server error".into())
                } else {
                    PumpStep::Record(n)
                }
            },
        )
        .await;
        assert_eq!(buffered(&state), vec![1], "nothing after the error");
        assert_eq!(
            lock(&state).status,
            LiveStatus::Ended("server error".into())
        );
    }

    /// Regression: a stop that lands after the connection opened but
    /// before the pump went live used to be overwritten with `Live`.
    #[tokio::test]
    async fn stop_during_connect_is_never_overwritten_with_live() {
        let state = state_with::<i32>(LiveStatus::Connecting);
        let cancel = CancellationToken::new();
        let open = {
            let state = state.clone();
            let cancel = cancel.clone();
            async move {
                // What `LiveCapture::stop` does, racing the connect's
                // completion.
                lock(&state).end(STOPPED_REASON.to_string());
                cancel.cancel();
                Ok(VecDeque::from(vec![1, 2]))
            }
        };
        run_capture(
            &state,
            &cancel,
            open,
            |items: &mut VecDeque<i32>| Box::pin(std::future::ready(items.pop_front())),
            "ended",
            PumpStep::Record,
            None,
        )
        .await;
        let state = lock(&state);
        assert_eq!(state.status, LiveStatus::Ended(STOPPED_REASON.into()));
        assert!(state.buffer.is_empty(), "nothing pumped after the stop");
    }

    #[tokio::test]
    async fn failed_open_ends_with_the_error_text() {
        let state = state_with::<i32>(LiveStatus::Connecting);
        let cancel = CancellationToken::new();
        run_capture(
            &state,
            &cancel,
            std::future::ready(Err::<VecDeque<i32>, _>(
                StudioError::CapabilityNotSupported("ERR unknown command 'MONITOR'".into()),
            )),
            |items: &mut VecDeque<i32>| Box::pin(std::future::ready(items.pop_front())),
            "ended",
            PumpStep::Record,
            None,
        )
        .await;
        match lock(&state).status.clone() {
            LiveStatus::Ended(reason) => {
                assert!(reason.contains("unknown command 'MONITOR'"), "{reason}")
            }
            other => panic!("expected ended, got {other:?}"),
        }
    }

    #[test]
    fn a_late_end_never_overwrites_the_first_reason() {
        let state = state_with::<i32>(LiveStatus::Live);
        lock(&state).end(STOPPED_REASON);
        lock(&state).end("time box");
        assert_eq!(
            lock(&state).status,
            LiveStatus::Ended(STOPPED_REASON.into())
        );
    }

    fn capture_with(entries: Vec<MonitorEntry>) -> ProfilerCapture {
        let state = state_with(LiveStatus::Live);
        for entry in entries {
            lock(&state).push(entry);
        }
        LiveCapture {
            state,
            cancel: CancellationToken::new(),
            task: Mutex::new(None),
        }
    }

    fn registered_capture(profile: uuid::Uuid) -> ProfilerCapture {
        let capture = capture_with(Vec::new());
        register(profile, &capture.state, &capture.cancel);
        capture
    }

    #[test]
    fn stop_captures_for_profile_stops_only_that_connection() {
        let a = uuid::Uuid::new_v4();
        let b = uuid::Uuid::new_v4();
        let a1 = registered_capture(a);
        let a2 = registered_capture(a);
        let b1 = registered_capture(b);
        let dropped = registered_capture(a);
        drop(dropped);

        assert_eq!(stop_captures_for_profile(a, DISCONNECTED_REASON), 2);
        for capture in [&a1, &a2] {
            assert_eq!(
                capture.status(),
                LiveStatus::Ended(DISCONNECTED_REASON.into())
            );
            assert!(capture.cancel.is_cancelled());
        }
        assert_eq!(b1.status(), LiveStatus::Live);
        assert!(!b1.cancel.is_cancelled());
        // Already stopped: a second disconnect has nothing left to stop.
        assert_eq!(stop_captures_for_profile(a, DISCONNECTED_REASON), 0);
        assert_eq!(stop_captures_for_profile(b, DISCONNECTED_REASON), 1);
    }

    #[test]
    fn stop_captures_for_profile_keeps_a_user_stop_reason() {
        let profile = uuid::Uuid::new_v4();
        let capture = registered_capture(profile);
        capture.stop();
        assert_eq!(stop_captures_for_profile(profile, DISCONNECTED_REASON), 0);
        assert_eq!(capture.status(), LiveStatus::Ended(stopped_reason()));
    }

    #[test]
    fn snapshot_tail_returns_the_newest_entries_with_stable_sequence_numbers() {
        let capture = capture_with(vec![
            entry("GET", "a:1", "\"get\" \"k1\""),
            entry("SET", "a:2", "\"set\" \"k2\""),
            entry("DEL", "a:3", "\"del\" \"k3\""),
        ]);
        let snapshot = capture.snapshot_tail(2);
        assert_eq!(snapshot.entries.len(), 2);
        assert_eq!(snapshot.entries[0].1.command, "SET");
        assert_eq!(snapshot.entries[1].1.command, "DEL");
        assert_eq!(
            snapshot
                .entries
                .iter()
                .map(|(seq, _)| *seq)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(snapshot.buffered, 3);
        assert_eq!(snapshot.cap, 100);
        assert_eq!(snapshot.status, LiveStatus::Live);

        // Overflow shifts ring positions but never an entry's number.
        capture.set_cap(3);
        lock(&capture.state).push(entry("PING", "a:4", "\"ping\""));
        let snapshot = capture.snapshot_tail(10);
        assert_eq!(
            snapshot
                .entries
                .iter()
                .map(|(seq, entry)| (*seq, entry.command.as_str()))
                .collect::<Vec<_>>(),
            vec![(1, "SET"), (2, "DEL"), (3, "PING")]
        );
    }

    #[test]
    fn change_stamp_moves_when_a_full_ring_rotates() {
        let capture = capture_with(Vec::new());
        capture.set_cap(2);
        lock(&capture.state).push(entry("A", "c", "a"));
        lock(&capture.state).push(entry("B", "c", "b"));
        let full = capture.change_stamp();
        lock(&capture.state).push(entry("C", "c", "c"));
        assert_ne!(capture.change_stamp(), full, "same length, new content");
    }

    #[test]
    fn snapshot_filtered_sifts_the_whole_ring_newest_first_capped() {
        let capture = capture_with(vec![
            entry("GET", "a:1", "\"get\" \"k1\""),
            entry("SET", "a:2", "\"set\" \"k2\""),
            entry("GET", "b:1", "\"get\" \"k3\""),
            entry("GET", "a:9", "\"get\" \"k4\""),
        ]);
        let filter = ProfilerFilter {
            command: "get".into(),
            ..Default::default()
        };
        let snapshot = capture.snapshot_filtered(2, &filter);
        // Whole-ring sift: all three GETs pass; the newest two are kept,
        // oldest first, with their ring sequence numbers.
        assert_eq!(snapshot.entries.len(), 2);
        assert_eq!(snapshot.entries[0].1.text, "\"get\" \"k3\"");
        assert_eq!(snapshot.entries[1].1.text, "\"get\" \"k4\"");
        assert_eq!(snapshot.entries[0].0, 2);
        assert_eq!(snapshot.entries[1].0, 3);
        assert_eq!(snapshot.buffered, 4, "counters describe the whole ring");
        let client_filter = ProfilerFilter {
            client: "A:".into(),
            ..Default::default()
        };
        let snapshot = capture.snapshot_filtered(10, &client_filter);
        assert_eq!(snapshot.entries.len(), 3);
    }

    #[test]
    fn capture_controls_mutate_the_shared_state() {
        let capture = capture_with(vec![entry("GET", "a:1", "\"get\" \"k\"")]);
        capture.set_paused(true);
        assert!(capture.is_paused());
        capture.set_cap(0);
        assert_eq!(capture.status(), LiveStatus::Live);
        capture.clear();
        let snapshot = capture.snapshot_tail(10);
        assert!(snapshot.entries.is_empty());
        assert_eq!(snapshot.cap, 1, "cap never drops below one");
        capture.stop();
        capture.stop();
        assert_eq!(capture.status(), LiveStatus::Ended(stopped_reason()));
    }

    #[test]
    fn start_pubsub_refuses_an_empty_target_list() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let profile = sqlly_core::ConnectionProfile::sql_login(
            "r",
            "127.0.0.1",
            "",
            sqlly_core::SecretRef {
                provider: sqlly_core::SecretProvider::InMemory,
                key: String::new(),
            },
        );
        let result = start_pubsub(runtime.handle(), profile, Vec::new(), 10);
        assert!(matches!(result, Err(StudioError::InvalidInputMessage(_))));
    }
}
