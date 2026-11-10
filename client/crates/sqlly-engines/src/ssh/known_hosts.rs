//! Host-key trust-on-first-use verification against OpenSSH known_hosts.

use parking_lot::{Mutex, RwLock};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tokio::time::Instant;

use russh::keys::known_hosts::learn_known_hosts_path;
use russh::keys::{
    Error as KeysError, HashAlg, PublicKey, PublicKeyOrCertificate, check_known_hosts_path,
};
use sqlly_core::StudioError;

pub struct HostKeyPrompt {
    pub host: String,
    pub port: u16,
    pub key_type: String,
    pub fingerprint_sha256: String,
}

pub type HostKeyDecider = Arc<dyn Fn(HostKeyPrompt) -> bool + Send + Sync>;

static HOST_KEY_DECIDER: OnceLock<HostKeyDecider> = OnceLock::new();

static KNOWN_HOSTS_OVERRIDE: RwLock<Option<PathBuf>> = RwLock::new(None);

/// Override the known_hosts path (tests only). Reads `SQLLY_SSH_KNOWN_HOSTS` when set.
pub fn known_hosts_path() -> PathBuf {
    if let Some(path) = KNOWN_HOSTS_OVERRIDE.read().clone() {
        return path;
    }
    if let Ok(path) = std::env::var("SQLLY_SSH_KNOWN_HOSTS") {
        return PathBuf::from(path);
    }
    dirs::home_dir()
        .map(|h| h.join(".ssh/known_hosts"))
        .unwrap_or_else(|| PathBuf::from(".ssh/known_hosts"))
}

#[doc(hidden)]
pub fn set_known_hosts_path_for_tests(path: PathBuf) {
    *KNOWN_HOSTS_OVERRIDE.write() = Some(path);
}

pub fn install_host_key_decider(decider: HostKeyDecider) {
    let _ = HOST_KEY_DECIDER.set(decider);
}

fn fingerprint_sha256(key: &PublicKey) -> String {
    key.fingerprint(HashAlg::Sha256).to_string()
}

/// Verify the server host key. Returns `Ok(())` when the key is trusted.
///
/// An unknown key goes to the installed decider, which may wait for the
/// user to answer a dialog. That wait blocks a thread, so the check runs
/// on tokio's blocking pool, where waiting is allowed; on a runtime worker
/// tokio would panic. This is the only entry point on purpose: every
/// caller gets the prompt clock, so the trust dialog's time is never
/// charged to the connect timeout.
///
/// `clock` runs for exactly as long as the user is being asked, and nothing
/// else: not the known_hosts read, not the wait for a blocking thread. The
/// SSH connect timeout stops while it runs, so a stall anywhere else in
/// this path still times out.
///
/// The blocking thread reads the clock itself when the prompt opens and
/// closes and sends both readings over; the runtime banks the difference.
/// So the banked time is the dialog's real open time even when the runtime
/// is slow to look at the events (a starved runtime, or a connect future
/// that is only polled after both have been queued). A blocking thread
/// runs inside the runtime's context, so its readings come from the same
/// clock as the connect timeout, paused test time included.
///
/// If the connect is abandoned (the timeout fired during a stall, dropping
/// this future) before the blocking thread gets to the dialog, the thread
/// sees that and returns without asking or recording anything: a trust
/// prompt for a connection that already failed would only confuse.
pub async fn verify_server_key_off_runtime(
    host: &str,
    port: u16,
    key: &PublicKeyOrCertificate,
    clock: Arc<PromptClock>,
) -> Result<(), HostKeyError> {
    let host = host.to_string();
    let public_key = key.public_key();
    let (events, mut prompt_events) = tokio::sync::mpsc::unbounded_channel();
    let announce_on = clock.clone();
    let check = tokio::task::spawn_blocking(move || {
        let reporter = PromptReporter {
            events: &events,
            clock: &announce_on,
        };
        verify_public_key(&host, port, &public_key, &reporter)
    });
    let (result, ()) = tokio::join!(check, bank_prompt_events(&clock, &mut prompt_events));
    result.unwrap_or_else(|error| {
        Err(HostKeyError::PromptUnavailable(format!(
            "SSH host key check stopped: {error}"
        )))
    })
}

