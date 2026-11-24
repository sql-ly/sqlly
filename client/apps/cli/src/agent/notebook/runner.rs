//! `sqlly notebook run`: plan, refuse what policy forbids, execute cells
//! headlessly, evaluate assertions and KPIs, and record the run.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use chrono::Utc;
use serde::Serialize;
use serde_json::{Value, json};
use sqlly_core::{CellValue, ResultColumn};
use sqlly_lang::sqlmodel::DatabaseType;
use sqlly_services::analytics::OutputView;
use sqlly_services::analytics::assertions::{
    AssertionContext, AssertionIssue, AssertionOutcome, AssertionStatus, evaluate_all,
};
use sqlly_services::analytics::kpi::{KpiError, KpiStatus};
use sqlly_services::analytics::profile::{ProfileOptions, profile_output};
use sqlly_services::local_analytics::{
    CancelToken, ImportFormat, ImportSpec, LocalAnalytics, LocalAnalyticsConfig, NotebookWorkspace,
    SnapshotRows, SourceInfo, WriteMode, error_message,
};
use sqlly_services::notebook::cells::attachment::{AttachmentFormat, AttachmentSpec};
use sqlly_services::notebook::expand::substitute_relations;
use sqlly_services::notebook::output::DEFAULT_OUTPUT_ROW_CAP;
use sqlly_services::notebook::params::{SystemClock, bind_sql, resolve_all};
use sqlly_services::notebook::plan::{BlockReason, StepAction};
use sqlly_services::notebook::references::{RefTarget, cell_references};
use sqlly_services::notebook::run_record::{
    CellRunRecord, RunError, RunTrigger, doc_hash, new_run_id, redact_params, sql_hash,
};
use sqlly_services::notebook::state::OutputMeta;
use sqlly_services::notebook::{
    Cell, CellEvent, CellFingerprint, CellKind, CellOutput, CellStatus, Completeness, Dialects,
    ExpandOptions, Expansion, FingerprintInputs, Freshness, Graph, NotebookDoc, NotebookState,
    PlanContext, Provenance, RandomIds, ResolvedParams, RunRecord, RunRequest, expand_sql,
    fingerprint_all, plan,
};

use super::policy::{self, PolicyOptions};
use super::records::{DETAILS_KEY, NotebookKey, OutputSummary, RunDetails, RunsDir, SlotSummary};
use super::remap::{self, LOCAL_SLOT, SlotTarget, Slots};
use super::select::{self, RunModeArg};
use super::specs;
use super::{NotebookFormat, params, security, source};
use crate::agent::connections::StoreFile;
use crate::agent::format::ResultSet;
use crate::agent::{CliError, ExitCode};
use sqlly_services::notebook::cells::assertion::BaselineRef;
use sqlly_services::notebook::cells::kpi;
use sqlly_services::notebook_security::audit::AuditOutcome;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailOn {
    /// Exit non-zero on a cell error or a failed assertion (exit 4).
    #[default]
    Assertion,
    /// Exit non-zero only when a cell could not run.
    Error,
    /// Always exit 0 once the run is recorded.
    Never,
}

#[derive(Debug, Clone, clap::Args)]
pub struct RunArgs {
    /// Notebook file (.sqllynb, .sqlnb.json or .ipynb).
    pub file: PathBuf,
    /// Parameter value, `name=value`. Repeat for more; wins over --param-set.
    #[arg(long = "param", value_name = "NAME=VALUE")]
    pub params: Vec<String>,
    /// JSON file of parameter values: `{"name": value, …}`.
    #[arg(long)]
    pub param_set: Option<PathBuf>,
    /// Map a connection slot to a saved connection: `slot=<name or id>`, or
    /// just the connection for a notebook with one slot. Repeat for more.
    #[arg(long = "connection", value_name = "SLOT=CONNECTION")]
    pub connections: Vec<String>,
    /// Cells to run: `name`, `a,b`, `name+` (and downstream), `..name`
    /// (above it), `name..` (it and below). Default: every cell.
    #[arg(long)]
    pub cells: Option<String>,
    /// `all` runs every selected cell; `stale` reuses outputs of the last
    /// recorded run whose fingerprints still match.
    #[arg(long, value_enum, default_value_t = RunModeArg::All)]
    pub mode: RunModeArg,
    /// Write the run record and summary as JSON to this file.
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// When the exit code is non-zero.
    #[arg(long, value_enum, default_value_t = FailOn::Assertion)]
    pub fail_on: FailOn,
    /// Per-cell timeout in seconds when the cell sets none (0 = no limit).
    #[arg(long, default_value_t = 300)]
    pub timeout: u64,
    /// Rows kept per cell output when the cell sets no limit (default 10000).
    #[arg(long)]
    pub max_rows: Option<usize>,
    #[arg(long, value_enum, default_value_t = NotebookFormat::Table)]
    pub format: NotebookFormat,
    /// Allow cells that change data (never on production connections).
    #[arg(long)]
    pub allow_writes: bool,
    /// Allow cells on connections marked production (read-only cells only).
    #[arg(long)]
    pub allow_production: bool,
    /// Output snapshots kept for this notebook, newest first (0 = keep none).
    #[arg(long, default_value_t = 20)]
    pub retain: usize,
    /// Print the plan and policy checks without running anything.
    #[arg(long)]
    pub dry_run: bool,
    /// Trust the notebook for this run only, when it has not been trusted
    /// on this machine (in the app, or with `schedule add --trust`) or
    /// changed since. Never saved.
    #[arg(long)]
    pub trust: bool,
}

impl RunArgs {
    pub fn new(file: PathBuf) -> Self {
        Self {
            file,
            params: Vec::new(),
            param_set: None,
            connections: Vec::new(),
            cells: None,
            mode: RunModeArg::All,
            out: None,
            fail_on: FailOn::Assertion,
            timeout: 300,
            max_rows: None,
            format: NotebookFormat::Table,
            allow_writes: false,
            allow_production: false,
            retain: 20,
            dry_run: false,
            trust: false,
        }
    }
}

