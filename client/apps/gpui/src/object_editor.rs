//! Object editor dialog — the per-object Properties/Edit surface opened from
//! the explorer's right-click → Properties for databases, tables, views,
//! routines, indexes, statistics, constraints, triggers, and user-defined types.
//!
//! One shell serves every kind, on the unified [`crate::dialog::Dialog`]
//! chrome (title = qualified name): a kind/context strip, a left page list, a
//! content area (property rows, a `SqllyDataTable` grid, or a
//! syntax-highlighted source pane), and a footer with status and the
//! Refresh / Script / Apply / Close actions. Pages lazy-fetch live metadata
//! through `MetadataService::run_metadata_query` on first visit and cache for
//! the session. Editable facets (renames, column nullability, statistic
//! NORECOMPUTE) stage into a [`PendingChanges`] set; Apply diffs that set
//! into a T-SQL batch, shows it in a review sheet, and executes on confirm.
//!
//! All page-shape, parsing, and DDL logic lives in the pure
//! [`crate::object_editor_model`] — this file is rendering and I/O only.

use std::collections::HashMap;

use gpui::{
    AnyElement, ClipboardItem, Context, Entity, EventEmitter, IntoElement, Render, ScrollHandle,
    Styled, Subscription, Window, div, prelude::*, px,
};
use gpui_component::{
    Disableable,
    button::{Button, ButtonVariant, ButtonVariants},
    checkbox::Checkbox,
    input::{Input, InputEvent, InputState},
};
use sqlly_core::ConnectionProfile;
use sqlly_datatable::{
    CellValue as GridCell, Column as GridColumn, ColumnKind, GridData, SqllyDataTable,
};
use sqlly_intel::metadata::{MetadataQuery, sql_for};
use sqlly_services::{QueryEvent, Services};

use crate::dialog::{Dialog, DialogFrame};
use crate::object_editor_model::{
    self, ColumnInfo, EditorObjectKind, EditorPage, EditorTarget, PageContent, PageData,
    PendingChanges, PropertyRow,
};
use crate::sql_syntax::classify;
use crate::theme::Theme;

/// Actions the host (app.rs) handles.
#[derive(Debug, Clone)]
pub enum ObjectEditorEvent {
    /// Script/definition text to open in a new query tab.
    OpenQuery(String),
    Closed,
}

impl EventEmitter<ObjectEditorEvent> for ObjectEditorView {}

/// Fetch state of one page.
#[derive(Debug, Clone)]
enum PageState {
    Unloaded,
    Loading,
    Loaded(PageData),
    Failed(String),
}

struct PageEntry {
    state: PageState,
    /// Bumped on every fetch; a landing result whose generation has moved on
    /// is dropped (same anti-stale-load discipline as the schema tree).
    generation: u64,
}

