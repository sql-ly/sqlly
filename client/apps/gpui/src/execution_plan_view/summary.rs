//! The Summary view: the plan told as numbered steps in execution order,
//! beside the ranked findings. A step and the findings about its operator
//! highlight each other.

use gpui::{
    AnyElement, ClipboardItem, Context, Div, Hsla, IntoElement, MouseButton, SharedString, Styled,
    div, prelude::*, px,
};
use gpui_component::Sizable as _;
use gpui_component::button::Button;
use sqlly_services::plans::{
    FindingConfidence, FindingSeverity, LEARN_SITE_ORIGIN, LearnLink, LearnLinkType, LearnSource,
    PlanFinding, StoryStep, engine_links_for_finding, human_share,
};

use super::{
    ExecutionPlanView, ExecutionPlanViewEvent, StatementPlans, glyphs, heat_color, is_wide,
    truncate,
};
use crate::theme::Theme;

/// The severity mark: a shape per severity, in the severity's colour.
pub(super) fn severity_mark(severity: FindingSeverity, size: f32) -> AnyElement {
    glyphs::severity_glyph(severity)
        .element(size, ExecutionPlanView::severity_color(severity))
        .into_any_element()
}

pub(super) fn severity_label(severity: FindingSeverity) -> String {
    let key = match severity {
        FindingSeverity::Critical => "query.execution_plan.severity_critical",
        FindingSeverity::Warning => "query.execution_plan.severity_warning",
        FindingSeverity::Info => "query.execution_plan.severity_info",
    };
    crate::localization::text(key)
}

fn confidence_label(confidence: FindingConfidence) -> String {
    let key = match confidence {
        FindingConfidence::High => "query.execution_plan.confidence_high",
        FindingConfidence::Medium => "query.execution_plan.confidence_medium",
        FindingConfidence::Low => "query.execution_plan.confidence_low",
    };
    crate::localization::text(key)
}

/// A dim, small section heading.
pub(super) fn heading(text: impl Into<SharedString>) -> Div {
    div()
        .flex_shrink_0()
        .text_size(px(Theme::font_size_ratio(0.85)))
        .text_color(Theme::text_dim())
        .child(text.into())
}

/// Wrapping body text.
pub(super) fn paragraph(text: impl Into<SharedString>, color: Hsla) -> Div {
    div()
        .w_full()
        .whitespace_normal()
        .text_size(px(Theme::font_size()))
        .text_color(color)
        .child(text.into())
}

/// A horizontal share bar: a filled fraction over a neutral track. Always
/// shown next to the share as a number.
pub(super) fn share_bar(share: f64, width: f32) -> Div {
    let s = Theme::scaled;
    let share = if share.is_finite() {
        share.clamp(0.0, 1.0) as f32
    } else {
        0.0
    };
    div()
        .flex_shrink_0()
        .w(px(width))
        .h(px(s(4.0)))
        .rounded(px(s(2.0)))
        .bg(Theme::border())
        .child(
            div()
                .h_full()
                .w(px(width * share))
                .rounded(px(s(2.0)))
                .bg(heat_color(share)),
        )
}

/// Open a Learn link in the browser. A failure to launch a browser is not
/// worth interrupting the reader for; the link text stays on screen.
fn open_learn_link(link: &LearnLink, campaign: &str) {
    let url = link.url(LearnSource::DesktopApp, campaign);
    if let Err(error) = crate::browser_open::open_in_browser(&url) {
        tracing::warn!(%url, ?error, "could not open the article in a browser");
    }
}

