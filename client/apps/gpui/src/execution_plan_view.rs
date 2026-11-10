//! Execution plan pane — the human-first plan surface.
//!
//! Findings first, ranked by how much they matter, each row carrying its
//! suggested action in the collapsed state. Operators follow, most expensive
//! first. Everything technical (node ids, cost figures, output lists, raw
//! warnings) stays one expansion away. Rendering follows DESIGN.md: the only
//! color is the semantic severity palette, flat hairline-separated rows, one
//! typeface.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};

use gpui::{
    AnyElement, Bounds, ClipboardItem, Context, Div, ElementId, EventEmitter, FocusHandle,
    IntoElement, MouseButton, MouseDownEvent, Pixels, Point, Render, ScrollHandle, Stateful,
    Styled, Window, canvas, div, prelude::*, px,
};
use gpui_component::Disableable as _;
use gpui_component::Selectable as _;
use gpui_component::Sizable as _;
use gpui_component::button::Button;
use sqlly_services::plans::{
    DEFAULT_MAX_COMPARISON_NODES, ExecutionPlan, FindingSeverity, OperatorDelta, OperatorDeltaKind,
    OperatorRow, PlanComparison, PlanDiagramLayout, PlanFinding, PlanTreeRow, compare_plans,
    findings_for_plan, human_count, operator_rows, plan_diagram_layout, plan_tree_rows,
    visible_tree_rows,
};

use crate::execution_plan_diagram::{DiagramPaint, paint_plan_diagram};
use crate::theme::Theme;

/// Which capture produced the plan being shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanKind {
    /// `SET SHOWPLAN_XML ON` — no query execution.
    Estimated,
    /// `SET STATISTICS XML ON` — the query actually ran; row counts are real.
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

/// How the pane shows a captured plan. Findings is the default; Tree and
/// Diagram share one selected operator for the detail panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanViewMode {
    Findings,
    Tree,
    Diagram,
}

impl PlanViewMode {
    fn label(self) -> String {
        let key = match self {
            PlanViewMode::Findings => "query.execution_plan.mode_findings",
            PlanViewMode::Tree => "query.execution_plan.mode_tree",
            PlanViewMode::Diagram => "query.execution_plan.mode_diagram",
        };
        crate::localization::text(key)
    }
}

/// Pane lifecycle. `Empty` is the pre-capture prompt; `Capturing` shows while
/// the server prepares the plan; `Failed` keeps the server's message visible
/// and copyable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanPaneStatus {
    Empty,
    Capturing,
    Ready,
    Failed(String),
}

/// One statement's plan, pre-digested for rendering: findings ranked, operator
/// rows sorted by cost. Built once per capture so render never re-derives.
#[derive(Debug, Clone)]
pub struct StatementPlans {
    pub statement_text: String,
    pub statement_type: String,
    pub findings: Vec<PlanFinding>,
    pub operators: Vec<OperatorRow>,
    pub tree_rows: Vec<PlanTreeRow>,
    pub diagram: PlanDiagramLayout,
    pub total_subtree_cost: f64,
}

impl StatementPlans {
    pub fn from_plan(plan: &ExecutionPlan) -> Self {
        Self {
            statement_text: plan.statement_text.clone(),
            statement_type: plan.statement_type.clone(),
            findings: findings_for_plan(plan),
            operators: operator_rows(plan),
            tree_rows: plan_tree_rows(plan),
            diagram: plan_diagram_layout(plan),
            total_subtree_cost: plan.total_subtree_cost,
        }
    }
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
}

impl EventEmitter<ExecutionPlanViewEvent> for ExecutionPlanView {}

