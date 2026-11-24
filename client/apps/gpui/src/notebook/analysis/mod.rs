//! Analysis cells (profile, assertion, diff, plan, entity detail): their
//! per-cell views, how a run executes them, and the requests the host runs
//! for the kinds that query a connection.
//!
//! In-process kinds (profile, assertion, diff) run on the background
//! executor against upstream outputs read through [`inputs::cell_output`].
//! Plan and entity detail cells hand an [`AnalysisRequest`] to the host
//! ([`super::view::NotebookEvent::AnalysisQuery`]), which runs it read-only
//! on the cell's connection and answers with [`NotebookView::analysis_reply`].

mod full_values;
pub mod inputs;
mod run;
#[cfg(test)]
mod tests;
pub(crate) mod ui;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use gpui::{AnyView, AppContext as _, Context, Entity, Task, Window};
use sqlly_core::{CellValue, DisplayMessage, ResultColumn};
use sqlly_intel::intellisense::CompletionCatalog;
use sqlly_services::notebook::CellOutput;
use sqlly_services::notebook::cells::assertion::TestSummary;
use sqlly_services::notebook::cells::entity_detail::{EntityDetailSpec, EntitySource};
use sqlly_services::plans::ExecutionPlan;

use super::cells::{assertion, diff, entity_detail, plan, profile};
use super::connection::CellRunTarget;
use super::history::RunHistory;
use super::view::{CellStatus, NotebookView};

pub const KIND_PROFILE: &str = "profile";
pub const KIND_ASSERTION: &str = "assertion";
pub const KIND_DIFF: &str = "diff";
pub const KIND_PLAN: &str = "plan";
pub const KIND_ENTITY_DETAIL: &str = "entity_detail";

pub fn is_analysis_kind(kind: &str) -> bool {
    matches!(
        kind,
        KIND_PROFILE | KIND_ASSERTION | KIND_DIFF | KIND_PLAN | KIND_ENTITY_DETAIL
    )
}

/// Rows a query in an [`AnalysisRequest`] returned.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct QueryRows {
    pub columns: Vec<ResultColumn>,
    pub rows: Vec<Vec<CellValue>>,
    /// More rows existed than the request's cap.
    pub truncated: bool,
    pub elapsed_ms: u64,
}

