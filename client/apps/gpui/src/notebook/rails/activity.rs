//! Bottom activity panel: the run queue, then a timeline of runs (timings,
//! messages, errors), newest first.

use gpui::{
    AnyElement, Context, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, Window, div, prelude::FluentBuilder as _, px,
};

use crate::notebook::view::{DragTarget, NotebookView};
use crate::pane_divider::{CollapseDirection, PaneDivider};
use crate::theme::Theme;

fn t(key: &str) -> String {
    crate::localization::text(key)
}

impl NotebookView {
    pub(in crate::notebook) fn render_activity(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let s = Theme::scaled;
        let this = cx.entity();
        let toggle = this.clone();
        let drag = this.clone();
        let divider = PaneDivider::horizontal("nb-activity-divider", CollapseDirection::Down)
            .label(t("notebook.shell.activity.title"))
            .collapsed(!self.layout.activity_visible)
            .on_toggle(move |collapsed, _, app| {
                toggle.update(app, |v, cx| {
                    v.update_layout(|l| l.activity_visible = !collapsed, cx)
                });
            })
            .on_drag_start(move |y, _, app| {
                drag.update(app, |v, _| v.begin_drag(DragTarget::Activity, y));
            });
        if !self.layout.activity_visible {
            return divider.into_any_element();
        }
        let queued = self.queued();
        let queue_text = if queued.is_empty() {
            t("notebook.shell.activity.queue_empty")
        } else {
            let names: Vec<String> = queued
                .iter()
                .map(|id| {
                    self.doc
                        .cell_by_id(id)
                        .map(|c| c.display_name().to_string())
                        .unwrap_or_else(|| id.clone())
                })
                .collect();
            crate::localization::format(
                "notebook.shell.activity.queue",
                &[("cells", names.join(" → "))],
            )
        };
        let local_rows = self.local_activity_rows(cx);
        let entries: Vec<AnyElement> = self
            .activity
            .iter()
            .rev()
            .map(|entry| {
                let mut row = div()
                    .flex()
                    .flex_row()
                    .gap(px(s(8.0)))
                    .px(px(s(10.0)))
                    .py(px(s(2.0)))
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .child(
                        div()
                            .flex_none()
                            .text_color(Theme::text_dim())
                            .child(entry.at.clone()),
                    );
                if let Some(cell) = &entry.cell {
                    row = row.child(
                        div()
                            .flex_none()
                            .text_color(Theme::text())
                            .child(cell.clone()),
                    );
                }
                row.child(
                    div()
                        .flex_1()
                        .text_color(if entry.error {
                            Theme::error()
                        } else {
                            Theme::text()
                        })
                        .child(entry.message.clone()),
                )
                .when_some(entry.elapsed_ms, |d, ms| {
                    d.child(div().flex_none().text_color(Theme::text_dim()).child(
                        crate::localization::format(
                            "notebook.shell.activity.elapsed",
                            &[("milliseconds", ms)],
                        ),
                    ))
                })
                .into_any_element()
            })
            .collect();
        div()
            .flex()
            .flex_col()
            .flex_none()
            .w_full()
            .child(divider)
            .child(
                div()
                    .id("nb-activity")
                    .h(px(self.layout.activity_height))
                    .w_full()
                    .flex()
                    .flex_col()
                    .bg(Theme::panel_bg())
                    .child(
                        div()
                            .px(px(s(10.0)))
                            .py(px(s(4.0)))
                            .border_b_1()
                            .border_color(Theme::border())
                            .text_size(px(Theme::font_size_ratio(0.85)))
                            .text_color(Theme::text_dim())
                            .child(queue_text),
                    )
                    .children(local_rows)
                    .child(
                        div()
                            .id("nb-activity-log")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .when(entries.is_empty(), |d| {
                                d.child(
                                    div()
                                        .px(px(s(10.0)))
                                        .py(px(s(6.0)))
                                        .text_size(px(Theme::font_size_ratio(0.85)))
                                        .text_color(Theme::text_dim())
                                        .child(t("notebook.shell.activity.empty")),
                                )
                            })
                            .children(entries),
                    ),
            )
            .into_any_element()
    }
}
