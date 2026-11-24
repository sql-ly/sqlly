//! The Time view (Cost on plans without timings): a flame chart of where the
//! statement spent itself, and the same total split by operator, by operator
//! family and by table.

use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    AnyElement, App, Bounds, ContentMask, Context, Corners, Div, Edges, IntoElement, MouseButton,
    MouseDownEvent, PaintQuad, Pixels, SharedString, Styled, TextAlign, TextRun, Window, canvas,
    div, point, prelude::*, px, size,
};
use sqlly_services::plans::{
    BreakdownEntry, FlameLayout, PlanMetric, human_cost, human_duration_ms, human_share,
};

use super::summary::{heading, share_bar};
use super::{ExecutionPlanView, glyphs, heat_color, is_wide, mix};
use crate::theme::Theme;

const ROW_HEIGHT: f32 = 22.0;

#[derive(Default)]
pub(super) struct FlameState {
    bounds: Cell<Option<Bounds<Pixels>>>,
    labels: Option<Rc<FlameLabels>>,
}

/// The chart's bar labels in the interface language, resolved once.
struct FlameLabels {
    labels: Vec<SharedString>,
    chrome_revision: u64,
}

/// A time or a cost, whichever the chart measures.
pub(super) fn metric_value(metric: PlanMetric, value: f64) -> String {
    match metric {
        PlanMetric::Time => human_duration_ms(value),
        _ => human_cost(value),
    }
}

/// The bar under a point given as fractions of the chart's width and a row.
pub(super) fn flame_cell_at(layout: &FlameLayout, x: f32, row: usize) -> Option<i32> {
    layout
        .cells
        .iter()
        .find(|cell| cell.depth == row && x >= cell.x && x <= cell.x + cell.width)
        .map(|cell| cell.node_id)
}

fn paint_flame(
    layout: &FlameLayout,
    labels: &FlameLabels,
    selected: Option<i32>,
    bounds: Bounds<Pixels>,
    window: &mut Window,
    cx: &mut App,
) {
    let (ox, oy) = (f32::from(bounds.origin.x), f32::from(bounds.origin.y));
    let width = f32::from(bounds.size.width);
    let row_h = Theme::scaled(ROW_HEIGHT);
    let font_px = Theme::font_size_ratio(0.88);
    let card = Theme::bg();
    let font = Theme::ui_font();
    window.with_content_mask(Some(ContentMask { bounds }), |window| {
        for (cell, label) in layout.cells.iter().zip(&labels.labels) {
            let w = cell.width * width;
            if w < 1.0 {
                continue;
            }
            let x = ox + cell.x * width;
            let y = oy + cell.depth as f32 * row_h;
            let is_selected = selected == Some(cell.node_id);
            let heat = heat_color(cell.heat);
            let fill = mix(card, heat, 0.18 + 0.5 * cell.heat);
            let bar = Bounds {
                origin: point(px(x), px(y)),
                size: size(px((w - 1.0).max(1.0)), px(row_h - 2.0)),
            };
            window.paint_quad(PaintQuad {
                bounds: bar,
                background: fill.into(),
                border_color: if is_selected {
                    Theme::accent()
                } else if cell.flags.bottleneck {
                    Theme::error()
                } else {
                    Theme::border()
                },
                border_widths: Edges::all(px(if is_selected || cell.flags.bottleneck {
                    2.0
                } else {
                    1.0
                })),
                corner_radii: Corners::all(px(2.0)),
                border_style: Default::default(),
            });
            if w < 34.0 {
                continue;
            }
            window.with_content_mask(
                Some(ContentMask {
                    bounds: Bounds {
                        origin: bar.origin,
                        size: size(px((w - 7.0).max(0.0)), px(row_h)),
                    },
                }),
                |window| {
                    let mut text_x = x + 5.0;
                    if w > 60.0 {
                        let side = font_px;
                        glyphs::category_glyph(cell.category).paint(
                            Bounds {
                                origin: point(px(text_x), px(y + (row_h - 2.0 - side) / 2.0)),
                                size: size(px(side), px(side)),
                            },
                            Theme::text_dim(),
                            window,
                        );
                        text_x += side + 5.0;
                    }
                    let run = TextRun {
                        len: label.len(),
                        font: font.clone(),
                        color: Theme::text(),
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    };
                    let line = window.text_system().clone().shape_line(
                        label.clone(),
                        px(font_px),
                        &[run],
                        None,
                    );
                    let _ = line.paint(
                        point(px(text_x), px(y + (row_h - 2.0 - font_px * 1.4) / 2.0)),
                        px(font_px * 1.4),
                        TextAlign::Left,
                        None,
                        window,
                        cx,
                    );
                },
            );
        }
    });
}

