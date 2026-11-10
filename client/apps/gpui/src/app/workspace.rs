use super::*;

pub(super) fn restored_tab_connection_target(
    state: &crate::workspace_session::TabState,
    connection_exists: impl Fn(uuid::Uuid) -> bool,
) -> Option<crate::tabs::TabConnectionTarget> {
    let profile_id = uuid::Uuid::parse_str(state.connection_id.as_deref()?).ok()?;
    connection_exists(profile_id).then(|| crate::tabs::TabConnectionTarget {
        profile_id,
        database: state
            .database
            .clone()
            .filter(|database| !database.is_empty()),
    })
}

impl AppView {
    /// Snapshot the current workspace layout to the session file. Called on
    /// sidebar resize, AI toggle, tab add/remove/switch. Mirrors the Swift
    /// `WorkspaceSession.save()` which fires on layout changes.
    pub(super) fn current_workspace_session(
        &self,
        cx: &gpui::App,
    ) -> crate::workspace_session::WorkspaceSession {
        // DDL viewer tabs are session-only: their content is a point-in-time
        // fetch, so they are excluded from persistence. The selected index is
        // re-based onto the surviving query tabs.
        let persistable = |t: &crate::tabs::QueryTab| {
            t.ddl_view.is_none() && (!t.is_notebook() || t.notebook_path.is_some())
        };
        let selected_tab = self
            .tabs
            .iter()
            .take(self.active_tab)
            .filter(|t| persistable(t))
            .count();
        crate::workspace_session::WorkspaceSession {
            version: crate::workspace_session::CURRENT_VERSION,
            tabs: self
                .tabs
                .iter()
                .filter(|t| persistable(t))
                .map(|t| {
                    // Save editor content for unsaved tabs so it survives
                    // restart. File-backed tabs re-read from disk on restore.
                    // Notebooks persist via `notebook_path`, never `content`.
                    let content = if t.is_file_backed() || t.is_notebook() {
                        None
                    } else {
                        Some(t.editor.read(cx).text().to_string())
                    };
                    crate::workspace_session::TabState {
                        title: t.name.clone(),
                        split_mode: crate::workspace_session::SplitMode::None,
                        file_path: t.file_path.clone(),
                        content,
                        connection_id: t
                            .connection_target
                            .as_ref()
                            .map(|c| c.profile_id.to_string()),
                        database: t
                            .connection_target
                            .as_ref()
                            .and_then(|c| c.database.clone()),
                        ddl_panel_width: t.ddl_panel_width,
                        #[cfg(not(target_arch = "wasm32"))]
                        history_panel_width: t.history_panel_width,
                        #[cfg(target_arch = "wasm32")]
                        history_panel_width: crate::workspace_session::DEFAULT_HISTORY_PANEL_WIDTH,
                        pinned: t.pinned,
                        user_named: t.user_named,
                        auto_named: Some(t.auto_named),
                        auto_number: t.persisted_auto_number(),
                        #[cfg(not(target_arch = "wasm32"))]
                        result_cache_key: t.result_cache_key.clone(),
                        #[cfg(target_arch = "wasm32")]
                        result_cache_key: None,
                        notebook_path: t.notebook_path.clone(),
                        query_params: t.query_params.persisted(cx),
                    }
                })
                .collect(),
            selected_tab,
            layout: crate::workspace_session::LayoutState {
                object_explorer_visible: self.sidebar_visible,
                results_pane_visible: self.results_visible(),
                results_pane: Some(self.results_pane),
                sidebar_width: self.sidebar_width,
                results_height: self.results_height,
                ai_assistant_visible: self.ai_visible,
                ai_assistant_width: self.ai_panel_width,
                ai_assistant_section: Some(
                    self.ai_panel
                        .read(cx)
                        .active_section()
                        .persisted_key()
                        .to_string(),
                ),
                editor_zoom: self.editor_zoom,
                // Persist the query-bar server + database selection so the
                // dropdowns come back prepopulated on next launch.
                active_connection_id: self.active_profile.as_ref().map(|p| p.id.0.to_string()),
                active_database: self
                    .active_profile
                    .as_ref()
                    .and_then(|p| p.database.clone())
                    .filter(|s| !s.is_empty()),
                project_file_picker_root: self
                    .project_file_picker_root
                    .as_ref()
                    .map(|p| p.to_string_lossy().to_string()),
                sidebar_panel: Some(self.sidebar.read(cx).panel_persist_key().to_string()),
                diagnostics_height: self.log_height,
                // The split's unfocused-tab index is re-based onto the
                // persisted (DDL-tab-free) list, same as `selected_tab`;
                // a split onto a session-only DDL tab is not persisted. A
                // same-buffer split carries no index — it re-attaches to the
                // selected tab on restore.
                cell_detail_visible: self.cell_detail_visible,
                cell_detail_width: self.cell_detail_width,
                cell_detail_height: self.cell_detail_height,
                editor_split: self.split.as_ref().and_then(|split| {
                    let axis = match split.axis {
                        crate::editor_groups::SplitAxis::Vertical => {
                            crate::workspace_session::SplitMode::Vertical
                        }
                        crate::editor_groups::SplitAxis::Horizontal => {
                            crate::workspace_session::SplitMode::Horizontal
                        }
                    };
                    let Some(split_other) = split.other_tab() else {
                        return Some(crate::workspace_session::EditorSplit {
                            axis,
                            other_tab: 0,
                            active_first: split.active_first,
                            first_size: split.first_size,
                            same_buffer: true,
                        });
                    };
                    let tab = self.tabs.get(split_other)?;
                    if tab.ddl_view.is_some()
                        || (tab.notebook.is_some() && tab.notebook_path.is_none())
                    {
                        return None;
                    }
                    let other_tab = self
                        .tabs
                        .iter()
                        .take(split_other)
                        .filter(|t| {
                            t.ddl_view.is_none()
                                && (t.notebook.is_none() || t.notebook_path.is_some())
                        })
                        .count();
                    Some(crate::workspace_session::EditorSplit {
                        axis,
                        other_tab,
                        active_first: split.active_first,
                        first_size: split.first_size,
                        same_buffer: false,
                    })
                }),
            },
        }
        .sanitized()
    }

