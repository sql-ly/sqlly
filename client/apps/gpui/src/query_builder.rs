//! Visual query builder overlay.
//!
//! Tables as cards, FK join lines on a canvas, clauses in a side panel, live
//! formatted SQL preview. The view never executes SQL — Insert and Run emit
//! [`QueryBuilderEvent`] for the host to route through the ordinary tab/gate.

use std::cell::Cell;
use std::sync::Arc;

use gpui::{
    App, Bounds, Entity, EventEmitter, FocusHandle, Focusable, Hsla, IntoElement, MouseButton,
    PaintQuad, ParentElement, Pixels, Point, Render, SharedString, Styled, TextAlign, TextRun,
    Window, canvas, div, point, prelude::*, px, uniform_list,
};
use gpui_component::Disableable as _;
use gpui_component::Sizable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::spinner::Spinner;
use gpui_component::switch::Switch;
use sqlly_intel::intellisense::{CompletionCatalog, CompletionObject};
use sqlly_lang::sqlformat::format_sql;
use sqlly_lang::sqlmodel::DatabaseType;

use crate::dialog::{Dialog, DialogFrame};
use crate::grid_filter_builder::{ALL_OPS, BuilderOp};
use crate::preferences::SqlFormattingPreferences;
use crate::query_builder_model::{
    Aggregate, BuildError, Condition, Join, JoinKind, OrderBy, QueryBuilderState, SortDir,
    join_dup, list_tables, suggest_joins,
};
use crate::theme::Theme;
use crate::tool_dialog::{ToolDialogContext, ToolDialogEvent};

const CARD_W: f32 = 240.0;
const CARD_HEADER: f32 = 32.0;
const COL_ROW: f32 = 22.0;
/// Row height of the left-hand table list; uniform so the list can be
/// virtualized (only visible rows are built each frame).
const TABLE_ROW: f32 = 24.0;

/// One entry of the left-hand table list, precomputed from the catalog so a
/// render never re-filters, re-sorts or re-formats thousands of objects.
#[derive(Clone)]
struct TableRow {
    schema: String,
    name: String,
    label: SharedString,
}

#[derive(Clone)]
pub enum QueryBuilderEvent {
    InsertIntoEditor { sql: String },
    Run { sql: String },
    Tool(ToolDialogEvent),
    Close,
}

pub struct QueryBuilderView {
    #[allow(dead_code)]
    context: ToolDialogContext,
    db: DatabaseType,
    catalog: Arc<CompletionCatalog>,
    pub(crate) state: QueryBuilderState,
    table_filter: Entity<InputState>,
    table_filter_text: String,
    /// The catalog's tables/views matching `table_filter_text`, sorted.
    /// Rebuilt only when the filter or the catalog changes.
    table_rows: Arc<Vec<TableRow>>,
    /// True while the host is still pulling this database's data model, so
    /// the table list shows a spinner instead of looking empty or frozen.
    catalog_loading: bool,
    /// Tables requested (e.g. Explorer → Design Query) before the catalog
    /// had them; added as soon as a catalog update contains them.
    pending_tables: Vec<(String, String)>,
    condition_inputs: Vec<(Entity<InputState>, Entity<InputState>)>,
    column_alias_inputs: Vec<Entity<InputState>>,
    limit_input: Entity<InputState>,
    formatting_prefs: SqlFormattingPreferences,
    drag: Option<(usize, (f32, f32))>,
    canvas_bounds: Cell<Option<Bounds<Pixels>>>,
    pub(crate) preview: Result<String, BuildError>,
    focus_handle: FocusHandle,
    adding_join: Option<(usize, usize, String, String)>,
    /// The optional decision provider behind inferred join suggestions;
    /// `None` (the default) leaves the builder exactly as it was.
    decision: Option<sqlly_ai::catalog::DynDecisionProvider>,
    /// Decision-model join suggestions awaiting the user's accept or
    /// dismiss. Never applied to `state.joins` without an explicit accept.
    inferred_suggestions: Vec<Join>,
    /// Bumped whenever table indices may have shifted, so an in-flight
    /// inference result for an older layout is dropped.
    infer_generation: u64,
    frame: DialogFrame,
    _subs: Vec<gpui::Subscription>,
}

impl EventEmitter<QueryBuilderEvent> for QueryBuilderView {}

