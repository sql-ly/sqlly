//! Execution plan pane.
//!
//! The pane reads a plan four ways over one shared selection:
//!
//! - **Summary**: the plan told as numbered steps in execution order, beside
//!   the ranked findings. Selecting a step or a finding highlights the other.
//! - **Diagram**: the operator tree as cards on a zoomable canvas, coloured
//!   by a switchable metric.
//! - **Operators**: every operator in a sortable grid.
//! - **Time** (or **Cost** on unmeasured plans): a flame chart with totals by
//!   operator, operator family and table.
//!
//! Everything shown comes from `sqlly_services::plans`; this module only
//! arranges it. Each statement is analysed once per capture ([`model`]) and
//! the diagram's laid-out scene is cached ([`diagram`]), so painting does no
//! plan work.

pub(crate) mod compact;
mod compare;
mod details;
mod diagram;
mod flame;
mod glyphs;
mod model;
pub(crate) mod schema;
mod summary;
mod table;
#[cfg(test)]
mod tests;

use std::cell::Cell;
use std::collections::HashSet;

use gpui::{
    AnyElement, Bounds, Context, Entity, EventEmitter, FocusHandle, Hsla, IntoElement, MouseButton,
    Pixels, Render, ScrollHandle, Styled, Subscription, Window, canvas, div, prelude::*, px,
};
use gpui_component::Disableable as _;
use gpui_component::Selectable as _;
use gpui_component::Sizable as _;
use gpui_component::button::Button;
use gpui_component::input::InputState;
use sqlly_core::DisplayMessage;
use sqlly_services::plans::{
    DEFAULT_MAX_COMPARISON_NODES, ExecutionPlan, FindingSeverity, NumberStyle, PlanChangeReport,
    PlanImportError, SchemaContext, explain_plan_changes,
};

use crate::theme::Theme;

pub(crate) use model::BatchPlans;
pub use model::StatementPlans;
use model::ai_explain_prompt_for;

/// Which capture produced the plan being shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanKind {
    /// The engine planned the query without running it.
    Estimated,
    /// The query actually ran; row counts (and timings, where the engine
    /// reports them) are real.
    Actual,
}

impl PlanKind {
    pub fn label(self) -> String {
        let key = match self {
            PlanKind::Estimated => "query.execution_plan.kind_estimated",
            PlanKind::Actual => "query.execution_plan.kind_actual",
        };
        crate::localization::text(key)
    }
}

/// How the pane shows a captured plan. All four share one selected operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanViewMode {
    Summary,
    Diagram,
    Operators,
    Breakdown,
}

/// Pane lifecycle. `Empty` is the pre-capture prompt; `Capturing` shows while
/// the server prepares the plan; `Analysing` while a very large plan is
/// analysed off the UI thread; `Failed` keeps the server's message visible
/// and copyable; `AnalysisFailed` says the analysis itself broke;
/// `ImportFailed` explains why a pasted or opened document could not be
/// read.
#[derive(Debug, Clone, PartialEq)]
pub enum PlanPaneStatus {
    Empty,
    Capturing,
    Analysing { node_count: usize },
    AnalysisFailed,
    Ready,
    Failed(String),
    ImportFailed(Box<PlanImportError>),
}

/// Events the pane raises. `ResultsView` subscribes and forwards the capture
/// request up to the app, which owns the services needed to run the query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionPlanViewEvent {
    /// The Re-capture button was pressed; re-capture the same kind of plan
    /// the pane is showing.
    CaptureRequested(PlanKind),
    /// Open a saved plan file into this pane.
    OpenPlanRequested,
    /// Paste a plan document from the clipboard.
    PastePlanRequested,
    /// Open generated finding SQL in a new editor tab without running it.
    OpenSqlInNewTab(String),
    /// A plan was analysed and installed (`failed: false`), or its
    /// background analysis broke (`failed: true`). Everything that shows the
    /// finding count or a digest of the plan re-reads it on this event: a
    /// large plan's findings arrive after the call that installed it
    /// returned.
    AnalysisFinished { failed: bool },
}

impl EventEmitter<ExecutionPlanViewEvent> for ExecutionPlanView {}

/// Capture history is per query tab. Count and node caps bound retained memory
/// and comparison work even when a server returns a pathological plan.
const MAX_CAPTURE_HISTORY: usize = 8;

/// Plans with more operators than this are analysed off the UI thread: the
/// story, findings and layouts of a several-thousand-operator plan take long
/// enough to stall a frame.
const BACKGROUND_ANALYSIS_NODES: usize = 500;

/// Below this pane width the Summary stacks findings above the story instead
/// of showing them side by side.
const WIDE_LAYOUT_MIN: f32 = 760.0;

/// Width of the operator details panel beside the diagram, table and chart.
const DETAILS_WIDTH: f32 = 320.0;

#[derive(Debug, Clone)]
struct PlanCapture {
    sequence: u64,
    kind: PlanKind,
    query_context: String,
    plans: Vec<ExecutionPlan>,
}

