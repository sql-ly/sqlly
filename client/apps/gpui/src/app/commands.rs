use super::*;
use crate::command_palette::ObjectAction;
use gpui_component::WindowExt as _;

impl AppView {
    /// Reinstall the application menus when the connection state has toggled
    /// or the active engine has changed since they were last built, so the
    /// Tools menu shows Connect while disconnected and Disconnect while
    /// connected, and the in-window menu bar grays engine-gated tools with
    /// the capability reason. Compared against cached fingerprints so
    /// steady-state frames do no work; the reinstall itself is deferred out
    /// of the render pass. Covers every path that sets or clears
    /// `active_profile` (connect, disconnect, drop, reconnect) without each
    /// having to remember to call it.
    pub(super) fn sync_connection_menus(&mut self, cx: &mut Context<Self>) {
        let connected = self.active_profile.is_some();
        let engine = self.active_profile.as_ref().map(|p| p.database_type);
        let localization_revision = crate::localization::chrome_revision();
        if connected == self.menus_reflect_connected
            && engine == self.menus_reflect_engine
            && localization_revision == self.menus_reflect_localization_revision
        {
            return;
        }
        self.menus_reflect_connected = connected;
        self.menus_reflect_engine = engine;
        self.menus_reflect_localization_revision = localization_revision;
        self.production_unlock = None;
        self.production_unlock_tick = None;
        let bar = self.window_menu_bar.clone();
        cx.defer(move |app| {
            // Native macOS application menu. gpui's native menu items carry no
            // per-item enabled state or tooltip, so engine gating there stays
            // at invocation time (`require_admin_tool` reports the reason).
            app.set_menus(crate::menu_bar::build_menus(connected));
            // Windows/Linux in-window menu bar (idle on macOS) — this one is
            // ours, so unavailable engine-gated actions gray out with the
            // capability reason beside them.
            bar.update(app, |bar, cx| {
                bar.set_menus(crate::menu_bar::build_menus(connected));
                bar.set_disabled_actions(Self::engine_gated_menu_actions(engine));
                cx.notify();
            });
        });
    }

    /// The menu actions unavailable on the given engine, keyed by action name,
    /// with their user-facing capability reasons. Disconnected (or unknown
    /// engine) yields an empty map: the Tools menu already reads Connect...
    /// and every launcher re-checks at invocation time.
    /// Pairs every engine-gated menu action with the palette command id whose
    /// capability governs it, so the in-window menu bar and the command
    /// palette share one capability matrix (`capabilities::command_capability`)
    /// instead of maintaining parallel lists that can drift.
    pub(super) fn engine_gated_action_commands() -> [(&'static str, &'static str); 21] {
        use gpui::Action;
        [
            (DiskUsageTool::name_for_type(), "tools.diskUsage"),
            (ObjectBrowserTool::name_for_type(), "tools.objectBrowser"),
            (IndexAnalyzerTool::name_for_type(), "tools.indexAnalyzer"),
            (
                TestDataGeneratorTool::name_for_type(),
                "tools.testDataGenerator",
            ),
            (BackupRestore::name_for_type(), "tools.backupRestore"),
            (
                ExportDatabaseSql::name_for_type(),
                "tools.exportDatabaseSql",
            ),
            (ServerDashboard::name_for_type(), "tools.serverDashboard"),
            (ActivityMonitor::name_for_type(), "tools.activityMonitor"),
            (SqlAgent::name_for_type(), "tools.sqlAgent"),
            (Security::name_for_type(), "tools.security"),
            (Designers::name_for_type(), "tools.designers"),
            ("Edit Structure…", "tools.structure_editor"),
            (SchemaCompare::name_for_type(), "tools.schemaCompare"),
            (SchemaHistory::name_for_type(), "tools.schemaHistory"),
            (
                SaveSchemaSnapshot::name_for_type(),
                "tools.saveSchemaSnapshot",
            ),
            (DataCompare::name_for_type(), "tools.dataCompare"),
            (DataTransfer::name_for_type(), "tools.dataTransfer"),
            (QueryStore::name_for_type(), "tools.queryStore"),
            (ExtendedEvents::name_for_type(), "tools.extendedEvents"),
            (ExecutionPlanViewer::name_for_type(), "query.explainPlan"),
            (
                EstimatedPlanViewer::name_for_type(),
                "query.explainEstimated",
            ),
        ]
    }

