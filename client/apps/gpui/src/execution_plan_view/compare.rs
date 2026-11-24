//! The comparison view: what changed between two captures of the same
//! query, as sentences marked better, worse or neutral.

use gpui::{AnyElement, Context, Hsla, IntoElement, Styled, div, prelude::*, px};
use gpui_component::Sizable as _;
use gpui_component::button::Button;
use sqlly_services::plans::{PlanChange, PlanChangeEffect};

use super::ExecutionPlanView;
use super::summary::paragraph;
use crate::theme::Theme;

/// The word for an effect. Shown beside every change so the verdict never
/// rests on colour.
fn effect_label(effect: PlanChangeEffect) -> String {
    let key = match effect {
        PlanChangeEffect::Better => "query.execution_plan.change_better",
        PlanChangeEffect::Worse => "query.execution_plan.change_worse",
        PlanChangeEffect::Neutral => "query.execution_plan.change_neutral",
    };
    crate::localization::text(key)
}

fn effect_color(effect: PlanChangeEffect) -> Hsla {
    match effect {
        PlanChangeEffect::Better => Theme::success(),
        PlanChangeEffect::Worse => Theme::error(),
        PlanChangeEffect::Neutral => Theme::text_dim(),
    }
}

pub(super) fn effect_tag(effect: PlanChangeEffect) -> impl IntoElement {
    let s = Theme::scaled;
    let color = effect_color(effect);
    div()
        .flex_shrink_0()
        .min_w(px(s(52.0)))
        .px(px(s(5.0)))
        .rounded(px(s(3.0)))
        .border_1()
        .border_color(color)
        .text_center()
        .text_size(px(Theme::font_size_ratio(0.82)))
        .text_color(color)
        .child(effect_label(effect))
}

impl ExecutionPlanView {
    /// Leave the comparison and show the operator a change is about, when
    /// the plan on screen is the comparison capture.
    fn open_change(&mut self, change: &PlanChange, cx: &mut Context<Self>) {
        let Some(node_id) = change.comparison_node_id else {
            return;
        };
        if !self.comparison_shows_current() {
            return;
        }
        self.comparison_mode = false;
        if change.statement_index != self.active_statement
            && change.statement_index < self.statements.len()
        {
            self.active_statement = change.statement_index;
            self.reset_statement_state();
        }
        self.show_in_diagram(node_id, cx);
    }

    /// The comparison capture is the plan the other views are showing.
    fn comparison_shows_current(&self) -> bool {
        self.captures
            .get(self.comparison_capture)
            .is_some_and(|capture| Some(capture.sequence) == self.shown_capture)
    }

    pub(super) fn render_comparison(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let s = Theme::scaled;
        let pickers = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .flex_shrink_0()
            .gap(px(s(6.0)))
            .child(
                Button::new("plan-comparison-baseline")
                    .small()
                    .label(crate::localization::format(
                        "query.execution_plan.baseline_button",
                        &[("capture", &self.capture_label(self.baseline_capture))],
                    ))
                    .on_click(cx.listener(|this, _, _, cx| this.cycle_baseline(cx))),
            )
            .child(
                Button::new("plan-comparison-target")
                    .small()
                    .label(crate::localization::format(
                        "query.execution_plan.comparison_button",
                        &[("capture", &self.capture_label(self.comparison_capture))],
                    ))
                    .on_click(cx.listener(|this, _, _, cx| this.cycle_comparison(cx))),
            );
        let mut body = div()
            .id("plan-comparison")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .w_full()
            .overflow_y_scroll()
            .p(px(s(12.0)))
            .gap(px(s(10.0)))
            .child(pickers);

        let linkable = self.comparison_shows_current();
        match self.comparison_result.clone() {
            None => {}
            Some(Err(issue)) => {
                body = body.child(paragraph(issue.text(), Theme::warning()));
            }
            Some(Ok(report)) => {
                body = body.child(
                    div()
                        .flex_shrink_0()
                        .w_full()
                        .whitespace_normal()
                        .text_size(px(Theme::font_size_ratio(1.1)))
                        .text_color(Theme::text())
                        .child(crate::localization::message(&report.headline)),
                );
                let many = report.statements.len() > 1;
                let mut any_change = false;
                for statement in &report.statements {
                    let mut block = div().flex().flex_col().flex_shrink_0().gap(px(s(4.0)));
                    if many || statement.changes.is_empty() {
                        block = block.child(
                            div()
                                .flex()
                                .flex_row()
                                .items_start()
                                .gap(px(s(8.0)))
                                .child(effect_tag(statement.headline_effect))
                                .child(
                                    paragraph(
                                        crate::localization::message(&statement.headline),
                                        Theme::text(),
                                    )
                                    .flex_1()
                                    .min_w_0(),
                                ),
                        );
                    }
                    for (index, change) in statement.changes.iter().enumerate() {
                        any_change = true;
                        let clickable = linkable && change.comparison_node_id.is_some();
                        let target = change.clone();
                        block = block.child(
                            div()
                                .id(("plan-change", statement.statement_index * 1000 + index))
                                .flex()
                                .flex_row()
                                .items_start()
                                .gap(px(s(8.0)))
                                .px(px(s(4.0)))
                                .py(px(s(2.0)))
                                .rounded(px(s(3.0)))
                                .when(clickable, |row| {
                                    row.cursor_pointer()
                                        .hover(|style| style.bg(Theme::row_hover()))
                                })
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.open_change(&target, cx);
                                }))
                                .child(effect_tag(change.effect))
                                .child(
                                    paragraph(
                                        crate::localization::message(&change.message),
                                        Theme::text(),
                                    )
                                    .flex_1()
                                    .min_w_0(),
                                ),
                        );
                    }
                    body = body.child(block);
                }
                if !any_change {
                    body = body.child(paragraph(
                        crate::localization::text("query.execution_plan.no_operator_changes"),
                        Theme::text_dim(),
                    ));
                }
            }
        }
        body.into_any_element()
    }
}
