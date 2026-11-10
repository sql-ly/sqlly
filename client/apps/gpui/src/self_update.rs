//! In-app download and install of a newer desktop build.
//!
//! Three steps, run from the update dialog:
//!
//! 1. [`download`] fetches this platform's artifact (`SQLLY-<os>-<arch>-<ver>`
//!    `.dmg` / `.zip` / `.tar.gz`) into a scratch folder, reporting progress.
//! 2. [`prepare`] unpacks and checks it while SQLLY keeps running, staging the
//!    new files beside the installed copy (same volume, so the swap is a rename).
//! 3. [`launch_installer`] starts a detached helper script that waits for this
//!    process to exit, swaps the staged files into place (putting the old ones
//!    back if any move fails), and relaunches SQLLY. The caller then quits.
//!
//! Whenever the running copy can't replace itself — a dev build outside an app
//! bundle, a folder the user can't write to, a macOS App Translocation path, a
//! store edition — [`unsupported_reason`] says why and the dialog falls back
//! to opening the download in the browser.

// The browser build only reaches `unsupported_reason`; the rest is native.
#![cfg_attr(target_arch = "wasm32", allow(dead_code))]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Bytes received so far and the expected total (0 while unknown), shared
/// between the download task and the dialog's progress poll, plus the
/// dialog's request to stop.
#[derive(Debug, Default)]
pub struct DownloadProgress {
    received: AtomicU64,
    total: AtomicU64,
    /// Set once the user cancels; wakes a download blocked on the network.
    cancelled: Latch,
    /// Set once the job behind this progress has fully wound down (download
    /// stopped, staging removed), so the next attempt can wait for it
    /// instead of racing it for the same scratch and staging paths.
    finished: Latch,
    /// The job's result, parked here for the dialog to collect. Guarded
    /// together with the cancel flag so a cancel and a finishing job never
    /// miss each other: whichever comes second sees the first.
    outcome: std::sync::Mutex<Option<Result<PreparedUpdate, UpdateError>>>,
    /// Set once the dialog took the parked result, so the job knows its
    /// staging is in the dialog's hands and may wind down.
    collected: Latch,
    /// Set while the job is queued behind an earlier attempt that is still
    /// removing its files, so the dialog can say so instead of showing an
    /// empty download.
    waiting_for_previous: AtomicBool,
    /// Version folders a stage let go of without removing them (it
    /// unwound): parked here by [`RemoveOnDrop`] for the job to remove,
    /// awaited, before it reports anything, so the next attempt never
    /// races that removal.
    #[cfg(not(target_arch = "wasm32"))]
    orphaned_dirs: std::sync::Mutex<Vec<PathBuf>>,
}

/// A flag that is set once and can be awaited: the one primitive behind
/// the progress's cancelled, finished and collected states. Natively a
/// [`tokio_util::sync::CancellationToken`], which keeps a set that landed
/// before any waiter and wakes every waiter, so none of that is written
/// here; the browser build has no runtime to wait on and keeps the flag.
#[derive(Default)]
pub struct Latch {
    #[cfg(not(target_arch = "wasm32"))]
    token: tokio_util::sync::CancellationToken,
    #[cfg(target_arch = "wasm32")]
    set: AtomicBool,
}

impl std::fmt::Debug for Latch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Latch")
            .field("set", &self.is_set())
            .finish()
    }
}

impl Latch {
    pub fn set(&self) {
        #[cfg(not(target_arch = "wasm32"))]
        self.token.cancel();
        #[cfg(target_arch = "wasm32")]
        self.set.store(true, Ordering::SeqCst);
    }

    pub fn is_set(&self) -> bool {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.token.is_cancelled()
        }
        #[cfg(target_arch = "wasm32")]
        {
            self.set.load(Ordering::SeqCst)
        }
    }

    /// Resolves once [`Self::set`] has been called (at once if it already was).
    #[cfg(not(target_arch = "wasm32"))]
    pub async fn wait(&self) {
        self.token.cancelled().await;
    }
}

/// What the dialog's poll finds on its progress, in the order it must look:
/// a job that ended has either parked a result, was cancelled, or died.
#[derive(Debug)]
pub enum Verdict {
    /// Still downloading or unpacking.
    Pending,
    /// The job's result, now the dialog's to act on.
    Outcome(Result<PreparedUpdate, UpdateError>),
    /// The user cancelled; the job removed its own files.
    Cancelled,
    /// The job ended without a result and nobody cancelled it (it panicked).
    Vanished,
}

/// What [`download`] and [`prepare_unless_cancelled`] return once the user
/// cancelled; the dialog has already gone back to its idle buttons.
pub const CANCELLED: &str = "The update was cancelled.";

/// A failure from the update job. The job downloads on the tokio runtime and
/// unpacks on the blocking pool, where the UI thread's active language is not
/// visible (translation state is per thread), so it carries a catalog key and
/// arguments with an English fallback. [`localized_error`] turns it into the
/// active language's text on the UI thread.
pub type UpdateError = sqlly_core::DisplayMessage;

/// The argument of `updates.errors.command_*` messages that holds a catalog
/// key (the operation that failed) rather than display text; it is
/// translated too when the error is shown.
const OPERATION_ARG: &str = "operation";

