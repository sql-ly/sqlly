use super::*;
use crate::tabs::should_auto_name;

/// What Reopen Closed Tab restores: identity, path, buffer text (for
/// unsaved/dirty tabs), and the tab's pinned connection.
pub(super) struct ClosedTabSnapshot {
    name: String,
    file_path: Option<String>,
    content: Option<String>,
    connection_target: Option<crate::tabs::TabConnectionTarget>,
    user_named: bool,
}

impl AppView {
    pub(super) fn next_tab_name(&mut self) -> String {
        self.tab_counter += 1;
        crate::localization::format(
            "tabs.automatic.query_title",
            &[("number", &self.tab_counter.to_string())],
        )
    }

    pub(super) fn make_cell_editor(
        &self,
        initial_text: Option<&str>,
        cx: &mut Context<Self>,
    ) -> Entity<SqlEditor> {
        let editor_services = self
            .active_profile
            .as_ref()
            .map(|p| self.services_for_profile(p))
            .unwrap_or_else(|| self.services.clone());
        let editor = cx.new(|cx| {
            let mut editor = SqlEditor::new(cx);
            editor.set_services(editor_services.clone(), cx);
            editor.set_editor_prefs(self.editor_prefs.clone(), cx);
            editor.set_ddl_nav_prefs(self.ddl_nav_prefs.clone(), cx);
            if let Some(profile) = &self.active_profile {
                editor.set_database_type(profile.database_type, cx);
            }
            editor.set_completion_rules(self.completion_rules.clone(), cx);
            editor.set_font(
                &self.appearance_prefs.editor_font,
                crate::editor_zoom::zoomed_font_size(
                    self.appearance_prefs.editor_font_size,
                    self.editor_zoom,
                ),
                cx,
            );
            if let Some(text) = initial_text {
                editor.set_text(text, cx);
            }
            editor
        });
        let redis_completion = self.redis_completion_context();
        editor.update(cx, |e, cx| {
            e.set_completion_catalog(self.completion_catalog.clone(), cx);
            e.set_schema_snapshot(self.completion_snapshot.clone(), cx);
            e.set_completion_procedures(self.completion_procedures.clone(), cx);
            e.set_redis_completion(redis_completion, cx);
        });
        editor
    }

    pub(super) fn make_cell_results(
        &self,
        editor: &Entity<SqlEditor>,
        cx: &mut Context<Self>,
    ) -> Entity<ResultsView> {
        let results = cx.new(|cx| {
            let mut rv = ResultsView::new(cx);
            rv.set_formatting(self.formatting.clone(), cx);
            rv.set_show_detailed_timing(self.show_detailed_query_timing, cx);
            rv
        });
        results.update(cx, |rv, cx| {
            rv.set_editor(editor.downgrade(), cx);
        });
        self.attach_cell_detail_panel(&results, cx);
        results
    }

    pub(super) fn new_tab(&mut self, cx: &mut Context<Self>, initial_text: Option<String>) {
        let name = self.next_tab_name();
        let editor = self.make_cell_editor(initial_text.as_deref(), cx);
        let results = self.make_cell_results(&editor, cx);

        cx.subscribe(&editor, Self::handle_editor_event).detach();
        cx.subscribe(&results, Self::handle_results_event).detach();

        let mut tab = QueryTab::new(format!("tab-{}", self.tab_counter), name, editor, results);
        tab.auto_number = u32::try_from(self.tab_counter).ok();
        // Inline AI autocomplete starts from the preferences seed (global
        // switch or the new-query opt-in); the toolbar sparkle toggle owns it
        // from here.
        tab.fim_autocomplete_enabled = self.fim_prefs.seed_new_tab_enabled();
        // Initialize the resume effective state for the new tab based on the
        // current global scope. New tabs are not file-backed.
        tab.resume_effective_enabled = crate::preferences::QueryResumePolicy::effective_enabled(
            self.query_resume_prefs.scope,
            false, // new tab is not file-backed
            None,  // no override
        );
        // Procedure detection normally rides editor Change events, which
        // programmatic set_text never emits — seed it here or the Proc
        // toggle stays dark on freshly opened files until the first edit.
        let proc_dialect = self.active_proc_dialect();
        tab.proc_mode
            .refresh_from_script_for(tab.editor.read(cx).text(), proc_dialect);
        self.tabs.push(tab);
        self.active_tab = self.tabs.len() - 1;
        self.record_tab_activation(self.active_tab);
        // A same-buffer split follows the active tab.
        self.retarget_same_buffer_view(cx);
        // A new tab is born pinned to the connection it inherits.
        self.stamp_active_tab_connection();
        // Sync toolbar to the new tab's state.
        self.sync_toolbar_to_active_tab();
        cx.notify();
    }

