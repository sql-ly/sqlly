//! The notebook side of AI cells: per-cell UI state (prompt input, the run
//! in flight, a pending cloud consent preview), the actions the cell's
//! buttons call, and what the host needs to start a run. The agent itself
//! (tools, budgets, evidence, proposals) is `sqlly_services::notebook_agent`;
//! the host (`app/notebook_ai.rs`) owns providers and connections.

pub mod inputs;
pub mod render;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use gpui::{AppContext as _, Context, Entity, Subscription, Window};
use gpui_component::input::{InputEvent, TextareaState};
use sqlly_ai::catalog::CancelFlag;
use sqlly_services::notebook::cells::ai::{
    AgentMode, AiCellSpec, AiResult, Budget, BudgetUsage, CompareRef, ProposalStatus, RunStatus,
};
use sqlly_services::notebook::{CellKind, CellOutput, NotebookDoc, RandomIds};
use sqlly_services::notebook_agent::explain::RecordedRun;
use sqlly_services::notebook_agent::log::{LogEntry, LogEvent, decision};
use sqlly_services::notebook_agent::proposals::{self, AppliedGroup};
use sqlly_services::notebook_agent::{
    AgentRequest, CellRuntime, ContextPreview, NotebookAgentEvent, NotebookContext,
};
use sqlly_services::notebook_security::TrustDecision;
use sqlly_services::notebook_security::trust::{CURRENT_CONNECTION, TrustLevel};

use super::reveal::Reveal;
use super::view::{NotebookEvent, NotebookView};

pub const KIND_AI: &str = "ai";

/// A run in flight.
#[derive(Clone)]
pub struct LiveRun {
    pub cancel: CancelFlag,
    pub turn: (u32, u32),
    pub usage: BudgetUsage,
    pub budget: Budget,
    /// The tool the agent is calling now.
    pub tool: Option<String>,
}

/// Consent shown, notice, and the stored result's summary and detail.
#[cfg(test)]
pub(crate) type AiStateForTest = (bool, Option<String>, Option<(String, Option<String>)>);

#[derive(Default)]
struct AiCellUi {
    prompt: Option<Entity<TextareaState>>,
    _prompt_sub: Option<Subscription>,
    run: Option<LiveRun>,
    consent: Option<ContextPreview>,
    notice: Option<(String, bool)>,
    show_queries: bool,
}

/// A recorded run as the compare menus name it.
fn run_label(record: &sqlly_services::notebook::run_record::RunRecord) -> String {
    crate::localization::format(
        "notebook.ai.compare.run",
        &[(
            "started",
            record
                .started_at
                .with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string(),
        )],
    )
}

/// AI state the notebook view keeps per tab.
#[derive(Default)]
pub struct NotebookAi {
    cells: HashMap<String, AiCellUi>,
    /// The last applied group of proposals, for Undo AI changes.
    applied: Option<AppliedGroup>,
    /// The provider id runs use, pushed by the host (for the egress line).
    provider: Option<String>,
}

impl NotebookAi {
    /// Drop the last applied group (a structural undo or redo moved past it).
    pub(in crate::notebook) fn forget_applied(&mut self) {
        self.applied = None;
    }
}

/// Everything the AI cell renderer draws.
#[derive(Clone)]
pub struct AiCellSnapshot {
    pub spec: AiCellSpec,
    pub prompt: Option<Entity<TextareaState>>,
    pub run: Option<LiveRun>,
    pub consent: Option<ContextPreview>,
    pub notice: Option<(String, bool)>,
    pub show_queries: bool,
    /// This cell's proposals are the last applied group.
    pub can_undo: bool,
    pub provider: Option<String>,
    /// Display names of the notebook's cells, by id.
    pub labels: HashMap<String, String>,
    /// Recorded runs Explain changes can compare, newest first: id and label.
    pub runs: Vec<(String, String)>,
}

/// What the host needs to start a run of one AI cell.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub struct AiRunInputs {
    pub doc: NotebookDoc,
    pub outputs: BTreeMap<String, CellOutput>,
    pub runtime: BTreeMap<String, CellRuntime>,
    pub trust: TrustDecision,
    pub request: AgentRequest,
    pub ai_cell: String,
    /// The outputs the previous run read (Explain changes' baseline).
    pub previous: Option<AiResult>,
}

