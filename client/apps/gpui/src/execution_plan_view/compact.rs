//! The plan pane's building blocks in a compact, stateless form, for places
//! that show a plan inside something else (the notebook's plan cell). They
//! take a statement that was analysed once ([`BatchPlans`]) and only lay it
//! out, so the same headline, findings, marks and wording appear wherever a
//! plan does.

use gpui::{AnyElement, Div, IntoElement, Styled, div, prelude::*, px};
use sqlly_services::plans::{
    OperatorTableRow, PlanChangeReport, engine_links_for_finding, human_share,
};

use super::StatementPlans;
use super::compare::effect_tag;
use super::summary::{
    bottleneck_tag, learn_links, paragraph, severity_label, severity_mark, share_bar,
};
use super::{ExecutionPlanView, glyphs};
use crate::theme::Theme;

fn small(text: String) -> Div {
    div()
        .w_full()
        .whitespace_normal()
        .text_size(px(Theme::font_size_ratio(0.85)))
        .text_color(Theme::text_dim())
        .child(text)
}

/// What the statement did and what it cost, in a sentence, followed by the
/// story's few lines on the plan as a whole.
pub(crate) fn headline(statement: &StatementPlans) -> Div {
    let s = Theme::scaled;
    div()
        .flex()
        .flex_col()
        .gap(px(s(2.0)))
        .child(paragraph(
            crate::localization::message(&statement.story.headline),
            Theme::text(),
        ))
        .children(
            statement
                .story
                .summary
                .iter()
                .map(|line| small(crate::localization::message(line))),
        )
}

/// The first `limit` findings, most important first: mark and word for the
/// severity, what is wrong, what to do, and where to read more.
pub(crate) fn findings(
    id: &'static str,
    statement: &StatementPlans,
    limit: usize,
) -> Vec<AnyElement> {
    let s = Theme::scaled;
    statement
        .findings
        .iter()
        .take(limit)
        .map(|finding| {
            let color = ExecutionPlanView::severity_color(finding.severity);
            div()
                .flex()
                .flex_row()
                .items_start()
                .gap(px(s(6.0)))
                .child(
                    div()
                        .flex_shrink_0()
                        .pt(px(s(3.0)))
                        .child(severity_mark(finding.severity, s(10.0))),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .gap(px(s(1.0)))
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .flex_wrap()
                                .items_baseline()
                                .gap(px(s(6.0)))
                                .child(
                                    div()
                                        .flex_shrink_0()
                                        .text_size(px(Theme::font_size_ratio(0.82)))
                                        .text_color(color)
                                        .child(severity_label(finding.severity)),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .whitespace_normal()
                                        .text_size(px(Theme::font_size_ratio(0.9)))
                                        .text_color(Theme::text())
                                        .child(crate::localization::message(&finding.summary)),
                                ),
                        )
                        .child(small(crate::localization::message(&finding.action)))
                        .children(learn_links(
                            id,
                            engine_links_for_finding(
                                finding.kind.as_str(),
                                statement.analysis.engine,
                            )
                            .into_iter()
                            .take(1)
                            .collect(),
                            finding.kind.as_str(),
                        )),
                )
                .into_any_element()
        })
        .collect()
}

/// The operator's share of the statement: of its time when the plan was
/// timed, otherwise of its estimated cost.
pub(crate) fn row_share(row: &OperatorTableRow) -> Option<f64> {
    row.time_share.or(row.cost_share)
}

/// One line of the operator tree: indentation for depth, the operator
/// family's shape, the operator and what it reads, its share as a bar and a
/// number, then `detail`.
pub(crate) fn operator_row(row: &OperatorTableRow, detail: String) -> Div {
    let s = Theme::scaled;
    let name = match (&row.object, &row.index) {
        (Some(object), _) => format!("{} · {object}", row.operator),
        (None, Some(index)) => format!("{} · {index}", row.operator),
        (None, None) => row.operator.clone(),
    };
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(s(6.0)))
        .pl(px(s(row.depth.min(24) as f32 * 12.0)))
        .child(glyphs::category_glyph(row.category).element(s(10.0), Theme::text_dim()))
        .child(
            div()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_size(px(Theme::font_size_ratio(0.9)))
                .text_color(Theme::text())
                .child(name),
        )
        .when(row.flags.bottleneck, |line| {
            line.child(
                div()
                    .flex_shrink_0()
                    .px(px(s(4.0)))
                    .rounded(px(s(3.0)))
                    .border_1()
                    .border_color(Theme::error())
                    .text_size(px(Theme::font_size_ratio(0.78)))
                    .text_color(Theme::error())
                    .child(bottleneck_tag()),
            )
        })
        .when_some(row_share(row), |line, share| {
            line.child(share_bar(share, s(36.0))).child(
                div()
                    .flex_shrink_0()
                    .text_size(px(Theme::font_size_ratio(0.82)))
                    .text_color(Theme::text())
                    .child(human_share(share)),
            )
        })
        .child(
            div()
                .flex_shrink_0()
                .text_size(px(Theme::font_size_ratio(0.82)))
                .text_color(Theme::text_dim())
                .child(detail),
        )
}

/// What changed between two plans: the verdict, then up to `limit` changes,
/// each marked better, worse or neutral in a word.
pub(crate) fn changes(report: &PlanChangeReport, limit: usize) -> Div {
    let s = Theme::scaled;
    div()
        .flex()
        .flex_col()
        .gap(px(s(3.0)))
        .child(
            div()
                .flex()
                .flex_row()
                .items_start()
                .gap(px(s(8.0)))
                // One statement: its verdict is the headline, so say in a
                // word which way it went.
                .children(match report.statements.as_slice() {
                    [only] => Some(effect_tag(only.headline_effect)),
                    _ => None,
                })
                .child(
                    paragraph(
                        crate::localization::message(&report.headline),
                        Theme::text(),
                    )
                    .flex_1()
                    .min_w_0(),
                ),
        )
        .children(report.changes().take(limit).map(|change| {
            div()
                .flex()
                .flex_row()
                .items_start()
                .gap(px(s(8.0)))
                .child(effect_tag(change.effect))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .whitespace_normal()
                        .text_size(px(Theme::font_size_ratio(0.9)))
                        .text_color(Theme::text())
                        .child(crate::localization::message(&change.message)),
                )
        }))
}