    pub(super) fn new_notebook_tab(
        &mut self,
        model: crate::notebook_model::Notebook,
        path: Option<String>,
        cx: &mut Context<Self>,
    ) -> usize {
        self.tab_counter += 1;
        let editor = self.make_cell_editor(None, cx);
        let results = self.make_cell_results(&editor, cx);
        cx.subscribe(&editor, Self::handle_editor_event).detach();
        cx.subscribe(&results, Self::handle_results_event).detach();

        let name = path
            .as_deref()
            .and_then(|p| {
                std::path::Path::new(p)
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| {
                crate::tabs::auto_notebook_title(u32::try_from(self.tab_counter).unwrap_or(0))
            });
        let mut tab = QueryTab::new(format!("tab-{}", self.tab_counter), name, editor, results);
        // Language-neutral number behind an unsaved `Notebook N` title, so a
        // language switch can re-title it.
        tab.auto_number = path
            .is_none()
            .then(|| u32::try_from(self.tab_counter).ok())
            .flatten();
        tab.fim_autocomplete_enabled = self.fim_prefs.seed_new_tab_enabled();
        tab.resume_effective_enabled = crate::preferences::QueryResumePolicy::effective_enabled(
            self.query_resume_prefs.scope,
            false,
            None,
        );
        tab.auto_named = path.is_none();
        let notebook = cx.new(|cx| crate::notebook_view::NotebookView::new(model, cx));
        if let Some(profile) = &self.active_profile {
            notebook.update(cx, |nb, _cx| nb.set_database_type(profile.database_type));
        }
        cx.subscribe(&notebook, Self::handle_notebook_event)
            .detach();
        tab.notebook = Some(notebook.clone());
        tab.notebook_path = path;
        self.tabs.push(tab);
        self.active_tab = self.tabs.len() - 1;
        self.record_tab_activation(self.active_tab);
        self.retarget_same_buffer_view(cx);
        self.stamp_active_tab_connection();
        self.sync_toolbar_to_active_tab();
        notebook.update(cx, |nb, cx| nb.ensure_cell_entities(cx));
        cx.notify();
        self.active_tab
    }

    pub(super) fn active_notebook(
        &self,
    ) -> Option<(usize, Entity<crate::notebook_view::NotebookView>)> {
        let idx = self.active_tab;
        let tab = self.tabs.get(idx)?;
        Some((idx, tab.notebook.clone()?))
    }

    /// Resolve a tab's stable id to its current index. Used to re-locate a tab
    /// after an async prompt/dialog, since a raw index can retarget the wrong
    /// tab if tabs are closed or reordered while the prompt is up.
    pub(super) fn tab_index_by_id(&self, id: &str) -> Option<usize> {
        tab_index_of(self.tabs.iter().map(|t| t.id.as_str()), id)
    }

    pub(super) fn close_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.tabs.len() {
            return;
        }
        if self.arm_transaction_close_if_dirty(index, TransactionCloseReason::CloseTab, cx) {
            return;
        }
        if self
            .tabs
            .get(index)
            .and_then(|t| t.manual_tx.as_ref())
            .is_some()
        {
            self.silent_rollback_tab(index, cx);
        }
        self.close_tab_impl(index, cx);
    }

    pub(super) fn close_tab_impl(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.tabs.len() {
            return;
        }
        self.teardown_detached_results_for_tab(index, cx);
        let closing_id = self.tabs[index].id.clone();
        let successor = if index == self.active_tab {
            self.tab_mru
                .next_after_close(&closing_id)
                .map(str::to_string)
        } else {
            None
        };
        self.cancel_tab_query(index, cx);
        #[cfg(not(target_arch = "wasm32"))]
        self.remove_cached_results_for_tab(index);
        self.snapshot_closing_tab(index, cx);
        // Query version history: an untitled tab's whole store dies with the
        // tab (index AND result payloads); a file-backed tab keeps its
        // history for the next session.
        #[cfg(not(target_arch = "wasm32"))]
        let history_purge = {
            let tab = self.tabs.get(index);
            tab.map(|t| {
                (
                    crate::query_version_history::VersionStore::store_key(
                        &Self::history_tab_id(&self.history_session_id, t),
                        t.file_path.as_deref(),
                    ),
                    t.is_file_backed(),
                )
            })
        };
        #[cfg(not(target_arch = "wasm32"))]
        if let Some((key, file_backed)) = history_purge
            && let Some(store) = self.history_stores.remove(&key)
            && !file_backed
        {
            store.purge();
        }
        // Flush the closing tab's full record (summaries + connection +
        // resume) to the metadata store before removing it, mirroring the
        // Swift `flushAISummary(for:)` + `flushQueryResumeDocumentMetadata()`.
        if let Some(tab) = self.tabs.get(index) {
            let connection = tab
                .pending_resume_connection
                .clone()
                .or_else(|| self.active_connection_metadata());
            let resume = tab.current_resume_metadata();
            let key = tab.metadata_key();
            let record = tab
                .ai_summary
                .make_record_with(connection.as_ref(), resume.as_ref());
            self.ai_summary_metadata.set(key, record);
            if let Some(path) = crate::query_ai_summary::ai_summary_metadata_file() {
                let _ = crate::query_ai_summary::QueryAISummaryMetadataStore::save(
                    &self.ai_summary_metadata,
                    &path,
                );
            }
        }
        self.tabs.remove(index);
        // Closing a tab left of the active one shifts the active tab down a
        // slot; follow it so focus stays on the same tab.
        if index < self.active_tab {
            self.active_tab -= 1;
        }
        // Keep `active_tab` in range. When the last tab is closed, `tabs` is
        // empty and the render falls through to the no-tab empty state.
        if self.tabs.is_empty() {
            self.active_tab = 0;
        } else if self.active_tab >= self.tabs.len() {
            self.active_tab = self.tabs.len() - 1;
        }
        self.tab_mru.remove(&closing_id);
        self.tab_history.remove(&closing_id);
        if let Some(id) = successor
            && let Some(i) = self.tab_index_by_id(&id)
        {
            self.active_tab = i;
        }
        // The split's unfocused-tab index must survive the removal — or the
        // split closes when it can't (its tab is gone, or both panes would
        // show the same one). A same-buffer split simply follows the
        // surviving active tab (or closes with the last tab).
        if let Some(split) = self.split.take() {
            self.split = split.after_tab_removed(index, self.active_tab, self.tabs.len());
        }
        if self.split.is_none() {
            self.drop_same_buffer_view(cx);
        }
        self.retarget_same_buffer_view(cx);
        // The tab that inherits focus brings its own pinned connection.
        self.sync_connection_to_active_tab(cx);
        self.save_workspace_session(cx);
        cx.notify();
    }

