//! The notebook Export dialog (spec 10.1): format, report layout, scope
//! (whole notebook, one dashboard, selected cells), where SQL and
//! provenance go, whether row data leaves the machine, and which run's
//! results to use, with a preview of what the format cannot carry and what
//! redaction removes before anything is written.
//!
//! Every export goes the same way: the notebook and its outputs are
//! redacted (`notebook_security::redact_for_export`), the export is
//! authorized (sensitive data asks first), a [`ReportDoc`] is built
//! (`report::from_notebook`, or the dashboard's grid for a dashboard), the
//! layout is applied, `report::redact` runs, and `report::render` produces
//! the files, which are written atomically next to the path the user
//! picks. `.sqllynb` and `.ipynb` exports are sanitized copies of the
//! notebook: no results, no personal or sensitive values, no trust marker,
//! and connections by name only.
//!
//! The pure part ([`build_export`], [`build_report`], [`sanitize_notebook`],
//! [`write_files`]) is what the toolbar's Markdown and Jupyter exports use
//! too.

// The browser preview cannot write files, so the dialog never opens there.
#![cfg_attr(target_arch = "wasm32", allow(dead_code, unused_imports))]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use gpui::{
    AnyElement, App, AppContext as _, Context, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_component::Disableable as _;
use gpui_component::Sizable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::checkbox::Checkbox;
use gpui_component::menu::{DropdownMenu as _, PopupMenuItem};
use sqlly_core::DisplayMessage;
use sqlly_services::analytics::assertions::AssertionOutcome;
use sqlly_services::analytics::kpi::KpiResult;
use sqlly_services::notebook::cells::layout::report::{
    DashboardReportOptions, cells_report, dashboard_report,
};
use sqlly_services::notebook::run_record::{RedactedParam, RunRecord};
use sqlly_services::notebook::{CellKind, CellOutput, NotebookDoc, to_ipynb};
use sqlly_services::notebook_security::audit::{
    AuditAction, AuditDecision, AuditEvent, AuditOutcome, ExportAudit, ExportScopeKind,
};
use sqlly_services::notebook_security::authorize::{
    AuthContext, Consents, DataPolicy, Decision, ExportDestination, ExportRequest, NotebookAction,
    authorize,
};
use sqlly_services::notebook_security::redact::{ExportPolicy, redact_for_export};
use sqlly_services::notebook_security::trust::{TrustDecision, TrustLevel};
use sqlly_services::report::doc::{ConnectionLabel, Placement};
use sqlly_services::report::{
    self, NotebookReportInput, RedactionPolicy, RedactionSummary, RenderOptions, RenderedFile,
    ReportDoc, ReportFormat, ReportLabels, TemplateKind, ThemeMode,
};

use super::view::NotebookView;
use crate::theme::Theme;

fn t(key: &str) -> String {
    crate::localization::text(key)
}

fn app_message(message: &DisplayMessage) -> String {
    crate::localization::message(message)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Html,
    Pdf,
    Xlsx,
    Markdown,
    Csv,
    Svg,
    Png,
    /// A sanitized `.sqllynb` copy.
    Notebook,
    /// A sanitized `.ipynb` copy.
    Jupyter,
}

impl ExportFormat {
    pub const ALL: [ExportFormat; 9] = [
        ExportFormat::Html,
        ExportFormat::Pdf,
        ExportFormat::Xlsx,
        ExportFormat::Markdown,
        ExportFormat::Csv,
        ExportFormat::Svg,
        ExportFormat::Png,
        ExportFormat::Notebook,
        ExportFormat::Jupyter,
    ];

    pub fn report_format(self) -> Option<ReportFormat> {
        Some(match self {
            ExportFormat::Html => ReportFormat::Html,
            ExportFormat::Pdf => ReportFormat::Pdf,
            ExportFormat::Xlsx => ReportFormat::Xlsx,
            ExportFormat::Markdown => ReportFormat::Markdown,
            ExportFormat::Csv => ReportFormat::Csv,
            ExportFormat::Svg => ReportFormat::Svg,
            ExportFormat::Png => ReportFormat::Png,
            ExportFormat::Notebook | ExportFormat::Jupyter => return None,
        })
    }

