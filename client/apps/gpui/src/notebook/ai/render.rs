//! The AI cell body: mode, prompt and Run/Stop; the budget meter; the
//! cloud consent preview; the run's status; the plan awaiting approval;
//! questions; findings with their evidence labels and links; proposals
//! with Accept/Reject, Apply and Undo; and the queries the agent ran.

use gpui::{
    AnyElement, Context, ElementId, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, Window, div,
    prelude::FluentBuilder as _, px, relative,
};
use gpui_component::Disableable as _;
use gpui_component::Sizable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::Textarea;
use sqlly_services::notebook::cells::ai::{
    AgentMode, AiCellSpec, AiResult, Budget, BudgetKind, BudgetUsage, DiffLine, Finding,
    FindingKind, InvestigationPlan, Proposal, ProposalAction, ProposalStatus, ReviewCheck,
    RunStatus, UnverifiedReason,
};
use sqlly_services::notebook_agent::context::{Exclusion, PreviewKind};
use sqlly_services::notebook_agent::{ContextPreview, model_location};
use sqlly_services::notebook_security::authorize::ModelLocation;

use super::{AiCellSnapshot, LiveRun};
use crate::notebook::cells::CellRenderCx;
use crate::notebook::view::NotebookView;
use crate::theme::Theme;

fn t(key: &str) -> String {
    crate::localization::text(key)
}

fn s(v: f32) -> f32 {
    Theme::scaled(v)
}

fn button(id: impl Into<ElementId>, label: String) -> Button {
    Button::new(id)
        .ghost()
        .xsmall()
        .label(label.clone())
        .accessibility_label(label)
}

fn primary(id: impl Into<ElementId>, label: String) -> Button {
    Button::new(id)
        .primary()
        .xsmall()
        .label(label.clone())
        .accessibility_label(label)
}

fn small(text: impl Into<SharedString>) -> gpui::Div {
    div()
        .text_size(px(Theme::font_size_ratio(0.85)))
        .text_color(Theme::text_dim())
        .child(text.into())
}

fn title(text: String) -> gpui::Div {
    div()
        .text_size(px(Theme::font_size_ratio(0.9)))
        .text_color(Theme::text())
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .child(text)
}

fn badge(text: String, color: gpui::Hsla) -> gpui::Div {
    div()
        .flex_none()
        .px(px(s(5.0)))
        .rounded(px(s(3.0)))
        .border_1()
        .border_color(color)
        .text_size(px(Theme::font_size_ratio(0.75)))
        .text_color(color)
        .child(text)
}

fn panel() -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap(px(s(4.0)))
        .px(px(s(10.0)))
        .py(px(s(6.0)))
        .rounded(px(s(4.0)))
        .border_1()
        .border_color(Theme::border())
        .bg(Theme::bg())
}

pub fn mode_label_key(mode: AgentMode) -> &'static str {
    match mode {
        AgentMode::Assist => "notebook.ai.mode.assist",
        AgentMode::Plan => "notebook.ai.mode.plan",
        AgentMode::Build => "notebook.ai.mode.build",
        AgentMode::Investigate => "notebook.ai.mode.investigate",
        AgentMode::Review => "notebook.ai.mode.review",
        AgentMode::ExplainChanges => "notebook.ai.mode.explain_changes",
    }
}

fn mode_hint_key(mode: AgentMode) -> &'static str {
    match mode {
        AgentMode::Assist => "notebook.ai.mode_hint.assist",
        AgentMode::Plan => "notebook.ai.mode_hint.plan",
        AgentMode::Build => "notebook.ai.mode_hint.build",
        AgentMode::Investigate => "notebook.ai.mode_hint.investigate",
        AgentMode::Review => "notebook.ai.mode_hint.review",
        AgentMode::ExplainChanges => "notebook.ai.mode_hint.explain_changes",
    }
}

pub fn finding_kind_key(kind: FindingKind) -> &'static str {
    match kind {
        FindingKind::ObservedFact => "notebook.ai.findings.kind.observed_fact",
        FindingKind::DerivedMetric => "notebook.ai.findings.kind.derived_metric",
        FindingKind::Hypothesis => "notebook.ai.findings.kind.hypothesis",
        FindingKind::Recommendation => "notebook.ai.findings.kind.recommendation",
    }
}

fn finding_color(kind: FindingKind) -> gpui::Hsla {
    match kind {
        FindingKind::ObservedFact => Theme::success(),
        FindingKind::DerivedMetric => Theme::accent(),
        FindingKind::Hypothesis => Theme::warning(),
        FindingKind::Recommendation => Theme::text(),
    }
}

fn unverified_key(reason: UnverifiedReason) -> &'static str {
    match reason {
        UnverifiedReason::NotExecuted => "notebook.ai.findings.unverified_reason.not_executed",
        UnverifiedReason::NoEvidence => "notebook.ai.findings.unverified_reason.no_evidence",
        UnverifiedReason::UnknownEvidence => {
            "notebook.ai.findings.unverified_reason.unknown_evidence"
        }
        UnverifiedReason::ValueNotInEvidence => {
            "notebook.ai.findings.unverified_reason.value_not_in_evidence"
        }
        UnverifiedReason::IncompleteEvidence => {
            "notebook.ai.findings.unverified_reason.incomplete_evidence"
        }
    }
}

