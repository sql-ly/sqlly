//! Notebook audit events (spec 12.2 "audit event fields: actor,
//! notebook/version, cell, connection role, action, time, policy decision,
//! row/byte count, duration, query hash; raw SQL optional under
//! configurable privacy policy", NB-10 "jobs are auditable").
//!
//! [`AuditEvent`] never holds credentials: raw SQL is kept only when
//! [`AuditPrivacy::include_raw_sql`] allows it, and then with secret-shaped
//! values masked; errors are masked and shortened the same way. Natively,
//! [`AuditLog`] appends events as JSON lines under
//! `<data dir>/SQLLY/notebooks/audit/` (owner-only) and rotates by size.
//! [`to_embedded_query_entry`] maps a run onto the Embedded bridge's query
//! audit contract (`GET /manage/queries`, `queries.jsonl`) so bridged
//! sessions report notebook runs in the same trail.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlly_core::ConnectionEnvironment;

use super::authorize::{Decision, NotebookAction};
use super::redact::{ExportWarning, mask_secrets};
use crate::report::RedactionSummary;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditAction {
    RunCell,
    Export,
    MaterializeLocally,
    ShareWithAi,
    Schedule,
    Publish,
    TrustGranted,
    TrustRevoked,
    CacheCleared,
    Benchmark,
}