/// The Apply flow: hidden, showing the review sheet, or executing.
#[derive(Debug, Clone, PartialEq)]
enum ApplyState {
    Hidden,
    Review(Vec<String>),
    Applying,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum StatusKind {
    Info,
    Error,
    Success,
}

pub struct ObjectEditorView {
    services: Services,
    /// Tokenized but secret-free profile; hydrated on a background executor
    /// per fetch/apply, exactly like the schema tree's lazy loads.
    profile: ConnectionProfile,
    target: EditorTarget,
    pages: Vec<object_editor_model::PageSpec>,
    active: EditorPage,
    states: HashMap<EditorPage, PageEntry>,
    grids: HashMap<EditorPage, Entity<SqllyDataTable>>,
    changes: PendingChanges,
    rename_input: Entity<InputState>,
    rename_error: Option<String>,
    column_rename_input: Entity<InputState>,
    column_rename_error: Option<String>,
    /// The column the Columns-page edit strip is bound to (grid selection).
    strip_column: Option<String>,
    /// Server setting the Configuration-page edit strip is bound to.
    strip_setting: Option<String>,
    setting_value_input: Entity<InputState>,
    /// Search box shared by the server Configuration and Status grids.
    settings_filter_input: Entity<InputState>,
    settings_filter: String,
    /// Table/view General-page description facet: the live server value and
    /// the editor bound to it. `None` until the inventory read lands.
    current_description: Option<String>,
    description_input: Entity<InputState>,
    description_generation: u64,
    /// Whether the live description has been read. The field stays hidden
    /// until it has: staging against an unknown value would pick the wrong
    /// SQL Server procedure (add vs update) or "clear" a comment unseen.
    description_read: DescriptionRead,
    apply: ApplyState,
    /// Snapshot of the host's production guard when the dialog opened: a
    /// locked production connection stages nothing and applies nothing (the
    /// Apply path runs statements directly, bypassing the editor's run gate).
    production_read_only: bool,
    status: Option<(StatusKind, String)>,
    confirm_discard: bool,
    properties_scroll: ScrollHandle,
    source_scroll: ScrollHandle,
    focus_handle: gpui::FocusHandle,
    frame: DialogFrame,
    _subscriptions: Vec<Subscription>,
}

impl gpui::Focusable for ObjectEditorView {
    fn focus_handle(&self, _: &gpui::App) -> gpui::FocusHandle {
        self.focus_handle.clone()
    }
}

impl ObjectEditorView {
    pub fn new(
        services: Services,
        profile: ConnectionProfile,
        target: EditorTarget,
        production_read_only: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let pages = object_editor_model::pages_for(target.kind);
        let active = pages
            .first()
            .map(|spec| spec.page)
            .unwrap_or(EditorPage::General);

        let rename_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(crate::localization::input_placeholder(
                    window,
                    cx,
                    "explorer.object_editor.rename_placeholder",
                ))
                .default_value(&target.name)
        });
        let column_rename_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "explorer.object_editor.column_rename_placeholder",
            ))
        });
        let setting_value_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "explorer.object_editor.setting_value_placeholder",
            ))
        });
        let settings_filter_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "explorer.object_editor.settings_filter_placeholder",
            ))
        });
        let description_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "explorer.object_editor.description_placeholder",
            ))
        });
        let subscriptions = vec![
            cx.subscribe_in(
                &rename_input,
                window,
                |this: &mut Self, input, event: &InputEvent, _window, cx| {
                    if let InputEvent::Change = event {
                        let value = input.read(cx).value().to_string();
                        let original = this.target.name.clone();
                        match this.changes.stage_rename(&original, &value) {
                            Ok(()) => this.rename_error = None,
                            Err(reason) => this.rename_error = Some(reason),
                        }
                        cx.notify();
                    }
                },
            ),
            cx.subscribe_in(
                &description_input,
                window,
                |this: &mut Self, input, event: &InputEvent, _window, cx| {
                    if let InputEvent::Change = event {
                        let value = input.read(cx).value().to_string();
                        let original = this.current_description.clone().unwrap_or_default();
                        this.changes.stage_description(&original, &value);
                        cx.notify();
                    }
                },
            ),
            cx.subscribe_in(
                &column_rename_input,
                window,
                |this: &mut Self, input, event: &InputEvent, _window, cx| {
                    if let InputEvent::Change = event {
                        let value = input.read(cx).value().to_string();
                        if let Some(column) = this.strip_column.clone() {
                            match this.changes.stage_column_rename(&column, &column, &value) {
                                Ok(()) => this.column_rename_error = None,
                                Err(reason) => this.column_rename_error = Some(reason),
                            }
                            cx.notify();
                        }
                    }
                },
            ),
            cx.subscribe_in(
                &settings_filter_input,
                window,
                |this: &mut Self, input, event: &InputEvent, _window, cx| {
                    if let InputEvent::Change = event {
                        let value = input.read(cx).value().to_string();
                        if value != this.settings_filter {
                            this.settings_filter = value;
                            this.rebuild_settings_grids(cx);
                            cx.notify();
                        }
                    }
                },
            ),
            cx.subscribe_in(
                &setting_value_input,
                window,
                |this: &mut Self, input, event: &InputEvent, _window, cx| {
                    if let InputEvent::Change = event {
                        let value = input.read(cx).value().to_string();
                        if let Some(name) = this.strip_setting.clone()
                            && let Some(setting) = this.setting_by_name(&name, cx)
                        {
                            let persist = this
                                .changes
                                .server_settings
                                .get(&name)
                                .map(|change| change.persist)
                                .unwrap_or(false);
                            this.changes.stage_server_setting(
                                &name,
                                &setting.value,
                                &value,
                                persist,
                            );
                            let needs_advanced = setting.advanced && this.show_advanced_off();
                            this.changes
                                .mark_server_setting_advanced(&name, needs_advanced);
                            cx.notify();
                        }
                    }
                },
            ),
        ];

        let mut view = Self {
            services,
            profile,
            target,
            pages,
            active,
            states: HashMap::new(),
            grids: HashMap::new(),
            changes: PendingChanges::default(),
            rename_input,
            rename_error: None,
            column_rename_input,
            column_rename_error: None,
            strip_column: None,
            strip_setting: None,
            setting_value_input,
            settings_filter_input,
            settings_filter: String::new(),
            current_description: None,
            description_input,
            description_generation: 0,
            description_read: DescriptionRead::Pending,
            apply: ApplyState::Hidden,
            production_read_only,
            status: None,
            confirm_discard: false,
            properties_scroll: ScrollHandle::new(),
            source_scroll: ScrollHandle::new(),
            focus_handle: cx.focus_handle(),
            frame: DialogFrame::new(Theme::scaled(880.0), Theme::scaled(600.0)),
            _subscriptions: subscriptions,
        };
        view.ensure_page_loaded(active, cx);
        view.ensure_description_loaded(cx);
        view
    }

    // ------------------------------------------------------------------
    // Fetching
    // ------------------------------------------------------------------

    fn entry(&mut self, page: EditorPage) -> &mut PageEntry {
        self.states.entry(page).or_insert(PageEntry {
            state: PageState::Unloaded,
            generation: 0,
        })
    }

    /// Kick off the page's metadata fetch unless it is already loaded or in
    /// flight. Pages are cheap (single catalog queries) and lazy: nothing is
    /// fetched for a page the user never opens.
    fn ensure_page_loaded(&mut self, page: EditorPage, cx: &mut Context<Self>) {
        let entry = self.entry(page);
        if !matches!(entry.state, PageState::Unloaded | PageState::Failed(_)) {
            return;
        }
        entry.state = PageState::Loading;
        entry.generation = entry.generation.wrapping_add(1);
        let generation = entry.generation;
        cx.notify();

        let query = object_editor_model::query_for_page(&self.target, page);
        // Redis has no metadata dialect; its Status page reads `INFO` through
        // the ordinary query path instead of a catalog SELECT.
        if page == EditorPage::Status && self.profile.database_type.is_command_engine() {
            self.fetch_redis_status(page, generation, cx);
            return;
        }
        // Server pages read server-scope catalogs and carry no database of
        // their own; run them wherever the profile already points.
        let database = if self.target.database.is_empty() {
            self.profile.database.clone().unwrap_or_default()
        } else {
            self.target.database.clone()
        };
        let profile = self.profile.clone();
        let services = self.services.clone();

        cx.spawn(async move |this, cx| {
            let hydrated = cx
                .background_executor()
                .spawn(async move { crate::sql_credentials::hydrate_profile(profile) })
                .await;
            let result = match hydrated {
                Ok(profile) => services
                    .metadata
                    .run_metadata_query(&profile, &database, &query)
                    .await
                    .map_err(|e| crate::localization::message(e.message())),
                Err(reason) => Err(crate::localization::format(
                    "explorer.object_editor.credentials_failed",
                    &[("error", &crate::localization::message(reason))],
                )),
            };
            this.update(cx, |view, cx| {
                view.finish_page_load(page, generation, result, cx);
            })
            .ok();
        })
        .detach();
    }

    /// The Status page for a command engine (Redis and relatives): run `INFO`
    /// and reshape its (section, key, value) rows into the page's
    /// (name, value, source) counter rows.
    fn fetch_redis_status(&mut self, page: EditorPage, generation: u64, cx: &mut Context<Self>) {
        let profile = self.profile.clone();
        let services = self.services.clone();
        cx.spawn(async move |this, cx| {
            let hydrated = cx
                .background_executor()
                .spawn(async move { crate::sql_credentials::hydrate_profile(profile) })
                .await;
            let result = match hydrated {
                Ok(profile) => match services.queries.execute(&profile, "INFO", 1024).await {
                    Ok(handle) => {
                        let mut events = handle.events;
                        let mut rows = Vec::new();
                        let mut failure = None;
                        while let Some(event) = events.recv().await {
                            match event {
                                QueryEvent::Rows { rows: batch, .. } => rows.extend(batch),
                                QueryEvent::Error { message } => {
                                    failure = Some(message);
                                    break;
                                }
                                QueryEvent::Completed { .. } | QueryEvent::Cancelled => break,
                                _ => {}
                            }
                        }
                        match failure {
                            Some(message) => Err(message),
                            None => Ok(object_editor_model::redis_status_rows(&rows)),
                        }
                    }
                    Err(e) => Err(crate::localization::message(e.message())),
                },
                Err(reason) => Err(crate::localization::format(
                    "explorer.object_editor.credentials_failed",
                    &[("error", &crate::localization::message(reason))],
                )),
            };
            this.update(cx, |view, cx| {
                view.finish_page_load(page, generation, result, cx);
            })
            .ok();
        })
        .detach();
    }

    fn finish_page_load(
        &mut self,
        page: EditorPage,
        generation: u64,
        result: Result<Vec<Vec<sqlly_core::CellValue>>, String>,
        cx: &mut Context<Self>,
    ) {
        let current_generation = self.states.get(&page).map(|entry| entry.generation);
        if current_generation != Some(generation) {
            return;
        }
        match result {
            Ok(rows) => {
                let data = object_editor_model::parse_page_data(&self.target, page, &rows);
                self.rebuild_grid(page, &data, cx);
                self.entry(page).state = PageState::Loaded(data);
                if page == self.active {
                    self.status = None;
                }
            }
            Err(message) => {
                self.entry(page).state = PageState::Failed(message.clone());
                if page == self.active {
                    self.status = Some((StatusKind::Error, message));
                }
            }
        }
        cx.notify();
    }

    /// Why this object's description cannot be edited on this engine (no
    /// comment store, or views without comments on MySQL/MariaDB,
    /// ClickHouse, CockroachDB). `None` when it can.
    fn description_unsupported_reason(&self) -> Option<String> {
        let kind = match self.target.kind {
            EditorObjectKind::View => sqlly_services::alter::CommentObjectKind::View,
            _ => sqlly_services::alter::CommentObjectKind::Table,
        };
        sqlly_services::alter::unsupported_reason_for_message(self.profile.database_type, kind)
            .map(crate::localization::message)
    }

    /// Fetch the object's live server-side description through the same
    /// ObjectInventory catalog read whose Comment column the explorer shows.
    /// Table/view General pages only; engines without a comment store skip
    /// the round trip (the facet renders their reason instead).
    fn ensure_description_loaded(&mut self, cx: &mut Context<Self>) {
        if !matches!(
            self.target.kind,
            EditorObjectKind::Table | EditorObjectKind::View
        ) || self.description_unsupported_reason().is_some()
        {
            return;
        }
        // A staged edit is kept; its baseline is the value already read.
        if self.changes.description.is_some() {
            return;
        }
        self.description_generation = self.description_generation.wrapping_add(1);
        // A refresh keeps showing the value already read until it lands.
        if self.description_read != DescriptionRead::Loaded {
            self.description_read = DescriptionRead::Pending;
        }
        let generation = self.description_generation;
        let schema = if self.target.schema.is_empty() {
            None
        } else {
            Some(self.target.schema.clone())
        };
        let query = MetadataQuery::ObjectInventory { schema };
        let database = if self.target.database.is_empty() {
            self.profile.database.clone().unwrap_or_default()
        } else {
            self.target.database.clone()
        };
        let profile = self.profile.clone();
        let services = self.services.clone();

        cx.spawn(async move |this, cx| {
            let hydrated = cx
                .background_executor()
                .spawn(async move { crate::sql_credentials::hydrate_profile(profile) })
                .await;
            let result = match hydrated {
                Ok(profile) => services
                    .metadata
                    .run_metadata_query(&profile, &database, &query)
                    .await
                    .map_err(|e| crate::localization::message(e.message())),
                Err(reason) => Err(crate::localization::format(
                    "explorer.object_editor.credentials_failed",
                    &[("error", &crate::localization::message(reason))],
                )),
            };
            this.update(cx, |view, cx| {
                if view.description_generation != generation {
                    return;
                }
                match result {
                    Ok(rows) => {
                        view.current_description =
                            object_editor_model::object_comment_from_inventory(
                                &rows,
                                view.target.kind,
                                &view.target.schema,
                                &view.target.name,
                            );
                        view.description_read = DescriptionRead::Loaded;
                    }
                    Err(message) => view.description_read = DescriptionRead::Failed(message),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// (Re)build the grid entity for a freshly loaded grid page.
    fn rebuild_grid(&mut self, page: EditorPage, data: &PageData, cx: &mut Context<Self>) {
        // The server Configuration/Status grids show only the rows matching
        // the search box; the edit strip maps selection through the same
        // filtered list (`visible_settings`).
        let filtered;
        let (data, headers) = match data {
            PageData::Settings(settings)
                if matches!(page, EditorPage::Configuration | EditorPage::Status) =>
            {
                filtered = PageData::Settings(object_editor_model::filter_settings(
                    settings,
                    &self.settings_filter,
                ));
                (&filtered, object_editor_model::settings_headers(page))
            }
            _ => (data, object_editor_model::grid_columns(data)),
        };
        if headers.is_empty() {
            self.grids.remove(&page);
            return;
        }
        let columns: Vec<GridColumn> = headers
            .iter()
            .map(|name| GridColumn {
                name: name.to_string(),
                kind: ColumnKind::Text,
                width: 160.0,
            })
            .collect();
        let rows: Vec<Vec<GridCell>> = object_editor_model::grid_rows(data)
            .into_iter()
            .map(|row| row.into_iter().map(GridCell::Text).collect())
            .collect();
        let Some(grid_data) = GridData::new(columns, rows).ok() else {
            self.grids.remove(&page);
            return;
        };
        // Builder-supplied theme keeps the grid host-themed; a post-build
        // `state.theme` write is overwritten by the widget's own palette on
        // the next render.
        let theme = crate::grid_bridge::build_grid_theme();
        let view = SqllyDataTable::builder(grid_data)
            .config(Default::default())
            .context_menu_visibility(crate::grid_bridge::grid_menu_tooltip_policy)
            .theme(theme)
            .build(cx);
        let entity = cx.new(|_cx| view);
        cx.observe(&entity, |_, _, cx| cx.notify()).detach();
        self.grids.insert(page, entity);
    }

    /// Rebuild the loaded Configuration/Status grids after the search box
    /// changes.
    fn rebuild_settings_grids(&mut self, cx: &mut Context<Self>) {
        for page in [EditorPage::Configuration, EditorPage::Status] {
            let data = match self.states.get(&page) {
                Some(PageEntry {
                    state: PageState::Loaded(data),
                    ..
                }) => data.clone(),
                _ => continue,
            };
            self.rebuild_grid(page, &data, cx);
        }
        // The grid selection no longer points at the same row.
        self.strip_setting = None;
    }

    /// The Configuration/Status rows currently shown (search applied).
    fn visible_settings(&self, page: EditorPage) -> Vec<object_editor_model::SettingInfo> {
        match self.states.get(&page) {
            Some(PageEntry {
                state: PageState::Loaded(PageData::Settings(settings)),
                ..
            }) => object_editor_model::filter_settings(settings, &self.settings_filter),
            _ => Vec::new(),
        }
    }

    /// Re-apply the app theme to every built page grid. Called by the app
    /// when the theme changes while the editor is open — grids are only
    /// rebuilt on page load, so without this they would keep the old palette.
    pub fn refresh_grid_themes(&mut self, cx: &mut Context<Self>) {
        let theme = crate::grid_bridge::build_grid_theme();
        for view in self.grids.values() {
            let state = view.read(cx).state.clone();
            state.update(cx, |s, cx| {
                s.theme = theme.clone();
                cx.notify();
            });
        }
        cx.notify();
    }

    fn refresh_active_page(&mut self, cx: &mut Context<Self>) {
        let entry = self.entry(self.active);
        entry.state = PageState::Unloaded;
        self.ensure_page_loaded(self.active, cx);
        if self.active == EditorPage::General {
            self.ensure_description_loaded(cx);
        }
    }

    // ------------------------------------------------------------------
    // Selection / activation
    // ------------------------------------------------------------------

    fn activate_page(&mut self, page: EditorPage, cx: &mut Context<Self>) {
        if self.active == page {
            return;
        }
        self.active = page;
        self.status = None;
        self.ensure_page_loaded(page, cx);
        cx.notify();
    }

    // ------------------------------------------------------------------
    // Editable facets
    // ------------------------------------------------------------------

    fn current_no_recompute(&self) -> bool {
        let Some(PageEntry {
            state: PageState::Loaded(PageData::Properties(rows)),
            ..
        }) = self.states.get(&EditorPage::General)
        else {
            return false;
        };
        crate::object_editor_model::statistic_no_recompute(rows)
    }

    fn toggle_no_recompute(&mut self, cx: &mut Context<Self>) {
        let staged = self
            .changes
            .statistic_no_recompute
            .unwrap_or_else(|| self.current_no_recompute());
        let current = self.current_no_recompute();
        self.changes.stage_no_recompute(current, !staged);
        cx.notify();
    }

    fn arm_update_statistics(&mut self, cx: &mut Context<Self>) {
        self.changes.update_statistics_now = !self.changes.update_statistics_now;
        cx.notify();
    }

    fn selected_column(&self, cx: &Context<Self>) -> Option<ColumnInfo> {
        let grid = self.grids.get(&EditorPage::Columns)?;
        let row = first_selected_source_row(grid.read(cx).state.read(cx))?;
        let Some(PageEntry {
            state: PageState::Loaded(PageData::Columns(columns)),
            ..
        }) = self.states.get(&EditorPage::Columns)
        else {
            return None;
        };
        columns.get(row).cloned()
    }

    fn toggle_column_nullability(&mut self, column: &ColumnInfo, cx: &mut Context<Self>) {
        let staged = self
            .changes
            .column_nullability
            .get(&column.name)
            .map(|change| change.nullable)
            .unwrap_or(column.is_nullable);
        self.changes.stage_nullability(column, !staged);
        cx.notify();
    }

    /// The Configuration grid's selected row as a typed setting. The grid
    /// was built from `visible_settings` in order, so its source rows index
    /// that list (display rows would not survive a header sort).
    fn selected_setting(&self, cx: &Context<Self>) -> Option<object_editor_model::SettingInfo> {
        let grid = self.grids.get(&EditorPage::Configuration)?;
        let row = first_selected_source_row(grid.read(cx).state.read(cx))?;
        self.visible_settings(EditorPage::Configuration)
            .get(row)
            .cloned()
    }

    /// SQL Server: whether `show advanced options` is off per the loaded
    /// Configuration page (advanced edits then need the plan's wrap).
    fn show_advanced_off(&self) -> bool {
        let Some(PageEntry {
            state: PageState::Loaded(PageData::Settings(settings)),
            ..
        }) = self.states.get(&EditorPage::Configuration)
        else {
            return false;
        };
        settings
            .iter()
            .find(|setting| setting.name == object_editor_model::SHOW_ADVANCED_OPTIONS)
            .is_some_and(|setting| setting.value.trim() == "0")
    }

    /// Why server-setting edits are blocked on this connection, if they are.
    fn settings_profile_block(&self) -> Option<String> {
        object_editor_model::server_settings_profile_block(
            self.profile.block_all_but_select,
            self.profile.auto_wrap_rollback,
            self.production_read_only,
        )
    }

    /// A loaded setting by name — the edit strip's binding target.
    fn setting_by_name(
        &self,
        name: &str,
        _cx: &Context<Self>,
    ) -> Option<object_editor_model::SettingInfo> {
        let Some(PageEntry {
            state: PageState::Loaded(PageData::Settings(settings)),
            ..
        }) = self.states.get(&EditorPage::Configuration)
        else {
            return None;
        };
        settings
            .iter()
            .find(|setting| setting.name == name)
            .cloned()
    }

    // ------------------------------------------------------------------
    // Apply flow
    // ------------------------------------------------------------------

    /// The staged changes as the connected engine's own statements, or the
    /// reason the engine cannot express one of them.
    fn engine_ddl_plan(&self) -> Result<Vec<String>, String> {
        object_editor_model::ddl_plan_for(
            self.profile.database_type,
            &self.target,
            &self.changes,
            self.current_no_recompute(),
            self.current_description.as_deref(),
        )
    }

    /// The guard every Apply passes before a statement runs: Apply executes
    /// through the query service directly, not the editor's run gate, so the
    /// production lock and the settings-specific profile blocks are enforced
    /// here.
    fn apply_block(&self) -> Option<String> {
        if self.production_read_only {
            return Some(crate::localization::text(
                "explorer.object_editor.production_locked",
            ));
        }
        if !self.changes.server_settings.is_empty() {
            return self.settings_profile_block();
        }
        None
    }

    fn begin_apply(&mut self, cx: &mut Context<Self>) {
        if let Some(reason) = self.apply_block() {
            self.status = Some((StatusKind::Error, reason));
            cx.notify();
            return;
        }
        let plan = match self.engine_ddl_plan() {
            Ok(plan) => plan,
            Err(reason) => {
                self.status = Some((StatusKind::Error, reason));
                cx.notify();
                return;
            }
        };
        if plan.is_empty() {
            return;
        }
        self.apply = ApplyState::Review(plan);
        cx.notify();
    }

    fn cancel_apply(&mut self, cx: &mut Context<Self>) {
        self.apply = ApplyState::Hidden;
        cx.notify();
    }

    fn execute_apply(&mut self, cx: &mut Context<Self>) {
        let ApplyState::Review(statements) = &self.apply else {
            return;
        };
        let statements = statements.clone();
        if let Some(reason) = self.apply_block() {
            self.status = Some((StatusKind::Error, reason));
            cx.notify();
            return;
        }
        // A settings script that switched `show advanced options` on must put
        // it back off even when a later statement fails.
        let cleanup = object_editor_model::server_settings_failure_cleanup(
            self.profile.database_type,
            &self.changes,
        );
        self.apply = ApplyState::Applying;
        self.status = Some((
            StatusKind::Info,
            crate::localization::text("explorer.object_editor.applying"),
        ));
        cx.notify();

        let profile = self.profile.clone();
        let services = self.services.clone();
        cx.spawn(async move |this, cx| {
            let hydrated = cx
                .background_executor()
                .spawn(async move { crate::sql_credentials::hydrate_profile(profile) })
                .await;
            let result = match hydrated {
                Ok(profile) => match run_statements(&services, &profile, &statements).await {
                    Ok(()) => Ok(()),
                    // Index 0 is the switch-on itself: nothing to undo.
                    Err((failed_at, message)) if failed_at > 0 && !cleanup.is_empty() => {
                        let restored = run_statements(&services, &profile, &cleanup).await;
                        Err(match restored {
                            Ok(()) => crate::localization::format(
                                "explorer.object_editor.advanced_restored",
                                &[
                                    ("error", &message),
                                    (
                                        "option",
                                        &object_editor_model::SHOW_ADVANCED_OPTIONS.to_string(),
                                    ),
                                ],
                            ),
                            Err((_, again)) => crate::localization::format(
                                "explorer.object_editor.advanced_restore_failed",
                                &[
                                    ("error", &message),
                                    (
                                        "option",
                                        &object_editor_model::SHOW_ADVANCED_OPTIONS.to_string(),
                                    ),
                                    ("cleanup_error", &again),
                                ],
                            ),
                        })
                    }
                    Err((_, message)) => Err(message),
                },
                Err(reason) => Err(crate::localization::format(
                    "explorer.object_editor.credentials_failed",
                    &[("error", &crate::localization::message(reason))],
                )),
            };
            this.update(cx, |view, cx| view.finish_apply(result, cx))
                .ok();
        })
        .detach();
    }

    fn finish_apply(&mut self, result: Result<(), String>, cx: &mut Context<Self>) {
        match result {
            Ok(()) => {
                self.status = Some((
                    StatusKind::Success,
                    crate::localization::text("explorer.object_editor.changes_applied"),
                ));
                self.apply = ApplyState::Hidden;
                // A rename moves the object: re-point the dialog at the new
                // name so Refresh and subsequent edits keep working. The
                // rename input already displays the new name (the user typed
                // it), so only the target needs updating.
                if let Some(new_name) = self.changes.rename_object.take() {
                    self.target.name = new_name;
                }
                // An applied description is the new live value; adopting it
                // directly saves a round trip and keeps the facet's original
                // in step (an empty staging means the comment was cleared).
                if let Some(staged) = self.changes.description.take() {
                    self.current_description = if staged.trim().is_empty() {
                        None
                    } else {
                        Some(staged)
                    };
                }
                self.changes = PendingChanges::default();
                self.confirm_discard = false;
                // Every loaded page is now stale; drop caches and refetch the
                // visible page.
                let pages: Vec<EditorPage> = self.states.keys().copied().collect();
                for page in pages {
                    self.entry(page).state = PageState::Unloaded;
                }
                self.ensure_page_loaded(self.active, cx);
            }
            Err(message) => {
                self.status = Some((
                    StatusKind::Error,
                    crate::localization::format(
                        "explorer.object_editor.apply_failed",
                        &[("error", &message)],
                    ),
                ));
                self.apply = ApplyState::Review(self.engine_ddl_plan().unwrap_or_default());
            }
        }
        cx.notify();
    }

    // ------------------------------------------------------------------
    // Script / close
    // ------------------------------------------------------------------

    fn script_active_page(&self, cx: &mut Context<Self>) {
        let query = object_editor_model::query_for_page(&self.target, self.active);
        let sql = match self.states.get(&self.active).map(|entry| &entry.state) {
            Some(PageState::Loaded(PageData::Definition(Some(definition)))) => definition.clone(),
            _ => format!(
                "-- {} · {}\n{}",
                self.target.qualified_name(),
                self.active.title(),
                sql_for(&query)
            ),
        };
        cx.emit(ObjectEditorEvent::OpenQuery(sql));
    }

    fn request_close(&mut self, cx: &mut Context<Self>) {
        if matches!(self.apply, ApplyState::Applying) {
            return;
        }
        if self.changes.is_dirty() && !self.confirm_discard {
            self.confirm_discard = true;
            cx.notify();
            return;
        }
        cx.emit(ObjectEditorEvent::Closed);
    }

    // ------------------------------------------------------------------
    // Rendering
    // ------------------------------------------------------------------

    fn footer_button(
        id: &'static str,
        label: impl Into<gpui::SharedString>,
        variant: ButtonVariant,
        enabled: bool,
        cx: &mut Context<Self>,
        on_click: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> AnyElement {
        let this = cx.entity();
        Button::new(id)
            .label(label)
            .with_variant(variant)
            .disabled(!enabled)
            .on_click(move |_: &gpui::ClickEvent, window, app| {
                this.update(app, |view, cx| on_click(view, window, cx));
            })
            .into_any_element()
    }

    /// The object name and the close ✕ live in the dialog chrome's title bar;
    /// this strip keeps the kind marker and parent context above the pages.
    fn render_header(&self) -> impl IntoElement {
        let s = Theme::scaled;
        let marker = self.target.kind.marker();
        let noun = self.target.kind.noun();
        // A server target has no database of its own; name the instance the
        // pages are reading instead of leaving the strip blank.
        let context = self
            .target
            .parent_context()
            .map(|parent| {
                crate::localization::format(
                    "explorer.object_editor.header_parent_context",
                    &[("parent", &parent), ("database", &self.target.database)],
                )
            })
            .unwrap_or_else(|| {
                if self.target.database.is_empty() {
                    self.profile.server.clone()
                } else {
                    self.target.database.clone()
                }
            });

        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(s(8.0)))
            .w_full()
            .px(px(s(12.0)))
            .py(px(s(8.0)))
            .border_b_1()
            .border_color(Theme::border())
            .overflow_hidden()
            .child(
                div()
                    .px(px(s(6.0)))
                    .py(px(s(2.0)))
                    .border_1()
                    .border_color(Theme::border())
                    .rounded(px(s(4.0)))
                    .text_color(Theme::text_dim())
                    .text_size(px(Theme::font_size_ratio(0.78)))
                    .child(marker),
            )
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.78)))
                    .text_color(Theme::text_dim())
                    .child(format!("{noun} · {context}")),
            )
    }

    fn render_page_list(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let active = self.active;
        let changes = &self.changes;
        let kind = self.target.kind;

        let rows = self.pages.iter().map(|spec| {
            let page = spec.page;
            let is_active = page == active;
            let dirty = page_dirty(kind, page, changes);
            let mut row = div()
                .id(("oe-page", page as u64))
                .cursor_pointer()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .w_full()
                .px(px(s(10.0)))
                .py(px(s(5.0)))
                .text_size(px(Theme::font_size_ratio(0.87)))
                .on_click(cx.listener(move |this, _event, _window, cx| {
                    this.activate_page(page, cx);
                }));
            row = if is_active {
                row.bg(Theme::accent().opacity(0.12))
                    .text_color(Theme::text())
            } else {
                row.text_color(Theme::text_dim())
            };
            row.child(spec.page.title()).child(
                div()
                    .w(px(s(6.0)))
                    .text_color(Theme::accent())
                    .child(if dirty { "●" } else { "" }),
            )
        });

        div()
            .flex()
            .flex_col()
            .w(px(s(168.0)))
            .h_full()
            .py(px(s(6.0)))
            .gap(px(s(1.0)))
            .bg(Theme::panel_bg())
            .border_r_1()
            .border_color(Theme::border())
            .child(
                div()
                    .px(px(s(10.0)))
                    .pb(px(s(4.0)))
                    .text_size(px(Theme::font_size_ratio(0.72)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text(
                        "explorer.object_editor.select_page",
                    )),
            )
            .children(rows)
    }

    fn render_content(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let state = self
            .states
            .get(&self.active)
            .map(|entry| entry.state.clone())
            .unwrap_or(PageState::Unloaded);
        let spec = self
            .pages
            .iter()
            .find(|spec| spec.page == self.active)
            .map(|spec| spec.content)
            .unwrap_or(PageContent::Properties);

        match state {
            PageState::Unloaded | PageState::Loading => {
                self.render_skeleton(spec).into_any_element()
            }
            PageState::Failed(message) => self.render_error(&message, cx).into_any_element(),
            PageState::Loaded(data) => match spec {
                PageContent::Properties => {
                    self.render_properties(&data, window, cx).into_any_element()
                }
                PageContent::Grid => self.render_grid_page(&data, window, cx).into_any_element(),
                PageContent::Source => self.render_source(&data).into_any_element(),
            },
        }
    }

    fn render_skeleton(&self, content: PageContent) -> impl IntoElement {
        let s = Theme::scaled;
        let bar = |width: f32| {
            div()
                .h(px(s(10.0)))
                .w(px(s(width)))
                .rounded(px(s(3.0)))
                .bg(Theme::text_dim().opacity(0.18))
        };
        let rows: Vec<gpui::Div> = (0..6)
            .map(|i| {
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(12.0)))
                    .child(bar(120.0))
                    .child(bar(200.0 + (i as f32 % 3.0) * 60.0))
            })
            .collect();
        let _ = content;
        div()
            .flex()
            .flex_col()
            .gap(px(s(14.0)))
            .p(px(s(16.0)))
            .children(rows)
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.8)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text("explorer.object_editor.loading")),
            )
    }

    fn render_error(&mut self, message: &str, cx: &mut Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        div()
            .flex()
            .flex_col()
            .gap(px(s(10.0)))
            .p(px(s(16.0)))
            .child(
                div()
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::error())
                    .child(message.to_string()),
            )
            .child(div().w(px(s(96.0))).child(Self::footer_button(
                "oe-retry",
                crate::localization::text("explorer.object_editor.retry_button"),
                ButtonVariant::Secondary,
                true,
                cx,
                |this, _window, cx| this.refresh_active_page(cx),
            )))
    }

    fn render_properties(
        &mut self,
        data: &PageData,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let s = Theme::scaled;
        let PageData::Properties(rows) = data else {
            return div().into_any_element();
        };
        let mut body = div().flex().flex_col().gap(px(s(6.0)));
        for row in rows {
            body = body.child(property_row(row));
        }
        if rows.is_empty() {
            body = body.child(empty_line(&crate::localization::format(
                "explorer.object_editor.no_properties",
                &[("kind", &self.target.kind.noun().to_lowercase())],
            )));
        }

        // Editable facets for this kind's General page.
        if self.target.kind.supports_rename() {
            body = body.child(self.render_rename_facet(cx));
        }
        if self.target.kind == EditorObjectKind::Statistic {
            body = body.child(self.render_statistic_facets(cx));
        }
        if matches!(
            self.target.kind,
            EditorObjectKind::Table | EditorObjectKind::View
        ) {
            body = body.child(self.render_description_facet(window, cx));
        }

        div()
            .id("oe-properties")
            .track_scroll(&self.properties_scroll)
            .flex_1()
            .w_full()
            .overflow_y_scroll()
            .p(px(s(16.0)))
            .child(body)
            .into_any_element()
    }

    fn render_rename_facet(&mut self, _cx: &mut Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let mut facet = div()
            .flex()
            .flex_col()
            .gap(px(s(4.0)))
            .pt(px(s(10.0)))
            .mt(px(s(6.0)))
            .border_t_1()
            .border_color(Theme::border())
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.74)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text(
                        "explorer.object_editor.rename_label",
                    )),
            )
            .child(div().w(px(s(280.0))).child(Input::new(&self.rename_input)));
        if let Some(error) = &self.rename_error {
            facet = facet.child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.78)))
                    .text_color(Theme::error())
                    .child(error.clone()),
            );
        }
        facet
    }

    /// Why the Description field is withheld, shown in its place: the engine
    /// or object kind has no comment store, the live value is still being
    /// read, or the read failed. `None` once the value has been read and the
    /// field can be edited against it.
    fn description_facet_notice(&self) -> Option<String> {
        match (
            &self.description_read,
            self.description_unsupported_reason(),
        ) {
            (_, Some(reason)) => Some(crate::localization::format(
                "explorer.object_editor.description_unsupported",
                &[("reason", &reason)],
            )),
            (DescriptionRead::Pending, None) => Some(crate::localization::text(
                "explorer.object_editor.description_reading",
            )),
            (DescriptionRead::Failed(err), None) => Some(crate::localization::format(
                "explorer.object_editor.description_read_failed",
                &[("error", err)],
            )),
            (DescriptionRead::Loaded, None) => None,
        }
    }

    /// The General page's Description facet for tables and views: the live
    /// server comment in an editable field, staged like every other facet and
    /// scripted in the engine's own comment DDL on Apply. Engines without a
    /// comment store get the plain reason instead of a dead field.
    fn render_description_facet(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let s = Theme::scaled;
        if let Some(message) = self.description_facet_notice() {
            return div()
                .flex()
                .flex_col()
                .gap(px(s(4.0)))
                .pt(px(s(10.0)))
                .mt(px(s(6.0)))
                .border_t_1()
                .border_color(Theme::border())
                .child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.78)))
                        .text_color(Theme::text_dim())
                        .child(message),
                )
                .into_any_element();
        }
        // Push the live value into the field only while nothing is staged —
        // otherwise the fetch landing would clobber an in-progress edit (the
        // programmatic set_value echoes Change, which un-stages because the
        // value equals the original again).
        let original = self.current_description.clone().unwrap_or_default();
        if self.changes.description.is_none()
            && self.description_input.read(cx).value().as_ref() != original.as_str()
        {
            let value = original.clone();
            self.description_input.update(cx, |input, cx| {
                input.set_value(value, window, cx);
            });
        }
        let staged = self.changes.description.is_some();
        div()
            .flex()
            .flex_col()
            .gap(px(s(4.0)))
            .pt(px(s(10.0)))
            .mt(px(s(6.0)))
            .border_t_1()
            .border_color(Theme::border())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(6.0)))
                    .child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.74)))
                            .text_color(Theme::text_dim())
                            .child(crate::localization::text(
                                "explorer.object_editor.description_label",
                            )),
                    )
                    .child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.74)))
                            .text_color(Theme::accent())
                            .child(if staged { "●" } else { "" }),
                    ),
            )
            .child(
                div()
                    .w(px(s(360.0)))
                    .child(Input::new(&self.description_input)),
            )
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.74)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text(
                        "explorer.object_editor.description_hint",
                    )),
            )
            .into_any_element()
    }

    fn render_statistic_facets(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let effective = self
            .changes
            .statistic_no_recompute
            .unwrap_or_else(|| self.current_no_recompute());
        let armed = self.changes.update_statistics_now;
        let this = cx.entity();

        div()
            .flex()
            .flex_col()
            .gap(px(s(6.0)))
            .pt(px(s(10.0)))
            .mt(px(s(6.0)))
            .border_t_1()
            .border_color(Theme::border())
            .child(
                Checkbox::new("oe-norecompute")
                    .checked(effective)
                    .label(crate::localization::text(
                        "explorer.object_editor.disable_auto_updates",
                    ))
                    .on_click(move |_: &bool, _window, app| {
                        this.update(app, |view, cx| view.toggle_no_recompute(cx));
                    }),
            )
            .child(
                Checkbox::new("oe-update-stats")
                    .checked(armed)
                    .label(crate::localization::text(
                        "explorer.object_editor.update_statistics_now",
                    ))
                    .on_click({
                        let this = cx.entity();
                        move |_: &bool, _window, app| {
                            this.update(app, |view, cx| view.arm_update_statistics(cx));
                        }
                    }),
            )
    }

    fn render_grid_page(
        &mut self,
        data: &PageData,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let s = Theme::scaled;
        if data.is_empty() {
            let copy = if self.active == EditorPage::Status {
                object_editor_model::status_empty_copy(self.profile.database_type)
            } else {
                empty_copy(self.target.kind, self.active)
            };
            return div()
                .p(px(s(16.0)))
                .child(empty_line(&copy))
                .into_any_element();
        }
        let mut page = div().flex().flex_col().size_full();
        if self.target.kind == EditorObjectKind::Server
            && matches!(self.active, EditorPage::Configuration | EditorPage::Status)
        {
            page = page.child(self.render_settings_search(data));
        }
        if let Some(grid) = self.grids.get(&self.active) {
            page = page.child(div().flex_1().overflow_hidden().child(grid.clone()));
        }
        if self.target.kind == EditorObjectKind::Table && self.active == EditorPage::Columns {
            page = page.child(self.render_column_edit_strip(window, cx));
        }
        if self.target.kind == EditorObjectKind::Server && self.active == EditorPage::Configuration
        {
            page = page.child(self.render_setting_edit_strip(window, cx));
        }
        page.into_any_element()
    }

    /// Search box above the server Configuration/Status grids, with the
    /// shown-of-total count.
    fn render_settings_search(&self, data: &PageData) -> AnyElement {
        let s = Theme::scaled;
        let total = match data {
            PageData::Settings(settings) => settings.len(),
            _ => 0,
        };
        let shown = self.visible_settings(self.active).len();
        let total_text = total.to_string();
        let shown_text = shown.to_string();
        let count = if self.settings_filter.trim().is_empty() {
            crate::localization::format(
                "explorer.object_editor.settings_count_all",
                &[("total", &total_text)],
            )
        } else {
            crate::localization::format(
                "explorer.object_editor.settings_count_filtered",
                &[("shown", &shown_text), ("total", &total_text)],
            )
        };
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(s(10.0)))
            .px(px(s(12.0)))
            .py(px(s(6.0)))
            .border_b_1()
            .border_color(Theme::border())
            .child(
                div()
                    .w(px(s(280.0)))
                    .child(Input::new(&self.settings_filter_input).cleanable(true)),
            )
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.78)))
                    .text_color(Theme::text_dim())
                    .child(count),
            )
            .into_any_element()
    }

    /// The Configuration page's edit strip: bound to the grid's selected row.
    /// Settings the engine marks dynamic get a value input that stages into
    /// the review-sheet plan (plus MySQL 8's SET PERSIST toggle); everything
    /// else gets the read-only reason. Nothing runs until Apply.
    fn render_setting_edit_strip(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let s = Theme::scaled;
        let selected = self.selected_setting(cx);
        if selected.as_ref().map(|st| st.name.as_str()) != self.strip_setting.as_deref() {
            self.strip_setting = selected.as_ref().map(|st| st.name.clone());
            let effective = self
                .strip_setting
                .as_ref()
                .and_then(|name| {
                    self.changes
                        .server_settings
                        .get(name)
                        .map(|c| c.value.clone())
                })
                .or_else(|| selected.as_ref().map(|st| st.value.clone()))
                .unwrap_or_default();
            self.setting_value_input.update(cx, |input, cx| {
                input.set_value(effective, window, cx);
            });
        }
        let note = |text: String| {
            div()
                .border_t_1()
                .border_color(Theme::border())
                .px(px(s(12.0)))
                .py(px(s(8.0)))
                .child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.78)))
                        .text_color(Theme::text_dim())
                        .child(text),
                )
                .into_any_element()
        };
        let Some(setting) = selected else {
            return note(crate::localization::text(
                "explorer.object_editor.setting_select_prompt",
            ));
        };
        if !setting.is_dynamic {
            let reason =
                object_editor_model::server_settings_edit_reason(self.profile.database_type)
                    .unwrap_or_else(|| {
                        crate::localization::text("explorer.object_editor.setting_not_dynamic")
                    });
            return note(crate::localization::format(
                "explorer.object_editor.setting_read_only",
                &[("name", &setting.name), ("reason", &reason)],
            ));
        }
        // SELECT-only, rollback-wrapped, and locked production connections
        // can't apply a setting, so they don't offer to stage one.
        if let Some(reason) = self.settings_profile_block() {
            return note(reason);
        }

        let staged = self.changes.server_settings.get(&setting.name);
        let staged_persist = staged.map(|change| change.persist).unwrap_or(false);
        let is_staged = staged.is_some();
        let is_reset = staged.is_some_and(|change| change.is_reset());
        let name = setting.name.clone();
        let this = cx.entity();
        let tds = self.profile.database_type.connection_kind()
            == sqlly_lang::sqlmodel::ConnectionKind::Tds;
        // SQL Server takes whole numbers and has no reset-to-default; the
        // other engines reset when the value is cleared.
        let hint_key = if tds {
            "explorer.object_editor.setting_hint_whole_number"
        } else {
            "explorer.object_editor.setting_hint_clear_default"
        };
        let hint = crate::localization::text(hint_key);

        let mut strip = div()
            .border_t_1()
            .border_color(Theme::border())
            .px(px(s(12.0)))
            .py(px(s(8.0)))
            .flex()
            .flex_row()
            .flex_wrap()
            .items_end()
            .gap(px(s(16.0)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(s(3.0)))
                    .child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.74)))
                            .text_color(Theme::text_dim())
                            .child(crate::localization::format(
                                "explorer.object_editor.setting_edit_label",
                                &[
                                    ("name", &setting.name),
                                    ("value", &setting.value),
                                    ("hint", &hint),
                                ],
                            )),
                    )
                    .child(
                        div()
                            .w(px(s(220.0)))
                            .child(Input::new(&self.setting_value_input)),
                    ),
            );
        // SET PERSIST is vanilla MySQL 8 only; MariaDB and the wire-compatibles
        // keep SET GLOBAL (until restart).
        if is_staged && self.profile.database_type == sqlly_lang::sqlmodel::DatabaseType::MySql {
            strip = strip.child(
                div().pb(px(s(2.0))).child(
                    Checkbox::new("oe-setting-persist")
                        .checked(staged_persist)
                        .label(crate::localization::text(
                            "explorer.object_editor.setting_persist",
                        ))
                        .on_click(move |_: &bool, _window, app| {
                            this.update(app, |view, cx| {
                                let persist = view
                                    .changes
                                    .server_settings
                                    .get(&name)
                                    .map(|change| change.persist)
                                    .unwrap_or(false);
                                view.changes.stage_server_setting_persist(&name, !persist);
                                cx.notify();
                            });
                        }),
                ),
            );
        }
        if !tds && !is_reset {
            strip = strip.child(Self::footer_button(
                "oe-setting-reset",
                crate::localization::text("explorer.object_editor.setting_reset_button"),
                ButtonVariant::Secondary,
                true,
                cx,
                |this, window, cx| {
                    this.setting_value_input.update(cx, |input, cx| {
                        input.set_value("", window, cx);
                    });
                },
            ));
        }
        if is_staged {
            strip = strip.child(
                div()
                    .pb(px(s(4.0)))
                    .text_size(px(Theme::font_size_ratio(0.78)))
                    .text_color(Theme::accent())
                    .child(crate::localization::text(if is_reset {
                        "explorer.object_editor.setting_reset_staged"
                    } else {
                        "explorer.object_editor.setting_staged"
                    })),
            );
        }
        if tds && setting.advanced && self.show_advanced_off() {
            strip = strip.child(
                div()
                    .pb(px(s(4.0)))
                    .text_size(px(Theme::font_size_ratio(0.78)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::format(
                        "explorer.object_editor.setting_advanced_hint",
                        &[(
                            "option",
                            &object_editor_model::SHOW_ADVANCED_OPTIONS.to_string(),
                        )],
                    )),
            );
        }
        if self.changes.server_settings.is_empty() {
            return strip.into_any_element();
        }
        // Live SQL preview of everything staged so far, in the engine's own
        // form — the same see-it-before-it-runs pattern as the security
        // dialog. Apply still opens the review sheet before anything runs.
        let preview = match self.engine_ddl_plan() {
            Ok(plan) => plan.join("\n"),
            Err(reason) => format!("-- {reason}"),
        };
        div()
            .flex()
            .flex_col()
            .child(strip)
            .child(
                div()
                    .id("oe-setting-sql-preview")
                    .max_h(px(s(120.0)))
                    .overflow_y_scroll()
                    .px(px(s(12.0)))
                    .py(px(s(6.0)))
                    .border_t_1()
                    .border_color(Theme::border())
                    .font_family(Theme::editor_font_family())
                    .text_size(px(Theme::font_size_ratio(0.78)))
                    .children(preview.lines().map(|line| div().child(line.to_string()))),
            )
            .into_any_element()
    }

    /// The Columns page's edit strip: bound to the grid's selected row, it
    /// stages column renames and NULL/NOT NULL flips. Computed columns cannot
    /// be altered, so their toggle is disabled.
    fn render_column_edit_strip(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let s = Theme::scaled;
        let selected = self.selected_column(cx);
        if selected.as_ref().map(|c| c.name.as_str()) != self.strip_column.as_deref() {
            self.strip_column = selected.as_ref().map(|c| c.name.clone());
            let name = self.strip_column.clone().unwrap_or_default();
            self.column_rename_input.update(cx, |input, cx| {
                input.set_value(name, window, cx);
            });
            self.column_rename_error = None;
        }
        let Some(column) = selected else {
            return div()
                .border_t_1()
                .border_color(Theme::border())
                .px(px(s(12.0)))
                .py(px(s(8.0)))
                .child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.78)))
                        .text_color(Theme::text_dim())
                        .child(crate::localization::text(
                            "explorer.object_editor.column_select_prompt",
                        )),
                )
                .into_any_element();
        };

        let staged_nullable = self
            .changes
            .column_nullability
            .get(&column.name)
            .map(|change| change.nullable)
            .unwrap_or(column.is_nullable);
        let this = cx.entity();
        let column_for_toggle = column.clone();

        let mut strip = div()
            .border_t_1()
            .border_color(Theme::border())
            .px(px(s(12.0)))
            .py(px(s(8.0)))
            .flex()
            .flex_row()
            .items_end()
            .gap(px(s(16.0)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(s(3.0)))
                    .child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.74)))
                            .text_color(Theme::text_dim())
                            .child(crate::localization::format(
                                "explorer.object_editor.column_rename_label",
                                &[("name", &column.name)],
                            )),
                    )
                    .child(
                        div()
                            .w(px(s(220.0)))
                            .child(Input::new(&self.column_rename_input)),
                    ),
            )
            .child(
                div().pb(px(s(2.0))).child(
                    Checkbox::new("oe-col-nullable")
                        .checked(staged_nullable)
                        .label(crate::localization::text(
                            "explorer.object_editor.column_null_allowed",
                        ))
                        .disabled(column.is_computed)
                        .on_click(move |_: &bool, _window, app| {
                            this.update(app, |view, cx| {
                                view.toggle_column_nullability(&column_for_toggle, cx);
                            });
                        }),
                ),
            );
        if let Some(error) = &self.column_rename_error {
            strip = strip.child(
                div()
                    .pb(px(s(4.0)))
                    .text_size(px(Theme::font_size_ratio(0.78)))
                    .text_color(Theme::error())
                    .child(error.clone()),
            );
        }
        strip.into_any_element()
    }

    fn render_source(&mut self, data: &PageData) -> AnyElement {
        let s = Theme::scaled;
        let PageData::Definition(definition) = data else {
            return div().into_any_element();
        };
        let Some(definition) = definition else {
            return div()
                .p(px(s(16.0)))
                .child(empty_line(&crate::localization::text(
                    "explorer.object_editor.no_stored_definition",
                )))
                .into_any_element();
        };
        let spans = classify(definition);
        let lines: Vec<&str> = definition.lines().collect();
        let mut line_start = 0usize;
        div()
            .id("oe-source")
            .track_scroll(&self.source_scroll)
            .flex_1()
            .w_full()
            .overflow_scroll()
            .px(px(s(12.0)))
            .py(px(s(8.0)))
            .font_family(Theme::editor_font_family())
            .children(lines.into_iter().map(|line| {
                let element = crate::ddl_viewer::render_sql_line(line, line_start, &spans);
                line_start += line.len() + 1;
                element
            }))
            .into_any_element()
    }

    fn render_review_sheet(&mut self, statements: &[String], cx: &mut Context<Self>) -> AnyElement {
        let s = Theme::scaled;
        let applying = matches!(self.apply, ApplyState::Applying);
        let summary = self.changes.summary_lines(self.target.kind);
        let sql = statements.join("\n");
        let spans = classify(&sql);

        let mut line_start = 0usize;
        let highlighted = sql
            .lines()
            .map(|line| {
                let element = crate::ddl_viewer::render_sql_line(line, line_start, &spans);
                line_start += line.len() + 1;
                element
            })
            .collect::<Vec<_>>();

        div()
            .flex()
            .flex_col()
            .size_full()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(s(4.0)))
                    .px(px(s(16.0)))
                    .py(px(s(10.0)))
                    .border_b_1()
                    .border_color(Theme::border())
                    .child(
                        div()
                            .text_size(px(Theme::font_size()))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(Theme::text())
                            .child(crate::localization::text(
                                "explorer.object_editor.review_changes",
                            )),
                    )
                    .children(summary.into_iter().map(|line| {
                        div()
                            .text_size(px(Theme::font_size_ratio(0.82)))
                            .text_color(Theme::text_dim())
                            .child(format!("• {line}"))
                    })),
            )
            .child(
                div()
                    .id("oe-review-sql")
                    .flex_1()
                    .w_full()
                    .overflow_scroll()
                    .px(px(s(16.0)))
                    .py(px(s(10.0)))
                    .font_family(Theme::editor_font_family())
                    .children(highlighted),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_end()
                    .gap(px(s(8.0)))
                    .px(px(s(16.0)))
                    .py(px(s(10.0)))
                    .border_t_1()
                    .border_color(Theme::border())
                    .child(Self::footer_button(
                        "oe-review-copy",
                        crate::localization::text("explorer.object_editor.review_copy_button"),
                        ButtonVariant::Secondary,
                        !applying,
                        cx,
                        move |this, _window, cx| {
                            if let ApplyState::Review(statements) = &this.apply {
                                cx.write_to_clipboard(ClipboardItem::new_string(
                                    statements.join("\n"),
                                ));
                            }
                        },
                    ))
                    .child(Self::footer_button(
                        "oe-review-cancel",
                        crate::localization::text("explorer.object_editor.review_cancel_button"),
                        ButtonVariant::Secondary,
                        !applying,
                        cx,
                        |this, _window, cx| this.cancel_apply(cx),
                    ))
                    .child(Self::footer_button(
                        "oe-review-execute",
                        crate::localization::text(if applying {
                            "explorer.object_editor.review_executing_button"
                        } else {
                            "explorer.object_editor.review_execute_button"
                        }),
                        ButtonVariant::Primary,
                        !applying,
                        cx,
                        |this, _window, cx| this.execute_apply(cx),
                    )),
            )
            .into_any_element()
    }
}