fn check_key(check: ReviewCheck) -> &'static str {
    match check {
        ReviewCheck::FanoutJoin => "notebook.ai.check.fanout_join",
        ReviewCheck::AmbiguousAggregate => "notebook.ai.check.ambiguous_aggregate",
        ReviewCheck::StaleResult => "notebook.ai.check.stale_result",
        ReviewCheck::MissingFilter => "notebook.ai.check.missing_filter",
        ReviewCheck::SampledData => "notebook.ai.check.sampled_data",
        ReviewCheck::Timezone => "notebook.ai.check.timezone",
        ReviewCheck::Security => "notebook.ai.check.security",
        ReviewCheck::AmbiguousTerm => "notebook.ai.check.ambiguous_term",
        ReviewCheck::ChangeExplained => "notebook.ai.check.change_explained",
    }
}

fn stopped_key(kind: Option<BudgetKind>) -> &'static str {
    match kind {
        Some(BudgetKind::Queries) => "notebook.ai.status.stopped_queries",
        Some(BudgetKind::Rows) => "notebook.ai.status.stopped_rows",
        Some(BudgetKind::Runtime) => "notebook.ai.status.stopped_runtime",
        Some(BudgetKind::Tokens) => "notebook.ai.status.stopped_tokens",
        Some(BudgetKind::Turns) => "notebook.ai.status.stopped_turns",
        Some(BudgetKind::Cost) => "notebook.ai.status.stopped_cost",
        None => "notebook.ai.status.stopped",
    }
}

fn preview_kind_key(kind: PreviewKind) -> &'static str {
    match kind {
        PreviewKind::Instructions => "notebook.ai.consent.item.instructions",
        PreviewKind::Request => "notebook.ai.consent.item.request",
        PreviewKind::NotebookOutline => "notebook.ai.consent.item.notebook_outline",
        PreviewKind::TargetCell => "notebook.ai.consent.item.target_cell",
        PreviewKind::Findings => "notebook.ai.consent.item.findings",
    }
}

/// A finding's text in the UI language: its catalog message when SQLLY
/// wrote it, else the model's words.
pub fn finding_text(finding: &Finding) -> String {
    match &finding.message {
        Some(message) => crate::localization::stored_message(message, &finding.text),
        None => finding.text.clone(),
    }
}

type Labels = std::collections::HashMap<String, String>;

fn cell_label(labels: &Labels, id: &str) -> String {
    labels.get(id).cloned().unwrap_or_else(|| id.to_string())
}

fn meter(label: String, used: f64, max: f64) -> gpui::Div {
    let fraction = if max > 0.0 {
        (used / max).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let color = if fraction >= 0.9 {
        Theme::warning()
    } else {
        Theme::accent()
    };
    div()
        .flex()
        .flex_col()
        .gap(px(s(2.0)))
        .min_w(px(s(96.0)))
        .child(small(label))
        .child(
            div()
                .h(px(s(4.0)))
                .w_full()
                .rounded(px(s(2.0)))
                .bg(Theme::input_bg())
                .child(
                    div()
                        .h_full()
                        .w(relative(fraction as f32))
                        .rounded(px(s(2.0)))
                        .bg(color),
                ),
        )
}

fn seconds(ms: u64) -> String {
    format!("{:.1}", ms as f64 / 1000.0)
}

fn budget_meter(budget: &Budget, usage: &BudgetUsage) -> gpui::Div {
    let mut row = div()
        .id("nb-ai-budget")
        .flex()
        .flex_row()
        .flex_wrap()
        .gap(px(s(12.0)))
        .child(small(t("notebook.ai.budget.title")));
    let budget = budget.clamped();
    if budget.max_queries == 0 {
        row = row.child(small(t("notebook.ai.budget.no_queries")));
    } else {
        row = row
            .child(meter(
                crate::localization::format(
                    "notebook.ai.budget.queries",
                    &[
                        ("used", usage.queries.to_string()),
                        ("max", budget.max_queries.to_string()),
                    ],
                ),
                f64::from(usage.queries),
                f64::from(budget.max_queries),
            ))
            .child(meter(
                crate::localization::format(
                    "notebook.ai.budget.runtime",
                    &[
                        ("used", seconds(usage.runtime_ms)),
                        ("max", seconds(budget.max_runtime_ms)),
                    ],
                ),
                usage.runtime_ms as f64,
                budget.max_runtime_ms as f64,
            ));
    }
    row = row
        .child(meter(
            crate::localization::format(
                "notebook.ai.budget.rows",
                &[
                    ("used", usage.rows.to_string()),
                    ("max", budget.max_rows.to_string()),
                ],
            ),
            usage.rows as f64,
            budget.max_rows as f64,
        ))
        .child(meter(
            crate::localization::format(
                "notebook.ai.budget.tokens",
                &[
                    ("used", usage.tokens.to_string()),
                    ("max", budget.max_tokens.to_string()),
                ],
            ),
            usage.tokens as f64,
            budget.max_tokens as f64,
        ))
        .child(meter(
            crate::localization::format(
                "notebook.ai.budget.turns",
                &[
                    ("used", usage.turns.to_string()),
                    ("max", budget.max_turns.to_string()),
                ],
            ),
            f64::from(usage.turns),
            f64::from(budget.max_turns),
        ));
    match cost_line(&budget, usage) {
        Some(CostLine::Meter { label, used, cap }) => row = row.child(meter(label, used, cap)),
        Some(CostLine::Note(label)) => row = row.child(small(label)),
        None => {}
    }
    div().child(row)
}

/// What the budget meter says about cost.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum CostLine {
    /// Estimated cost against the cap.
    Meter {
        label: String,
        used: f64,
        cap: f64,
    },
    Note(String),
}

