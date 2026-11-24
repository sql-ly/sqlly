//! AI analysis cells (spec 4.3): a prompt, a mode and a budget; after a
//! run, the plan, labelled findings with evidence links and the proposals
//! the user accepts or rejects. The body lives in `notebook::ai::render`.

use gpui::{AnyElement, Context, IntoElement, Window, div};

use super::{CellRenderCx, NotebookCell};
use crate::notebook::view::NotebookView;

pub struct AiCell;

impl NotebookCell for AiCell {
    fn kind(&self) -> &'static str {
        crate::notebook::ai::KIND_AI
    }

    fn label_key(&self) -> &'static str {
        "notebook.ai.cell.kind"
    }

    fn render_body(
        &self,
        cell: &CellRenderCx,
        window: &mut Window,
        cx: &mut Context<NotebookView>,
    ) -> AnyElement {
        match &cell.ai {
            Some(snapshot) => crate::notebook::ai::render::body(cell, snapshot, window, cx),
            None => div().into_any_element(),
        }
    }
}
