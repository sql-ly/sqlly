//! Immutable run records: what ran, as whom, under which fingerprints, with
//! which (redacted) parameters and outcome. A [`RunStore`] appends them and
//! never rewrites one; [`JsonlRunStore`] keeps one JSON-lines file per
//! notebook (native only), [`MemoryRunStore`] serves tests, and
//! [`TextRunStore`](super::text_store::TextRunStore) keeps the same lines in
//! the browser's storage.

use std::collections::BTreeMap;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlly_lang::DisplayMessage;

use super::document::{Extra, IdSource, NotebookDoc, random_token, ser_extra};
use super::fingerprint::Fingerprint;
use super::output::Completeness;
use super::params::{ParameterDef, ResolvedParams};
use super::state::CellStatus;

/// What started a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunTrigger {
    Manual,
    Reactive,
    Scheduled,
    Cli,
}

/// A parameter value as recorded; sensitive and secret values are never
/// written.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RedactedParam {
    pub name: String,
    pub type_tag: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    #[serde(default)]
    pub redacted: bool,
}

/// `names`' values from `resolved`, redacted per their definitions.
pub fn redact_params(
    defs: &[ParameterDef],
    resolved: &ResolvedParams,
    names: &[&str],
) -> Vec<RedactedParam> {
    names
        .iter()
        .filter_map(|name| {
            let def = defs.iter().find(|d| d.name.eq_ignore_ascii_case(name))?;
            let value = resolved.get(&def.name);
            let redacted = def.is_redacted();
            Some(RedactedParam {
                name: def.name.clone(),
                type_tag: value.map_or("missing", |v| v.type_tag()).to_string(),
                value: if redacted {
                    None
                } else {
                    value.map(|v| v.to_json())
                },
                redacted,
            })
        })
        .collect()
}

/// An error as recorded: the message key, fallback text and arguments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunError {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    pub message: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub args: BTreeMap<String, String>,
    /// Arguments that are messages themselves, so a reader translates them
    /// too; `args` holds their English text under the same name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub nested: BTreeMap<String, RunError>,
}

