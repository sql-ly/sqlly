//! Object Browser pane — a sortable inventory of tables, views, routines, and
//! triggers with a side detail column and review-first batch scripts.
//!
//! Read-only against the server: refresh loads catalog rows, batch actions
//! emit SQL into an editor tab, and nothing here executes a write.

#[cfg(target_arch = "wasm32")]
use crate::wasm_tokio as tokio;
use std::collections::HashMap;

use gpui::{
    ClipboardItem, Entity, EventEmitter, FocusHandle, IntoElement, ParentElement, PromptButton,
    PromptLevel, Render, Styled, Subscription, Window, div, prelude::*, px,
};
use gpui_component::Disableable as _;
use gpui_component::button::Button;
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputEvent, InputState};
use sqlly_core::{CellValue, ResultColumn};
use sqlly_datatable::{Selection, SqllyDataTable};
use sqlly_intel::intellisense::{ClickedRole, NavigationTarget, RoutineKind};
use sqlly_intel::metadata::{MetadataQuery, sql_for_engine};
use sqlly_services::fetch_ddl_document_with;

use crate::app::StatusLevel;
use crate::dialog::{Dialog, DialogFrame};
use crate::object_browser_model::{
    BatchAction, InventoryRow, batch_script, copy_names_text, filter_rows, grid_set,
    rows_from_outcome,
};
use crate::table_actions::ColumnDescriptor;
use crate::theme::Theme;
use crate::tool_dialog::{
    TOOL_QUERY_ROW_CAP, ToolDialogContext, ToolDialogEvent, ToolQueryOutcome, ToolQuerySet,
    build_result_grid, execute_collect, run_query,
};

const KIND_OPTIONS: [&str; 5] = ["TABLE", "VIEW", "PROCEDURE", "FUNCTION", "TRIGGER"];
const SCRIPT_CREATE_TABLE_CAP: usize = 50;

pub fn truncation_note(rows_len: usize) -> Option<String> {
    (rows_len >= TOOL_QUERY_ROW_CAP)
        .then(|| crate::localization::text("explorer.object_browser.truncation_note"))
}

#[derive(Default)]
struct DetailState {
    columns: Option<Entity<SqllyDataTable>>,
    keys: Option<Entity<SqllyDataTable>>,
    ddl: Option<Entity<crate::ddl_viewer::DdlViewerView>>,
}

impl EventEmitter<ToolDialogEvent> for ObjectBrowserPane {}

pub struct ObjectBrowserPane {
    context: ToolDialogContext,
    database: String,
    schema: Option<String>,
    rows: Vec<InventoryRow>,
    filtered: Vec<InventoryRow>,
    filter_input: Entity<InputState>,
    filter: String,
    kind_filter: Vec<String>,
    grid: Option<Entity<SqllyDataTable>>,
    selected: Vec<usize>,
    detail: DetailState,
    detail_key: Option<(String, String, String)>,
    detail_generation: u64,
    error: Option<String>,
    loading: bool,
    refresh_generation: u64,
    refresh_in_flight: bool,
    truncated: bool,
    focus_handle: FocusHandle,
    frame: DialogFrame,
    _filter_sub: Subscription,
    _grid_obs: Option<Subscription>,
    _ddl_sub: Option<Subscription>,
}

