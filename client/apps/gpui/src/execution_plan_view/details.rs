//! The operator details panel shown beside the diagram, the operator table
//! and the time chart: what the selected operator is, what it did in this
//! plan, what it cost, and what the findings say about it.

use gpui::{AnyElement, Context, Div, IntoElement, SharedString, Styled, div, prelude::*, px};
use gpui_component::Sizable as _;
use gpui_component::button::Button;
use sqlly_services::plans::{
    EstimateDirection, OperatorTableColumn, OperatorTableRow, engine_links_for_operator_in,
    human_cost, human_duration_ms, human_factor, human_number, human_share, human_size_kb,
};

use super::summary::{bottleneck_tag, heading, learn_links, paragraph, severity_mark};
use super::{DETAILS_WIDTH, ExecutionPlanView, PlanViewMode, StatementPlans, glyphs};
use crate::theme::Theme;

/// One operator-table cell as a person reads it, or `None` when the plan
/// does not report that number for this operator.
pub(super) fn metric_text(row: &OperatorTableRow, column: OperatorTableColumn) -> Option<String> {
    use OperatorTableColumn as C;
    Some(match column {
        C::Step => row.step.to_string(),
        C::Operator => row.operator.clone(),
        C::Object => match (&row.object, &row.index) {
            (Some(object), Some(index)) => format!("{object} ({index})"),
            (Some(object), None) => object.clone(),
            (None, Some(index)) => index.clone(),
            (None, None) => return None,
        },
        C::SelfTime => human_duration_ms(row.self_time_ms?),
        C::TotalTime => human_duration_ms(row.inclusive_time_ms?),
        C::TimeShare => human_share(row.time_share?),
        C::SelfCost => human_cost(row.self_cost?),
        C::TotalCost => human_cost(row.inclusive_cost?),
        C::CostShare => human_share(row.cost_share?),
        C::Rows => human_number(row.rows?),
        C::EstimatedRows => human_number(row.estimated_rows?),
        C::EstimateFactor => {
            let factor = human_factor(row.estimate_factor?);
            // The arrow says which way the estimate missed: up when more
            // rows arrived than the optimizer expected.
            match row.estimate_direction {
                Some(EstimateDirection::Under) => format!("↑ {factor}×"),
                Some(EstimateDirection::Over) => format!("↓ {factor}×"),
                _ => format!("{factor}×"),
            }
        }
        C::Loops => human_number(row.loops?),
        C::RowsRemoved => human_number(row.rows_removed?),
        C::Io => human_number(row.io_pages?),
        C::Memory => human_size_kb(row.memory_kb?),
    })
}

/// The numbers the panel lists, in reading order.
const DETAIL_METRICS: [OperatorTableColumn; 13] = [
    OperatorTableColumn::SelfTime,
    OperatorTableColumn::TotalTime,
    OperatorTableColumn::TimeShare,
    OperatorTableColumn::SelfCost,
    OperatorTableColumn::TotalCost,
    OperatorTableColumn::CostShare,
    OperatorTableColumn::Rows,
    OperatorTableColumn::EstimatedRows,
    OperatorTableColumn::EstimateFactor,
    OperatorTableColumn::Loops,
    OperatorTableColumn::RowsRemoved,
    OperatorTableColumn::Io,
    OperatorTableColumn::Memory,
];

impl ExecutionPlanView {
    /// The panel for the selected operator, or a hint when nothing is
    /// selected.
    pub(super) fn render_details(
        &self,
        active: &StatementPlans,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let s = Theme::scaled;
        let panel = div()
            .id("plan-details")
            .flex()
            .flex_col()
            .flex_shrink_0()
            .w(px(s(DETAILS_WIDTH)))
            .h_full()
            .border_l_1()
            .border_color(Theme::border())
            .bg(Theme::panel_bg())
            .overflow_y_scroll()
            .track_scroll(&self.details_scroll);
        let Some(node_id) = self.selected_node else {
            return panel
                .child(div().p(px(s(12.0))).child(paragraph(
                    crate::localization::text("query.execution_plan.details_hint"),
                    Theme::text_dim(),
                )))
                .into_any_element();
        };
        let Some(node) = active.analysis.node(node_id) else {
            return panel.into_any_element();
        };
        let say = |message: &sqlly_core::DisplayMessage| crate::localization::message(message);
        let section = || div().flex().flex_col().flex_shrink_0().gap(px(s(3.0)));

        let mut title = section().child(
            div()
                .w_full()
                .whitespace_normal()
                .text_size(px(Theme::font_size_ratio(1.1)))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(Theme::text())
                .child(say(&node.title())),
        );
        let mut tags = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap(px(s(6.0)))
            .child(glyphs::category_glyph(node.category()).element(s(12.0), Theme::text_dim()))
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .text_color(Theme::text_dim())
                    .child(say(&node.category().label())),
            );
        if node.flags.bottleneck {
            tags = tags.child(
                div()
                    .px(px(s(5.0)))
                    .rounded(px(s(3.0)))
                    .border_1()
                    .border_color(Theme::error())
                    .text_size(px(Theme::font_size_ratio(0.8)))
                    .text_color(Theme::error())
                    .child(bottleneck_tag()),
            );
        }
        title = title.child(tags);

