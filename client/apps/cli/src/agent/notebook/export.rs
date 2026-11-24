//! `sqlly notebook export`: a recorded run as a report, through the same
//! builder (`report::from_notebook`) and renderers the app's export dialog
//! uses. Sensitive parameters and columns are redacted before rendering;
//! `--structure-only` drops every row. The fidelity report (what the
//! format could not carry) is printed after the files are written.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use sqlly_services::analytics::assertions::AssertionOutcome;
use sqlly_services::analytics::kpi::KpiResult;
use sqlly_services::report::doc::ConnectionLabel;
use sqlly_services::report::{
    self, NotebookReportInput, RedactionPolicy, RenderOptions, ReportFormat, TemplateKind,
    ThemeMode,
};

use sqlly_services::notebook::cells::layout::report::{DashboardReportOptions, dashboard_report};
use sqlly_services::notebook::{CellKind, NotebookDoc};
use sqlly_services::notebook_security::audit::{ExportAudit, ExportScopeKind};

use super::records::{RunDetails, RunsDir, details_of};
use super::{security, source};
use sqlly_services::notebook_security::authorize::{
    AuthContext, Consents, DataPolicy, Decision, ExportDestination, ExportRequest, NotebookAction,
    authorize,
};
use sqlly_services::notebook_security::redact::{ExportPolicy, RedactedExport, redact_for_export};
use sqlly_services::notebook_security::trust::TrustDecision;

/// `authorize(Export)` for a local file. Trust does not matter to an export
/// of results already recorded; sensitive findings need the user's word.
fn authorize_export(redacted: &RedactedExport, includes_data: bool, accept: bool) -> Decision {
    let trust = TrustDecision::untrusted(&redacted.doc);
    let connections = BTreeMap::new();
    let policy = DataPolicy::default();
    let consents = Consents {
        sensitive_export: accept,
        ..Consents::default()
    };
    authorize(
        &NotebookAction::Export(ExportRequest {
            includes_data,
            destination: ExportDestination::LocalFile,
            sensitive_present: redacted.sensitive_present(),
        }),
        &AuthContext {
            doc: &redacted.doc,
            trust: &trust,
            connections: &connections,
            policy: &policy,
            consents: &consents,
        },
    )
}
use crate::agent::{CliError, ExitCode};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ExportFormat {
    Html,
    Pdf,
    Xlsx,
    Md,
    Csv,
    Svg,
    Png,
}