/// Execute the reviewed batch statement-by-statement, stopping at the first
/// server error, which comes back with the index of the statement that
/// failed. DDL returns no result sets; only the terminal/error frames
/// matter.
async fn run_statements(
    services: &Services,
    profile: &ConnectionProfile,
    statements: &[String],
) -> Result<(), (usize, String)> {
    for (index, sql) in statements.iter().enumerate() {
        let handle = services
            .queries
            .execute(profile, sql, 100)
            .await
            .map_err(|e| (index, crate::localization::message(e.message())))?;
        let mut events = handle.events;
        while let Some(event) = events.recv().await {
            match event {
                QueryEvent::Error { message } => return Err((index, message)),
                QueryEvent::Completed { .. } | QueryEvent::Cancelled => break,
                _ => {}
            }
        }
    }
    Ok(())
}

/// The data-row index behind the grid's first selected row. `selected_rows`
/// reports DISPLAY rows (after the user's header sort/filter); the page data
/// is indexed by the grid's SOURCE rows, so every selection → data lookup
/// goes through `resident_row_for_display`.
fn first_selected_source_row(state: &sqlly_datatable::GridState) -> Option<usize> {
    let display = state.selected_rows().first().copied()?;
    state.resident_row_for_display(display)
}

/// Whether a page carries pending edits (drives the page-list dirty dot).
fn page_dirty(kind: EditorObjectKind, page: EditorPage, changes: &PendingChanges) -> bool {
    match page {
        EditorPage::General => {
            changes.rename_object.is_some()
                || (kind == EditorObjectKind::Statistic
                    && (changes.statistic_no_recompute.is_some() || changes.update_statistics_now))
                || (matches!(kind, EditorObjectKind::Table | EditorObjectKind::View)
                    && changes.description.is_some())
        }
        EditorPage::Columns => {
            kind == EditorObjectKind::Table
                && (!changes.rename_columns.is_empty() || !changes.column_nullability.is_empty())
        }
        EditorPage::Configuration => {
            kind == EditorObjectKind::Server && !changes.server_settings.is_empty()
        }
        _ => false,
    }
}

