//! Search a connected database for a literal value across tables.

use gpui::{
    Entity, EventEmitter, FocusHandle, IntoElement, ParentElement, Render, Styled, Subscription,
    Window, div, prelude::*, px, relative,
};
use gpui_component::Disableable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::select::{Select, SelectEvent, SelectState};
use sqlly_core::{CellValue, ResultColumn};
use sqlly_lang::sqlmodel::DatabaseType;
use sqlly_services::{
    MatchMode, ScriptCancel, SearchHit, SearchKind, SearchOptions, SearchProgress, SearchSummary,
    build_table_sql, run_search,
};
use tokio::sync::mpsc;

use crate::app::StatusLevel;
use crate::dialog::{Dialog, DialogFrame};
use crate::result_export::cell_display;
use crate::table_actions::{QualifiedTable, view_data_sql_for};
use crate::theme::Theme;
use crate::tool_dialog::{ToolDialogContext, ToolDialogEvent, ToolQuerySet, build_result_grid};
use crate::where_actions::{
    WhereSelectedValue, apply_where_action_for, build_equals_action_for, classify_value,
};

const MAX_TABLES_MIN: usize = 1;
const MAX_TABLES_MAX: usize = 5000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Page {
    Setup,
    Running,
    Results,
}

/// Form state for validation and search options.
#[derive(Clone, Debug)]
pub struct DatabaseSearchModel {
    pub value: String,
    pub kind: SearchKind,
    pub match_mode: MatchMode,
    pub case_insensitive: bool,
    pub max_tables: usize,
    pub schema_filter: String,
}

impl Default for DatabaseSearchModel {
    fn default() -> Self {
        Self {
            value: String::new(),
            kind: SearchKind::Text,
            match_mode: MatchMode::Exact,
            case_insensitive: true,
            max_tables: 200,
            schema_filter: String::new(),
        }
    }
}

impl DatabaseSearchModel {
    pub fn validate(&self) -> Result<(), String> {
        if self.value.trim().is_empty() {
            return Err(crate::localization::text(
                "explorer.database_search.value_required",
            ));
        }
        if self.kind == SearchKind::Number && self.value.trim().parse::<f64>().is_err() {
            return Err(crate::localization::text(
                "explorer.database_search.number_required",
            ));
        }
        if self.max_tables < MAX_TABLES_MIN || self.max_tables > MAX_TABLES_MAX {
            return Err(crate::localization::format(
                "explorer.database_search.table_limit_invalid",
                &[
                    ("minimum", &MAX_TABLES_MIN.to_string()),
                    ("maximum", &MAX_TABLES_MAX.to_string()),
                ],
            ));
        }
        Ok(())
    }

    pub fn to_options(&self) -> SearchOptions {
        SearchOptions {
            value: self.value.clone(),
            kind: self.kind,
            match_mode: self.match_mode,
            case_insensitive: self.case_insensitive,
            max_tables: self.max_tables,
            rows_per_table: 10,
            schema_filter: if self.schema_filter.trim().is_empty() {
                None
            } else {
                Some(self.schema_filter.trim().to_string())
            },
        }
    }
}

impl EventEmitter<ToolDialogEvent> for DatabaseSearchDialog {}

pub struct DatabaseSearchDialog {
    context: ToolDialogContext,
    page: Page,
    model: DatabaseSearchModel,
    value_input: Entity<InputState>,
    max_tables_input: Entity<InputState>,
    schema_input: Entity<InputState>,
    kind_select: Entity<SelectState<Vec<String>>>,
    match_select: Entity<SelectState<Vec<String>>>,
    progress: Option<SearchProgress>,
    summary: Option<SearchSummary>,
    grid: Option<Entity<sqlly_datatable::SqllyDataTable>>,
    cancel: Option<ScriptCancel>,
    error: Option<String>,
    focus_handle: FocusHandle,
    frame: DialogFrame,
    inputs_dirty: bool,
    _subscriptions: Vec<Subscription>,
    _tasks: Vec<gpui::Task<()>>,
}

/// Kinds in the order of the kind select's labels.
const KIND_CHOICES: [SearchKind; 2] = [SearchKind::Text, SearchKind::Number];

fn kind_from_row(row: Option<usize>) -> SearchKind {
    row.and_then(|row| KIND_CHOICES.get(row).copied())
        .unwrap_or(SearchKind::Text)
}

