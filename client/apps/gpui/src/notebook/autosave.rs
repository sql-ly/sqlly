//! Autosave and recovery. Unsaved notebooks are written (debounced, atomic)
//! to `<data dir>/SQLLY/notebooks/autosave/<notebook id>.json` and reopened on
//! the next launch; notebooks saved to a file autosave to that file only when
//! the user turned it on. A file changed on disk since it was read or last
//! saved raises a conflict instead of being overwritten. The browser build
//! keeps the same files in the page's notebooks folder
//! ([`super::browser`]).

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use sqlly_services::notebook::{NotebookDoc, TextStore};

pub const AUTOSAVE_DEBOUNCE: Duration = Duration::from_millis(1500);
pub const CONFLICT_POLL: Duration = Duration::from_secs(3);

#[cfg(test)]
thread_local! {
    static TEST_ROOT: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// Points autosave, trust, audit and the notebook preferences at `root` for
/// this test thread.
#[cfg(test)]
pub fn set_test_root(root: Option<PathBuf>) {
    TEST_ROOT.with(|cell| *cell.borrow_mut() = root);
}

pub(super) fn data_root() -> PathBuf {
    #[cfg(test)]
    if let Some(root) = TEST_ROOT.with(|cell| cell.borrow().clone()) {
        return root;
    }
    // Test builds keep notebook preferences in the per-process folder the
    // test binary set up (see `isolate_notebook_test_data` in lib.rs).
    #[cfg(test)]
    if let Some(dir) = std::env::var_os("SQLLY_NOTEBOOK_DATA_DIR")
        && let Some(root) = PathBuf::from(dir).parent()
    {
        return root.to_path_buf();
    }
    crate::preferences::preferences_path()
}

/// `<data dir>/SQLLY/notebooks`, where every notebook store lives (recovery
/// copies here, trust and audit beside them).
pub fn notebooks_dir() -> PathBuf {
    #[cfg(test)]
    if let Some(root) = TEST_ROOT.with(|cell| cell.borrow().clone()) {
        return root.join("notebooks");
    }
    app_notebooks_dir()
}

#[cfg(not(target_arch = "wasm32"))]
fn app_notebooks_dir() -> PathBuf {
    sqlly_services::notebook::data_dir()
}

#[cfg(target_arch = "wasm32")]
fn app_notebooks_dir() -> PathBuf {
    data_root().join("notebooks")
}

pub fn autosave_dir() -> PathBuf {
    notebooks_dir().join("autosave")
}

/// The recovery file for notebook `id`. Ids come from files the user opened,
/// so anything but a plain token is refused rather than joined into a path.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub fn autosave_path(id: &str) -> Option<PathBuf> {
    plain_id(id).then(|| autosave_dir().join(format!("{id}.json")))
}

fn plain_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// The recovery copy's name in browser storage (`autosave/<id>.json`).
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
fn recovery_name(id: &str) -> Option<String> {
    plain_id(id).then(|| format!("autosave/{id}.json"))
}

#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
fn write_recovery_in(store: &dyn TextStore, nb: &NotebookDoc) -> std::io::Result<PathBuf> {
    let name = recovery_name(&nb.notebook_id)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "notebook id"))?;
    store
        .write(&name, &nb.to_canonical_json())
        .map_err(std::io::Error::other)?;
    Ok(PathBuf::from(format!(
        "{}{name}",
        super::browser::PATH_PREFIX
    )))
}

#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
fn load_recovery_in(store: &dyn TextStore, id: &str) -> Option<NotebookDoc> {
    let name = recovery_name(id)?;
    let text = store.read(&name).ok()??;
    super::files::load_file(&name, &text, None)
        .ok()
        .map(|loaded| loaded.doc)
}