/// Why two captures cannot be compared. Kept as data so the message follows
/// the interface language.
#[derive(Debug, Clone, PartialEq)]
enum CompareIssue {
    BaselineRequired,
    ComparisonRequired,
    DistinctRequired,
    DifferentQuery,
    Core(DisplayMessage),
}

impl CompareIssue {
    fn text(&self) -> String {
        let key = match self {
            CompareIssue::BaselineRequired => "query.execution_plan.baseline_required",
            CompareIssue::ComparisonRequired => "query.execution_plan.comparison_required",
            CompareIssue::DistinctRequired => "query.execution_plan.distinct_captures_required",
            CompareIssue::DifferentQuery => "query.execution_plan.different_query_context",
            CompareIssue::Core(message) => return crate::localization::message(message),
        };
        crate::localization::text(key)
    }
}

pub struct ExecutionPlanView {
    status: PlanPaneStatus,
    kind: PlanKind,
    /// The line that says which statement of a batch matters.
    batch_headline: Option<DisplayMessage>,
    statements: Vec<StatementPlans>,
    captures: Vec<PlanCapture>,
    next_capture_sequence: u64,
    /// Bumped by every install and failure, so a background analysis that
    /// finishes after a newer plan or error arrived is dropped.
    install_generation: u64,
    /// Sequence of the capture `statements` was built from, when it was
    /// retained.
    shown_capture: Option<u64>,
    comparison_mode: bool,
    baseline_capture: usize,
    comparison_capture: usize,
    comparison_result: Option<Result<PlanChangeReport, CompareIssue>>,
    /// Operator count of the last capture that was too large to keep for
    /// comparison.
    oversized_capture: Option<usize>,
    active_statement: usize,
    view_mode: PlanViewMode,
    /// The operator every view agrees is selected.
    selected_node: Option<i32>,
    selected_finding: Option<usize>,
    expanded_findings: HashSet<usize>,
    show_minor_steps: bool,
    show_raw_properties: bool,
    diagram: diagram::DiagramState,
    flame: flame::FlameState,
    table: table::TableState,
    search_input: Option<Entity<InputState>>,
    search_subscription: Option<Subscription>,
    story_scroll: ScrollHandle,
    findings_scroll: ScrollHandle,
    details_scroll: ScrollHandle,
    /// Where the pane body was last painted; decides the Summary layout.
    body_bounds: Cell<Option<Bounds<Pixels>>>,
    focus_handle: FocusHandle,
    /// Which capture kinds the active engine cannot produce, with the
    /// reason. Pushed down by the app; drives the Re-capture button's
    /// disabled state and the empty-state text.
    capture_gate: crate::capabilities::PlanCaptureGate,
}