impl ObjectBrowserPane {
    pub fn new(
        context: ToolDialogContext,
        database: String,
        schema: Option<String>,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        let filter_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "explorer.object_browser.filter_placeholder",
            ))
        });
        let filter_sub = cx.subscribe_in(
            &filter_input,
            window,
            |this: &mut Self, state, event: &InputEvent, _window, cx| {
                if let InputEvent::Change = event {
                    this.filter = state.read(cx).value().to_string();
                    this.rebuild_grid(cx);
                }
            },
        );
        let mut this = Self {
            context,
            database,
            schema,
            rows: Vec::new(),
            filtered: Vec::new(),
            filter_input,
            filter: String::new(),
            kind_filter: KIND_OPTIONS.iter().map(|k| (*k).to_string()).collect(),
            grid: None,
            selected: Vec::new(),
            detail: DetailState::default(),
            detail_key: None,
            detail_generation: 0,
            error: None,
            loading: true,
            refresh_generation: 0,
            refresh_in_flight: false,
            truncated: false,
            focus_handle: cx.focus_handle(),
            // Fixed frame: the inventory grid and detail column manage their
            // own scrolling.
            frame: DialogFrame::new(Theme::scaled(1120.0), Theme::scaled(640.0)),
            _filter_sub: filter_sub,
            _grid_obs: None,
            _ddl_sub: None,
        };
        this.refresh(cx);
        this
    }

    fn refresh(&mut self, cx: &mut gpui::Context<Self>) {
        if self.refresh_in_flight {
            return;
        }
        let db = self.context.profile.database_type;
        let query = MetadataQuery::ObjectInventory {
            schema: self.schema.clone(),
        };
        let sql = match sql_for_engine(&query, db) {
            Ok(sql) => sql,
            Err(_) => {
                self.loading = false;
                self.refresh_in_flight = false;
                self.error = Some(crate::localization::format(
                    "explorer.object_browser.unsupported_engine",
                    &[("engine", &db.display_name())],
                ));
                self.grid = None;
                cx.notify();
                return;
            }
        };
        self.refresh_in_flight = true;
        self.refresh_generation += 1;
        self.loading = true;
        self.error = None;
        let generation = self.refresh_generation;
        run_query(&self.context, sql, cx, move |this, outcome, cx| {
            if this.refresh_generation != generation {
                return;
            }
            this.refresh_in_flight = false;
            this.loading = false;
            if let Some(err) = outcome.error {
                this.error = Some(err);
                this.grid = None;
                this.rows.clear();
                this.filtered.clear();
                this.truncated = false;
            } else {
                this.error = None;
                this.rows = rows_from_outcome(&outcome);
                this.truncated = this.rows.len() >= TOOL_QUERY_ROW_CAP;
                this.rebuild_grid(cx);
            }
            cx.notify();
        });
        cx.notify();
    }

    fn rebuild_grid(&mut self, cx: &mut gpui::Context<Self>) {
        let filtered = filter_rows(&self.rows, &self.filter, &self.kind_filter);
        self.filtered = filtered.into_iter().cloned().collect();
        let refs: Vec<&InventoryRow> = self.filtered.iter().collect();
        let set = grid_set(&refs);
        self.grid = build_result_grid(&set, cx);
        self.selected.clear();
        self.observe_grid(cx);
        cx.notify();
    }

    fn observe_grid(&mut self, cx: &mut gpui::Context<Self>) {
        let Some(grid) = self.grid.clone() else {
            self._grid_obs = None;
            return;
        };
        let state = grid.read(cx).state.clone();
        self._grid_obs = Some(cx.observe(&state, |this, _state, cx| {
            this.sync_selection(cx);
        }));
    }

    fn sync_selection(&mut self, cx: &mut gpui::Context<Self>) {
        self.selected = self.read_selected_indices(cx);
        if let Some(&last) = self.selected.last() {
            self.load_detail(last, cx);
        }
        cx.notify();
    }

    fn read_selected_indices(&self, cx: &gpui::App) -> Vec<usize> {
        let Some(grid) = &self.grid else {
            return Vec::new();
        };
        selected_row_indices(&grid.read(cx).state.read(cx).selection)
            .into_iter()
            .filter(|index| *index < self.filtered.len())
            .collect()
    }

    fn selected_rows(&self) -> Vec<&InventoryRow> {
        self.selected
            .iter()
            .filter_map(|index| self.filtered.get(*index))
            .collect()
    }

    fn toggle_kind(&mut self, kind: &str, checked: bool, cx: &mut gpui::Context<Self>) {
        if checked {
            if !self
                .kind_filter
                .iter()
                .any(|existing| existing.eq_ignore_ascii_case(kind))
            {
                self.kind_filter.push(kind.to_string());
            }
        } else {
            self.kind_filter
                .retain(|existing| !existing.eq_ignore_ascii_case(kind));
        }
        self.rebuild_grid(cx);
    }

    fn load_detail(&mut self, index: usize, cx: &mut gpui::Context<Self>) {
        let Some(row) = self.filtered.get(index).cloned() else {
            return;
        };
        let key = (row.kind.clone(), row.schema.clone(), row.name.clone());
        if self.detail_key.as_ref() == Some(&key) {
            return;
        }
        self.detail_key = Some(key);
        self.detail_generation += 1;
        let generation = self.detail_generation;
        self.detail = DetailState::default();
        self.load_columns(&row, generation, cx);
        self.load_keys(&row, generation, cx);
        self.load_ddl(&row, generation, cx);
    }

    fn load_columns(&mut self, row: &InventoryRow, generation: u64, cx: &mut gpui::Context<Self>) {
        let db = self.context.profile.database_type;
        let Ok(sql) = sql_for_engine(
            &MetadataQuery::Columns {
                schema: row.schema.clone(),
                object: row.name.clone(),
            },
            db,
        ) else {
            return;
        };
        run_query(&self.context, sql, cx, move |this, outcome, cx| {
            if this.detail_generation != generation {
                return;
            }
            if outcome.error.is_none() {
                let set = outcome.sets.into_iter().next().unwrap_or_default();
                this.detail.columns = build_result_grid(&set, cx);
            }
            cx.notify();
        });
    }

    fn load_keys(&mut self, row: &InventoryRow, generation: u64, cx: &mut gpui::Context<Self>) {
        let db = self.context.profile.database_type;
        let schema = row.schema.clone();
        let name = row.name.clone();
        let Ok(pk_sql) = sql_for_engine(&MetadataQuery::AllPrimaryKeys, db) else {
            return;
        };
        let Ok(fk_sql) = sql_for_engine(&MetadataQuery::AllForeignKeys, db) else {
            return;
        };
        run_query(&self.context, pk_sql, cx, {
            let schema = schema.clone();
            let name = name.clone();
            move |this, pk_outcome, cx| {
                if this.detail_generation != generation {
                    return;
                }
                run_query(&this.context, fk_sql, cx, move |this, fk_outcome, cx| {
                    if this.detail_generation != generation {
                        return;
                    }
                    this.detail.keys = build_result_grid(
                        &keys_set_for(&schema, &name, &pk_outcome, &fk_outcome),
                        cx,
                    );
                    cx.notify();
                });
            }
        });
    }

    fn load_ddl(&mut self, row: &InventoryRow, generation: u64, cx: &mut gpui::Context<Self>) {
        let Some(target) = navigation_target(row, &self.database) else {
            let viewer = cx.new(|_cx| {
                crate::ddl_viewer::DdlViewerView::loading(
                    row.qualified_name(),
                    false,
                    self.context.profile.database_type.family(),
                )
            });
            viewer.update(cx, |viewer, cx| {
                viewer.set_error(
                    crate::localization::format(
                        "explorer.object_browser.no_definition_viewer",
                        &[("kind", &row.kind)],
                    ),
                    cx,
                );
            });
            self.detail.ddl = Some(viewer);
            return;
        };
        let title = row.qualified_name();
        let dialect = self.context.profile.database_type.family();
        let viewer = cx.new(|_cx| crate::ddl_viewer::DdlViewerView::loading(title, false, dialect));
        self._ddl_sub = Some(cx.subscribe(&viewer, |this, _view, event, cx| {
            if let crate::ddl_viewer::DdlViewerEvent::OpenAsQuery(sql) = event {
                let mut profile = this.context.profile.clone();
                profile.database = Some(this.database.clone());
                cx.emit(ToolDialogEvent::ScriptToEditorForProfile {
                    sql: sql.clone(),
                    profile: Box::new(profile),
                });
            }
        }));
        self.detail.ddl = Some(viewer.clone());
        let services = self.context.services.clone();
        let profile = self.context.profile.clone();
        let database = self.database.clone();
        let ddl_options = crate::preferences::DdlNavigationPreferences::load().ddl_options();
        let (tx, rx) = tokio::sync::oneshot::channel();
        cx.background_spawn(async move {
            crate::tokio_handle()
                .spawn(async move {
                    let result = fetch_ddl_document_with(
                        services.queries.as_ref(),
                        &profile,
                        &database,
                        &target,
                        ddl_options,
                    )
                    .await;
                    let _ = tx.send(result);
                })
                .await
                .ok();
        })
        .detach();
        let async_cx = cx.to_async();
        let weak = cx.weak_entity();
        cx.spawn(async move |_this, _cx| {
            let Ok(result) = rx.await else { return };
            let mut cx = async_cx.clone();
            weak.update(&mut cx, |this, cx| {
                if this.detail_generation != generation {
                    return;
                }
                let Some(viewer) = this.detail.ddl.clone() else {
                    return;
                };
                viewer.update(cx, |viewer, cx| match result {
                    Ok(document) => viewer.set_document(document, cx),
                    Err(err) => viewer.set_error(err.to_string(), cx),
                });
            })
            .ok();
        })
        .detach();
    }

    fn emit_batch(&mut self, action: BatchAction, cx: &mut gpui::Context<Self>) {
        self.sync_selection(cx);
        let selected: Vec<InventoryRow> = self.selected_rows().into_iter().cloned().collect();
        if selected.is_empty() {
            cx.emit(ToolDialogEvent::Status {
                level: StatusLevel::Info,
                message: crate::localization::text("explorer.object_browser.select_first"),
            });
            return;
        }
        let refs: Vec<&InventoryRow> = selected.iter().collect();
        if action == BatchAction::CopyNames {
            let text = copy_names_text(&refs, self.context.profile.database_type);
            cx.write_to_clipboard(ClipboardItem::new_string(text));
            cx.emit(ToolDialogEvent::Status {
                level: StatusLevel::Info,
                message: crate::localization::format(
                    "explorer.object_browser.names_copied",
                    &[("count", &selected.len().to_string())],
                ),
            });
            return;
        }
        if action == BatchAction::ScriptCreate {
            self.emit_script_create(selected, cx);
            return;
        }
        let script = batch_script(
            self.context.profile.database_type,
            &self.database,
            action,
            &refs,
            &HashMap::new(),
        );
        self.emit_script(script.sql, script.skipped, cx);
    }

    fn emit_script_create(&mut self, selected: Vec<InventoryRow>, cx: &mut gpui::Context<Self>) {
        let tables: Vec<InventoryRow> = selected
            .iter()
            .filter(|row| row.kind.eq_ignore_ascii_case("TABLE"))
            .cloned()
            .collect();
        if tables.len() > SCRIPT_CREATE_TABLE_CAP {
            cx.emit(ToolDialogEvent::Status {
                level: StatusLevel::Warning,
                message: crate::localization::format(
                    "explorer.object_browser.create_limit",
                    &[("limit", &SCRIPT_CREATE_TABLE_CAP.to_string())],
                ),
            });
            return;
        }
        let owned = selected;
        let db = self.context.profile.database_type;
        let services = self.context.services.clone();
        let profile = self.context.profile.clone();
        let database = self.database.clone();
        let generation = self.refresh_generation;
        let (tx, mut rx) =
            tokio::sync::mpsc::channel::<HashMap<(String, String), Vec<ColumnDescriptor>>>(1);
        cx.background_spawn(async move {
            crate::tokio_handle()
                .spawn(async move {
                    let mut columns_by_object = HashMap::new();
                    for table in tables {
                        let Ok(sql) = sql_for_engine(
                            &MetadataQuery::Columns {
                                schema: table.schema.clone(),
                                object: table.name.clone(),
                            },
                            db,
                        ) else {
                            continue;
                        };
                        let outcome = execute_collect(&services, &profile, &sql).await;
                        columns_by_object.insert(
                            (table.schema.clone(), table.name.clone()),
                            columns_from_outcome(&outcome),
                        );
                    }
                    let _ = tx.send(columns_by_object).await;
                })
                .await
                .ok();
        })
        .detach();
        let async_cx = cx.to_async();
        let weak = cx.weak_entity();
        cx.spawn(async move |_this, _cx| {
            let Some(columns_by_object) = rx.recv().await else {
                return;
            };
            let mut cx = async_cx.clone();
            weak.update(&mut cx, |this, cx| {
                if this.refresh_generation != generation {
                    return;
                }
                let refs: Vec<&InventoryRow> = owned.iter().collect();
                let script = batch_script(
                    db,
                    &database,
                    BatchAction::ScriptCreate,
                    &refs,
                    &columns_by_object,
                );
                this.emit_script(script.sql, script.skipped, cx);
            })
            .ok();
        })
        .detach();
    }

    fn emit_script(&mut self, sql: String, skipped: Vec<String>, cx: &mut gpui::Context<Self>) {
        let mut profile = self.context.profile.clone();
        profile.database = Some(self.database.clone());
        cx.emit(ToolDialogEvent::ScriptToEditorForProfile {
            sql,
            profile: Box::new(profile),
        });
        if !skipped.is_empty() {
            cx.emit(ToolDialogEvent::Status {
                level: StatusLevel::Warning,
                message: crate::localization::format(
                    "explorer.object_browser.skipped_summary",
                    &[
                        ("count", &skipped.len().to_string()),
                        ("details", &skipped.join("; ")),
                    ],
                ),
            });
        }
    }

    fn confirm_then_script(
        &mut self,
        action: BatchAction,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        self.sync_selection(cx);
        let n = self.selected.len();
        if n == 0 {
            cx.emit(ToolDialogEvent::Status {
                level: StatusLevel::Info,
                message: crate::localization::text("explorer.object_browser.select_first"),
            });
            return;
        }
        let verb = match action {
            BatchAction::Drop => "explorer.object_browser.confirm_drop_verb",
            BatchAction::Truncate => "explorer.object_browser.confirm_truncate_verb",
            _ => return self.emit_batch(action, cx),
        };
        let rx = window.prompt(
            PromptLevel::Warning,
            &crate::localization::format(
                "explorer.object_browser.confirm_batch_title",
                &[
                    ("verb", &crate::localization::text(verb)),
                    ("count", &n.to_string()),
                    ("database", &self.database),
                ],
            ),
            Some(&crate::localization::text(
                "explorer.object_browser.confirm_batch_description",
            )),
            &[
                PromptButton::new(crate::localization::text(
                    "explorer.object_browser.confirm_script_button",
                )),
                PromptButton::cancel(crate::localization::text(
                    "explorer.object_browser.confirm_cancel_button",
                )),
            ],
            cx,
        );
        let weak = cx.weak_entity();
        let async_cx = cx.to_async();
        cx.spawn(async move |_this, _cx| {
            if rx.await != Ok(0) {
                return;
            }
            let mut cx = async_cx.clone();
            weak.update(&mut cx, |this, cx| this.emit_batch(action, cx))
                .ok();
        })
        .detach();
    }

    fn close(&mut self, cx: &mut gpui::Context<Self>) {
        cx.emit(ToolDialogEvent::Close);
    }

    fn title(&self) -> String {
        match &self.schema {
            Some(schema) if !schema.is_empty() => crate::localization::format(
                "explorer.object_browser.title_schema",
                &[("database", &self.database), ("schema", schema)],
            ),
            _ => crate::localization::format(
                "explorer.object_browser.title_database",
                &[("database", &self.database)],
            ),
        }
    }
}

