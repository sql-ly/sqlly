//! Query plan cells: the estimated plan of another cell's query (or its
//! actual plan, after the user confirms that the query will run) as the
//! plan pane's headline, findings and operator tree in compact form, a
//! side-by-side comparison with another
//! query variant or an earlier run, session counters where the engine has
//! them, and parameter sweep timings. Every figure says which engine and
//! which measurement it came from.

use gpui::{
    AnyElement, Context, Entity, IntoElement, ParentElement as _, Render, Styled as _, WeakEntity,
    Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::Disableable as _;
use gpui_component::Sizable as _;
use gpui_component::button::ButtonVariants as _;
use gpui_component::input::{Input, InputState};
use sqlly_services::notebook::cells::plan::{
    PlanCellSpec, PlanCompare, PlanMode, StatDelta, StatsSource, SweepSpec, TimingStats,
    TimingVerdict, compare_timings,
};
use sqlly_services::plans::{
    DEFAULT_MAX_COMPARISON_NODES, ExecutionPlan, PlanChangeReport, explain_plan_changes,
    human_number,
};

use super::{CellRenderCx, NotebookCell};
use crate::execution_plan_view::{BatchPlans, StatementPlans, compact};
use crate::notebook::analysis::ui::{self, body_text, caption, chip, field, number, picker, t};
use crate::notebook::view::NotebookView;
use crate::theme::Theme;

pub struct PlanCell;

impl NotebookCell for PlanCell {
    fn kind(&self) -> &'static str {
        crate::notebook::analysis::KIND_PLAN
    }

    fn label_key(&self) -> &'static str {
        "notebook.analysis.kind.plan"
    }

    fn runnable(&self) -> bool {
        true
    }

    fn render_body(
        &self,
        cell: &CellRenderCx,
        _window: &mut Window,
        _cx: &mut Context<NotebookView>,
    ) -> AnyElement {
        ui::cell_body(cell)
    }
}

/// A question the cell asks before its next run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prompt {
    /// An actual plan executes the query.
    ConfirmActual,
    /// The sweep would run on a production connection.
    AuthorizeProductionSweep,
}

/// One swept parameter value's timings.
#[derive(Debug, Clone, PartialEq)]
pub struct SweepPoint {
    pub value: String,
    pub timing: TimingStats,
    pub errors: usize,
    /// Session counter changes summed over the value's runs.
    pub stats: Vec<StatDelta>,
}

#[derive(Debug, Clone)]
pub struct PlanResult {
    pub actual: bool,
    pub engine: String,
    pub plans: Vec<ExecutionPlan>,
    /// The plan this one is compared with.
    pub baseline: Option<Result<Vec<ExecutionPlan>, sqlly_core::DisplayMessage>>,
    pub stats_source: Option<StatsSource>,
    pub stats: Vec<StatDelta>,
    pub sweep: Vec<SweepPoint>,
    /// Catalog key of why the sweep did not run.
    pub sweep_note: Option<&'static str>,
}

impl PlanResult {
    /// What changed from the baseline to this plan, in the same words the
    /// plan pane's comparison uses.
    pub fn comparison(&self) -> Option<Result<PlanChangeReport, sqlly_core::DisplayMessage>> {
        let baseline = self.baseline.as_ref()?;
        Some(match baseline {
            Ok(base) => explain_plan_changes(base, &self.plans, DEFAULT_MAX_COMPARISON_NODES)
                .map_err(|e| e.message()),
            Err(e) => Err(e.clone()),
        })
    }
}

/// A plan result analysed once, when it arrives, so drawing the cell does no
/// plan work.
struct Analysed {
    plans: BatchPlans,
    baseline: Option<BatchPlans>,
    comparison: Option<Result<PlanChangeReport, sqlly_core::DisplayMessage>>,
}

impl Analysed {
    fn new(result: &PlanResult) -> Self {
        Self {
            plans: BatchPlans::build(result.plans.clone(), None, None),
            baseline: result
                .baseline
                .as_ref()
                .and_then(|baseline| baseline.as_ref().ok())
                .map(|plans| BatchPlans::build(plans.clone(), None, None)),
            comparison: result.comparison(),
        }
    }
}

/// The measurement label: "Estimated plan · SQL Server · plan capture".
pub(crate) fn source_label(result: &PlanResult) -> String {
    crate::localization::format(
        if result.actual {
            "notebook.analysis.plan.source_actual"
        } else {
            "notebook.analysis.plan.source_estimated"
        },
        &[("engine", result.engine.as_str())],
    )
}