/// Where a run reads connections from and writes its records to.
pub struct RunEnv {
    pub store: StoreFile,
    pub runs: RunsDir,
    pub trigger: RunTrigger,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct AssertionTally {
    pub passed: usize,
    pub warned: usize,
    pub failed: usize,
    pub errors: usize,
    pub inconclusive: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct KpiLine {
    pub cell: String,
    pub status: KpiStatus,
    pub value: Option<f64>,
    pub inconclusive: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct RunSummary {
    pub executed: usize,
    pub reused: usize,
    /// Cells that could not run (errors, timeouts, refusals).
    pub failed: Vec<String>,
    /// Cells blocked by a failed input, a cycle, a broken reference or a
    /// bad parameter.
    pub blocked: Vec<String>,
    pub canceled: Vec<String>,
    /// Cells the command line does not run (charts, AI, Python…).
    pub skipped: Vec<String>,
    /// Assertion cells with a failing check.
    pub assertion_cells_failed: Vec<String>,
    pub assertions: AssertionTally,
    pub kpis: Vec<KpiLine>,
    /// Cells whose output was cut short (row cap, partial).
    pub incomplete: Vec<String>,
    pub interrupted: bool,
}

impl RunSummary {
    pub fn has_errors(&self) -> bool {
        !self.failed.is_empty()
            || !self.blocked.is_empty()
            || !self.canceled.is_empty()
            || self.assertions.errors > 0
    }
}

/// The process exit code for a finished run.
pub fn exit_for(summary: &RunSummary, fail_on: FailOn) -> ExitCode {
    if summary.interrupted {
        return ExitCode::Cancelled;
    }
    match fail_on {
        FailOn::Never => ExitCode::Ok,
        FailOn::Error if summary.has_errors() => ExitCode::Error,
        FailOn::Error => ExitCode::Ok,
        FailOn::Assertion if summary.has_errors() => ExitCode::Error,
        FailOn::Assertion if summary.assertions.failed > 0 => ExitCode::AssertionFailed,
        FailOn::Assertion => ExitCode::Ok,
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PlannedStep {
    pub cell_id: String,
    pub cell: String,
    pub kind: String,
    pub action: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CellLine {
    pub cell_id: String,
    pub cell: String,
    pub kind: String,
    pub status: CellStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rows: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunReport {
    pub notebook_id: String,
    pub title: String,
    pub dry_run: bool,
    pub planned: Vec<PlannedStep>,
    pub cells: Vec<CellLine>,
    pub summary: RunSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record: Option<RunRecord>,
    #[serde(skip)]
    pub details: RunDetails,
}

struct Failure {
    key: &'static str,
    message: String,
    canceled: bool,
}

impl Failure {
    fn new(key: &'static str, message: impl Into<String>) -> Self {
        Self {
            key,
            message: message.into(),
            canceled: false,
        }
    }

    fn canceled() -> Self {
        Self {
            key: "canceled",
            message: "interrupted".into(),
            canceled: true,
        }
    }
}

fn view(output: &CellOutput) -> OutputView<'_> {
    sqlly_services::notebook::cells::output_view(output)
}

fn text_column(name: &str) -> ResultColumn {
    ResultColumn {
        name: name.into(),
        data_type: "text".into(),
        nullable: true,
    }
}

/// `missing_column (id)` style text for an assertion issue.
pub fn issue_text(issue: &AssertionIssue) -> String {
    let value = serde_json::to_value(issue).unwrap_or(Value::Null);
    let kind = value
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("issue")
        .replace('_', " ");
    let details: Vec<String> = value
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(k, _)| k.as_str() != "kind")
        .map(|(_, v)| match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .collect();
    if details.is_empty() {
        kind
    } else {
        format!("{kind} ({})", details.join(", "))
    }
}

fn kpi_error_text(error: &KpiError) -> String {
    match error {
        KpiError::MissingColumn { column } => format!("the input has no column {column}"),
        KpiError::NoColumns => "the input has no columns".into(),
        KpiError::RowOutOfRange { row, rows } => {
            format!("row {row} is past the input's {rows} rows")
        }
        KpiError::NotNumeric { column, row } => {
            format!("{column} in row {row} is not a number")
        }
        KpiError::NoValue => "there are no numeric values to aggregate".into(),
        KpiError::PeriodNeedsAggregate => {
            "a previous-period comparison needs an aggregate value".into()
        }
        KpiError::TimeSeries(e) => format!("{e:?}"),
    }
}

/// An assertion's display name: its `name`, else its kind.
pub fn outcome_label(spec: &sqlly_services::analytics::assertions::AssertionSpec) -> String {
    spec.name.clone().unwrap_or_else(|| {
        serde_json::to_value(&spec.assertion)
            .ok()
            .and_then(|v| v.get("kind").and_then(Value::as_str).map(str::to_string))
            .unwrap_or_else(|| "assertion".into())
    })
}

struct Exec<'a> {
    doc: &'a NotebookDoc,
    dialects: &'a Dialects,
    slots: &'a Slots,
    resolved: &'a ResolvedParams,
    fingerprints: &'a BTreeMap<String, CellFingerprint>,
    redacted: Vec<String>,
    run_id: String,
    default_timeout: u64,
    default_cap: usize,
    outputs: BTreeMap<String, CellOutput>,
    previous: BTreeMap<String, CellOutput>,
    materialized: BTreeSet<String>,
    services: Option<sqlly_services::Services>,
    analytics: LocalAnalytics,
    /// The notebook's data scope: its local workspace's name.
    scope: String,
    workspace: Option<NotebookWorkspace>,
    interrupted: Arc<AtomicBool>,
    assertions: BTreeMap<String, Vec<AssertionOutcome>>,
    kpis: BTreeMap<String, Value>,
    /// The SQL each cell sent, sensitive values left unbound, for the
    /// audit log's hash.
    sql_sent: BTreeMap<String, String>,
    /// The notebook file's folder, which attachment paths are relative to.
    notebook_dir: Option<PathBuf>,
    /// Absolute attachment files picked in the app on this machine for this
    /// notebook; never read by scheduled runs.
    approved_files: BTreeSet<String>,
    scheduled: bool,
}

impl<'a> Exec<'a> {
    fn label(&self, id: &str) -> String {
        self.doc
            .cell(id)
            .map_or_else(|| id.to_string(), |c| c.relation_name().to_string())
    }

    fn cap_for(&self, cell: &Cell) -> usize {
        cell.execution_policy
            .max_rows
            .and_then(|n| usize::try_from(n).ok())
            .unwrap_or(self.default_cap)
            .max(1)
    }

    fn timeout_for(&self, cell: &Cell) -> u64 {
        cell.execution_policy
            .timeout_seconds
            .map_or(self.default_timeout, u64::from)
    }

    fn input_cell(&self, token: &str) -> Result<&'a Cell, Failure> {
        self.doc
            .cell(token)
            .or_else(|| self.doc.resolve(token))
            .ok_or_else(|| Failure::new("unknown_input", format!("there is no cell {token}")))
    }

    fn output_of(&self, cell: &Cell) -> Result<&CellOutput, Failure> {
        self.outputs.get(&cell.id).ok_or_else(|| {
            Failure::new(
                "input_missing",
                format!("cell {} has no output to read", cell.relation_name()),
            )
        })
    }

    async fn workspace(&mut self) -> Result<NotebookWorkspace, Failure> {
        if let Some(ws) = &self.workspace {
            return Ok(ws.clone());
        }
        if !sqlly_services::local_analytics::is_available() {
            return Err(Failure::new(
                "local_unavailable",
                "local notebook data is not available in this build",
            ));
        }
        let id = super::records::notebook_stem(&self.scope);
        let ws = self
            .analytics
            .open_with_keys(&id, workspace_keys())
            .await
            .map_err(|e| Failure::new("local_failed", error_message(&e).fallback()))?;
        self.workspace = Some(ws.clone());
        Ok(ws)
    }

    fn provenance(&self, target: &SlotTarget, sql: &str) -> Provenance {
        let (database, environment) = match target {
            SlotTarget::Remote { profile, .. } => (
                profile.database.clone(),
                serde_json::to_value(profile.environment)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string)),
            ),
            SlotTarget::Local => (None, None),
        };
        Provenance {
            connection_label: target.label(),
            database,
            environment,
            extracted_at: Some(Utc::now()),
            sql_hash: Some(sql_hash(sql)),
            sources: Vec::new(),
        }
    }

    async fn ensure_materialized(&mut self, upstream_id: &str) -> Result<(), Failure> {
        if self.materialized.contains(upstream_id) {
            return Ok(());
        }
        let upstream = self.input_cell(upstream_id)?;
        if upstream.kind == CellKind::Attachment {
            // Its table is the imported file, kept in the workspace by the
            // attachment's own run; its output is only a preview.
            return Ok(());
        }
        let output = self.output_of(upstream)?.clone();
        let relation = upstream.relation_name().to_string();
        let mut source = match self.slots.get(&Slots::key_for(self.doc, upstream)) {
            Some(SlotTarget::Remote { profile, .. }) if upstream.kind == CellKind::Sql => {
                SourceInfo::from_profile(profile)
            }
            _ => SourceInfo::query(DatabaseType::DuckDb, relation.clone()),
        };
        source.sql_hash = output.provenance.sql_hash.clone();
        source.cell_id = Some(upstream.id.clone());
        source.run_id = Some(output.run_id.clone());
        source.extracted_at = output.provenance.extracted_at.map(|t| t.to_rfc3339());
        if !output.completeness.supports_summaries() {
            source.source_incomplete = Some(match output.completeness {
                Completeness::Truncated { cap } => format!("capped at {cap} rows"),
                Completeness::Sampled { n } => format!("a sample of {n} rows"),
                _ => "partial output".to_string(),
            });
        }
        let upstream_key = upstream.id.clone();
        let ws = self.workspace().await?;
        ws.materialize(
            relation,
            output.columns.clone(),
            SnapshotRows::new(output.rows.clone()),
            source,
            WriteMode::Replace,
            CancelToken::new(),
        )
        .await
        .map_err(|e| Failure::new("materialize_failed", error_message(&e).fallback()))?;
        self.materialized.insert(upstream_key);
        Ok(())
    }

    async fn run_sql(&mut self, cell: &Cell) -> Result<CellOutput, Failure> {
        let key = Slots::key_for(self.doc, cell);
        let target = self.slots.get(&key).cloned().ok_or_else(|| {
            Failure::new("unmapped_connection", "the cell's connection is not mapped")
        })?;
        let is_local = matches!(target, SlotTarget::Local);
        let db = target.engine();
        let expansion = expand_sql(
            self.doc,
            &cell.id,
            ExpandOptions {
                dialects: self.dialects,
                materialized: &self.materialized,
            },
        )
        .map_err(|e| Failure::new("expand_failed", e.message().fallback()))?;
        let sql = match expansion {
            Expansion::Sql(expanded) => expanded.sql,
            Expansion::NeedsLocalMaterialization { cells } if !is_local => {
                let names: Vec<String> = cells.iter().map(|id| self.label(id)).collect();
                return Err(Failure::new(
                    "needs_local",
                    format!(
                        "this cell reads {} from another connection or a non-SQL cell; set its connection to `{LOCAL_SLOT}` to join them in the notebook's local data",
                        names.join(", ")
                    ),
                ));
            }
            Expansion::NeedsLocalMaterialization { .. } => {
                let referenced: Vec<String> = cell_references(self.doc, cell, self.dialects)
                    .into_iter()
                    .filter_map(|r| match r.target {
                        RefTarget::Cell(token) => self.doc.resolve(&token).map(|c| c.id.clone()),
                        RefTarget::Param { .. } => None,
                    })
                    .collect();
                for id in referenced {
                    self.ensure_materialized(&id).await?;
                }
                substitute_relations(self.doc, &cell.id, self.dialects, &|c: &Cell| {
                    DatabaseType::DuckDb.quote_identifier(&local_relation_of(c))
                })
                .map_err(|e| Failure::new("expand_failed", e.message().fallback()))?
            }
        };
        let bound = bind_sql(db, &sql, &self.doc.parameters, self.resolved)
            .map_err(|e| Failure::new("parameter", e.message().fallback()))?;
        let shareable = sqlly_services::notebook::params::shareable_sql(
            db,
            &sql,
            &self.doc.parameters,
            self.resolved,
            &bound,
        );
        self.sql_sent.insert(cell.id.clone(), shareable.clone());
        let cap = self.cap_for(cell);
        let timeout = self.timeout_for(cell);
        let started = Instant::now();
        let (columns, rows) = match &target {
            SlotTarget::Local => self.query_local(&bound.sql, cap + 1, timeout).await?,
            SlotTarget::Remote { profile, .. } => {
                let profile = if cell.execution_policy.read_only {
                    let read_only = policy::read_only_profile(profile);
                    sqlly_intel::query::safety::check_connection_policy_for(&read_only, &bound.sql)
                        .map_err(|e| Failure::new("not_read_only", e.to_string()))?;
                    read_only
                } else {
                    (**profile).clone()
                };
                self.query_remote(&profile, &bound.sql, cap + 1, timeout)
                    .await?
            }
        };
        let elapsed = started.elapsed().as_millis() as u64;
        let fingerprint = self.fingerprints[&cell.id].value.clone();
        let mut provenance = self.provenance(&target, &shareable);
        if is_local {
            provenance.sources = self
                .materialized
                .iter()
                .filter_map(|id| self.outputs.get(id).map(|o| o.provenance.clone()))
                .collect();
        }
        Ok(CellOutput::capped(
            columns,
            rows,
            None,
            cap,
            elapsed,
            self.run_id.clone(),
            fingerprint,
            provenance,
        ))
    }

    async fn query_remote(
        &mut self,
        profile: &sqlly_core::ConnectionProfile,
        sql: &str,
        limit: usize,
        timeout: u64,
    ) -> Result<(Vec<ResultColumn>, Vec<Vec<CellValue>>), Failure> {
        let services = self
            .services
            .get_or_insert_with(sqlly_services::Services::new)
            .clone();
        let explain = |message: String| {
            sqlly_intel::query::safety::explain_read_only_refusal(profile.database_type, &message)
                .unwrap_or(message)
        };
        let handle = services
            .queries
            .execute_full_values(profile, None, sql, 256)
            .await
            .map_err(|e| Failure::new("query_failed", explain(CliError::from(e).body.message)))?;
        match crate::agent::query::drain_events(&services, handle, limit, timeout).await {
            Ok((sets, _, _)) => Ok(last_result(sets)),
            Err(err) if err.body.code == "cancelled" => Err(Failure::canceled()),
            Err(err) => Err(Failure::new("query_failed", explain(err.body.message))),
        }
    }

    async fn query_local(
        &mut self,
        sql: &str,
        limit: usize,
        timeout: u64,
    ) -> Result<(Vec<ResultColumn>, Vec<Vec<CellValue>>), Failure> {
        let ws = self.workspace().await?;
        let cancel = CancelToken::new();
        let limit_for = if timeout == 0 {
            Duration::MAX
        } else {
            Duration::from_secs(timeout)
        };
        let query = ws.query(sql.to_string(), Some(limit), cancel.clone());
        tokio::select! {
            result = tokio::time::timeout(limit_for, query) => match result {
                Ok(Ok(sets)) => Ok(sets
                    .into_iter()
                    .rev()
                    .find(|s| !s.columns.is_empty())
                    .map(|s| (s.columns, s.rows))
                    .unwrap_or_default()),
                Ok(Err(e)) => Err(Failure::new("query_failed", error_message(&e).fallback())),
                Err(_) => {
                    cancel.cancel();
                    Err(Failure::new("timeout", format!("statement timed out after {timeout} s")))
                }
            },
            _ = tokio::signal::ctrl_c() => {
                cancel.cancel();
                Err(Failure::canceled())
            }
        }
    }

    fn attachment_path(&self, spec: &AttachmentSpec) -> Result<PathBuf, Failure> {
        attachment_file(
            spec,
            self.notebook_dir.as_deref(),
            &self.approved_files,
            self.scheduled,
        )
    }

    /// Import an attachment's data file into the notebook's local data as
    /// its table (replacing the last import) and read a preview of it.
    async fn run_attachment(&mut self, cell: &Cell) -> Result<CellOutput, Failure> {
        let spec = AttachmentSpec::of(cell);
        let path = self.attachment_path(&spec)?;
        let format = match spec.format {
            Some(AttachmentFormat::Csv) => ImportFormat::Csv,
            Some(AttachmentFormat::Parquet) => ImportFormat::Parquet,
            Some(AttachmentFormat::Json) => ImportFormat::Json,
            Some(AttachmentFormat::Ndjson) => ImportFormat::NdJson,
            Some(AttachmentFormat::Sqlite) => ImportFormat::Sqlite {
                table: spec.table.clone().ok_or_else(|| {
                    Failure::new(
                        "attachment_no_table",
                        "choose the SQLite table to import in the app",
                    )
                })?,
            },
            _ => {
                return Err(Failure::new(
                    "attachment_format",
                    "the attachment's file is not a CSV, Parquet, JSON, NDJSON or SQLite file",
                ));
            }
        };
        let relation = local_relation_of(cell);
        let started = Instant::now();
        let ws = self.workspace().await?;
        let mut import = ImportSpec::new(relation.clone(), path, format);
        import.mode = WriteMode::Replace;
        import.cell_id = Some(cell.id.clone());
        import.run_id = Some(self.run_id.clone());
        let outcome = ws
            .import_file(import, CancelToken::new())
            .await
            .map_err(|e| Failure::new("import_failed", error_message(&e).fallback()))?;
        self.materialized.insert(cell.id.clone());
        let cap = self.cap_for(cell);
        let preview = format!(
            "SELECT * FROM {}",
            DatabaseType::DuckDb.quote_identifier(&relation)
        );
        let (columns, rows) = self
            .query_local(&preview, cap, self.timeout_for(cell))
            .await?;
        let mut output = CellOutput::capped(
            columns,
            rows,
            Some(outcome.row_count),
            cap,
            started.elapsed().as_millis() as u64,
            self.run_id.clone(),
            self.fingerprints[&cell.id].value.clone(),
            Provenance {
                connection_label: spec.file_name.clone().unwrap_or_else(|| relation.clone()),
                extracted_at: Some(Utc::now()),
                ..Provenance::default()
            },
        );
        if !outcome.is_complete() {
            output.completeness = Completeness::Partial;
        }
        Ok(output)
    }

    fn run_assertion(&mut self, cell: &Cell) -> Result<CellOutput, Failure> {
        let spec = specs::assertion_spec(cell).map_err(|e| Failure::new("invalid_spec", e.0))?;
        let input_cell = self.input_cell(&spec.input)?;
        let input = self.output_of(input_cell)?;
        let mut named: BTreeMap<String, OutputView<'_>> = BTreeMap::new();
        for (id, output) in &self.outputs {
            named.insert(id.clone(), view(output));
            if let Some(c) = self.doc.cell(id) {
                named.insert(c.relation_name().to_string(), view(output));
            }
        }
        let baseline = (spec.baseline == BaselineRef::PreviousRun)
            .then(|| self.previous.get(&input_cell.id))
            .flatten()
            .map(|old| profile_output(&view(old), &ProfileOptions::default()));
        let mut context = AssertionContext::new(view(input));
        context.now = Some(Utc::now().naive_utc());
        context.elapsed_ms = Some(input.elapsed_ms);
        context.outputs = &named;
        context.baseline = baseline.as_ref();
        let mut outcomes = evaluate_all(&spec.assertions, &context);
        for (s, o) in spec.assertions.iter().zip(outcomes.iter_mut()) {
            o.name.get_or_insert_with(|| outcome_label(s));
        }
        let rows: Vec<Vec<CellValue>> = spec
            .assertions
            .iter()
            .zip(&outcomes)
            .map(|(s, o)| {
                vec![
                    CellValue::String(outcome_label(s)),
                    CellValue::String(status_text(o.status).into()),
                    CellValue::I64(o.evidence.failing_count as i64),
                    CellValue::I64(o.evidence.checked_rows as i64),
                    o.issue
                        .as_ref()
                        .map_or(CellValue::Null, |i| CellValue::String(issue_text(i))),
                ]
            })
            .collect();
        let output = CellOutput::capped(
            ["check", "status", "failing", "checked", "issue"]
                .map(text_column)
                .to_vec(),
            rows,
            None,
            usize::MAX,
            0,
            self.run_id.clone(),
            self.fingerprints[&cell.id].value.clone(),
            Provenance {
                connection_label: "assertions".into(),
                sources: vec![input.provenance.clone()],
                ..Provenance::default()
            },
        );
        let failing: Vec<String> = spec
            .assertions
            .iter()
            .zip(&outcomes)
            .filter(|(_, o)| matches!(o.status, AssertionStatus::Fail | AssertionStatus::Error))
            .map(|(s, _)| outcome_label(s))
            .collect();
        let total = outcomes.len();
        self.assertions.insert(cell.id.clone(), outcomes);
        if failing.is_empty() {
            Ok(output)
        } else {
            self.outputs.insert(cell.id.clone(), output);
            Err(Failure::new(
                "assertion_failed",
                format!(
                    "{} of {total} checks failed: {}",
                    failing.len(),
                    failing.join(", ")
                ),
            ))
        }
    }

    fn run_kpi(&mut self, cell: &Cell) -> Result<CellOutput, Failure> {
        let spec = specs::kpi_spec(cell).map_err(|e| Failure::new("invalid_spec", e.0))?;
        let input_cell = self.input_cell(spec.input.as_deref().unwrap_or_default())?;
        let input = self.output_of(input_cell)?;
        let baseline = match spec.baseline.as_deref() {
            None => None,
            Some(_) if spec.baseline_is_previous_run() => self.previous.get(&input_cell.id),
            Some(name) => Some(self.output_of(self.input_cell(name)?)?),
        };
        let baseline_view = baseline.map(view);
        let outcomes = kpi::evaluate(&spec, &view(input), baseline_view.as_ref());
        if let Some(error) = outcomes.iter().find_map(|o| o.result.as_ref().err()) {
            return Err(Failure::new("kpi_failed", kpi_error_text(error)));
        }
        let (columns, rows) = kpi::output_rows(&outcomes);
        let output = CellOutput::capped(
            columns,
            rows,
            None,
            outcomes.len(),
            0,
            self.run_id.clone(),
            self.fingerprints[&cell.id].value.clone(),
            Provenance {
                connection_label: "kpi".into(),
                sources: vec![input.provenance.clone()],
                ..Provenance::default()
            },
        );
        for (n, outcome) in outcomes.iter().enumerate() {
            let key = match n {
                0 => cell.id.clone(),
                n => format!("{}#{}", cell.id, n + 1),
            };
            if let Ok(result) = &outcome.result {
                self.kpis
                    .insert(key, serde_json::to_value(result).unwrap_or(Value::Null));
            }
        }
        Ok(output)
    }
}

pub fn status_text(status: AssertionStatus) -> &'static str {
    match status {
        AssertionStatus::Pass => "pass",
        AssertionStatus::Warn => "warn",
        AssertionStatus::Fail => "fail",
        AssertionStatus::Error => "error",
        AssertionStatus::Inconclusive => "inconclusive",
    }
}

fn last_result(sets: Vec<ResultSet>) -> (Vec<ResultColumn>, Vec<Vec<CellValue>>) {
    sets.into_iter()
        .rev()
        .find(|s| !s.columns.is_empty())
        .map(|s| (s.columns, s.rows))
        .unwrap_or_default()
}

fn action_text(action: StepAction) -> &'static str {
    match action {
        StepAction::Execute => "run",
        StepAction::Instant => "instant",
        StepAction::Reuse(_) => "reuse",
        StepAction::Validate => "validate",
    }
}

