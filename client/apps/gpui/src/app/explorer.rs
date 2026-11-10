use super::*;

impl AppView {
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn handle_snapshot_browser_event(
        &mut self,
        _view: Entity<crate::result_snapshots::SnapshotBrowser>,
        event: &crate::result_snapshots::SnapshotBrowserEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            crate::result_snapshots::SnapshotBrowserEvent::Open { name, text } => {
                match crate::import_export::parse_sqlly_export_snapshot(text) {
                    Ok(snapshot) => {
                        self.install_sqlly_export(
                            std::path::PathBuf::from(format!("{name}.sqllyexport")),
                            snapshot,
                            cx,
                        );
                    }
                    Err(error) => {
                        self.set_status(
                            StatusLevel::Error,
                            crate::localization::format(
                                "shell.explorer.snapshot_open_failed",
                                &[("error", &error)],
                            ),
                        );
                    }
                }
                cx.notify();
            }
            crate::result_snapshots::SnapshotBrowserEvent::Closed => {
                self.overlays.snapshot_browser = None;
                cx.notify();
            }
        }
    }

    /// Open the Relationship Graph for the active connection's database.
    pub(super) fn handle_relationship_graph(
        &mut self,
        _action: &RelationshipGraph,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_relationship_graph_focused(None, window, cx);
    }

    /// Open the visual query builder for the active connection.
    pub(super) fn handle_query_builder(
        &mut self,
        _action: &QueryBuilder,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.overlays.query_builder.is_some() {
            return;
        }
        let Some(profile) = self.active_profile.clone() else {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.explorer.connect_query_builder"),
            );
            cx.notify();
            return;
        };
        let context = crate::tool_dialog::ToolDialogContext::new(
            profile.clone(),
            self.services_for_profile(&profile),
        );
        let view = cx.new(|cx| {
            crate::query_builder::QueryBuilderView::new(
                context,
                self.completion_catalog.clone(),
                profile.database_type,
                crate::preferences::load_sql_formatting_preferences(),
                self.datamodel_refreshes_in_flight > 0,
                window,
                cx,
            )
        });
        cx.subscribe(&view, Self::handle_query_builder_event)
            .detach();
        self.overlays.query_builder = Some(view);
        cx.notify();
    }

    pub(super) fn open_query_builder_with_table(
        &mut self,
        schema: &str,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.overlays.query_builder.is_none() {
            self.handle_query_builder(&QueryBuilder, window, cx);
        }
        let schema = schema.to_string();
        let name = name.to_string();
        if let Some(view) = self.overlays.query_builder.clone() {
            view.update(cx, |v, cx| v.add_table_named(&schema, &name, cx));
        }
    }

    pub(super) fn handle_query_builder_event(
        &mut self,
        _view: Entity<crate::query_builder::QueryBuilderView>,
        event: &crate::query_builder::QueryBuilderEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            crate::query_builder::QueryBuilderEvent::InsertIntoEditor { sql } => {
                let sql = sql.clone();
                let fallback = self
                    .tabs
                    .get(self.active_tab)
                    .is_none_or(|tab| tab.ddl_view.is_some() || tab.notebook.is_some());
                if fallback {
                    self.new_tab(cx, Some(sql));
                } else if let Some(tab) = self.tabs.get(self.active_tab) {
                    tab.editor.update(cx, |ed, cx| ed.insert(&sql, cx));
                }
                self.overlays.query_builder = None;
                self.set_status(
                    StatusLevel::Info,
                    crate::localization::text("shell.explorer.generated_query_inserted"),
                );
                cx.notify();
            }
            crate::query_builder::QueryBuilderEvent::Run { sql } => {
                self.apply_tool_dialog_event(
                    &crate::tool_dialog::ToolDialogEvent::ScriptToEditorAndRun { sql: sql.clone() },
                    cx,
                );
                self.overlays.query_builder = None;
                cx.notify();
            }
            crate::query_builder::QueryBuilderEvent::Tool(tool) => {
                let close = matches!(tool, crate::tool_dialog::ToolDialogEvent::Close)
                    || self.apply_tool_dialog_event(tool, cx);
                if close {
                    self.overlays.query_builder = None;
                }
                cx.notify();
            }
            crate::query_builder::QueryBuilderEvent::Close => {
                self.overlays.query_builder = None;
                cx.notify();
            }
        }
    }

    pub(super) fn open_relationship_graph_focused(
        &mut self,
        focus: Option<(String, String)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.overlays.er_diagram = None;
        let Some(profile) = self.active_profile.clone() else {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.explorer.connect_relationships"),
            );
            cx.notify();
            return;
        };
        let database = profile.database.clone().unwrap_or_default();
        let context = crate::tool_dialog::ToolDialogContext::new(
            profile.clone(),
            self.services_for_profile(&profile),
        );
        let view = cx
            .new(|cx| crate::er_diagram::ErDiagramView::new(context, database, focus, window, cx));
        cx.subscribe(&view, Self::handle_er_diagram_event).detach();
        self.overlays.er_diagram = Some(view);
        cx.notify();
    }

    pub(super) fn handle_er_diagram_event(
        &mut self,
        _view: Entity<crate::er_diagram::ErDiagramView>,
        event: &crate::er_diagram::ErDiagramEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            crate::er_diagram::ErDiagramEvent::RevealObject { schema, name } => {
                let server_id = self.active_profile.as_ref().map(|profile| profile.id.0);
                let schema = schema.clone();
                let name = name.clone();
                let revealed = self.sidebar.update(cx, |sidebar, cx| {
                    sidebar.reveal_object(server_id, Some(&schema), &name, cx)
                });
                if revealed {
                    self.sidebar_visible = true;
                    self.set_status(
                        StatusLevel::Info,
                        crate::localization::format(
                            "shell.explorer.object_revealed",
                            &[("name", &name)],
                        ),
                    );
                } else {
                    self.set_status(
                        StatusLevel::Info,
                        crate::localization::format(
                            "shell.explorer.object_not_loaded",
                            &[("name", &name)],
                        ),
                    );
                }
                cx.notify();
            }
            crate::er_diagram::ErDiagramEvent::Status { level, message } => {
                self.set_status(*level, message.clone());
                cx.notify();
            }
            crate::er_diagram::ErDiagramEvent::Close => {
                self.overlays.er_diagram = None;
                cx.notify();
            }
            crate::er_diagram::ErDiagramEvent::ScriptToEditor { sql, profile } => {
                // "Script as CREATE" lands in a reviewed editor tab bound to
                // the diagram's own connection and database — the workbench
                // may have switched while the script was being built (or the
                // diagram is a popped-out window over another connection).
                // The diagram posts its own status line.
                let sql = sql.clone();
                self.with_explorer_target_active(
                    Some(profile.as_ref().clone()),
                    cx,
                    move |app, cx| {
                        app.new_tab(cx, Some(sql));
                        cx.notify();
                    },
                );
            }
            crate::er_diagram::ErDiagramEvent::PopOut { selection, focus } => {
                // Open the standalone window here (not in the view) so this
                // host can subscribe to the new view — clicking a table in the
                // popped-out window must still reveal it in the schema tree.
                if let Some(profile) = self.active_profile.clone() {
                    let database = profile.database.clone().unwrap_or_default();
                    let context = crate::tool_dialog::ToolDialogContext::new(
                        profile.clone(),
                        self.services_for_profile(&profile),
                    );
                    if let Some(view) = crate::er_diagram::open_er_diagram_window(
                        context,
                        database,
                        selection.clone(),
                        focus.clone(),
                        cx,
                    ) {
                        cx.subscribe(&view, Self::handle_er_diagram_event).detach();
                    }
                }
                // The modal has served its purpose either way.
                self.overlays.er_diagram = None;
                cx.notify();
            }
        }
    }

    /// Open the Connection Health dashboard (Tools menu / palette).
    pub(super) fn handle_connection_health(
        &mut self,
        _action: &ConnectionHealth,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.overlays.connection_health_dialog.is_some() {
            return;
        }
        let router = self.service_router.clone();
        let view = cx.new(|cx| {
            crate::connection_health::ConnectionHealthDashboard::with_router(router, window, cx)
        });
        // The dashboard's row dots show the explorer's live per-server state —
        // one source of connection truth, two views.
        let live_states = self.sidebar.read(cx).connection_states(cx);
        view.update(cx, |dashboard, cx| {
            dashboard.set_live_states(live_states, cx)
        });
        cx.subscribe(
            &view,
            |this: &mut Self,
             _view,
             event: &crate::tool_dialog::ToolDialogEvent,
             cx: &mut Context<Self>| {
                if matches!(event, crate::tool_dialog::ToolDialogEvent::Close) {
                    this.overlays.connection_health_dialog = None;
                    cx.notify();
                }
            },
        )
        .detach();
        self.overlays.connection_health_dialog = Some(view);
        cx.notify();
    }

    pub(super) fn handle_sidebar_event(
        &mut self,
        _sidebar: &Entity<Sidebar>,
        event: &SidebarEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            SidebarEvent::ConnectRequested(id) => {
                // Enter / double-click on a server row. This used to swap the
                // saved profile straight into `active_profile`, bypassing the
                // connect path (no tab pin, no bound server, no relay or
                // tunnel setup) — and since a saved profile usually names no
                // database, activating the server you were already on quietly
                // moved every later query to the login's default database
                // while the query bar still showed the one you had picked.
                let store = ConnectionStore::global();
                if let Some(mut profile) = store.get(*id) {
                    // Re-activating the active server keeps the query-bar
                    // database when the saved profile names none: that choice
                    // is the only statement of context there is (same rule as
                    // `adopt_saved_profile`).
                    if let Some(active) = self.active_profile.as_ref()
                        && active.id == profile.id
                        && profile.database.as_deref().unwrap_or("").is_empty()
                    {
                        profile.database = active.database.clone();
                    }
                    self.connect_profile(profile, cx);
                }
            }
            SidebarEvent::DisconnectRequested => {
                self.active_profile = None;
                self.agent_write_grant = None;
                self.sidebar.update(cx, |s, cx| {
                    s.set_bound_server(None, cx);
                });
                self.ai_panel.update(cx, |p, cx| {
                    p.set_profile(None, cx);
                });
                self.clear_completion_catalogs(cx);
                self.clear_auto_refresh_on_disconnect(cx);
                cx.notify();
            }
            SidebarEvent::RefreshSchema => {
                // Refresh the server the focused query window is bound to —
                // the action's context — not a global "active server".
                if let Some(id) = self.active_profile.as_ref().map(|p| p.id.0) {
                    self.sidebar.update(cx, |s, cx| {
                        s.refresh_server_schema(id, cx);
                    });
                }
                if let Some(profile) = self.active_profile.clone() {
                    self.refresh_completion_catalog(profile, cx);
                }
                self.set_status(
                    StatusLevel::Success,
                    crate::localization::text("shell.explorer.schema_refreshed_status"),
                );
                cx.notify();
            }
            SidebarEvent::AddConnectionRequested => {
                tracing::debug!("sidebar event: AddConnectionRequested -> open connection editor");
                self.open_new_connection_editor(window, cx);
            }
            SidebarEvent::ManageConnectionsRequested => {
                tracing::debug!(
                    "sidebar event: ManageConnectionsRequested -> open connection management"
                );
                self.handle_show_connection_management(&ShowConnectionManagement, window, cx);
            }
            // Editing and deleting a connection now reach the app through the
            // row's context menu (`ExplorerMenuAction::EditConnection` /
            // `DeleteConnection`), which is the same path the right-click menu
            // has always used. The sidebar's own two events were a second,
            // identical route that existed only to serve the inline
            // `edit`/`del` links.
            SidebarEvent::LinkAzureDatabase(discovery) => {
                // A click in the explorer adopts the discovered database as a
                // saved connection and stops there. Clicking a *saved* server
                // does not retarget the query tab, and a discovered one must not
                // either — the row being new is no reason to move the editor,
                // its completions, and the next execution onto it. Connecting is
                // the row's "Link & Connect" menu item.
                let profile = self.adopt_azure_database(discovery, cx);
                self.set_status(
                    StatusLevel::Success,
                    crate::localization::format(
                        "shell.explorer.azure_linked",
                        &[("name", &profile.name)],
                    ),
                );
                cx.notify();
            }
            SidebarEvent::PanelChanged(panel) => {
                let _ = panel;
                self.save_workspace_session(cx);
            }
            #[cfg(not(target_arch = "wasm32"))]
            SidebarEvent::Library(event) => {
                self.handle_sql_library_event(event, window, cx);
            }
            SidebarEvent::SchemaTree(event) => match event {
                SchemaTreeEvent::SelectObject {
                    schema,
                    name,
                    server_id,
                    database,
                } => {
                    use crate::table_actions::{QualifiedTable, select_top_rows_sql_for};
                    // Retarget the active connection to the server/database the
                    // node lives on, so the SELECT runs where the user clicked
                    // rather than against whatever connection happens to be
                    // active. Falls back to the current connection when the node
                    // carries no resolvable server id.
                    let target =
                        self.resolve_explorer_target(server_id.as_ref(), database.as_deref(), cx);
                    // Generate the SELECT for the target's dialect (known
                    // synchronously from the profile), not the possibly-stale
                    // active connection.
                    let db_type = target
                        .as_ref()
                        .map(|p| p.database_type)
                        .unwrap_or_else(|| self.active_db_type());
                    let qt = QualifiedTable::new(name).with_schema(schema);
                    let sql = select_top_rows_sql_for(db_type, 1000, &qt);
                    self.with_explorer_target_active(target, cx, move |app, cx| {
                        app.new_tab(cx, Some(sql));
                        app.set_status(
                            StatusLevel::Info,
                            crate::localization::text("shell.explorer.select_query_opened"),
                        );
                        cx.notify();
                    });
                }
                SchemaTreeEvent::DesignQuery {
                    schema,
                    name,
                    server_id,
                    database,
                } => {
                    let target =
                        self.resolve_explorer_target(server_id.as_ref(), database.as_deref(), cx);
                    let schema = schema.clone();
                    let name = name.clone();
                    match target {
                        Some(profile) => {
                            match retarget_decision(self.active_profile.as_ref(), &profile) {
                                RetargetDecision::Connect => {
                                    self.connect_profile_then(profile, cx, move |app, cx| {
                                        app.overlays.pending_query_builder = Some((schema, name));
                                        cx.notify();
                                    });
                                }
                                RetargetDecision::SwitchDatabase(db) => {
                                    self.switch_database(db, cx);
                                    self.open_query_builder_with_table(&schema, &name, window, cx);
                                }
                                RetargetDecision::AlreadyActive => {
                                    self.open_query_builder_with_table(&schema, &name, window, cx);
                                }
                            }
                        }
                        None => {
                            self.open_query_builder_with_table(&schema, &name, window, cx);
                        }
                    }
                }
                SchemaTreeEvent::OpenSqlInNewTab {
                    sql,
                    server_id,
                    database,
                } => {
                    // Never write into the focused editor: `set_text` clears the
                    // undo stack, so scripting a table from the tree used to
                    // destroy unsaved work unrecoverably. A new tab is also what
                    // double-clicking a table does, so both tree paths now agree.
                    // Retargets to the node's server/database first, exactly
                    // like double-click — a context-menu "Select Top 1000" on a
                    // browsed non-active server must not run elsewhere.
                    let target =
                        self.resolve_explorer_target(server_id.as_ref(), database.as_deref(), cx);
                    let sql = sql.clone();
                    self.with_explorer_target_active(target, cx, move |app, cx| {
                        app.new_tab(cx, Some(sql));
                        app.set_status(
                            StatusLevel::Info,
                            crate::localization::text("shell.explorer.sql_opened_new_tab"),
                        );
                        cx.notify();
                    });
                }
                SchemaTreeEvent::RunCommandInNewTab {
                    sql,
                    title,
                    server_id,
                    database,
                } => {
                    // The key browser's Open Value: a read the user chose by
                    // clicking the key, so it runs at once (every execute
                    // gate still applies) on the key's own server/database.
                    let target =
                        self.resolve_explorer_target(server_id.as_ref(), database.as_deref(), cx);
                    let sql = sql.clone();
                    let title = title.clone();
                    self.with_explorer_target_active(target, cx, move |app, cx| {
                        app.new_tab(cx, Some(sql));
                        let tab_idx = app.active_tab;
                        // A capped reader names its cap in the tab title so
                        // "first 1,000" is never mistaken for the whole value.
                        if let Some(title) = title
                            && let Some(tab) = app.tabs.get_mut(tab_idx)
                        {
                            tab.name = title;
                            tab.auto_named = false;
                        }
                        app.execute_query(tab_idx, cx);
                        cx.notify();
                    });
                }
                SchemaTreeEvent::OpenRedisKey {
                    key,
                    kind,
                    server_id,
                    database,
                } => {
                    // The key view runs on the key's own server and logical
                    // database (never the active tab's); the browser build
                    // has no key view and runs the reader in a tab instead.
                    #[cfg(not(target_arch = "wasm32"))]
                    {
                        let _ = kind;
                        let target = self.resolve_explorer_target(
                            server_id.as_ref(),
                            database.as_deref(),
                            cx,
                        );
                        let Some(profile) = target else {
                            self.set_status(
                                StatusLevel::Info,
                                crate::localization::text("shell.explorer.connect_redis_first"),
                            );
                            cx.notify();
                            return;
                        };
                        self.open_redis_key_view(profile, Some(key.clone().into_bytes()), cx);
                    }
                    #[cfg(target_arch = "wasm32")]
                    {
                        let target = self.resolve_explorer_target(
                            server_id.as_ref(),
                            database.as_deref(),
                            cx,
                        );
                        let sql = crate::schema_tree::redis::read_command(key, kind);
                        self.with_explorer_target_active(target, cx, move |app, cx| {
                            app.new_tab(cx, Some(sql));
                            let tab_idx = app.active_tab;
                            app.execute_query(tab_idx, cx);
                            cx.notify();
                        });
                    }
                }
                #[cfg(not(target_arch = "wasm32"))]
                SchemaTreeEvent::SearchDatabaseValue {
                    server_id,
                    database,
                } => {
                    let target =
                        self.resolve_explorer_target(server_id.as_ref(), database.as_deref(), cx);
                    match target {
                        Some(profile) => {
                            match retarget_decision(self.active_profile.as_ref(), &profile) {
                                RetargetDecision::Connect => {
                                    self.connect_profile_then(profile, cx, move |app, cx| {
                                        app.overlays.pending_database_search = true;
                                        cx.notify();
                                    });
                                }
                                RetargetDecision::SwitchDatabase(db) => {
                                    self.switch_database(db, cx);
                                    self.overlays.pending_database_search = true;
                                    cx.notify();
                                }
                                RetargetDecision::AlreadyActive => {
                                    self.overlays.pending_database_search = true;
                                    cx.notify();
                                }
                            }
                        }
                        None => {
                            self.overlays.pending_database_search = true;
                            cx.notify();
                        }
                    }
                }
                #[cfg(not(target_arch = "wasm32"))]
                SchemaTreeEvent::RunSqlFile {
                    server_id,
                    database,
                } => {
                    let target =
                        self.resolve_explorer_target(server_id.as_ref(), database.as_deref(), cx);
                    match target {
                        Some(profile) => {
                            match retarget_decision(self.active_profile.as_ref(), &profile) {
                                RetargetDecision::Connect => {
                                    self.connect_profile_then(profile, cx, move |app, cx| {
                                        app.overlays.pending_run_sql_file = true;
                                        cx.notify();
                                    });
                                }
                                RetargetDecision::SwitchDatabase(db) => {
                                    self.switch_database(db, cx);
                                    self.overlays.pending_run_sql_file = true;
                                    cx.notify();
                                }
                                RetargetDecision::AlreadyActive => {
                                    self.overlays.pending_run_sql_file = true;
                                    cx.notify();
                                }
                            }
                        }
                        None => {
                            self.overlays.pending_run_sql_file = true;
                            cx.notify();
                        }
                    }
                }
                #[cfg(target_arch = "wasm32")]
                SchemaTreeEvent::RunSqlFile { .. } => {
                    self.set_status(
                        StatusLevel::Error,
                        crate::browser_preview::unavailable(&crate::localization::text(
                            "shell.explorer.run_sql_file_unavailable",
                        )),
                    );
                    cx.notify();
                }
                #[cfg(target_arch = "wasm32")]
                SchemaTreeEvent::SearchDatabaseValue { .. } => {
                    self.set_status(
                        StatusLevel::Error,
                        crate::browser_preview::unavailable(&crate::localization::text(
                            "shell.explorer.database_search_unavailable",
                        )),
                    );
                    cx.notify();
                }
                SchemaTreeEvent::BrowseObjects {
                    server_id,
                    database,
                    schema,
                } => {
                    let target =
                        self.resolve_explorer_target(server_id.as_ref(), database.as_deref(), cx);
                    let Some(profile) = target.or_else(|| self.active_profile.clone()) else {
                        self.set_status(
                            StatusLevel::Info,
                            crate::localization::text("shell.explorer.connect_browse_objects"),
                        );
                        cx.notify();
                        return;
                    };
                    self.open_object_browser_for(
                        profile,
                        database.clone().unwrap_or_default(),
                        schema.clone(),
                        window,
                        cx,
                    );
                }
                #[cfg(not(target_arch = "wasm32"))]
                SchemaTreeEvent::ExportDatabaseSql {
                    server_id,
                    database,
                } => {
                    let target =
                        self.resolve_explorer_target(server_id.as_ref(), database.as_deref(), cx);
                    let database = database.clone();
                    match target {
                        Some(profile) => {
                            match retarget_decision(self.active_profile.as_ref(), &profile) {
                                RetargetDecision::Connect => {
                                    self.connect_profile_then(profile, cx, move |app, cx| {
                                        app.overlays.pending_database_dump = Some(database);
                                        cx.notify();
                                    });
                                }
                                RetargetDecision::SwitchDatabase(db) => {
                                    self.switch_database(db, cx);
                                    self.open_database_dump_dialog(database, window, cx);
                                }
                                RetargetDecision::AlreadyActive => {
                                    self.open_database_dump_dialog(database, window, cx);
                                }
                            }
                        }
                        None => self.open_database_dump_dialog(database, window, cx),
                    }
                }
                #[cfg(target_arch = "wasm32")]
                SchemaTreeEvent::ExportDatabaseSql { .. } => {
                    self.set_status(
                        StatusLevel::Error,
                        crate::browser_preview::unavailable(&crate::localization::text(
                            "shell.explorer.dump_unavailable",
                        )),
                    );
                    cx.notify();
                }
                SchemaTreeEvent::ScriptTableAs {
                    schema,
                    name,
                    kind,
                    server_id,
                    database,
                } => {
                    // Scripting reads the table's rows, so the read has to run
                    // on the server/database the node lives on — same retarget
                    // contract as opening SQL.
                    let target =
                        self.resolve_explorer_target(server_id.as_ref(), database.as_deref(), cx);
                    let (schema, name, kind) = (schema.clone(), name.clone(), kind.clone());
                    self.with_explorer_target_active(target, cx, move |app, cx| {
                        app.run_script_table_as(schema, name, kind, cx);
                    });
                }
                SchemaTreeEvent::PromptPinGroupName {
                    server_id,
                    rename,
                    pin,
                } => {
                    self.overlays.pin_group_dialog = Some(overlays::PinGroupDialogState {
                        server_id: *server_id,
                        value: rename.as_ref().map(|(_, n)| n.clone()).unwrap_or_default(),
                        rename: rename.clone(),
                        pin: pin.clone(),
                        input: None,
                        error: None,
                        _subscription: None,
                    });
                    cx.notify();
                }
                SchemaTreeEvent::ColumnsLoaded {
                    schema,
                    name,
                    columns,
                } => {
                    // Update the datamodel snapshot first (single source of
                    // truth), then update the derived catalog to stay in sync.
                    let dialect = self
                        .active_profile
                        .as_ref()
                        .map(|p| p.database_type.family())
                        .unwrap_or(sqlly_lang::sqlmodel::Dialect::SqlServer);
                    sqlly_services::merge_columns_into_snapshot(
                        Arc::make_mut(&mut self.completion_snapshot),
                        dialect,
                        schema,
                        name,
                        columns.clone(),
                    );
                    // Incrementally enrich the derived catalog (cheaper than
                    // regenerating from the snapshot).
                    let updated = sqlly_services::merge_columns(
                        Arc::make_mut(&mut self.completion_catalog),
                        schema,
                        name,
                        columns.clone(),
                    );
                    if updated > 0 {
                        for tab in &self.tabs {
                            tab.editor.update(cx, |e, cx| {
                                e.set_completion_catalog(self.completion_catalog.clone(), cx);
                            });
                        }
                    }
                    let _ = updated;
                    // Fold the enriched columns back into the cached snapshot so
                    // switching away and back keeps them.
                    self.sync_active_datamodel_cache();
                }
                SchemaTreeEvent::RedisKeysLoaded {
                    profile_id,
                    database,
                    keys,
                } => {
                    // Key names from the explorer's scan feed the key slots
                    // of command completion for that logical database.
                    self.merge_redis_keys_from_scan(*profile_id, database, keys.clone(), cx);
                }
                SchemaTreeEvent::ProcedureParametersLoaded {
                    schema,
                    name,
                    parameters,
                } => {
                    // Update the datamodel snapshot first (single source of
                    // truth), then update the derived procedures to stay in
                    // sync.
                    let dialect = self
                        .active_profile
                        .as_ref()
                        .map(|p| p.database_type.family())
                        .unwrap_or(sqlly_lang::sqlmodel::Dialect::SqlServer);
                    sqlly_services::merge_procedure_params_into_snapshot(
                        Arc::make_mut(&mut self.completion_snapshot),
                        dialect,
                        schema,
                        name,
                        parameters.clone(),
                    );
                    // Incrementally enrich the derived procedures.
                    let procedures = Arc::make_mut(&mut self.completion_procedures);
                    let matched = sqlly_services::merge_procedure_parameters(
                        procedures,
                        schema,
                        name,
                        parameters.clone(),
                    );
                    if matched {
                        self.push_completion_state_to_editors(cx);
                    }
                    // Fold the enriched proc parameters back into the cached
                    // snapshot so switching away and back keeps them.
                    self.sync_active_datamodel_cache();
                }
                // Server-level events (ActivateServer/EditServer/DeleteServer)
                // are translated by the sidebar into ConnectRequested /
                // EditConnectionRequested / DeleteConnectionRequested and never
                // arrive here.
                SchemaTreeEvent::ExplorerActionRequested { context, action } => {
                    self.handle_explorer_action(context.clone(), action.clone(), window, cx);
                }
                SchemaTreeEvent::BulkActionRequested { action, objects } => {
                    self.handle_bulk_tree_action(*action, objects.clone(), window, cx);
                }
                SchemaTreeEvent::DatabasesLoaded { profile_id } => {
                    // A server's databases finished loading: we verifiably
                    // talked to it. Record that on the server itself — health
                    // is a per-server fact, whether or not this server is the
                    // one the focused query window is bound to. The store
                    // write (persisted last-connected metadata) and the tree
                    // write (the in-memory green dot) are one fact recorded
                    // in two places: whoever adds another success path must
                    // update both or they diverge.
                    let now = chrono::Utc::now();
                    ConnectionStore::global().record_connected(*profile_id, now);
                    self.sidebar.update(cx, |s, cx| {
                        s.set_connection_state(
                            *profile_id,
                            crate::schema_tree::ConnectionState::Connected,
                            cx,
                        );
                    });
                    // The query-bar's database dropdown reads its options from
                    // the bound server's tree node, so re-render to pick them
                    // up — otherwise switching servers leaves the old database
                    // list. Only that connection feeds the dropdown; a load for
                    // a server the user is merely browsing needs no re-render.
                    if self.active_profile.as_ref().map(|p| p.id.0) == Some(*profile_id) {
                        if let Some(profile) = self.active_profile.as_mut() {
                            profile.status.record_connected(now);
                        }
                        // The active server's databases loaded: the connection is
                        // now verified, so confirm it (settling the "Connecting…"
                        // status) instead of having claimed success up front.
                        if let Some(name) = self.active_profile.as_ref().map(|p| p.name.clone()) {
                            self.set_status(
                                StatusLevel::Success,
                                crate::localization::format(
                                    "shell.explorer.connected",
                                    &[("name", &name)],
                                ),
                            );
                        }
                        // The database listing replaced the server node's
                        // children wholesale, dropping any subtree hydrated
                        // from the completion snapshot — re-seed it so the
                        // active database stays fully searchable.
                        self.hydrate_tree_from_active_snapshot(cx);
                        cx.notify();
                    }
                }
                SchemaTreeEvent::LoadFailed {
                    profile_id,
                    message,
                    server_node,
                } => {
                    ConnectionStore::global().record_failure(
                        *profile_id,
                        chrono::Utc::now(),
                        message.clone(),
                    );
                    // Red means "we tried and could not reach it" — and only a
                    // failed database enumeration on the server node itself
                    // proves that. A deeper load failing (a schema folder, a
                    // table's columns, a permission error) paints its own
                    // retry dot on that node and says nothing about server
                    // reachability.
                    if *server_node {
                        self.sidebar.update(cx, |s, cx| {
                            s.set_connection_state(
                                *profile_id,
                                crate::schema_tree::ConnectionState::Unreachable,
                                cx,
                            );
                        });
                    }
                    // Browsing a server whose firewall is blocking us is the
                    // usual way the block is discovered. The failing server is
                    // whichever node was expanded, not necessarily the tab's
                    // connection, so the prompt names that one.
                    let profile = self
                        .sidebar
                        .read(cx)
                        .connections()
                        .iter()
                        .find(|p| p.id.0 == *profile_id)
                        .cloned();
                    // If the server that failed is the one the query bar points
                    // at, correct the optimistic "Connected to X" success: the
                    // connection did not come up, so the strip must not keep a
                    // stale green claim (mirrors the status circle going red).
                    if self.active_profile.as_ref().map(|p| p.id.0) == Some(*profile_id) {
                        if let Some(active) = self.active_profile.as_mut() {
                            active
                                .status
                                .record_failure(chrono::Utc::now(), message.clone());
                        }
                        let name = profile.as_ref().map(|p| p.name.clone()).unwrap_or_else(|| {
                            crate::localization::text("shell.connections.server_fallback")
                        });
                        // The tree marks the failure with a dot and keeps the
                        // reason in its tooltip; the status line is where the
                        // user actually looks, so say why here too.
                        self.set_status(
                            StatusLevel::Error,
                            crate::localization::format(
                                "shell.explorer.connect_failed",
                                &[("name", &name), ("error", message)],
                            ),
                        );
                    }
                    let message = message.clone();
                    self.arm_firewall_prompt(&message, profile, cx);
                }
                _ => {}
            },
        }
    }

    /// Palette entry: open the active connection's native CLI in a terminal.
    pub(super) fn open_active_connection_in_terminal(&mut self, cx: &mut Context<Self>) {
        let Some(profile) = self.active_profile.clone() else {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.explorer.connect_terminal"),
            );
            cx.notify();
            return;
        };
        #[cfg(target_arch = "wasm32")]
        {
            let _ = profile;
            self.set_status(
                StatusLevel::Info,
                crate::browser_preview::unavailable(&crate::localization::text(
                    "shell.explorer.terminal_unavailable",
                )),
            );
            cx.notify();
        }
        #[cfg(not(target_arch = "wasm32"))]
        self.open_connection_in_terminal(profile, cx);
    }

    /// Launch the connection's native CLI in a terminal window. Everything
    /// that touches disk, the keyring, or the network (credential hydration,
    /// tool discovery, an SSH tunnel, the notice preference) runs off the UI
    /// thread on the tokio runtime; the launch itself is a fire-and-forget
    /// spawn.
    #[cfg(not(target_arch = "wasm32"))]
    fn open_connection_in_terminal(&mut self, profile: ConnectionProfile, cx: &mut Context<Self>) {
        use super::pending_dialogs::{TerminalLaunchPhase, terminal_launch_status};
        let name = profile.name.clone();
        // Refused here rather than in `prepare_launch`, which runs on the
        // runtime where the UI language is not visible.
        if let Some(reason) = super::tools::direct_route_refusal(
            &profile,
            &crate::localization::text("shell.explorer.open_in_terminal_feature"),
        ) {
            self.set_status(StatusLevel::Warning, reason);
            cx.notify();
            return;
        }
        let (level, text) = terminal_launch_status(&name, &TerminalLaunchPhase::Preparing);
        self.set_status(level, text);
        cx.spawn(async move |this, cx| {
            let prepared = cx
                .background_executor()
                .spawn(async move {
                    // Failures come back unworded: this executor thread does
                    // not see the UI language, so the UI thread words them.
                    let profile = crate::sql_credentials::hydrate_profile(profile)
                        .map_err(TerminalPrepareFailure::Credentials)?;
                    let launch = crate::tokio_handle()
                        .spawn(crate::terminal_launch::prepare_launch(profile))
                        .await
                        .map_err(|e| TerminalPrepareFailure::Task(e.to_string()))?
                        .map_err(TerminalPrepareFailure::Refused)?;
                    let dismissed = crate::preferences::NativeToolPreferences::load()
                        .terminal_secret_notice_dismissed;
                    Ok::<_, TerminalPrepareFailure>((launch, dismissed))
                })
                .await
                .map_err(|failure| match failure {
                    TerminalPrepareFailure::Credentials(reason) => crate::localization::format(
                        "shell.explorer.credentials_unresolved",
                        &[("error", &crate::localization::message(reason))],
                    ),
                    TerminalPrepareFailure::Task(reason) => crate::localization::format(
                        "shell.explorer.terminal_launch_failed",
                        &[("error", &reason)],
                    ),
                    TerminalPrepareFailure::Refused(reason) => crate::localization::message(reason),
                });
            this.update(cx, |app, cx| match prepared {
                Ok((launch, dismissed)) => {
                    let host = crate::terminal_launch::host_terminal();
                    if !dismissed
                        && crate::terminal_launch::launch_writes_secret_file(host, &launch)
                    {
                        // Explain the launcher file before it is written; the
                        // spawn happens on confirm. Nothing is opening while
                        // the notice waits, so the status must not say so.
                        let (level, text) =
                            terminal_launch_status(&name, &TerminalLaunchPhase::AwaitingNotice);
                        app.set_status(level, text);
                        app.pending.terminal_notice =
                            Some(super::pending_dialogs::PendingTerminalNotice {
                                name,
                                launch,
                                dont_show_again: false,
                            });
                        cx.notify();
                    } else {
                        app.spawn_prepared_terminal(name, launch, cx);
                    }
                }
                Err(reason) => {
                    app.set_status(StatusLevel::Warning, reason);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Write the launcher (if any) and spawn the terminal off the UI thread,
    /// then report the outcome with any caveats the launch carries (a mapped
    /// port, a TLS check the CLI can't make through a tunnel).
    #[cfg(not(target_arch = "wasm32"))]
    fn spawn_prepared_terminal(
        &mut self,
        name: String,
        launch: crate::terminal_launch::CliLaunch,
        cx: &mut Context<Self>,
    ) {
        use super::pending_dialogs::{TerminalLaunchPhase, terminal_launch_status};
        let (level, text) = terminal_launch_status(&name, &TerminalLaunchPhase::Launching);
        self.set_status(level, text);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let notes = launch
                .notes
                .iter()
                .map(crate::localization::message)
                .collect::<Vec<_>>();
            let result = cx
                .background_executor()
                .spawn(async move { crate::terminal_launch::spawn_terminal(&launch) })
                .await;
            this.update(cx, |app, cx| {
                match result {
                    Ok(()) => {
                        let (level, text) =
                            terminal_launch_status(&name, &TerminalLaunchPhase::Launched { notes });
                        app.set_status(level, text);
                    }
                    Err(reason) => {
                        app.set_status(StatusLevel::Warning, crate::localization::message(reason))
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The user read the macOS launcher-file notice and chose Open Terminal.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn confirm_terminal_notice(&mut self, cx: &mut Context<Self>) {
        let Some(pending) = self.pending.terminal_notice.take() else {
            return;
        };
        if pending.dont_show_again {
            let mut prefs = crate::preferences::NativeToolPreferences::load();
            prefs.terminal_secret_notice_dismissed = true;
            prefs.save();
        }
        self.spawn_prepared_terminal(pending.name, pending.launch, cx);
    }

    /// The user backed out of the notice: nothing is written or launched, and
    /// the status clears.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn cancel_terminal_notice(&mut self, cx: &mut Context<Self>) {
        if let Some(pending) = self.pending.terminal_notice.take() {
            use super::pending_dialogs::{TerminalLaunchPhase, terminal_launch_status};
            let (level, text) =
                terminal_launch_status(&pending.name, &TerminalLaunchPhase::Cancelled);
            self.set_status(level, text);
            cx.notify();
        }
    }

    /// Handle a typed explorer action from the schema tree context menu.
    /// SQL-bearing table actions (SelectTopRows, ScriptInsert, etc.) are
    /// mapped to existing events by `action_to_event` in schema_tree.rs and
    /// never arrive here. This handler covers all other actions: server
    /// management, refresh, properties, clipboard, and stubs.
    pub(super) fn handle_explorer_action(
        &mut self,
        context: ExplorerActionContext,
        action: ExplorerMenuAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match &action {
            ExplorerMenuAction::Tool(tool) => {
                // The node decides the target, not whatever connection happens
                // to be active: that is the whole point of these living on the
                // right-click menu.
                let target = self.resolve_explorer_target(
                    context.server_id.as_ref(),
                    context.database.as_deref(),
                    cx,
                );
                // A database node scopes the scope-aware tools (Disk Usage,
                // Activity Monitor, Extended Events) to that database; the
                // server node keeps them server-wide.
                let scope = match &context.kind {
                    NodeKind::Server { .. } => crate::tool_dialog::ToolScope::Server,
                    _ => crate::tool_dialog::ToolScope::Database,
                };
                self.open_explorer_tool(*tool, target, scope, window, cx);
            }
            ExplorerMenuAction::Connect => {
                if let Some(sid) = context.server_id {
                    let store = ConnectionStore::global();
                    if let Some(profile) = store.get(sid) {
                        self.connect_profile(profile, cx);
                    }
                } else if let NodeKind::AzureDatabase(discovery) = &context.kind {
                    self.link_azure_database(discovery, cx);
                }
            }
            ExplorerMenuAction::Disconnect => {
                // Per node: disconnect the server this row belongs to,
                // returning its dot to the default gray. Other connected
                // servers are untouched.
                if let Some(sid) = context.server_id {
                    self.disconnect_profile(sid, cx);
                }
            }
            ExplorerMenuAction::EditConnection => {
                if let Some(sid) = context.server_id {
                    self.open_connection_editor(sid, window, cx);
                }
            }
            ExplorerMenuAction::DuplicateConnection => {
                if self.refuse_if_connections_proxied(cx) {
                    return;
                }
                if let Some(sid) = context.server_id {
                    self.open_duplicate_connection_editor(sid, window, cx);
                }
            }
            ExplorerMenuAction::OpenInTerminal => {
                // The menu item is disabled on wasm; this arm is native-only
                // because the spawn is.
                #[cfg(not(target_arch = "wasm32"))]
                if let Some(profile) = context.profile.as_deref().cloned() {
                    self.open_connection_in_terminal(profile, cx);
                }
            }
            ExplorerMenuAction::DeleteConnection => {
                if self.refuse_if_connections_proxied(cx) {
                    return;
                }
                if let Some(sid) = context.server_id {
                    // The menu item's trailing ellipsis promises a dialog; this
                    // is what makes good on it.
                    self.request_delete_connection(sid, cx);
                }
            }
            ExplorerMenuAction::Refresh => {
                if matches!(
                    &context.kind,
                    NodeKind::AzureAccount
                        | NodeKind::AzureSubscription
                        | NodeKind::AzureSqlServer
                        | NodeKind::AzureDatabase(_)
                ) {
                    self.trigger_azure_discovery(cx);
                } else {
                    self.sidebar.update(cx, |s, cx| {
                        s.refresh_schema_node(&context.node_id, cx);
                    });
                }
            }
            ExplorerMenuAction::NewQuery => {
                // Right-clicking a node assumes its own server/database:
                // retarget to it first — connecting if nothing is active for
                // that server, switching database if only the database differs
                // — so the new query runs against the node you clicked rather
                // than whatever connection happened to be active.
                let target = self.resolve_explorer_target(
                    context.server_id.as_ref(),
                    context.database.as_deref(),
                    cx,
                );
                let db = context.database.clone();
                self.with_explorer_target_active(target, cx, move |app, cx| {
                    app.new_tab(cx, None);
                    match &db {
                        Some(database) => app.set_status(
                            StatusLevel::Info,
                            crate::localization::format(
                                "shell.explorer.new_query_database",
                                &[("database", database)],
                            ),
                        ),
                        None => app.set_status(
                            StatusLevel::Info,
                            crate::localization::text("shell.explorer.new_query"),
                        ),
                    }
                    cx.notify();
                });
            }
            ExplorerMenuAction::AddServer => {
                if self.refuse_if_connections_proxied(cx) {
                    return;
                }
                let azure = self.azure_discoveries.clone();
                let editor = cx.new(|cx| {
                    let mut editor = ConnectionEditor::new(
                        self.service_router.clone(),
                        self.log.clone(),
                        self.entra_accounts_registry.clone(),
                        window,
                        cx,
                    );
                    editor.set_azure_discoveries(azure, cx);
                    editor.set_entra_sql_token_provider(self.entra_sql_token_provider.clone());
                    editor
                });
                cx.subscribe(&editor, Self::handle_editor_modal_event)
                    .detach();
                self.overlays.connection_editor = Some(editor);
                cx.notify();
            }
            ExplorerMenuAction::ManageConnections => {
                if self.overlays.connection_management.is_some() {
                    return;
                }
                let azure = self.azure_discoveries.clone();
                let view = cx.new(|cx| {
                    let mut cm = ConnectionManagement::new(
                        self.service_router.clone(),
                        self.log.clone(),
                        self.entra_accounts_registry.clone(),
                        window,
                        cx,
                    );
                    cm.set_azure_discoveries(azure, cx);
                    cm.set_entra_sql_token_provider(self.entra_sql_token_provider.clone());
                    cm
                });
                cx.subscribe(&view, Self::handle_management_event).detach();
                self.overlays.connection_management = Some(view);
                cx.notify();
            }
            ExplorerMenuAction::CreateTo {
                destination: ScriptDestination::Clipboard,
            } => {
                use crate::table_actions::{QualifiedTable, create_table_ddl_for};
                let name = context.name.clone().unwrap_or_default();
                let schema = context.schema.clone().unwrap_or_default();
                let qt = QualifiedTable::new(&name).with_schema(&schema);
                if context.columns.is_empty() {
                    self.set_status(
                        StatusLevel::Warning,
                        crate::localization::text("shell.explorer.columns_not_loaded"),
                    );
                }
                let db_type = self
                    .resolve_explorer_target(
                        context.server_id.as_ref(),
                        context.database.as_deref(),
                        cx,
                    )
                    .map(|p| p.database_type)
                    .unwrap_or_else(|| self.active_db_type());
                let ddl = create_table_ddl_for(db_type, &qt, &context.columns);
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(ddl));
                self.set_status(
                    StatusLevel::Success,
                    crate::localization::text("shell.explorer.create_copied"),
                );
                cx.notify();
            }
            ExplorerMenuAction::CreateTo {
                destination: ScriptDestination::NewQuery,
            } => {
                use crate::table_actions::{QualifiedTable, create_table_ddl_for};
                let name = context.name.clone().unwrap_or_default();
                let schema = context.schema.clone().unwrap_or_default();
                let qt = QualifiedTable::new(&name).with_schema(&schema);
                let db_type = self
                    .resolve_explorer_target(
                        context.server_id.as_ref(),
                        context.database.as_deref(),
                        cx,
                    )
                    .map(|p| p.database_type)
                    .unwrap_or_else(|| self.active_db_type());
                let ddl = create_table_ddl_for(db_type, &qt, &context.columns);
                let sql = if context.columns.is_empty() {
                    crate::localization::format(
                        "shell.explorer.columns_warning_sql_comment",
                        &[("ddl", &ddl)],
                    )
                } else {
                    ddl
                };
                self.new_tab(cx, Some(sql));
                self.set_status(
                    StatusLevel::Info,
                    crate::localization::text("shell.explorer.create_opened"),
                );
                cx.notify();
            }
            ExplorerMenuAction::CopyName => {
                // Precomputed raw leaf name; the sanitized display title is only
                // the last-resort fallback so a copy never silently produces
                // nothing.
                let name = context
                    .copy_name
                    .clone()
                    .unwrap_or_else(|| context.title.clone());
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(name));
                self.set_status(
                    StatusLevel::Success,
                    crate::localization::text("shell.explorer.name_copied"),
                );
                cx.notify();
            }
            ExplorerMenuAction::CopyQualifiedName => {
                // Precomputed, bracket-quoted `[schema].[table].[name]`; fall back
                // to the raw name, then the display title.
                let qualified = context
                    .copy_qualified
                    .clone()
                    .or_else(|| context.copy_name.clone())
                    .unwrap_or_else(|| context.title.clone());
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(qualified));
                self.set_status(
                    StatusLevel::Success,
                    crate::localization::text("shell.explorer.qualified_name_copied"),
                );
                cx.notify();
            }
            ExplorerMenuAction::Properties => {
                use crate::object_editor_model::{EditorObjectKind, EditorTarget};
                // Everything with live catalog details opens in the editor —
                // the server and its databases included.
                let editor_kind = match &context.kind {
                    NodeKind::Server { .. } => Some(EditorObjectKind::Server),
                    NodeKind::Database(_) => Some(EditorObjectKind::Database),
                    NodeKind::SchemaFolder { .. } => Some(EditorObjectKind::Schema),
                    NodeKind::Table { .. } => Some(EditorObjectKind::Table),
                    NodeKind::View { .. } => Some(EditorObjectKind::View),
                    NodeKind::Procedure { .. } => Some(EditorObjectKind::Procedure),
                    NodeKind::Function { .. } => Some(EditorObjectKind::Function),
                    NodeKind::Index { .. } => Some(EditorObjectKind::Index),
                    NodeKind::Statistic { .. } => Some(EditorObjectKind::Statistic),
                    NodeKind::Constraint { .. } => Some(EditorObjectKind::Constraint),
                    NodeKind::Trigger { .. } => Some(EditorObjectKind::Trigger),
                    NodeKind::Type { .. } => Some(EditorObjectKind::Type),
                    NodeKind::Column => Some(EditorObjectKind::Column),
                    _ => None,
                };
                if let Some(kind) = editor_kind {
                    let target_parts = match kind {
                        // The server pages read server-scope catalogs, so the
                        // database is only the context they run in: whatever
                        // the node (or the profile) is pointed at.
                        EditorObjectKind::Server => {
                            let name = context
                                .name
                                .clone()
                                .unwrap_or_else(|| context.title.clone());
                            let database = context
                                .database
                                .clone()
                                .or_else(|| {
                                    context.profile.as_deref().and_then(|p| p.database.clone())
                                })
                                .unwrap_or_default();
                            Some((String::new(), name, database))
                        }
                        // Database nodes carry their name in `database`, not
                        // `name` (which stays `None` for them), so read that —
                        // otherwise Properties dead-ends on "not available".
                        EditorObjectKind::Database => context
                            .database
                            .clone()
                            .or_else(|| context.name.clone())
                            .map(|name| (String::new(), name.clone(), name)),
                        EditorObjectKind::Schema => context.schema.clone().and_then(|name| {
                            context
                                .database
                                .clone()
                                .map(|database| (String::new(), name, database))
                        }),
                        _ => match (
                            context.schema.clone(),
                            context.name.clone(),
                            context.database.clone(),
                        ) {
                            (Some(schema), Some(name), Some(database)) => {
                                Some((schema, name, database))
                            }
                            _ => None,
                        },
                    };
                    let Some((schema, name, database)) = target_parts else {
                        self.set_status(
                            StatusLevel::Error,
                            crate::localization::text("shell.explorer.properties_unavailable_node"),
                        );
                        cx.notify();
                        return;
                    };
                    let Some(profile) = context.profile.as_deref().cloned() else {
                        self.set_status(
                            StatusLevel::Error,
                            crate::localization::text("shell.explorer.no_profile_node"),
                        );
                        cx.notify();
                        return;
                    };
                    let mut target = EditorTarget::new(kind, database, schema, name);
                    if let Some(parent) = context.table.clone() {
                        target = target.child_of(parent);
                    }
                    if self.overlays.object_editor.is_some() {
                        self.overlays.object_editor = None;
                    }
                    // The editor runs live metadata queries against `profile`,
                    // so it must use that profile's route (relay or direct).
                    let services = self.services_for_profile(&profile);
                    let production_read_only = self.production_writes_locked(&profile);
                    let view = cx.new(|cx| {
                        crate::object_editor::ObjectEditorView::new(
                            services,
                            profile,
                            target,
                            production_read_only,
                            window,
                            cx,
                        )
                    });
                    cx.subscribe(&view, Self::handle_object_editor_event)
                        .detach();
                    self.overlays.object_editor = Some(view);
                    self.set_status(
                        StatusLevel::Info,
                        crate::localization::text("shell.explorer.properties_opened"),
                    );
                    cx.notify();
                    return;
                }
                self.set_status(
                    StatusLevel::Error,
                    crate::localization::text("shell.explorer.properties_unavailable_type"),
                );
                cx.notify();
            }
            ExplorerMenuAction::SaveDataAs { format } => {
                let name = context.name.clone().unwrap_or_default();
                let schema = context.schema.clone().unwrap_or_default();
                // Every Save Data As form reads the table's rows, so the read
                // must run on the server/database the node lives on — same
                // retarget contract as Script Table As.
                let target = self.resolve_explorer_target(
                    context.server_id.as_ref(),
                    context.database.as_deref(),
                    cx,
                );
                let format = format.clone();
                use crate::schema_tree::{TableSaveFormat, TableScriptKind};
                match format {
                    TableSaveFormat::InsertInto
                    | TableSaveFormat::UpdateOrInsert
                    | TableSaveFormat::NewTempTableImport => {
                        let kind = match format {
                            TableSaveFormat::InsertInto => TableScriptKind::Insert,
                            TableSaveFormat::UpdateOrInsert => TableScriptKind::Merge,
                            _ => TableScriptKind::TempTable,
                        };
                        self.with_explorer_target_active(target, cx, move |app, cx| {
                            app.run_script_table_as(schema, name, kind, cx);
                        });
                    }
                    TableSaveFormat::Csv
                    | TableSaveFormat::Excel
                    | TableSaveFormat::SqllyExport => {
                        #[cfg(target_arch = "wasm32")]
                        {
                            // The browser preview has no save-file dialog; keep
                            // the assisted SELECT-then-export flow there.
                            use crate::table_actions::{QualifiedTable, select_top_rows_sql_for};
                            let qt = QualifiedTable::new(&name).with_schema(&schema);
                            let sql = select_top_rows_sql_for(self.active_db_type(), 10_000, &qt);
                            self.new_tab(cx, Some(sql));
                            self.set_status(
                                StatusLevel::Info,
                                crate::localization::text("shell.explorer.save_data_select_opened"),
                            );
                            cx.notify();
                            let _ = target;
                        }
                        #[cfg(not(target_arch = "wasm32"))]
                        self.with_explorer_target_active(target, cx, move |app, cx| {
                            app.save_table_data_to_file(schema, name, format, cx);
                        });
                    }
                }
            }
            ExplorerMenuAction::TransferDataTo => {
                // Data Transfer is native-only; the browser preview has no
                // transfer engine, so this menu action is a no-op there.
                #[cfg(not(target_arch = "wasm32"))]
                {
                    let target = self.resolve_explorer_target(
                        context.server_id.as_ref(),
                        context.database.as_deref(),
                        cx,
                    );
                    let preselect = (
                        context.database.clone(),
                        context.schema.clone(),
                        context.name.clone().unwrap_or_default(),
                    );
                    self.with_explorer_target_active(target, cx, move |app, cx| {
                        app.overlays.pending_data_transfer_preselect = Some(preselect);
                        cx.notify();
                    });
                }
            }
            ExplorerMenuAction::FocusInRelationships => {
                let schema = context.schema.clone().unwrap_or_default();
                let table = context.name.clone().unwrap_or_default();
                if table.is_empty() {
                    self.set_status(
                        StatusLevel::Info,
                        crate::localization::text("shell.explorer.relationships_select_table"),
                    );
                    cx.notify();
                } else {
                    self.open_relationship_graph_focused(Some((schema, table)), window, cx);
                }
            }
            ExplorerMenuAction::AskSchema
            | ExplorerMenuAction::GenerateSql
            | ExplorerMenuAction::FindJoinPath
            | ExplorerMenuAction::PinToContext => {
                self.route_explorer_ai_action(&context, &action, cx);
            }
            ExplorerMenuAction::RunProcedure | ExplorerMenuAction::CopyInvocation => {
                let copy_only = matches!(action, ExplorerMenuAction::CopyInvocation);
                self.handle_procedure_invocation(&context, copy_only, cx);
            }
            ExplorerMenuAction::FilterData => {
                self.handle_filter_data(&context, cx);
            }
            ExplorerMenuAction::ConnectToCurrentQuery => {
                // Retarget the active connection to the node's server/database.
                let target = self.resolve_explorer_target(
                    context.server_id.as_ref(),
                    context.database.as_deref(),
                    cx,
                );
                self.with_explorer_target_active(target, cx, move |app, cx| {
                    app.set_status(
                        StatusLevel::Success,
                        crate::localization::text("shell.explorer.query_connection_switched"),
                    );
                    cx.notify();
                });
            }
            ExplorerMenuAction::OpenInNewTab => {
                // A fresh query tab pre-targeted at the node's server/database.
                let target = self.resolve_explorer_target(
                    context.server_id.as_ref(),
                    context.database.as_deref(),
                    cx,
                );
                self.with_explorer_target_active(target, cx, move |app, cx| {
                    app.new_tab(cx, None);
                    app.set_status(
                        StatusLevel::Info,
                        crate::localization::text("shell.explorer.new_query_selected_server"),
                    );
                    cx.notify();
                });
            }
            ExplorerMenuAction::EditStructure => {
                self.open_structure_editor(&context, window, cx);
            }
            ExplorerMenuAction::GenerateModel {
                language,
                destination,
            } => {
                self.generate_model_for_node(&context, *language, destination.clone(), cx);
            }
            ExplorerMenuAction::GenerateModelsForAll { language } => {
                self.generate_models_for_scope(&context, *language, cx);
            }
            ExplorerMenuAction::EditDefinition => {
                self.handle_edit_definition(&context, false, cx);
            }
            ExplorerMenuAction::ScriptAlter => {
                // Tables are handled synchronously in the tree (scaffold);
                // definition-carrying objects fetch their source first.
                self.handle_edit_definition(&context, true, cx);
            }
            ExplorerMenuAction::ViewDependencies | ExplorerMenuAction::FindUsages => {
                self.open_object_dependencies(&context, cx);
            }
            // Routed through `SchemaTreeEvent::ExportDatabaseSql` from the menu.
            ExplorerMenuAction::ExportDatabaseSql => {}
            ExplorerMenuAction::ExportTables => {
                #[cfg(not(target_arch = "wasm32"))]
                {
                    let schema = match &context.kind {
                        NodeKind::SchemaFolder { schema } => Some(schema.clone()),
                        _ => None,
                    };
                    let database = context.database.clone().unwrap_or_default();
                    let target = self.resolve_explorer_target(
                        context.server_id.as_ref(),
                        Some(database.as_str()),
                        cx,
                    );
                    match target {
                        Some(profile) => {
                            match retarget_decision(self.active_profile.as_ref(), &profile) {
                                RetargetDecision::Connect => {
                                    self.connect_profile_then(profile, cx, move |app, cx| {
                                        app.overlays.pending_table_export =
                                            Some((database, schema));
                                        cx.notify();
                                    });
                                }
                                RetargetDecision::SwitchDatabase(db) => {
                                    self.switch_database(db, cx);
                                    self.open_table_export_dialog(database, schema, window, cx);
                                }
                                RetargetDecision::AlreadyActive => {
                                    self.open_table_export_dialog(database, schema, window, cx);
                                }
                            }
                        }
                        None => self.open_table_export_dialog(database, schema, window, cx),
                    }
                }
                #[cfg(target_arch = "wasm32")]
                {
                    self.set_status(
                        StatusLevel::Error,
                        crate::browser_preview::unavailable(&crate::localization::text(
                            "shell.explorer.export_tables_unavailable",
                        ))
                        .to_string(),
                    );
                    cx.notify();
                }
            }
            ExplorerMenuAction::ImportFromClipboard => {
                #[cfg(not(target_arch = "wasm32"))]
                {
                    let target = self.resolve_explorer_target(
                        context.server_id.as_ref(),
                        context.database.as_deref(),
                        cx,
                    );
                    let schema = context.schema.clone();
                    match target {
                        Some(profile) => {
                            match retarget_decision(self.active_profile.as_ref(), &profile) {
                                RetargetDecision::Connect => {
                                    self.connect_profile_then(profile, cx, move |app, cx| {
                                        app.overlays.pending_import_from_clipboard = Some(schema);
                                        cx.notify();
                                    });
                                }
                                RetargetDecision::SwitchDatabase(db) => {
                                    self.switch_database(db, cx);
                                    self.open_import_from_clipboard(schema, window, cx);
                                }
                                RetargetDecision::AlreadyActive => {
                                    self.open_import_from_clipboard(schema, window, cx);
                                }
                            }
                        }
                        None => self.open_import_from_clipboard(schema, window, cx),
                    }
                }
                #[cfg(target_arch = "wasm32")]
                {
                    self.set_status(
                        StatusLevel::Error,
                        crate::browser_preview::unavailable(&crate::localization::text(
                            "shell.explorer.import_clipboard_unavailable",
                        ))
                        .to_string(),
                    );
                    cx.notify();
                }
            }
            ExplorerMenuAction::RunSqlFile => {}
            ExplorerMenuAction::BrowseObjects => {}
            ExplorerMenuAction::SearchDatabaseValue => {}
            // Snapshot tools read the node's own profile and database; no
            // retarget of the active connection is needed.
            ExplorerMenuAction::SaveSchemaSnapshot | ExplorerMenuAction::SchemaHistory => {
                let target = self.resolve_explorer_target(
                    context.server_id.as_ref(),
                    context.database.as_deref(),
                    cx,
                );
                let Some(profile) = target.filter(|profile| profile.database.is_some()) else {
                    self.set_status(
                        StatusLevel::Info,
                        crate::localization::text("shell.explorer.snapshot_select_database"),
                    );
                    cx.notify();
                    return;
                };
                if matches!(action, ExplorerMenuAction::SaveSchemaSnapshot) {
                    self.open_save_schema_snapshot(profile, window, cx);
                } else {
                    self.open_schema_history(profile, window, cx);
                }
            }
            ExplorerMenuAction::HideSchema | ExplorerMenuAction::ShowHiddenSchemas => {
                self.handle_hidden_schema_action(&action, &context, cx);
            }
            // Pin/unpin and reorder are handled inside the tree; these never
            // reach this dispatcher.
            ExplorerMenuAction::ToggleFavorite
            | ExplorerMenuAction::Unpin
            | ExplorerMenuAction::MovePinnedUp
            | ExplorerMenuAction::MovePinnedDown
            | ExplorerMenuAction::PinToGroup { .. }
            | ExplorerMenuAction::NewPinGroup
            | ExplorerMenuAction::RenamePinGroup
            | ExplorerMenuAction::DeletePinGroup => {}
            // Redis key browser. Open Value and Open in Query Tab never
            // reach here (the tree turns them into OpenRedisKey /
            // RunCommandInNewTab); the rest copy, prompt, or reload.
            ExplorerMenuAction::RedisOpenValue | ExplorerMenuAction::RedisOpenInQueryTab => {}
            // Search-index actions are turned into open/run events by the
            // tree as well (menus.rs); nothing reaches the dispatcher.
            ExplorerMenuAction::RedisIndexInfo
            | ExplorerMenuAction::RedisIndexQuery
            | ExplorerMenuAction::RedisIndexExplain
            | ExplorerMenuAction::RedisIndexProfile
            | ExplorerMenuAction::RedisIndexDrop => {}
            // Load more / Scan more are handled inside the tree, which owns
            // the retained listing; they never reach this dispatcher.
            ExplorerMenuAction::RedisLoadMore | ExplorerMenuAction::RedisScanMore => {}
            // The analyzer opens its read-only report pane on the
            // right-clicked database's connection; retarget first so a
            // context-menu open never lands on the wrong server.
            ExplorerMenuAction::RedisMemoryAnalyzer => {
                let target = self.resolve_explorer_target(
                    context.server_id.as_ref(),
                    context.database.as_deref(),
                    cx,
                );
                self.with_explorer_target_active(target, cx, |app, cx| {
                    app.open_redis_memory_analyzer(cx);
                });
            }
            // The topology pane opens on the right-clicked server's
            // connection (a cluster has one logical database).
            ExplorerMenuAction::RedisClusterTopology => {
                let target = self.resolve_explorer_target(
                    context.server_id.as_ref(),
                    context.database.as_deref(),
                    cx,
                );
                #[cfg(not(target_arch = "wasm32"))]
                self.with_explorer_target_active(target, cx, |app, cx| {
                    app.open_redis_cluster_topology(cx);
                });
                #[cfg(target_arch = "wasm32")]
                let _ = target;
            }
            ExplorerMenuAction::RedisCopyCommand => {
                let command = match &context.kind {
                    NodeKind::RedisKey { key, kind, .. } => {
                        crate::schema_tree::redis::read_command(key, kind)
                    }
                    NodeKind::RedisKeyFolder { prefix } => format!(
                        "SCAN 0 MATCH {} COUNT 1000",
                        sqlly_intel::query::safety::redis::quote_argument(
                            crate::schema_tree::redis::prefix_pattern(prefix).as_bytes()
                        )
                    ),
                    _ => return,
                };
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(command));
                self.set_status(
                    StatusLevel::Success,
                    crate::localization::text("shell.explorer.command_copied"),
                );
                cx.notify();
            }
            ExplorerMenuAction::RedisFilterKeys => {
                #[cfg(not(target_arch = "wasm32"))]
                {
                    let current = self.sidebar.read(cx).redis_key_filter(&context.node_id, cx);
                    let recent = context
                        .server_id
                        .map(crate::redis_key_filters::recent_patterns)
                        .unwrap_or_default();
                    self.open_redis_key_dialog(
                        crate::redis_key_dialog::RedisKeyDialogMode::Filter {
                            node_id: context.node_id.clone(),
                            current,
                            recent,
                        },
                        &context,
                        window,
                        cx,
                    );
                }
            }
            ExplorerMenuAction::RedisClearFilter => {
                self.sidebar.update(cx, |s, cx| {
                    s.set_redis_key_filter(&context.node_id, None, cx);
                });
            }
            ExplorerMenuAction::RedisSetTtl
            | ExplorerMenuAction::RedisPersist
            | ExplorerMenuAction::RedisDeleteKey => {
                #[cfg(not(target_arch = "wasm32"))]
                {
                    let NodeKind::RedisKey { key, .. } = &context.kind else {
                        return;
                    };
                    let key = key.clone();
                    // The listing to reload afterwards is the key's parent
                    // (its folder or database), never the key itself.
                    let node_id = context
                        .node_id
                        .rsplit_once(":k:")
                        .map(|(parent, _)| parent.to_string())
                        .unwrap_or_else(|| context.node_id.clone());
                    let mode = match action {
                        ExplorerMenuAction::RedisSetTtl => {
                            crate::redis_key_dialog::RedisKeyDialogMode::SetTtl { node_id, key }
                        }
                        ExplorerMenuAction::RedisPersist => {
                            crate::redis_key_dialog::RedisKeyDialogMode::Persist { node_id, key }
                        }
                        _ => crate::redis_key_dialog::RedisKeyDialogMode::Delete { node_id, key },
                    };
                    self.open_redis_key_dialog(mode, &context, window, cx);
                }
            }
            ExplorerMenuAction::RedisBulkDelete
            | ExplorerMenuAction::RedisBulkTtl
            | ExplorerMenuAction::RedisImportCommands => {
                #[cfg(not(target_arch = "wasm32"))]
                {
                    // The bulk pattern: a folder's exact-prefix glob, or the
                    // database node's own key filter (every key when unset).
                    let pattern = match &context.kind {
                        NodeKind::RedisKeyFolder { prefix } => {
                            crate::schema_tree::redis::prefix_pattern(prefix)
                        }
                        NodeKind::Database(_) => self
                            .sidebar
                            .read(cx)
                            .redis_key_filter(&context.node_id, cx)
                            .unwrap_or_else(|| "*".to_string()),
                        _ => return,
                    };
                    let node_id = context.node_id.clone();
                    let mode = match action {
                        ExplorerMenuAction::RedisBulkDelete => {
                            crate::redis_key_dialog::RedisKeyDialogMode::BulkDelete {
                                node_id,
                                pattern,
                            }
                        }
                        ExplorerMenuAction::RedisBulkTtl => {
                            crate::redis_key_dialog::RedisKeyDialogMode::BatchTtl {
                                node_id,
                                pattern,
                            }
                        }
                        _ => {
                            crate::redis_key_dialog::RedisKeyDialogMode::ImportCommands { node_id }
                        }
                    };
                    self.open_redis_key_dialog(mode, &context, window, cx);
                }
            }
            ExplorerMenuAction::RedisRenameKey
            | ExplorerMenuAction::RedisCopyToDatabase
            | ExplorerMenuAction::RedisCopyToConnection
            | ExplorerMenuAction::RedisExportCommands
            | ExplorerMenuAction::RedisNameDatabase
            | ExplorerMenuAction::RedisVisibleDatabases => {
                #[cfg(not(target_arch = "wasm32"))]
                self.open_redis_key_action(&action, &context, window, cx);
            }
            ExplorerMenuAction::CreateDatabase => {
                #[cfg(not(target_arch = "wasm32"))]
                self.open_database_dialog(
                    crate::database_dialog::DatabaseDialogMode::Create,
                    context.server_id,
                    None,
                    window,
                    cx,
                );
            }
            ExplorerMenuAction::DropDatabase => {
                #[cfg(not(target_arch = "wasm32"))]
                {
                    let Some(name) = context.database.clone() else {
                        return;
                    };
                    let is_active = self
                        .active_profile
                        .as_ref()
                        .and_then(|p| p.database.as_deref())
                        == Some(name.as_str())
                        && self.active_profile.as_ref().map(|p| p.id.0) == context.server_id;
                    self.open_database_dialog(
                        crate::database_dialog::DatabaseDialogMode::Drop { name, is_active },
                        context.server_id,
                        None,
                        window,
                        cx,
                    );
                }
            }
            ExplorerMenuAction::ScriptCreate => {
                self.handle_script_create(&context, cx);
            }
            ExplorerMenuAction::EditEvent => {
                self.handle_edit_event(&context, cx);
            }
            ExplorerMenuAction::InstallExtension => {
                self.handle_install_extension(&context, cx);
            }
            ExplorerMenuAction::RefreshMaterializedView => {}
            ExplorerMenuAction::Maintenance { op, mode } => {
                self.handle_maintenance(&context, *op, *mode, cx);
            }
            ExplorerMenuAction::RunTemplate { name } => {
                self.handle_run_template_action(&context, name, cx);
            }
            ExplorerMenuAction::AddToDeployCart { include_data } => {
                #[cfg(not(target_arch = "wasm32"))]
                self.add_to_deploy_cart(&context, *include_data, cx);
                #[cfg(target_arch = "wasm32")]
                let _ = include_data;
            }
            ExplorerMenuAction::OpenDeployCart => {
                #[cfg(not(target_arch = "wasm32"))]
                self.open_deploy_cart(cx);
            }
            ExplorerMenuAction::Rename => {
                let Some(name) = context.name.clone().filter(|name| !name.is_empty()) else {
                    return;
                };
                let db = self.active_db_type();
                let kind = crate::schema_tree::script_object_kind_public(&context.kind)
                    .unwrap_or(crate::table_actions::ScriptObjectKind::Table);
                self.overlays.rename_dialog = Some(overlays::RenameDialogState {
                    server_id: context.server_id,
                    database: context.database.clone(),
                    schema: context.schema.clone().unwrap_or_default(),
                    name,
                    kind,
                    db,
                    input: None,
                    value: String::new(),
                    error: None,
                    _subscription: None,
                });
                cx.notify();
            }
            // SQL-bearing actions are mapped to InsertSql/ScriptTableAs by
            // `action_to_event` in schema_tree.rs and never arrive here.
            ExplorerMenuAction::SelectTopRows { .. }
            | ExplorerMenuAction::DesignQuery
            | ExplorerMenuAction::ViewData
            | ExplorerMenuAction::CountRows
            | ExplorerMenuAction::ScriptInsert
            | ExplorerMenuAction::ScriptMerge
            | ExplorerMenuAction::DeleteAllRows
            | ExplorerMenuAction::TruncateTable
            | ExplorerMenuAction::ScriptDrop
            | ExplorerMenuAction::DropTable => {
                tracing::warn!(
                    "SQL-bearing action reached handle_explorer_action (should have been mapped to InsertSql/ScriptTableAs by action_to_event)"
                );
            }
        }
    }

    /// Resolve the saved profile for an explorer node's server/database
    /// identity, with the node's database (when present) overriding the
    /// profile's stored default. `None` when the node carries no resolvable
    /// server id — callers then act on the current connection.
    ///
    /// Resolves from the sidebar's in-memory profile list — this runs on
    /// every explorer double-click/context action. The process-wide store is
    /// consulted only as a fallback for a profile the sidebar hasn't caught up
    /// with yet (it refreshes on every store mutation, so this is effectively
    /// never).
    /// Explorer "Filter Data": open a reviewed, type-aware SELECT scaffold
    /// for the selected table/view in a NEW tab (never mutating the active
    /// editor), retargeted to the object's own profile/database.
    pub(super) fn handle_filter_data(
        &mut self,
        context: &ExplorerActionContext,
        cx: &mut Context<Self>,
    ) {
        let Some(name) = context.name.clone() else {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.explorer.filter_select_object"),
            );
            cx.notify();
            return;
        };
        let mut table = crate::table_actions::QualifiedTable::new(name);
        if let Some(schema) = &context.schema {
            table = table.with_schema(schema.clone());
        }
        if let Some(db) = &context.database {
            table = table.with_database(db.clone());
        }
        let target = self.resolve_explorer_target(
            context.server_id.as_ref(),
            context.database.as_deref(),
            cx,
        );
        // Render for the target's engine (known synchronously from the
        // profile), not the possibly-different active connection.
        let db_type = target
            .as_ref()
            .map(|p| p.database_type)
            .unwrap_or_else(|| self.active_db_type());
        let sql = crate::table_actions::filter_data_sql_for(db_type, &table, &context.columns);
        self.with_explorer_target_active(target, cx, move |app, cx| {
            app.new_tab(cx, Some(sql));
            cx.notify();
        });
    }

    /// Explorer "Edit Definition" / "Script as ALTER" for definition-carrying
    /// objects (views, procedures, functions): fetch the server definition on
    /// the node's own profile route, then open it in a NEW editable query tab
    /// retargeted to that connection. `as_alter` rewrites the fetched CREATE
    /// into the engine's alter form. Script-first: nothing executes.
    pub(super) fn handle_edit_definition(
        &mut self,
        context: &ExplorerActionContext,
        as_alter: bool,
        cx: &mut Context<Self>,
    ) {
        use sqlly_intel::intellisense::{ClickedRole, NavigationTarget, RoutineKind};

        let Some(name) = context.name.clone() else {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.explorer.select_object"),
            );
            cx.notify();
            return;
        };
        let target_profile = self.resolve_explorer_target(
            context.server_id.as_ref(),
            context.database.as_deref(),
            cx,
        );
        let Some(profile) = target_profile
            .clone()
            .or_else(|| self.active_profile.clone())
        else {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.explorer.connect_fetch_definition"),
            );
            cx.notify();
            return;
        };
        let db_type = profile.database_type;
        let schema = context
            .schema
            .clone()
            .or_else(|| db_type.default_schema().map(str::to_string))
            .unwrap_or_default();
        let script_kind = match &context.kind {
            NodeKind::View { .. } => crate::table_actions::ScriptObjectKind::View,
            NodeKind::Procedure { .. } => crate::table_actions::ScriptObjectKind::Procedure,
            NodeKind::Function { .. } => crate::table_actions::ScriptObjectKind::Function,
            _ => {
                self.set_status(
                    StatusLevel::Info,
                    crate::localization::text("shell.explorer.edit_definition_supported"),
                );
                cx.notify();
                return;
            }
        };
        let nav_target = match script_kind {
            crate::table_actions::ScriptObjectKind::View => NavigationTarget::Table {
                catalog: None,
                schema: schema.clone(),
                name: name.clone(),
                clicked: ClickedRole::View,
            },
            crate::table_actions::ScriptObjectKind::Function => NavigationTarget::Routine {
                catalog: None,
                schema: schema.clone(),
                name: name.clone(),
                kind: RoutineKind::Function,
                clicked: ClickedRole::Function,
            },
            _ => NavigationTarget::Routine {
                catalog: None,
                schema: schema.clone(),
                name: name.clone(),
                kind: RoutineKind::Procedure,
                clicked: ClickedRole::Procedure,
            },
        };
        let database = context
            .database
            .clone()
            .or_else(|| profile.database.clone())
            .unwrap_or_else(|| "master".to_string());
        let qt = crate::table_actions::QualifiedTable::new(&name).with_schema(&schema);
        let action_label = if as_alter {
            crate::localization::text("shell.explorer.action_alter_script")
        } else {
            crate::localization::text("shell.explorer.action_definition")
        };
        self.set_status(
            StatusLevel::Busy,
            crate::localization::format(
                "shell.explorer.fetching_definition",
                &[
                    ("action_label", &action_label),
                    ("schema", &schema),
                    ("name", &name),
                ],
            ),
        );
        cx.notify();

        // The fetch routes by the node's own profile, so no retarget is
        // needed to read; the retarget happens when the tab opens.
        let services = self.services_for_profile(&profile);
        let fetch_profile = profile.clone();
        // Script as CREATE carries PostgreSQL privileges when the DDL
        // preference says so; Script as ALTER stays the definition alone.
        let ddl_options = if as_alter {
            sqlly_services::DdlOptions::default()
        } else {
            crate::preferences::DdlNavigationPreferences::load().ddl_options()
        };
        let (tx, rx) = tokio::sync::oneshot::channel::<
            sqlly_core::StudioResult<sqlly_services::DdlDocument>,
        >();
        cx.background_spawn(async move {
            crate::tokio_handle()
                .spawn(async move {
                    let result = sqlly_services::fetch_ddl_document_with(
                        services.queries.as_ref(),
                        &fetch_profile,
                        &database,
                        &nav_target,
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
        let weak_self = cx.weak_entity();
        cx.spawn(async move |_this, _cx| {
            let Ok(result) = rx.await else { return };
            let mut cx = async_cx.clone();
            weak_self
                .update(&mut cx, |this, cx| {
                    let document = match result {
                        Ok(document) => document,
                        Err(e) => {
                            this.set_status(
                                StatusLevel::Error,
                                crate::localization::format(
                                    "shell.explorer.definition_fetch_failed",
                                    &[("error", crate::localization::message(e.message()))],
                                ),
                            );
                            cx.notify();
                            return;
                        }
                    };
                    if let Some(err) = &document.error {
                        this.set_status(StatusLevel::Warning, err.clone());
                        cx.notify();
                        return;
                    }
                    let body = document
                        .sources
                        .iter()
                        .map(|s| s.body.as_str())
                        .collect::<Vec<_>>()
                        .join("\n\n");
                    if body.trim().is_empty() {
                        this.set_status(
                            StatusLevel::Warning,
                            crate::localization::format(
                                "shell.explorer.definition_unavailable",
                                &[("schema", &schema), ("name", &name)],
                            ),
                        );
                        cx.notify();
                        return;
                    }
                    let sql = if as_alter {
                        crate::table_actions::alter_from_definition_for(
                            db_type,
                            script_kind,
                            &qt,
                            &body,
                        )
                    } else {
                        body
                    };
                    this.with_explorer_target_active(target_profile, cx, move |app, cx| {
                        app.new_tab(cx, Some(sql));
                        app.set_status(
                            StatusLevel::Success,
                            crate::localization::format(
                                "shell.explorer.definition_opened",
                                &[
                                    ("action_label", &action_label),
                                    ("schema", &schema),
                                    ("name", &name),
                                ],
                            ),
                        );
                        cx.notify();
                    });
                })
                .ok();
        })
        .detach();
    }

    fn handle_script_create(&mut self, context: &ExplorerActionContext, cx: &mut Context<Self>) {
        use sqlly_core::CellValue;
        use sqlly_intel::metadata::MetadataQuery;

        fn cell_display(cell: CellValue) -> Option<String> {
            match cell {
                CellValue::String(s) => Some(s),
                CellValue::I64(n) => Some(n.to_string()),
                CellValue::F64(f) => Some(f.to_string()),
                CellValue::Bool(b) => Some(b.to_string()),
                CellValue::Null | CellValue::Bytes(_) => None,
            }
        }

        let Some(profile) = self
            .resolve_explorer_target(context.server_id.as_ref(), context.database.as_deref(), cx)
            .or_else(|| self.active_profile.clone())
        else {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.explorer.connect_script_objects"),
            );
            cx.notify();
            return;
        };
        let database = context
            .database
            .clone()
            .or_else(|| profile.database.clone())
            .unwrap_or_else(|| "master".to_string());
        let name = context.name.clone().unwrap_or_default();
        let db_type = profile.database_type;
        let services = self.services_for_profile(&profile);
        let target_profile = profile.clone();
        let kind = context.kind.clone();

        self.set_status(
            StatusLevel::Busy,
            crate::localization::format(
                "shell.explorer.fetching_create_script",
                &[("name", &name)],
            ),
        );
        cx.notify();

        let (tx, rx) = tokio::sync::oneshot::channel::<Option<String>>();
        cx.background_spawn(async move {
            crate::tokio_handle()
                .spawn(async move {
                    let sql = match &kind {
                        NodeKind::Trigger { schema, name, .. } => services
                            .metadata
                            .object_definition_text(
                                &profile,
                                &database,
                                &MetadataQuery::TriggerDefinition {
                                    schema: schema.clone(),
                                    name: name.clone(),
                                },
                            )
                            .await
                            .ok()
                            .flatten(),
                        NodeKind::Constraint {
                            schema,
                            table,
                            name,
                            ..
                        } => services
                            .metadata
                            .object_definition_text(
                                &profile,
                                &database,
                                &MetadataQuery::ConstraintDefinition {
                                    schema: schema.clone(),
                                    table: table.clone(),
                                    name: name.clone(),
                                },
                            )
                            .await
                            .ok()
                            .flatten(),
                        NodeKind::Sequence { schema, name } => Some(format!(
                            "CREATE SEQUENCE {}.{} AS bigint START WITH 1 INCREMENT BY 1;",
                            db_type.quote_identifier(schema),
                            db_type.quote_identifier(name)
                        )),
                        NodeKind::Synonym {
                            schema,
                            name,
                            base_object,
                        } => Some(format!(
                            "CREATE SYNONYM {}.{} FOR {base_object};",
                            db_type.quote_identifier(schema),
                            db_type.quote_identifier(name)
                        )),
                        NodeKind::Extension { name, .. } => {
                            Some(format!("CREATE EXTENSION IF NOT EXISTS {name};"))
                        }
                        NodeKind::MaterializedView { schema, name } => services
                            .metadata
                            .object_definition_text(
                                &profile,
                                &database,
                                &MetadataQuery::ObjectDefinition {
                                    schema: schema.clone(),
                                    object: name.clone(),
                                },
                            )
                            .await
                            .ok()
                            .flatten(),
                        NodeKind::Event { schema, name } => {
                            let rows = services
                                .metadata
                                .run_metadata_query(
                                    &profile,
                                    &database,
                                    &MetadataQuery::Events {
                                        schema: Some(schema.clone()),
                                    },
                                )
                                .await
                                .unwrap_or_default();
                            rows.into_iter().find_map(|row| {
                                let mut cells = row.into_iter();
                                let row_schema = cells.next().and_then(cell_display);
                                let row_name = cells.next().and_then(cell_display)?;
                                if row_schema.as_deref() != Some(schema.as_str())
                                    || row_name != *name
                                {
                                    return None;
                                }
                                let _status = cells.next();
                                let _event_type = cells.next();
                                let interval = cells
                                    .next()
                                    .and_then(cell_display)
                                    .unwrap_or_else(|| "INTERVAL 1 DAY".to_string());
                                let definition = cells
                                    .next()
                                    .and_then(cell_display)
                                    .unwrap_or_else(|| "BEGIN END".to_string());
                                Some(format!(
                                    "CREATE EVENT {}.{} ON SCHEDULE {interval} DO {definition};",
                                    db_type.quote_identifier(schema),
                                    db_type.quote_identifier(name)
                                ))
                            })
                        }
                        _ => None,
                    };
                    let _ = tx.send(sql);
                })
                .await
                .ok();
        })
        .detach();

        let async_cx = cx.to_async();
        let weak_self = cx.weak_entity();
        cx.spawn(async move |_this, _cx| {
            let Ok(Some(sql)) = rx.await else {
                return;
            };
            let mut cx = async_cx.clone();
            weak_self
                .update(&mut cx, |this, cx| {
                    this.with_explorer_target_active(Some(target_profile), cx, move |app, cx| {
                        app.new_tab(cx, Some(sql));
                        app.set_status(
                            StatusLevel::Success,
                            crate::localization::text("shell.explorer.create_script_opened"),
                        );
                        cx.notify();
                    });
                })
                .ok();
        })
        .detach();
    }

    fn handle_edit_event(&mut self, context: &ExplorerActionContext, cx: &mut Context<Self>) {
        use sqlly_core::CellValue;
        use sqlly_intel::metadata::MetadataQuery;

        fn cell_display(cell: CellValue) -> Option<String> {
            match cell {
                CellValue::String(s) => Some(s),
                CellValue::I64(n) => Some(n.to_string()),
                CellValue::F64(f) => Some(f.to_string()),
                CellValue::Bool(b) => Some(b.to_string()),
                CellValue::Null | CellValue::Bytes(_) => None,
            }
        }

        let NodeKind::Event { schema, name } = &context.kind else {
            return;
        };
        let Some(profile) = self
            .resolve_explorer_target(context.server_id.as_ref(), context.database.as_deref(), cx)
            .or_else(|| self.active_profile.clone())
        else {
            return;
        };
        let database = context
            .database
            .clone()
            .or_else(|| profile.database.clone())
            .unwrap_or_default();
        let db_type = profile.database_type;
        let services = self.services_for_profile(&profile);
        let target_profile = profile.clone();
        let schema = schema.clone();
        let name = name.clone();

        let (tx, rx) = tokio::sync::oneshot::channel::<Option<String>>();
        cx.background_spawn(async move {
            crate::tokio_handle()
                .spawn(async move {
                    let sql = services
                        .metadata
                        .run_metadata_query(
                            &profile,
                            &database,
                            &MetadataQuery::Events {
                                schema: Some(schema.clone()),
                            },
                        )
                        .await
                        .ok()
                        .and_then(|rows| {
                            rows.into_iter().find_map(|row| {
                                let mut cells = row.into_iter();
                                let row_schema = cells.next().and_then(cell_display);
                                let row_name = cells.next().and_then(cell_display)?;
                                if row_schema.as_deref() != Some(schema.as_str())
                                    || row_name != name
                                {
                                    return None;
                                }
                                let _status = cells.next();
                                let _event_type = cells.next();
                                let interval = cells
                                    .next()
                                    .and_then(cell_display)
                                    .unwrap_or_else(|| "INTERVAL 1 DAY".to_string());
                                let definition = cells
                                    .next()
                                    .and_then(cell_display)
                                    .unwrap_or_else(|| "BEGIN END".to_string());
                                Some(format!(
                                    "-- Edit the schedule or body, review, then run.\nALTER EVENT {}.{} ON SCHEDULE {interval} DO\n{definition};",
                                    db_type.quote_identifier(&schema),
                                    db_type.quote_identifier(&name)
                                ))
                            })
                        });
                    let _ = tx.send(sql);
                })
                .await
                .ok();
        })
        .detach();

        let async_cx = cx.to_async();
        let weak_self = cx.weak_entity();
        cx.spawn(async move |_this, _cx| {
            let Ok(Some(sql)) = rx.await else {
                return;
            };
            let mut cx = async_cx.clone();
            weak_self
                .update(&mut cx, |this, cx| {
                    this.with_explorer_target_active(Some(target_profile), cx, move |app, cx| {
                        app.new_tab(cx, Some(sql));
                        cx.notify();
                    });
                })
                .ok();
        })
        .detach();
    }

    fn handle_install_extension(
        &mut self,
        context: &ExplorerActionContext,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self
            .resolve_explorer_target(context.server_id.as_ref(), context.database.as_deref(), cx)
            .or_else(|| self.active_profile.clone())
        else {
            return;
        };
        let database = context
            .database
            .clone()
            .or_else(|| profile.database.clone())
            .unwrap_or_default();
        let services = self.services_for_profile(&profile);
        let target_profile = profile.clone();

        let (tx, rx) = tokio::sync::oneshot::channel::<Option<String>>();
        cx.background_spawn(async move {
            crate::tokio_handle()
                .spawn(async move {
                    let extensions = services
                        .metadata
                        .list_available_extensions(&profile, &database)
                        .await
                        .unwrap_or_default();
                    let mut lines = vec![
                        "-- Change <name> to the extension you want, review, then run.".to_string(),
                    ];
                    for ext in extensions
                        .iter()
                        .filter(|e| !e.detail.as_deref().is_some_and(|d| d.contains("installed")))
                    {
                        lines.push(format!(
                            "-- {} — {}",
                            ext.name,
                            ext.detail.as_deref().unwrap_or("")
                        ));
                    }
                    let first = extensions
                        .iter()
                        .find(|e| !e.detail.as_deref().is_some_and(|d| d.contains("installed")))
                        .map(|e| e.name.as_str())
                        .unwrap_or("pg_stat_statements");
                    lines.push(format!("CREATE EXTENSION IF NOT EXISTS {first};"));
                    let _ = tx.send(Some(lines.join("\n")));
                })
                .await
                .ok();
        })
        .detach();

        let async_cx = cx.to_async();
        let weak_self = cx.weak_entity();
        cx.spawn(async move |_this, _cx| {
            let Ok(Some(sql)) = rx.await else {
                return;
            };
            let mut cx = async_cx.clone();
            weak_self
                .update(&mut cx, |this, cx| {
                    this.with_explorer_target_active(Some(target_profile), cx, move |app, cx| {
                        app.new_tab(cx, Some(sql));
                        cx.notify();
                    });
                })
                .ok();
        })
        .detach();
    }

    /// Explorer "View Dependencies" / "Find Usages": open the dependencies
    /// dialog for the selected object, on the node's own profile route.
    pub(super) fn open_object_dependencies(
        &mut self,
        context: &ExplorerActionContext,
        cx: &mut Context<Self>,
    ) {
        let Some(name) = context.name.clone() else {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.explorer.dependencies_select_object"),
            );
            cx.notify();
            return;
        };
        let Some(profile) = self
            .resolve_explorer_target(context.server_id.as_ref(), context.database.as_deref(), cx)
            .or_else(|| self.active_profile.clone())
        else {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.explorer.connect_dependencies"),
            );
            cx.notify();
            return;
        };
        if let Some(reason) = crate::capabilities::dependencies(profile.database_type).reason() {
            self.set_status(StatusLevel::Info, reason.to_string());
            cx.notify();
            return;
        }
        let schema = context.schema.clone().unwrap_or_default();
        let kind_label = match &context.kind {
            NodeKind::View { .. } => crate::localization::text("shell.explorer.kind_view"),
            NodeKind::Procedure { .. } => {
                crate::localization::text("shell.explorer.kind_procedure")
            }
            NodeKind::Function { .. } => crate::localization::text("shell.explorer.kind_function"),
            _ => crate::localization::text("shell.explorer.kind_table"),
        };
        if self.overlays.object_dependencies_pane.is_none() {
            let tool_context = crate::tool_dialog::ToolDialogContext::new(
                profile.clone(),
                self.services_for_profile(&profile),
            );
            let pane = cx.new(|cx| {
                crate::object_dependencies::ObjectDependenciesView::new(
                    tool_context,
                    schema,
                    name,
                    &kind_label,
                    cx,
                )
            });
            cx.subscribe(&pane, Self::handle_object_dependencies_event)
                .detach();
            self.overlays.object_dependencies_pane = Some(pane);
        }
        cx.notify();
    }

    pub(super) fn handle_object_dependencies_event(
        &mut self,
        _view: Entity<crate::object_dependencies::ObjectDependenciesView>,
        event: &crate::tool_dialog::ToolDialogEvent,
        cx: &mut Context<Self>,
    ) {
        if self.apply_tool_dialog_event(event, cx) {
            self.overlays.object_dependencies_pane = None;
            self.restore_editor_focus(cx);
            cx.notify();
        }
    }

    /// Explorer Run / Copy-EXEC on a procedure or function node: build the
    /// invocation script (typed parameter placeholders when the IntelliSense
    /// catalog for the *active* connection knows the object; a runnable
    /// scaffold otherwise) and either copy it or open it in a new tab
    /// retargeted to the object's own profile/database.
    pub(super) fn handle_procedure_invocation(
        &mut self,
        context: &ExplorerActionContext,
        copy_only: bool,
        cx: &mut Context<Self>,
    ) {
        let (Some(schema), Some(name)) = (context.schema.clone(), context.name.clone()) else {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.explorer.invocation_select_routine"),
            );
            cx.notify();
            return;
        };
        let is_function = matches!(context.kind, NodeKind::Function { .. });
        let target = self.resolve_explorer_target(
            context.server_id.as_ref(),
            context.database.as_deref(),
            cx,
        );
        // Render the call in the target engine's form (EXEC vs CALL), known
        // synchronously from the profile.
        let db_type = target
            .as_ref()
            .map(|p| p.database_type)
            .unwrap_or_else(|| self.active_db_type());
        let sql = if is_function {
            crate::table_actions::function_invocation_sql_for(db_type, &schema, &name)
        } else {
            // The loaded procedure catalog describes the ACTIVE connection's
            // database only; parameters from a same-named proc elsewhere
            // would be wrong, so any other target gets the plain scaffold.
            let target_is_active = self.active_profile.as_ref().is_some_and(|p| {
                Some(p.id.0) == context.server_id
                    && (context.database.is_none()
                        || p.database.as_deref() == context.database.as_deref())
            });
            let info = target_is_active
                .then(|| {
                    self.completion_procedures
                        .iter()
                        .find(|p| {
                            p.schema.eq_ignore_ascii_case(&schema)
                                && p.name.eq_ignore_ascii_case(&name)
                        })
                        .cloned()
                })
                .flatten();
            crate::table_actions::procedure_invocation_sql_for(
                db_type,
                &schema,
                &name,
                info.as_ref(),
            )
        };
        if copy_only {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(sql));
            self.set_status(
                StatusLevel::Success,
                crate::localization::format(
                    "shell.explorer.invocation_copied",
                    &[("schema", &schema), ("name", &name)],
                ),
            );
            cx.notify();
            return;
        }
        self.with_explorer_target_active(target, cx, move |app, cx| {
            app.new_tab(cx, Some(sql));
            cx.notify();
        });
    }

    /// Route an explorer AI action (Ask AI / Generate SQL / Find Join Path /
    /// Pin to AI Context) into the AI panel: resolve the *selected object's*
    /// profile and database — never the active tab's — swap the panel onto
    /// that profile's service route, open the right lane, and seed the
    /// object's qualified name.
    pub(super) fn route_explorer_ai_action(
        &mut self,
        context: &ExplorerActionContext,
        action: &ExplorerMenuAction,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self.resolve_explorer_target(
            context.server_id.as_ref(),
            context.database.as_deref(),
            cx,
        ) else {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.explorer.connect_ai"),
            );
            cx.notify();
            return;
        };
        // Plain schema-qualified name: the AI endpoints match object names,
        // and bracket quoting would just pollute prompts and join lookups.
        let object = match (&context.schema, &context.name) {
            (Some(schema), Some(name)) => format!("{schema}.{name}"),
            (None, Some(name)) => name.clone(),
            _ => context.title.clone(),
        };
        let services = self.services_for_profile(&profile);
        self.ai_visible = true;
        let pinned_status = matches!(action, ExplorerMenuAction::PinToContext).then(|| {
            crate::localization::format("shell.explorer.ai_context_pinned", &[("object", &object)])
        });
        let action = action.clone();
        self.ai_panel.update(cx, |panel, cx| {
            panel.set_services(services, cx);
            panel.retarget_profile(profile, cx);
            match action {
                ExplorerMenuAction::AskSchema => panel.open_ask_about_object(&object, cx),
                ExplorerMenuAction::GenerateSql => panel.open_generate_for_object(&object, cx),
                ExplorerMenuAction::FindJoinPath => panel.open_join_path_from(&object, cx),
                ExplorerMenuAction::PinToContext => panel.pin_context_object(&object, cx),
                _ => {}
            }
        });
        if let Some(status) = pinned_status {
            self.set_status(StatusLevel::Info, status);
        }
        self.save_workspace_session(cx);
        cx.notify();
    }

    pub(super) fn resolve_explorer_target(
        &self,
        server_id: Option<&uuid::Uuid>,
        database: Option<&str>,
        cx: &Context<Self>,
    ) -> Option<ConnectionProfile> {
        let id = *server_id?;
        let mut profile = self
            .sidebar
            .read(cx)
            .connections()
            .iter()
            .find(|p| p.id.0 == id)
            .cloned()
            .or_else(|| ConnectionStore::global().get(id))?;
        if let Some(db) = database.filter(|d| !d.is_empty()) {
            profile.database = Some(db.to_string());
        }
        Some(profile)
    }

    pub(super) fn quick_open_reveal(
        &mut self,
        candidate: &crate::quick_open::QuickOpenCandidate,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let revealed = self
            .sidebar
            .update(cx, |s, cx| s.reveal_node_id(&candidate.node_id, cx));
        if revealed {
            self.handle_focus_explorer(&FocusExplorer, window, cx);
        } else {
            self.set_status(
                StatusLevel::Info,
                crate::localization::format(
                    "shell.explorer.quick_open_not_loaded",
                    &[("name", &candidate.title)],
                ),
            );
        }
    }

    pub(super) fn quick_open_select_rows(
        &mut self,
        candidate: &crate::quick_open::QuickOpenCandidate,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::quick_open::QuickOpenKind;
        use crate::table_actions::{QualifiedTable, select_top_rows_sql_for};

        match candidate.kind {
            QuickOpenKind::Procedure | QuickOpenKind::Function => {
                // There is no sensible SELECT for a routine; open the definition instead.
                self.quick_open_definition(candidate, window, cx);
            }
            QuickOpenKind::Server | QuickOpenKind::Database => {
                let target = self.resolve_explorer_target(
                    candidate.server_id.as_ref(),
                    candidate.database.as_deref(),
                    cx,
                );
                self.with_explorer_target_active(target, cx, move |app, cx| {
                    app.new_tab(cx, None);
                    app.set_status(
                        StatusLevel::Info,
                        crate::localization::text(
                            "shell.explorer.quick_open_new_query_selected_server",
                        ),
                    );
                    cx.notify();
                });
            }
            QuickOpenKind::Table | QuickOpenKind::View => {
                let target = self.resolve_explorer_target(
                    candidate.server_id.as_ref(),
                    candidate.database.as_deref(),
                    cx,
                );
                let db_type = target
                    .as_ref()
                    .map(|p| p.database_type)
                    .unwrap_or_else(|| self.active_db_type());
                let qt = QualifiedTable {
                    database: None,
                    schema: candidate.schema.clone(),
                    table: candidate.name.clone(),
                };
                let sql = select_top_rows_sql_for(db_type, 1000, &qt);
                self.with_explorer_target_active(target, cx, move |app, cx| {
                    app.new_tab(cx, Some(sql));
                    app.set_status(
                        StatusLevel::Info,
                        crate::localization::text("shell.explorer.quick_open_sql_opened_new_tab"),
                    );
                    cx.notify();
                });
            }
        }
    }

    pub(super) fn quick_open_definition(
        &mut self,
        candidate: &crate::quick_open::QuickOpenCandidate,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::quick_open::QuickOpenKind;

        match candidate.kind {
            QuickOpenKind::Server | QuickOpenKind::Database => {
                self.set_status(
                    StatusLevel::Info,
                    crate::localization::text("shell.explorer.definition_supported_objects"),
                );
                self.quick_open_reveal(candidate, window, cx);
            }
            _ => {
                let schema = candidate.schema.clone().unwrap_or_default();
                let kind = match candidate.kind {
                    QuickOpenKind::Table => NodeKind::Table {
                        schema: schema.clone(),
                        name: candidate.name.clone(),
                    },
                    QuickOpenKind::View => NodeKind::View {
                        schema: schema.clone(),
                        name: candidate.name.clone(),
                    },
                    QuickOpenKind::Procedure => NodeKind::Procedure {
                        schema: schema.clone(),
                        name: candidate.name.clone(),
                    },
                    QuickOpenKind::Function => NodeKind::Function {
                        schema: schema.clone(),
                        name: candidate.name.clone(),
                    },
                    QuickOpenKind::Server | QuickOpenKind::Database => unreachable!(),
                };
                let ctx = ExplorerActionContext {
                    node_id: candidate.node_id.clone(),
                    title: candidate.title.clone(),
                    kind,
                    server_id: candidate.server_id,
                    database: candidate.database.clone(),
                    schema: candidate.schema.clone(),
                    name: Some(candidate.name.clone()),
                    table: None,
                    copy_name: None,
                    copy_qualified: None,
                    columns: vec![],
                    profile: None,
                    position: gpui::point(px(0.0), px(0.0)),
                };
                self.handle_edit_definition(&ctx, false, cx);
            }
        }
    }

    /// Make the explorer target the active connection, then run `then`.
    /// The shared retarget contract for every explorer gesture that produces
    /// SQL (double-click, context-menu Select/Script actions):
    /// - unresolvable target → `then` runs now, against the current connection;
    /// - already active → `then` runs now;
    /// - same server, other database → synchronous switch, `then` runs now;
    /// - other server → async connect (Entra token, relay session); `then`
    ///   waits for [`Self::mark_connected`] and is dropped if the connect
    ///   fails, so no SQL ever lands on the wrong connection.
    pub(super) fn with_explorer_target_active(
        &mut self,
        target: Option<ConnectionProfile>,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, &mut Context<Self>) + 'static,
    ) {
        match target {
            Some(profile) => match retarget_decision(self.active_profile.as_ref(), &profile) {
                RetargetDecision::Connect => self.connect_profile_then(profile, cx, then),
                RetargetDecision::SwitchDatabase(db) => {
                    // `switch_database` retargets `active_profile`
                    // synchronously (only its sidebar/completion refresh is
                    // async), so `then` can run right away and execution will
                    // see the new database.
                    self.switch_database(db, cx);
                    then(self, cx);
                }
                RetargetDecision::AlreadyActive => then(self, cx),
            },
            None => then(self, cx),
        }
    }

    /// Open a Tools-submenu dialog on the node that was right-clicked.
    ///
    /// Every one of these tools reads the *active* connection, so the target
    /// has to become active first. Switching databases is synchronous, so the
    /// tool opens right away; connecting to another server is not, and the tool
    /// is queued for the next render instead — that is also where a `Window`
    /// exists again, which the async path has none of.
    pub(super) fn open_explorer_tool(
        &mut self,
        tool: ExplorerTool,
        target: Option<ConnectionProfile>,
        scope: crate::tool_dialog::ToolScope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = target else {
            // No resolvable node profile (an unsaved or Azure-only node): open
            // against the current connection rather than doing nothing.
            self.dispatch_explorer_tool(tool, scope, window, cx);
            return;
        };
        match retarget_decision(self.active_profile.as_ref(), &profile) {
            RetargetDecision::Connect => {
                self.connect_profile_then(profile, cx, move |app, cx| {
                    app.overlays.pending_explorer_tool = Some((tool, scope));
                    cx.notify();
                });
            }
            RetargetDecision::SwitchDatabase(db) => {
                self.switch_database(db, cx);
                self.dispatch_explorer_tool(tool, scope, window, cx);
            }
            RetargetDecision::AlreadyActive => self.dispatch_explorer_tool(tool, scope, window, cx),
        }
    }

    /// Open the tool's dialog. The connection is already the one the user
    /// clicked; each handler is the same one the application menu used to call.
    /// `scope` narrows the few tools that can address a single database (Disk
    /// Usage, Activity Monitor, Extended Events); every other tool ignores it.
    pub(super) fn dispatch_explorer_tool(
        &mut self,
        tool: ExplorerTool,
        scope: crate::tool_dialog::ToolScope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match tool {
            ExplorerTool::ServerDashboard => {
                self.handle_server_dashboard(&ServerDashboard, window, cx)
            }
            ExplorerTool::SqlAgent => self.handle_sql_agent(&SqlAgent, window, cx),
            ExplorerTool::Security => self.handle_security(&Security, window, cx),
            ExplorerTool::ConnectionHealth => {
                self.handle_connection_health(&ConnectionHealth, window, cx)
            }
            ExplorerTool::BackupRestore => self.handle_backup_restore(&BackupRestore, window, cx),
            ExplorerTool::ActivityMonitor => self.open_activity_monitor(scope, window, cx),
            ExplorerTool::Designers => self.handle_designers(&Designers, window, cx),
            ExplorerTool::ImportExport => self.handle_import_export(&ImportExport, window, cx),
            ExplorerTool::SchemaCompare => self.handle_schema_compare(&SchemaCompare, window, cx),
            ExplorerTool::DataCompare => self.handle_data_compare(&DataCompare, window, cx),
            #[cfg(not(target_arch = "wasm32"))]
            ExplorerTool::DataTransfer => self.handle_data_transfer(&DataTransfer, window, cx),
            ExplorerTool::QueryInsights => self.handle_query_store(&QueryStore, window, cx),
            ExplorerTool::DiskUsage => self.open_disk_usage(scope, cx),
            ExplorerTool::IndexAnalyzer => {
                self.handle_index_analyzer(&IndexAnalyzerTool, window, cx)
            }
            ExplorerTool::TestDataGenerator => {
                self.handle_test_data_generator(&TestDataGeneratorTool, window, cx)
            }
            ExplorerTool::RelationshipGraph => {
                self.handle_relationship_graph(&RelationshipGraph, window, cx)
            }
            ExplorerTool::QueryBuilder => self.handle_query_builder(&QueryBuilder, window, cx),
            ExplorerTool::DocumentSchema => {
                self.handle_export_schema_docs(&ExportSchemaDocs, window, cx)
            }
            ExplorerTool::TenantScoping => self.handle_tenant_scoping(&TenantScoping, window, cx),
            ExplorerTool::QueryHistory => {
                self.handle_show_query_history(&ShowQueryHistory, window, cx)
            }
            ExplorerTool::ExtendedEvents => self.open_extended_events(scope, window, cx),
            ExplorerTool::RedisProfiler => {
                self.open_redis_live_tab(crate::activity_monitor::ActivityTab::Profiler, window, cx)
            }
            ExplorerTool::RedisPubSub => {
                self.open_redis_live_tab(crate::activity_monitor::ActivityTab::PubSub, window, cx)
            }
        }
    }

    /// The Properties command with nothing selected: the active connection's
    /// own server pages, the same dialog the server node's context menu opens.
    pub(super) fn handle_open_object_properties(
        &mut self,
        _action: &OpenObjectProperties,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::object_editor_model::{EditorObjectKind, EditorTarget};

        if self.overlays.object_editor.is_some() {
            return;
        }
        let Some(profile) = self.active_profile.clone() else {
            self.set_status(
                StatusLevel::Error,
                crate::localization::text("shell.explorer.connect_server_properties"),
            );
            cx.notify();
            return;
        };
        let target = EditorTarget::new(
            EditorObjectKind::Server,
            profile.database.clone().unwrap_or_default(),
            String::new(),
            profile.name.clone(),
        );
        // Follows the active connection's route (see the explorer Properties
        // branch for the same rule).
        let services = self.services_for_profile(&profile);
        let production_read_only = self.production_writes_locked(&profile);
        let view = cx.new(|cx| {
            crate::object_editor::ObjectEditorView::new(
                services,
                profile,
                target,
                production_read_only,
                window,
                cx,
            )
        });
        cx.subscribe(&view, Self::handle_object_editor_event)
            .detach();
        self.overlays.object_editor = Some(view);
        cx.notify();
    }

    pub(super) fn handle_object_editor_event(
        &mut self,
        _: Entity<crate::object_editor::ObjectEditorView>,
        event: &crate::object_editor::ObjectEditorEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            crate::object_editor::ObjectEditorEvent::OpenQuery(sql) => {
                self.new_tab(cx, Some(sql.clone()));
                // The editor stays open: scripting is a side trip, not the
                // end of the editing session.
                self.set_status(
                    StatusLevel::Info,
                    crate::localization::text("shell.explorer.script_opened_new_query"),
                );
                cx.notify();
            }
            crate::object_editor::ObjectEditorEvent::Closed => {
                self.restore_editor_focus(cx);
                self.overlays.object_editor = None;
                cx.notify();
            }
        }
    }

    /// Reload what a schema change made stale: the changed server's explorer
    /// subtree (expanded nodes, an altered table's columns included, reload
    /// from the server) and the active connection's IntelliSense catalog. The
    /// epoch bump keeps the old snapshot from re-seeding the tree meanwhile.
    pub(super) fn refresh_schema_after_change(
        &mut self,
        server_id: uuid::Uuid,
        cx: &mut Context<Self>,
    ) {
        self.schema_change_epoch = self.schema_change_epoch.wrapping_add(1);
        self.sidebar
            .update(cx, |s, cx| s.refresh_server_schema(server_id, cx));
        if let Some(profile) = self.active_profile.clone() {
            self.refresh_completion_catalog(profile, cx);
        }
    }

    pub(super) fn handle_refresh_object_explorer(
        &mut self,
        _action: &RefreshObjectExplorer,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sidebar.update(cx, |s, cx| s.refresh_connections(cx));
        if let Some(id) = self.active_profile.as_ref().map(|p| p.id.0) {
            self.refresh_schema_after_change(id, cx);
        }
        self.set_status(
            StatusLevel::Success,
            crate::localization::text("shell.explorer.refreshed"),
        );
        cx.notify();
    }

    pub(super) fn insert_dragged_identifier(
        &mut self,
        d: crate::schema_tree::DraggedIdentifier,
        cx: &mut Context<Self>,
    ) {
        let db = self
            .resolve_explorer_target(d.server_id.as_ref(), d.database.as_deref(), cx)
            .map(|p| p.database_type)
            .unwrap_or_else(|| self.active_db_type());
        let text = dragged_identifier_text(db, &d);
        if let Some(tab) = self.tabs.get(self.active_tab) {
            tab.editor.update(cx, |ed, cx| ed.insert(&text, cx));
        } else {
            self.new_tab(cx, Some(text));
        }
        self.set_status(
            StatusLevel::Info,
            crate::localization::text("shell.explorer.identifier_inserted"),
        );
        cx.notify();
    }
}