impl ExportFormat {
    fn report_format(self) -> ReportFormat {
        match self {
            ExportFormat::Html => ReportFormat::Html,
            ExportFormat::Pdf => ReportFormat::Pdf,
            ExportFormat::Xlsx => ReportFormat::Xlsx,
            ExportFormat::Md => ReportFormat::Markdown,
            ExportFormat::Csv => ReportFormat::Csv,
            ExportFormat::Svg => ReportFormat::Svg,
            ExportFormat::Png => ReportFormat::Png,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum TemplateArg {
    Executive,
    TechnicalIncident,
    Audit,
    Financial,
    Operational,
}

impl TemplateArg {
    fn kind(self) -> TemplateKind {
        match self {
            TemplateArg::Executive => TemplateKind::Executive,
            TemplateArg::TechnicalIncident => TemplateKind::TechnicalIncident,
            TemplateArg::Audit => TemplateKind::Audit,
            TemplateArg::Financial => TemplateKind::Financial,
            TemplateArg::Operational => TemplateKind::Operational,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ThemeArg {
    Light,
    Dark,
}

#[derive(Debug, Clone, clap::Args)]
pub struct ExportArgs {
    pub file: PathBuf,
    /// Run id (or prefix), `latest`, or `latest~N`.
    #[arg(long, default_value = "latest")]
    pub run: String,
    #[arg(long, value_enum)]
    pub format: ExportFormat,
    /// The report file. Formats that write several files (Markdown chart
    /// pictures, one CSV per table, one SVG/PNG per chart) put the rest
    /// next to it.
    #[arg(long)]
    pub out: PathBuf,
    /// Report layout: executive, technical-incident, audit, financial or
    /// operational.
    #[arg(long, value_enum)]
    pub template: Option<TemplateArg>,
    /// Export a dashboard (its layout cell's id or name) with its sections
    /// and grid instead of the whole notebook.
    #[arg(long)]
    pub dashboard: Option<String>,
    /// Leave every row and value out: structure, SQL and checks only.
    #[arg(long)]
    pub structure_only: bool,
    /// Leave SQL text out.
    #[arg(long)]
    pub no_sql: bool,
    /// With --dashboard: start every section on a new page (PDF).
    #[arg(long, requires = "dashboard")]
    pub page_per_section: bool,
    /// Theme for PDF, SVG and PNG (HTML follows the reader).
    #[arg(long, value_enum, default_value_t = ThemeArg::Light)]
    pub theme: ThemeArg,
    /// Overwrite existing files.
    #[arg(long)]
    pub force: bool,
    /// Export even though sensitive values were found (they are removed or
    /// masked either way; this acknowledges the warnings).
    #[arg(long)]
    pub accept_redactions: bool,
}

fn parsed<T: serde::de::DeserializeOwned>(value: &Value) -> Option<T> {
    serde_json::from_value(value.clone()).ok()
}

fn assertions_of(details: &RunDetails) -> BTreeMap<String, Vec<AssertionOutcome>> {
    details
        .assertions
        .iter()
        .map(|(id, list)| {
            let outcomes = list
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(parsed::<AssertionOutcome>)
                .collect();
            (id.clone(), outcomes)
        })
        .collect()
}

fn kpis_of(details: &RunDetails) -> BTreeMap<String, KpiResult> {
    details
        .kpis
        .iter()
        .filter_map(|(id, v)| parsed::<KpiResult>(v).map(|k| (id.clone(), k)))
        .collect()
}

fn connections_of(details: &RunDetails) -> Vec<ConnectionLabel> {
    details
        .connections
        .values()
        .map(|slot| ConnectionLabel {
            label: slot.label.clone(),
            engine: Some(slot.engine.clone()),
            database: slot.database.clone(),
            production: slot.environment.as_deref() == Some("production"),
            environment: slot.environment.clone(),
        })
        .collect()
}

/// Where each rendered file goes: the first at `out`, the rest by their
/// relative path next to it.
pub fn destinations(out: &Path, paths: &[String]) -> Vec<PathBuf> {
    let dir = out.parent().map(Path::to_path_buf).unwrap_or_default();
    paths
        .iter()
        .enumerate()
        .map(|(i, p)| {
            if i == 0 {
                out.to_path_buf()
            } else {
                dir.join(p)
            }
        })
        .collect()
}

/// Write the report; returns the summary text and the JSON result.
pub fn export(args: &ExportArgs, runs: &RunsDir) -> Result<(String, Value), CliError> {
    {
        let format = args.format.report_format();
        if !format.is_available() {
            return Err(CliError::usage(format!(
                "{} export is not available in this build",
                format.extension()
            )));
        }
        let doc = source::load_file(&args.file)?.doc;
        let key = super::records::NotebookKey::of_file(&args.file, runs)?;
        let record = runs.resolve_run(&key, &args.run)?;
        let details = details_of(&record);
        let outputs = runs
            .read_outputs(&key.scope, &record.run_id)
            .unwrap_or_default();
        // Lane 9's export redaction first: secrets masked, sensitive and
        // non-shareable parameters dropped, sensitive columns removed.
        let redacted = redact_for_export(&doc, &outputs, &ExportPolicy::default());
        let warnings: Vec<String> = redacted
            .warnings
            .iter()
            .map(|w| w.message().fallback().to_string())
            .collect();
        let dashboard = args
            .dashboard
            .as_deref()
            .map(|wanted| dashboard_cell(&doc, wanted))
            .transpose()?;
        let decision = authorize_export(&redacted, !args.structure_only, args.accept_redactions);
        let auditor = security::Auditor::new(
            &runs.audit_dir(),
            &format!(
                "{} (sqlly CLI)",
                std::env::var("USER")
                    .or_else(|_| std::env::var("USERNAME"))
                    .unwrap_or_else(|_| "unknown".into())
            ),
            &doc.notebook_id,
            &record.doc_hash,
        );
        let mut audit = match &dashboard {
            Some(cell) => ExportAudit::new(
                format.extension(),
                ExportScopeKind::Dashboard,
                args.structure_only,
            )
            .with_cells([cell.clone()]),
            None => ExportAudit::new(
                format.extension(),
                ExportScopeKind::Whole,
                args.structure_only,
            ),
        }
        .with_findings(&redacted.warnings);
        if !decision.is_allowed() {
            auditor.export(&decision, audit);
            return Err(CliError::policy(
                format!(
                    "refused: {} ({})",
                    decision
                        .reason()
                        .map_or_else(String::new, |r| r.message().fallback().to_string()),
                    warnings.join("; ")
                ),
                Some(
                    "pass --accept-redactions to export with them removed, or --structure-only"
                        .into(),
                ),
            ));
        }
        let (doc, outputs) = (redacted.doc, redacted.outputs);
        let assertions = assertions_of(&details);
        let kpis = kpis_of(&details);
        let connections = connections_of(&details);
        let generated = record.ended_at.unwrap_or(record.started_at).to_rfc3339();
        let input = NotebookReportInput {
            doc: &doc,
            run: Some(&record),
            outputs: &outputs,
            assertions: &assertions,
            kpis: &kpis,
            parameters: &details.parameters,
            connections: &connections,
            generated_at: Some(&generated),
            labels: report::ReportLabels::english(),
            resolve_message: report::english,
        };
        let mut built = match &dashboard {
            Some(cell) => dashboard_report(
                &input,
                cell,
                DashboardReportOptions {
                    page_per_section: args.page_per_section,
                },
            )
            .ok_or_else(|| CliError::usage(format!("{cell} is not a readable dashboard")))?,
            None => report::from_notebook(&input),
        };
        if let Some(template) = args.template {
            built = if dashboard.is_some() {
                // A dashboard keeps its own arrangement; the template brings
                // its cover, row caps and SQL/provenance placement.
                built.meta.template = Some(template.kind());
                built.meta.layout = template.kind().layout();
                built
            } else {
                report::apply_template(&built, template.kind())
            };
        }
        let mut policy = if args.structure_only {
            RedactionPolicy::structure_only()
        } else {
            RedactionPolicy::default()
        };
        policy.drop_sql = args.no_sql;
        let (built, redaction) = report::redact(&built, &policy);
        audit = audit.with_redaction(redaction.clone());
        auditor.export(&decision, audit);
        let options = RenderOptions {
            theme: match args.theme {
                ThemeArg::Light => ThemeMode::Light,
                ThemeArg::Dark => ThemeMode::Dark,
            },
            ..RenderOptions::default()
        };
        let rendered = report::render(&built, format, &options)
            .map_err(|e| CliError::usage(e.message().fallback().to_string()))?;
        let paths: Vec<String> = rendered.files.iter().map(|f| f.path.clone()).collect();
        let targets = destinations(&args.out, &paths);
        if !args.force
            && let Some(existing) = targets.iter().find(|p| p.exists())
        {
            return Err(CliError::usage(format!(
                "{} exists; pass --force to overwrite",
                existing.display()
            )));
        }
        let mut written = Vec::new();
        for (file, target) in rendered.files.iter().zip(&targets) {
            if let Some(dir) = target.parent().filter(|d| !d.as_os_str().is_empty()) {
                std::fs::create_dir_all(dir).map_err(|e| CliError::io(e.to_string()))?;
            }
            std::fs::write(target, &file.bytes)
                .map_err(|e| CliError::io(format!("cannot write {}: {e}", target.display())))?;
            written.push(
                json!({"path": target, "bytes": file.bytes.len(), "media_type": file.media_type}),
            );
        }
        let notes: Vec<String> = rendered
            .fidelity
            .notes
            .iter()
            .map(|n| (options.resolve_message)(&n.message()))
            .collect();
        let mut text = String::new();
        for target in &targets {
            text.push_str(&format!("wrote {}\n", target.display()));
        }
        text.push_str(&format!("from run {}\n", record.run_id));
        if outputs.is_empty() {
            text.push_str("note: this run's output snapshots were not kept; tables are empty\n");
        }
        for warning in &warnings {
            text.push_str(&format!("redacted: {warning}\n"));
        }
        if notes.is_empty() {
            text.push_str("fidelity: nothing was lost in this format");
        } else {
            text.push_str("fidelity:");
            for note in &notes {
                text.push_str(&format!("\n  - {note}"));
            }
        }
        Ok((
            text,
            json!({
                "run_id": record.run_id,
                "dashboard": dashboard,
                "files": written,
                "fidelity": rendered.fidelity,
                "fidelity_notes": notes,
                "redaction": redaction,
                "redaction_warnings": warnings,
            }),
        ))
    }
}

/// The layout cell `wanted` names, by id or name.
fn dashboard_cell(doc: &NotebookDoc, wanted: &str) -> Result<String, CliError> {
    let layouts: Vec<&sqlly_services::notebook::Cell> = doc
        .cells
        .iter()
        .filter(|c| c.kind == CellKind::Layout)
        .collect();
    layouts
        .iter()
        .find(|c| c.id == wanted)
        .or_else(|| {
            layouts.iter().find(|c| {
                c.name
                    .as_deref()
                    .is_some_and(|n| n.eq_ignore_ascii_case(wanted))
            })
        })
        .map(|c| c.id.clone())
        .ok_or_else(|| {
            let known: Vec<String> = layouts
                .iter()
                .map(|c| c.name.clone().unwrap_or_else(|| c.id.clone()))
                .collect();
            CliError::usage(if known.is_empty() {
                format!("no dashboard named {wanted}: this notebook has no dashboards")
            } else {
                format!(
                    "no dashboard named {wanted}; dashboards: {}",
                    known.join(", ")
                )
            })
        })
}

pub fn command(args: &ExportArgs, runs: &RunsDir, json: bool) -> ExitCode {
    match export(args, runs) {
        Ok((text, data)) => crate::agent::emit(json, Some(text), Ok(data)),
        Err(err) => crate::agent::emit(json, None, Err(err)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extra_files_land_next_to_the_report() {
        let out = Path::new("reports").join("weekly.md");
        let paths = vec!["revenue.md".to_string(), "assets/chart-1.svg".to_string()];
        let targets = destinations(&out, &paths);
        assert_eq!(targets[0], out);
        assert_eq!(targets[1], Path::new("reports").join("assets/chart-1.svg"));
    }
}