impl AuditAction {
    pub fn for_action(action: &NotebookAction<'_>) -> AuditAction {
        match action {
            NotebookAction::RunCell { .. } => AuditAction::RunCell,
            NotebookAction::Export(_) => AuditAction::Export,
            NotebookAction::MaterializeLocally { .. } => AuditAction::MaterializeLocally,
            NotebookAction::ShareWithAi(_) => AuditAction::ShareWithAi,
            NotebookAction::Schedule { .. } => AuditAction::Schedule,
            NotebookAction::Publish(_) => AuditAction::Publish,
            NotebookAction::Benchmark { .. } => AuditAction::Benchmark,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditDecision {
    Allow,
    Deny,
    NeedsApproval,
    /// The user approved a needs-approval decision.
    Approved,
}

impl From<&Decision> for AuditDecision {
    fn from(decision: &Decision) -> Self {
        match decision {
            Decision::Allow { .. } => AuditDecision::Allow,
            Decision::Deny(_) => AuditDecision::Deny,
            Decision::NeedsApproval(_) => AuditDecision::NeedsApproval,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditOutcome {
    Completed,
    Failed,
    Canceled,
}

/// What the event may keep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuditPrivacy {
    pub include_raw_sql: bool,
    pub max_sql_bytes: usize,
}

impl Default for AuditPrivacy {
    fn default() -> Self {
        AuditPrivacy {
            include_raw_sql: false,
            max_sql_bytes: 64 * 1024,
        }
    }
}

/// What part of the notebook an export took.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportScopeKind {
    Whole,
    Dashboard,
    Selected,
}

/// How an export was made and what redaction took out of it, never any of
/// the exported data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportAudit {
    /// The format's file extension (`html`, `pdf`, `sqllynb`, …).
    pub format: String,
    pub scope: ExportScopeKind,
    /// The dashboard's layout cell or the selected cells, by id.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cells: Vec<String>,
    pub structure_only: bool,
    /// What the report's redaction removed, once a report was built.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redaction: Option<RedactionSummary>,
    /// The message keys of the sensitive findings removed or masked before
    /// rendering, each once.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<String>,
}

impl ExportAudit {
    pub fn new(format: impl Into<String>, scope: ExportScopeKind, structure_only: bool) -> Self {
        ExportAudit {
            format: format.into(),
            scope,
            cells: Vec::new(),
            structure_only,
            redaction: None,
            findings: Vec::new(),
        }
    }

    pub fn with_cells(mut self, cells: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.cells = cells.into_iter().map(Into::into).collect();
        self
    }

    /// Keep the kinds of `warnings` (their keys), never their subjects.
    pub fn with_findings(mut self, warnings: &[ExportWarning]) -> Self {
        let mut keys: Vec<String> = warnings
            .iter()
            .map(|w| w.warning_key().to_string())
            .collect();
        keys.sort_unstable();
        keys.dedup();
        self.findings = keys;
        self
    }

    pub fn with_redaction(mut self, summary: RedactionSummary) -> Self {
        self.redaction = Some(summary);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEvent {
    pub at: DateTime<Utc>,
    /// Who acted, as the app labels them (account email, OS user, runner).
    pub actor: String,
    pub notebook_id: String,
    /// The executable hash the decision was made against.
    pub notebook_version: String,
    pub action: AuditAction,
    pub decision: AuditDecision,
    /// The reason key of a deny / needs-approval decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cell_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// SHA-256 of the exact SQL sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sql: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sql_truncated: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<AuditOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub export: Option<ExportAudit>,
}

pub fn environment_label(environment: ConnectionEnvironment) -> &'static str {
    match environment {
        ConnectionEnvironment::Local => "local",
        ConnectionEnvironment::Dev => "development",
        ConnectionEnvironment::Test => "test",
        ConnectionEnvironment::Staging => "staging",
        ConnectionEnvironment::Production => "production",
        _ => "unknown",
    }
}

/// `text` cut to at most `max` bytes on a character boundary.
fn cut(text: &str, max: usize) -> (&str, bool) {
    if text.len() <= max {
        return (text, false);
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

impl AuditEvent {
    pub fn new(
        at: DateTime<Utc>,
        actor: impl Into<String>,
        notebook_id: impl Into<String>,
        notebook_version: impl Into<String>,
        action: AuditAction,
        decision: &Decision,
    ) -> AuditEvent {
        AuditEvent {
            at,
            actor: actor.into(),
            notebook_id: notebook_id.into(),
            notebook_version: notebook_version.into(),
            action,
            decision: decision.into(),
            reason: decision.reason().map(|r| r.reason_key().to_string()),
            cell_id: None,
            profile_id: None,
            environment: None,
            rows: None,
            bytes: None,
            duration_ms: None,
            query_hash: None,
            sql: None,
            sql_truncated: None,
            outcome: None,
            error: None,
            export: None,
        }
    }

    pub fn with_cell(mut self, cell_id: impl Into<String>) -> Self {
        self.cell_id = Some(cell_id.into());
        self
    }

    pub fn with_connection(
        mut self,
        profile_id: Option<&str>,
        environment: ConnectionEnvironment,
    ) -> Self {
        self.profile_id = profile_id.map(str::to_string);
        self.environment = Some(environment_label(environment).to_string());
        self
    }

    /// Always records the query hash; the text only when `privacy` allows,
    /// with secret-shaped values masked and cut to the size limit.
    pub fn with_sql(mut self, sql: &str, privacy: &AuditPrivacy) -> Self {
        self.query_hash = Some(crate::notebook::run_record::sql_hash(sql));
        if privacy.include_raw_sql {
            let (masked, _) = mask_secrets(sql);
            let (text, truncated) = cut(&masked, privacy.max_sql_bytes);
            self.sql = Some(text.to_string());
            self.sql_truncated = Some(truncated);
        }
        self
    }

    pub fn with_counts(
        mut self,
        rows: Option<u64>,
        bytes: Option<u64>,
        duration_ms: Option<u64>,
    ) -> Self {
        self.rows = rows;
        self.bytes = bytes;
        self.duration_ms = duration_ms;
        self
    }

    /// The outcome; `error` is masked and shortened before it is kept.
    pub fn with_outcome(mut self, outcome: AuditOutcome, error: Option<&str>) -> Self {
        self.outcome = Some(outcome);
        self.error = error.map(|e| sqlly_core::redact_error_summary(&mask_secrets(e).0));
        self
    }

    pub fn with_export(mut self, export: ExportAudit) -> Self {
        self.export = Some(export);
        self
    }

    /// One JSON line (no trailing newline).
    pub fn to_json_line(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

// ── Embedded query audit contract ───────────────────────────────────────

/// SQL longer than this is cut and flagged, as the bridge does.
pub const EMBEDDED_MAX_SQL_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EmbeddedQueryKind {
    Query,
    RowEdit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EmbeddedQueryStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
}

/// The JSON shape of one entry of the bridge's query audit trail
/// (`apps/relay/src/embedded/audit.rs` `QueryEntry`), field for field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EmbeddedQueryEntry {
    pub id: String,
    pub session_id: u64,
    pub kind: EmbeddedQueryKind,
    pub at: DateTime<Utc>,
    pub user: String,
    pub profile: String,
    pub profile_name: String,
    pub connection: String,
    pub server: String,
    pub database: Option<String>,
    pub engine: String,
    pub sql: String,
    pub sql_truncated: bool,
    pub status: EmbeddedQueryStatus,
    pub duration_ms: Option<u64>,
    pub error: Option<String>,
}

/// The bridge session a notebook run went through.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EmbeddedSession {
    pub session_id: u64,
    pub user: String,
    pub profile: String,
    pub profile_name: String,
    pub connection: String,
    pub server: String,
    pub database: Option<String>,
    pub engine: String,
}

/// A run-cell event as a bridge query audit entry; `None` for other
/// actions. Refused runs are recorded as failed with the reason key, and
/// when the event keeps no SQL the entry describes the run by notebook,
/// cell and query hash instead.
pub fn to_embedded_query_entry(
    event: &AuditEvent,
    request_id: &str,
    session: &EmbeddedSession,
) -> Option<EmbeddedQueryEntry> {
    if event.action != AuditAction::RunCell {
        return None;
    }
    let sql = match &event.sql {
        Some(sql) => sql.clone(),
        None => format!(
            "-- notebook {} cell {} query sha256:{}",
            event.notebook_id,
            event.cell_id.as_deref().unwrap_or("-"),
            event.query_hash.as_deref().unwrap_or("-")
        ),
    };
    let (sql_text, cut_here) = cut(&sql, EMBEDDED_MAX_SQL_BYTES);
    let (status, error) = match (event.decision, event.outcome) {
        (AuditDecision::Deny | AuditDecision::NeedsApproval, _) => (
            EmbeddedQueryStatus::Failed,
            Some(format!(
                "refused by notebook policy ({})",
                event.reason.as_deref().unwrap_or("notebook.security")
            )),
        ),
        (_, Some(AuditOutcome::Completed)) => (EmbeddedQueryStatus::Completed, None),
        (_, Some(AuditOutcome::Failed)) => (EmbeddedQueryStatus::Failed, event.error.clone()),
        (_, Some(AuditOutcome::Canceled)) => (EmbeddedQueryStatus::Cancelled, event.error.clone()),
        (_, None) => (EmbeddedQueryStatus::Running, None),
    };
    Some(EmbeddedQueryEntry {
        id: request_id.to_string(),
        session_id: session.session_id,
        kind: EmbeddedQueryKind::Query,
        at: event.at,
        user: session.user.clone(),
        profile: session.profile.clone(),
        profile_name: session.profile_name.clone(),
        connection: session.connection.clone(),
        server: session.server.clone(),
        database: session.database.clone(),
        engine: session.engine.clone(),
        sql: sql_text.to_string(),
        sql_truncated: cut_here || event.sql_truncated == Some(true),
        status,
        duration_ms: event.duration_ms,
        error,
    })
}

// ── Local log ───────────────────────────────────────────────────────────

#[cfg(not(target_arch = "wasm32"))]
pub use log::AuditLog;

#[cfg(not(target_arch = "wasm32"))]
mod log {
    use std::io::{BufRead, Write};
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    use super::AuditEvent;
    use crate::notebook_security::messages::private_fs;

    const FILE: &str = "audit.jsonl";

    /// Append-only JSON-lines audit log with size rotation:
    /// `audit.jsonl`, then `audit.1.jsonl` (newest rotated) … up to
    /// `max_files` files in all.
    pub struct AuditLog {
        dir: PathBuf,
        max_file_bytes: u64,
        max_files: usize,
        lock: Mutex<()>,
    }

    impl AuditLog {
        pub fn new(dir: impl Into<PathBuf>) -> AuditLog {
            AuditLog {
                dir: dir.into(),
                max_file_bytes: 8 * 1024 * 1024,
                max_files: 5,
                lock: Mutex::new(()),
            }
        }

        pub fn with_rotation(mut self, max_file_bytes: u64, max_files: usize) -> AuditLog {
            self.max_file_bytes = max_file_bytes.max(1);
            self.max_files = max_files.max(1);
            self
        }

        /// `<data dir>/SQLLY/notebooks/audit`.
        pub fn default_dir() -> PathBuf {
            crate::notebook::data_dir().join("audit")
        }

        fn file(&self, index: usize) -> PathBuf {
            if index == 0 {
                self.dir.join(FILE)
            } else {
                self.dir.join(format!("audit.{index}.jsonl"))
            }
        }

        fn rotate(&self) -> std::io::Result<()> {
            let last = self.max_files - 1;
            if last == 0 {
                return private_fs::secure_remove(&self.file(0));
            }
            let _ = std::fs::remove_file(self.file(last));
            for index in (0..last).rev() {
                let from = self.file(index);
                if from.exists() {
                    std::fs::rename(&from, self.file(index + 1))?;
                }
            }
            Ok(())
        }

        pub fn append(&self, event: &AuditEvent) -> std::io::Result<()> {
            let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
            private_fs::create_private_dir(&self.dir)?;
            let mut line = event.to_json_line();
            line.push('\n');
            let current = std::fs::metadata(self.file(0))
                .map(|m| m.len())
                .unwrap_or(0);
            if current > 0 && current + line.len() as u64 > self.max_file_bytes {
                self.rotate()?;
            }
            let path = self.file(0);
            let mut options = std::fs::OpenOptions::new();
            options.append(true).create(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&path)?;
            file.write_all(line.as_bytes())?;
            file.flush()
        }

        /// The newest `limit` events, newest first, across rotated files.
        /// Unreadable lines (a torn write) are skipped.
        pub fn recent(&self, limit: usize) -> Vec<AuditEvent> {
            let mut out = Vec::new();
            for index in 0..self.max_files {
                let Ok(file) = std::fs::File::open(self.file(index)) else {
                    continue;
                };
                let mut events: Vec<AuditEvent> = std::io::BufReader::new(file)
                    .lines()
                    .map_while(Result::ok)
                    .filter_map(|l| serde_json::from_str(&l).ok())
                    .collect();
                events.reverse();
                out.extend(events);
                if out.len() >= limit {
                    break;
                }
            }
            out.truncate(limit);
            out
        }

        pub fn dir(&self) -> &Path {
            &self.dir
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::notebook_security::authorize::Reason;

    pub(crate) fn at() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-18T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn run_event(decision: &Decision) -> AuditEvent {
        AuditEvent::new(
            at(),
            "matt@example.com",
            "nb-1",
            "hash-1",
            AuditAction::RunCell,
            decision,
        )
        .with_cell("c_1")
        .with_connection(Some("p-1"), ConnectionEnvironment::Production)
    }

    const SECRET_SQL: &str = "ALTER LOGIN app WITH PASSWORD = 'Rotated-Pass-42'; SELECT 1";

    #[test]
    fn raw_sql_is_kept_only_when_allowed_and_never_with_secrets() {
        let allow = Decision::Allow { notices: vec![] };
        let hidden = run_event(&allow).with_sql(SECRET_SQL, &AuditPrivacy::default());
        assert!(hidden.sql.is_none());
        assert_eq!(hidden.query_hash.as_deref().map(str::len), Some(64));
        let shown = run_event(&allow).with_sql(
            SECRET_SQL,
            &AuditPrivacy {
                include_raw_sql: true,
                max_sql_bytes: 1024,
            },
        );
        let sql = shown.sql.as_deref().unwrap();
        assert!(sql.contains("ALTER LOGIN app"));
        assert!(!sql.contains("Rotated-Pass-42"));
        assert_eq!(shown.query_hash, hidden.query_hash);
        let cut = run_event(&allow).with_sql(
            "SELECT 'é' AS x",
            &AuditPrivacy {
                include_raw_sql: true,
                max_sql_bytes: 9,
            },
        );
        assert_eq!(cut.sql.as_deref(), Some("SELECT '"));
        assert_eq!(cut.sql_truncated, Some(true));
    }

    #[test]
    fn errors_are_masked_and_decisions_carry_reason_keys() {
        let deny = Decision::Deny(Reason::WritesNotAllowed);
        let event = run_event(&deny).with_outcome(
            AuditOutcome::Failed,
            Some("Login failed; connection string Server=x;Password=Leak-77;\nretry"),
        );
        assert_eq!(event.decision, AuditDecision::Deny);
        assert_eq!(
            event.reason.as_deref(),
            Some("notebook.security.reason.writes_not_allowed")
        );
        let error = event.error.as_deref().unwrap();
        assert!(!error.contains("Leak-77"), "{error}");
        assert!(!error.contains('\n'));
        let line = event.to_json_line();
        assert!(!line.contains("Leak-77"));
        assert!(line.contains("\"environment\":\"production\""));
        let back: AuditEvent = serde_json::from_str(&line).unwrap();
        assert_eq!(back, event);
    }

    #[test]
    fn embedded_entries_match_the_bridge_contract() {
        let allow = Decision::Allow { notices: vec![] };
        let event = run_event(&allow)
            .with_sql(SECRET_SQL, &AuditPrivacy::default())
            .with_counts(Some(10), Some(400), Some(25))
            .with_outcome(AuditOutcome::Completed, None);
        let session = EmbeddedSession {
            session_id: 7,
            user: "viewer@example.com".into(),
            profile: "prof-1".into(),
            profile_name: "Sales".into(),
            connection: "warehouse".into(),
            server: "sql.internal".into(),
            database: Some("sales".into()),
            engine: "SQL Server".into(),
        };
        let entry = to_embedded_query_entry(&event, "req-9", &session).unwrap();
        assert_eq!(entry.status, EmbeddedQueryStatus::Completed);
        assert_eq!(entry.duration_ms, Some(25));
        assert!(
            entry
                .sql
                .starts_with("-- notebook nb-1 cell c_1 query sha256:")
        );
        assert!(!entry.sql.contains("Rotated"));
        let value = serde_json::to_value(&entry).unwrap();
        let keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        let mut expected = vec![
            "id",
            "sessionId",
            "kind",
            "at",
            "user",
            "profile",
            "profileName",
            "connection",
            "server",
            "database",
            "engine",
            "sql",
            "sqlTruncated",
            "status",
            "durationMs",
            "error",
        ];
        expected.sort_unstable();
        let mut keys = keys;
        keys.sort_unstable();
        assert_eq!(keys, expected);
        assert_eq!(value["kind"], "query");
        assert_eq!(value["status"], "completed");
        assert_eq!(value["error"], serde_json::Value::Null);

        let refused = run_event(&Decision::NeedsApproval(Reason::TrustRequired));
        let entry = to_embedded_query_entry(&refused, "req-10", &session).unwrap();
        assert_eq!(entry.status, EmbeddedQueryStatus::Failed);
        assert!(
            entry
                .error
                .unwrap()
                .contains("notebook.security.reason.trust_required")
        );
        let canceled = run_event(&allow).with_outcome(AuditOutcome::Canceled, None);
        assert_eq!(
            to_embedded_query_entry(&canceled, "r", &session)
                .unwrap()
                .status,
            EmbeddedQueryStatus::Cancelled
        );
        let export = AuditEvent::new(at(), "a", "nb", "h", AuditAction::Export, &allow);
        assert!(to_embedded_query_entry(&export, "r", &session).is_none());
    }

    #[test]
    fn export_events_keep_how_and_what_was_removed_but_no_data() {
        use crate::notebook_security::redact::WarningKind;
        let allow = Decision::Allow { notices: vec![] };
        let warnings = vec![
            ExportWarning {
                kind: WarningKind::SensitiveColumnRemoved,
                cell_id: Some("c_1".into()),
                subject: Some("ssn_column".into()),
            },
            ExportWarning {
                kind: WarningKind::SensitiveColumnRemoved,
                cell_id: Some("c_2".into()),
                subject: Some("card_number".into()),
            },
        ];
        let summary = RedactionSummary {
            columns_removed: 2,
            rows_removed: 40,
            ..RedactionSummary::default()
        };
        let event = AuditEvent::new(at(), "a", "nb", "h", AuditAction::Export, &allow).with_export(
            ExportAudit::new("pdf", ExportScopeKind::Dashboard, true)
                .with_cells(["cell_dashboard"])
                .with_findings(&warnings)
                .with_redaction(summary.clone()),
        );
        let line = event.to_json_line();
        assert!(
            !line.contains("ssn_column") && !line.contains("card_number"),
            "{line}"
        );
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["export"]["format"], "pdf");
        assert_eq!(value["export"]["scope"], "dashboard");
        assert_eq!(value["export"]["structure_only"], true);
        assert_eq!(
            value["export"]["cells"],
            serde_json::json!(["cell_dashboard"])
        );
        assert_eq!(
            value["export"]["findings"],
            serde_json::json!(["notebook.security.export.sensitive_column_removed"])
        );
        assert_eq!(value["export"]["redaction"]["rows_removed"], 40);
        let back: AuditEvent = serde_json::from_str(&line).unwrap();
        assert_eq!(back, event);
        let plain = AuditEvent::new(at(), "a", "nb", "h", AuditAction::RunCell, &allow);
        assert!(!plain.to_json_line().contains("export"));
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn local_log_appends_rotates_and_reads_back_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let log = AuditLog::new(dir.path().join("audit")).with_rotation(600, 3);
        let allow = Decision::Allow { notices: vec![] };
        for i in 0..40 {
            log.append(&run_event(&allow).with_cell(format!("c_{i}")))
                .unwrap();
        }
        let files: Vec<String> = std::fs::read_dir(log.dir())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(files.len() <= 3, "{files:?}");
        assert!(files.contains(&"audit.jsonl".to_string()));
        let recent = log.recent(5);
        assert_eq!(recent.len(), 5);
        assert_eq!(recent[0].cell_id.as_deref(), Some("c_39"));
        assert_eq!(recent[4].cell_id.as_deref(), Some("c_35"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(log.dir().join("audit.jsonl"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
        std::fs::OpenOptions::new()
            .append(true)
            .open(log.dir().join("audit.jsonl"))
            .and_then(|mut f| std::io::Write::write_all(&mut f, b"{\"torn"))
            .unwrap();
        assert_eq!(log.recent(1)[0].cell_id.as_deref(), Some("c_39"));
    }
}