fn identity_label() -> String {
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown".into());
    format!("{user} (sqlly CLI)")
}

/// Where the app keeps the keys of notebooks whose local data it
/// encrypted, so a run opens the same workspace.
fn workspace_keys() -> Arc<dyn sqlly_services::notebook_security::workspace_keys::WorkspaceKeyStore>
{
    #[cfg(test)]
    return Arc::new(
        sqlly_services::notebook_security::workspace_keys::MemoryWorkspaceKeys::default(),
    );
    #[cfg(not(test))]
    Arc::new(sqlly_services::notebook_security::workspace_keys::KeyringWorkspaceKeys::default())
}

/// An attachment's file on this machine, under the same rules as the app:
/// a path relative to the notebook (followed through links only while it
/// stays inside the folder), or an absolute path only when it was picked in
/// the app on this computer, and never in a scheduled run.
fn attachment_file(
    spec: &AttachmentSpec,
    notebook_dir: Option<&std::path::Path>,
    approved: &BTreeSet<String>,
    scheduled: bool,
) -> Result<PathBuf, Failure> {
    if spec.path.is_some() {
        return spec.resolve(notebook_dir).ok_or_else(|| {
            Failure::new(
                "attachment_outside_folder",
                "the attachment's file is not inside the notebook's folder",
            )
        });
    }
    let Some(absolute) = &spec.absolute_path else {
        return Err(Failure::new(
            "attachment_no_file",
            "the attachment has no file; choose one in the app",
        ));
    };
    if scheduled {
        return Err(Failure::new(
            "attachment_unattended",
            "scheduled runs only read attachment files inside the notebook's folder",
        ));
    }
    if !approved.contains(absolute) {
        return Err(Failure::new(
            "attachment_not_approved",
            "the attachment's file is outside the notebook's folder and was not picked on this computer; choose it again in the app",
        ));
    }
    Ok(PathBuf::from(absolute))
}