impl From<&DisplayMessage> for RunError {
    fn from(message: &DisplayMessage) -> Self {
        Self {
            key: message.key().map(str::to_string),
            message: message.fallback().to_string(),
            args: message
                .arguments()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            nested: message
                .nested_arguments()
                .map(|(k, inner)| (k.to_string(), RunError::from(inner)))
                .collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CellRunRecord {
    pub cell_id: String,
    pub fingerprint: Fingerprint,
    pub status: CellStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub row_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completeness: Option<Completeness>,
    /// The output was reused rather than produced.
    #[serde(default)]
    pub reused: bool,
    /// SHA-256 of the exact SQL sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sql_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RunError>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub params: Vec<RedactedParam>,
}

/// One notebook run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunRecord {
    pub run_id: String,
    pub notebook_id: String,
    /// [`doc_hash`] of the document as it ran.
    pub doc_hash: String,
    pub started_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<DateTime<Utc>>,
    pub trigger: RunTrigger,
    /// Who the queries ran as, for display (never a credential).
    pub identity_label: String,
    /// SQLLY build and engine versions.
    pub engine_version: String,
    pub cells: Vec<CellRunRecord>,
    #[serde(flatten, serialize_with = "ser_extra")]
    pub extra: Extra,
}

/// SHA-256 (hex) of the document's canonical JSON.
pub fn doc_hash(doc: &NotebookDoc) -> String {
    Sha256::digest(doc.to_canonical_json().as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// SHA-256 (hex) of SQL text, for provenance and run records.
pub fn sql_hash(sql: &str) -> String {
    Sha256::digest(sql.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A new run id: `r_` + 16 base32 characters.
pub fn new_run_id(ids: &mut dyn IdSource) -> String {
    random_token(ids, "r_", 16)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunStoreError {
    Io {
        detail: String,
    },
    /// Records are immutable: a run id can be appended once.
    Duplicate {
        run_id: String,
    },
}

impl RunStoreError {
    pub fn message(&self) -> DisplayMessage {
        match self {
            RunStoreError::Io { detail } => DisplayMessage::new(
                "notebook.core.runs.io",
                format!("Could not read or write the run history: {detail}"),
            )
            .with_arg("detail", detail.clone()),
            RunStoreError::Duplicate { run_id } => DisplayMessage::new(
                "notebook.core.runs.duplicate",
                format!("Run {run_id} is already recorded"),
            )
            .with_arg("run_id", run_id.clone()),
        }
    }
}

/// Append-only storage of run records.
pub trait RunStore: Send + Sync {
    /// Record a run. Refuses a run id that is already stored.
    fn append(&self, record: &RunRecord) -> Result<(), RunStoreError>;

    /// A notebook's runs, oldest first.
    fn list(&self, notebook_id: &str) -> Result<Vec<RunRecord>, RunStoreError>;

    fn get(&self, notebook_id: &str, run_id: &str) -> Result<Option<RunRecord>, RunStoreError> {
        Ok(self
            .list(notebook_id)?
            .into_iter()
            .find(|r| r.run_id == run_id))
    }

    fn latest(&self, notebook_id: &str) -> Result<Option<RunRecord>, RunStoreError> {
        Ok(self.list(notebook_id)?.pop())
    }
}

/// Runs kept in memory.
#[derive(Debug, Default)]
pub struct MemoryRunStore {
    runs: Mutex<BTreeMap<String, Vec<RunRecord>>>,
}

impl RunStore for MemoryRunStore {
    fn append(&self, record: &RunRecord) -> Result<(), RunStoreError> {
        let mut runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
        let list = runs.entry(record.notebook_id.clone()).or_default();
        if list.iter().any(|r| r.run_id == record.run_id) {
            return Err(RunStoreError::Duplicate {
                run_id: record.run_id.clone(),
            });
        }
        list.push(record.clone());
        Ok(())
    }

    fn list(&self, notebook_id: &str) -> Result<Vec<RunRecord>, RunStoreError> {
        let runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
        Ok(runs.get(notebook_id).cloned().unwrap_or_default())
    }
}

/// The JSON-lines file name a notebook's runs live in. Ids that are not
/// plain `[A-Za-z0-9_-]` are hashed so they can never escape a directory.
pub fn runs_file_name(notebook_id: &str) -> String {
    let plain = !notebook_id.is_empty()
        && notebook_id.len() <= 128
        && notebook_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    let stem = if plain {
        notebook_id.to_string()
    } else {
        sql_hash(notebook_id)
    };
    format!("{stem}.runs.jsonl")
}

/// The records of `notebook_id` in a JSON-lines text; a torn line (crash
/// mid-write) is skipped.
pub fn parse_runs(text: &str, notebook_id: &str) -> Vec<RunRecord> {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<RunRecord>(line).ok())
        .filter(|r| r.notebook_id == notebook_id)
        .collect()
}

/// `record` as one JSON line, newline included.
pub fn run_line(record: &RunRecord) -> Result<String, RunStoreError> {
    let mut line = serde_json::to_string(record).map_err(|e| RunStoreError::Io {
        detail: e.to_string(),
    })?;
    line.push('\n');
    Ok(line)
}

/// One `<notebook id>.runs.jsonl` file per notebook under a directory the
/// caller chooses. A torn last line (crash mid-write) is skipped on read.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug)]
pub struct JsonlRunStore {
    dir: std::path::PathBuf,
    lock: Mutex<()>,
}

#[cfg(not(target_arch = "wasm32"))]
impl JsonlRunStore {
    pub fn new(dir: impl Into<std::path::PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            lock: Mutex::new(()),
        }
    }

    /// The file a notebook's runs live in. Ids that are not plain
    /// `[A-Za-z0-9_-]` are hashed so they can never escape the directory.
    pub fn path_for(&self, notebook_id: &str) -> std::path::PathBuf {
        self.dir.join(runs_file_name(notebook_id))
    }

    fn read(&self, notebook_id: &str) -> Result<Vec<RunRecord>, RunStoreError> {
        let path = self.path_for(notebook_id);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(RunStoreError::Io {
                    detail: e.to_string(),
                });
            }
        };
        Ok(parse_runs(&text, notebook_id))
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl RunStore for JsonlRunStore {
    fn append(&self, record: &RunRecord) -> Result<(), RunStoreError> {
        use std::io::Write;
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        if self
            .read(&record.notebook_id)?
            .iter()
            .any(|r| r.run_id == record.run_id)
        {
            return Err(RunStoreError::Duplicate {
                run_id: record.run_id.clone(),
            });
        }
        let io = |e: std::io::Error| RunStoreError::Io {
            detail: e.to_string(),
        };
        std::fs::create_dir_all(&self.dir).map_err(io)?;
        let path = self.path_for(&record.notebook_id);
        // A previous crash may have left a line without its newline.
        let needs_newline = std::fs::read(&path)
            .map(|bytes| bytes.last().is_some_and(|b| *b != b'\n'))
            .unwrap_or(false);
        let mut line = run_line(record)?;
        if needs_newline {
            line.insert(0, '\n');
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(io)?;
        file.write_all(line.as_bytes()).map_err(io)?;
        file.sync_data().map_err(io)
    }

    fn list(&self, notebook_id: &str) -> Result<Vec<RunRecord>, RunStoreError> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        self.read(notebook_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notebook::document::SeededIds;
    use crate::notebook::params::{BindError, ParamError, ParamType, ParamValue};

    fn record(run_id: &str, notebook: &str) -> RunRecord {
        RunRecord {
            run_id: run_id.into(),
            notebook_id: notebook.into(),
            doc_hash: "h".into(),
            started_at: DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            ended_at: None,
            trigger: RunTrigger::Manual,
            identity_label: "alice @ prod".into(),
            engine_version: "sqlly 1.39".into(),
            cells: vec![CellRunRecord {
                cell_id: "c_1".into(),
                fingerprint: Fingerprint("f".into()),
                status: CellStatus::Fresh,
                started_at: None,
                ended_at: None,
                row_count: Some(3),
                completeness: Some(Completeness::Complete),
                reused: false,
                sql_hash: Some(sql_hash("SELECT 1")),
                error: Some(RunError::from(
                    &DisplayMessage::new("notebook.core.x", "x").with_arg("a", "b"),
                )),
                params: vec![],
            }],
            extra: Extra::new(),
        }
    }

    #[test]
    fn a_parameter_error_keeps_its_inner_message_for_translation() {
        let error = BindError::Param {
            name: "region".into(),
            error: ParamError::Required,
        };
        let stored = RunError::from(&error.message());
        assert_eq!(stored.key.as_deref(), Some("notebook.core.bind.param"));
        let inner = &stored.nested["error"];
        assert_eq!(inner.key, error_key(&ParamError::Required));
        assert_eq!(stored.args["error"], inner.message);
        let json = serde_json::to_string(&stored).unwrap();
        let back: RunError = serde_json::from_str(&json).unwrap();
        assert_eq!(back, stored);
        let plain: RunError =
            serde_json::from_str(r#"{"key":"notebook.core.x","message":"x"}"#).unwrap();
        assert!(plain.nested.is_empty());
    }

    fn error_key(error: &ParamError) -> Option<String> {
        error.message().key().map(str::to_string)
    }

    #[test]
    fn sensitive_and_secret_values_are_redacted() {
        let mut token = ParameterDef::new("token", ParamType::SecretRef);
        token.id = "t".into();
        let mut pin = ParameterDef::new("pin", ParamType::String);
        pin.sensitive = true;
        let region = ParameterDef::new("region", ParamType::String);
        let mut resolved = ResolvedParams::default();
        resolved
            .values
            .insert("token".into(), ParamValue::SecretRef("vault.key".into()));
        resolved
            .values
            .insert("pin".into(), ParamValue::Text("1234".into()));
        resolved
            .values
            .insert("region".into(), ParamValue::Text("EMEA".into()));
        let out = redact_params(
            &[token, pin, region],
            &resolved,
            &["token", "PIN", "region", "nope"],
        );
        assert_eq!(out.len(), 3);
        assert!(out[0].redacted && out[0].value.is_none());
        assert!(out[1].redacted && out[1].value.is_none());
        assert_eq!(out[2].value, Some(Value::String("EMEA".into())));
        let json = serde_json::to_string(&out).unwrap();
        assert!(!json.contains("1234") && !json.contains("vault.key"));
    }

    #[test]
    fn hashes_and_ids_are_stable() {
        let doc = NotebookDoc::new("t", &mut SeededIds(1));
        assert_eq!(doc_hash(&doc), doc_hash(&doc.clone()));
        assert_eq!(doc_hash(&doc).len(), 64);
        let id = new_run_id(&mut SeededIds(1));
        assert!(id.starts_with("r_") && id.len() == 18);
        assert_eq!(id, new_run_id(&mut SeededIds(1)));
    }

    #[test]
    fn memory_store_is_append_only() {
        let store = MemoryRunStore::default();
        store.append(&record("r1", "nb")).unwrap();
        store.append(&record("r2", "nb")).unwrap();
        assert_eq!(
            store.append(&record("r1", "nb")),
            Err(RunStoreError::Duplicate {
                run_id: "r1".into()
            })
        );
        assert_eq!(store.list("nb").unwrap().len(), 2);
        assert_eq!(store.latest("nb").unwrap().unwrap().run_id, "r2");
        assert!(store.get("nb", "r1").unwrap().is_some());
        assert!(store.list("other").unwrap().is_empty());
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn jsonl_store_round_trips_and_survives_a_torn_line() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonlRunStore::new(dir.path().join("runs"));
        assert!(store.list("nb-1").unwrap().is_empty());
        store.append(&record("r1", "nb-1")).unwrap();
        let path = store.path_for("nb-1");
        // Simulate a crash mid-write.
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("{\"run_id\":\"r9\",\"note");
        std::fs::write(&path, text).unwrap();
        store.append(&record("r2", "nb-1")).unwrap();
        let runs = store.list("nb-1").unwrap();
        assert_eq!(
            runs.iter().map(|r| r.run_id.as_str()).collect::<Vec<_>>(),
            vec!["r1", "r2"]
        );
        assert_eq!(runs[0], record("r1", "nb-1"));
        assert!(matches!(
            store.append(&record("r2", "nb-1")),
            Err(RunStoreError::Duplicate { .. })
        ));
        let reopened = JsonlRunStore::new(dir.path().join("runs"));
        assert_eq!(reopened.list("nb-1").unwrap().len(), 2);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn hostile_notebook_ids_stay_inside_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonlRunStore::new(dir.path());
        for id in ["../../etc/passwd", "a/b", "C:\\x", "", "nb id"] {
            let path = store.path_for(id);
            assert_eq!(path.parent().unwrap(), dir.path(), "{id}");
            store.append(&record("r1", id)).unwrap();
            assert_eq!(store.list(id).unwrap().len(), 1);
        }
    }
}
