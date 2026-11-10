use super::*;

impl AppView {
    // ── Query version history ────────────────────────────────────────────

    /// The tab-id component of a history store key: untitled tabs are scoped
    /// to this window's session id so windows never share a store.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn history_tab_id(session_id: &str, tab: &crate::tabs::QueryTab) -> String {
        if tab.is_file_backed() {
            tab.id.clone()
        } else {
            format!("{}-{}", session_id, tab.id)
        }
    }

    /// Open (lazily) the version store for a tab. Keyed by file-path hash for
    /// file-backed tabs (history survives sessions) and by tab id for
    /// untitled tabs (history is purged when the tab closes).
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn history_store_for_tab(
        &mut self,
        tab_idx: usize,
    ) -> Option<&mut crate::query_version_history::VersionStore> {
        if !self.query_history_prefs.enabled {
            return None;
        }
        let key = {
            let tab = self.tabs.get(tab_idx)?;
            let tab_id = Self::history_tab_id(&self.history_session_id, tab);
            crate::query_version_history::VersionStore::store_key(&tab_id, tab.file_path.as_deref())
        };
        if !self.history_stores.contains_key(&key) {
            let dir = crate::query_version_history::VersionStore::base_dir().join(&key);
            let store = crate::query_version_history::VersionStore::open(
                dir,
                self.query_history_prefs.caps(),
            )
            .ok()?;
            self.history_stores.insert(key.clone(), store);
        }
        self.history_stores.get_mut(&key)
    }

    /// Debounced idle edit-seal: each keystroke bumps the tab's token and a
    /// timer fires `idle_seal_secs` later; if the token still matches (no
    /// further edits), the current text seals as an Edit version. The store
    /// dedupes identical consecutive text, so this is cheap to over-schedule.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn schedule_history_seal(&mut self, tab_idx: usize, cx: &mut Context<Self>) {
        if !self.query_history_prefs.enabled {
            return;
        }
        let Some(tab) = self.tabs.get_mut(tab_idx) else {
            return;
        };
        tab.history_seal_token += 1;
        let token = tab.history_seal_token;
        let delay = self.query_history_prefs.idle_seal_secs();
        let weak_self = cx.weak_entity();
        let mut async_cx = cx.to_async();
        cx.spawn(async move |_this, _cx| {
            async_cx
                .background_executor()
                .timer(std::time::Duration::from_secs(delay))
                .await;
            weak_self
                .update(&mut async_cx, |this, cx| {
                    let Some(tab) = this.tabs.get(tab_idx) else {
                        return;
                    };
                    if tab.history_seal_token != token {
                        return;
                    }
                    this.seal_edit_version(tab_idx, cx);
                })
                .ok();
        })
        .detach();
    }

    /// Seal the tab's current editor text as an Edit version (no-op when the
    /// text is empty or differs from the last sealed version only in
    /// whitespace).
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn seal_edit_version(&mut self, tab_idx: usize, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(tab_idx) else {
            return;
        };
        let sql = tab.editor.read(cx).text().to_string();
        if sql.trim().is_empty() {
            return;
        }
        let now = chrono::Utc::now().timestamp_millis();
        if let Some(store) = self.history_store_for_tab(tab_idx) {
            let _ = store.seal(crate::query_version_history::VersionKind::Edit, &sql, now);
        }
        self.refresh_history_panel(tab_idx, cx);
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn refresh_history_panel(&mut self, tab_idx: usize, cx: &mut Context<Self>) {
        let results_enabled = self.query_history_prefs.store_results;
        if let Some(tab) = self.tabs.get(tab_idx)
            && let Some(panel) = &tab.history_panel
        {
            panel.update(cx, |p, cx| p.refresh(results_enabled, cx));
        }
    }

    /// Hang the completed run's result snapshot on the version sealed at
    /// dispatch (when result storage is enabled).
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn attach_run_results(
        &mut self,
        results: &Entity<ResultsView>,
        cx: &mut Context<Self>,
    ) {
        let Some(tab_idx) = self.tabs.iter().position(|t| &t.results == results) else {
            return;
        };
        let Some(version_id) = self
            .tabs
            .get_mut(tab_idx)
            .and_then(|t| t.pending_run_version_id.take())
        else {
            return;
        };
        if !self.query_history_prefs.enabled || !self.query_history_prefs.store_results {
            return;
        }
        let sets = results.read(cx).snapshot_sets(cx);
        if sets.is_empty() {
            return;
        }
        let engine = results.read(cx).results_engine();
        if let Some(store) = self.history_store_for_tab(tab_idx) {
            let _ = store.attach_results(&version_id, &sets, engine);
        }
        self.refresh_history_panel(tab_idx, cx);
    }

    /// Restore a version into the tab: the current editor text is sealed
    /// first (restore is never lossy), then the version's SQL loads into the
    /// editor and its stored results (when present) render read-only under
    /// the historical-results banner.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn restore_history_version(
        &mut self,
        tab_idx: usize,
        version_id: String,
        cx: &mut Context<Self>,
    ) {
        let entry = self
            .history_store_for_tab(tab_idx)
            .and_then(|store| store.get(&version_id).cloned());
        let Some(entry) = entry else {
            // Evicted or deleted between the click and the restore.
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.history.version_unavailable"),
            );
            self.refresh_history_panel(tab_idx, cx);
            cx.notify();
            return;
        };
        self.seal_edit_version(tab_idx, cx);
        let has_results = entry.results.is_some();
        let sealed_at = entry.sealed_at_unix_ms;
        let proc_dialect = self.active_proc_dialect();
        if let Some(tab) = self.tabs.get_mut(tab_idx) {
            tab.proc_mode
                .refresh_from_script_for(&entry.sql, proc_dialect);
            tab.editor
                .update(cx, |e, cx| e.replace_document(entry.sql.clone(), cx));
        }
        // Persist the restored text like typed edits are: without this, a
        // restore followed by an unclean exit (before any structural event)
        // would reopen with the pre-restore buffer.
        self.schedule_session_autosave(cx);
        // `replace_document` restores the version as ONE undoable edit (Ctrl+Z
        // brings back the pre-restore text) without wiping the tab's undo
        // history. Bump the seal token so any pending idle-seal timer does not
        // re-seal the restored text as a duplicate Edit version.
        if let Some(tab) = self.tabs.get_mut(tab_idx) {
            tab.history_seal_token += 1;
            tab.history_cursor = Some(version_id.clone());
            tab.dirty = true;
        }
        if has_results {
            let stored = self.history_store_for_tab(tab_idx).and_then(|store| {
                let sets = store.load_results(&version_id).ok().flatten()?;
                Some((sets, store.results_engine(&version_id)))
            });
            let (sets, engine) = match stored {
                Some((sets, engine)) => (Some(sets), engine),
                None => (None, None),
            };
            match (self.tabs.get(tab_idx), sets) {
                (Some(tab), Some(sets)) => {
                    let banner = crate::localization::format(
                        "shell.history.viewing_results_banner",
                        &[(
                            "timestamp",
                            &crate::history_panel::HistoryPanelView::format_time(sealed_at),
                        )],
                    );
                    // The engine the results ran on, as the version
                    // recorded it (the tab may be connected elsewhere
                    // now); unknown for versions stored before that.
                    tab.results.update(cx, |r, cx| {
                        r.set_results_engine(engine);
                        r.set_historical_results(banner, sets, cx)
                    });
                }
                _ => {
                    // Payload missing or undecodable: the SQL restore still
                    // succeeded, so say what was skipped rather than failing
                    // silently.
                    self.set_status(
                        StatusLevel::Warning,
                        crate::localization::text("shell.history.results_unavailable_sql_restored"),
                    );
                }
            }
        }
        let results_enabled = self.query_history_prefs.store_results;
        if let Some(tab) = self.tabs.get(tab_idx)
            && let Some(panel) = &tab.history_panel
        {
            panel.update(cx, |p, cx| {
                p.set_current_version(Some(version_id.clone()), cx);
                p.refresh(results_enabled, cx);
            });
        }
        cx.notify();
    }

    /// Step the tab's history cursor one version older / newer, restoring as
    /// it goes. With no cursor, "older" starts at the newest version.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn step_history(&mut self, tab_idx: usize, older: bool, cx: &mut Context<Self>) {
        if !self.query_history_prefs.enabled {
            return;
        }
        let versions = self
            .history_store_for_tab(tab_idx)
            .map(|store| store.versions().to_vec())
            .unwrap_or_default();
        if versions.is_empty() {
            return;
        }
        let cursor = self
            .tabs
            .get(tab_idx)
            .and_then(|t| t.history_cursor.clone());
        let idx = match cursor.as_deref() {
            Some(cur) => versions
                .iter()
                .position(|v| v.id == cur)
                .unwrap_or(versions.len()),
            None => versions.len(),
        };
        let target = if older {
            idx.checked_sub(1)
        } else if idx + 1 < versions.len() {
            Some(idx + 1)
        } else {
            None
        };
        if let Some(t) = target {
            let id = versions[t].id.clone();
            self.restore_history_version(tab_idx, id, cx);
        }
    }

    /// Create the per-tab history panel if needed. Returns whether it exists.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn ensure_history_panel(&mut self, tab_idx: usize, cx: &mut Context<Self>) -> bool {
        let enabled = self.query_history_prefs.enabled;
        let show_git = self.query_history_prefs.show_git_history;
        let Some(tab) = self.tabs.get(tab_idx) else {
            return false;
        };
        if tab.history_panel.is_some() {
            return true;
        }
        if !enabled {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.history.disabled"),
            );
            cx.notify();
            return false;
        }
        let key = {
            let tab_id = Self::history_tab_id(&self.history_session_id, tab);
            crate::query_version_history::VersionStore::store_key(&tab_id, tab.file_path.as_deref())
        };
        let file_path = tab.file_path.clone();
        let show_git = show_git && file_path.is_some();
        let results_enabled = self.query_history_prefs.store_results;
        let panel = cx.new(|cx| {
            crate::history_panel::HistoryPanelView::new(
                key,
                file_path,
                show_git,
                results_enabled,
                cx,
            )
        });
        cx.subscribe(&panel, Self::handle_history_panel_event)
            .detach();
        if let Some(tab) = self.tabs.get_mut(tab_idx) {
            tab.history_panel = Some(panel);
        }
        true
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn toggle_history_panel(&mut self, tab_idx: usize, cx: &mut Context<Self>) {
        let enabled = self.query_history_prefs.enabled;
        let show_git = self.query_history_prefs.show_git_history;
        let Some(tab) = self.tabs.get_mut(tab_idx) else {
            return;
        };
        if tab.history_panel.is_some() {
            tab.history_panel = None;
            tab.history_split = false;
            cx.notify();
            return;
        }
        if !enabled {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.history.toggle_disabled"),
            );
            cx.notify();
            return;
        }
        let key = {
            let tab_id = Self::history_tab_id(&self.history_session_id, tab);
            crate::query_version_history::VersionStore::store_key(&tab_id, tab.file_path.as_deref())
        };
        let file_path = tab.file_path.clone();
        let show_git = show_git && file_path.is_some();
        let results_enabled = self.query_history_prefs.store_results;
        let panel = cx.new(|cx| {
            crate::history_panel::HistoryPanelView::new(
                key,
                file_path,
                show_git,
                results_enabled,
                cx,
            )
        });
        cx.subscribe(&panel, Self::handle_history_panel_event)
            .detach();
        if let Some(tab) = self.tabs.get_mut(tab_idx) {
            tab.history_panel = Some(panel);
            tab.history_split = true;
        }
        cx.notify();
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn handle_history_panel_event(
        &mut self,
        panel: Entity<crate::history_panel::HistoryPanelView>,
        event: &crate::history_panel::HistoryPanelEvent,
        cx: &mut Context<Self>,
    ) {
        let Some(tab_idx) = self
            .tabs
            .iter()
            .position(|t| t.history_panel.as_ref() == Some(&panel))
        else {
            return;
        };
        match event {
            crate::history_panel::HistoryPanelEvent::RestoreRequested { version_id } => {
                self.restore_history_version(tab_idx, version_id.clone(), cx);
            }
            crate::history_panel::HistoryPanelEvent::DeleteRequested { version_id } => {
                if let Some(store) = self.history_store_for_tab(tab_idx) {
                    let _ = store.delete_version(version_id);
                }
                self.refresh_history_panel(tab_idx, cx);
            }
            crate::history_panel::HistoryPanelEvent::CompareRequested { version_id } => {
                // Diff the sealed version's SQL against the live buffer and
                // open the result as a read-only unified diff in a new tab.
                let version_sql = self.history_store_for_tab(tab_idx).and_then(|store| {
                    store
                        .versions()
                        .iter()
                        .find(|entry| entry.id == *version_id)
                        .map(|entry| entry.sql.clone())
                });
                let Some(version_sql) = version_sql else {
                    self.set_status(
                        StatusLevel::Warning,
                        crate::localization::text("shell.history.version_not_found"),
                    );
                    cx.notify();
                    return;
                };
                let current = self
                    .tabs
                    .get(tab_idx)
                    .map(|tab| tab.editor.read(cx).text().to_string())
                    .unwrap_or_default();
                let diff = crate::history_panel::diff_lines_capped(
                    &version_sql,
                    &current,
                    crate::history_panel::DIFF_LINE_CAP,
                );
                let mut text = crate::localization::text("shell.history.diff_header");
                text.push_str("\n\n");
                for line in &diff.lines {
                    let prefix = match line.kind {
                        crate::history_panel::DiffKind::Added => "+ ",
                        crate::history_panel::DiffKind::Removed => "- ",
                        crate::history_panel::DiffKind::Same => "  ",
                    };
                    text.push_str(prefix);
                    text.push_str(&line.text);
                    text.push('\n');
                }
                if diff.truncated {
                    text.push('\n');
                    text.push_str(&crate::localization::text("shell.history.diff_truncated"));
                    text.push('\n');
                }
                self.new_tab(cx, Some(text));
                if let Some(tab) = self.tabs.last_mut() {
                    tab.name = crate::localization::text("shell.history.diff_tab_title");
                    tab.dirty = false;
                }
                cx.notify();
            }
            crate::history_panel::HistoryPanelEvent::Closed => {
                if let Some(tab) = self.tabs.get_mut(tab_idx) {
                    tab.history_panel = None;
                    tab.history_split = false;
                }
                cx.notify();
            }
        }
    }

    pub(super) fn handle_toggle_history_panel_action(
        &mut self,
        _action: &ToggleHistoryPanel,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let active = self.active_tab;
            self.toggle_history_panel(active, cx);
        }
        #[cfg(target_arch = "wasm32")]
        let _ = cx;
    }

    pub(super) fn handle_history_prev_version_action(
        &mut self,
        _action: &HistoryPrevVersion,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let active = self.active_tab;
            self.step_history(active, true, cx);
        }
        #[cfg(target_arch = "wasm32")]
        let _ = cx;
    }

    pub(super) fn handle_history_next_version_action(
        &mut self,
        _action: &HistoryNextVersion,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let active = self.active_tab;
            self.step_history(active, false, cx);
        }
        #[cfg(target_arch = "wasm32")]
        let _ = cx;
    }
}