/// Cells the command line runs; everything else local is display-only
/// (attachment pictures among them).
fn runs_headless(cell: &Cell) -> bool {
    match cell.kind {
        CellKind::Sql | CellKind::Assertion | CellKind::Kpi => true,
        CellKind::Attachment => !AttachmentSpec::of(cell).is_image(),
        _ => false,
    }
}

/// The table a cell's result is known by in local data: an attachment's
/// import target, else its relation name.
fn local_relation_of(cell: &Cell) -> String {
    match cell.kind {
        CellKind::Attachment => sqlly_services::notebook::cells::local::local_relation(cell),
        _ => cell.relation_name().to_string(),
    }
}

/// Seed the state from the newest run with outputs, so `--mode stale`
/// reuses what still matches.
fn seed_state(
    graph: &Graph,
    fingerprints: &BTreeMap<String, CellFingerprint>,
    previous: &Option<(RunRecord, BTreeMap<String, CellOutput>)>,
) -> (NotebookState, BTreeMap<String, CellOutput>) {
    let mut state = NotebookState::default();
    let mut outputs = BTreeMap::new();
    let Some((record, saved)) = previous else {
        return (state, outputs);
    };
    let details = super::records::details_of(record);
    let finished = record
        .ended_at
        .unwrap_or(record.started_at)
        .timestamp_millis();
    for cell in &record.cells {
        if !matches!(cell.status, CellStatus::Fresh | CellStatus::Cached) {
            continue;
        }
        let (Some(output), Some(parts)) = (
            saved.get(&cell.cell_id),
            details.fingerprints.get(&cell.cell_id),
        ) else {
            continue;
        };
        let fingerprint = CellFingerprint {
            value: cell.fingerprint.clone(),
            parts: parts.clone(),
        };
        let meta = OutputMeta {
            run_id: record.run_id.clone(),
            fingerprint,
            completeness: output.completeness,
            row_count: output.row_count,
            finished_at_ms: finished,
            from_cache: true,
            freshness: Freshness::Snapshot,
        };
        let started = state.apply(
            graph,
            &cell.cell_id,
            CellEvent::Started {
                run_id: record.run_id.clone(),
            },
        );
        if started.is_ok()
            && state
                .apply(graph, &cell.cell_id, CellEvent::Completed { output: meta })
                .is_ok()
        {
            let mut reused = output.clone();
            reused.freshness = Freshness::Snapshot;
            outputs.insert(cell.cell_id.clone(), reused);
        }
    }
    state.refresh_staleness(fingerprints);
    (state, outputs)
}

