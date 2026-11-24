//! The parameters rail page, the Apply bar, the definition editor overlay
//! and the parameter cell body.

use gpui::{
    AnyElement, Context, InteractiveElement as _, IntoElement, ParentElement as _, Styled as _,
    Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_component::switch::Switch;
use gpui_component::{Icon, Sizable as _};
use sqlly_core::ConnectionEnvironment;
use sqlly_services::notebook::RunMode;

use super::widget::Surface;
use crate::icon_catalog::AppIcon;
use crate::notebook::cells::CellRenderCx;
use crate::notebook::connection::environment_badge;
use crate::notebook::view::NotebookView;
use crate::theme::Theme;

fn t(key: &str) -> String {
    crate::localization::text(key)
}

fn note(text: String) -> AnyElement {
    div()
        .text_size(px(Theme::font_size_ratio(0.8)))
        .text_color(Theme::text_dim())
        .child(text)
        .into_any_element()
}

impl NotebookView {
    fn sets_menu_button(&self, cx: &mut Context<Self>) -> AnyElement {
        let this = cx.entity();
        let sets = self.param_sets();
        let label = t("notebook.params.presets.sets_menu");
        Button::new("nbp-rail-sets")
            .ghost()
            .xsmall()
            .label(label.clone())
            .icon(Icon::empty().path(AppIcon::ChevronDown.path()))
            .accessibility_label(label)
            .dropdown_menu(move |mut menu, _, _| {
                for (set, personal) in &sets {
                    let apply = this.clone();
                    let (id, personal) = (set.id.clone(), *personal);
                    let key = if personal {
                        "notebook.params.presets.apply_personal_set"
                    } else {
                        "notebook.params.presets.apply_shared_set"
                    };
                    menu = menu.item(
                        PopupMenuItem::new(crate::localization::format(
                            key,
                            &[("set_name", set.name.as_str())],
                        ))
                        .on_click(move |_, _, app| {
                            apply.update(app, |v, cx| v.apply_param_set(&id, personal, cx))
                        }),
                    );
                }
                if !sets.is_empty() {
                    menu = menu.separator();
                }
                for (set, personal) in &sets {
                    let delete = this.clone();
                    let (id, personal) = (set.id.clone(), *personal);
                    menu = menu.item(
                        PopupMenuItem::new(crate::localization::format(
                            "notebook.params.presets.delete_set",
                            &[("set_name", set.name.as_str())],
                        ))
                        .on_click(move |_, _, app| {
                            delete.update(app, |v, cx| v.delete_param_set(&id, personal, cx))
                        }),
                    );
                }
                if sets.is_empty() {
                    menu = menu.item(
                        PopupMenuItem::new(t("notebook.params.presets.no_sets")).disabled(true),
                    );
                }
                menu
            })
            .into_any_element()
    }

    /// The Parameters page of the left rail: one widget per parameter.
    pub(in crate::notebook) fn params_rail_page(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        self.ensure_param_widgets(window, cx);
        let s = Theme::scaled;
        let reactive = self.doc.run_mode() == RunMode::Reactive;
        let this = cx.entity();
        let mut out = vec![
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(s(6.0)))
                .px(px(s(10.0)))
                .py(px(s(6.0)))
                .child(
                    Button::new("nbp-rail-add")
                        .ghost()
                        .xsmall()
                        .icon(Icon::empty().path(AppIcon::Add.path()))
                        .label(t("notebook.params.rail.add"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.open_param_editor(None, window, cx)
                        })),
                )
                .child(self.sets_menu_button(cx))
                .child(div().flex_1())
                .child(
                    Switch::new("nbp-rail-reactive")
                        .checked(reactive)
                        .label(t("notebook.params.rail.reactive"))
                        .accessibility_label(t("notebook.params.rail.reactive_accessibility"))
                        .on_click(move |on, _, app| {
                            let on = *on;
                            this.update(app, |v, cx| v.set_reactive(on, cx));
                        }),
                )
                .into_any_element(),
        ];
        let names: Vec<String> = self
            .doc
            .param_defs()
            .iter()
            .map(|d| d.name.clone())
            .collect();
        if names.is_empty() {
            out.push(
                div()
                    .px(px(s(10.0)))
                    .py(px(s(8.0)))
                    .child(note(t("notebook.shell.rail.parameters_empty")))
                    .into_any_element(),
            );
        }
        for name in names {
            if let Some(widget) = self.param_ui.widget(&Surface::Rail, &name).cloned() {
                out.push(
                    div()
                        .px(px(s(10.0)))
                        .py(px(s(6.0)))
                        .border_b_1()
                        .border_color(Theme::border())
                        .child(widget)
                        .into_any_element(),
                );
            }
        }
        out.push(
            div()
                .px(px(s(10.0)))
                .py(px(s(6.0)))
                .child(note(t("notebook.params.rail.hint")))
                .into_any_element(),
        );
        out
    }

    /// The bar listing changes that wait for Apply and the cells they re-run.
    pub(in crate::notebook) fn render_param_apply_bar(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.param_ui.has_pending() {
            return None;
        }
        let s = Theme::scaled;
        let reactive = self.doc.run_mode() == RunMode::Reactive;
        let names = self.param_ui.pending_names();
        let cells = self.param_apply_preview();
        let headline = if names.is_empty() {
            crate::localization::format(
                "notebook.params.apply.remote_waiting",
                &[("count", cells.len())],
            )
        } else {
            crate::localization::format(
                "notebook.params.apply.changes_waiting",
                &[
                    ("count", names.len().to_string()),
                    ("parameters", names.join(", ")),
                ],
            )
        };
        let detail = if reactive {
            t("notebook.params.apply.reruns")
        } else {
            t("notebook.params.apply.goes_stale")
        };
        let chips: Vec<AnyElement> = cells
            .iter()
            .enumerate()
            .map(|(n, id)| {
                let name = self
                    .doc
                    .cell_by_id(id)
                    .map(|c| c.display_name().to_string())
                    .unwrap_or_else(|| id.clone());
                let (choice, _) = self.cell_connection(id);
                let (label, env) = match &choice {
                    Some(c) => (c.label.clone(), c.environment),
                    None => (
                        t("notebook.params.apply.local_cell"),
                        ConnectionEnvironment::Local,
                    ),
                };
                div()
                    .id(("nbp-apply-cell", n))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(4.0)))
                    .px(px(s(6.0)))
                    .py(px(s(1.0)))
                    .rounded(px(s(3.0)))
                    .border_1()
                    .border_color(Theme::border())
                    .text_size(px(Theme::font_size_ratio(0.8)))
                    .child(
                        div()
                            .text_color(Theme::text())
                            .child(crate::localization::format(
                                "notebook.params.apply.cell_on_connection",
                                &[
                                    ("cell_name", name.as_str()),
                                    ("connection_name", label.as_str()),
                                ],
                            )),
                    )
                    .when(choice.is_some(), |d| d.child(environment_badge(env)))
                    .into_any_element()
            })
            .collect();
        Some(
            div()
                .id("nbp-apply-bar")
                .flex()
                .flex_col()
                .gap(px(s(6.0)))
                .px(px(s(12.0)))
                .py(px(s(6.0)))
                .border_b_1()
                .border_color(Theme::warning())
                .bg(Theme::panel_bg())
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(s(8.0)))
                        .child(
                            div()
                                .flex_1()
                                .text_size(px(Theme::font_size_ratio(0.9)))
                                .text_color(Theme::text())
                                .child(headline),
                        )
                        .child(
                            Button::new("nbp-apply-discard")
                                .ghost()
                                .xsmall()
                                .label(t("notebook.params.apply.discard"))
                                .on_click(cx.listener(|this, _, _, cx| this.discard_params(cx))),
                        )
                        .child(
                            Button::new("nbp-apply")
                                .primary()
                                .xsmall()
                                .label(t("notebook.params.apply.apply"))
                                .on_click(cx.listener(|this, _, _, cx| this.apply_params(cx))),
                        ),
                )
                .when(!chips.is_empty(), |d| {
                    d.child(note(detail)).child(
                        div()
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .gap(px(s(4.0)))
                            .children(chips),
                    )
                })
                .into_any_element(),
        )
    }

    /// The definition editor, over the notebook.
    pub(in crate::notebook) fn render_param_editor(&mut self) -> Option<AnyElement> {
        let editor = self.param_ui.editor()?.clone();
        Some(
            div()
                .id("nbp-editor-scrim")
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(Theme::scrim())
                .child(editor)
                .into_any_element(),
        )
    }

    /// A parameter cell's body: its widget, or a picker when it shows no
    /// parameter yet.
    pub(in crate::notebook) fn render_param_cell(
        &mut self,
        cell: &CellRenderCx,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let s = Theme::scaled;
        if cell.collapsed {
            return div().into_any_element();
        }
        let shown = self
            .doc
            .model()
            .cell(&cell.id)
            .and_then(super::cell_parameter);
        if let Some(name) = &shown
            && let Some(widget) = self.param_cell_widget(&cell.id, name, window, cx)
        {
            return div()
                .px(px(s(12.0)))
                .py(px(s(8.0)))
                .child(widget)
                .into_any_element();
        }
        let names: Vec<String> = self
            .doc
            .param_defs()
            .iter()
            .map(|d| d.name.clone())
            .collect();
        let prompt = match &shown {
            Some(missing) => crate::localization::format(
                "notebook.params.cell.missing",
                &[("parameter_name", missing.as_str())],
            ),
            None => t("notebook.params.cell.choose"),
        };
        let mut row = div().flex().flex_row().flex_wrap().gap(px(s(6.0)));
        for (n, name) in names.into_iter().enumerate() {
            let id = cell.id.clone();
            row = row.child(
                Button::new(("nbp-cell-pick", cell.index * 1000 + n))
                    .ghost()
                    .xsmall()
                    .label(format!("@{name}"))
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.bind_param_cell(&id, &name, cx)),
                    ),
            );
        }
        let id = cell.id.clone();
        row =
            row.child(
                Button::new(("nbp-cell-new", cell.index))
                    .xsmall()
                    .icon(Icon::empty().path(AppIcon::Add.path()))
                    .label(t("notebook.params.cell.new"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.new_param_for_cell(&id, window, cx)
                    })),
            );
        div()
            .id(("nbp-cell-empty", cell.index))
            .flex()
            .flex_col()
            .gap(px(s(6.0)))
            .px(px(s(12.0)))
            .py(px(s(8.0)))
            .child(note(prompt))
            .child(row)
            .into_any_element()
    }
}