fn dollars(value: f64) -> String {
    if value > 0.0 && value < 0.01 {
        format!("{value:.4}")
    } else {
        format!("{value:.2}")
    }
}

/// The estimated cost when the provider has a price, against the cap if
/// there is one; once a run has started, a cap with no price says it could
/// not apply.
pub(super) fn cost_line(budget: &Budget, usage: &BudgetUsage) -> Option<CostLine> {
    match (usage.cost_usd, budget.max_cost_usd) {
        (Some(used), Some(cap)) => Some(CostLine::Meter {
            label: crate::localization::format(
                "notebook.ai.budget.cost",
                &[("used", dollars(used)), ("max", dollars(cap))],
            ),
            used,
            cap,
        }),
        (Some(used), None) => Some(CostLine::Note(crate::localization::format(
            "notebook.ai.budget.cost_estimate",
            &[("used", dollars(used))],
        ))),
        (None, Some(cap)) if usage.turns > 0 => Some(CostLine::Note(crate::localization::format(
            "notebook.ai.budget.cost_unpriced",
            &[("max", dollars(cap))],
        ))),
        (None, Some(cap)) => Some(CostLine::Meter {
            label: crate::localization::format(
                "notebook.ai.budget.cost",
                &[("used", dollars(0.0)), ("max", dollars(cap))],
            ),
            used: 0.0,
            cap,
        }),
        (None, None) => None,
    }
}

fn status_line(run: Option<&LiveRun>, result: Option<&AiResult>) -> Option<(String, gpui::Hsla)> {
    if let Some(run) = run {
        let (turn, max) = run.turn;
        let text = match &run.tool {
            Some(tool) => crate::localization::format(
                "notebook.ai.status.running_tool",
                &[
                    ("turn", turn.to_string()),
                    ("max", max.to_string()),
                    ("tool", tool.clone()),
                ],
            ),
            None => crate::localization::format(
                "notebook.ai.status.running",
                &[("turn", turn.to_string()), ("max", max.to_string())],
            ),
        };
        return Some((text, Theme::accent()));
    }
    let result = result?;
    Some(match result.status {
        RunStatus::Completed => (t("notebook.ai.status.completed"), Theme::success()),
        RunStatus::AwaitingApproval => (t("notebook.ai.status.awaiting_approval"), Theme::accent()),
        RunStatus::NeedsClarification => (
            t("notebook.ai.status.needs_clarification"),
            Theme::warning(),
        ),
        RunStatus::Stopped => (t(stopped_key(result.stopped_by)), Theme::warning()),
        RunStatus::Failed | RunStatus::Refused => (
            match &result.detail {
                // The agent's own reasons arrive as their catalog keys.
                Some(detail) if detail == sqlly_services::notebook_agent::NO_MODEL_DETAIL => {
                    t("notebook.ai.host.no_model")
                }
                Some(detail)
                    if detail == sqlly_services::notebook_agent::INSTRUCTIONS_ECHOED_DETAIL =>
                {
                    t("notebook.ai.host.instructions_echoed")
                }
                Some(detail) => crate::localization::format(
                    "notebook.ai.status.failed",
                    &[("detail", detail.as_str())],
                ),
                None => t("notebook.ai.status.failed_empty"),
            },
            Theme::error(),
        ),
    })
}