impl gpui::Focusable for ObjectBrowserPane {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ObjectBrowserPane {
    fn render(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let has_selection = !self.selected.is_empty() || !self.read_selected_indices(cx).is_empty();
        let truncated = truncation_note(self.rows.len()).filter(|_| self.truncated);

        let inventory: gpui::AnyElement = if let Some(err) = &self.error {
            div()
                .p(px(s(12.0)))
                .text_color(Theme::error())
                .text_size(px(Theme::font_size_ratio(0.87)))
                .whitespace_normal()
                .child(err.clone())
                .into_any_element()
        } else if let Some(grid) = &self.grid {
            div().size_full().child(grid.clone()).into_any_element()
        } else if self.loading {
            div()
                .p(px(s(12.0)))
                .text_color(Theme::text_dim())
                .child(crate::localization::text("explorer.object_browser.loading"))
                .into_any_element()
        } else {
            div()
                .p(px(s(12.0)))
                .text_color(Theme::text_dim())
                .child(crate::localization::text("explorer.object_browser.empty"))
                .into_any_element()
        };

        let kind_checks = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(s(10.0)))
            .children(KIND_OPTIONS.iter().copied().map(|kind| {
                let checked = self
                    .kind_filter
                    .iter()
                    .any(|existing| existing.eq_ignore_ascii_case(kind));
                Checkbox::new(format!("ob-kind-{kind}"))
                    .label(kind)
                    .checked(checked)
                    .on_click(cx.listener(move |this, value: &bool, _window, cx| {
                        this.toggle_kind(kind, *value, cx);
                    }))
            }));