/// Match modes in the order of the match select's labels.
const MATCH_CHOICES: [MatchMode; 2] = [MatchMode::Exact, MatchMode::Contains];

fn match_mode_from_row(row: Option<usize>) -> MatchMode {
    row.and_then(|row| MATCH_CHOICES.get(row).copied())
        .unwrap_or(MatchMode::Exact)
}

impl DatabaseSearchDialog {
    pub fn new(
        context: ToolDialogContext,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        let kind_select = cx.new(|cx| {
            SelectState::new(
                crate::localization::select_labels(window, cx, || {
                    vec![
                        crate::localization::text("explorer.database_search.kind_text"),
                        crate::localization::text("explorer.database_search.kind_number"),
                    ]
                }),
                Some(gpui_component::IndexPath::new(0)),
                window,
                cx,
            )
        });
        let match_select = cx.new(|cx| {
            SelectState::new(
                crate::localization::select_labels(window, cx, || {
                    vec![
                        crate::localization::text("explorer.database_search.match_exact"),
                        crate::localization::text("explorer.database_search.match_contains"),
                    ]
                }),
                Some(gpui_component::IndexPath::new(0)),
                window,
                cx,
            )
        });
        let value_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "explorer.database_search.value_placeholder",
            ))
        });
        let max_tables_input = cx.new(|cx| InputState::new(window, cx).placeholder("200"));
        let schema_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "explorer.database_search.schema_placeholder",
            ))
        });

        let subscriptions = vec![
            cx.subscribe_in(
                &value_input,
                window,
                |this: &mut Self, state, event: &InputEvent, _window, cx| {
                    if let InputEvent::Change = event {
                        this.model.value = state.read(cx).value().to_string();
                        this.error = None;
                        cx.notify();
                    }
                },
            ),
            cx.subscribe_in(
                &max_tables_input,
                window,
                |this: &mut Self, state, event: &InputEvent, _window, cx| {
                    if let InputEvent::Change = event {
                        let text = state.read(cx).value();
                        this.model.max_tables =
                            text.trim().parse().unwrap_or(this.model.max_tables);
                        cx.notify();
                    }
                },
            ),
            cx.subscribe_in(
                &schema_input,
                window,
                |this: &mut Self, state, event: &InputEvent, _window, cx| {
                    if let InputEvent::Change = event {
                        this.model.schema_filter = state.read(cx).value().to_string();
                        cx.notify();
                    }
                },
            ),
            cx.subscribe_in(
                &kind_select,
                window,
                |this: &mut Self, select, event: &SelectEvent<Vec<String>>, _window, cx| {
                    let SelectEvent::Confirm(value) = event;
                    if value.is_some() {
                        this.model.kind =
                            kind_from_row(select.read(cx).selected_index(cx).map(|ix| ix.row));
                        cx.notify();
                    }
                },
            ),
            cx.subscribe_in(
                &match_select,
                window,
                |this: &mut Self, select, event: &SelectEvent<Vec<String>>, _window, cx| {
                    let SelectEvent::Confirm(value) = event;
                    if value.is_some() {
                        this.model.match_mode = match_mode_from_row(
                            select.read(cx).selected_index(cx).map(|ix| ix.row),
                        );
                        cx.notify();
                    }
                },
            ),
        ];

        Self {
            context,
            page: Page::Setup,
            model: DatabaseSearchModel::default(),
            value_input,
            max_tables_input,
            schema_input,
            kind_select,
            match_select,
            progress: None,
            summary: None,
            grid: None,
            cancel: None,
            error: None,
            focus_handle: cx.focus_handle(),
            // Fixed frame: the results page embeds a data grid, which must
            // not sit inside the chrome's scrollable auto-height body.
            frame: DialogFrame::new(Theme::scaled(640.0), Theme::scaled(640.0)),
            inputs_dirty: true,
            _subscriptions: subscriptions,
            _tasks: Vec::new(),
        }
    }

    fn connection_line(&self) -> String {
        let p = &self.context.profile;
        format!("{} — {}", p.name, p.server)
    }

    fn close(&mut self, cx: &mut gpui::Context<Self>) {
        if let Some(cancel) = &self.cancel {
            cancel.cancel();
        }
        cx.emit(ToolDialogEvent::Close);
    }

    fn sync_inputs(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        if !self.inputs_dirty {
            return;
        }
        self.value_input.update(cx, |state, cx| {
            state.set_value(self.model.value.clone(), window, cx);
        });
        self.max_tables_input.update(cx, |state, cx| {
            state.set_value(self.model.max_tables.to_string(), window, cx);
        });
        self.schema_input.update(cx, |state, cx| {
            state.set_value(self.model.schema_filter.clone(), window, cx);
        });
        self.inputs_dirty = false;
    }

    fn start_search(&mut self, cx: &mut gpui::Context<Self>) {
        if let Err(reason) = self.model.validate() {
            self.error = Some(reason);
            cx.notify();
            return;
        }
        self.error = None;
        self.page = Page::Running;
        self.progress = None;
        self.summary = None;
        self.grid = None;

        let cancel = ScriptCancel::new();
        self.cancel = Some(cancel.clone());

        let profile = self.context.profile.clone();
        let services = self.context.services.clone();
        let database = profile
            .database
            .clone()
            .unwrap_or_else(|| "master".to_string());
        let options = self.model.to_options();
        let (progress_tx, mut progress_rx) = mpsc::channel::<SearchProgress>(32);

        let weak = cx.weak_entity();
        let async_cx = cx.to_async();
        let weak_progress = weak.clone();
        let async_cx_progress = async_cx.clone();

        let progress_task = cx.spawn(async move |_this, _cx| {
            while let Some(progress) = progress_rx.recv().await {
                let mut cx = async_cx_progress.clone();
                weak_progress
                    .update(&mut cx, |this, cx| {
                        this.progress = Some(progress);
                        cx.notify();
                    })
                    .ok();
            }
        });

        let weak_run = weak.clone();
        let async_cx_run = async_cx.clone();
        cx.spawn(async move |_this, _cx| {
            let result = crate::tokio_handle()
                .spawn(async move {
                    run_search(
                        &services,
                        &profile,
                        &database,
                        &options,
                        progress_tx,
                        cancel,
                    )
                    .await
                })
                .await;

            let mut cx = async_cx_run.clone();
            weak_run
                .update(&mut cx, |this, cx| match result {
                    Ok(Ok(summary)) => this.finish(summary, cx),
                    Ok(Err(err)) => {
                        this.page = Page::Setup;
                        this.error = Some(err.to_string());
                        cx.notify();
                    }
                    Err(err) => {
                        this.page = Page::Setup;
                        this.error = Some(err.to_string());
                        cx.notify();
                    }
                })
                .ok();
        })
        .detach();

        self._tasks.push(progress_task);
        cx.notify();
    }

    fn finish(&mut self, summary: SearchSummary, cx: &mut gpui::Context<Self>) {
        self.page = Page::Results;
        self.summary = Some(summary.clone());
        self.progress = None;
        self.cancel = None;

        let columns = vec![
            ResultColumn {
                name: crate::localization::text("explorer.database_search.column_schema"),
                data_type: "nvarchar".into(),
                nullable: false,
            },
            ResultColumn {
                name: crate::localization::text("explorer.database_search.column_table"),
                data_type: "nvarchar".into(),
                nullable: false,
            },
            ResultColumn {
                name: crate::localization::text("explorer.database_search.column_column"),
                data_type: "nvarchar".into(),
                nullable: false,
            },
            ResultColumn {
                name: crate::localization::text("explorer.database_search.column_match"),
                data_type: "nvarchar".into(),
                nullable: false,
            },
            ResultColumn {
                name: crate::localization::text("explorer.database_search.column_preview"),
                data_type: "nvarchar".into(),
                nullable: false,
            },
        ];
        let mut rows = Vec::new();
        for hit in &summary.hits {
            let match_text = hit
                .columns
                .iter()
                .position(|c| c.name == hit.column)
                .and_then(|idx| hit.row.get(idx))
                .map(cell_display)
                .unwrap_or_default();
            rows.push(vec![
                CellValue::String(hit.schema.clone()),
                CellValue::String(hit.table.clone()),
                CellValue::String(hit.column.clone()),
                CellValue::String(match_text),
                CellValue::String(row_preview(&hit.columns, &hit.row)),
            ]);
        }
        let set = ToolQuerySet { columns, rows };
        self.grid = build_result_grid(&set, cx);
        cx.notify();
    }

    fn selected_hit(&self, cx: &gpui::App) -> Option<SearchHit> {
        let summary = self.summary.as_ref()?;
        let grid = self.grid.as_ref()?;
        let row = crate::tool_dialog::selected_source_row(grid, cx)?;
        summary.hits.get(row).cloned()
    }

    fn open_selected_row(&mut self, cx: &mut gpui::Context<Self>) {
        let Some(hit) = self.selected_hit(cx) else {
            return;
        };
        let db = self.context.profile.database_type;
        let Some(sql) = open_row_sql_for_hit(db, &hit) else {
            return;
        };
        cx.emit(ToolDialogEvent::ScriptToEditorAndRun { sql });
    }

    fn copy_selected_sql(&mut self, cx: &mut gpui::Context<Self>) {
        let Some(hit) = self.selected_hit(cx) else {
            return;
        };
        let db = self.context.profile.database_type;
        let target = sqlly_services::SearchTarget {
            schema: hit.schema.clone(),
            table: hit.table.clone(),
            columns: vec![hit.column.clone()],
        };
        let sql = build_table_sql(db, &target, &self.model.to_options());
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(sql));
        cx.emit(ToolDialogEvent::Status {
            level: StatusLevel::Info,
            message: crate::localization::text("explorer.database_search.sql_copied"),
        });
    }

    fn reset_to_setup(&mut self, cx: &mut gpui::Context<Self>) {
        self.page = Page::Setup;
        self.progress = None;
        self.summary = None;
        self.grid = None;
        self.cancel = None;
        self.inputs_dirty = true;
        cx.notify();
    }
}

