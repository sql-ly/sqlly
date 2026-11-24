//! Chart cells: a chart over another cell's output (see `notebook::visual::chart`).

use gpui::{AnyElement, Context, IntoElement, ParentElement as _, Window, div};

use super::{CellRenderCx, NotebookCell};
use crate::notebook::view::NotebookView;

pub struct ChartCell;

impl NotebookCell for ChartCell {
    fn kind(&self) -> &'static str {
        "chart"
    }

    fn label_key(&self) -> &'static str {
        "notebook.visual.kinds.chart"
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
