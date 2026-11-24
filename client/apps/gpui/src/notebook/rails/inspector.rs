//! Right inspector: the active cell's name, kind, connection, execution
//! policy, dependencies, state and last run.

use gpui::{
    AnyElement, Context, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::Sizable as _;
use gpui_component::checkbox::Checkbox;
use gpui_component::input::Input;
use gpui_component::menu::DropdownMenu as _;

use super::chrome::{OnPick, connection_menu};
use super::text_button;
use crate::notebook::connection::environment_badge;
use crate::notebook::view::{DragTarget, NotebookView};
use crate::pane_divider::{CollapseDirection, PaneDivider};
use crate::theme::Theme;

fn t(key: &str) -> String {
    crate::localization::text(key)
}

fn section(title: String) -> gpui::Div {
    div()
        .pt(px(Theme::scaled(10.0)))
        .pb(px(Theme::scaled(2.0)))
        .text_size(px(Theme::font_size_ratio(0.8)))
        .text_color(Theme::text_dim())
        .child(title)
}

fn field(label: String, value: impl IntoElement) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(Theme::scaled(8.0)))
        .py(px(Theme::scaled(2.0)))
        .child(
            div()
                .text_size(px(Theme::font_size_ratio(0.85)))
                .text_color(Theme::text_dim())
                .child(label),
        )
        .child(
            div()
                .text_size(px(Theme::font_size_ratio(0.85)))
                .text_color(Theme::text())
                .child(value),
        )
}

impl NotebookView {
    fn cell_links(&self, prefix: &str, ids: Vec<String>, cx: &mut Context<Self>) -> AnyElement {
        if ids.is_empty() {
            return div()
                .text_size(px(Theme::font_size_ratio(0.85)))
                .text_color(Theme::text_dim())
                .child(t("notebook.shell.rail.lineage_none"))
                .into_any_element();
        }
        let mut row = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap(px(Theme::scaled(4.0)));
        for (n, id) in ids.into_iter().enumerate() {
            let name = self
                .doc
                .cell_by_id(&id)
                .map(|c| c.display_name().to_string())
                .unwrap_or_else(|| id.clone());
            row = row.child(
                text_button(format!("{prefix}-{n}"), name)
                    .on_click(cx.listener(move |this, _, _, cx| this.reveal_cell(&id, cx))),
            );
        }
        row.into_any_element()
    }