        let batch = |id: &'static str, label: &'static str, action: BatchAction, confirm: bool| {
            Button::new(id)
                .label(crate::localization::text(label))
                .disabled(!has_selection)
                .on_click(cx.listener(move |this, _, window, cx| {
                    if confirm {
                        this.confirm_then_script(action, window, cx);
                    } else {
                        this.emit_batch(action, cx);
                    }
                }))
        };

        let filter_strip = div()
            .px(px(s(14.0)))
            .py(px(s(8.0)))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(s(10.0)))
            .border_b_1()
            .border_color(Theme::border())
            .child(div().w(px(s(220.0))).child(Input::new(&self.filter_input)))
            .child(kind_checks)
            .children(truncated.map(|note| {
                div()
                    .flex_1()
                    .text_size(px(Theme::font_size_ratio(0.80)))
                    .text_color(Theme::text_dim())
                    .child(note)
            }));

        let batch_strip = div()
            .px(px(s(14.0)))
            .py(px(s(8.0)))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(s(8.0)))
            .border_b_1()
            .border_color(Theme::border())
            .child(batch(
                "ob-drop",
                "explorer.object_browser.drop_button",
                BatchAction::Drop,
                true,
            ))
            .child(batch(
                "ob-truncate",
                "explorer.object_browser.truncate_button",
                BatchAction::Truncate,
                true,
            ))
            .child(batch(
                "ob-create",
                "explorer.object_browser.create_button",
                BatchAction::ScriptCreate,
                false,
            ))
            .child(batch(
                "ob-script-drop",
                "explorer.object_browser.script_drop_button",
                BatchAction::ScriptDrop,
                false,
            ))
            .child(batch(
                "ob-copy-names",
                "explorer.object_browser.copy_names_button",
                BatchAction::CopyNames,
                false,
            ));

        let main = div()
            .flex_1()
            .min_h(px(0.0))
            .flex()
            .flex_row()
            .child(div().flex_1().min_w(px(0.0)).child(inventory))
            .child(
                div()
                    .w(px(s(360.0)))
                    .min_h(px(0.0))
                    .border_l_1()
                    .border_color(Theme::border())
                    .flex()
                    .flex_col()
                    .child(self.render_detail(cx)),
            );

        // Bare body: the inventory grid and detail column manage their own
        // scrolling, and the strips carry their own padding.
        let focus_handle = self.focus_handle.clone();
        Dialog::new("object-browser-pane", |this: &mut Self| &mut this.frame)
            .title(self.title())
            .easy_close(true)
            .key_context("ObjectBrowserPane")
            .track_focus(&focus_handle)
            .bare_body()
            .on_close(|this, _window, cx| this.close(cx))
            .child(filter_strip)
            .child(batch_strip)
            .child(main)
            .leading_button(
                Button::new("object-browser-refresh")
                    .label(crate::localization::text(
                        "explorer.object_browser.refresh_button",
                    ))
                    .disabled(self.loading)
                    .on_click(cx.listener(|this, _, _window, cx| this.refresh(cx))),
            )
            .build(self, window, cx)
    }
}