    pub(super) fn engine_gated_menu_actions(
        engine: Option<sqlly_lang::sqlmodel::DatabaseType>,
    ) -> std::collections::HashMap<&'static str, String> {
        let Some(db) = engine else {
            return std::collections::HashMap::new();
        };
        Self::engine_gated_action_commands()
            .into_iter()
            .filter_map(|(name, command_id)| {
                crate::capabilities::command_unavailable_reason(command_id, Some(db))
                    .map(|reason| (name, reason))
            })
            .collect()
    }

    pub(super) fn handle_toggle_command_palette(
        &mut self,
        _action: &ToggleCommandPalette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.overlays.command_palette.is_some() {
            self.restore_workbench_focus(Some(window), cx);
            self.overlays.command_palette = None;
        } else {
            self.open_command_palette(window, cx);
        }
        cx.notify();
    }

    pub(super) fn handle_quick_open(
        &mut self,
        _action: &QuickOpen,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.overlays.command_palette.is_some() {
            self.restore_workbench_focus(Some(window), cx);
            self.overlays.command_palette = None;
        } else {
            self.open_quick_open(window, cx);
        }
        cx.notify();
    }

    pub(super) fn open_quick_open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let objects = self.sidebar.read(cx).quick_open_candidates(cx);
        // Ranking favors the server the user is currently querying against —
        // the focused tab's binding, a window property, not a global one.
        let active = self
            .tabs
            .get(self.active_tab)
            .and_then(|tab| tab.connection_target.as_ref())
            .map(|target| target.profile_id)
            .or_else(|| self.active_profile.as_ref().map(|p| p.id.0));
        let palette = cx.new(|cx| {
            CommandPalette::new_objects(self.command_registry.clone(), objects, active, window, cx)
        });
        palette.update(cx, |palette, cx| palette.focus_search(window, cx));
        cx.subscribe_in(&palette, window, Self::handle_command_palette_event)
            .detach();
        self.overlays.command_palette = Some(palette);
    }

    pub(super) fn open_command_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let database_type = self.active_profile.as_ref().map(|p| p.database_type);
        let unavailable_reasons = self
            .command_registry
            .all()
            .iter()
            .filter_map(|command| {
                crate::capabilities::command_unavailable_reason(&command.id, database_type)
                    .map(|reason| (command.id.clone(), reason))
            })
            .collect();
        let palette = cx.new(|cx| {
            CommandPalette::new(
                self.command_registry.clone(),
                unavailable_reasons,
                window,
                cx,
            )
        });
        palette.update(cx, |palette, cx| palette.focus_search(window, cx));
        cx.subscribe_in(&palette, window, Self::handle_command_palette_event)
            .detach();
        self.overlays.command_palette = Some(palette);
    }

    pub(super) fn handle_command_palette_event(
        &mut self,
        _palette: &Entity<CommandPalette>,
        event: &CommandPaletteEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            CommandPaletteEvent::Execute(cmd_id) => {
                self.restore_workbench_focus(Some(window), cx);
                self.overlays.command_palette = None;
                let cmd_id = cmd_id.clone();
                self.execute_command(&cmd_id, window, cx);
            }
            CommandPaletteEvent::Unavailable(reason) => {
                self.overlays.command_palette = None;
                self.set_status(StatusLevel::Info, reason.clone());
                cx.notify();
            }
            CommandPaletteEvent::Cancelled => {
                self.restore_workbench_focus(Some(window), cx);
                self.overlays.command_palette = None;
                cx.notify();
            }
            CommandPaletteEvent::Object { candidate, action } => {
                self.overlays.command_palette = None;
                let candidate = candidate.clone();
                match action {
                    ObjectAction::Reveal => self.quick_open_reveal(&candidate, window, cx),
                    ObjectAction::SelectRows => self.quick_open_select_rows(&candidate, window, cx),
                    ObjectAction::Definition => self.quick_open_definition(&candidate, window, cx),
                }
                cx.notify();
            }
        }
    }

    pub(super) fn execute_command(
        &mut self,
        cmd_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(reason) = crate::capabilities::command_unavailable_reason(
            cmd_id,
            self.active_profile.as_ref().map(|p| p.database_type),
        ) {
            self.set_status(StatusLevel::Info, reason);
            cx.notify();
            return;
        }
        match cmd_id {
            "file.newQuery" => self.new_tab(cx, None),
            "file.newNotebook" => {
                self.new_notebook_tab(crate::notebook_model::Notebook::new(), None, cx);
            }
            "file.openNotebook" => self.open_notebook(cx),
            "file.saveNotebook" => {
                if let Some((idx, _)) = self.active_notebook() {
                    self.save_notebook(idx, false, cx);
                } else {
                    self.save_notebook_required_status(cx);
                }
            }
            "query.runNotebookCell" => self.handle_run_notebook_cell_inner(window, cx),
            "query.runAllNotebookCells" => self.handle_run_all_notebook_cells_inner(cx),
            "file.saveWorkspaceAs" => self.handle_save_workspace_as(&SaveWorkspaceAs, window, cx),
            "file.openWorkspace" => self.handle_open_workspace(&OpenWorkspace, window, cx),
            "file.manageWorkspaces" => self.handle_manage_workspaces(&ManageWorkspaces, window, cx),
            "file.closeTab" => self.request_close_tab(self.active_tab, window, cx),
            "tab.rename" => self.begin_tab_rename(self.active_tab, cx),
            // Cross-window work is deferred inside `move_tab_to_window`.
            #[cfg(not(target_arch = "wasm32"))]
            "tab.moveToNewWindow" => self.move_tab_to_window(self.active_tab, None, cx),
            #[cfg(target_arch = "wasm32")]
            "tab.moveToNewWindow" => {
                self.set_status(
                    StatusLevel::Info,
                    crate::localization::text("shell.commands.single_browser_window"),
                );
                cx.notify();
            }
            "file.reopenClosedTab" => self.handle_reopen_closed_tab(&ReopenClosedTab, window, cx),
            "tools.resultSnapshots" => self.handle_result_snapshots(&ResultSnapshots, window, cx),
            "tools.savedCharts" => self.handle_saved_charts(&SavedCharts, window, cx),
            "tools.chartDashboard" => self.handle_chart_dashboard(&ChartDashboardTool, window, cx),
            "results.filterRows" => {
                if let Some(tab) = self.tabs.get(self.active_tab) {
                    tab.results
                        .update(cx, |v, cx| v.open_filter_builder(v.active_tab_index(), cx));
                }
            }
            "results.sortAscending" => {
                if let Some(tab) = self.tabs.get(self.active_tab) {
                    tab.results
                        .update(cx, |v, cx| v.sort_active_column(false, cx));
                }
            }
            "results.sortDescending" => {
                if let Some(tab) = self.tabs.get(self.active_tab) {
                    tab.results
                        .update(cx, |v, cx| v.sort_active_column(true, cx));
                }
            }
            "results.transpose" => {
                if let Some(tab) = self.tabs.get(self.active_tab) {
                    tab.results
                        .update(cx, |v, cx| v.toggle_transpose(v.active_tab_index(), cx));
                }
            }
            "results.stage_undo" => {
                if let Some(tab) = self.tabs.get(self.active_tab) {
                    tab.results.update(cx, |v, cx| v.stage_undo(cx));
                }
            }
            "results.stage_redo" => {
                if let Some(tab) = self.tabs.get(self.active_tab) {
                    tab.results.update(cx, |v, cx| v.stage_redo(cx));
                }
            }
            "results.save_staged" => {
                if let Some(tab) = self.tabs.get(self.active_tab) {
                    tab.results.update(cx, |v, cx| v.save_staged(cx));
                }
            }
            "results.discard_staged" => {
                if let Some(tab) = self.tabs.get(self.active_tab) {
                    tab.results.update(cx, |v, cx| v.discard_staged(cx));
                }
            }
            "results.preview_staged_sql" => {
                if let Some(tab) = self.tabs.get(self.active_tab) {
                    tab.results.update(cx, |v, cx| v.preview_staged_sql(cx));
                }
            }
            "results.copy_staged_sql" => {
                if let Some(tab) = self.tabs.get(self.active_tab) {
                    tab.results.update(cx, |v, cx| {
                        v.copy_preview_sql(cx);
                    });
                }
            }
            "results.findInGrid" => {
                if let Some(tab) = self.tabs.get(self.active_tab) {
                    tab.results.update(cx, |v, cx| v.toggle_grid_find(cx));
                }
            }
            "results.gotoColumn" => {
                window.dispatch_action(Box::new(GotoColumnInResults), cx);
            }
            "results.focusToggle" => {
                window.dispatch_action(Box::new(ToggleEditorResultsFocus), cx);
            }
            "results.stageInsertRow" => {
                if let Some(tab) = self.tabs.get(self.active_tab) {
                    tab.results.update(cx, |v, cx| v.stage_insert_from_grid(cx));
                }
            }
            "results.stageDeleteRow" => {
                if let Some(tab) = self.tabs.get(self.active_tab) {
                    tab.results.update(cx, |v, cx| v.stage_delete_from_grid(cx));
                }
            }
            "results.refresh" => self.execute_query(self.active_tab, cx),
            "tools.connectionHealth" => {
                self.handle_connection_health(&ConnectionHealth, window, cx)
            }
            "tools.relationshipGraph" => {
                self.handle_relationship_graph(&RelationshipGraph, window, cx)
            }
            "tools.queryBuilder" => self.handle_query_builder(&QueryBuilder, window, cx),
            "query.execute" => self.execute_query(self.active_tab, cx),
            "query.executeKeepResults" => self.execute_query_keep_results(self.active_tab, cx),
            "query.runPicker" => self.handle_run_picker(&RunPicker, window, cx),
            "query.executeNoRollback" => {
                // Run once with the connection's rollback wrap disabled — for
                // deploying DDL (which the wrap would roll back) or a batch the
                // wrap's leading BEGIN TRANSACTION would break. One-shot: the
                // flag is consumed at dispatch, never left armed.
                self.execute_rollback_bypass_once = true;
                self.execute_query(self.active_tab, cx);
            }
            "safety.unlockWrites1m" => self
                .unlock_production_writes(sqlly_intel::query::safety::UnlockWindow::OneMinute, cx),
            "safety.unlockWrites5m" => self.unlock_production_writes(
                sqlly_intel::query::safety::UnlockWindow::FiveMinutes,
                cx,
            ),
            "safety.unlockWrites15m" => self.unlock_production_writes(
                sqlly_intel::query::safety::UnlockWindow::FifteenMinutes,
                cx,
            ),
            "safety.lockWrites" => self.lock_production_writes(cx),
            "transaction.toggleMode" => self.toggle_transaction_mode(self.active_tab, cx),
            "transaction.commit" => self.commit_transaction(self.active_tab, cx),
            "transaction.rollback" => self.rollback_transaction(self.active_tab, cx),
            "safety.unlockReadOnly1m" => {
                self.unlock_read_only(sqlly_intel::query::safety::UnlockWindow::OneMinute, cx)
            }
            "safety.unlockReadOnly5m" => {
                self.unlock_read_only(sqlly_intel::query::safety::UnlockWindow::FiveMinutes, cx)
            }
            "safety.unlockReadOnly15m" => {
                self.unlock_read_only(sqlly_intel::query::safety::UnlockWindow::FifteenMinutes, cx)
            }
            "safety.lockReadOnly" => self.lock_read_only(cx),
            "query.explainPlan" => self.explain_active_query(cx),
            "query.explainEstimated" => self.explain_estimated_query(cx),
            "plan.profile" => {
                self.explain_with_preset(sqlly_services::plans::ExplainPreset::Profile, cx)
            }
            "plan.open" => self.open_plan_file(cx),
            "plan.paste" => self.paste_plan(cx),
            "query.history" => self.handle_show_query_history(&ShowQueryHistory, window, cx),
            "query.cancel" => self.cancel_query_no_window(cx),
            "query.runOnTargets" => self.handle_run_on_targets(&RunOnTargets, window, cx),
            "results.autoRefreshOff" => self.set_active_auto_refresh(None, cx),
            "results.autoRefresh5s" => self.set_active_auto_refresh(Some(5), cx),
            "results.autoRefresh10s" => self.set_active_auto_refresh(Some(10), cx),
            "results.autoRefresh30s" => self.set_active_auto_refresh(Some(30), cx),
            "results.autoRefresh60s" => self.set_active_auto_refresh(Some(60), cx),
            "query.validate" => self.validate_sql(self.active_tab, cx),
            "query.formatSql" => {
                if let Some(tab) = self.tabs.get(self.active_tab) {
                    let editor = tab.editor.clone();
                    editor.update(cx, |ed, cx| ed.format_sql_buffer(cx));
                }
            }
            "editor.formatDocument" => {
                if let Some(tab) = self.tabs.get(self.active_tab) {
                    let editor = tab.editor.clone();
                    editor.update(cx, |ed, cx| ed.format_sql_document(cx));
                }
            }
            "editor.refreshDates" => {
                if let Some(tab) = self.tabs.get(self.active_tab) {
                    let editor = tab.editor.clone();
                    let changed = editor.update(cx, |ed, cx| ed.refresh_all_dates(cx));
                    self.set_status(
                        StatusLevel::Info,
                        match changed {
                            0 => crate::localization::text("shell.commands.dates_current"),
                            1 => crate::localization::text("shell.commands.date_refreshed"),
                            n => crate::localization::format(
                                "shell.commands.dates_refreshed",
                                &[("count", &n.to_string())],
                            ),
                        },
                    );
                    cx.notify();
                }
            }
            "editor.minify" => {
                if let Some(tab) = self.tabs.get(self.active_tab) {
                    let editor = tab.editor.clone();
                    editor.update(cx, |ed, cx| ed.minify_sql_buffer(cx));
                }
            }
            "edit.find" => {
                if let Some(tab) = self.tabs.get(self.active_tab) {
                    let editor = tab.editor.clone();
                    editor.update(cx, |ed, cx| ed.open_find_bar(false, window, cx));
                }
            }
            "view.splitVertical" => {
                self.split_editor(crate::editor_groups::SplitAxis::Vertical, window, cx)
            }
            "view.splitHorizontal" => {
                self.split_editor(crate::editor_groups::SplitAxis::Horizontal, window, cx)
            }
            "view.splitSameBufferVertical" => {
                self.split_same_buffer(crate::editor_groups::SplitAxis::Vertical, window, cx)
            }
            "view.splitSameBufferHorizontal" => {
                self.split_same_buffer(crate::editor_groups::SplitAxis::Horizontal, window, cx)
            }
            "view.closeSplit" => self.close_split(cx),
            "edit.replace" => {
                if let Some(tab) = self.tabs.get(self.active_tab) {
                    let editor = tab.editor.clone();
                    editor.update(cx, |ed, cx| ed.open_find_bar(true, window, cx));
                }
            }
            "editor.toggleVimMode" => {
                self.editor_prefs.vim_mode = !self.editor_prefs.vim_mode;
                crate::preferences::PreferencesStore::new().set_editor(self.editor_prefs.clone());
                for tab in &self.tabs {
                    tab.editor.update(cx, |ed, cx| {
                        ed.set_editor_prefs(self.editor_prefs.clone(), cx)
                    });
                }
                cx.notify();
            }
            "view.toggleHistoryPanel" => {
                #[cfg(not(target_arch = "wasm32"))]
                {
                    let active = self.active_tab;
                    self.toggle_history_panel(active, cx);
                }
            }
            "view.toggleSidebar" => {
                self.handle_toggle_sidebar(&ToggleSidebar, window, cx);
            }
            "view.toggleAiPanel" | "view.toggleAiVisibility" => {
                self.handle_toggle_ai_panel(&ToggleAiPanel, window, cx);
            }
            "ai.toggleAgentMode" => {
                self.handle_toggle_agent_mode(&ToggleAgentMode, window, cx);
            }
            "view.toggleLogConsole" => {
                self.log_console.update(cx, |view, cx| {
                    view.toggle_visibility(cx);
                });
                cx.notify();
            }
            "connection.add" => {
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
            "connection.disconnect" => {
                // The command acts on the focused window's connection — the
                // command's context. Per-server disconnects for other servers
                // live on the server explorer's right-click menu.
                if let Some(profile_id) = self.active_profile.as_ref().map(|p| p.id.0) {
                    self.disconnect_profile(profile_id, cx);
                } else {
                    self.set_status(
                        StatusLevel::Info,
                        crate::localization::text("shell.commands.no_connection_to_disconnect"),
                    );
                }
            }
            "schema.refresh" => {
                match self.active_profile.clone() {
                    Some(profile) => {
                        self.refresh_completion_catalog(profile, cx);
                        self.set_status(
                            StatusLevel::Busy,
                            crate::localization::text("shell.commands.refreshing_catalog"),
                        );
                    }
                    None => {
                        self.set_status(
                            StatusLevel::Info,
                            crate::localization::text(
                                "shell.commands.connect_before_catalog_refresh",
                            ),
                        );
                    }
                }
                cx.notify();
            }
            "tools.commandPalette" => {
                self.open_command_palette(window, cx);
                cx.notify();
            }
            "tools.quickOpen" => {
                self.open_quick_open(window, cx);
                cx.notify();
            }
            // Menu-bar mirror commands: each dispatches the same handler the
            // menu action uses, so palette and menu can never diverge.
            "file.newWindow" => window.dispatch_action(Box::new(NewWindow), cx),
            "file.openSqlFile" => self.handle_open_sql_file(&OpenSqlFile, window, cx),
            "file.runSqlFile" => self.handle_run_sql_file(&RunSqlFile, window, cx),
            "tools.searchDatabaseValue" => {
                self.handle_search_database_value(&SearchDatabaseValue, window, cx)
            }
            "file.openProjectFile" => self.handle_open_project_file(&OpenProjectFile, window, cx),
            "file.openSqllyExport" => self.handle_open_sqlly_export(&OpenSqllyExport, window, cx),
            "file.exportConnections" => {
                self.handle_export_connections(&ExportConnections, window, cx)
            }
            "file.exportCredentials" => {
                self.handle_export_credentials(&ExportCredentials, window, cx)
            }
            "file.importConnections" => {
                self.handle_import_connections(&ImportConnections, window, cx)
            }
            "file.importConnectionsFromTools" => {
                self.handle_import_connections_from_tools(&ImportConnectionsFromTools, window, cx)
            }
            "file.exportForPgAdmin" => {
                self.handle_export_for_pg_admin(&ExportForPgAdmin, window, cx)
            }
            "file.exportForAzureDataStudio" => {
                self.handle_export_for_azure_data_studio(&ExportForAzureDataStudio, window, cx)
            }
            "file.exportForPgService" => {
                self.handle_export_for_pg_service(&ExportForPgService, window, cx)
            }
            "file.exportForDbeaver" => {
                self.handle_export_for_dbeaver(&ExportForDbeaver, window, cx)
            }
            "file.exportForDataGrip" => {
                self.handle_export_for_data_grip(&ExportForDataGrip, window, cx)
            }
            "file.exportForDbGate" => self.handle_export_for_db_gate(&ExportForDbGate, window, cx),
            "file.openFolder" => self.handle_open_folder(&OpenFolder, window, cx),
            "file.projectGit" => self.open_project_git(cx),
            "file.saveSql" => self.handle_save_sql(&SaveSql, window, cx),
            "file.saveToLibrary" => self.handle_save_to_library(&SaveToLibrary, window, cx),
            "file.saveResults" => self.handle_save_results(&SaveResultsCsv, window, cx),
            "connection.connect" => self.handle_connect(&Connect, window, cx),
            "view.toggleResultsPane" => {
                self.handle_toggle_results_pane(&ToggleResultsPane, window, cx)
            }
            "view.toggleResultsMaximized" => {
                self.handle_toggle_results_maximized(&ToggleResultsMaximized, window, cx)
            }
            "view.detachResults" => self.handle_detach_results(&DetachResults, window, cx),
            "view.reattachResults" => self.handle_reattach_results(&ReattachResults, window, cx),
            "view.toggleCellDetails" => {
                self.handle_toggle_cell_details(&ToggleCellDetails, window, cx)
            }
            "view.searchSchema" => self.handle_search_schema(&SearchSchema, window, cx),
            "view.sqlLibrary" => self.handle_show_sql_library(&ShowSqlLibrary, window, cx),
            "view.toggleIntellisenseDebug" => {
                self.handle_toggle_intellisense_debug(&ToggleIntelliSenseDebug, window, cx)
            }
            "view.zoomIn" => self.handle_zoom_editor_in(&ZoomEditorIn, window, cx),
            "view.zoomOut" => self.handle_zoom_editor_out(&ZoomEditorOut, window, cx),
            "view.zoomReset" => self.handle_zoom_editor_reset(&ZoomEditorReset, window, cx),
            "view.themeSystem" => self.handle_theme_system(&ThemeSystem, window, cx),
            "view.themeLight" => self.handle_theme_light(&ThemeLight, window, cx),
            "view.themeDark" => self.handle_theme_dark(&ThemeDark, window, cx),
            "view.themeHighContrast" => {
                self.handle_theme_high_contrast(&ThemeHighContrast, window, cx)
            }
            "query.generateSql" => self.handle_generate_sql(&GenerateSql, window, cx),
            "query.askSchema" => self.handle_ask_schema(&AskSchema, window, cx),
            "query.sendSelectionToAi" => {
                self.handle_send_selection_to_ai(&SendSelectionToAi, window, cx)
            }
            "query.explainError" => self.handle_explain_last_error(cx),
            "query.explainPlanAi" => self.handle_explain_plan_with_ai(cx),
            "ai.analyzeLastRun" => self.handle_analyze_last_run_with_ai(cx),
            #[cfg(not(target_arch = "wasm32"))]
            "ai.buildCatalog" => self.handle_build_ai_catalog(cx),
            #[cfg(not(target_arch = "wasm32"))]
            "ai.invalidateData" => self.handle_invalidate_ai_data(cx),
            #[cfg(target_arch = "wasm32")]
            "ai.buildCatalog" | "ai.invalidateData" => self.set_status(
                StatusLevel::Info,
                crate::browser_preview::unavailable(&crate::localization::text(
                    "shell.commands.ai_catalog_unavailable",
                )),
            ),
            "connection.objectProperties" => {
                self.handle_open_object_properties(&OpenObjectProperties, window, cx)
            }
            "connection.refreshExplorer" => {
                self.handle_refresh_object_explorer(&RefreshObjectExplorer, window, cx)
            }
            "connection.openInTerminal" => self.open_active_connection_in_terminal(cx),
            "tools.preferences" => self.handle_open_preferences(&OpenPreferences, window, cx),
            "tools.keybindings" => self.handle_open_keybindings(&OpenKeybindings, window, cx),
            "ai.mcpServerSettings" => {
                self.handle_open_mcp_server_settings(&OpenMcpServerSettings, window, cx)
            }
            "tools.connectionManagement" => {
                self.handle_show_connection_management(&ShowConnectionManagement, window, cx)
            }
            "connections.tunnelProfiles" => {
                #[cfg(not(target_arch = "wasm32"))]
                {
                    self.overlays.pending_tunnel_profiles = true;
                    cx.notify();
                }
            }
            "tools.externalEditor" => {
                self.handle_open_external_editor(&OpenExternalEditor, window, cx)
            }
            "tools.templateExplorer" => {
                self.handle_template_explorer(&TemplateExplorer, window, cx)
            }
            "tools.deployCart" => {
                #[cfg(not(target_arch = "wasm32"))]
                self.open_deploy_cart(cx);
                #[cfg(target_arch = "wasm32")]
                {
                    self.set_status(
                        StatusLevel::Info,
                        crate::browser_preview::unavailable(&crate::localization::text(
                            "shell.commands.deploy_cart_unavailable",
                        )),
                    );
                    cx.notify();
                }
            }
            "tools.backupRestore" => self.handle_backup_restore(&BackupRestore, window, cx),
            "tools.exportDatabaseSql" => {
                self.handle_export_database_sql(&ExportDatabaseSql, window, cx)
            }
            "tools.scheduleTask" => self.handle_add_scheduled_dump(&AddScheduledDump, window, cx),
            "tools.activityMonitor" => self.handle_activity_monitor(&ActivityMonitor, window, cx),
            "tools.redisProfiler" => {
                self.open_redis_live_tab(crate::activity_monitor::ActivityTab::Profiler, window, cx)
            }
            "tools.redisPubSub" => {
                self.open_redis_live_tab(crate::activity_monitor::ActivityTab::PubSub, window, cx)
            }
            "tools.serverDashboard" => self.handle_server_dashboard(&ServerDashboard, window, cx),
            "tools.sqlAgent" => self.handle_sql_agent(&SqlAgent, window, cx),
            "tools.security" => self.handle_security(&Security, window, cx),
            "tools.designers" => self.handle_designers(&Designers, window, cx),
            "tools.structure_editor" => self.handle_structure_editor_palette(window, cx),
            "tools.importExport" => self.handle_import_export(&ImportExport, window, cx),
            "tools.importFromClipboard" => {
                self.handle_import_from_clipboard(&ImportFromClipboard, window, cx)
            }
            "tools.schemaCompare" => self.handle_schema_compare(&SchemaCompare, window, cx),
            "tools.schemaHistory" => self.handle_schema_history(&SchemaHistory, window, cx),
            "tools.saveSchemaSnapshot" => {
                self.handle_save_schema_snapshot(&SaveSchemaSnapshot, window, cx)
            }
            "tools.dataCompare" => self.handle_data_compare(&DataCompare, window, cx),
            #[cfg(not(target_arch = "wasm32"))]
            "tools.dataTransfer" => self.handle_data_transfer(&DataTransfer, window, cx),
            "tools.queryStore" => self.handle_query_store(&QueryStore, window, cx),
            "tools.diskUsage" => self.handle_disk_usage(&DiskUsageTool, window, cx),
            "tools.redisMemoryAnalyzer" => {
                self.handle_redis_memory_analyzer(&RedisMemoryAnalyzerTool, window, cx)
            }
            #[cfg(not(target_arch = "wasm32"))]
            "tools.redisKeyView" => self.handle_redis_key_view_palette(cx),
            #[cfg(not(target_arch = "wasm32"))]
            "tools.redisClusterTopology" => self.open_redis_cluster_topology(cx),
            "tools.objectBrowser" => self.handle_object_browser(&ObjectBrowserTool, window, cx),
            "tools.indexAnalyzer" => self.handle_index_analyzer(&IndexAnalyzerTool, window, cx),
            "tools.testDataGenerator" => {
                self.handle_test_data_generator(&TestDataGeneratorTool, window, cx)
            }
            "tools.exportSchemaDocs" => {
                self.handle_export_schema_docs(&ExportSchemaDocs, window, cx)
            }
            "tools.tenantScoping" => self.handle_tenant_scoping(&TenantScoping, window, cx),
            "tools.extendedEvents" => self.handle_extended_events(&ExtendedEvents, window, cx),
            "tools.azureSqlConnection" => {
                self.handle_azure_sql_connection(&AzureSqlConnection, window, cx)
            }
            "tools.addFirewallRule" => self.handle_add_firewall_rule(&AddFirewallRule, window, cx),
            "window.nextEditorTab" => self.handle_next_editor_tab(&NextEditorTab, window, cx),
            "window.previousEditorTab" => {
                self.handle_previous_editor_tab(&PreviousEditorTab, window, cx)
            }
            "window.previousTabInHistory" => {
                self.handle_previous_tab_in_history(&PreviousTabInHistory, window, cx)
            }
            "window.nextTabInHistory" => {
                self.handle_next_tab_in_history(&NextTabInHistory, window, cx)
            }
            "help.sqllyHelp" => self.handle_sqlly_help(&SqllyHelp, window, cx),
            "help.formattingDirectives" => {
                self.handle_format_directives_reference(&FormatDirectivesReference, window, cx)
            }
            "help.sendFeedback" => self.open_send_feedback(window, cx),
            "help.tipOfTheDay" => self.handle_tip_of_the_day(&TipOfTheDay, window, cx),
            "help.checkForUpdates" => self.handle_check_for_updates(&CheckForUpdates, window, cx),
            "help.unlockSqlly" => self.handle_unlock_sqlly(&UnlockSqlly, window, cx),
            "help.appStoreEdition" => self.handle_app_store_edition(&AppStoreEdition, window, cx),
            id if id.starts_with("connection.quickSwitch") => {
                if let Some(n) = id
                    .strip_prefix("connection.quickSwitch")
                    .and_then(|rest| rest.parse::<usize>().ok())
                    && (1..=9).contains(&n)
                {
                    self.handle_quick_switch_connection(n, cx);
                }
            }
            id => {
                if let Some(slot) = crate::snippets::SnippetPreferences::slot_from_command_id(id) {
                    self.run_quick_action(slot, window, cx);
                } else {
                    self.set_status(
                        StatusLevel::Error,
                        crate::localization::format(
                            "shell.commands.unknown_command",
                            &[("command_id", cmd_id)],
                        ),
                    );
                    cx.notify();
                }
            }
        }
    }

    fn run_quick_action(&mut self, slot: u8, window: &mut Window, cx: &mut Context<Self>) {
        let Some(action) = self
            .snippet_prefs
            .quick_actions
            .iter()
            .find(|a| a.slot == slot)
            .cloned()
        else {
            self.set_status(
                StatusLevel::Info,
                crate::localization::format(
                    "shell.commands.quick_action_unconfigured",
                    &[("slot", &slot.to_string())],
                ),
            );
            cx.notify();
            return;
        };
        let Some(tab) = self.tabs.get(self.active_tab) else {
            return;
        };
        let selected = tab.editor.read(cx).selected_text();
        let word = tab.editor.read(cx).word_at_cursor();
        let database = self
            .active_profile
            .as_ref()
            .and_then(|p| p.database.clone());
        let ctx_qa = crate::snippets::QuickActionContext {
            selection: selected.as_deref(),
            word_at_cursor: &word,
            database: database.as_deref(),
        };
        let sql = crate::snippets::render_quick_action(&action.body, &ctx_qa);
        match action.target {
            crate::snippets::QuickActionTarget::ReplaceSelection => {
                if let Some(tab) = self.tabs.get(self.active_tab) {
                    tab.editor.update(cx, |ed, cx| ed.insert(&sql, cx));
                }
            }
            crate::snippets::QuickActionTarget::NewTab => {
                self.new_tab(cx, Some(sql));
            }
            crate::snippets::QuickActionTarget::RunInNewTab => {
                self.new_tab(cx, Some(sql));
                self.execute_query(self.active_tab, cx);
            }
        }
        let _ = window;
    }

    /// Every command id `execute_command` dispatches. Kept adjacent to the
    /// match above; the command-palette test asserts every registry entry is
    /// listed here, so registering a command without wiring dispatch fails CI.
    /// (Test-facing only — hence the dead_code allowance in the lib build.)
    #[allow(dead_code)]
    pub(crate) const PALETTE_DISPATCH_IDS: &'static [&'static str] = &[
        "file.newQuery",
        "file.newNotebook",
        "file.openNotebook",
        "file.saveNotebook",
        "file.saveWorkspaceAs",
        "file.openWorkspace",
        "file.manageWorkspaces",
        "file.closeTab",
        "tab.rename",
        "tab.moveToNewWindow",
        "file.reopenClosedTab",
        "tools.resultSnapshots",
        "tools.savedCharts",
        "tools.chartDashboard",
        "results.filterRows",
        "results.sortAscending",
        "results.sortDescending",
        "results.transpose",
        "results.stage_undo",
        "results.stage_redo",
        "results.save_staged",
        "results.discard_staged",
        "results.preview_staged_sql",
        "results.copy_staged_sql",
        "results.findInGrid",
        "results.gotoColumn",
        "results.focusToggle",
        "results.stageInsertRow",
        "results.stageDeleteRow",
        "results.refresh",
        "tools.connectionHealth",
        "tools.relationshipGraph",
        "tools.queryBuilder",
        "query.execute",
        "query.executeKeepResults",
        "query.runPicker",
        "query.runNotebookCell",
        "query.runAllNotebookCells",
        "query.executeNoRollback",
        "safety.unlockWrites1m",
        "safety.unlockWrites5m",
        "safety.unlockWrites15m",
        "safety.lockWrites",
        "transaction.toggleMode",
        "transaction.commit",
        "transaction.rollback",
        "safety.unlockReadOnly1m",
        "safety.unlockReadOnly5m",
        "safety.unlockReadOnly15m",
        "safety.lockReadOnly",
        "query.explainPlan",
        "query.explainEstimated",
        "plan.profile",
        "plan.open",
        "plan.paste",
        "query.history",
        "query.cancel",
        "query.runOnTargets",
        "results.autoRefreshOff",
        "results.autoRefresh5s",
        "results.autoRefresh10s",
        "results.autoRefresh30s",
        "results.autoRefresh60s",
        "query.validate",
        "query.formatSql",
        "editor.formatDocument",
        "editor.refreshDates",
        "editor.minify",
        "edit.find",
        "edit.replace",
        "editor.toggleVimMode",
        "view.splitVertical",
        "view.splitHorizontal",
        "view.splitSameBufferVertical",
        "view.splitSameBufferHorizontal",
        "view.closeSplit",
        "view.toggleHistoryPanel",
        "view.toggleSidebar",
        "view.toggleAiPanel",
        "view.toggleAiVisibility",
        "ai.toggleAgentMode",
        "view.toggleLogConsole",
        "connection.add",
        "connection.disconnect",
        "schema.refresh",
        "tools.commandPalette",
        "tools.quickOpen",
        "file.newWindow",
        "file.openSqlFile",
        "file.runSqlFile",
        "tools.searchDatabaseValue",
        "file.openProjectFile",
        "file.openSqllyExport",
        "file.exportConnections",
        "file.exportCredentials",
        "file.importConnections",
        "file.importConnectionsFromTools",
        "file.exportForPgAdmin",
        "file.exportForAzureDataStudio",
        "file.exportForPgService",
        "file.exportForDbeaver",
        "file.exportForDataGrip",
        "file.exportForDbGate",
        "file.openFolder",
        "file.projectGit",
        "file.saveSql",
        "file.saveToLibrary",
        "file.saveResults",
        "connection.connect",
        "connection.quickSwitch1",
        "connection.quickSwitch2",
        "connection.quickSwitch3",
        "connection.quickSwitch4",
        "connection.quickSwitch5",
        "connection.quickSwitch6",
        "connection.quickSwitch7",
        "connection.quickSwitch8",
        "connection.quickSwitch9",
        "view.toggleResultsPane",
        "view.toggleResultsMaximized",
        "view.detachResults",
        "view.reattachResults",
        "view.toggleCellDetails",
        "view.searchSchema",
        "view.sqlLibrary",
        "view.toggleIntellisenseDebug",
        "view.zoomIn",
        "view.zoomOut",
        "view.zoomReset",
        "view.themeSystem",
        "view.themeLight",
        "view.themeDark",
        "view.themeHighContrast",
        "query.generateSql",
        "query.askSchema",
        "query.sendSelectionToAi",
        "query.explainError",
        "query.explainPlanAi",
        "ai.analyzeLastRun",
        "ai.buildCatalog",
        "ai.invalidateData",
        "connection.objectProperties",
        "connection.openInTerminal",
        "connection.refreshExplorer",
        "tools.preferences",
        "tools.keybindings",
        "ai.mcpServerSettings",
        "tools.connectionManagement",
        "connections.tunnelProfiles",
        "tools.externalEditor",
        "tools.templateExplorer",
        "tools.deployCart",
        "tools.backupRestore",
        "tools.exportDatabaseSql",
        "tools.scheduleTask",
        "tools.activityMonitor",
        "tools.redisProfiler",
        "tools.redisPubSub",
        "tools.serverDashboard",
        "tools.sqlAgent",
        "tools.security",
        "tools.designers",
        "tools.structure_editor",
        "tools.importExport",
        "tools.importFromClipboard",
        "tools.schemaCompare",
        "tools.schemaHistory",
        "tools.saveSchemaSnapshot",
        "tools.dataCompare",
        "tools.dataTransfer",
        "tools.queryStore",
        "tools.diskUsage",
        "tools.redisMemoryAnalyzer",
        "tools.redisKeyView",
        "tools.redisClusterTopology",
        "tools.objectBrowser",
        "tools.indexAnalyzer",
        "tools.testDataGenerator",
        "tools.exportSchemaDocs",
        "tools.tenantScoping",
        "tools.extendedEvents",
        "tools.azureSqlConnection",
        "tools.addFirewallRule",
        "window.nextEditorTab",
        "window.previousEditorTab",
        "window.previousTabInHistory",
        "window.nextTabInHistory",
        "help.sqllyHelp",
        "help.formattingDirectives",
        "help.sendFeedback",
        "help.tipOfTheDay",
        "help.checkForUpdates",
        "help.unlockSqlly",
        "help.appStoreEdition",
        "quickAction.slot1",
        "quickAction.slot2",
        "quickAction.slot3",
        "quickAction.slot4",
        "quickAction.slot5",
        "quickAction.slot6",
        "quickAction.slot7",
        "quickAction.slot8",
        "quickAction.slot9",
    ];

    pub(super) fn handle_new_query_tab_action(
        &mut self,
        _action: &NewQueryTab,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.new_tab(cx, None);
        self.save_workspace_session(cx);
    }

    pub(super) fn handle_new_notebook(
        &mut self,
        _action: &NewNotebook,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.new_notebook_tab(crate::notebook_model::Notebook::new(), None, cx);
        self.save_workspace_session(cx);
    }

    pub(super) fn handle_open_notebook(
        &mut self,
        _action: &OpenNotebook,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_notebook(cx);
    }

    fn save_notebook_required_status(&mut self, _cx: &mut Context<Self>) {
        self.set_status(
            StatusLevel::Info,
            crate::localization::text("shell.commands.save_notebook_required"),
        );
    }

    pub(super) fn handle_save_notebook(
        &mut self,
        _action: &SaveNotebook,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some((idx, _)) = self.active_notebook() {
            self.save_notebook(idx, false, cx);
        } else {
            self.save_notebook_required_status(cx);
            cx.notify();
        }
    }

    fn handle_run_notebook_cell_inner(&mut self, window: &Window, cx: &mut Context<Self>) {
        if let Some((idx, nb)) = self.active_notebook() {
            if let Some(cell_id) = nb.read(cx).focused_cell(window, cx) {
                self.run_notebook_cell(idx, cell_id, cx);
            } else {
                self.set_status(
                    StatusLevel::Info,
                    crate::localization::text("shell.commands.focus_notebook_cell"),
                );
                cx.notify();
            }
        } else {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.commands.run_notebook_cell_requires_notebook"),
            );
            cx.notify();
        }
    }

    pub(super) fn handle_run_notebook_cell(
        &mut self,
        _action: &RunNotebookCell,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.handle_run_notebook_cell_inner(window, cx);
    }

    fn handle_run_all_notebook_cells_inner(&mut self, cx: &mut Context<Self>) {
        if let Some((_, nb)) = self.active_notebook() {
            nb.update(cx, |n, cx| n.run_all(cx));
        } else {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text(
                    "shell.commands.run_all_notebook_cells_requires_notebook",
                ),
            );
            cx.notify();
        }
    }

    pub(super) fn handle_run_all_notebook_cells(
        &mut self,
        _action: &RunAllNotebookCells,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.handle_run_all_notebook_cells_inner(cx);
    }

    pub(super) fn handle_quit(
        &mut self,
        _action: &Quit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.ai_panel.read(cx).is_run_in_flight() {
            let app_view = cx.weak_entity();
            window.open_alert_dialog(cx, move |dialog, _window, _cx| {
                let app_view = app_view.clone();
                dialog
                    .title(crate::localization::text("shell.commands.ai_quit_title"))
                    .description(crate::localization::text(
                        "shell.commands.ai_quit_description",
                    ))
                    .confirm()
                    .ok_text(crate::localization::text("shell.commands.ai_quit_confirm"))
                    .cancel_text(crate::localization::text("shell.commands.ai_quit_cancel"))
                    .on_ok(move |_ev, window, cx| {
                        let _ = app_view.update(cx, |this, cx| {
                            this.save_workspace_session(cx);
                        });
                        exit_app(window, cx);
                        true
                    })
            });
            return;
        }
        // Save the workspace session (including unsaved editor content) so
        // it survives app restart.
        self.save_workspace_session(cx);
        exit_app(window, cx);
    }

    pub(super) fn handle_clear_all_data(
        &mut self,
        _action: &ClearAllData,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let title = crate::localization::text("shell.commands.delete_all_title");
        let description = crate::localization::text("shell.commands.delete_all_description");
        let cancel = crate::localization::text("shell.commands.delete_all_cancel");
        let confirm = crate::localization::text("shell.commands.delete_all_confirm");
        let answer = window.prompt(
            PromptLevel::Critical,
            &title,
            Some(&description),
            &[PromptButton::cancel(&cancel), PromptButton::new(&confirm)],
            cx,
        );
        // Disarm any pending session-autosave timer: firing after the wipe
        // below would re-create session.json out of "delete everything".
        self.session_autosave_token = self.session_autosave_token.wrapping_add(1);
        let token_cache = self.entra_token_cache.clone();
        let token_refs = self
            .entra_accounts_registry
            .list()
            .into_iter()
            .map(|account| account.token_cache_ref)
            .collect::<Vec<_>>();
        let sql_account_ids = ConnectionStore::global()
            .sql_auth_accounts()
            .into_iter()
            .map(|account| account.id)
            .collect::<Vec<_>>();
        let weak = cx.weak_entity();
        let async_cx = cx.to_async();

        cx.spawn(async move |_this, _cx| {
            if answer.await != Ok(1) {
                return;
            }

            // Worker threads return raw error text; this UI-thread task words
            // it, since translation state is per thread.
            let credential_result = crate::tokio_handle()
                .spawn(async move {
                    token_cache
                        .delete_all(&token_refs)
                        .await
                        .map_err(|error| error.to_string())
                })
                .await
                .map_err(|error| {
                    crate::localization::format(
                        "shell.commands.cleanup_task_failed",
                        &[("error", &error.to_string())],
                    )
                })
                .and_then(|result| {
                    result.map_err(|error| {
                        crate::localization::format(
                            "shell.commands.clear_entra_credentials_failed",
                            &[("error", &error)],
                        )
                    })
                });

            let reset_result = crate::tokio_handle()
                .spawn_blocking(move || {
                    // `true` marks a SQL-credential failure, worded below.
                    if credential_result.is_ok()
                        && let Err(error) = crate::sql_credentials::delete_all(&sql_account_ids)
                    {
                        return Err((true, error.to_string()));
                    }
                    crate::app_data_reset::schedule_reset(credential_result)
                        .map_err(|error| (false, error))
                })
                .await
                .map_err(|error| {
                    crate::localization::format(
                        "shell.commands.cleanup_start_failed",
                        &[("error", &error.to_string())],
                    )
                })
                .and_then(|result| {
                    result.map_err(|(sql_credentials_failed, error)| {
                        let error = if sql_credentials_failed {
                            crate::localization::format(
                                "shell.commands.clear_sql_credentials_failed",
                                &[("error", &error)],
                            )
                        } else {
                            error
                        };
                        crate::localization::format(
                            "shell.commands.delete_all_failed",
                            &[("error", &error)],
                        )
                    })
                });

            let mut cx = async_cx.clone();
            let _ = weak.update(&mut cx, |this, cx| match reset_result {
                Ok(()) => cx.quit(),
                Err(error) => {
                    this.set_status(StatusLevel::Error, error);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(super) fn handle_close_tab_action(
        &mut self,
        _action: &CloseTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.request_close_tab(self.active_tab, window, cx);
    }

    /// Put the keyboard in the server explorer (⌘⇧B), revealing the sidebar
    /// first if it is collapsed.
    ///
    /// The explorer's arrow-key navigation needs a way in, and Tab is not it:
    /// focus starts in the SQL editor, which consumes Tab for indentation — as a
    /// SQL editor should — so a keyboard-only user could never leave. ⌘B shows
    /// the rail; ⌘⇧B puts you in it.
    pub(super) fn handle_focus_explorer(
        &mut self,
        _action: &FocusExplorer,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.sidebar_visible {
            self.sidebar_visible = true;
            self.save_workspace_session(cx);
        }
        self.sidebar.update(cx, |sidebar, cx| {
            sidebar.focus_tree(window, cx);
        });
        cx.notify();
    }

    pub(super) fn handle_toggle_sidebar(
        &mut self,
        _action: &ToggleSidebar,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.sidebar_visible {
            self.sidebar
                .update(cx, |sidebar, cx| sidebar.dismiss_floating_ui(cx));
            self.restore_workbench_focus(Some(window), cx);
            self.sidebar_visible = false;
        } else {
            self.sidebar_visible = true;
        }
        self.save_workspace_session(cx);
        cx.notify();
    }

    pub(super) fn handle_show_sql_library(
        &mut self,
        _action: &ShowSqlLibrary,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sidebar_visible = true;
        self.sidebar
            .update(cx, |sidebar, cx| sidebar.show_library(cx));
        self.save_workspace_session(cx);
        cx.notify();
    }

    pub(super) fn handle_open_external_editor(
        &mut self,
        _action: &OpenExternalEditor,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.get(self.active_tab) else {
            return;
        };
        let editor = tab.editor.clone();
        let text = editor.read(cx).text().to_string();
        let file_label = format!("{}.sql", tab.name);
        let prefs_path = crate::external_editor::external_editor_prefs_file();
        let prefs = crate::external_editor::ExternalEditorPreferencesStore::load(&prefs_path);

        // The error crosses back unworded: `LaunchError`'s Display reads the
        // UI thread's language.
        let (tx, rx) =
            tokio::sync::oneshot::channel::<Result<String, crate::external_editor::LaunchError>>();
        cx.background_spawn(async move {
            crate::tokio_handle()
                .spawn(async move {
                    let result = tokio::task::spawn_blocking(move || {
                        crate::external_editor::open(&text, &file_label, &prefs)
                            .and_then(|path| {
                                std::fs::read_to_string(&path).map_err(|e| {
                                    crate::external_editor::LaunchError::WriteFailed {
                                        reason: e.to_string(),
                                    }
                                })
                            })
                            .map(|content| content.trim().to_string())
                    })
                    .await
                    .map_err(|e| crate::external_editor::LaunchError::LaunchFailed {
                        reason: e.to_string(),
                    })
                    .and_then(|inner| inner);
                    let _ = tx.send(result);
                })
                .await
                .ok();
        })
        .detach();

        let async_cx = cx.to_async();
        let weak_self = cx.weak_entity();
        cx.spawn(async move |_this, _cx| {
            if let Ok(result) = rx.await {
                let mut cx = async_cx.clone();
                let _ = weak_self.update(&mut cx, |this, cx| match result {
                    Ok(text) => {
                        let proc_dialect = this.active_proc_dialect();
                        if let Some(tab) = this.tabs.get_mut(this.active_tab) {
                            tab.proc_mode.refresh_from_script_for(&text, proc_dialect);
                            tab.editor.update(cx, |e, cx| {
                                e.set_text(text, cx);
                            });
                        }
                        this.set_status(
                            StatusLevel::Success,
                            crate::localization::text("shell.commands.external_editor_updated"),
                        );
                        cx.notify();
                    }
                    Err(e) => {
                        this.set_status(
                            StatusLevel::Error,
                            crate::localization::format(
                                "shell.commands.external_editor_failed",
                                &[("error", &e.to_string())],
                            ),
                        );
                        cx.notify();
                    }
                });
            }
        })
        .detach();
    }

    pub(super) fn handle_toggle_log_console(
        &mut self,
        _action: &ToggleLogConsole,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.log_console.update(cx, |view, cx| {
            view.toggle_visibility(cx);
        });
        cx.notify();
    }

    pub(super) fn handle_execute_active_query(
        &mut self,
        _action: &ExecuteActiveQuery,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some((idx, nb)) = self.active_notebook()
            && let Some(cell_id) = nb.read(cx).focused_cell(window, cx)
        {
            self.run_notebook_cell(idx, cell_id, cx);
            return;
        }
        self.execute_query(self.active_tab, cx);
    }

    pub(super) fn handle_execute_active_query_keep_results(
        &mut self,
        _action: &ExecuteActiveQueryKeepResults,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.execute_query_keep_results(self.active_tab, cx);
    }

    pub(super) fn handle_find_in_results(
        &mut self,
        _action: &FindInResults,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(tab) = self.tabs.get(self.active_tab) {
            tab.results.update(cx, |v, cx| v.toggle_grid_find(cx));
        }
    }

    pub(super) fn handle_toggle_editor_results_focus(
        &mut self,
        _action: &ToggleEditorResultsFocus,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.get(self.active_tab) else {
            return;
        };
        let editor = tab.editor.clone();
        let results = tab.results.clone();
        let editor_handle = editor.read(cx).focus_handle(cx);
        // "In the editor" covers the same-buffer split's second view and any
        // focusable child (find bar, inline widgets), not only the exact
        // primary handle, so F6 from any of them goes to the grid.
        let in_editor = editor_handle.contains_focused(window, cx)
            || self
                .same_buffer_view
                .as_ref()
                .is_some_and(|view| view.read(cx).focus_handle(cx).contains_focused(window, cx));
        if in_editor {
            // Editor → grid (no-op until a run has produced one). A hidden
            // results pane is shown first, so focus never lands on a grid
            // the user cannot see.
            if results.read(cx).active_grid_state(cx).is_none() {
                return;
            }
            if !self.results_visible() {
                self.show_results();
                cx.notify();
            }
            results.update(cx, |v, cx| v.focus_active_grid(window, cx));
        } else {
            // Grid (or anywhere else) → editor.
            editor.update(cx, |e, cx| e.request_focus(cx));
            window.focus(&editor_handle, cx);
        }
    }

    pub(super) fn handle_goto_column_in_results(
        &mut self,
        _action: &GotoColumnInResults,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(tab) = self.tabs.get(self.active_tab) {
            tab.results.update(cx, |v, cx| v.open_goto_column(cx));
        }
    }

    pub(super) fn handle_stage_insert_row_in_results(
        &mut self,
        _action: &StageInsertRowInResults,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(tab) = self.tabs.get(self.active_tab) {
            tab.results.update(cx, |v, cx| v.stage_insert_from_grid(cx));
        }
    }

    pub(super) fn handle_stage_delete_row_in_results(
        &mut self,
        _action: &StageDeleteRowInResults,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(tab) = self.tabs.get(self.active_tab) {
            tab.results.update(cx, |v, cx| v.stage_delete_from_grid(cx));
        }
    }

    pub(super) fn handle_stage_undo_in_results(
        &mut self,
        _action: &StageUndoInResults,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(tab) = self.tabs.get(self.active_tab) {
            tab.results.update(cx, |v, cx| v.stage_undo(cx));
        }
    }

    pub(super) fn handle_stage_redo_in_results(
        &mut self,
        _action: &StageRedoInResults,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(tab) = self.tabs.get(self.active_tab) {
            tab.results.update(cx, |v, cx| v.stage_redo(cx));
        }
    }

    pub(super) fn handle_refresh_results(
        &mut self,
        _action: &RefreshResults,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.execute_query(self.active_tab, cx);
    }

    pub(super) fn handle_run_picker(
        &mut self,
        _action: &RunPicker,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.get(self.active_tab) else {
            return;
        };
        let editor = tab.editor.read(cx);
        let text = editor.text();
        if text.trim().is_empty() {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.commands.no_statements"),
            );
            cx.notify();
            return;
        }
        let dialect = editor.database_type().family();
        let model =
            crate::run_picker::RunPickerModel::from_sql(text, dialect, Some(editor.cursor()));
        if model.rows.is_empty() {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.commands.no_statements"),
            );
            cx.notify();
            return;
        }
        self.overlays.run_picker = Some(crate::app::overlays::RunPickerOverlay {
            tab_idx: self.active_tab,
            model,
        });
        cx.notify();
    }

    pub(super) fn handle_cancel_active_query(
        &mut self,
        _action: &CancelActiveQuery,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cancel_query_no_window(cx);
    }

    pub(super) fn handle_commit_transaction(
        &mut self,
        _action: &CommitTransaction,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.commit_transaction(self.active_tab, cx);
    }

    pub(super) fn handle_rollback_transaction(
        &mut self,
        _action: &RollbackTransaction,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.rollback_transaction(self.active_tab, cx);
    }

    pub(super) fn handle_toggle_transaction_mode(
        &mut self,
        _action: &ToggleTransactionMode,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_transaction_mode(self.active_tab, cx);
    }

    pub(super) fn handle_run_on_targets(
        &mut self,
        _action: &RunOnTargets,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The dialog picks its own targets from the saved connections, so no
        // active connection is needed — only something to pick.
        let profiles = ConnectionStore::global().list();
        if profiles.is_empty() {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.commands.multi_target_requires_connection"),
            );
            cx.notify();
            return;
        }
        if self
            .tabs
            .get(self.active_tab)
            .is_some_and(|tab| tab.proc_mode.enabled)
        {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.commands.multi_target_proc_mode"),
            );
            cx.notify();
            return;
        }
        let preselected = self
            .tabs
            .get(self.active_tab)
            .map(|tab| {
                if !tab.last_multi_target_ids.is_empty() {
                    tab.last_multi_target_ids.clone()
                } else {
                    tab.connection_target
                        .as_ref()
                        .map(|t| t.profile_id.to_string())
                        .into_iter()
                        .collect()
                }
            })
            .unwrap_or_default();
        let dialog = cx.new(|cx| {
            crate::multi_target_dialog::MultiTargetDialog::new(&profiles, &preselected, cx)
        });
        cx.subscribe_in(&dialog, window, |this, _, event, window, cx| {
            this.handle_multi_target_dialog_event(event, window, cx);
        })
        .detach();
        self.overlays.multi_target_dialog = Some(dialog);
        cx.notify();
    }

    fn handle_multi_target_dialog_event(
        &mut self,
        event: &crate::multi_target_dialog::MultiTargetDialogEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            crate::multi_target_dialog::MultiTargetDialogEvent::Run { target_ids } => {
                self.overlays.multi_target_dialog = None;
                self.restore_editor_focus(cx);
                let tab_idx = self.active_tab;
                let ids = target_ids.clone();
                self.execute_query_on_targets(
                    tab_idx,
                    ids,
                    super::execute::ExecuteOrigin::UserRun,
                    cx,
                );
            }
            crate::multi_target_dialog::MultiTargetDialogEvent::Cancel => {
                self.overlays.multi_target_dialog = None;
                self.restore_editor_focus(cx);
                cx.notify();
            }
        }
        let _ = window;
    }

    pub(super) fn handle_validate_active_query(
        &mut self,
        _action: &ValidateActiveQuery,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.validate_sql(self.active_tab, cx);
    }

    pub(super) fn handle_format_active_query(
        &mut self,
        _action: &FormatActiveQuery,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(tab) = self.tabs.get(self.active_tab) {
            let editor = tab.editor.clone();
            editor.update(cx, |ed, cx| ed.format_sql_buffer(cx));
        }
    }

    pub(super) fn handle_minify_active_query(
        &mut self,
        _action: &MinifyActiveQuery,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(tab) = self.tabs.get(self.active_tab) {
            let editor = tab.editor.clone();
            editor.update(cx, |ed, cx| ed.minify_sql_buffer(cx));
        }
    }

    pub(super) fn handle_open_preferences(
        &mut self,
        _action: &OpenPreferences,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.overlays.preferences_window.is_some() {
            return;
        }
        let store = crate::preferences::PreferencesStore::new();
        let prefs_window =
            cx.new(|cx| crate::preferences_ui::PreferencesWindow::new(store, window, cx));
        cx.subscribe(&prefs_window, Self::handle_preferences_event)
            .detach();
        self.overlays.preferences_window = Some(prefs_window);
        cx.notify();
    }

    pub(super) fn handle_preferences_event(
        &mut self,
        entity: Entity<crate::preferences_ui::PreferencesWindow>,
        event: &crate::preferences_ui::PreferencesWindowEvent,
        cx: &mut Context<Self>,
    ) {
        if matches!(
            event,
            crate::preferences_ui::PreferencesWindowEvent::LanguageChanged
        ) {
            self.appearance_prefs = crate::preferences::AppearancePreferences::load();
            cx.defer(crate::localization::refresh);
            return;
        }
        // Theme changes apply live while the dialog stays open: the global
        // theme state is already updated by the prefs pane, so just re-theme
        // the grids and repaint the chrome.
        if matches!(
            event,
            crate::preferences_ui::PreferencesWindowEvent::ThemeChanged
        ) {
            self.appearance_prefs = crate::preferences::AppearancePreferences::load();
            self.refresh_all_grid_themes(cx);
            cx.notify();
            return;
        }
        if matches!(
            event,
            crate::preferences_ui::PreferencesWindowEvent::ResultsFontChanged
        ) {
            self.appearance_prefs = crate::preferences::AppearancePreferences::load();
            for tab in &self.tabs {
                tab.results.update(cx, |rv, cx| {
                    rv.apply_density_to_live_grids(cx);
                    cx.notify();
                });
            }
            return;
        }
        // A cache clear from the IntelliSense Cache pane applies live while the
        // dialog stays open: drop any in-memory snapshot whose file is now gone
        // so the next visit to that database refreshes from the server.
        if matches!(
            event,
            crate::preferences_ui::PreferencesWindowEvent::IntelliSenseCacheChanged
        ) {
            self.reconcile_datamodel_cache_with_disk();
            return;
        }
        if matches!(
            event,
            crate::preferences_ui::PreferencesWindowEvent::SnippetsChanged
        ) {
            self.apply_snippet_preferences(cx);
            cx.notify();
            return;
        }
        #[cfg(not(target_arch = "wasm32"))]
        if matches!(
            event,
            crate::preferences_ui::PreferencesWindowEvent::OpenMcpActivity
        ) {
            self.overlays.pending_open_mcp_activity = true;
            cx.notify();
            return;
        }
        // Only the currently-tracked dialog may close the slot. Events are
        // delivered asynchronously, so a `Closed` queued by a previous
        // instance could otherwise arrive after a reopen and drop the new
        // dialog — and leave focus pointing at the dead entity.
        if self.overlays.preferences_window.as_ref() != Some(&entity) {
            return;
        }
        self.restore_editor_focus(cx);
        self.overlays.preferences_window = None;
        self.formatting = crate::preferences::ResultFormattingPreferences::load();
        self.show_detailed_query_timing =
            crate::preferences::PerformancePreferences::load().show_detailed_query_timing;
        // Autocomplete settings are read by `schedule_fim_completion` off this
        // cached copy, so without a reload here the pane would save the user's
        // choice and the editor would keep using the values from launch.
        self.fim_prefs = crate::fim_completion::FimPreferences::load();
        self.editor_prefs = crate::preferences::EditorPreferences::load();
        self.editor_prefs.apply_match_options();
        self.query_history_prefs = crate::preferences::QueryHistoryPreferences::load();
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.result_cache_prefs = crate::preferences::ResultCachePreferences::load();
        }
        self.transaction_prefs = crate::preferences::TransactionPreferences::load();
        self.appearance_prefs = crate::preferences::AppearancePreferences::load();
        // Query-summary settings (Preferences ▸ AI ▸ On device) apply on
        // dialog close, like the other cached preference copies above.
        {
            let ai_prefs = crate::preferences::AiSummaryPreferences::load();
            self.ai_summary_enabled = ai_prefs.enabled;
            self.ai_summary_model = match ai_prefs.model_override() {
                Some(model) => model.to_string(),
                None => crate::fim_completion::FimAiModel::Qwen25Coder15BBase
                    .ollama_model_name()
                    .to_string(),
            };
        }
        self.ddl_nav_prefs = crate::preferences::DdlNavigationPreferences::load();
        self.apply_snippet_preferences(cx);
        // Data Model (server explorer organization) settings may have changed
        // — push them into the schema tree so it re-shapes on next expand.
        self.sidebar
            .update(cx, |sidebar, cx| sidebar.reload_data_model_prefs(cx));
        // Reload query resume preferences and recompute effective state for
        // all tabs. Mirrors Swift `applyQueryResumePreferences()`.
        let new_prefs = crate::preferences::QueryResumePreferences::load();
        if new_prefs.scope != self.query_resume_prefs.scope {
            self.query_resume_prefs = new_prefs;
            self.apply_resume_prefs_to_all_tabs(cx);
        }
        crate::theme::Theme::set_font_prefs(
            &self.appearance_prefs.interface_font,
            self.appearance_prefs.interface_font_size,
            &self.appearance_prefs.editor_font,
            self.appearance_prefs.editor_font_size,
        );
        crate::theme::Theme::set_results_font_prefs(
            &self.appearance_prefs.results_font,
            self.appearance_prefs.results_font_size,
        );
        // Apply the (possibly changed) diagnostic log level to the live log
        // so logging turns on/off immediately without an app restart.
        let diag = crate::preferences::DiagnosticLoggingPreferences::load();
        self.log.set_level(diag.level.into());
        for tab in &self.tabs {
            tab.results.update(cx, |rv, cx| {
                rv.set_formatting(self.formatting.clone(), cx);
                rv.set_show_detailed_timing(self.show_detailed_query_timing, cx);
                rv.reload_performance_prefs(cx);
            });
            tab.editor.update(cx, |ed, cx| {
                ed.set_editor_prefs(self.editor_prefs.clone(), cx);
                // Push the DDL gesture config too, so rebinding the modifier
                // takes effect in already-open editors immediately.
                ed.set_ddl_nav_prefs(self.ddl_nav_prefs.clone(), cx);
                ed.set_font(
                    &self.appearance_prefs.editor_font,
                    crate::editor_zoom::zoomed_font_size(
                        self.appearance_prefs.editor_font_size,
                        self.editor_zoom,
                    ),
                    cx,
                );
            });
        }
        if self.cell_detail_visible {
            self.apply_cell_detail_layout(cx);
        }
        self.set_status(
            StatusLevel::Success,
            crate::localization::text("shell.commands.preferences_saved"),
        );
        cx.notify();
    }

    /// The active tab's editor view that currently holds keyboard focus:
    /// the canonical editor, or the same-buffer split's second view. `None`
    /// when neither is focused (focus is in a dialog, the sidebar, …).
    pub(super) fn keyboard_focused_editor(
        &self,
        window: &Window,
        cx: &Context<Self>,
    ) -> Option<Entity<SqlEditor>> {
        if let Some(tab) = self.tabs.get(self.active_tab)
            && tab.editor.read(cx).focus_handle(cx).is_focused(window)
        {
            return Some(tab.editor.clone());
        }
        if let Some(view) = &self.same_buffer_view
            && view.read(cx).focus_handle(cx).is_focused(window)
        {
            return Some(view.clone());
        }
        None
    }

    pub(super) fn handle_undo(&mut self, _: &Undo, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(editor) = self.keyboard_focused_editor(window, cx) {
            editor.update(cx, |e, cx| e.undo(cx));
        }
    }

    pub(super) fn handle_redo(&mut self, _: &Redo, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(editor) = self.keyboard_focused_editor(window, cx) {
            editor.update(cx, |e, cx| e.redo(cx));
        }
    }

    /// Kept so a focused window can open a sibling directly; the app-level
    /// `NewWindow` handler in `register_global_actions` is what makes the menu
    /// item work when no window is left.
    #[allow(dead_code)]
    pub(super) fn handle_new_window(
        &mut self,
        _action: &NewWindow,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let services = self.services.clone();
        cx.defer(move |cx| {
            let _ = AppView::open_window(services, cx);
        });
    }

    pub(super) fn handle_toggle_results_pane(
        &mut self,
        _action: &ToggleResultsPane,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.results_pane = self.results_pane.toggled();
        cx.notify();
    }

    pub(super) fn handle_toggle_results_maximized(
        &mut self,
        _action: &ToggleResultsMaximized,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.results_pane = self.results_pane.toggled_maximized();
        self.save_workspace_session(cx);
        cx.notify();
    }

    pub(super) fn handle_detach_results(
        &mut self,
        _action: &DetachResults,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.detach_results(cx);
    }

    pub(super) fn handle_reattach_results(
        &mut self,
        _action: &ReattachResults,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.reattach_results(cx);
    }

    fn detached_tab_key(tab: &crate::tabs::QueryTab) -> String {
        tab.file_path.clone().unwrap_or_else(|| tab.name.clone())
    }

    pub(super) fn detach_results(&mut self, cx: &mut Context<Self>) {
        if cfg!(target_arch = "wasm32") {
            // The web platform is single-window; a second gpui window cannot open.
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.commands.detach_browser_unavailable"),
            );
            return;
        }
        let Some(tab) = self.tabs.get(self.active_tab) else {
            return;
        };
        if tab.is_notebook() || tab.ddl_view.is_some() {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.commands.detach_query_tabs_only"),
            );
            return;
        }
        if tab.results_detached || tab.detached_results.is_some() {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.commands.already_detached"),
            );
            return;
        }
        let tab_id = tab.id.clone();
        let tab_name = tab.name.clone();
        let tab_key = Self::detached_tab_key(tab);
        let results = tab.results.clone();
        let prefs = self.detached_results_prefs.clone();
        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
            tab.results_detached = true;
        }
        cx.notify();
        let weak = cx.weak_entity();
        cx.defer(move |cx: &mut gpui::App| {
            if let Some(handle) = cx.active_window() {
                let weak = weak.clone();
                let tab_id = tab_id.clone();
                let tab_name = tab_name.clone();
                let tab_key = tab_key.clone();
                let results = results.clone();
                let prefs = prefs.clone();
                let _ = handle.update(cx, |_, window, _cx| {
                    window.on_next_frame(move |_, cx| {
                        AppView::open_detached_results_window(
                            cx, weak, tab_id, tab_name, tab_key, results, prefs,
                        );
                    });
                });
            } else {
                AppView::open_detached_results_window(
                    cx, weak, tab_id, tab_name, tab_key, results, prefs,
                );
            }
        });
    }

    fn open_detached_results_window(
        cx: &mut gpui::App,
        weak: gpui::WeakEntity<AppView>,
        tab_id: String,
        tab_name: String,
        tab_key: String,
        results: Entity<ResultsView>,
        prefs: crate::preferences::DetachedResultsPreferences,
    ) {
        let current = cx.active_window().and_then(|handle| {
            handle
                .update(cx, |_, window, cx| {
                    crate::ddl_navigation::current_window_monitor(window, cx)
                })
                .ok()
                .flatten()
        });
        let bounds = crate::detached_results::initial_bounds(
            &prefs,
            &tab_key,
            current.as_ref().map(|m| m.bounds),
        );
        let display_id = current.as_ref().and_then(|m| m.id);
        let window_title = gpui::SharedString::from(crate::localization::format(
            "shell.commands.detached_window_title",
            &[("tab", &tab_name)],
        ));
        let close_weak = weak.clone();
        let close_tab_id = tab_id.clone();
        let result = cx.open_window(
            gpui::WindowOptions {
                window_bounds: Some(gpui::WindowBounds::Windowed(bounds)),
                display_id,
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some(window_title),
                    ..Default::default()
                }),
                app_id: Some(crate::about::APP_ID.into()),
                is_resizable: true,
                ..Default::default()
            },
            move |window, cx| {
                window.on_window_should_close(cx, {
                    let weak = close_weak.clone();
                    let tab_id = close_tab_id.clone();
                    move |window, cx| {
                        let b = window.bounds();
                        weak.update(cx, |app, cx| {
                            app.on_detached_results_closing(&tab_id, b, cx)
                        })
                        .ok();
                        true
                    }
                });
                cx.new(|cx| gpui_component::Root::new(results, window, cx))
            },
        );
        match result {
            Ok(handle) => {
                let _ = weak.update(cx, |app, cx| {
                    if let Some(tab) = app
                        .tab_index_by_id(&tab_id)
                        .and_then(|i| app.tabs.get_mut(i))
                    {
                        tab.detached_results = Some(handle);
                        tab.results_detached = true;
                    }
                    app.set_status(
                        StatusLevel::Info,
                        crate::localization::text("shell.commands.detached_status"),
                    );
                    cx.notify();
                });
            }
            Err(_) => {
                let _ = weak.update(cx, |app, cx| {
                    if let Some(tab) = app
                        .tab_index_by_id(&tab_id)
                        .and_then(|i| app.tabs.get_mut(i))
                    {
                        tab.results_detached = false;
                        tab.detached_results = None;
                    }
                    app.set_status(
                        StatusLevel::Error,
                        crate::localization::text("shell.commands.results_window_failed"),
                    );
                    cx.notify();
                });
            }
        }
    }

    pub(super) fn on_detached_results_closing(
        &mut self,
        tab_id: &str,
        bounds: gpui::Bounds<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) {
        if let Some(idx) = self.tab_index_by_id(tab_id) {
            let key = Self::detached_tab_key(&self.tabs[idx]);
            self.detached_results_prefs
                .remember(&key, crate::detached_results::bounds_tuple(&bounds));
            self.detached_results_prefs.save();
            if let Some(tab) = self.tabs.get_mut(idx) {
                tab.detached_results = None;
                tab.results_detached = false;
            }
        }
        cx.notify();
    }

    pub(super) fn reattach_results(&mut self, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(self.active_tab) else {
            return;
        };
        if !tab.results_detached && tab.detached_results.is_none() {
            return;
        }
        let tab_key = Self::detached_tab_key(tab);
        let handle = self.tabs[self.active_tab].detached_results.take();
        self.tabs[self.active_tab].results_detached = false;
        if let Some(handle) = handle {
            if let Ok(bounds) = handle.update(cx, |_, window, _| window.bounds()) {
                self.detached_results_prefs
                    .remember(&tab_key, crate::detached_results::bounds_tuple(&bounds));
                self.detached_results_prefs.save();
            }
            let _ = handle.update(cx, |_, window, _| window.remove_window());
        }
        cx.notify();
    }

    pub(super) fn teardown_detached_results_for_tab(
        &mut self,
        index: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        if !tab.results_detached && tab.detached_results.is_none() {
            return;
        }
        let tab_key = Self::detached_tab_key(tab);
        let handle = self.tabs[index].detached_results.take();
        self.tabs[index].results_detached = false;
        if let Some(handle) = handle {
            if let Ok(bounds) = handle.update(cx, |_, window, _| window.bounds()) {
                self.detached_results_prefs
                    .remember(&tab_key, crate::detached_results::bounds_tuple(&bounds));
                self.detached_results_prefs.save();
            }
            let _ = handle.update(cx, |_, window, _| window.remove_window());
        }
    }

    pub(super) fn cell_detail_size_for_side(&self) -> f32 {
        match self.formatting.cell_detail_side {
            crate::preferences::CellDetailSide::Right => self.cell_detail_width,
            crate::preferences::CellDetailSide::Bottom => self.cell_detail_height,
        }
    }

    pub(super) fn attach_cell_detail_panel(
        &self,
        results: &Entity<ResultsView>,
        cx: &mut Context<Self>,
    ) {
        if !self.cell_detail_visible {
            return;
        }
        let side = self.formatting.cell_detail_side.as_dock_side();
        let size = self.cell_detail_size_for_side();
        let panel = cx.new(crate::cell_detail_panel::CellDetailPanel::new);
        results.update(cx, |rv, cx| rv.set_cell_detail(Some(panel), side, size, cx));
    }

    pub(super) fn push_cell_detail_to_tabs(&mut self, cx: &mut Context<Self>) {
        let side = self.formatting.cell_detail_side.as_dock_side();
        let size = self.cell_detail_size_for_side();
        let tabs: Vec<_> = self.tabs.iter().map(|t| t.results.clone()).collect();
        if self.cell_detail_visible {
            for results in tabs {
                let panel = cx.new(crate::cell_detail_panel::CellDetailPanel::new);
                results.update(cx, |rv, cx| {
                    rv.set_cell_detail(Some(panel), side, size, cx);
                });
            }
        } else {
            for results in tabs {
                results.update(cx, |rv, cx| {
                    rv.set_cell_detail(None, side, size, cx);
                });
            }
        }
    }

    pub(super) fn apply_cell_detail_layout(&mut self, cx: &mut Context<Self>) {
        let side = self.formatting.cell_detail_side.as_dock_side();
        let size = self.cell_detail_size_for_side();
        let tabs: Vec<_> = self.tabs.iter().map(|t| t.results.clone()).collect();
        for results in tabs {
            results.update(cx, |rv, cx| rv.set_cell_detail_side(side, size, cx));
        }
    }

    pub(super) fn handle_toggle_cell_details(
        &mut self,
        _action: &ToggleCellDetails,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cell_detail_visible = !self.cell_detail_visible;
        self.push_cell_detail_to_tabs(cx);
        self.save_workspace_session(cx);
        cx.notify();
    }

    pub(super) fn handle_select_output_results(
        &mut self,
        _action: &SelectOutputResults,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_output_shortcut(OutputPanelTab::Results, cx);
    }

    pub(super) fn handle_select_output_messages(
        &mut self,
        _action: &SelectOutputMessages,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_output_shortcut(OutputPanelTab::Messages, cx);
    }

    pub(super) fn handle_select_output_plan(
        &mut self,
        _action: &SelectOutputPlan,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_output_shortcut(OutputPanelTab::Plan, cx);
    }

    pub(super) fn handle_select_output_history(
        &mut self,
        _action: &SelectOutputHistory,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_output_shortcut(OutputPanelTab::History, cx);
    }

    pub(super) fn handle_select_output_ddl(
        &mut self,
        _action: &SelectOutputDdl,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_output_shortcut(OutputPanelTab::Ddl, cx);
    }

    pub(super) fn handle_select_output_diff(
        &mut self,
        _action: &SelectOutputDiff,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_output_shortcut(OutputPanelTab::Diff, cx);
    }

    pub(super) fn handle_reopen_closed_tab(
        &mut self,
        _action: &ReopenClosedTab,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.reopen_closed_tab(cx);
    }

    pub(super) fn unlock_production_writes(
        &mut self,
        window: sqlly_intel::query::safety::UnlockWindow,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self.active_profile.as_ref() else {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.commands.production_lock_no_active_connection"),
            );
            cx.notify();
            return;
        };
        let store = ConnectionStore::global();
        let safety = crate::mutation_review::SafetyFeatures::effective_for_profile(profile, store);
        if !safety.is_production {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.commands.not_production"),
            );
            cx.notify();
            return;
        }
        let now = sqlly_intel::query::safety::Instant::now();
        self.production_unlock = Some(sqlly_intel::query::safety::WriteUnlock::start(window, now));
        self.set_status(
            StatusLevel::Info,
            crate::localization::text(match window {
                sqlly_intel::query::safety::UnlockWindow::OneMinute => {
                    "shell.commands.unlocked_one_minute"
                }
                sqlly_intel::query::safety::UnlockWindow::FiveMinutes => {
                    "shell.commands.unlocked_five_minutes"
                }
                sqlly_intel::query::safety::UnlockWindow::FifteenMinutes => {
                    "shell.commands.unlocked_fifteen_minutes"
                }
            }),
        );
        self.schedule_production_unlock_tick(cx);
        cx.notify();
    }

    pub(super) fn lock_production_writes(&mut self, cx: &mut Context<Self>) {
        self.production_unlock = None;
        self.production_unlock_tick = None;
        self.set_status(
            StatusLevel::Info,
            crate::localization::text("shell.commands.production_locked"),
        );
        cx.notify();
    }

    pub(super) fn schedule_production_unlock_tick(&mut self, cx: &mut Context<Self>) {
        if self.production_unlock.is_none() {
            self.production_unlock_tick = None;
            return;
        }
        let this = cx.entity();
        self.production_unlock_tick = Some(cx.spawn(async move |_this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_secs(1))
                .await;
            this.update(cx, |this, cx| {
                this.production_unlock_tick = None;
                let now = sqlly_intel::query::safety::Instant::now();
                if this
                    .production_unlock
                    .as_ref()
                    .is_some_and(|unlock| !unlock.is_active(now))
                {
                    this.production_unlock = None;
                    this.set_status(
                        StatusLevel::Info,
                        crate::localization::text("shell.commands.production_lock_expired"),
                    );
                } else if this.production_unlock.is_some() {
                    this.schedule_production_unlock_tick(cx);
                }
                cx.notify();
            });
        }));
    }
}