impl QueryBuilderView {
    pub fn new(
        context: ToolDialogContext,
        catalog: Arc<CompletionCatalog>,
        db: DatabaseType,
        formatting_prefs: SqlFormattingPreferences,
        catalog_loading: bool,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        let table_filter = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "query.builder.filter_tables_placeholder",
            ))
        });
        let limit_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "query.builder.limit_placeholder",
            ))
        });
        let filter_sub = cx.subscribe_in(
            &table_filter,
            window,
            |this: &mut Self, input, event: &InputEvent, _window, cx| {
                if let InputEvent::Change = event {
                    this.table_filter_text = input.read(cx).value().to_string();
                    this.rebuild_table_rows();
                    cx.notify();
                }
            },
        );
        let limit_sub = cx.subscribe_in(
            &limit_input,
            window,
            |this: &mut Self, input, event: &InputEvent, _window, cx| {
                if let InputEvent::Change = event {
                    let text = input.read(cx).value().to_string();
                    this.state.limit = parse_limit(&text);
                    this.refresh_preview(cx);
                }
            },
        );
        let table_rows = Arc::new(build_table_rows(&catalog, ""));
        Self {
            context,
            db,
            catalog,
            state: QueryBuilderState::default(),
            table_filter,
            table_filter_text: String::new(),
            table_rows,
            catalog_loading,
            pending_tables: Vec::new(),
            condition_inputs: Vec::new(),
            column_alias_inputs: Vec::new(),
            limit_input,
            formatting_prefs,
            drag: None,
            canvas_bounds: Cell::new(None),
            preview: Err(BuildError::NoTables),
            focus_handle: cx.focus_handle(),
            adding_join: None,
            decision: crate::decision_ai::decision_provider(),
            inferred_suggestions: Vec::new(),
            infer_generation: 0,
            frame: DialogFrame::new(Theme::scaled(1100.0), Theme::scaled(700.0)),
            _subs: vec![filter_sub, limit_sub],
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn add_table_by_index(&mut self, index: usize, cx: &mut gpui::Context<Self>) {
        let Some(obj) = list_tables(&self.catalog, "").get(index).copied() else {
            return;
        };
        let obj = obj.clone();
        self.add_object(&obj, cx);
    }

    pub fn add_table_named(&mut self, schema: &str, name: &str, cx: &mut gpui::Context<Self>) {
        let Some(obj) = self.find_object(schema, name).cloned() else {
            // The data model is still loading: remember the request and add
            // the table when the catalog arrives instead of silently dropping it.
            if self.catalog_loading
                && !self
                    .pending_tables
                    .iter()
                    .any(|(s, n)| s == schema && n == name)
            {
                self.pending_tables
                    .push((schema.to_string(), name.to_string()));
                cx.notify();
            }
            return;
        };
        self.add_object(&obj, cx);
    }

    fn find_object(&self, schema: &str, name: &str) -> Option<&CompletionObject> {
        self.catalog
            .objects
            .iter()
            .find(|o| o.schema.as_deref().unwrap_or("") == schema && o.name == name)
    }

    /// Install the host's live catalog. The builder opens with whatever
    /// catalog is live at that moment — often empty or disk-seeded right after
    /// a connect or database switch — so the host pushes every update here.
    pub fn set_catalog(
        &mut self,
        catalog: Arc<CompletionCatalog>,
        loading: bool,
        cx: &mut gpui::Context<Self>,
    ) {
        let changed = !Arc::ptr_eq(&self.catalog, &catalog);
        if !changed && self.catalog_loading == loading {
            return;
        }
        self.catalog_loading = loading;
        if changed {
            self.catalog = catalog;
            self.rebuild_table_rows();
            for (schema, name) in std::mem::take(&mut self.pending_tables) {
                match self.find_object(&schema, &name).cloned() {
                    Some(obj) => self.add_object(&obj, cx),
                    None => self.pending_tables.push((schema, name)),
                }
            }
        }
        if !self.catalog_loading {
            // Nothing more is coming: a still-missing table does not exist.
            self.pending_tables.clear();
        }
        cx.notify();
    }

    fn rebuild_table_rows(&mut self) {
        self.table_rows = Arc::new(build_table_rows(&self.catalog, &self.table_filter_text));
    }

    fn add_object(&mut self, obj: &CompletionObject, cx: &mut gpui::Context<Self>) {
        let before = self.state.tables.len();
        let idx = self.state.add_table(obj);
        let suggested = suggest_joins(&self.catalog, &self.state, idx);
        let no_fk = suggested.is_empty();
        for join in suggested {
            self.state.joins.push(join);
        }
        // No declared FK connects the newly added table to anything on the
        // canvas: ask the optional decision model for inferred candidates.
        // With no provider configured (the default) nothing happens at all.
        if no_fk && self.state.tables.len() > before && self.state.tables.len() >= 2 {
            self.spawn_infer_joins(idx, cx);
        }
        self.refresh_preview(cx);
    }

    /// The inference view of the table at `index`: schema-qualified name plus
    /// column names. The builder catalog carries no column types, so types
    /// are unknown — only names ever leave here, never data.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    fn infer_table_cols(&self, index: usize) -> Option<sqlly_ai::catalog::TableCols> {
        let table = self.state.tables.get(index)?;
        let name = if table.schema.is_empty() {
            table.name.clone()
        } else {
            format!("{}.{}", table.schema, table.name)
        };
        Some(sqlly_ai::catalog::TableCols {
            name,
            columns: table.columns.iter().map(|c| (c.clone(), None)).collect(),
        })
    }

    /// Browser build: the Jev client is native-only and `decision_provider`
    /// always returns `None`, so there is nothing to spawn.
    #[cfg(target_arch = "wasm32")]
    fn spawn_infer_joins(&mut self, _new_table: usize, _cx: &mut gpui::Context<Self>) {}

    /// Judge candidate joins between the newly added table and every table
    /// already on the canvas, off the UI thread. Fire-and-forget: the
    /// builder never waits on it, and an error or timeout simply leaves no
    /// suggestions — behaviour is unchanged from today.
    #[cfg(not(target_arch = "wasm32"))]
    fn spawn_infer_joins(&mut self, new_table: usize, cx: &mut gpui::Context<Self>) {
        let Some(provider) = self.decision.clone() else {
            return;
        };
        let Some(added) = self.infer_table_cols(new_table) else {
            return;
        };
        let others: Vec<(usize, sqlly_ai::catalog::TableCols)> = (0..self.state.tables.len())
            .filter(|index| *index != new_table)
            .filter_map(|index| Some((index, self.infer_table_cols(index)?)))
            .collect();
        if others.is_empty() {
            return;
        }
        let generation = self.infer_generation;
        type InferOutcome = Vec<(usize, sqlly_ai::catalog::InferredJoin)>;
        let (tx, rx) = tokio::sync::oneshot::channel::<InferOutcome>();
        cx.background_spawn(async move {
            crate::tokio_handle()
                .spawn(async move {
                    let mut found = Vec::new();
                    for (index, other) in &others {
                        for inferred in
                            sqlly_ai::catalog::infer_joins(provider.as_ref(), &added, other).await
                        {
                            found.push((*index, inferred));
                        }
                    }
                    let _ = tx.send(found);
                })
                .await
                .ok();
        })
        .detach();
        let async_cx = cx.to_async();
        let weak_self = cx.weak_entity();
        cx.spawn(async move |_this, _cx| {
            if let Ok(found) = rx.await {
                let mut cx = async_cx.clone();
                weak_self
                    .update(&mut cx, |this, cx| {
                        this.apply_inferred_suggestions(generation, new_table, found, cx);
                    })
                    .ok();
            }
        })
        .detach();
    }

    /// Turn inference results into pending suggestions. Suggestions are
    /// never pushed into `state.joins` here — only an explicit accept does
    /// that. Results from a stale table layout are dropped.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    fn apply_inferred_suggestions(
        &mut self,
        generation: u64,
        new_table: usize,
        found: Vec<(usize, sqlly_ai::catalog::InferredJoin)>,
        cx: &mut gpui::Context<Self>,
    ) {
        if generation != self.infer_generation {
            return;
        }
        for (other, inferred) in found {
            if new_table >= self.state.tables.len() || other >= self.state.tables.len() {
                continue;
            }
            let join = Join {
                from: new_table,
                from_column: inferred.a_col,
                to: other,
                to_column: inferred.b_col,
                kind: JoinKind::Inner,
                from_fk: false,
                inferred: Some(inferred.probability),
            };
            if !join_dup(&self.state.joins, &join) && !join_dup(&self.inferred_suggestions, &join) {
                self.inferred_suggestions.push(join);
            }
        }
        cx.notify();
    }

    /// Accept one pending suggestion: move it into the query's joins.
    fn accept_inferred_suggestion(&mut self, index: usize, cx: &mut gpui::Context<Self>) {
        if index >= self.inferred_suggestions.len() {
            return;
        }
        let join = self.inferred_suggestions.remove(index);
        if !join_dup(&self.state.joins, &join) {
            self.state.joins.push(join);
        }
        self.refresh_preview(cx);
    }

    /// Dismiss one pending suggestion without applying it.
    fn dismiss_inferred_suggestion(&mut self, index: usize, cx: &mut gpui::Context<Self>) {
        if index < self.inferred_suggestions.len() {
            self.inferred_suggestions.remove(index);
        }
        cx.notify();
    }

    pub fn refresh_preview(&mut self, cx: &mut gpui::Context<Self>) {
        self.preview = self.state.to_sql(self.db);
        cx.notify();
    }

    fn formatted_preview(&self) -> String {
        match &self.preview {
            Ok(sql) => format_sql(sql, &self.formatting_prefs, None),
            Err(err) => err.to_string(),
        }
    }

    fn emit_insert(&mut self, cx: &mut gpui::Context<Self>) {
        if let Ok(sql) = &self.preview {
            let formatted = format_sql(sql, &self.formatting_prefs, None);
            cx.emit(QueryBuilderEvent::InsertIntoEditor { sql: formatted });
        }
    }

    fn emit_run(&mut self, cx: &mut gpui::Context<Self>) {
        if let Ok(sql) = &self.preview {
            let formatted = format_sql(sql, &self.formatting_prefs, None);
            cx.emit(QueryBuilderEvent::Run { sql: formatted });
        }
    }

    fn close(&mut self, cx: &mut gpui::Context<Self>) {
        cx.emit(QueryBuilderEvent::Close);
        cx.emit(QueryBuilderEvent::Tool(ToolDialogEvent::Close));
    }

    fn canvas_local(&self, position: Point<Pixels>) -> Option<(f32, f32)> {
        let bounds = self.canvas_bounds.get()?;
        let lx = f32::from(position.x - bounds.origin.x);
        let ly = f32::from(position.y - bounds.origin.y);
        Some((lx, ly))
    }

    fn card_height(column_count: usize) -> f32 {
        CARD_HEADER + column_count as f32 * COL_ROW + 8.0
    }

    fn on_mouse_move(
        &mut self,
        event: &gpui::MouseMoveEvent,
        _window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        if event.pressed_button != Some(MouseButton::Left) {
            return;
        }
        let Some((index, grab)) = self.drag else {
            return;
        };
        let Some((mx, my)) = self.canvas_local(event.position) else {
            return;
        };
        if let Some(table) = self.state.tables.get_mut(index) {
            table.position = ((mx - grab.0).max(0.0), (my - grab.1).max(0.0));
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn on_mouse_up(
        &mut self,
        _event: &gpui::MouseUpEvent,
        _window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        if self.drag.take().is_some() {
            cx.notify();
        }
    }

    fn start_drag(&mut self, index: usize, position: Point<Pixels>, cx: &mut gpui::Context<Self>) {
        let Some((mx, my)) = self.canvas_local(position) else {
            return;
        };
        let Some(table) = self.state.tables.get(index) else {
            return;
        };
        self.drag = Some((index, (mx - table.position.0, my - table.position.1)));
        cx.notify();
    }

    fn sync_condition_inputs(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        while self.condition_inputs.len() < self.state.conditions.len() {
            let i = self.condition_inputs.len();
            let a_val = self.state.conditions[i].a.clone();
            let b_val = self.state.conditions[i].b.clone();
            let a = cx.new(|cx| {
                let mut s = InputState::new(window, cx).placeholder(
                    crate::localization::input_placeholder(
                        window,
                        cx,
                        "query.builder.value_placeholder",
                    ),
                );
                s.set_value(a_val.clone(), window, cx);
                s
            });
            let b = cx.new(|cx| {
                let mut s = InputState::new(window, cx).placeholder(
                    crate::localization::input_placeholder(
                        window,
                        cx,
                        "query.builder.and_placeholder",
                    ),
                );
                s.set_value(b_val.clone(), window, cx);
                s
            });
            let a_sub = cx.subscribe_in(
                &a,
                window,
                move |this: &mut Self, input, event: &InputEvent, _w, cx| {
                    if let InputEvent::Change = event
                        && let Some(row) = this.state.conditions.get_mut(i)
                    {
                        row.a = input.read(cx).value().to_string();
                        this.refresh_preview(cx);
                    }
                },
            );
            let b_sub = cx.subscribe_in(
                &b,
                window,
                move |this: &mut Self, input, event: &InputEvent, _w, cx| {
                    if let InputEvent::Change = event
                        && let Some(row) = this.state.conditions.get_mut(i)
                    {
                        row.b = input.read(cx).value().to_string();
                        this.refresh_preview(cx);
                    }
                },
            );
            self._subs.push(a_sub);
            self._subs.push(b_sub);
            self.condition_inputs.push((a, b));
        }
        self.condition_inputs.truncate(self.state.conditions.len());
    }

    fn sync_alias_inputs(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        while self.column_alias_inputs.len() < self.state.columns.len() {
            let i = self.column_alias_inputs.len();
            let val = self.state.columns[i].alias.clone().unwrap_or_default();
            let input = cx.new(|cx| {
                let mut s = InputState::new(window, cx).placeholder(
                    crate::localization::input_placeholder(
                        window,
                        cx,
                        "query.builder.alias_placeholder",
                    ),
                );
                s.set_value(val.clone(), window, cx);
                s
            });
            let sub = cx.subscribe_in(
                &input,
                window,
                move |this: &mut Self, inp, event: &InputEvent, _w, cx| {
                    if let InputEvent::Change = event
                        && let Some(col) = this.state.columns.get_mut(i)
                    {
                        let v = inp.read(cx).value().to_string();
                        col.alias = if v.trim().is_empty() { None } else { Some(v) };
                        this.refresh_preview(cx);
                    }
                },
            );
            self._subs.push(sub);
            self.column_alias_inputs.push(input);
        }
        self.column_alias_inputs.truncate(self.state.columns.len());
    }

    fn all_column_choices(&self) -> Vec<(usize, String, String)> {
        let mut out = Vec::new();
        for (ti, table) in self.state.tables.iter().enumerate() {
            for col in &table.columns {
                out.push((ti, col.clone(), format!("{}.{}", table.alias, col)));
            }
        }
        out
    }

    fn render_table_list(&self, cx: &mut gpui::Context<Self>) -> gpui::AnyElement {
        let s = Theme::scaled;
        let rows = self.table_rows.clone();
        let status = self.render_table_list_status();
        let list = uniform_list(
            "qb-table-list",
            rows.len(),
            cx.processor(
                move |this: &mut Self, range: std::ops::Range<usize>, _window, cx| {
                    let mut items = Vec::with_capacity(range.len());
                    for ix in range {
                        let Some(row) = rows.get(ix) else {
                            continue;
                        };
                        let already = this
                            .state
                            .tables
                            .iter()
                            .any(|t| t.schema == row.schema && t.name == row.name);
                        let schema = row.schema.clone();
                        let name = row.name.clone();
                        items.push(
                            div()
                                .id(("qb-tbl", ix))
                                .h(px(s(TABLE_ROW)))
                                .px(px(s(6.0)))
                                .flex()
                                .items_center()
                                .rounded(px(s(4.0)))
                                .cursor_pointer()
                                .hover(|d| d.bg(Theme::row_hover()))
                                .when(already, |d| d.bg(Theme::selection_fill()))
                                .text_color(Theme::text())
                                .text_size(px(Theme::font_size_ratio(0.85)))
                                .whitespace_nowrap()
                                .overflow_hidden()
                                .text_ellipsis()
                                .child(row.label.clone())
                                .on_click(cx.listener(move |this, _, _window, cx| {
                                    this.add_table_named(&schema, &name, cx);
                                })),
                        );
                    }
                    items
                },
            ),
        );
        div()
            .flex()
            .flex_col()
            .gap(px(s(8.0)))
            .w(px(s(240.0)))
            .h_full()
            .min_h_0()
            .child(Input::new(&self.table_filter).cleanable(true))
            .when_some(status, |d, status| d.child(status))
            .child(list.flex_1().min_h_0())
            .into_any_element()
    }

    /// The line under the filter box: a spinner while the data model loads,
    /// or an empty-state message once it has loaded with nothing to show.
    fn render_table_list_status(&self) -> Option<gpui::AnyElement> {
        let s = Theme::scaled;
        let empty = self.table_rows.is_empty();
        let key = if self.catalog_loading {
            if empty {
                "query.builder.loading_tables"
            } else {
                "query.builder.refreshing_tables"
            }
        } else if !empty {
            return None;
        } else if self.table_filter_text.trim().is_empty() {
            "query.builder.no_tables"
        } else {
            "query.builder.no_matching_tables"
        };
        Some(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(s(6.0)))
                .px(px(s(6.0)))
                .text_color(Theme::text_dim())
                .text_size(px(Theme::font_size_ratio(0.8)))
                .when(self.catalog_loading, |d| {
                    d.child(Spinner::new().color(Theme::text_dim()))
                })
                .child(crate::localization::text(key))
                .into_any_element(),
        )
    }

    fn render_card(&self, index: usize, cx: &mut gpui::Context<Self>) -> gpui::AnyElement {
        let s = Theme::scaled;
        let Some(table) = self.state.tables.get(index) else {
            return div().into_any_element();
        };
        let (x, y) = table.position;
        let title = if table.schema.is_empty() {
            format!("{} ({})", table.name, table.alias)
        } else {
            format!("{}.{} ({})", table.schema, table.name, table.alias)
        };
        let mut body = div().flex().flex_col();
        for col in &table.columns {
            let checked = self.state.column_selected(index, col);
            let col_name = col.clone();
            body = body.child(
                div()
                    .h(px(COL_ROW))
                    .px(px(s(6.0)))
                    .flex()
                    .flex_row()
                    .items_center()
                    .child(
                        Checkbox::new(format!("qb-col-{index}-{col}"))
                            .checked(checked)
                            .label(col.clone())
                            .on_click(cx.listener(move |this, _checked, _w, cx| {
                                this.state.toggle_column(index, &col_name);
                                this.refresh_preview(cx);
                            })),
                    ),
            );
        }
        div()
            .absolute()
            .left(px(x))
            .top(px(y))
            .w(px(CARD_W))
            .bg(Theme::popup_bg())
            .border_1()
            .border_color(Theme::border())
            .rounded(px(s(6.0)))
            .child(
                div()
                    .h(px(CARD_HEADER))
                    .px(px(s(8.0)))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(4.0)))
                    .border_b_1()
                    .border_color(Theme::border())
                    .cursor(gpui::CursorStyle::PointingHand)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, ev: &gpui::MouseDownEvent, _w, cx| {
                            this.start_drag(index, ev.position, cx);
                            cx.stop_propagation();
                        }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .text_size(px(Theme::font_size_ratio(0.85)))
                            .text_color(Theme::text())
                            .child(title),
                    )
                    .child(
                        Button::new(("qb-rm", index))
                            .small()
                            .ghost()
                            .label("×")
                            .on_click(cx.listener(move |this, _, _w, cx| {
                                this.state.remove_table(index);
                                // Table indices shifted: pending and
                                // in-flight suggestions are stale.
                                this.inferred_suggestions.clear();
                                this.infer_generation += 1;
                                this.refresh_preview(cx);
                            })),
                    ),
            )
            .child(body)
            .into_any_element()
    }

    fn render_canvas(&self, cx: &mut gpui::Context<Self>) -> gpui::AnyElement {
        let paint = self.build_paint();
        let entity = cx.entity();
        let mut stack = div()
            .id("qb-canvas")
            .relative()
            .flex_1()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .overflow_hidden()
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .child(
                canvas(
                    move |bounds, _window, cx| {
                        entity.read(cx).canvas_bounds.set(Some(bounds));
                        paint.clone()
                    },
                    move |bounds, paint, window, cx| paint_joins(&paint, bounds, window, cx),
                )
                .size_full()
                .absolute()
                .top_0()
                .left_0(),
            );
        for i in 0..self.state.tables.len() {
            stack = stack.child(self.render_card(i, cx));
        }
        stack.into_any_element()
    }

    fn build_paint(&self) -> JoinPaint {
        let mut cards = Vec::new();
        for table in &self.state.tables {
            cards.push(CardPaint {
                x: table.position.0,
                y: table.position.1,
                h: Self::card_height(table.columns.len()),
            });
        }
        let mut edges = Vec::new();
        for join in &self.state.joins {
            edges.push(JoinEdgePaint {
                from: join.from,
                to: join.to,
                label: join.kind.label().to_string(),
                from_fk: join.from_fk,
            });
        }
        JoinPaint {
            cards,
            edges,
            background: Theme::panel_bg(),
            line: Theme::text_dim(),
            font_size: Theme::font_size(),
        }
    }

    fn render_clauses(&self, cx: &mut gpui::Context<Self>) -> gpui::AnyElement {
        let s = Theme::scaled;
        let caption = |text: &str| {
            div()
                .text_size(px(Theme::font_size_ratio(0.74)))
                .text_color(Theme::text_dim())
                .child(text.to_string())
        };
        let choices = self.all_column_choices();
        let mut joins = div().flex().flex_col().gap(px(s(4.0)));
        for (ji, join) in self.state.joins.iter().enumerate() {
            let from_a = self
                .state
                .tables
                .get(join.from)
                .map(|t| t.alias.as_str())
                .unwrap_or("?");
            let to_a = self
                .state
                .tables
                .get(join.to)
                .map(|t| t.alias.as_str())
                .unwrap_or("?");
            let label = format!("{from_a}.{} → {to_a}.{}", join.from_column, join.to_column);
            joins = joins.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(4.0)))
                    .h(px(Theme::row_height()))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .text_size(px(Theme::font_size_ratio(0.8)))
                            .text_color(Theme::text())
                            .child(label),
                    )
                    .when_some(join.inferred, |d, probability| {
                        d.child(
                            div()
                                .text_size(px(Theme::font_size_ratio(0.74)))
                                .text_color(Theme::text_dim())
                                .child(inferred_label(probability)),
                        )
                    })
                    .child(
                        Button::new(("qb-jk", ji))
                            .small()
                            .label(join.kind.label())
                            .on_click(cx.listener(move |this, _, _w, cx| {
                                this.state.cycle_join_kind(ji);
                                this.refresh_preview(cx);
                            })),
                    )
                    .child(
                        Button::new(("qb-jrm", ji))
                            .small()
                            .ghost()
                            .label("×")
                            .on_click(cx.listener(move |this, _, _w, cx| {
                                if ji < this.state.joins.len() {
                                    this.state.joins.remove(ji);
                                }
                                this.refresh_preview(cx);
                            })),
                    ),
            );
        }
        // Pending decision-model suggestions: shown in the same list, dim,
        // and applied only through their Add button.
        for (si, suggestion) in self.inferred_suggestions.iter().enumerate() {
            let from_a = self
                .state
                .tables
                .get(suggestion.from)
                .map(|t| t.alias.as_str())
                .unwrap_or("?");
            let to_a = self
                .state
                .tables
                .get(suggestion.to)
                .map(|t| t.alias.as_str())
                .unwrap_or("?");
            let label = format!(
                "{from_a}.{} → {to_a}.{}",
                suggestion.from_column, suggestion.to_column
            );
            joins = joins.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(4.0)))
                    .h(px(Theme::row_height()))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .text_size(px(Theme::font_size_ratio(0.8)))
                            .text_color(Theme::text_dim())
                            .child(label),
                    )
                    .child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.74)))
                            .text_color(Theme::text_dim())
                            .child(inferred_label(suggestion.inferred.unwrap_or(0.0))),
                    )
                    .child(
                        Button::new(("qb-inf-add", si))
                            .small()
                            .label(crate::localization::text(
                                "query.builder.accept_inferred_join_button",
                            ))
                            .on_click(cx.listener(move |this, _, _w, cx| {
                                this.accept_inferred_suggestion(si, cx);
                            })),
                    )
                    .child(
                        Button::new(("qb-inf-rm", si))
                            .small()
                            .ghost()
                            .label("×")
                            .on_click(cx.listener(move |this, _, _w, cx| {
                                this.dismiss_inferred_suggestion(si, cx);
                            })),
                    ),
            );
        }
        joins = joins.child(
            Button::new("qb-add-join")
                .small()
                .ghost()
                .label(crate::localization::text("query.builder.add_join_button"))
                .on_click(cx.listener(|this, _, _w, cx| {
                    if this.state.tables.len() >= 2 {
                        this.adding_join = Some((
                            0,
                            1.min(this.state.tables.len() - 1),
                            this.state.tables[0]
                                .columns
                                .first()
                                .cloned()
                                .unwrap_or_default(),
                            this.state.tables[1.min(this.state.tables.len() - 1)]
                                .columns
                                .first()
                                .cloned()
                                .unwrap_or_default(),
                        ));
                    }
                    cx.notify();
                })),
        );
        if let Some((from, to, from_col, to_col)) = &self.adding_join {
            let from = *from;
            let to = *to;
            let from_col = from_col.clone();
            let to_col = to_col.clone();
            joins = joins.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(s(4.0)))
                    .child(self.render_join_pickers(from, to, &from_col, &to_col, &choices, cx))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(s(4.0)))
                            .child(
                                Button::new("qb-join-ok")
                                    .small()
                                    .label(crate::localization::text(
                                        "query.builder.confirm_join_button",
                                    ))
                                    .on_click(cx.listener(move |this, _, _w, cx| {
                                        this.state.joins.push(Join {
                                            from,
                                            from_column: from_col.clone(),
                                            to,
                                            to_column: to_col.clone(),
                                            kind: JoinKind::Inner,
                                            from_fk: false,
                                            inferred: None,
                                        });
                                        this.adding_join = None;
                                        this.refresh_preview(cx);
                                    })),
                            )
                            .child(
                                Button::new("qb-join-cancel")
                                    .small()
                                    .ghost()
                                    .label(crate::localization::text(
                                        "query.builder.cancel_join_button",
                                    ))
                                    .on_click(cx.listener(|this, _, _w, cx| {
                                        this.adding_join = None;
                                        cx.notify();
                                    })),
                            ),
                    ),
            );
        }

        let mut cols = div().flex().flex_col().gap(px(s(4.0)));
        cols = cols.child(
            Switch::new("qb-distinct")
                .label("DISTINCT")
                .checked(self.state.distinct)
                .on_click(cx.listener(|this, checked, _w, cx| {
                    this.state.distinct = *checked;
                    this.refresh_preview(cx);
                })),
        );
        for (ci, col) in self.state.columns.iter().enumerate() {
            let label = self
                .state
                .tables
                .get(col.table)
                .map(|t| format!("{}.{}", t.alias, col.column))
                .unwrap_or_else(|| col.column.clone());
            let alias_input = self.column_alias_inputs.get(ci).cloned();
            cols = cols.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(4.0)))
                    .h(px(Theme::row_height()))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .text_size(px(Theme::font_size_ratio(0.8)))
                            .text_color(Theme::text())
                            .child(label),
                    )
                    .child(
                        Button::new(("qb-agg", ci))
                            .small()
                            .label(col.aggregate.label())
                            .on_click(cx.listener(move |this, _, _w, cx| {
                                if let Some(c) = this.state.columns.get_mut(ci) {
                                    c.aggregate = c.aggregate.cycle();
                                }
                                this.refresh_preview(cx);
                            })),
                    )
                    .when_some(alias_input, |d, input| {
                        d.child(div().w(px(s(88.0))).child(Input::new(&input)))
                    }),
            );
        }

        let mut wheres = div().flex().flex_col().gap(px(s(4.0)));
        for (ri, cond) in self.state.conditions.iter().enumerate() {
            let cond_label = self
                .state
                .tables
                .get(cond.table)
                .map(|t| format!("{}.{}", t.alias, cond.column))
                .unwrap_or_else(|| cond.column.clone());
            let inputs = self.condition_inputs.get(ri).cloned();
            let show_a = cond.op.needs_a();
            let show_b = cond.op.needs_b();
            wheres = wheres.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(s(2.0)))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(s(4.0)))
                            .child(
                                Button::new(("qb-wcol", ri))
                                    .small()
                                    .label(cond_label)
                                    .on_click(cx.listener(move |this, _, _w, cx| {
                                        this.cycle_condition_column(ri);
                                        this.refresh_preview(cx);
                                    })),
                            )
                            .child(
                                Button::new(("qb-wop", ri))
                                    .small()
                                    .label(cond.op.label())
                                    .on_click(cx.listener(move |this, _, _w, cx| {
                                        if let Some(c) = this.state.conditions.get_mut(ri) {
                                            c.op = cycle_op(c.op);
                                        }
                                        this.refresh_preview(cx);
                                    })),
                            )
                            .child(
                                Button::new(("qb-wrm", ri))
                                    .small()
                                    .ghost()
                                    .label("×")
                                    .on_click(cx.listener(move |this, _, _w, cx| {
                                        if ri < this.state.conditions.len() {
                                            this.state.conditions.remove(ri);
                                        }
                                        this.refresh_preview(cx);
                                    })),
                            ),
                    )
                    .when(show_a, |d| {
                        d.when_some(inputs.clone(), |d, (a, _)| d.child(Input::new(&a)))
                    })
                    .when(show_b, |d| {
                        d.when_some(inputs, |d, (_, b)| d.child(Input::new(&b)))
                    }),
            );
        }
        wheres = wheres.child(
            Button::new("qb-add-where")
                .small()
                .ghost()
                .label(crate::localization::text("query.builder.add_filter_button"))
                .on_click(cx.listener(|this, _, _w, cx| {
                    if let Some((table, column, _)) = this.all_column_choices().first().cloned() {
                        this.state.conditions.push(Condition {
                            table,
                            column,
                            op: BuilderOp::Contains,
                            a: String::new(),
                            b: String::new(),
                        });
                        this.refresh_preview(cx);
                    }
                })),
        );

        let mut groups = div().flex().flex_col().gap(px(s(2.0)));
        for col in self
            .state
            .columns
            .iter()
            .filter(|c| c.aggregate == Aggregate::None)
        {
            let checked = self
                .state
                .group_by
                .contains(&(col.table, col.column.clone()));
            let table = col.table;
            let column = col.column.clone();
            let label = self
                .state
                .tables
                .get(table)
                .map(|t| format!("{}.{}", t.alias, column))
                .unwrap_or_else(|| column.clone());
            groups = groups.child(
                Checkbox::new(format!("qb-gb-{table}-{column}"))
                    .checked(checked)
                    .label(label)
                    .on_click(cx.listener(move |this, _, _w, cx| {
                        let key = (table, column.clone());
                        if let Some(i) = this.state.group_by.iter().position(|g| g == &key) {
                            this.state.group_by.remove(i);
                        } else {
                            this.state.group_by.push(key);
                        }
                        this.refresh_preview(cx);
                    })),
            );
        }

        let mut orders = div().flex().flex_col().gap(px(s(4.0)));
        for (oi, order) in self.state.order_by.iter().enumerate() {
            let label = self
                .state
                .tables
                .get(order.table)
                .map(|t| format!("{}.{}", t.alias, order.column))
                .unwrap_or_else(|| order.column.clone());
            orders = orders.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(4.0)))
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(Theme::font_size_ratio(0.8)))
                            .text_color(Theme::text())
                            .child(label),
                    )
                    .child(
                        Button::new(("qb-odir", oi))
                            .small()
                            .label(order.dir.sql())
                            .on_click(cx.listener(move |this, _, _w, cx| {
                                if let Some(o) = this.state.order_by.get_mut(oi) {
                                    o.dir = o.dir.cycle();
                                }
                                this.refresh_preview(cx);
                            })),
                    )
                    .child(
                        Button::new(("qb-orm", oi))
                            .small()
                            .ghost()
                            .label("×")
                            .on_click(cx.listener(move |this, _, _w, cx| {
                                if oi < this.state.order_by.len() {
                                    this.state.order_by.remove(oi);
                                }
                                this.refresh_preview(cx);
                            })),
                    ),
            );
        }
        orders = orders.child(
            Button::new("qb-add-order")
                .small()
                .ghost()
                .label(crate::localization::text("query.builder.add_order_button"))
                .on_click(cx.listener(|this, _, _w, cx| {
                    if let Some(col) = this.state.columns.first() {
                        this.state.order_by.push(OrderBy {
                            table: col.table,
                            column: col.column.clone(),
                            dir: SortDir::Asc,
                        });
                        this.refresh_preview(cx);
                    }
                })),
        );

        let preview_color = if self.preview.is_ok() {
            Theme::text()
        } else {
            Theme::text_dim()
        };
        div()
            .flex()
            .flex_col()
            .gap(px(s(10.0)))
            .w(px(s(280.0)))
            .h_full()
            .min_h_0()
            .id("qb-clauses")
            .overflow_y_scroll()
            .child(caption(&crate::localization::text(
                "query.builder.joins_caption",
            )))
            .child(joins)
            .child(caption(&crate::localization::text(
                "query.builder.columns_caption",
            )))
            .child(cols)
            .child(caption(&crate::localization::text(
                "query.builder.where_caption",
            )))
            .child(wheres)
            .child(caption(&crate::localization::text(
                "query.builder.group_by_caption",
            )))
            .child(groups)
            .child(caption(&crate::localization::text(
                "query.builder.order_by_caption",
            )))
            .child(orders)
            .child(caption(&crate::localization::text(
                "query.builder.limit_caption",
            )))
            .child(Input::new(&self.limit_input))
            .child(caption(&crate::localization::text(
                "query.builder.preview_caption",
            )))
            .child(
                div()
                    .p(px(s(8.0)))
                    .rounded(px(s(4.0)))
                    .border_1()
                    .border_color(Theme::border())
                    .bg(Theme::panel_bg())
                    .font_family(Theme::editor_font_family())
                    .text_size(px(Theme::font_size_ratio(0.8)))
                    .text_color(preview_color)
                    .whitespace_normal()
                    .child(self.formatted_preview()),
            )
            .into_any_element()
    }

    fn render_join_pickers(
        &self,
        from: usize,
        to: usize,
        from_col: &str,
        to_col: &str,
        _choices: &[(usize, String, String)],
        cx: &mut gpui::Context<Self>,
    ) -> gpui::AnyElement {
        let s = Theme::scaled;
        let from_label = self
            .state
            .tables
            .get(from)
            .map(|t| format!("{}.{}", t.alias, from_col))
            .unwrap_or_default();
        let to_label = self
            .state
            .tables
            .get(to)
            .map(|t| format!("{}.{}", t.alias, to_col))
            .unwrap_or_default();
        div()
            .flex()
            .flex_row()
            .gap(px(s(4.0)))
            .child(
                Button::new("qb-jf")
                    .small()
                    .label(from_label)
                    .on_click(cx.listener(|this, _, _w, cx| {
                        this.cycle_adding_join_from();
                        cx.notify();
                    })),
            )
            .child(
                Button::new("qb-jt")
                    .small()
                    .label(to_label)
                    .on_click(cx.listener(|this, _, _w, cx| {
                        this.cycle_adding_join_to();
                        cx.notify();
                    })),
            )
            .into_any_element()
    }

    fn cycle_adding_join_from(&mut self) {
        let choices = self.all_column_choices();
        if choices.is_empty() {
            return;
        }
        let Some((from, _to, from_col, _to_col)) = self.adding_join.as_ref() else {
            return;
        };
        let cur = choices
            .iter()
            .position(|(t, c, _)| t == from && c == from_col)
            .unwrap_or(0);
        let next = (cur + 1) % choices.len();
        if let Some(slot) = self.adding_join.as_mut() {
            slot.0 = choices[next].0;
            slot.2 = choices[next].1.clone();
        }
    }

    fn cycle_adding_join_to(&mut self) {
        let choices = self.all_column_choices();
        if choices.is_empty() {
            return;
        }
        let Some((_from, to, _from_col, to_col)) = self.adding_join.as_ref() else {
            return;
        };
        let cur = choices
            .iter()
            .position(|(t, c, _)| t == to && c == to_col)
            .unwrap_or(0);
        let next = (cur + 1) % choices.len();
        if let Some(slot) = self.adding_join.as_mut() {
            slot.1 = choices[next].0;
            slot.3 = choices[next].1.clone();
        }
    }

    fn cycle_condition_column(&mut self, ri: usize) {
        let choices = self.all_column_choices();
        if choices.is_empty() {
            return;
        }
        let Some(cond) = self.state.conditions.get_mut(ri) else {
            return;
        };
        let cur = choices
            .iter()
            .position(|(t, c, _)| *t == cond.table && c == &cond.column)
            .unwrap_or(0);
        let next = (cur + 1) % choices.len();
        cond.table = choices[next].0;
        cond.column = choices[next].1.clone();
    }
}