/// Banks every prompt the blocking check reports, until it drops its
/// sender (however it ends). A prompt still open then (the thread died
/// mid-prompt) is closed here, at the time the loop notices.
async fn bank_prompt_events(
    clock: &PromptClock,
    events: &mut tokio::sync::mpsc::UnboundedReceiver<PromptEvent>,
) {
    let mut open: Option<OpenPrompt<'_>> = None;
    while let Some(event) = events.recv().await {
        match event {
            PromptEvent::Opened(at) => {
                // A prompt still open is closed at the moment the new one
                // opened, before the new open is stamped: the old guard's
                // close would otherwise read the new `open_since` and bank
                // nothing, leaving the clock closed with a dialog up.
                if let Some(prompt) = open.take() {
                    prompt.close_at(at);
                }
                open = Some(clock.open_at(at));
            }
            PromptEvent::Closed(at) => {
                if let Some(prompt) = open.take() {
                    prompt.close_at(at);
                }
            }
        }
    }
    drop(open);
    // An announced prompt whose `Opened` never arrived (the thread died
    // first) would otherwise count as open forever.
    clock.withdraw_announcement();
}

/// What the blocking check tells the runtime side about the trust dialog,
/// each with the moment it happened, read on the prompting thread.
enum PromptEvent {
    Opened(Instant),
    Closed(Instant),
}

/// How the blocking check reports the trust dialog: the clock hears at
/// once that a prompt is about to open (so the connect timeout cannot fire
/// in the moment before the runtime banks it), and the events carry the
/// open and close times to the runtime. The sender also tells the check
/// whether anyone is still waiting for the answer.
struct PromptReporter<'a> {
    events: &'a tokio::sync::mpsc::UnboundedSender<PromptEvent>,
    clock: &'a PromptClock,
}

fn verify_public_key(
    host: &str,
    port: u16,
    public_key: &PublicKey,
    reporter: &PromptReporter<'_>,
) -> Result<(), HostKeyError> {
    verify_public_key_against(
        host,
        port,
        public_key,
        &known_hosts_path(),
        HOST_KEY_DECIDER.get(),
        reporter,
    )
}

/// [`verify_public_key`] against a given known_hosts file and decider, so
/// a test can drive it without touching the process-wide ones.
fn verify_public_key_against(
    host: &str,
    port: u16,
    public_key: &PublicKey,
    path: &Path,
    decider: Option<&HostKeyDecider>,
    reporter: &PromptReporter<'_>,
) -> Result<(), HostKeyError> {
    match check_known_hosts_path(host, port, public_key, path) {
        Ok(true) => Ok(()),
        Ok(false) => {
            let fp = fingerprint_sha256(public_key);
            let decider = decider.ok_or_else(|| {
                HostKeyError::PromptUnavailable(format!(
                    "SSH host key for {host}:{port} is not trusted yet (SHA256:{fp}); \
                     open the connection once in the desktop app to review and trust it"
                ))
            })?;
            // Announce first, then look for an abandon: the timeout does the
            // mirror image (abandon first, then look for an announcement),
            // so whichever of the two moves second sees the other. Without
            // that, both could pass their checks and the dialog would open
            // for a connect already reported as timed out. A connect dropped
            // for any other reason (session error, user cancelled) abandons
            // the clock too, from its drop guard; the session runs on its
            // own task, so that drop does not reach this thread any other
            // way. The receiver going is kept as a second signal. Either way
            // there is nothing to trust the key for: no dialog, no
            // known_hosts entry.
            if !reporter.clock.announce_open() || reporter.events.is_closed() {
                reporter.clock.withdraw_announcement();
                return Err(abandoned(host, port));
            }
            let _ = reporter.events.send(PromptEvent::Opened(Instant::now()));
            let accept = decider(HostKeyPrompt {
                host: host.to_string(),
                port,
                key_type: public_key.algorithm().to_string(),
                fingerprint_sha256: fp.clone(),
            });
            let _ = reporter.events.send(PromptEvent::Closed(Instant::now()));
            // The connect may have ended while the dialog was open (the
            // user cancelled it, the session failed): then there is no
            // connection to trust the key for, and a Trust must not be
            // persisted on its behalf.
            if reporter.clock.is_abandoned() || reporter.events.is_closed() {
                // Nobody is left to stamp the close either.
                reporter.clock.withdraw_announcement();
                return Err(abandoned(host, port));
            }
            if !accept {
                return Err(HostKeyError::Rejected);
            }
            learn_known_hosts_path(host, port, public_key, path).map_err(HostKeyError::Learn)?;
            Ok(())
        }
        Err(KeysError::KeyChanged { line }) => Err(HostKeyError::Changed { line }),
        Err(e) => Err(HostKeyError::Learn(e)),
    }
}

