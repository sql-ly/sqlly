//! Per-action authorization (spec 12.2 "per-run authorization checks at
//! execution, not only at notebook creation", 5.3, 9.2, 10.2).
//!
//! The host calls [`authorize`] right before it does anything: run a cell,
//! export, materialize locally, share context with a model, install a
//! schedule, or publish/present as an app. The answer is computed from the
//! document as it is now, the current trust decision, the live connection
//! facts and the user's consents, never from what was true when the
//! notebook was created. [`Decision::NeedsApproval`] means the host may ask
//! the user and call again with the consent recorded; [`Decision::Deny`]
//! means no prompt can unlock it here. This is a policy layer in front of,
//! not instead of, the enforced controls (driver read-only sessions, the
//! production guard, mutation review, lane D's DuckDB lockdown).

use std::collections::{BTreeMap, BTreeSet};

use sqlly_core::{ConnectionEnvironment, DatabaseType, DisplayMessage};

use super::messages::message;
use super::trust::{CURRENT_CONNECTION, TrustDecision, TrustLevel};
use crate::notebook::cells::diff::DiffCellSpec;
use crate::notebook::references::Dialects;
use crate::notebook::{Cell, CellKind, ExecClass, Graph, Node, NotebookDoc, migrate};

/// What the host is about to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotebookAction<'a> {
    RunCell {
        cell_id: &'a str,
    },
    Export(ExportRequest),
    MaterializeLocally {
        cell_id: &'a str,
    },
    ShareWithAi(AiShare),
    Schedule {
        allow_production: bool,
    },
    Publish(PublishMode),
    /// Run read-only cell `cell_id` `runs` times to time it (a parameter
    /// sweep). `production_authorized` is the user's yes for this one
    /// benchmark when the cell runs on production.
    Benchmark {
        cell_id: &'a str,
        runs: usize,
        production_authorized: bool,
    },
}

/// Most executions one benchmark may make.
pub const MAX_BENCHMARK_RUNS: usize = crate::notebook::cells::plan::MAX_SWEEP_RUNS;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExportRequest {
    /// Result rows (not just SQL and text) leave the app.
    pub includes_data: bool,
    pub destination: ExportDestination,
    /// Sensitive parameters, sensitive columns or secret-shaped values are
    /// in what would be exported ([`super::redact::sensitive_present`]).
    pub sensitive_present: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportDestination {
    LocalFile,
    Clipboard,
    /// A webhook, email or other configured integration.
    External,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AiShare {
    pub model: ModelLocation,
    /// Result rows or row summaries, not only schema and SQL.
    pub includes_rows: bool,
    pub sensitive_present: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelLocation {
    Local,
    Cloud,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishMode {
    /// Presentation / viewer mode that re-runs cells.
    Present,
    /// An interactive dashboard or app others use.
    InteractiveApp,
    /// A rendered report of existing outputs; nothing re-runs.
    StaticReport,
}

/// What the host knows about a connection slot right now.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConnectionFacts {
    /// The profile the slot maps to on this machine; `None` is unmapped.
    pub profile_id: Option<String>,
    pub engine: Option<DatabaseType>,
    pub environment: ConnectionEnvironment,
    /// The connection's opt-in to keeping copies of its results locally.
    pub allow_local_materialize: bool,
    /// The production guard's timed write unlock is active.
    pub writes_unlocked: bool,
}

impl ConnectionFacts {
    fn is_production(&self) -> bool {
        self.environment == ConnectionEnvironment::Production
    }
}

/// Settings-level data controls (spec 12.2 "export allow/deny,
/// remote-model sharing approval").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataPolicy {
    pub export_allowed: bool,
    pub external_destinations_allowed: bool,
    pub cloud_ai_allowed: bool,
}

impl Default for DataPolicy {
    fn default() -> Self {
        DataPolicy {
            export_allowed: true,
            external_destinations_allowed: true,
            cloud_ai_allowed: true,
        }
    }
}

/// What the user approved for this session.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Consents {
    /// Write cells the user elevated (the existing mutation review still
    /// runs on their SQL).
    pub elevated_cells: BTreeSet<String>,
    pub cloud_ai: bool,
    pub sensitive_export: bool,
    pub external_destination: bool,
}

pub struct AuthContext<'a> {
    pub doc: &'a NotebookDoc,
    pub trust: &'a TrustDecision,
    /// Facts per slot id; [`CURRENT_CONNECTION`] is the host's connection.
    pub connections: &'a BTreeMap<String, ConnectionFacts>,
    pub policy: &'a DataPolicy,
    pub consents: &'a Consents,
}

/// Labels the host shows with an allowed action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Notice {
    /// The run touches a production connection (spec 12.3).
    Production,
    /// Sensitive data is included under the user's consent.
    SensitiveData,
    /// The notebook is not trusted here; its text is untrusted model input.
    UntrustedContent,
}

