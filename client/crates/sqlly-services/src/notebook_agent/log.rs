//! The per-notebook AI action log (spec 9.2): who ran what, with which
//! model, prompt-template version and tool inventory, what was approved,
//! and hashes of results. Prompts and data are never written, only their
//! SHA-256, unless the user turns raw prompts on. One JSON line per entry
//! at `notebook::data_dir()/ai/<notebook id>.jsonl` (native, owner-only).
//!
//! Also here: the per-notebook cloud consent store, local to this machine
//! (`ai/consent.json`), so a shared notebook never carries consent with it.

use serde::{Deserialize, Serialize};

use super::context::{ContextPreview, context_hash, hex};
use super::host::HostRecords;
use crate::notebook::cells::ai::{
    AgentMode, AiResult, BudgetUsage, ModelLocationTag, ProposalStatus, RunStatus,
};

/// What happened.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum LogEvent {
    RunStarted {
        mode: AgentMode,
        provider: String,
        model_location: ModelLocationTag,
        template_version: String,
        tools: Vec<String>,
        /// SHA-256 of the first request's text.
        context_hash: String,
        /// The prompt itself, only with [`LogPrivacy::include_prompts`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt: Option<String>,
    },
    ToolCall {
        tool: String,
        arguments_hash: String,
        result_hash: String,
        is_error: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        refused_by: Option<String>,
    },
    Query {
        query: String,
        connection: String,
        sql_hash: String,
        rows: u64,
        elapsed_ms: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result_hash: Option<String>,
        failed: bool,
    },
    RunEnded {
        status: RunStatus,
        usage: BudgetUsage,
        findings: usize,
        unverified: usize,
        proposals: usize,
    },
    /// The user accepted cloud sharing for this notebook.
    ConsentGranted {
        provider: String,
        context_hash: String,
    },
    ProposalDecided {
        proposal: String,
        status: ProposalStatus,
    },
    /// Accepted proposals applied as one undo group.
    Applied {
        proposals: Vec<String>,
        inserted: Vec<String>,
        edited: Vec<String>,
    },
    Undone {
        proposals: Vec<String>,
    },
    PlanApproved,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogEntry {
    pub at: String,
    pub notebook_id: String,
    pub ai_cell: String,
    pub run_id: String,
    #[serde(flatten)]
    pub event: LogEvent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LogPrivacy {
    pub include_prompts: bool,
}

/// The entries one finished run writes, in order.
#[allow(clippy::too_many_arguments)]
pub fn run_entries(
    notebook_id: &str,
    ai_cell: &str,
    at: &str,
    result: &AiResult,
    records: &HostRecords,
    preview: &ContextPreview,
    prompt: &str,
    privacy: LogPrivacy,
) -> Vec<LogEntry> {
    let entry = |event| LogEntry {
        at: at.to_string(),
        notebook_id: notebook_id.to_string(),
        ai_cell: ai_cell.to_string(),
        run_id: result.run_id.clone(),
        event,
    };
    let mut out = vec![entry(LogEvent::RunStarted {
        mode: result.mode,
        provider: result.provider.clone(),
        model_location: result.model_location.unwrap_or(ModelLocationTag::Local),
        template_version: result.template_version.clone(),
        tools: result.tools.clone(),
        context_hash: context_hash(&preview.items),
        prompt: privacy.include_prompts.then(|| prompt.to_string()),
    })];
    for call in &records.log {
        out.push(entry(LogEvent::ToolCall {
            tool: call.name.clone(),
            arguments_hash: call.arguments_hash.clone(),
            result_hash: call.result_hash.clone(),
            is_error: call.is_error,
            refused_by: call.refused_by.map(|k| k.as_str().to_string()),
        }));
    }
    for query in &result.queries {
        out.push(entry(LogEvent::Query {
            query: query.id.clone(),
            connection: query.connection.clone(),
            sql_hash: query.sql_hash.clone(),
            rows: query.rows,
            elapsed_ms: query.elapsed_ms,
            result_hash: query.result_hash.clone(),
            failed: query.error.is_some(),
        }));
    }
    out.push(entry(LogEvent::RunEnded {
        status: result.status,
        usage: result.usage.clone(),
        findings: result.findings.len(),
        unverified: result.findings.iter().filter(|f| !f.verified).count(),
        proposals: result.proposals.len(),
    }));
    out
}

/// A log entry for a user decision outside a run.
pub fn decision(
    notebook_id: &str,
    ai_cell: &str,
    run_id: &str,
    at: &str,
    event: LogEvent,
) -> LogEntry {
    LogEntry {
        at: at.to_string(),
        notebook_id: notebook_id.to_string(),
        ai_cell: ai_cell.to_string(),
        run_id: run_id.to_string(),
        event,
    }
}

/// File stem for a notebook id: the id itself when it is plain, else its hash.
pub fn stem(notebook_id: &str) -> String {
    let plain = !notebook_id.is_empty()
        && notebook_id.len() <= 128
        && notebook_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if plain {
        notebook_id.to_string()
    } else {
        use sha2::Digest;
        hex(&sha2::Sha256::digest(notebook_id.as_bytes()))
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub use store::{ActionLog, ConsentStore};

#[cfg(not(target_arch = "wasm32"))]
mod store {
    use std::collections::BTreeMap;
    use std::io::Write;
    use std::path::{Path, PathBuf};

    use serde::{Deserialize, Serialize};

    use super::{LogEntry, stem};

    /// `notebook::data_dir()/ai`.
    pub fn default_dir() -> PathBuf {
        crate::notebook::data_dir().join("ai")
    }

    fn private_dir(dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }

    fn private_append(path: &Path) -> std::io::Result<std::fs::File> {
        let mut options = std::fs::OpenOptions::new();
        options.append(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(path)
    }

    /// The JSON-lines action log, one file per notebook.
    pub struct ActionLog {
        dir: PathBuf,
    }

    impl ActionLog {
        pub fn new(dir: impl Into<PathBuf>) -> ActionLog {
            ActionLog { dir: dir.into() }
        }

        pub fn default_location() -> ActionLog {
            ActionLog::new(default_dir())
        }

        pub fn path_for(&self, notebook_id: &str) -> PathBuf {
            self.dir.join(format!("{}.jsonl", stem(notebook_id)))
        }

        pub fn append(&self, entries: &[LogEntry]) -> std::io::Result<()> {
            let Some(first) = entries.first() else {
                return Ok(());
            };
            private_dir(&self.dir)?;
            let mut file = private_append(&self.path_for(&first.notebook_id))?;
            let mut text = String::new();
            for entry in entries {
                text.push_str(&serde_json::to_string(entry).map_err(std::io::Error::other)?);
                text.push('\n');
            }
            file.write_all(text.as_bytes())?;
            file.sync_data()
        }

        /// Every entry for a notebook, oldest first; a torn last line is skipped.
        pub fn read(&self, notebook_id: &str) -> std::io::Result<Vec<LogEntry>> {
            match std::fs::read_to_string(self.path_for(notebook_id)) {
                Ok(text) => Ok(text
                    .lines()
                    .filter_map(|l| serde_json::from_str::<LogEntry>(l).ok())
                    .filter(|e| e.notebook_id == notebook_id)
                    .collect()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(vec![]),
                Err(e) => Err(e),
            }
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
    struct ConsentFile {
        version: u32,
        /// notebook id → provider ids the user agreed to share with.
        notebooks: BTreeMap<String, Vec<String>>,
    }

    /// Cloud consent per notebook and provider, on this machine only.
    pub struct ConsentStore {
        path: PathBuf,
    }

    impl ConsentStore {
        pub fn new(path: impl Into<PathBuf>) -> ConsentStore {
            ConsentStore { path: path.into() }
        }

        pub fn default_location() -> ConsentStore {
            ConsentStore::new(default_dir().join("consent.json"))
        }

        fn load(&self) -> ConsentFile {
            std::fs::read_to_string(&self.path)
                .ok()
                .and_then(|t| serde_json::from_str(&t).ok())
                .unwrap_or_default()
        }

        fn save(&self, file: &ConsentFile) -> std::io::Result<()> {
            if let Some(parent) = self.path.parent() {
                private_dir(parent)?;
            }
            let tmp = self.path.with_extension("json.tmp");
            {
                let mut options = std::fs::OpenOptions::new();
                options.write(true).create(true).truncate(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(0o600);
                }
                let mut out = options.open(&tmp)?;
                out.write_all(
                    serde_json::to_string_pretty(file)
                        .map_err(std::io::Error::other)?
                        .as_bytes(),
                )?;
                out.sync_all()?;
            }
            std::fs::rename(tmp, &self.path)
        }

        pub fn granted(&self, notebook_id: &str, provider: &str) -> bool {
            self.load()
                .notebooks
                .get(notebook_id)
                .is_some_and(|p| p.iter().any(|x| x == provider))
        }

        pub fn grant(&self, notebook_id: &str, provider: &str) -> std::io::Result<()> {
            let mut file = self.load();
            file.version = 1;
            let providers = file.notebooks.entry(notebook_id.to_string()).or_default();
            if !providers.iter().any(|x| x == provider) {
                providers.push(provider.to_string());
            }
            self.save(&file)
        }

        pub fn revoke(&self, notebook_id: &str) -> std::io::Result<()> {
            let mut file = self.load();
            if file.notebooks.remove(notebook_id).is_some() {
                self.save(&file)?;
            }
            Ok(())
        }
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use crate::notebook::cells::ai::{Finding, FindingKind, QueryRecord};
    use crate::notebook_agent::context::{PreviewItem, PreviewKind};
    use crate::notebook_agent::host::ToolLog;

    fn preview() -> ContextPreview {
        ContextPreview {
            provider: "ollama:llama3".into(),
            cloud: false,
            items: vec![PreviewItem {
                kind: PreviewKind::Request,
                text: "Why did EU refunds double? secret detail".into(),
            }],
            excluded: vec![],
            tools: vec![],
            may_read_rows: false,
            max_rows: 0,
            max_queries: 0,
        }
    }

    #[test]
    fn the_log_keeps_hashes_not_prompts_or_data() {
        let dir = tempfile::tempdir().unwrap();
        let log = ActionLog::new(dir.path());
        let result = AiResult {
            run_id: "run_ai_1".into(),
            mode: AgentMode::Investigate,
            provider: "ollama:llama3".into(),
            template_version: crate::notebook::cells::ai::TEMPLATE_VERSION.into(),
            findings: vec![Finding::new("f1", FindingKind::Hypothesis, "x")],
            queries: vec![QueryRecord {
                id: "q1".into(),
                connection: "conn_1".into(),
                sql: "SELECT secret FROM t".into(),
                sql_hash: "abc".into(),
                rows: 3,
                ..QueryRecord::default()
            }],
            tools: vec!["list_cells".into()],
            ..AiResult::default()
        };
        let records = HostRecords {
            log: vec![ToolLog {
                name: "run_readonly_query".into(),
                arguments_hash: "h1".into(),
                result_hash: "h2".into(),
                is_error: false,
                refused_by: None,
            }],
            ..HostRecords::default()
        };
        let entries = run_entries(
            "nb/1",
            "c_ai",
            "2026-11-18T10:00:00Z",
            &result,
            &records,
            &preview(),
            "Why did EU refunds double?",
            LogPrivacy::default(),
        );
        log.append(&entries).unwrap();
        log.append(&[decision(
            "nb/1",
            "c_ai",
            "run_ai_1",
            "2026-11-18T10:01:00Z",
            LogEvent::PlanApproved,
        )])
        .unwrap();
        let path = log.path_for("nb/1");
        assert!(
            path.file_name().unwrap().to_str().unwrap().len() > 60,
            "unsafe ids are hashed"
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("secret"));
        assert!(!text.contains("refunds"));
        assert!(text.contains("\"template_version\":\"notebook-agent/1\""));
        let back = log.read("nb/1").unwrap();
        assert_eq!(back.len(), 5);
        assert!(matches!(
            back[0].event,
            LogEvent::RunStarted { prompt: None, .. }
        ));
        assert!(matches!(back[4].event, LogEvent::PlanApproved));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let with_prompt = run_entries(
            "nb",
            "c",
            "t",
            &result,
            &records,
            &preview(),
            "the prompt",
            LogPrivacy {
                include_prompts: true,
            },
        );
        assert!(
            matches!(&with_prompt[0].event, LogEvent::RunStarted { prompt: Some(p), .. } if p == "the prompt")
        );
    }

    #[test]
    fn consent_is_per_notebook_and_provider() {
        let dir = tempfile::tempdir().unwrap();
        let store = ConsentStore::new(dir.path().join("ai").join("consent.json"));
        assert!(!store.granted("nb1", "claude:sonnet"));
        store.grant("nb1", "claude:sonnet").unwrap();
        assert!(store.granted("nb1", "claude:sonnet"));
        assert!(!store.granted("nb1", "openai:gpt"));
        assert!(!store.granted("nb2", "claude:sonnet"));
        store.revoke("nb1").unwrap();
        assert!(!store.granted("nb1", "claude:sonnet"));
    }
}