    /// Debounced autosave of the workspace session after an edit. Structural
    /// events (open/close/split/…) already checkpoint synchronously; this closes
    /// the gap for *typing*, which previously only marked the tab dirty — so on
    /// an unclean exit (crash, kill, power loss) keystrokes since the last
    /// structural checkpoint were lost. Each edit bumps a token and arms a
    /// short timer; the save fires only if no further edit arrived meanwhile,
    /// mirroring the query-history idle-seal debounce.
    pub(crate) fn schedule_session_autosave(&mut self, cx: &mut Context<Self>) {
        self.session_autosave_token = self.session_autosave_token.wrapping_add(1);
        let token = self.session_autosave_token;
        let weak_self = cx.weak_entity();
        let mut async_cx = cx.to_async();
        cx.spawn(async move |_this, _cx| {
            async_cx
                .background_executor()
                .timer(std::time::Duration::from_millis(1500))
                .await;
            weak_self
                .update(&mut async_cx, |this, cx| {
                    if this.session_autosave_token != token {
                        return;
                    }
                    this.save_workspace_session(cx);
                })
                .ok();
        })
        .detach();
    }

    pub(super) fn save_workspace_session(&mut self, cx: &Context<Self>) {
        let session = self.current_workspace_session(cx);
        // One session file serves every window: fold in the other windows'
        // tabs so this save cannot drop them (tab_move.rs).
        #[cfg(not(target_arch = "wasm32"))]
        let session = Self::session_with_other_windows(session, cx.entity_id(), cx);
        if let Err(e) = crate::workspace_session::save_session(
            &session,
            &crate::workspace_session::session_file(),
        ) {
            tracing::warn!(error = %e, "could not save workspace session");
        }
        let Some(active_id) = self.named_workspace_index.active_workspace_id.clone() else {
            return;
        };
        let Some(name) = self
            .named_workspace_index
            .workspaces
            .iter()
            .find(|item| item.id == active_id)
            .map(|item| item.name.clone())
        else {
            self.named_workspace_index.active_workspace_id = None;
            return;
        };
        if let Err(error) = self.named_workspace_store.save(
            &mut self.named_workspace_index,
            Some(&active_id),
            &name,
            &session,
            chrono::Utc::now().timestamp_millis(),
        ) {
            tracing::warn!(%error, "could not update active named workspace");
        }
    }