fn abandoned(host: &str, port: u16) -> HostKeyError {
    HostKeyError::PromptUnavailable(format!(
        "SSH connection to {host}:{port} was abandoned before its host key could be reviewed"
    ))
}

/// How long the host-key trust dialog has been open, in total, during one
/// SSH connect. The connect timeout adds this to its deadline so the user's
/// reading time is never charged to the server (see
/// `tunnel::timeout_unless_prompting`).
///
/// Exact, not sampled: the prompting side records when the dialog opened and
/// closed, and those readings are what get banked, so how long the runtime
/// takes to hear about them changes nothing. Whoever waits on the timeout
/// is woken when the dialog closes.
const GATE_IDLE: u8 = 0;
const GATE_ANNOUNCED: u8 = 1;
const GATE_ABANDONED: u8 = 2;

#[derive(Debug, Default)]
pub struct PromptClock {
    /// Prompt time already banked by closed prompts, in nanoseconds: exact,
    /// so the server is never short-changed by rounding.
    closed_nanos: AtomicU64,
    /// When the currently open prompt opened, if one is open.
    open_since: Mutex<Option<Instant>>,
    /// Who has the connect: nobody yet ([`GATE_IDLE`]), a prompt that is
    /// opening or open ([`GATE_ANNOUNCED`], set by the prompting thread the
    /// instant before it asks, ahead of the runtime stamping `open_since`),
    /// or nobody any more ([`GATE_ABANDONED`], the connect was given up).
    /// One atomic, so a prompt and a timeout that race settle it with a
    /// single compare-and-swap each: exactly one of them wins.
    gate: AtomicU8,
    /// Woken every time a prompt closes.
    changed: tokio::sync::Notify,
}

/// Keeps a prompt open for as long as it lives; dropping it closes the
/// prompt and banks the time.
pub struct OpenPrompt<'a> {
    clock: &'a PromptClock,
}

impl PromptClock {
    /// Marks the dialog as open. Hold the guard while the user is deciding.
    pub fn open(&self) -> OpenPrompt<'_> {
        self.open_at(Instant::now())
    }

    /// Marks the dialog as open since `at`, a reading taken where it really
    /// opened; the clock credits the prompt from then, not from now.
    pub fn open_at(&self, at: Instant) -> OpenPrompt<'_> {
        *self.open_since.lock() = Some(at);
        OpenPrompt { clock: self }
    }

    /// Prompt time banked so far, plus the open prompt's time if one is open.
    pub fn total(&self) -> Duration {
        let closed = Duration::from_nanos(self.closed_nanos.load(Ordering::SeqCst));
        match *self.open_since.lock() {
            Some(since) => closed + since.elapsed(),
            None => closed,
        }
    }

    /// Whether the dialog is open right now, or announced and about to be.
    pub fn is_open(&self) -> bool {
        self.gate.load(Ordering::SeqCst) == GATE_ANNOUNCED || self.open_since.lock().is_some()
    }

    /// Claims the connect for a prompt that is about to open. Safe from any
    /// thread; [`PromptClock::open`] then stamps the time on the runtime.
    /// `false` when the connect was already given up: the prompt must not
    /// open.
    pub fn announce_open(&self) -> bool {
        match self.gate.compare_exchange(
            GATE_IDLE,
            GATE_ANNOUNCED,
            Ordering::SeqCst,
            Ordering::SeqCst,
        ) {
            Ok(_) => true,
            Err(current) => current == GATE_ANNOUNCED,
        }
    }

    /// Takes back an announcement no [`PromptClock::open`] followed, waking
    /// a waiter that was holding off for the close.
    pub fn withdraw_announcement(&self) {
        if self
            .gate
            .compare_exchange(
                GATE_ANNOUNCED,
                GATE_IDLE,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
        {
            self.changed.notify_one();
        }
    }

    /// The timeout's claim on the connect, to give it up. `true` when it
    /// is given up (now or already); `false` when a prompt holds it, and
    /// the timeout keeps waiting for that prompt to close. Settled against
    /// [`PromptClock::announce_open`] by one compare-and-swap, so exactly
    /// one side wins a race: never a dialog for a connect reported as timed
    /// out, and never both backing off.
    pub fn try_abandon(&self) -> bool {
        match self.gate.compare_exchange(
            GATE_IDLE,
            GATE_ABANDONED,
            Ordering::SeqCst,
            Ordering::SeqCst,
        ) {
            Ok(_) => true,
            Err(current) => current == GATE_ABANDONED,
        }
    }

    /// The connect is gone, whatever holds it (its future was dropped): a
    /// dialog that is open stays answered for nothing, and none opens.
    pub fn abandon(&self) {
        self.gate.store(GATE_ABANDONED, Ordering::SeqCst);
    }

    /// Whether the connect has been given up.
    pub fn is_abandoned(&self) -> bool {
        self.gate.load(Ordering::SeqCst) == GATE_ABANDONED
    }

    /// The next time a prompt closes. A close that happened before a waiter
    /// registered is still seen: a real close stores one wake-up permit
    /// (`notify_one`), which the next `closed()` consumes at once. Only a
    /// prompt that was really open stores one, so a waiter is never woken
    /// for nothing. A caller that reads the clock's state before waiting
    /// should register first ([`tokio::sync::futures::Notified::enable`]),
    /// so it never depends on the permit alone.
    pub fn closed(&self) -> tokio::sync::futures::Notified<'_> {
        self.changed.notified()
    }
}