fn row_preview(columns: &[ResultColumn], row: &[CellValue]) -> String {
    let mut parts = Vec::new();
    for (column, value) in columns.iter().zip(row.iter()) {
        parts.push(format!("{}={}", column.name, cell_display(value)));
    }
    let joined = parts.join(", ");
    if joined.len() > 200 {
        format!("{}…", &joined[..200])
    } else {
        joined
    }
}

/// Build the filtered SELECT used by Open Row (unit-tested).
pub fn open_row_sql_for_hit(db: DatabaseType, hit: &SearchHit) -> Option<String> {
    let table = QualifiedTable::new(&hit.table).with_schema(&hit.schema);
    let base = view_data_sql_for(db, &table);
    let col_idx = hit.columns.iter().position(|c| c.name == hit.column)?;
    let cell = hit.row.get(col_idx)?;
    let column = hit.columns.get(col_idx)?;
    let display = cell_display(cell);
    let kind = classify_value(db, cell, Some(column));
    let action = build_equals_action_for(
        db,
        &hit.column,
        col_idx,
        vec![WhereSelectedValue {
            cell_value: cell.clone(),
            display_text: display,
            kind,
            placeholder: false,
        }],
    )?;
    Some(apply_where_action_for(db, &base, &action).updated_sql)
}

