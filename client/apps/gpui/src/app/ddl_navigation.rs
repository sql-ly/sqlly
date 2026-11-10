use super::*;

impl AppView {
    // ── DDL navigation (modifier-click → go to definition) ─────────────────

    /// Handle a modifier-click in a tab's editor: resolve the clicked symbol
    /// locally (no DB round trip on the click hot path), create the
    /// presentation surface in a loading state, then fetch the DDL async.
    pub(super) fn start_ddl_navigation(
        &mut self,
        tab_idx: usize,
        sql: gpui::SharedString,
        offset: usize,
        cx: &mut Context<Self>,
    ) {
        use sqlly_intel::intellisense::SymbolResolution;

        let Some(profile) = self.active_profile.clone() else {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.ddl.connect_navigation"),
            );
            cx.notify();
            return;
        };
        let database = profile
            .database
            .clone()
            .unwrap_or_else(|| "master".to_string());

        let target = match sqlly_intel::intellisense::resolve_symbol_at(
            &sql,
            offset,
            &self.completion_catalog,
            "dbo",
            Some(&database),
        ) {
            SymbolResolution::Resolved(target) => target,
            SymbolResolution::Ambiguous { name, candidates } => {
                self.set_status(
                    StatusLevel::Error,
                    crate::localization::format(
                        "shell.ddl.ambiguous_symbol",
                        &[("name", &name), ("candidates", &candidates.join(", "))],
                    ),
                );
                cx.notify();
                return;
            }
            SymbolResolution::NotFound => {
                self.set_status(
                    StatusLevel::Info,
                    crate::localization::text("shell.ddl.no_navigable_object"),
                );
                cx.notify();
                return;
            }
        };