#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
fn remove_recovery_in(store: &dyn TextStore, id: &str) {
    if let Some(name) = recovery_name(id) {
        let _ = store.remove(&name);
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub fn write_recovery(nb: &NotebookDoc) -> std::io::Result<PathBuf> {
    let path = autosave_path(&nb.notebook_id)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "notebook id"))?;
    crate::fs_safe::write_atomic(&path, nb.to_canonical_json().as_bytes())?;
    Ok(path)
}

#[cfg(not(target_arch = "wasm32"))]
pub fn load_recovery(id: &str) -> Option<NotebookDoc> {
    let path = autosave_path(id)?;
    let text = std::fs::read_to_string(&path).ok()?;
    super::files::load_file(&path.to_string_lossy(), &text, None)
        .ok()
        .map(|loaded| loaded.doc)
}

/// Whether a recovery copy of notebook `id` exists.
#[cfg(not(target_arch = "wasm32"))]
pub fn has_recovery(id: &str) -> bool {
    autosave_path(id).is_some_and(|path| path.exists())
}

#[cfg(not(target_arch = "wasm32"))]
pub fn remove_recovery(id: &str) {
    if let Some(path) = autosave_path(id) {
        let _ = std::fs::remove_file(path);
    }
}

/// Browser build: the recovery copy lives in the page's notebooks folder.
#[cfg(target_arch = "wasm32")]
pub fn has_recovery(id: &str) -> bool {
    recovery_name(id).is_some_and(|name| matches!(super::browser::store().read(&name), Ok(Some(_))))
}

#[cfg(target_arch = "wasm32")]
pub fn write_recovery(nb: &NotebookDoc) -> std::io::Result<PathBuf> {
    write_recovery_in(super::browser::store(), nb)
}

#[cfg(target_arch = "wasm32")]
pub fn load_recovery(id: &str) -> Option<NotebookDoc> {
    load_recovery_in(super::browser::store(), id)
}

#[cfg(target_arch = "wasm32")]
pub fn remove_recovery(id: &str) {
    remove_recovery_in(super::browser::store(), id)
}

/// The name, in the recovery folder, of the copy of a file-backed
/// notebook's unsaved changes (written at quit; the file stays as it was).
fn file_recovery_name(id: &str) -> Option<String> {
    plain_id(id).then(|| format!("autosave/{id}.unsaved.json"))
}

/// Write `nb`'s unsaved changes beside the recovery copies, leaving its
/// file untouched.
#[cfg(not(target_arch = "wasm32"))]
pub fn write_file_recovery(nb: &NotebookDoc) -> std::io::Result<()> {
    let name = file_recovery_name(&nb.notebook_id)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "notebook id"))?;
    crate::fs_safe::write_atomic(
        &notebooks_dir().join(name),
        nb.to_canonical_json().as_bytes(),
    )
}

#[cfg(target_arch = "wasm32")]
pub fn write_file_recovery(nb: &NotebookDoc) -> std::io::Result<()> {
    let name = file_recovery_name(&nb.notebook_id)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "notebook id"))?;
    super::browser::store()
        .write(&name, &nb.to_canonical_json())
        .map_err(std::io::Error::other)
}

pub fn remove_file_recovery(id: &str) {
    let Some(name) = file_recovery_name(id) else {
        return;
    };
    #[cfg(not(target_arch = "wasm32"))]
    let _ = std::fs::remove_file(notebooks_dir().join(name));
    #[cfg(target_arch = "wasm32")]
    let _ = super::browser::store().remove(&name);
}

/// The unsaved copy of notebook `id` and when it was written.
fn load_file_recovery(id: &str) -> Option<(NotebookDoc, Option<SystemTime>)> {
    let name = file_recovery_name(id)?;
    #[cfg(not(target_arch = "wasm32"))]
    let (text, modified) = {
        let path = notebooks_dir().join(&name);
        let text = std::fs::read_to_string(&path).ok()?;
        (
            text,
            std::fs::metadata(&path).and_then(|m| m.modified()).ok(),
        )
    };
    #[cfg(target_arch = "wasm32")]
    let (text, modified) = {
        let store = super::browser::store();
        let text = store.read(&name).ok()??;
        let modified = store
            .modified(&name)
            .map(|ms| std::time::UNIX_EPOCH + Duration::from_millis(ms.max(0) as u64));
        (text, modified)
    };
    let doc = super::files::load_file(&name, &text, None).ok()?.doc;
    (doc.notebook_id == id).then_some((doc, modified))
}

