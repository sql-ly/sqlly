//! Run history: every notebook run in the app (Run All and single-cell
//! runs alike) becomes an immutable run record, with sensitive parameter
//! values left out, each cell's output kept encrypted for later, named
//! snapshots, and run-to-run comparison.
//!
//! The record's `extra.details` is the shared
//! [`sqlly_services::notebook::run_details::RunDetails`], so the History
//! tab lists CLI runs and the CLI reads app runs.

pub mod compare;
pub(crate) mod rail;
pub mod store;

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use gpui::{AppContext as _, Context, Entity};
use serde_json::Value;
use sqlly_datatable::SqllyDataTable;
use sqlly_services::analytics::assertions::AssertionOutcome;
use sqlly_services::notebook::cells::assertion::{AssertionRun, TestSummary};
pub use sqlly_services::notebook::run_details::{
    DETAILS_KEY, OutputSummary, RunDetails, SlotSummary, details_of,
};
use sqlly_services::notebook::run_record::{
    CellRunRecord, RunError, RunRecord, RunTrigger, doc_hash, new_run_id, redact_params, sql_hash,
};
use sqlly_services::notebook::{
    CellFingerprint, CellOutput, CellStatus, ConnectionIdentity, RandomIds,
};
use sqlly_services::plans::ExecutionPlan;

use self::compare::RunComparison;
use self::store::{HistoryStores, SnapshotEntry};
use super::analysis::inputs;
use super::doc_adapter::RunScope;
use super::view::NotebookView;

/// Runs the History tab keeps outputs for; older outputs age out of the
/// encrypted cache on its own schedule.
pub const MAX_LISTED_RUNS: usize = 200;

/// Test results of a run, from its assertion outcomes.
pub fn run_tests(details: &RunDetails) -> TestSummary {
    let mut total = TestSummary::default();
    for value in details.assertions.values() {
        if let Ok(outcomes) = serde_json::from_value::<Vec<AssertionOutcome>>(value.clone()) {
            total.merge(TestSummary::of(&outcomes));
        }
    }
    total
}

fn scope_name(scope: RunScope) -> &'static str {
    match scope {
        RunScope::Cell => "cell",
        RunScope::CellWithUpstream => "cell_with_inputs",
        RunScope::All => "all",
        RunScope::Stale => "stale",
        RunScope::Downstream => "downstream",
        RunScope::Above => "above",
        RunScope::Below => "below",
        RunScope::Selected => "selected",
        RunScope::ValidateOnly => "validate",
    }
}

/// The run being recorded.
pub(crate) struct RunInFlight {
    run_id: String,
    started_at: DateTime<Utc>,
    request: &'static str,
    cells: Vec<CellRunRecord>,
    details: RunDetails,
    outputs: Vec<(String, CellOutput)>,
}

/// A past run opened in the History tab: each cell's stored output.
pub(crate) struct OpenedRun {
    pub run_id: String,
    pub cells: Vec<(String, Option<Entity<SqllyDataTable>>, Option<u64>)>,
}

#[derive(Default)]
pub struct RunHistory {
    /// Opened on first use, so a notebook that never runs never touches
    /// the cache key.
    stores: std::cell::OnceCell<Arc<HistoryStores>>,
    wanted: bool,
    current: Option<RunInFlight>,
    /// Analysis results waiting for their cell's record.
    pending_assertions: BTreeMap<String, AssertionRun>,
    pending_plans: BTreeMap<String, Vec<ExecutionPlan>>,
    pub(crate) runs: Vec<RunRecord>,
    pub(crate) loaded: bool,
    pub(crate) selected: Vec<String>,
    pub(crate) comparison: Option<RunComparison>,
    pub(crate) opened: Option<OpenedRun>,
    pub(crate) error: Option<sqlly_core::DisplayMessage>,
    pub(crate) snapshot_name: Option<Entity<gpui_component::input::InputState>>,
}

impl RunHistory {
    pub fn is_enabled(&self) -> bool {
        self.wanted || self.stores.get().is_some()
    }

    pub(crate) fn stores(&self) -> Option<&Arc<HistoryStores>> {
        if !self.wanted {
            return self.stores.get();
        }
        Some(
            self.stores
                .get_or_init(|| Arc::new(HistoryStores::open_default())),
        )
    }

    pub(crate) fn note_assertions(&mut self, cell: &str, run: AssertionRun) {
        self.pending_assertions.insert(cell.to_string(), run);
    }

