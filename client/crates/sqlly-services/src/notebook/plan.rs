//! The run planner: a [`RunRequest`] becomes a [`RunPlan`] of ordered steps
//! (dependencies first, ties in document order), each marked execute,
//! instant, reuse or validate, with its execution lane and the steps it
//! waits for. Cells that cannot run are listed in `blocked` with the reason;
//! anything depending on them is blocked too.
//!
//! Concurrency rules ([`Schedule`]): independent reads may run at once, up
//! to a limit per connection slot; cells sharing a `session_group` run one
//! at a time, in document order, on one pinned session; a write cell runs
//! alone on its connection slot (after everything before it there, before
//! everything after it). Markdown, parameter and reused steps are instant.

use std::collections::{BTreeMap, BTreeSet};

use sqlly_lang::DisplayMessage;

use super::document::{CachePolicy, Cell, CellKind, ExecClass, NotebookDoc};
use super::fingerprint::{CellFingerprint, Fingerprint};
use super::graph::{EdgeKind, Graph, Node, Unresolved};
use super::params::{ParamError, ResolvedParams};
use super::state::{CellStatus, NotebookState};

/// What the user asked to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunRequest {
    /// One cell; with `include_stale_upstream`, also every upstream cell
    /// whose output is not current.
    Cell {
        id: String,
        include_stale_upstream: bool,
    },
    All,
    /// Every cell whose output is not current.
    Stale,
    /// The cell and everything that depends on it.
    Downstream(String),
    /// Every cell above this one (not including it).
    Above(String),
    /// This cell and every cell below it.
    Below(String),
    Selected(Vec<String>),
    /// Plan `inner` but run nothing.
    DryRun(Box<RunRequest>),
    /// Check every cell; run nothing.
    ValidateOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReuseReason {
    /// The current output matches the fingerprint.
    Fresh,
    /// The current output came from the cache and matches.
    Cached,
    /// Pinned by the user; kept on bulk runs.
    Pinned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepAction {
    Execute,
    /// Nothing to execute (Markdown, parameter, layout).
    Instant,
    Reuse(ReuseReason),
    Validate,
}

/// Where a step executes.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Lane {
    Instant,
    /// In-process over upstream outputs.
    Local,
    /// On a connection slot (`None`: the host's current connection).
    Connection {
        slot: Option<String>,
    },
    /// Serially on one pinned session of a slot.
    Session {
        slot: Option<String>,
        group: String,
    },
}