fn consent_panel(
    id: &str,
    preview: &ContextPreview,
    view: &Labels,
    cx: &mut Context<NotebookView>,
) -> AnyElement {
    let mut items = div().flex().flex_col().gap(px(s(4.0)));
    for (i, item) in preview.items.iter().enumerate() {
        items = items.child(small(t(preview_kind_key(item.kind)))).child(
            div()
                .id(("nb-ai-preview-item", i))
                .max_h(px(s(140.0)))
                .overflow_y_scroll()
                .px(px(s(6.0)))
                .py(px(s(4.0)))
                .rounded(px(s(3.0)))
                .bg(Theme::input_bg())
                .font_family(crate::theme::Theme::editor_font_family())
                .text_size(px(Theme::font_size_ratio(0.8)))
                .text_color(Theme::text())
                .child(item.text.clone()),
        );
    }
    let mut excluded = div().flex().flex_col().gap(px(s(2.0)));
    for exclusion in &preview.excluded {
        let text = match exclusion {
            Exclusion::Credentials => t("notebook.ai.consent.excluded_credentials"),
            Exclusion::SensitiveParameter { name } => crate::localization::format(
                "notebook.ai.consent.excluded_parameter",
                &[("name", name.as_str())],
            ),
            Exclusion::SensitiveColumn { cell, column } => crate::localization::format(
                "notebook.ai.consent.excluded_column",
                &[
                    ("column", column.clone()),
                    ("cell_name", cell_label(view, cell)),
                ],
            ),
        };
        excluded = excluded.child(small(format!("• {text}")));
    }
    let body = if preview.may_read_rows {
        crate::localization::format(
            "notebook.ai.consent.body",
            &[
                ("rows", preview.max_rows.to_string()),
                ("queries", preview.max_queries.to_string()),
            ],
        )
    } else {
        t("notebook.ai.consent.body_no_rows")
    };
    let share = id.to_string();
    let cancel = id.to_string();
    panel()
        .id("nb-ai-consent")
        .border_color(Theme::warning())
        .child(title(crate::localization::format(
            "notebook.ai.consent.title",
            &[("provider", preview.provider.as_str())],
        )))
        .child(small(body))
        .child(items)
        .child(small(t("notebook.ai.consent.excluded_title")))
        .child(excluded)
        .child(
            div()
                .flex()
                .flex_row()
                .gap(px(s(6.0)))
                .child(
                    primary("nb-ai-consent-share", t("notebook.ai.consent.share")).on_click(
                        cx.listener(move |this, _, _, cx| this.ai_request_run(&share, true, cx)),
                    ),
                )
                .child(
                    button("nb-ai-consent-cancel", t("notebook.ai.consent.cancel")).on_click(
                        cx.listener(move |this, _, _, cx| this.ai_dismiss_consent(&cancel, cx)),
                    ),
                ),
        )
        .into_any_element()
}

fn plan_panel(id: &str, plan: &InvestigationPlan, cx: &mut Context<NotebookView>) -> AnyElement {
    let list = |key: &str, items: &[String]| {
        (!items.is_empty()).then(|| {
            div()
                .flex()
                .flex_col()
                .child(small(t(key)))
                .children(items.iter().map(|i| {
                    div()
                        .text_size(px(Theme::font_size_ratio(0.85)))
                        .text_color(Theme::text())
                        .child(format!("• {i}"))
                }))
        })
    };
    let mut steps = div()
        .flex()
        .flex_col()
        .child(small(t("notebook.ai.plan.steps")));
    for (n, step) in plan.steps.iter().enumerate() {
        let mut line = format!("{}. {}", n + 1, step.title);
        if !step.detail.is_empty() {
            line.push_str(&format!(" — {}", step.detail));
        }
        steps = steps.child(
            div()
                .text_size(px(Theme::font_size_ratio(0.85)))
                .text_color(Theme::text())
                .child(line),
        );
    }
    let approve = id.to_string();
    panel()
        .id("nb-ai-plan")
        .child(title(t("notebook.ai.plan.title")))
        .when(!plan.goal.is_empty(), |d| {
            d.child(small(t("notebook.ai.plan.goal"))).child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.9)))
                    .text_color(Theme::text())
                    .child(plan.goal.clone()),
            )
        })
        .child(steps)
        .children(list("notebook.ai.plan.sources", &plan.sources))
        .children(list("notebook.ai.plan.assumptions", &plan.assumptions))
        .children(list("notebook.ai.plan.concerns", &plan.concerns))
        .children(list("notebook.ai.plan.permissions", &plan.permissions))
        .child(if plan.approved {
            badge(t("notebook.ai.plan.approved"), Theme::success()).into_any_element()
        } else {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(s(8.0)))
                .child(
                    primary("nb-ai-plan-approve", t("notebook.ai.plan.approve")).on_click(
                        cx.listener(move |this, _, _, cx| this.ai_approve_plan(&approve, cx)),
                    ),
                )
                .child(small(t("notebook.ai.plan.nothing_runs")))
                .into_any_element()
        })
        .into_any_element()
}