    pub(crate) fn note_plans(&mut self, cell: &str, plans: Vec<ExecutionPlan>) {
        self.pending_plans.insert(cell.to_string(), plans);
    }
}

impl NotebookView {
    /// Record this notebook's runs under the app's notebooks folder (the
    /// host turns this on; tests pass their own stores).
    pub fn enable_history(&mut self) {
        self.analysis.history.wanted = true;
    }

    #[cfg(test)]
    pub fn set_history_stores(&mut self, stores: Arc<HistoryStores>) {
        self.analysis.history.error = stores.outputs_unavailable();
        self.analysis.history.stores = std::cell::OnceCell::from(stores);
        self.analysis.history.wanted = false;
        self.analysis.history.loaded = false;
    }

    /// Forget the history stores opened so far; the next use opens the
    /// ones of the notebook's current trust scope.
    pub(super) fn reopen_history(&mut self) {
        let wanted = self.analysis.history.wanted;
        self.analysis.history = RunHistory {
            wanted,
            ..RunHistory::default()
        };
    }

    /// The notebook's recorded runs, oldest first (loaded once, then kept
    /// up to date as runs finish).
    pub fn recorded_runs(&mut self) -> &[RunRecord] {
        let history = &mut self.analysis.history;
        if !history.loaded {
            history.loaded = true;
            if let (Some(stores), Some(scope)) =
                (history.stores().cloned(), self.trust.scope.clone())
            {
                if history.error.is_none() {
                    history.error = stores.outputs_unavailable();
                }
                match stores.list(&scope, self.doc.id()) {
                    Ok(mut runs) => {
                        let skip = runs.len().saturating_sub(MAX_LISTED_RUNS);
                        runs.drain(..skip);
                        history.runs = runs;
                    }
                    Err(e) => history.error = Some(e),
                }
            }
        }
        &self.analysis.history.runs
    }

    pub(super) fn begin_run_record(&mut self, scope: RunScope) {
        self.analysis.history.current = Some(RunInFlight {
            run_id: new_run_id(&mut RandomIds),
            started_at: Utc::now(),
            request: scope_name(scope),
            cells: Vec::new(),
            details: RunDetails::default(),
            outputs: Vec::new(),
        });
    }

    /// Errors recorded so far for the run being recorded.
    #[cfg(test)]
    pub(crate) fn recorded_errors(&self) -> Vec<Option<RunError>> {
        self.analysis
            .history
            .current
            .as_ref()
            .map(|run| run.cells.iter().map(|c| c.error.clone()).collect())
            .unwrap_or_default()
    }