impl Lane {
    fn slot(&self) -> Option<&Option<String>> {
        match self {
            Lane::Connection { slot } | Lane::Session { slot, .. } => Some(slot),
            Lane::Instant | Lane::Local => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanStep {
    pub cell_id: String,
    pub action: StepAction,
    pub lane: Lane,
    /// Indices of steps that must finish first.
    pub depends_on: Vec<usize>,
    pub fingerprint: Option<Fingerprint>,
    /// A write cell: the host must run its elevation / mutation review.
    pub requires_approval: bool,
    /// Named by the request itself (not pulled in as upstream/downstream).
    pub explicit: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockReason {
    UnknownCell,
    /// The cell is on, or depends on, a cycle (closed path).
    Cycle {
        path: Vec<Node>,
    },
    Unresolved(Unresolved),
    Parameter {
        name: String,
        error: ParamError,
    },
    UpstreamBlocked {
        cell: String,
    },
    /// An upstream output the cell reads is missing or stale and the request
    /// does not run it.
    UpstreamNotCurrent {
        cell: String,
    },
    /// Python or a kind from a newer SQLLY.
    UnsupportedKind {
        kind: String,
    },
}

impl BlockReason {
    pub fn message(&self) -> DisplayMessage {
        match self {
            BlockReason::UnknownCell => DisplayMessage::new(
                "notebook.core.plan.unknown_cell",
                "This cell no longer exists",
            ),
            BlockReason::Cycle { path } => {
                let path = path
                    .iter()
                    .map(Node::to_string)
                    .collect::<Vec<_>>()
                    .join(" → ");
                DisplayMessage::new(
                    "notebook.core.cycle.loop",
                    format!("These cells depend on each other in a loop: {path}"),
                )
                .with_arg("path", path)
            }
            BlockReason::Unresolved(u) => u.message(),
            BlockReason::Parameter { name, error } => {
                let inner = error.message();
                DisplayMessage::new(
                    "notebook.core.plan.parameter",
                    format!("Parameter {name}: {}", inner.fallback()),
                )
                .with_arg("name", name.clone())
                .with_message_arg("error", inner)
            }
            BlockReason::UpstreamBlocked { cell } => DisplayMessage::new(
                "notebook.core.plan.upstream_blocked",
                format!("Waiting on cell {cell}, which cannot run"),
            )
            .with_arg("cell", cell.clone()),
            BlockReason::UpstreamNotCurrent { cell } => DisplayMessage::new(
                "notebook.core.plan.upstream_not_current",
                format!("Cell {cell} has no current result; run it first or run with its inputs"),
            )
            .with_arg("cell", cell.clone()),
            BlockReason::UnsupportedKind { kind } => DisplayMessage::new(
                "notebook.core.plan.unsupported_kind",
                format!("This version of SQLLY cannot run {kind} cells"),
            )
            .with_arg("kind", kind.clone()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockedStep {
    pub cell_id: String,
    pub reason: BlockReason,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RunPlan {
    pub steps: Vec<PlanStep>,
    pub blocked: Vec<BlockedStep>,
    pub dry_run: bool,
    pub validate_only: bool,
}

impl RunPlan {
    /// Steps that will touch a database or compute something.
    pub fn executing(&self) -> impl Iterator<Item = &PlanStep> {
        self.steps
            .iter()
            .filter(|s| s.action == StepAction::Execute)
    }

    pub fn step(&self, cell_id: &str) -> Option<&PlanStep> {
        self.steps.iter().find(|s| s.cell_id == cell_id)
    }

    /// Indices of every step that (transitively) waits for `step`.
    pub fn dependents_of(&self, step: usize) -> Vec<usize> {
        let mut out: BTreeSet<usize> = BTreeSet::new();
        let mut frontier = vec![step];
        while let Some(s) = frontier.pop() {
            for (i, other) in self.steps.iter().enumerate() {
                if other.depends_on.contains(&s) && out.insert(i) {
                    frontier.push(i);
                }
            }
        }
        out.into_iter().collect()
    }
}

/// What the planner reads.
#[derive(Debug, Clone, Copy)]
pub struct PlanContext<'a> {
    pub doc: &'a NotebookDoc,
    pub graph: &'a Graph,
    pub fingerprints: &'a BTreeMap<String, CellFingerprint>,
    pub state: &'a NotebookState,
    pub params: &'a ResolvedParams,
    /// Unix milliseconds, for TTL checks.
    pub now_ms: i64,
}

impl PlanContext<'_> {
    fn current(&self, cell: &Cell) -> bool {
        let state = self.state.get(&cell.id);
        let Some(fp) = self.fingerprints.get(&cell.id) else {
            return false;
        };
        if !state.is_current_for(fp) {
            return false;
        }
        match cell.execution_policy.cache {
            CachePolicy::Ttl { seconds } => state.output.as_ref().is_some_and(|o| {
                self.now_ms.saturating_sub(o.finished_at_ms)
                    <= (seconds as i64).saturating_mul(1000)
            }),
            _ => true,
        }
    }

    fn reuse_reason(&self, cell: &Cell) -> ReuseReason {
        if cell.execution_policy.cache == CachePolicy::Pinned {
            ReuseReason::Pinned
        } else if self.state.get(&cell.id).status == CellStatus::Cached {
            ReuseReason::Cached
        } else {
            ReuseReason::Fresh
        }
    }
}

fn lane_for(doc: &NotebookDoc, cell: &Cell) -> Lane {
    match cell.exec_class() {
        ExecClass::Instant | ExecClass::Unsupported => Lane::Instant,
        ExecClass::Local => Lane::Local,
        ExecClass::Remote => {
            let slot = doc.effective_connection_ref(cell).map(str::to_string);
            match &cell.session_group {
                Some(group) => Lane::Session {
                    slot,
                    group: group.clone(),
                },
                None => Lane::Connection { slot },
            }
        }
    }
}

/// Whether `cell` needs `upstream`'s output (not just its SQL text).
fn needs_output(doc: &NotebookDoc, cell: &Cell, upstream: &Cell, kind: EdgeKind) -> bool {
    // A SQL cell on the notebook's local data copies its SQL inputs in
    // itself (re-running their queries when needed).
    if kind == EdgeKind::Sql
        && upstream.kind == CellKind::Sql
        && super::cells::local::targets_local(cell)
    {
        return false;
    }
    kind.needs_output()
        || (kind == EdgeKind::Sql
            && (upstream.kind != CellKind::Sql
                || doc.effective_connection_ref(upstream) != doc.effective_connection_ref(cell)))
}

/// Plan `request`.
pub fn plan(request: &RunRequest, ctx: &PlanContext<'_>) -> RunPlan {
    let doc = ctx.doc;
    let mut dry_run = false;
    let mut request = request;
    while let RunRequest::DryRun(inner) = request {
        dry_run = true;
        request = inner;
    }
    let mut out = RunPlan {
        dry_run,
        ..RunPlan::default()
    };
    let index_of = |id: &str| doc.cell_index(id);
    let all: Vec<&Cell> = doc.cells.iter().collect();
    let (included, explicit): (Vec<&Cell>, BTreeSet<String>) = match request {
        RunRequest::ValidateOnly => {
            out.validate_only = true;
            (all.clone(), BTreeSet::new())
        }
        RunRequest::All => (all.clone(), all.iter().map(|c| c.id.clone()).collect()),
        RunRequest::Stale => (
            all.iter().copied().filter(|c| !ctx.current(c)).collect(),
            BTreeSet::new(),
        ),
        RunRequest::Cell {
            id,
            include_stale_upstream,
        } => match doc.cell(id) {
            None => {
                out.blocked.push(BlockedStep {
                    cell_id: id.clone(),
                    reason: BlockReason::UnknownCell,
                });
                return out;
            }
            Some(cell) => {
                let mut set = vec![cell];
                if *include_stale_upstream {
                    set.extend(
                        ctx.graph
                            .upstream_closure(&Node::cell(id.as_str()))
                            .iter()
                            .filter_map(|n| n.as_cell().and_then(|i| doc.cell(i))),
                    );
                }
                (set, BTreeSet::from([id.clone()]))
            }
        },
        RunRequest::Downstream(id) | RunRequest::Above(id) | RunRequest::Below(id)
            if index_of(id).is_none() =>
        {
            out.blocked.push(BlockedStep {
                cell_id: id.clone(),
                reason: BlockReason::UnknownCell,
            });
            return out;
        }
        RunRequest::Downstream(id) => {
            let mut set = vec![doc.cell(id).expect("checked")];
            set.extend(
                ctx.graph
                    .downstream_closure(&Node::cell(id.as_str()))
                    .iter()
                    .filter_map(|n| n.as_cell().and_then(|i| doc.cell(i))),
            );
            (set, BTreeSet::from([id.clone()]))
        }
        RunRequest::Above(id) => {
            let at = index_of(id).expect("checked");
            let set: Vec<&Cell> = doc.cells[..at].iter().collect();
            let names = set.iter().map(|c| c.id.clone()).collect();
            (set, names)
        }
        RunRequest::Below(id) => {
            let at = index_of(id).expect("checked");
            let set: Vec<&Cell> = doc.cells[at..].iter().collect();
            let names = set.iter().map(|c| c.id.clone()).collect();
            (set, names)
        }
        RunRequest::Selected(ids) => {
            for id in ids.iter().filter(|id| doc.cell(id).is_none()) {
                out.blocked.push(BlockedStep {
                    cell_id: id.clone(),
                    reason: BlockReason::UnknownCell,
                });
            }
            let set: Vec<&Cell> = ids.iter().filter_map(|id| doc.cell(id)).collect();
            (set, ids.iter().cloned().collect())
        }
        RunRequest::DryRun(_) => unreachable!("unwrapped above"),
    };

    let ordered: Vec<&Cell> = ctx
        .graph
        .in_order(included.iter().map(|c| Node::cell(c.id.as_str())))
        .iter()
        .filter_map(|n| n.as_cell().and_then(|id| doc.cell(id)))
        .collect();
    let in_plan: BTreeSet<&str> = ordered.iter().map(|c| c.id.as_str()).collect();
    let mut step_of: BTreeMap<&str, usize> = BTreeMap::new();
    let mut blocked_ids: BTreeSet<&str> = BTreeSet::new();
    // Per connection slot: the last write step and the steps since it.
    let mut barriers: BTreeMap<Option<String>, (Option<usize>, Vec<usize>)> = BTreeMap::new();
    let mut sessions: BTreeMap<(Option<String>, String), usize> = BTreeMap::new();

    for cell in ordered {
        let node = Node::cell(cell.id.as_str());
        if out.validate_only {
            let reason = blocker(ctx, cell, &node);
            if let Some(reason) = reason {
                push_block(&mut out, &mut blocked_ids, cell, reason);
            }
            out.steps.push(PlanStep {
                cell_id: cell.id.clone(),
                action: StepAction::Validate,
                lane: Lane::Instant,
                depends_on: Vec::new(),
                fingerprint: ctx.fingerprints.get(&cell.id).map(|f| f.value.clone()),
                requires_approval: false,
                explicit: false,
            });
            continue;
        }
        if let Some(reason) = blocker(ctx, cell, &node) {
            push_block(&mut out, &mut blocked_ids, cell, reason);
            continue;
        }
        let mut depends_on: BTreeSet<usize> = BTreeSet::new();
        let mut reason: Option<BlockReason> = None;
        for (up, kind) in ctx.graph.upstream(&node) {
            let Some(up_cell) = up.as_cell().and_then(|id| doc.cell(id)) else {
                continue;
            };
            if blocked_ids.contains(up_cell.id.as_str()) {
                reason = Some(BlockReason::UpstreamBlocked {
                    cell: up_cell.id.clone(),
                });
                break;
            }
            if let Some(&step) = step_of.get(up_cell.id.as_str()) {
                depends_on.insert(step);
            } else if !in_plan.contains(up_cell.id.as_str())
                && needs_output(doc, cell, up_cell, *kind)
                && !ctx.current(up_cell)
            {
                reason = Some(BlockReason::UpstreamNotCurrent {
                    cell: up_cell.id.clone(),
                });
                break;
            }
        }
        if let Some(reason) = reason {
            push_block(&mut out, &mut blocked_ids, cell, reason);
            continue;
        }
        let class = cell.exec_class();
        let is_explicit = explicit.contains(&cell.id);
        let pinned_bulk = cell.execution_policy.cache == CachePolicy::Pinned
            && !matches!(request, RunRequest::Cell { .. } | RunRequest::Selected(_));
        let action = if class == ExecClass::Instant {
            StepAction::Instant
        } else if (!is_explicit || pinned_bulk)
            && cell.execution_policy.cache != CachePolicy::Off
            && ctx.current(cell)
        {
            StepAction::Reuse(ctx.reuse_reason(cell))
        } else {
            StepAction::Execute
        };
        let lane = lane_for(doc, cell);
        let index = out.steps.len();
        let write = class == ExecClass::Remote && !cell.execution_policy.read_only;
        if action == StepAction::Execute
            && let Some(slot) = lane.slot()
        {
            let entry = barriers.entry(slot.clone()).or_default();
            if let Some(last) = entry.0 {
                depends_on.insert(last);
            }
            if write {
                depends_on.extend(entry.1.drain(..));
                entry.0 = Some(index);
            } else {
                entry.1.push(index);
            }
            if let Lane::Session { slot, group } = &lane
                && let Some(previous) = sessions.insert((slot.clone(), group.clone()), index)
            {
                depends_on.insert(previous);
            }
        }
        step_of.insert(cell.id.as_str(), index);
        out.steps.push(PlanStep {
            cell_id: cell.id.clone(),
            action,
            lane,
            depends_on: depends_on.into_iter().collect(),
            fingerprint: ctx.fingerprints.get(&cell.id).map(|f| f.value.clone()),
            requires_approval: write && action == StepAction::Execute,
            explicit: is_explicit,
        });
    }
    out
}

fn push_block<'d>(
    out: &mut RunPlan,
    blocked: &mut BTreeSet<&'d str>,
    cell: &'d Cell,
    reason: BlockReason,
) {
    blocked.insert(cell.id.as_str());
    out.blocked.push(BlockedStep {
        cell_id: cell.id.clone(),
        reason,
    });
}

/// Why `cell` cannot run at all, regardless of the request.
fn blocker(ctx: &PlanContext<'_>, cell: &Cell, node: &Node) -> Option<BlockReason> {
    if ctx.graph.cyclic().contains(node) {
        let closure: BTreeSet<Node> = ctx.graph.upstream_closure(node).into_iter().collect();
        let cycle = ctx
            .graph
            .cycles()
            .iter()
            .find(|c| c.path.contains(node))
            .or_else(|| {
                ctx.graph
                    .cycles()
                    .iter()
                    .find(|c| c.path.iter().any(|n| closure.contains(n)))
            });
        return Some(BlockReason::Cycle {
            path: cycle.map(|c| c.path.clone()).unwrap_or_default(),
        });
    }
    if let Some(u) = ctx.graph.unresolved().iter().find(|u| u.owner() == *node) {
        return Some(BlockReason::Unresolved(u.clone()));
    }
    for name in ctx.graph.params_of(&cell.id) {
        if let Some(error) = ctx.params.error(name) {
            return Some(BlockReason::Parameter {
                name: name.to_string(),
                error: error.clone(),
            });
        }
    }
    if cell.kind.exec_class() == ExecClass::Unsupported {
        return Some(BlockReason::UnsupportedKind {
            kind: cell.kind.as_str().to_string(),
        });
    }
    None
}

/// What [`merge_plan`] changed in the run it merged into.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MergeOutcome {
    /// Steps added, or finished steps put back to pending.
    pub queued: Vec<usize>,
    /// Running steps the new plan executes again: re-run each once it
    /// finishes ([`Schedule::requeue`]).
    pub rerun: Vec<usize>,
}

/// Fold `next` into `plan`, a run a [`Schedule`] is driving with `status`.
/// Steps the run does not have are added; finished, failed or skipped
/// steps the new plan names start over; running steps it executes are
/// returned for a re-run. Every added or restarted step waits for what the
/// new plan says it depends on, and for the write barriers and session
/// lanes of the run's steps still pending or running on its connection.
pub fn merge_plan(plan: &mut RunPlan, status: &mut Vec<StepStatus>, next: RunPlan) -> MergeOutcome {
    status.resize(plan.steps.len(), StepStatus::Pending);
    let mut out = MergeOutcome::default();
    let mut index_of: Vec<usize> = Vec::with_capacity(next.steps.len());
    let mut from_next: BTreeSet<usize> = BTreeSet::new();
    for step in next.steps {
        let mapped: Vec<usize> = step
            .depends_on
            .iter()
            .filter_map(|d| index_of.get(*d).copied())
            .collect();
        let Some(existing) = plan.steps.iter().position(|s| s.cell_id == step.cell_id) else {
            let index = plan.steps.len();
            index_of.push(index);
            from_next.insert(index);
            plan.steps.push(PlanStep {
                depends_on: mapped,
                ..step
            });
            status.push(StepStatus::Pending);
            out.queued.push(index);
            continue;
        };
        index_of.push(existing);
        from_next.insert(existing);
        let blocked_by_cycle = |plan: &RunPlan, d: usize| plan.dependents_of(existing).contains(&d);
        match status[existing] {
            StepStatus::Running => {
                for d in mapped {
                    if d != existing
                        && !blocked_by_cycle(plan, d)
                        && !plan.steps[existing].depends_on.contains(&d)
                    {
                        plan.steps[existing].depends_on.push(d);
                    }
                }
                if step.action == StepAction::Execute {
                    let current = &mut plan.steps[existing];
                    current.action = StepAction::Execute;
                    current.fingerprint = step.fingerprint;
                    current.requires_approval = step.requires_approval;
                    out.rerun.push(existing);
                }
            }
            StepStatus::Pending => {
                for d in mapped {
                    if d != existing
                        && !blocked_by_cycle(plan, d)
                        && !plan.steps[existing].depends_on.contains(&d)
                    {
                        plan.steps[existing].depends_on.push(d);
                    }
                }
                if step.action == StepAction::Execute
                    && plan.steps[existing].action != StepAction::Execute
                {
                    let explicit = plan.steps[existing].explicit || step.explicit;
                    let depends_on = std::mem::take(&mut plan.steps[existing].depends_on);
                    plan.steps[existing] = PlanStep {
                        depends_on,
                        explicit,
                        ..step
                    };
                    out.queued.push(existing);
                }
            }
            StepStatus::Done | StepStatus::Failed | StepStatus::Skipped => {
                let depends_on = mapped
                    .into_iter()
                    .filter(|d| *d != existing && !blocked_by_cycle(plan, *d))
                    .collect();
                plan.steps[existing] = PlanStep { depends_on, ..step };
                status[existing] = StepStatus::Pending;
                out.queued.push(existing);
            }
        }
    }
    for &step in out.queued.iter().chain(&out.rerun) {
        let Some(slot) = plan.steps[step].lane.slot().cloned() else {
            continue;
        };
        if plan.steps[step].action != StepAction::Execute {
            continue;
        }
        let downstream: BTreeSet<usize> = plan.dependents_of(step).into_iter().collect();
        let barriers: Vec<usize> = (0..plan.steps.len())
            .filter(|e| *e != step && !from_next.contains(e) && !downstream.contains(e))
            .filter(|e| matches!(status[*e], StepStatus::Pending | StepStatus::Running))
            .filter(|e| {
                let (this, other) = (&plan.steps[step], &plan.steps[*e]);
                other.action == StepAction::Execute
                    && other.lane.slot() == Some(&slot)
                    && (this.requires_approval
                        || other.requires_approval
                        || (matches!(this.lane, Lane::Session { .. }) && this.lane == other.lane))
            })
            .collect();
        for e in barriers {
            if !plan.steps[step].depends_on.contains(&e) {
                plan.steps[step].depends_on.push(e);
            }
        }
    }
    out
}

/// Concurrency limits for [`Schedule`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Concurrent queries per connection slot.
    pub per_connection: usize,
    /// Concurrent in-process steps.
    pub local: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            per_connection: 2,
            local: 2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepStatus {
    Pending,
    Running,
    Done,
    Failed,
    /// Not run because something it waits for failed.
    Skipped,
}

/// Drives a plan: hands out steps whose dependencies are done, within the
/// limits, and skips the dependents of failures.
#[derive(Debug, Clone)]
pub struct Schedule<'p> {
    plan: &'p RunPlan,
    limits: Limits,
    status: Vec<StepStatus>,
}

impl<'p> Schedule<'p> {
    pub fn new(plan: &'p RunPlan, limits: Limits) -> Self {
        Self {
            plan,
            limits,
            status: vec![StepStatus::Pending; plan.steps.len()],
        }
    }

    /// Continue a schedule from statuses saved with [`Self::statuses`], so a
    /// host can keep the plan and its progress without borrowing.
    pub fn resume(plan: &'p RunPlan, limits: Limits, status: Vec<StepStatus>) -> Self {
        let mut status = status;
        status.resize(plan.steps.len(), StepStatus::Pending);
        Self {
            plan,
            limits,
            status,
        }
    }

    pub fn status(&self, step: usize) -> StepStatus {
        self.status[step]
    }

    pub fn statuses(&self) -> &[StepStatus] {
        &self.status
    }

    fn running(&self, pred: impl Fn(&PlanStep) -> bool) -> usize {
        self.plan
            .steps
            .iter()
            .zip(&self.status)
            .filter(|(s, st)| **st == StepStatus::Running && pred(s))
            .count()
    }

    /// Mark and return (in plan order) every step that may start now. A dry
    /// run never starts anything.
    pub fn start_ready(&mut self) -> Vec<usize> {
        if self.plan.dry_run {
            return Vec::new();
        }
        self.skip_unreachable();
        let mut started = Vec::new();
        for i in 0..self.plan.steps.len() {
            let step = &self.plan.steps[i];
            if self.status[i] != StepStatus::Pending
                || !step
                    .depends_on
                    .iter()
                    .all(|d| self.status[*d] == StepStatus::Done)
            {
                continue;
            }
            let executing = step.action == StepAction::Execute;
            let fits = !executing
                || match &step.lane {
                    Lane::Instant => true,
                    Lane::Local => {
                        self.running(|s| s.lane == Lane::Local && s.action == StepAction::Execute)
                            < self.limits.local
                    }
                    Lane::Connection { slot } | Lane::Session { slot, .. } => {
                        let on_slot = |s: &PlanStep| {
                            s.action == StepAction::Execute && s.lane.slot() == Some(slot)
                        };
                        let session_busy = matches!(&step.lane, Lane::Session { .. })
                            && self.running(|s| s.lane == step.lane) > 0;
                        !session_busy && self.running(on_slot) < self.limits.per_connection.max(1)
                    }
                };
            if fits {
                self.status[i] = StepStatus::Running;
                started.push(i);
            }
        }
        started
    }

    /// Skip every pending step waiting on a step that failed or was skipped
    /// (a step merged into a run after its upstream had already failed).
    /// Returns the steps skipped, each after the steps it waits for.
    pub fn skip_unreachable(&mut self) -> Vec<usize> {
        let mut skipped = Vec::new();
        loop {
            let before = skipped.len();
            for i in 0..self.plan.steps.len() {
                if self.status[i] == StepStatus::Pending
                    && self.plan.steps[i].depends_on.iter().any(|d| {
                        matches!(self.status[*d], StepStatus::Failed | StepStatus::Skipped)
                    })
                {
                    self.status[i] = StepStatus::Skipped;
                    skipped.push(i);
                }
            }
            if skipped.len() == before {
                return skipped;
            }
        }
    }

    /// Put a step back to pending so it runs again (a re-run asked for
    /// while it was running). Its dependents are not skipped.
    pub fn requeue(&mut self, step: usize) {
        self.status[step] = StepStatus::Pending;
    }

    /// Record a running step's outcome. Returns the steps skipped because
    /// it failed.
    pub fn finish(&mut self, step: usize, ok: bool) -> Vec<usize> {
        if self.status[step] != StepStatus::Running {
            return Vec::new();
        }
        if ok {
            self.status[step] = StepStatus::Done;
            return Vec::new();
        }
        self.status[step] = StepStatus::Failed;
        let skipped: Vec<usize> = self
            .plan
            .dependents_of(step)
            .into_iter()
            .filter(|i| self.status[*i] == StepStatus::Pending)
            .collect();
        for i in &skipped {
            self.status[*i] = StepStatus::Skipped;
        }
        skipped
    }

    /// Nothing is pending or running.
    pub fn is_finished(&self) -> bool {
        self.status
            .iter()
            .all(|s| !matches!(s, StepStatus::Pending | StepStatus::Running))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notebook::document::{IdSource, SeededIds};
    use crate::notebook::fingerprint::{FingerprintInputs, fingerprint_all};
    use crate::notebook::output::{Completeness, Freshness};
    use crate::notebook::params::{ParamType, ParamValue, ParameterDef};
    use crate::notebook::references::Dialects;
    use crate::notebook::state::{CellEvent, OutputMeta};

    struct Fixture {
        doc: NotebookDoc,
        state: NotebookState,
        params: ResolvedParams,
    }

    impl Fixture {
        fn new(cells: &[(&str, CellKind, &str)]) -> Self {
            let mut doc = NotebookDoc::empty("n", "t");
            for (id, kind, src) in cells {
                let mut c = Cell::new(*id, kind.clone(), Default::default());
                c.source = src.to_string();
                doc.cells.push(c);
            }
            Fixture {
                doc,
                state: NotebookState::default(),
                params: ResolvedParams::default(),
            }
        }

        fn parts(&self) -> (Graph, BTreeMap<String, CellFingerprint>) {
            let dialects = Dialects::default();
            let graph = Graph::build(&self.doc, &dialects);
            let fps = fingerprint_all(
                &self.doc,
                &graph,
                &FingerprintInputs {
                    dialects: &dialects,
                    connections: &BTreeMap::new(),
                    params: &self.params,
                    upstream_override: &BTreeMap::new(),
                },
            );
            (graph, fps)
        }

        fn plan(&self, request: RunRequest) -> RunPlan {
            let (graph, fps) = self.parts();
            plan(
                &request,
                &PlanContext {
                    doc: &self.doc,
                    graph: &graph,
                    fingerprints: &fps,
                    state: &self.state,
                    params: &self.params,
                    now_ms: 10_000,
                },
            )
        }

        /// Give `ids` a current output.
        fn run_ok(&mut self, ids: &[&str]) {
            let (graph, fps) = self.parts();
            for id in ids {
                let run = format!("r_{id}");
                self.state
                    .apply(
                        &graph,
                        id,
                        CellEvent::Started {
                            run_id: run.clone(),
                        },
                    )
                    .unwrap();
                let output = OutputMeta {
                    run_id: run,
                    fingerprint: fps[*id].clone(),
                    completeness: Completeness::Complete,
                    row_count: 1,
                    finished_at_ms: 5_000,
                    from_cache: false,
                    freshness: Freshness::Live,
                };
                self.state
                    .apply(&graph, id, CellEvent::Completed { output })
                    .unwrap();
            }
        }
    }

    fn summary(plan: &RunPlan) -> Vec<(String, StepAction)> {
        plan.steps
            .iter()
            .map(|s| (s.cell_id.clone(), s.action))
            .collect()
    }

    fn ids(plan: &RunPlan) -> Vec<&str> {
        plan.steps.iter().map(|s| s.cell_id.as_str()).collect()
    }

    fn chain() -> Fixture {
        Fixture::new(&[
            ("a", CellKind::Sql, "SELECT 1 AS x"),
            ("m", CellKind::Markdown, "Notes about {{b}}"),
            ("b", CellKind::Sql, "SELECT * FROM {{a}}"),
            ("ch", CellKind::Chart, ""),
            ("z", CellKind::Sql, "SELECT 2"),
        ])
    }

    fn with_chart_input(mut f: Fixture) -> Fixture {
        f.doc.cells[3].spec = serde_json::json!({"input": "b"});
        f
    }

    #[test]
    fn run_all_orders_and_executes_everything() {
        let f = with_chart_input(chain());
        let p = f.plan(RunRequest::All);
        assert_eq!(ids(&p), vec!["a", "b", "m", "ch", "z"]);
        assert_eq!(p.step("m").unwrap().action, StepAction::Instant);
        assert!(p.executing().count() == 4);
        let b = p.step("b").unwrap();
        assert_eq!(b.depends_on, vec![0]);
        assert_eq!(b.lane, Lane::Connection { slot: None });
        assert_eq!(p.step("ch").unwrap().lane, Lane::Local);
        assert!(p.blocked.is_empty());
    }

    #[test]
    fn run_cell_alone_or_with_stale_upstream() {
        let mut f = with_chart_input(chain());
        let p = f.plan(RunRequest::Cell {
            id: "b".into(),
            include_stale_upstream: false,
        });
        assert_eq!(
            ids(&p),
            vec!["b"],
            "SQL upstreams inline, nothing else needed"
        );
        let p = f.plan(RunRequest::Cell {
            id: "ch".into(),
            include_stale_upstream: false,
        });
        assert!(p.steps.is_empty());
        assert_eq!(
            p.blocked[0].reason,
            BlockReason::UpstreamNotCurrent { cell: "b".into() }
        );
        let p = f.plan(RunRequest::Cell {
            id: "ch".into(),
            include_stale_upstream: true,
        });
        assert_eq!(
            summary(&p),
            vec![
                ("a".into(), StepAction::Execute),
                ("b".into(), StepAction::Execute),
                ("ch".into(), StepAction::Execute),
            ]
        );
        f.run_ok(&["a", "b"]);
        let p = f.plan(RunRequest::Cell {
            id: "ch".into(),
            include_stale_upstream: true,
        });
        assert_eq!(
            summary(&p),
            vec![
                ("a".into(), StepAction::Reuse(ReuseReason::Fresh)),
                ("b".into(), StepAction::Reuse(ReuseReason::Fresh)),
                ("ch".into(), StepAction::Execute),
            ]
        );
        let p = f.plan(RunRequest::Cell {
            id: "ch".into(),
            include_stale_upstream: false,
        });
        assert_eq!(ids(&p), vec!["ch"]);
        // The target itself always runs when asked for, even if current.
        let p = f.plan(RunRequest::Cell {
            id: "b".into(),
            include_stale_upstream: false,
        });
        assert_eq!(p.steps[0].action, StepAction::Execute);
    }

    #[test]
    fn stale_downstream_above_below_selected() {
        let mut f = with_chart_input(chain());
        f.run_ok(&["a", "z"]);
        assert_eq!(ids(&f.plan(RunRequest::Stale)), vec!["b", "m", "ch"]);
        let p = f.plan(RunRequest::Downstream("a".into()));
        assert_eq!(ids(&p), vec!["a", "b", "m", "ch"]);
        assert!(p.steps[0].explicit && !p.steps[1].explicit);
        assert_eq!(
            ids(&f.plan(RunRequest::Above("ch".into()))),
            vec!["a", "b", "m"]
        );
        // Markdown above that reads a cell below it cannot show it yet.
        let above = f.plan(RunRequest::Above("b".into()));
        assert_eq!(ids(&above), vec!["a"]);
        assert_eq!(
            above.blocked[0].reason,
            BlockReason::UpstreamNotCurrent { cell: "b".into() }
        );
        let below = f.plan(RunRequest::Below("b".into()));
        assert_eq!(ids(&below), vec!["b", "ch", "z"]);
        assert_eq!(
            below.step("z").unwrap().action,
            StepAction::Execute,
            "named by the request"
        );
        let sel = f.plan(RunRequest::Selected(vec![
            "z".into(),
            "a".into(),
            "ghost".into(),
        ]));
        assert_eq!(ids(&sel), vec!["a", "z"]);
        assert_eq!(
            sel.blocked,
            vec![BlockedStep {
                cell_id: "ghost".into(),
                reason: BlockReason::UnknownCell
            }]
        );
        let p = f.plan(RunRequest::Downstream("ghost".into()));
        assert_eq!(p.blocked[0].reason, BlockReason::UnknownCell);
    }

    #[test]
    fn markdown_reading_an_upstream_needs_its_output() {
        let mut f = chain();
        let p = f.plan(RunRequest::Cell {
            id: "m".into(),
            include_stale_upstream: false,
        });
        assert_eq!(
            p.blocked[0].reason,
            BlockReason::UpstreamNotCurrent { cell: "b".into() }
        );
        f.run_ok(&["a", "b"]);
        let p = f.plan(RunRequest::Cell {
            id: "m".into(),
            include_stale_upstream: false,
        });
        assert_eq!(summary(&p), vec![("m".into(), StepAction::Instant)]);
    }

    #[test]
    fn dry_runs_and_validation_run_nothing() {
        let f = Fixture::new(&[
            ("a", CellKind::Sql, "SELECT * FROM {{ghost}}"),
            ("b", CellKind::Sql, "SELECT 1"),
            ("p", CellKind::Python, "print(1)"),
        ]);
        let dry = f.plan(RunRequest::DryRun(Box::new(RunRequest::All)));
        assert!(dry.dry_run);
        assert_eq!(ids(&dry), vec!["b"]);
        assert_eq!(dry.blocked.len(), 2);
        assert!(
            Schedule::new(&dry, Limits::default())
                .start_ready()
                .is_empty()
        );
        let v = f.plan(RunRequest::ValidateOnly);
        assert!(v.validate_only);
        assert!(v.steps.iter().all(|s| s.action == StepAction::Validate));
        assert_eq!(ids(&v), vec!["a", "b", "p"]);
        assert_eq!(
            v.blocked
                .iter()
                .map(|b| b.reason.message().key().unwrap())
                .collect::<Vec<_>>(),
            vec![
                "notebook.core.graph.unknown_cell",
                "notebook.core.plan.unsupported_kind"
            ]
        );
    }

    #[test]
    fn cycles_and_parameters_block_with_reasons() {
        let mut f = Fixture::new(&[
            ("a", CellKind::Sql, "SELECT * FROM {{b}}"),
            ("b", CellKind::Sql, "SELECT * FROM {{a}}"),
            ("c", CellKind::Sql, "SELECT * FROM {{b}}"),
            ("d", CellKind::Sql, "SELECT :region"),
            ("e", CellKind::Sql, "SELECT * FROM {{d}}"),
            ("f", CellKind::Sql, "SELECT 1"),
        ]);
        f.doc
            .parameters
            .push(ParameterDef::new("region", ParamType::String));
        f.params.errors.insert("region".into(), ParamError::Missing);
        let p = f.plan(RunRequest::All);
        assert_eq!(ids(&p), vec!["f"]);
        let reasons: BTreeMap<&str, &BlockReason> = p
            .blocked
            .iter()
            .map(|b| (b.cell_id.as_str(), &b.reason))
            .collect();
        assert!(matches!(reasons["a"], BlockReason::Cycle { path } if path.len() == 3));
        assert!(matches!(reasons["c"], BlockReason::Cycle { path } if !path.is_empty()));
        assert_eq!(
            reasons["d"],
            &BlockReason::Parameter {
                name: "region".into(),
                error: ParamError::Missing
            }
        );
        assert_eq!(
            reasons["e"],
            &BlockReason::UpstreamBlocked { cell: "d".into() }
        );
        f.params.errors.clear();
        f.params
            .values
            .insert("region".into(), ParamValue::Text("EMEA".into()));
        assert_eq!(ids(&f.plan(RunRequest::All)), vec!["d", "e", "f"]);
    }

    #[test]
    fn cache_policies_shape_reuse() {
        let mut f = chain();
        f.run_ok(&["a", "z"]);
        // Downstream of b: a is outside the plan; b executes (explicit).
        f.doc.cells[0].execution_policy.cache = CachePolicy::Pinned;
        f.run_ok(&["a"]);
        let p = f.plan(RunRequest::All);
        assert_eq!(
            p.step("a").unwrap().action,
            StepAction::Reuse(ReuseReason::Pinned)
        );
        assert_eq!(p.step("z").unwrap().action, StepAction::Execute);
        let p = f.plan(RunRequest::Cell {
            id: "a".into(),
            include_stale_upstream: false,
        });
        assert_eq!(
            p.steps[0].action,
            StepAction::Execute,
            "asking for a pinned cell runs it"
        );
        f.doc.cells[4].execution_policy.cache = CachePolicy::Off;
        f.run_ok(&["z"]);
        assert!(ids(&f.plan(RunRequest::Stale)).contains(&"b"));
        let mut g = chain();
        g.doc.cells[0].execution_policy.cache = CachePolicy::Ttl { seconds: 1 };
        g.run_ok(&["a"]);
        // finished 5_000, now 10_000: a 1 s TTL has lapsed.
        assert!(ids(&g.plan(RunRequest::Stale)).contains(&"a"));
        g.doc.cells[0].execution_policy.cache = CachePolicy::Ttl { seconds: 60 };
        g.run_ok(&["a"]);
        assert!(!ids(&g.plan(RunRequest::Stale)).contains(&"a"));
    }

    #[test]
    fn sessions_and_writes_serialize_on_their_connection() {
        let mut f = Fixture::new(&[
            ("r1", CellKind::Sql, "SELECT 1"),
            ("r2", CellKind::Sql, "SELECT 2"),
            ("w", CellKind::Sql, "UPDATE t SET x = 1"),
            ("r3", CellKind::Sql, "SELECT 3"),
            ("s1", CellKind::Sql, "SELECT * INTO #t FROM x"),
            ("s2", CellKind::Sql, "SELECT * FROM #t"),
            ("o", CellKind::Sql, "SELECT 4"),
        ]);
        f.doc.cells[2].execution_policy.read_only = false;
        f.doc.cells[4].session_group = Some("tmp".into());
        f.doc.cells[5].session_group = Some("tmp".into());
        f.doc.cells[6].connection_ref = Some("other".into());
        let p = f.plan(RunRequest::All);
        let dep = |id: &str| -> Vec<&str> {
            p.step(id)
                .unwrap()
                .depends_on
                .iter()
                .map(|i| p.steps[*i].cell_id.as_str())
                .collect()
        };
        assert!(dep("r1").is_empty() && dep("r2").is_empty());
        assert_eq!(dep("w"), vec!["r1", "r2"]);
        assert!(p.step("w").unwrap().requires_approval);
        assert_eq!(dep("r3"), vec!["w"]);
        assert_eq!(dep("s2"), vec!["w", "s1"]);
        assert!(dep("o").is_empty(), "another connection is independent");
        assert_eq!(
            p.step("s1").unwrap().lane,
            Lane::Session {
                slot: None,
                group: "tmp".into()
            }
        );
    }

    #[test]
    fn schedule_respects_limits_and_skips_after_failures() {
        let f = Fixture::new(&[
            ("a", CellKind::Sql, "SELECT 1"),
            ("b", CellKind::Sql, "SELECT 2"),
            ("c", CellKind::Sql, "SELECT 3"),
            ("d", CellKind::Sql, "SELECT * FROM {{a}}"),
            ("m", CellKind::Markdown, "hi"),
        ]);
        let p = f.plan(RunRequest::All);
        let mut s = Schedule::new(
            &p,
            Limits {
                per_connection: 2,
                local: 1,
            },
        );
        let first = s.start_ready();
        let names = |v: &[usize]| {
            v.iter()
                .map(|i| p.steps[*i].cell_id.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(&first), vec!["a", "b", "m"]);
        assert!(s.start_ready().is_empty());
        let a = first[0];
        assert_eq!(names(&s.finish(a, false)), vec!["d"]);
        assert_eq!(names(&s.start_ready()), vec!["c"]);
        for i in 0..p.steps.len() {
            s.finish(i, true);
        }
        assert!(s.is_finished());
        assert_eq!(s.status(a), StepStatus::Failed);
    }

    #[test]
    fn merging_restarts_finished_steps_and_reruns_running_ones() {
        let mut f = Fixture::new(&[
            ("a", CellKind::Sql, "SELECT 1"),
            ("b", CellKind::Sql, "SELECT 2"),
            ("c", CellKind::Sql, "SELECT * FROM {{b}}"),
        ]);
        let mut run = f.plan(RunRequest::All);
        let limits = Limits::default();
        let mut s = Schedule::new(&run, limits);
        assert_eq!(s.start_ready(), vec![0, 1]);
        s.finish(0, true);
        let mut status = s.statuses().to_vec();
        f.run_ok(&["a"]);

        let again = f.plan(RunRequest::Cell {
            id: "a".into(),
            include_stale_upstream: false,
        });
        let merged = merge_plan(&mut run, &mut status, again);
        assert_eq!(merged.queued, vec![0], "a finished, so it starts over");
        assert!(merged.rerun.is_empty());
        let mut s = Schedule::resume(&run, limits, status.clone());
        assert_eq!(s.start_ready(), vec![0], "a is dispatched again");
        status = s.statuses().to_vec();

        let again = f.plan(RunRequest::Cell {
            id: "b".into(),
            include_stale_upstream: false,
        });
        let merged = merge_plan(&mut run, &mut status, again);
        assert_eq!(
            merged.rerun,
            vec![1],
            "b is still running: run it again after"
        );
        let mut s = Schedule::resume(&run, limits, status);
        s.requeue(1);
        assert_eq!(s.start_ready(), vec![1]);
        assert_eq!(s.status(2), StepStatus::Pending, "c still waits for b");
    }

    #[test]
    fn merged_steps_keep_write_barriers_and_skip_after_failed_upstreams() {
        let mut f = Fixture::new(&[
            ("r", CellKind::Sql, "SELECT 1"),
            ("w", CellKind::Sql, "UPDATE t SET x = 1"),
            ("u", CellKind::Sql, "SELECT 2"),
            ("d", CellKind::Chart, ""),
        ]);
        f.doc.cells[1].execution_policy.read_only = false;
        f.doc.cells[3].spec = serde_json::json!({"input": "u"});
        let mut run = f.plan(RunRequest::Cell {
            id: "r".into(),
            include_stale_upstream: false,
        });
        let limits = Limits::default();
        let mut s = Schedule::new(&run, limits);
        assert_eq!(s.start_ready(), vec![0]);
        let mut status = s.statuses().to_vec();

        let write = f.plan(RunRequest::Cell {
            id: "w".into(),
            include_stale_upstream: false,
        });
        merge_plan(&mut run, &mut status, write);
        let w = run.steps.iter().position(|s| s.cell_id == "w").unwrap();
        assert_eq!(
            run.steps[w].depends_on,
            vec![0],
            "the write waits for the running read"
        );
        let mut s = Schedule::resume(&run, limits, status);
        assert!(s.start_ready().is_empty(), "never beside the read");
        let mut status = s.statuses().to_vec();

        let u_then_d = f.plan(RunRequest::Downstream("u".into()));
        merge_plan(&mut run, &mut status, u_then_d);
        let u = run.steps.iter().position(|s| s.cell_id == "u").unwrap();
        let d = run.steps.iter().position(|s| s.cell_id == "d").unwrap();
        assert_eq!(
            run.steps[u].depends_on,
            vec![w],
            "a read after a pending write waits for it"
        );
        let mut s = Schedule::resume(&run, limits, status);
        s.finish(0, true);
        assert_eq!(s.start_ready(), vec![w]);
        s.finish(w, false);
        assert_eq!(s.status(u), StepStatus::Skipped);
        assert_eq!(s.status(d), StepStatus::Skipped);
        assert!(s.is_finished(), "nothing waits forever on a failed step");

        // A step merged after its upstream already failed is skipped too.
        let plan = f.plan(RunRequest::All);
        let mut status = vec![StepStatus::Pending; plan.steps.len()];
        let u = plan.steps.iter().position(|s| s.cell_id == "u").unwrap();
        status[u] = StepStatus::Failed;
        for (i, st) in status.iter_mut().enumerate() {
            if i != u {
                *st = StepStatus::Done;
            }
        }
        let d = plan.steps.iter().position(|s| s.cell_id == "d").unwrap();
        status[d] = StepStatus::Pending;
        let mut s = Schedule::resume(&plan, limits, status);
        assert_eq!(s.skip_unreachable(), vec![d]);
        assert!(s.is_finished());
    }

    /// Fixed-seed random notebooks, run to completion with random finish
    /// order: dependencies always finish first, connection limits and
    /// session exclusivity always hold, and every step ends.
    #[test]
    fn randomized_schedules_keep_every_rule() {
        let mut src = SeededIds(31337);
        let mut next = || {
            let mut b = [0u8; 1];
            src.fill(&mut b);
            b[0] as usize
        };
        for _ in 0..150 {
            let n = 1 + next() % 14;
            let mut f = Fixture::new(&[]);
            for i in 0..n {
                let kind = match next() % 5 {
                    0 => CellKind::Markdown,
                    1 => CellKind::Chart,
                    _ => CellKind::Sql,
                };
                let mut c = Cell::new(format!("n{i}"), kind.clone(), Default::default());
                if kind == CellKind::Sql {
                    let refs: Vec<String> = (0..i)
                        .filter(|_| next() % 4 == 0)
                        .map(|j| format!("{{{{n{j}}}}}"))
                        .collect();
                    c.source = format!("SELECT 1 {}", refs.join(" "));
                    if next() % 4 == 0 {
                        c.session_group = Some(format!("g{}", next() % 2));
                    }
                    if next() % 6 == 0 {
                        c.execution_policy.read_only = false;
                    }
                    if next() % 3 == 0 {
                        c.connection_ref = Some(format!("s{}", next() % 2));
                    }
                } else if kind == CellKind::Chart && i > 0 {
                    c.spec = serde_json::json!({"input": format!("n{}", next() % i)});
                }
                f.doc.cells.push(c);
            }
            let p = f.plan(RunRequest::All);
            let limits = Limits {
                per_connection: 1 + next() % 3,
                local: 1 + next() % 2,
            };
            let mut s = Schedule::new(&p, limits);
            let mut running: Vec<usize> = Vec::new();
            let mut guard = 0;
            while !s.is_finished() {
                guard += 1;
                assert!(guard < 1_000, "schedule stalled");
                running.extend(s.start_ready());
                for &r in &running {
                    for d in &p.steps[r].depends_on {
                        assert_eq!(s.status(*d), StepStatus::Done);
                    }
                }
                let exec: Vec<&PlanStep> = running
                    .iter()
                    .map(|i| &p.steps[*i])
                    .filter(|st| st.action == StepAction::Execute)
                    .collect();
                let mut per_slot: BTreeMap<Option<String>, usize> = BTreeMap::new();
                let mut sessions: BTreeMap<Lane, usize> = BTreeMap::new();
                for st in &exec {
                    if let Some(slot) = st.lane.slot() {
                        *per_slot.entry(slot.clone()).or_default() += 1;
                    }
                    if matches!(st.lane, Lane::Session { .. }) {
                        *sessions.entry(st.lane.clone()).or_default() += 1;
                    }
                }
                assert!(per_slot.values().all(|c| *c <= limits.per_connection));
                assert!(sessions.values().all(|c| *c <= 1));
                assert!(exec.iter().filter(|st| st.lane == Lane::Local).count() <= limits.local);
                if running.is_empty() {
                    continue;
                }
                let pick = next() % running.len();
                let step = running.remove(pick);
                s.finish(step, true);
            }
        }
    }
}