fn property_row(row: &PropertyRow) -> gpui::Div {
    let s = Theme::scaled;
    div()
        .flex()
        .flex_row()
        .items_baseline()
        .gap(px(s(12.0)))
        .child(
            div()
                .w(px(s(140.0)))
                .flex_shrink_0()
                .text_size(px(Theme::font_size_ratio(0.82)))
                .text_color(Theme::text_dim())
                .child(row.label.clone()),
        )
        .child(
            div()
                .text_size(px(Theme::font_size_ratio(0.87)))
                .text_color(Theme::text())
                .child(row.value.clone()),
        )
}

fn empty_line(copy: &str) -> gpui::Div {
    div()
        .text_size(px(Theme::font_size_ratio(0.82)))
        .text_color(Theme::text_dim())
        .child(copy.to_string())
}

/// Empty-state copy per grid page.
fn empty_copy(kind: EditorObjectKind, page: EditorPage) -> String {
    let noun = kind.noun().to_lowercase();
    let formatted = |key: &str| crate::localization::format(key, &[("kind", &noun)]);
    match page {
        EditorPage::Columns => match kind {
            EditorObjectKind::Index => {
                crate::localization::text("explorer.object_editor.empty_index_columns")
            }
            EditorObjectKind::Statistic => {
                crate::localization::text("explorer.object_editor.empty_statistic_columns")
            }
            _ => formatted("explorer.object_editor.empty_columns"),
        },
        EditorPage::Indexes => crate::localization::text("explorer.object_editor.empty_indexes"),
        EditorPage::Statistics => {
            crate::localization::text("explorer.object_editor.empty_statistics")
        }
        EditorPage::Constraints => {
            crate::localization::text("explorer.object_editor.empty_constraints")
        }
        EditorPage::Triggers => crate::localization::text("explorer.object_editor.empty_triggers"),
        EditorPage::Parameters => formatted("explorer.object_editor.empty_parameters"),
        EditorPage::Permissions => {
            crate::localization::text("explorer.object_editor.empty_permissions")
        }
        EditorPage::Databases => {
            crate::localization::text("explorer.object_editor.empty_databases")
        }
        EditorPage::Configuration => {
            crate::localization::text("explorer.object_editor.empty_configuration")
        }
        EditorPage::Status => crate::localization::text("explorer.object_editor.empty_status"),
        EditorPage::Files => crate::localization::text("explorer.object_editor.empty_files"),
        EditorPage::Objects => formatted("explorer.object_editor.empty_objects"),
        _ => crate::localization::text("explorer.object_editor.empty_default"),
    }
}

