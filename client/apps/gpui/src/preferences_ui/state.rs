//! Store-field setters and small state mutators shared by the panes.
//!
//! Split from `preferences_ui.rs`; code moved verbatim.

use super::*;

impl PreferencesWindow {
    /// Persist the dialog's UI state so the next open restores it exactly.
    pub(super) fn save_ui_state(&self) {
        PreferencesUiState {
            selected_pane: Some(self.selected_pane.clone()),
            expanded: self.expanded.iter().cloned().collect(),
            sidebar_scroll_y: f32::from(self.sidebar_scroll.offset().y),
            pane_scroll_y: f32::from(self.pane_scroll.offset().y),
        }
        .save();
    }

    pub(super) fn apply_search(&mut self, query: String, cx: &mut Context<Self>) {
        let was_searching = !self.search_query.trim().is_empty();
        self.search_query = query;
        if self.search_query.trim().is_empty() {
            // Search cleared: show the whole tree again, keep whatever is
            // selected, make sure its folders are open, and scroll the
            // selected row back into view. The pane content stays put.
            self.filtered_tree = self.tree.clone();
            self.expand_to_selected();
            self.pane_reveal_pending.set(false);
            if was_searching {
                self.sidebar_reveal_pending.set(true);
                self.mark_search_index_stale();
            }
        } else {
            // The index is rebuilt off the keystroke path (after open, on
            // leaving a pane, on clearing the search), so it already holds
            // values edited this session. The filter itself runs in `render`
            // (`refresh_search`), which has the window the index pass needs.
            self.search_dirty = true;
        }
        cx.notify();
    }

    /// Open or close a sidebar folder.
    pub(super) fn toggle_expand(&mut self, title: &str, cx: &mut Context<Self>) {
        if !self.expanded.remove(title) {
            self.expanded.insert(title.to_string());
        }
        cx.notify();
        self.save_ui_state();
    }