impl ObjectBrowserPane {
    fn render_detail(&self, _cx: &mut gpui::Context<Self>) -> gpui::AnyElement {
        let s = Theme::scaled;
        if self.detail_key.is_none() {
            return div()
                .p(px(s(12.0)))
                .text_color(Theme::text_dim())
                .child(crate::localization::text(
                    "explorer.object_browser.detail_prompt",
                ))
                .into_any_element();
        }
        let section = |title: &'static str, child: gpui::AnyElement| {
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_h(px(0.0))
                .child(
                    div()
                        .px(px(s(10.0)))
                        .py(px(s(6.0)))
                        .border_b_1()
                        .border_color(Theme::border())
                        .text_size(px(Theme::font_size_ratio(0.80)))
                        .text_color(Theme::text_dim())
                        .child(crate::localization::text(title)),
                )
                .child(div().flex_1().min_h(px(0.0)).child(child))
        };
        let columns = self
            .detail
            .columns
            .clone()
            .map(|grid| div().size_full().child(grid).into_any_element())
            .unwrap_or_else(|| {
                div()
                    .p(px(s(10.0)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text(
                        "explorer.object_browser.columns_empty",
                    ))
                    .into_any_element()
            });
        let keys = self
            .detail
            .keys
            .clone()
            .map(|grid| div().size_full().child(grid).into_any_element())
            .unwrap_or_else(|| {
                div()
                    .p(px(s(10.0)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text(
                        "explorer.object_browser.keys_empty",
                    ))
                    .into_any_element()
            });
        let ddl = self
            .detail
            .ddl
            .clone()
            .map(|viewer| div().size_full().child(viewer).into_any_element())
            .unwrap_or_else(|| {
                div()
                    .p(px(s(10.0)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text(
                        "explorer.object_browser.definition_empty",
                    ))
                    .into_any_element()
            });
        div()
            .flex()
            .flex_col()
            .size_full()
            .child(section("explorer.object_browser.columns_section", columns))
            .child(div().h(px(1.0)).bg(Theme::border()))
            .child(section("explorer.object_browser.keys_section", keys))
            .child(div().h(px(1.0)).bg(Theme::border()))
            .child(section("explorer.object_browser.definition_section", ddl))
            .into_any_element()
    }
}

fn selected_row_indices(sel: &Selection) -> Vec<usize> {
    match sel {
        Selection::Row(row) => vec![*row],
        Selection::RowRange(a, b) => (*a.min(b)..=*a.max(b)).collect(),
        Selection::Rows(rows) => rows.clone(),
        Selection::Cell(row, _) => vec![*row],
        Selection::CellRange(r1, _, r2, _) => (*r1.min(r2)..=*r1.max(r2)).collect(),
        Selection::Cells(cells) => {
            let mut rows: Vec<usize> = cells.iter().map(|(row, _)| *row).collect();
            rows.sort_unstable();
            rows.dedup();
            rows
        }
        _ => Vec::new(),
    }
}

fn navigation_target(row: &InventoryRow, database: &str) -> Option<NavigationTarget> {
    let schema = row.schema.clone();
    let name = row.name.clone();
    let catalog = if database.is_empty() {
        None
    } else {
        Some(database.to_string())
    };
    match row.kind.to_ascii_uppercase().as_str() {
        "TABLE" => Some(NavigationTarget::Table {
            catalog,
            schema,
            name,
            clicked: ClickedRole::Table,
        }),
        "VIEW" | "MATERIALIZED VIEW" => Some(NavigationTarget::Table {
            catalog,
            schema,
            name,
            clicked: ClickedRole::View,
        }),
        "PROCEDURE" => Some(NavigationTarget::Routine {
            catalog,
            schema,
            name,
            kind: RoutineKind::Procedure,
            clicked: ClickedRole::Procedure,
        }),
        "FUNCTION" => Some(NavigationTarget::Routine {
            catalog,
            schema,
            name,
            kind: RoutineKind::Function,
            clicked: ClickedRole::Function,
        }),
        _ => None,
    }
}

fn columns_from_outcome(outcome: &ToolQueryOutcome) -> Vec<ColumnDescriptor> {
    let Some(set) = outcome.sets.first() else {
        return Vec::new();
    };
    set.rows
        .iter()
        .map(|row| ColumnDescriptor {
            name: cell_text(row.first()),
            data_type: cell_text(row.get(1)),
            nullable: cell_truthy(row.get(5)),
        })
        .filter(|column| !column.name.is_empty())
        .collect()
}

fn keys_set_for(
    schema: &str,
    name: &str,
    pk_outcome: &ToolQueryOutcome,
    fk_outcome: &ToolQueryOutcome,
) -> ToolQuerySet {
    let mut rows = Vec::new();
    if let Some(set) = pk_outcome.sets.first() {
        for row in &set.rows {
            if cell_text(row.first()).eq_ignore_ascii_case(schema)
                && cell_text(row.get(1)).eq_ignore_ascii_case(name)
            {
                rows.push(vec![
                    CellValue::String("PRIMARY KEY".into()),
                    cell_value(row.get(2)),
                    CellValue::String(format!("ordinal {}", cell_text(row.get(3)))),
                ]);
            }
        }
    }
    if let Some(set) = fk_outcome.sets.first() {
        for row in &set.rows {
            if cell_text(row.get(1)).eq_ignore_ascii_case(schema)
                && cell_text(row.get(2)).eq_ignore_ascii_case(name)
            {
                rows.push(vec![
                    CellValue::String("FOREIGN KEY".into()),
                    cell_value(row.first()),
                    CellValue::String(format!(
                        "{} → {}.{}.{}",
                        cell_text(row.get(3)),
                        cell_text(row.get(4)),
                        cell_text(row.get(5)),
                        cell_text(row.get(6))
                    )),
                ]);
            }
        }
    }
    ToolQuerySet {
        columns: vec![
            ResultColumn {
                name: "Kind".into(),
                data_type: "nvarchar".into(),
                nullable: true,
            },
            ResultColumn {
                name: "Name".into(),
                data_type: "nvarchar".into(),
                nullable: true,
            },
            ResultColumn {
                name: "Detail".into(),
                data_type: "nvarchar".into(),
                nullable: true,
            },
        ],
        rows,
    }
}

fn cell_text(value: Option<&CellValue>) -> String {
    match value {
        Some(CellValue::String(s)) => s.clone(),
        Some(CellValue::I64(n)) => n.to_string(),
        Some(CellValue::F64(n)) => n.to_string(),
        Some(CellValue::Bool(b)) => b.to_string(),
        Some(CellValue::Null) | None => String::new(),
        Some(other) => format!("{other:?}"),
    }
}

fn cell_value(value: Option<&CellValue>) -> CellValue {
    value.cloned().unwrap_or(CellValue::Null)
}

fn cell_truthy(value: Option<&CellValue>) -> bool {
    matches!(value, Some(CellValue::Bool(true)) | Some(CellValue::I64(1)))
        || matches!(value, Some(CellValue::String(v)) if v == "1" || v.eq_ignore_ascii_case("true"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_reports_truncation_at_row_cap() {
        assert_eq!(truncation_note(TOOL_QUERY_ROW_CAP - 1), None);
        assert_eq!(
            truncation_note(TOOL_QUERY_ROW_CAP),
            Some("Showing the first 500 objects — narrow the filter or pick a schema".to_string())
        );
        assert!(truncation_note(TOOL_QUERY_ROW_CAP + 10).is_some());
    }
}