impl Notice {
    pub fn notice_key(self) -> &'static str {
        match self {
            Notice::Production => "notebook.security.notice.production",
            Notice::SensitiveData => "notebook.security.notice.sensitive_data",
            Notice::UntrustedContent => "notebook.security.notice.untrusted_content",
        }
    }

    pub fn message(self) -> DisplayMessage {
        let text = match self {
            Notice::Production => "Production",
            Notice::SensitiveData => "Includes sensitive data",
            Notice::UntrustedContent => "From an untrusted notebook",
        };
        message(self.notice_key(), text, &[])
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    TrustRequired,
    UnknownCell {
        cell_id: String,
    },
    UnsupportedKind {
        kind: String,
    },
    ConnectionUnmapped {
        slot: String,
    },
    ConnectionNotAuthorized {
        slot: String,
    },
    ReadOnlyCellChangesData,
    WritesNotAllowed,
    WriteNeedsElevation,
    ProductionWritesLocked,
    ExportDisabled,
    SensitiveData,
    ExternalDestinationDisabled,
    ExternalDestination,
    MaterializeNotEnabled {
        slot: String,
    },
    CloudModelDisabled,
    CloudModelConsent,
    ScheduleNeedsFullTrust,
    ScheduleHasWriteCells {
        count: usize,
    },
    ScheduleProduction {
        slot: String,
    },
    PublishHasWriteCells {
        count: usize,
    },
    BenchmarkNoRuns,
    BenchmarkTooManyRuns {
        max: usize,
    },
    /// Repeating a cell that changes data would repeat the change.
    BenchmarkWritesData,
    BenchmarkProduction {
        slot: String,
    },
}