        let mut body = div()
            .flex()
            .flex_col()
            .p(px(s(12.0)))
            .gap(px(s(12.0)))
            .child(title);

        // What it did here, in the story's words.
        if let Some(step) = active.story.step_for_node(node_id) {
            let mut told = section().child(paragraph(say(&step.sentence), Theme::text()));
            for note in &step.notes {
                told = told.child(
                    paragraph(say(&note.message), Theme::text_dim())
                        .text_size(px(Theme::font_size_ratio(0.92))),
                );
            }
            body = body.child(told);
        }

        // What it cost.
        if let Some(row) = active.table.iter().find(|row| row.node_id == node_id) {
            let lines: Vec<Div> = DETAIL_METRICS
                .into_iter()
                .filter_map(|column| {
                    let value = metric_text(row, column)?;
                    Some(
                        div()
                            .flex()
                            .flex_row()
                            .justify_between()
                            .gap(px(s(8.0)))
                            .text_size(px(Theme::font_size_ratio(0.92)))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .whitespace_normal()
                                    .text_color(Theme::text_dim())
                                    .child(say(&column.label())),
                            )
                            .child(div().flex_shrink_0().text_color(Theme::text()).child(value)),
                    )
                })
                .collect();
            if !lines.is_empty() {
                body = body.child(section().children(lines));
            }
        }

        // What the findings say about it.
        let findings: Vec<usize> = active.findings_for_node(node_id).collect();
        if !findings.is_empty() {
            let mut list = section().child(heading(crate::localization::text(
                "query.execution_plan.details_findings_heading",
            )));
            for index in findings {
                let finding = &active.findings[index];
                list = list.child(
                    div()
                        .id(("plan-details-finding", index))
                        .flex()
                        .flex_row()
                        .items_start()
                        .gap(px(s(6.0)))
                        .cursor_pointer()
                        .hover(|style| style.bg(Theme::row_hover()))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.expanded_findings.insert(index);
                            this.view_mode = PlanViewMode::Summary;
                            this.select_finding(index, cx);
                            this.findings_scroll.scroll_to_item(index + 1);
                        }))
                        .child(
                            div()
                                .flex_shrink_0()
                                .pt(px(s(3.0)))
                                .child(severity_mark(finding.severity, s(10.0))),
                        )
                        .child(
                            paragraph(say(&finding.summary), Theme::text())
                                .flex_1()
                                .min_w_0(),
                        ),
                );
            }
            body = body.child(list);
        }

        // What this kind of operator is, in general.
        if node.operator.known {
            body = body.child(
                section()
                    .child(heading(crate::localization::text(
                        "query.execution_plan.details_about_heading",
                    )))
                    .child(paragraph(say(&node.operator.description), Theme::text()))
                    .child(paragraph(
                        say(&node.operator.performance),
                        Theme::text_dim(),
                    )),
            );
        }
        if let Some(links) = learn_links(
            "plan-operator-learn",
            engine_links_for_operator_in(
                node.operator.key,
                node.category(),
                active.analysis.engine,
            ),
            node.operator.key,
        ) {
            body = body.child(links);
        }

        // Everything the engine printed for this operator, on request.
        if let Some(position) = active.analysis.position(node_id)
            && let Some(plan_node) = active.plan.nodes.get(position)
        {
            let mut raw: Vec<(SharedString, SharedString)> = Vec::new();
            let mut add = |name: &str, value: Option<&str>| {
                if let Some(value) = value.filter(|value| !value.trim().is_empty()) {
                    raw.push((
                        SharedString::from(name.to_string()),
                        SharedString::from(value.to_string()),
                    ));
                }
            };
            for property in &plan_node.properties {
                add(&property.name, Some(&property.value));
            }
            let extra = &plan_node.extra;
            if !raw.is_empty() || !extra.is_empty() {
                let shown = self.show_raw_properties;
                let mut block = section().child(
                    div().flex().flex_row().child(
                        Button::new("plan-raw-properties")
                            .small()
                            .label(if shown {
                                crate::localization::text(
                                    "query.execution_plan.hide_engine_properties",
                                )
                            } else {
                                crate::localization::text(
                                    "query.execution_plan.show_engine_properties",
                                )
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.show_raw_properties = !this.show_raw_properties;
                                cx.notify();
                            })),
                    ),
                );
                if shown {
                    for (name, value) in raw {
                        block = block.child(
                            div()
                                .flex()
                                .flex_col()
                                .child(
                                    div()
                                        .text_size(px(Theme::font_size_ratio(0.8)))
                                        .text_color(Theme::text_dim())
                                        .child(name),
                                )
                                .child(
                                    div()
                                        .w_full()
                                        .whitespace_normal()
                                        .font_family(Theme::editor_font_family())
                                        .text_size(px(Theme::font_size_ratio(0.88)))
                                        .text_color(Theme::text())
                                        .child(value),
                                ),
                        );
                    }
                    for line in extra {
                        block = block.child(
                            div()
                                .w_full()
                                .whitespace_normal()
                                .font_family(Theme::editor_font_family())
                                .text_size(px(Theme::font_size_ratio(0.88)))
                                .text_color(Theme::text())
                                .child(SharedString::from(line.clone())),
                        );
                    }
                }
                body = body.child(block);
            }
        }

        panel.child(body).into_any_element()
    }
}