#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
impl AiRunInputs {
    /// The run's context, with the connection facts the host resolved.
    pub fn context(
        &self,
        connections: BTreeMap<String, sqlly_services::notebook_security::ConnectionFacts>,
    ) -> NotebookContext {
        NotebookContext {
            doc: self.doc.clone(),
            outputs: self.outputs.clone(),
            runtime: self.runtime.clone(),
            trust: self.trust.clone(),
            connections,
            ai_cell: self.ai_cell.clone(),
        }
    }
}

fn t(key: &str) -> String {
    crate::localization::text(key)
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Write log entries off the UI thread; a failed write is logged, never shown.
pub fn append_log(entries: Vec<LogEntry>, cx: &mut gpui::App) {
    #[cfg(not(target_arch = "wasm32"))]
    cx.background_spawn(async move {
        if let Err(error) =
            sqlly_services::notebook_agent::log::ActionLog::new(ai_dir()).append(&entries)
        {
            tracing::warn!(%error, "could not write the notebook AI action log");
        }
    })
    .detach();
    #[cfg(target_arch = "wasm32")]
    let _ = (entries, cx);
}

impl NotebookView {
    fn ai_spec(&self, id: &str) -> AiCellSpec {
        self.doc
            .model()
            .cell(id)
            .map(AiCellSpec::of)
            .unwrap_or_default()
    }

    fn ai_write_spec(&mut self, id: &str, spec: &AiCellSpec, cx: &mut Context<Self>) {
        if self.doc.set_spec(id, spec.to_value()) {
            self.mark_dirty(cx);
        }
        self.ai_remeasure(id);
        cx.notify();
    }

    fn ai_remeasure(&self, id: &str) {
        if let Some(index) = self.doc.index_of(id) {
            self.list_state.remeasure_items(index..index + 1);
        }
    }

    /// The renderer's snapshot of one AI cell; creates its prompt input.
    pub(super) fn ai_snapshot(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AiCellSnapshot {
        let spec = self.ai_spec(id);
        if self.ai.cells.get(id).is_none_or(|ui| ui.prompt.is_none()) {
            let source = self
                .doc
                .cell_by_id(id)
                .map(|c| c.source.to_string())
                .unwrap_or_default();
            let input = cx.new(|cx| {
                TextareaState::new(window, cx)
                    .soft_wrap(true)
                    .auto_grow(2, 8)
                    .placeholder(crate::localization::input_placeholder(
                        window,
                        cx,
                        "notebook.ai.cell.prompt_placeholder",
                    ))
            });
            if !source.is_empty() {
                input.update(cx, |state, cx| state.set_value(source, window, cx));
            }
            let cell_id = id.to_string();
            let sub = cx.subscribe(&input, move |this, input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    let value = input.read(cx).value().to_string();
                    if this.doc.set_source(&cell_id, &value) {
                        this.mark_dirty(cx);
                    }
                }
            });
            let ui = self.ai.cells.entry(id.to_string()).or_default();
            ui.prompt = Some(input);
            ui._prompt_sub = Some(sub);
        }
        let runs = if spec.mode == AgentMode::ExplainChanges {
            self.recorded_runs()
                .iter()
                .rev()
                .map(|r| (r.run_id.clone(), run_label(r)))
                .collect()
        } else {
            Vec::new()
        };
        let ui = self.ai.cells.get(id);
        AiCellSnapshot {
            runs,
            can_undo: self.ai.applied.as_ref().is_some_and(|g| g.ai_cell == id),
            spec,
            prompt: ui.and_then(|u| u.prompt.clone()),
            run: ui.and_then(|u| u.run.clone()),
            consent: ui.and_then(|u| u.consent.clone()),
            notice: ui.and_then(|u| u.notice.clone()),
            show_queries: ui.is_some_and(|u| u.show_queries),
            provider: self.ai.provider.clone(),
            labels: self
                .doc
                .cells()
                .map(|c| (c.id.to_string(), c.display_name().to_string()))
                .collect(),
        }
    }

    /// The provider runs use, for the cell's egress line.
    pub fn set_ai_provider(&mut self, provider: Option<String>) {
        self.ai.provider = provider;
    }

    pub fn is_ai_running(&self, id: &str) -> bool {
        self.ai.cells.get(id).is_some_and(|ui| ui.run.is_some())
    }

    /// Insert an AI cell after `after` (or at the end) and return its id.
    pub fn insert_ai_cell(
        &mut self,
        after: Option<&str>,
        mode: AgentMode,
        target: Option<String>,
        prompt: &str,
        cx: &mut Context<Self>,
    ) -> String {
        let index = after
            .and_then(|a| self.doc.index_of(a))
            .map(|i| i + 1)
            .unwrap_or(self.doc.len());
        let id = self.insert_cell(KIND_AI, index, cx);
        let mut spec = AiCellSpec::new(mode);
        spec.target = target;
        self.doc.set_source(&id, prompt);
        self.ai_write_spec(&id, &spec, cx);
        self.reveal(&id, Reveal::Select, cx);
        id
    }

    /// "Explain with AI" on a failed or plan cell: an Assist cell below it,
    /// aimed at it, which starts at once.
    pub fn explain_with_ai(&mut self, cell_id: &str, cx: &mut Context<Self>) {
        if self.doc.cell_by_id(cell_id).is_none() {
            return;
        }
        let is_plan = self
            .doc
            .cell_by_id(cell_id)
            .is_some_and(|c| c.kind == CellKind::Plan.as_str());
        let prompt = t(
            if self.doc.status(cell_id) == super::view::CellStatus::Failed {
                "notebook.ai.explain.error_prompt"
            } else if is_plan {
                "notebook.ai.explain.plan_prompt"
            } else {
                "notebook.ai.explain.result_prompt"
            },
        );
        let id = self.insert_ai_cell(
            Some(cell_id),
            AgentMode::Assist,
            Some(cell_id.to_string()),
            &prompt,
            cx,
        );
        self.ai_request_run(&id, false, cx);
    }

    /// The failed cell "Explain Last Error" means: the active cell when it
    /// failed, else the last failed cell in the document.
    pub fn last_failed_cell(&self) -> Option<String> {
        let failed = |id: &str| self.doc.status(id) == super::view::CellStatus::Failed;
        self.active.clone().filter(|id| failed(id)).or_else(|| {
            self.doc
                .cells()
                .filter(|c| failed(c.id))
                .last()
                .map(|c| c.id.to_string())
        })
    }

    /// What Explain changes compares: recorded run `before` (empty: the
    /// last time this AI cell ran) against run `after` (empty: now).
    pub(crate) fn ai_set_compare(
        &mut self,
        id: &str,
        before: String,
        after: String,
        cx: &mut Context<Self>,
    ) {
        let mut spec = self.ai_spec(id);
        let compare = (!before.is_empty()).then_some(CompareRef { before, after });
        if spec.compare == compare {
            return;
        }
        spec.compare = compare;
        self.ai_write_spec(id, &spec, cx);
    }

    /// The recorded runs `compare` names, read as Explain changes compares
    /// them: the outputs each kept that may be shown now, and the
    /// fingerprint parts it recorded. `None` when `before` is not set or
    /// not recorded.
    pub fn ai_compare_runs(
        &mut self,
        compare: &CompareRef,
    ) -> Option<(RecordedRun, Option<RecordedRun>)> {
        if compare.before.is_empty() {
            return None;
        }
        let before = self.ai_recorded_run(&compare.before)?;
        let after = if compare.after.is_empty() {
            None
        } else {
            Some(self.ai_recorded_run(&compare.after)?)
        };
        Some((before, after))
    }

    fn ai_recorded_run(&mut self, run_id: &str) -> Option<RecordedRun> {
        let record = self
            .recorded_runs()
            .iter()
            .find(|r| r.run_id == run_id)
            .cloned()?;
        let details = super::history::details_of(&record);
        let mut outputs = BTreeMap::new();
        for cell in &record.cells {
            if let Some(output) = self.recorded_output(run_id, &cell.cell_id) {
                outputs.insert(cell.cell_id.clone(), output);
            }
        }
        Some(RecordedRun {
            run_id: run_id.to_string(),
            outputs,
            parts: details.fingerprints,
        })
    }

    pub(crate) fn ai_set_mode(&mut self, id: &str, mode: AgentMode, cx: &mut Context<Self>) {
        let mut spec = self.ai_spec(id);
        if spec.mode == mode {
            return;
        }
        spec.mode = mode;
        spec.budget = Budget::for_mode(mode);
        self.ai_write_spec(id, &spec, cx);
    }

    /// Ask the host to run the cell. Untrusted notebooks run nothing.
    pub(crate) fn ai_request_run(&mut self, id: &str, consented: bool, cx: &mut Context<Self>) {
        if self.live_trust().level == TrustLevel::Untrusted {
            self.trust_flash = true;
            self.status(t("notebook.shell.trust.run_blocked"), true, cx);
            return;
        }
        if self.is_ai_running(id) {
            return;
        }
        let ui = self.ai.cells.entry(id.to_string()).or_default();
        ui.consent = None;
        ui.notice = None;
        if let Some(prompt) = ui.prompt.clone() {
            let value = prompt.read(cx).value().to_string();
            if self.doc.set_source(id, &value) {
                self.mark_dirty(cx);
            }
        }
        self.activate_cell(id, cx);
        cx.emit(NotebookEvent::AiRunRequested {
            cell_id: id.to_string(),
            consented,
        });
        cx.notify();
    }

    pub(crate) fn ai_stop(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(run) = self.ai.cells.get(id).and_then(|ui| ui.run.as_ref()) {
            run.cancel.store(true, Ordering::Relaxed);
        }
        cx.notify();
    }

    /// The trust decision the agent's connection checks use: the notebook's
    /// live trust, with the tab's own connection authorized once trusted.
    pub fn ai_trust_decision(&self) -> TrustDecision {
        let mut decision = self.live_trust();
        if decision.level != TrustLevel::Untrusted {
            decision
                .connections
                .entry(CURRENT_CONNECTION.to_string())
                .or_insert(None);
        }
        decision
    }

    /// What the host needs to run `id`, captured now.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub fn ai_run_inputs(&self, id: &str, cx: &gpui::App) -> Option<AiRunInputs> {
        let cell = self.doc.model().cell(id)?;
        let spec = AiCellSpec::of(cell);
        let previous = spec.result.clone();
        let approved_plan = previous
            .as_ref()
            .and_then(|r| r.plan.clone())
            .filter(|p| p.approved && spec.mode != AgentMode::Plan);
        Some(AiRunInputs {
            doc: self.doc.model().clone(),
            outputs: inputs::outputs(self, cx),
            runtime: inputs::runtime(self),
            trust: self.ai_trust_decision(),
            request: AgentRequest {
                mode: spec.mode,
                prompt: cell.source.clone(),
                target: spec.target.clone(),
                budget: spec.budget.clamped(),
                approved_plan,
                deterministic: vec![],
                run_id: sqlly_services::notebook::run_record::new_run_id(&mut RandomIds),
            },
            ai_cell: id.to_string(),
            previous,
        })
    }

    /// The host needs the user's consent before sharing with a cloud model.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub fn ai_needs_consent(&mut self, id: &str, preview: ContextPreview, cx: &mut Context<Self>) {
        let ui = self.ai.cells.entry(id.to_string()).or_default();
        ui.consent = Some(preview);
        ui.notice = None;
        self.ai_remeasure(id);
        cx.notify();
    }

    pub(crate) fn ai_dismiss_consent(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(ui) = self.ai.cells.get_mut(id) {
            ui.consent = None;
        }
        self.ai_remeasure(id);
        cx.notify();
    }

    /// Whether `id` shows the cloud sharing preview, the notice it shows,
    /// and the stored result's summary and failure detail.
    #[cfg(test)]
    pub(crate) fn ai_state_for_test(&self, id: &str) -> AiStateForTest {
        let ui = self.ai.cells.get(id);
        (
            ui.is_some_and(|u| u.consent.is_some()),
            ui.and_then(|u| u.notice.as_ref().map(|(text, _)| text.clone())),
            self.ai_spec(id)
                .result
                .map(|r| (r.summary.clone(), r.detail.clone())),
        )
    }

    /// The host refused the run (policy, no connection, no provider).
    pub fn ai_refused(&mut self, id: &str, message: String, cx: &mut Context<Self>) {
        let ui = self.ai.cells.entry(id.to_string()).or_default();
        ui.notice = Some((message, true));
        ui.run = None;
        self.ai_remeasure(id);
        cx.notify();
    }

    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub fn ai_started(
        &mut self,
        id: &str,
        cancel: CancelFlag,
        budget: Budget,
        cx: &mut Context<Self>,
    ) {
        let ui = self.ai.cells.entry(id.to_string()).or_default();
        ui.run = Some(LiveRun {
            cancel,
            turn: (0, budget.max_turns),
            usage: BudgetUsage::default(),
            budget,
            tool: None,
        });
        ui.consent = None;
        ui.notice = None;
        self.log(Some(id), t("notebook.ai.activity.started"), false, None);
        self.ai_remeasure(id);
        cx.notify();
    }

    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub fn ai_progress(&mut self, id: &str, event: NotebookAgentEvent, cx: &mut Context<Self>) {
        let Some(run) = self.ai.cells.get_mut(id).and_then(|ui| ui.run.as_mut()) else {
            return;
        };
        match event {
            NotebookAgentEvent::Turn { n, max } => run.turn = (n, max),
            NotebookAgentEvent::ToolCall(call) => run.tool = Some(call.name),
            NotebookAgentEvent::ToolResult(_) => run.tool = None,
            NotebookAgentEvent::Usage(usage) => run.usage = usage,
            NotebookAgentEvent::Compacted { .. } => {}
        }
        cx.notify();
    }

    /// A run ended: its result goes into the cell's spec.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub fn ai_finished(&mut self, id: &str, mut result: AiResult, cx: &mut Context<Self>) {
        if let Some(ui) = self.ai.cells.get_mut(id) {
            ui.run = None;
        }
        result.ended_at = Some(now());
        let mut spec = self.ai_spec(id);
        let key = match result.status {
            RunStatus::Completed => "notebook.ai.activity.completed",
            RunStatus::AwaitingApproval => "notebook.ai.activity.awaiting_approval",
            RunStatus::NeedsClarification => "notebook.ai.activity.needs_clarification",
            RunStatus::Stopped => "notebook.ai.activity.stopped",
            RunStatus::Failed | RunStatus::Refused => "notebook.ai.activity.failed",
        };
        let error = matches!(result.status, RunStatus::Failed | RunStatus::Refused);
        self.log(Some(id), t(key), error, None);
        if self.ai.applied.as_ref().is_some_and(|g| g.ai_cell == id) {
            self.ai.applied = None;
        }
        spec.result = Some(result);
        self.ai_write_spec(id, &spec, cx);
    }

    fn ai_log(&self, id: &str, run_id: &str, event: LogEvent, cx: &mut gpui::App) {
        let entry = decision(&self.doc.model().notebook_id, id, run_id, &now(), event);
        append_log(vec![entry], cx);
    }

    /// Accept or reject one proposal.
    pub(crate) fn ai_decide(
        &mut self,
        id: &str,
        proposal: &str,
        accept: bool,
        cx: &mut Context<Self>,
    ) {
        let status = if accept {
            ProposalStatus::Accepted
        } else {
            ProposalStatus::Rejected
        };
        let changed = self
            .doc
            .edit_model(|doc| proposals::set_status(doc, id, proposal, status));
        if changed {
            let run_id = self
                .ai_spec(id)
                .result
                .map(|r| r.run_id)
                .unwrap_or_default();
            self.ai_log(
                id,
                &run_id,
                LogEvent::ProposalDecided {
                    proposal: proposal.to_string(),
                    status,
                },
                cx,
            );
            self.mark_dirty(cx);
            self.ai_remeasure(id);
        }
        cx.notify();
    }

    /// Apply every accepted proposal as one undo group: new cells, edits,
    /// then the accepted read-only runs through the normal run path.
    pub(crate) fn ai_apply(&mut self, id: &str, cx: &mut Context<Self>) {
        let ctx = NotebookContext {
            doc: self.doc.model().clone(),
            outputs: BTreeMap::new(),
            runtime: BTreeMap::new(),
            trust: self.ai_trust_decision(),
            connections: self.ai_connection_facts(),
            ai_cell: id.to_string(),
        };
        let group = self
            .doc
            .edit_model(|doc| proposals::apply_accepted(&ctx, doc, &mut RandomIds));
        if group.proposals.is_empty() {
            self.status(t("notebook.ai.proposals.nothing_accepted"), false, cx);
            return;
        }
        self.list_state.reset(self.doc.len() + 1);
        self.ai_log(
            id,
            &group.run_id,
            LogEvent::Applied {
                proposals: group.proposals.clone(),
                inserted: group.inserted.clone(),
                edited: group.edits.iter().map(|e| e.cell_id.clone()).collect(),
            },
            cx,
        );
        for edit in &group.edits {
            if let Some((editor, _)) = self.cell_entities(&edit.cell_id)
                && let Some(source) = self
                    .doc
                    .cell_by_id(&edit.cell_id)
                    .map(|c| c.source.to_string())
            {
                editor.update(cx, |e, cx| e.replace_document(source, cx));
            }
        }
        let run = group.run.clone();
        let message = crate::localization::format(
            if group.proposals.len() == 1 {
                "notebook.ai.proposals.applied_one"
            } else {
                "notebook.ai.proposals.applied"
            },
            &[("count", group.proposals.len())],
        );
        self.ai.applied = Some(group);
        self.editing_label("notebook.editing.step.ai");
        self.mark_dirty(cx);
        self.status(message, false, cx);
        if !run.is_empty() {
            self.selected = run;
            self.run_scope(super::doc_adapter::RunScope::Selected, None, cx);
        }
    }

    /// Undo the last applied group in one step.
    pub(crate) fn ai_undo(&mut self, cx: &mut Context<Self>) {
        let Some(group) = self.ai.applied.take() else {
            return;
        };
        self.doc.edit_model(|doc| proposals::undo(doc, &group));
        for inserted in &group.inserted {
            self.cells.remove(inserted);
        }
        for edit in &group.edits {
            if let Some((editor, _)) = self.cell_entities(&edit.cell_id) {
                let source = edit.source.clone();
                editor.update(cx, |e, cx| e.replace_document(source, cx));
            }
        }
        self.list_state.reset(self.doc.len() + 1);
        self.ai_log(
            &group.ai_cell,
            &group.run_id,
            LogEvent::Undone {
                proposals: group.proposals.clone(),
            },
            cx,
        );
        self.editing_label("notebook.editing.step.ai");
        self.mark_dirty(cx);
        self.status(t("notebook.ai.proposals.undone"), false, cx);
    }

    /// Approve the plan and build it: the cell switches to Build mode and
    /// runs with the plan.
    pub(crate) fn ai_approve_plan(&mut self, id: &str, cx: &mut Context<Self>) {
        let mut spec = self.ai_spec(id);
        let Some(plan) = spec.result.as_mut().and_then(|r| r.plan.as_mut()) else {
            return;
        };
        plan.approved = true;
        let run_id = spec
            .result
            .as_ref()
            .map(|r| r.run_id.clone())
            .unwrap_or_default();
        spec.mode = AgentMode::Build;
        spec.budget = Budget::for_mode(AgentMode::Build);
        self.ai_write_spec(id, &spec, cx);
        self.ai_log(id, &run_id, LogEvent::PlanApproved, cx);
        self.ai_request_run(id, false, cx);
    }

    pub(crate) fn ai_toggle_queries(&mut self, id: &str, cx: &mut Context<Self>) {
        let ui = self.ai.cells.entry(id.to_string()).or_default();
        ui.show_queries = !ui.show_queries;
        self.ai_remeasure(id);
        cx.notify();
    }

    /// Jump to a cell a finding cites.
    pub(crate) fn ai_reveal(&mut self, cell: &str, cx: &mut Context<Self>) {
        if self.doc.index_of(cell).is_some() {
            self.activate_cell(cell, cx);
            self.reveal(cell, Reveal::Select, cx);
        }
    }

    /// Run All reached an AI cell: it keeps its stored result (models never
    /// run as part of a notebook run) and the queue moves on.
    pub fn ai_skip_queued(&mut self, id: &str, cx: &mut Context<Self>) {
        self.dispatch_result(id, super::view::DispatchOutcome::Started, cx);
        self.cell_finished(id, true, cx);
    }

    /// Connection facts from the notebook's own knowledge (engine and
    /// environment of each mapped slot); the host's richer facts win.
    pub fn ai_connection_facts(
        &self,
    ) -> BTreeMap<String, sqlly_services::notebook_security::ConnectionFacts> {
        let mut out = BTreeMap::new();
        let fact = |c: &super::connection::ConnectionChoice| {
            sqlly_services::notebook_security::ConnectionFacts {
                profile_id: Some(c.profile_id.clone()),
                engine: Some(c.database_type),
                environment: c.environment,
                ..Default::default()
            }
        };
        for slot in &self.doc.model().connection_refs {
            if let Some(choice) = slot
                .profile_id
                .as_deref()
                .and_then(|id| self.choices.iter().find(|c| c.profile_id == id))
            {
                out.insert(slot.id.clone(), fact(choice));
            }
        }
        if let Some(tab) = &self.tab_connection {
            let mut facts = fact(tab);
            facts.profile_id = None;
            out.insert(CURRENT_CONNECTION.to_string(), facts);
        }
        out
    }
}

/// A cancel flag for a new run.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub fn new_cancel() -> CancelFlag {
    Arc::new(AtomicBool::new(false))
}

/// Where notebook AI keeps its action log and cloud consents: the services'
/// `notebook::data_dir()/ai`, or the test root during tests, so a test's
/// consent never lands in the real data folder.
pub(crate) fn ai_dir() -> std::path::PathBuf {
    crate::notebook::autosave::notebooks_dir().join("ai")
}