        let title = crate::localization::format(
            "shell.ddl.viewer_title",
            &[("object", &target.qualified_name())],
        );
        let viewer = match self.ddl_nav_prefs.presentation {
            crate::preferences::DdlNavigationPresentation::OpenInNewTab => {
                self.open_ddl_tab(&title, cx)
            }
            crate::preferences::DdlNavigationPresentation::SidePanel => {
                self.show_ddl_panel(tab_idx, &title, cx)
            }
            crate::preferences::DdlNavigationPresentation::Popup => self.open_ddl_popup(&title, cx),
        };
        self.set_status(
            StatusLevel::Busy,
            crate::localization::format(
                "shell.ddl.fetching",
                &[("object", &target.qualified_name())],
            ),
        );
        cx.notify();
        // Keep the explorer focused on what the user is navigating to, when
        // the Data Model preference asks for it. Best-effort: only nodes the
        // tree has already loaded can be revealed.
        if crate::preferences::DataModelPreferences::load().auto_navigate_into_view {
            let server_id = profile.id.0;
            let schema = target.schema().to_string();
            let name = target.name().to_string();
            self.sidebar.update(cx, |sidebar, cx| {
                sidebar.reveal_object(Some(server_id), Some(&schema), &name, cx);
            });
        }
        self.fetch_ddl_into_viewer(profile, database, target, viewer, cx);
    }

    /// Opens a data-model object chosen in the project-file picker as a
    /// read-only DDL viewer tab, generating the definition server-side
    /// (`sp_helptext`-style via `fetch_ddl_document`).
    pub(super) fn open_model_object_ddl(
        &mut self,
        schema: Option<String>,
        name: String,
        kind: crate::project_file_picker_model::SqlObjectKind,
        cx: &mut Context<Self>,
    ) {
        use crate::project_file_picker_model::SqlObjectKind;
        use sqlly_intel::intellisense::{ClickedRole, NavigationTarget, RoutineKind};

        let Some(profile) = self.active_profile.clone() else {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.ddl.connect_model_definition"),
            );
            cx.notify();
            return;
        };
        let database = profile
            .database
            .clone()
            .unwrap_or_else(|| "master".to_string());
        let schema = schema.unwrap_or_else(|| "dbo".to_string());

        let target = match kind {
            SqlObjectKind::Table => NavigationTarget::Table {
                catalog: None,
                schema,
                name,
                clicked: ClickedRole::Table,
            },
            SqlObjectKind::View => NavigationTarget::Table {
                catalog: None,
                schema,
                name,
                clicked: ClickedRole::View,
            },
            SqlObjectKind::Sproc => NavigationTarget::Routine {
                catalog: None,
                schema,
                name,
                kind: RoutineKind::Procedure,
                clicked: ClickedRole::Procedure,
            },
            SqlObjectKind::Function => NavigationTarget::Routine {
                catalog: None,
                schema,
                name,
                kind: RoutineKind::Function,
                clicked: ClickedRole::Function,
            },
            SqlObjectKind::Type => NavigationTarget::Type {
                catalog: None,
                schema,
                name,
            },
            // Triggers/indexes never appear as model rows (the catalog
            // mapping skips them), and Unknown has no navigable target.
            SqlObjectKind::Trigger | SqlObjectKind::Index | SqlObjectKind::Unknown => {
                self.set_status(
                    StatusLevel::Info,
                    crate::localization::format("shell.ddl.no_viewer", &[("kind", kind.label())]),
                );
                cx.notify();
                return;
            }
        };

        let title = crate::localization::format(
            "shell.ddl.model_viewer_title",
            &[("object", &target.qualified_name())],
        );
        let viewer = self.open_ddl_tab(&title, cx);
        self.set_status(
            StatusLevel::Busy,
            crate::localization::format(
                "shell.ddl.model_fetching",
                &[("object", &target.qualified_name())],
            ),
        );
        cx.notify();
        self.fetch_ddl_into_viewer(profile, database, target, viewer, cx);
    }

    /// Fetches a DDL document on the tokio runtime and delivers it into the
    /// given viewer. Shared by editor go-to-definition and the project-file
    /// picker's data-model rows.
    pub(super) fn fetch_ddl_into_viewer(
        &mut self,
        profile: ConnectionProfile,
        database: String,
        target: sqlly_intel::intellisense::NavigationTarget,
        viewer: Entity<crate::ddl_viewer::DdlViewerView>,
        cx: &mut Context<Self>,
    ) {
        // Fetch on the tokio runtime (same pattern as `run_value_lookup`).
        // Routed per-profile: a Via-Relay profile's DDL comes over the relay.
        let services = self.services_for_profile(&profile);
        let ddl_options = crate::preferences::DdlNavigationPreferences::load().ddl_options();
        let (tx, rx) = tokio::sync::oneshot::channel::<
            sqlly_core::StudioResult<sqlly_services::DdlDocument>,
        >();
        cx.background_spawn(async move {
            crate::tokio_handle()
                .spawn(async move {
                    let result = sqlly_services::fetch_ddl_document_with(
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

        // The viewer entity was captured at request time, so a completing
        // fetch always lands in the surface that initiated it — the side
        // panel of the originating tab, never the now-active tab. A closed
        // connection surfaces as a controlled error in the same viewer.
        let async_cx = cx.to_async();
        let weak_self = cx.weak_entity();
        cx.spawn(async move |_this, _cx| {
            let Ok(result) = rx.await else { return };
            let mut cx = async_cx.clone();
            weak_self
                .update(&mut cx, |this, cx| {
                    match result {
                        Ok(document) => {
                            let (level, message) = if document.error.is_some() {
                                (
                                    StatusLevel::Warning,
                                    crate::localization::format(
                                        "shell.ddl.unavailable",
                                        &[("title", &document.title)],
                                    ),
                                )
                            } else {
                                (
                                    StatusLevel::Success,
                                    crate::localization::format(
                                        "shell.ddl.loaded",
                                        &[("title", &document.title)],
                                    ),
                                )
                            };
                            this.set_status(level, message);
                            viewer.update(cx, |v, cx| v.set_document(document, cx));
                        }
                        Err(e) => {
                            let error = crate::localization::message(e.message());
                            this.set_status(
                                StatusLevel::Error,
                                crate::localization::format(
                                    "shell.ddl.fetch_failed_status",
                                    &[("error", &error)],
                                ),
                            );
                            viewer.update(cx, |v, cx| {
                                v.set_error(
                                    crate::localization::format(
                                        "shell.ddl.fetch_failed_viewer",
                                        &[("error", &error)],
                                    ),
                                    cx,
                                );
                            });
                        }
                    }
                    cx.notify();
                })
                .ok();
        })
        .detach();
    }

    /// Open a read-only DDL viewer tab (never an editable query tab — that
    /// risks accidental execution of generated DDL). The tab's editor/results
    /// entities exist only to satisfy `QueryTab`; they are never rendered.
    /// Engine family of the active tab's connection. Dialect-aware UI (the
    /// DDL viewer's highlighting) parses as this rather than assuming T-SQL.
    pub(super) fn active_dialect(&self, cx: &gpui::App) -> sqlly_lang::sqlmodel::Dialect {
        self.tabs
            .get(self.active_tab)
            .map(|tab| tab.editor.read(cx).database_type().family())
            .unwrap_or(sqlly_lang::sqlmodel::Dialect::SqlServer)
    }

    pub(super) fn open_ddl_tab(
        &mut self,
        title: &str,
        cx: &mut Context<Self>,
    ) -> Entity<crate::ddl_viewer::DdlViewerView> {
        let dialect = self.active_dialect(cx);
        let viewer = cx
            .new(|_| crate::ddl_viewer::DdlViewerView::loading(title.to_string(), false, dialect));
        cx.subscribe(&viewer, Self::handle_ddl_viewer_event)
            .detach();

        let editor = cx.new(SqlEditor::new);
        let results = cx.new(ResultsView::new);
        self.tab_counter += 1;
        let mut tab = QueryTab::new(
            format!("tab-{}", self.tab_counter),
            title.to_string(),
            editor,
            results,
        );
        tab.ddl_view = Some(viewer.clone());
        self.tabs.push(tab);
        self.active_tab = self.tabs.len() - 1;
        // A DDL viewer tab cannot host a same-buffer split; this closes it.
        self.retarget_same_buffer_view(cx);
        self.sync_toolbar_to_active_tab();
        viewer
    }

    /// Show (or replace) the DDL side panel on the originating tab.
    pub(super) fn show_ddl_panel(
        &mut self,
        tab_idx: usize,
        title: &str,
        cx: &mut Context<Self>,
    ) -> Entity<crate::ddl_viewer::DdlViewerView> {
        let dialect = self.active_dialect(cx);
        let viewer =
            cx.new(|_| crate::ddl_viewer::DdlViewerView::loading(title.to_string(), true, dialect));
        cx.subscribe(&viewer, Self::handle_ddl_viewer_event)
            .detach();
        if let Some(tab) = self.tabs.get_mut(tab_idx) {
            tab.ddl_panel = Some(viewer.clone());
            tab.ddl_split = true;
        }
        viewer
    }

    /// Open the DDL popup window. Placement comes from the popup preferences
    /// (size, position fractions, monitor), clamped to the selected monitor
    /// so the window can never open offscreen. Window creation is deferred
    /// so it runs after the in-progress main-window update completes.
    pub(super) fn open_ddl_popup(
        &mut self,
        title: &str,
        cx: &mut Context<Self>,
    ) -> Entity<crate::ddl_viewer::DdlViewerView> {
        let dialect = self.active_dialect(cx);
        let viewer = cx
            .new(|_| crate::ddl_viewer::DdlViewerView::loading(title.to_string(), false, dialect));
        cx.subscribe(&viewer, Self::handle_ddl_viewer_event)
            .detach();

        let popup_prefs = self.ddl_nav_prefs.popup.clone();
        let window_title = gpui::SharedString::from(title.to_string());
        let popup_viewer = viewer.clone();
        cx.defer(move |cx: &mut gpui::App| {
            let monitors = crate::ddl_navigation::detect_monitors(cx);
            let current = cx.active_window().and_then(|handle| {
                handle
                    .update(cx, |_, window, cx| {
                        crate::ddl_navigation::current_window_monitor(window, cx)
                    })
                    .ok()
                    .flatten()
            });
            let selected = crate::ddl_navigation::select_monitor(
                &popup_prefs.monitor,
                &monitors,
                current.as_ref(),
            );
            let (bounds, display_id) = match selected {
                Some(monitor) => (
                    crate::ddl_navigation::popup_bounds(&popup_prefs, monitor.bounds),
                    monitor.id,
                ),
                // No display information at all: center a default-sized
                // window and let the platform place it.
                None => (
                    Bounds::centered(
                        None,
                        size(
                            px(popup_prefs.clamped().width as f32),
                            px(popup_prefs.clamped().height as f32),
                        ),
                        cx,
                    ),
                    None,
                ),
            };
            let _ = cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    display_id,
                    titlebar: Some(gpui::TitlebarOptions {
                        title: Some(window_title),
                        ..Default::default()
                    }),
                    app_id: Some(crate::about::APP_ID.into()),
                    is_resizable: true,
                    ..Default::default()
                },
                move |window, cx| cx.new(|cx| gpui_component::Root::new(popup_viewer, window, cx)),
            );
        });
        viewer
    }

    pub(super) fn handle_ddl_viewer_event(
        &mut self,
        viewer: Entity<crate::ddl_viewer::DdlViewerView>,
        event: &crate::ddl_viewer::DdlViewerEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            crate::ddl_viewer::DdlViewerEvent::Close => {
                for tab in &mut self.tabs {
                    if tab.ddl_panel.as_ref() == Some(&viewer) {
                        tab.ddl_panel = None;
                        tab.ddl_split = false;
                    }
                }
                cx.notify();
            }
            crate::ddl_viewer::DdlViewerEvent::OpenAsQuery(sql) => {
                self.new_tab(cx, Some(sql.clone()));
                cx.notify();
            }
        }
    }

    /// Schedule a debounced auto-refresh of the LSP debug panel (300 ms). Only
    /// meaningful while the panel is visible — the caller gates on
    /// `is_visible()`. Cancels any prior pending debounce by replacing the
    /// `lsp_debug_debounce_task` field. Mirrors the `schedule_fim_completion`
    /// debounce pattern.
    pub(super) fn schedule_lsp_debug_refresh(&mut self, cx: &mut Context<Self>) {
        self.lsp_debug_debounce_task = None;
        let weak_self = cx.weak_entity();
        let async_cx = cx.to_async();
        self.lsp_debug_debounce_task = Some(cx.spawn(async move |_this, _cx| {
            async_cx
                .background_executor()
                .timer(std::time::Duration::from_millis(300))
                .await;
            let mut cx = async_cx.clone();
            weak_self
                .update(&mut cx, |this, cx| {
                    this.refresh_lsp_debug(cx);
                })
                .ok();
        }));
    }

    /// Schedule a debounced auto-validation of the active tab's SQL. Mirrors
    /// the Swift app's `scheduleValidation()`: cancel any pending validation,
    /// increment the token, start a 2-second debounce timer, then fire
    /// `validate_sql` which calls `Services::catalog::validate_sql` and
    /// surfaces diagnostics in the editor + results diagnostics tab.
    pub(super) fn schedule_validation(&mut self, tab_idx: usize, cx: &mut Context<Self>) {
        self.validation_debounce_task = None;
        let token = self.validation_token + 1;
        self.validation_token = token;
        let weak_self = cx.weak_entity();
        let mut async_cx = cx.to_async();
        self.validation_debounce_task = Some(cx.spawn(async move |_this, _cx| {
            async_cx
                .background_executor()
                .timer(std::time::Duration::from_millis(2000))
                .await;
            weak_self
                .update(&mut async_cx, |this, cx| {
                    if this.validation_token != token {
                        return;
                    }
                    this.validate_sql_silent(tab_idx, cx);
                })
                .ok();
        }));
    }
}
