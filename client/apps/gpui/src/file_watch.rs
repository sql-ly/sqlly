//! Watch open `.sql` files for external edits and deletions.
//!
//! `DiskState` / `snapshot` / `classify` compile on every target so the tab
//! strip can name on-disk status without a wasm shim. The `notify` hub is
//! native-only: `sync` watches each file's **parent directory** with
//! `NonRecursive` so platforms that drop per-file delete events still report
//! a removal. `drain` then keeps only paths that are in the watched set.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[cfg(not(target_arch = "wasm32"))]
use std::collections::HashSet;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::mpsc::{self, Receiver, TryRecvError};
#[cfg(not(target_arch = "wasm32"))]
use std::time::Duration;

#[cfg(not(target_arch = "wasm32"))]
use notify::{RecommendedWatcher, RecursiveMode};
#[cfg(not(target_arch = "wasm32"))]
use notify_debouncer_mini::{DebounceEventResult, Debouncer, new_debouncer};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum DiskState {
    #[default]
    Clean,
    ModifiedOnDisk,
    DeletedOnDisk,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WatchSnapshot {
    pub mtime: Option<SystemTime>,
    pub len: u64,
    /// Hash of the file's bytes; set only on a clean baseline ([`baseline`]),
    /// never on the metadata-only [`snapshot`] taken per watch event.
    pub content_hash: Option<u64>,
}

pub fn snapshot(path: &Path) -> WatchSnapshot {
    match std::fs::metadata(path) {
        Ok(meta) => WatchSnapshot {
            mtime: meta.modified().ok(),
            len: meta.len(),
            content_hash: None,
        },
        Err(_) => WatchSnapshot {
            mtime: None,
            len: 0,
            content_hash: None,
        },
    }
}

/// A clean baseline after open, save or reload: metadata plus a hash of the
/// bytes, so a later metadata-only change can be told apart from an edit.
pub fn baseline(path: &Path) -> WatchSnapshot {
    WatchSnapshot {
        content_hash: content_hash(path),
        ..snapshot(path)
    }
}

fn content_hash(path: &Path) -> Option<u64> {
    use std::hash::{Hash, Hasher};
    let bytes = std::fs::read(path).ok()?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    Some(hasher.finish())
}

/// [`classify`] against a [`baseline`], reading the file when only its
/// timestamp moved. Sync clients (Dropbox, OneDrive, iCloud) re-stamp a file
/// right after the app saves it; identical bytes stay Clean, and the returned
/// snapshot is the refreshed baseline to record so the next event is cheap.
pub fn settle(
    path: &Path,
    recorded: &WatchSnapshot,
    current: &WatchSnapshot,
) -> (DiskState, Option<WatchSnapshot>) {
    let state = classify(recorded, current);
    if state == DiskState::ModifiedOnDisk
        && recorded.len == current.len
        && recorded.content_hash.is_some()
        && content_hash(path) == recorded.content_hash
    {
        let refreshed = WatchSnapshot {
            content_hash: recorded.content_hash,
            ..*current
        };
        return (DiskState::Clean, Some(refreshed));
    }
    (state, None)
}

/// Given the recorded snapshot and the current on-disk snapshot, decide state.
/// Missing file → [`DiskState::DeletedOnDisk`]; same mtime and len → Clean;
/// otherwise Modified.
pub fn classify(recorded: &WatchSnapshot, current: &WatchSnapshot) -> DiskState {
    if current.mtime.is_none() {
        return DiskState::DeletedOnDisk;
    }
    if recorded.mtime == current.mtime && recorded.len == current.len {
        DiskState::Clean
    } else {
        DiskState::ModifiedOnDisk
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub struct FileWatchHub {
    debouncer: Debouncer<RecommendedWatcher>,
    rx: Receiver<PathBuf>,
    watched: HashSet<PathBuf>,
    watched_dirs: HashSet<PathBuf>,
}

#[cfg(not(target_arch = "wasm32"))]
impl FileWatchHub {
    pub fn new() -> notify::Result<Self> {
        let (tx, rx) = mpsc::channel();
        let debouncer = new_debouncer(
            Duration::from_millis(300),
            move |res: DebounceEventResult| {
                if let Ok(events) = res {
                    for event in events {
                        let _ = tx.send(event.path);
                    }
                }
            },
        )?;
        Ok(Self {
            debouncer,
            rx,
            watched: HashSet::new(),
            watched_dirs: HashSet::new(),
        })
    }

    pub fn sync(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        let next_files: HashSet<PathBuf> = paths.into_iter().collect();
        let next_dirs: HashSet<PathBuf> = next_files
            .iter()
            .filter_map(|p| p.parent().map(|d| d.to_path_buf()))
            .collect();

        for dir in self.watched_dirs.difference(&next_dirs) {
            let _ = self.debouncer.watcher().unwatch(dir);
        }
        for dir in next_dirs.difference(&self.watched_dirs) {
            let _ = self
                .debouncer
                .watcher()
                .watch(dir, RecursiveMode::NonRecursive);
        }
        self.watched = next_files;
        self.watched_dirs = next_dirs;
    }

    pub fn drain(&mut self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        loop {
            match self.rx.try_recv() {
                Ok(path) => {
                    if self.event_matches_watched(&path) {
                        out.push(canonicalize_or_owned(&path));
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
        out.sort();
        out.dedup();
        out
    }

    fn event_matches_watched(&self, event_path: &Path) -> bool {
        if self.watched.contains(event_path) {
            return true;
        }
        let event_c = canonicalize_or_owned(event_path);
        self.watched.iter().any(|watched| {
            watched == event_path
                || canonicalize_or_owned(watched) == event_c
                || event_path.file_name() == watched.file_name()
                    && event_path.parent().map(canonicalize_or_owned)
                        == watched.parent().map(canonicalize_or_owned)
        })
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn canonicalize_or_owned(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_same_snapshot_is_clean() {
        let snap = WatchSnapshot {
            mtime: Some(SystemTime::UNIX_EPOCH),
            len: 12,
            content_hash: None,
        };
        assert_eq!(classify(&snap, &snap), DiskState::Clean);
    }

    #[test]
    fn classify_missing_is_deleted() {
        let recorded = WatchSnapshot {
            mtime: Some(SystemTime::UNIX_EPOCH),
            len: 4,
            content_hash: None,
        };
        let current = WatchSnapshot {
            mtime: None,
            len: 0,
            content_hash: None,
        };
        assert_eq!(classify(&recorded, &current), DiskState::DeletedOnDisk);
    }

    #[test]
    fn classify_changed_len_is_modified() {
        let recorded = WatchSnapshot {
            mtime: Some(SystemTime::UNIX_EPOCH),
            len: 4,
            content_hash: None,
        };
        let current = WatchSnapshot {
            mtime: Some(SystemTime::UNIX_EPOCH),
            len: 8,
            content_hash: None,
        };
        assert_eq!(classify(&recorded, &current), DiskState::ModifiedOnDisk);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn settle_treats_timestamp_only_change_as_clean() {
        // HeidiSQL #2390: a sync client re-stamps the file just saved; the
        // bytes are ours, so no "changed on disk" prompt.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("q.sql");
        std::fs::write(&path, "SELECT 1;\n").unwrap();
        let recorded = baseline(&path);
        let touched = WatchSnapshot {
            mtime: recorded
                .mtime
                .map(|t| t + std::time::Duration::from_secs(5)),
            ..snapshot(&path)
        };
        let (state, refreshed) = settle(&path, &recorded, &touched);
        assert_eq!(state, DiskState::Clean);
        let refreshed = refreshed.expect("baseline refreshed to the new timestamp");
        assert_eq!(refreshed.mtime, touched.mtime);
        assert_eq!(refreshed.content_hash, recorded.content_hash);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn settle_reports_same_length_edit_as_modified() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("q.sql");
        std::fs::write(&path, "SELECT 1;\n").unwrap();
        let recorded = baseline(&path);
        std::fs::write(&path, "SELECT 2;\n").unwrap();
        let current = WatchSnapshot {
            mtime: recorded
                .mtime
                .map(|t| t + std::time::Duration::from_secs(5)),
            ..snapshot(&path)
        };
        assert_eq!(
            settle(&path, &recorded, &current),
            (DiskState::ModifiedOnDisk, None)
        );
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn hub_reports_external_write() {
        use std::io::Write;
        use std::thread;
        use std::time::Duration;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("watched.sql");
        std::fs::write(&path, b"select 1\n").unwrap();
        let mut hub = FileWatchHub::new().expect("watcher");
        hub.sync([path.clone()]);
        thread::sleep(Duration::from_millis(700));
        {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(&path)
                .unwrap();
            f.write_all(b"select 2\n").unwrap();
            f.sync_all().unwrap();
        }
        thread::sleep(Duration::from_millis(700));
        let drained = hub.drain();
        assert!(
            drained
                .iter()
                .any(|p| p == &path || p.ends_with("watched.sql")),
            "drain={drained:?} path={path:?}"
        );
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn hub_reports_delete() {
        use std::thread;
        use std::time::Duration;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gone.sql");
        std::fs::write(&path, b"select 1\n").unwrap();
        let mut hub = FileWatchHub::new().expect("watcher");
        hub.sync([path.clone()]);
        thread::sleep(Duration::from_millis(700));
        std::fs::remove_file(&path).unwrap();
        thread::sleep(Duration::from_millis(700));
        let drained = hub.drain();
        assert!(
            drained.iter().any(|p| p.file_name() == path.file_name()),
            "drain={drained:?} path={path:?}"
        );
    }
}