impl gpui::Focusable for DatabaseSearchDialog {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for DatabaseSearchDialog {
    fn render(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        self.sync_inputs(window, cx);

        let body = match self.page {
            Page::Setup => self.render_setup(s, cx).into_any_element(),
            Page::Running => self.render_running(s, cx).into_any_element(),
            Page::Results => self.render_results(s, cx).into_any_element(),
        };

        // Phase-dependent action cluster; the chrome's default Close button
        // replaces the old per-phase Close buttons.
        let mut buttons: Vec<gpui::AnyElement> = Vec::new();
        match self.page {
            Page::Setup => {
                buttons.push(
                    Button::new("dbs-search")
                        .primary()
                        .label(crate::localization::text(
                            "explorer.database_search.search_button",
                        ))
                        .on_click(cx.listener(|this, _, _window, cx| this.start_search(cx)))
                        .into_any_element(),
                );
            }
            Page::Running => {
                buttons.push(
                    Button::new("dbs-cancel-run")
                        .label(crate::localization::text(
                            "explorer.database_search.cancel_button",
                        ))
                        .on_click(cx.listener(|this, _, _window, cx| {
                            if let Some(cancel) = &this.cancel {
                                cancel.cancel();
                            }
                            cx.notify();
                        }))
                        .into_any_element(),
                );
            }
            Page::Results => {
                let has_selection = self.selected_hit(cx).is_some();
                buttons.push(
                    Button::new("dbs-open-row")
                        .label(crate::localization::text(
                            "explorer.database_search.open_row_button",
                        ))
                        .disabled(!has_selection)
                        .on_click(cx.listener(|this, _, _window, cx| this.open_selected_row(cx)))
                        .into_any_element(),
                );
                buttons.push(
                    Button::new("dbs-copy-sql")
                        .label(crate::localization::text(
                            "explorer.database_search.copy_sql_button",
                        ))
                        .disabled(!has_selection)
                        .on_click(cx.listener(|this, _, _window, cx| this.copy_selected_sql(cx)))
                        .into_any_element(),
                );
                buttons.push(
                    Button::new("dbs-new-search")
                        .label(crate::localization::text(
                            "explorer.database_search.new_search_button",
                        ))
                        .on_click(cx.listener(|this, _, _window, cx| this.reset_to_setup(cx)))
                        .into_any_element(),
                );
            }
        }

        let focus_handle = self.focus_handle.clone();
        let mut dialog = Dialog::new("database-search-dialog", |this: &mut Self| &mut this.frame)
            .title(crate::localization::text("explorer.database_search.title"))
            .key_context("DatabaseSearchDialog")
            .track_focus(&focus_handle)
            .dismiss_on_scrim_click(false)
            // Results embeds a data grid; the body lays out edge-to-edge and
            // each phase carries its own padding.
            .bare_body()
            .on_close(|this, _window, cx| this.close(cx))
            .child(
                div()
                    .flex_shrink_0()
                    .px(px(s(16.0)))
                    .pt(px(s(12.0)))
                    .text_size(px(Theme::font_size_ratio(0.80)))
                    .text_color(Theme::text_dim())
                    .child(self.connection_line()),
            )
            .child(body);
        for button in buttons {
            dialog = dialog.button(button);
        }
        dialog.build(self, window, cx)
    }
}

impl DatabaseSearchDialog {
    fn field(
        label: &'static str,
        control: impl IntoElement,
        s: fn(f32) -> f32,
    ) -> impl IntoElement {
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(s(8.0)))
            .child(
                div()
                    .w(px(s(110.0)))
                    .flex_shrink_0()
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text(label)),
            )
            .child(div().flex_1().min_w_0().child(control))
    }

    fn render_setup(
        &mut self,
        s: fn(f32) -> f32,
        cx: &mut gpui::Context<Self>,
    ) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap(px(s(10.0)))
            .px(px(s(16.0)))
            .py(px(s(12.0)))
            .child(Self::field(
                "explorer.database_search.value_label",
                Input::new(&self.value_input),
                s,
            ))
            .when_some(self.error.clone(), |el, message| {
                el.child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.80)))
                        .text_color(Theme::error())
                        .child(message),
                )
            })
            .child(Self::field(
                "explorer.database_search.kind_label",
                Select::new(&self.kind_select),
                s,
            ))
            .child(Self::field(
                "explorer.database_search.match_label",
                Select::new(&self.match_select),
                s,
            ))
            .child(
                Checkbox::new("case-insensitive")
                    .label(crate::localization::text(
                        "explorer.database_search.case_insensitive",
                    ))
                    .checked(self.model.case_insensitive)
                    .on_click(cx.listener(|this, checked: &bool, _window, cx| {
                        this.model.case_insensitive = *checked;
                        cx.notify();
                    })),
            )
            .child(Self::field(
                "explorer.database_search.table_limit_label",
                Input::new(&self.max_tables_input),
                s,
            ))
            .child(Self::field(
                "explorer.database_search.schema_filter_label",
                Input::new(&self.schema_input),
                s,
            ))
    }

    fn render_running(
        &mut self,
        s: fn(f32) -> f32,
        _cx: &mut gpui::Context<Self>,
    ) -> impl IntoElement {
        let progress = self.progress.clone();
        let pct = progress
            .as_ref()
            .map(|p| {
                if p.tables_total == 0 {
                    0.0
                } else {
                    p.tables_done as f32 / p.tables_total as f32
                }
            })
            .unwrap_or(0.0);
        let hits = progress.as_ref().map(|p| p.hits).unwrap_or(0);
        let current = progress
            .as_ref()
            .and_then(|p| p.current_table.clone())
            .unwrap_or_else(|| "…".into());

        div()
            .flex()
            .flex_col()
            .gap(px(s(10.0)))
            .px(px(s(16.0)))
            .py(px(s(12.0)))
            .child(
                div()
                    .h(px(s(4.0)))
                    .bg(Theme::border())
                    .rounded(px(s(2.0)))
                    .child(
                        div()
                            .h_full()
                            .w(relative(pct))
                            .bg(Theme::accent())
                            .rounded(px(s(2.0))),
                    ),
            )
            .child(crate::localization::format(
                "explorer.database_search.running_status",
                &[("table", &current), ("count", &hits.to_string())],
            ))
    }

    fn render_results(
        &mut self,
        s: fn(f32) -> f32,
        _cx: &mut gpui::Context<Self>,
    ) -> impl IntoElement {
        let status = if let Some(summary) = self.summary.as_ref() {
            let skipped = if summary.tables_skipped > 0 {
                crate::localization::format(
                    "explorer.database_search.skipped_suffix",
                    &[("count", &summary.tables_skipped.to_string())],
                )
            } else {
                String::new()
            };
            let errors = if summary.errors.is_empty() {
                String::new()
            } else {
                crate::localization::format(
                    "explorer.database_search.errors_suffix",
                    &[("count", &summary.errors.len().to_string())],
                )
            };
            let cancelled = if summary.cancelled {
                crate::localization::text("explorer.database_search.cancelled_suffix")
            } else {
                String::new()
            };
            crate::localization::format(
                "explorer.database_search.results_status",
                &[
                    ("hits", &summary.hits.len().to_string()),
                    ("tables", &summary.tables_searched.to_string()),
                    ("skipped", &skipped),
                    ("errors", &errors),
                    ("cancelled", &cancelled),
                ],
            )
        } else {
            crate::localization::text("explorer.database_search.no_results")
        };

        let grid = self.grid.clone();

        div()
            .flex()
            .flex_col()
            .gap(px(s(10.0)))
            .flex_1()
            .min_h_0()
            .px(px(s(16.0)))
            .py(px(s(12.0)))
            .child(status)
            .children(grid.map(|g| div().flex_1().min_h(px(s(240.0))).min_w_0().child(g)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::CellValue;

    #[test]
    fn kind_and_match_dispatch_on_row() {
        assert_eq!(kind_from_row(Some(0)), SearchKind::Text);
        assert_eq!(kind_from_row(Some(1)), SearchKind::Number);
        assert_eq!(kind_from_row(None), SearchKind::Text);
        assert_eq!(match_mode_from_row(Some(0)), MatchMode::Exact);
        assert_eq!(match_mode_from_row(Some(1)), MatchMode::Contains);
        assert_eq!(match_mode_from_row(None), MatchMode::Exact);
    }

    #[test]
    fn validate_rejects_empty_value_and_non_numeric_number_search() {
        let mut model = DatabaseSearchModel::default();
        assert!(model.validate().is_err());
        model.value = "abc".into();
        model.kind = SearchKind::Number;
        assert!(model.validate().is_err());
        model.value = "42".into();
        assert!(model.validate().is_ok());
    }

    #[test]
    fn validate_rejects_bad_table_limit() {
        let mut model = DatabaseSearchModel {
            value: "x".into(),
            max_tables: 0,
            ..DatabaseSearchModel::default()
        };
        assert!(model.validate().is_err());
        model.max_tables = 5001;
        assert!(model.validate().is_err());
        model.max_tables = 200;
        assert!(model.validate().is_ok());
    }

    #[test]
    fn to_options_maps_fields() {
        let model = DatabaseSearchModel {
            value: "needle".into(),
            kind: SearchKind::Number,
            match_mode: MatchMode::Contains,
            case_insensitive: false,
            max_tables: 50,
            schema_filter: "sales".into(),
        };
        let opts = model.to_options();
        assert_eq!(opts.value, "needle");
        assert_eq!(opts.kind, SearchKind::Number);
        assert_eq!(opts.match_mode, MatchMode::Contains);
        assert!(!opts.case_insensitive);
        assert_eq!(opts.max_tables, 50);
        assert_eq!(opts.schema_filter.as_deref(), Some("sales"));
        assert_eq!(opts.rows_per_table, 10);
    }

    #[test]
    fn open_row_sql_filters_on_matched_column() {
        let hit = SearchHit {
            schema: "dbo".into(),
            table: "Users".into(),
            column: "email".into(),
            columns: vec![ResultColumn {
                name: "email".into(),
                data_type: "nvarchar".into(),
                nullable: true,
            }],
            row: vec![CellValue::String("user@example.com".into())],
        };
        let sql = open_row_sql_for_hit(DatabaseType::SqlServer, &hit).unwrap();
        assert!(sql.contains("WHERE"));
        assert!(sql.contains("[email]"));
    }
}
