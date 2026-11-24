//! Where notebook runs live on disk, and the CLI's additions to a run
//! record.
//!
//! Layout under the notebooks data directory
//! (`sqlly_services::notebook::data_dir()`, `<data dir>/SQLLY/notebooks`;
//! `SQLLY_NOTEBOOK_DATA_DIR` overrides it):
//!
//! ```text
//! runs/<scope>/<notebook id>.runs.jsonl         immutable run records (F1 JsonlRunStore)
//! runs/<scope>/outputs/<run id>.json            which cells kept outputs (no rows), pruned by --retain
//! cache/                                        the outputs themselves, encrypted (lane 9's notebook cache)
//! schedules.json                                scheduled jobs
//! schedule-state/<job id>.json                  per-job alert/breaker state
//! local/<scope>/                                local DuckDB workspace (lane D)
//! trust.json, audit/audit.jsonl                 trust grants and audit log (lane 9)
//! ```
//!
//! `<scope>` is the data scope of the notebook's trust record (its id for
//! notebooks trusted before scopes existed), so a file that only borrows
//! another notebook's id finds none of its runs or local data. Names that
//! are not plain `[A-Za-z0-9_-]` are hashed, the same rule the run store
//! uses for its file name.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlly_services::notebook::run_record::RunStore;
use sqlly_services::notebook::{CellOutput, JsonlRunStore, RunRecord};
#[cfg(not(test))]
use sqlly_services::notebook_security::KeyringCacheKey;
use sqlly_services::notebook_security::SnapshotStore;
use sqlly_services::notebook_security::cache::{
    EntryKey, GetOutcome, PutOptions, SnapshotStoreConfig,
};

use crate::agent::CliError;

/// The notebooks data directory (`<data dir>/SQLLY/notebooks`, shared with
/// the app and every notebook store).
pub fn notebooks_root() -> PathBuf {
    if let Ok(dir) = std::env::var("SQLLY_NOTEBOOK_DATA_DIR")
        && !dir.trim().is_empty()
    {
        return PathBuf::from(dir);
    }
    sqlly_services::notebook::data_dir()
}

/// The directory name for a notebook id.
pub fn notebook_stem(notebook_id: &str) -> String {
    let plain = !notebook_id.is_empty()
        && notebook_id.len() <= 128
        && notebook_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if plain {
        notebook_id.to_string()
    } else {
        hex(&Sha256::digest(notebook_id.as_bytes()))
    }
}

/// Where one notebook's runs are: its data scope (the directory) and its id
/// (the records inside).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotebookKey {
    pub scope: String,
    pub notebook_id: String,
}

impl NotebookKey {
    /// `scope` from the notebook's trust; the id itself when it has none.
    pub fn new(scope: Option<&str>, notebook_id: &str) -> Self {
        Self {
            scope: scope.unwrap_or(notebook_id).to_string(),
            notebook_id: notebook_id.to_string(),
        }
    }