impl ExecutionPlanView {
    fn flame_labels(&mut self, layout: &FlameLayout) -> Rc<FlameLabels> {
        let chrome = crate::localization::chrome_revision();
        if let Some(labels) = &self.flame.labels
            && labels.chrome_revision == chrome
            && labels.labels.len() == layout.cells.len()
        {
            return labels.clone();
        }
        let labels = Rc::new(FlameLabels {
            labels: layout
                .cells
                .iter()
                .map(|cell| {
                    // The share goes in the label so the chart reads
                    // without judging widths or colours.
                    SharedString::from(format!(
                        "{} · {}",
                        crate::localization::message(&cell.label),
                        human_share(cell.share)
                    ))
                })
                .collect(),
            chrome_revision: chrome,
        });
        self.flame.labels = Some(labels.clone());
        labels
    }

    fn on_flame_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle, cx);
        let Some(bounds) = self.flame.bounds.get() else {
            return;
        };
        let Some(layout) = self
            .statements
            .get(self.active_statement)
            .and_then(|active| active.flame.as_ref())
        else {
            return;
        };
        let width = f32::from(bounds.size.width);
        if width <= 0.0 {
            return;
        }
        let x = f32::from(event.position.x - bounds.origin.x) / width;
        let y = f32::from(event.position.y - bounds.origin.y);
        if y < 0.0 {
            return;
        }
        let row = (y / Theme::scaled(ROW_HEIGHT)) as usize;
        let hit = flame_cell_at(layout, x, row);
        self.select_node(hit, cx);
    }

    fn breakdown_list(
        &self,
        id: &'static str,
        title: String,
        metric: PlanMetric,
        entries: &[BreakdownEntry],
        cx: &mut Context<Self>,
    ) -> Option<Div> {
        if entries.is_empty() {
            return None;
        }
        let s = Theme::scaled;
        let mut list = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(s(220.0)))
            .gap(px(s(2.0)))
            .child(heading(title));
        for (index, entry) in entries.iter().enumerate() {
            let target = entry.node_ids.first().copied();
            let selected = self
                .selected_node
                .is_some_and(|node| entry.node_ids.contains(&node));
            list = list.child(
                div()
                    .id((id, index))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(6.0)))
                    .px(px(s(4.0)))
                    .py(px(s(2.0)))
                    .rounded(px(s(3.0)))
                    .when(selected, |row| row.bg(Theme::selection()))
                    .when(target.is_some(), |row| {
                        row.cursor_pointer()
                            .hover(|style| style.bg(Theme::row_hover()))
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if target.is_some() {
                            this.select_node(target, cx);
                        }
                    }))
                    .when_some(entry.category, |row, category| {
                        row.child(
                            glyphs::category_glyph(category).element(s(11.0), Theme::text_dim()),
                        )
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_size(px(Theme::font_size()))
                            .text_color(Theme::text())
                            .child(crate::localization::message(&entry.label)),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_size(px(Theme::font_size_ratio(0.9)))
                            .text_color(Theme::text_dim())
                            .child(metric_value(metric, entry.value)),
                    )
                    .child(share_bar(entry.share, 60.0))
                    .child(
                        div()
                            .flex_shrink_0()
                            .w(px(s(38.0)))
                            .text_right()
                            .text_size(px(Theme::font_size_ratio(0.9)))
                            .text_color(Theme::text())
                            .child(human_share(entry.share)),
                    ),
            );
        }
        Some(list)
    }

    fn render_flame_chart(
        &mut self,
        layout: &FlameLayout,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let labels = self.flame_labels(layout);
        let paint_layout = Rc::new(layout.clone());
        let selected = self.selected_node;
        let entity = cx.entity();
        let height = Theme::scaled(ROW_HEIGHT) * layout.depth.max(1) as f32;
        div()
            .id("plan-flame")
            .flex_shrink_0()
            .w_full()
            .h(px(height))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_flame_mouse_down))
            .child(
                canvas(
                    move |bounds, _window, cx| {
                        entity.read(cx).flame.bounds.set(Some(bounds));
                    },
                    move |bounds, _, window, cx| {
                        paint_flame(&paint_layout, &labels, selected, bounds, window, cx);
                    },
                )
                .size_full(),
            )
    }

    pub(super) fn render_breakdown(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let s = Theme::scaled;
        let wide = self
            .body_bounds
            .get()
            .is_none_or(|bounds| is_wide(bounds.size.width));
        // The flame chart needs `&mut self` for its label cache, so build it
        // before borrowing the statement; only its layout is copied.
        let Some(flame) = self
            .statements
            .get(self.active_statement)
            .map(|active| active.flame.clone())
        else {
            return div().into_any_element();
        };
        let mut flame_chart = flame
            .as_ref()
            .map(|layout| self.render_flame_chart(layout, cx));
        let Some(active) = self.statements.get(self.active_statement) else {
            return div().into_any_element();
        };
        let mut column = div()
            .id("plan-breakdown")
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_y_scroll()
            .p(px(s(12.0)))
            .gap(px(s(12.0)));

        match (&active.flame, &active.breakdown) {
            (None, None) => {
                column = column.child(
                    div()
                        .whitespace_normal()
                        .text_size(px(Theme::font_size()))
                        .text_color(Theme::text_dim())
                        .child(crate::localization::text(
                            "query.execution_plan.breakdown_unavailable",
                        )),
                );
            }
            (flame, breakdown) => {
                if let Some(layout) = flame {
                    let total = metric_value(layout.metric, layout.total);
                    let operators = active.analysis.nodes.len().to_string();
                    let key = if layout.metric == PlanMetric::Time {
                        "query.execution_plan.flame_total_time"
                    } else {
                        "query.execution_plan.flame_total_cost"
                    };
                    column = column
                        .child(
                            div()
                                .flex_shrink_0()
                                .whitespace_normal()
                                .text_size(px(Theme::font_size()))
                                .text_color(Theme::text())
                                .child(crate::localization::format(
                                    key,
                                    &[("total", &total), ("operators", &operators)],
                                )),
                        )
                        .children(flame_chart.take())
                        .child(
                            div()
                                .flex_shrink_0()
                                .whitespace_normal()
                                .text_size(px(Theme::font_size_ratio(0.88)))
                                .text_color(Theme::text_dim())
                                .child(crate::localization::text(
                                    "query.execution_plan.flame_help",
                                )),
                        );
                }
                if let Some(breakdown) = breakdown {
                    let lists = [
                        self.breakdown_list(
                            "plan-breakdown-operator",
                            crate::localization::text("query.execution_plan.breakdown_by_operator"),
                            breakdown.metric,
                            &breakdown.by_operator,
                            cx,
                        ),
                        self.breakdown_list(
                            "plan-breakdown-family",
                            crate::localization::text("query.execution_plan.breakdown_by_family"),
                            breakdown.metric,
                            &breakdown.by_category,
                            cx,
                        ),
                        self.breakdown_list(
                            "plan-breakdown-table",
                            crate::localization::text("query.execution_plan.breakdown_by_table"),
                            breakdown.metric,
                            &breakdown.by_table,
                            cx,
                        ),
                    ];
                    column = column.child(
                        div()
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .flex_shrink_0()
                            .items_start()
                            .gap(px(s(20.0)))
                            .children(lists.into_iter().flatten()),
                    );
                }
            }
        }

        div()
            .flex()
            .flex_row()
            .flex_1()
            .min_h_0()
            .w_full()
            .child(column)
            .when(wide, |row| row.child(self.render_details(active, cx)))
            .into_any_element()
    }
}