impl Reason {
    pub fn reason_key(&self) -> &'static str {
        match self {
            Reason::TrustRequired => "notebook.security.reason.trust_required",
            Reason::UnknownCell { .. } => "notebook.security.reason.unknown_cell",
            Reason::UnsupportedKind { .. } => "notebook.security.reason.unsupported_kind",
            Reason::ConnectionUnmapped { .. } => "notebook.security.reason.connection_unmapped",
            Reason::ConnectionNotAuthorized { .. } => {
                "notebook.security.reason.connection_not_authorized"
            }
            Reason::ReadOnlyCellChangesData => "notebook.security.reason.read_only_changes_data",
            Reason::WritesNotAllowed => "notebook.security.reason.writes_not_allowed",
            Reason::WriteNeedsElevation => "notebook.security.reason.write_needs_elevation",
            Reason::ProductionWritesLocked => "notebook.security.reason.production_writes_locked",
            Reason::ExportDisabled => "notebook.security.reason.export_disabled",
            Reason::SensitiveData => "notebook.security.reason.sensitive_data",
            Reason::ExternalDestinationDisabled => {
                "notebook.security.reason.external_destination_disabled"
            }
            Reason::ExternalDestination => "notebook.security.reason.external_destination",
            Reason::MaterializeNotEnabled { .. } => {
                "notebook.security.reason.materialize_not_enabled"
            }
            Reason::CloudModelDisabled => "notebook.security.reason.cloud_model_disabled",
            Reason::CloudModelConsent => "notebook.security.reason.cloud_model_consent",
            Reason::ScheduleNeedsFullTrust => "notebook.security.reason.schedule_needs_full_trust",
            Reason::ScheduleHasWriteCells { .. } => {
                "notebook.security.reason.schedule_has_write_cells"
            }
            Reason::ScheduleProduction { .. } => "notebook.security.reason.schedule_production",
            Reason::PublishHasWriteCells { .. } => {
                "notebook.security.reason.publish_has_write_cells"
            }
            Reason::BenchmarkNoRuns => "notebook.security.reason.benchmark_no_runs",
            Reason::BenchmarkTooManyRuns { .. } => {
                "notebook.security.reason.benchmark_too_many_runs"
            }
            Reason::BenchmarkWritesData => "notebook.security.reason.benchmark_writes_data",
            Reason::BenchmarkProduction { .. } => "notebook.security.reason.benchmark_production",
        }
    }

    pub fn message(&self) -> DisplayMessage {
        let key = self.reason_key();
        match self {
            Reason::TrustRequired => message(
                key,
                "Trust this notebook and choose its connections before running it.",
                &[],
            ),
            Reason::UnknownCell { cell_id } => message(
                key,
                "There is no cell \"{cell_id}\" in this notebook.",
                &[("cell_id", cell_id.clone())],
            ),
            Reason::UnsupportedKind { kind } => message(
                key,
                "\"{kind}\" cells can't run in this version of SQLLY.",
                &[("kind", kind.clone())],
            ),
            Reason::ConnectionUnmapped { slot } => message(
                key,
                "Choose which of your connections \"{slot}\" should use.",
                &[("slot", slot.clone())],
            ),
            Reason::ConnectionNotAuthorized { slot } => message(
                key,
                "This notebook isn't allowed to use \"{slot}\" yet. Allow the connection to run the cell.",
                &[("slot", slot.clone())],
            ),
            Reason::ReadOnlyCellChangesData => message(
                key,
                "This cell is read-only but its SQL changes data. Allow changes for the cell or edit the SQL.",
                &[],
            ),
            Reason::WritesNotAllowed => message(
                key,
                "This notebook is trusted for read-only runs, so cells that change data can't run.",
                &[],
            ),
            Reason::WriteNeedsElevation => message(
                key,
                "This cell changes data. Allow changes for this cell and review the SQL before it runs.",
                &[],
            ),
            Reason::ProductionWritesLocked => message(
                key,
                "Writes to this production connection are locked. Unlock writes to run the cell.",
                &[],
            ),
            Reason::ExportDisabled => message(
                key,
                "Exporting notebook data is turned off in your settings.",
                &[],
            ),
            Reason::SensitiveData => message(
                key,
                "This includes sensitive parameters, columns or values that look like secrets. Confirm to continue with them removed or masked.",
                &[],
            ),
            Reason::ExternalDestinationDisabled => message(
                key,
                "Sending notebook data to webhooks, email or other services is turned off in your settings.",
                &[],
            ),
            Reason::ExternalDestination => message(
                key,
                "This sends notebook data outside SQLLY. Confirm the destination to continue.",
                &[],
            ),
            Reason::MaterializeNotEnabled { slot } => message(
                key,
                "The connection used by \"{slot}\" doesn't allow keeping copies of its results on this computer. Turn it on in the connection's settings.",
                &[("slot", slot.clone())],
            ),
            Reason::CloudModelDisabled => message(
                key,
                "Sharing notebook content with cloud AI models is turned off in your settings.",
                &[],
            ),
            Reason::CloudModelConsent => message(
                key,
                "This shares notebook content with a cloud AI model. Review what will be sent and confirm.",
                &[],
            ),
            Reason::ScheduleNeedsFullTrust => message(
                key,
                "Only fully trusted notebooks can run on a schedule.",
                &[],
            ),
            Reason::ScheduleHasWriteCells { count: 1 } => message(
                "notebook.security.reason.schedule_has_write_cells_one",
                "Notebooks with cells that change data can't run on a schedule (1 such cell).",
                &[("count", "1".to_string())],
            ),
            Reason::ScheduleHasWriteCells { count } => message(
                key,
                "Notebooks with cells that change data can't run on a schedule ({count} such cells).",
                &[("count", count.to_string())],
            ),
            Reason::ScheduleProduction { slot } => message(
                key,
                "\"{slot}\" is a production connection. Allow production explicitly to schedule this notebook.",
                &[("slot", slot.clone())],
            ),
            Reason::PublishHasWriteCells { count: 1 } => message(
                "notebook.security.reason.publish_has_write_cells_one",
                "Notebooks with cells that change data can't be presented or published as an app (1 such cell).",
                &[("count", "1".to_string())],
            ),
            Reason::PublishHasWriteCells { count } => message(
                key,
                "Notebooks with cells that change data can't be presented or published as an app ({count} such cells).",
                &[("count", count.to_string())],
            ),
            Reason::BenchmarkNoRuns => message(
                key,
                "There is nothing to time: give the sweep at least one value.",
                &[],
            ),
            Reason::BenchmarkTooManyRuns { max } => message(
                key,
                "A benchmark can run the query at most {max} times. Use fewer values or repetitions.",
                &[("max", max.to_string())],
            ),
            Reason::BenchmarkWritesData => message(
                key,
                "This cell changes data, so it can't be timed: every run would repeat the change.",
                &[],
            ),
            Reason::BenchmarkProduction { slot } => message(
                key,
                "\"{slot}\" is a production connection. Allow this benchmark explicitly to run the query repeatedly there.",
                &[("slot", slot.clone())],
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow { notices: Vec<Notice> },
    Deny(Reason),
    NeedsApproval(Reason),
}

impl Decision {
    fn allow(notices: impl IntoIterator<Item = Notice>) -> Decision {
        let mut notices: Vec<Notice> = notices.into_iter().collect();
        notices.sort();
        notices.dedup();
        Decision::Allow { notices }
    }

    pub fn is_allowed(&self) -> bool {
        matches!(self, Decision::Allow { .. })
    }

    pub fn reason(&self) -> Option<&Reason> {
        match self {
            Decision::Allow { .. } => None,
            Decision::Deny(reason) | Decision::NeedsApproval(reason) => Some(reason),
        }
    }

    /// `allow`, `deny` or `needs_approval`, for audit events.
    pub fn verdict(&self) -> &'static str {
        match self {
            Decision::Allow { .. } => "allow",
            Decision::Deny(_) => "deny",
            Decision::NeedsApproval(_) => "needs_approval",
        }
    }
}

/// Whether `cell` changes data: an explicit write cell, or a read-only cell
/// whose SQL does (judged per engine; the driver's read-only session is the
/// real guard).
fn is_write_cell(cell: &Cell) -> bool {
    !cell.execution_policy.read_only
}

fn runs_remotely(cell: &Cell) -> bool {
    cell.exec_class() == ExecClass::Remote
}

fn slot_of<'a>(doc: &'a NotebookDoc, cell: &'a Cell) -> &'a str {
    doc.effective_connection_ref(cell)
        .unwrap_or(CURRENT_CONNECTION)
}

/// The remote cells `cell_id` reads from, itself included.
fn remote_sources<'a>(ctx: &'a AuthContext<'_>, cell_id: &str) -> Vec<&'a Cell> {
    let dialects = Dialects {
        default: ctx
            .connections
            .get(CURRENT_CONNECTION)
            .and_then(|f| f.engine)
            .unwrap_or_default(),
        by_connection_ref: ctx
            .connections
            .iter()
            .filter_map(|(slot, f)| f.engine.map(|e| (slot.clone(), e)))
            .collect(),
    };
    let graph = Graph::build(ctx.doc, &dialects);
    let mut ids: BTreeSet<String> = graph
        .upstream_closure(&Node::cell(cell_id))
        .into_iter()
        .filter_map(|n| n.as_cell().map(str::to_string))
        .collect();
    ids.insert(cell_id.to_string());
    ctx.doc
        .cells
        .iter()
        .filter(|c| ids.contains(&c.id) && runs_remotely(c))
        .collect()
}

/// The slot check shared by runs: mapped, authorized, same profile.
fn check_slot<'a>(ctx: &AuthContext<'a>, slot: &str) -> Result<&'a ConnectionFacts, Decision> {
    let Some(facts) = ctx.connections.get(slot).filter(|f| f.profile_id.is_some()) else {
        return Err(Decision::Deny(Reason::ConnectionUnmapped {
            slot: slot.to_string(),
        }));
    };
    if !ctx.trust.authorizes(slot, facts.profile_id.as_deref()) {
        return Err(Decision::NeedsApproval(Reason::ConnectionNotAuthorized {
            slot: slot.to_string(),
        }));
    }
    Ok(facts)
}

