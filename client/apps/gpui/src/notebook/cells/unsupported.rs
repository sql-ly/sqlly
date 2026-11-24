//! Placeholder for cell kinds this build does not know (written by a newer
//! SQLLY or another lane's cell type). The cell is shown read-only and saved
//! back exactly as it was read.

use gpui::{
    AnyElement, Context, IntoElement, ParentElement as _, Styled as _, Window, div,
    prelude::FluentBuilder as _, px,
};

use super::{CellRenderCx, NotebookCell};
use crate::notebook::view::NotebookView;
use crate::theme::Theme;

pub struct UnsupportedCell;

/// Characters of the preserved source shown under the notice.
const SOURCE_PREVIEW_CHARS: usize = 400;

impl NotebookCell for UnsupportedCell {
    fn kind(&self) -> &'static str {
        "unsupported"
    }

    fn label_key(&self) -> &'static str {
        "notebook.shell.cell.kind_unsupported"
    }

    fn insertable(&self) -> bool {
        false
    }

    fn render_body(
        &self,
        cell: &CellRenderCx,
        _window: &mut Window,
        _cx: &mut Context<NotebookView>,
    ) -> AnyElement {
        let s = Theme::scaled;
        let excerpt: String = cell.source.chars().take(SOURCE_PREVIEW_CHARS).collect();
        div()
            .flex()
            .flex_col()
            .gap(px(s(4.0)))
            .px(px(s(12.0)))
            .py(px(s(8.0)))
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.9)))
                    .text_color(Theme::text_dim())
                    .child(notice(&cell.kind)),
            )
            .when(!excerpt.trim().is_empty(), |d| {
                d.child(
                    div()
                        .max_h(px(s(120.0)))
                        .overflow_hidden()
                        .font_family(cell.metrics.font_family.clone())
                        .text_size(px(Theme::font_size_ratio(0.85)))
                        .text_color(Theme::text_dim())
                        .child(excerpt),
                )
            })
            .into_any_element()
    }

    /// The notice alone: the preserved source is not shown on a tile.
    fn render_output(
        &self,
        cell: &CellRenderCx,
        _window: &mut Window,
        _cx: &mut Context<NotebookView>,
    ) -> AnyElement {
        div()
            .px(px(Theme::scaled(12.0)))
            .py(px(Theme::scaled(8.0)))
            .text_size(px(Theme::font_size_ratio(0.9)))
            .text_color(Theme::text_dim())
            .child(notice(&cell.kind))
            .into_any_element()
    }
}

/// The notice a cell of a kind this version does not know shows.
fn notice(kind: &str) -> String {
    crate::localization::format("notebook.shell.cell.unsupported_notice", &[("kind", kind)])
}