/// What is left of a file-backed notebook's unsaved changes from the last
/// session.
#[derive(Debug, Clone, PartialEq)]
pub enum FileRecovery {
    None,
    /// Written after the file last changed: offer it.
    Newer(NotebookDoc),
    /// The file is gone; the unsaved copy is all there is.
    FileMissing(NotebookDoc),
}

/// The unsaved copy of notebook `id` (saved at `path`), if it is worth
/// offering. A copy older than the file (saved or changed since) is
/// deleted.
pub fn file_recovery(id: &str, path: &str) -> FileRecovery {
    let Some((doc, written)) = load_file_recovery(id) else {
        return FileRecovery::None;
    };
    let Some(file) = file_stamp(Path::new(path)) else {
        return FileRecovery::FileMissing(doc);
    };
    match (written, file.modified) {
        (Some(written), Some(changed)) if written < changed => {
            remove_file_recovery(id);
            FileRecovery::None
        }
        _ => FileRecovery::Newer(doc),
    }
}

/// Notebook-wide preferences, kept beside the app's other stores.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NotebookPrefs {
    /// Write a saved notebook back to its own file as it changes.
    pub autosave_files: bool,
    /// Keep a recovery copy of unsaved notebooks.
    pub recovery_autosave: bool,
}

impl Default for NotebookPrefs {
    fn default() -> Self {
        Self {
            autosave_files: false,
            recovery_autosave: true,
        }
    }
}

const PREFS_FILE: &str = "notebook-preferences.json";

#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
fn prefs_path() -> PathBuf {
    data_root().join(PREFS_FILE)
}

impl NotebookPrefs {
    #[cfg(not(target_arch = "wasm32"))]
    pub fn load() -> Self {
        match crate::fs_safe::load_json_store::<Self>(&prefs_path()) {
            crate::fs_safe::StoreLoad::Loaded(prefs) => prefs,
            _ => Self::default(),
        }
    }

    #[cfg(target_arch = "wasm32")]
    pub fn load() -> Self {
        super::browser::store()
            .read(PREFS_FILE)
            .ok()
            .flatten()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn save(&self) {
        if let Ok(json) = serde_json::to_vec_pretty(self) {
            let _ = crate::fs_safe::write_atomic(&prefs_path(), &json);
        }
    }

    #[cfg(target_arch = "wasm32")]
    pub fn save(&self) {
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = super::browser::store().write(PREFS_FILE, &json);
        }
    }
}

/// What a notebook file looked like when this tab last read or wrote it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStamp {
    pub modified: Option<SystemTime>,
    pub len: u64,
}

pub fn file_stamp(path: &Path) -> Option<FileStamp> {
    if let Some(path) = path.to_str().filter(|p| super::browser::is_browser_path(p)) {
        return browser_stamp(super::browser::store(), path);
    }
    let meta = std::fs::metadata(path).ok()?;
    Some(FileStamp {
        modified: meta.modified().ok(),
        len: meta.len(),
    })
}

/// A browser notebook's stamp: when the page (or another tab) last wrote it.
fn browser_stamp(store: &dyn TextStore, path: &str) -> Option<FileStamp> {
    let name = super::browser::store_name(path)?;
    let text = store.read(name).ok()??;
    Some(FileStamp {
        modified: store
            .modified(name)
            .map(|ms| std::time::UNIX_EPOCH + Duration::from_millis(ms.max(0) as u64)),
        len: text.len() as u64,
    })
}