    /// Close a tab, first prompting to save when it has unsaved changes.
    ///
    /// Clean tabs close immediately. A dirty tab raises a native
    /// Save / Don't Save / Cancel prompt: "Don't Save" closes and discards,
    /// "Save" writes the file (opening a save dialog for an untitled tab) and
    /// then closes, and "Cancel" (or dismissing the prompt) leaves the tab open.
    pub(super) fn request_close_tab(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        match close_tab_prompt(tab.dirty, tab_has_staged_grid_edits(tab, cx)) {
            CloseTabPrompt::None => {
                self.close_tab(index, cx);
                return;
            }
            CloseTabPrompt::StagedGridEdits => {
                self.prompt_discard_staged_then_close(index, window, cx);
                return;
            }
            CloseTabPrompt::UnsavedText => {}
        }
        let name = self
            .tabs
            .get(index)
            .map(|t| t.name.clone())
            .unwrap_or_else(|| crate::localization::text("tabs.close.fallback_name"));
        let message = crate::localization::format("tabs.close.unsaved_message", &[("name", &name)]);
        let detail = crate::localization::text("tabs.close.unsaved_detail");
        let save = crate::localization::text("tabs.close.save_button");
        let discard = crate::localization::text("tabs.close.discard_button");
        let cancel = crate::localization::text("tabs.close.cancel_button");
        let rx = window.prompt(
            gpui::PromptLevel::Warning,
            &message,
            Some(&detail),
            &[
                gpui::PromptButton::new(&save),
                gpui::PromptButton::new(&discard),
                gpui::PromptButton::cancel(&cancel),
            ],
            cx,
        );
        // Capture the tab's stable id, not its index: the index can retarget a
        // different tab if tabs are closed/reordered while the prompt is up.
        let Some(tab_id) = self.tabs.get(index).map(|t| t.id.clone()) else {
            return;
        };
        // Button index: 0 Save, 1 Don't Save, else (Cancel / dismissed) no-op.
        let weak = cx.weak_entity();
        let async_cx = cx.to_async();
        cx.spawn(async move |_this, _cx| {
            let choice = rx.await;
            let mut cx = async_cx.clone();
            weak.update(&mut cx, |this, cx| match choice {
                Ok(0) => this.save_tab_then_close(tab_id.clone(), cx),
                Ok(1) => {
                    if let Some(i) = this.tab_index_by_id(&tab_id) {
                        this.close_tab(i, cx);
                    }
                }
                _ => {}
            })
            .ok();
        })
        .detach();
    }