    pub fn extension(self) -> &'static str {
        match self {
            ExportFormat::Notebook => sqlly_services::notebook::document::EXTENSION,
            ExportFormat::Jupyter => "ipynb",
            other => other
                .report_format()
                .map(ReportFormat::extension)
                .unwrap_or("txt"),
        }
    }

    pub fn label_key(self) -> &'static str {
        match self {
            ExportFormat::Html => "notebook.dashboard.export.format_html",
            ExportFormat::Pdf => "notebook.dashboard.export.format_pdf",
            ExportFormat::Xlsx => "notebook.dashboard.export.format_xlsx",
            ExportFormat::Markdown => "notebook.dashboard.export.format_markdown",
            ExportFormat::Csv => "notebook.dashboard.export.format_csv",
            ExportFormat::Svg => "notebook.dashboard.export.format_svg",
            ExportFormat::Png => "notebook.dashboard.export.format_png",
            ExportFormat::Notebook => "notebook.dashboard.export.format_notebook",
            ExportFormat::Jupyter => "notebook.dashboard.export.format_jupyter",
        }
    }

    pub fn is_available(self) -> bool {
        self.report_format().is_none_or(ReportFormat::is_available)
    }

    /// Whether the format is a report (as opposed to a notebook copy).
    pub fn is_report(self) -> bool {
        self.report_format().is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportScope {
    Whole,
    /// The dashboard in this layout cell.
    Dashboard(String),
    /// These cells, in document order.
    Selected(Vec<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlPlacement {
    Inline,
    Appendix,
    Omit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataPermission {
    Full,
    StructureOnly,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunSource {
    /// The results showing in the notebook now.
    Current,
    /// A recorded run: the app's, the command-line runner's or a schedule's.
    Saved(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExportOptions {
    pub format: ExportFormat,
    /// `None` keeps the notebook's own order.
    pub template: Option<TemplateKind>,
    pub scope: ExportScope,
    pub sql: SqlPlacement,
    pub provenance: bool,
    pub data: DataPermission,
    pub run: RunSource,
    pub theme: ThemeMode,
    /// Dashboards: each section on its own PDF page.
    pub page_per_section: bool,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            format: ExportFormat::Html,
            template: None,
            scope: ExportScope::Whole,
            sql: SqlPlacement::Appendix,
            provenance: true,
            data: DataPermission::Full,
            run: RunSource::Current,
            theme: ThemeMode::Light,
            page_per_section: false,
        }
    }
}

/// A run the dialog can export instead of the current results.
#[derive(Debug, Clone, PartialEq)]
pub struct SavedRun {
    pub record: RunRecord,
    pub outputs: BTreeMap<String, CellOutput>,
}

/// Everything the dialog reads from the notebook, taken when it opens.
#[derive(Debug, Clone)]
pub struct ExportSnapshot {
    pub doc: NotebookDoc,
    pub outputs: BTreeMap<String, CellOutput>,
    pub connections: Vec<ConnectionLabel>,
    /// Layout cells: id and display name.
    pub dashboards: Vec<(String, String)>,
    /// The cells selected in the notebook.
    pub selected: Vec<String>,
    pub runs: Vec<SavedRun>,
    pub trusted: bool,
    /// Where the save prompt starts.
    pub directory: Option<PathBuf>,
}

/// What an export will write, shown before it is written.
#[derive(Debug, Clone, PartialEq)]
pub struct ExportPreview {
    pub files: Vec<RenderedFile>,
    /// What the format could not carry, as display text.
    pub notes: Vec<String>,
    pub redaction: RedactionSummary,
    /// What redaction removed or masked, as display text.
    pub removed: Vec<String>,
    /// The message keys of the sensitive findings removed or masked.
    pub findings: Vec<String>,
    pub decision: ExportDecision,
    /// The report the files were rendered from (report formats only).
    pub report: Option<ReportDoc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportDecision {
    Allowed,
    /// Sensitive data was found; writing needs the user's go-ahead.
    NeedsConfirmation(String),
    Refused(String),
}

/// The report for `options`, from an already-redacted notebook and outputs.
pub fn build_report(
    doc: &NotebookDoc,
    outputs: &BTreeMap<String, CellOutput>,
    connections: &[ConnectionLabel],
    run: Option<&RunRecord>,
    options: &ExportOptions,
) -> ReportDoc {
    let assertions: BTreeMap<String, Vec<AssertionOutcome>> = run
        .and_then(|r| r.extra.get("details"))
        .and_then(|d| d.get("assertions"))
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    let kpis: BTreeMap<String, KpiResult> = run
        .and_then(|r| r.extra.get("details"))
        .and_then(|d| d.get("kpis"))
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    let resolved = sqlly_services::notebook::params::resolve_all(
        &doc.parameters,
        &BTreeMap::new(),
        &sqlly_services::notebook::params::SystemClock,
    );
    let names: Vec<&str> = doc.parameters.iter().map(|p| p.name.as_str()).collect();
    let parameters: Vec<RedactedParam> =
        sqlly_services::notebook::run_record::redact_params(&doc.parameters, &resolved, &names)
            .into_iter()
            .filter(|p| !p.redacted && p.value.is_some())
            .collect();
    let labels = ReportLabels::resolve(sqlly_i18n::try_text);
    let input = NotebookReportInput {
        doc,
        run,
        outputs,
        assertions: &assertions,
        kpis: &kpis,
        parameters: &parameters,
        connections,
        generated_at: None,
        labels: &labels,
        resolve_message: app_message,
    };
    let mut report = match &options.scope {
        ExportScope::Whole => report::from_notebook(&input),
        ExportScope::Selected(cells) => cells_report(&input, cells),
        ExportScope::Dashboard(cell) => dashboard_report(
            &input,
            cell,
            DashboardReportOptions {
                page_per_section: options.page_per_section,
            },
        )
        .unwrap_or_else(|| report::from_notebook(&input)),
    };
    if let Some(kind) = options.template {
        report = if matches!(options.scope, ExportScope::Dashboard(_)) {
            // A dashboard keeps its own arrangement; the template brings its
            // cover, row caps and SQL/provenance placement.
            let mut kept = report;
            kept.meta.template = Some(kind);
            kept.meta.layout = kind.layout();
            kept
        } else {
            report::apply_template(&report, kind)
        };
    }
    report.meta.layout.sql = match options.sql {
        SqlPlacement::Inline => Placement::Inline,
        SqlPlacement::Appendix => Placement::Appendix,
        SqlPlacement::Omit => Placement::Omit,
    };
    report.meta.layout.provenance = match (options.provenance, report.meta.layout.provenance) {
        (false, _) => Placement::Omit,
        (true, Placement::Omit) => Placement::Appendix,
        (true, kept) => kept,
    };
    report
}

/// A copy of the notebook that is safe to hand to someone else: sensitive,
/// non-shareable and personal parameter values gone, secrets masked,
/// machine-local connection mappings and the trust marker removed, and no
/// embedded results.
pub fn sanitize_notebook(doc: &NotebookDoc) -> (NotebookDoc, Vec<String>) {
    let redacted = redact_for_export(doc, &BTreeMap::new(), &ExportPolicy::default());
    let mut clean = redacted.doc;
    let mut notes: Vec<String> = redacted
        .warnings
        .iter()
        .map(|w| app_message(&w.message()))
        .collect();
    let mut personal = 0;
    for param in &mut clean.parameters {
        if param.scope == sqlly_services::notebook::params::ParamScope::Personal
            && param.default.take().is_some()
        {
            personal += 1;
        }
    }
    if personal > 0 {
        notes.push(crate::localization::format(
            if personal == 1 {
                "notebook.dashboard.export.personal_removed_one"
            } else {
                "notebook.dashboard.export.personal_removed"
            },
            &[("count", personal)],
        ));
    }
    clean.extra.remove("sqlly_origin");
    clean.data_retention.embed_outputs = false;
    for cell in &mut clean.cells {
        cell.extra.remove("outputs");
    }
    for slot in &mut clean.connection_refs {
        slot.profile_id = None;
    }
    notes.push(t("notebook.dashboard.export.notebook_note"));
    (clean, notes)
}

fn jupyter_notes(doc: &NotebookDoc) -> Vec<String> {
    let mut notes = vec![t("notebook.dashboard.export.jupyter_outputs")];
    let other = doc
        .cells
        .iter()
        .filter(|c| !matches!(c.kind, CellKind::Sql | CellKind::Markdown))
        .count();
    if other > 0 {
        notes.push(crate::localization::format(
            if other == 1 {
                "notebook.dashboard.export.jupyter_other_cells_one"
            } else {
                "notebook.dashboard.export.jupyter_other_cells"
            },
            &[("count", other)],
        ));
    }
    notes
}

/// The files `options` would write and what the export loses or removes.
/// `consented` is the user's go-ahead for sensitive data.
pub fn build_export(
    snapshot: &ExportSnapshot,
    options: &ExportOptions,
    consented: bool,
) -> Result<ExportPreview, String> {
    if !options.format.is_available() {
        return Err(t("notebook.dashboard.export.unavailable"));
    }
    let (outputs, run) = match &options.run {
        RunSource::Current => (&snapshot.outputs, None),
        RunSource::Saved(id) => {
            let saved = snapshot
                .runs
                .iter()
                .find(|r| r.record.run_id == *id)
                .ok_or_else(|| t("notebook.dashboard.export.run_missing"))?;
            (&saved.outputs, Some(&saved.record))
        }
    };
    let title = snapshot.doc.title.clone();
    let stem = {
        let slug = report::text::slug(&title);
        if slug == "section" {
            "notebook".to_string()
        } else {
            slug
        }
    };
    if !options.format.is_report() {
        let (clean, mut notes) = sanitize_notebook(&snapshot.doc);
        let (path, bytes) = if options.format == ExportFormat::Jupyter {
            notes.extend(jupyter_notes(&clean));
            (format!("{stem}.ipynb"), to_ipynb(&clean).into_bytes())
        } else {
            (
                format!("{stem}.{}", options.format.extension()),
                clean.to_canonical_json().into_bytes(),
            )
        };
        return Ok(ExportPreview {
            files: vec![RenderedFile {
                path,
                media_type: "application/json",
                bytes,
            }],
            notes,
            redaction: RedactionSummary::default(),
            removed: Vec::new(),
            findings: Vec::new(),
            decision: ExportDecision::Allowed,
            report: None,
        });
    }
    let redacted = redact_for_export(&snapshot.doc, outputs, &ExportPolicy::default());
    let includes_data = options.data == DataPermission::Full;
    let mut trust = TrustDecision::untrusted(&snapshot.doc);
    if snapshot.trusted {
        trust.level = TrustLevel::Trusted;
    }
    let connections = BTreeMap::new();
    let consents = Consents {
        sensitive_export: consented,
        ..Consents::default()
    };
    let decision = match authorize(
        &NotebookAction::Export(ExportRequest {
            includes_data,
            destination: ExportDestination::LocalFile,
            sensitive_present: redacted.sensitive_present(),
        }),
        &AuthContext {
            doc: &snapshot.doc,
            trust: &trust,
            connections: &connections,
            policy: &DataPolicy::default(),
            consents: &consents,
        },
    ) {
        Decision::Allow { .. } => ExportDecision::Allowed,
        Decision::NeedsApproval(reason) => {
            ExportDecision::NeedsConfirmation(app_message(&reason.message()))
        }
        Decision::Deny(reason) => ExportDecision::Refused(app_message(&reason.message())),
    };
    let report = build_report(
        &redacted.doc,
        &redacted.outputs,
        &snapshot.connections,
        run,
        options,
    );
    let policy = RedactionPolicy {
        drop_data: !includes_data,
        drop_sql: options.sql == SqlPlacement::Omit,
        ..RedactionPolicy::default()
    };
    let (report, summary) = report::redact(&report, &policy);
    let render_options = RenderOptions {
        theme: options.theme,
        labels: ReportLabels::resolve(sqlly_i18n::try_text),
        language: crate::localization::current_language().code().to_string(),
        resolve_message: app_message,
        // A browser download is one file: chart pictures go inside.
        markdown_images: if cfg!(target_arch = "wasm32") {
            report::MarkdownImages::Inline
        } else {
            report::MarkdownImages::Files
        },
        ..RenderOptions::default()
    };
    let format = options.format.report_format().expect("a report format");
    let out =
        report::render(&report, format, &render_options).map_err(|e| app_message(&e.message()))?;
    let mut removed: Vec<String> = redacted
        .warnings
        .iter()
        .map(|w| app_message(&w.message()))
        .collect();
    if summary.rows_removed > 0 {
        removed.push(crate::localization::format(
            if summary.rows_removed == 1 {
                "notebook.dashboard.export.rows_removed_one"
            } else {
                "notebook.dashboard.export.rows_removed"
            },
            &[("count", summary.rows_removed)],
        ));
    }
    if summary.sql_removed > 0 {
        removed.push(crate::localization::format(
            if summary.sql_removed == 1 {
                "notebook.dashboard.export.sql_removed_one"
            } else {
                "notebook.dashboard.export.sql_removed"
            },
            &[("count", summary.sql_removed)],
        ));
    }
    Ok(ExportPreview {
        files: out.files,
        notes: out
            .fidelity
            .notes
            .iter()
            .map(|n| app_message(&n.message()))
            .collect(),
        redaction: summary,
        removed,
        findings: ExportAudit::new("", ExportScopeKind::Whole, false)
            .with_findings(&redacted.warnings)
            .findings,
        decision,
        report: Some(report),
    })
}

/// The audit event for an export written (or failed to write) with
/// `options`: how it was made and what redaction removed, never the data.
pub fn export_audit_event(
    snapshot: &ExportSnapshot,
    options: &ExportOptions,
    preview: &ExportPreview,
    consented: bool,
    error: Option<&str>,
) -> AuditEvent {
    let (scope, cells) = match &options.scope {
        ExportScope::Whole => (ExportScopeKind::Whole, Vec::new()),
        ExportScope::Dashboard(cell) => (ExportScopeKind::Dashboard, vec![cell.clone()]),
        ExportScope::Selected(cells) => (ExportScopeKind::Selected, cells.clone()),
    };
    let mut details = ExportAudit::new(
        options.format.extension(),
        scope,
        options.format.is_report() && options.data == DataPermission::StructureOnly,
    )
    .with_cells(cells);
    details.findings = preview.findings.clone();
    if preview.report.is_some() {
        details = details.with_redaction(preview.redaction.clone());
    }
    let mut event = AuditEvent::new(
        chrono::Utc::now(),
        super::trust::actor(),
        snapshot.doc.notebook_id.as_str(),
        sqlly_services::notebook_security::trust::executable_hash(&snapshot.doc),
        AuditAction::Export,
        &Decision::Allow {
            notices: Vec::new(),
        },
    )
    .with_export(details)
    .with_outcome(
        if error.is_some() {
            AuditOutcome::Failed
        } else {
            AuditOutcome::Completed
        },
        error,
    );
    if consented {
        event.decision = AuditDecision::Approved;
    }
    event
}

/// Write `files` atomically: the first at `primary`, the rest at their
/// relative paths next to it (chart images, one CSV per table). Returns
/// the paths written.
pub fn write_files(primary: &Path, files: &[RenderedFile]) -> std::io::Result<Vec<PathBuf>> {
    let dir = primary.parent().map(Path::to_path_buf).unwrap_or_default();
    let mut written = Vec::new();
    for (n, file) in files.iter().enumerate() {
        let path = if n == 0 {
            primary.to_path_buf()
        } else {
            let relative = Path::new(&file.path);
            if relative.is_absolute()
                || relative
                    .components()
                    .any(|c| !matches!(c, std::path::Component::Normal(_)))
            {
                continue;
            }
            dir.join(relative)
        };
        crate::fs_safe::write_atomic(&path, &file.bytes)?;
        written.push(path);
    }
    Ok(written)
}

/// Saved runs of a notebook under its data scope, newest first, with the
/// outputs each kept (decrypted from the notebook cache; the indexes on
/// disk name cells, never rows). The command-line runner and schedules
/// list their kept cells in an outputs index; the app keeps each cell's
/// output under who ran it, and `readable` says whether rows recorded
/// under that identity may be shown now. App runs with nothing readable
/// are left out.
#[cfg(not(target_arch = "wasm32"))]
pub fn saved_runs(
    stores: &super::history::store::HistoryStores,
    scope: &str,
    notebook_id: &str,
    readable: &dyn Fn(&str, Option<&sqlly_services::notebook::ConnectionIdentity>) -> bool,
) -> Vec<SavedRun> {
    use sqlly_services::notebook::{JsonlRunStore, RunStore as _};
    #[derive(serde::Deserialize)]
    struct OutputsIndex {
        run_id: String,
        cells: Vec<String>,
    }
    let dir = stores.runs_dir(scope);
    let Ok(records) = JsonlRunStore::new(&dir).list(notebook_id) else {
        return Vec::new();
    };
    let mut runs: Vec<SavedRun> = records
        .into_iter()
        .filter_map(|record| {
            let stem = super::history::store::notebook_stem(&record.run_id);
            let file = dir.join("outputs").join(format!("{stem}.json"));
            let index = std::fs::read_to_string(file)
                .ok()
                .and_then(|text| serde_json::from_str::<OutputsIndex>(&text).ok());
            let outputs: BTreeMap<String, CellOutput> = match index {
                Some(index) => {
                    if index.run_id != record.run_id {
                        return None;
                    }
                    index
                        .cells
                        .iter()
                        .filter_map(|cell| {
                            stores
                                .run_output(scope, None, &record.run_id, cell)
                                .map(|o| (cell.clone(), o))
                        })
                        .collect()
                }
                None => {
                    let identities = super::history::details_of(&record).identities;
                    let outputs: BTreeMap<String, CellOutput> = record
                        .cells
                        .iter()
                        .filter_map(|c| {
                            let identity = identities.get(&c.cell_id);
                            if !readable(&c.cell_id, identity) {
                                return None;
                            }
                            stores
                                .run_output(scope, identity, &record.run_id, &c.cell_id)
                                .map(|o| (c.cell_id.clone(), o))
                        })
                        .collect();
                    if outputs.is_empty() {
                        return None;
                    }
                    outputs
                }
            };
            Some(SavedRun { record, outputs })
        })
        .collect();
    runs.reverse();
    runs
}

impl NotebookView {
    /// What the Export dialog works from.
    pub fn export_snapshot(&self, _cx: &App) -> ExportSnapshot {
        let doc = self.doc.model().clone();
        let mut connections: Vec<ConnectionLabel> = Vec::new();
        let mut push = |choice: Option<super::connection::ConnectionChoice>| {
            if let Some(c) = choice
                && !connections.iter().any(|l| l.label == c.label)
            {
                connections.push(ConnectionLabel {
                    label: c.label.clone(),
                    engine: None,
                    database: c.database.clone(),
                    environment: None,
                    production: c.environment == sqlly_core::ConnectionEnvironment::Production,
                });
            }
        };
        for cell in doc.cells.iter().filter(|c| c.kind == CellKind::Sql) {
            push(self.cell_connection(&cell.id).0);
        }
        #[cfg(not(target_arch = "wasm32"))]
        let runs = match &self.trust.scope {
            Some(scope) => {
                let stores = self.analysis.history.stores().cloned().unwrap_or_else(|| {
                    std::sync::Arc::new(super::history::store::HistoryStores::open_default())
                });
                saved_runs(&stores, scope, &doc.notebook_id, &|cell, identity| {
                    self.readable_as_now(cell, identity)
                })
            }
            None => Vec::new(),
        };
        #[cfg(target_arch = "wasm32")]
        let runs = Vec::new();
        ExportSnapshot {
            outputs: doc
                .cells
                .iter()
                .filter_map(|c| {
                    self.outputs
                        .get(&c.id)
                        .map(|o| (c.id.clone(), o.as_ref().clone()))
                })
                .collect(),
            connections,
            dashboards: self.layout_cells(),
            selected: self.selected.clone(),
            runs,
            trusted: self.live_trust().level
                != sqlly_services::notebook_security::TrustLevel::Untrusted,
            directory: self
                .file_path
                .as_deref()
                .and_then(|p| Path::new(p).parent().map(Path::to_path_buf)),
            doc,
        }
    }

    /// Open the Export dialog on `options`.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn open_export_dialog_with(&mut self, options: ExportOptions, cx: &mut Context<Self>) {
        let snapshot = self.export_snapshot(cx);
        cx.defer(move |cx| open_dialog_window(snapshot, options, cx));
    }

    /// Open the Export dialog, starting on `format` and `scope` when given.
    pub fn open_export_dialog(
        &mut self,
        format: Option<ExportFormat>,
        scope: Option<ExportScope>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let snapshot = self.export_snapshot(cx);
        let mut options = ExportOptions {
            format: format.unwrap_or(ExportFormat::Html),
            ..ExportOptions::default()
        };
        options.scope = scope.unwrap_or_else(|| {
            if snapshot.selected.is_empty() {
                ExportScope::Whole
            } else {
                ExportScope::Selected(snapshot.selected.clone())
            }
        });
        // The browser has no second window: the export downloads at once.
        #[cfg(target_arch = "wasm32")]
        {
            let _ = snapshot;
            self.export_in_browser(options, cx);
        }
        #[cfg(not(target_arch = "wasm32"))]
        cx.defer(move |cx| open_dialog_window(snapshot, options, cx));
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn open_dialog_window(snapshot: ExportSnapshot, options: ExportOptions, cx: &mut App) {
    let bounds = gpui::Bounds::centered(None, gpui::size(px(760.0), px(640.0)), cx);
    let window_options = gpui::WindowOptions {
        window_bounds: Some(gpui::WindowBounds::Windowed(bounds)),
        titlebar: Some(gpui::TitlebarOptions {
            title: Some(t("notebook.dashboard.export.window_title").into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let _ = cx.open_window(window_options, |window, cx| {
        crate::localization::register_window_title_key(
            window.window_handle(),
            "notebook.dashboard.export.window_title",
        );
        let dialog = cx.new(|_| NotebookExportDialog::new(snapshot, options));
        cx.new(|cx| gpui_component::Root::new(dialog, window, cx))
    });
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Stage {
    Options,
    /// Sensitive data was found; the user confirms the redaction first.
    Confirm,
    Done(String),
}

pub struct NotebookExportDialog {
    snapshot: ExportSnapshot,
    pub options: ExportOptions,
    preview: Option<Result<ExportPreview, String>>,
    consented: bool,
    stage: Stage,
}

impl NotebookExportDialog {
    pub fn new(snapshot: ExportSnapshot, options: ExportOptions) -> Self {
        let mut this = Self {
            snapshot,
            options,
            preview: None,
            consented: false,
            stage: Stage::Options,
        };
        this.refresh_preview();
        this
    }

    #[cfg(test)]
    pub fn preview(&self) -> Option<&Result<ExportPreview, String>> {
        self.preview.as_ref()
    }

    #[cfg(test)]
    pub fn needs_confirmation(&self) -> bool {
        self.stage == Stage::Confirm
    }

    fn refresh_preview(&mut self) {
        self.preview = Some(build_export(&self.snapshot, &self.options, self.consented));
    }

    pub fn set_options(&mut self, change: impl FnOnce(&mut ExportOptions), cx: &mut Context<Self>) {
        change(&mut self.options);
        self.stage = Stage::Options;
        self.consented = false;
        self.refresh_preview();
        cx.notify();
    }

    /// Export pressed: confirm first when sensitive data was found, then
    /// ask where to save.
    pub fn export(&mut self, cx: &mut Context<Self>) {
        let decision = match &self.preview {
            Some(Ok(preview)) => preview.decision.clone(),
            _ => return,
        };
        match decision {
            ExportDecision::Refused(_) => {}
            ExportDecision::NeedsConfirmation(_) if !self.consented => {
                self.stage = Stage::Confirm;
                cx.notify();
            }
            _ => self.choose_path_and_write(cx),
        }
    }

    pub fn confirm_sensitive(&mut self, cx: &mut Context<Self>) {
        self.consented = true;
        self.refresh_preview();
        self.stage = Stage::Options;
        self.choose_path_and_write(cx);
    }

    fn choose_path_and_write(&mut self, cx: &mut Context<Self>) {
        let Some(Ok(preview)) = self.preview.clone() else {
            return;
        };
        if preview.decision != ExportDecision::Allowed {
            return;
        }
        let name = preview
            .files
            .first()
            .map(|f| f.path.clone())
            .unwrap_or_else(|| format!("notebook.{}", self.options.format.extension()));
        let dir = self
            .snapshot
            .directory
            .clone()
            .unwrap_or_else(|| PathBuf::from("."));
        let rx = cx.prompt_for_new_path(&dir, Some(&name));
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(path))) = rx.await else {
                return;
            };
            let result = write_files(&path, &preview.files);
            this.update(cx, |this, cx| {
                let error = result.as_ref().err().map(|e| e.to_string());
                super::trust::audit(&export_audit_event(
                    &this.snapshot,
                    &this.options,
                    &preview,
                    this.consented,
                    error.as_deref(),
                ));
                this.stage = Stage::Done(match result {
                    Ok(written) => crate::localization::format(
                        if written.len() == 1 {
                            "notebook.dashboard.export.written_one"
                        } else {
                            "notebook.dashboard.export.written"
                        },
                        &[
                            ("count", written.len().to_string()),
                            ("path", path.display().to_string()),
                        ],
                    ),
                    Err(error) => crate::localization::format(
                        "notebook.dashboard.export.write_failed",
                        &[("error", error.to_string())],
                    ),
                });
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn option_row(label: String, control: impl IntoElement) -> gpui::Div {
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(Theme::scaled(8.0)))
            .child(
                div()
                    .w(px(Theme::scaled(130.0)))
                    .text_size(px(Theme::font_size_ratio(0.9)))
                    .text_color(Theme::text_dim())
                    .child(label),
            )
            .child(control)
    }

    fn menu_button(id: &'static str, label: String) -> Button {
        Button::new(id)
            .ghost()
            .small()
            .label(label.clone())
            .accessibility_label(label)
    }

    fn scope_label(&self) -> String {
        match &self.options.scope {
            ExportScope::Whole => t("notebook.dashboard.export.scope_whole"),
            ExportScope::Dashboard(id) => crate::localization::format(
                "notebook.dashboard.export.scope_dashboard",
                &[(
                    "name",
                    self.snapshot
                        .dashboards
                        .iter()
                        .find(|(d, _)| d == id)
                        .map(|(_, n)| n.as_str())
                        .unwrap_or(id.as_str()),
                )],
            ),
            ExportScope::Selected(cells) => crate::localization::format(
                "notebook.dashboard.export.scope_selected",
                &[("count", cells.len())],
            ),
        }
    }

    fn render_options(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let s = Theme::scaled;
        let this = cx.entity();
        let report = self.options.format.is_report();
        let format_menu = {
            let this = this.clone();
            let current = self.options.format;
            Self::menu_button("nb-export-format", t(current.label_key())).dropdown_menu(
                move |mut menu, _, _| {
                    for format in ExportFormat::ALL {
                        let this = this.clone();
                        menu = menu.item(
                            PopupMenuItem::new(t(format.label_key()))
                                .checked(format == current)
                                .disabled(!format.is_available())
                                .on_click(move |_, _, app| {
                                    this.update(app, |d, cx| {
                                        d.set_options(|o| o.format = format, cx)
                                    })
                                }),
                        );
                    }
                    menu
                },
            )
        };
        let template_menu = {
            let this = this.clone();
            let current = self.options.template;
            let label = current
                .map(|k| t(k.label_key()))
                .unwrap_or_else(|| t("notebook.dashboard.export.template_none"));
            Self::menu_button("nb-export-template", label)
                .disabled(!report)
                .dropdown_menu(move |mut menu, _, _| {
                    let none = this.clone();
                    menu = menu.item(
                        PopupMenuItem::new(t("notebook.dashboard.export.template_none"))
                            .checked(current.is_none())
                            .on_click(move |_, _, app| {
                                none.update(app, |d, cx| d.set_options(|o| o.template = None, cx))
                            }),
                    );
                    for kind in TemplateKind::ALL {
                        let this = this.clone();
                        menu = menu.item(
                            PopupMenuItem::new(t(kind.label_key()))
                                .checked(current == Some(kind))
                                .on_click(move |_, _, app| {
                                    this.update(app, |d, cx| {
                                        d.set_options(|o| o.template = Some(kind), cx)
                                    })
                                }),
                        );
                    }
                    menu
                })
        };
        let scope_menu = {
            let this = this.clone();
            let dashboards = self.snapshot.dashboards.clone();
            let selected = self.snapshot.selected.clone();
            Self::menu_button("nb-export-scope", self.scope_label())
                .disabled(!report)
                .dropdown_menu(move |mut menu, _, _| {
                    let whole = this.clone();
                    menu = menu.item(
                        PopupMenuItem::new(t("notebook.dashboard.export.scope_whole")).on_click(
                            move |_, _, app| {
                                whole.update(app, |d, cx| {
                                    d.set_options(|o| o.scope = ExportScope::Whole, cx)
                                })
                            },
                        ),
                    );
                    for (id, name) in &dashboards {
                        let this = this.clone();
                        let id = id.clone();
                        menu = menu.item(
                            PopupMenuItem::new(crate::localization::format(
                                "notebook.dashboard.export.scope_dashboard",
                                &[("name", name.as_str())],
                            ))
                            .on_click(move |_, _, app| {
                                let id = id.clone();
                                this.update(app, |d, cx| {
                                    d.set_options(|o| o.scope = ExportScope::Dashboard(id), cx)
                                })
                            }),
                        );
                    }
                    let picked = this.clone();
                    let cells = selected.clone();
                    menu = menu.item(
                        PopupMenuItem::new(crate::localization::format(
                            "notebook.dashboard.export.scope_selected",
                            &[("count", cells.len())],
                        ))
                        .disabled(cells.is_empty())
                        .on_click(move |_, _, app| {
                            let cells = cells.clone();
                            picked.update(app, |d, cx| {
                                d.set_options(|o| o.scope = ExportScope::Selected(cells), cx)
                            })
                        }),
                    );
                    menu
                })
        };
        let sql_menu = {
            let this = this.clone();
            let current = self.options.sql;
            let key = |p: SqlPlacement| match p {
                SqlPlacement::Inline => "notebook.dashboard.export.sql_inline",
                SqlPlacement::Appendix => "notebook.dashboard.export.sql_appendix",
                SqlPlacement::Omit => "notebook.dashboard.export.sql_none",
            };
            Self::menu_button("nb-export-sql", t(key(current)))
                .disabled(!report)
                .dropdown_menu(move |mut menu, _, _| {
                    for placement in [
                        SqlPlacement::Inline,
                        SqlPlacement::Appendix,
                        SqlPlacement::Omit,
                    ] {
                        let this = this.clone();
                        menu = menu.item(
                            PopupMenuItem::new(t(key(placement)))
                                .checked(placement == current)
                                .on_click(move |_, _, app| {
                                    this.update(app, |d, cx| {
                                        d.set_options(|o| o.sql = placement, cx)
                                    })
                                }),
                        );
                    }
                    menu
                })
        };
        let run_menu = {
            let this = this.clone();
            let runs: Vec<(String, String)> = self
                .snapshot
                .runs
                .iter()
                .map(|r| {
                    (
                        r.record.run_id.clone(),
                        crate::localization::format(
                            "notebook.dashboard.export.run_saved",
                            &[(
                                "started",
                                r.record.started_at.format("%Y-%m-%d %H:%M").to_string(),
                            )],
                        ),
                    )
                })
                .collect();
            let label = match &self.options.run {
                RunSource::Current => t("notebook.dashboard.export.run_current"),
                RunSource::Saved(id) => runs
                    .iter()
                    .find(|(r, _)| r == id)
                    .map(|(_, l)| l.clone())
                    .unwrap_or_else(|| id.clone()),
            };
            Self::menu_button("nb-export-run", label)
                .disabled(!report)
                .dropdown_menu(move |mut menu, _, _| {
                    let current = this.clone();
                    menu = menu.item(
                        PopupMenuItem::new(t("notebook.dashboard.export.run_current")).on_click(
                            move |_, _, app| {
                                current.update(app, |d, cx| {
                                    d.set_options(|o| o.run = RunSource::Current, cx)
                                })
                            },
                        ),
                    );
                    for (id, label) in &runs {
                        let this = this.clone();
                        let id = id.clone();
                        menu = menu.item(PopupMenuItem::new(label.clone()).on_click(
                            move |_, _, app| {
                                let id = id.clone();
                                this.update(app, |d, cx| {
                                    d.set_options(|o| o.run = RunSource::Saved(id), cx)
                                })
                            },
                        ));
                    }
                    menu
                })
        };
        let structure_only = self.options.data == DataPermission::StructureOnly;
        let provenance = self.options.provenance;
        let dark = self.options.theme == ThemeMode::Dark;
        let paged = self.options.page_per_section;
        let dashboard = matches!(self.options.scope, ExportScope::Dashboard(_));
        let themed = matches!(
            self.options.format,
            ExportFormat::Pdf | ExportFormat::Svg | ExportFormat::Png
        );
        div()
            .flex()
            .flex_col()
            .gap(px(s(6.0)))
            .child(Self::option_row(
                t("notebook.dashboard.export.format_label"),
                format_menu,
            ))
            .child(Self::option_row(
                t("notebook.dashboard.export.template_label"),
                template_menu,
            ))
            .child(Self::option_row(
                t("notebook.dashboard.export.scope_label"),
                scope_menu,
            ))
            .child(Self::option_row(
                t("notebook.dashboard.export.sql_label"),
                sql_menu,
            ))
            .child(Self::option_row(
                t("notebook.dashboard.export.run_label"),
                run_menu,
            ))
            .child(
                Checkbox::new("nb-export-provenance")
                    .label(t("notebook.dashboard.export.provenance"))
                    .checked(provenance)
                    .on_click(cx.listener(|d, checked: &bool, _, cx| {
                        let on = *checked;
                        d.set_options(|o| o.provenance = on, cx)
                    })),
            )
            .child(
                Checkbox::new("nb-export-structure")
                    .label(t("notebook.dashboard.export.structure_only"))
                    .checked(structure_only)
                    .on_click(cx.listener(|d, checked: &bool, _, cx| {
                        let on = *checked;
                        d.set_options(
                            |o| {
                                o.data = if on {
                                    DataPermission::StructureOnly
                                } else {
                                    DataPermission::Full
                                }
                            },
                            cx,
                        )
                    })),
            )
            .when(themed, |d| {
                d.child(
                    Checkbox::new("nb-export-dark")
                        .label(t("notebook.dashboard.export.dark_theme"))
                        .checked(dark)
                        .on_click(cx.listener(|d, checked: &bool, _, cx| {
                            let on = *checked;
                            d.set_options(
                                |o| {
                                    o.theme = if on {
                                        ThemeMode::Dark
                                    } else {
                                        ThemeMode::Light
                                    }
                                },
                                cx,
                            )
                        })),
                )
            })
            .when(dashboard && self.options.format == ExportFormat::Pdf, |d| {
                d.child(
                    Checkbox::new("nb-export-paged")
                        .label(t("notebook.dashboard.export.page_per_section"))
                        .checked(paged)
                        .on_click(cx.listener(|d, checked: &bool, _, cx| {
                            let on = *checked;
                            d.set_options(|o| o.page_per_section = on, cx)
                        })),
                )
            })
            .into_any_element()
    }

    fn render_preview(&self) -> AnyElement {
        let s = Theme::scaled;
        let list = |title: String, items: Vec<String>, empty: String, color: gpui::Hsla| {
            div()
                .flex()
                .flex_col()
                .gap(px(s(3.0)))
                .child(
                    div()
                        .text_color(Theme::text())
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child(title),
                )
                .children(if items.is_empty() {
                    vec![
                        div()
                            .text_size(px(Theme::font_size_ratio(0.9)))
                            .text_color(Theme::text_dim())
                            .child(empty),
                    ]
                } else {
                    items
                        .into_iter()
                        .map(|item| {
                            div()
                                .text_size(px(Theme::font_size_ratio(0.9)))
                                .text_color(color)
                                .child(format!("• {item}"))
                        })
                        .collect()
                })
        };
        match &self.preview {
            None => div().into_any_element(),
            Some(Err(message)) => div()
                .text_color(Theme::error())
                .child(message.clone())
                .into_any_element(),
            Some(Ok(preview)) => {
                let files: Vec<String> = preview
                    .files
                    .iter()
                    .map(|f| {
                        crate::localization::format(
                            "notebook.dashboard.export.file_line",
                            &[
                                ("path", f.path.clone()),
                                ("size", f.bytes.len().to_string()),
                            ],
                        )
                    })
                    .collect();
                div()
                    .id("nb-export-preview")
                    .flex()
                    .flex_col()
                    .gap(px(s(12.0)))
                    .child(list(
                        t("notebook.dashboard.export.files_title"),
                        files,
                        String::new(),
                        Theme::text(),
                    ))
                    .child(list(
                        t("notebook.dashboard.export.fidelity_title"),
                        preview.notes.clone(),
                        t("notebook.dashboard.export.fidelity_none"),
                        Theme::text(),
                    ))
                    .child(list(
                        t("notebook.dashboard.export.removed_title"),
                        preview.removed.clone(),
                        t("notebook.dashboard.export.removed_none"),
                        Theme::warning(),
                    ))
                    .when_some(
                        match &preview.decision {
                            ExportDecision::Refused(reason) => Some(reason.clone()),
                            _ => None,
                        },
                        |d, reason| d.child(div().text_color(Theme::error()).child(reason)),
                    )
                    .into_any_element()
            }
        }
    }

    fn render_confirm(&self, cx: &mut Context<Self>) -> AnyElement {
        let s = Theme::scaled;
        let (reason, removed) = match &self.preview {
            Some(Ok(p)) => (
                match &p.decision {
                    ExportDecision::NeedsConfirmation(reason) => reason.clone(),
                    _ => String::new(),
                },
                p.removed.clone(),
            ),
            _ => (String::new(), Vec::new()),
        };
        div()
            .id("nb-export-confirm")
            .flex()
            .flex_col()
            .gap(px(s(8.0)))
            .p(px(s(12.0)))
            .rounded(px(s(5.0)))
            .border_1()
            .border_color(Theme::warning())
            .bg(Theme::panel_bg())
            .child(
                div()
                    .text_color(Theme::text())
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(t("notebook.dashboard.export.confirm_title")),
            )
            .child(div().text_color(Theme::text()).child(reason))
            .children(removed.into_iter().map(|item| {
                div()
                    .text_size(px(Theme::font_size_ratio(0.9)))
                    .text_color(Theme::warning())
                    .child(format!("• {item}"))
            }))
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.9)))
                    .text_color(Theme::text_dim())
                    .child(t("notebook.dashboard.export.confirm_detail")),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(s(8.0)))
                    .child(
                        Button::new("nb-export-confirm-cancel")
                            .ghost()
                            .label(t("notebook.dashboard.export.confirm_cancel"))
                            .on_click(cx.listener(|d, _, _, cx| {
                                d.stage = Stage::Options;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("nb-export-confirm-ok")
                            .primary()
                            .label(t("notebook.dashboard.export.confirm_ok"))
                            .on_click(cx.listener(|d, _, _, cx| d.confirm_sensitive(cx))),
                    ),
            )
            .into_any_element()
    }
}

impl Render for NotebookExportDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let blocked = match &self.preview {
            Some(Ok(p)) => matches!(p.decision, ExportDecision::Refused(_)),
            _ => true,
        };
        let done = match &self.stage {
            Stage::Done(message) => Some(message.clone()),
            _ => None,
        };
        let confirming = self.stage == Stage::Confirm;
        div()
            .flex()
            .flex_col()
            .size_full()
            .gap(px(s(10.0)))
            .p(px(s(14.0)))
            .bg(Theme::panel_bg())
            .text_color(Theme::text())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h_0()
                    .gap(px(s(16.0)))
                    .child(div().w(px(s(330.0))).child(self.render_options(cx)))
                    .child(
                        div()
                            .id("nb-export-preview-scroll")
                            .flex_1()
                            .min_w_0()
                            .overflow_y_scroll()
                            .child(self.render_preview()),
                    ),
            )
            .when(confirming, |d| d.child(self.render_confirm(cx)))
            .when_some(done, |d, message| {
                d.child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.9)))
                        .text_color(Theme::text())
                        .child(message),
                )
            })
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(s(8.0)))
                    .child(
                        Button::new("nb-export-close")
                            .ghost()
                            .label(t("notebook.dashboard.export.close"))
                            .on_click(|_, window, _| window.remove_window()),
                    )
                    .child(
                        Button::new("nb-export-write")
                            .primary()
                            .label(t("notebook.dashboard.export.export_button"))
                            .disabled(blocked || confirming)
                            .on_click(cx.listener(|d, _, _, cx| d.export(cx))),
                    ),
            )
    }
}

#[cfg(test)]
mod tests;