/// Run a notebook.
pub async fn run(args: &RunArgs, env: &RunEnv) -> Result<RunReport, CliError> {
    let loaded = match source::load_scanned(&args.file)? {
        source::Scanned::Loaded(loaded, report) => {
            for warning in source::findings_text(&report) {
                eprintln!("warning: {warning}");
            }
            *loaded
        }
        source::Scanned::Rejected(report) => {
            return Err(CliError::policy(
                format!(
                    "refused to open the notebook: {}",
                    source::findings_text(&report).join("; ")
                ),
                None,
            ));
        }
    };
    let doc = loaded.doc;
    let param_set = match &args.param_set {
        Some(path) => params::read_param_set(path)?,
        None => BTreeMap::new(),
    };
    let supplied = params::collect(&doc, &param_set, &args.params)?;
    let resolved = resolve_all(&doc.parameters, &supplied, &SystemClock);
    params::check_supplied(&doc, &supplied, &resolved)?;
    let mappings = remap::parse_mappings(&doc, &args.connections)?;
    let slots = remap::resolve(&doc, &mappings, &env.store)?;
    let dialects = slots.dialects();
    let graph = Graph::build(&doc, &dialects);
    let identities = slots.identities();
    let no_overrides = BTreeMap::new();
    let fingerprints = fingerprint_all(
        &doc,
        &graph,
        &FingerprintInputs {
            dialects: &dialects,
            connections: &identities,
            params: &resolved,
            upstream_override: &no_overrides,
        },
    );
    let effective = security::effective_doc(&doc, &slots);
    let scheduled = env.trigger == RunTrigger::Scheduled;
    let trust = security::trust_for_run(
        &effective,
        &args.file,
        &env.runs.trust_path(),
        args.trust && !scheduled,
        args.allow_writes && !scheduled,
    )?;
    let key = NotebookKey::new(
        Some(&trust.scope.clone().unwrap_or_else(|| {
            security::data_scope(&effective, &args.file, &env.runs.trust_path())
        })),
        &doc.notebook_id,
    );
    let needs_previous = args.mode == RunModeArg::Stale
        || doc.cells.iter().any(|c| c.spec.get("baseline").is_some());
    let previous = if needs_previous {
        env.runs.latest_with_outputs(&key)
    } else {
        None
    };
    let (state, seeded) = if args.mode == RunModeArg::Stale {
        seed_state(&graph, &fingerprints, &previous)
    } else {
        (NotebookState::default(), BTreeMap::new())
    };
    let mut request = select::request(&doc, &graph, args.cells.as_deref(), args.mode)?;
    if args.dry_run {
        request = RunRequest::DryRun(Box::new(request));
    }
    let now_ms = Utc::now().timestamp_millis();
    let run_plan = plan(
        &request,
        &PlanContext {
            doc: &doc,
            graph: &graph,
            fingerprints: &fingerprints,
            state: &state,
            params: &resolved,
            now_ms,
        },
    );
    let problems = policy::preflight(
        &doc,
        &run_plan,
        &slots,
        PolicyOptions {
            allow_writes: args.allow_writes && env.trigger != RunTrigger::Scheduled,
            allow_production: args.allow_production,
        },
    );
    if !problems.is_empty() {
        let lines: Vec<String> = problems.iter().map(|p| p.message()).collect();
        let hints: Vec<String> = problems.iter().filter_map(|p| p.hint()).collect();
        let mut hints_dedup: Vec<String> = Vec::new();
        for hint in hints {
            if !hints_dedup.contains(&hint) {
                hints_dedup.push(hint);
            }
        }
        return Err(CliError::policy(
            format!("refused: {}", lines.join("; ")),
            (!hints_dedup.is_empty()).then(|| hints_dedup.join("; ")),
        ));
    }
    let planned: Vec<PlannedStep> = run_plan
        .steps
        .iter()
        .map(|step| PlannedStep {
            cell_id: step.cell_id.clone(),
            cell: doc
                .cell(&step.cell_id)
                .map_or(step.cell_id.clone(), |c| c.relation_name().to_string()),
            kind: doc
                .cell(&step.cell_id)
                .map_or(String::new(), |c| c.kind.as_str().to_string()),
            action: action_text(step.action).to_string(),
        })
        .collect();
    let title = doc.title.clone();
    if run_plan.dry_run {
        let mut summary = RunSummary::default();
        for blocked in &run_plan.blocked {
            summary.blocked.push(blocked.cell_id.clone());
        }
        return Ok(RunReport {
            notebook_id: doc.notebook_id.clone(),
            title,
            dry_run: true,
            planned,
            cells: Vec::new(),
            summary,
            record: None,
            details: RunDetails::default(),
        });
    }

    let auditor = security::Auditor::new(
        &env.runs.audit_dir(),
        &identity_label(),
        &doc.notebook_id,
        &trust.executable_hash,
    );
    let checks = security::check_cells(
        &effective,
        &run_plan,
        &slots,
        &trust,
        args.allow_writes && !scheduled,
    );
    let environment_of = |cell: &Cell| match slots.get(&Slots::key_for(&doc, cell)) {
        Some(SlotTarget::Remote { profile, .. }) => {
            (Some(profile.id.0.to_string()), profile.environment)
        }
        _ => (None, sqlly_core::ConnectionEnvironment::Local),
    };
    let refused: Vec<&security::CellCheck> =
        checks.iter().filter(|c| !c.decision.is_allowed()).collect();
    if !refused.is_empty() {
        for check in &refused {
            let cell = doc.cell(&check.cell_id).expect("planned cell");
            let (profile_id, environment) = environment_of(cell);
            auditor.cell(security::CellAudit {
                cell_id: &check.cell_id,
                decision: &check.decision,
                profile_id: profile_id.as_deref(),
                environment,
                sql: None,
                rows: None,
                duration_ms: None,
                outcome: None,
                error: None,
            });
        }
        let reasons: Vec<String> = refused
            .iter()
            .map(|c| {
                format!(
                    "cell {}: {}",
                    c.label,
                    c.decision
                        .reason()
                        .map_or_else(String::new, |r| r.message().fallback().to_string())
                )
            })
            .collect();
        let hint = if trust.level == sqlly_services::notebook_security::trust::TrustLevel::Untrusted
        {
            Some(format!(
                "{} Trust it in the app, or pass --trust to run it this once",
                trust.reason.message().fallback()
            ))
        } else {
            None
        };
        return Err(CliError::policy(
            format!("refused: {}", reasons.join("; ")),
            hint,
        ));
    }
    let decisions: BTreeMap<String, sqlly_services::notebook_security::authorize::Decision> =
        checks
            .into_iter()
            .map(|c| (c.cell_id, c.decision))
            .collect();

    let run_id = new_run_id(&mut RandomIds);
    let started_at = Utc::now();
    let interrupted = Arc::new(AtomicBool::new(false));
    let watcher = {
        let flag = Arc::clone(&interrupted);
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                flag.store(true, Ordering::SeqCst);
            }
        })
    };
    let mut exec = Exec {
        doc: &doc,
        dialects: &dialects,
        slots: &slots,
        resolved: &resolved,
        fingerprints: &fingerprints,
        redacted: params::redacted_texts(&doc, &resolved),
        run_id: run_id.clone(),
        default_timeout: args.timeout,
        default_cap: args.max_rows.unwrap_or(DEFAULT_OUTPUT_ROW_CAP),
        outputs: seeded,
        previous: previous.map(|(_, outputs)| outputs).unwrap_or_default(),
        materialized: BTreeSet::new(),
        services: None,
        analytics: LocalAnalytics::new(LocalAnalyticsConfig::new(env.runs.local_root())),
        scope: key.scope.clone(),
        workspace: None,
        interrupted: Arc::clone(&interrupted),
        assertions: BTreeMap::new(),
        kpis: BTreeMap::new(),
        sql_sent: BTreeMap::new(),
        notebook_dir: args
            .file
            .canonicalize()
            .ok()
            .and_then(|f| f.parent().map(std::path::Path::to_path_buf)),
        approved_files: env.runs.approved_files(&key.scope),
        scheduled,
    };
    let progress = std::io::IsTerminal::is_terminal(&std::io::stderr())
        && args.format == NotebookFormat::Table;

    let mut summary = RunSummary::default();
    let mut lines: Vec<CellLine> = Vec::new();
    let mut records: Vec<CellRunRecord> = Vec::new();
    let mut failed_steps: BTreeSet<usize> = BTreeSet::new();
    // Cells whose output belongs to this run (produced or reused).
    let mut kept: BTreeSet<String> = BTreeSet::new();
    let record_for = |cell: &Cell, status: CellStatus| CellRunRecord {
        cell_id: cell.id.clone(),
        fingerprint: fingerprints[&cell.id].value.clone(),
        status,
        started_at: None,
        ended_at: None,
        row_count: None,
        completeness: None,
        reused: false,
        sql_hash: None,
        error: None,
        params: redact_params(&doc.parameters, &resolved, &graph.params_of(&cell.id)),
    };
    let line_for = |cell: &Cell, status: CellStatus, note: Option<String>| CellLine {
        cell_id: cell.id.clone(),
        cell: cell.relation_name().to_string(),
        kind: cell.kind.as_str().to_string(),
        status,
        rows: None,
        elapsed_ms: None,
        note,
    };

    // `--mode stale` plans only what is not current; what is current is
    // reused, and recorded so the run stays a complete picture.
    let planned_ids: BTreeSet<&str> = run_plan
        .steps
        .iter()
        .map(|s| s.cell_id.as_str())
        .chain(run_plan.blocked.iter().map(|b| b.cell_id.as_str()))
        .collect();
    if args.mode == RunModeArg::Stale {
        for cell in doc
            .cells
            .iter()
            .filter(|c| !planned_ids.contains(c.id.as_str()))
        {
            let Some(output) = exec.outputs.get(&cell.id) else {
                continue;
            };
            let mut record = record_for(cell, CellStatus::Cached);
            record.reused = true;
            record.row_count = Some(output.row_count);
            record.completeness = Some(output.completeness);
            record.sql_hash = output.provenance.sql_hash.clone();
            records.push(record);
            let mut line = line_for(cell, CellStatus::Cached, Some("reused".into()));
            line.rows = Some(output.row_count);
            lines.push(line);
            summary.reused += 1;
            kept.insert(cell.id.clone());
        }
    }

    for (index, step) in run_plan.steps.iter().enumerate() {
        let Some(cell) = doc.cell(&step.cell_id) else {
            continue;
        };
        if step.action == StepAction::Instant {
            continue;
        }
        let label = cell.relation_name().to_string();
        if exec.interrupted.load(Ordering::SeqCst) || summary.interrupted {
            summary.interrupted = true;
            summary.canceled.push(label);
            records.push(record_for(cell, CellStatus::Canceled));
            lines.push(line_for(cell, CellStatus::Canceled, None));
            failed_steps.insert(index);
            continue;
        }
        if let Some(&dep) = step.depends_on.iter().find(|d| failed_steps.contains(d)) {
            let upstream = run_plan.steps[dep].cell_id.clone();
            let message = BlockReason::UpstreamBlocked {
                cell: exec.label(&upstream),
            }
            .message();
            let mut record = record_for(cell, CellStatus::Blocked);
            record.error = Some(RunError::from(&message));
            records.push(record);
            lines.push(line_for(
                cell,
                CellStatus::Blocked,
                Some(message.fallback().to_string()),
            ));
            summary.blocked.push(label);
            failed_steps.insert(index);
            continue;
        }
        if let StepAction::Reuse(_) = step.action
            && let Some(output) = exec.outputs.get(&cell.id)
        {
            let mut record = record_for(cell, CellStatus::Cached);
            record.reused = true;
            record.row_count = Some(output.row_count);
            record.completeness = Some(output.completeness);
            record.sql_hash = output.provenance.sql_hash.clone();
            records.push(record);
            let mut line = line_for(cell, CellStatus::Cached, Some("reused".into()));
            line.rows = Some(output.row_count);
            lines.push(line);
            summary.reused += 1;
            kept.insert(cell.id.clone());
            continue;
        }
        if !runs_headless(cell) {
            let note = "not run from the command line".to_string();
            let mut record = record_for(cell, CellStatus::Unavailable);
            record.error = Some(RunError {
                key: Some("notebook.cli.not_headless".into()),
                message:
                    "Not run from the command line. Open the notebook in SQLLY to run this cell."
                        .into(),
                args: BTreeMap::new(),
                nested: BTreeMap::new(),
            });
            records.push(record);
            lines.push(line_for(cell, CellStatus::Unavailable, Some(note)));
            summary.skipped.push(label);
            failed_steps.insert(index);
            continue;
        }
        if progress {
            eprintln!("running {label}…");
        }
        let cell_started = Utc::now();
        let result = match cell.kind {
            CellKind::Sql => exec.run_sql(cell).await,
            CellKind::Assertion => exec.run_assertion(cell),
            CellKind::Kpi => exec.run_kpi(cell),
            CellKind::Attachment => exec.run_attachment(cell).await,
            _ => unreachable!("runs_headless"),
        };
        let mut record = record_for(cell, CellStatus::Fresh);
        record.started_at = Some(cell_started);
        record.ended_at = Some(Utc::now());
        if let Some(decision) = decisions.get(&cell.id) {
            let (profile_id, environment) = environment_of(cell);
            let (rows, elapsed, outcome, error) = match &result {
                Ok(o) => (
                    Some(o.row_count),
                    Some(o.elapsed_ms),
                    AuditOutcome::Completed,
                    None,
                ),
                Err(f) if f.canceled => (None, None, AuditOutcome::Canceled, None),
                Err(f) => (
                    None,
                    None,
                    AuditOutcome::Failed,
                    Some(params::scrub(&f.message, &exec.redacted)),
                ),
            };
            auditor.cell(security::CellAudit {
                cell_id: &cell.id,
                decision,
                profile_id: profile_id.as_deref(),
                environment,
                sql: exec.sql_sent.get(&cell.id).map(String::as_str),
                rows,
                duration_ms: elapsed,
                outcome: Some(outcome),
                error: error.as_deref(),
            });
        }
        match result {
            Ok(output) => {
                summary.executed += 1;
                if !output.completeness.supports_summaries() {
                    summary.incomplete.push(label.clone());
                }
                record.row_count = Some(output.row_count);
                record.completeness = Some(output.completeness);
                record.sql_hash = (cell.kind == CellKind::Sql)
                    .then(|| output.provenance.sql_hash.clone())
                    .flatten();
                let mut line = line_for(cell, CellStatus::Fresh, None);
                line.rows = Some(output.row_count);
                line.elapsed_ms = Some(output.elapsed_ms);
                if let Completeness::Truncated { cap } = output.completeness {
                    line.note = Some(format!("first {cap} rows kept"));
                }
                lines.push(line);
                exec.outputs.insert(cell.id.clone(), output);
                kept.insert(cell.id.clone());
            }
            Err(failure) => {
                let message = sqlly_services::notebook_security::redact::mask_secrets(
                    &params::scrub(&failure.message, &exec.redacted),
                )
                .0;
                let status = if failure.canceled {
                    CellStatus::Canceled
                } else {
                    CellStatus::Failed
                };
                record.status = status;
                record.error = Some(RunError {
                    key: Some(format!("notebook.cli.{}", failure.key)),
                    message: message.clone(),
                    args: BTreeMap::new(),
                    nested: BTreeMap::new(),
                });
                if let Some(output) = exec.outputs.get(&cell.id) {
                    record.row_count = Some(output.row_count);
                }
                lines.push(line_for(cell, status, Some(message)));
                if failure.canceled {
                    summary.interrupted = true;
                    summary.canceled.push(label);
                } else if failure.key == "assertion_failed" {
                    summary.executed += 1;
                    kept.insert(cell.id.clone());
                    summary.assertion_cells_failed.push(label);
                } else {
                    summary.failed.push(label);
                }
                failed_steps.insert(index);
            }
        }
        records.push(record);
    }
    watcher.abort();
    if exec.interrupted.load(Ordering::SeqCst) {
        summary.interrupted = true;
    }

    for blocked in &run_plan.blocked {
        let Some(cell) = doc.cell(&blocked.cell_id) else {
            continue;
        };
        let message = blocked.reason.message();
        let mut record = record_for(cell, CellStatus::Blocked);
        record.error = Some(RunError::from(&message));
        let mut text = message.fallback().to_string();
        if let BlockReason::Cycle { .. } = blocked.reason
            && let Some(cycle) = graph.cycles().first()
        {
            text = format!("{text}. {}", cycle.suggestion().fallback());
        }
        records.push(record);
        lines.push(line_for(cell, CellStatus::Blocked, Some(text)));
        if matches!(blocked.reason, BlockReason::UnsupportedKind { .. }) {
            summary.skipped.push(cell.relation_name().to_string());
        } else {
            summary.blocked.push(cell.relation_name().to_string());
        }
    }

    let position = |id: &str| doc.cell_index(id).unwrap_or(usize::MAX);
    records.sort_by_key(|r| position(&r.cell_id));
    lines.sort_by_key(|l| position(&l.cell_id));

    for outcomes in exec.assertions.values() {
        for outcome in outcomes {
            match outcome.status {
                AssertionStatus::Pass => summary.assertions.passed += 1,
                AssertionStatus::Warn => summary.assertions.warned += 1,
                AssertionStatus::Fail => summary.assertions.failed += 1,
                AssertionStatus::Error => summary.assertions.errors += 1,
                AssertionStatus::Inconclusive => summary.assertions.inconclusive += 1,
            }
        }
    }
    for (cell_id, value) in &exec.kpis {
        summary.kpis.push(KpiLine {
            cell: exec.label(cell_id),
            status: value
                .get("status")
                .and_then(|s| serde_json::from_value(s.clone()).ok())
                .unwrap_or(KpiStatus::Unknown),
            value: value.get("value").and_then(Value::as_f64),
            inconclusive: value
                .get("inconclusive")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        });
    }

    let produced: BTreeMap<String, CellOutput> = exec
        .outputs
        .iter()
        .filter(|(id, _)| kept.contains(*id))
        .map(|(id, o)| (id.clone(), o.clone()))
        .collect();
    let details = RunDetails {
        notebook_title: doc.title.clone(),
        notebook_file: args
            .file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        request: args.cells.clone().unwrap_or_else(|| {
            match args.mode {
                RunModeArg::All => "all",
                RunModeArg::Stale => "stale",
            }
            .to_string()
        }),
        parameters: redact_params(
            &doc.parameters,
            &resolved,
            &doc.parameters
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>(),
        ),
        fingerprints: fingerprints
            .iter()
            .map(|(id, fp)| (id.clone(), fp.parts.clone()))
            .collect(),
        connections: slots
            .targets
            .iter()
            .map(|(key, target)| (key.clone(), slot_summary(target)))
            .collect(),
        outputs: produced
            .iter()
            .map(|(id, o)| (id.clone(), OutputSummary::of(o)))
            .collect(),
        assertions: exec
            .assertions
            .iter()
            .map(|(id, o)| (id.clone(), serde_json::to_value(o).unwrap_or(Value::Null)))
            .collect(),
        kpis: exec.kpis.clone(),
        ..RunDetails::default()
    };
    let mut extra = BTreeMap::new();
    extra.insert(
        DETAILS_KEY.to_string(),
        serde_json::to_value(&details).unwrap_or(Value::Null),
    );
    let record = RunRecord {
        run_id: run_id.clone(),
        notebook_id: doc.notebook_id.clone(),
        doc_hash: doc_hash(&doc),
        started_at,
        ended_at: Some(Utc::now()),
        trigger: env.trigger,
        identity_label: identity_label(),
        engine_version: format!("sqlly-cli {}", env!("CARGO_PKG_VERSION")),
        cells: records,
        extra,
    };
    env.runs.append(&key.scope, &record)?;
    if args.retain > 0 {
        env.runs.write_outputs(&key.scope, &run_id, &produced)?;
    }
    env.runs.prune_outputs(&key, args.retain);
    Ok(RunReport {
        notebook_id: doc.notebook_id.clone(),
        title,
        dry_run: false,
        planned,
        cells: lines,
        summary,
        record: Some(record),
        details,
    })
}