impl Render for ObjectEditorView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let reviewing = matches!(self.apply, ApplyState::Review(_) | ApplyState::Applying);

        let body = if reviewing {
            let statements = match &self.apply {
                ApplyState::Review(statements) => statements.clone(),
                _ => self.engine_ddl_plan().unwrap_or_default(),
            };
            self.render_review_sheet(&statements, cx)
        } else {
            div()
                .flex()
                .flex_row()
                .flex_1()
                .overflow_hidden()
                .child(self.render_page_list(cx))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .overflow_hidden()
                        .bg(Theme::bg())
                        .child(self.render_content(window, cx)),
                )
                .into_any_element()
        };

        let dirty_count = self.changes.pending_count();
        let (status_color, status_text) = match &self.status {
            Some((StatusKind::Error, text)) => (Theme::error(), text.clone()),
            Some((StatusKind::Success, text)) => (Theme::success(), text.clone()),
            Some((StatusKind::Info, text)) => (Theme::text_dim(), text.clone()),
            None if dirty_count > 0 => {
                let key = if dirty_count == 1 {
                    "explorer.object_editor.pending_change"
                } else {
                    "explorer.object_editor.pending_changes"
                };
                (
                    Theme::text_dim(),
                    crate::localization::format(key, &[("count", &dirty_count.to_string())]),
                )
            }
            None => (Theme::text_dim(), String::new()),
        };
        let status_line = div()
            .text_size(px(Theme::font_size_ratio(0.8)))
            .text_color(status_color)
            .child(status_text);

        let focus_handle = self.focus_handle.clone();
        let mut dialog = Dialog::new("oe-dialog", |this: &mut Self| &mut this.frame)
            .title(self.target.qualified_name())
            .bare_body()
            .track_focus(&focus_handle)
            .on_close(|this, _window, cx| this.request_close(cx))
            .child(self.render_header())
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .overflow_hidden()
                    .child(body),
            );

        if self.confirm_discard {
            // Same second-chance semantics as the old footer: the scrim (and
            // the ✕) still route through request_close, which closes now that
            // the confirm is showing.
            dialog = dialog
                .leading_button(status_line)
                .button(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.82)))
                        .text_color(Theme::text())
                        .child(crate::localization::format(
                            "explorer.object_editor.discard_confirmation",
                            &[("count", &dirty_count.to_string())],
                        )),
                )
                .confirm(
                    crate::localization::text("explorer.object_editor.discard_button"),
                    crate::localization::text("explorer.object_editor.keep_editing_button"),
                )
                .danger()
                .dismiss_on_scrim_click(true)
                .on_confirm(|this, _window, cx| {
                    this.confirm_discard = false;
                    this.changes = PendingChanges::default();
                    cx.emit(ObjectEditorEvent::Closed);
                })
                .on_reject(|this, _window, cx| {
                    this.confirm_discard = false;
                    cx.notify();
                });
        } else {
            dialog = dialog
                .leading_button(Self::footer_button(
                    "oe-refresh",
                    crate::localization::text("explorer.object_editor.refresh_button"),
                    ButtonVariant::Secondary,
                    true,
                    cx,
                    |this, _window, cx| this.refresh_active_page(cx),
                ))
                .leading_button(Self::footer_button(
                    "oe-script",
                    crate::localization::text("explorer.object_editor.script_button"),
                    ButtonVariant::Secondary,
                    true,
                    cx,
                    |this, _window, cx| this.script_active_page(cx),
                ))
                .leading_button(status_line)
                .button(Self::footer_button(
                    "oe-apply",
                    crate::localization::text("explorer.object_editor.apply_button"),
                    ButtonVariant::Primary,
                    dirty_count > 0,
                    cx,
                    |this, _window, cx| this.begin_apply(cx),
                ));
        }
        dialog.build(self, window, cx)
    }
}