fn findings_panel(result: &AiResult, view: &Labels, cx: &mut Context<NotebookView>) -> AnyElement {
    let mut list = div().flex().flex_col().gap(px(s(6.0)));
    for (n, finding) in result.findings.iter().enumerate() {
        let mut head = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap(px(s(5.0)))
            .child(badge(
                t(finding_kind_key(finding.kind)),
                finding_color(finding.kind),
            ));
        if !finding.verified {
            let reason = finding
                .unverified_reason
                .map(|r| t(unverified_key(r)))
                .unwrap_or_default();
            head = head.child(badge(t("notebook.ai.findings.unverified"), Theme::error()));
            if !reason.is_empty() {
                head = head.child(small(reason));
            }
        }
        if let Some(check) = finding.check {
            head = head.child(badge(
                crate::localization::format(
                    "notebook.ai.findings.check_badge",
                    &[("check", t(check_key(check)))],
                ),
                Theme::text_dim(),
            ));
        }
        let mut links = div().flex().flex_row().flex_wrap().gap(px(s(4.0)));
        for (e, evidence) in finding.evidence.iter().enumerate() {
            let label = match (&evidence.cell, &evidence.run, &evidence.query) {
                (Some(cell), Some(run), _) => crate::localization::format(
                    "notebook.ai.findings.evidence_cell",
                    &[
                        ("cell_name", cell_label(view, cell)),
                        ("run", run.chars().take(8).collect::<String>()),
                    ],
                ),
                (Some(cell), None, _) => cell_label(view, cell),
                (None, _, Some(query)) => crate::localization::format(
                    "notebook.ai.findings.evidence_query",
                    &[("query", query.as_str())],
                ),
                _ => continue,
            };
            let label = match &evidence.group {
                Some(group) => crate::localization::format(
                    "notebook.ai.findings.evidence_group",
                    &[("evidence", label.as_str()), ("group", group.as_str())],
                ),
                None => label,
            };
            match evidence.cell.clone() {
                Some(cell) => {
                    links = links.child(
                        button(("nb-ai-evidence", n * 100 + e), label)
                            .tooltip(t("notebook.ai.findings.evidence_tooltip"))
                            .on_click(cx.listener(move |this, _, _, cx| this.ai_reveal(&cell, cx))),
                    )
                }
                None => links = links.child(badge(label, Theme::text_dim())),
            }
        }
        let conflicts = (!finding.conflicts_with.is_empty()).then(|| {
            small(crate::localization::format(
                "notebook.ai.findings.conflicts",
                &[("findings", finding.conflicts_with.join(", "))],
            ))
            .text_color(Theme::warning())
        });
        list = list.child(
            div()
                .flex()
                .flex_col()
                .gap(px(s(2.0)))
                .child(head)
                .child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.9)))
                        .text_color(Theme::text())
                        .child(format!("{} {}", finding.id, finding_text(finding))),
                )
                .children(conflicts)
                .when(!finding.evidence.is_empty(), |d| d.child(links)),
        );
    }
    panel()
        .id("nb-ai-findings")
        .child(title(t("notebook.ai.findings.title")))
        .child(list)
        .into_any_element()
}

fn proposal_summary(proposal: &Proposal, view: &Labels) -> String {
    match &proposal.action {
        ProposalAction::InsertCells { after, cells } => crate::localization::format(
            if cells.len() == 1 {
                "notebook.ai.proposals.insert_one"
            } else {
                "notebook.ai.proposals.insert"
            },
            &[
                ("count", cells.len().to_string()),
                (
                    "cell_name",
                    after
                        .as_deref()
                        .map(|a| cell_label(view, a))
                        .unwrap_or_else(|| t("notebook.ai.proposals.this_cell")),
                ),
            ],
        ),
        ProposalAction::EditCell { cell_id, .. } => crate::localization::format(
            "notebook.ai.proposals.edit",
            &[("cell_name", cell_label(view, cell_id))],
        ),
        ProposalAction::RunCells { cells } => crate::localization::format(
            "notebook.ai.proposals.run",
            &[(
                "cells",
                cells
                    .iter()
                    .map(|c| cell_label(view, c))
                    .collect::<Vec<_>>()
                    .join(", "),
            )],
        ),
    }
}

fn mono(text: String, color: gpui::Hsla) -> gpui::Div {
    div()
        .font_family(crate::theme::Theme::editor_font_family())
        .text_size(px(Theme::font_size_ratio(0.8)))
        .text_color(color)
        .child(text)
}

fn proposal_detail(proposal: &Proposal) -> gpui::Div {
    let mut detail = div().flex().flex_col().pl(px(s(8.0)));
    match &proposal.action {
        ProposalAction::InsertCells { cells, .. } => {
            for cell in cells {
                let first = cell
                    .source
                    .lines()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("")
                    .chars()
                    .take(120)
                    .collect::<String>();
                let name = cell.name.clone().unwrap_or_default();
                detail = detail.child(mono(
                    crate::localization::format(
                        "notebook.ai.proposals.cell_line",
                        &[
                            ("kind", cell.kind.as_str()),
                            ("name", name.as_str()),
                            ("first_line", first.as_str()),
                        ],
                    ),
                    Theme::text(),
                ));
            }
        }
        ProposalAction::EditCell { diff, .. } => {
            for line in diff.iter().take(40) {
                detail = detail.child(match line {
                    DiffLine::Same(text) => mono(format!("  {text}"), Theme::text_dim()),
                    DiffLine::Removed(text) => mono(format!("- {text}"), Theme::error()),
                    DiffLine::Added(text) => mono(format!("+ {text}"), Theme::success()),
                });
            }
        }
        ProposalAction::RunCells { .. } => {}
    }
    detail
}