impl ExecutionPlanView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            status: PlanPaneStatus::Empty,
            kind: PlanKind::Estimated,
            batch_headline: None,
            statements: Vec::new(),
            captures: Vec::new(),
            next_capture_sequence: 1,
            install_generation: 0,
            shown_capture: None,
            comparison_mode: false,
            baseline_capture: 0,
            comparison_capture: 0,
            comparison_result: None,
            oversized_capture: None,
            active_statement: 0,
            view_mode: PlanViewMode::Summary,
            selected_node: None,
            selected_finding: None,
            expanded_findings: HashSet::new(),
            show_minor_steps: false,
            show_raw_properties: false,
            diagram: diagram::DiagramState::default(),
            flame: flame::FlameState::default(),
            table: table::TableState::default(),
            search_input: None,
            search_subscription: None,
            story_scroll: ScrollHandle::new(),
            findings_scroll: ScrollHandle::new(),
            details_scroll: ScrollHandle::new(),
            body_bounds: Cell::new(None),
            focus_handle: cx.focus_handle(),
            capture_gate: crate::capabilities::PlanCaptureGate::default(),
        }
    }

    /// Record which plan kinds the active connection can capture. Cheap to
    /// call every frame: it only notifies when the gate changes.
    pub fn set_capture_gate(
        &mut self,
        gate: crate::capabilities::PlanCaptureGate,
        cx: &mut Context<Self>,
    ) {
        if self.capture_gate != gate {
            self.capture_gate = gate;
            cx.notify();
        }
    }

    /// Why Re-capture is disabled right now, or `None` when it can run.
    pub fn recapture_unavailable_reason(&self) -> Option<&str> {
        self.capture_gate.reason(self.kind)
    }

    /// The empty pane's guidance: how to capture a plan here, or why this
    /// engine cannot produce one.
    pub fn empty_state_text(&self) -> String {
        if let Some(reason) = self.capture_gate.reason(PlanKind::Estimated) {
            reason.to_string()
        } else if self.capture_gate.reason(PlanKind::Actual).is_some() {
            crate::localization::text("query.execution_plan.empty_estimated_only")
        } else {
            crate::localization::text("query.execution_plan.empty_all_modes")
        }
    }

    pub fn is_capturing(&self) -> bool {
        self.status == PlanPaneStatus::Capturing
    }

    /// True while a large plan is being analysed in the background. Its
    /// finding count is not known yet.
    pub fn is_analysing(&self) -> bool {
        matches!(self.status, PlanPaneStatus::Analysing { .. })
    }

    /// Findings across all statements: the number the results footer shows.
    pub fn finding_count(&self) -> usize {
        self.statements.iter().map(|s| s.findings.len()).sum()
    }

    /// A bounded, plain-text digest of the captured plan for the AI drawer:
    /// the story headline, the ranked findings and the most expensive
    /// operators. `None` when no plan has been captured. The digest is capped
    /// so a large plan cannot bloat the prompt, and it never includes the raw
    /// plan document.
    pub fn ai_explain_prompt(&self) -> Option<String> {
        ai_explain_prompt_for(&self.statements)
    }

    /// Mark a capture as in flight. The previous plan stays visible until the
    /// new one lands (or fails), so a re-capture never blanks the pane.
    pub fn set_capturing(&mut self, kind: PlanKind, cx: &mut Context<Self>) {
        self.kind = kind;
        self.status = PlanPaneStatus::Capturing;
        cx.notify();
    }

    /// Install freshly parsed plans and keep a bounded comparison snapshot
    /// for this query context. `schema` holds definitions of the tables the
    /// plan touches, so the findings can check column types and keys.
    /// Resets selection and view state because node ids and finding indexes
    /// from the old plan do not carry over.
    pub fn set_plans_with_schema(
        &mut self,
        kind: PlanKind,
        query_context: String,
        plans: Vec<ExecutionPlan>,
        schema: Option<SchemaContext>,
        cx: &mut Context<Self>,
    ) {
        let query_text = Some(query_context.clone());
        self.build_and_install(kind, query_context, plans, query_text, schema, cx);
    }

    /// Install plans read from a pasted or opened document. `source` names
    /// where they came from; it is not SQL, so table aliases are resolved
    /// from the statement text the document itself carries, if any.
    pub fn set_imported_plans(
        &mut self,
        kind: PlanKind,
        source: String,
        plans: Vec<ExecutionPlan>,
        cx: &mut Context<Self>,
    ) {
        self.build_and_install(kind, source, plans, None, None, cx);
    }

    /// Analyse `plans` and install them: in place for ordinary plans, on the
    /// background executor for very large ones. The pane keeps showing what
    /// it showed until the analysis lands.
    fn build_and_install(
        &mut self,
        kind: PlanKind,
        query_context: String,
        plans: Vec<ExecutionPlan>,
        query_text: Option<String>,
        schema: Option<SchemaContext>,
        cx: &mut Context<Self>,
    ) {
        self.install_generation += 1;
        let generation = self.install_generation;
        let nodes: usize = plans.iter().map(|plan| plan.nodes.len()).sum();
        let style = plan_number_style();
        if nodes <= BACKGROUND_ANALYSIS_NODES {
            let batch =
                BatchPlans::build_styled(style, plans, query_text.as_deref(), schema.as_ref());
            self.install(kind, query_context, batch, cx);
            return;
        }
        // The old plan goes now: its findings and digest must not stand in
        // for the new plan's while the analysis runs.
        self.kind = kind;
        self.statements.clear();
        self.batch_headline = None;
        self.comparison_mode = false;
        self.reset_statement_state();
        self.status = PlanPaneStatus::Analysing { node_count: nodes };
        cx.notify();
        #[cfg(test)]
        let fail = query_context == tests::PANIC_IN_ANALYSIS;
        let analysis = cx.background_spawn(async move {
            // A panic in the analysis must not leave the pane analysing
            // forever; it becomes a message instead.
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                #[cfg(test)]
                if fail {
                    panic!("test: analysis failed");
                }
                BatchPlans::build_styled(style, plans, query_text.as_deref(), schema.as_ref())
            }))
        });
        cx.spawn(async move |this, cx| {
            let result = analysis.await;
            this.update(cx, |view, cx| {
                if view.install_generation != generation {
                    return;
                }
                match result {
                    Ok(batch) => view.install(kind, query_context, batch, cx),
                    Err(_) => {
                        tracing::error!(nodes, "execution plan analysis panicked");
                        view.status = PlanPaneStatus::AnalysisFailed;
                        cx.emit(ExecutionPlanViewEvent::AnalysisFinished { failed: true });
                        cx.notify();
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    fn install(
        &mut self,
        kind: PlanKind,
        query_context: String,
        batch: BatchPlans,
        cx: &mut Context<Self>,
    ) {
        self.kind = kind;
        // History keeps the alias-resolved plans, so a comparison describes
        // tables by the same names the views do.
        let plans: Vec<ExecutionPlan> = batch
            .statements
            .iter()
            .map(|statement| statement.plan.clone())
            .collect();
        self.batch_headline = batch.headline;
        self.statements = batch.statements;
        let node_count: usize = plans.iter().map(|plan| plan.nodes.len()).sum();
        self.shown_capture = None;
        if !plans.is_empty() && node_count <= DEFAULT_MAX_COMPARISON_NODES {
            self.captures.push(PlanCapture {
                sequence: self.next_capture_sequence,
                kind,
                query_context: normalize_query_context(&query_context),
                plans,
            });
            self.shown_capture = Some(self.next_capture_sequence);
            self.next_capture_sequence += 1;
            if self.captures.len() > MAX_CAPTURE_HISTORY {
                self.captures.remove(0);
            }
            self.comparison_capture = self.captures.len() - 1;
            self.baseline_capture = self.comparison_capture.saturating_sub(1);
            self.oversized_capture = None;
        } else if node_count > DEFAULT_MAX_COMPARISON_NODES {
            self.oversized_capture = Some(node_count);
        }
        self.comparison_mode = false;
        self.comparison_result = None;
        self.active_statement = self.heaviest_statement();
        self.view_mode = PlanViewMode::Summary;
        self.reset_statement_state();
        self.status = PlanPaneStatus::Ready;
        cx.emit(ExecutionPlanViewEvent::AnalysisFinished { failed: false });
        cx.notify();
    }

    pub fn set_failed(&mut self, kind: PlanKind, message: String, cx: &mut Context<Self>) {
        self.kind = kind;
        self.install_generation += 1;
        self.statements.clear();
        self.batch_headline = None;
        self.status = PlanPaneStatus::Failed(message);
        cx.notify();
    }

    /// A pasted or opened document could not be read as a plan. The pane
    /// shows what it looked like, where reading stopped and how to capture a
    /// plan the importer accepts.
    pub fn set_import_failed(&mut self, error: PlanImportError, cx: &mut Context<Self>) {
        self.kind = PlanKind::Estimated;
        self.install_generation += 1;
        self.statements.clear();
        self.batch_headline = None;
        self.status = PlanPaneStatus::ImportFailed(Box::new(error));
        cx.notify();
    }

    /// The statement to open on: the one the batch spends the most on, so a
    /// long script does not open on its `SET NOCOUNT ON`.
    fn heaviest_statement(&self) -> usize {
        self.statements
            .iter()
            .enumerate()
            .filter_map(|(index, statement)| Some((index, statement.batch_share?)))
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
            .map_or(0, |(index, _)| index)
    }

    /// Forget everything tied to one statement's node ids.
    fn reset_statement_state(&mut self) {
        self.selected_node = None;
        self.selected_finding = None;
        self.expanded_findings.clear();
        self.show_minor_steps = false;
        self.show_raw_properties = false;
        self.diagram.reset();
        // The matches are gone with the statement, so the box that showed
        // the query goes too; the diagram makes a fresh one when it renders.
        self.search_input = None;
        self.search_subscription = None;
        self.flame = flame::FlameState::default();
        self.table.reset();
        self.story_scroll = ScrollHandle::new();
        self.findings_scroll = ScrollHandle::new();
        self.details_scroll = ScrollHandle::new();
    }

    fn active(&self) -> Option<&StatementPlans> {
        self.statements.get(self.active_statement)
    }

    /// The warning shown when the last capture was too large to keep.
    fn capture_history_notice(&self) -> Option<String> {
        let node_count = self.oversized_capture?;
        Some(crate::localization::format(
            "query.execution_plan.capture_not_retained",
            &[
                ("node_count", &node_count.to_string()),
                ("limit", &DEFAULT_MAX_COMPARISON_NODES.to_string()),
            ],
        ))
    }

    fn capture_label(&self, index: usize) -> String {
        self.captures.get(index).map_or_else(
            || crate::localization::text("query.execution_plan.capture_unavailable"),
            |capture| {
                crate::localization::format(
                    "query.execution_plan.capture_label",
                    &[
                        ("sequence", &capture.sequence.to_string()),
                        ("kind", &capture.kind.label()),
                    ],
                )
            },
        )
    }

    fn selected_comparison(&self) -> Result<PlanChangeReport, CompareIssue> {
        let baseline = self
            .captures
            .get(self.baseline_capture)
            .ok_or(CompareIssue::BaselineRequired)?;
        let comparison = self
            .captures
            .get(self.comparison_capture)
            .ok_or(CompareIssue::ComparisonRequired)?;
        if self.baseline_capture == self.comparison_capture {
            return Err(CompareIssue::DistinctRequired);
        }
        if !baseline.query_context.is_empty()
            && !comparison.query_context.is_empty()
            && baseline.query_context != comparison.query_context
        {
            return Err(CompareIssue::DifferentQuery);
        }
        explain_plan_changes(
            &baseline.plans,
            &comparison.plans,
            DEFAULT_MAX_COMPARISON_NODES,
        )
        .map_err(|error| CompareIssue::Core(error.message()))
    }

    fn refresh_comparison(&mut self) {
        self.comparison_result = Some(self.selected_comparison());
    }

    fn toggle_comparison(&mut self, cx: &mut Context<Self>) {
        if self.captures.len() < 2 {
            return;
        }
        self.comparison_mode = !self.comparison_mode;
        if self.comparison_mode {
            self.refresh_comparison();
        }
        cx.notify();
    }

    fn cycle_baseline(&mut self, cx: &mut Context<Self>) {
        if self.captures.len() < 2 {
            return;
        }
        self.baseline_capture = next_distinct_index(
            self.baseline_capture,
            self.comparison_capture,
            self.captures.len(),
        );
        self.refresh_comparison();
        cx.notify();
    }

    fn cycle_comparison(&mut self, cx: &mut Context<Self>) {
        if self.captures.len() < 2 {
            return;
        }
        self.comparison_capture = next_distinct_index(
            self.comparison_capture,
            self.baseline_capture,
            self.captures.len(),
        );
        self.refresh_comparison();
        cx.notify();
    }

    fn select_statement(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.statements.len() && index != self.active_statement {
            self.active_statement = index;
            self.reset_statement_state();
            cx.notify();
        }
    }

    fn set_view_mode(&mut self, mode: PlanViewMode, cx: &mut Context<Self>) {
        if self.view_mode == mode {
            return;
        }
        self.view_mode = mode;
        if mode == PlanViewMode::Diagram {
            self.reveal_selected_in_diagram();
        }
        cx.notify();
    }

    /// Select an operator everywhere. `None` clears the selection.
    fn select_node(&mut self, node_id: Option<i32>, cx: &mut Context<Self>) {
        if self.selected_node != node_id {
            self.selected_node = node_id;
            self.show_raw_properties = false;
            self.details_scroll = ScrollHandle::new();
        }
        // A finding stays selected only while the operator is one of its own.
        if let (Some(index), Some(active)) = (self.selected_finding, self.active()) {
            let keeps = node_id.is_some_and(|id| {
                active
                    .findings
                    .get(index)
                    .is_some_and(|finding| model::finding_touches(finding, id))
            });
            if !keeps {
                self.selected_finding = None;
            }
        }
        if let Some(node_id) = node_id {
            self.reveal_step(node_id);
        }
        cx.notify();
    }

    /// Select a finding and the operator it is about.
    fn select_finding(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(finding) = self.active().and_then(|active| active.findings.get(index)) else {
            return;
        };
        let node = finding
            .node_id
            .or_else(|| finding.node_ids.first().copied());
        self.selected_finding = Some(index);
        if self.selected_node != node {
            self.selected_node = node;
            self.show_raw_properties = false;
        }
        if let Some(node) = node {
            self.reveal_step(node);
        }
        cx.notify();
    }

    fn toggle_finding(&mut self, index: usize, cx: &mut Context<Self>) {
        if !self.expanded_findings.insert(index) {
            self.expanded_findings.remove(&index);
        }
        self.select_finding(index, cx);
    }

    /// Jump to the diagram with `node_id` selected and in view.
    fn show_in_diagram(&mut self, node_id: i32, cx: &mut Context<Self>) {
        self.selected_node = Some(node_id);
        self.diagram
            .expand_to(node_id, self.statements.get(self.active_statement));
        self.view_mode = PlanViewMode::Diagram;
        self.reveal_selected_in_diagram();
        cx.notify();
    }

    fn reveal_selected_in_diagram(&mut self) {
        if let Some(node_id) = self.selected_node {
            self.diagram.reveal_pending.set(Some(node_id));
        }
    }

    /// Nodes to emphasise because of the selected finding.
    fn finding_nodes(&self) -> HashSet<i32> {
        let Some(finding) = self
            .selected_finding
            .and_then(|index| self.active()?.findings.get(index))
        else {
            return HashSet::new();
        };
        finding
            .node_ids
            .iter()
            .copied()
            .chain(finding.node_id)
            .collect()
    }

    fn mode_label(&self, mode: PlanViewMode) -> String {
        let timed = self
            .active()
            .is_none_or(|active| active.analysis.has_timing);
        let key = match mode {
            PlanViewMode::Summary => "query.execution_plan.mode_summary",
            PlanViewMode::Diagram => "query.execution_plan.mode_diagram",
            PlanViewMode::Operators => "query.execution_plan.mode_operators",
            PlanViewMode::Breakdown if timed => "query.execution_plan.mode_time",
            PlanViewMode::Breakdown => "query.execution_plan.mode_cost",
        };
        crate::localization::text(key)
    }

    pub(super) fn severity_color(severity: FindingSeverity) -> Hsla {
        match severity {
            FindingSeverity::Critical => Theme::error(),
            FindingSeverity::Warning => Theme::warning(),
            FindingSeverity::Info => Theme::info(),
        }
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let capturing = self.is_capturing();
        let has_plan = !self.statements.is_empty();
        div()
            .flex()
            .flex_row()
            .flex_shrink_0()
            .items_center()
            .h(px(s(30.0)))
            .px(px(s(8.0)))
            .gap(px(s(6.0)))
            .border_b_1()
            .border_color(Theme::border())
            .child(
                div()
                    .flex_shrink_0()
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text_dim())
                    .child(self.kind.label()),
            )
            .child(
                // Statement switcher. A large batch (dozens of statements)
                // would overflow the header, so the strip scrolls
                // horizontally while the buttons stay pinned.
                div()
                    .id("plan-statement-switcher")
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .overflow_x_scroll()
                    .flex()
                    .flex_row()
                    .items_center()
                    .children(if self.statements.len() > 1 {
                        self.statements
                            .iter()
                            .enumerate()
                            .map(|(i, statement)| self.render_statement_tab(i, statement, cx))
                            .collect::<Vec<_>>()
                    } else {
                        Vec::new()
                    }),
            )
            .when(has_plan && !self.comparison_mode, |header| {
                let modes = [
                    PlanViewMode::Summary,
                    PlanViewMode::Diagram,
                    PlanViewMode::Operators,
                    PlanViewMode::Breakdown,
                ];
                header.children(modes.into_iter().enumerate().map(|(i, mode)| {
                    let selected = self.view_mode == mode;
                    Button::new(("plan-mode", i))
                        .small()
                        .label(self.mode_label(mode))
                        .selected(selected)
                        .on_click(cx.listener(move |this, _, _window, cx| {
                            this.set_view_mode(mode, cx);
                        }))
                        .into_any_element()
                }))
            })
            .when(self.captures.len() >= 2, |header| {
                header.child(
                    Button::new("plan-compare-toggle")
                        .small()
                        .label(if self.comparison_mode {
                            crate::localization::text("query.execution_plan.show_plan_button")
                        } else {
                            crate::localization::text("query.execution_plan.compare_button")
                        })
                        .on_click(cx.listener(|this, _, _window, cx| this.toggle_comparison(cx))),
                )
            })
            .child(
                Button::new("plan-open")
                    .small()
                    .label(crate::localization::text(
                        "query.execution_plan.open_button",
                    ))
                    .on_click(cx.listener(|_, _, _window, cx| {
                        cx.emit(ExecutionPlanViewEvent::OpenPlanRequested);
                    })),
            )
            .child(
                Button::new("plan-paste")
                    .small()
                    .label(crate::localization::text(
                        "query.execution_plan.paste_button",
                    ))
                    .on_click(cx.listener(|_, _, _window, cx| {
                        cx.emit(ExecutionPlanViewEvent::PastePlanRequested);
                    })),
            )
            .child({
                // Re-capture repeats the kind of plan on screen; when the
                // active engine cannot produce that kind the button stays
                // visible but disabled, with the capability reason as its
                // tooltip.
                let unavailable = self.recapture_unavailable_reason().map(str::to_string);
                Button::new("plan-recapture")
                    .small()
                    .label(if capturing {
                        crate::localization::text("query.execution_plan.capturing_button")
                    } else {
                        crate::localization::text("query.execution_plan.recapture_button")
                    })
                    .disabled(capturing || unavailable.is_some())
                    .when_some(unavailable, |button, reason| button.tooltip(reason))
                    .on_click(cx.listener(|this, _, _window, cx| {
                        if !this.is_capturing() && this.recapture_unavailable_reason().is_none() {
                            cx.emit(ExecutionPlanViewEvent::CaptureRequested(this.kind));
                        }
                    }))
            })
    }

    fn render_statement_tab(
        &self,
        index: usize,
        statement: &StatementPlans,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let s = Theme::scaled;
        let is_active = index == self.active_statement;
        let statement_type = if statement.statement_type.is_empty() {
            crate::localization::text("query.execution_plan.statement_fallback")
        } else {
            statement.statement_type.clone()
        };
        let label = crate::localization::format(
            "query.execution_plan.statement_tab",
            &[
                ("number", &(index + 1).to_string()),
                ("statement_type", &statement_type),
            ],
        );
        div()
            .id(("plan-statement", index))
            .px(px(s(8.0)))
            .h_full()
            .flex()
            .flex_row()
            .flex_shrink_0()
            .items_center()
            .gap(px(s(5.0)))
            .cursor_pointer()
            .text_size(px(Theme::font_size()))
            .text_color(if is_active {
                Theme::text()
            } else {
                Theme::text_dim()
            })
            .when(is_active, |d| d.bg(Theme::selection()))
            .hover(|style| style.text_color(Theme::text()))
            .child(label)
            // How much of the batch this statement is, as a number: the tab
            // that matters is findable without opening each one.
            .when_some(statement.batch_share, |tab, share| {
                tab.child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.85)))
                        .text_color(Theme::text_dim())
                        .child(sqlly_services::plans::human_share(share)),
                )
            })
            .when(!statement.findings.is_empty(), |tab| {
                let worst = statement
                    .findings
                    .iter()
                    .map(|finding| finding.severity)
                    .min_by_key(|severity| model::severity_rank(*severity))
                    .unwrap_or(FindingSeverity::Info);
                tab.child(summary::severity_mark(worst, s(9.0)))
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    this.select_statement(index, cx);
                }),
            )
            .into_any_element()
    }

    fn render_ready(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        if self.comparison_mode {
            return self.render_comparison(cx);
        }
        if self.active().is_none() {
            return div()
                .p(px(Theme::scaled(12.0)))
                .text_size(px(Theme::font_size()))
                .text_color(Theme::text_dim())
                .child(crate::localization::text(
                    "query.execution_plan.findings_empty_plan",
                ))
                .into_any_element();
        }
        match self.view_mode {
            PlanViewMode::Summary => self.render_summary(cx),
            PlanViewMode::Diagram => self.render_diagram(window, cx),
            PlanViewMode::Operators => self.render_table(cx),
            PlanViewMode::Breakdown => self.render_breakdown(cx),
        }
    }

    fn render_import_failure(&self, error: &PlanImportError) -> AnyElement {
        let s = Theme::scaled;
        let headline = match &error.format_label {
            Some(label) => crate::localization::format(
                "query.execution_plan.import_failed_as",
                &[("format", label)],
            ),
            None => crate::localization::text("query.execution_plan.import_failed_unknown"),
        };
        let position = match (error.line, error.column) {
            (Some(line), Some(column)) => Some(crate::localization::format(
                "query.execution_plan.import_position",
                &[("line", &line.to_string()), ("column", &column.to_string())],
            )),
            (Some(line), None) => Some(crate::localization::format(
                "query.execution_plan.import_line",
                &[("line", &line.to_string())],
            )),
            _ => None,
        };
        div()
            .id("plan-import-error")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .w_full()
            .overflow_y_scroll()
            .p(px(s(16.0)))
            .gap(px(s(8.0)))
            .child(
                div()
                    .flex_shrink_0()
                    .whitespace_normal()
                    .text_size(px(Theme::font_size_ratio(1.15)))
                    .text_color(Theme::error())
                    .child(headline),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .whitespace_normal()
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text())
                    .child(error.detail.clone()),
            )
            .when_some(position, |el, position| {
                el.child(
                    div()
                        .flex_shrink_0()
                        .text_size(px(Theme::font_size()))
                        .text_color(Theme::text_dim())
                        .font_family(Theme::editor_font_family())
                        .child(position),
                )
            })
            .when_some(error.hint.clone(), |el, hint| {
                el.child(
                    div()
                        .flex_shrink_0()
                        .mt(px(s(4.0)))
                        .p(px(s(10.0)))
                        .rounded(px(s(4.0)))
                        .border_1()
                        .border_color(Theme::border())
                        .bg(Theme::bg())
                        .flex()
                        .flex_col()
                        .gap(px(s(4.0)))
                        .child(
                            div()
                                .text_size(px(Theme::font_size_ratio(0.85)))
                                .text_color(Theme::text_dim())
                                .child(crate::localization::text(
                                    "query.execution_plan.import_hint_heading",
                                )),
                        )
                        .child(
                            div()
                                .whitespace_normal()
                                .text_size(px(Theme::font_size()))
                                .text_color(Theme::text())
                                .child(hint),
                        ),
                )
            })
            .into_any_element()
    }

    fn on_key_down(
        &mut self,
        event: &gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.status != PlanPaneStatus::Ready || self.comparison_mode {
            return;
        }
        // Typing in the search box is text, not navigation.
        if self.search_has_focus(window, cx) {
            return;
        }
        let key = event.keystroke.key.as_str();
        let handled = match self.view_mode {
            PlanViewMode::Summary => self.summary_key(key, cx),
            PlanViewMode::Diagram => self.diagram_key(key, cx),
            PlanViewMode::Operators | PlanViewMode::Breakdown => false,
        };
        if handled {
            cx.stop_propagation();
        }
    }
}

