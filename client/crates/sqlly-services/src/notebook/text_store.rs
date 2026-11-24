//! Named text files for notebook stores on a build with no file system.
//!
//! The browser build keeps recovery copies, trust, run history and the
//! notebooks saved in the browser in the page's origin storage
//! (`browser::notebook_files`), under `/`-separated names relative to one
//! notebooks folder: `autosave/<id>.json`, `trust.json`,
//! `runs/<id>.runs.jsonl`, `library/<id>/<file name>`. [`TextStore`] is that
//! storage seen from Rust; [`MemoryTextStore`] stands in for it in tests.
//! [`TextRunStore`] is the run-record store over it, writing the same JSON
//! lines as the native [`JsonlRunStore`](super::run_record::JsonlRunStore).

use std::collections::BTreeMap;
use std::sync::Mutex;

use super::run_record::{RunRecord, RunStore, RunStoreError, parse_runs, run_line, runs_file_name};

/// Longest name a store accepts, in bytes.
pub const MAX_NAME_BYTES: usize = 512;

/// A flat map of names to text that survives a reload.
pub trait TextStore: Send + Sync {
    /// The text stored under `name`, `None` when there is none.
    fn read(&self, name: &str) -> Result<Option<String>, String>;
    /// Replace the text under `name`.
    fn write(&self, name: &str, text: &str) -> Result<(), String>;
    /// Remove `name`; removing a missing name succeeds.
    fn remove(&self, name: &str) -> Result<(), String>;
    /// Every name starting with `prefix`, sorted.
    fn list(&self, prefix: &str) -> Vec<String>;
    /// When `name` was last written, in milliseconds since the Unix epoch.
    fn modified(&self, name: &str) -> Option<i64>;
}

/// Whether `name` is a store name: non-empty `/`-separated segments, none of
/// them `.` or `..`, no backslash or control characters.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_BYTES
        && name.split('/').all(|segment| {
            !segment.is_empty()
                && segment != "."
                && segment != ".."
                && segment.len() <= 255
                && !segment.chars().any(|c| c == '\\' || c.is_control())
        })
}

fn checked(name: &str) -> Result<&str, String> {
    if valid_name(name) {
        Ok(name)
    } else {
        Err(format!("invalid storage name {name:?}"))
    }
}

/// A store held in memory, with a logical clock for `modified`.
#[derive(Debug, Default)]
pub struct MemoryTextStore {
    state: Mutex<MemoryFiles>,
}

#[derive(Debug, Default)]
struct MemoryFiles {
    /// Name → (text, write tick).
    files: BTreeMap<String, (String, i64)>,
    tick: i64,
}

impl MemoryTextStore {
    fn state(&self) -> std::sync::MutexGuard<'_, MemoryFiles> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl TextStore for MemoryTextStore {
    fn read(&self, name: &str) -> Result<Option<String>, String> {
        let name = checked(name)?;
        Ok(self.state().files.get(name).map(|(text, _)| text.clone()))
    }

    fn write(&self, name: &str, text: &str) -> Result<(), String> {
        let name = checked(name)?;
        let mut state = self.state();
        state.tick += 1;
        let at = state.tick;
        state.files.insert(name.to_string(), (text.to_string(), at));
        Ok(())
    }

    fn remove(&self, name: &str) -> Result<(), String> {
        let name = checked(name)?;
        self.state().files.remove(name);
        Ok(())
    }

    fn list(&self, prefix: &str) -> Vec<String> {
        self.state()
            .files
            .keys()
            .filter(|name| name.starts_with(prefix))
            .cloned()
            .collect()
    }

    fn modified(&self, name: &str) -> Option<i64> {
        self.state().files.get(name).map(|(_, at)| *at)
    }
}