fn proposals_panel(
    id: &str,
    result: &AiResult,
    can_undo: bool,
    view: &Labels,
    cx: &mut Context<NotebookView>,
) -> AnyElement {
    let mut list = div().flex().flex_col().gap(px(s(6.0)));
    for (n, proposal) in result.proposals.iter().enumerate() {
        let mut head = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap(px(s(6.0)))
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.9)))
                    .text_color(Theme::text())
                    .child(format!(
                        "{} {}",
                        proposal.id,
                        proposal_summary(proposal, view)
                    )),
            );
        head = match proposal.status {
            ProposalStatus::Pending | ProposalStatus::Accepted | ProposalStatus::Rejected => {
                let accepted = proposal.status == ProposalStatus::Accepted;
                let rejected = proposal.status == ProposalStatus::Rejected;
                let (accept, reject) = (id.to_string(), id.to_string());
                let (pa, pr) = (proposal.id.clone(), proposal.id.clone());
                let accept_button = if accepted {
                    primary(("nb-ai-accept", n), t("notebook.ai.proposals.accepted"))
                } else {
                    button(("nb-ai-accept", n), t("notebook.ai.proposals.accept"))
                };
                head.child(accept_button.on_click(
                    cx.listener(move |this, _, _, cx| this.ai_decide(&accept, &pa, true, cx)),
                ))
                .child(
                    button(
                        ("nb-ai-reject", n),
                        t(if rejected {
                            "notebook.ai.proposals.rejected"
                        } else {
                            "notebook.ai.proposals.reject"
                        }),
                    )
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.ai_decide(&reject, &pr, false, cx)),
                    ),
                )
            }
            ProposalStatus::Applied => head.child(badge(
                t("notebook.ai.proposals.status_applied"),
                Theme::success(),
            )),
            ProposalStatus::Refused => head.child(badge(
                t("notebook.ai.proposals.status_refused"),
                Theme::error(),
            )),
        };
        let refusal = proposal.refusal.as_ref().map(|r| {
            small(crate::localization::stored_message(
                r,
                proposal.refusal_text.as_deref().unwrap_or_default(),
            ))
            .text_color(Theme::error())
        });
        list = list.child(
            div()
                .flex()
                .flex_col()
                .gap(px(s(2.0)))
                .child(head)
                .when(!proposal.rationale.is_empty(), |d| {
                    d.child(small(proposal.rationale.clone()))
                })
                .children(refusal)
                .child(proposal_detail(proposal)),
        );
    }
    let accepted = result
        .proposals
        .iter()
        .filter(|p| p.status == ProposalStatus::Accepted)
        .count();
    let apply = id.to_string();
    panel()
        .id("nb-ai-proposals")
        .child(title(t("notebook.ai.proposals.title")))
        .child(list)
        .child(
            div()
                .flex()
                .flex_row()
                .gap(px(s(6.0)))
                .child(
                    primary(
                        "nb-ai-apply",
                        crate::localization::format(
                            "notebook.ai.proposals.apply",
                            &[("count", accepted)],
                        ),
                    )
                    .disabled(accepted == 0)
                    .on_click(cx.listener(move |this, _, _, cx| this.ai_apply(&apply, cx))),
                )
                .when(can_undo, |d| {
                    d.child(
                        button("nb-ai-undo", t("notebook.ai.proposals.undo"))
                            .on_click(cx.listener(|this, _, _, cx| this.ai_undo(cx))),
                    )
                }),
        )
        .into_any_element()
}

fn queries_panel(
    id: &str,
    result: &AiResult,
    open: bool,
    cx: &mut Context<NotebookView>,
) -> AnyElement {
    let toggle = id.to_string();
    let mut panel = panel().id("nb-ai-queries").child(
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(s(6.0)))
            .child(title(crate::localization::format(
                "notebook.ai.queries.title",
                &[("count", result.queries.len())],
            )))
            .child(
                button(
                    "nb-ai-queries-toggle",
                    t(if open {
                        "notebook.ai.queries.hide"
                    } else {
                        "notebook.ai.queries.show"
                    }),
                )
                .on_click(cx.listener(move |this, _, _, cx| this.ai_toggle_queries(&toggle, cx))),
            ),
    );
    if open {
        for query in &result.queries {
            let line = match &query.error {
                None => crate::localization::format(
                    "notebook.ai.queries.line",
                    &[
                        ("query", query.id.clone()),
                        ("connection", query.connection.clone()),
                        ("rows", query.rows.to_string()),
                        ("milliseconds", query.elapsed_ms.to_string()),
                    ],
                ),
                Some(_) => crate::localization::format(
                    "notebook.ai.queries.failed_line",
                    &[
                        ("query", query.id.as_str()),
                        ("connection", query.connection.as_str()),
                    ],
                ),
            };
            panel = panel
                .child(small(line))
                .child(mono(query.sql.clone(), Theme::text()));
        }
    }
    panel.into_any_element()
}

