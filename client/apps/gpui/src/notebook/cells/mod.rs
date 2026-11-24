//! Cell renderer registry. Every cell kind implements [`NotebookCell`] and is
//! listed once in [`REGISTRY`]; the shell draws the common cell frame and
//! header and asks the kind's renderer for the body. A kind this build does
//! not know falls back to [`unsupported::UnsupportedCell`], which shows a
//! read-only placeholder and keeps the cell unchanged on save.
//!
//! Adding a kind: create `cells/<kind>.rs` with a unit struct implementing
//! the trait and add it to `REGISTRY`. Nothing in the shell changes.

use std::rc::Rc;

use gpui::{AnyElement, Context, Entity, Window};

use super::preview::SqlPreview;
use super::view::{CellStatus, NotebookView};
use super::visual::VisualCellView;
use crate::editor::SqlEditor;
use crate::results::ResultsView;

pub mod ai;
pub mod assertion;
pub mod attachment;
pub mod chart;
pub mod diff;
pub mod entity_detail;
pub mod kpi;
pub mod layout;
pub mod markdown;
pub mod parameter;
pub mod pivot;
pub mod plan;
pub mod profile;
pub mod scalar;
pub mod sql;
pub mod table;
pub mod transform;
pub mod unsupported;

/// Editor typography the shell lays cells out with, so a static preview and
/// the live editor that replaces it take the same height.
#[derive(Debug, Clone, PartialEq)]
pub struct EditorMetrics {
    pub font_family: gpui::SharedString,
    pub font_size: f32,
}

impl EditorMetrics {
    pub fn line_height(&self) -> f32 {
        self.font_size * 1.5
    }
}

impl Default for EditorMetrics {
    fn default() -> Self {
        Self {
            font_family: crate::theme::Theme::editor_font_family().into(),
            font_size: crate::theme::Theme::editor_font_size(),
        }
    }
}

/// Everything a renderer needs to draw one cell body. Owned snapshots, so a
/// renderer can hold it across `cx.listener` closures without borrowing the view.
pub struct CellRenderCx {
    pub id: String,
    pub index: usize,
    pub kind: String,
    pub source: String,
    pub active: bool,
    pub collapsed: bool,
    pub state: CellStatus,
    /// Whether the last result no longer matches the cell's current inputs.
    pub stale: bool,
    pub editor: Option<Entity<SqlEditor>>,
    pub results: Option<Entity<ResultsView>>,
    pub results_height: f32,
    pub preview: Option<Rc<SqlPreview>>,
    pub editing: bool,
    pub metrics: EditorMetrics,
    pub elapsed_ms: Option<u64>,
    pub message: Option<String>,
    /// An attachment cell's file, import and inputs.
    pub attachment: Option<Rc<crate::notebook::local::AttachmentCx>>,
    /// Drawn on a dashboard tile: the output only, no controls.
    pub output_only: bool,
    /// The cell's own view, for kinds that draw one (charts, tables, …).
    pub visual: Option<Entity<VisualCellView>>,
    /// Markdown with `{{name}}` references to scalar cells filled in.
    pub interpolated: Option<String>,
    /// Attachment pictures a Markdown cell's `attachment:` images name.
    pub markdown_images: Option<std::sync::Arc<markdown::MarkdownImages>>,
    /// The cell's own view, for kinds that keep state of their own.
    pub analysis: Option<gpui::AnyView>,
    /// The AI cell's spec and live run state (AI cells only).
    pub ai: Option<crate::notebook::ai::AiCellSnapshot>,
    /// The cell's kind-specific settings (`Cell::spec`).
    pub spec: serde_json::Value,
}

