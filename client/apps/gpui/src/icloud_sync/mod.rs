//! iCloud sync: keeps saved connections, snippets, SQL Library queries and
//! shareable settings the same on the user's Macs through a folder in their
//! iCloud Drive. Off until the user turns it on (Settings ▸ Workspace ▸
//! iCloud Sync); macOS direct-download edition only (see [`location`]).
//!
//! Secrets never sync: passwords, SSH key passphrases and tokens stay in
//! this Mac's keychain ([`secrets`]), so a connection arriving on a new Mac
//! asks for its password on first connect. Each Mac writes only its own
//! file in iCloud ([`document`]); items merge one by one, newest change
//! winning, deletes kept as tombstones ([`merge`]); a damaged, partial or
//! newer-format file is skipped whole and never overwrites anything here.

pub mod document;
pub mod engine;
pub mod local;
pub mod location;
pub mod merge;
pub mod secrets;

#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

/// The on/off switch, kept in the settings folder and never synced itself.
pub const PREFERENCES_FILE: &str = "icloud-sync.json";

/// iCloud sync settings. `enabled` is off by default.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ICloudSyncPreferences {
    pub enabled: bool,
}

impl ICloudSyncPreferences {
    /// Read from `dir`. A missing or unreadable file means "off".
    pub fn load_from(dir: &Path) -> Self {
        match crate::fs_safe::load_json_store::<Self>(&dir.join(PREFERENCES_FILE)) {
            crate::fs_safe::StoreLoad::Loaded(prefs) => prefs,
            _ => Self::default(),
        }
    }

    pub fn save_to(&self, dir: &Path) -> std::io::Result<()> {
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        crate::fs_safe::write_atomic(&dir.join(PREFERENCES_FILE), &bytes)
    }
}

fn settings_dir() -> PathBuf {
    crate::preferences::preferences_path()
}

static ENABLED: AtomicBool = AtomicBool::new(false);
static ENABLED_LOADED: std::sync::Once = std::sync::Once::new();

/// Whether the user turned sync on (read once, then cached).
pub fn is_enabled() -> bool {
    ENABLED_LOADED.call_once(|| {
        ENABLED.store(
            ICloudSyncPreferences::load_from(&settings_dir()).enabled,
            Ordering::SeqCst,
        );
    });
    ENABLED.load(Ordering::SeqCst)
}

/// Whether this build offers sync at all (macOS, not the App Store edition).
pub fn is_offered() -> bool {
    cfg!(target_os = "macos") && !crate::distribution::is_app_store()
}

/// Turn sync on or off, save the choice, and (when on) sync right away.
pub fn set_enabled(enabled: bool) -> std::io::Result<()> {
    let _ = is_enabled();
    ICloudSyncPreferences { enabled }.save_to(&settings_dir())?;
    ENABLED.store(enabled, Ordering::SeqCst);
    runtime::sync_now();
    Ok(())
}

/// The SQL Library folder sync covers: the default one only. A library the
/// user moved to a folder of their own (often already a git repository or
/// another sync service) is left alone.
fn synced_library_root() -> Option<PathBuf> {
    let prefs = crate::preferences::SqlLibraryPreferences::load();
    let default = crate::sql_library_model::default_library_root();
    match prefs.root.as_deref().map(str::trim) {
        None | Some("") => Some(default),
        Some(root) if Path::new(root) == default => Some(default),
        Some(_) => None,
    }
}

/// What the Settings pane shows about sync.
#[derive(Debug, Clone, Default)]
pub struct SyncStatus {
    /// One line on how sync stands right now.
    pub summary: Option<sqlly_core::DisplayMessage>,
    /// Further notes from the last pass (skipped files, items waiting).
    pub notes: Vec<sqlly_core::DisplayMessage>,
    /// Whether the summary reports a problem.
    pub is_problem: bool,
}

