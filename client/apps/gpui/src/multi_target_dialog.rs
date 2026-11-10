//! "Run on…" dialog — pick multiple saved connections and fan the current
//! statement out to each of them.

use std::collections::BTreeSet;

use gpui::{
    Context, EventEmitter, FocusHandle, Focusable, IntoElement, ParentElement, Render, Styled,
    Window, div, prelude::*, px,
};
use gpui_component::Disableable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::checkbox::Checkbox;
use gpui_component::scroll::ScrollableElement;
use sqlly_core::{ConnectionEnvironment, ConnectionProfile};

use crate::connect_picker::{self, EnvironmentGroup};
use crate::dialog::{Dialog, DialogFrame};
use crate::theme::Theme;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MultiTargetDialogEvent {
    Run { target_ids: Vec<String> },
    Cancel,
}

impl EventEmitter<MultiTargetDialogEvent> for MultiTargetDialog {}

pub struct MultiTargetDialog {
    focus_handle: FocusHandle,
    groups: Vec<EnvironmentGroup>,
    selected: BTreeSet<String>,
    focus_requested: bool,
    frame: DialogFrame,
}

impl Focusable for MultiTargetDialog {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl MultiTargetDialog {
    pub fn new(
        profiles: &[ConnectionProfile],
        preselected: &[String],
        cx: &mut Context<Self>,
    ) -> Self {
        let groups = connect_picker::grouped_by_environment(profiles);
        let selected: BTreeSet<String> = preselected.iter().cloned().collect();
        Self {
            focus_handle: cx.focus_handle(),
            groups,
            selected,
            focus_requested: false,
            frame: DialogFrame::new_auto(Theme::scaled(520.0)),
        }
    }

    fn selected_count(&self) -> usize {
        self.selected.len()
    }

    fn toggle(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.selected.contains(id) {
            self.selected.remove(id);
        } else {
            self.selected.insert(id.to_string());
        }
        cx.notify();
    }

    fn set_group(
        &mut self,
        rows: &[connect_picker::TargetRow],
        select: bool,
        cx: &mut Context<Self>,
    ) {
        for row in rows {
            if select {
                self.selected.insert(row.target_id.clone());
            } else {
                self.selected.remove(&row.target_id);
            }
        }
        cx.notify();
    }

    fn run(&mut self, cx: &mut Context<Self>) {
        if self.selected.is_empty() {
            return;
        }
        let mut ids: Vec<String> = self.selected.iter().cloned().collect();
        ids.sort();
        cx.emit(MultiTargetDialogEvent::Run { target_ids: ids });
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        cx.emit(MultiTargetDialogEvent::Cancel);
    }

    fn on_key_down(
        &mut self,
        event: &gpui::KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let m = &event.keystroke.modifiers;
        if m.control || m.alt || m.platform || m.function {
            return;
        }
        // Escape is handled by the dialog chrome (`on_close`); Enter keeps
        // its run-selection meaning here, which the chrome only offers in
        // confirm mode.
        if event.keystroke.key.as_str() == "enter" && self.selected_count() > 0 {
            self.run(cx);
        }
    }
}

impl Render for MultiTargetDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.focus_requested {
            self.focus_requested = true;
            window.focus(&self.focus_handle, cx);
        }

        let s = Theme::scaled;
        let n = self.selected_count();
        let run_key = if n == 1 {
            "query.multi_target.run_one"
        } else {
            "query.multi_target.run_many"
        };
        let run_label = if n == 1 {
            crate::localization::text(run_key)
        } else {
            crate::localization::format(run_key, &[("count", &n.to_string())])
        };

        let subtitle = div()
            .text_color(Theme::text_dim())
            .text_size(px(Theme::font_size_ratio(0.80)))
            .child(crate::localization::text("query.multi_target.subtitle"));

        let mut body_children: Vec<gpui::AnyElement> = Vec::new();
        for (group_ix, group) in self.groups.iter().enumerate() {
            if group_ix > 0 {
                body_children.push(
                    div()
                        .h(px(1.0))
                        .bg(Theme::border())
                        .my(px(s(4.0)))
                        .into_any_element(),
                );
            }
            let all_selected = !group.rows.is_empty()
                && group
                    .rows
                    .iter()
                    .all(|r| self.selected.contains(&r.target_id));
            let group_rows = group.rows.clone();
            body_children.push(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.80)))
                            .text_color(Theme::text_dim())
                            .child(group.label.to_string()),
                    )
                    .child(
                        Button::new(("mt-group", group_ix))
                            .label(if all_selected {
                                crate::localization::text("query.multi_target.clear_group_button")
                            } else {
                                crate::localization::text("query.multi_target.select_group_button")
                            })
                            .ghost()
                            .on_click(cx.listener(move |this, _, _window, cx| {
                                this.set_group(&group_rows, !all_selected, cx);
                            })),
                    )
                    .into_any_element(),
            );
            for (row_ix, row) in group.rows.iter().enumerate() {
                let id = row.target_id.clone();
                let checked = self.selected.contains(&id);
                let mut detail = row.detail.clone();
                if row.environment == ConnectionEnvironment::Production {
                    detail = crate::localization::format(
                        "query.multi_target.production_detail",
                        &[("detail", &detail)],
                    );
                }
                let display = row.display_name.clone();
                body_children.push(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .h(px(Theme::row_height()))
                        .gap(px(s(8.0)))
                        .child(
                            Checkbox::new(("mt", group_ix * 1000 + row_ix))
                                .checked(checked)
                                .label(display)
                                .on_click(cx.listener(move |this, _, _window, cx| {
                                    this.toggle(&id, cx);
                                })),
                        )
                        .child(
                            div()
                                .flex_1()
                                .text_color(Theme::text_dim())
                                .text_size(px(Theme::font_size_ratio(0.87)))
                                .child(detail),
                        )
                        .into_any_element(),
                );
            }
        }

        let body = div()
            .flex()
            .flex_col()
            .gap(px(s(4.0)))
            .max_h(px(s(360.0)))
            .overflow_y_scrollbar()
            .children(body_children);

        let focus_handle = self.focus_handle.clone();
        Dialog::new("multi-target-dialog", |this: &mut Self| &mut this.frame)
            .title(crate::localization::text("query.multi_target.dialog_title"))
            .key_context("MultiTargetDialog")
            .track_focus(&focus_handle)
            .dismiss_on_scrim_click(false)
            .on_close(|this, _window, cx| this.cancel(cx))
            .child(subtitle)
            .child(body)
            .button(
                Button::new("mt-run")
                    .label(run_label)
                    .primary()
                    .disabled(n == 0)
                    .on_click(cx.listener(|this, _, _window, cx| this.run(cx))),
            )
            .build(self, window, cx)
            // Key events bubble from the focused card to this overlay, so
            // Enter-to-run still fires alongside the chrome's Escape.
            .on_key_down(cx.listener(Self::on_key_down))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status_bar::environment_label;

    #[test]
    fn production_detail_suffix_is_plain_text() {
        let label = environment_label(ConnectionEnvironment::Production);
        assert_eq!(label, "Production");
    }
}
