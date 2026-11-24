//! Parameter cells: one notebook parameter's input in the document, the
//! same widget the parameters rail shows.

use gpui::{AnyElement, Context, IntoElement, Window, div};

use super::{CellRenderCx, NotebookCell};
use crate::notebook::view::NotebookView;

pub struct ParameterCell;

impl NotebookCell for ParameterCell {
    fn kind(&self) -> &'static str {
        sqlly_services::notebook::cells::parameter::KIND
    }

    fn label_key(&self) -> &'static str {
        "notebook.params.cell.kind_label"
    }

    fn render_with_view(
        &self,
        view: &mut NotebookView,
        cell: &CellRenderCx,
        window: &mut Window,
        cx: &mut Context<NotebookView>,
    ) -> Option<AnyElement> {
        Some(view.render_param_cell(cell, window, cx))
    }

    fn render_body(
        &self,
        _cell: &CellRenderCx,
        _window: &mut Window,
        _cx: &mut Context<NotebookView>,
    ) -> AnyElement {
        div().into_any_element()
    }
}