impl<S: TextStore + ?Sized> TextStore for &S {
    fn read(&self, name: &str) -> Result<Option<String>, String> {
        (**self).read(name)
    }
    fn write(&self, name: &str, text: &str) -> Result<(), String> {
        (**self).write(name, text)
    }
    fn remove(&self, name: &str) -> Result<(), String> {
        (**self).remove(name)
    }
    fn list(&self, prefix: &str) -> Vec<String> {
        (**self).list(prefix)
    }
    fn modified(&self, name: &str) -> Option<i64> {
        (**self).modified(name)
    }
}

/// Run history a notebook keeps in a [`TextStore`] when no budget is given.
pub const DEFAULT_RUNS_BYTES: usize = 2 << 20;

/// Run records as JSON lines in a [`TextStore`], one `<dir>/<id>.runs.jsonl`
/// name per notebook. Browser storage is small and shared with everything
/// else the page keeps, so a notebook's history is capped at `max_bytes`:
/// the oldest runs are dropped first, never the one being recorded.
#[derive(Debug)]
pub struct TextRunStore<S> {
    store: S,
    dir: String,
    max_bytes: usize,
    lock: Mutex<()>,
}

impl<S: TextStore> TextRunStore<S> {
    pub fn new(store: S, dir: impl Into<String>) -> Self {
        Self {
            store,
            dir: dir.into(),
            max_bytes: DEFAULT_RUNS_BYTES,
            lock: Mutex::new(()),
        }
    }

    pub fn with_max_bytes(mut self, max_bytes: usize) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    /// The store name a notebook's runs live under.
    pub fn name_for(&self, notebook_id: &str) -> String {
        let file = runs_file_name(notebook_id);
        if self.dir.is_empty() {
            file
        } else {
            format!("{}/{file}", self.dir.trim_end_matches('/'))
        }
    }

    fn text(&self, notebook_id: &str) -> Result<String, RunStoreError> {
        self.store
            .read(&self.name_for(notebook_id))
            .map(Option::unwrap_or_default)
            .map_err(|detail| RunStoreError::Io { detail })
    }
}

/// `text` without its oldest lines, until it fits in `max_bytes` or only
/// the last line is left.
fn trim_oldest(text: &str, max_bytes: usize) -> &str {
    let mut rest = text;
    while rest.len() > max_bytes {
        match rest.find('\n') {
            Some(end) if end + 1 < rest.len() => rest = &rest[end + 1..],
            _ => break,
        }
    }
    rest
}

