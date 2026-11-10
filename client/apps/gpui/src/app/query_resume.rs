use super::*;

impl AppView {
    // ---- Query resume helpers (parity with Swift QueryWorkspaceController) ----

    /// Apply a metadata record to a tab, restoring AI summaries and resume
    /// state. Mirrors Swift `loadAISummaries(from:)`. Layout/diff/FIM are
    /// gated behind `resume_effective_enabled` — when resume is OFF, those
    /// fields are NOT restored (Swift line 5353). The connection is always
    /// remembered (armed as pending) regardless of resume state.
    pub(super) fn apply_metadata_record_to_tab(
        tab: &mut QueryTab,
        record: &crate::query_ai_summary::QueryAISummaryRecord,
    ) {
        tab.ai_summary = crate::query_ai_summary::QueryAISummaryState::from_record(record);

        if let Some(ref resume) = record.resume {
            tab.resume_override_enabled = resume.override_enabled;
            tab.suppress_resume_connection_confirmation =
                resume.suppress_first_run_confirmation.unwrap_or(false);
            // Layout/diff/FIM are only restored when resume is effective ON.
            // This matches Swift `loadAISummaries` line 5353 which gates
            // these behind `queryResumeEffectiveEnabled`.
            if tab.resume_effective_enabled {
                tab.per_tab_result_layout = resume.result_layout.clone();
                tab.diff_mode_enabled = resume.diff_mode_enabled.unwrap_or(false);
                // OR, not overwrite: metadata written before the toolbar
                // toggle worked persisted `Some(false)` for every tab, so
                // treating it as authoritative would silently switch FIM off
                // for users who have it on globally. A stored `true` (an
                // explicit opt-in) always survives; a stored `false` yields
                // to the creation-time preferences seed.
                tab.fim_autocomplete_enabled = resume.fim_autocomplete_enabled.unwrap_or(false)
                    || tab.fim_autocomplete_enabled;
            }
            if let Some(ids) = resume.multi_target_ids.clone() {
                tab.last_multi_target_ids = ids;
            }
        }

        // Remember the connection (not applied yet — armed as pending for
        // deferred activation on next query). This happens regardless of
        // resume state, but the confirmation is only armed when resume is ON.
        if let Some(ref conn) = record.connection {
            tab.pending_resume_connection = Some(conn.clone());
        }
    }

    /// Recompute the effective resume state for a single tab using the current
    /// global scope and the tab's override. Mirrors Swift
    /// `recomputeQueryResumeEffectiveState()`.
    pub(super) fn recompute_resume_effective(&self, tab_idx: usize) -> bool {
        let Some(tab) = self.tabs.get(tab_idx) else {
            return false;
        };
        crate::preferences::QueryResumePolicy::effective_enabled(
            self.query_resume_prefs.scope,
            tab.is_file_backed(),
            tab.resume_override_enabled,
        )
    }

    /// Update a tab's `resume_effective_enabled` from the policy and sync the
    /// toolbar toggle for the active tab.
    pub(super) fn update_resume_effective_state(&mut self, tab_idx: usize, cx: &mut Context<Self>) {
        let enabled = self.recompute_resume_effective(tab_idx);
        let was_enabled = self
            .tabs
            .get(tab_idx)
            .map(|t| t.resume_effective_enabled)
            .unwrap_or(false);
        if let Some(tab) = self.tabs.get_mut(tab_idx) {
            tab.resume_effective_enabled = enabled;
        }
        // Re-arm or drop pending connection confirmation when state changed.
        if enabled != was_enabled {
            self.arm_or_drop_pending_connection(tab_idx, enabled);
        }
        // Sync toolbar toggle if this is the active tab.
        if tab_idx == self.active_tab {
            self.toolbar.query_resume = enabled;
        }
        cx.notify();
    }

    /// Arm or drop the pending resume connection confirmation based on the
    /// effective resume state. Mirrors the Swift logic in
    /// `queryResumeToggleChanged` and `loadAISummaries`.
    pub(super) fn arm_or_drop_pending_connection(&mut self, tab_idx: usize, resume_enabled: bool) {
        if let Some(tab) = self.tabs.get_mut(tab_idx) {
            tab.has_pending_resume_confirmation =
                resume_enabled && tab.pending_resume_connection.is_some();
        }
    }

    /// Toggle the per-tab resume override. Flips the override against the
    /// current effective state. Mirrors Swift `toggleQueryResumeOverride()`.
    pub(super) fn toggle_resume_override(&mut self, cx: &mut Context<Self>) {
        let tab_idx = self.active_tab;
        // Guard: None scope disables the toggle entirely.
        if self.query_resume_prefs.scope == crate::preferences::QueryResumeScope::None {
            return;
        }
        let current_effective = self
            .tabs
            .get(tab_idx)
            .map(|t| t.resume_effective_enabled)
            .unwrap_or(false);
        if let Some(tab) = self.tabs.get_mut(tab_idx) {
            tab.resume_override_enabled = Some(!current_effective);
        }
        self.update_resume_effective_state(tab_idx, cx);
        self.flush_tab_resume_metadata(tab_idx);
    }