/// Whether the file on disk no longer matches what the tab last saw.
pub fn changed_on_disk(path: &Path, known: Option<FileStamp>) -> bool {
    match (known, file_stamp(path)) {
        (Some(known), Some(now)) => known != now,
        (Some(_), None) => true,
        (None, _) => false,
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    struct Root(#[allow(dead_code)] tempfile::TempDir);
    impl Root {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            set_test_root(Some(dir.path().to_path_buf()));
            Self(dir)
        }
    }
    impl Drop for Root {
        fn drop(&mut self) {
            set_test_root(None);
        }
    }

    #[test]
    fn recovery_round_trips_and_is_removed() {
        let _root = Root::new();
        let mut nb = super::super::files::new_document();
        nb.cells[0].source = "SELECT 42".into();
        let path = write_recovery(&nb).unwrap();
        assert!(path.starts_with(autosave_dir()));
        let back = load_recovery(&nb.notebook_id).unwrap();
        assert_eq!(back, nb);
        remove_recovery(&nb.notebook_id);
        assert!(load_recovery(&nb.notebook_id).is_none());
    }

    #[test]
    fn hostile_ids_never_become_paths() {
        let _root = Root::new();
        for id in [
            "../../etc/passwd",
            "a/b",
            "",
            "x".repeat(65).as_str(),
            "c:\\evil",
        ] {
            assert!(autosave_path(id).is_none(), "{id}");
        }
        let mut nb = super::super::files::new_document();
        nb.notebook_id = "../escape".into();
        assert!(write_recovery(&nb).is_err());
    }

    #[test]
    fn prefs_default_to_recovery_on_and_file_autosave_off() {
        let _root = Root::new();
        let prefs = NotebookPrefs::load();
        assert!(prefs.recovery_autosave);
        assert!(!prefs.autosave_files);
        NotebookPrefs {
            autosave_files: true,
            ..prefs
        }
        .save();
        assert!(NotebookPrefs::load().autosave_files);
    }

    #[test]
    fn browser_recovery_copies_round_trip_in_storage() {
        let store = sqlly_services::notebook::MemoryTextStore::default();
        let mut nb = super::super::files::new_document();
        nb.cells[0].source = "SELECT 7".into();
        let path = write_recovery_in(&store, &nb).unwrap();
        assert_eq!(
            path.to_string_lossy(),
            format!("browser:autosave/{}.json", nb.notebook_id)
        );
        assert_eq!(load_recovery_in(&store, &nb.notebook_id).unwrap(), nb);
        remove_recovery_in(&store, &nb.notebook_id);
        assert!(load_recovery_in(&store, &nb.notebook_id).is_none());
        nb.notebook_id = "../escape".into();
        assert!(write_recovery_in(&store, &nb).is_err());
        assert!(store.list("").is_empty());
    }

    #[test]
    fn a_browser_notebook_written_elsewhere_reads_as_changed() {
        let store = sqlly_services::notebook::MemoryTextStore::default();
        let path = super::super::browser::library_path("nb_1", "n.sqllynb");
        assert!(browser_stamp(&store, &path).is_none());
        super::super::browser::write_in(&store, &path, "{}").unwrap();
        let first = browser_stamp(&store, &path).unwrap();
        assert_eq!(first, browser_stamp(&store, &path).unwrap());
        super::super::browser::write_in(&store, &path, "{}").unwrap();
        assert_ne!(first, browser_stamp(&store, &path).unwrap());
    }

    #[test]
    fn detects_a_file_changed_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("n.sqlnb.json");
        std::fs::write(&path, "{}").unwrap();
        let stamp = file_stamp(&path);
        assert!(!changed_on_disk(&path, stamp));
        std::fs::write(&path, "{\"cells\":[]}").unwrap();
        assert!(changed_on_disk(&path, stamp));
        assert!(!changed_on_disk(&path, None));
    }
}