/// What the host runs for an analysis cell. Every query runs on the cell's
/// connection clone with the cell's read-only policy.
#[derive(Debug, Clone, PartialEq)]
pub enum AnalysisWork {
    /// Independent SELECTs, each capped at `max_rows`.
    Rows {
        queries: Vec<String>,
        max_rows: usize,
    },
    /// A plan capture of `sql`, and of `compare_sql` when set. With
    /// `stats_sql`, the counters are read before and after an actual
    /// capture on the same session.
    Plan {
        sql: String,
        actual: bool,
        dialect: sqlly_services::plans::PlanDialect,
        compare_sql: Option<String>,
        stats_sql: Option<String>,
    },
    /// Each query run in order on one session, timed; with `stats_sql` the
    /// counters are read around every run.
    Sweep {
        runs: Vec<String>,
        stats_sql: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct AnalysisRequest {
    pub cell_id: String,
    pub request_id: u64,
    pub target: CellRunTarget,
    pub work: AnalysisWork,
}

/// A stats snapshot pair around one measured statement.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct StatsPair {
    pub before: QueryRows,
    pub after: QueryRows,
}

#[derive(Debug, Clone)]
/// Failures stay catalog messages (server text raw), worded when drawn.
#[allow(clippy::large_enum_variant)]
pub enum AnalysisReply {
    Rows(Vec<Result<QueryRows, DisplayMessage>>),
    Plan {
        plans: Result<Vec<ExecutionPlan>, DisplayMessage>,
        compare: Option<Result<Vec<ExecutionPlan>, DisplayMessage>>,
        stats: Option<StatsPair>,
    },
    Sweep(Vec<Result<(u64, Option<StatsPair>), DisplayMessage>>),
    /// The host could not start the work at all.
    Failed(DisplayMessage),
}

/// One analysis cell's view entity.
#[derive(Clone)]
pub(crate) enum CellViewHandle {
    Profile(Entity<profile::ProfileView>),
    Assertion(Entity<assertion::AssertionView>),
    Diff(Entity<diff::DiffView>),
    Plan(Entity<plan::PlanView>),
    EntityDetail(Entity<entity_detail::EntityDetailView>),
}

impl CellViewHandle {
    fn any(&self) -> AnyView {
        match self {
            CellViewHandle::Profile(e) => e.clone().into(),
            CellViewHandle::Assertion(e) => e.clone().into(),
            CellViewHandle::Diff(e) => e.clone().into(),
            CellViewHandle::Plan(e) => e.clone().into(),
            CellViewHandle::EntityDetail(e) => e.clone().into(),
        }
    }
}

/// Lets Stop reach the queries an analysis cell's request runs: the host
/// records each query here as it starts and checks it before starting the
/// next one.
#[derive(Clone, Debug, Default)]
pub struct AnalysisCancel(Arc<AnalysisCancelInner>);

#[derive(Debug, Default)]
struct AnalysisCancelInner {
    canceled: std::sync::atomic::AtomicBool,
    queries: std::sync::Mutex<Vec<String>>,
}

impl AnalysisCancel {
    pub fn is_canceled(&self) -> bool {
        self.0.canceled.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Note a query that started. False when the cell was stopped
    /// meanwhile: the caller cancels the query itself.
    pub fn record(&self, task_id: &str) -> bool {
        let mut queries = self.0.queries.lock().unwrap_or_else(|e| e.into_inner());
        if self.is_canceled() {
            return false;
        }
        queries.push(task_id.to_string());
        true
    }

    /// A query that ended on its own.
    pub fn finished(&self, task_id: &str) {
        let mut queries = self.0.queries.lock().unwrap_or_else(|e| e.into_inner());
        queries.retain(|q| q != task_id);
    }

    /// Stop: no further query starts; returns the ones still running.
    fn cancel(&self) -> Vec<String> {
        let mut queries = self.0.queries.lock().unwrap_or_else(|e| e.into_inner());
        self.0
            .canceled
            .store(true, std::sync::atomic::Ordering::SeqCst);
        std::mem::take(&mut *queries)
    }
}

/// Analysis bookkeeping a notebook view keeps.
#[derive(Default)]
pub struct AnalysisState {
    /// Host queries of the analysis requests in flight, by cell.
    cancels: HashMap<String, AnalysisCancel>,
    /// Queries of stopped analysis cells the host has not canceled yet.
    stopped_queries: Vec<String>,
    /// The services each cell's analysis request runs on.
    routes: HashMap<String, sqlly_services::Services>,
    pub(crate) views: HashMap<String, CellViewHandle>,
    /// Background work in flight, by cell; dropping it cancels the work.
    tasks: HashMap<String, Task<()>>,
    /// Host requests in flight: request id → cell id and purpose.
    pending: HashMap<u64, (String, run::Pending)>,
    next_request: u64,
    /// Outputs the analysis cells produced, readable by later cells.
    pub(crate) outputs: HashMap<String, CellOutput>,
    /// Latest test results per assertion cell.
    pub(crate) tests: BTreeMap<String, TestSummary>,
    /// The summary was seeded from the run history (once per tab).
    tests_restored: bool,
    pub(crate) catalog: Option<Arc<CompletionCatalog>>,
    pub(crate) history: RunHistory,
    /// Cells whose next run the user authorized: actual plans (they
    /// execute the query) and sweeps on production. Consumed by that run.
    pub(crate) authorized: HashSet<String>,
    /// SQL results whose cut values were read again whole.
    pub(crate) full_values: full_values::FullValues,
}

impl AnalysisState {
    /// Test results across the notebook.
    pub fn test_summary(&self) -> TestSummary {
        let mut total = TestSummary::default();
        for s in self.tests.values() {
            total.merge(*s);
        }
        total
    }
}

impl NotebookView {
    /// The view entity of an analysis cell, created on first render.
    pub(super) fn analysis_view(
        &mut self,
        id: &str,
        kind: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyView> {
        if !is_analysis_kind(kind) {
            return None;
        }
        if let Some(handle) = self.analysis.views.get(id) {
            return Some(handle.any());
        }
        let notebook = cx.weak_entity();
        let cell = id.to_string();
        let handle = match kind {
            KIND_PROFILE => {
                CellViewHandle::Profile(cx.new(|_| profile::ProfileView::new(notebook, cell)))
            }
            KIND_ASSERTION => {
                CellViewHandle::Assertion(cx.new(|_| assertion::AssertionView::new(notebook, cell)))
            }
            KIND_DIFF => CellViewHandle::Diff(cx.new(|_| diff::DiffView::new(notebook, cell))),
            KIND_PLAN => CellViewHandle::Plan(cx.new(|_| plan::PlanView::new(notebook, cell))),
            _ => CellViewHandle::EntityDetail(
                cx.new(|_| entity_detail::EntityDetailView::new(notebook, cell)),
            ),
        };
        let _ = window;
        let any = handle.any();
        self.analysis.views.insert(id.to_string(), handle);
        Some(any)
    }

    /// The completion catalog the entity detail cell reads foreign keys from.
    pub fn set_analysis_catalog(&mut self, catalog: Arc<CompletionCatalog>) {
        let same = self
            .analysis
            .catalog
            .as_ref()
            .is_some_and(|c| Arc::ptr_eq(c, &catalog));
        if !same {
            self.analysis.catalog = Some(catalog);
        }
    }

    /// Change an analysis cell's spec from its view.
    pub(crate) fn set_cell_spec(
        &mut self,
        id: &str,
        spec: serde_json::Value,
        cx: &mut Context<Self>,
    ) {
        if self.doc.set_spec(id, spec) {
            self.remeasure_cell(id);
            self.mark_dirty(cx);
        }
    }

    /// Cells that can feed an analysis cell: everything before `id` that
    /// has a result, as `(id, label)`.
    pub(crate) fn input_choices(&self, id: &str) -> Vec<(String, String)> {
        let end = self.doc.index_of(id).unwrap_or(self.doc.len());
        self.doc
            .cells()
            .take(end)
            .filter(|c| c.is_sql() || is_analysis_kind(c.kind))
            .map(|c| (c.id.to_string(), c.display_name().to_string()))
            .collect()
    }

    /// The cell id a spec's `input` token names (an id or a cell name).
    pub(crate) fn resolve_cell(&self, token: &str) -> Option<String> {
        if token.is_empty() {
            return None;
        }
        if let Some(c) = self.doc.cell_by_id(token) {
            return Some(c.id.to_string());
        }
        self.doc
            .cells()
            .find(|c| c.name.is_some_and(|n| n.eq_ignore_ascii_case(token)))
            .map(|c| c.id.to_string())
    }

    pub(crate) fn display_name(&self, id: &str) -> String {
        self.doc
            .cell_by_id(id)
            .map(|c| c.display_name().to_string())
            .unwrap_or_else(|| id.to_string())
    }

    fn next_request_id(&mut self, cell: &str, pending: run::Pending) -> u64 {
        self.analysis.next_request += 1;
        let id = self.analysis.next_request;
        self.analysis
            .pending
            .insert(id, (cell.to_string(), pending));
        id
    }

    /// Drop the background work and host requests of `cell` (Stop).
    pub(super) fn cancel_analysis(&mut self, cell: Option<&str>) {
        let Some(cell) = cell else {
            return;
        };
        self.analysis.tasks.remove(cell);
        self.analysis.pending.retain(|_, (c, _)| c != cell);
        let route = self.analysis.routes.remove(cell);
        if let Some(cancel) = self.analysis.cancels.remove(cell) {
            let queries = cancel.cancel();
            if let Some(services) = route {
                for id in &queries {
                    self.route_query(id, services.clone());
                }
            }
            self.analysis.stopped_queries.extend(queries);
        }
    }

    /// The host started `cell`'s analysis request; `cancel` reaches its
    /// queries.
    pub fn track_analysis(&mut self, cell: &str, cancel: AnalysisCancel) {
        self.analysis.cancels.insert(cell.to_string(), cancel);
    }

    /// `cell`'s analysis queries run on `services` (a Via-Relay
    /// connection's relay): Stop cancels them there.
    pub fn route_analysis(&mut self, cell: &str, services: sqlly_services::Services) {
        self.analysis.routes.insert(cell.to_string(), services);
    }

    /// Queries of stopped analysis cells, for the host to cancel.
    pub fn take_analysis_queries(&mut self) -> Vec<String> {
        std::mem::take(&mut self.analysis.stopped_queries)
    }

    /// The notebook-wide test summary for the top bar.
    pub fn test_summary(&self) -> TestSummary {
        self.analysis.test_summary()
    }

    /// A table cell's grid selection moved to `row` (an index into the
    /// rows it shows): entity detail cells reading that table follow it,
    /// and ones that have run fetch the new row's related rows. The row
    /// arrows stay for stepping without the table.
    pub(crate) fn table_row_selected(
        &mut self,
        table: &str,
        row: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        self.outputs.select_row(table, row);
        let Some(row) = row else {
            return;
        };
        let name = self
            .doc
            .cell_by_id(table)
            .map(|c| c.display_name().to_string());
        let followers: Vec<String> = self
            .doc
            .cells()
            .filter(|c| c.kind == KIND_ENTITY_DETAIL)
            .map(|c| c.id.to_string())
            .collect();
        for id in followers {
            let mut spec = EntityDetailSpec::from_spec(&self.doc.spec(&id));
            let EntitySource::Row { cell, row: shown } = &mut spec.source else {
                continue;
            };
            if (cell != table && name.as_deref() != Some(cell.as_str())) || *shown == row {
                continue;
            }
            *shown = row;
            self.set_cell_spec(&id, spec.to_spec(), cx);
            let state = self.cell_state(&id);
            if state != CellStatus::Idle && !state.is_active() {
                self.request_run_from_host(&id, cx);
            }
        }
    }

    /// On open, before any check runs here, show each assertion cell's
    /// results from the newest recorded run that checked it. Waits until
    /// the notebook's data scope is known.
    pub(in crate::notebook) fn restore_test_summary(&mut self) {
        if self.analysis.tests_restored
            || self.trust.scope.is_none()
            || !self.analysis.history.is_enabled()
        {
            return;
        }
        self.analysis.tests_restored = true;
        let runs = self.recorded_runs().to_vec();
        for run in runs.iter().rev() {
            for (cell, value) in super::history::details_of(run).assertions {
                if self.analysis.tests.contains_key(&cell) || self.doc.cell_by_id(&cell).is_none() {
                    continue;
                }
                if let Ok(outcomes) = serde_json::from_value::<
                    Vec<sqlly_services::analytics::assertions::AssertionOutcome>,
                >(value)
                {
                    self.analysis.tests.insert(cell, TestSummary::of(&outcomes));
                }
            }
        }
    }
}