/// The background worker. One per process, whatever the number of windows.
pub mod runtime {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Condvar, Mutex, OnceLock};
    use std::time::Duration;

    use super::SyncStatus;

    /// Time between passes while sync is on.
    pub const INTERVAL: Duration = Duration::from_secs(60);

    struct Shared {
        wake: Mutex<bool>,
        signal: Condvar,
        status: Mutex<SyncStatus>,
    }

    fn shared() -> &'static Shared {
        static SHARED: OnceLock<Shared> = OnceLock::new();
        SHARED.get_or_init(|| Shared {
            wake: Mutex::new(false),
            signal: Condvar::new(),
            status: Mutex::new(SyncStatus::default()),
        })
    }

    /// Bumped whenever a pass changed something on this Mac; windows poll
    /// it to refresh their connection list and snippets.
    static GENERATION: AtomicU64 = AtomicU64::new(0);
    /// What the last pass that changed something brought in.
    static LAST_CHANGES: Mutex<Option<super::local::ApplyReport>> = Mutex::new(None);

    pub fn generation() -> u64 {
        GENERATION.load(Ordering::SeqCst)
    }

    /// The changes behind the latest [`generation`].
    pub fn last_changes() -> Option<super::local::ApplyReport> {
        LAST_CHANGES.lock().ok().and_then(|guard| guard.clone())
    }

    pub fn status() -> SyncStatus {
        shared()
            .status
            .lock()
            .map(|s| s.clone())
            .unwrap_or_default()
    }

    fn set_status(status: SyncStatus) {
        if let Ok(mut guard) = shared().status.lock() {
            *guard = status;
        }
    }

    /// Run a pass now instead of waiting for the next one.
    pub fn sync_now() {
        let shared = shared();
        if let Ok(mut wake) = shared.wake.lock() {
            *wake = true;
            shared.signal.notify_all();
        }
    }

    /// Start the worker (once per process). A no-op where sync is not
    /// offered.
    pub fn start() {
        static STARTED: OnceLock<()> = OnceLock::new();
        if !super::is_offered() {
            return;
        }
        STARTED.get_or_init(|| {
            let _ = std::thread::Builder::new()
                .name("icloud-sync".into())
                .spawn(worker);
        });
    }

    fn worker() {
        let shared = shared();
        loop {
            tick();
            let Ok(mut wake) = shared.wake.lock() else {
                return;
            };
            if !*wake {
                wake = match shared.signal.wait_timeout(wake, INTERVAL) {
                    Ok((guard, _)) => guard,
                    Err(_) => return,
                };
            }
            *wake = false;
        }
    }

    fn tick() {
        use super::location::Availability;
        if !super::is_enabled() {
            set_status(SyncStatus {
                summary: Some(crate::localization::deferred_text("icloud_sync.status.off")),
                ..SyncStatus::default()
            });
            return;
        }
        let devices_dir = match super::location::detect() {
            Availability::Available { devices } => devices,
            other => {
                let key = match other {
                    Availability::NotSignedIn => "icloud_sync.status.not_signed_in",
                    Availability::DriveOff => "icloud_sync.status.drive_off",
                    _ => "icloud_sync.status.unavailable",
                };
                set_status(SyncStatus {
                    summary: Some(crate::localization::deferred_text(key)),
                    notes: Vec::new(),
                    is_problem: true,
                });
                return;
            }
        };
        let settings_dir = super::settings_dir();
        let input = super::engine::PassInput {
            places: super::local::LocalPlaces {
                store: crate::connection::ConnectionStore::global(),
                settings_dir: settings_dir.clone(),
                library_root: super::synced_library_root(),
            },
            devices_dir,
            state_path: settings_dir.join(super::engine::STATE_FILE),
            now: chrono::Utc::now().timestamp_millis(),
        };
        let result = super::engine::run_pass(&input);
        if let Ok(report) = &result {
            for placeholder in &report.pending {
                super::location::request_download(placeholder);
            }
            if report.apply.applied > 0 {
                if let Ok(mut guard) = LAST_CHANGES.lock() {
                    *guard = Some(report.apply.clone());
                }
                GENERATION.fetch_add(1, Ordering::SeqCst);
            }
        }
        set_status(super::status_for(&result, chrono::Local::now()));
    }
}

/// The Settings pane's status for a pass result. Pure apart from building
/// messages, so it is tested directly.
pub fn status_for(
    result: &Result<engine::PassReport, engine::PassError>,
    at: chrono::DateTime<chrono::Local>,
) -> SyncStatus {
    use crate::localization::deferred;
    match result {
        Err(error) => {
            let (key, path, detail) = match error {
                engine::PassError::StateUnreadable { path, detail } => {
                    ("icloud_sync.status.state_unreadable", path, detail)
                }
                engine::PassError::FolderUnreadable { path, detail } => {
                    ("icloud_sync.status.folder_unreadable", path, detail)
                }
                engine::PassError::WriteFailed { path, detail } => {
                    ("icloud_sync.status.write_failed", path, detail)
                }
            };
            SyncStatus {
                summary: Some(deferred(
                    key,
                    &[
                        ("path", path.display().to_string()),
                        ("detail", detail.clone()),
                    ],
                )),
                notes: Vec::new(),
                is_problem: true,
            }
        }
        Ok(report) => {
            let time = at.format("%H:%M").to_string();
            let summary = if report.apply.applied > 0 {
                deferred(
                    "icloud_sync.status.synced_with_changes",
                    &[("time", time), ("count", report.apply.applied.to_string())],
                )
            } else {
                deferred("icloud_sync.status.synced", &[("time", time)])
            };
            let mut notes = Vec::new();
            for skipped in &report.skipped {
                notes.push(match skipped {
                    document::SkippedFile::Damaged { file_name, .. } => deferred(
                        "icloud_sync.status.skipped_damaged",
                        &[("file_name", file_name.clone())],
                    ),
                    document::SkippedFile::Newer { file_name, .. } => deferred(
                        "icloud_sync.status.skipped_newer",
                        &[("file_name", file_name.clone())],
                    ),
                });
            }
            if !report.pending.is_empty() {
                notes.push(deferred(
                    "icloud_sync.status.downloading",
                    &[("count", report.pending.len().to_string())],
                ));
            }
            if !report.apply.failed.is_empty() {
                notes.push(deferred(
                    "icloud_sync.status.not_applied",
                    &[("count", report.apply.failed.len().to_string())],
                ));
            }
            for copy in &report.apply.conflicted_copies {
                notes.push(deferred(
                    "icloud_sync.status.conflicted_copy",
                    &[("file_name", copy.clone())],
                ));
            }
            SyncStatus {
                summary: Some(summary),
                notes,
                is_problem: false,
            }
        }
    }
}