impl OpenPrompt<'_> {
    /// Closes the prompt as of `at`, a reading taken where it really closed.
    pub fn close_at(self, at: Instant) {
        self.clock.close(at);
        std::mem::forget(self);
    }
}

impl PromptClock {
    fn close(&self, at: Instant) {
        if let Some(since) = self.open_since.lock().take() {
            let open_for = at.saturating_duration_since(since);
            let nanos = u64::try_from(open_for.as_nanos()).unwrap_or(u64::MAX);
            self.closed_nanos.fetch_add(nanos, Ordering::SeqCst);
            // The prompt hands the connect back; an abandon that landed
            // while it was open stays.
            let _ = self.gate.compare_exchange(
                GATE_ANNOUNCED,
                GATE_IDLE,
                Ordering::SeqCst,
                Ordering::SeqCst,
            );
            self.changed.notify_one();
        }
    }
}

impl Drop for OpenPrompt<'_> {
    fn drop(&mut self) {
        self.clock.close(Instant::now());
    }
}

#[derive(Debug)]
pub enum HostKeyError {
    Changed { line: usize },
    Rejected,
    PromptUnavailable(String),
    Learn(KeysError),
}

impl HostKeyError {
    pub fn to_studio_error(self, host: &str, port: u16) -> StudioError {
        match self {
            HostKeyError::Changed { line } => StudioError::Connection(format!(
                "SSH host key for {host}:{port} changed (known_hosts line {line}); \
                 refusing to connect — possible man-in-the-middle attack"
            )),
            HostKeyError::Rejected => {
                StudioError::Connection(format!("SSH host key for {host}:{port} was not trusted"))
            }
            HostKeyError::PromptUnavailable(msg) => StudioError::Connection(msg),
            HostKeyError::Learn(e) => {
                StudioError::Connection(format!("SSH host key check for {host}:{port} failed: {e}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use russh::keys::{Algorithm, PrivateKey};
    use std::sync::atomic::AtomicUsize;

    /// The runtime may only get to the events long after the dialog
    /// closed; the time banked is still the dialog's real open time.
    #[tokio::test(start_paused = true)]
    async fn prompt_time_is_banked_from_the_reported_instants() {
        let clock = PromptClock::default();
        let (events, mut prompt_events) = tokio::sync::mpsc::unbounded_channel();
        events.send(PromptEvent::Opened(Instant::now())).unwrap();
        tokio::time::advance(Duration::from_secs(30)).await;
        events.send(PromptEvent::Closed(Instant::now())).unwrap();
        drop(events);
        // Nothing has looked at the events yet; time keeps moving.
        tokio::time::advance(Duration::from_secs(5)).await;
        bank_prompt_events(&clock, &mut prompt_events).await;
        assert_eq!(clock.total(), Duration::from_secs(30));
        assert!(!clock.is_open());
    }

    /// A second `Opened` while a prompt is open (one dialog replaced by
    /// another) closes the first at the moment the second opened, so the
    /// clock stays open throughout and banks the whole span.
    #[tokio::test(start_paused = true)]
    async fn a_second_open_continues_the_prompt_time() {
        let clock = PromptClock::default();
        let (events, mut prompt_events) = tokio::sync::mpsc::unbounded_channel();
        events.send(PromptEvent::Opened(Instant::now())).unwrap();
        tokio::time::advance(Duration::from_secs(10)).await;
        events.send(PromptEvent::Opened(Instant::now())).unwrap();
        tokio::time::advance(Duration::from_secs(20)).await;
        events.send(PromptEvent::Closed(Instant::now())).unwrap();
        // The first two events are banked with the dialog still open.
        let banking = tokio::spawn({
            let clock = std::sync::Arc::new(clock);
            let clock_for_task = clock.clone();
            async move {
                bank_prompt_events(&clock_for_task, &mut prompt_events).await;
                clock
            }
        });
        tokio::task::yield_now().await;
        drop(events);
        let clock = banking.await.unwrap();
        assert_eq!(clock.total(), Duration::from_secs(30));
        assert!(!clock.is_open());
    }

    /// A reading taken on a blocking thread is from the same clock as the
    /// runtime's, paused test time included.
    #[tokio::test(start_paused = true)]
    async fn a_blocking_thread_reads_the_runtime_clock() {
        let before = Instant::now();
        let read = tokio::task::spawn_blocking(Instant::now).await.unwrap();
        assert_eq!(read, before);
    }

    /// The connect ends while the dialog is open (the decider is still
    /// running when the receiver goes): its Trust is not persisted, since
    /// there is no connection to trust the key for.
    #[test]
    fn a_trust_for_a_connect_that_ended_meanwhile_is_not_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("known_hosts");
        let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
        let public_key = key.public_key().clone();
        let clock = PromptClock::default();
        let (events, prompt_events) = tokio::sync::mpsc::unbounded_channel();
        let receiver = std::sync::Mutex::new(Some(prompt_events));
        let decider: HostKeyDecider = Arc::new(move |_| {
            // The connect goes away while the user is looking at the dialog.
            drop(receiver.lock().unwrap().take());
            true
        });
        let reporter = PromptReporter {
            events: &events,
            clock: &clock,
        };
        let result = verify_public_key_against(
            "example.test",
            22,
            &public_key,
            &path,
            Some(&decider),
            &reporter,
        );
        assert!(
            matches!(result, Err(HostKeyError::PromptUnavailable(ref msg)) if msg.contains("abandoned")),
            "{result:?}"
        );
        assert!(!clock.is_open());
        assert!(
            !path.exists(),
            "a Trust for a connect that no longer exists is not written"
        );
    }

    /// The connect future is dropped while the dialog is open (the user
    /// cancelled; the session lives on its own task, so only the drop guard
    /// tells the clock): the Trust is not persisted.
    #[test]
    fn a_trust_after_the_connect_was_dropped_is_not_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("known_hosts");
        let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
        let public_key = key.public_key().clone();
        let clock = Arc::new(PromptClock::default());
        let (events, _prompt_events) = tokio::sync::mpsc::unbounded_channel();
        let dropped = clock.clone();
        let decider: HostKeyDecider = Arc::new(move |_| {
            // What `tunnel::AbandonOnDrop` does when the connect future goes
            // while the user is looking at the dialog.
            dropped.abandon();
            true
        });
        let reporter = PromptReporter {
            events: &events,
            clock: &clock,
        };
        let result = verify_public_key_against(
            "example.test",
            22,
            &public_key,
            &path,
            Some(&decider),
            &reporter,
        );
        assert!(
            matches!(result, Err(HostKeyError::PromptUnavailable(ref msg)) if msg.contains("abandoned")),
            "{result:?}"
        );
        assert!(!clock.is_open());
        assert!(
            !path.exists(),
            "a Trust for a dropped connect is not written"
        );
    }

    /// A connect that gave up before the check reached the dialog gets no
    /// dialog and leaves no known_hosts entry behind. Drives the check with
    /// its own decider and file: the process-wide ones are shared by every
    /// test in this binary.
    #[test]
    fn an_abandoned_connect_is_not_asked_to_trust_anything() {
        let decider_calls = Arc::new(AtomicUsize::new(0));
        let counted = decider_calls.clone();
        let decider: HostKeyDecider = Arc::new(move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
            true
        });
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("known_hosts");
        let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
        let public_key = key.public_key().clone();
        let clock = PromptClock::default();
        let (events, prompt_events) = tokio::sync::mpsc::unbounded_channel();
        drop(prompt_events);
        let reporter = PromptReporter {
            events: &events,
            clock: &clock,
        };
        let result = verify_public_key_against(
            "example.test",
            22,
            &public_key,
            &path,
            Some(&decider),
            &reporter,
        );
        assert!(
            matches!(result, Err(HostKeyError::PromptUnavailable(ref msg)) if msg.contains("abandoned")),
            "{result:?}"
        );
        assert_eq!(decider_calls.load(Ordering::SeqCst), 0);
        assert!(!clock.is_open());
        assert!(
            !path.exists(),
            "nothing was trusted, so nothing was written"
        );
    }
}