/// A local cell that re-runs SQL elsewhere (a diff side on another
/// connection) needs that slot authorized like a remote cell.
fn authorize_side_connections(ctx: &AuthContext<'_>, cell: &Cell) -> Decision {
    if cell.kind != CellKind::Diff {
        return Decision::allow([]);
    }
    let spec = DiffCellSpec::from_spec(&cell.spec);
    let mut notices = vec![];
    for slot in spec.connection_slots() {
        match check_slot(ctx, slot) {
            Ok(facts) if facts.is_production() => notices.push(Notice::Production),
            Ok(_) => {}
            Err(decision) => return decision,
        }
    }
    notices.dedup();
    Decision::allow(notices)
}

fn authorize_run(ctx: &AuthContext<'_>, cell_id: &str) -> Decision {
    let Some(cell) = ctx.doc.cell(cell_id) else {
        return Decision::Deny(Reason::UnknownCell {
            cell_id: cell_id.to_string(),
        });
    };
    match cell.kind.exec_class() {
        ExecClass::Instant => return Decision::allow([]),
        ExecClass::Unsupported => {
            return Decision::Deny(Reason::UnsupportedKind {
                kind: cell.kind.as_str().to_string(),
            });
        }
        ExecClass::Local | ExecClass::Remote => {}
    }
    if ctx.trust.level == TrustLevel::Untrusted {
        return Decision::NeedsApproval(Reason::TrustRequired);
    }
    if cell.exec_class() == ExecClass::Local {
        return authorize_side_connections(ctx, cell);
    }
    let slot = slot_of(ctx.doc, cell);
    let facts = match check_slot(ctx, slot) {
        Ok(facts) => facts,
        Err(decision) => return decision,
    };
    let mut notices = vec![];
    if facts.is_production() {
        notices.push(Notice::Production);
    }
    if !is_write_cell(cell) {
        if migrate::writes_data(facts.engine, &cell.source) {
            return Decision::Deny(Reason::ReadOnlyCellChangesData);
        }
        return Decision::allow(notices);
    }
    if !ctx.trust.allow_writes {
        return Decision::Deny(Reason::WritesNotAllowed);
    }
    if !ctx.consents.elevated_cells.contains(&cell.id) {
        return Decision::NeedsApproval(Reason::WriteNeedsElevation);
    }
    if facts.is_production() && !facts.writes_unlocked {
        return Decision::NeedsApproval(Reason::ProductionWritesLocked);
    }
    Decision::allow(notices)
}

fn authorize_benchmark(
    ctx: &AuthContext<'_>,
    cell_id: &str,
    runs: usize,
    production_authorized: bool,
) -> Decision {
    if runs == 0 {
        return Decision::Deny(Reason::BenchmarkNoRuns);
    }
    if runs > MAX_BENCHMARK_RUNS {
        return Decision::Deny(Reason::BenchmarkTooManyRuns {
            max: MAX_BENCHMARK_RUNS,
        });
    }
    if ctx.doc.cell(cell_id).is_some_and(is_write_cell) {
        return Decision::Deny(Reason::BenchmarkWritesData);
    }
    let decision = authorize_run(ctx, cell_id);
    let Decision::Allow { notices } = &decision else {
        return decision;
    };
    if notices.contains(&Notice::Production) && !production_authorized {
        let slot = ctx
            .doc
            .cell(cell_id)
            .map(|cell| slot_of(ctx.doc, cell).to_string())
            .unwrap_or_default();
        return Decision::NeedsApproval(Reason::BenchmarkProduction { slot });
    }
    decision
}

fn authorize_export(ctx: &AuthContext<'_>, request: ExportRequest) -> Decision {
    let mut notices = vec![];
    if request.includes_data && !ctx.policy.export_allowed {
        return Decision::Deny(Reason::ExportDisabled);
    }
    if request.destination == ExportDestination::External {
        if !ctx.policy.external_destinations_allowed {
            return Decision::Deny(Reason::ExternalDestinationDisabled);
        }
        if !ctx.consents.external_destination {
            return Decision::NeedsApproval(Reason::ExternalDestination);
        }
    }
    if request.sensitive_present {
        if !ctx.consents.sensitive_export {
            return Decision::NeedsApproval(Reason::SensitiveData);
        }
        notices.push(Notice::SensitiveData);
    }
    Decision::allow(notices)
}

fn authorize_materialize(ctx: &AuthContext<'_>, cell_id: &str) -> Decision {
    if ctx.doc.cell(cell_id).is_none() {
        return Decision::Deny(Reason::UnknownCell {
            cell_id: cell_id.to_string(),
        });
    }
    if ctx.trust.level == TrustLevel::Untrusted {
        return Decision::NeedsApproval(Reason::TrustRequired);
    }
    let mut notices = vec![];
    for source in remote_sources(ctx, cell_id) {
        let slot = slot_of(ctx.doc, source);
        let facts = match check_slot(ctx, slot) {
            Ok(facts) => facts,
            Err(decision) => return decision,
        };
        if !facts.allow_local_materialize {
            return Decision::Deny(Reason::MaterializeNotEnabled {
                slot: slot.to_string(),
            });
        }
        if facts.is_production() {
            notices.push(Notice::Production);
        }
    }
    Decision::allow(notices)
}

fn authorize_ai(ctx: &AuthContext<'_>, share: AiShare) -> Decision {
    let mut notices = vec![];
    if ctx.trust.level == TrustLevel::Untrusted {
        notices.push(Notice::UntrustedContent);
    }
    if share.model == ModelLocation::Cloud {
        if !ctx.policy.cloud_ai_allowed {
            return Decision::Deny(Reason::CloudModelDisabled);
        }
        if !ctx.consents.cloud_ai {
            return Decision::NeedsApproval(Reason::CloudModelConsent);
        }
        if share.includes_rows && share.sensitive_present {
            if !ctx.consents.sensitive_export {
                return Decision::NeedsApproval(Reason::SensitiveData);
            }
            notices.push(Notice::SensitiveData);
        }
    }
    Decision::allow(notices)
}