impl Focusable for QueryBuilderView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for QueryBuilderView {
    fn render(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        self.sync_condition_inputs(window, cx);
        self.sync_alias_inputs(window, cx);
        let s = Theme::scaled;
        let can_apply = self.preview.is_ok();
        let focus_handle = self.focus_handle.clone();
        Dialog::new("query-builder", |this: &mut Self| &mut this.frame)
            .title(crate::localization::text("query.builder.dialog_title"))
            .easy_close(true)
            .key_context("QueryBuilderView")
            .track_focus(&focus_handle)
            .on_close(|this, _window, cx| this.close(cx))
            .bare_body()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h(px(0.0))
                    .gap(px(s(8.0)))
                    .p(px(s(14.0)))
                    .font(Theme::ui_font())
                    .child(self.render_table_list(cx))
                    .child(self.render_canvas(cx))
                    .child(self.render_clauses(cx)),
            )
            .button(
                Button::new("qb-insert")
                    .label(crate::localization::text("query.builder.insert_button"))
                    .disabled(!can_apply)
                    .on_click(cx.listener(|this, _, _w, cx| this.emit_insert(cx))),
            )
            .button(
                Button::new("qb-run")
                    .primary()
                    .label(crate::localization::text("query.builder.run_button"))
                    .disabled(!can_apply)
                    .on_click(cx.listener(|this, _, _w, cx| this.emit_run(cx))),
            )
            .build(self, window, cx)
    }
}