/// Leave the app: quit on desktop; in the browser, where gpui cannot quit,
/// ask the browser to close the tab instead of blanking the page.
fn exit_app(window: &mut Window, cx: &mut App) {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = window;
        cx.quit();
    }
    #[cfg(target_arch = "wasm32")]
    crate::browser_tab::close_tab(window, cx);
}

#[cfg(test)]
#[cfg(test)]
mod menu_gating_tests {
    use super::AppView;
    use gpui::Action;
    use sqlly_lang::sqlmodel::DatabaseType;

    #[test]
    fn disconnected_disables_nothing() {
        assert!(AppView::engine_gated_menu_actions(None).is_empty());
    }

    #[test]
    fn sql_server_keeps_all_tools_enabled() {
        assert!(AppView::engine_gated_menu_actions(Some(DatabaseType::SqlServer)).is_empty());
    }

    #[test]
    fn sqlite_grays_engine_bound_tools_with_reasons() {
        let disabled = AppView::engine_gated_menu_actions(Some(DatabaseType::Sqlite));
        for action in [
            super::BackupRestore::name_for_type(),
            super::ActivityMonitor::name_for_type(),
            super::SqlAgent::name_for_type(),
            super::Security::name_for_type(),
            super::ExtendedEvents::name_for_type(),
            super::ExecutionPlanViewer::name_for_type(),
        ] {
            assert!(disabled.contains_key(action), "{action} should be gated");
            assert!(!disabled[action].is_empty());
        }
        // Cross-engine tools stay live.
        assert!(!disabled.contains_key(super::SchemaCompare::name_for_type()));
        assert!(!disabled.contains_key(super::DataCompare::name_for_type()));
        assert!(!disabled.contains_key(super::EstimatedPlanViewer::name_for_type()));
    }