fn write_cells(doc: &NotebookDoc) -> usize {
    doc.cells
        .iter()
        .filter(|c| runs_remotely(c) && is_write_cell(c))
        .count()
}

fn authorize_schedule(ctx: &AuthContext<'_>, allow_production: bool) -> Decision {
    if ctx.trust.level != TrustLevel::Trusted {
        return Decision::Deny(Reason::ScheduleNeedsFullTrust);
    }
    let writes = write_cells(ctx.doc);
    if writes > 0 {
        return Decision::Deny(Reason::ScheduleHasWriteCells { count: writes });
    }
    let mut notices = vec![];
    let slots: BTreeSet<&str> = ctx
        .doc
        .cells
        .iter()
        .filter(|c| runs_remotely(c))
        .map(|c| slot_of(ctx.doc, c))
        .collect();
    for slot in slots {
        // Nobody is there to answer a prompt when a schedule fires.
        let facts = match check_slot(ctx, slot) {
            Ok(facts) => facts,
            Err(Decision::NeedsApproval(reason)) => return Decision::Deny(reason),
            Err(decision) => return decision,
        };
        if facts.is_production() {
            if !allow_production {
                return Decision::Deny(Reason::ScheduleProduction {
                    slot: slot.to_string(),
                });
            }
            notices.push(Notice::Production);
        }
    }
    Decision::allow(notices)
}

fn authorize_publish(ctx: &AuthContext<'_>, mode: PublishMode) -> Decision {
    if mode == PublishMode::StaticReport {
        return Decision::allow([]);
    }
    let writes = write_cells(ctx.doc);
    if writes > 0 {
        return Decision::Deny(Reason::PublishHasWriteCells { count: writes });
    }
    if ctx.trust.level == TrustLevel::Untrusted {
        return Decision::NeedsApproval(Reason::TrustRequired);
    }
    Decision::allow([])
}