/// The table list rows for `filter`, in [`list_tables`] order.
fn build_table_rows(catalog: &CompletionCatalog, filter: &str) -> Vec<TableRow> {
    list_tables(catalog, filter)
        .into_iter()
        .map(|obj| {
            let schema = obj.schema.clone().unwrap_or_default();
            let label = if schema.is_empty() {
                obj.name.clone()
            } else {
                format!("{schema}.{}", obj.name)
            };
            TableRow {
                schema,
                name: obj.name.clone(),
                label: label.into(),
            }
        })
        .collect()
}

/// The dim marker text for an inferred join, e.g. `Inferred (87%)`.
fn inferred_label(probability: f32) -> String {
    crate::localization::format(
        "query.builder.inferred_join_label",
        &[(
            "percent",
            &format!("{:.0}", (probability * 100.0).clamp(0.0, 100.0)),
        )],
    )
}

fn parse_limit(text: &str) -> Option<u64> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    t.parse().ok()
}

fn cycle_op(op: BuilderOp) -> BuilderOp {
    let i = ALL_OPS.iter().position(|o| *o == op).unwrap_or(0);
    ALL_OPS[(i + 1) % ALL_OPS.len()]
}

#[derive(Clone)]
struct CardPaint {
    x: f32,
    y: f32,
    h: f32,
}