pub(crate) fn stats_label(source: StatsSource, engine: &str) -> String {
    crate::localization::format(
        "notebook.analysis.plan.stats_source",
        &[("source", source.label()), ("engine", engine)],
    )
}

fn verdict_key(v: TimingVerdict) -> &'static str {
    match v {
        TimingVerdict::Faster => "notebook.analysis.sweep.faster",
        TimingVerdict::Slower => "notebook.analysis.sweep.slower",
        TimingVerdict::WithinNoise => "notebook.analysis.sweep.within_noise",
        TimingVerdict::NotEnoughRuns => "notebook.analysis.sweep.not_enough_runs",
    }
}

/// The row counts beside an operator: estimated, and actual when the plan
/// has them.
fn rows_detail(statement: &StatementPlans, estimated: Option<f64>, rows: Option<f64>) -> String {
    let figure = |value: Option<f64>| value.map_or_else(|| "—".to_string(), human_number);
    if statement.analysis.has_actuals {
        crate::localization::format(
            "notebook.analysis.plan.rows_actual",
            &[("estimated", figure(estimated)), ("actual", figure(rows))],
        )
    } else {
        crate::localization::format(
            "notebook.analysis.plan.rows_estimated",
            &[("estimated", figure(estimated.or(rows)))],
        )
    }
}

/// Operator tree lines: depth, operator, row counts.
#[cfg(test)]
pub(crate) fn tree_lines(statement: &StatementPlans) -> Vec<(usize, String, String)> {
    statement
        .table
        .iter()
        .map(|row| {
            (
                row.depth,
                row.operator.clone(),
                rows_detail(statement, row.estimated_rows, row.rows),
            )
        })
        .collect()
}

pub struct PlanView {
    notebook: WeakEntity<NotebookView>,
    cell_id: String,
    result: Option<PlanResult>,
    analysed: Option<Analysed>,
    prompt: Option<Prompt>,
    values_input: Option<Entity<InputState>>,
    reps_input: Option<Entity<InputState>>,
}

impl PlanView {
    pub fn new(notebook: WeakEntity<NotebookView>, cell_id: String) -> Self {
        Self {
            notebook,
            cell_id,
            result: None,
            analysed: None,
            prompt: None,
            values_input: None,
            reps_input: None,
        }
    }

    #[cfg(test)]
    pub fn result(&self) -> Option<&PlanResult> {
        self.result.as_ref()
    }

    #[cfg(test)]
    pub fn prompt(&self) -> Option<Prompt> {
        self.prompt
    }

    pub fn set_result(&mut self, result: PlanResult, cx: &mut Context<Self>) {
        if result.sweep_note.is_none() {
            self.prompt = None;
        }
        self.analysed = Some(Analysed::new(&result));
        self.result = Some(result);
        cx.notify();
    }

    pub fn set_prompt(&mut self, prompt: Option<Prompt>, cx: &mut Context<Self>) {
        self.prompt = prompt;
        cx.notify();
    }

    fn spec(&self, cx: &gpui::App) -> PlanCellSpec {
        self.notebook
            .upgrade()
            .map(|nb| PlanCellSpec::from_spec(&nb.read(cx).doc.spec(&self.cell_id)))
            .unwrap_or_default()
    }

    fn edit(&self, cx: &mut gpui::App, change: impl FnOnce(&mut PlanCellSpec)) {
        Self::edit_at(&self.notebook, &self.cell_id, cx, change);
    }

    fn edit_view(view: &Entity<Self>, cx: &mut gpui::App, change: impl FnOnce(&mut PlanCellSpec)) {
        let (notebook, id) = {
            let v = view.read(cx);
            (v.notebook.clone(), v.cell_id.clone())
        };
        Self::edit_at(&notebook, &id, cx, change);
    }

    fn edit_at(
        notebook: &WeakEntity<NotebookView>,
        id: &str,
        cx: &mut gpui::App,
        change: impl FnOnce(&mut PlanCellSpec),
    ) {
        let Some(nb) = notebook.upgrade() else {
            return;
        };
        nb.update(cx, |nb, cx| {
            let mut spec = PlanCellSpec::from_spec(&nb.doc.spec(id));
            change(&mut spec);
            nb.set_cell_spec(id, spec.to_spec(), cx);
        });
    }