/// One cell kind's behavior and body renderer.
pub trait NotebookCell: Sync {
    /// The kind id stored in the notebook file (`sql`, `markdown`, …).
    fn kind(&self) -> &'static str;
    /// Translation key of the kind's short label (cell header, inspector).
    fn label_key(&self) -> &'static str;
    /// Whether runs execute this cell.
    fn runnable(&self) -> bool {
        false
    }
    /// Whether the cell is edited in a [`SqlEditor`] the host provides.
    fn uses_editor(&self) -> bool {
        false
    }
    /// Whether the add-cell controls offer this kind.
    fn insertable(&self) -> bool {
        true
    }
    /// A body that needs the notebook view itself (stateful widgets the
    /// view owns). When it returns `Some`, [`Self::render_body`] is not
    /// called.
    fn render_with_view(
        &self,
        _view: &mut NotebookView,
        _cell: &CellRenderCx,
        _window: &mut Window,
        _cx: &mut Context<NotebookView>,
    ) -> Option<AnyElement> {
        None
    }
    /// Whether runs compute the cell in the notebook (from other cells'
    /// outputs) instead of sending it to a connection.
    fn runs_locally(&self) -> bool {
        false
    }
    /// The cell body under the shared header.
    fn render_body(
        &self,
        cell: &CellRenderCx,
        window: &mut Window,
        cx: &mut Context<NotebookView>,
    ) -> AnyElement;
    /// The cell's output alone, for dashboard tiles and the viewer: no
    /// editor, no SQL, no settings.
    fn render_output(
        &self,
        cell: &CellRenderCx,
        window: &mut Window,
        cx: &mut Context<NotebookView>,
    ) -> AnyElement {
        self.render_body(cell, window, cx)
    }
}

/// Every cell kind this build knows. One entry per kind.
pub static REGISTRY: &[&(dyn NotebookCell + Sync)] = &[
    &sql::SqlCell,
    &markdown::MarkdownCell,
    &attachment::AttachmentCell,
    &chart::ChartCell,
    &table::TableCell,
    &pivot::PivotCell,
    &kpi::KpiCell,
    &scalar::ScalarCell,
    &transform::TransformCell,
    &profile::ProfileCell,
    &assertion::AssertionCell,
    &diff::DiffCell,
    &plan::PlanCell,
    &entity_detail::EntityDetailCell,
    &ai::AiCell,
    &layout::LayoutCell,
    &parameter::ParameterCell,
];

/// The renderer for `kind`, or the read-only placeholder for kinds this
/// build does not know.
pub fn renderer_for(kind: &str) -> &'static (dyn NotebookCell + Sync) {
    REGISTRY
        .iter()
        .copied()
        .find(|r| r.kind() == kind)
        .unwrap_or(&unsupported::UnsupportedCell)
}

/// Kinds the add-cell controls offer, in registry order.
pub fn insertable_kinds() -> impl Iterator<Item = &'static (dyn NotebookCell + Sync)> {
    REGISTRY.iter().copied().filter(|r| r.insertable())
}

/// `view` drawn from gpui's frame cache, filling a box whose size its
/// parent sets: a keystroke in another cell, a cursor blink or a hover
/// elsewhere repaints it without rebuilding it (the view notifies itself
/// when what it shows changes). Used for the editors and result grids
/// inside cells, which are the heaviest things a cell draws.
pub(crate) fn frame_cached<V: gpui::Render>(view: Entity<V>) -> AnyElement {
    use gpui::{IntoElement as _, Styled as _};
    view.cached(gpui::StyleRefinement::default().size_full())
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_kinds_are_unique_and_unknown_kinds_fall_back() {
        let mut kinds: Vec<&str> = REGISTRY.iter().map(|r| r.kind()).collect();
        let before = kinds.len();
        kinds.sort_unstable();
        kinds.dedup();
        assert_eq!(kinds.len(), before);
        assert_eq!(renderer_for("sql").kind(), "sql");
        assert!(renderer_for("sql").runnable());
        assert_eq!(renderer_for("markdown").kind(), "markdown");
        assert!(!renderer_for("markdown").runnable());
        assert_eq!(renderer_for("chart").kind(), "chart");
        assert!(renderer_for("chart").runs_locally());
        assert_eq!(
            renderer_for("python").kind(),
            unsupported::UnsupportedCell.kind()
        );
        assert!(!renderer_for("python").insertable());
    }

    #[test]
    fn every_kind_label_is_translated() {
        for renderer in REGISTRY
            .iter()
            .copied()
            .chain([&unsupported::UnsupportedCell as _])
        {
            assert!(
                sqlly_i18n::contains_key(renderer.label_key()),
                "{}",
                renderer.label_key()
            );
        }
    }
}