/// Decide whether `action` may happen now.
pub fn authorize(action: &NotebookAction<'_>, ctx: &AuthContext<'_>) -> Decision {
    match action {
        NotebookAction::RunCell { cell_id } => authorize_run(ctx, cell_id),
        NotebookAction::Export(request) => authorize_export(ctx, *request),
        NotebookAction::MaterializeLocally { cell_id } => authorize_materialize(ctx, cell_id),
        NotebookAction::ShareWithAi(share) => authorize_ai(ctx, *share),
        NotebookAction::Schedule { allow_production } => authorize_schedule(ctx, *allow_production),
        NotebookAction::Publish(mode) => authorize_publish(ctx, *mode),
        NotebookAction::Benchmark {
            cell_id,
            runs,
            production_authorized,
        } => authorize_benchmark(ctx, cell_id, *runs, *production_authorized),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notebook::{CellKind, ConnectionRef, SeededIds};
    use crate::notebook_security::trust::tests::now;
    use crate::notebook_security::trust::{Grant, TrustStore, grant};

    struct Fixture {
        doc: NotebookDoc,
        trust: TrustDecision,
        connections: BTreeMap<String, ConnectionFacts>,
        policy: DataPolicy,
        consents: Consents,
    }

    const READ: &str = "c_read";
    const WRITE: &str = "c_write";
    const CHART: &str = "c_chart";
    const NOTE: &str = "c_note";
    const PY: &str = "c_py";

    fn doc() -> NotebookDoc {
        let mut ids = SeededIds(4);
        let mut doc = NotebookDoc::new("Ops", &mut ids);
        doc.cells.clear();
        doc.connection_refs.push(ConnectionRef {
            id: "db".into(),
            profile_id: Some("p-1".into()),
            label: "Orders DB".into(),
            database: None,
            extra: Default::default(),
        });
        let mut add = |id: &str, kind: CellKind, source: &str, read_only: bool| {
            let mut cell = Cell::new(id, kind, Default::default());
            cell.source = source.into();
            cell.execution_policy.read_only = read_only;
            doc.cells.push(cell);
        };
        add(READ, CellKind::Sql, "SELECT * FROM orders", true);
        add(WRITE, CellKind::Sql, "UPDATE orders SET flag = 1", false);
        add(NOTE, CellKind::Markdown, "# hi", true);
        add(PY, CellKind::Python, "print(1)", true);
        add(CHART, CellKind::Chart, "", true);
        doc.cells[4].spec = serde_json::json!({"input": READ});
        doc
    }

    fn facts(env: ConnectionEnvironment) -> ConnectionFacts {
        ConnectionFacts {
            profile_id: Some("p-1".into()),
            engine: Some(DatabaseType::SqlServer),
            environment: env,
            allow_local_materialize: true,
            writes_unlocked: false,
        }
    }

    fn fixture(level: TrustLevel) -> Fixture {
        let doc = doc();
        let mut store = TrustStore::default();
        let trust = match level {
            TrustLevel::Untrusted => TrustDecision::untrusted(&doc),
            TrustLevel::Trusted => store.record_created(&doc, None, now()),
            TrustLevel::Restricted => grant(
                &mut store,
                &doc,
                &Grant {
                    level,
                    connections: ["db".to_string()].into(),
                    allow_writes: false,
                },
                None,
                now(),
            )
            .unwrap(),
        };
        Fixture {
            doc,
            trust,
            connections: [("db".to_string(), facts(ConnectionEnvironment::Dev))].into(),
            policy: DataPolicy::default(),
            consents: Consents::default(),
        }
    }

    impl Fixture {
        fn check(&self, action: NotebookAction<'_>) -> Decision {
            authorize(
                &action,
                &AuthContext {
                    doc: &self.doc,
                    trust: &self.trust,
                    connections: &self.connections,
                    policy: &self.policy,
                    consents: &self.consents,
                },
            )
        }

        fn production(mut self) -> Self {
            self.connections.get_mut("db").unwrap().environment = ConnectionEnvironment::Production;
            self
        }
    }

    fn allow(notices: &[Notice]) -> Decision {
        Decision::Allow {
            notices: notices.to_vec(),
        }
    }

    fn run(id: &str) -> NotebookAction<'_> {
        NotebookAction::RunCell { cell_id: id }
    }

    #[test]
    fn benchmarks_run_read_only_cells_and_ask_on_production() {
        use Decision::{Deny, NeedsApproval};
        let bench = |cell_id, runs, production_authorized| NotebookAction::Benchmark {
            cell_id,
            runs,
            production_authorized,
        };
        let f = fixture(TrustLevel::Trusted);
        assert_eq!(f.check(bench(READ, 6, false)), allow(&[]));
        assert_eq!(
            f.check(bench(READ, 0, false)),
            Deny(Reason::BenchmarkNoRuns)
        );
        assert_eq!(
            f.check(bench(READ, MAX_BENCHMARK_RUNS + 1, true)),
            Deny(Reason::BenchmarkTooManyRuns {
                max: MAX_BENCHMARK_RUNS
            })
        );
        let mut elevated = fixture(TrustLevel::Trusted);
        elevated.consents.elevated_cells.insert(WRITE.into());
        assert_eq!(
            elevated.check(bench(WRITE, 2, true)),
            Deny(Reason::BenchmarkWritesData),
            "even an elevated write cell is never repeated"
        );
        assert_eq!(
            fixture(TrustLevel::Untrusted).check(bench(READ, 2, true)),
            NeedsApproval(Reason::TrustRequired)
        );
        let prod = fixture(TrustLevel::Trusted).production();
        assert_eq!(
            prod.check(bench(READ, 4, false)),
            NeedsApproval(Reason::BenchmarkProduction { slot: "db".into() })
        );
        assert_eq!(
            prod.check(bench(READ, 4, true)),
            allow(&[Notice::Production])
        );
        assert_eq!(
            crate::notebook_security::audit::AuditAction::for_action(&bench(READ, 1, false)),
            crate::notebook_security::audit::AuditAction::Benchmark
        );
    }

    #[test]
    fn run_cell_table() {
        use Decision::{Deny, NeedsApproval};
        use TrustLevel::*;
        let elevated = |mut f: Fixture| {
            f.consents.elevated_cells.insert(WRITE.into());
            f
        };
        let unlocked = |mut f: Fixture| {
            f.connections.get_mut("db").unwrap().writes_unlocked = true;
            f
        };
        let remapped = |mut f: Fixture| {
            f.connections.get_mut("db").unwrap().profile_id = Some("p-other".into());
            f
        };
        let unmapped = |mut f: Fixture| {
            f.connections.get_mut("db").unwrap().profile_id = None;
            f
        };
        type Case = (&'static str, Fixture, &'static str, Decision);
        let cases: Vec<Case> = vec![
            (
                "markdown never needs trust",
                fixture(Untrusted),
                NOTE,
                allow(&[]),
            ),
            (
                "untrusted read",
                fixture(Untrusted),
                READ,
                NeedsApproval(Reason::TrustRequired),
            ),
            (
                "untrusted local cell",
                fixture(Untrusted),
                CHART,
                NeedsApproval(Reason::TrustRequired),
            ),
            (
                "python is reserved",
                fixture(Trusted),
                PY,
                Deny(Reason::UnsupportedKind {
                    kind: "python".into(),
                }),
            ),
            (
                "unknown cell",
                fixture(Trusted),
                "nope",
                Deny(Reason::UnknownCell {
                    cell_id: "nope".into(),
                }),
            ),
            ("trusted read", fixture(Trusted), READ, allow(&[])),
            ("trusted local cell", fixture(Trusted), CHART, allow(&[])),
            ("restricted read", fixture(Restricted), READ, allow(&[])),
            (
                "restricted write",
                fixture(Restricted),
                WRITE,
                Deny(Reason::WritesNotAllowed),
            ),
            (
                "write needs elevation",
                fixture(Trusted),
                WRITE,
                NeedsApproval(Reason::WriteNeedsElevation),
            ),
            (
                "elevated write",
                elevated(fixture(Trusted)),
                WRITE,
                allow(&[]),
            ),
            (
                "production read is labelled",
                fixture(Trusted).production(),
                READ,
                allow(&[Notice::Production]),
            ),
            (
                "production write locked",
                elevated(fixture(Trusted).production()),
                WRITE,
                NeedsApproval(Reason::ProductionWritesLocked),
            ),
            (
                "production write unlocked",
                unlocked(elevated(fixture(Trusted).production())),
                WRITE,
                allow(&[Notice::Production]),
            ),
            (
                "remapped after grant",
                remapped(fixture(Restricted)),
                READ,
                NeedsApproval(Reason::ConnectionNotAuthorized { slot: "db".into() }),
            ),
            (
                "unmapped slot",
                unmapped(fixture(Trusted)),
                READ,
                Deny(Reason::ConnectionUnmapped { slot: "db".into() }),
            ),
        ];
        for (name, fixture, cell, expected) in cases {
            assert_eq!(fixture.check(run(cell)), expected, "{name}");
        }
    }

    #[test]
    fn read_only_cell_that_writes_is_refused_before_the_driver_sees_it() {
        let mut f = fixture(TrustLevel::Trusted);
        f.doc.cell_mut(READ).unwrap().source = "DELETE FROM orders".into();
        assert_eq!(
            f.check(run(READ)),
            Decision::Deny(Reason::ReadOnlyCellChangesData)
        );
    }

    #[test]
    fn unauthorized_slot_needs_approval_and_grant_covers_it() {
        let mut f = fixture(TrustLevel::Trusted);
        f.doc.connection_refs.push(ConnectionRef {
            id: "crm".into(),
            profile_id: Some("p-9".into()),
            label: "CRM".into(),
            database: None,
            extra: Default::default(),
        });
        f.doc.cell_mut(READ).unwrap().connection_ref = Some("crm".into());
        f.connections.insert(
            "crm".into(),
            ConnectionFacts {
                profile_id: Some("p-9".into()),
                ..facts(ConnectionEnvironment::Dev)
            },
        );
        // The edit changed the hash; the stale decision still lacks "crm".
        assert_eq!(
            f.check(run(READ)),
            Decision::NeedsApproval(Reason::ConnectionNotAuthorized { slot: "crm".into() })
        );
    }

    #[test]
    fn diff_connection_sides_need_their_slot_authorized() {
        let mut f = fixture(TrustLevel::Restricted);
        f.doc.connection_refs.push(ConnectionRef {
            id: "prod".into(),
            profile_id: Some("p-9".into()),
            label: "Prod".into(),
            database: None,
            extra: Default::default(),
        });
        f.connections.insert(
            "prod".into(),
            ConnectionFacts {
                profile_id: Some("p-9".into()),
                ..facts(ConnectionEnvironment::Production)
            },
        );
        let mut diff = Cell::new("c_diff", CellKind::Diff, Default::default());
        diff.spec = serde_json::json!({"inputs": [READ, {"cell": READ, "connection_ref": "prod"}]});
        f.doc.cells.push(diff);
        assert_eq!(
            f.check(run("c_diff")),
            Decision::NeedsApproval(Reason::ConnectionNotAuthorized {
                slot: "prod".into()
            })
        );
        f.connections.remove("prod");
        assert_eq!(
            f.check(run("c_diff")),
            Decision::Deny(Reason::ConnectionUnmapped {
                slot: "prod".into()
            })
        );
        let mut trusted = fixture(TrustLevel::Trusted);
        trusted.doc.cells.push(f.doc.cells.last().unwrap().clone());
        trusted.doc.cell_mut("c_diff").unwrap().spec =
            serde_json::json!({"inputs": [READ, {"cell": READ, "connection_ref": "db"}]});
        let trusted = trusted.production();
        assert_eq!(trusted.check(run("c_diff")), allow(&[Notice::Production]));
    }

    #[test]
    fn export_table() {
        use Decision::{Deny, NeedsApproval};
        let req = |includes_data, destination, sensitive_present| {
            NotebookAction::Export(ExportRequest {
                includes_data,
                destination,
                sensitive_present,
            })
        };
        let base = fixture(TrustLevel::Untrusted);
        assert_eq!(
            base.check(req(true, ExportDestination::LocalFile, false)),
            allow(&[])
        );
        assert_eq!(
            base.check(req(true, ExportDestination::LocalFile, true)),
            NeedsApproval(Reason::SensitiveData)
        );
        assert_eq!(
            base.check(req(false, ExportDestination::External, false)),
            NeedsApproval(Reason::ExternalDestination)
        );
        let mut consented = fixture(TrustLevel::Trusted);
        consented.consents.sensitive_export = true;
        consented.consents.external_destination = true;
        assert_eq!(
            consented.check(req(true, ExportDestination::External, true)),
            allow(&[Notice::SensitiveData])
        );
        let mut off = fixture(TrustLevel::Trusted);
        off.policy.export_allowed = false;
        off.policy.external_destinations_allowed = false;
        assert_eq!(
            off.check(req(true, ExportDestination::Clipboard, false)),
            Deny(Reason::ExportDisabled)
        );
        assert_eq!(
            off.check(req(false, ExportDestination::LocalFile, false)),
            allow(&[])
        );
        assert_eq!(
            off.check(req(false, ExportDestination::External, false)),
            Deny(Reason::ExternalDestinationDisabled)
        );
    }

    #[test]
    fn materialize_table_follows_upstream_connections() {
        let f = fixture(TrustLevel::Trusted);
        assert_eq!(
            f.check(NotebookAction::MaterializeLocally { cell_id: CHART }),
            allow(&[])
        );
        let mut off = fixture(TrustLevel::Trusted).production();
        off.connections
            .get_mut("db")
            .unwrap()
            .allow_local_materialize = false;
        let expected = Decision::Deny(Reason::MaterializeNotEnabled { slot: "db".into() });
        assert_eq!(
            off.check(NotebookAction::MaterializeLocally { cell_id: CHART }),
            expected
        );
        assert_eq!(
            off.check(NotebookAction::MaterializeLocally { cell_id: READ }),
            expected
        );
        assert_eq!(
            fixture(TrustLevel::Untrusted)
                .check(NotebookAction::MaterializeLocally { cell_id: READ }),
            Decision::NeedsApproval(Reason::TrustRequired)
        );
        assert_eq!(
            fixture(TrustLevel::Trusted)
                .production()
                .check(NotebookAction::MaterializeLocally { cell_id: READ }),
            allow(&[Notice::Production])
        );
    }

    #[test]
    fn ai_sharing_table() {
        let share = |model, includes_rows, sensitive_present| {
            NotebookAction::ShareWithAi(AiShare {
                model,
                includes_rows,
                sensitive_present,
            })
        };
        let f = fixture(TrustLevel::Trusted);
        assert_eq!(f.check(share(ModelLocation::Local, true, true)), allow(&[]));
        assert_eq!(
            f.check(share(ModelLocation::Cloud, false, false)),
            Decision::NeedsApproval(Reason::CloudModelConsent)
        );
        let mut consent = fixture(TrustLevel::Untrusted);
        consent.consents.cloud_ai = true;
        assert_eq!(
            consent.check(share(ModelLocation::Cloud, false, false)),
            allow(&[Notice::UntrustedContent])
        );
        assert_eq!(
            consent.check(share(ModelLocation::Cloud, true, true)),
            Decision::NeedsApproval(Reason::SensitiveData)
        );
        let mut off = fixture(TrustLevel::Trusted);
        off.policy.cloud_ai_allowed = false;
        off.consents.cloud_ai = true;
        assert_eq!(
            off.check(share(ModelLocation::Cloud, false, false)),
            Decision::Deny(Reason::CloudModelDisabled)
        );
    }

    fn read_only_doc(mut f: Fixture) -> Fixture {
        f.doc.cells.retain(|c| c.id != WRITE);
        f.trust = TrustStore::default().record_created(&f.doc, None, now());
        f
    }

    #[test]
    fn schedule_table() {
        let schedule = |allow_production| NotebookAction::Schedule { allow_production };
        assert_eq!(
            fixture(TrustLevel::Restricted).check(schedule(true)),
            Decision::Deny(Reason::ScheduleNeedsFullTrust)
        );
        assert_eq!(
            fixture(TrustLevel::Trusted).check(schedule(true)),
            Decision::Deny(Reason::ScheduleHasWriteCells { count: 1 })
        );
        let f = read_only_doc(fixture(TrustLevel::Trusted));
        assert_eq!(f.check(schedule(false)), allow(&[]));
        let prod = read_only_doc(fixture(TrustLevel::Trusted).production());
        assert_eq!(
            prod.check(schedule(false)),
            Decision::Deny(Reason::ScheduleProduction { slot: "db".into() })
        );
        assert_eq!(prod.check(schedule(true)), allow(&[Notice::Production]));
        let mut remapped = read_only_doc(fixture(TrustLevel::Trusted));
        remapped.connections.get_mut("db").unwrap().profile_id = Some("p-x".into());
        assert_eq!(
            remapped.check(schedule(false)),
            Decision::Deny(Reason::ConnectionNotAuthorized { slot: "db".into() }),
            "a schedule cannot prompt, so approval turns into denial"
        );
    }

    #[test]
    fn publish_table() {
        let f = fixture(TrustLevel::Trusted);
        for mode in [PublishMode::Present, PublishMode::InteractiveApp] {
            assert_eq!(
                f.check(NotebookAction::Publish(mode)),
                Decision::Deny(Reason::PublishHasWriteCells { count: 1 })
            );
        }
        assert_eq!(
            f.check(NotebookAction::Publish(PublishMode::StaticReport)),
            allow(&[])
        );
        let ro = read_only_doc(fixture(TrustLevel::Trusted));
        assert_eq!(
            ro.check(NotebookAction::Publish(PublishMode::InteractiveApp)),
            allow(&[])
        );
        let mut untrusted = read_only_doc(fixture(TrustLevel::Trusted));
        untrusted.trust = TrustDecision::untrusted(&untrusted.doc);
        assert_eq!(
            untrusted.check(NotebookAction::Publish(PublishMode::Present)),
            Decision::NeedsApproval(Reason::TrustRequired)
        );
    }

    #[test]
    fn decisions_are_evaluated_at_call_time() {
        let mut f = read_only_doc(fixture(TrustLevel::Trusted));
        assert!(f.check(run(READ)).is_allowed());
        f.doc.cell_mut(READ).unwrap().execution_policy.read_only = false;
        assert_eq!(
            f.check(run(READ)),
            Decision::NeedsApproval(Reason::WriteNeedsElevation)
        );
        f.trust = TrustDecision::untrusted(&f.doc);
        assert_eq!(
            f.check(run(READ)),
            Decision::NeedsApproval(Reason::TrustRequired)
        );
    }

    #[test]
    fn every_reason_has_a_security_key_and_fallback() {
        let reasons = [
            Reason::TrustRequired,
            Reason::UnknownCell {
                cell_id: "x".into(),
            },
            Reason::UnsupportedKind { kind: "x".into() },
            Reason::ConnectionUnmapped { slot: "x".into() },
            Reason::ConnectionNotAuthorized { slot: "x".into() },
            Reason::ReadOnlyCellChangesData,
            Reason::WritesNotAllowed,
            Reason::WriteNeedsElevation,
            Reason::ProductionWritesLocked,
            Reason::ExportDisabled,
            Reason::SensitiveData,
            Reason::ExternalDestinationDisabled,
            Reason::ExternalDestination,
            Reason::MaterializeNotEnabled { slot: "x".into() },
            Reason::CloudModelDisabled,
            Reason::CloudModelConsent,
            Reason::ScheduleNeedsFullTrust,
            Reason::ScheduleHasWriteCells { count: 2 },
            Reason::ScheduleProduction { slot: "x".into() },
            Reason::PublishHasWriteCells { count: 2 },
        ];
        let keys: BTreeSet<&str> = reasons.iter().map(Reason::reason_key).collect();
        assert_eq!(keys.len(), reasons.len());
        for reason in &reasons {
            let m = reason.message();
            assert!(m.key().unwrap().starts_with("notebook.security.reason."));
            assert!(!m.fallback().contains('{'), "{}", m.fallback());
        }
    }
}