fn slot_summary(target: &SlotTarget) -> SlotSummary {
    match target {
        SlotTarget::Local => SlotSummary {
            label: target.label(),
            engine: "DuckDB".into(),
            environment: None,
            profile_id: None,
            database: None,
        },
        SlotTarget::Remote { profile, .. } => SlotSummary {
            label: profile.name.clone(),
            engine: profile.database_type.display_name().to_string(),
            environment: serde_json::to_value(profile.environment)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string)),
            profile_id: Some(profile.id.0.to_string()),
            database: profile.database.clone(),
        },
    }
}

/// The human summary of a report.
pub fn render_text(report: &RunReport) -> String {
    use comfy_table::presets::UTF8_FULL_CONDENSED;
    use comfy_table::{ContentArrangement, Table};
    let mut out = String::new();
    if report.dry_run {
        out.push_str(&format!(
            "Plan for \"{}\" (dry run, nothing ran)\n",
            report.title
        ));
        let mut table = Table::new();
        table.load_style(UTF8_FULL_CONDENSED);
        table.set_content_arrangement(ContentArrangement::Disabled);
        table.set_header(["cell", "kind", "action"]);
        for step in &report.planned {
            table.add_row([step.cell.as_str(), step.kind.as_str(), step.action.as_str()]);
        }
        out.push_str(&table.to_string());
        out.push('\n');
        if !report.summary.blocked.is_empty() {
            out.push_str(&format!("blocked: {}\n", report.summary.blocked.join(", ")));
        }
        return out;
    }
    let run_id = report.record.as_ref().map_or("", |r| r.run_id.as_str());
    out.push_str(&format!("Ran \"{}\" as {run_id}\n", report.title));
    let mut table = Table::new();
    table.load_style(UTF8_FULL_CONDENSED);
    table.set_content_arrangement(ContentArrangement::Disabled);
    table.set_header(["cell", "kind", "status", "rows", "ms", "note"]);
    for line in &report.cells {
        let status = serde_json::to_value(line.status)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default();
        table.add_row([
            line.cell.clone(),
            line.kind.clone(),
            status,
            line.rows.map(|r| r.to_string()).unwrap_or_default(),
            line.elapsed_ms.map(|r| r.to_string()).unwrap_or_default(),
            line.note.clone().unwrap_or_default(),
        ]);
    }
    out.push_str(&table.to_string());
    out.push('\n');
    for (cell_id, outcomes) in &report.details.assertions {
        let Some(list) = outcomes.as_array() else {
            continue;
        };
        for outcome in list {
            let status = outcome.get("status").and_then(Value::as_str).unwrap_or("");
            if matches!(status, "fail" | "error" | "warn" | "inconclusive") {
                let name = outcome
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("check");
                let failing = outcome
                    .pointer("/evidence/failing_count")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let values: Vec<&str> = outcome
                    .pointer("/evidence/failing_values")
                    .and_then(Value::as_array)
                    .map(|v| v.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                let mut text = format!("  {status}: {cell_id} {name}");
                if failing > 0 {
                    text.push_str(&format!(", {failing} failing rows"));
                }
                if !values.is_empty() {
                    text.push_str(&format!(" (e.g. {})", values.join(", ")));
                }
                out.push_str(&text);
                out.push('\n');
            }
        }
    }
    for kpi in &report.summary.kpis {
        let status = serde_json::to_value(kpi.status)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default();
        let value = kpi.value.map_or("-".to_string(), |v| format!("{v}"));
        out.push_str(&format!("  kpi {}: {value} ({status})\n", kpi.cell));
    }
    let s = &report.summary;
    let mut parts = vec![format!("{} ran", s.executed)];
    if s.reused > 0 {
        parts.push(format!("{} reused", s.reused));
    }
    for (count, word) in [
        (s.failed.len(), "failed"),
        (s.blocked.len(), "blocked"),
        (s.canceled.len(), "canceled"),
        (s.skipped.len(), "skipped"),
    ] {
        if count > 0 {
            parts.push(format!("{count} {word}"));
        }
    }
    let a = &s.assertions;
    if a.passed + a.warned + a.failed + a.errors + a.inconclusive > 0 {
        parts.push(format!(
            "checks: {} passed, {} failed, {} warned, {} inconclusive, {} errors",
            a.passed, a.failed, a.warned, a.inconclusive, a.errors
        ));
    }
    out.push_str(&parts.join("; "));
    if s.interrupted {
        out.push_str(" (interrupted)");
    }
    out
}

pub fn report_json(report: &RunReport) -> Value {
    json!({
        "notebook_id": report.notebook_id,
        "title": report.title,
        "dry_run": report.dry_run,
        "planned": report.planned,
        "cells": report.cells,
        "summary": report.summary,
        "record": report.record,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary() -> RunSummary {
        RunSummary::default()
    }

    #[test]
    fn exit_codes_follow_fail_on() {
        let clean = summary();
        for fail_on in [FailOn::Assertion, FailOn::Error, FailOn::Never] {
            assert_eq!(exit_for(&clean, fail_on), ExitCode::Ok);
        }
        let mut checks = summary();
        checks.assertions.failed = 1;
        assert_eq!(
            exit_for(&checks, FailOn::Assertion),
            ExitCode::AssertionFailed
        );
        assert_eq!(exit_for(&checks, FailOn::Error), ExitCode::Ok);
        let mut broken = summary();
        broken.failed.push("orders".into());
        assert_eq!(exit_for(&broken, FailOn::Assertion), ExitCode::Error);
        assert_eq!(exit_for(&broken, FailOn::Error), ExitCode::Error);
        assert_eq!(exit_for(&broken, FailOn::Never), ExitCode::Ok);
        let mut stopped = summary();
        stopped.interrupted = true;
        assert_eq!(exit_for(&stopped, FailOn::Never), ExitCode::Cancelled);
    }

    #[test]
    fn inconclusive_and_warned_checks_do_not_fail_a_run() {
        let mut s = summary();
        s.assertions.inconclusive = 2;
        s.assertions.warned = 1;
        assert_eq!(exit_for(&s, FailOn::Assertion), ExitCode::Ok);
        s.assertions.errors = 1;
        assert_eq!(exit_for(&s, FailOn::Assertion), ExitCode::Error);
    }

    #[test]
    fn attachment_files_follow_the_apps_approval_rules() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("targets.csv"), "a\n1\n").unwrap();
        let outside = tempfile::tempdir().unwrap();
        let elsewhere = outside.path().join("elsewhere.csv");
        std::fs::write(&elsewhere, "a\n1\n").unwrap();
        let elsewhere_text = elsewhere.to_string_lossy().into_owned();
        let relative = AttachmentSpec {
            path: Some("targets.csv".into()),
            ..Default::default()
        };
        let none = BTreeSet::new();
        for scheduled in [false, true] {
            let found = attachment_file(&relative, Some(dir.path()), &none, scheduled).ok();
            assert_eq!(
                found.map(|p| p.canonicalize().unwrap()),
                Some(dir.path().join("targets.csv").canonicalize().unwrap())
            );
        }
        let climbing = AttachmentSpec {
            path: Some("../elsewhere.csv".into()),
            ..Default::default()
        };
        let refused = |r: Result<PathBuf, Failure>| r.err().map(|f| f.key);
        assert_eq!(
            refused(attachment_file(&climbing, Some(dir.path()), &none, false)),
            Some("attachment_outside_folder")
        );
        let absolute = AttachmentSpec {
            absolute_path: Some(elsewhere_text.clone()),
            ..Default::default()
        };
        assert_eq!(
            refused(attachment_file(&absolute, Some(dir.path()), &none, false)),
            Some("attachment_not_approved")
        );
        let approved: BTreeSet<String> = [elsewhere_text].into_iter().collect();
        assert_eq!(
            attachment_file(&absolute, Some(dir.path()), &approved, false).ok(),
            Some(elsewhere.clone())
        );
        assert_eq!(
            refused(attachment_file(
                &absolute,
                Some(dir.path()),
                &approved,
                true
            )),
            Some("attachment_unattended"),
            "a scheduled run reads only files inside the notebook's folder"
        );
        assert_eq!(
            refused(attachment_file(
                &AttachmentSpec::default(),
                Some(dir.path()),
                &none,
                false
            )),
            Some("attachment_no_file")
        );
    }

    #[test]
    fn issues_read_as_text() {
        assert_eq!(
            issue_text(&AssertionIssue::MissingColumn {
                column: "id".into()
            }),
            "missing column (id)"
        );
        assert_eq!(issue_text(&AssertionIssue::NoClock), "no clock");
    }
}