    /// The key of the notebook in `file`: its trust record's scope, else
    /// the scope a grant for this file would get (one-run `--trust` runs
    /// keep their records there).
    pub fn of_file(file: &Path, runs: &RunsDir) -> Result<Self, CliError> {
        let doc = super::source::load_file(file)?.doc;
        let scope = super::security::data_scope(&doc, file, &runs.trust_path());
        Ok(Self::new(Some(&scope), &doc.notebook_id))
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Write `bytes` to `path` atomically (temp file + rename), readable only
/// by the current user on Unix.
pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

pub use sqlly_services::notebook::run_details::{
    DETAILS_KEY, OutputSummary, RunDetails, SlotSummary, details_of,
};

/// The index of a run's kept outputs. Rows never go here: they are in the
/// encrypted notebook cache, under the notebook's data scope.
#[derive(Debug, Serialize, Deserialize)]
pub struct OutputsIndex {
    pub run_id: String,
    pub cells: Vec<String>,
}

fn output_entry(run_id: &str, cell_id: &str) -> EntryKey {
    EntryKey::Run {
        run_id: run_id.to_string(),
        cell_id: cell_id.to_string(),
    }
}

/// One notebooks data directory.
#[derive(Debug, Clone)]
pub struct RunsDir {
    pub root: PathBuf,
}

impl RunsDir {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn runs_root(&self) -> PathBuf {
        self.root.join("runs")
    }

    pub fn dir(&self, scope: &str) -> PathBuf {
        self.runs_root().join(notebook_stem(scope))
    }

    pub fn store(&self, scope: &str) -> JsonlRunStore {
        JsonlRunStore::new(self.dir(scope))
    }

    pub fn local_root(&self) -> PathBuf {
        self.root.join("local")
    }

    /// The app's local data preferences, beside this folder: the absolute
    /// attachment files picked in the app, by data scope.
    pub fn local_preferences_path(&self) -> PathBuf {
        self.root
            .parent()
            .unwrap_or(&self.root)
            .join("notebook-local-preferences.json")
    }

    /// The absolute attachment files the app approved for `scope` on this
    /// machine (none when the preferences can't be read).
    pub fn approved_files(&self, scope: &str) -> BTreeSet<String> {
        #[derive(serde::Deserialize, Default)]
        #[serde(default)]
        struct LocalPreferences {
            approved_files: BTreeMap<String, BTreeSet<String>>,
        }
        std::fs::read(self.local_preferences_path())
            .ok()
            .and_then(|bytes| serde_json::from_slice::<LocalPreferences>(&bytes).ok())
            .and_then(|mut prefs| prefs.approved_files.remove(scope))
            .unwrap_or_default()
    }

    pub fn trust_path(&self) -> PathBuf {
        self.root.join("trust.json")
    }

    pub fn audit_dir(&self) -> PathBuf {
        self.root.join("audit")
    }

    fn outputs_dir(&self, scope: &str) -> PathBuf {
        self.dir(scope).join("outputs")
    }

    fn outputs_path(&self, scope: &str, run_id: &str) -> PathBuf {
        self.outputs_dir(scope)
            .join(format!("{}.json", notebook_stem(run_id)))
    }

    pub fn append(&self, scope: &str, record: &RunRecord) -> Result<(), CliError> {
        self.store(scope)
            .append(record)
            .map_err(|e| CliError::io(e.message().fallback().to_string()))?;
        restrict(&self.store(scope).path_for(&record.notebook_id));
        Ok(())
    }

    pub fn list(&self, key: &NotebookKey) -> Result<Vec<RunRecord>, CliError> {
        self.store(&key.scope)
            .list(&key.notebook_id)
            .map_err(|e| CliError::io(e.message().fallback().to_string()))
    }

    /// The encrypted notebook cache (shared with the app), keyed from the
    /// OS credential store.
    fn outputs_cache(&self) -> Result<SnapshotStore, CliError> {
        #[cfg(not(test))]
        let keys = KeyringCacheKey::default();
        #[cfg(test)]
        let keys = sqlly_services::notebook_security::MemoryCacheKey::with_key(
            sqlly_services::notebook_security::cache::MasterKey::from_bytes([3; 32]),
        );
        SnapshotStore::open(SnapshotStoreConfig::new(self.root.join("cache")), &keys)
            .map_err(|e| CliError::io(e.message().fallback().to_string()))
    }

    pub fn write_outputs(
        &self,
        scope: &str,
        run_id: &str,
        outputs: &BTreeMap<String, CellOutput>,
    ) -> Result<(), CliError> {
        // Without a usable key the run record is still kept; only the rows
        // are dropped, as in the app.
        let cache = match self.outputs_cache() {
            Ok(cache) => cache,
            Err(e) => {
                eprintln!("warning: {}", e.body.message);
                return Ok(());
            }
        };
        let local = cache.local_scope(scope);
        let now = chrono::Utc::now().timestamp_millis();
        let mut cells = Vec::new();
        for (cell_id, output) in outputs {
            let options = PutOptions {
                ttl: None,
                pinned: false,
            };
            if let Err(e) = cache.put(&local, &output_entry(run_id, cell_id), output, options, now)
            {
                eprintln!("warning: {}", e.message().fallback());
                continue;
            }
            cells.push(cell_id.clone());
        }
        let index = OutputsIndex {
            run_id: run_id.to_string(),
            cells,
        };
        let bytes = serde_json::to_vec(&index).map_err(|e| CliError::internal(e.to_string()))?;
        write_private(&self.outputs_path(scope, run_id), &bytes)
            .map_err(|e| CliError::io(format!("cannot save run outputs: {e}")))
    }

    pub fn read_outputs(&self, scope: &str, run_id: &str) -> Option<BTreeMap<String, CellOutput>> {
        let text = std::fs::read_to_string(self.outputs_path(scope, run_id)).ok()?;
        let index: OutputsIndex = serde_json::from_str(&text).ok()?;
        if index.run_id != run_id {
            return None;
        }
        let cache = self.outputs_cache().ok()?;
        let local = cache.local_scope(scope);
        let now = chrono::Utc::now().timestamp_millis();
        Some(
            index
                .cells
                .iter()
                .filter_map(
                    |cell| match cache.get(&local, &output_entry(run_id, cell), now) {
                        Ok(GetOutcome::Hit(hit)) => Some((cell.clone(), hit.output)),
                        _ => None,
                    },
                )
                .collect(),
        )
    }

    /// The newest run that still has its outputs, with them.
    pub fn latest_with_outputs(
        &self,
        key: &NotebookKey,
    ) -> Option<(RunRecord, BTreeMap<String, CellOutput>)> {
        let runs = self.list(key).ok()?;
        runs.into_iter().rev().find_map(|run| {
            let outputs = self.read_outputs(&key.scope, &run.run_id)?;
            Some((run, outputs))
        })
    }

    /// Delete output snapshots beyond the newest `retain` runs. Run records
    /// themselves are never rewritten.
    pub fn prune_outputs(&self, key: &NotebookKey, retain: usize) -> Vec<String> {
        let Ok(runs) = self.list(key) else {
            return Vec::new();
        };
        let order: Vec<String> = runs.iter().map(|r| r.run_id.clone()).collect();
        let present: BTreeSet<String> = order
            .iter()
            .filter(|id| self.outputs_path(&key.scope, id).exists())
            .cloned()
            .collect();
        let doomed = prunable(&order, &present, retain);
        let cache = self.outputs_cache().ok();
        for run_id in &doomed {
            let path = self.outputs_path(&key.scope, run_id);
            if let (Some(cache), Some(index)) = (
                &cache,
                std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|t| serde_json::from_str::<OutputsIndex>(&t).ok()),
            ) {
                let local = cache.local_scope(&key.scope);
                for cell in &index.cells {
                    let _ = cache.remove(&local, &output_entry(run_id, cell));
                }
            }
            let _ = std::fs::remove_file(path);
        }
        doomed
    }

    /// Find a run by id (or unique id prefix) across every notebook.
    pub fn find_run(&self, run_id: &str) -> Result<RunRecord, CliError> {
        let mut found: Vec<RunRecord> = Vec::new();
        let entries = std::fs::read_dir(self.runs_root())
            .into_iter()
            .flatten()
            .flatten();
        for entry in entries {
            let dir = entry.path();
            let files = std::fs::read_dir(&dir).into_iter().flatten().flatten();
            for file in files {
                let path = file.path();
                if !path.to_string_lossy().ends_with(".runs.jsonl") {
                    continue;
                }
                let text = std::fs::read_to_string(&path).unwrap_or_default();
                found.extend(
                    text.lines()
                        .filter_map(|line| serde_json::from_str::<RunRecord>(line).ok())
                        .filter(|r| r.run_id.starts_with(run_id)),
                );
            }
        }
        if let Some(exact) = found.iter().find(|r| r.run_id == run_id) {
            return Ok(exact.clone());
        }
        match found.len() {
            0 => Err(CliError::not_found(format!("no recorded run {run_id}"))),
            1 => Ok(found.remove(0)),
            n => Err(CliError::usage(format!(
                "run id prefix {run_id} matches {n} runs; give more of the id"
            ))),
        }
    }

    /// `latest`, `latest~N`, or a run id (prefix) of this notebook.
    pub fn resolve_run(&self, key: &NotebookKey, spec: &str) -> Result<RunRecord, CliError> {
        let runs = self.list(key)?;
        if let Some(back) = spec.strip_prefix("latest") {
            let back: usize = match back.strip_prefix('~') {
                Some(n) => n
                    .parse()
                    .map_err(|_| CliError::usage(format!("invalid run '{spec}'")))?,
                None if back.is_empty() => 0,
                None => return Err(CliError::usage(format!("invalid run '{spec}'"))),
            };
            return runs
                .iter()
                .rev()
                .nth(back)
                .cloned()
                .ok_or_else(|| CliError::not_found(format!("the notebook has no run {spec}")));
        }
        let matches: Vec<&RunRecord> = runs.iter().filter(|r| r.run_id.starts_with(spec)).collect();
        match matches.as_slice() {
            [one] => Ok((*one).clone()),
            [] => Err(CliError::not_found(format!(
                "the notebook has no run {spec}"
            ))),
            _ => runs
                .iter()
                .find(|r| r.run_id == spec)
                .cloned()
                .ok_or_else(|| CliError::usage(format!("run id prefix {spec} is ambiguous"))),
        }
    }
}

fn restrict(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// Runs (oldest first) whose outputs go when only the newest `retain` runs
/// with outputs are kept.
pub fn prunable(order: &[String], with_outputs: &BTreeSet<String>, retain: usize) -> Vec<String> {
    let having: Vec<&String> = order
        .iter()
        .filter(|id| with_outputs.contains(*id))
        .collect();
    let excess = having.len().saturating_sub(retain);
    having.into_iter().take(excess).cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_outputs_are_kept_encrypted_and_pruned() {
        let dir = tempfile::tempdir().unwrap();
        let runs = RunsDir::new(dir.path());
        let output = CellOutput::capped(
            vec![sqlly_core::ResultColumn {
                name: "ssn".into(),
                data_type: "text".into(),
                nullable: false,
            }],
            vec![vec![sqlly_core::CellValue::String("078-05-1120".into())]],
            None,
            10,
            1,
            "r_1",
            sqlly_services::notebook::Fingerprint("f".into()),
            Default::default(),
        );
        let outputs: BTreeMap<String, CellOutput> = [("c_people".to_string(), output)].into();
        runs.write_outputs("p_scope", "r_1", &outputs).unwrap();
        for entry in walk(dir.path()) {
            let bytes = std::fs::read(&entry).unwrap();
            assert!(
                !String::from_utf8_lossy(&bytes).contains("078-05-1120"),
                "{} holds a row in plain text",
                entry.display()
            );
        }
        let back = runs.read_outputs("p_scope", "r_1").unwrap();
        assert_eq!(back["c_people"].rows.len(), 1);
        assert!(
            runs.read_outputs("nb_other", "r_1").is_none(),
            "another notebook's scope reads nothing"
        );
    }

    fn walk(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(walk(&path));
            } else {
                out.push(path);
            }
        }
        out
    }

    #[test]
    fn stems_stay_inside_the_runs_dir() {
        assert_eq!(notebook_stem("nb_1-a"), "nb_1-a");
        let hashed = notebook_stem("../../etc");
        assert_eq!(hashed.len(), 64);
        assert!(!hashed.contains(['/', '\\', '.']));
    }

    #[test]
    fn retention_keeps_the_newest_runs_with_outputs() {
        let order: Vec<String> = ["r1", "r2", "r3", "r4"].map(String::from).to_vec();
        let with: BTreeSet<String> = ["r1", "r2", "r4"].map(String::from).into();
        assert_eq!(prunable(&order, &with, 2), vec!["r1".to_string()]);
        assert_eq!(prunable(&order, &with, 0).len(), 3);
        assert!(prunable(&order, &with, 10).is_empty());
    }

    #[test]
    fn private_writes_replace_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a").join("f.json");
        write_private(&path, b"one").unwrap();
        write_private(&path, b"two").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "two");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }
}