    /// Note a finished cell in the run being recorded.
    pub(super) fn record_cell_run(
        &mut self,
        id: &str,
        ok: bool,
        fingerprint: Option<&CellFingerprint>,
        elapsed_ms: Option<u64>,
        cx: &mut Context<Self>,
    ) {
        let lone = self.analysis.history.current.is_none();
        if lone {
            self.begin_run_record(RunScope::Cell);
        }
        let output = if ok {
            inputs::cell_output(self, id, cx).map(|i| i.output)
        } else {
            None
        };
        let fingerprint = fingerprint
            .cloned()
            .or_else(|| self.doc.cell_fingerprint(id));
        let kind = self
            .doc
            .cell_by_id(id)
            .map(|c| c.kind.to_string())
            .unwrap_or_default();
        let ended = Utc::now();
        let started = elapsed_ms.map(|ms| ended - chrono::Duration::milliseconds(ms as i64));
        let used = self.doc.params_of(id);
        let used: Vec<&str> = used.iter().map(String::as_str).collect();
        let resolved = self.doc.resolved();
        let params = redact_params(&self.doc.model().parameters, &resolved, &used);
        let sql = (kind == super::doc_adapter::KIND_SQL)
            .then(|| {
                let ran = self.doc.sql_for_run(id).ok()?;
                Some(self.doc.history_sql_for_run(id).unwrap_or(ran))
            })
            .flatten();
        let error = (!ok).then(|| {
            let mut error = match self.cells.get(id).and_then(|rt| rt.message.as_ref()) {
                Some(message) => RunError::from(message),
                None => RunError::from(&crate::localization::deferred_text(
                    "notebook.shell.activity.failed",
                )),
            };
            mask_run_error(&mut error);
            error
        });
        let slot = self.doc.effective_connection(id).cloned();
        let identity = (kind == super::doc_adapter::KIND_SQL)
            .then(|| self.doc.identity_for_cell(id))
            .flatten();
        let choice = slot
            .as_ref()
            .map(|s| self.choice_for_ref(s))
            .or_else(|| self.tab_connection.clone());
        let pending_assertions = self.analysis.history.pending_assertions.remove(id);
        let pending_plans = self.analysis.history.pending_plans.remove(id);
        let Some(run) = self.analysis.history.current.as_mut() else {
            return;
        };
        let record = CellRunRecord {
            cell_id: id.to_string(),
            fingerprint: fingerprint
                .as_ref()
                .map(|f| f.value.clone())
                .unwrap_or_else(|| sqlly_services::notebook::Fingerprint(String::new())),
            status: if ok {
                CellStatus::Fresh
            } else {
                CellStatus::Failed
            },
            started_at: started,
            ended_at: Some(ended),
            row_count: output.as_ref().map(|o| o.row_count),
            completeness: output.as_ref().map(|o| o.completeness),
            reused: false,
            sql_hash: sql.as_deref().map(sql_hash),
            error,
            params,
        };
        run.cells.retain(|c| c.cell_id != id);
        run.cells.push(record);
        if let Some(f) = fingerprint {
            run.details.fingerprints.insert(id.to_string(), f.parts);
        }
        if let Some(choice) = choice {
            run.details.connections.insert(
                slot.as_ref().map(|s| s.id.clone()).unwrap_or_default(),
                SlotSummary {
                    label: choice.label.clone(),
                    engine: choice.database_type.display_name().to_string(),
                    environment: Some(format!("{:?}", choice.environment).to_ascii_lowercase()),
                    profile_id: Some(choice.profile_id.clone()),
                    database: slot
                        .as_ref()
                        .and_then(|s| s.database.clone())
                        .or(choice.database),
                },
            );
        }
        if let Some(identity) = identity {
            run.details.identities.insert(id.to_string(), identity);
        }
        if let Some(a) = pending_assertions {
            run.details.assertions.insert(
                id.to_string(),
                serde_json::to_value(&a.outcomes).unwrap_or(Value::Null),
            );
            if !a.samples.is_empty() {
                run.details
                    .assertion_samples
                    .insert(id.to_string(), a.samples);
            }
        }
        if let Some(p) = pending_plans {
            run.details.plans.insert(id.to_string(), p);
        }
        if let Some(output) = output {
            run.details
                .outputs
                .insert(id.to_string(), OutputSummary::of(&output));
            run.outputs.retain(|(c, _)| c != id);
            run.outputs.push((id.to_string(), output));
        }
        if lone {
            self.finish_run_record(cx);
        }
    }

