//! Dashboard (`layout`) cells in the notebook: a summary of the dashboard
//! with buttons that open its editor, the viewer or a presentation. The
//! dashboard itself is edited on its own surface (`notebook/dashboard`).

use gpui::{
    AnyElement, Context, IntoElement, ParentElement as _, Styled as _, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_component::Icon;
use sqlly_services::notebook::cells::layout::LayoutSpec;

use super::{CellRenderCx, NotebookCell};
use crate::icon_catalog::AppIcon;
use crate::notebook::rails::text_button;
use crate::notebook::view::NotebookView;
use crate::theme::Theme;

pub struct LayoutCell;

fn t(key: &str) -> String {
    crate::localization::text(key)
}

impl NotebookCell for LayoutCell {
    fn kind(&self) -> &'static str {
        "layout"
    }

    fn label_key(&self) -> &'static str {
        "notebook.dashboard.cell.kind_label"
    }

    fn render_body(
        &self,
        cell: &CellRenderCx,
        _window: &mut Window,
        cx: &mut Context<NotebookView>,
    ) -> AnyElement {
        let s = Theme::scaled;
        let spec = LayoutSpec::from_value(&cell.spec)
            .map_err(|e| crate::localization::message(e.message()));
        let (summary, empty) = match &spec {
            Ok(spec) => (
                crate::localization::format(
                    "notebook.dashboard.cell.summary",
                    &[
                        ("sections", spec.sections.len()),
                        ("tiles", spec.tiles().count()),
                    ],
                ),
                spec.tiles().next().is_none(),
            ),
            Err(message) => (message.clone(), false),
        };
        let readable = spec.is_ok();
        let title = spec
            .as_ref()
            .ok()
            .map(|s: &LayoutSpec| s.title.clone())
            .filter(|t| !t.trim().is_empty());
        let id = cell.id.clone();
        let (edit, view_id, present, set_up) = (id.clone(), id.clone(), id.clone(), id);
        div()
            .flex()
            .flex_col()
            .gap(px(s(6.0)))
            .px(px(s(12.0)))
            .py(px(s(8.0)))
            .when_some(title, |d, title| {
                d.child(div().text_color(Theme::text()).child(title))
            })
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.9)))
                    .text_color(if readable {
                        Theme::text_dim()
                    } else {
                        Theme::warning()
                    })
                    .child(summary),
            )
            .when(readable, |d| {
                d.child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .gap(px(s(6.0)))
                        .when(empty, |d| {
                            d.child(
                                text_button(
                                    format!("nb-layout-setup-{}", cell.index),
                                    t("notebook.dashboard.cell.set_up"),
                                )
                                .icon(Icon::empty().path(AppIcon::Add.path()))
                                .on_click(cx.listener(
                                    move |this, _, _, cx| this.set_up_dashboard(&set_up, cx),
                                )),
                            )
                        })
                        .child(
                            text_button(
                                format!("nb-layout-edit-{}", cell.index),
                                t("notebook.dashboard.cell.edit"),
                            )
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.open_dashboard_editor(Some(edit.clone()), cx)
                                },
                            )),
                        )
                        .child(
                            text_button(
                                format!("nb-layout-view-{}", cell.index),
                                t("notebook.dashboard.cell.view"),
                            )
                            .icon(Icon::empty().path(AppIcon::View.path()))
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.open_dashboard_viewer(Some(view_id.clone()), cx)
                                },
                            )),
                        )
                        .child(
                            text_button(
                                format!("nb-layout-present-{}", cell.index),
                                t("notebook.dashboard.cell.present"),
                            )
                            .icon(Icon::empty().path(AppIcon::Play.path()))
                            .on_click(cx.listener(
                                move |this, _, window, cx| {
                                    this.start_presentation(Some(present.clone()), window, cx)
                                },
                            )),
                        ),
                )
            })
            .into_any_element()
    }

    fn render_output(
        &self,
        _cell: &CellRenderCx,
        _window: &mut Window,
        _cx: &mut Context<NotebookView>,
    ) -> AnyElement {
        div().into_any_element()
    }
}
