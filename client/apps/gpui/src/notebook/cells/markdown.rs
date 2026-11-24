//! Markdown cells: rendered text, swapped for an editor while being edited.
//! Images show only attachment cells' pictures, named with
//! `![alt](attachment:<cell name or id>)`; any other image (a web address, a
//! file, inline data) is drawn as a small placeholder and never loaded.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use gpui::{
    AnyElement, Context, ElementId, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, Window, div, px,
};
use gpui_component::Sizable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::text::TextView;
use sqlly_services::notebook::cells::attachment::markdown_attachment_name;

use super::{CellRenderCx, NotebookCell, sql::editor_height};
use crate::notebook::view::NotebookView;
use crate::theme::Theme;

pub struct MarkdownCell;

/// The pictures a Markdown cell may show, by the name its source uses.
#[derive(Default)]
pub struct MarkdownImages(pub HashMap<String, Arc<gpui::Image>>);

/// A small neutral frame drawn where an image can't be shown.
fn placeholder() -> Arc<gpui::Image> {
    static IMAGE: OnceLock<Arc<gpui::Image>> = OnceLock::new();
    IMAGE
        .get_or_init(|| {
            let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="44" height="32" viewBox="0 0 44 32"><rect x="1" y="1" width="42" height="30" rx="4" fill="none" stroke="#8a8f98" stroke-width="1.5" stroke-dasharray="3 2"/><circle cx="15" cy="12" r="3" fill="#8a8f98"/><path d="M8 25l8-8 5 5 6-7 9 10z" fill="#8a8f98" opacity="0.6"/><path d="M10 28L34 4" stroke="#8a8f98" stroke-width="1.5"/></svg>"##;
            Arc::new(gpui::Image::from_bytes(
                gpui::ImageFormat::Svg,
                svg.as_bytes().to_vec(),
            ))
        })
        .clone()
}

/// The image a Markdown destination resolves to: the named attachment's
/// checked picture, else the placeholder.
pub(crate) fn markdown_image(
    destination: &str,
    images: Option<&MarkdownImages>,
) -> Arc<gpui::Image> {
    markdown_attachment_name(destination)
        .and_then(|name| images?.0.get(name).cloned())
        .unwrap_or_else(placeholder)
}

/// Markdown text whose images come only from `images`.
pub fn markdown_view(
    id: impl Into<ElementId>,
    text: impl Into<SharedString>,
    images: Option<Arc<MarkdownImages>>,
) -> TextView {
    TextView::markdown(id, text).image_source(move |uri| {
        gpui::ImageSource::Image(markdown_image(uri.as_ref(), images.as_deref()))
    })
}

impl NotebookCell for MarkdownCell {
    fn kind(&self) -> &'static str {
        "markdown"
    }

    fn label_key(&self) -> &'static str {
        "notebook.shell.cell.kind_markdown"
    }

    fn uses_editor(&self) -> bool {
        true
    }

    fn render_body(
        &self,
        cell: &CellRenderCx,
        _window: &mut Window,
        cx: &mut Context<NotebookView>,
    ) -> AnyElement {
        let s = Theme::scaled;
        if cell.collapsed {
            return div().into_any_element();
        }
        if cell.editing
            && let Some(editor) = cell.editor.clone()
        {
            return div()
                .flex()
                .flex_col()
                .gap(px(s(4.0)))
                .child(
                    div()
                        .h(px(editor_height(&cell.source, cell.metrics.line_height())))
                        .w_full()
                        .child(editor),
                )
                .child(
                    div().px(px(s(8.0))).pb(px(s(6.0))).child(
                        Button::new(("nb-md-done", cell.index))
                            .ghost()
                            .xsmall()
                            .label(crate::localization::text(
                                "notebook.shell.cell.markdown_done",
                            ))
                            .accessibility_label(crate::localization::text(
                                "notebook.shell.cell.markdown_done_accessibility",
                            ))
                            .on_click(cx.listener(|this, _, _, cx| this.finish_markdown_edit(cx))),
                    ),
                )
                .into_any_element();
        }
        let id = cell.id.clone();
        let text = if cell.source.trim().is_empty() {
            SharedString::from(crate::localization::text(
                "notebook.shell.cell.markdown_placeholder",
            ))
        } else {
            SharedString::from(
                cell.interpolated
                    .clone()
                    .unwrap_or_else(|| cell.source.clone()),
            )
        };
        div()
            .id(("nb-md-view", cell.index))
            .px(px(s(12.0)))
            .py(px(s(8.0)))
            .w_full()
            .on_click(cx.listener(move |this, event: &gpui::ClickEvent, _, cx| {
                if event.click_count() >= 2 {
                    this.edit_cell(&id, cx);
                } else {
                    this.activate_cell(&id, cx);
                }
            }))
            .child(markdown_view(
                SharedString::from(format!("nb-md-{}", cell.id)),
                text,
                cell.markdown_images.clone(),
            ))
            .into_any_element()
    }

    /// The rendered text, without the double-click-to-edit affordance.
    fn render_output(
        &self,
        cell: &CellRenderCx,
        _window: &mut Window,
        _cx: &mut Context<NotebookView>,
    ) -> AnyElement {
        div()
            .px(px(Theme::scaled(12.0)))
            .py(px(Theme::scaled(8.0)))
            .w_full()
            .child(markdown_view(
                SharedString::from(format!("nb-md-out-{}", cell.id)),
                SharedString::from(
                    cell.interpolated
                        .clone()
                        .unwrap_or_else(|| cell.source.clone()),
                ),
                cell.markdown_images.clone(),
            ))
            .into_any_element()
    }
}
