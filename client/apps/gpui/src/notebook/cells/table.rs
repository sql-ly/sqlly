//! Table cells: a formatted results grid over another cell's output (see `notebook::visual::table`).

use gpui::{AnyElement, Context, IntoElement, ParentElement as _, Window, div};

use super::{CellRenderCx, NotebookCell};
use crate::notebook::view::NotebookView;

pub struct TableCell;

impl NotebookCell for TableCell {
    fn kind(&self) -> &'static str {
        "table"
    }

    fn label_key(&self) -> &'static str {
        "notebook.visual.kinds.table"
    }

    fn runnable(&self) -> bool {
        true
    }

    fn runs_locally(&self) -> bool {
        true
    }

    fn render_body(
        &self,
        cell: &CellRenderCx,
        _window: &mut Window,
        _cx: &mut Context<NotebookView>,
    ) -> AnyElement {
        if cell.collapsed {
            return div().into_any_element();
        }
        div().children(cell.visual.clone()).into_any_element()
    }
}