    pub(super) fn apply_workspace_session(
        &mut self,
        session: crate::workspace_session::WorkspaceSession,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let session = session.sanitized();
        if let Some(profile) = self.active_profile.take() {
            self.clear_remote_services(&profile.id);
        }
        self.sidebar
            .update(cx, |sidebar, cx| sidebar.set_bound_server(None, cx));
        self.ai_panel
            .update(cx, |panel, cx| panel.set_profile(None, cx));
        self.clear_completion_catalogs(cx);
        self.tabs.clear();
        self.active_tab = 0;
        self.tab_counter = 0;
        self.split = None;
        self.same_buffer_view = None;
        self.sidebar_visible = session.layout.object_explorer_visible;
        self.sidebar_width = session.layout.sidebar_width;
        self.results_pane = session.layout.effective_results_pane();
        self.results_height = session.layout.results_height;
        self.cell_detail_visible = session.layout.cell_detail_visible;
        self.cell_detail_width = session.layout.cell_detail_width;
        self.cell_detail_height = session.layout.cell_detail_height;
        self.ai_visible = session.layout.ai_assistant_visible;
        self.ai_width = session.layout.ai_assistant_width;
        self.ai_panel_width = clamp_ai_width(session.layout.ai_assistant_width);
        self.project_file_picker_root = session
            .layout
            .project_file_picker_root
            .as_deref()
            .map(PathBuf::from)
            .filter(|path| path.is_dir());
        self.sidebar.update(cx, |sidebar, cx| {
            sidebar.restore_panel(session.layout.sidebar_panel.as_deref(), cx);
        });

        let connection_store = ConnectionStore::global();
        let restore_policy = crate::preferences::TabPreferences::load().restore_policy;
        let (restore_tabs, selected) = crate::workspace_session::filter_tabs_for_restore(
            &session.tabs,
            session.selected_tab,
            restore_policy,
        );
        if restore_tabs.is_empty() {
            self.new_tab(cx, Some(super::DEFAULT_TAB_SQL.to_string()));
        } else {
            for tab_state in &restore_tabs {
                if let Some(path) = tab_state.notebook_path.as_ref() {
                    if std::path::Path::new(path).exists() {
                        self.open_notebook_from_path(path, cx);
                        if let Some(tab) = self.tabs.last_mut() {
                            tab.name = tab_state.title.clone();
                            tab.pinned = tab_state.pinned;
                            tab.user_named = tab_state.user_named;
                            tab.auto_named = !tab.user_named;
                            tab.ddl_panel_width = tab_state.ddl_panel_width;
                            #[cfg(not(target_arch = "wasm32"))]
                            {
                                tab.history_panel_width = tab_state.history_panel_width;
                            }
                            tab.connection_target =
                                restored_tab_connection_target(tab_state, |id| {
                                    connection_store.get(id).is_some()
                                });
                            tab.restored_connection_inert = tab.connection_target.is_some();
                            #[cfg(not(target_arch = "wasm32"))]
                            {
                                tab.result_cache_key = tab_state.result_cache_key.clone();
                            }
                        }
                    } else {
                        tracing::debug!(path, "notebook file missing; skipping restore");
                    }
                    continue;
                }
                let initial_text = match tab_state.file_path.as_deref() {
                    Some(path) => std::fs::read_to_string(path)
                        .ok()
                        .or_else(|| Some(String::new())),
                    None => tab_state.content.clone(),
                };
                self.new_tab(cx, initial_text);
                if let Some(tab) = self.tabs.last_mut() {
                    tab.name = tab_state.title.clone();
                    tab.file_path = tab_state.file_path.clone();
                    tab.pinned = tab_state.pinned;
                    tab.user_named = tab_state.user_named;
                    tab.apply_restored_auto_naming(tab_state);
                    tab.ddl_panel_width = tab_state.ddl_panel_width;
                    #[cfg(not(target_arch = "wasm32"))]
                    {
                        tab.history_panel_width = tab_state.history_panel_width;
                    }
                    tab.connection_target = restored_tab_connection_target(tab_state, |id| {
                        connection_store.get(id).is_some()
                    });
                    tab.restored_connection_inert = tab.connection_target.is_some();
                    tab.query_params = crate::query_params_strip::ParamStripState::from_persisted(
                        &tab_state.query_params,
                    );
                    #[cfg(not(target_arch = "wasm32"))]
                    {
                        tab.result_cache_key = tab_state.result_cache_key.clone();
                    }
                }
            }
        }
        self.active_tab = selected.min(self.tabs.len().saturating_sub(1));
        self.tab_counter = self
            .tabs
            .iter()
            .filter_map(|tab| {
                tab.auto_number
                    .or_else(|| crate::tabs::auto_title_number(&tab.name))
                    .map(|number| number as usize)
            })
            .max()
            .unwrap_or(self.tabs.len());
        self.seed_tab_navigation();

        if let Some(saved) = session.layout.editor_split {
            let axis = match saved.axis {
                crate::workspace_session::SplitMode::Vertical => {
                    Some(crate::editor_groups::SplitAxis::Vertical)
                }
                crate::workspace_session::SplitMode::Horizontal => {
                    Some(crate::editor_groups::SplitAxis::Horizontal)
                }
                crate::workspace_session::SplitMode::None => None,
            };
            if let Some(axis) = axis {
                if saved.same_buffer {
                    let split = crate::editor_groups::SplitState::new_same_buffer(
                        axis,
                        saved.active_first,
                        saved.first_size,
                    );
                    if split.is_valid(self.active_tab, self.tabs.len())
                        && let Some(tab) = self.tabs.get(self.active_tab)
                    {
                        let view = SqlEditor::new_shared_view(&tab.editor, cx);
                        cx.subscribe(&view, Self::handle_editor_event).detach();
                        self.same_buffer_view = Some(view);
                        self.split = Some(split);
                    }
                } else {
                    let split = crate::editor_groups::SplitState::new(
                        axis,
                        saved.other_tab,
                        saved.active_first,
                        saved.first_size,
                    );
                    if split.is_valid(self.active_tab, self.tabs.len()) {
                        self.split = Some(split);
                    }
                }
            }
        }
        if let Some(section) = session
            .layout
            .ai_assistant_section
            .as_deref()
            .and_then(AiSection::from_persisted_key)
        {
            self.ai_panel
                .update(cx, |panel, cx| panel.set_active_section(section, cx));
        }
        self.set_editor_zoom(session.layout.editor_zoom, window, cx);
        self.sync_toolbar_to_active_tab();
        self.set_status(
            StatusLevel::Success,
            crate::localization::text("workspace.status.restored"),
        );
        cx.notify();
    }