/// Explain changes' two run pickers: what to compare (the last time this
/// AI cell ran, or a recorded run) and what with (now, or another run).
fn compare_menus(
    id: &str,
    spec: &AiCellSpec,
    runs: &[(String, String)],
    running: bool,
    cx: &mut Context<NotebookView>,
) -> gpui::Div {
    use gpui_component::menu::{DropdownMenu as _, PopupMenuItem};
    let compare = spec.compare.clone().unwrap_or_default();
    let label_of = |run: &str, empty: &str| {
        runs.iter()
            .find(|(r, _)| r == run)
            .map(|(_, l)| l.clone())
            .unwrap_or_else(|| t(empty))
    };
    let before_label = if compare.before.is_empty() {
        t("notebook.ai.compare.last_ai_run")
    } else {
        label_of(&compare.before, "notebook.ai.compare.last_ai_run")
    };
    let after_label = if compare.after.is_empty() {
        t("notebook.ai.compare.now")
    } else {
        label_of(&compare.after, "notebook.ai.compare.now")
    };
    let view = cx.entity();
    let before_menu = {
        let (view, id, runs, after) = (
            view.clone(),
            id.to_string(),
            runs.to_vec(),
            compare.after.clone(),
        );
        button(
            "nb-ai-compare-before",
            crate::localization::format("notebook.ai.compare.before", &[("choice", before_label)]),
        )
        .disabled(running)
        .dropdown_menu(move |mut menu, _, _| {
            let pick = |run: String| {
                let (view, id, after) = (view.clone(), id.clone(), after.clone());
                move |_: &gpui::ClickEvent, _: &mut Window, app: &mut gpui::App| {
                    let after = if run.is_empty() {
                        String::new()
                    } else {
                        after.clone()
                    };
                    let run = run.clone();
                    view.update(app, |nb, cx| nb.ai_set_compare(&id, run, after, cx));
                }
            };
            menu = menu.item(
                PopupMenuItem::new(t("notebook.ai.compare.last_ai_run"))
                    .on_click(pick(String::new())),
            );
            for (run, label) in &runs {
                menu = menu.item(PopupMenuItem::new(label.clone()).on_click(pick(run.clone())));
            }
            menu
        })
    };
    let after_menu = {
        let (view, id, runs, before) =
            (view, id.to_string(), runs.to_vec(), compare.before.clone());
        button(
            "nb-ai-compare-after",
            crate::localization::format("notebook.ai.compare.after", &[("choice", after_label)]),
        )
        .disabled(running || compare.before.is_empty())
        .dropdown_menu(move |mut menu, _, _| {
            let pick = |run: String| {
                let (view, id, before) = (view.clone(), id.clone(), before.clone());
                move |_: &gpui::ClickEvent, _: &mut Window, app: &mut gpui::App| {
                    let (before, run) = (before.clone(), run.clone());
                    view.update(app, |nb, cx| nb.ai_set_compare(&id, before, run, cx));
                }
            };
            menu = menu.item(
                PopupMenuItem::new(t("notebook.ai.compare.now")).on_click(pick(String::new())),
            );
            for (run, label) in runs.iter().filter(|(r, _)| *r != before) {
                menu = menu.item(PopupMenuItem::new(label.clone()).on_click(pick(run.clone())));
            }
            menu
        })
    };
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(s(4.0)))
        .child(before_menu)
        .child(after_menu)
}

