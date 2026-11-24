//! Table cells: the input's rows in the app's results grid, so formats,
//! conditional colors, computed columns, frozen and hidden columns, sort,
//! column filters, column statistics, the cell detail and the row form all
//! work as they do for a query. The cell's settings are `-- sqlly`
//! directive lines kept in its spec; the grid reads them as it reads a
//! query's, and the grid's own Lock and Save buttons write back into the
//! directive editor, which saves them to the spec.

use std::hash::{Hash, Hasher};

use gpui::{
    AnyElement, AppContext as _, Context, Entity, IntoElement, ParentElement as _, Styled as _,
    Subscription, Window, div, px,
};
use sqlly_core::{CellValue, DisplayMessage, ResultColumn};
use sqlly_services::QueryEvent;
use sqlly_services::notebook::cells::spec;
use sqlly_services::notebook::cells::table::{TableCellSpec, search_rows};

use super::{
    VisualCellView, VisualEvent, VisualInputs, dim, editor_panel, editor_row, t, text_field,
};
use crate::editor::{SqlEditor, SqlEditorEvent};
use crate::notebook::embedded::grid_view::{self, DrillRows, GridChrome};
use crate::results::ResultsView;
use crate::theme::Theme;

/// Rows handed to the grid per batch.
const BATCH_ROWS: usize = 5_000;

#[derive(Default)]
pub struct TablePart {
    pub(crate) results: Option<Entity<ResultsView>>,
    pub(crate) directives: Option<Entity<SqlEditor>>,
    fed: u64,
    /// The selected row last reported (an index into the shown rows).
    selected: Option<usize>,
    _editor_sub: Option<Subscription>,
    _grid_sub: Option<Subscription>,
    /// Grids whose chrome the cell changed (embedded, locked view).
    pub(crate) chrome: std::collections::HashSet<gpui::EntityId>,
    /// The rows behind a drill while the grid cannot show them itself.
    pub(crate) drill: Option<DrillRows>,
}

/// A table's output: the input rows after the followed filter and search.
pub(crate) fn evaluate(
    inputs: &VisualInputs,
) -> Result<(Vec<ResultColumn>, Vec<Vec<CellValue>>), DisplayMessage> {
    let (columns, rows) = super::pass_through(inputs)?;
    let cell: TableCellSpec = spec::read(&inputs.spec);
    let rows = match &cell.search {
        Some(query) => search_rows(&rows, query),
        None => rows,
    };
    Ok((columns, rows))
}

impl VisualCellView {
    /// Report the grid's selected row when it changes, so entity detail
    /// cells reading this table can follow the selection.
    pub(crate) fn watch_table_selection(&mut self, cx: &mut Context<Self>) {
        if self.table._grid_sub.is_some() {
            return;
        }
        let Some(results) = self.table.results.clone() else {
            return;
        };
        let Some(state) = results.read(cx).active_grid_state(cx) else {
            return;
        };
        self.table._grid_sub = Some(cx.observe(&state, |this, state, cx| {
            let selected = {
                let s = state.read(cx);
                crate::tool_dialog::selected_row(&s.selection)
                    .and_then(|display| s.resident_row_for_display(display))
            };
            if selected != this.table.selected {
                this.table.selected = selected;
                cx.emit(VisualEvent::RowSelected(selected));
            }
        }));
    }

    fn table_spec(&self) -> TableCellSpec {
        spec::read(&self.inputs.spec)
    }

    fn ensure_table(&mut self, cx: &mut Context<Self>) -> (Entity<ResultsView>, Entity<SqlEditor>) {
        if let (Some(results), Some(editor)) =
            (self.table.results.clone(), self.table.directives.clone())
        {
            return (results, editor);
        }
        let editor = cx.new(SqlEditor::new);
        let text = self.table_spec().directive_text();
        editor.update(cx, |e, cx| e.set_text(text, cx));
        let results = cx.new(ResultsView::new);
        results.update(cx, |r, cx| r.set_editor(editor.downgrade(), cx));
        let sub = cx.subscribe(&editor, |this, editor, event: &SqlEditorEvent, cx| {
            if matches!(event, SqlEditorEvent::Change) {
                let text = editor.read(cx).text().to_string();
                let mut cell = this.table_spec();
                cell.set_directive_text(&text);
                this.emit_spec(spec::write(&cell), cx);
            }
        });
        self.table.results = Some(results.clone());
        self.table.directives = Some(editor.clone());
        self.table._editor_sub = Some(sub);
        (results, editor)
    }