    pub(in crate::notebook) fn render_inspector(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let s = Theme::scaled;
        let this = cx.entity();
        let toggle = this.clone();
        let drag = this.clone();
        let title = t("notebook.shell.inspector.title");
        let divider = PaneDivider::vertical("nb-right-divider", CollapseDirection::Right)
            .label(title.clone())
            .collapsed(!self.layout.right_visible)
            .on_toggle(move |collapsed, _, app| {
                toggle.update(app, |v, cx| {
                    v.update_layout(|l| l.right_visible = !collapsed, cx)
                });
            })
            .on_drag_start(move |x, _, app| {
                drag.update(app, |v, _| v.begin_drag(DragTarget::Right, x));
            });
        if !self.layout.right_visible {
            return divider.into_any_element();
        }

        let mut body = div()
            .flex()
            .flex_col()
            .px(px(s(10.0)))
            .py(px(s(8.0)))
            .child(div().text_color(Theme::text()).child(title));

        match self.active.clone().and_then(|id| {
            self.doc.cell_by_id(&id).map(|c| {
                (
                    id,
                    c.kind.to_string(),
                    c.read_only(),
                    c.is_sql(),
                    c.policy.clone(),
                )
            })
        }) {
            None => {
                body = body.child(
                    div()
                        .pt(px(s(8.0)))
                        .text_size(px(Theme::font_size_ratio(0.85)))
                        .text_color(Theme::text_dim())
                        .child(t("notebook.shell.inspector.no_cell")),
                );
            }
            Some((id, kind, read_only, is_sql, policy)) => {
                let renderer = crate::notebook::cells::renderer_for(&kind);
                body = body
                    .child(section(t("notebook.shell.inspector.name_label")))
                    .when_some(self.name_input.clone(), |d, (_, input)| {
                        d.child(Input::new(&input).small())
                    })
                    .child(field(t("notebook.shell.inspector.id_label"), id.clone()))
                    .child(field(
                        t("notebook.shell.inspector.kind_label"),
                        t(renderer.label_key()),
                    ));

                if is_sql {
                    let (choice, own) = self.cell_connection(&id);
                    let label = choice
                        .as_ref()
                        .map(|c| c.label.clone())
                        .unwrap_or_else(|| t("notebook.shell.connection.none"));
                    let choices = self.choices_source();
                    let current = own
                        .then(|| choice.as_ref().map(|c| c.profile_id.clone()))
                        .flatten();
                    let pick_this = this.clone();
                    let pick_id = id.clone();
                    let pick: OnPick = std::rc::Rc::new(move |choice, app| {
                        pick_this.update(app, |v, cx| v.set_cell_connection(&pick_id, choice, cx));
                    });
                    let toggle_id = id.clone();
                    let (reads, used_by) = self.doc.shown_links(&id);
                    body = body
                        .child(section(t("notebook.shell.inspector.connection_section")))
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(s(6.0)))
                                .child(
                                    text_button("nb-inspector-connection", label)
                                        .tooltip(t("notebook.shell.inspector.connection_tooltip"))
                                        .dropdown_menu(move |menu, _, _| {
                                            connection_menu(
                                                menu,
                                                t("notebook.shell.inspector.use_notebook_connection"),
                                                choices(),
                                                current.clone(),
                                                pick.clone(),
                                            )
                                        }),
                                )
                                .when_some(choice.as_ref().map(|c| c.environment), |d, env| {
                                    d.child(environment_badge(env))
                                }),
                        )
                        .when_some(choice.and_then(|c| c.database), |d, db| {
                            d.child(field(t("notebook.shell.inspector.database_label"), db))
                        })
                        .child(section(t("notebook.shell.inspector.policy_section")))
                        .child(
                            Checkbox::new("nb-inspector-read-only")
                                .checked(read_only)
                                .label(t("notebook.shell.inspector.read_only_label"))
                                .accessibility_label(t(
                                    "notebook.shell.inspector.read_only_accessibility",
                                ))
                                .on_click({
                                    let this = this.clone();
                                    move |checked: &bool, _, app| {
                                        let checked = *checked;
                                        let id = toggle_id.clone();
                                        this.update(app, |v, cx| {
                                            if checked {
                                                v.make_read_only(&id, cx);
                                            } else {
                                                v.request_allow_writes(&id, cx);
                                            }
                                        });
                                    }
                                }),
                        )
                        .child(field(
                            t("notebook.shell.inspector.timeout_label"),
                            match policy.timeout_seconds {
                                Some(seconds) => crate::localization::format(
                                    "notebook.shell.inspector.timeout_value",
                                    &[("seconds", seconds)],
                                ),
                                None => t("notebook.shell.inspector.timeout_default"),
                            },
                        ))
                        .child(field(
                            t("notebook.shell.inspector.max_rows_label"),
                            match policy.max_rows {
                                Some(rows) => crate::localization::format(
                                    if rows == 1 {
"notebook.shell.inspector.max_rows_value_one"
} else {
"notebook.shell.inspector.max_rows_value"
},
                                    &[("count", rows)],
                                ),
                                None => t("notebook.shell.inspector.max_rows_default"),
                            },
                        ))
                        .child(section(t("notebook.shell.inspector.dependencies_section")))
                        .child(field(t("notebook.shell.inspector.reads_label"), div()))
                        .child(self.cell_links("nb-inspector-reads", reads, cx))
                        .child(field(t("notebook.shell.inspector.used_by_label"), div()))
                        .child(self.cell_links("nb-inspector-used-by", used_by, cx));

                    let state = self.cell_state(&id);
                    let stale = self.is_stale(&id);
                    let fingerprint = self.doc.shown_fingerprint(&id).unwrap_or_default();
                    let rt = self.cells.get(&id);
                    let last_run = rt.and_then(|rt| {
                        rt.finished_at.as_ref().map(|at| match rt.elapsed_ms {
                            Some(ms) => crate::localization::format(
                                "notebook.shell.inspector.last_run_value",
                                &[("time", at.clone()), ("milliseconds", ms.to_string())],
                            ),
                            None => at.clone(),
                        })
                    });
                    body = body
                        .child(section(t("notebook.shell.inspector.run_section")))
                        .child(field(
                            t("notebook.shell.inspector.state_label"),
                            t(crate::notebook::view::status_label_key(state, stale)),
                        ))
                        .child(field(
                            t("notebook.shell.inspector.last_run_label"),
                            last_run.unwrap_or_else(|| t("notebook.shell.inspector.never_run")),
                        ))
                        .child(field(
                            t("notebook.shell.inspector.fingerprint_label"),
                            fingerprint,
                        ))
                        .children(self.render_local_inspector(&id, cx));
                }
            }
        }

        div()
            .flex()
            .flex_row()
            .h_full()
            .flex_none()
            .child(divider)
            .child(
                div()
                    .id("nb-inspector")
                    .w(px(self.layout.right_width))
                    .h_full()
                    .overflow_y_scroll()
                    .bg(Theme::sidebar_bg())
                    .child(body),
            )
            .into_any_element()
    }
}