    #[test]
    fn redis_grays_both_plan_actions_naming_the_engine() {
        let disabled = AppView::engine_gated_menu_actions(Some(DatabaseType::Redis));
        for action in [
            super::ExecutionPlanViewer::name_for_type(),
            super::EstimatedPlanViewer::name_for_type(),
        ] {
            let reason = disabled.get(action).expect("plan action gated on Redis");
            assert!(
                reason.starts_with("Query plans are disabled for this database type (Redis)."),
                "{action}: {reason}"
            );
        }
        // SQLite keeps Estimated live but explains why Actual is off.
        let sqlite = AppView::engine_gated_menu_actions(Some(DatabaseType::Sqlite));
        assert!(
            sqlite[super::ExecutionPlanViewer::name_for_type()]
                .contains("disabled for this database type (SQLite)")
        );
    }

    #[test]
    fn postgres_keeps_ported_admin_tools() {
        let disabled = AppView::engine_gated_menu_actions(Some(DatabaseType::Postgres));
        assert!(!disabled.contains_key(super::ActivityMonitor::name_for_type()));
        assert!(!disabled.contains_key(super::ServerDashboard::name_for_type()));
        assert!(!disabled.contains_key(super::QueryStore::name_for_type()));
        assert!(disabled.contains_key(super::SqlAgent::name_for_type()));
    }