/// Progress of the General page's live-description read.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DescriptionRead {
    Pending,
    Loaded,
    Failed(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- Description facet gating (windowed) --------------------------------

    use sqlly_core::{CellValue, StudioResult};
    use sqlly_services::ProcedureParameter;
    use sqlly_services::metadata::{MetadataNode, MetadataService};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// How the fake answers the ObjectInventory read the facet waits on.
    #[derive(Clone)]
    enum InventoryReply {
        /// Never answers: the read is still in flight.
        Pending,
        Rows(Vec<Vec<CellValue>>),
        Fail(&'static str),
    }

    /// Metadata fake: every page read is empty; the inventory read answers
    /// per [`InventoryReply`] and is counted.
    struct DescriptionMetadata {
        reply: InventoryReply,
        inventory_calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl MetadataService for DescriptionMetadata {
        async fn list_schemas(&self, _: &ConnectionProfile, _: &str) -> StudioResult<Vec<String>> {
            Ok(Vec::new())
        }
        async fn list_tables(
            &self,
            _: &ConnectionProfile,
            _: &str,
            _: Option<&str>,
        ) -> StudioResult<Vec<MetadataNode>> {
            Ok(Vec::new())
        }
        async fn list_views(
            &self,
            _: &ConnectionProfile,
            _: &str,
            _: Option<&str>,
        ) -> StudioResult<Vec<MetadataNode>> {
            Ok(Vec::new())
        }
        async fn list_programmable_objects(
            &self,
            _: &ConnectionProfile,
            _: &str,
            _: Option<&str>,
        ) -> StudioResult<Vec<MetadataNode>> {
            Ok(Vec::new())
        }
        async fn list_columns(
            &self,
            _: &ConnectionProfile,
            _: &str,
            _: &str,
            _: &str,
        ) -> StudioResult<Vec<MetadataNode>> {
            Ok(Vec::new())
        }
        async fn list_procedure_parameters(
            &self,
            _: &ConnectionProfile,
            _: &str,
            _: &str,
            _: &str,
        ) -> StudioResult<Vec<ProcedureParameter>> {
            Ok(Vec::new())
        }
        async fn list_indexes(
            &self,
            _: &ConnectionProfile,
            _: &str,
            _: &str,
            _: &str,
        ) -> StudioResult<Vec<MetadataNode>> {
            Ok(Vec::new())
        }
        async fn list_triggers(
            &self,
            _: &ConnectionProfile,
            _: &str,
            _: &str,
            _: &str,
        ) -> StudioResult<Vec<MetadataNode>> {
            Ok(Vec::new())
        }
        async fn list_constraints(
            &self,
            _: &ConnectionProfile,
            _: &str,
            _: &str,
            _: &str,
        ) -> StudioResult<Vec<MetadataNode>> {
            Ok(Vec::new())
        }
        async fn list_statistics(
            &self,
            _: &ConnectionProfile,
            _: &str,
            _: &str,
            _: &str,
        ) -> StudioResult<Vec<MetadataNode>> {
            Ok(Vec::new())
        }
        async fn run_metadata_query(
            &self,
            _: &ConnectionProfile,
            _: &str,
            query: &MetadataQuery,
        ) -> StudioResult<Vec<Vec<CellValue>>> {
            if !matches!(query, MetadataQuery::ObjectInventory { .. }) {
                return Ok(Vec::new());
            }
            self.inventory_calls.fetch_add(1, Ordering::SeqCst);
            match self.reply.clone() {
                InventoryReply::Pending => std::future::pending().await,
                InventoryReply::Rows(rows) => Ok(rows),
                InventoryReply::Fail(message) => {
                    Err(sqlly_core::StudioError::Connection(message.to_string()))
                }
            }
        }
    }

    fn ensure_components_initialized(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            if cx.try_global::<gpui_component::Theme>().is_none() {
                gpui_component::init(cx);
            }
        });
    }

    /// Open the editor on `kind` `dbo.Customers` for `db`, with the inventory
    /// read answering `reply`. Returns the view, its window context, and the
    /// inventory call counter.
    fn open_editor(
        cx: &mut gpui::TestAppContext,
        db: sqlly_core::DatabaseType,
        kind: EditorObjectKind,
        reply: InventoryReply,
    ) -> (
        Entity<ObjectEditorView>,
        &mut gpui::VisualTestContext,
        Arc<AtomicUsize>,
    ) {
        ensure_components_initialized(cx);
        let calls = Arc::new(AtomicUsize::new(0));
        let real = Services::new();
        let services = Services::from_parts(
            real.connections.clone(),
            real.intellisense.clone(),
            real.queries.clone(),
            real.catalog.clone(),
            Arc::new(DescriptionMetadata {
                reply,
                inventory_calls: calls.clone(),
            }),
            real.admin_tools.clone(),
            real.row_editing.clone(),
        );
        let mut profile = ConnectionProfile::sql_login(
            "facet",
            "host",
            "sa",
            sqlly_core::SecretRef {
                provider: sqlly_core::SecretProvider::InMemory,
                key: String::new(),
            },
        );
        profile.database_type = db;
        let target = EditorTarget::new(kind, "Sales", "dbo", "Customers");
        let (view, cx) = cx.add_window_view(|window, cx| {
            ObjectEditorView::new(services, profile, target, false, window, cx)
        });
        cx.run_until_parked();
        (view, cx, calls)
    }

    /// Render the facet (which pushes a read value into the field) and
    /// return what the Description field then holds.
    fn render_facet(view: &Entity<ObjectEditorView>, cx: &mut gpui::VisualTestContext) -> String {
        view.update_in(cx, |view, window, cx| {
            let _ = view.render_description_facet(window, cx);
            view.description_input.read(cx).value().to_string()
        })
    }

    fn inventory_row(kind: &str, name: &str, comment: Option<&str>) -> Vec<CellValue> {
        let mut row = vec![
            CellValue::String(kind.into()),
            CellValue::String("dbo".into()),
            CellValue::String(name.into()),
        ];
        row.extend((3..7).map(|_| CellValue::Null));
        row.push(comment.map_or(CellValue::Null, |c| CellValue::String(c.into())));
        row
    }

    /// Until the live value has been read, the field is withheld: staging
    /// against an unknown baseline would pick the wrong SQL Server procedure.
    #[gpui::test]
    fn description_field_is_hidden_while_the_value_is_being_read(cx: &mut gpui::TestAppContext) {
        let (view, cx, calls) = open_editor(
            cx,
            sqlly_core::DatabaseType::SqlServer,
            EditorObjectKind::Table,
            InventoryReply::Pending,
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let notice = view.read_with(cx, |view, _| view.description_facet_notice());
        assert_eq!(notice.as_deref(), Some("Reading the current description…"));
        assert_eq!(render_facet(&view, cx), "");
    }

    #[gpui::test]
    fn description_field_shows_the_read_value(cx: &mut gpui::TestAppContext) {
        let rows = vec![
            inventory_row("TABLE", "Orders", Some("not this one")),
            inventory_row("TABLE", "Customers", Some("Customer master")),
        ];
        let (view, cx, _) = open_editor(
            cx,
            sqlly_core::DatabaseType::SqlServer,
            EditorObjectKind::Table,
            InventoryReply::Rows(rows),
        );
        let notice = view.read_with(cx, |view, _| view.description_facet_notice());
        assert_eq!(notice, None, "a read value makes the field editable");
        assert_eq!(render_facet(&view, cx), "Customer master");
    }

    #[gpui::test]
    fn description_field_is_hidden_with_the_reason_when_the_read_fails(
        cx: &mut gpui::TestAppContext,
    ) {
        let (view, cx, _) = open_editor(
            cx,
            sqlly_core::DatabaseType::SqlServer,
            EditorObjectKind::Table,
            InventoryReply::Fail("permission denied"),
        );
        let notice = view
            .read_with(cx, |view, _| view.description_facet_notice())
            .expect("a failed read withholds the field");
        assert!(notice.contains("could not be read"), "{notice}");
        assert!(notice.contains("permission denied"), "{notice}");
        assert_eq!(render_facet(&view, cx), "");
    }

    /// An engine that cannot store the description shows why, and never
    /// spends a round trip reading one.
    #[gpui::test]
    fn description_field_is_hidden_with_the_reason_when_unsupported(cx: &mut gpui::TestAppContext) {
        let (view, cx, calls) = open_editor(
            cx,
            sqlly_core::DatabaseType::MySql,
            EditorObjectKind::View,
            InventoryReply::Rows(vec![inventory_row("VIEW", "Customers", Some("VIEW"))]),
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0, "no inventory read");
        let notice = view
            .read_with(cx, |view, _| view.description_facet_notice())
            .expect("unsupported withholds the field");
        assert!(
            notice.contains("cannot store a description on a view"),
            "{notice}"
        );
        assert!(
            notice.ends_with("there is no description to edit."),
            "{notice}"
        );
        assert_eq!(render_facet(&view, cx), "");
    }

    /// Regression: the edit strips indexed their data with the grid's
    /// DISPLAY row, so after a header sort the Configuration strip bound to
    /// (and staged ALTER SYSTEM for) a different setting than the one the
    /// user clicked. The lookup must follow the selected row's source row.
    #[gpui::test]
    fn selection_maps_through_a_sorted_grid_to_the_source_row(cx: &mut gpui::TestAppContext) {
        let names = ["autovacuum", "max_connections", "work_mem"];
        let columns = ["Setting", "Value", "Source"]
            .iter()
            .map(|name| GridColumn {
                name: name.to_string(),
                kind: ColumnKind::Text,
                width: 120.0,
            })
            .collect();
        let rows = names
            .iter()
            .map(|name| {
                vec![
                    GridCell::Text(name.to_string()),
                    GridCell::Text("1".into()),
                    GridCell::Text("default".into()),
                ]
            })
            .collect();
        let data = GridData::new(columns, rows).unwrap();
        let table = cx.update(|cx| {
            SqllyDataTable::builder(data)
                .theme(crate::grid_bridge::build_grid_theme())
                .build(cx)
        });
        let state = table.state.clone();
        cx.update(|cx| {
            state.update(cx, |grid, _| {
                // Descending by name: work_mem is displayed first.
                grid.toggle_sort(0);
                grid.toggle_sort(0);
                grid.selection = sqlly_datatable::Selection::Row(0);
            });
        });
        let source = cx.update(|cx| first_selected_source_row(state.read(cx)));
        assert_eq!(
            source,
            Some(2),
            "display row 0 is source row 2 after the sort"
        );
        assert_eq!(names[source.unwrap()], "work_mem");
        // Unsorted, display and source agree.
        cx.update(|cx| {
            state.update(cx, |grid, _| {
                grid.toggle_sort(0);
                grid.selection = sqlly_datatable::Selection::Row(0);
            });
        });
        let source = cx.update(|cx| first_selected_source_row(state.read(cx)));
        assert_eq!(source, Some(0));
    }

    #[test]
    fn page_dirty_tracks_each_facet() {
        let mut changes = PendingChanges::default();
        assert!(!page_dirty(
            EditorObjectKind::Table,
            EditorPage::General,
            &changes
        ));
        changes.stage_rename("Orders", "Orders2").unwrap();
        assert!(page_dirty(
            EditorObjectKind::Table,
            EditorPage::General,
            &changes
        ));
        assert!(!page_dirty(
            EditorObjectKind::Table,
            EditorPage::Columns,
            &changes
        ));
    }

    #[test]
    fn page_dirty_marks_statistic_facets_on_general_only() {
        let mut changes = PendingChanges::default();
        changes.stage_no_recompute(false, true);
        assert!(page_dirty(
            EditorObjectKind::Statistic,
            EditorPage::General,
            &changes
        ));
        assert!(!page_dirty(
            EditorObjectKind::Statistic,
            EditorPage::Columns,
            &changes
        ));
    }

    #[test]
    fn page_dirty_marks_column_changes_on_columns_page() {
        let mut changes = PendingChanges::default();
        changes
            .stage_column_rename("Total", "Total", "OrderTotal")
            .unwrap();
        assert!(page_dirty(
            EditorObjectKind::Table,
            EditorPage::Columns,
            &changes
        ));
        assert!(!page_dirty(
            EditorObjectKind::Table,
            EditorPage::General,
            &changes
        ));
    }

    #[test]
    fn page_dirty_marks_description_on_general_for_tables_and_views() {
        let mut changes = PendingChanges::default();
        changes.stage_description("", "new text");
        assert!(page_dirty(
            EditorObjectKind::Table,
            EditorPage::General,
            &changes
        ));
        assert!(page_dirty(
            EditorObjectKind::View,
            EditorPage::General,
            &changes
        ));
        assert!(!page_dirty(
            EditorObjectKind::Procedure,
            EditorPage::General,
            &changes
        ));
        assert!(!page_dirty(
            EditorObjectKind::Table,
            EditorPage::Columns,
            &changes
        ));
    }

    #[test]
    fn empty_copy_names_the_object_and_page() {
        assert_eq!(
            empty_copy(EditorObjectKind::Table, EditorPage::Indexes),
            "This table has no indexes."
        );
        assert_eq!(
            empty_copy(EditorObjectKind::Procedure, EditorPage::Parameters),
            "This procedure has no parameters."
        );
    }
}