impl Render for ExecutionPlanView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Numbers formatted while rendering (shares, times, the diagram's
        // box lines) follow the interface language like the analysed text.
        sqlly_services::plans::with_number_style(plan_number_style(), || {
            self.render_pane(window, cx)
        })
    }
}

impl ExecutionPlanView {
    fn render_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) -> gpui::Div {
        let s = Theme::scaled;
        let body = match self.status.clone() {
            PlanPaneStatus::Empty => div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .p(px(s(16.0)))
                .whitespace_normal()
                .text_size(px(Theme::font_size()))
                .text_color(Theme::text_dim())
                .child(self.empty_state_text())
                .into_any_element(),
            // The first capture has nothing to keep on screen; a re-capture
            // leaves the previous plan up and only the button says so.
            PlanPaneStatus::Capturing if self.statements.is_empty() => div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .p(px(s(16.0)))
                .text_size(px(Theme::font_size()))
                .text_color(Theme::text_dim())
                .child(crate::localization::format(
                    "query.execution_plan.capturing_status",
                    &[("kind", &self.kind.label().to_lowercase())],
                ))
                .into_any_element(),
            // Server errors run long (a full TDS token error names the server,
            // line, code and state), so the message wraps and scrolls rather
            // than running off both edges of a centred row.
            PlanPaneStatus::Failed(message) => div()
                .id("plan-error")
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .w_full()
                .overflow_y_scroll()
                .child(
                    div()
                        .flex_shrink_0()
                        .w_full()
                        .p(px(s(16.0)))
                        .whitespace_normal()
                        .text_size(px(Theme::font_size()))
                        .text_color(Theme::error())
                        .child(message),
                )
                .into_any_element(),
            // A large plan being analysed: the old plan is already gone, so
            // the pane says what it is doing rather than going blank.
            PlanPaneStatus::Analysing { node_count } => div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .p(px(s(16.0)))
                .whitespace_normal()
                .text_size(px(Theme::font_size()))
                .text_color(Theme::text_dim())
                .child(crate::localization::format(
                    "query.execution_plan.analysing_status",
                    &[("node_count", &node_count.to_string())],
                ))
                .into_any_element(),
            PlanPaneStatus::AnalysisFailed => div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .p(px(s(16.0)))
                .whitespace_normal()
                .text_size(px(Theme::font_size()))
                .text_color(Theme::error())
                .child(crate::localization::text(
                    "query.execution_plan.analysis_failed",
                ))
                .into_any_element(),
            PlanPaneStatus::ImportFailed(error) => self.render_import_failure(&error),
            PlanPaneStatus::Ready | PlanPaneStatus::Capturing => self.render_ready(window, cx),
        };

        let entity = cx.entity();
        div()
            .key_context("ExecutionPlan")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key_down))
            .flex()
            .flex_col()
            .size_full()
            .bg(Theme::panel_bg())
            .child(self.render_header(cx))
            .child(
                div()
                    .relative()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    // Records the body's size for the next frame's layout
                    // choice; paints nothing.
                    .child(
                        canvas(
                            move |bounds, _window, cx| {
                                let view = entity.read(cx);
                                if view.body_bounds.get() != Some(bounds) {
                                    let had = view.body_bounds.replace(Some(bounds));
                                    let was_wide = had.is_some_and(|b| is_wide(b.size.width));
                                    if had.is_none() || was_wide != is_wide(bounds.size.width) {
                                        cx.notify(entity.entity_id());
                                    }
                                }
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full(),
                    )
                    .child(body),
            )
    }
}