    /// Registry-derived drift proofing: the expected gated set comes from the
    /// palette's own command registry, so a newly registered engine-bound
    /// tool cannot be silently missed by the capability matrix or the
    /// in-window menu pairing. Connection-independent tools are exempted
    /// explicitly, with the reason beside each id.
    #[test]
    fn every_registered_tool_command_is_gated_or_deliberately_exempt() {
        // Tools that open app-local surfaces and never touch an engine.
        const ENGINE_INDEPENDENT_TOOLS: &[&str] = &[
            "tools.commandPalette",       // the palette itself
            "tools.quickOpen",            // local index of the loaded explorer tree
            "tools.keybindings",          // local settings editor
            "tools.preferences",          // local settings dialog
            "tools.externalEditor",       // local file/editor hand-off
            "tools.importExport",         // file wizard picks its own target
            "tools.importFromClipboard",  // clipboard wizard picks its own target
            "tools.templateExplorer",     // local template catalog
            "tools.deployCart", // local cart; each group resolves its own saved connection
            "tools.connectionManagement", // manages profiles while disconnected
            "tools.connectionHealth", // lists/tests every profile, no active connection needed
            "tools.resultSnapshots", // local snapshot store
            "tools.savedCharts", // local chart library (saved connection)
            "tools.chartDashboard", // local chart library (saved connections)
            "tools.azureSqlConnection", // starts a connection flow
            "tools.addFirewallRule", // Azure flow with its own gating
            "tools.entraAccounts", // account management dialog
            "tools.scheduleTask", // schedule editor reports a missing connection itself
        ];
        // Admin tools with no gpui action and no menu item — there is
        // nothing in-window to gray. They are still capability-gated: the
        // palette hides them off-engine and `require_admin_tool` refuses
        // them with the matrix's reason.
        const PALETTE_ONLY_ADMIN_TOOLS: &[&str] = &[
            "tools.redisProfiler", // Redis-only tab of the (paired) Activity Monitor
            "tools.redisPubSub",   // Redis-only tab of the (paired) Activity Monitor
            // Redis-only report pane; node-scoped like Disk Usage, so it is
            // never in the application menu and needs no graying pairing.
            "tools.redisMemoryAnalyzer",
            // Redis-only key view; opened from a key in the explorer (the
            // palette form prompts for a key), never from the application menu.
            "tools.redisKeyView",
            // Redis-only cluster pane; opened from the palette, the Server
            // Dashboard or a database's menu, never the application menu.
            "tools.redisClusterTopology",
        ];
        let registry = crate::command_palette::CommandRegistry::default_registry();
        let pairings = AppView::engine_gated_action_commands();
        let paired_ids: Vec<&str> = pairings.iter().map(|(_, id)| *id).collect();
        for command in registry.all() {
            let id = command.id.as_str();
            if !id.starts_with("tools.") || ENGINE_INDEPENDENT_TOOLS.contains(&id) {
                continue;
            }
            assert!(
                crate::capabilities::command_capability(id, Some(DatabaseType::SqlServer))
                    .is_some(),
                "registered tool {id} is neither in the central capability matrix nor on the engine-independent exempt list"
            );
            // Engine-varying admin tools must also be paired for in-window
            // menu graying (connection-gated-but-universal tools need no
            // per-engine pairing).
            if crate::capabilities::AdminTool::from_command_id(id).is_some()
                && !PALETTE_ONLY_ADMIN_TOOLS.contains(&id)
            {
                assert!(
                    paired_ids.contains(&id),
                    "admin tool {id} has no in-window menu-action pairing"
                );
            }
        }
        // The plan captures are gated too, and pairing names are unique.
        for id in ["query.explainPlan", "query.explainEstimated"] {
            assert!(paired_ids.contains(&id), "{id} missing menu pairing");
        }
        let mut names: Vec<&str> = pairings.iter().map(|(name, _)| *name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), pairings.len());
    }
}