impl<S: TextStore> RunStore for TextRunStore<S> {
    fn append(&self, record: &RunRecord) -> Result<(), RunStoreError> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut text = self.text(&record.notebook_id)?;
        if parse_runs(&text, &record.notebook_id)
            .iter()
            .any(|r| r.run_id == record.run_id)
        {
            return Err(RunStoreError::Duplicate {
                run_id: record.run_id.clone(),
            });
        }
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&run_line(record)?);
        let kept = trim_oldest(&text, self.max_bytes);
        self.store
            .write(&self.name_for(&record.notebook_id), kept)
            .map_err(|detail| RunStoreError::Io { detail })
    }

    fn list(&self, notebook_id: &str) -> Result<Vec<RunRecord>, RunStoreError> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        Ok(parse_runs(&self.text(notebook_id)?, notebook_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notebook::document::Extra;
    use chrono::DateTime;

    fn record(run_id: &str, notebook: &str) -> RunRecord {
        RunRecord {
            run_id: run_id.into(),
            notebook_id: notebook.into(),
            doc_hash: "h".into(),
            started_at: DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            ended_at: None,
            trigger: crate::notebook::run_record::RunTrigger::Manual,
            identity_label: "alice".into(),
            engine_version: "sqlly".into(),
            cells: vec![],
            extra: Extra::new(),
        }
    }

    #[test]
    fn names_are_relative_and_never_climb() {
        for good in [
            "trust.json",
            "autosave/c_abc.json",
            "library/nb_1/Quarterly report.sqllynb",
            "library/x/売上.ipynb",
        ] {
            assert!(valid_name(good), "{good}");
        }
        for bad in [
            "",
            "/abs",
            "a//b",
            "a/../b",
            "..",
            "./x",
            "a\\b",
            "a/\u{0}b",
            "tail/",
            &"x".repeat(MAX_NAME_BYTES + 1),
        ] {
            assert!(!valid_name(bad), "{bad:?}");
        }
        let store = MemoryTextStore::default();
        assert!(store.write("../escape", "x").is_err());
        assert!(store.read("a\\b").is_err());
    }

    #[test]
    fn memory_store_reads_lists_and_dates_writes() {
        let store = MemoryTextStore::default();
        assert_eq!(store.read("a.json").unwrap(), None);
        store.write("autosave/b.json", "B").unwrap();
        store.write("autosave/a.json", "A").unwrap();
        store.write("trust.json", "{}").unwrap();
        assert_eq!(store.read("autosave/a.json").unwrap().as_deref(), Some("A"));
        assert_eq!(
            store.list("autosave/"),
            vec!["autosave/a.json", "autosave/b.json"]
        );
        let first = store.modified("autosave/a.json").unwrap();
        store.write("autosave/a.json", "A2").unwrap();
        assert!(store.modified("autosave/a.json").unwrap() > first);
        store.remove("autosave/a.json").unwrap();
        store.remove("autosave/a.json").unwrap();
        assert_eq!(store.list("autosave/"), vec!["autosave/b.json"]);
        assert_eq!(store.modified("missing"), None);
    }

    #[test]
    fn run_store_writes_the_native_json_lines() {
        let store = MemoryTextStore::default();
        let runs = TextRunStore::new(&store, "runs");
        assert!(runs.list("nb-1").unwrap().is_empty());
        runs.append(&record("r1", "nb-1")).unwrap();
        runs.append(&record("r2", "nb-1")).unwrap();
        assert!(matches!(
            runs.append(&record("r1", "nb-1")),
            Err(RunStoreError::Duplicate { .. })
        ));
        assert_eq!(runs.name_for("nb-1"), "runs/nb-1.runs.jsonl");
        let text = store.read("runs/nb-1.runs.jsonl").unwrap().unwrap();
        assert_eq!(text.lines().count(), 2);
        assert_eq!(
            parse_runs(&text, "nb-1"),
            vec![record("r1", "nb-1"), record("r2", "nb-1")]
        );
        assert_eq!(runs.latest("nb-1").unwrap().unwrap().run_id, "r2");
        let hostile = runs.name_for("../../etc/passwd");
        assert!(hostile.starts_with("runs/") && valid_name(&hostile));
        runs.append(&record("r1", "../../etc/passwd")).unwrap();
        assert_eq!(runs.list("../../etc/passwd").unwrap().len(), 1);
    }

    #[test]
    fn run_store_survives_a_torn_line_and_drops_the_oldest_runs_past_its_budget() {
        let store = MemoryTextStore::default();
        store
            .write("runs/nb.runs.jsonl", "{\"run_id\":\"torn")
            .unwrap();
        let one = run_line(&record("r0", "nb")).unwrap().len();
        let runs = TextRunStore::new(&store, "runs").with_max_bytes(one * 3);
        for i in 1..=5 {
            runs.append(&record(&format!("r{i}"), "nb")).unwrap();
        }
        let kept: Vec<String> = runs
            .list("nb")
            .unwrap()
            .into_iter()
            .map(|r| r.run_id)
            .collect();
        assert_eq!(kept, vec!["r3", "r4", "r5"]);
        let tiny = TextRunStore::new(&store, "tiny").with_max_bytes(1);
        tiny.append(&record("r1", "nb")).unwrap();
        tiny.append(&record("r2", "nb")).unwrap();
        assert_eq!(tiny.list("nb").unwrap(), vec![record("r2", "nb")]);
    }
}