/// Build the bounded AI prompt for a set of captured statement plans. Pure so
/// the digest (ranked findings + most-expensive operators, capped) is unit
/// tested without a window. Returns `None` when there are no statements.
fn ai_explain_prompt_for(statements: &[StatementPlans]) -> Option<String> {
    if statements.is_empty() {
        return None;
    }
    const MAX_FINDINGS: usize = 8;
    const MAX_OPERATORS: usize = 8;
    let mut out = String::from(
        "Explain this SQL execution plan in plain language: what is expensive, why, \
         and what could make it faster. Do not rewrite my SQL.\n",
    );

    let findings: Vec<&PlanFinding> = statements.iter().flat_map(|s| &s.findings).collect();
    if !findings.is_empty() {
        out.push_str("\nFindings:");
        for finding in findings.iter().take(MAX_FINDINGS) {
            out.push_str(&format!(
                "\n- [{:?}] {} — {} Suggested: {}",
                finding.severity,
                finding.summary.trim(),
                finding.why.trim(),
                finding.action.trim(),
            ));
        }
    }

    let mut operators: Vec<&OperatorRow> = statements.iter().flat_map(|s| &s.operators).collect();
    operators.sort_by(|a, b| {
        b.cost_percent
            .partial_cmp(&a.cost_percent)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    if !operators.is_empty() {
        out.push_str("\n\nMost expensive operators:");
        for op in operators.iter().take(MAX_OPERATORS) {
            out.push_str(&format!("\n- {}", op.summary.trim()));
        }
    }
    Some(out)
}

/// Rows in the keyboard-navigable sequence: every finding, then every
/// operator, of the active statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaneRow {
    Finding(usize),
    Operator(i32),
    TreeRow(i32),
}

/// Tallest an expanded row's detail is allowed to grow before it becomes a
/// scroll region of its own. An operator's detail carries its whole output
/// column list, which for a `SELECT *` over a wide table runs to hundreds of
/// lines — without a cap, expanding one row pushes every other row out of
/// reach.
const DETAIL_MAX_HEIGHT: f32 = 200.0;

/// Capture history is per query tab. Count and node caps bound retained memory
/// and comparison work even when a server returns a pathological plan.
const MAX_CAPTURE_HISTORY: usize = 8;

#[derive(Debug, Clone)]
struct PlanCapture {
    sequence: u64,
    kind: PlanKind,
    query_context: String,
    plans: Vec<ExecutionPlan>,
}

pub struct ExecutionPlanView {
    status: PlanPaneStatus,
    kind: PlanKind,
    statements: Vec<StatementPlans>,
    captures: Vec<PlanCapture>,
    next_capture_sequence: u64,
    comparison_mode: bool,
    baseline_capture: usize,
    comparison_capture: usize,
    comparison_result: Option<Result<PlanComparison, String>>,
    capture_history_notice: Option<String>,
    active_statement: usize,
    expanded_findings: HashSet<usize>,
    expanded_operators: HashSet<i32>,
    view_mode: PlanViewMode,
    collapsed_tree_nodes: HashSet<i32>,
    selected_tree_node: Option<i32>,
    diagram_pan: (f32, f32),
    diagram_bounds: Cell<Option<Bounds<Pixels>>>,
    /// Index into the flattened row sequence for keyboard navigation.
    selected_row: usize,
    /// Scroll position of the pane body, tracked so keyboard navigation can
    /// bring the selected row into view.
    body_scroll: ScrollHandle,
    /// Per-expanded-row scroll positions for the bounded detail regions,
    /// keyed the same way the expansion sets are. Only live while the row is
    /// expanded; tracking them is what lets a detail region decide whether it
    /// still has room to move before handing the wheel back to the body.
    finding_detail_scrolls: HashMap<usize, ScrollHandle>,
    operator_detail_scrolls: HashMap<i32, ScrollHandle>,
    focus_handle: FocusHandle,
    /// Which capture kinds the active engine cannot produce, with the
    /// reason. Pushed down by the app; drives the Re-capture button's
    /// disabled state and the empty-state text.
    capture_gate: crate::capabilities::PlanCaptureGate,
}

impl ExecutionPlanView {
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

    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            status: PlanPaneStatus::Empty,
            kind: PlanKind::Estimated,
            statements: Vec::new(),
            captures: Vec::new(),
            next_capture_sequence: 1,
            comparison_mode: false,
            baseline_capture: 0,
            comparison_capture: 0,
            comparison_result: None,
            capture_history_notice: None,
            active_statement: 0,
            expanded_findings: HashSet::new(),
            expanded_operators: HashSet::new(),
            view_mode: PlanViewMode::Findings,
            collapsed_tree_nodes: HashSet::new(),
            selected_tree_node: None,
            diagram_pan: (0.0, 0.0),
            diagram_bounds: Cell::new(None),
            selected_row: 0,
            body_scroll: ScrollHandle::new(),
            finding_detail_scrolls: HashMap::new(),
            operator_detail_scrolls: HashMap::new(),
            focus_handle: cx.focus_handle(),
            capture_gate: crate::capabilities::PlanCaptureGate::default(),
        }
    }

    pub fn is_capturing(&self) -> bool {
        self.status == PlanPaneStatus::Capturing
    }

    /// Findings across all statements — the number the results footer shows.
    pub fn finding_count(&self) -> usize {
        self.statements.iter().map(|s| s.findings.len()).sum()
    }

    /// A bounded, plain-text digest of the captured plan for the AI drawer:
    /// the ranked findings and the most expensive operators. `None` when no
    /// plan has been captured. The digest is capped so a large plan cannot
    /// bloat the prompt, and it never includes raw operator XML.
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

    /// Install freshly parsed plans and retain a bounded comparison snapshot
    /// for this query context. Resets expansion and selection because node ids
    /// and finding indexes from the old plan do not carry over.
    pub fn set_plans_for_context(
        &mut self,
        kind: PlanKind,
        query_context: String,
        plans: Vec<ExecutionPlan>,
        cx: &mut Context<Self>,
    ) {
        self.kind = kind;
        self.statements = plans.iter().map(StatementPlans::from_plan).collect();
        let node_count: usize = plans.iter().map(|plan| plan.nodes.len()).sum();
        if !plans.is_empty() && node_count <= DEFAULT_MAX_COMPARISON_NODES {
            self.captures.push(PlanCapture {
                sequence: self.next_capture_sequence,
                kind,
                query_context: normalize_query_context(&query_context),
                plans,
            });
            self.next_capture_sequence += 1;
            if self.captures.len() > MAX_CAPTURE_HISTORY {
                self.captures.remove(0);
            }
            self.comparison_capture = self.captures.len() - 1;
            self.baseline_capture = self.comparison_capture.saturating_sub(1);
            self.capture_history_notice = None;
        } else if node_count > DEFAULT_MAX_COMPARISON_NODES {
            self.capture_history_notice = Some(crate::localization::format(
                "query.execution_plan.capture_not_retained",
                &[
                    ("node_count", &node_count.to_string()),
                    ("limit", &DEFAULT_MAX_COMPARISON_NODES.to_string()),
                ],
            ));
        }
        self.comparison_mode = false;
        self.comparison_result = None;
        self.active_statement = 0;
        self.expanded_findings.clear();
        self.expanded_operators.clear();
        self.view_mode = PlanViewMode::Findings;
        self.collapsed_tree_nodes.clear();
        self.selected_tree_node = None;
        self.diagram_pan = (0.0, 0.0);
        self.diagram_bounds.set(None);
        self.finding_detail_scrolls.clear();
        self.operator_detail_scrolls.clear();
        self.selected_row = 0;
        self.status = PlanPaneStatus::Ready;
        cx.notify();
    }

    pub fn set_failed(&mut self, kind: PlanKind, message: String, cx: &mut Context<Self>) {
        self.kind = kind;
        self.statements.clear();
        self.status = PlanPaneStatus::Failed(message);
        cx.notify();
    }

    fn active(&self) -> Option<&StatementPlans> {
        self.statements.get(self.active_statement)
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

    fn selected_comparison(&self) -> Result<PlanComparison, String> {
        let baseline = self
            .captures
            .get(self.baseline_capture)
            .ok_or_else(|| crate::localization::text("query.execution_plan.baseline_required"))?;
        let comparison = self
            .captures
            .get(self.comparison_capture)
            .ok_or_else(|| crate::localization::text("query.execution_plan.comparison_required"))?;
        if self.baseline_capture == self.comparison_capture {
            return Err(crate::localization::text(
                "query.execution_plan.distinct_captures_required",
            ));
        }
        if !baseline.query_context.is_empty()
            && !comparison.query_context.is_empty()
            && baseline.query_context != comparison.query_context
        {
            return Err(crate::localization::text(
                "query.execution_plan.different_query_context",
            ));
        }
        compare_plans(
            &baseline.plans,
            &comparison.plans,
            DEFAULT_MAX_COMPARISON_NODES,
        )
        .map_err(|error| crate::localization::message(error.message()))
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

    #[cfg(test)]
    fn finding_sql_action_count(&self) -> usize {
        self.active()
            .map(|statement| {
                statement
                    .findings
                    .iter()
                    .filter(|finding| finding.action_sql.is_some())
                    .count()
                    * 2
            })
            .unwrap_or(0)
    }

    fn rows(&self) -> Vec<PaneRow> {
        let Some(active) = self.active() else {
            return Vec::new();
        };
        if self.view_mode == PlanViewMode::Tree {
            return visible_tree_rows(&active.tree_rows, &self.collapsed_tree_nodes)
                .into_iter()
                .map(|row| PaneRow::TreeRow(row.node_id))
                .collect();
        }
        let mut rows: Vec<PaneRow> = (0..active.findings.len()).map(PaneRow::Finding).collect();
        rows.extend(
            active
                .operators
                .iter()
                .map(|op| PaneRow::Operator(op.node_id)),
        );
        rows
    }

    /// Position of `row` among the pane body's direct children. Keyboard
    /// navigation uses it to bring the selected row into view, so it has to
    /// mirror the order `render_ready` appends children in.
    fn child_index_for_row(&self, row: PaneRow) -> Option<usize> {
        let active = self.active()?;
        let mut base = 0;
        if self.statements.len() > 1 && !active.statement_text.is_empty() {
            base += 1; // statement strip
        }
        if self.view_mode == PlanViewMode::Tree {
            let PaneRow::TreeRow(node_id) = row else {
                return None;
            };
            let visible = visible_tree_rows(&active.tree_rows, &self.collapsed_tree_nodes);
            let position = visible
                .iter()
                .position(|operator| operator.node_id == node_id)?;
            return Some(base + 1 + position);
        }
        // Either the "Findings (n)" heading or, with nothing to report, the
        // "This plan looks good" row — one child either way.
        base += 1;
        match row {
            PaneRow::Finding(index) => Some(base + index),
            PaneRow::Operator(node_id) => {
                let position = active
                    .operators
                    .iter()
                    .position(|operator| operator.node_id == node_id)?;
                // + 1 for the "Operators (n)" heading.
                Some(base + active.findings.len() + 1 + position)
            }
            PaneRow::TreeRow(_) => None,
        }
    }

    /// Scroll the keyboard selection into view. A no-op when the pane is not
    /// tall enough to have scrolled away from it.
    fn reveal_selected_row(&self) {
        let Some(row) = self.rows().get(self.selected_row).copied() else {
            return;
        };
        if let Some(index) = self.child_index_for_row(row) {
            self.body_scroll.scroll_to_item(index);
        }
    }

    fn select_statement(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.statements.len() && index != self.active_statement {
            self.active_statement = index;
            self.selected_row = 0;
            cx.notify();
        }
    }

    fn toggle_finding(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.expanded_findings.insert(index) {
            self.finding_detail_scrolls.entry(index).or_default();
        } else {
            self.expanded_findings.remove(&index);
            self.finding_detail_scrolls.remove(&index);
        }
        cx.notify();
    }

    fn toggle_operator(&mut self, node_id: i32, cx: &mut Context<Self>) {
        if self.expanded_operators.insert(node_id) {
            self.operator_detail_scrolls.entry(node_id).or_default();
        } else {
            self.expanded_operators.remove(&node_id);
            self.operator_detail_scrolls.remove(&node_id);
        }
        cx.notify();
    }

    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let count = self.rows().len();
        if count == 0 {
            return;
        }
        let next = (self.selected_row as isize + delta).clamp(0, count as isize - 1);
        self.selected_row = next as usize;
        self.reveal_selected_row();
        cx.notify();
    }

    fn toggle_selected(&mut self, cx: &mut Context<Self>) {
        let rows = self.rows();
        match rows.get(self.selected_row) {
            Some(PaneRow::Finding(i)) => self.toggle_finding(*i, cx),
            Some(PaneRow::Operator(id)) => self.toggle_operator(*id, cx),
            Some(PaneRow::TreeRow(id)) => self.toggle_tree_row(*id, cx),
            None => {}
        }
        self.reveal_selected_row();
    }

    fn toggle_tree_row(&mut self, node_id: i32, cx: &mut Context<Self>) {
        let Some(row) = self
            .active()
            .and_then(|active| active.tree_rows.iter().find(|row| row.node_id == node_id))
            .cloned()
        else {
            return;
        };
        if row.has_children {
            self.toggle_tree_collapse(node_id, cx);
        } else if self.selected_tree_node == Some(node_id) {
            self.selected_tree_node = None;
            cx.notify();
        } else {
            self.select_tree_node(node_id, cx);
        }
    }

    fn toggle_tree_collapse(&mut self, node_id: i32, cx: &mut Context<Self>) {
        if !self.collapsed_tree_nodes.insert(node_id) {
            self.collapsed_tree_nodes.remove(&node_id);
        }
        cx.notify();
    }

    fn select_tree_node(&mut self, node_id: i32, cx: &mut Context<Self>) {
        self.selected_tree_node = Some(node_id);
        if let Some(index) = self
            .rows()
            .iter()
            .position(|row| matches!(row, PaneRow::TreeRow(id) if *id == node_id))
        {
            self.selected_row = index;
        }
        self.operator_detail_scrolls.entry(node_id).or_default();
        cx.notify();
    }

    fn tree_parent_id(&self, node_id: i32) -> Option<i32> {
        let rows = &self.active()?.tree_rows;
        let index = rows.iter().position(|row| row.node_id == node_id)?;
        let depth = rows[index].depth;
        if depth == 0 {
            return None;
        }
        rows[..index]
            .iter()
            .rev()
            .find(|row| row.depth == depth - 1)
            .map(|row| row.node_id)
    }

    fn tree_row(&self, node_id: i32) -> Option<PlanTreeRow> {
        self.active()?
            .tree_rows
            .iter()
            .find(|row| row.node_id == node_id)
            .cloned()
    }

    fn handle_tree_left(&mut self, cx: &mut Context<Self>) {
        let Some(PaneRow::TreeRow(id)) = self.rows().get(self.selected_row).copied() else {
            return;
        };
        let Some(row) = self.tree_row(id) else {
            return;
        };
        if row.has_children && !self.collapsed_tree_nodes.contains(&id) {
            self.toggle_tree_collapse(id, cx);
            return;
        }
        if let Some(parent) = self.tree_parent_id(id) {
            self.select_tree_node(parent, cx);
            self.reveal_selected_row();
        }
    }

    fn handle_tree_right(&mut self, cx: &mut Context<Self>) {
        let Some(PaneRow::TreeRow(id)) = self.rows().get(self.selected_row).copied() else {
            return;
        };
        let Some(row) = self.tree_row(id) else {
            return;
        };
        if row.has_children && self.collapsed_tree_nodes.contains(&id) {
            self.toggle_tree_collapse(id, cx);
        }
    }

    fn set_view_mode(&mut self, mode: PlanViewMode, cx: &mut Context<Self>) {
        if self.view_mode == mode {
            return;
        }
        self.view_mode = mode;
        self.selected_row = 0;
        cx.notify();
    }

    fn on_diagram_click(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(active) = self.active() else {
            return;
        };
        let Some(bounds) = self.diagram_bounds.get() else {
            return;
        };
        if let Some(id) = crate::execution_plan_diagram::node_at(
            &active.diagram,
            self.diagram_pan,
            bounds,
            position,
        ) {
            self.select_tree_node(id, cx);
        }
    }

    fn on_diagram_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.on_diagram_click(event.position, cx);
    }

    fn on_diagram_scroll_wheel(
        &mut self,
        event: &gpui::ScrollEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let delta = event.delta.pixel_delta(px(24.0));
        self.diagram_pan.0 += f32::from(delta.x);
        self.diagram_pan.1 += f32::from(delta.y);
        self.clamp_diagram_pan();
        cx.stop_propagation();
        cx.notify();
    }

    fn clamp_diagram_pan(&mut self) {
        let (view_w, view_h) = match self.diagram_bounds.get() {
            Some(b) => (f32::from(b.size.width), f32::from(b.size.height)),
            None => (800.0, 480.0),
        };
        let (width, height) = self
            .active()
            .map(|active| (active.diagram.width, active.diagram.height))
            .unwrap_or((0.0, 0.0));
        let min_x = (view_w - width).min(0.0);
        let min_y = (view_h - height).min(0.0);
        self.diagram_pan.0 = self.diagram_pan.0.clamp(min_x, 0.0);
        self.diagram_pan.1 = self.diagram_pan.1.clamp(min_y, 0.0);
    }

    fn selected_matches(&self, row: PaneRow) -> bool {
        self.rows().get(self.selected_row) == Some(&row)
    }

    fn severity_color(severity: FindingSeverity) -> gpui::Hsla {
        match severity {
            FindingSeverity::Critical => Theme::error(),
            FindingSeverity::Warning => Theme::warning(),
            FindingSeverity::Info => Theme::info(),
        }
    }
    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let capturing = self.is_capturing();
        div()
            .flex()
            .flex_row()
            .items_center()
            .h(px(s(28.0)))
            .px(px(s(8.0)))
            .gap(px(s(8.0)))
            .border_b_1()
            .border_color(Theme::border())
            .child(
                div()
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text_dim())
                    .child(self.kind.label()),
            )
            .child(
                // Statement switcher. A large batch (dozens of statements)
                // would overflow the header, so the strip scrolls
                // horizontally while the Re-capture button stays pinned.
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
                            .map(|(i, statement)| {
                                let is_active = i == self.active_statement;
                                div()
                                    .id(("plan-statement", i))
                                    .px(px(s(8.0)))
                                    .h_full()
                                    .flex()
                                    .flex_shrink_0()
                                    .items_center()
                                    .cursor_pointer()
                                    .text_size(px(Theme::font_size()))
                                    .text_color(if is_active {
                                        Theme::text()
                                    } else {
                                        Theme::text_dim()
                                    })
                                    .when(is_active, |d| d.bg(Theme::selection()))
                                    .hover(|style| style.text_color(Theme::text()))
                                    .child(crate::localization::format(
                                        "query.execution_plan.statement_tab",
                                        &[
                                            ("number", &(i + 1).to_string()),
                                            (
                                                "statement_type",
                                                &if statement.statement_type.is_empty() {
                                                    crate::localization::text(
                                                        "query.execution_plan.statement_fallback",
                                                    )
                                                } else {
                                                    statement.statement_type.clone()
                                                },
                                            ),
                                        ],
                                    ))
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |this, _, _, cx| {
                                            this.select_statement(i, cx);
                                        }),
                                    )
                                    .into_any_element()
                            })
                            .collect::<Vec<_>>()
                    } else {
                        Vec::new()
                    }),
            )
            .when(!self.comparison_mode, |header| {
                let modes = [
                    PlanViewMode::Findings,
                    PlanViewMode::Tree,
                    PlanViewMode::Diagram,
                ];
                header.children(modes.into_iter().enumerate().map(|(i, mode)| {
                    let selected = self.view_mode == mode;
                    Button::new(("plan-mode", i))
                        .small()
                        .label(mode.label())
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

    fn render_finding_row(
        &self,
        index: usize,
        finding: &PlanFinding,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let s = Theme::scaled;
        let expanded = self.expanded_findings.contains(&index);
        let selected = self.selected_matches(PaneRow::Finding(index));
        let severity = Self::severity_color(finding.severity);
        div()
            .id(("plan-finding", index))
            .flex()
            .flex_col()
            .flex_shrink_0()
            .w_full()
            .border_b_1()
            .border_color(Theme::border())
            .when(selected, |d| d.bg(Theme::selection()))
            .when(!selected, |d| d.hover(|style| style.bg(Theme::row_hover())))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_start()
                    .gap(px(s(8.0)))
                    .px(px(s(8.0)))
                    .py(px(s(6.0)))
                    .cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            this.toggle_finding(index, cx);
                        }),
                    )
                    .child(
                        div()
                            .w(px(s(14.0)))
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_color(Theme::text_dim())
                            .child(crate::icons::disclosure_chevron(expanded)),
                    )
                    .child(
                        div()
                            .w(px(s(8.0)))
                            .h(px(s(8.0)))
                            .mt(px(s(4.0)))
                            .flex_shrink_0()
                            .rounded_full()
                            .bg(severity),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(s(2.0)))
                            .child(
                                div()
                                    .whitespace_normal()
                                    .text_size(px(Theme::font_size()))
                                    .text_color(Theme::text())
                                    .child(crate::localization::message(&finding.summary)),
                            )
                            .child(
                                div()
                                    .whitespace_normal()
                                    .text_size(px(Theme::font_size()))
                                    .text_color(Theme::text_dim())
                                    .child(crate::localization::message(&finding.action)),
                            )
                            .when_some(finding.action_sql.clone(), |el, sql| {
                                el.child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .gap(px(s(6.0)))
                                        .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                            cx.stop_propagation()
                                        })
                                        .child(
                                            Button::new(("finding-copy-sql", index))
                                                .small()
                                                .label(crate::localization::text(
                                                    "query.execution_plan.copy_finding_sql_button",
                                                ))
                                                .on_click({
                                                    let sql = sql.clone();
                                                    move |_, _, cx| {
                                                        cx.write_to_clipboard(
                                                            ClipboardItem::new_string(sql.clone()),
                                                        );
                                                    }
                                                }),
                                        )
                                        .child(
                                            Button::new(("finding-open-tab", index))
                                                .small()
                                                .label(crate::localization::text(
                                                    "query.execution_plan.open_finding_tab_button",
                                                ))
                                                .on_click({
                                                    let sql = sql.clone();
                                                    cx.listener(move |_, _, _, cx| {
                                                        cx.emit(
                                                            ExecutionPlanViewEvent::OpenSqlInNewTab(
                                                                sql.clone(),
                                                            ),
                                                        );
                                                    })
                                                }),
                                        ),
                                )
                            }),
                    ),
            )
            .when(expanded, |d| {
                d.child(
                    detail_scroll_region(
                        ("plan-finding-detail", index),
                        self.finding_detail_scrolls.get(&index),
                        px(s(DETAIL_MAX_HEIGHT)),
                    )
                    .pl(px(s(26.0)))
                    .pr(px(s(8.0)))
                    .pb(px(s(6.0)))
                    .flex()
                    .flex_col()
                    .gap(px(s(4.0)))
                    .child(
                        div()
                            .flex_shrink_0()
                            .whitespace_normal()
                            .text_size(px(Theme::font_size()))
                            .text_color(Theme::text())
                            .child(crate::localization::message(&finding.why)),
                    )
                    .when_some(finding.action_sql.clone(), |el, sql| {
                        el.child(
                            div()
                                .flex_shrink_0()
                                .whitespace_normal()
                                .text_size(px(Theme::font_size()))
                                .text_color(Theme::text_dim())
                                .font_family(Theme::editor_font_family())
                                .child(sql),
                        )
                    })
                    .child(
                        div()
                            .flex_shrink_0()
                            .whitespace_normal()
                            .text_size(px(Theme::font_size()))
                            .text_color(Theme::text_dim())
                            .child(crate::localization::message(&finding.detail)),
                    ),
                )
            })
    }

    fn render_operator_row(
        &self,
        operator: &OperatorRow,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let s = Theme::scaled;
        let expanded = self.expanded_operators.contains(&operator.node_id);
        let selected = self.selected_matches(PaneRow::Operator(operator.node_id));
        let node_id = operator.node_id;
        div()
            .id(("plan-operator", node_id as usize))
            .flex()
            .flex_col()
            .flex_shrink_0()
            .w_full()
            .border_b_1()
            .border_color(Theme::border())
            .when(selected, |d| d.bg(Theme::selection()))
            .when(!selected, |d| d.hover(|style| style.bg(Theme::row_hover())))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_start()
                    .gap(px(s(8.0)))
                    .px(px(s(8.0)))
                    .py(px(s(5.0)))
                    .cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            this.toggle_operator(node_id, cx);
                        }),
                    )
                    .child(
                        div()
                            .w(px(s(14.0)))
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_color(Theme::text_dim())
                            .child(crate::icons::disclosure_chevron(expanded)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .whitespace_normal()
                            .text_size(px(Theme::font_size()))
                            .text_color(Theme::text())
                            .child(crate::localization::message(&operator.summary)),
                    )
                    .when(operator.has_warnings, |d| {
                        d.child(
                            div()
                                .w(px(s(8.0)))
                                .h(px(s(8.0)))
                                .mt(px(s(4.0)))
                                .flex_shrink_0()
                                .rounded_full()
                                .bg(Theme::warning()),
                        )
                    }),
            )
            .when(expanded, |d| {
                d.child(
                    detail_scroll_region(
                        ("plan-operator-detail", node_id as usize),
                        self.operator_detail_scrolls.get(&node_id),
                        px(s(DETAIL_MAX_HEIGHT)),
                    )
                    .pl(px(s(26.0)))
                    .pr(px(s(8.0)))
                    .pb(px(s(6.0)))
                    .child(
                        div()
                            .flex_shrink_0()
                            .whitespace_normal()
                            .text_size(px(Theme::font_size()))
                            .text_color(Theme::text_dim())
                            .child(crate::localization::message(&operator.detail)),
                    ),
                )
            })
    }

    fn render_tree_row(&self, row: &PlanTreeRow, cx: &mut Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let collapsed = self.collapsed_tree_nodes.contains(&row.node_id);
        let selected = self.selected_matches(PaneRow::TreeRow(row.node_id));
        let node_id = row.node_id;
        let has_children = row.has_children;
        let mismatch = row.actual_rows.is_some_and(|actual| {
            const MISMATCH_RATIO: f64 = 10.0;
            if row.estimated_rows <= 0.0 {
                actual > 0.0
            } else {
                let ratio = actual / row.estimated_rows;
                ratio >= MISMATCH_RATIO || ratio <= 1.0 / MISMATCH_RATIO
            }
        });
        div()
            .id(("plan-tree-row", node_id as usize))
            .flex()
            .flex_col()
            .flex_shrink_0()
            .w_full()
            .border_b_1()
            .border_color(Theme::border())
            .when(selected, |d| d.bg(Theme::selection()))
            .when(!selected, |d| d.hover(|style| style.bg(Theme::row_hover())))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(8.0)))
                    .pl(px(s(8.0 + 14.0 * row.depth as f32)))
                    .pr(px(s(8.0)))
                    .py(px(s(5.0)))
                    .child(
                        div()
                            .id(("plan-tree-chevron", node_id as usize))
                            .w(px(s(14.0)))
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_color(Theme::text_dim())
                            .when(has_children, |d| {
                                d.cursor_pointer()
                                    .child(crate::icons::disclosure_chevron(!collapsed))
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |this, _, _, cx| {
                                            this.toggle_tree_collapse(node_id, cx);
                                        }),
                                    )
                            }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(s(8.0)))
                            .cursor_pointer()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _, _, cx| {
                                    this.select_tree_node(node_id, cx);
                                }),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .whitespace_normal()
                                    .text_size(px(Theme::font_size()))
                                    .text_color(Theme::text())
                                    .child(row.title.clone()),
                            )
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .text_size(px(Theme::font_size()))
                                    .text_color(Theme::text_dim())
                                    .child(format!("{:.0}%", row.cost_percent)),
                            )
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .text_size(px(Theme::font_size()))
                                    .text_color(Theme::text_dim())
                                    .child(human_count(row.estimated_rows)),
                            )
                            .when_some(row.actual_rows, |d, actual| {
                                d.child(
                                    div()
                                        .flex_shrink_0()
                                        .text_size(px(Theme::font_size()))
                                        .text_color(if mismatch {
                                            Theme::warning()
                                        } else {
                                            Theme::text_dim()
                                        })
                                        .child(format!("→ {}", human_count(actual))),
                                )
                            })
                            .when(row.warning_count > 0, |d| {
                                d.child(
                                    div()
                                        .flex_shrink_0()
                                        .text_size(px(Theme::font_size()))
                                        .text_color(Theme::warning())
                                        .child(format!("⚠ {}", row.warning_count)),
                                )
                            }),
                    ),
            )
    }

    fn render_selected_detail(&self) -> Option<AnyElement> {
        let id = self.selected_tree_node?;
        let s = Theme::scaled;
        let active = self.active()?;
        let operator = active.operators.iter().find(|op| op.node_id == id)?;
        Some(
            detail_scroll_region(
                ("plan-tree-detail", id as usize),
                self.operator_detail_scrolls.get(&id),
                px(s(DETAIL_MAX_HEIGHT)),
            )
            .pl(px(s(26.0)))
            .pr(px(s(8.0)))
            .pb(px(s(6.0)))
            .child(
                div()
                    .flex_shrink_0()
                    .whitespace_normal()
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::message(&operator.detail)),
            )
            .into_any_element(),
        )
    }

    fn render_diagram(&self, cx: &mut Context<Self>) -> AnyElement {
        let s = Theme::scaled;
        let Some(active) = self.active() else {
            return div().into_any_element();
        };
        let paint = DiagramPaint {
            layout: active.diagram.clone(),
            selected: self.selected_tree_node,
            pan: self.diagram_pan,
            background: Theme::panel_bg(),
            node_bg: Theme::popup_bg(),
            node_border: Theme::border(),
            accent: Theme::accent(),
            warning: Theme::warning(),
            text: Theme::text(),
            text_dim: Theme::text_dim(),
            font_size: Theme::font_size(),
        };
        let entity = cx.entity();
        let mut body = div()
            .id("plan-diagram-body")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .w_full();
        if self.statements.len() > 1 && !active.statement_text.is_empty() {
            body = body.child(
                div()
                    .flex_shrink_0()
                    .px(px(s(8.0)))
                    .py(px(s(4.0)))
                    .border_b_1()
                    .border_color(Theme::border())
                    .whitespace_normal()
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text_dim())
                    .child(truncate(&active.statement_text, 120)),
            );
        }
        body = body
            .child(
                div()
                    .flex_shrink_0()
                    .px(px(s(8.0)))
                    .py(px(s(4.0)))
                    .text_size(px(Theme::font_size_ratio(0.8)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text(
                        "query.execution_plan.diagram_help",
                    )),
            )
            .child(
                div()
                    .id("plan-diagram-canvas")
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::on_diagram_mouse_down))
                    .on_scroll(cx.listener(Self::on_diagram_scroll_wheel))
                    .child(
                        canvas(
                            move |bounds, _window, cx| {
                                entity.read(cx).diagram_bounds.set(Some(bounds));
                                paint.clone()
                            },
                            move |bounds, paint, window, cx| {
                                paint_plan_diagram(&paint, bounds, window, cx)
                            },
                        )
                        .size_full(),
                    ),
            );
        if let Some(detail) = self.render_selected_detail() {
            body = body.child(detail);
        }
        body.into_any_element()
    }

    fn render_tree(&self, cx: &mut Context<Self>) -> AnyElement {
        let s = Theme::scaled;
        let Some(active) = self.active() else {
            return div()
                .p(px(s(12.0)))
                .text_size(px(Theme::font_size()))
                .text_color(Theme::text_dim())
                .child(crate::localization::text("query.execution_plan.tree_empty"))
                .into_any_element();
        };
        let visible = visible_tree_rows(&active.tree_rows, &self.collapsed_tree_nodes);
        let mut body = div()
            .id("plan-tree-body")
            .track_scroll(&self.body_scroll)
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .w_full()
            .overflow_y_scroll();
        if self.statements.len() > 1 && !active.statement_text.is_empty() {
            body = body.child(
                div()
                    .flex_shrink_0()
                    .px(px(s(8.0)))
                    .py(px(s(4.0)))
                    .border_b_1()
                    .border_color(Theme::border())
                    .whitespace_normal()
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text_dim())
                    .child(truncate(&active.statement_text, 120)),
            );
        }
        body = body.child(
            div()
                .flex_shrink_0()
                .px(px(s(8.0)))
                .pt(px(s(8.0)))
                .pb(px(s(4.0)))
                .text_size(px(Theme::font_size()))
                .text_color(Theme::text_dim())
                .child(crate::localization::format(
                    "query.execution_plan.tree_operators_summary",
                    &[
                        ("count", &active.operators.len().to_string()),
                        ("total_cost", &format!("{:.4}", active.total_subtree_cost)),
                    ],
                )),
        );
        for row in &visible {
            body = body.child(self.render_tree_row(row, cx));
        }
        if let Some(detail) = self.render_selected_detail() {
            body = body.child(detail);
        }
        body.into_any_element()
    }

    fn render_comparison_delta(&self, delta: &OperatorDelta) -> impl IntoElement {
        let s = Theme::scaled;
        let tone = match delta.kind {
            OperatorDeltaKind::Regressed => Theme::error(),
            OperatorDeltaKind::Improved => Theme::success(),
            OperatorDeltaKind::Added => Theme::warning(),
            OperatorDeltaKind::Removed => Theme::info(),
        };
        div()
            .flex()
            .flex_row()
            .items_start()
            .gap(px(s(8.0)))
            .px(px(s(8.0)))
            .py(px(s(6.0)))
            .border_b_1()
            .border_color(Theme::border())
            .child(
                div()
                    .w(px(s(72.0)))
                    .flex_shrink_0()
                    .text_size(px(Theme::font_size_ratio(0.8)))
                    .text_color(tone)
                    .child(operator_delta_kind_label(delta.kind)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(s(2.0)))
                    .child(
                        div()
                            .text_size(px(Theme::font_size()))
                            .text_color(Theme::text())
                            .child(crate::localization::format(
                                "query.execution_plan.delta_statement",
                                &[
                                    ("number", &(delta.statement_index + 1).to_string()),
                                    ("operator", &delta.operator),
                                ],
                            )),
                    )
                    .child(
                        div()
                            .whitespace_normal()
                            .text_size(px(Theme::font_size_ratio(0.8)))
                            .text_color(Theme::text_dim())
                            .child(operator_delta_detail(delta)),
                    ),
            )
    }

    fn render_comparison(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let s = Theme::scaled;
        let baseline_label = self.capture_label(self.baseline_capture);
        let comparison_label = self.capture_label(self.comparison_capture);
        let mut body = div()
            .id("plan-comparison-body")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(8.0)))
                    .px(px(s(8.0)))
                    .py(px(s(6.0)))
                    .border_b_1()
                    .border_color(Theme::border())
                    .child(
                        Button::new("plan-comparison-baseline")
                            .small()
                            .label(crate::localization::format(
                                "query.execution_plan.baseline_button",
                                &[("capture", &baseline_label)],
                            ))
                            .on_click(cx.listener(|this, _, _window, cx| this.cycle_baseline(cx))),
                    )
                    .child(
                        Button::new("plan-comparison-target")
                            .small()
                            .label(crate::localization::format(
                                "query.execution_plan.comparison_button",
                                &[("capture", &comparison_label)],
                            ))
                            .on_click(
                                cx.listener(|this, _, _window, cx| this.cycle_comparison(cx)),
                            ),
                    ),
            );

        match self.comparison_result.as_ref() {
            Some(Ok(comparison)) => {
                body = body.child(
                    div()
                        .flex_shrink_0()
                        .px(px(s(8.0)))
                        .py(px(s(8.0)))
                        .border_b_1()
                        .border_color(Theme::border())
                        .flex()
                        .flex_col()
                        .gap(px(s(3.0)))
                        .child(
                            div()
                                .text_size(px(Theme::font_size()))
                                .text_color(Theme::text())
                                .child(crate::localization::format(
                                    "query.execution_plan.comparison_summary",
                                    &[
                                        (
                                            "cost_delta",
                                            &signed_delta(comparison.total_cost_delta()),
                                        ),
                                        (
                                            "baseline_operators",
                                            &comparison.baseline_node_count.to_string(),
                                        ),
                                        (
                                            "comparison_operators",
                                            &comparison.comparison_node_count.to_string(),
                                        ),
                                        (
                                            "baseline_warnings",
                                            &comparison.baseline_warning_count.to_string(),
                                        ),
                                        (
                                            "comparison_warnings",
                                            &comparison.comparison_warning_count.to_string(),
                                        ),
                                    ],
                                )),
                        )
                        .child(
                            div()
                                .text_size(px(Theme::font_size_ratio(0.8)))
                                .text_color(Theme::text_dim())
                                .child(crate::localization::format(
                                    "query.execution_plan.comparison_delta_counts",
                                    &[
                                        (
                                            "regressed",
                                            &comparison
                                                .count(OperatorDeltaKind::Regressed)
                                                .to_string(),
                                        ),
                                        (
                                            "improved",
                                            &comparison
                                                .count(OperatorDeltaKind::Improved)
                                                .to_string(),
                                        ),
                                        (
                                            "added",
                                            &comparison.count(OperatorDeltaKind::Added).to_string(),
                                        ),
                                        (
                                            "removed",
                                            &comparison
                                                .count(OperatorDeltaKind::Removed)
                                                .to_string(),
                                        ),
                                    ],
                                )),
                        ),
                );
                if comparison.operator_deltas.is_empty() {
                    body = body.child(
                        div()
                            .p(px(s(12.0)))
                            .text_size(px(Theme::font_size()))
                            .text_color(Theme::text_dim())
                            .child(crate::localization::text(
                                "query.execution_plan.no_operator_changes",
                            )),
                    );
                } else {
                    for delta in &comparison.operator_deltas {
                        body = body.child(self.render_comparison_delta(delta));
                    }
                }
            }
            Some(Err(message)) => {
                body = body.child(
                    div()
                        .p(px(s(12.0)))
                        .whitespace_normal()
                        .text_size(px(Theme::font_size()))
                        .text_color(Theme::warning())
                        .child(message.clone()),
                );
            }
            None => {}
        }
        body.into_any_element()
    }

    fn render_ready(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        if self.comparison_mode {
            return self.render_comparison(cx);
        }
        match self.view_mode {
            PlanViewMode::Tree => return self.render_tree(cx),
            PlanViewMode::Diagram => return self.render_diagram(cx),
            PlanViewMode::Findings => {}
        }
        let s = Theme::scaled;
        let Some(active) = self.active() else {
            return div()
                .p(px(s(12.0)))
                .text_size(px(Theme::font_size()))
                .text_color(Theme::text_dim())
                .child(crate::localization::text(
                    "query.execution_plan.findings_empty_plan",
                ))
                .into_any_element();
        };
        // The pane body is one vertical scroll region holding every section:
        // statement strip, findings, operators. Its children are appended
        // directly and pinned with `flex_shrink_0` — a shrinkable child in a
        // height-bounded flex column gets squeezed down to the container, so
        // the content never exceeds the viewport and nothing ever scrolls.
        // `min_h_0` keeps the region itself from growing past its flex share
        // for the same reason.
        let mut body = div()
            .id("plan-body")
            .track_scroll(&self.body_scroll)
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .w_full()
            .overflow_y_scroll();

        // Which statement the findings below belong to. Shown whenever a
        // batch has more than one, so the switcher's choice stays visible
        // after the header scrolls out of mind.
        if self.statements.len() > 1 && !active.statement_text.is_empty() {
            body = body.child(
                div()
                    .flex_shrink_0()
                    .px(px(s(8.0)))
                    .py(px(s(4.0)))
                    .border_b_1()
                    .border_color(Theme::border())
                    .whitespace_normal()
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text_dim())
                    .child(truncate(&active.statement_text, 120)),
            );
        }

        if active.findings.is_empty() {
            body = body.child(
                div()
                    .flex_shrink_0()
                    .px(px(s(8.0)))
                    .py(px(s(8.0)))
                    .border_b_1()
                    .border_color(Theme::border())
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text())
                    .child(crate::localization::text(
                        "query.execution_plan.no_findings",
                    )),
            );
        } else {
            body = body.child(
                div()
                    .flex_shrink_0()
                    .px(px(s(8.0)))
                    .pt(px(s(8.0)))
                    .pb(px(s(4.0)))
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::format(
                        "query.execution_plan.findings_count",
                        &[("count", &active.findings.len().to_string())],
                    )),
            );
            for (index, finding) in active.findings.iter().enumerate() {
                body = body.child(self.render_finding_row(index, finding, cx));
            }
        }

        body = body.child(
            div()
                .flex_shrink_0()
                .px(px(s(8.0)))
                .pt(px(s(8.0)))
                .pb(px(s(4.0)))
                .text_size(px(Theme::font_size()))
                .text_color(Theme::text_dim())
                .child(crate::localization::format(
                    "query.execution_plan.findings_operators_summary",
                    &[
                        ("count", &active.operators.len().to_string()),
                        ("total_cost", &format!("{:.4}", active.total_subtree_cost)),
                    ],
                )),
        );
        for operator in &active.operators {
            body = body.child(self.render_operator_row(operator, cx));
        }

        if let Some(notice) = &self.capture_history_notice {
            body = body.child(
                div()
                    .flex_shrink_0()
                    .px(px(s(8.0)))
                    .py(px(s(6.0)))
                    .whitespace_normal()
                    .text_size(px(Theme::font_size_ratio(0.8)))
                    .text_color(Theme::warning())
                    .child(notice.clone()),
            );
        }

        body.into_any_element()
    }
}