#[derive(Clone)]
struct JoinEdgePaint {
    from: usize,
    to: usize,
    label: String,
    from_fk: bool,
}

#[derive(Clone)]
struct JoinPaint {
    cards: Vec<CardPaint>,
    edges: Vec<JoinEdgePaint>,
    background: Hsla,
    line: Hsla,
    font_size: f32,
}

fn rect_border_point(center: (f32, f32), half: (f32, f32), toward: (f32, f32)) -> (f32, f32) {
    let dx = toward.0 - center.0;
    let dy = toward.1 - center.1;
    if dx == 0.0 && dy == 0.0 {
        return center;
    }
    let sx = if dx != 0.0 {
        half.0 / dx.abs()
    } else {
        f32::INFINITY
    };
    let sy = if dy != 0.0 {
        half.1 / dy.abs()
    } else {
        f32::INFINITY
    };
    let s = sx.min(sy).min(1.0);
    (center.0 + dx * s, center.1 + dy * s)
}

fn paint_joins(p: &JoinPaint, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
    window.paint_quad(PaintQuad {
        bounds,
        background: p.background.into(),
        border_color: gpui::transparent_black(),
        border_widths: Default::default(),
        corner_radii: Default::default(),
        border_style: Default::default(),
    });
    let ox = f32::from(bounds.origin.x);
    let oy = f32::from(bounds.origin.y);
    let text_system = window.text_system().clone();
    let font = Theme::ui_font();
    let shape = |text: &str, color: Hsla, fs: f32| {
        let run = TextRun {
            len: text.len(),
            color,
            font: font.clone(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        text_system.shape_line(text.to_owned().into(), px(fs), &[run], None)
    };
    for edge in &p.edges {
        if edge.from == edge.to {
            continue;
        }
        let Some(a) = p.cards.get(edge.from) else {
            continue;
        };
        let Some(b) = p.cards.get(edge.to) else {
            continue;
        };
        let ha = (CARD_W / 2.0, a.h / 2.0);
        let hb = (CARD_W / 2.0, b.h / 2.0);
        let ca = (ox + a.x + ha.0, oy + a.y + ha.1);
        let cb = (ox + b.x + hb.0, oy + b.y + hb.1);
        let start = rect_border_point(ca, ha, cb);
        let end = rect_border_point(cb, hb, ca);
        if edge.from_fk {
            let mut stroke = gpui::PathBuilder::stroke(px(1.5));
            stroke.move_to(point(px(start.0), px(start.1)));
            stroke.line_to(point(px(end.0), px(end.1)));
            if let Ok(path) = stroke.build() {
                window.paint_path(path, p.line);
            }
        } else {
            paint_dashed(window, start, end, p.line, 6.0, 4.0);
        }
        let fs = p.font_size * 0.72;
        let line = shape(&edge.label, p.line, fs);
        let mid = ((start.0 + end.0) / 2.0, (start.1 + end.1) / 2.0);
        let _ = line.paint(
            Point {
                x: px(mid.0 - f32::from(line.width) / 2.0),
                y: px(mid.1 - fs * 1.2),
            },
            px(fs * 1.4),
            TextAlign::Left,
            None,
            window,
            cx,
        );
    }
}

fn paint_dashed(
    window: &mut Window,
    start: (f32, f32),
    end: (f32, f32),
    color: Hsla,
    dash: f32,
    gap: f32,
) {
    let dx = end.0 - start.0;
    let dy = end.1 - start.1;
    let len = (dx * dx + dy * dy).sqrt();
    if len < 1.0 {
        return;
    }
    let ux = dx / len;
    let uy = dy / len;
    let mut d = 0.0;
    while d < len {
        let d2 = (d + dash).min(len);
        let mut stroke = gpui::PathBuilder::stroke(px(1.5));
        stroke.move_to(point(px(start.0 + ux * d), px(start.1 + uy * d)));
        stroke.line_to(point(px(start.0 + ux * d2), px(start.1 + uy * d2)));
        if let Ok(path) = stroke.build() {
            window.paint_path(path, color);
        }
        d += dash + gap;
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use sqlly_ai::catalog::{Answer, InferredJoin, StubDecisionProvider};
    use sqlly_core::{ConnectionProfile, SecretProvider, SecretRef};
    use sqlly_intel::intellisense::{CompletionObject, FkRelation};
    use sqlly_services::Services;

    fn ensure_components_initialized(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            if cx.try_global::<gpui_component::Theme>().is_none() {
                gpui_component::init(cx);
            }
        });
    }

    fn test_context() -> ToolDialogContext {
        let secret = SecretRef {
            provider: SecretProvider::InMemory,
            key: "k".to_string(),
        };
        let profile = ConnectionProfile::sql_login("test", "host", "sa", secret);
        ToolDialogContext::new(profile, Services::new())
    }

    fn test_catalog() -> Arc<CompletionCatalog> {
        let customers =
            CompletionObject::table("dbo", "Customers", vec!["Id".into(), "Name".into()]);
        let mut orders =
            CompletionObject::table("dbo", "Orders", vec!["Id".into(), "CustomerId".into()]);
        orders.foreign_keys.push(FkRelation {
            column: "CustomerId".into(),
            target_schema: "dbo".into(),
            target_table: "Customers".into(),
            target_column: "Id".into(),
            constraint: None,
        });
        Arc::new(CompletionCatalog {
            objects: vec![customers, orders],
            ..Default::default()
        })
    }

    /// The same two tables as [`test_catalog`] but with no declared foreign
    /// key between them.
    fn no_fk_catalog() -> Arc<CompletionCatalog> {
        let customers =
            CompletionObject::table("dbo", "Customers", vec!["Id".into(), "Name".into()]);
        let orders =
            CompletionObject::table("dbo", "Orders", vec!["Id".into(), "CustomerId".into()]);
        Arc::new(CompletionCatalog {
            objects: vec![customers, orders],
            ..Default::default()
        })
    }

    fn open_view(
        cx: &mut gpui::TestAppContext,
    ) -> (gpui::Entity<QueryBuilderView>, &mut gpui::VisualTestContext) {
        open_view_with(test_catalog(), cx)
    }

    fn open_view_with(
        catalog: Arc<CompletionCatalog>,
        cx: &mut gpui::TestAppContext,
    ) -> (gpui::Entity<QueryBuilderView>, &mut gpui::VisualTestContext) {
        ensure_components_initialized(cx);
        cx.add_window_view(|window, cx| {
            QueryBuilderView::new(
                test_context(),
                catalog,
                DatabaseType::Postgres,
                SqlFormattingPreferences::default(),
                false,
                window,
                cx,
            )
        })
    }

    #[gpui::test]
    fn design_query_before_catalog_loads_adds_table_on_arrival(cx: &mut gpui::TestAppContext) {
        let (view, cx) = open_view_with(Arc::new(CompletionCatalog::default()), cx);
        view.update(cx, |v, cx| {
            v.set_catalog(Arc::new(CompletionCatalog::default()), true, cx);
            v.add_table_named("dbo", "Customers", cx);
            assert!(v.state.tables.is_empty());
            assert!(v.table_rows.is_empty());
            v.set_catalog(test_catalog(), true, cx);
            assert_eq!(v.state.tables.len(), 1);
            assert_eq!(v.state.tables[0].name, "Customers");
            assert!(v.pending_tables.is_empty());
            assert!(!v.table_rows.is_empty());
        });
    }

    #[gpui::test]
    fn finished_load_drops_missing_pending_table(cx: &mut gpui::TestAppContext) {
        let (view, cx) = open_view_with(Arc::new(CompletionCatalog::default()), cx);
        view.update(cx, |v, cx| {
            v.set_catalog(Arc::new(CompletionCatalog::default()), true, cx);
            v.add_table_named("dbo", "Nope", cx);
            assert_eq!(v.pending_tables.len(), 1);
            v.set_catalog(test_catalog(), false, cx);
            assert!(v.pending_tables.is_empty());
            assert!(v.state.tables.is_empty());
        });
    }

    #[gpui::test]
    fn table_rows_follow_filter(cx: &mut gpui::TestAppContext) {
        let (view, cx) = open_view(cx);
        view.update(cx, |v, _cx| {
            let all = v.table_rows.len();
            assert!(all >= 2);
            v.table_filter_text = "orders".into(); // case-insensitive
            v.rebuild_table_rows();
            assert_eq!(v.table_rows.len(), 1);
            assert_eq!(v.table_rows[0].name, "Orders");
        });
    }

    #[gpui::test]
    fn adding_two_fk_related_tables_suggests_a_join_and_previews_sql(
        cx: &mut gpui::TestAppContext,
    ) {
        let (view, cx) = open_view(cx);
        view.update(cx, |view, cx| {
            view.add_table_by_index(0, cx);
            view.add_table_by_index(1, cx);
            view.state.toggle_column(0, "Name");
            view.refresh_preview(cx);
            assert_eq!(view.state.joins.len(), 1);
            assert!(view.preview.is_ok(), "{:?}", view.preview);
        });
    }

    #[gpui::test]
    fn fk_joins_unchanged(cx: &mut gpui::TestAppContext) {
        let (view, cx) = open_view(cx);
        view.update(cx, |view, cx| {
            // Even with a decision provider present, declared-FK joins are
            // auto-applied exactly as before and no inference runs.
            view.decision = Some(Arc::new(StubDecisionProvider::with_answers(
                BTreeMap::from([("p0".to_string(), Answer::Noul { probability: 0.99 })]),
            )));
            view.add_table_named("dbo", "Customers", cx);
            view.add_table_named("dbo", "Orders", cx);
            assert_eq!(view.state.joins.len(), 1);
            let join = &view.state.joins[0];
            assert!(join.from_fk);
            assert_eq!(join.inferred, None);
            assert!(view.inferred_suggestions.is_empty());
        });
    }

    #[gpui::test]
    fn inferred_joins_never_auto_applied(cx: &mut gpui::TestAppContext) {
        crate::ensure_test_tokio_runtime();
        let (view, cx) = open_view_with(no_fk_catalog(), cx);
        view.update(cx, |view, cx| {
            // With no provider (the default), adding unrelated tables
            // behaves exactly as before this feature existed.
            view.decision = None;
            view.add_table_named("dbo", "Customers", cx);
            view.add_table_named("dbo", "Orders", cx);
            assert!(view.state.joins.is_empty());
            assert!(view.inferred_suggestions.is_empty());

            // Judge the same pair through the scripted stub: three candidate
            // pairs, only Orders.CustomerId → Customers.Id passes 0.7.
            let added = view.infer_table_cols(1).expect("orders");
            let other = view.infer_table_cols(0).expect("customers");
            let stub = StubDecisionProvider::with_answers(BTreeMap::from([
                ("p0".to_string(), Answer::Noul { probability: 0.4 }),
                ("p1".to_string(), Answer::Noul { probability: 0.87 }),
                ("p2".to_string(), Answer::Noul { probability: 0.1 }),
            ]));
            let found = crate::tokio_handle()
                .block_on(async { sqlly_ai::catalog::infer_joins(&stub, &added, &other).await });
            assert_eq!(found.len(), 1, "{found:?}");

            // Results land as pending suggestions, never in the query.
            let generation = view.infer_generation;
            let found: Vec<(usize, InferredJoin)> =
                found.into_iter().map(|join| (0, join)).collect();
            view.apply_inferred_suggestions(generation, 1, found, cx);
            assert!(
                view.state.joins.is_empty(),
                "inferred joins must never auto-apply"
            );
            assert_eq!(view.inferred_suggestions.len(), 1);
            let suggestion = &view.inferred_suggestions[0];
            assert!(!suggestion.from_fk);
            assert_eq!(suggestion.from_column, "CustomerId");
            assert_eq!(suggestion.to_column, "Id");
            assert_eq!(suggestion.inferred, Some(0.87));

            // A result from a stale table layout is dropped.
            let stale = vec![(
                0,
                InferredJoin {
                    a_col: "Id".to_string(),
                    b_col: "Id".to_string(),
                    probability: 0.9,
                },
            )];
            view.apply_inferred_suggestions(generation + 1, 1, stale, cx);
            assert_eq!(view.inferred_suggestions.len(), 1);

            // Only an explicit accept moves the suggestion into the query.
            view.accept_inferred_suggestion(0, cx);
            assert!(view.inferred_suggestions.is_empty());
            assert_eq!(view.state.joins.len(), 1);
            assert!(!view.state.joins[0].from_fk);
            assert_eq!(view.state.joins[0].inferred, Some(0.87));
        });
    }

    #[gpui::test]
    fn remove_table_clears_preview_to_no_tables_error(cx: &mut gpui::TestAppContext) {
        let (view, cx) = open_view(cx);
        view.update(cx, |view, cx| {
            view.add_table_by_index(0, cx);
            view.state.toggle_column(0, "Name");
            view.refresh_preview(cx);
            assert!(view.preview.is_ok());
            view.state.remove_table(0);
            view.refresh_preview(cx);
            assert_eq!(view.preview, Err(BuildError::NoTables));
        });
    }
}