    /// Add an AI cell below that explains this plan, and start it.
    pub fn explain_with_ai(&self, cx: &mut gpui::App) {
        let Some(nb) = self.notebook.upgrade() else {
            return;
        };
        let id = self.cell_id.clone();
        nb.update(cx, |nb, cx| nb.explain_with_ai(&id, cx));
    }

    fn sweep_edit(&self, cx: &mut gpui::App, change: impl FnOnce(&mut SweepSpec)) {
        self.edit(cx, |s| apply_sweep(s, change));
    }

    fn toolbar(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let s = Theme::scaled;
        let spec = self.spec(cx);
        if self.values_input.is_none() {
            let values = spec
                .sweep
                .as_ref()
                .map(|w| {
                    w.values
                        .iter()
                        .map(|v| match v {
                            serde_json::Value::String(s) => s.clone(),
                            other => other.to_string(),
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            self.values_input = Some(ui::input(
                window,
                cx,
                "notebook.analysis.sweep.values_placeholder",
                &values,
                |this: &mut Self, text, cx| {
                    let values: Vec<serde_json::Value> = text
                        .split(',')
                        .map(str::trim)
                        .filter(|v| !v.is_empty())
                        .map(|v| serde_json::Value::String(v.to_string()))
                        .collect();
                    this.sweep_edit(cx, |w| w.values = values);
                },
            ));
        }
        if self.reps_input.is_none() {
            let reps = spec
                .sweep
                .as_ref()
                .map(|w| w.repetitions.to_string())
                .unwrap_or_default();
            self.reps_input = Some(ui::input(
                window,
                cx,
                "notebook.analysis.sweep.repetitions_placeholder",
                &reps,
                |this: &mut Self, text, cx| {
                    if let Ok(n) = text.trim().parse::<u32>() {
                        this.sweep_edit(cx, |w| w.repetitions = n);
                    }
                },
            ));
        }
        let (choices, params, running) = self
            .notebook
            .upgrade()
            .map(|nb| {
                let nb = nb.read(cx);
                (
                    nb.input_choices(&self.cell_id),
                    nb.doc
                        .params()
                        .into_iter()
                        .map(|(n, _)| n)
                        .collect::<Vec<_>>(),
                    nb.cell_state(&self.cell_id).is_active(),
                )
            })
            .unwrap_or_default();
        let current = choices
            .iter()
            .find(|(id, l)| *id == spec.input || l.eq_ignore_ascii_case(&spec.input))
            .cloned();
        let this = cx.weak_entity();
        let on_input: ui::OnChoose = std::rc::Rc::new({
            let this = this.clone();
            move |v, app| {
                if let Some(view) = this.upgrade() {
                    Self::edit_view(&view, app, |s| s.input = v);
                }
            }
        });
        let mut compares = vec![
            ("none".to_string(), t("notebook.analysis.plan.compare_none")),
            (
                "previous".to_string(),
                t("notebook.analysis.plan.compare_previous"),
            ),
        ];
        for (id, label) in &choices {
            if Some(id) != current.as_ref().map(|(i, _)| i) {
                compares.push((
                    format!("cell:{id}"),
                    crate::localization::format(
                        "notebook.analysis.plan.compare_cell",
                        &[("cell", label.as_str())],
                    ),
                ));
            }
        }
        let compare_value = match &spec.compare {
            None => "none".to_string(),
            Some(PlanCompare::PreviousRun) | Some(PlanCompare::Run(_)) => "previous".to_string(),
            Some(PlanCompare::Cell(c)) => format!("cell:{c}"),
        };
        let compare_label = compares
            .iter()
            .find(|(v, _)| *v == compare_value)
            .map(|(_, l)| l.clone())
            .unwrap_or_else(|| t("notebook.analysis.plan.compare_none"));
        let on_compare: ui::OnChoose = std::rc::Rc::new({
            let this = this.clone();
            move |v, app| {
                if let Some(view) = this.upgrade() {
                    Self::edit_view(&view, app, |s| {
                        s.compare = match v.as_str() {
                            "previous" => Some(PlanCompare::PreviousRun),
                            c if c.starts_with("cell:") => {
                                Some(PlanCompare::Cell(c[5..].to_string()))
                            }
                            _ => None,
                        }
                    });
                }
            }
        });
        let sweep_param = spec
            .sweep
            .as_ref()
            .map(|w| w.parameter.clone())
            .unwrap_or_default();
        let on_param: ui::OnChoose = std::rc::Rc::new(move |v, app| {
            if let Some(view) = this.upgrade() {
                Self::edit_view(&view, app, |s| apply_sweep(s, |w| w.parameter = v));
            }
        });
        let actual = spec.mode == PlanMode::Actual;
        let notebook = self.notebook.clone();
        let id = self.cell_id.clone();
        div()
            .flex()
            .flex_col()
            .gap(px(s(4.0)))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .gap(px(s(8.0)))
                    .child(field(
                        t("notebook.analysis.plan.query_label"),
                        picker(
                            "nb-plan-input",
                            current
                                .as_ref()
                                .map(|(_, l)| l.clone())
                                .unwrap_or_else(|| t("notebook.analysis.input_choose")),
                            choices.clone(),
                            current.map(|(id, _)| id),
                            on_input,
                        ),
                    ))
                    .child(
                        ui::button(
                            "nb-plan-mode",
                            t(if actual {
                                "notebook.analysis.plan.mode_actual"
                            } else {
                                "notebook.analysis.plan.mode_estimated"
                            }),
                        )
                        .tooltip(t("notebook.analysis.plan.mode_tooltip"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.edit(cx, |s| {
                                s.mode = if s.mode == PlanMode::Actual {
                                    PlanMode::Estimated
                                } else {
                                    PlanMode::Actual
                                }
                            })
                        })),
                    )
                    .child(field(
                        t("notebook.analysis.plan.compare_label"),
                        picker(
                            "nb-plan-compare",
                            compare_label,
                            compares,
                            Some(compare_value),
                            on_compare,
                        ),
                    ))
                    .child(
                        ui::checkbox(
                            "nb-plan-stats",
                            t("notebook.analysis.plan.session_stats"),
                            spec.session_stats,
                        )
                        .tooltip(t("notebook.analysis.plan.session_stats_tooltip"))
                        .on_click(cx.listener(|this, _: &bool, _, cx| {
                            this.edit(cx, |s| s.session_stats = !s.session_stats)
                        })),
                    )
                    .child(
                        ui::primary_button("nb-plan-run", t("notebook.analysis.plan.run"))
                            .disabled(running)
                            .on_click(move |_, _, app| {
                                if let Some(nb) = notebook.upgrade() {
                                    nb.update(app, |nb, cx| nb.request_run_from_host(&id, cx));
                                }
                            }),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .gap(px(s(8.0)))
                    .child(field(
                        t("notebook.analysis.sweep.parameter"),
                        picker(
                            "nb-sweep-param",
                            if sweep_param.is_empty() {
                                t("notebook.analysis.sweep.no_parameter")
                            } else {
                                sweep_param.clone()
                            },
                            params.iter().map(|p| (p.clone(), p.clone())).collect(),
                            Some(sweep_param),
                            on_param,
                        ),
                    ))
                    .children(self.values_input.clone().map(|i| {
                        field(
                            t("notebook.analysis.sweep.values"),
                            div().w(px(s(180.0))).child(Input::new(&i).small()),
                        )
                    }))
                    .children(self.reps_input.clone().map(|i| {
                        field(
                            t("notebook.analysis.sweep.repetitions"),
                            div().w(px(s(50.0))).child(Input::new(&i).small()),
                        )
                    })),
            )
            .into_any_element()
    }

    fn prompt_box(&self, prompt: Prompt) -> AnyElement {
        let s = Theme::scaled;
        let (message, action) = match prompt {
            Prompt::ConfirmActual => (
                "notebook.analysis.plan.confirm_actual",
                "notebook.analysis.plan.confirm_actual_button",
            ),
            Prompt::AuthorizeProductionSweep => (
                "notebook.analysis.sweep.authorize_production",
                "notebook.analysis.sweep.authorize_button",
            ),
        };
        let notebook = self.notebook.clone();
        let id = self.cell_id.clone();
        div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap(px(s(8.0)))
            .p(px(s(6.0)))
            .rounded(px(s(4.0)))
            .border_1()
            .border_color(Theme::warning())
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .text_color(Theme::warning())
                    .child(t(message)),
            )
            .child(
                ui::primary_button("nb-plan-authorize", t(action)).on_click(move |_, _, app| {
                    if let Some(nb) = notebook.upgrade() {
                        nb.update(app, |nb, cx| nb.authorize_and_run(&id, cx));
                    }
                }),
            )
            .into_any_element()
    }
}

fn apply_sweep(spec: &mut PlanCellSpec, change: impl FnOnce(&mut SweepSpec)) {
    let mut sweep = spec.sweep.clone().unwrap_or(SweepSpec {
        parameter: String::new(),
        values: Vec::new(),
        repetitions: 3,
    });
    change(&mut sweep);
    spec.sweep = (!sweep.parameter.is_empty() || !sweep.values.is_empty()).then_some(sweep);
}

fn plan_column(title: String, plans: &BatchPlans) -> AnyElement {
    let s = Theme::scaled;
    let mut col = div()
        .flex()
        .flex_col()
        .flex_1()
        .min_w(px(s(240.0)))
        .gap(px(s(1.0)))
        .child(ui::section(title));
    for (i, statement) in plans.statements.iter().enumerate() {
        if plans.statements.len() > 1 {
            col = col.child(caption(crate::localization::format(
                "notebook.analysis.plan.statement",
                &[("index", (i + 1).to_string())],
            )));
        }
        for row in &statement.table {
            col = col.child(compact::operator_row(
                row,
                rows_detail(statement, row.estimated_rows, row.rows),
            ));
        }
    }
    col.into_any_element()
}

impl Render for PlanView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let output_only =
            crate::notebook::present::cell_output_only(&self.notebook, &self.cell_id, cx);
        let toolbar = self.toolbar(window, cx);
        let mut root = div()
            .flex()
            .flex_col()
            .gap(px(s(6.0)))
            .px(px(s(10.0)))
            .py(px(s(8.0)))
            .w_full()
            .when(!output_only, |d| d.child(toolbar))
            .children(self.prompt.map(|p| self.prompt_box(p)));
        let Some(result) = &self.result else {
            return root.child(caption(t("notebook.analysis.plan.empty")));
        };
        let explain = t("notebook.ai.explain.button");
        root = root.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(s(8.0)))
                .child(chip(source_label(result), Theme::accent()))
                .when(!output_only, |d| {
                    d.child(
                        gpui_component::button::Button::new("nb-plan-explain-ai")
                            .ghost()
                            .xsmall()
                            .label(explain.clone())
                            .accessibility_label(explain)
                            .on_click(cx.listener(|this, _, _, cx| this.explain_with_ai(cx))),
                    )
                }),
        );
        let Some(analysed) = &self.analysed else {
            return root;
        };
        for statement in &analysed.plans.statements {
            root = root.child(compact::headline(statement));
        }
        let findings: Vec<AnyElement> = analysed
            .plans
            .statements
            .iter()
            .flat_map(|statement| compact::findings("nb-plan-finding-learn", statement, 8))
            .take(8)
            .collect();
        if !findings.is_empty() {
            root = root
                .child(ui::section(t("notebook.analysis.plan.findings")))
                .children(findings);
        }
        root = root.child(
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap(px(s(12.0)))
                .children(
                    analysed
                        .baseline
                        .as_ref()
                        .map(|b| plan_column(t("notebook.analysis.plan.baseline"), b)),
                )
                .child(plan_column(
                    t(if analysed.baseline.is_some() {
                        "notebook.analysis.plan.this_plan"
                    } else {
                        "notebook.analysis.plan.operators"
                    }),
                    &analysed.plans,
                )),
        );
        match &analysed.comparison {
            Some(Ok(report)) => {
                root = root
                    .child(ui::section(t("notebook.analysis.plan.changes")))
                    .child(compact::changes(report, 20));
            }
            Some(Err(e)) => {
                root = root.child(caption(crate::localization::message(e)));
            }
            None => {}
        }
        if let Some(source) = result.stats_source {
            root = root.child(ui::section(stats_label(source, &result.engine)));
            if result.stats.is_empty() {
                root = root.child(caption(t("notebook.analysis.plan.stats_none")));
            }
            for d in &result.stats {
                root = root.child(caption(format!(
                    "{}: {} → {} ({:+})",
                    d.name,
                    number(d.before),
                    number(d.after),
                    d.delta()
                )));
            }
        }
        if let Some(note) = &result.sweep_note {
            root = root.child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .text_color(Theme::warning())
                    .child(crate::localization::text(note)),
            );
        }
        if !result.sweep.is_empty() {
            root = root
                .child(ui::section(crate::localization::format(
                    "notebook.analysis.sweep.title",
                    &[("engine", result.engine.as_str())],
                )))
                .child(caption(t("notebook.analysis.sweep.measured")));
            let base = result.sweep[0].timing;
            for (i, point) in result.sweep.iter().enumerate() {
                let verdict =
                    (i > 0).then(|| t(verdict_key(compare_timings(&base, &point.timing))));
                root = root.child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .gap(px(s(8.0)))
                        .child(body_text(point.value.clone()))
                        .child(caption(crate::localization::format(
                            "notebook.analysis.sweep.timing",
                            &[
                                ("runs", point.timing.runs.to_string()),
                                ("mean", number(point.timing.mean_ms)),
                                ("sd", number(point.timing.stddev_ms)),
                                ("min", number(point.timing.min_ms)),
                                ("max", number(point.timing.max_ms)),
                                ("median", number(point.timing.median_ms)),
                            ],
                        )))
                        .when(point.errors > 0, |d| {
                            d.child(chip(
                                crate::localization::format(
                                    "notebook.analysis.sweep.errors",
                                    &[("count", point.errors.to_string())],
                                ),
                                Theme::error(),
                            ))
                        })
                        .when_some(verdict, |d, v| d.child(caption(v)))
                        .when(!point.stats.is_empty(), |d| {
                            d.child(caption(
                                point
                                    .stats
                                    .iter()
                                    .map(|s| format!("{}: {:+}", s.name, s.delta()))
                                    .collect::<Vec<_>>()
                                    .join(", "),
                            ))
                        }),
                );
            }
        }
        root
    }
}