    pub(super) fn open_workspace_dialog(
        &mut self,
        mode: WorkspaceDialogMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.named_workspace_index = self.named_workspace_store.load_index();
        let dialog =
            cx.new(|cx| WorkspaceDialog::new(mode, self.named_workspace_index.clone(), window, cx));
        cx.subscribe_in(&dialog, window, Self::handle_workspace_dialog_event)
            .detach();
        dialog.update(cx, |dialog, cx| dialog.focus_name(window, cx));
        self.overlays.workspace_dialog = Some(dialog);
        cx.notify();
    }

    pub(super) fn handle_workspace_dialog_event(
        &mut self,
        dialog: &Entity<WorkspaceDialog>,
        event: &WorkspaceDialogEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let now = chrono::Utc::now().timestamp_millis();
        let result = match event {
            WorkspaceDialogEvent::Save(name) => {
                let session = self.current_workspace_session(cx);
                self.named_workspace_store
                    .save(&mut self.named_workspace_index, None, name, &session, now)
                    .map(|_| crate::localization::text("workspace.status.saved"))
            }
            WorkspaceDialogEvent::Open(id) => {
                let Some(session) = self.named_workspace_store.load(id) else {
                    dialog.update(cx, |dialog, cx| {
                        dialog.set_error(
                            crate::localization::text("workspace.store.snapshot_corrupt"),
                            cx,
                        )
                    });
                    return;
                };
                match self
                    .named_workspace_store
                    .set_active(&mut self.named_workspace_index, Some(id))
                {
                    Ok(()) => {
                        self.apply_workspace_session(session, window, cx);
                        self.save_workspace_session(cx);
                        self.overlays.workspace_dialog = None;
                        return;
                    }
                    Err(error) => Err(error),
                }
            }
            WorkspaceDialogEvent::Rename { id, name } => self
                .named_workspace_store
                .rename(&mut self.named_workspace_index, id, name, now)
                .map(|_| crate::localization::text("workspace.status.renamed")),
            WorkspaceDialogEvent::Duplicate { id, name } => self
                .named_workspace_store
                .duplicate(&mut self.named_workspace_index, id, name, now)
                .map(|_| crate::localization::text("workspace.status.duplicated")),
            WorkspaceDialogEvent::Delete(id) => self
                .named_workspace_store
                .delete(&mut self.named_workspace_index, id)
                .map(|_| crate::localization::text("workspace.status.deleted")),
            WorkspaceDialogEvent::Close => {
                self.overlays.workspace_dialog = None;
                cx.notify();
                return;
            }
        };
        match result {
            Ok(message) => {
                self.set_status(StatusLevel::Success, message);
                self.named_workspace_index = self.named_workspace_store.load_index();
                dialog.update(cx, |dialog, cx| {
                    dialog.update_index(self.named_workspace_index.clone(), cx)
                });
                if matches!(event, WorkspaceDialogEvent::Save(_)) {
                    self.overlays.workspace_dialog = None;
                }
            }
            Err(error) => dialog.update(cx, |dialog, cx| dialog.set_error(error.to_string(), cx)),
        }
        cx.notify();
    }

    pub(super) fn handle_save_workspace_as(
        &mut self,
        _action: &SaveWorkspaceAs,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_workspace_dialog(WorkspaceDialogMode::SaveAs, window, cx);
    }

    pub(super) fn handle_open_workspace(
        &mut self,
        _action: &OpenWorkspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_workspace_dialog(WorkspaceDialogMode::Manage, window, cx);
    }

    pub(super) fn handle_manage_workspaces(
        &mut self,
        _action: &ManageWorkspaces,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_workspace_dialog(WorkspaceDialogMode::Manage, window, cx);
    }
}