/// How plan numbers separate thousands and decimals in the interface
/// language. The plan text is analysed once per capture, so a language
/// change shows in numbers from the next capture on; text formatted while
/// rendering follows at once.
pub(crate) fn plan_number_style() -> NumberStyle {
    use sqlly_i18n::Language;
    match crate::localization::current_language() {
        Language::German | Language::Spanish | Language::BrazilianPortuguese => NumberStyle {
            decimal: ',',
            group: '.',
        },
        // CLDR: a narrow no-break space in French, a no-break space in
        // Russian, so a number never splits across lines.
        Language::French => NumberStyle {
            decimal: ',',
            group: '\u{202F}',
        },
        Language::Russian => NumberStyle {
            decimal: ',',
            group: '\u{00A0}',
        },
        Language::English
        | Language::SimplifiedChinese
        | Language::TraditionalChinese
        | Language::Japanese
        | Language::Korean => NumberStyle::ENGLISH,
    }
}

fn is_wide(width: Pixels) -> bool {
    f32::from(width) >= Theme::scaled(WIDE_LAYOUT_MIN)
}

fn normalize_query_context(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn next_distinct_index(current: usize, excluded: usize, len: usize) -> usize {
    for offset in 1..=len {
        let candidate = (current + offset) % len;
        if candidate != excluded {
            return candidate;
        }
    }
    current
}

/// Flatten `text` to one line and clip it to `max_chars`.
pub(crate) fn truncate(text: &str, max_chars: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = flat.chars();
    let clipped: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{clipped}…")
    } else {
        clipped
    }
}

/// Blend `b` into `a` by `t` (0 keeps `a`, 1 gives `b`), in RGB.
pub(crate) fn mix(a: Hsla, b: Hsla, t: f32) -> Hsla {
    let t = t.clamp(0.0, 1.0);
    let (a, b) = (gpui::Rgba::from(a), gpui::Rgba::from(b));
    Hsla::from(gpui::Rgba {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: a.a + (b.a - a.a) * t,
    })
}

/// The colour for a 0..1 intensity: the theme's neutral border when cold,
/// through its warning colour, to its error colour when hot. Every view that
/// colours by a metric uses this one ramp, and always beside a number or a
/// bar so colour is never the only signal.
pub(crate) fn heat_color(heat: f32) -> Hsla {
    let heat = if heat.is_finite() {
        heat.clamp(0.0, 1.0)
    } else {
        0.0
    };
    if heat < 0.5 {
        mix(Theme::border_hover(), Theme::warning(), heat * 2.0)
    } else {
        mix(Theme::warning(), Theme::error(), (heat - 0.5) * 2.0)
    }
}