pub(super) fn dragged_identifier_text(
    db: sqlly_lang::sqlmodel::DatabaseType,
    d: &crate::schema_tree::DraggedIdentifier,
) -> String {
    if let Some(column) = &d.column {
        sqlly_services::identifier_quoting::quote_identifier(db, column)
    } else {
        sqlly_services::identifier_quoting::qualified_identifier(db, d.schema.as_deref(), &d.object)
    }
}

/// Why preparing an Open in Terminal launch failed, carried back unworded
/// from the background executor so the UI thread can word it.
#[cfg(not(target_arch = "wasm32"))]
enum TerminalPrepareFailure {
    Credentials(sqlly_core::DisplayMessage),
    Task(String),
    /// A complete keyed message from `prepare_launch`.
    Refused(sqlly_core::DisplayMessage),
}

#[cfg(test)]
mod tests {
    use super::dragged_identifier_text;
    use crate::schema_tree::DraggedIdentifier;
    use sqlly_lang::sqlmodel::DatabaseType;

    #[test]
    fn dragged_identifier_text_is_dialect_quoted() {
        let table = DraggedIdentifier {
            server_id: None,
            database: None,
            schema: Some("dbo".into()),
            object: "Orders".into(),
            column: None,
        };
        assert_eq!(
            dragged_identifier_text(DatabaseType::SqlServer, &table),
            "[dbo].[Orders]"
        );
        let column = DraggedIdentifier {
            server_id: None,
            database: None,
            schema: Some("public".into()),
            object: "orders".into(),
            column: Some("order_id".into()),
        };
        assert_eq!(
            dragged_identifier_text(DatabaseType::Postgres, &column),
            "\"order_id\""
        );
        let mysql = DraggedIdentifier {
            server_id: None,
            database: None,
            schema: None,
            object: "orders".into(),
            column: None,
        };
        assert_eq!(
            dragged_identifier_text(DatabaseType::MySql, &mysql),
            "`orders`"
        );
    }
}