/// The two lines of a Learn link. The link table is written in English, the
/// language of the articles' titles, so it is shown as written only in an
/// English interface. Other languages get a translated line naming the kind
/// of page and the page's address, rather than an English title in the
/// middle of translated text.
pub(super) fn learn_link_text(link: &LearnLink) -> (SharedString, SharedString) {
    if crate::localization::current_language() == sqlly_i18n::Language::English {
        return (
            SharedString::from(link.title.clone()),
            SharedString::from(link.learn.clone()),
        );
    }
    let key = match link.link_type {
        LearnLinkType::Article => "query.execution_plan.learn_link_article",
        LearnLinkType::Function => "query.execution_plan.learn_link_function",
        LearnLinkType::Setting => "query.execution_plan.learn_link_setting",
        LearnLinkType::Snippet => "query.execution_plan.learn_link_snippet",
        LearnLinkType::Tool => "query.execution_plan.learn_link_tool",
    };
    let host = LEARN_SITE_ORIGIN
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    (
        SharedString::from(crate::localization::text(key)),
        SharedString::from(format!("{host}{}", link.path)),
    )
}

/// A row of article links, or nothing when there are none.
pub(super) fn learn_links(
    id: &'static str,
    links: Vec<&'static LearnLink>,
    campaign: &'static str,
) -> Option<Div> {
    if links.is_empty() {
        return None;
    }
    let s = Theme::scaled;
    Some(
        div()
            .flex()
            .flex_col()
            .gap(px(s(2.0)))
            .child(heading(crate::localization::text(
                "query.execution_plan.learn_heading",
            )))
            .children(links.into_iter().enumerate().map(move |(index, link)| {
                let (title, learn) = learn_link_text(link);
                div()
                    .id((id, index))
                    .flex()
                    .flex_col()
                    .cursor_pointer()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(move |_, _, _| open_learn_link(link, campaign))
                    .child(
                        div()
                            .w_full()
                            .whitespace_normal()
                            .text_size(px(Theme::font_size()))
                            .text_color(Theme::accent())
                            .hover(|style| style.underline())
                            .child(title),
                    )
                    .child(
                        div()
                            .w_full()
                            .whitespace_normal()
                            .text_size(px(Theme::font_size_ratio(0.85)))
                            .text_color(Theme::text_dim())
                            .child(learn),
                    )
            })),
    )
}

impl ExecutionPlanView {
    /// The "Show in diagram" button, shared by steps and findings.
    pub(super) fn show_in_diagram_button(
        &self,
        id: impl Into<gpui::ElementId>,
        node_id: i32,
        cx: &mut Context<Self>,
    ) -> Button {
        Button::new(id)
            .small()
            .label(crate::localization::text(
                "query.execution_plan.show_in_diagram_button",
            ))
            .on_click(cx.listener(move |this, _, _, cx| this.show_in_diagram(node_id, cx)))
    }