impl Render for ExecutionPlanView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let body = match &self.status {
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
            PlanPaneStatus::Capturing => div()
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
                        .child(message.clone()),
                )
                .into_any_element(),
            PlanPaneStatus::Ready => self.render_ready(cx),
        };

        div()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                match event.keystroke.key.as_str() {
                    "up" => this.move_selection(-1, cx),
                    "down" => this.move_selection(1, cx),
                    "left" if this.view_mode == PlanViewMode::Tree => this.handle_tree_left(cx),
                    "right" if this.view_mode == PlanViewMode::Tree => this.handle_tree_right(cx),
                    "enter" | "space" => this.toggle_selected(cx),
                    _ => {}
                }
            }))
            .flex()
            .flex_col()
            .size_full()
            .bg(Theme::panel_bg())
            .child(self.render_header(cx))
            .child(body)
    }
}

/// An expanded row's detail area: capped at `max_height` and scrollable
/// inside that cap, so one operator's output column list cannot bury the
/// rows below it.
///
/// The wheel is swallowed only while this region still has somewhere to go in
/// the direction of the gesture. At either end the event bubbles on to the
/// pane body, so a flick that runs past the bottom of a detail keeps scrolling
/// the pane instead of dead-ending — and a detail short enough to fit never
/// traps the wheel at all. The handler runs after gpui's built-in scroll
/// listener (registered later, and bubble dispatch is reverse-registration
/// order), so the offset it reads already reflects this event.
fn detail_scroll_region(
    id: impl Into<ElementId>,
    scroll: Option<&ScrollHandle>,
    max_height: Pixels,
) -> Stateful<Div> {
    let region = div().id(id).max_h(max_height).overflow_y_scroll();
    let Some(scroll) = scroll.cloned() else {
        return region;
    };
    region
        .track_scroll(&scroll)
        .on_scroll(move |event, window, cx| {
            let delta = event.delta.pixel_delta(window.line_height()).y;
            let offset = scroll.offset().y;
            let max = scroll.max_offset().y;
            let consumed = if delta < px(0.0) {
                offset > -max
            } else if delta > px(0.0) {
                offset < px(0.0)
            } else {
                false
            };
            if consumed {
                cx.stop_propagation();
            }
        })
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

fn signed_delta(value: f64) -> String {
    format!("{value:+.4}")
}

fn operator_delta_kind_label(kind: OperatorDeltaKind) -> String {
    let key = match kind {
        OperatorDeltaKind::Regressed => "query.execution_plan.delta_kind_regressed",
        OperatorDeltaKind::Improved => "query.execution_plan.delta_kind_improved",
        OperatorDeltaKind::Added => "query.execution_plan.delta_kind_added",
        OperatorDeltaKind::Removed => "query.execution_plan.delta_kind_removed",
    };
    crate::localization::text(key)
}

fn operator_delta_detail(delta: &OperatorDelta) -> String {
    let localized =
        |key: &str, arguments: &[(&str, &str)]| crate::localization::format(key, arguments);
    let before_cost = delta
        .baseline_cost
        .map(|value| format!("{value:.4}"))
        .unwrap_or_default();
    let after_cost = delta
        .comparison_cost
        .map(|value| format!("{value:.4}"))
        .unwrap_or_default();
    let cost_delta = match (delta.baseline_cost, delta.comparison_cost) {
        (Some(before), Some(after)) => signed_delta(after - before),
        _ => String::new(),
    };
    let cost = match (delta.baseline_cost, delta.comparison_cost) {
        (Some(_), Some(_)) => localized(
            "query.execution_plan.delta_cost_change",
            &[
                ("before", &before_cost),
                ("after", &after_cost),
                ("delta", &cost_delta),
            ],
        ),
        (None, Some(_)) => localized(
            "query.execution_plan.delta_cost_comparison",
            &[("after", &after_cost)],
        ),
        (Some(_), None) => localized(
            "query.execution_plan.delta_cost_baseline",
            &[("before", &before_cost)],
        ),
        (None, None) => crate::localization::text("query.execution_plan.delta_cost_unavailable"),
    };
    let before_estimated = delta
        .baseline_estimated_rows
        .map(|value| format!("{value:.2}"))
        .unwrap_or_default();
    let after_estimated = delta
        .comparison_estimated_rows
        .map(|value| format!("{value:.2}"))
        .unwrap_or_default();
    let estimated_rows = match (
        delta.baseline_estimated_rows,
        delta.comparison_estimated_rows,
    ) {
        (Some(_), Some(_)) => localized(
            "query.execution_plan.delta_estimated_change",
            &[("before", &before_estimated), ("after", &after_estimated)],
        ),
        (None, Some(_)) => localized(
            "query.execution_plan.delta_estimated_comparison",
            &[("after", &after_estimated)],
        ),
        (Some(_), None) => localized(
            "query.execution_plan.delta_estimated_baseline",
            &[("before", &before_estimated)],
        ),
        (None, None) => {
            crate::localization::text("query.execution_plan.delta_estimated_unavailable")
        }
    };
    let before_actual = delta
        .baseline_actual_rows
        .map(|value| format!("{value:.2}"))
        .unwrap_or_default();
    let after_actual = delta
        .comparison_actual_rows
        .map(|value| format!("{value:.2}"))
        .unwrap_or_default();
    let actual_rows = match (delta.baseline_actual_rows, delta.comparison_actual_rows) {
        (Some(_), Some(_)) => localized(
            "query.execution_plan.delta_actual_change",
            &[("before", &before_actual), ("after", &after_actual)],
        ),
        (None, Some(_)) => localized(
            "query.execution_plan.delta_actual_comparison",
            &[("after", &after_actual)],
        ),
        (Some(_), None) => localized(
            "query.execution_plan.delta_actual_baseline",
            &[("before", &before_actual)],
        ),
        (None, None) => crate::localization::text("query.execution_plan.delta_actual_unavailable"),
    };
    let warning_count = format!("{:+}", delta.warning_delta);
    let warnings = if delta.warning_delta == 0 {
        crate::localization::text("query.execution_plan.delta_warnings_unchanged")
    } else {
        localized(
            "query.execution_plan.delta_warnings_changed",
            &[("count", &warning_count)],
        )
    };
    localized(
        "query.execution_plan.delta_detail",
        &[
            ("cost", &cost),
            ("estimated_rows", &estimated_rows),
            ("actual_rows", &actual_rows),
            ("warnings", &warnings),
        ],
    )
}

/// Clip a statement to one display line. Statement text can be a whole
/// batch; the strip shows the first line, ellipsized.
/// One-line label for a multi-line string: every run of whitespace (the
/// newlines and tabs SHOWPLAN embeds in `StatementText`, among others) becomes
/// a single space, then the result is clipped with an ellipsis.
///
/// This used to keep only the first line, which read fine while XML numeric
/// character references were leaking through unescaped — the "first line" was
/// the whole statement. With those decoded properly, a formatted statement's
/// first line is often just `SELECT`, so the header has to flatten instead of
/// clip.
pub(crate) fn truncate(text: &str, max_chars: usize) -> String {
    let flattened = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = flattened.chars();
    let clipped: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{clipped}…")
    } else {
        clipped
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, size};
    use sqlly_services::plans::parser::PlanParser;

    const SCAN_XML: &str = r#"<ShowPlanXML><StmtSimple StatementText="SELECT *" StatementType="SELECT" StatementSubTreeCost="2"><RelOp NodeId="1" LogicalOp="Scan" PhysicalOp="Table Scan" EstimateRows="4" EstimatedTotalSubtreeCost="2"><Object Database="[db]" Schema="[dbo]" Table="[Orders]" /></RelOp></StmtSimple></ShowPlanXML>"#;

    fn sample_plan() -> ExecutionPlan {
        PlanParser::new().parse_xml(SCAN_XML).unwrap()
    }

    fn ensure_components_initialized(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            if cx.try_global::<gpui_component::Theme>().is_none() {
                gpui_component::init(cx);
            }
        });
    }

    #[test]
    fn ai_explain_prompt_digests_findings_and_operators() {
        assert!(ai_explain_prompt_for(&[]).is_none());
        let plans = vec![StatementPlans::from_plan(&sample_plan())];
        let prompt = ai_explain_prompt_for(&plans).expect("has statements");
        assert!(prompt.contains("Explain this SQL execution plan"));
        assert!(prompt.contains("Findings:"));
        assert!(prompt.contains("Scans every row in dbo.Orders"));
        assert!(prompt.contains("Most expensive operators:"));
        assert!(prompt.contains("Table Scan"));
        // No raw plan XML leaks into the prompt.
        assert!(!prompt.contains("<RelOp"));
    }

    #[test]
    fn statement_plans_precomputes_findings_and_operators() {
        let plans = StatementPlans::from_plan(&sample_plan());
        assert_eq!(plans.statement_type, "SELECT");
        assert_eq!(plans.findings.len(), 1);
        assert_eq!(
            plans.findings[0].summary,
            "Scans every row in dbo.Orders (4 rows) to find matches"
        );
        assert_eq!(plans.operators.len(), 1);
        assert_eq!(plans.operators[0].title, "Table Scan");
    }

    #[gpui::test]
    fn finding_with_action_sql_exposes_buttons(cx: &mut gpui::TestAppContext) {
        ensure_components_initialized(cx);
        let (view, cx) = cx.add_window_view(|_window, cx| ExecutionPlanView::new(cx));
        view.update(cx, |view, cx| {
            view.set_plans_for_context(PlanKind::Estimated, String::new(), vec![sample_plan()], cx);
            assert_eq!(view.finding_sql_action_count(), 0);
            assert_eq!(view.child_index_for_row(PaneRow::Finding(0)), Some(1));
            if let Some(finding) = view
                .statements
                .get_mut(view.active_statement)
                .and_then(|s| s.findings.get_mut(0))
            {
                finding.action_sql =
                    Some("CREATE NONCLUSTERED INDEX [IX_t] ON [dbo].[t] ([a]);".to_string());
            }
            assert_eq!(view.finding_sql_action_count(), 2);
            // Buttons live inside the finding row; pane child order is unchanged.
            assert_eq!(view.child_index_for_row(PaneRow::Finding(0)), Some(1));
            assert_eq!(view.child_index_for_row(PaneRow::Operator(1)), Some(3));
        });
    }

    /// The Re-capture button and the empty state follow the engine gate the
    /// app pushes down: Redis disables every capture with the engine named,
    /// SQLite only disables re-capturing an actual plan and points the empty
    /// pane at Estimated.
    #[gpui::test]
    fn capture_gate_drives_recapture_and_empty_state(cx: &mut gpui::TestAppContext) {
        use crate::capabilities::PlanCaptureGate;
        use sqlly_lang::sqlmodel::DatabaseType;
        ensure_components_initialized(cx);
        let (view, cx) = cx.add_window_view(|_window, cx| ExecutionPlanView::new(cx));
        view.update(cx, |view, cx| {
            assert_eq!(view.recapture_unavailable_reason(), None);
            assert!(view.empty_state_text().contains("⌘L"));

            view.set_capture_gate(PlanCaptureGate::for_engine(Some(DatabaseType::Redis)), cx);
            assert!(
                view.empty_state_text()
                    .starts_with("Query plans are disabled for this database type (Redis).")
            );
            view.set_plans_for_context(PlanKind::Estimated, String::new(), vec![sample_plan()], cx);
            assert!(
                view.recapture_unavailable_reason()
                    .is_some_and(|r| r.contains("(Redis)"))
            );

            view.set_capture_gate(PlanCaptureGate::for_engine(Some(DatabaseType::Sqlite)), cx);
            assert_eq!(
                view.recapture_unavailable_reason(),
                None,
                "an estimated plan can be re-captured on SQLite"
            );
            assert!(view.empty_state_text().contains("Estimated"));
            view.set_plans_for_context(PlanKind::Actual, String::new(), vec![sample_plan()], cx);
            assert!(
                view.recapture_unavailable_reason()
                    .is_some_and(|r| r.contains("(SQLite)"))
            );
        });
    }

    #[test]
    fn empty_plan_yields_friendly_summary() {
        let xml = r#"<ShowPlanXML><StmtSimple StatementText="SELECT 1" StatementType="SELECT" StatementSubTreeCost="0.1"><RelOp NodeId="0" LogicalOp="Select" PhysicalOp="Compute Scalar" EstimateRows="1" EstimatedTotalSubtreeCost="0.1" /></StmtSimple></ShowPlanXML>"#;
        let plan = PlanParser::new().parse_xml(xml).unwrap();
        let plans = StatementPlans::from_plan(&plan);
        assert!(plans.findings.is_empty());
        assert_eq!(plans.operators.len(), 1);
    }

    /// `child_index_for_row` has to mirror the order `render_ready` appends
    /// children in, or keyboard navigation scrolls to the wrong row. These
    /// pin both shapes: with findings (header, findings, header, operators)
    /// and without (the "looks good" row standing in for the findings block).
    #[gpui::test]
    fn child_index_mirrors_body_child_order(cx: &mut gpui::TestAppContext) {
        ensure_components_initialized(cx);
        let (view, cx) = cx.add_window_view(|_window, cx| ExecutionPlanView::new(cx));
        view.update(cx, |view, cx| {
            view.set_plans_for_context(PlanKind::Estimated, String::new(), vec![sample_plan()], cx);
            // [0] "Findings (1)"  [1] the finding  [2] "Operators (1)"  [3] the operator
            assert_eq!(view.child_index_for_row(PaneRow::Finding(0)), Some(1));
            assert_eq!(view.child_index_for_row(PaneRow::Operator(1)), Some(3));
            // A node id that is not in this plan has no row to scroll to.
            assert_eq!(view.child_index_for_row(PaneRow::Operator(99)), None);
        });
    }

    #[gpui::test]
    fn child_index_accounts_for_the_no_findings_row(cx: &mut gpui::TestAppContext) {
        ensure_components_initialized(cx);
        let xml = r#"<ShowPlanXML><StmtSimple StatementText="SELECT 1" StatementType="SELECT" StatementSubTreeCost="0.1"><RelOp NodeId="0" LogicalOp="Select" PhysicalOp="Compute Scalar" EstimateRows="1" EstimatedTotalSubtreeCost="0.1" /></StmtSimple></ShowPlanXML>"#;
        let plan = PlanParser::new().parse_xml(xml).unwrap();
        let (view, cx) = cx.add_window_view(|_window, cx| ExecutionPlanView::new(cx));
        view.update(cx, |view, cx| {
            view.set_plans_for_context(PlanKind::Estimated, String::new(), vec![plan], cx);
            assert!(view.active().unwrap().findings.is_empty());
            // [0] "This plan looks good"  [1] "Operators (1)"  [2] the operator
            assert_eq!(view.child_index_for_row(PaneRow::Operator(0)), Some(2));
        });
    }

    /// Collapsing a row drops its detail scroll position, so re-expanding it
    /// starts at the top rather than wherever it was left.
    #[gpui::test]
    fn detail_scroll_state_follows_expansion(cx: &mut gpui::TestAppContext) {
        ensure_components_initialized(cx);
        let (view, cx) = cx.add_window_view(|_window, cx| ExecutionPlanView::new(cx));
        view.update(cx, |view, cx| {
            view.set_plans_for_context(PlanKind::Estimated, String::new(), vec![sample_plan()], cx);
            view.toggle_operator(1, cx);
            assert!(view.operator_detail_scrolls.contains_key(&1));
            view.toggle_operator(1, cx);
            assert!(view.operator_detail_scrolls.is_empty());

            view.toggle_finding(0, cx);
            assert!(view.finding_detail_scrolls.contains_key(&0));
            // A fresh capture clears expansion, and the scroll state with it.
            view.set_plans_for_context(PlanKind::Estimated, String::new(), vec![sample_plan()], cx);
            assert!(view.finding_detail_scrolls.is_empty());
        });
    }

    #[test]
    fn truncate_clips_to_one_line() {
        assert_eq!(truncate("SELECT 1", 120), "SELECT 1");
        // Flattened, not clipped at the first newline: a formatted statement
        // must still read as a statement in the header.
        assert_eq!(truncate("SELECT 1\nFROM t", 120), "SELECT 1 FROM t");
        assert_eq!(
            truncate("SELECT\n\t@Offset = t.Utc * 60\nFROM  Shared.Tenant t", 120),
            "SELECT @Offset = t.Utc * 60 FROM Shared.Tenant t"
        );
        let long = "x".repeat(200);
        let clipped = truncate(&long, 120);
        assert!(clipped.ends_with('…'));
        assert_eq!(clipped.chars().count(), 121);
        assert_eq!(truncate("", 120), "");
    }

    #[gpui::test]
    fn capture_history_is_bounded_and_defaults_to_latest_pair(cx: &mut gpui::TestAppContext) {
        ensure_components_initialized(cx);
        let (view, cx) = cx.add_window_view(|_window, cx| ExecutionPlanView::new(cx));
        view.update(cx, |view, cx| {
            for sequence in 0..(MAX_CAPTURE_HISTORY + 2) {
                let mut plan = sample_plan();
                plan.nodes[0].estimated_cost = sequence as f64 + 1.0;
                view.set_plans_for_context(
                    PlanKind::Estimated,
                    "SELECT * FROM Orders".into(),
                    vec![plan],
                    cx,
                );
            }
            assert_eq!(view.captures.len(), MAX_CAPTURE_HISTORY);
            assert_eq!(view.captures.first().unwrap().sequence, 3);
            assert_eq!(view.captures.last().unwrap().sequence, 10);
            assert_eq!(view.baseline_capture, MAX_CAPTURE_HISTORY - 2);
            assert_eq!(view.comparison_capture, MAX_CAPTURE_HISTORY - 1);
        });
    }

    #[gpui::test]
    fn comparison_preserves_capture_kinds_and_checks_query_context(cx: &mut gpui::TestAppContext) {
        ensure_components_initialized(cx);
        let (view, cx) = cx.add_window_view(|_window, cx| ExecutionPlanView::new(cx));
        view.update(cx, |view, cx| {
            let baseline = sample_plan();
            let mut comparison = sample_plan();
            comparison.nodes[0].id = 99;
            comparison.nodes[0].estimated_cost = 4.0;
            view.set_plans_for_context(
                PlanKind::Actual,
                "SELECT  *  FROM Orders".into(),
                vec![baseline],
                cx,
            );
            view.set_plans_for_context(
                PlanKind::Estimated,
                "SELECT * FROM Orders".into(),
                vec![comparison],
                cx,
            );
            view.toggle_comparison(cx);
            assert!(view.comparison_mode);
            assert!(view.comparison_result.as_ref().unwrap().is_ok());
            assert!(view.capture_label(0).contains("Actual plan"));
            assert!(view.capture_label(1).contains("Estimated plan"));

            view.set_plans_for_context(
                PlanKind::Estimated,
                "SELECT * FROM Customers".into(),
                vec![sample_plan()],
                cx,
            );
            view.toggle_comparison(cx);
            assert!(
                view.comparison_result
                    .as_ref()
                    .unwrap()
                    .as_ref()
                    .unwrap_err()
                    .contains("different query text")
            );
        });
    }

    #[gpui::test]
    fn oversized_capture_is_not_retained_for_comparison(cx: &mut gpui::TestAppContext) {
        ensure_components_initialized(cx);
        let mut plan = sample_plan();
        let template = plan.nodes[0].clone();
        plan.nodes = (0..=DEFAULT_MAX_COMPARISON_NODES)
            .map(|id| {
                let mut node = template.clone();
                node.id = id as i32;
                node
            })
            .collect();
        let (view, cx) = cx.add_window_view(|_window, cx| ExecutionPlanView::new(cx));
        view.update(cx, |view, cx| {
            view.set_plans_for_context(
                PlanKind::Estimated,
                "SELECT * FROM LargePlan".into(),
                vec![plan],
                cx,
            );
            assert!(view.captures.is_empty());
            assert!(
                view.capture_history_notice
                    .as_ref()
                    .unwrap()
                    .contains("not retained")
            );
        });
    }

    #[gpui::test]
    fn tree_mode_rows_follow_pre_order_and_collapse(cx: &mut gpui::TestAppContext) {
        ensure_components_initialized(cx);
        let (view, cx) = cx.add_window_view(|_window, cx| ExecutionPlanView::new(cx));
        view.update(cx, |view, cx| {
            view.set_plans_for_context(PlanKind::Estimated, String::new(), vec![sample_plan()], cx);
            view.view_mode = PlanViewMode::Tree;
            assert_eq!(view.rows().len(), view.active().unwrap().tree_rows.len());
            view.selected_row = 0;
            view.toggle_selected(cx);
            assert_eq!(view.rows().len(), 1);
        });
    }

    #[gpui::test]
    fn new_capture_resets_view_mode_and_tree_state(cx: &mut gpui::TestAppContext) {
        ensure_components_initialized(cx);
        let (view, cx) = cx.add_window_view(|_window, cx| ExecutionPlanView::new(cx));
        view.update(cx, |view, cx| {
            view.set_plans_for_context(PlanKind::Estimated, String::new(), vec![sample_plan()], cx);
            view.view_mode = PlanViewMode::Tree;
            view.collapsed_tree_nodes.insert(1);
            view.selected_tree_node = Some(1);
            view.set_plans_for_context(PlanKind::Estimated, String::new(), vec![sample_plan()], cx);
            assert_eq!(view.view_mode, PlanViewMode::Findings);
            assert!(view.collapsed_tree_nodes.is_empty());
            assert!(view.selected_tree_node.is_none());
        });
    }

    #[gpui::test]
    fn diagram_click_selects_node_and_shows_detail(cx: &mut gpui::TestAppContext) {
        ensure_components_initialized(cx);
        let (view, cx) = cx.add_window_view(|_window, cx| ExecutionPlanView::new(cx));
        view.update(cx, |view, cx| {
            view.set_plans_for_context(PlanKind::Estimated, String::new(), vec![sample_plan()], cx);
            view.view_mode = PlanViewMode::Diagram;
            view.diagram_bounds.set(Some(Bounds {
                origin: point(px(100.0), px(100.0)),
                size: size(px(800.0), px(480.0)),
            }));
            view.diagram_pan = (0.0, 0.0);
            let root = view.active().unwrap().diagram.nodes[0].node_id;
            let x = view.active().unwrap().diagram.nodes[0].x;
            let y = view.active().unwrap().diagram.nodes[0].y;
            view.on_diagram_click(point(px(100.0 + x + 8.0), px(100.0 + y + 8.0)), cx);
            assert_eq!(view.selected_tree_node, Some(root));
            assert!(view.operator_detail_scrolls.contains_key(&root));
        });
    }
}