    pub fn select_pane(&mut self, pane: PreferencesPane, cx: &mut Context<Self>) {
        if pane == PreferencesPane::Keybindings {
            self.ensure_keybindings_model();
        }
        if pane == PreferencesPane::ScheduledDumps {
            // Opening the pane re-reads the store once (a file fixed by hand
            // shows up); renders in between use the cached copy.
            self.invalidate_scheduled_dumps_cache();
        }
        if pane == PreferencesPane::DdlNavigation {
            // Monitors can be plugged/unplugged while the app runs; refresh
            // the cached list when the pane opens (not every render).
            self.monitors = crate::ddl_navigation::detect_monitors(cx);
        }
        // A newly opened pane starts at its top; only a reopen of the dialog
        // restores a saved scroll position.
        if self.selected_pane != pane {
            self.pane_scroll.set_offset(point(px(0.), px(0.)));
            self.mark_search_index_stale();
        }
        self.selected_pane = pane;
        // Callers include menu deep-links (⌘K shortcuts, MCP settings) that
        // land on a pane whose folder may be collapsed and off-screen: open
        // the folders above it and scroll its row into view. For an ordinary
        // sidebar click both are no-ops — the row was already visible.
        self.expand_to_selected();
        self.sidebar_reveal_pending.set(true);
        // A half-confirmed destructive action must not wait around for you to
        // come back to the pane and click something that used to be safe.
        self.reset_all_armed = false;
        self.cache_clear_all_armed = false;
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.result_cache_clear_armed = false;
        }
        cx.notify();
        self.save_ui_state();
    }

    /// Save UI state, hand focus back to whatever had it before the dialog
    /// opened, and tell the app we're done. Every close path (Close button,
    /// title-bar ✕, scrim click, Esc) goes through here.
    pub(super) fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.save_ui_state();
        if let Some(previous) = self.previous_focus.take() {
            window.focus(&previous, cx);
        } else {
            // Nothing was focused before the dialog opened. Blur explicitly:
            // leaving the window's focus pointing at this soon-dead handle
            // kills action dispatch (⌘, and the Settings… menu item silently
            // stop working) until the user happens to click something
            // focusable.
            window.blur(cx);
        }
        cx.emit(PreferencesWindowEvent::Closed);
    }

    // ---- editor preferences helpers ----

    pub(super) fn set_editor_field<F>(&self, cx: &mut Context<Self>, f: F)
    where
        F: FnOnce(&mut preferences::EditorPreferences),
    {
        let mut e = self.store.editor();
        f(&mut e);
        self.store.set_editor(e);
        cx.notify();
    }

    pub(super) fn set_rf_field<F>(&self, cx: &mut Context<Self>, f: F)
    where
        F: FnOnce(&mut preferences::ResultFormattingPreferences),
    {
        let mut r = self.store.result_formatting();
        f(&mut r);
        self.store.set_result_formatting(r);
        cx.notify();
    }

    pub(super) fn set_masking_field<F>(&self, cx: &mut Context<Self>, f: F)
    where
        F: FnOnce(&mut crate::masking_rules::MaskingRules),
    {
        let mut m = self.store.masking_rules();
        f(&mut m);
        self.store.set_masking_rules(m);
        cx.notify();
    }

    pub(super) fn set_export_field<F>(&self, cx: &mut Context<Self>, f: F)
    where
        F: FnOnce(&mut preferences::ExportPreferences),
    {
        let mut e = self.store.export();
        f(&mut e);
        self.store.set_export(e);
        cx.notify();
    }

    pub(super) fn set_export_options_field<F>(&self, cx: &mut Context<Self>, f: F)
    where
        F: FnOnce(&mut preferences::ExportOptionsPreferences),
    {
        let mut o = preferences::ExportOptionsPreferences::load();
        f(&mut o);
        o.save();
        cx.notify();
    }

    pub(super) fn set_performance_field<F>(&self, cx: &mut Context<Self>, f: F)
    where
        F: FnOnce(&mut preferences::PerformancePreferences),
    {
        let mut p = self.store.performance();
        f(&mut p);
        self.store.set_performance(p);
        cx.notify();
    }

    pub(super) fn set_tab_field<F>(&self, cx: &mut Context<Self>, f: F)
    where
        F: FnOnce(&mut preferences::TabPreferences),
    {
        let mut t = self.store.tabs();
        f(&mut t);
        self.store.set_tabs(t);
        cx.notify();
    }

    /// Routes a numeric text field to a `PerformancePreferences` field.
    ///
    /// The old version silently swallowed anything unparseable: you could type
    /// `abc` into the spill-row threshold, watch the field accept it, and the
    /// store would quietly keep the previous number with nothing on screen
    /// saying so. A field that ignores you is worse than one that refuses you,
    /// so a bad value now records an error the pane renders next to the input.
    /// An empty field is mid-edit, not invalid — it clears the error and writes
    /// nothing.
    pub(super) fn set_performance_usize(
        &mut self,
        id: &'static str,
        text: &str,
        cx: &mut Context<Self>,
        f: fn(&mut preferences::PerformancePreferences, usize),
    ) {
        match parse_field::<usize>(text) {
            FieldParse::Value(value) => {
                self.field_errors.remove(id);
                self.set_performance_field(cx, |p| f(p, value));
            }
            FieldParse::Empty => {
                self.field_errors.remove(id);
                cx.notify();
            }
            FieldParse::Invalid => {
                self.field_errors.insert(
                    id,
                    crate::localization::text("settings.state.static.enter_a_whole_number")
                        .to_string(),
                );
                cx.notify();
            }
        }
    }

    /// Write one `QueryHistoryPreferences` field and persist the set.
    pub(super) fn set_query_history_field<F>(&self, cx: &mut Context<Self>, f: F)
    where
        F: FnOnce(&mut preferences::QueryHistoryPreferences),
    {
        let mut p = self.store.query_history();
        f(&mut p);
        self.store.set_query_history(p);
        cx.notify();
    }

    pub(super) fn set_sql_library_field<F>(&self, cx: &mut Context<Self>, f: F)
    where
        F: FnOnce(&mut preferences::SqlLibraryPreferences),
    {
        let mut p = self.store.sql_library();
        f(&mut p);
        self.store.set_sql_library(p);
        cx.notify();
    }

    /// Routes a numeric text field to a `QueryHistoryPreferences` field, with
    /// the same empty/invalid treatment as the performance fields.
    pub(super) fn set_query_history_usize(
        &mut self,
        id: &'static str,
        text: &str,
        cx: &mut Context<Self>,
        f: fn(&mut preferences::QueryHistoryPreferences, usize),
    ) {
        match parse_field::<usize>(text) {
            FieldParse::Value(value) => {
                self.field_errors.remove(id);
                self.set_query_history_field(cx, |p| f(p, value));
            }
            FieldParse::Empty => {
                self.field_errors.remove(id);
                cx.notify();
            }
            FieldParse::Invalid => {
                self.field_errors.insert(
                    id,
                    crate::localization::text("settings.state.static.enter_a_whole_number_2")
                        .to_string(),
                );
                cx.notify();
            }
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn set_result_cache_field<F>(&self, cx: &mut Context<Self>, f: F)
    where
        F: FnOnce(&mut preferences::ResultCachePreferences),
    {
        let mut p = self.store.result_cache();
        let was_enabled = p.enabled;
        f(&mut p);
        if was_enabled
            && !p.enabled
            && let Err(error) = crate::result_cache::ResultCache::default_store().clear_all()
        {
            tracing::warn!(%error, "failed to clear result cache after disable");
        }
        self.store.set_result_cache(p);
        cx.notify();
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn set_result_cache_usize(
        &mut self,
        id: &'static str,
        text: &str,
        cx: &mut Context<Self>,
        f: fn(&mut preferences::ResultCachePreferences, usize),
    ) {
        match parse_field::<usize>(text) {
            FieldParse::Value(value) => {
                self.field_errors.remove(id);
                self.set_result_cache_field(cx, |p| f(p, value));
            }
            FieldParse::Empty => {
                self.field_errors.remove(id);
                cx.notify();
            }
            FieldParse::Invalid => {
                self.field_errors.insert(
                    id,
                    crate::localization::text("settings.state.static.enter_a_whole_number_3")
                        .to_string(),
                );
                cx.notify();
            }
        }
    }

    pub(super) fn set_transaction_field<F>(&self, cx: &mut Context<Self>, f: F)
    where
        F: FnOnce(&mut preferences::TransactionPreferences),
    {
        let mut p = self.store.transaction();
        f(&mut p);
        self.store.set_transaction(p);
        cx.notify();
    }

    pub(super) fn set_transaction_usize(
        &mut self,
        id: &'static str,
        text: &str,
        cx: &mut Context<Self>,
        f: fn(&mut preferences::TransactionPreferences, usize),
    ) {
        match parse_field::<usize>(text) {
            FieldParse::Value(value) => {
                self.field_errors.remove(id);
                self.set_transaction_field(cx, |p| f(p, value));
            }
            FieldParse::Empty => {
                self.field_errors.remove(id);
                cx.notify();
            }
            FieldParse::Invalid => {
                self.field_errors.insert(
                    id,
                    crate::localization::text("settings.state.static.enter_a_whole_number_4")
                        .to_string(),
                );
                cx.notify();
            }
        }
    }

    // ---- SQL formatting helpers ----

    /// Write one SQL-formatting field and mark the set as `Custom`.
    ///
    /// The preset is a *description* of the current values, so any hand edit
    /// makes the old preset name a lie. Setting `Custom` here is what keeps the
    /// Preset dropdown honest — it is not bookkeeping, it is the invariant.
    pub(super) fn set_sql_field<F>(&self, cx: &mut Context<Self>, f: F)
    where
        F: FnOnce(&mut preferences::SqlFormattingPreferences),
    {
        self.store
            .set_sql_formatting(sql_edited(self.store.sql_formatting(), f));
        cx.notify();
    }

    /// Replace the whole set from a preset. Deliberately does not go through
    /// [`Self::set_sql_field`], which would immediately stamp `Custom` over the
    /// preset the user just picked.
    pub(super) fn apply_sql_preset(
        &mut self,
        preset: Preset,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut s = preferences::SqlFormattingPreferences::for_preset(preset);
        s.preset = preset;
        // A preset is a layout style; when formatting runs is the user's
        // separate choice and survives switching presets.
        let current = self.store.sql_formatting();
        s.format_as_you_go = current.format_as_you_go;
        s.format_on_save = current.format_on_save;
        s.format_on_paste = current.format_on_paste;
        self.store.set_sql_formatting(s);
        self.sync_sql_text_fields(window, cx);
        cx.notify();
    }

    /// Re-seed the SQL free-text inputs from the store. Needed after a preset
    /// rewrite, or the text fields would keep showing the old numbers while the
    /// store holds new ones.
    pub(super) fn sync_sql_text_fields(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let s = self.store.sql_formatting();
        self.set_input_value(
            "sql_line_len",
            &s.max_line_length_raw.to_string(),
            window,
            cx,
        );
        self.set_input_value(
            "sql_short_len",
            &s.short_query_max_length_raw.to_string(),
            window,
            cx,
        );
        self.field_errors.remove("sql_line_len");
        self.field_errors.remove("sql_short_len");
    }

    /// Push a value into a text input programmatically (reset, preset apply,
    /// key saved/removed). The `InputEvent::Change` this emits is reserved in
    /// `suppressed_input_changes` so the field's apply logic only ever runs
    /// for user edits.
    pub(super) fn set_input_value(
        &mut self,
        id: &'static str,
        value: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.inputs.get(id).cloned() else {
            return;
        };
        *self.suppressed_input_changes.entry(id).or_insert(0) += 1;
        state.set_value(value.to_string(), window, cx);
    }

    /// Routes a numeric text field to an i32 SQL-formatting field, with the same
    /// honest-validation contract as [`Self::set_performance_usize`].
    pub(super) fn set_sql_i32(
        &mut self,
        id: &'static str,
        text: &str,
        cx: &mut Context<Self>,
        f: fn(&mut preferences::SqlFormattingPreferences, i32),
    ) {
        match parse_field::<i32>(text) {
            FieldParse::Value(value) => {
                self.field_errors.remove(id);
                self.set_sql_field(cx, |s| f(s, value));
            }
            FieldParse::Empty => {
                self.field_errors.remove(id);
                cx.notify();
            }
            FieldParse::Invalid => {
                self.field_errors.insert(
                    id,
                    crate::localization::text("settings.state.static.enter_a_whole_number_5")
                        .to_string(),
                );
                cx.notify();
            }
        }
    }

    // ---- AI summary helpers ----

    /// Write one `AiSummaryPreferences` field and persist the set.
    pub(super) fn set_ai_summary_field<F>(&self, cx: &mut Context<Self>, f: F)
    where
        F: FnOnce(&mut preferences::AiSummaryPreferences),
    {
        let mut p = self.store.ai_summary();
        f(&mut p);
        self.store.set_ai_summary(p);
        cx.notify();
    }

    // ---- FIM helpers ----

    pub(super) fn set_fim_field<F>(&self, cx: &mut Context<Self>, f: F)
    where
        F: FnOnce(&mut crate::fim_completion::FimPreferences),
    {
        let mut p = self.store.fim();
        f(&mut p);
        self.store.set_fim(p);
        cx.notify();
    }

    pub(super) fn fim_check<F>(
        &self,
        id: &'static str,
        label: &str,
        value: bool,
        toggle: F,
        cx: &mut Context<Self>,
    ) -> AnyElement
    where
        F: Fn(&mut crate::fim_completion::FimPreferences) + Copy + 'static,
    {
        self.checkbox(id, label, value, cx, move |this, cx| {
            this.set_fim_field(cx, toggle)
        })
    }

    pub(super) fn set_fim_usize(
        &mut self,
        id: &'static str,
        text: &str,
        cx: &mut Context<Self>,
        f: fn(&mut crate::fim_completion::FimPreferences, usize),
    ) {
        match parse_field::<usize>(text) {
            FieldParse::Value(value) => {
                self.field_errors.remove(id);
                self.set_fim_field(cx, |p| f(p, value));
            }
            FieldParse::Empty => {
                self.field_errors.remove(id);
                cx.notify();
            }
            FieldParse::Invalid => {
                self.field_errors.insert(
                    id,
                    crate::localization::text("settings.state.static.enter_a_whole_number_6")
                        .to_string(),
                );
                cx.notify();
            }
        }
    }

    pub(super) fn set_fim_f64(
        &mut self,
        id: &'static str,
        text: &str,
        cx: &mut Context<Self>,
        f: fn(&mut crate::fim_completion::FimPreferences, f64),
    ) {
        match parse_field::<f64>(text) {
            FieldParse::Value(value) => {
                self.field_errors.remove(id);
                self.set_fim_field(cx, |p| f(p, value));
            }
            FieldParse::Empty => {
                self.field_errors.remove(id);
                cx.notify();
            }
            FieldParse::Invalid => {
                self.field_errors.insert(
                    id,
                    crate::localization::text("settings.state.static.enter_a_number_e_g_0_25")
                        .to_string(),
                );
                cx.notify();
            }
        }
    }
}