fn english(key: &'static str) -> &'static str {
    sqlly_i18n::compiled_value(sqlly_i18n::Language::English, key).unwrap_or(key)
}

/// A keyed update failure with no arguments. Safe on any thread.
fn update_error(key: &'static str) -> UpdateError {
    UpdateError::new(key, english(key))
}

/// A keyed update failure with named arguments. Safe on any thread: the
/// fallback is built from the compiled English catalog, not the UI thread's
/// language.
fn update_error_with<T: std::fmt::Display>(
    key: &'static str,
    arguments: &[(&str, T)],
) -> UpdateError {
    let arguments = arguments
        .iter()
        .map(|(name, value)| (*name, value.to_string()))
        .collect::<Vec<_>>();
    let mut fallback = english(key).to_string();
    for (name, value) in &arguments {
        let shown = if *name == OPERATION_ARG {
            sqlly_i18n::compiled_value(sqlly_i18n::Language::English, value).unwrap_or(value)
        } else {
            value.as_str()
        };
        fallback = fallback.replace(&format!("{{{name}}}"), shown);
    }
    arguments
        .into_iter()
        .fold(UpdateError::new(key, fallback), |error, (name, value)| {
            error.with_arg(name, value)
        })
}

/// The job's result after a cancel.
pub fn cancelled() -> UpdateError {
    UpdateError::new("updates.errors.cancelled", CANCELLED)
}

/// The job's result after it died (a panic) rather than failed.
pub fn stopped_unexpectedly() -> UpdateError {
    UpdateError::new("updates.errors.stopped_unexpectedly", STOPPED_UNEXPECTEDLY)
}

/// Resolve an update failure in the active language. Call on the UI thread.
pub fn localized_error(error: &UpdateError) -> String {
    let operation = error
        .arguments()
        .find_map(|(name, value)| (name == OPERATION_ARG).then_some(value));
    let (Some(key), Some(operation)) = (error.key(), operation) else {
        return crate::localization::message(error);
    };
    let arguments = error
        .arguments()
        .map(|(name, value)| {
            let value = if name == OPERATION_ARG && sqlly_i18n::contains_key(operation) {
                crate::localization::text(operation)
            } else {
                value.to_string()
            };
            (name, value)
        })
        .collect::<Vec<_>>();
    crate::localization::format(key, &arguments)
}

/// Where a job's result went when it was delivered.
#[derive(Debug)]
pub enum Delivery {
    /// Parked for the dialog to collect; the job waits for that (or a
    /// cancel) before it winds down.
    Parked,
    /// The user had already cancelled: the staged update is the job's
    /// again, to discard.
    HandedBack(PreparedUpdate),
    /// The user had already cancelled and there was nothing staged (the
    /// job failed); nothing to park, nothing to discard.
    Dropped,
}

/// Keeps a job collectable only while the dialog's poll is alive. Dropped
/// without the result having been collected — the view was torn down, the
/// poll task replaced — it cancels the job, so a staging parked for a poll
/// that no longer exists is reclaimed and removed by the job instead of
/// waiting forever to be collected.
#[cfg(not(target_arch = "wasm32"))]
pub struct InstallHandle(std::sync::Arc<DownloadProgress>);

#[cfg(not(target_arch = "wasm32"))]
impl InstallHandle {
    pub fn new(progress: std::sync::Arc<DownloadProgress>) -> Self {
        Self(progress)
    }

    pub fn progress(&self) -> &DownloadProgress {
        &self.0
    }

    /// The poll's next step. Once a result is handed over, dropping the
    /// handle no longer cancels anything. The poll never hears "cancelled":
    /// the only cancels are the dialog's Cancel, which drops the poll in
    /// the same call, and this handle's own drop, so a job that ended with
    /// nothing to collect is simply over, the same as one that vanished.
    pub fn verdict(&self) -> Verdict {
        self.0.verdict()
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Drop for InstallHandle {
    fn drop(&mut self) {
        if !self.0.was_collected() {
            self.0.cancel();
        }
    }
}

impl DownloadProgress {
    /// Ask the running download to stop: a network wait ends at once, a
    /// chunk in hand is not written, and unpacking discards what it staged.
    /// Nothing on disk is touched here: the job owns its files and removes
    /// them itself before it reports finished, so the next attempt can wait
    /// for that instead of racing it.
    pub fn cancel(&self) {
        let _outcome = self.outcome.lock().unwrap_or_else(|e| e.into_inner());
        self.cancelled.set();
    }

    /// Park the job's result for the dialog, unless the user cancelled
    /// first: then a staged update comes back to be discarded by the job.
    pub fn deliver(&self, result: Result<PreparedUpdate, UpdateError>) -> Delivery {
        let mut outcome = self.outcome.lock().unwrap_or_else(|e| e.into_inner());
        if self.is_cancelled() {
            return match result {
                Ok(staged) => Delivery::HandedBack(staged),
                Err(_) => Delivery::Dropped,
            };
        }
        *outcome = Some(result);
        Delivery::Parked
    }

    /// A staged update the dialog never collected because the user cancelled
    /// after it was parked; the job discards it. `None` once the dialog has
    /// it, or when nothing was staged.
    pub fn reclaim(&self) -> Option<PreparedUpdate> {
        let mut outcome = self.outcome.lock().unwrap_or_else(|e| e.into_inner());
        outcome.take().and_then(Result::ok)
    }

    /// The job's parked result, once it has one. Refused after a cancel:
    /// the staging is then the job's to remove, not the dialog's to install.
    pub fn take_outcome(&self) -> Option<Result<PreparedUpdate, UpdateError>> {
        let mut outcome = self.outcome.lock().unwrap_or_else(|e| e.into_inner());
        if self.is_cancelled() {
            return None;
        }
        let taken = outcome.take();
        if taken.is_some() {
            self.collected.set();
        }
        taken
    }

    /// Whether the dialog took the parked result.
    pub fn was_collected(&self) -> bool {
        self.collected.is_set()
    }

    /// Resolves once the dialog took the parked result.
    #[cfg(not(target_arch = "wasm32"))]
    pub async fn collected(&self) {
        self.collected.wait().await;
    }

    /// What the dialog's poll should do now. Looks at `finished` before the
    /// parked result, so a job that ended is never mistaken for one still
    /// running, and a parked result is collected whether or not the job's
    /// guard dropped in between.
    pub fn verdict(&self) -> Verdict {
        let finished = self.is_finished();
        if let Some(outcome) = self.take_outcome() {
            return Verdict::Outcome(outcome);
        }
        if self.is_cancelled() {
            return Verdict::Cancelled;
        }
        if finished {
            return Verdict::Vanished;
        }
        Verdict::Pending
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.is_set()
    }

    /// Resolves once [`Self::cancel`] has been called (at once if it already was).
    #[cfg(not(target_arch = "wasm32"))]
    pub async fn cancelled(&self) {
        self.cancelled.wait().await;
    }

    /// Marks the job behind this progress as fully wound down. Called from
    /// the job's drop guard, so it also fires if the job is aborted.
    pub fn mark_finished(&self) {
        self.finished.set();
    }

    pub fn is_finished(&self) -> bool {
        self.finished.is_set()
    }

    /// Whether the job is still waiting for an earlier attempt's cleanup
    /// before it downloads anything.
    pub fn is_waiting_for_previous(&self) -> bool {
        self.waiting_for_previous.load(Ordering::Relaxed)
    }

    /// Says, before the job has even been scheduled, that it will queue
    /// behind an earlier attempt: the dialog reads this from its first
    /// frame on, and [`run_staging`] clears it once the wait is over, so
    /// the state never flips back to "downloading" in between.
    pub fn announce_waiting_for_previous(&self) {
        self.waiting_for_previous.store(true, Ordering::Relaxed);
    }

    /// Parks a version folder a dying stage left behind; the job removes it
    /// (see [`DownloadProgress::take_orphaned_dirs`]).
    #[cfg(not(target_arch = "wasm32"))]
    fn orphan_dir(&self, dir: PathBuf) {
        self.orphaned_dirs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(dir);
    }

    /// The folders parked by [`DownloadProgress::orphan_dir`], handed to
    /// whoever will remove them.
    #[cfg(not(target_arch = "wasm32"))]
    fn take_orphaned_dirs(&self) -> Vec<PathBuf> {
        std::mem::take(&mut *self.orphaned_dirs.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// Whether the job has parked a result the dialog has not collected.
    #[cfg(test)]
    fn has_parked_outcome(&self) -> bool {
        self.outcome
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    /// Resolves once [`Self::mark_finished`] has been called.
    #[cfg(not(target_arch = "wasm32"))]
    pub async fn finished(&self) {
        self.finished.wait().await;
    }
}

/// Marks its progress finished when dropped, so a job that ends any way —
/// done, failed, cancelled, or aborted mid-await — always reports it.
#[cfg(not(target_arch = "wasm32"))]
pub struct FinishGuard(pub std::sync::Arc<DownloadProgress>);

#[cfg(not(target_arch = "wasm32"))]
impl Drop for FinishGuard {
    /// A folder still parked here was orphaned after the job's own awaited
    /// removal, which only happens when the job itself was dropped (the
    /// runtime is shutting down): the last resort is a thread nobody
    /// waits for, since there is nobody left to wait.
    fn drop(&mut self) {
        for dir in self.0.take_orphaned_dirs() {
            std::thread::spawn(move || remove_dir_now(&dir));
        }
        self.0.mark_finished();
    }
}

/// Runs the download-and-stage `stage` to its end on behalf of `progress`,
/// then parks the result for the dialog. The job is the only thing that
/// removes its files: a cancel that lands after the staging was parked, but
/// before the dialog collected it, is discarded here, and only after that
/// does the guard report the job finished — so the next attempt, which waits
/// for that, never races this one's cleanup for the same folders.
///
/// `previous` is the job this one queued behind (a cancelled attempt that
/// may still be removing its files). The stage does not start until it has
/// finished, and this job does not report finished until it has either —
/// even when cancelled while still waiting — so the chain of attempts never
/// forgets a job that is still touching the folders.
///
/// A stage that panics is caught here, logged, and reported as a failure
/// like any other, so the job still parks a result for the dialog, still
/// waits for `previous`, and never lets the next attempt past it early.
#[cfg(not(target_arch = "wasm32"))]
pub async fn run_staging(
    progress: std::sync::Arc<DownloadProgress>,
    previous: Option<std::sync::Arc<DownloadProgress>>,
    stage: impl std::future::Future<Output = Result<PreparedUpdate, UpdateError>>,
) {
    let _finished = FinishGuard(progress.clone());
    let stage = async {
        match catch_unwind(stage).await {
            Ok(result) => result,
            Err(payload) => {
                tracing::error!(
                    panic = %panic_message(&payload),
                    "the in-app update job panicked"
                );
                Err(stopped_unexpectedly())
            }
        }
    };
    let result = match &previous {
        Some(previous) => {
            progress.waiting_for_previous.store(true, Ordering::Relaxed);
            let waited = unless_cancelled(&progress, previous.finished()).await;
            progress
                .waiting_for_previous
                .store(false, Ordering::Relaxed);
            match waited {
                Ok(()) => stage.await,
                Err(cancelled) => Err(cancelled),
            }
        }
        None => stage.await,
    };
    // A stage that unwound let go of its version folder without removing
    // it; that removal is the job's, awaited here before the result is
    // parked, so nothing the next attempt does can race it.
    for dir in progress.take_orphaned_dirs() {
        remove_dir_blocking(dir).await;
    }
    let late = match progress.deliver(result) {
        Delivery::HandedBack(late) => Some(late),
        Delivery::Dropped => None,
        Delivery::Parked => {
            tokio::select! {
                biased;
                _ = progress.collected() => None,
                _ = progress.cancelled() => progress.reclaim(),
            }
        }
    };
    if let Some(late) = late {
        let _ = tokio::task::spawn_blocking(move || discard(&late)).await;
    }
    if let Some(previous) = previous {
        previous.finished().await;
    }
}

/// What the dialog shows for a job that died (a panic) rather than failed.
pub const STOPPED_UNEXPECTEDLY: &str = "The download stopped unexpectedly.";

/// Polls `fut` to completion, turning a panic in any poll into `Err` with
/// the panic's payload. The future is not polled again after a panic.
#[cfg(not(target_arch = "wasm32"))]
async fn catch_unwind<T>(
    fut: impl std::future::Future<Output = T>,
) -> Result<T, Box<dyn std::any::Any + Send>> {
    let mut fut = Box::pin(fut);
    std::future::poll_fn(move |cx| {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| fut.as_mut().poll(cx))) {
            Ok(std::task::Poll::Ready(value)) => std::task::Poll::Ready(Ok(value)),
            Ok(std::task::Poll::Pending) => std::task::Poll::Pending,
            Err(payload) => std::task::Poll::Ready(Err(payload)),
        }
    })
    .await
}

/// The text of a panic payload, for the log.
#[cfg(not(target_arch = "wasm32"))]
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic payload".to_string())
}

/// The whole staging: download the newest build, then unpack and check it
/// on the blocking pool. What the dialog's job runs under [`run_staging`].
#[cfg(not(target_arch = "wasm32"))]
pub async fn stage_update(
    progress: std::sync::Arc<DownloadProgress>,
) -> Result<PreparedUpdate, UpdateError> {
    let (version, archive) = download(progress.clone()).await?;
    let staging = install_target().map(|t| staging_dir(&t)).ok();
    let unpack = {
        let archive = archive.clone();
        move || prepare_unless_cancelled(&version, &archive, &progress)
    };
    prepare_on_pool(archive, staging, unpack).await
}

/// Where [`prepare`] unpacks the new build before swapping it in: beside
/// the installed copy, on the same volume, so the swap is a rename. One
/// place names it, for the unpack and for the cleanup after an unpack that
/// died.
#[cfg(not(target_arch = "wasm32"))]
fn staging_dir(target: &InstallTarget) -> PathBuf {
    match target {
        InstallTarget::MacBundle(bundle) => bundle
            .parent()
            .map(|p| p.join(MAC_STAGING_NAME))
            .unwrap_or_else(|| PathBuf::from(MAC_STAGING_NAME)),
        InstallTarget::WindowsDir { dir, .. } => dir.join(STAGING_DIR_NAME),
        InstallTarget::LinuxExe(exe) => exe
            .parent()
            .map(|p| p.join(STAGING_DIR_NAME))
            .unwrap_or_else(|| PathBuf::from(STAGING_DIR_NAME)),
    }
}

/// The staged bundle beside `SQLLY.app` on macOS.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const MAC_STAGING_NAME: &str = ".SQLLY-update.app";
/// The staging folder beside the executable on Windows and Linux.
#[cfg_attr(target_os = "macos", allow(dead_code))]
const STAGING_DIR_NAME: &str = ".sqlly-update";

/// Runs `prepare` (the unpack of `archive`) on the blocking pool. A prepare
/// that panics is logged and reported as a failure, and what it may have
/// left behind goes: the archive's folder, as for a prepare that failed
/// outright, and `staging`, the half-unpacked build beside the installed
/// copy, which only a prepare that finished would remove.
#[cfg(not(target_arch = "wasm32"))]
async fn prepare_on_pool(
    archive: PathBuf,
    staging: Option<PathBuf>,
    prepare: impl FnOnce() -> Result<PreparedUpdate, UpdateError> + Send + 'static,
) -> Result<PreparedUpdate, UpdateError> {
    match tokio::task::spawn_blocking(prepare).await {
        Ok(result) => result,
        Err(died) => {
            if died.is_panic() {
                tracing::error!(
                    panic = %panic_message(&*died.into_panic()),
                    "unpacking the in-app update panicked"
                );
            }
            if let Some(dir) = archive.parent() {
                remove_dir_blocking(dir.to_path_buf()).await;
            }
            if let Some(staging) = staging {
                remove_dir_blocking(staging).await;
            }
            Err(update_error("updates.errors.unpack_stopped"))
        }
    }
}

/// Runs `fut` unless the user cancels first, in which case it is dropped and
/// [`CANCELLED`] comes back.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) async fn unless_cancelled<T>(
    progress: &DownloadProgress,
    fut: impl std::future::Future<Output = T>,
) -> Result<T, UpdateError> {
    tokio::select! {
        biased;
        _ = progress.cancelled() => Err(cancelled()),
        value = fut => Ok(value),
    }
}

impl DownloadProgress {
    pub fn snapshot(&self) -> (u64, Option<u64>) {
        let total = self.total.load(Ordering::Relaxed);
        (
            self.received.load(Ordering::Relaxed),
            (total > 0).then_some(total),
        )
    }
}

/// Where the in-app install stands, as the update dialog shows it.
#[derive(Debug, Clone, Default)]
pub enum InstallState {
    #[default]
    Idle,
    Downloading {
        received: u64,
        total: Option<u64>,
    },
    /// Unpacking and checking the download.
    Preparing,
    /// Queued behind an earlier, cancelled attempt that is still removing
    /// its files from the same folders.
    CleaningUp,
    /// Staged and checked; restarting finishes the install.
    Ready(PreparedUpdate),
    Failed(String),
}

impl InstallState {
    pub fn is_busy(&self) -> bool {
        matches!(
            self,
            Self::Downloading { .. } | Self::Preparing | Self::CleaningUp
        )
    }
}

/// A staged update, ready for [`launch_installer`].
#[derive(Debug, Clone)]
pub struct PreparedUpdate {
    pub version: String,
    /// `(staged, installed)` pairs, swapped in order.
    moves: Vec<(PathBuf, PathBuf)>,
    /// What the helper starts once the swap is done (or rolled back).
    relaunch: PathBuf,
    /// Scratch folders the helper removes after a successful swap.
    cleanup: Vec<PathBuf>,
}

/// The installed copy the update replaces.
#[derive(Debug, Clone, PartialEq, Eq)]
enum InstallTarget {
    /// `/Applications/SQLLY.app`, replaced as a whole bundle.
    MacBundle(PathBuf),
    /// The folder holding `SQLLY.exe` and `duckdb.dll`.
    WindowsDir { dir: PathBuf, exe: PathBuf },
    /// The `SQLLY` binary, inside an unpacked tarball or on its own.
    LinuxExe(PathBuf),
}

/// Percent text for the dialog button: "42%", or the received size while the
/// server sent no length.
pub fn progress_label(received: u64, total: Option<u64>) -> String {
    match total {
        Some(total) if total > 0 => format!("{}%", (received.saturating_mul(100) / total).min(100)),
        _ => format!("{:.1} MB", received as f64 / 1_048_576.0),
    }
}

/// Why this copy can't update itself, or `None` when it can.
pub fn unsupported_reason() -> Option<String> {
    if !crate::distribution::checks_for_updates() {
        return Some(crate::localization::text("updates.errors.store_managed"));
    }
    #[cfg(target_arch = "wasm32")]
    {
        Some(crate::localization::text("updates.errors.browser_current"))
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        install_target().err().map(|error| localized_error(&error))
    }
}

/// The scratch folder for downloads and DMG mounts.
#[cfg(not(target_arch = "wasm32"))]
fn work_root() -> PathBuf {
    std::env::temp_dir().join("sqlly-update")
}

/// Where a failed swap leaves its reason for the next launch to report.
#[cfg(not(target_arch = "wasm32"))]
fn failure_marker() -> PathBuf {
    work_root().join("last-failure.txt")
}

/// The reason the last helper run could not install the update, removed once
/// read so it is reported a single time.
pub fn take_install_failure() -> Option<String> {
    #[cfg(target_arch = "wasm32")]
    {
        None
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let path = failure_marker();
        let text = std::fs::read_to_string(&path).ok()?;
        let _ = std::fs::remove_file(&path);
        let text = text.trim().to_string();
        (!text.is_empty()).then_some(text)
    }
}

/// Finds the copy to replace from the running executable.
#[cfg(not(target_arch = "wasm32"))]
fn install_target() -> Result<InstallTarget, UpdateError> {
    let exe = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .map_err(|_| update_error("updates.errors.install_location"))?;
    let target = target_for_exe(&exe, std::env::consts::OS)?;
    let parent = match &target {
        InstallTarget::MacBundle(bundle) => bundle.parent(),
        InstallTarget::WindowsDir { dir, .. } => Some(dir.as_path()),
        InstallTarget::LinuxExe(exe) => exe.parent(),
    }
    .ok_or_else(|| update_error("updates.errors.install_location"))?;
    if tempfile::Builder::new()
        .prefix(".sqlly-write-check")
        .tempfile_in(parent)
        .is_err()
    {
        return Err(update_error_with(
            "updates.errors.install_not_writable",
            &[("path", &parent.display().to_string())],
        ));
    }
    Ok(target)
}

/// The install target for `exe` on `os`. Pure, so the path rules are testable
/// on any host.
fn target_for_exe(exe: &Path, os: &str) -> Result<InstallTarget, UpdateError> {
    match os {
        "macos" => {
            let text = exe.to_string_lossy();
            if text.contains("/AppTranslocation/") {
                return Err(update_error("updates.errors.translocated"));
            }
            // …/SQLLY.app/Contents/MacOS/SQLLY
            let macos = exe.parent().filter(|p| p.ends_with("Contents/MacOS"));
            let bundle = macos
                .and_then(|p| p.parent())
                .and_then(|p| p.parent())
                .filter(|p| p.extension().is_some_and(|e| e == "app"));
            match bundle {
                Some(bundle) if bundle.starts_with("/Volumes") => {
                    Err(update_error("updates.errors.disk_image"))
                }
                Some(bundle) => Ok(InstallTarget::MacBundle(bundle.to_path_buf())),
                None => Err(update_error("updates.errors.not_app_bundle")),
            }
        }
        "windows" => {
            let dir = exe
                .parent()
                .ok_or_else(|| update_error("updates.errors.install_location"))?;
            Ok(InstallTarget::WindowsDir {
                dir: dir.to_path_buf(),
                exe: exe.to_path_buf(),
            })
        }
        "linux" => {
            if exe.file_name().is_some_and(|n| n == "SQLLY") {
                Ok(InstallTarget::LinuxExe(exe.to_path_buf()))
            } else {
                Err(update_error("updates.errors.not_packaged_binary"))
            }
        }
        other => Err(update_error_with(
            "updates.errors.unsupported_os",
            &[("os", other)],
        )),
    }
}

/// The artifact extension this platform installs from.
fn expected_extension(os: &str) -> &'static str {
    match os {
        "macos" => ".dmg",
        "windows" => ".zip",
        _ => ".tar.gz",
    }
}

/// Rejects a server-supplied file name that isn't a plain app artifact for
/// this platform (no path segments, `SQLLY-` prefix, the platform's archive
/// type), since it becomes a local path.
fn validate_file_name(name: &str, os: &str) -> Result<(), UpdateError> {
    let plain = !name.is_empty()
        && !name.contains(['/', '\\'])
        && !name.contains("..")
        && name.starts_with("SQLLY-")
        && name.ends_with(expected_extension(os));
    if plain {
        Ok(())
    } else {
        Err(update_error_with(
            "updates.errors.unexpected_download",
            &[("name", name)],
        ))
    }
}

/// The absolute URL for the artifact: the server's stable, download-counted
/// path, or one built from version + file when the response omitted it.
fn artifact_url(base: &str, latest: &crate::store::LatestVersion) -> String {
    let base = base.trim_end_matches('/');
    if latest.url.starts_with('/') {
        format!("{base}{}", latest.url)
    } else {
        format!("{base}/downloads/{}/{}", latest.version, latest.file_name)
    }
}

/// The User-Agent the update download sends. The server counts downloads
/// per platform but drops any without a User-Agent (or with an HTTP-library
/// one) as a crawler's, so an in-app update must name itself to show up in
/// the platform download stats.
#[cfg(not(target_arch = "wasm32"))]
fn update_user_agent() -> String {
    format!(
        "SQLLY/{} ({}; {}; in-app update)",
        crate::about::APP_VERSION,
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}

/// A SHA-256 as the server publishes it (64 hex digits), decoded to bytes.
/// `None` for an absent or malformed value, which is never trusted.
fn parse_sha256(hex: &str) -> Option<[u8; 32]> {
    let hex = hex.trim();
    if hex.len() != 64 || !hex.is_ascii() {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// Whether a computed digest equals the published one.
fn checksum_matches(computed: &[u8], expected: &[u8; 32]) -> bool {
    computed == expected.as_slice()
}

/// Downloads the newest build for this platform into the scratch folder and
/// returns `(version, archive path)`. The file is hashed as it streams and
/// discarded unless it matches the release's published SHA-256. Run on the
/// tokio runtime.
#[cfg(not(target_arch = "wasm32"))]
pub async fn download(
    progress: std::sync::Arc<DownloadProgress>,
) -> Result<(String, PathBuf), UpdateError> {
    let latest = unless_cancelled(&progress, crate::store::fetch_latest_version())
        .await?
        .map_err(|_| update_error("updates.errors.download_server_unreachable"))?
        .ok_or_else(|| update_error("updates.errors.no_build"))?;
    if !crate::app_update::is_newer(&latest.version, crate::about::APP_VERSION) {
        return Err(update_error("updates.errors.already_current"));
    }
    validate_file_name(&latest.file_name, std::env::consts::OS)?;
    // Never install what can't be checked: a release without a recorded
    // checksum goes through the browser download instead.
    let expected = parse_sha256(&latest.sha256)
        .ok_or_else(|| update_error("updates.errors.checksum_missing"))?;

    // Everything that can fail without touching the disk comes before the
    // folder exists, so a failure here leaves nothing behind.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .read_timeout(std::time::Duration::from_secs(60))
        .user_agent(update_user_agent())
        .build()
        .map_err(|_| update_error("updates.errors.download_start_failed"))?;
    let url = artifact_url(&crate::store::base_url(), &latest);

    let dir = work_root().join(crate::app_update::display_version(&latest.version));
    // A previous attempt may have left a whole staged app here; removing it
    // can take seconds, so keep that off the runtime's worker threads.
    reset_dir(dir.clone()).await?;
    // Should this job die mid-download (a panic), the folder still goes;
    // every ordinary exit removes it itself or hands it on.
    let folder = RemoveOnDrop::new(dir.clone(), progress.clone());
    let dest = dir.join(&latest.file_name);
    let partial = dir.join(format!("{}.part", latest.file_name));
    if let Err(message) = fetch_verified(&client, &url, &partial, &expected, &progress).await {
        folder.remove().await;
        return Err(message);
    }
    finish_download(folder, &partial, &dest).await?;
    Ok((crate::app_update::display_version(&latest.version), dest))
}

/// A download's version folder (`work_root()/<version>`), handed to the
/// job for removal when dropped unless [`Self::keep`] was called: the
/// safety net under a stage that unwinds, where no ordinary cleanup runs.
/// Every ordinary exit removes it through [`Self::remove`] (off the
/// runtime's worker threads) or hands it on with [`Self::keep`], so the
/// folder goes exactly once.
#[cfg(not(target_arch = "wasm32"))]
struct RemoveOnDrop {
    dir: Option<PathBuf>,
    progress: std::sync::Arc<DownloadProgress>,
}

#[cfg(not(target_arch = "wasm32"))]
impl RemoveOnDrop {
    fn new(dir: PathBuf, progress: std::sync::Arc<DownloadProgress>) -> Self {
        Self {
            dir: Some(dir),
            progress,
        }
    }

    /// The folder is handed on; leave it.
    fn keep(mut self) {
        self.dir = None;
    }

    /// Remove the folder now, on the blocking pool.
    async fn remove(mut self) {
        if let Some(dir) = self.dir.take() {
            remove_dir_blocking(dir).await;
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Drop for RemoveOnDrop {
    /// A drop is not a place to delete a folder that may hold hundreds of
    /// megabytes, nor to start a removal nobody waits for: it runs on
    /// whatever thread let go of the guard, and a removal still running
    /// when the job reports finished is one the next attempt would race.
    /// The folder is parked on the progress instead; the job removes it,
    /// awaited, before it reports anything (`run_staging`), and
    /// [`FinishGuard`] catches a job that was itself dropped.
    fn drop(&mut self) {
        if let Some(dir) = self.dir.take() {
            self.progress.orphan_dir(dir);
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn save_download_error(error: &dyn std::fmt::Display) -> UpdateError {
    update_error_with("updates.errors.download_save_failed", &[("error", error)])
}

/// Give the verified download its real name. A failed rename removes the
/// version folder like every other failure in this job: nothing else would.
#[cfg(not(target_arch = "wasm32"))]
async fn finish_download(
    folder: RemoveOnDrop,
    partial: &Path,
    dest: &Path,
) -> Result<(), UpdateError> {
    // Through tokio, so a slow share or a scanner holding the file never
    // stalls a runtime worker.
    match tokio::fs::rename(partial, dest).await {
        Ok(()) => {
            folder.keep();
            Ok(())
        }
        Err(e) => {
            folder.remove().await;
            Err(save_download_error(&e))
        }
    }
}

/// Empty and recreate `dir` on the blocking pool.
#[cfg(not(target_arch = "wasm32"))]
async fn reset_dir(dir: PathBuf) -> Result<(), UpdateError> {
    tokio::task::spawn_blocking(move || {
        remove_dir_now(&dir);
        std::fs::create_dir_all(&dir).map_err(|e| {
            update_error_with(
                "updates.errors.download_directory_failed",
                &[
                    ("path", &dir.display().to_string()),
                    ("error", &e.to_string()),
                ],
            )
        })
    })
    .await
    .map_err(|_| update_error("updates.errors.download_start_failed"))?
}

/// The one place a download's version folder (`work_root()/<version>`) is
/// removed, with everything in it; a missing or stubborn folder is not an
/// error. A folder that will not go at first is tried again a few times:
/// a stage that unwound may have let go of its archive file with a write
/// still in flight on the blocking pool, and on Windows that open handle
/// makes the first removal fail. Blocking: callers on the runtime go
/// through [`remove_dir_blocking`] or [`RemoveOnDrop`].
#[cfg(not(target_arch = "wasm32"))]
fn remove_dir_now(dir: &Path) {
    retry_until(REMOVAL_TRIES, REMOVAL_BACKOFF, || {
        !dir.exists() || std::fs::remove_dir_all(dir).is_ok()
    });
}

#[cfg(not(target_arch = "wasm32"))]
const REMOVAL_TRIES: usize = 5;
#[cfg(not(target_arch = "wasm32"))]
const REMOVAL_BACKOFF: std::time::Duration = std::time::Duration::from_millis(100);

/// Runs `attempt` until it reports success or `tries` are used up, sleeping
/// `backoff` between attempts. Blocking.
#[cfg(not(target_arch = "wasm32"))]
fn retry_until(
    tries: usize,
    backoff: std::time::Duration,
    mut attempt: impl FnMut() -> bool,
) -> bool {
    for n in 0..tries {
        if attempt() {
            return true;
        }
        if n + 1 < tries {
            std::thread::sleep(backoff);
        }
    }
    false
}

/// [`remove_dir_now`] on the blocking pool.
#[cfg(not(target_arch = "wasm32"))]
async fn remove_dir_blocking(dir: PathBuf) {
    let _ = tokio::task::spawn_blocking(move || remove_dir_now(&dir)).await;
}

/// [`remove_dir_now`] for the folder holding `archive`, from blocking code.
#[cfg(not(target_arch = "wasm32"))]
fn remove_archive_dir(archive: &Path) {
    if let Some(dir) = archive.parent() {
        remove_dir_now(dir);
    }
}

/// Stream `url` into `partial`, keeping `progress` current, and check the
/// result's length and SHA-256. Stops with [`CANCELLED`] once the user
/// cancels. The caller removes `partial` on failure.
#[cfg(not(target_arch = "wasm32"))]
async fn fetch_verified(
    client: &reqwest::Client,
    url: &str,
    partial: &Path,
    expected: &[u8; 32],
    progress: &DownloadProgress,
) -> Result<(), UpdateError> {
    use sha2::Digest as _;
    use tokio::io::AsyncWriteExt as _;

    let mut resp = unless_cancelled(progress, client.get(url).send())
        .await?
        .map_err(|_| update_error("updates.errors.download_server_unreachable"))?;
    if !resp.status().is_success() {
        return Err(update_error_with(
            "updates.errors.download_http_failed",
            &[("status", &resp.status().as_u16().to_string())],
        ));
    }
    // Read the length now: once the body is consumed, `content_length()`
    // reports the bytes still to come (zero), not the response's size.
    let expected_len = resp.content_length();
    progress
        .total
        .store(expected_len.unwrap_or(0), Ordering::Relaxed);

    // Written through tokio's file so the disk waits (and the final fsync)
    // happen off the runtime's worker threads, which other work shares.
    // Buffered, so the blocking pool sees one write per megabyte rather
    // than one per network chunk.
    let mut file = tokio::io::BufWriter::with_capacity(
        1 << 20,
        tokio::fs::File::create(partial)
            .await
            .map_err(|e| save_download_error(&e))?,
    );
    let mut received = 0u64;
    let mut hasher = sha2::Sha256::new();
    // Whatever way the stream ends, the file is closed before this returns:
    // the caller removes the folder next, and a write still in flight on
    // the blocking pool would hold the file open under it (on Windows, a
    // removal that then fails).
    let streamed: Result<(), UpdateError> = async {
        while let Some(chunk) = unless_cancelled(progress, resp.chunk())
            .await?
            .map_err(|_| update_error("updates.errors.download_interrupted"))?
        {
            file.write_all(&chunk)
                .await
                .map_err(|e| save_download_error(&e))?;
            hasher.update(&chunk);
            received += chunk.len() as u64;
            progress.received.store(received, Ordering::Relaxed);
        }
        file.flush().await.map_err(|e| save_download_error(&e))?;
        file.get_ref()
            .sync_all()
            .await
            .map_err(|e| save_download_error(&e))
    }
    .await;
    // Waits for any write the pool is still doing, then closes the handle;
    // on a failed stream the unwritten buffer is dropped with it.
    let mut file = file.into_inner();
    let _ = file.flush().await;
    drop(file);
    streamed?;
    if let Some(total) = expected_len
        && total != received
    {
        return Err(update_error("updates.errors.download_incomplete"));
    }
    if !checksum_matches(&hasher.finalize(), expected) {
        return Err(update_error("updates.errors.checksum_mismatch"));
    }
    Ok(())
}

/// [`prepare`], unless the user cancelled first or while it ran; a staging
/// that finishes after a cancel is removed again, and so is the downloaded
/// archive's folder, whichever point the cancel landed at. A failed unpack
/// removes the archive's folder too: the job is the only thing that cleans
/// up after itself, and nothing else would. Blocking.
#[cfg(not(target_arch = "wasm32"))]
pub fn prepare_unless_cancelled(
    version: &str,
    archive: &Path,
    progress: &DownloadProgress,
) -> Result<PreparedUpdate, UpdateError> {
    if progress.is_cancelled() {
        remove_archive_dir(archive);
        return Err(cancelled());
    }
    let prepared = match prepare(version, archive) {
        Ok(prepared) => prepared,
        Err(message) => {
            remove_archive_dir(archive);
            return Err(message);
        }
    };
    if progress.is_cancelled() {
        discard(&prepared);
        return Err(cancelled());
    }
    Ok(prepared)
}

/// Remove everything [`prepare`] staged, leaving the installed copy alone.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn discard(prepared: &PreparedUpdate) {
    for (staged, _) in &prepared.moves {
        if staged.is_dir() {
            let _ = std::fs::remove_dir_all(staged);
        } else {
            let _ = std::fs::remove_file(staged);
        }
    }
    for dir in &prepared.cleanup {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// Unpacks and checks `archive`, staging the new files beside the installed
/// copy. Blocking; run off the UI thread.
#[cfg(not(target_arch = "wasm32"))]
pub fn prepare(version: &str, archive: &Path) -> Result<PreparedUpdate, UpdateError> {
    prepare_into(version, archive, install_target()?)
}

/// [`prepare`] for a given install shape. Whatever the unpack leaves in the
/// staging folder beside the installed copy goes with a failure: a copy
/// that ran out of disk halfway, an archive with no app in it, a build
/// signed by someone else. Only a prepared update keeps its staging, to be
/// swapped in or [`discard`]ed later.
#[cfg(not(target_arch = "wasm32"))]
fn prepare_into(
    version: &str,
    archive: &Path,
    target: InstallTarget,
) -> Result<PreparedUpdate, UpdateError> {
    let scratch = archive
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(work_root);
    let staging = staging_dir(&target);
    let unpacked = match target {
        InstallTarget::MacBundle(bundle) => prepare_mac(archive, &scratch, &bundle),
        InstallTarget::WindowsDir { dir, exe } => prepare_windows(archive, &dir, &exe),
        InstallTarget::LinuxExe(exe) => prepare_linux(archive, &exe),
    };
    let mut prepared = match unpacked {
        Ok(prepared) => prepared,
        Err(message) => {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(message);
        }
    };
    prepared.version = version.to_string();
    prepared.cleanup.push(scratch);
    Ok(prepared)
}

/// Runs `cmd`, turning a non-zero exit into its stderr.
#[cfg(not(target_arch = "wasm32"))]
fn run(cmd: &mut std::process::Command, what: &'static str) -> Result<String, UpdateError> {
    let out = cmd.output().map_err(|e| {
        update_error_with(
            "updates.errors.command_spawn_failed",
            &[(OPERATION_ARG, what), ("error", &e.to_string())],
        )
    })?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        Err(update_error_with(
            "updates.errors.command_failed",
            &[(OPERATION_ARG, what), ("error", err.trim())],
        ))
    }
}

/// `TeamIdentifier=` from `codesign -dv` output, ignoring an unsigned or
/// ad-hoc build's `not set`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn team_identifier(codesign_display: &str) -> Option<String> {
    codesign_display
        .lines()
        .find_map(|l| l.strip_prefix("TeamIdentifier="))
        .map(str::trim)
        .filter(|t| !t.is_empty() && *t != "not set")
        .map(str::to_string)
}

#[cfg(target_os = "macos")]
fn mac_team(path: &Path) -> Option<String> {
    let out = std::process::Command::new("/usr/bin/codesign")
        .args(["-dv", "--verbose=2"])
        .arg(path)
        .output()
        .ok()?;
    // codesign prints the display to stderr.
    team_identifier(&String::from_utf8_lossy(&out.stderr))
}

#[cfg(target_os = "macos")]
fn prepare_mac(dmg: &Path, scratch: &Path, bundle: &Path) -> Result<PreparedUpdate, UpdateError> {
    use std::process::Command;

    let mount = scratch.join("mount");
    let _ = std::fs::create_dir_all(&mount);
    run(
        Command::new("/usr/bin/hdiutil")
            .args([
                "attach",
                "-nobrowse",
                "-readonly",
                "-noautoopen",
                "-mountpoint",
            ])
            .arg(&mount)
            .arg(dmg),
        "updates.errors.open_disk_image",
    )?;
    // Detach however the copy goes.
    struct Detach<'a>(&'a Path);
    impl Drop for Detach<'_> {
        fn drop(&mut self) {
            let _ = std::process::Command::new("/usr/bin/hdiutil")
                .args(["detach", "-force"])
                .arg(self.0)
                .output();
        }
    }
    let _detach = Detach(&mount);

    let source = std::fs::read_dir(&mount)
        .map_err(|e| {
            update_error_with(
                "updates.errors.disk_image_read_failed",
                &[("error", &e.to_string())],
            )
        })?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|e| e == "app"))
        .ok_or_else(|| update_error("updates.errors.disk_image_no_app"))?;

    let parent = bundle
        .parent()
        .ok_or_else(|| update_error("updates.errors.install_location"))?;
    let staged = parent.join(MAC_STAGING_NAME);
    let _ = std::fs::remove_dir_all(&staged);
    run(
        Command::new("/usr/bin/ditto").arg(&source).arg(&staged),
        "updates.errors.copy_new_version",
    )?;

    let checked = (|| {
        run(
            Command::new("/usr/bin/codesign")
                .args(["--verify", "--deep", "--strict"])
                .arg(&staged),
            "updates.errors.verify_signature",
        )?;
        // A signed install only accepts a build signed by the same team.
        if let Some(team) = mac_team(bundle)
            && mac_team(&staged).as_deref() != Some(team.as_str())
        {
            return Err(update_error("updates.errors.wrong_developer"));
        }
        let plist = staged.join("Contents/Info.plist");
        let version = run(
            Command::new("/usr/libexec/PlistBuddy")
                .args(["-c", "Print :CFBundleShortVersionString"])
                .arg(&plist),
            "updates.errors.read_new_version",
        )?;
        if !crate::app_update::is_newer(version.trim(), crate::about::APP_VERSION) {
            return Err(update_error_with(
                "updates.errors.not_newer",
                &[("version", version.trim())],
            ));
        }
        Ok(())
    })();
    if let Err(e) = checked {
        let _ = std::fs::remove_dir_all(&staged);
        return Err(e);
    }

    Ok(PreparedUpdate {
        version: String::new(),
        moves: vec![(staged, bundle.to_path_buf())],
        relaunch: bundle.to_path_buf(),
        cleanup: Vec::new(),
    })
}

#[cfg(all(not(target_os = "macos"), not(target_arch = "wasm32")))]
fn prepare_mac(_: &Path, _: &Path, _: &Path) -> Result<PreparedUpdate, UpdateError> {
    Err(update_error("updates.errors.mac_requires_mac"))
}

/// Authenticode status and signer subject of `path` (`"Valid|CN=…"`).
#[cfg(target_os = "windows")]
fn windows_signature(path: &Path) -> Option<(String, String)> {
    let script = format!(
        "$s = Get-AuthenticodeSignature -LiteralPath {}; \
         \"$($s.Status)|$($s.SignerCertificate.Subject)\"",
        ps_quote(&path.to_string_lossy())
    );
    let out = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let (status, subject) = text.split_once('|')?;
    Some((status.to_string(), subject.to_string()))
}

#[cfg(target_os = "windows")]
fn prepare_windows(zip: &Path, dir: &Path, exe: &Path) -> Result<PreparedUpdate, UpdateError> {
    let staging = dir.join(STAGING_DIR_NAME);
    let _ = std::fs::remove_dir_all(&staging);
    let script = format!(
        "Expand-Archive -LiteralPath {} -DestinationPath {} -Force",
        ps_quote(&zip.to_string_lossy()),
        ps_quote(&staging.to_string_lossy())
    );
    run(
        std::process::Command::new("powershell.exe").args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &script,
        ]),
        "updates.errors.unpack_update",
    )?;
    let new_exe = staging.join("SQLLY.exe");
    if !new_exe.is_file() {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(update_error("updates.errors.windows_binary_missing"));
    }
    // A signed install only accepts a build signed by the same publisher.
    if let Some((status, subject)) = windows_signature(exe)
        && status == "Valid"
    {
        match windows_signature(&new_exe) {
            Some((s, subj)) if s == "Valid" && subj == subject => {}
            _ => {
                let _ = std::fs::remove_dir_all(&staging);
                return Err(update_error("updates.errors.wrong_publisher"));
            }
        }
    }
    let mut moves = Vec::new();
    for entry in std::fs::read_dir(&staging)
        .map_err(|e| {
            update_error_with(
                "updates.errors.update_read_failed",
                &[("error", &e.to_string())],
            )
        })?
        .filter_map(Result::ok)
    {
        let name = entry.file_name();
        let dest = if name == "SQLLY.exe" {
            exe.to_path_buf()
        } else {
            dir.join(&name)
        };
        moves.push((entry.path(), dest));
    }
    // The executable last, so a failed DLL swap never leaves a new exe
    // beside an old DLL.
    moves.sort_by_key(|(src, _)| src.file_name().is_some_and(|n| n == "SQLLY.exe"));
    Ok(PreparedUpdate {
        version: String::new(),
        moves,
        relaunch: exe.to_path_buf(),
        cleanup: vec![staging],
    })
}

#[cfg(all(not(target_os = "windows"), not(target_arch = "wasm32")))]
fn prepare_windows(_: &Path, _: &Path, _: &Path) -> Result<PreparedUpdate, UpdateError> {
    Err(update_error("updates.errors.windows_requires_windows"))
}

/// The desktop entry that marks a folder as an unpacked SQLLY tarball, whose
/// icons and helper files are refreshed along with the binary.
const LINUX_DESKTOP_FILE: &str = "io.zebrasoft.sqlly.desktop";

#[cfg(not(target_arch = "wasm32"))]
fn prepare_linux(tarball: &Path, exe: &Path) -> Result<PreparedUpdate, UpdateError> {
    let dir = exe
        .parent()
        .ok_or_else(|| update_error("updates.errors.install_location"))?;
    let staging = dir.join(STAGING_DIR_NAME);
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(|e| {
        update_error_with("updates.errors.stage_failed", &[("error", &e.to_string())])
    })?;
    if let Err(e) = run(
        std::process::Command::new("tar")
            .arg("-xzf")
            .arg(tarball)
            .arg("-C")
            .arg(&staging),
        "updates.errors.unpack_update",
    ) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e);
    }
    let root = staging.join("linux");
    let new_exe = root.join("SQLLY");
    if !new_exe.is_file() {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(update_error("updates.errors.linux_binary_missing"));
    }
    let mut moves = Vec::new();
    // In an unpacked tarball, refresh the desktop entry, icons and installer
    // too; a binary copied somewhere on its own is replaced alone.
    if dir.join(LINUX_DESKTOP_FILE).exists()
        && let Ok(entries) = std::fs::read_dir(&root)
    {
        for entry in entries.filter_map(Result::ok) {
            if entry.file_name() != "SQLLY" {
                moves.push((entry.path(), dir.join(entry.file_name())));
            }
        }
    }
    moves.push((new_exe, exe.to_path_buf()));
    Ok(PreparedUpdate {
        version: String::new(),
        moves,
        relaunch: exe.to_path_buf(),
        cleanup: vec![staging],
    })
}

/// Quotes `s` as one POSIX shell word.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Quotes `s` as one PowerShell literal string.
fn ps_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// The POSIX helper: wait for `pid` to exit, swap every staged path in (each
/// old copy kept as `<path>.sqlly-old` until all moves succeed), restore the
/// old copies if any move fails, then relaunch.
fn unix_script(update: &PreparedUpdate, pid: u32, log: &Path, failure: &Path, mac: bool) -> String {
    let q = |p: &Path| sh_quote(&p.to_string_lossy());
    let launch = if mac {
        format!("/usr/bin/open {}", q(&update.relaunch))
    } else {
        format!("nohup {} >/dev/null 2>&1 &", q(&update.relaunch))
    };
    let mut s = String::new();
    s.push_str("#!/bin/sh\n# SQLLY updater: installs a downloaded build once SQLLY exits.\n");
    s.push_str(&format!("exec >>{} 2>&1\n", q(log)));
    s.push_str(&format!(
        "i=0\nwhile kill -0 {pid} 2>/dev/null; do\n  i=$((i+1))\n  if [ \"$i\" -gt 600 ]; then echo 'SQLLY did not exit; update skipped'; exit 1; fi\n  sleep 0.2\ndone\n"
    ));
    s.push_str(
        "swap() {\n  if [ -e \"$2\" ]; then rm -rf \"$2.sqlly-old\"; mv \"$2\" \"$2.sqlly-old\" || return 1; fi\n  mv \"$1\" \"$2\"\n}\n\
         unswap() {\n  if [ -e \"$2.sqlly-old\" ]; then\n    if [ -e \"$2\" ]; then mv \"$2\" \"$1\" 2>/dev/null || rm -rf \"$2\"; fi\n    mv \"$2.sqlly-old\" \"$2\"\n  fi\n}\n",
    );
    s.push_str("rollback() {\n");
    for (n, (src, dst)) in update.moves.iter().enumerate().rev() {
        s.push_str(&format!(
            "  [ \"$step\" -ge {} ] && unswap {} {}\n",
            n + 1,
            q(src),
            q(dst)
        ));
    }
    s.push_str("  :\n}\n");
    s.push_str(&format!(
        "fail() {{\n  echo \"update failed at step $step\"\n  rollback\n  printf '%s\\n' {} >{}\n  {launch}\n  exit 1\n}}\n",
        sh_quote(
            &crate::localization::format(
                "updates.errors.install_failed_kept_previous",
                &[("version", &update.version)],
            ),
        ),
        q(failure)
    ));
    s.push_str("step=0\n");
    for (n, (src, dst)) in update.moves.iter().enumerate() {
        s.push_str(&format!(
            "step={}; swap {} {} || fail\n",
            n + 1,
            q(src),
            q(dst)
        ));
    }
    for (_, dst) in &update.moves {
        s.push_str(&format!("rm -rf {}.sqlly-old\n", q(dst)));
    }
    if mac {
        // A download made in-process carries no quarantine flag, but clear
        // one anyway so Gatekeeper never stops the relaunch.
        s.push_str(&format!(
            "/usr/bin/xattr -dr com.apple.quarantine {} 2>/dev/null\n",
            q(&update.relaunch)
        ));
    } else {
        s.push_str(&format!("chmod +x {}\n", q(&update.relaunch)));
    }
    for dir in &update.cleanup {
        s.push_str(&format!("rm -rf {}\n", q(dir)));
    }
    s.push_str(&format!("echo 'installed v{}'\n{launch}\n", update.version));
    s
}

/// The PowerShell helper, same contract as [`unix_script`].
fn windows_script(update: &PreparedUpdate, pid: u32, log: &Path, failure: &Path) -> String {
    let q = |p: &Path| ps_quote(&p.to_string_lossy());
    let steps = update
        .moves
        .iter()
        .map(|(src, dst)| format!("[void]$steps.Add(@({}, {}))", q(src), q(dst)))
        .collect::<Vec<_>>()
        .join("\n");
    let cleanup = update
        .cleanup
        .iter()
        .map(|p| {
            format!(
                "Remove-Item -LiteralPath {} -Recurse -Force -ErrorAction SilentlyContinue",
                q(p)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let relaunch = q(&update.relaunch);
    let workdir = update
        .relaunch
        .parent()
        .map(&q)
        .unwrap_or_else(|| "'.'".to_string());
    let failure_text = ps_quote(&crate::localization::format(
        "updates.errors.install_failed_kept_previous",
        &[("version", &update.version)],
    ));
    format!(
        r#"# SQLLY updater: installs a downloaded build once SQLLY exits.
$ErrorActionPreference = 'Stop'
$log = {log}
function Log($m) {{ Add-Content -LiteralPath $log -Value $m }}
for ($i = 0; $i -lt 600 -and (Get-Process -Id {pid} -ErrorAction SilentlyContinue); $i++) {{ Start-Sleep -Milliseconds 200 }}
if (Get-Process -Id {pid} -ErrorAction SilentlyContinue) {{ Log 'SQLLY did not exit; update skipped'; exit 1 }}
# An ArrayList, because @( @(a, b) ) flattens a single step into two strings.
$steps = New-Object System.Collections.ArrayList
{steps}
$done = -1
try {{
  for ($n = 0; $n -lt $steps.Count; $n++) {{
    $done = $n
    $src = $steps[$n][0]; $dst = $steps[$n][1]
    if (Test-Path -LiteralPath $dst) {{
      Remove-Item -LiteralPath "$dst.sqlly-old" -Recurse -Force -ErrorAction SilentlyContinue
      Move-Item -LiteralPath $dst -Destination "$dst.sqlly-old"
    }}
    Move-Item -LiteralPath $src -Destination $dst
  }}
}} catch {{
  Log "update failed at step $done`: $_"
  for ($n = $done; $n -ge 0; $n--) {{
    $src = $steps[$n][0]; $dst = $steps[$n][1]
    if (Test-Path -LiteralPath "$dst.sqlly-old") {{
      if (Test-Path -LiteralPath $dst) {{ Move-Item -LiteralPath $dst -Destination $src -ErrorAction SilentlyContinue }}
      Move-Item -LiteralPath "$dst.sqlly-old" -Destination $dst -Force
    }}
  }}
  Set-Content -LiteralPath {failure} -Value {failure_text}
  Start-Process -FilePath {relaunch} -WorkingDirectory {workdir}
  exit 1
}}
foreach ($s in $steps) {{ Remove-Item -LiteralPath "$($s[1]).sqlly-old" -Recurse -Force -ErrorAction SilentlyContinue }}
{cleanup}
Log 'installed v{version}'
Start-Process -FilePath {relaunch} -WorkingDirectory {workdir}
"#,
        log = q(log),
        failure = q(failure),
        version = update.version,
    )
}

#[cfg(not(target_arch = "wasm32"))]
fn installer_start_error(error: &dyn std::fmt::Display) -> UpdateError {
    update_error_with("updates.errors.installer_start_failed", &[("error", error)])
}

/// Writes the helper script and starts it detached. On success the caller
/// must quit promptly: the helper gives up after two minutes.
#[cfg(not(target_arch = "wasm32"))]
pub fn launch_installer(update: &PreparedUpdate) -> Result<(), UpdateError> {
    use std::process::{Command, Stdio};

    let root = work_root();
    std::fs::create_dir_all(&root).map_err(|e| installer_start_error(&e))?;
    let log = root.join("install.log");
    let failure = failure_marker();
    let _ = std::fs::remove_file(&failure);
    let pid = std::process::id();

    let mut cmd;
    if cfg!(target_os = "windows") {
        let script = root.join("install.ps1");
        std::fs::write(&script, windows_script(update, pid, &log, &failure))
            .map_err(|e| installer_start_error(&e))?;
        cmd = Command::new("powershell.exe");
        cmd.args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-WindowStyle",
            "Hidden",
            "-File",
        ])
        .arg(&script);
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt as _;
            const DETACHED_PROCESS: u32 = 0x0000_0008;
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
        }
    } else {
        let script = root.join("install.sh");
        std::fs::write(
            &script,
            unix_script(update, pid, &log, &failure, cfg!(target_os = "macos")),
        )
        .map_err(|e| installer_start_error(&e))?;
        cmd = Command::new("/bin/sh");
        cmd.arg(&script);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            // Its own process group, so quitting SQLLY doesn't take it down.
            cmd.process_group(0);
        }
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(drop)
        .map_err(|e| installer_start_error(&e))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn staged(version: &str) -> PreparedUpdate {
        PreparedUpdate {
            version: version.into(),
            moves: vec![],
            relaunch: PathBuf::new(),
            cleanup: vec![],
        }
    }

    fn staged_in(version: &str, dir: &Path) -> PreparedUpdate {
        PreparedUpdate {
            cleanup: vec![dir.to_path_buf()],
            ..staged(version)
        }
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    /// Let every spawned task on the single-threaded runtime run until it
    /// next waits; no wall-clock involved.
    async fn settle() {
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
    }

    /// Resolves once the job parked its result for the dialog.
    async fn parked(progress: &DownloadProgress) {
        while !progress.has_parked_outcome() {
            tokio::task::yield_now().await;
        }
    }

    /// Cancel and delivery never miss each other: whichever comes second
    /// sees the first, and the staging always ends up with the job.
    #[test]
    fn a_cancel_before_delivery_hands_the_staging_back_to_the_job() {
        let progress = DownloadProgress::default();
        progress.cancel();
        let Delivery::HandedBack(handed_back) = progress.deliver(Ok(staged("1.2.4"))) else {
            panic!("delivering after a cancel hands the staging back");
        };
        assert_eq!(handed_back.version, "1.2.4");
        assert!(
            matches!(progress.deliver(Err("nope".into())), Delivery::Dropped),
            "a failure after a cancel has nothing to hand back"
        );
        assert!(progress.take_outcome().is_none());

        let progress = DownloadProgress::default();
        assert!(matches!(
            progress.deliver(Ok(staged("1.2.3"))),
            Delivery::Parked
        ));
        progress.cancel();
        assert!(
            progress.take_outcome().is_none(),
            "the dialog must not install what the user cancelled"
        );
        assert_eq!(
            progress.reclaim().map(|p| p.version).as_deref(),
            Some("1.2.3")
        );

        let progress = DownloadProgress::default();
        assert!(matches!(
            progress.deliver(Err("nope".into())),
            Delivery::Parked
        ));
        assert!(matches!(progress.take_outcome(), Some(Err(m)) if m == "nope"));
    }

    /// A cancel that lands after the job parked its staging, but before the
    /// dialog collected it, is cleaned up by the job itself, and the job only
    /// reports finished once that cleanup is done — so the next attempt,
    /// which waits for `finished`, can't race the removal.
    #[test]
    fn a_late_cancel_is_discarded_by_the_job_before_it_finishes() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path().join("staging");
        std::fs::create_dir_all(&staging).unwrap();
        let progress = std::sync::Arc::new(DownloadProgress::default());
        runtime().block_on(async {
            let job = tokio::spawn(run_staging(
                progress.clone(),
                None,
                std::future::ready(Ok(staged_in("1.9.0", &staging))),
            ));
            parked(&progress).await;
            settle().await;
            assert!(
                !progress.is_finished(),
                "a job with an uncollected staging is not finished"
            );
            assert!(staging.exists());
            progress.cancel();
            tokio::time::timeout(std::time::Duration::from_secs(5), progress.finished())
                .await
                .expect("the job winds down after a late cancel");
            assert!(!staging.exists(), "the job removes what it staged");
            job.await.unwrap();
        });
        assert!(matches!(progress.verdict(), Verdict::Cancelled));
    }

    /// The dialog's poll owns an [`InstallHandle`]; if the poll goes away
    /// (the view was torn down, the task replaced) before it collected the
    /// result, dropping the handle cancels the job, which reclaims and
    /// removes its parked staging instead of waiting forever. A handle that
    /// did collect the result leaves the staging alone.
    #[test]
    fn dropping_the_handle_uncollected_releases_the_job() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path().join("staging");
        std::fs::create_dir_all(&staging).unwrap();
        let progress = std::sync::Arc::new(DownloadProgress::default());
        runtime().block_on(async {
            let job = tokio::spawn(run_staging(
                progress.clone(),
                None,
                std::future::ready(Ok(staged_in("1.9.0", &staging))),
            ));
            let handle = InstallHandle::new(progress.clone());
            parked(&progress).await;
            settle().await;
            assert!(!progress.is_finished());
            drop(handle);
            tokio::time::timeout(std::time::Duration::from_secs(5), progress.finished())
                .await
                .expect("the job winds down once its poll is gone");
            assert!(
                !staging.exists(),
                "the job removes the staging nobody collected"
            );
            job.await.unwrap();
        });

        let staging = dir.path().join("staging-collected");
        std::fs::create_dir_all(&staging).unwrap();
        let progress = std::sync::Arc::new(DownloadProgress::default());
        runtime().block_on(async {
            let job = tokio::spawn(run_staging(
                progress.clone(),
                None,
                std::future::ready(Ok(staged_in("1.9.1", &staging))),
            ));
            let handle = InstallHandle::new(progress.clone());
            parked(&progress).await;
            let Verdict::Outcome(Ok(prepared)) = handle.verdict() else {
                panic!("the parked staging is collected");
            };
            assert_eq!(prepared.version, "1.9.1");
            drop(handle);
            tokio::time::timeout(std::time::Duration::from_secs(5), progress.finished())
                .await
                .expect("the job finishes once its result was collected");
            job.await.unwrap();
        });
        assert!(
            !progress.is_cancelled(),
            "a collected handle cancels nothing"
        );
        assert!(staging.exists());
    }

    /// Once the dialog collects the staging, the job finishes without
    /// touching it.
    #[test]
    fn a_collected_staging_stays_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path().join("staging");
        std::fs::create_dir_all(&staging).unwrap();
        let progress = std::sync::Arc::new(DownloadProgress::default());
        runtime().block_on(async {
            let job = tokio::spawn(run_staging(
                progress.clone(),
                None,
                std::future::ready(Ok(staged_in("1.9.0", &staging))),
            ));
            parked(&progress).await;
            let Verdict::Outcome(Ok(prepared)) = progress.verdict() else {
                panic!("the parked staging is collected");
            };
            assert_eq!(prepared.version, "1.9.0");
            tokio::time::timeout(std::time::Duration::from_secs(5), progress.finished())
                .await
                .expect("the job finishes once its result was collected");
            job.await.unwrap();
        });
        assert!(staging.exists());
    }

    /// Attempts queue behind each other: B waits for A, and C for B. A B that
    /// is cancelled while still waiting for A must not finish before A does,
    /// or C would take B's place in the chain and run its download while A
    /// is still removing files from the same folders.
    #[test]
    fn a_cancelled_waiter_keeps_the_chain_to_the_job_before_it() {
        let a = std::sync::Arc::new(DownloadProgress::default());
        let b = std::sync::Arc::new(DownloadProgress::default());
        let c = std::sync::Arc::new(DownloadProgress::default());
        let c_started = std::sync::Arc::new(AtomicBool::new(false));
        runtime().block_on(async {
            // A is slow: it stays busy until released.
            let (release_a, released) = tokio::sync::oneshot::channel::<()>();
            let job_a = tokio::spawn(run_staging(a.clone(), None, async move {
                let _ = released.await;
                Err::<PreparedUpdate, UpdateError>("A gave up".into())
            }));
            let job_b = tokio::spawn(run_staging(b.clone(), Some(a.clone()), async {
                panic!("B's stage must never run: it is cancelled while waiting for A")
            }));
            settle().await;
            assert!(b.is_waiting_for_previous(), "B queues behind A");
            b.cancel();
            settle().await;
            assert!(
                !b.is_finished(),
                "a cancelled B still waits for A before it reports finished"
            );

            let started = c_started.clone();
            let job_c = tokio::spawn(run_staging(c.clone(), Some(b.clone()), async move {
                started.store(true, Ordering::Relaxed);
                Err::<PreparedUpdate, UpdateError>("C gave up".into())
            }));
            settle().await;
            assert!(c.is_waiting_for_previous(), "C queues behind B");
            assert!(
                !c_started.load(Ordering::Relaxed),
                "C must not start while A is still running"
            );

            release_a.send(()).unwrap();
            // In the app the dialog's poll collects a result; here A's is
            // never collected, so a cancel lets it wind down, and C's is
            // collected the way the poll would.
            a.cancel();
            let collected = async {
                loop {
                    if let Verdict::Outcome(outcome) = c.verdict() {
                        break outcome;
                    }
                    tokio::task::yield_now().await;
                }
            };
            let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), collected)
                .await
                .expect("C runs once A and B are finished");
            assert!(matches!(outcome, Err(m) if m == "C gave up"));
            assert!(c_started.load(Ordering::Relaxed));
            assert!(a.is_finished() && b.is_finished());
            tokio::time::timeout(std::time::Duration::from_secs(5), c.finished())
                .await
                .expect("C finishes once its result was collected");
            job_a.await.unwrap();
            job_b.await.unwrap();
            job_c.await.unwrap();
        });
    }

    /// A stage that panics is caught: the job parks a failure for the
    /// dialog like any other, and winds down once it is collected.
    #[test]
    fn a_job_that_panics_reports_a_failure() {
        let progress = std::sync::Arc::new(DownloadProgress::default());
        runtime().block_on(async {
            let job = tokio::spawn(run_staging(progress.clone(), None, async {
                panic!("malformed archive");
            }));
            parked(&progress).await;
            assert!(
                matches!(progress.verdict(), Verdict::Outcome(Err(m)) if m == STOPPED_UNEXPECTEDLY)
            );
            tokio::time::timeout(std::time::Duration::from_secs(5), progress.finished())
                .await
                .expect("the job finishes once its failure was collected");
            job.await.expect("the panic was caught inside the job");
        });
        assert!(!progress.is_cancelled());
    }

    /// A job that ended without parking anything and without a cancel (it
    /// was aborted) is what the dialog sees as vanished.
    #[test]
    fn a_job_that_ends_with_nothing_is_vanished() {
        let progress = DownloadProgress::default();
        progress.mark_finished();
        assert!(matches!(progress.verdict(), Verdict::Vanished));
    }

    /// B's stage panics once it runs behind A. The panic is caught, so B
    /// still reports a failure and still holds its place in the chain: C,
    /// queued behind B, does not start until A and B are both finished.
    #[test]
    fn a_panicking_job_keeps_its_place_in_the_chain() {
        let a = std::sync::Arc::new(DownloadProgress::default());
        let b = std::sync::Arc::new(DownloadProgress::default());
        let c = std::sync::Arc::new(DownloadProgress::default());
        let c_saw_both_finished = std::sync::Arc::new(AtomicBool::new(false));
        runtime().block_on(async {
            let (release_a, released) = tokio::sync::oneshot::channel::<()>();
            let job_a = tokio::spawn(run_staging(a.clone(), None, async move {
                let _ = released.await;
                Err::<PreparedUpdate, UpdateError>("A gave up".into())
            }));
            let job_b = tokio::spawn(run_staging(b.clone(), Some(a.clone()), async {
                panic!("B's archive is malformed")
            }));
            let (a2, b2, saw) = (a.clone(), b.clone(), c_saw_both_finished.clone());
            let job_c = tokio::spawn(run_staging(c.clone(), Some(b.clone()), async move {
                saw.store(a2.is_finished() && b2.is_finished(), Ordering::Relaxed);
                Err::<PreparedUpdate, UpdateError>("C gave up".into())
            }));
            settle().await;
            assert!(b.is_waiting_for_previous() && c.is_waiting_for_previous());

            release_a.send(()).unwrap();
            a.cancel();
            // B runs, panics, and parks a failure; the dialog collects it.
            parked(&b).await;
            assert!(matches!(b.verdict(), Verdict::Outcome(Err(m)) if m == STOPPED_UNEXPECTEDLY));
            let collected = async {
                loop {
                    if let Verdict::Outcome(outcome) = c.verdict() {
                        break outcome;
                    }
                    tokio::task::yield_now().await;
                }
            };
            let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), collected)
                .await
                .expect("C runs once A and B are finished");
            assert!(matches!(outcome, Err(m) if m == "C gave up"));
            assert!(
                c_saw_both_finished.load(Ordering::Relaxed),
                "C started only after A and B had both finished"
            );
            tokio::time::timeout(std::time::Duration::from_secs(5), c.finished())
                .await
                .unwrap();
            job_a.await.unwrap();
            job_b.await.expect("B's panic was caught inside the job");
            job_c.await.unwrap();
        });
    }

    /// An unpack that panics is reported as a failure and leaves no
    /// download behind, like an unpack that fails outright.
    #[test]
    fn a_prepare_that_panics_removes_the_download() {
        let dir = tempfile::tempdir().unwrap();
        let archive_dir = dir.path().join("1.9.0");
        std::fs::create_dir_all(&archive_dir).unwrap();
        let archive = archive_dir.join("SQLLY.dmg");
        std::fs::write(&archive, b"not really a dmg").unwrap();
        // The unpack got as far as staging a build beside the installed
        // copy before it died.
        let staging = dir.path().join("Apps").join(MAC_STAGING_NAME);
        std::fs::create_dir_all(staging.join("Contents")).unwrap();
        let result = runtime().block_on(prepare_on_pool(archive, Some(staging.clone()), || {
            panic!("the archive walker tripped")
        }));
        assert_eq!(
            result.err().as_deref(),
            Some("Unpacking the update stopped unexpectedly.")
        );
        assert!(!archive_dir.exists(), "the download folder is removed");
        assert!(!staging.exists(), "the half-staged build is removed");
    }

    /// An unpack that fails partway leaves nothing beside the installed
    /// copy: whatever it staged is removed with the failure.
    #[test]
    fn a_failed_unpack_removes_what_it_staged() {
        let dir = tempfile::tempdir().unwrap();
        let archive_dir = dir.path().join("1.9.0");
        std::fs::create_dir_all(&archive_dir).unwrap();
        let archive = archive_dir.join("SQLLY.archive");
        std::fs::write(&archive, b"not an archive of anything").unwrap();
        let apps = dir.path().join("Apps");
        std::fs::create_dir_all(&apps).unwrap();
        let target = if cfg!(target_os = "macos") {
            InstallTarget::MacBundle(apps.join("SQLLY.app"))
        } else if cfg!(target_os = "windows") {
            InstallTarget::WindowsDir {
                dir: apps.clone(),
                exe: apps.join("SQLLY.exe"),
            }
        } else {
            InstallTarget::LinuxExe(apps.join("SQLLY"))
        };
        let staging = staging_dir(&target);
        // Half a build from an unpack that got that far before failing.
        std::fs::create_dir_all(staging.join("Contents")).unwrap();
        let result = prepare_into("1.9.0", &archive, target);
        assert!(result.is_err(), "a bogus archive cannot be unpacked");
        assert!(!staging.exists(), "the half-staged build is removed");
    }

    /// The unpack and the cleanup after a dead unpack name the same staging
    /// folder, beside the installed copy, for every install shape.
    #[test]
    fn the_staging_folder_sits_beside_the_installed_copy() {
        assert_eq!(
            staging_dir(&InstallTarget::MacBundle(PathBuf::from("/Apps/SQLLY.app"))),
            PathBuf::from("/Apps/.SQLLY-update.app")
        );
        assert_eq!(
            staging_dir(&InstallTarget::WindowsDir {
                dir: PathBuf::from("C:/SQLLY"),
                exe: PathBuf::from("C:/SQLLY/SQLLY.exe"),
            }),
            PathBuf::from("C:/SQLLY/.sqlly-update")
        );
        assert_eq!(
            staging_dir(&InstallTarget::LinuxExe(PathBuf::from("/opt/sqlly/SQLLY"))),
            PathBuf::from("/opt/sqlly/.sqlly-update")
        );
    }

    /// The download's folder guard: dropped mid-job it removes the folder
    /// (on its own thread, so the wait here is for that thread); kept, it
    /// leaves it for the unpack.
    #[test]
    fn the_download_folder_goes_with_a_job_that_dies() {
        let dir = tempfile::tempdir().unwrap();
        let version_dir = dir.path().join("1.9.0");
        std::fs::create_dir_all(&version_dir).unwrap();
        std::fs::write(version_dir.join("SQLLY.dmg.part"), b"half").unwrap();
        let progress = std::sync::Arc::new(DownloadProgress::default());
        drop(RemoveOnDrop::new(version_dir.clone(), progress.clone()));
        assert!(
            version_dir.exists(),
            "a drop parks the folder for the job rather than removing it itself"
        );
        assert_eq!(progress.take_orphaned_dirs(), vec![version_dir.clone()]);

        RemoveOnDrop::new(version_dir.clone(), progress.clone()).keep();
        assert!(
            progress.take_orphaned_dirs().is_empty(),
            "a kept folder is nobody's to remove"
        );
        assert!(version_dir.exists(), "a kept folder stays");

        // The last resort: a job dropped with a folder still parked (the
        // runtime shutting down) removes it from its finish guard.
        progress.orphan_dir(version_dir.clone());
        drop(FinishGuard(progress.clone()));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while version_dir.exists() && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(!version_dir.exists());
    }

    /// A removal that fails at first (an archive file still held open by a
    /// write in flight, as on Windows) is tried again a few times, with a
    /// pause between, and gives up after the last try.
    #[test]
    fn a_removal_that_fails_at_first_is_retried() {
        let mut calls = 0;
        let ok = retry_until(5, std::time::Duration::from_millis(1), || {
            calls += 1;
            calls == 3
        });
        assert!(ok);
        assert_eq!(calls, 3, "stops at the first success");

        let mut calls = 0;
        let started = std::time::Instant::now();
        let ok = retry_until(3, std::time::Duration::from_millis(20), || {
            calls += 1;
            false
        });
        assert!(!ok);
        assert_eq!(calls, 3, "every try is used");
        assert!(
            started.elapsed() >= std::time::Duration::from_millis(40),
            "a pause between tries, none after the last"
        );

        // A folder that goes on the first try is not waited on.
        let dir = tempfile::tempdir().unwrap();
        let version_dir = dir.path().join("1.9.0");
        std::fs::create_dir_all(&version_dir).unwrap();
        std::fs::write(version_dir.join("SQLLY.dmg.part"), b"half").unwrap();
        let started = std::time::Instant::now();
        remove_dir_now(&version_dir);
        assert!(!version_dir.exists());
        assert!(started.elapsed() < REMOVAL_BACKOFF);
    }

    /// A download stage that panics leaves its version folder to the job,
    /// which removes it, awaited, before it parks the failure: by the time
    /// the dialog can see anything, the folder is gone, and the job has
    /// not reported finished.
    #[test]
    fn a_panicking_download_has_its_folder_removed_before_the_job_reports() {
        let dir = tempfile::tempdir().unwrap();
        let version_dir = dir.path().join("1.9.0");
        std::fs::create_dir_all(&version_dir).unwrap();
        std::fs::write(version_dir.join("SQLLY.dmg.part"), b"half").unwrap();
        let progress = std::sync::Arc::new(DownloadProgress::default());
        runtime().block_on(async {
            let guard_progress = progress.clone();
            let folder = version_dir.clone();
            let job = tokio::spawn(run_staging(progress.clone(), None, async move {
                let _folder = RemoveOnDrop::new(folder, guard_progress);
                panic!("stream broke");
            }));
            parked(&progress).await;
            assert!(
                !version_dir.exists(),
                "the folder is removed before the failure is parked"
            );
            assert!(!progress.is_finished(), "nothing is reported finished yet");
            assert!(
                matches!(progress.verdict(), Verdict::Outcome(Err(m)) if m == STOPPED_UNEXPECTEDLY)
            );
            tokio::time::timeout(std::time::Duration::from_secs(5), progress.finished())
                .await
                .expect("the job finishes once its failure was collected");
            job.await.unwrap();
        });
    }

    /// The poll's reads are ordered so a job that delivered and dropped its
    /// guard between two reads is still collected, never mistaken for a
    /// cancel, and a still-running job is left alone.
    #[test]
    fn the_verdict_collects_a_result_however_the_reads_interleave() {
        let progress = DownloadProgress::default();
        assert!(matches!(progress.verdict(), Verdict::Pending));

        // Delivered and finished before the poll looked at either.
        assert!(matches!(
            progress.deliver(Ok(staged("1.9.0"))),
            Delivery::Parked
        ));
        progress.mark_finished();
        assert!(matches!(progress.verdict(), Verdict::Outcome(Ok(p)) if p.version == "1.9.0"));

        let progress = DownloadProgress::default();
        assert!(matches!(
            progress.deliver(Err("nope".into())),
            Delivery::Parked
        ));
        assert!(matches!(progress.verdict(), Verdict::Outcome(Err(m)) if m == "nope"));

        let progress = DownloadProgress::default();
        progress.cancel();
        progress.mark_finished();
        assert!(matches!(progress.verdict(), Verdict::Cancelled));
    }

    /// Serve `body` once over plain HTTP/1.1 with a `Content-Length`, as the
    /// release blob does, and return its URL.
    async fn serve_once(body: Vec<u8>) -> String {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 1024];
            let _ = socket.read(&mut request).await;
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/octet-stream\r\n\r\n",
                body.len()
            );
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.write_all(&body).await;
            let _ = socket.shutdown().await;
        });
        format!("http://{addr}/SQLLY.dmg")
    }

    fn fetch(
        body: Vec<u8>,
        progress: &DownloadProgress,
    ) -> (Result<(), UpdateError>, PathBuf, tempfile::TempDir) {
        use sha2::Digest as _;
        let expected: [u8; 32] = sha2::Sha256::digest(&body).into();
        let dir = tempfile::tempdir().unwrap();
        let partial = dir.path().join("SQLLY.dmg.part");
        let _ = rustls::crypto::ring::default_provider().install_default();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = runtime.block_on(async {
            let url = serve_once(body).await;
            fetch_verified(&reqwest::Client::new(), &url, &partial, &expected, progress).await
        });
        (result, partial, dir)
    }

    #[test]
    fn a_complete_download_is_accepted() {
        // Regression: the length was read after the body was consumed, when
        // reqwest reports the bytes still to come (0), so every download
        // failed as "incomplete".
        let body: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let progress = DownloadProgress::default();
        let (result, partial, _dir) = fetch(body.clone(), &progress);
        assert_eq!(result, Ok(()));
        assert_eq!(std::fs::read(&partial).unwrap(), body);
        assert_eq!(
            progress.snapshot(),
            (body.len() as u64, Some(body.len() as u64))
        );
    }

    #[test]
    fn a_cancelled_download_stops() {
        let progress = DownloadProgress::default();
        progress.cancel();
        let (result, _, _dir) = fetch(vec![7; 50_000], &progress);
        assert_eq!(result, Err(cancelled()));
    }

    #[test]
    fn a_cancel_during_a_network_stall_stops_at_once() {
        // The server sends headers and then nothing: the chunk read would
        // sit until the 60 s read timeout, but a cancel ends it now.
        let progress = std::sync::Arc::new(DownloadProgress::default());
        let dir = tempfile::tempdir().unwrap();
        let partial = dir.path().join("SQLLY.dmg.part");
        let _ = rustls::crypto::ring::default_provider().install_default();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (result, elapsed) = runtime.block_on(async {
            use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0u8; 1024];
                let _ = socket.read(&mut request).await;
                let _ = socket
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 50000\r\n\r\n")
                    .await;
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            });
            let canceller = progress.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                canceller.cancel();
            });
            let started = std::time::Instant::now();
            let url = format!("http://{addr}/SQLLY.dmg");
            let result = fetch_verified(
                &reqwest::Client::new(),
                &url,
                &partial,
                &[0u8; 32],
                &progress,
            )
            .await;
            (result, started.elapsed())
        });
        assert_eq!(result, Err(cancelled()));
        assert!(elapsed < std::time::Duration::from_secs(5), "{elapsed:?}");
    }

    #[test]
    fn a_cancel_before_unpacking_skips_it_and_removes_the_download() {
        let dir = tempfile::tempdir().unwrap();
        let archive_dir = dir.path().join("1.9.0");
        std::fs::create_dir_all(&archive_dir).unwrap();
        let archive = archive_dir.join("SQLLY.dmg");
        std::fs::write(&archive, b"not really a dmg").unwrap();
        let progress = DownloadProgress::default();
        progress.cancel();
        let result = prepare_unless_cancelled("1.9.0", &archive, &progress);
        assert_eq!(result.err().as_deref(), Some(CANCELLED));
        assert!(
            !archive_dir.exists(),
            "a download cancelled before unpacking must not stay on disk"
        );
    }

    /// A download whose final rename fails is removed with its folder,
    /// like every other failure in the job.
    #[tokio::test]
    async fn a_failed_rename_removes_the_download() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("1.9.0");
        std::fs::create_dir_all(&dir).unwrap();
        let partial = dir.join("SQLLY.dmg.part");
        std::fs::write(&partial, b"verified bytes").unwrap();
        // A non-empty folder in the way makes the rename fail.
        let dest = dir.join("SQLLY.dmg");
        std::fs::create_dir_all(dest.join("occupied")).unwrap();

        let progress = std::sync::Arc::new(DownloadProgress::default());
        let err = finish_download(RemoveOnDrop::new(dir.clone(), progress), &partial, &dest)
            .await
            .expect_err("renaming onto a non-empty folder fails");
        assert!(err.starts_with("Couldn't save the download"), "{err}");
        assert!(
            !dir.exists(),
            "the version folder is removed with the download"
        );
    }

    /// A download that unpacks badly is not left on disk for a user who
    /// never tries again.
    #[test]
    fn a_failed_unpack_removes_the_download() {
        let dir = tempfile::tempdir().unwrap();
        let archive_dir = dir.path().join("1.9.0");
        std::fs::create_dir_all(&archive_dir).unwrap();
        let archive = archive_dir.join("SQLLY.dmg");
        std::fs::write(&archive, b"not really a dmg").unwrap();
        let progress = DownloadProgress::default();
        let result = prepare_unless_cancelled("1.9.0", &archive, &progress);
        assert!(result.is_err());
        assert_ne!(result.err().as_deref(), Some(CANCELLED));
        assert!(
            !archive_dir.exists(),
            "a download that failed to unpack must not stay on disk"
        );
    }

    /// A latch wakes every task waiting on it, not just the first: the
    /// chain can have the dialog and a queued job both waiting on one
    /// `finished`.
    /// The dialog announces the wait before the job is scheduled, so the
    /// progress says "queued" from the first frame; the job clears it once
    /// the earlier attempt has finished, never flipping it back to a
    /// download in between.
    #[test]
    fn an_announced_wait_holds_until_the_previous_attempt_finished() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let a = std::sync::Arc::new(DownloadProgress::default());
            let b = std::sync::Arc::new(DownloadProgress::default());
            b.announce_waiting_for_previous();
            assert!(b.is_waiting_for_previous(), "queued before the job runs");
            let job = tokio::spawn(run_staging(b.clone(), Some(a.clone()), async {
                Err::<PreparedUpdate, UpdateError>("stage".into())
            }));
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
            assert!(b.is_waiting_for_previous(), "still queued while A runs");
            a.mark_finished();
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while b.is_waiting_for_previous() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("the wait ends once A finished");
            assert!(b.has_parked_outcome());
            drop(b.take_outcome());
            job.await.unwrap();
        });
    }

    #[test]
    fn a_latch_wakes_every_waiter() {
        let latch = std::sync::Arc::new(Latch::default());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let waiters: Vec<_> = (0..3)
                .map(|_| {
                    let latch = latch.clone();
                    tokio::spawn(async move { latch.wait().await })
                })
                .collect();
            // Let every waiter register before the set lands.
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
            latch.set();
            for waiter in waiters {
                tokio::time::timeout(std::time::Duration::from_secs(5), waiter)
                    .await
                    .expect("every waiter wakes")
                    .unwrap();
            }
            // A waiter that arrives after the set returns at once.
            tokio::time::timeout(std::time::Duration::from_secs(5), latch.wait())
                .await
                .expect("a late waiter sees the set");
        });
    }

    #[test]
    fn the_finish_guard_reports_a_job_that_ended_any_way() {
        let progress = std::sync::Arc::new(DownloadProgress::default());
        assert!(!progress.is_finished());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let guard = FinishGuard(progress.clone());
            let job = tokio::spawn(async move {
                let _guard = guard;
                std::future::pending::<()>().await;
            });
            job.abort();
            tokio::time::timeout(std::time::Duration::from_secs(5), progress.finished())
                .await
                .expect("an aborted job still reports that it finished");
        });
        assert!(progress.is_finished());
    }

    fn update() -> PreparedUpdate {
        PreparedUpdate {
            version: "1.9.0".to_string(),
            moves: vec![
                (
                    PathBuf::from("/Apps/.SQLLY-update.app"),
                    PathBuf::from("/Apps/SQLLY's.app"),
                ),
                (PathBuf::from("/Apps/b.new"), PathBuf::from("/Apps/b")),
            ],
            relaunch: PathBuf::from("/Apps/SQLLY's.app"),
            cleanup: vec![PathBuf::from("/tmp/sqlly-update/1.9.0")],
        }
    }

    #[test]
    fn mac_target_is_the_enclosing_bundle() {
        let exe = Path::new("/Applications/SQLLY.app/Contents/MacOS/SQLLY");
        assert_eq!(
            target_for_exe(exe, "macos"),
            Ok(InstallTarget::MacBundle(PathBuf::from(
                "/Applications/SQLLY.app"
            )))
        );
    }

    #[test]
    fn mac_refuses_dev_builds_dmg_and_translocated_copies() {
        assert!(target_for_exe(Path::new("/repo/target/debug/SQLLY"), "macos").is_err());
        assert!(
            target_for_exe(
                Path::new("/Volumes/SQLLY/SQLLY.app/Contents/MacOS/SQLLY"),
                "macos"
            )
            .is_err()
        );
        let translocated =
            "/private/var/folders/x/AppTranslocation/ABC/d/SQLLY.app/Contents/MacOS/SQLLY";
        let err = target_for_exe(Path::new(translocated), "macos").unwrap_err();
        assert!(err.contains("Applications"), "{err}");
    }

    #[test]
    fn linux_target_is_the_packaged_binary_only() {
        assert_eq!(
            target_for_exe(Path::new("/opt/sqlly/linux/SQLLY"), "linux"),
            Ok(InstallTarget::LinuxExe(PathBuf::from(
                "/opt/sqlly/linux/SQLLY"
            )))
        );
        assert!(target_for_exe(Path::new("/repo/target/debug/sqlly-gpui"), "linux").is_err());
    }

    #[test]
    fn file_names_must_be_plain_platform_artifacts() {
        assert!(validate_file_name("SQLLY-macos-arm64-1.9.0.dmg", "macos").is_ok());
        assert!(validate_file_name("SQLLY-windows-amd64-1.9.0.zip", "windows").is_ok());
        assert!(validate_file_name("SQLLY-linux-amd64-1.9.0.tar.gz", "linux").is_ok());
        assert!(validate_file_name("SQLLY-linux-amd64-1.9.0.tar.gz", "macos").is_err());
        assert!(validate_file_name("../SQLLY-macos-arm64-1.9.0.dmg", "macos").is_err());
        assert!(validate_file_name("SQLLY-x/../../evil.dmg", "macos").is_err());
        assert!(validate_file_name("sqlly-bridge-macos-arm64-1.9.0.dmg", "macos").is_err());
        assert!(validate_file_name("", "linux").is_err());
    }

    #[test]
    fn update_user_agent_names_the_app_and_is_not_a_crawler() {
        let agent = update_user_agent();
        assert!(agent.starts_with(&format!("SQLLY/{} (", crate::about::APP_VERSION)));
        assert!(agent.contains(std::env::consts::OS));
        // Mirrors BotClassifier.AutomatedAgentMarkers on the server: any of
        // these would file the download under crawlers, not platform stats.
        let lower = agent.to_lowercase();
        for marker in [
            "bot",
            "crawl",
            "spider",
            "slurp",
            "scrapy",
            "python-",
            "python/",
            "aiohttp",
            "httpx",
            "go-http-client",
            "java/",
            "okhttp",
            "libwww",
            "apache-httpclient",
            "node-fetch",
            "axios",
            "headlesschrome",
            "phantomjs",
            "facebookexternalhit",
            "ia_archiver",
            "preview",
        ] {
            assert!(!lower.contains(marker), "{agent} contains {marker}");
        }
    }

    #[test]
    fn artifact_url_prefers_the_server_path() {
        let latest = crate::store::LatestVersion {
            version: "1.9.0".into(),
            file_name: "SQLLY-linux-amd64-1.9.0.tar.gz".into(),
            url: "/downloads/1.9.0/SQLLY-linux-amd64-1.9.0.tar.gz".into(),
            sha256: String::new(),
        };
        assert_eq!(
            artifact_url("https://sqlly.app/", &latest),
            "https://sqlly.app/downloads/1.9.0/SQLLY-linux-amd64-1.9.0.tar.gz"
        );
        let bare = crate::store::LatestVersion {
            url: String::new(),
            ..latest
        };
        assert_eq!(
            artifact_url("https://sqlly.app", &bare),
            "https://sqlly.app/downloads/1.9.0/SQLLY-linux-amd64-1.9.0.tar.gz"
        );
    }

    #[test]
    fn checksums_must_be_well_formed_and_match() {
        use sha2::Digest as _;
        let digest = sha2::Sha256::digest(b"test");
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        let expected = parse_sha256(&hex).unwrap();
        assert!(checksum_matches(&digest, &expected));
        assert_eq!(parse_sha256(&hex.to_uppercase()), Some(expected));
        assert!(!checksum_matches(
            &sha2::Sha256::digest(b"tampered"),
            &expected
        ));
        assert_eq!(parse_sha256(""), None);
        assert_eq!(parse_sha256(&hex[..62]), None);
        assert_eq!(parse_sha256(&format!("zz{}", &hex[2..])), None);
        assert_eq!(parse_sha256(&"é".repeat(32)), None);
    }

    #[test]
    fn progress_label_shows_percent_or_size() {
        assert_eq!(progress_label(50, Some(200)), "25%");
        assert_eq!(progress_label(300, Some(200)), "100%");
        assert_eq!(progress_label(1_572_864, None), "1.5 MB");
    }

    #[test]
    fn team_identifier_ignores_unsigned_builds() {
        let display = "Executable=/A\nIdentifier=io.zebrasoft.sqlly\nTeamIdentifier=ABCDE12345\n";
        assert_eq!(team_identifier(display).as_deref(), Some("ABCDE12345"));
        assert_eq!(team_identifier("TeamIdentifier=not set\n"), None);
        assert_eq!(team_identifier("Signature=adhoc\n"), None);
    }

    #[test]
    fn shell_quoting_survives_apostrophes() {
        assert_eq!(sh_quote("a'b"), r"'a'\''b'");
        assert_eq!(ps_quote("a'b"), "'a''b'");
    }

    #[test]
    fn unix_script_swaps_in_order_and_rolls_back_in_reverse() {
        let s = unix_script(
            &update(),
            42,
            Path::new("/tmp/log"),
            Path::new("/tmp/fail"),
            true,
        );
        assert!(s.contains("while kill -0 42"));
        let first = s
            .find("step=1; swap '/Apps/.SQLLY-update.app' '/Apps/SQLLY'\\''s.app' || fail")
            .unwrap();
        let second = s
            .find("step=2; swap '/Apps/b.new' '/Apps/b' || fail")
            .unwrap();
        assert!(first < second);
        let undo2 = s.find("-ge 2 ] && unswap").unwrap();
        let undo1 = s.find("-ge 1 ] && unswap").unwrap();
        assert!(undo2 < undo1, "rollback runs newest step first");
        assert!(s.contains("/usr/bin/open '/Apps/SQLLY'\\''s.app'"));
        assert!(s.contains("rm -rf '/tmp/sqlly-update/1.9.0'"));
        assert!(s.contains("couldn'\\''t install v1.9.0"));
    }

    #[cfg(unix)]
    #[test]
    fn unix_script_is_valid_sh() {
        let s = unix_script(
            &update(),
            42,
            Path::new("/tmp/log"),
            Path::new("/tmp/fail"),
            false,
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("install.sh");
        std::fs::write(&path, s).unwrap();
        let ok = std::process::Command::new("/bin/sh")
            .arg("-n")
            .arg(&path)
            .status()
            .unwrap();
        assert!(ok.success());
    }

    #[cfg(unix)]
    #[test]
    fn unix_script_installs_and_relaunches() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("app"), "old").unwrap();
        std::fs::write(root.join("app.new"), "new").unwrap();
        std::fs::create_dir(root.join("scratch")).unwrap();
        let marker = root.join("launched");
        let launcher = root.join("launch.sh");
        std::fs::write(
            &launcher,
            format!("#!/bin/sh\ntouch {}\n", marker.display()),
        )
        .unwrap();
        let prepared = PreparedUpdate {
            version: "9.9.9".into(),
            moves: vec![(root.join("app.new"), root.join("app"))],
            relaunch: launcher.clone(),
            cleanup: vec![root.join("scratch")],
        };
        // A pid that has already exited.
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        let script = root.join("install.sh");
        std::fs::write(
            &script,
            unix_script(&prepared, pid, &root.join("log"), &root.join("fail"), false),
        )
        .unwrap();
        assert!(
            std::process::Command::new("/bin/sh")
                .arg(&script)
                .status()
                .unwrap()
                .success()
        );
        assert_eq!(std::fs::read_to_string(root.join("app")).unwrap(), "new");
        assert!(!root.join("app.sqlly-old").exists());
        assert!(!root.join("scratch").exists());
        for _ in 0..50 {
            if marker.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(marker.exists(), "relaunched");
    }

    #[cfg(unix)]
    #[test]
    fn unix_script_restores_the_old_copy_when_a_move_fails() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a"), "old-a").unwrap();
        std::fs::write(root.join("a.new"), "new-a").unwrap();
        std::fs::write(root.join("b"), "old-b").unwrap();
        // b.new is missing, so the second move fails.
        let prepared = PreparedUpdate {
            version: "9.9.9".into(),
            moves: vec![
                (root.join("a.new"), root.join("a")),
                (root.join("b.new"), root.join("b")),
            ],
            relaunch: PathBuf::from("/usr/bin/true"),
            cleanup: Vec::new(),
        };
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        let script = root.join("install.sh");
        let fail = root.join("fail");
        std::fs::write(
            &script,
            unix_script(&prepared, pid, &root.join("log"), &fail, false),
        )
        .unwrap();
        let status = std::process::Command::new("/bin/sh")
            .arg(&script)
            .status()
            .unwrap();
        assert!(!status.success());
        assert_eq!(std::fs::read_to_string(root.join("a")).unwrap(), "old-a");
        assert_eq!(std::fs::read_to_string(root.join("b")).unwrap(), "old-b");
        assert!(
            std::fs::read_to_string(&fail)
                .unwrap()
                .contains("kept the version")
        );
    }

    #[test]
    fn windows_script_quotes_paths_and_relaunches() {
        let s = windows_script(
            &update(),
            7,
            Path::new(r"C:\t\log"),
            Path::new(r"C:\t\fail"),
        );
        assert!(s.contains("Get-Process -Id 7"));
        assert!(s.contains("[void]$steps.Add(@('/Apps/.SQLLY-update.app', '/Apps/SQLLY''s.app'))"));
        assert!(s.contains("Start-Process -FilePath '/Apps/SQLLY''s.app'"));
        assert!(s.contains("couldn''t install v1.9.0"));
    }

    /// Builds an ad-hoc-signed app into a real DMG, stages it with
    /// [`prepare_mac`], then runs the helper to swap it over an installed copy.
    /// Mounts a disk image, so it is opt-in: `cargo test -- --ignored self_update`.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "mounts a disk image"]
    fn mac_dmg_stages_and_swaps_into_place() {
        use std::process::Command;

        fn make_app(at: &Path, version: &str, marker: &str) {
            let macos = at.join("Contents/MacOS");
            std::fs::create_dir_all(&macos).unwrap();
            std::fs::write(
                at.join("Contents/Info.plist"),
                format!(
                    r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>SQLLY</string>
<key>CFBundleIdentifier</key><string>io.zebrasoft.sqlly.selfupdate-test</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleShortVersionString</key><string>{version}</string>
</dict></plist>"#
                ),
            )
            .unwrap();
            std::fs::write(macos.join("SQLLY"), format!("#!/bin/sh\necho {marker}\n")).unwrap();
            assert!(
                Command::new("chmod")
                    .arg("+x")
                    .arg(macos.join("SQLLY"))
                    .status()
                    .unwrap()
                    .success()
            );
            assert!(
                Command::new("/usr/bin/codesign")
                    .args(["--force", "--sign", "-"])
                    .arg(at)
                    .status()
                    .unwrap()
                    .success()
            );
        }

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let apps = root.join("Applications");
        let installed = apps.join("SQLLY.app");
        make_app(&installed, "0.0.1", "old");

        let src = root.join("dmg-src");
        make_app(&src.join("SQLLY.app"), "999.0.0", "new");
        let dmg = root.join("SQLLY-macos-arm64-999.0.0.dmg");
        assert!(
            Command::new("/usr/bin/hdiutil")
                .args([
                    "create",
                    "-quiet",
                    "-volname",
                    "SQLLY",
                    "-format",
                    "UDZO",
                    "-srcfolder"
                ])
                .arg(&src)
                .arg(&dmg)
                .status()
                .unwrap()
                .success()
        );

        let scratch = root.join("scratch");
        std::fs::create_dir_all(&scratch).unwrap();
        let mut prepared = prepare_mac(&dmg, &scratch, &installed).unwrap();
        prepared.version = "999.0.0".into();
        prepared.relaunch = PathBuf::from("/usr/bin/true");
        prepared.cleanup.push(scratch.clone());
        assert!(
            apps.join(".SQLLY-update.app/Contents/MacOS/SQLLY")
                .is_file()
        );
        assert!(
            !scratch.join("mount/SQLLY.app").exists(),
            "disk image detached"
        );

        let mut child = Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        let script = root.join("install.sh");
        std::fs::write(
            &script,
            unix_script(&prepared, pid, &root.join("log"), &root.join("fail"), false),
        )
        .unwrap();
        assert!(
            Command::new("/bin/sh")
                .arg(&script)
                .status()
                .unwrap()
                .success()
        );
        let exe = std::fs::read_to_string(installed.join("Contents/MacOS/SQLLY")).unwrap();
        assert!(exe.contains("new"), "{exe}");
        assert!(!apps.join(".SQLLY-update.app").exists());
        assert!(!apps.join("SQLLY.app.sqlly-old").exists());
        assert!(!scratch.exists());
    }

    #[test]
    fn job_errors_built_off_thread_resolve_in_the_ui_language() {
        let _guard = crate::localization::test_language(crate::localization::Language::Spanish);
        let (plain, nested) = std::thread::spawn(|| {
            (
                update_error("updates.errors.checksum_mismatch"),
                update_error_with(
                    "updates.errors.command_failed",
                    &[
                        (OPERATION_ARG, "updates.errors.unpack_update"),
                        ("error", "disk full"),
                    ],
                ),
            )
        })
        .join()
        .unwrap();
        // The worker thread never sees the UI language: fallbacks are English.
        assert_eq!(
            plain.fallback(),
            english("updates.errors.checksum_mismatch")
        );
        assert!(
            nested
                .fallback()
                .contains(english("updates.errors.unpack_update")),
            "{nested}"
        );
        assert!(nested.fallback().ends_with("disk full"), "{nested}");
        // Resolved on this (UI) thread, both come out in Spanish.
        assert_eq!(
            localized_error(&plain),
            crate::localization::text("updates.errors.checksum_mismatch")
        );
        let shown = localized_error(&nested);
        assert!(
            shown.contains(&crate::localization::text("updates.errors.unpack_update")),
            "{shown}"
        );
        assert_ne!(shown, nested.fallback());
        sqlly_i18n::activate(sqlly_i18n::Language::English, None).unwrap();
    }

    #[test]
    fn cancel_and_stop_fallbacks_match_the_english_catalog() {
        assert_eq!(english("updates.errors.cancelled"), CANCELLED);
        assert_eq!(
            english("updates.errors.stopped_unexpectedly"),
            STOPPED_UNEXPECTEDLY
        );
        assert_eq!(cancelled(), CANCELLED);
    }
}