    /// Flush the full record (summaries + connection + resume) for a tab to
    /// the metadata store and persist to disk. Mirrors Swift
    /// `flushQueryResumeDocumentMetadata()`. Uses the tab's remembered
    /// connection when available, falling back to the active connection.
    pub(super) fn flush_tab_resume_metadata(&mut self, tab_idx: usize) {
        let Some(tab) = self.tabs.get(tab_idx) else {
            return;
        };
        // Prefer the tab's remembered connection; fall back to the active
        // connection. This ensures per-tab connection metadata survives
        // even when the active connection is different.
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

    /// The engine type of the active connection, for rendering SQL with that
    /// engine's pattern equivalents (row limiting, identifier quoting).
    /// Defaults to SQL Server when nothing is connected.
    pub(super) fn active_db_type(&self) -> sqlly_lang::sqlmodel::DatabaseType {
        self.active_profile
            .as_ref()
            .map(|p| p.database_type)
            .unwrap_or_default()
    }

    /// Snapshot the active connection as `QueryConnectionMetadata`.
    pub(super) fn active_connection_metadata(
        &self,
    ) -> Option<crate::query_ai_summary::QueryConnectionMetadata> {
        self.active_profile
            .as_ref()
            .map(|p| crate::query_ai_summary::QueryConnectionMetadata {
                server: p.server.clone(),
                database: p.database.clone(),
            })
    }

    /// Build a dynamic tooltip for the resume toggle based on the current
    /// scope, override, and effective state. Mirrors Swift
    /// `refreshQueryResumeButton()` tooltip logic.
    pub(super) fn resume_toggle_tooltip(&self) -> String {
        let scope = self.query_resume_prefs.scope;
        if scope == crate::preferences::QueryResumeScope::None {
            return crate::localization::format(
                "shell.resume.disabled_scope_tooltip",
                &[("scope", scope.display_title())],
            );
        }
        let tab = self.tabs.get(self.active_tab);
        let effective = tab.map(|t| t.resume_effective_enabled).unwrap_or(false);
        let override_val = tab.and_then(|t| t.resume_override_enabled);
        // An override that has not taken effect yet shows the plain tooltip.
        if matches!(
            (override_val, effective),
            (Some(true), false) | (Some(false), true)
        ) {
            return crate::localization::text("toolbar.toggle.query_resume.tooltip");
        }
        let key = match (override_val, effective) {
            (Some(true), _) => "shell.resume.forced_on_tooltip",
            (Some(false), _) => "shell.resume.forced_off_tooltip",
            (None, true) => "shell.resume.following_on_tooltip",
            (None, false) => "shell.resume.following_off_tooltip",
        };
        crate::localization::format(key, &[("scope", scope.display_title())])
    }

    /// Clear pending resume connection confirmation for the active tab when
    /// the user manually connects to a database. Mirrors Swift behavior where
    /// manually connecting dismisses any pending resume-connection prompt.
    pub(super) fn clear_pending_resume_on_manual_connect(&mut self) {
        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
            tab.has_pending_resume_confirmation = false;
        }
        // Also dismiss any armed confirmation dialog.
        self.pending.resume_confirmation = None;
    }

    /// Recompute the resume effective state for a tab after its file backing
    /// changes (file saved or opened). Mirrors Swift
    /// `handleFileBackingChanged()` — the default-enabled state depends on
    /// `isFileBacked`, so a FileOnly-scope tab that becomes file-backed
    /// transitions from OFF to ON, and vice versa.
    pub(super) fn handle_file_backing_changed(&mut self, tab_idx: usize, cx: &mut Context<Self>) {
        self.update_resume_effective_state(tab_idx, cx);
        #[cfg(not(target_arch = "wasm32"))]
        {
            if let Some(tab) = self.tabs.get_mut(tab_idx)
                && tab.is_watchable()
                && tab.disk_snapshot.is_none()
                && let Some(path) = tab.file_path.clone()
            {
                tab.record_clean_disk(std::path::Path::new(&path));
            }
            self.ensure_file_watch_ticker(cx);
        }
    }

    /// Recompute resume effective state for all tabs (used when the scope
    /// changes in preferences). Mirrors Swift `applyQueryResumePreferences()`.
    /// Tabs whose effective state changed are flushed to disk immediately.
    pub(super) fn apply_resume_prefs_to_all_tabs(&mut self, cx: &mut Context<Self>) {
        let new_scope = self.query_resume_prefs.scope;
        let mut changed_indices: Vec<usize> = Vec::new();
        for i in 0..self.tabs.len() {
            let enabled = self.recompute_resume_effective(i);
            let was_enabled = self
                .tabs
                .get(i)
                .map(|t| t.resume_effective_enabled)
                .unwrap_or(false);
            if let Some(tab) = self.tabs.get_mut(i) {
                tab.resume_effective_enabled = enabled;
            }
            if enabled != was_enabled {
                changed_indices.push(i);
                self.arm_or_drop_pending_connection(i, enabled);
            }
        }
        // Sync toolbar for the active tab.
        if let Some(tab) = self.tabs.get(self.active_tab) {
            self.toolbar.query_resume = tab.resume_effective_enabled;
        }
        // Flush metadata for tabs whose effective state changed.
        for &i in &changed_indices {
            self.flush_tab_resume_metadata(i);
        }
        if !changed_indices.is_empty() {
            cx.notify();
        }
        tracing::info!(
            "Applied query resume scope {:?} to {} tabs ({} changed)",
            new_scope,
            self.tabs.len(),
            changed_indices.len(),
        );
    }
}
