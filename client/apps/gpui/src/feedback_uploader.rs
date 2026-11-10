//! Background upload queue for both feedback channels — "Improve
//! IntelliSense" and Send Feedback.
//!
//! Submitting either dialog writes the report to a local JSON file first
//! (one file per submission, under `pending-feedback/` or
//! `pending-general-feedback/`), then tries to upload it at once. The file
//! stays on disk until the server accepts the report; a single background
//! worker ([`sync::start`]) re-sends everything still pending when the app
//! starts and then every [`RETRY_INTERVAL`]. How a failure is retried depends
//! on why it failed ([`sync::Disposition`]): network and server errors retry
//! every interval with no give-up, so a report survives outages and
//! restarts; a report the server says it will never accept (too large,
//! malformed) is dropped; any rate limit is retryable but pauses the whole
//! queue until the limit resets plus a random 2,000-18,000 seconds (so
//! clients don't all return at the same moment); an auth refusal waits a
//! day. The
//! client stamps every record with a GUID the server dedupes on, so a retry
//! after a half-landed POST cannot double-post.

use serde::{Deserialize, Serialize};

/// How often the background worker re-sends reports still on disk.
pub const RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// A submission that has not reached the store yet. Persisted so retries
/// survive an app restart.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingFeedback {
    /// Client-generated GUID identifying this submission everywhere: the
    /// local record file, the request body, and the server's dedup check.
    pub submission_id: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Paired device id at capture time, when there was one.
    pub device_id: Option<String>,
    /// The user's note ("what should it have offered").
    pub expected_text: String,
    /// The disclosure-filtered context JSON, Brotli-compressed, base64.
    pub context_brotli_b64: String,
    /// Failed upload attempts so far (informational).
    pub attempts: u32,
    /// Don't try again before this time (set when the server asked us to
    /// wait). `None` means the next regular pass.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_attempt_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl PendingFeedback {
    /// The compressed context bytes for the request body. `None` when the
    /// stored base64 is corrupt (treat as unrecoverable and drop the record).
    pub fn context_brotli(&self) -> Option<Vec<u8>> {
        base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            &self.context_brotli_b64,
        )
        .ok()
    }
}

/// "5 minutes" for status-bar copy.
pub fn retry_interval_label() -> String {
    let minutes = RETRY_INTERVAL.as_secs() / 60;
    let key = if minutes == 1 {
        "feedback.uploader.retry_interval_one"
    } else {
        "feedback.uploader.retry_interval_many"
    };
    if minutes == 1 {
        crate::localization::text(key)
    } else {
        crate::localization::format(key, &[("count", &minutes.to_string())])
    }
}