    /// Write the run being recorded, if any cell ran.
    pub(super) fn finish_run_record(&mut self, cx: &mut Context<Self>) {
        let Some(mut run) = self.analysis.history.current.take() else {
            return;
        };
        if run.cells.is_empty() {
            return;
        }
        let model = self.doc.model();
        let all: Vec<&str> = model.parameters.iter().map(|p| p.name.as_str()).collect();
        run.details.parameters = redact_params(&model.parameters, &self.doc.resolved(), &all);
        run.details.notebook_title = model.title.clone();
        run.details.notebook_file = self
            .file_path
            .as_deref()
            .and_then(|p| std::path::Path::new(p).file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        run.details.request = run.request.to_string();
        let tests = run_tests(&run.details);
        let mut extra = BTreeMap::new();
        extra.insert(
            DETAILS_KEY.to_string(),
            serde_json::to_value(&run.details).unwrap_or(Value::Null),
        );
        let identity_label = self
            .tab_connection
            .as_ref()
            .map(|c| c.label.clone())
            .unwrap_or_default();
        let record = RunRecord {
            run_id: run.run_id.clone(),
            notebook_id: model.notebook_id.clone(),
            doc_hash: doc_hash(model),
            started_at: run.started_at,
            ended_at: Some(Utc::now()),
            trigger: RunTrigger::Manual,
            identity_label,
            engine_version: format!("sqlly {}", env!("CARGO_PKG_VERSION")),
            cells: run.cells,
            extra,
        };
        if tests.total() > 0 {
            let message = if tests.failing() > 0 {
                crate::localization::format(
                    if tests.failing() == 1 {
                        "notebook.analysis.tests.run_failing_one"
                    } else {
                        "notebook.analysis.tests.run_failing"
                    },
                    &[("count", tests.failing().to_string())],
                )
            } else {
                crate::localization::format(
                    if tests.total() == 1 {
                        "notebook.analysis.tests.run_passing_one"
                    } else {
                        "notebook.analysis.tests.run_passing"
                    },
                    &[("count", tests.total().to_string())],
                )
            };
            self.log(None, message.clone(), tests.failing() > 0, None);
            self.status(message, tests.failing() > 0, cx);
        }
        let (Some(stores), Some(scope)) = (
            self.analysis.history.stores().cloned(),
            self.trust.scope.clone(),
        ) else {
            return;
        };
        if self.analysis.history.loaded {
            self.analysis.history.runs.push(record.clone());
        }
        let identities = details_of(&record).identities;
        let outputs = std::mem::take(&mut run.outputs);
        let task = cx.background_spawn(async move {
            stores.append(&scope, &record)?;
            for (cell, output) in &outputs {
                // Outputs are a convenience: a missing cache key keeps the
                // record and skips them.
                let _ = stores.put_run_output(
                    &scope,
                    identities.get(cell),
                    &record.run_id,
                    cell,
                    output,
                );
            }
            Ok::<(), store::StoreError>(())
        });
        cx.spawn(async move |this, cx| {
            if let Err(error) = task.await {
                this.update(cx, |this, cx| {
                    this.log(None, crate::localization::message(&error), true, None);
                    this.analysis.history.error = Some(error);
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    /// The newest recorded output of `cell` before the run in flight.
    /// Whether rows recorded under `recorded` may be shown now: only to the
    /// same principal (profile, login and safety flags) the cell runs as
    /// today, never because the record names them.
    pub(in crate::notebook) fn readable_as_now(
        &self,
        cell: &str,
        recorded: Option<&ConnectionIdentity>,
    ) -> bool {
        match recorded {
            None => true,
            Some(recorded) => self.doc.identity_for_cell(cell).as_ref() == Some(recorded),
        }
    }

    pub(crate) fn previous_output(&mut self, cell: &str) -> Option<CellOutput> {
        let scope = self.trust.scope.clone()?;
        let runs = self.recorded_runs().to_vec();
        let stores = self.analysis.history.stores().cloned()?;
        runs.iter().rev().find_map(|run| {
            let rec = run
                .cells
                .iter()
                .find(|c| c.cell_id == cell && c.status == CellStatus::Fresh)?;
            let identity = details_of(run).identities.get(&rec.cell_id).cloned();
            if !self.readable_as_now(cell, identity.as_ref()) {
                return None;
            }
            stores.run_output(&scope, identity.as_ref(), &run.run_id, cell)
        })
    }

    pub(crate) fn recorded_output(&mut self, run_id: &str, cell: &str) -> Option<CellOutput> {
        let scope = self.trust.scope.clone()?;
        let run = self
            .recorded_runs()
            .iter()
            .find(|r| r.run_id == run_id)
            .cloned()?;
        let identity = details_of(&run).identities.get(cell).cloned();
        if !self.readable_as_now(cell, identity.as_ref()) {
            return None;
        }
        self.analysis
            .history
            .stores()
            .cloned()?
            .run_output(&scope, identity.as_ref(), run_id, cell)
    }

    pub(crate) fn snapshot_output(&self, name: &str) -> Option<CellOutput> {
        let scope = self.trust.scope.as_deref()?;
        let (entry, output) = self
            .analysis
            .history
            .stores()?
            .snapshot_output(scope, name)?;
        self.readable_as_now(&entry.cell_id, entry.identity.as_ref())
            .then_some(output)
    }

    pub(crate) fn snapshot_names(&self) -> Vec<String> {
        let Some(scope) = self.trust.scope.as_deref() else {
            return Vec::new();
        };
        self.analysis
            .history
            .stores()
            .map(|s| s.snapshots(scope).into_iter().map(|e| e.name).collect())
            .unwrap_or_default()
    }

    /// Keep `cell`'s current output as snapshot `name`.
    pub fn pin_snapshot(&mut self, cell: &str, name: &str, cx: &mut Context<Self>) -> bool {
        let name = name.trim();
        let Some(stores) = self.analysis.history.stores().cloned() else {
            return false;
        };
        let Some(input) = inputs::cell_output(self, cell, cx) else {
            self.status(
                crate::localization::text("notebook.analysis.history.pin_no_output"),
                true,
                cx,
            );
            return false;
        };
        if name.is_empty() {
            return false;
        }
        let is_sql = self.doc.cell_by_id(cell).is_some_and(|c| c.is_sql());
        let entry = SnapshotEntry {
            name: name.to_string(),
            cell_id: cell.to_string(),
            run_id: input.output.run_id.clone(),
            pinned_at: Utc::now(),
            row_count: input.output.row_count,
            identity: is_sql.then(|| self.doc.identity_for_cell(cell)).flatten(),
        };
        let Some(scope) = self.trust.scope.clone() else {
            return false;
        };
        let result = stores.pin(
            &scope,
            entry,
            &input.output.snapshot(input.output.rows.len()),
        );
        let (message, error) = match result {
            Ok(()) => (
                crate::localization::format(
                    "notebook.analysis.history.pinned",
                    &[("name", name), ("cell", &self.display_name(cell))],
                ),
                false,
            ),
            Err(e) => (crate::localization::message(&e), true),
        };
        self.log(Some(cell), message.clone(), error, None);
        self.status(message, error, cx);
        !error
    }

    pub(crate) fn toggle_run_selected(&mut self, run_id: &str, cx: &mut Context<Self>) {
        let selected = &mut self.analysis.history.selected;
        if let Some(at) = selected.iter().position(|r| r == run_id) {
            selected.remove(at);
        } else {
            selected.push(run_id.to_string());
            if selected.len() > 2 {
                selected.remove(0);
            }
        }
        cx.notify();
    }

    /// Compare the two selected runs (older first).
    pub fn compare_selected_runs(&mut self, cx: &mut Context<Self>) {
        let selected = self.analysis.history.selected.clone();
        let runs = self.recorded_runs().to_vec();
        let mut picked: Vec<&RunRecord> = runs
            .iter()
            .filter(|r| selected.contains(&r.run_id))
            .collect();
        if picked.len() != 2 {
            return;
        }
        picked.sort_by_key(|r| r.started_at);
        let upstream = |cell: &str| self.doc.dependencies(cell);
        let comparison = compare::compare_runs(picked[0], picked[1], &upstream);
        self.analysis.history.opened = None;
        self.analysis.history.comparison = Some(comparison);
        cx.notify();
    }

    /// Show a past run's stored outputs.
    pub fn open_run(&mut self, run_id: &str, cx: &mut Context<Self>) {
        let Some(run) = self
            .recorded_runs()
            .iter()
            .find(|r| r.run_id == run_id)
            .cloned()
        else {
            return;
        };
        let mut cells = Vec::new();
        for cell in &run.cells {
            let grid = self.recorded_output(run_id, &cell.cell_id).and_then(|o| {
                crate::tool_dialog::build_result_grid(
                    &crate::tool_dialog::ToolQuerySet {
                        columns: o.columns.clone(),
                        rows: o.rows.as_ref().clone(),
                    },
                    cx,
                )
            });
            cells.push((cell.cell_id.clone(), grid, cell.row_count));
        }
        self.analysis.history.comparison = None;
        self.analysis.history.opened = Some(OpenedRun {
            run_id: run_id.to_string(),
            cells,
        });
        cx.notify();
    }

    pub(crate) fn close_history_panel(&mut self, cx: &mut Context<Self>) {
        self.analysis.history.comparison = None;
        self.analysis.history.opened = None;
        cx.notify();
    }

    pub(crate) fn ensure_snapshot_input(
        &mut self,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) -> Entity<gpui_component::input::InputState> {
        if let Some(input) = &self.analysis.history.snapshot_name {
            return input.clone();
        }
        let input = cx.new(|cx| {
            gpui_component::input::InputState::new(window, cx).placeholder(
                crate::localization::input_placeholder(
                    window,
                    cx,
                    "notebook.analysis.history.snapshot_name_placeholder",
                ),
            )
        });
        self.analysis.history.snapshot_name = Some(input.clone());
        input
    }
}

/// Mask secrets in a recorded error's text and arguments.
fn mask_run_error(error: &mut RunError) {
    let mask = |text: &str| sqlly_services::notebook_security::redact::mask_secrets(text).0;
    error.message = mask(&error.message);
    for value in error.args.values_mut() {
        *value = mask(value);
    }
    for nested in error.nested.values_mut() {
        mask_run_error(nested);
    }
}