    /// Closing a tab whose results hold staged (unsaved) grid edits: ask
    /// Discard / Cancel. Discard drops the staged edits and carries on with
    /// the ordinary close, so unsaved SQL text still gets its own prompt.
    fn prompt_discard_staged_then_close(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        let tab_id = tab.id.clone();
        let message =
            crate::localization::format("tabs.close.staged_edits_message", &[("name", &tab.name)]);
        let detail = crate::localization::text("tabs.close.staged_edits_detail");
        let discard = crate::localization::text("tabs.close.staged_edits_discard_button");
        let cancel = crate::localization::text("tabs.close.cancel_button");
        let rx = window.prompt(
            gpui::PromptLevel::Warning,
            &message,
            Some(&detail),
            &[
                gpui::PromptButton::new(&discard),
                gpui::PromptButton::cancel(&cancel),
            ],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if rx.await != Ok(0) {
                return;
            }
            let _ = this.update_in(cx, |this, window, cx| {
                // Re-resolve by id: tabs may have moved while the prompt was up.
                let Some(i) = this.tab_index_by_id(&tab_id) else {
                    return;
                };
                let results = this.tabs[i].results.clone();
                results.update(cx, |r, cx| r.discard_staged(cx));
                this.request_close_tab(i, window, cx);
            });
        })
        .detach();
    }

    /// Save the tab identified by `tab_id` and close it on success. A file-backed
    /// tab writes in place; an untitled tab opens a save dialog first. A failed
    /// write (or a cancelled save dialog) leaves the tab open. The tab is
    /// re-located by id after the (async) save dialog, so a tab closed or
    /// reordered while the dialog is up can never be written to the wrong path.
    pub(super) fn save_tab_then_close(&mut self, tab_id: String, cx: &mut Context<Self>) {
        #[cfg(target_arch = "wasm32")]
        {
            let _ = tab_id;
            self.set_status(
                StatusLevel::Error,
                crate::localization::format(
                    "tabs.save.browser_unavailable",
                    &[(
                        "reason",
                        &crate::browser_preview::unavailable(&crate::localization::text(
                            "tabs.save.feature_unavailable",
                        )),
                    )],
                ),
            );
            cx.notify();
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let Some(index) = self.tab_index_by_id(&tab_id) else {
                return;
            };
            let (editor, existing_path) = match self.tabs.get(index) {
                Some(t) => (t.editor.clone(), t.file_path.clone()),
                None => return,
            };
            let text = editor.read(cx).text().to_string();

            if let Some(path) = existing_path {
                match std::fs::write(&path, &text) {
                    Ok(()) => {
                        if let Some(t) = self.tabs.get_mut(index) {
                            t.dirty = false;
                            // Match the ordinary Save path: without this the
                            // tab's disk baseline stays stale, so a reopen of
                            // the same file reads as changed-on-disk.
                            t.record_clean_disk(std::path::Path::new(&path));
                        }
                        self.set_status(
                            StatusLevel::Success,
                            crate::localization::format("tabs.save.succeeded", &[("path", &path)]),
                        );
                        self.close_tab(index, cx);
                    }
                    Err(error) => {
                        self.set_status(
                            StatusLevel::Error,
                            crate::localization::format(
                                "tabs.save.failed",
                                &[("error", &error.to_string())],
                            ),
                        );
                        cx.notify();
                    }
                }
                return;
            }

            let rx = cx.prompt_for_new_path(std::path::Path::new("."), Some("query.sql"));
            let weak = cx.weak_entity();
            let async_cx = cx.to_async();
            cx.spawn(async move |_this, _cx| {
                // `Ok(Ok(Some(path)))` = a path was chosen; None = cancelled.
                if let Ok(Ok(Some(path))) = rx.await {
                    match std::fs::write(&path, &text) {
                        Ok(()) => {
                            let mut cx = async_cx.clone();
                            weak.update(&mut cx, |this, cx| {
                                // Re-locate the tab by id: it may have moved
                                // while the save dialog was open.
                                let Some(index) = this.tab_index_by_id(&tab_id) else {
                                    return;
                                };
                                if let Some(t) = this.tabs.get_mut(index) {
                                    let old_key = t.metadata_key();
                                    t.file_path = Some(path.to_string_lossy().to_string());
                                    t.dirty = false;
                                    t.record_clean_disk(&path);
                                    let new_key = t.metadata_key();
                                    if old_key != new_key {
                                        this.ai_summary_metadata.migrate(&old_key, &new_key);
                                    }
                                }
                                this.set_status(
                                    StatusLevel::Success,
                                    crate::localization::format(
                                        "tabs.save.succeeded",
                                        &[("path", &path.display().to_string())],
                                    ),
                                );
                                this.close_tab(index, cx);
                            })
                            .ok();
                        }
                        Err(error) => {
                            let mut cx = async_cx.clone();
                            weak.update(&mut cx, |this, cx| {
                                this.set_status(
                                    StatusLevel::Error,
                                    crate::localization::format(
                                        "tabs.save.failed",
                                        &[("error", &error.to_string())],
                                    ),
                                );
                                cx.notify();
                            })
                            .ok();
                        }
                    }
                }
            })
            .detach();
        }
    }

    /// Hand keyboard focus back to the active query editor.
    ///
    /// Overlay dialogs (Preferences, Connection Management, …) grab a focus
    /// handle while open. When one closes we clear its tracking field, but if
    /// focus is left pointing at the now-dropped dialog entity the root
    /// `stack`'s element-level `on_action` handlers fall out of the focus chain
    /// — so the *next* menu invocation silently no-ops until the user clicks
    /// the editor to rebuild a valid focus chain. Every dialog-close path must
    /// call this, mirroring the fix already living in the Entra dialog's
    /// `Dismissed` handler.
    pub(super) fn restore_editor_focus(&self, cx: &mut Context<Self>) {
        if let Some(tab) = self.tabs.get(self.active_tab) {
            tab.editor.update(cx, |e, cx| {
                e.request_focus(cx);
            });
        }
    }

    pub(super) fn select_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.tabs.len() {
            // Selecting the tab shown in the split's unfocused pane focuses
            // that pane instead of duplicating the tab into this one.
            if let Some(split) = &mut self.split {
                split.on_select_tab(self.active_tab, index);
            }
            self.active_tab = index;
            self.record_tab_activation(index);
            // A same-buffer split follows the active tab: re-bind its second
            // view to the newly selected tab's document.
            self.retarget_same_buffer_view(cx);
            // Sync toolbar toggle states to the newly active tab's per-tab
            // resume/diff/FIM state. Mirrors Swift `selectDocument(_:)`.
            self.sync_toolbar_to_active_tab();
            // Each tab owns its connection: focusing a tab pinned to a
            // different server/database switches the workbench to it.
            self.sync_connection_to_active_tab(cx);
            #[cfg(not(target_arch = "wasm32"))]
            self.maybe_queue_disk_prompt_for_tab(index);
            if let Some(tab) = self.tabs.get(index) {
                tab.editor.update(cx, |e, cx| {
                    e.request_focus(cx);
                });
            }
            self.save_workspace_session(cx);
            cx.notify();
        }
    }

    pub(super) fn record_tab_activation(&mut self, index: usize) {
        if let Some(id) = self.tabs.get(index).map(|t| t.id.clone()) {
            self.tab_mru.touch(&id);
            if !self.navigating_history {
                self.tab_history.push(&id);
            }
        }
    }

    pub(super) fn seed_tab_navigation(&mut self) {
        self.tab_mru
            .reconcile(self.tabs.iter().map(|t| t.id.as_str()));
        self.tab_history =
            crate::tab_history::TabHistory::new(crate::tab_history::TabHistory::DEFAULT_CAP);
        if let Some(id) = self.tabs.get(self.active_tab).map(|t| t.id.clone()) {
            self.tab_mru.touch(&id);
            self.tab_history.push(&id);
        }
    }

    /// Record the current active connection as the focused tab's pinned
    /// target. Called whenever the active connection changes (connect,
    /// database switch) so the tab remembers where it points. DDL viewer
    /// tabs are session-only surfaces and carry no pin.
    pub(super) fn stamp_active_tab_connection(&mut self) {
        let target = self
            .active_profile
            .as_ref()
            .map(|p| crate::tabs::TabConnectionTarget {
                profile_id: p.id.0,
                database: p.database.clone().filter(|d| !d.is_empty()),
            });
        if let Some(tab) = self.tabs.get_mut(self.active_tab)
            && tab.ddl_view.is_none()
        {
            tab.connection_target = target;
            tab.restored_connection_inert = false;
        }
    }

    /// Make the focused tab's pinned connection the active one. A tab with
    /// no pin adopts the current connection instead — so pre-existing and
    /// restored-legacy tabs keep today's behavior until first focus.
    pub(super) fn sync_connection_to_active_tab(&mut self, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(self.active_tab) else {
            return;
        };
        if tab.ddl_view.is_some() {
            return;
        }
        if tab.restored_connection_inert {
            return;
        }
        let Some(target) = tab.connection_target.clone() else {
            self.stamp_active_tab_connection();
            return;
        };
        let already_active = self.active_profile.as_ref().is_some_and(|p| {
            p.id.0 == target.profile_id
                && p.database.clone().filter(|d| !d.is_empty()) == target.database
        });
        if already_active {
            return;
        }
        // Resolve the pinned profile the same way explorer retargeting does
        // (sidebar's in-memory list, store fallback) and switch to it.
        let profile =
            self.resolve_explorer_target(Some(&target.profile_id), target.database.as_deref(), cx);
        if profile.is_some() {
            self.with_explorer_target_active(profile, cx, |_, _| {});
        }
    }

    pub(super) fn active_tab(&self) -> Option<&QueryTab> {
        self.tabs.get(self.active_tab)
    }

    pub(super) fn results_visible(&self) -> bool {
        self.results_pane.is_visible()
    }

    pub(super) fn show_results(&mut self) {
        self.results_pane = self.results_pane.shown();
    }

    pub(super) fn find_tab_by_editor(&self, editor: &Entity<SqlEditor>) -> Option<usize> {
        if let Some(i) = self.tabs.iter().position(|t| &t.editor == editor) {
            return Some(i);
        }
        // The same-buffer split's second view shows the active tab's
        // document, so its events belong to the active tab.
        if self.same_buffer_view.as_ref() == Some(editor) && self.active_tab < self.tabs.len() {
            return Some(self.active_tab);
        }
        None
    }

    /// The editor entity the user is editing in for `tab_idx`: normally the
    /// tab's canonical editor, but the second view when a same-buffer split
    /// has its trailing pane focused. Per-view surfaces (FIM ghost text)
    /// must land here, not blindly on `tab.editor`.
    pub(super) fn focused_editor_for_tab(&self, tab_idx: usize) -> Option<Entity<SqlEditor>> {
        if tab_idx == self.active_tab
            && let (Some(split), Some(view)) = (&self.split, &self.same_buffer_view)
            && split.is_same_buffer()
            && !split.active_first
        {
            return Some(view.clone());
        }
        self.tabs.get(tab_idx).map(|t| t.editor.clone())
    }

    /// Snapshot the tab for Reopen Closed Tab. File-backed clean tabs carry
    /// just the path (reopen re-reads the file); everything else carries the
    /// buffer text so unsaved work is one keystroke from coming back.
    pub(super) fn snapshot_closing_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        const CLOSED_TAB_CAP: usize = 10;
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        if tab.ddl_view.is_some() || tab.snapshot_only {
            return;
        }
        let content = if tab.is_file_backed() && !tab.dirty {
            None
        } else {
            Some(tab.editor.read(cx).text().to_string())
        };
        self.closed_tabs.push(ClosedTabSnapshot {
            name: tab.name.clone(),
            file_path: tab.file_path.clone(),
            content,
            connection_target: tab.connection_target.clone(),
            user_named: tab.user_named,
        });
        if self.closed_tabs.len() > CLOSED_TAB_CAP {
            self.closed_tabs.remove(0);
        }
    }

    /// Reopen the most recently closed tab (Cmd/Ctrl+Shift+T).
    pub(super) fn reopen_closed_tab(&mut self, cx: &mut Context<Self>) {
        let Some(snapshot) = self.closed_tabs.pop() else {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("tabs.reopen.none"),
            );
            cx.notify();
            return;
        };
        let content = snapshot.content.clone().or_else(|| {
            snapshot
                .file_path
                .as_deref()
                .and_then(|path| std::fs::read_to_string(path).ok())
        });
        self.new_tab(cx, content);
        if let Some(tab) = self.tabs.last_mut() {
            tab.name = snapshot.name;
            tab.file_path = snapshot.file_path;
            tab.connection_target = snapshot.connection_target;
            tab.restored_connection_inert = tab.connection_target.is_some();
            tab.user_named = snapshot.user_named;
            tab.auto_named = !tab.user_named && !tab.is_file_backed() && name_looks_auto(&tab.name);
            tab.auto_number = tab
                .auto_named
                .then(|| crate::tabs::auto_query_number(&tab.name))
                .flatten();
            tab.relocalize_auto_name();
        }
        self.save_workspace_session(cx);
        cx.notify();
    }

    pub(super) fn reorder_query_tab(&mut self, from: usize, to: usize, cx: &mut Context<Self>) {
        if apply_tab_reorder(&mut self.tabs, from, to).is_none() {
            return;
        }
        self.active_tab = remap_index_after_move(self.active_tab, from, to);
        self.tab_strip_focus_index = remap_index_after_move(self.tab_strip_focus_index, from, to);
        if let Some(drag) = self.dragging_tab {
            self.dragging_tab = Some(remap_index_after_move(drag, from, to));
        }
        self.save_workspace_session(cx);
        cx.notify();
    }

    pub(super) fn maybe_auto_name_tab(&mut self, tab_idx: usize, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(tab_idx) else {
            return;
        };
        if !should_auto_name(tab.auto_named, tab.user_named, tab.is_file_backed()) {
            return;
        }
        let sql = tab.editor.read(cx).text().to_string();
        let fallback = tab_idx + 1;
        let name = auto_name_from_sql(&sql, fallback);
        let number = u32::try_from(fallback)
            .ok()
            .filter(|number| name == crate::tabs::auto_query_title(*number));
        if let Some(tab) = self.tabs.get_mut(tab_idx) {
            tab.name = name;
            tab.auto_number = number;
        }
    }

    /// Rename by user action. Trims; ignores empty/unchanged; sets `user_named` and clears `auto_named`.
    pub(super) fn rename_tab(
        &mut self,
        tab_idx: usize,
        name: String,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(tab) = self.tabs.get_mut(tab_idx) else {
            return false;
        };
        let Some(name) = crate::tabs::apply_explicit_rename(&tab.name, &name) else {
            return false;
        };
        tab.name = name;
        tab.user_named = true;
        tab.auto_named = false;
        tab.auto_number = None;
        self.save_workspace_session(cx);
        cx.notify();
        true
    }

    pub(super) fn begin_tab_rename(&mut self, tab_idx: usize, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(tab_idx) else {
            return;
        };
        let value = tab.name.clone();
        self.overlays.tab_rename = Some(overlays::TabRenameState {
            tab_idx,
            input: None,
            value,
            focused: false,
            _subscription: None,
        });
        cx.notify();
    }

    pub(super) fn confirm_tab_rename(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.overlays.tab_rename.take() else {
            return;
        };
        self.rename_tab(state.tab_idx, state.value, cx);
    }

    pub(super) fn cancel_tab_rename(&mut self, cx: &mut Context<Self>) {
        if self.overlays.tab_rename.take().is_some() {
            cx.notify();
        }
    }

    /// Bulk close for the tab context menu (Close Others / Close to the Right /
    /// Close All). Clean tabs close immediately; dirty tabs raise ONE batch
    /// prompt (not a cascade) asking whether to close them too. Pinned tabs are
    /// always kept. Without the prompt these verbs looked dead: an untitled
    /// scratch tab is dirty from the first keystroke and stays dirty forever,
    /// so a silent keep-dirty policy kept everything.
    pub(super) fn close_tabs_where_prompting(
        &mut self,
        keep: impl Fn(usize, &crate::tabs::QueryTab) -> bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Partition up front by stable id: closing the clean victims shifts
        // indices, and the prompt is async besides.
        let (clean_ids, dirty_ids): (Vec<String>, Vec<String>) = {
            let victims: Vec<(&crate::tabs::QueryTab, bool)> = self
                .tabs
                .iter()
                .enumerate()
                .filter(|(i, tab)| !keep(*i, tab) && !tab.pinned)
                .map(|(_, tab)| (tab, tab.dirty || tab_has_staged_grid_edits(tab, cx)))
                .collect();
            (
                victims
                    .iter()
                    .filter(|(_, dirty)| !dirty)
                    .map(|(tab, _)| tab.id.clone())
                    .collect(),
                victims
                    .iter()
                    .filter(|(_, dirty)| *dirty)
                    .map(|(tab, _)| tab.id.clone())
                    .collect(),
            )
        };
        for id in &clean_ids {
            if let Some(index) = self.tab_index_by_id(id) {
                self.close_tab(index, cx);
            }
        }
        cx.notify();
        if dirty_ids.is_empty() {
            return;
        }
        let n = dirty_ids.len();
        let key = if n == 1 {
            "tabs.close_many.unsaved_singular"
        } else {
            "tabs.close_many.unsaved_plural"
        };
        let message = crate::localization::format(key, &[("count", &n.to_string())]);
        let detail = crate::localization::text("tabs.close_many.unsaved_detail");
        let close = crate::localization::text("tabs.close_many.close_button");
        let keep = crate::localization::text("tabs.close_many.keep_button");
        let rx = window.prompt(
            gpui::PromptLevel::Warning,
            &message,
            Some(&detail),
            &[
                gpui::PromptButton::new(&close),
                gpui::PromptButton::cancel(&keep),
            ],
            cx,
        );
        let weak = cx.weak_entity();
        let async_cx = cx.to_async();
        cx.spawn(async move |_this, _cx| {
            let choice = rx.await;
            if choice != Ok(0) {
                return;
            }
            let mut cx = async_cx.clone();
            weak.update(&mut cx, |this, cx| {
                // Re-resolve each tab by id: tabs may have been closed or
                // reordered while the prompt was up.
                for id in &dirty_ids {
                    if let Some(index) = this.tab_index_by_id(id) {
                        this.close_tab(index, cx);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Bulk close helpers. Dirty and pinned tabs are kept (no prompt
    /// cascades); the status line reports what was kept.
    pub(super) fn close_tabs_where(
        &mut self,
        keep: impl Fn(usize, &crate::tabs::QueryTab) -> bool,
        cx: &mut Context<Self>,
    ) {
        let victims: Vec<String> = self
            .tabs
            .iter()
            .enumerate()
            .filter(|(i, tab)| {
                !keep(*i, tab) && !tab.pinned && !tab.dirty && !tab_has_staged_grid_edits(tab, cx)
            })
            .map(|(_, tab)| tab.id.clone())
            .collect();
        let kept_dirty = self
            .tabs
            .iter()
            .enumerate()
            .filter(|(i, tab)| {
                !keep(*i, tab) && !tab.pinned && (tab.dirty || tab_has_staged_grid_edits(tab, cx))
            })
            .count();
        for id in victims {
            if let Some(index) = self.tab_index_by_id(&id) {
                self.close_tab(index, cx);
            }
        }
        if kept_dirty > 0 {
            let key = if kept_dirty == 1 {
                "tabs.close_many.kept_singular"
            } else {
                "tabs.close_many.kept_plural"
            };
            self.set_status(
                StatusLevel::Info,
                crate::localization::format(key, &[("count", &kept_dirty.to_string())]),
            );
        }
        cx.notify();
    }
}

/// Deterministic coverage of the stable-`tab_id` retargeting invariant that the
/// async close/save flow relies on. These exercise the real resolver
/// (`tab_index_of`, which `tab_index_by_id` delegates to) so the "re-resolve by
/// id, never trust a captured index" rule can't silently regress. Full driving
/// of the native Save/Save-As prompts needs a window harness that doesn't exist
/// here; the retargeting decision is the part that actually breaks, and it is
/// pure.
#[cfg(test)]
mod tab_retargeting_tests {
    use super::tab_index_of;

    fn idx(ids: &[&str], target: &str) -> Option<usize> {
        tab_index_of(ids.iter().copied(), target)
    }

    #[test]
    fn resolves_the_current_index_when_unchanged() {
        assert_eq!(idx(&["a", "b", "c"], "b"), Some(1));
    }

    #[test]
    fn target_removed_while_prompt_pending_resolves_to_none() {
        // The dirty tab "b" was closed elsewhere while its Save prompt was up:
        // the save/close must no-op, not act on a stale index.
        let after_removal = ["a", "c"];
        assert_eq!(idx(&after_removal, "b"), None);
    }

    #[test]
    fn reorder_while_prompt_pending_retargets_to_the_moved_tab() {
        // Tabs were reordered while the prompt was up; the id still finds the
        // right tab at its new index, where a captured index would have been
        // wrong.
        let before = ["a", "b", "c"];
        let after_reorder = ["c", "a", "b"];
        assert_eq!(idx(&before, "b"), Some(1));
        assert_eq!(idx(&after_reorder, "b"), Some(2));
    }

    #[test]
    fn another_tab_removed_shifts_the_target_index() {
        // Removing an earlier tab shifts the target left; id-resolution follows.
        let before = ["a", "b", "c"];
        let after = ["b", "c"]; // "a" closed
        assert_eq!(idx(&before, "c"), Some(2));
        assert_eq!(idx(&after, "c"), Some(1));
    }

    #[test]
    fn same_index_replacement_is_not_mistaken_for_the_target() {
        // The classic bug: the target "b" (index 1) is closed and a NEW tab "z"
        // now sits at index 1. A stale captured index 1 would close "z"; resolving
        // by id returns None, so nothing is closed by mistake.
        let stale_index = 1usize;
        let after = ["a", "z", "c"];
        assert_eq!(idx(&after, "b"), None, "must not retarget the replacement");
        assert_ne!(
            after.get(stale_index).copied(),
            Some("b"),
            "the stale index now points at a different tab",
        );
    }

    #[test]
    fn save_failure_keeps_the_still_present_target_resolvable() {
        // On a save failure/cancel the tab stays open; it remains resolvable so
        // the caller can leave it in place (dirty) rather than close it.
        let unchanged = ["a", "b", "c"];
        assert_eq!(idx(&unchanged, "b"), Some(1));
    }
}

/// What closing one tab must ask first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CloseTabPrompt {
    /// Nothing unsaved: close straight away.
    None,
    /// The SQL text is unsaved: Save / Don't Save / Cancel.
    UnsavedText,
    /// The results hold staged grid edits that were never saved to the
    /// database: Discard / Cancel.
    StagedGridEdits,
}

/// Staged grid edits are asked about before unsaved SQL text: Save on the
/// text prompt writes the file, never the grid edits, so they need their own
/// answer. Discarding them then falls through to the text prompt.
pub(super) fn close_tab_prompt(dirty: bool, staged_grid_edits: bool) -> CloseTabPrompt {
    if staged_grid_edits {
        CloseTabPrompt::StagedGridEdits
    } else if dirty {
        CloseTabPrompt::UnsavedText
    } else {
        CloseTabPrompt::None
    }
}

/// Whether `tab`'s results hold staged (unsaved) row edits. The tab's
/// `dirty` flag tracks only the SQL text, so close guards and the tab's
/// unsaved dot check this too.
pub(super) fn tab_has_staged_grid_edits(tab: &crate::tabs::QueryTab, cx: &App) -> bool {
    tab.results.read(cx).has_staged_changes()
}

#[cfg(test)]
mod close_tab_prompt_tests {
    use super::{CloseTabPrompt, close_tab_prompt};

    #[test]
    fn a_clean_tab_closes_without_asking() {
        assert_eq!(close_tab_prompt(false, false), CloseTabPrompt::None);
    }

    #[test]
    fn unsaved_sql_text_asks_to_save_the_file() {
        assert_eq!(close_tab_prompt(true, false), CloseTabPrompt::UnsavedText);
    }

    #[test]
    fn staged_grid_edits_ask_even_when_the_sql_text_is_saved() {
        // The SQL-text dirty flag never sees grid edits; closing must not
        // silently discard them.
        assert_eq!(
            close_tab_prompt(false, true),
            CloseTabPrompt::StagedGridEdits
        );
    }

    #[test]
    fn staged_grid_edits_are_asked_about_first() {
        // Save on the text prompt writes the file, not the grid edits.
        assert_eq!(
            close_tab_prompt(true, true),
            CloseTabPrompt::StagedGridEdits
        );
    }
}

/// Gridex #42: three query tabs opened in a row could not be closed (X or
/// right-click). Drives the real `AppView` close path: open three tabs, close
/// each through the same `request_close_tab` the X button, middle-click and
/// the context-menu "Close" row call, and check every close lands.
#[cfg(test)]
mod close_three_tabs_tests {
    use super::super::AppView;
    use gpui::{AppContext as _, TestAppContext};

    fn open_app(cx: &mut TestAppContext) -> (gpui::Entity<AppView>, &mut gpui::VisualTestContext) {
        crate::ensure_test_tokio_runtime();
        // `AppView::new` starts launch-time work (account-session read) that
        // completes on a tokio worker; allow_parking() makes that late
        // cross-thread wake harmless (same rationale as `ai_panel`).
        cx.dispatcher.allow_parking();
        cx.update(|cx| {
            if cx.try_global::<gpui_component::Theme>().is_none() {
                gpui_component::init(cx);
            }
            // Keep the startup update check off the network: a check already
            // "in flight" absorbs the launch request.
            let status = crate::update_status::handle(cx);
            status.update(cx, |s, _| {
                s.state = crate::update_status::UpdateCheckState::Checking;
            });
        });
        let (root, vcx) = cx.add_window_view(|window, cx| {
            let services = sqlly_services::Services::new();
            let app = cx.new(|cx| AppView::new(services, window, cx));
            gpui_component::Root::new(app, window, cx)
        });
        let app = vcx.update(|_, cx| {
            root.read(cx)
                .view()
                .clone()
                .downcast::<AppView>()
                .expect("root hosts the AppView")
        });
        (app, vcx)
    }

    fn ids(app: &gpui::Entity<AppView>, cx: &mut gpui::VisualTestContext) -> Vec<String> {
        cx.update(|_, cx| app.read(cx).tabs.iter().map(|t| t.id.clone()).collect())
    }

    #[gpui::test]
    fn three_new_query_tabs_each_close(cx: &mut TestAppContext) {
        let (app, cx) = open_app(cx);
        let before = ids(&app, cx).len();
        cx.update(|_, cx| {
            app.update(cx, |this, cx| {
                for _ in 0..3 {
                    this.new_tab(cx, None);
                }
            })
        });
        let opened = ids(&app, cx);
        assert_eq!(opened.len(), before + 3, "three tabs opened");
        let new_ids: Vec<String> = opened[before..].to_vec();
        for id in &new_ids {
            cx.update(|window, cx| {
                app.update(cx, |this, cx| {
                    let i = this.tab_index_by_id(id).expect("tab still open");
                    this.request_close_tab(i, window, cx);
                })
            });
            assert!(
                !ids(&app, cx).contains(id),
                "tab {id} closed through the X / context-menu path"
            );
        }
        assert_eq!(ids(&app, cx).len(), before, "all three closed");
        cx.update(|_, cx| {
            let this = app.read(cx);
            assert!(this.tabs.is_empty() || this.active_tab < this.tabs.len());
        });
    }

    /// Closing a background tab to the left of the active one must leave the
    /// same tab active, not the one that slid into its index.
    #[gpui::test]
    fn closing_a_tab_left_of_the_active_one_keeps_the_active_tab(cx: &mut TestAppContext) {
        let (app, cx) = open_app(cx);
        cx.update(|_, cx| {
            app.update(cx, |this, cx| {
                for _ in 0..4 {
                    this.new_tab(cx, None);
                }
            })
        });
        let all = ids(&app, cx);
        let n = all.len();
        assert!(n >= 4);
        // Activate the second-to-last tab, then close the first tab.
        let active_id = all[n - 2].clone();
        cx.update(|_, cx| {
            app.update(cx, |this, cx| {
                let i = this.tab_index_by_id(&active_id).unwrap();
                this.select_tab(i, cx);
            })
        });
        let first = all[0].clone();
        cx.update(|window, cx| {
            app.update(cx, |this, cx| {
                let i = this.tab_index_by_id(&first).unwrap();
                this.request_close_tab(i, window, cx);
            })
        });
        let active_after = cx.update(|_, cx| {
            let this = app.read(cx);
            this.tabs[this.active_tab].id.clone()
        });
        assert_eq!(active_after, active_id, "the active tab stays active");
    }

    /// Sequel Ace #2260 (memory is not released after closing a window): a
    /// closed tab's results view, and everything it owns, must be freed.
    #[gpui::test]
    fn closing_a_tab_releases_its_results_view(cx: &mut TestAppContext) {
        let (app, cx) = open_app(cx);
        cx.update(|_, cx| app.update(cx, |this, cx| this.new_tab(cx, Some("SELECT 1;".into()))));
        let (id, results) = cx.update(|_, cx| {
            let this = app.read(cx);
            let tab = &this.tabs[this.active_tab];
            (tab.id.clone(), tab.results.downgrade())
        });
        assert!(results.upgrade().is_some());
        cx.update(|window, cx| {
            app.update(cx, |this, cx| {
                let i = this.tab_index_by_id(&id).unwrap();
                this.request_close_tab(i, window, cx);
            })
        });
        cx.run_until_parked();
        assert!(
            results.upgrade().is_none(),
            "the closed tab's results view is still alive"
        );
    }

    /// Gridex #65 (switching between tables lags 1-2 s, even switching back).
    /// Switching tabs runs no SQL; it does save the workspace session
    /// synchronously. Time switch-backs across tabs holding sizeable scripts.
    #[gpui::test]
    fn switching_back_to_a_loaded_tab_is_fast(cx: &mut TestAppContext) {
        let (app, cx) = open_app(cx);
        let script = "SELECT * FROM dbo.Orders WHERE id = 1;\n".repeat(2_000);
        cx.update(|_, cx| {
            app.update(cx, |this, cx| {
                for _ in 0..10 {
                    this.new_tab(cx, Some(script.clone()));
                }
            })
        });
        let n = ids(&app, cx).len();
        let mut worst = std::time::Duration::ZERO;
        let mut total = std::time::Duration::ZERO;
        for k in 0..40 {
            let target = (k * 7) % n;
            let elapsed = cx.update(|_, cx| {
                app.update(cx, |this, cx| {
                    let start = std::time::Instant::now();
                    this.select_tab(target, cx);
                    start.elapsed()
                })
            });
            worst = worst.max(elapsed);
            total += elapsed;
        }
        eprintln!(
            "tab switch: {n} tabs, avg {:?}, worst {:?}",
            total / 40,
            worst
        );
        assert!(
            worst < std::time::Duration::from_millis(250),
            "a tab switch took {worst:?}"
        );
    }
}