/// One-JSON-file-per-submission persistence shared by the feedback queues:
/// each record lives at `<dir>/<guid>.json` until it uploads or expires.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod queue_store {
    use std::path::{Path, PathBuf};

    fn record_path(dir: &Path, submission_id: &str) -> PathBuf {
        // The id is a client-generated GUID, but sanitize anyway so a corrupt
        // record can never write outside the pending directory.
        let safe: String = submission_id
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
            .collect();
        dir.join(format!("{safe}.json"))
    }

    /// Writes (or rewrites) the record. Errors are swallowed: a failed write
    /// only costs restart-survival, not the in-flight retry loop.
    pub fn save_in<T: serde::Serialize>(dir: &Path, submission_id: &str, record: &T) {
        let _ = std::fs::create_dir_all(dir);
        if let Ok(json) = serde_json::to_vec_pretty(record) {
            let _ = std::fs::write(record_path(dir, submission_id), json);
        }
    }

    pub fn delete_in(dir: &Path, submission_id: &str) {
        let _ = std::fs::remove_file(record_path(dir, submission_id));
    }

    pub fn exists_in(dir: &Path, submission_id: &str) -> bool {
        record_path(dir, submission_id).exists()
    }

    /// All records in `dir`, corrupt files skipped (and removed — they can
    /// never upload).
    pub fn load_all_in<T: serde::de::DeserializeOwned>(dir: &Path) -> Vec<T> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut records = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            match std::fs::read(&path)
                .ok()
                .and_then(|data| serde_json::from_slice::<T>(&data).ok())
            {
                Some(record) => records.push(record),
                None => {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
        records
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod fs_store {
    use super::PendingFeedback;
    use super::queue_store::{delete_in, exists_in, load_all_in, save_in};
    use std::path::PathBuf;

    /// One file per pending submission, named by its GUID.
    pub fn pending_dir() -> PathBuf {
        let mut p = crate::preferences::preferences_path();
        p.push("pending-feedback");
        p
    }

    pub fn save(record: &PendingFeedback) {
        save_in(&pending_dir(), &record.submission_id, record);
    }

    pub fn delete(submission_id: &str) {
        delete_in(&pending_dir(), submission_id);
    }

    pub fn load_all() -> Vec<PendingFeedback> {
        load_all_in(&pending_dir())
    }

    pub fn exists(submission_id: &str) -> bool {
        exists_in(&pending_dir(), submission_id)
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub use fs_store::{delete, exists, load_all, save};

/// The upload side of both queues: one attempt per record at a time, the
/// file deleted only once the server has accepted it.
#[cfg(not(target_arch = "wasm32"))]
pub mod sync {
    use super::RETRY_INTERVAL;
    use std::collections::HashSet;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// What one upload attempt did.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Outcome {
        /// The server has the report; its local file is gone.
        Sent,
        /// The upload failed; the report stays on disk for the next retry.
        Pending(sqlly_core::DisplayMessage),
        /// The server asked us to wait; the report stays on disk and is not
        /// retried before `until`.
        Deferred {
            reason: sqlly_core::DisplayMessage,
            until: chrono::DateTime<chrono::Utc>,
        },
        /// The server will never accept this report (too large, malformed);
        /// its local file is gone.
        Rejected(sqlly_core::DisplayMessage),
        /// Another attempt for the same record is already running (or the
        /// record was sent meanwhile) — nothing to do.
        Skipped,
        /// The stored record can never upload (corrupt) and was removed.
        Dropped,
    }

    /// What to do with a report after a failed upload.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Disposition {
        /// Transient (offline, 5xx, timeout): retry on the next pass.
        Retry,
        /// Wait until the given time; `whole_queue` pauses every report.
        Defer {
            until: chrono::DateTime<chrono::Utc>,
            whole_queue: bool,
        },
        /// Permanent: the server will never take it. Delete it.
        Drop,
    }

    /// How long to hold a report the server refused to authenticate (the
    /// build's key may have been retired; a newer build can still send it).
    pub const AUTH_REFUSED_WAIT: chrono::Duration = chrono::Duration::hours(24);

    /// Extra wait, in seconds, added after a rate limit resets: a random
    /// number in this range, so throttled clients spread out instead of all
    /// retrying the moment the limit resets.
    pub const RATE_LIMIT_JITTER_SECS: std::ops::RangeInclusive<i64> = 2_000..=18_000;

    /// A fresh random pick from [`RATE_LIMIT_JITTER_SECS`].
    pub fn rate_limit_jitter_secs() -> i64 {
        use rand::RngExt as _;
        rand::rng().random_range(RATE_LIMIT_JITTER_SECS)
    }

    /// The retry policy, by failure. Pure: `jitter_secs` is the random extra
    /// wait for a rate limit (see [`rate_limit_jitter_secs`]).
    pub fn disposition(
        error: &crate::store::StoreError,
        now: chrono::DateTime<chrono::Utc>,
        jitter_secs: i64,
    ) -> Disposition {
        use crate::store::StoreError;
        match error {
            // Every rate limit is retryable, but the whole queue waits: the
            // next report would hit the same limit.
            StoreError::FeedbackThrottled {
                retry_after_secs, ..
            } => Disposition::Defer {
                until: now
                    + chrono::Duration::seconds(*retry_after_secs as i64)
                    + chrono::Duration::seconds(jitter_secs),
                whole_queue: true,
            },
            StoreError::Server {
                status: 400 | 413 | 422,
                ..
            } => Disposition::Drop,
            StoreError::Server {
                status: 401 | 403, ..
            }
            | StoreError::FeedbackUnsigned
            | StoreError::Standalone => Disposition::Defer {
                until: now + AUTH_REFUSED_WAIT,
                whole_queue: true,
            },
            _ => Disposition::Retry,
        }
    }

    static IN_FLIGHT: Mutex<Option<HashSet<String>>> = Mutex::new(None);
    static STARTED: AtomicBool = AtomicBool::new(false);
    /// A queue-wide pause: no upload, background or new, is attempted before
    /// `until`. `reason` names the limit that caused it, for the status bar.
    #[derive(Clone)]
    struct QueuePause {
        until: chrono::DateTime<chrono::Utc>,
        reason: sqlly_core::DisplayMessage,
    }

    /// Set when a failure affects every report (any rate limit, auth).
    static QUEUE_PAUSE: Mutex<Option<QueuePause>> = Mutex::new(None);

    fn active_pause(now: chrono::DateTime<chrono::Utc>) -> Option<QueuePause> {
        let guard = QUEUE_PAUSE.lock().unwrap_or_else(|e| e.into_inner());
        guard.clone().filter(|pause| pause.until > now)
    }

    fn pause_queue_until(until: chrono::DateTime<chrono::Utc>, reason: sqlly_core::DisplayMessage) {
        let mut guard = QUEUE_PAUSE.lock().unwrap_or_else(|e| e.into_inner());
        if guard.as_ref().is_none_or(|current| current.until < until) {
            *guard = Some(QueuePause { until, reason });
        }
    }

    /// Exclusive right to upload one record; released on drop.
    struct Claim(String);

    impl Claim {
        fn take(id: &str) -> Option<Self> {
            let mut guard = IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
            guard
                .get_or_insert_with(HashSet::new)
                .insert(id.to_string())
                .then(|| Claim(id.to_string()))
        }
    }

    impl Drop for Claim {
        fn drop(&mut self) {
            let mut guard = IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(set) = guard.as_mut() {
                set.remove(&self.0);
            }
        }
    }

    /// Whether a record may be tried now: not paused, not waiting.
    fn due(
        next_attempt_at: Option<chrono::DateTime<chrono::Utc>>,
        now: chrono::DateTime<chrono::Utc>,
    ) -> bool {
        active_pause(now).is_none() && next_attempt_at.is_none_or(|at| at <= now)
    }

    /// The shared attempt: claim `id`, skip it if its file is already gone,
    /// run `send`, then delete (success or permanent refusal) or keep it
    /// with the wait the failure calls for.
    async fn attempt<Fut>(
        id: &str,
        still_pending: impl FnOnce() -> bool,
        send: Fut,
        on_sent: impl FnOnce(),
        on_failed: impl FnOnce(Option<chrono::DateTime<chrono::Utc>>),
    ) -> Outcome
    where
        Fut: std::future::Future<Output = Result<(), crate::store::StoreError>>,
    {
        let Some(_claim) = Claim::take(id) else {
            return Outcome::Skipped;
        };
        if !still_pending() {
            return Outcome::Skipped;
        }
        let now = chrono::Utc::now();
        if let Some(pause) = active_pause(now) {
            on_failed(Some(pause.until));
            return Outcome::Deferred {
                reason: pause.reason,
                until: pause.until,
            };
        }
        match send.await {
            Ok(()) => {
                on_sent();
                Outcome::Sent
            }
            Err(error) => match disposition(&error, now, rate_limit_jitter_secs()) {
                Disposition::Retry => {
                    on_failed(None);
                    Outcome::Pending(error.into())
                }
                Disposition::Defer { until, whole_queue } => {
                    let reason: sqlly_core::DisplayMessage = error.into();
                    if whole_queue {
                        pause_queue_until(until, reason.clone());
                    }
                    on_failed(Some(until));
                    Outcome::Deferred { reason, until }
                }
                Disposition::Drop => {
                    on_sent();
                    Outcome::Rejected(error.into())
                }
            },
        }
    }

    /// The signed-in session's token, when there is one: signed-in reports
    /// are linked to the account and exempt from the anonymous quotas.
    fn bearer() -> Option<String> {
        crate::store::session::load()
            .filter(|s| !s.is_expired())
            .map(|s| s.access_token)
    }

    /// One upload attempt for an "Improve IntelliSense" record. Runs on the
    /// tokio runtime.
    pub async fn upload_intellisense(record: super::PendingFeedback) -> Outcome {
        let Some(context) = record.context_brotli() else {
            super::delete(&record.submission_id);
            return Outcome::Dropped;
        };
        let id = record.submission_id.clone();
        let send = async {
            crate::store::submit_intellisense_feedback(
                &record.submission_id,
                record.device_id.as_deref(),
                &record.expected_text,
                &context,
                bearer(),
            )
            .await
            .map(|_| ())
        };
        let mut failed = record.clone();
        attempt(
            &id,
            || super::exists(&id),
            send,
            || super::delete(&id),
            move |wait| {
                failed.attempts += 1;
                failed.next_attempt_at = wait;
                super::save(&failed);
            },
        )
        .await
    }

    /// One upload attempt for a Send Feedback record. Runs on the tokio
    /// runtime.
    pub async fn upload_general(
        record: crate::general_feedback::PendingGeneralFeedback,
    ) -> Outcome {
        use crate::general_feedback as queue;

        let id = record.submission_id().to_string();
        let body = record.body_bytes();
        let send = async {
            // Signed in → the report is linked to the account.
            crate::store::submit_general_feedback(body, bearer())
                .await
                .map(|_| ())
        };
        let mut failed = record;
        attempt(
            &id,
            || queue::exists(&id),
            send,
            || queue::delete(&id),
            move |wait| {
                failed.attempts += 1;
                failed.next_attempt_at = wait;
                queue::save(&failed);
            },
        )
        .await
    }

    /// Re-send every report still on disk, both channels. Returns how many
    /// went through.
    pub async fn sync_all() -> usize {
        let mut sent = 0;
        for record in super::load_all() {
            if !due(record.next_attempt_at, chrono::Utc::now()) {
                continue;
            }
            let id = record.submission_id.clone();
            log_outcome(
                "IntelliSense feedback",
                &id,
                upload_intellisense(record).await,
                &mut sent,
            );
        }
        for record in crate::general_feedback::load_all() {
            if !due(record.next_attempt_at, chrono::Utc::now()) {
                continue;
            }
            let id = record.submission_id().to_string();
            log_outcome(
                "Send Feedback report",
                &id,
                upload_general(record).await,
                &mut sent,
            );
        }
        sent
    }

    fn log_outcome(what: &str, id: &str, outcome: Outcome, sent: &mut usize) {
        match outcome {
            Outcome::Sent => *sent += 1,
            Outcome::Pending(reason) => tracing::info!("{what} {id} still pending: {reason}"),
            Outcome::Deferred { reason, until } => {
                tracing::info!("{what} {id} waiting until {until}: {reason}")
            }
            Outcome::Rejected(reason) => {
                tracing::warn!("{what} {id} refused by the server and dropped: {reason}")
            }
            Outcome::Skipped | Outcome::Dropped => {}
        }
    }

    /// Start the background worker (once per process, whatever the number of
    /// windows): an immediate pass over reports left by earlier runs, then
    /// one every [`RETRY_INTERVAL`].
    pub fn start() {
        if crate::intellisense_feedback::feedback_upload()
            != crate::intellisense_feedback::FeedbackUpload::Available
        {
            return;
        }
        if STARTED.swap(true, Ordering::SeqCst) {
            return;
        }
        crate::tokio_handle().spawn(async {
            loop {
                let sent = sync_all().await;
                if sent > 0 {
                    tracing::info!("feedback sync: uploaded {sent} pending report(s)");
                }
                tokio::time::sleep(RETRY_INTERVAL).await;
            }
        });
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::cell::Cell;

        fn run<F: std::future::Future>(f: F) -> F::Output {
            crate::ensure_test_tokio_runtime();
            crate::tokio_handle().block_on(f)
        }

        /// The queue pause is process-wide; tests that run uploads take
        /// turns so one test's pause can't leak into another.
        fn serial() -> std::sync::MutexGuard<'static, ()> {
            static LOCK: Mutex<()> = Mutex::new(());
            let guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
            *QUEUE_PAUSE.lock().unwrap_or_else(|e| e.into_inner()) = None;
            guard
        }

        use crate::store::StoreError;

        #[test]
        fn success_deletes_and_failure_keeps_the_record() {
            let _serial = serial();
            let deleted = Cell::new(false);
            let failed = Cell::new(false);
            let outcome = run(attempt(
                "sync-test-ok",
                || true,
                async { Ok::<(), StoreError>(()) },
                || deleted.set(true),
                |_| failed.set(true),
            ));
            assert_eq!(outcome, Outcome::Sent);
            assert!(deleted.get() && !failed.get());

            let deleted = Cell::new(false);
            let failed = Cell::new(None);
            let outcome = run(attempt(
                "sync-test-fail",
                || true,
                async { Err::<(), _>(StoreError::Network) },
                || deleted.set(true),
                |wait| failed.set(Some(wait)),
            ));
            assert!(matches!(outcome, Outcome::Pending(_)));
            assert!(!deleted.get());
            assert_eq!(failed.get(), Some(None), "transient: retry next pass");
        }

        #[test]
        fn a_report_the_server_will_never_accept_is_dropped() {
            let _serial = serial();
            for status in [400, 413, 422] {
                let deleted = Cell::new(false);
                let outcome = run(attempt(
                    &format!("sync-test-drop-{status}"),
                    || true,
                    async {
                        Err::<(), _>(StoreError::Server {
                            status,
                            message: "nope".into(),
                        })
                    },
                    || deleted.set(true),
                    |_| panic!("a permanent refusal must not be kept for retry"),
                ));
                assert!(matches!(outcome, Outcome::Rejected(_)), "{status}");
                assert!(deleted.get(), "{status}: the file is removed");
            }
        }

        #[test]
        fn policy_by_failure() {
            use crate::store::ThrottleScope;
            let now = chrono::Utc::now();
            assert_eq!(
                disposition(&StoreError::Network, now, 5000),
                Disposition::Retry
            );
            assert_eq!(
                disposition(
                    &StoreError::Server {
                        status: 503,
                        message: String::new()
                    },
                    now,
                    5000
                ),
                Disposition::Retry
            );
            // Every rate limit: retryable, whole queue waits for the reset
            // plus the random extra.
            for scope in [
                ThrottleScope::Device,
                ThrottleScope::Network,
                ThrottleScope::Global,
                ThrottleScope::RequestRate,
            ] {
                assert_eq!(
                    disposition(
                        &StoreError::FeedbackThrottled {
                            retry_after_secs: 600,
                            scope
                        },
                        now,
                        2_500
                    ),
                    Disposition::Defer {
                        until: now + chrono::Duration::seconds(600 + 2_500),
                        whole_queue: true
                    },
                    "{scope:?}"
                );
            }
            for error in [
                StoreError::Server {
                    status: 401,
                    message: String::new(),
                },
                StoreError::FeedbackUnsigned,
            ] {
                assert_eq!(
                    disposition(&error, now, 5000),
                    Disposition::Defer {
                        until: now + AUTH_REFUSED_WAIT,
                        whole_queue: true
                    }
                );
            }
        }

        #[test]
        fn rate_limit_jitter_stays_in_range() {
            let picks: Vec<i64> = (0..2_000).map(|_| rate_limit_jitter_secs()).collect();
            assert!(picks.iter().all(|s| (2_000..=18_000).contains(s)));
            // Random, not a constant.
            assert!(picks.iter().any(|s| *s != picks[0]));
        }

        #[test]
        fn a_rate_limit_pauses_every_report_with_its_reason() {
            let _serial = serial();
            let outcome = run(attempt(
                "sync-test-throttled",
                || true,
                async {
                    Err::<(), _>(StoreError::FeedbackThrottled {
                        retry_after_secs: 60,
                        scope: crate::store::ThrottleScope::Network,
                    })
                },
                || panic!("a rate-limited report must be kept"),
                |wait| assert!(wait.is_some()),
            ));
            let Outcome::Deferred { reason, until } = outcome else {
                panic!("expected a deferral, got {outcome:?}");
            };
            let now = chrono::Utc::now();
            assert!(until >= now + chrono::Duration::seconds(60 + 2_000 - 5));
            assert!(until <= now + chrono::Duration::seconds(60 + 18_000));

            // The next report is held without contacting the server, with
            // the same reason.
            let next = run(attempt(
                "sync-test-throttled-next",
                || true,
                async { panic!("must not send while the queue is paused") },
                || panic!("must not be sent"),
                |wait| assert_eq!(wait, Some(until)),
            ));
            assert_eq!(next, Outcome::Deferred { reason, until });
        }

        #[test]
        fn a_waiting_record_is_not_due_until_its_time() {
            // `due` also checks the shared queue pause, which the rate-limit
            // test sets; serial() holds that test off and clears the pause.
            let _serial = serial();
            let now = chrono::Utc::now();
            assert!(due(None, now));
            assert!(due(Some(now - chrono::Duration::seconds(1)), now));
            assert!(!due(Some(now + chrono::Duration::minutes(10)), now));
        }

        #[test]
        fn a_record_is_never_uploaded_twice_at_once() {
            let _serial = serial();
            let held = Claim::take("sync-test-busy").unwrap();
            let outcome = run(attempt(
                "sync-test-busy",
                || true,
                async { Ok::<(), StoreError>(()) },
                || panic!("must not send while another attempt holds the claim"),
                |_| (),
            ));
            assert_eq!(outcome, Outcome::Skipped);
            drop(held);
            assert!(Claim::take("sync-test-busy").is_some(), "claim released");
        }

        #[test]
        fn an_already_sent_record_is_skipped() {
            let _serial = serial();
            let outcome = run(attempt(
                "sync-test-gone",
                || false,
                async { Ok::<(), StoreError>(()) },
                || panic!("a vanished record must not be re-sent"),
                |_| (),
            ));
            assert_eq!(outcome, Outcome::Skipped);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(created_at: chrono::DateTime<chrono::Utc>) -> PendingFeedback {
        PendingFeedback {
            submission_id: "3f9a".to_string(),
            created_at,
            device_id: None,
            expected_text: "should offer tables".to_string(),
            context_brotli_b64: base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                [1u8, 2, 3],
            ),
            attempts: 0,
            next_attempt_at: None,
        }
    }

    #[test]
    fn retries_every_five_minutes() {
        assert_eq!(RETRY_INTERVAL.as_secs(), 300);
        assert_eq!(retry_interval_label(), "5 minutes");
    }

    #[test]
    fn context_roundtrips_through_base64() {
        let r = record(chrono::Utc::now());
        assert_eq!(r.context_brotli(), Some(vec![1, 2, 3]));
    }

    #[test]
    fn corrupt_base64_yields_none() {
        let mut r = record(chrono::Utc::now());
        r.context_brotli_b64 = "not base64!!!".to_string();
        assert_eq!(r.context_brotli(), None);
    }

    #[test]
    fn old_records_survive_any_age() {
        // No give-up horizon: a record queued long ago still loads and
        // uploads.
        let dir = tempfile::tempdir().unwrap();
        let old = record(chrono::Utc::now() - chrono::Duration::days(30));
        queue_store::save_in(dir.path(), &old.submission_id, &old);
        assert!(queue_store::exists_in(dir.path(), &old.submission_id));
        let loaded: Vec<PendingFeedback> = queue_store::load_all_in(dir.path());
        assert_eq!(loaded, vec![old]);
    }

    #[test]
    fn record_json_uses_camel_case() {
        let json = serde_json::to_value(record(chrono::Utc::now())).unwrap();
        for key in [
            "submissionId",
            "createdAt",
            "expectedText",
            "contextBrotliB64",
            "attempts",
        ] {
            assert!(json.get(key).is_some(), "missing key {key}");
        }
    }
}
