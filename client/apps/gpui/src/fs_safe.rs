//! Shared file-safety helpers for app-owned stores and temp hand-off files.
//!
//! Every JSON store the app writes (saved filters, import profiles, dump
//! schedules, carts, dashboard pins, ...) goes through [`write_atomic`] so a
//! crash mid-write never leaves a truncated file, and loads through
//! [`load_json_store`] so a file that exists but does not parse is reported —
//! never silently treated as empty and then overwritten by the next save.
//!
//! Files handed to other programs (a spreadsheet app, a terminal, a data
//! dump staged on disk) go through [`private_temp_file`]: created exclusively
//! (no symlink following, no clobbering) and owner-only on Unix, so other
//! local users cannot read query results out of a shared `/tmp`.

use std::io::Write;
use std::path::{Path, PathBuf};

/// Write `bytes` to `path` via a uniquely named, owner-only (0600 on Unix)
/// sibling temp file + fsync + rename, creating the parent directory when
/// missing. On failure the temp file is removed and
/// `path` is left untouched.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    write_atomic_with(path, bytes, Mode::OwnerOnly)
}

/// [`write_atomic`] for a document the user owns (a notebook file): an
/// existing file keeps its permissions, and a new one gets the usual ones
/// rather than the app stores' owner-only mode.
pub fn write_document_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    write_atomic_with(path, bytes, Mode::Document)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    OwnerOnly,
    Document,
}

fn write_atomic_with(path: &Path, bytes: &[u8], mode: Mode) -> std::io::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("store");
    let tmp = path.with_file_name(format!(".{file_name}.{}.tmp", uuid::Uuid::new_v4()));
    let existing = std::fs::metadata(path).ok().map(|m| m.permissions());
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        if mode == Mode::OwnerOnly {
            use std::os::unix::fs::OpenOptionsExt;
            // App stores can hold connection details and query text: owner-only.
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        if mode == Mode::Document
            && let Some(permissions) = existing
        {
            file.set_permissions(permissions)?;
        }
        std::fs::rename(&tmp, path)?;
        sync_parent(path);
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Make the rename itself durable: a crash right after it must not bring
/// the old file back. Best effort; Windows has no directory handles to sync.
fn sync_parent(path: &Path) {
    #[cfg(unix)]
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty())
        && let Ok(dir) = std::fs::File::open(parent)
    {
        let _ = dir.sync_all();
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// The outcome of loading a JSON store from disk.
#[derive(Debug)]
pub enum StoreLoad<T> {
    /// No file yet: start from the default, saving is safe.
    Missing,
    /// Parsed successfully.
    Loaded(T),
    /// The file exists but could not be read or parsed. Callers must surface
    /// `error` and refuse to overwrite the file (the user's data is in it).
    Corrupt { error: sqlly_core::DisplayMessage },
}

/// Read and parse a JSON store, distinguishing "no file" from "unreadable".
pub fn load_json_store<T: serde::de::DeserializeOwned>(path: &Path) -> StoreLoad<T> {
    match std::fs::read(path) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => StoreLoad::Missing,
        Err(err) => StoreLoad::Corrupt {
            error: crate::localization::deferred(
                "shell.local_stores.read_failed",
                &[
                    ("path", path.display().to_string()),
                    ("detail", err.to_string()),
                ],
            ),
        },
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(value) => StoreLoad::Loaded(value),
            Err(err) => StoreLoad::Corrupt {
                error: crate::localization::deferred(
                    "shell.local_stores.invalid_json",
                    &[
                        ("path", path.display().to_string()),
                        ("detail", err.to_string()),
                    ],
                ),
            },
        },
    }
}

/// Create a new, exclusively-opened temp file under the OS temp dir (inside a
/// `sqlly-<purpose>` subdirectory) named `<stem>-<random><suffix>`. On Unix
/// the file is mode 0600 and the subdirectory 0700. The file is kept (not
/// deleted on drop); the caller owns cleanup.
#[cfg(not(target_arch = "wasm32"))]
pub fn private_temp_file(
    purpose: &str,
    stem: &str,
    suffix: &str,
) -> std::io::Result<(std::fs::File, PathBuf)> {
    let dir = private_temp_dir(purpose)?;
    let file = tempfile::Builder::new()
        .prefix(&format!("{stem}-"))
        .suffix(suffix)
        .tempfile_in(&dir)?;
    let (file, path) = file.keep().map_err(|e| e.error)?;
    Ok((file, path))
}

/// `<os temp>/sqlly-<purpose>`, created owner-only (0700 on Unix).
#[cfg(not(target_arch = "wasm32"))]
pub fn private_temp_dir(purpose: &str) -> std::io::Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!("sqlly-{purpose}"));
    std::fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(dir)
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn documents_keep_their_permissions_and_stores_stay_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let doc = dir.path().join("shared.sqllynb");
        std::fs::write(&doc, "old").unwrap();
        std::fs::set_permissions(&doc, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_document_atomic(&doc, b"new").unwrap();
        assert_eq!(std::fs::read_to_string(&doc).unwrap(), "new");
        assert_eq!(
            std::fs::metadata(&doc).unwrap().permissions().mode() & 0o777,
            0o644,
            "saving a notebook keeps the mode the user gave it"
        );
        let store = dir.path().join("store.json");
        std::fs::write(&store, "old").unwrap();
        std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_atomic(&store, b"new").unwrap();
        assert_eq!(
            std::fs::metadata(&store).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn write_atomic_creates_parent_and_replaces_contents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("store.json");
        write_atomic(&path, b"one").unwrap();
        write_atomic(&path, b"two").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn load_json_store_distinguishes_missing_loaded_and_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.json");
        assert!(matches!(
            load_json_store::<Vec<u32>>(&path),
            StoreLoad::Missing
        ));
        std::fs::write(&path, b"[1,2]").unwrap();
        assert!(
            matches!(load_json_store::<Vec<u32>>(&path), StoreLoad::Loaded(v) if v == vec![1, 2])
        );
        std::fs::write(&path, b"{not json").unwrap();
        let StoreLoad::Corrupt { error } = load_json_store::<Vec<u32>>(&path) else {
            panic!("expected a corrupt store");
        };
        assert_eq!(error.key(), Some("shell.local_stores.invalid_json"));
        let shown = crate::localization::message(&error);
        assert!(
            shown.contains("s.json") && shown.contains("left untouched"),
            "{shown}"
        );
    }

    #[test]
    fn private_temp_file_is_exclusive_and_owner_only() {
        let (_file, path) = private_temp_file("fs-safe-test", "probe", ".csv").unwrap();
        assert!(path.exists());
        assert!(path.to_string_lossy().ends_with(".csv"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let _ = std::fs::remove_file(path);
    }
}