    /// A block of generated SQL with Copy and Open-in-tab buttons.
    fn sql_block(&self, id: (&'static str, usize), sql: &str, cx: &mut Context<Self>) -> Div {
        let s = Theme::scaled;
        let (name, index) = id;
        div()
            .flex()
            .flex_col()
            .gap(px(s(4.0)))
            .child(
                div()
                    .w_full()
                    .p(px(s(6.0)))
                    .rounded(px(s(3.0)))
                    .bg(Theme::bg())
                    .border_1()
                    .border_color(Theme::border())
                    .whitespace_normal()
                    .font_family(Theme::editor_font_family())
                    .text_size(px(Theme::font_size_ratio(0.92)))
                    .text_color(Theme::text())
                    .child(SharedString::from(sql.to_string())),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(s(6.0)))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        Button::new((SharedString::from(format!("{name}-copy")), index))
                            .small()
                            .label(crate::localization::text(
                                "query.execution_plan.copy_finding_sql_button",
                            ))
                            .on_click({
                                let sql = sql.to_string();
                                move |_, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(sql.clone()));
                                }
                            }),
                    )
                    .child(
                        Button::new((SharedString::from(format!("{name}-open")), index))
                            .small()
                            .label(crate::localization::text(
                                "query.execution_plan.open_finding_tab_button",
                            ))
                            .on_click({
                                let sql = sql.to_string();
                                cx.listener(move |_, _, _, cx| {
                                    cx.emit(ExecutionPlanViewEvent::OpenSqlInNewTab(sql.clone()));
                                })
                            }),
                    ),
            )
    }

    /// Steps the story shows right now, with their index in `story.steps`.
    fn visible_steps<'a>(&self, active: &'a StatementPlans) -> Vec<&'a StoryStep> {
        active
            .story
            .steps
            .iter()
            .filter(|step| self.show_minor_steps || !step.minor)
            .collect()
    }

    /// Make the step for `node_id` visible: unfold minor steps when it is one
    /// of them and scroll the story to it.
    pub(super) fn reveal_step(&mut self, node_id: i32) {
        let Some(active) = self.statements.get(self.active_statement) else {
            return;
        };
        let Some(step) = active.story.step_for_node(node_id) else {
            return;
        };
        if step.minor {
            self.show_minor_steps = true;
        }
        let show_minor = self.show_minor_steps;
        let Some(position) = active
            .story
            .steps
            .iter()
            .filter(|candidate| show_minor || !candidate.minor)
            .position(|candidate| candidate.node_id == node_id)
        else {
            return;
        };
        // Children ahead of the first step in the scrolling column: the story
        // heading, plus the findings block when the layout is stacked.
        let wide = self
            .body_bounds
            .get()
            .is_none_or(|bounds| is_wide(bounds.size.width));
        let lead = if wide { 1 } else { 2 };
        self.story_scroll.scroll_to_item(position + lead);
    }

    pub(super) fn summary_key(&mut self, key: &str, cx: &mut Context<Self>) -> bool {
        let Some(active) = self.active() else {
            return false;
        };
        let steps: Vec<i32> = self
            .visible_steps(active)
            .iter()
            .map(|step| step.node_id)
            .collect();
        if steps.is_empty() {
            return false;
        }
        let current = self
            .selected_node
            .and_then(|node| steps.iter().position(|id| *id == node));
        match key {
            "down" => {
                let next = current.map_or(0, |index| (index + 1).min(steps.len() - 1));
                self.select_node(Some(steps[next]), cx);
                true
            }
            "up" => {
                let next = current.map_or(0, |index| index.saturating_sub(1));
                self.select_node(Some(steps[next]), cx);
                true
            }
            "enter" => match self.selected_node {
                Some(node) => {
                    self.show_in_diagram(node, cx);
                    true
                }
                None => false,
            },
            "escape" if self.selected_node.is_some() => {
                self.select_node(None, cx);
                true
            }
            _ => false,
        }
    }

    pub(super) fn render_summary(&self, cx: &mut Context<Self>) -> AnyElement {
        let s = Theme::scaled;
        let Some(active) = self.active() else {
            return div().into_any_element();
        };
        let wide = self
            .body_bounds
            .get()
            .is_none_or(|bounds| is_wide(bounds.size.width));
        let finding_nodes = self.finding_nodes();

        let steps = self.visible_steps(active);
        let minor_count = active.story.steps.iter().filter(|step| step.minor).count();
        let step_rows: Vec<AnyElement> = steps
            .iter()
            .map(|step| self.render_step(active, step, finding_nodes.contains(&step.node_id), cx))
            .collect();
        let minor_toggle = (minor_count > 0).then(|| {
            div().flex_shrink_0().px(px(s(12.0))).py(px(s(6.0))).child(
                Button::new("plan-minor-steps")
                    .small()
                    .label(if self.show_minor_steps {
                        crate::localization::text("query.execution_plan.hide_minor_steps")
                    } else {
                        crate::localization::format(
                            "query.execution_plan.show_minor_steps",
                            &[("count", &minor_count.to_string())],
                        )
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.show_minor_steps = !this.show_minor_steps;
                        cx.notify();
                    })),
            )
        });
        let story_heading = div()
            .flex_shrink_0()
            .px(px(s(12.0)))
            .pt(px(s(8.0)))
            .child(heading(crate::localization::text(
                "query.execution_plan.story_heading",
            )));
        let findings = self.render_findings(active, cx);

        let columns = if wide {
            div()
                .flex()
                .flex_row()
                .flex_1()
                .min_h_0()
                .w_full()
                .child(
                    div()
                        .id("plan-story")
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .overflow_y_scroll()
                        .track_scroll(&self.story_scroll)
                        .child(story_heading)
                        .children(step_rows)
                        .children(minor_toggle),
                )
                .child(
                    div()
                        .id("plan-findings")
                        .flex()
                        .flex_col()
                        .w(gpui::relative(0.44))
                        .flex_shrink_0()
                        .h_full()
                        .border_l_1()
                        .border_color(Theme::border())
                        .overflow_y_scroll()
                        .track_scroll(&self.findings_scroll)
                        .children(findings),
                )
        } else {
            div().flex().flex_col().flex_1().min_h_0().w_full().child(
                div()
                    .id("plan-story")
                    .flex()
                    .flex_col()
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.story_scroll)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_shrink_0()
                            .border_b_1()
                            .border_color(Theme::border())
                            .children(findings),
                    )
                    .child(story_heading)
                    .children(step_rows)
                    .children(minor_toggle),
            )
        };

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .w_full()
            .child(self.render_overview(active, cx))
            .child(columns)
            .into_any_element()
    }

    /// The block above the columns: what the statement does, where the time
    /// goes, and what the reading is based on.
    fn render_overview(&self, active: &StatementPlans, cx: &mut Context<Self>) -> Div {
        let s = Theme::scaled;
        let say = |message| crate::localization::message(message);
        let mut block = div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .w_full()
            .px(px(s(12.0)))
            .py(px(s(10.0)))
            .gap(px(s(4.0)))
            .border_b_1()
            .border_color(Theme::border());
        if self.statements.len() > 1
            && let Some(headline) = &self.batch_headline
        {
            block = block.child(paragraph(say(headline), Theme::text_dim()));
        }
        block = block.child(
            div()
                .w_full()
                .whitespace_normal()
                .text_size(px(Theme::font_size_ratio(1.2)))
                .text_color(Theme::text())
                .child(say(&active.story.headline)),
        );
        for line in &active.story.summary {
            block = block.child(paragraph(say(line), Theme::text()));
        }
        if let Some(bottleneck) = &active.story.bottleneck {
            let node_id = bottleneck.node_id;
            let mut text = say(&bottleneck.message);
            if let Some(detail) = &bottleneck.detail {
                text.push(' ');
                text.push_str(&say(detail));
            }
            // The tag only where the sentence calls the step the bottleneck;
            // "No single step dominates…" names the slowest step without it.
            let tagged = bottleneck.dominant;
            block = block.child(
                div()
                    .id("plan-bottleneck")
                    .flex()
                    .flex_row()
                    .items_start()
                    .gap(px(s(6.0)))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.select_node(Some(node_id), cx);
                    }))
                    .when(tagged, |row| {
                        row.child(
                            div()
                                .flex_shrink_0()
                                .px(px(s(5.0)))
                                .rounded(px(s(3.0)))
                                .border_1()
                                .border_color(Theme::error())
                                .text_size(px(Theme::font_size_ratio(0.85)))
                                .text_color(Theme::error())
                                .child(bottleneck_tag()),
                        )
                    })
                    .child(paragraph(text, Theme::text()).flex_1().min_w_0()),
            );
        }
        if self.statements.len() > 1 && !active.statement_text.trim().is_empty() {
            block = block.child(
                div()
                    .w_full()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .font_family(Theme::editor_font_family())
                    .text_size(px(Theme::font_size_ratio(0.92)))
                    .text_color(Theme::text_dim())
                    .child(truncate(&active.statement_text, 160)),
            );
        }
        // What the reading rests on. Said once, plainly, so nobody takes an
        // optimizer guess for a measurement.
        let mut basis: Vec<String> = Vec::new();
        if !active.analysis.has_actuals {
            basis.push(crate::localization::text(
                "query.execution_plan.estimated_notice",
            ));
        } else if !active.analysis.has_timing {
            basis.push(crate::localization::text(
                "query.execution_plan.no_timing_notice",
            ));
        }
        if active.used_schema {
            basis.push(crate::localization::text(
                "query.execution_plan.schema_used",
            ));
        }
        if active.resolved_aliases > 0 {
            basis.push(crate::localization::format(
                "query.execution_plan.aliases_resolved",
                &[("count", &active.resolved_aliases.to_string())],
            ));
        }
        for line in basis {
            block = block.child(
                div()
                    .w_full()
                    .whitespace_normal()
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .text_color(Theme::text_dim())
                    .child(line),
            );
        }
        if let Some(notice) = self.capture_history_notice() {
            block = block.child(
                div()
                    .w_full()
                    .whitespace_normal()
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .text_color(Theme::warning())
                    .child(notice),
            );
        }
        block
    }

    fn render_step(
        &self,
        active: &StatementPlans,
        step: &StoryStep,
        in_finding: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let s = Theme::scaled;
        let say = |message| crate::localization::message(message);
        let node_id = step.node_id;
        let selected = self.selected_node == Some(node_id);
        let severity = active.node_severity(node_id);
        let text_color = if step.never_executed {
            Theme::text_dim()
        } else {
            Theme::text()
        };
        // The left edge says why a step stands out: selected, or part of the
        // selected finding. Both also change the background or show a mark.
        let edge = if selected {
            Theme::accent()
        } else if in_finding {
            Theme::warning()
        } else {
            gpui::transparent_black()
        };
        let mut title_row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(s(6.0)))
            .child(
                div()
                    .flex_shrink_0()
                    .min_w(px(s(22.0)))
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text_dim())
                    .child(format!("{}.", step.number)),
            )
            .child(
                glyphs::category_glyph(
                    active
                        .analysis
                        .node(node_id)
                        .map_or(sqlly_services::plans::OperatorCategory::Other, |node| {
                            node.category()
                        }),
                )
                .element(s(12.0), Theme::text_dim()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .whitespace_normal()
                    .text_size(px(Theme::font_size()))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(text_color)
                    .child(say(&step.title)),
            );
        if let Some(severity) = severity {
            title_row = title_row.child(severity_mark(severity, s(11.0)));
        }
        if step.is_bottleneck {
            title_row = title_row.child(
                div()
                    .flex_shrink_0()
                    .px(px(s(5.0)))
                    .rounded(px(s(3.0)))
                    .border_1()
                    .border_color(Theme::error())
                    .text_size(px(Theme::font_size_ratio(0.8)))
                    .text_color(Theme::error())
                    .child(bottleneck_tag()),
            );
        }
        if step.never_executed {
            title_row = title_row.child(
                div()
                    .flex_shrink_0()
                    .text_size(px(Theme::font_size_ratio(0.8)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text(
                        "query.execution_plan.never_executed_tag",
                    )),
            );
        }
        if let Some(share) = step.share {
            title_row = title_row.child(share_bar(share, s(48.0))).child(
                div()
                    .flex_shrink_0()
                    .min_w(px(s(38.0)))
                    .text_size(px(Theme::font_size_ratio(0.92)))
                    .text_color(Theme::text_dim())
                    .child(human_share(share)),
            );
        }

        let mut body = div()
            .flex()
            .flex_col()
            .gap(px(s(2.0)))
            .pl(px(s(28.0)))
            .child(paragraph(say(&step.sentence), text_color));
        for note in &step.notes {
            body = body.child(
                div()
                    .w_full()
                    .whitespace_normal()
                    .text_size(px(Theme::font_size_ratio(0.92)))
                    .text_color(Theme::text_dim())
                    .child(say(&note.message)),
            );
        }
        if !step.inputs.is_empty() {
            let inputs = step
                .inputs
                .iter()
                .map(|number| number.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            body = body.child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::format(
                        "query.execution_plan.step_inputs",
                        &[("steps", &inputs)],
                    )),
            );
        }
        if selected {
            body = body.child(
                div()
                    .flex()
                    .flex_row()
                    .pt(px(s(4.0)))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(self.show_in_diagram_button(
                        ("plan-step-diagram", step.number),
                        node_id,
                        cx,
                    )),
            );
        }

        div()
            .id(("plan-step", step.number))
            .flex()
            .flex_col()
            .flex_shrink_0()
            .w_full()
            .px(px(s(10.0)))
            .py(px(s(6.0)))
            .gap(px(s(2.0)))
            .border_l_2()
            .border_color(edge)
            .cursor_pointer()
            .when(selected, |row| row.bg(Theme::selection_fill()))
            .when(!selected, |row| {
                row.hover(|style| style.bg(Theme::row_hover()))
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    this.focus_handle.focus(window, cx);
                    this.select_node(Some(node_id), cx);
                }),
            )
            .child(title_row)
            .child(body)
            .into_any_element()
    }

    fn render_findings(&self, active: &StatementPlans, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let s = Theme::scaled;
        let mut out: Vec<AnyElement> = Vec::with_capacity(active.findings.len() + 1);
        out.push(
            div()
                .flex_shrink_0()
                .px(px(s(12.0)))
                .pt(px(s(8.0)))
                .pb(px(s(2.0)))
                .child(heading(crate::localization::format(
                    "query.execution_plan.findings_count",
                    &[("count", &active.findings.len().to_string())],
                )))
                .into_any_element(),
        );
        if active.findings.is_empty() {
            out.push(
                div()
                    .flex_shrink_0()
                    .px(px(s(12.0)))
                    .py(px(s(8.0)))
                    .child(paragraph(
                        crate::localization::text("query.execution_plan.no_findings"),
                        Theme::success(),
                    ))
                    .into_any_element(),
            );
            return out;
        }
        for (index, finding) in active.findings.iter().enumerate() {
            out.push(self.render_finding(active, index, finding, cx));
        }
        out
    }

    fn render_finding(
        &self,
        active: &StatementPlans,
        index: usize,
        finding: &PlanFinding,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let s = Theme::scaled;
        let say = |message| crate::localization::message(message);
        let expanded = self.expanded_findings.contains(&index);
        let selected = self.selected_finding == Some(index);
        // A finding about the selected operator is marked even when the
        // operator was picked somewhere else.
        let about_selected = self
            .selected_node
            .is_some_and(|node| super::model::finding_touches(finding, node));
        let color = Self::severity_color(finding.severity);

        let mut head = div()
            .flex()
            .flex_row()
            .items_start()
            .gap(px(s(6.0)))
            .child(
                div()
                    .flex_shrink_0()
                    .pt(px(s(3.0)))
                    .child(severity_mark(finding.severity, s(11.0))),
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
                            .w_full()
                            .whitespace_normal()
                            .text_size(px(Theme::font_size()))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(Theme::text())
                            .child(say(&finding.summary)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .gap(px(s(8.0)))
                            .text_size(px(Theme::font_size_ratio(0.85)))
                            .child(
                                div()
                                    .text_color(color)
                                    .child(severity_label(finding.severity)),
                            )
                            .child(
                                div()
                                    .text_color(Theme::text_dim())
                                    .child(confidence_label(finding.confidence)),
                            ),
                    )
                    // The impact is a whole sentence, longer than the column
                    // in some languages, so it gets a line of its own that
                    // wraps.
                    .when_some(finding.impact_message(), |column, impact| {
                        column.child(
                            paragraph(crate::localization::message(&impact), Theme::text())
                                .text_size(px(Theme::font_size_ratio(0.85))),
                        )
                    }),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .pt(px(s(2.0)))
                    .text_color(Theme::text_dim())
                    .child(crate::icons::disclosure_chevron(expanded)),
            );
        if let Some(impact) = finding.impact.as_ref() {
            head = head.child(
                div()
                    .flex_shrink_0()
                    .pt(px(s(7.0)))
                    .child(share_bar(impact.share, s(36.0))),
            );
        }

        let mut row = div()
            .id(("plan-finding", index))
            .flex()
            .flex_col()
            .flex_shrink_0()
            .w_full()
            .px(px(s(10.0)))
            .py(px(s(7.0)))
            .gap(px(s(4.0)))
            .border_l_2()
            .border_color(if selected {
                Theme::accent()
            } else if about_selected {
                Theme::warning()
            } else {
                gpui::transparent_black()
            })
            .cursor_pointer()
            .when(selected, |row| row.bg(Theme::selection_fill()))
            .when(!selected, |row| {
                row.hover(|style| style.bg(Theme::row_hover()))
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    this.focus_handle.focus(window, cx);
                    this.toggle_finding(index, cx);
                }),
            )
            .child(head)
            // The fix is the point of a finding, so it shows without
            // expanding anything.
            .child(
                div()
                    .pl(px(s(17.0)))
                    .child(paragraph(say(&finding.action), Theme::text())),
            );

        if expanded {
            let mut detail = div()
                .flex()
                .flex_col()
                .gap(px(s(6.0)))
                .pl(px(s(17.0)))
                .child(paragraph(say(&finding.why), Theme::text_dim()));
            // `detail` is the operator line plus the evidence joined into one
            // text, for surfaces without an evidence list. The list below
            // carries the same facts, so it is only shown when there is none.
            let extra = say(&finding.detail);
            if finding.evidence.is_empty() && !extra.trim().is_empty() {
                detail = detail.child(paragraph(extra, Theme::text_dim()));
            }
            if let Some(sql) = finding.action_sql.as_deref() {
                detail = detail.child(self.sql_block(("plan-finding-sql", index), sql, cx));
            }
            if !finding.evidence.is_empty() {
                detail = detail.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(s(1.0)))
                        .child(heading(crate::localization::text(
                            "query.execution_plan.evidence_heading",
                        )))
                        .children(finding.evidence.iter().map(|line| {
                            paragraph(say(line), Theme::text())
                                .text_size(px(Theme::font_size_ratio(0.92)))
                        })),
                );
            }
            if !finding.alternatives.is_empty() {
                let mut alternatives = div().flex().flex_col().gap(px(s(4.0))).child(heading(
                    crate::localization::text("query.execution_plan.alternatives_heading"),
                ));
                for (alt_index, alternative) in finding.alternatives.iter().enumerate() {
                    alternatives =
                        alternatives.child(paragraph(say(&alternative.label), Theme::text()));
                    if let Some(sql) = alternative.sql.as_deref() {
                        alternatives = alternatives.child(self.sql_block(
                            ("plan-finding-alt", index * 100 + alt_index),
                            sql,
                            cx,
                        ));
                    }
                }
                detail = detail.child(alternatives);
            }
            for note in &finding.notes {
                detail = detail.child(
                    paragraph(say(note), Theme::text_dim())
                        .text_size(px(Theme::font_size_ratio(0.85))),
                );
            }
            if let Some(links) = learn_links(
                "plan-finding-learn",
                engine_links_for_finding(finding.kind.as_str(), active.analysis.engine),
                finding.kind.as_str(),
            ) {
                detail = detail.child(links);
            }
            if let Some(node_id) = finding
                .node_id
                .or_else(|| finding.node_ids.first().copied())
            {
                detail = detail.child(
                    div()
                        .flex()
                        .flex_row()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(self.show_in_diagram_button(
                            ("plan-finding-diagram", index),
                            node_id,
                            cx,
                        )),
                );
            }
            row = row.child(detail);
        }
        row.into_any_element()
    }
}

/// The word that marks the operator the statement spends the most on.
pub(super) fn bottleneck_tag() -> String {
    crate::localization::text("query.execution_plan.bottleneck_tag")
}