/// The AI cell's body.
pub fn body(
    cell: &CellRenderCx,
    snapshot: &AiCellSnapshot,
    _window: &mut Window,
    cx: &mut Context<NotebookView>,
) -> AnyElement {
    let view = &snapshot.labels;
    let id = cell.id.clone();
    let spec = &snapshot.spec;
    let result = spec.result.as_ref();
    let running = snapshot.run.is_some();
    if cell.collapsed {
        return div()
            .px(px(s(10.0)))
            .py(px(s(4.0)))
            .child(small(crate::localization::format(
                if result.is_some_and(|r| r.findings.len() == 1) {
                    "notebook.ai.cell.collapsed_one"
                } else {
                    "notebook.ai.cell.collapsed"
                },
                &[
                    ("mode", t(mode_label_key(spec.mode))),
                    (
                        "count",
                        result.map(|r| r.findings.len()).unwrap_or(0).to_string(),
                    ),
                ],
            )))
            .into_any_element();
    }

    let mut modes = div().flex().flex_row().flex_wrap().gap(px(s(2.0)));
    for (n, mode) in AgentMode::ALL.into_iter().enumerate() {
        let target = id.clone();
        let label = t(mode_label_key(mode));
        let b = if mode == spec.mode {
            primary(("nb-ai-mode", n), label)
        } else {
            button(("nb-ai-mode", n), label)
        };
        modes = modes.child(
            b.tooltip(t(mode_hint_key(mode)))
                .disabled(running)
                .on_click(cx.listener(move |this, _, _, cx| this.ai_set_mode(&target, mode, cx))),
        );
    }

    let run_target = id.clone();
    let run_button = if running {
        button("nb-ai-stop", t("notebook.ai.cell.stop"))
            .on_click(cx.listener(move |this, _, _, cx| this.ai_stop(&run_target, cx)))
    } else {
        primary(
            "nb-ai-run",
            t(if result.is_some() {
                "notebook.ai.cell.run_again"
            } else {
                "notebook.ai.cell.run"
            }),
        )
        .on_click(cx.listener(move |this, _, _, cx| this.ai_request_run(&run_target, false, cx)))
    };
    let mut controls = div()
        .flex()
        .flex_row()
        .flex_wrap()
        .items_center()
        .gap(px(s(8.0)))
        .child(run_button);
    if let Some(target) = &spec.target {
        controls = controls.child(small(crate::localization::format(
            "notebook.ai.cell.target",
            &[("cell_name", cell_label(view, target))],
        )));
    }
    if spec.mode == AgentMode::ExplainChanges {
        controls = controls.child(compare_menus(&id, spec, &snapshot.runs, running, cx));
    }
    if let Some(provider) = &snapshot.provider {
        let local = model_location(provider) == ModelLocation::Local;
        controls = controls.child(
            small(crate::ai_panel::agent_egress_label(provider)).text_color(if local {
                Theme::text_dim()
            } else {
                Theme::warning()
            }),
        );
    }

    let (budget, usage) = match (&snapshot.run, result) {
        (Some(run), _) => (run.budget.clone(), run.usage.clone()),
        (None, Some(r)) => (r.budget.clone(), r.usage.clone()),
        (None, None) => (spec.budget.clone(), BudgetUsage::default()),
    };

    let mut body = div()
        .flex()
        .flex_col()
        .gap(px(s(6.0)))
        .px(px(s(10.0)))
        .py(px(s(8.0)))
        .w_full()
        .child(modes)
        .children(
            snapshot
                .prompt
                .as_ref()
                .map(|input| div().w_full().child(Textarea::new(input).disabled(running))),
        )
        .child(controls)
        .child(budget_meter(&budget, &usage));

    if let Some(preview) = &snapshot.consent {
        body = body.child(consent_panel(&id, preview, view, cx));
    }
    if let Some((message, error)) = &snapshot.notice {
        body = body.child(small(message.clone()).text_color(if *error {
            Theme::error()
        } else {
            Theme::text_dim()
        }));
    }
    if let Some((text, color)) = status_line(snapshot.run.as_ref(), result) {
        body = body.child(small(text).text_color(color));
    }
    let Some(result) = result.filter(|_| !running) else {
        return body.into_any_element();
    };
    if !result.summary.is_empty() {
        body = body.child(
            div()
                .text_size(px(Theme::font_size_ratio(0.9)))
                .text_color(Theme::text())
                .child(result.summary.clone()),
        );
    }
    if let Some(plan) = &result.plan {
        body = body.child(plan_panel(&id, plan, cx));
    }
    if !result.clarifications.is_empty() {
        body = body.child(
            panel()
                .child(title(t("notebook.ai.clarify.title")))
                .children(result.clarifications.iter().map(|q| {
                    div()
                        .text_size(px(Theme::font_size_ratio(0.9)))
                        .text_color(Theme::text())
                        .child(format!("• {q}"))
                }))
                .child(small(t("notebook.ai.clarify.hint"))),
        );
    }
    if !result.findings.is_empty() {
        body = body.child(findings_panel(result, view, cx));
    }
    if !result.proposals.is_empty() {
        body = body.child(proposals_panel(&id, result, snapshot.can_undo, view, cx));
    }
    if !result.queries.is_empty() {
        body = body.child(queries_panel(&id, result, snapshot.show_queries, cx));
    }
    body.child(small(crate::localization::format(
        "notebook.ai.cell.provenance",
        &[
            ("provider", result.provider.as_str()),
            ("template", result.template_version.as_str()),
            ("run", &result.run_id.chars().take(8).collect::<String>()),
        ],
    )))
    .into_any_element()
}

/// The header badge of a cell the agent generated: "AI", from which AI cell.
pub fn attribution_badge(view: &NotebookView, id: &str) -> Option<gpui::Div> {
    let by = view
        .doc
        .model()
        .cell(id)
        .and_then(sqlly_services::notebook::cells::ai::generated_by)?;
    let from = view
        .doc
        .cell_by_id(&by.ai_cell)
        .map(|c| c.display_name().to_string())
        .unwrap_or(by.ai_cell);
    Some(
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(s(4.0)))
            .child(badge(
                t("notebook.ai.cell.attribution_badge"),
                Theme::accent(),
            ))
            .child(small(crate::localization::format(
                "notebook.ai.cell.attribution",
                &[("cell_name", from.as_str())],
            ))),
    )
}