    /// Hand the grid the input rows when they or the settings changed.
    fn feed_table(&mut self, results: &Entity<ResultsView>, cx: &mut Context<Self>) -> bool {
        let Ok((columns, rows)) = evaluate(&self.inputs) else {
            return false;
        };
        let cell = self.table_spec();
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.inputs
            .input
            .as_ref()
            .and_then(|i| i.output.as_ref())
            .map(|o| o.run_id.clone())
            .hash(&mut h);
        rows.len().hash(&mut h);
        cell.directives.hash(&mut h);
        cell.search.hash(&mut h);
        format!("{:?}", self.inputs.filter).hash(&mut h);
        let key = h.finish();
        if key == self.table.fed {
            return true;
        }
        self.table.fed = key;
        self.table._grid_sub = None;
        if self.table.selected.take().is_some() {
            cx.emit(VisualEvent::RowSelected(None));
        }
        let directives = cell.directive_text();
        results.update(cx, |r, cx| {
            r.clear(cx);
            r.set_query_sql(&directives, cx);
            r.handle_event(QueryEvent::Started, cx);
            r.handle_event(QueryEvent::ResultSet { index: 0, columns }, cx);
            for chunk in rows.chunks(BATCH_ROWS) {
                r.handle_event(
                    QueryEvent::Rows {
                        index: 0,
                        rows: chunk.to_vec(),
                    },
                    cx,
                );
            }
            r.handle_event(QueryEvent::Completed { elapsed_ms: 0 }, cx);
        });
        true
    }

    pub(crate) fn render_table(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let s = Theme::scaled;
        let cell = self.table_spec();
        let (results, editor) = self.ensure_table(cx);
        let mut root = div().flex().flex_col().w_full();
        if self.editing {
            let id = self.inputs.cell_id.clone();
            let search = self.field(
                &format!("table.search.{id}"),
                cell.search.as_deref().unwrap_or_default(),
                "notebook.visual.table.search_placeholder",
                window,
                cx,
                |v, text, cx| {
                    let mut cell = v.table_spec();
                    let text = text.trim().to_string();
                    cell.search = (!text.is_empty()).then_some(text);
                    v.emit_spec(spec::write(&cell), cx);
                },
            );
            root = root.child(
                editor_panel()
                    .child(editor_row(
                        t("notebook.visual.table.search"),
                        text_field(&search, 260.0),
                    ))
                    .child(dim(t("notebook.visual.table.directives_help")))
                    .child(div().h(px(s(110.0))).w_full().child(editor)),
            );
        }
        if !self.feed_table(&results, cx) {
            return root
                .child(
                    div()
                        .h(px(s(60.0)))
                        .px(px(s(12.0)))
                        .flex()
                        .items_center()
                        .child(dim(t("notebook.visual.table.no_data"))),
                )
                .into_any_element();
        }
        self.watch_table_selection(cx);
        let chrome = GridChrome {
            bare: self.output_only,
            lock: self.display.lock,
        };
        let grid = (self.output_only || chrome.lock.is_some() || !self.table.chrome.is_empty())
            .then(|| results.update(cx, |r, cx| r.embedded_grid(cx)))
            .flatten();
        if let Some(grid) = &grid {
            let weak = cx.weak_entity();
            let source = grid.downgrade();
            let drill = move || -> sqlly_datatable::RowSourceDrillHandler {
                std::rc::Rc::new(move |drill, app| {
                    let Some(source) = source.upgrade() else {
                        return;
                    };
                    let rows = DrillRows::of(&source, &drill, app);
                    weak.update(app, |v, cx| {
                        v.table.drill = rows;
                        cx.notify();
                    })
                    .ok();
                })
            };
            grid_view::apply(grid, chrome, &mut self.table.chrome, drill, cx);
        }
        let drill = self.table.drill.as_ref().map(|rows| {
            let close = super::small_button(
                format!("nb-table-drill-close-{}", self.inputs.cell_id),
                t("notebook.embedded.drill.close"),
            )
            .on_click(cx.listener(|v, _, _, cx| {
                v.table.drill = None;
                cx.notify();
            }));
            rows.render(close, cx)
        });
        if self.output_only
            && let Some(grid) = grid
        {
            let height = self
                .display
                .height
                .unwrap_or_else(|| grid_view::fit_height(&grid, cx));
            return root
                .child(
                    div()
                        .h(px(height))
                        .w_full()
                        .min_h_0()
                        .child(crate::notebook::cells::frame_cached(grid)),
                )
                .children(drill)
                .into_any_element();
        }
        root.child(
            div()
                .h(px(Theme::scaled(cell.height())))
                .w_full()
                .min_h_0()
                .child(crate::notebook::cells::frame_cached(results)),
        )
        .children(drill)
        .into_any_element()
    }
}