#[cfg(test)]
pub fn operators(plan: &ExecutionPlan) -> Vec<String> {
    plan.nodes
        .iter()
        .map(sqlly_services::notebook::cells::plan::operator_name)
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use sqlly_services::plans::{PlanEdge, PlanNode};

    pub(crate) fn plan(ops: &[(&str, f64)]) -> ExecutionPlan {
        let nodes: Vec<PlanNode> = ops
            .iter()
            .enumerate()
            .map(|(i, (op, cost))| PlanNode {
                id: i as i32,
                logical_op: op.to_string(),
                physical_op: op.to_string(),
                estimated_rows: 100.0,
                estimated_cost: *cost,
                estimated_cpu: 0.0,
                estimated_io: 0.0,
                actual_rows: None,
                actual_rewinds: None,
                actual_rebinds: None,
                row_count: None,
                output_list: Vec::new(),
                warnings: Vec::new(),
                ..Default::default()
            })
            .collect();
        let edges = (1..nodes.len())
            .map(|i| PlanEdge {
                from_node: 0,
                to_node: i as i32,
            })
            .collect();
        ExecutionPlan {
            statement_text: "SELECT 1".into(),
            statement_type: "SELECT".into(),
            total_subtree_cost: ops.iter().map(|(_, c)| c).sum(),
            estimated_rows: 100.0,
            nodes,
            edges,
            warnings: Vec::new(),
            ..Default::default()
        }
    }

    #[test]
    fn compared_plans_report_operator_and_cost_changes() {
        let result = PlanResult {
            actual: false,
            engine: "SQL Server".into(),
            plans: vec![plan(&[("Select", 0.1), ("Table Scan", 4.0)])],
            baseline: Some(Ok(vec![plan(&[("Select", 0.1), ("Index Seek", 0.2)])])),
            stats_source: None,
            stats: Vec::new(),
            sweep: Vec::new(),
            sweep_note: None,
        };
        let cmp = result.comparison().unwrap().unwrap();
        assert!(
            cmp.changes().next().is_some(),
            "the operator that changed is named"
        );
        assert!(source_label(&result).contains("SQL Server"));
        let analysed = Analysed::new(&result);
        let lines = tree_lines(&analysed.plans.statements[0]);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1].0, 1, "the scan sits under the select");
        assert!(analysed.baseline.is_some());
        assert!(matches!(analysed.comparison, Some(Ok(_))));
        assert!(
            stats_label(StatsSource::DmExecSessions, "SQL Server").contains("dm_exec_sessions")
        );
        assert_eq!(operators(&result.plans[0])[1], "Table Scan");
    }
}
