use super::*;

impl AppView {
    /// Apply the current `editor_zoom` multiplier to every tab's editor font,
    /// keeping the configured editor font family and preference font size as
    /// the 100% baseline.
    pub(super) fn apply_editor_zoom(&mut self, cx: &mut Context<Self>) {
        let family = self.appearance_prefs.editor_font.clone();
        let size = crate::editor_zoom::zoomed_font_size(
            self.appearance_prefs.editor_font_size,
            self.editor_zoom,
        );
        for tab in &self.tabs {
            tab.editor.update(cx, |ed, cx| {
                ed.set_font(&family, size, cx);
            });
        }
    }

    /// Set the editor zoom multiplier (clamped) and re-apply it to all editors.
    pub(super) fn set_editor_zoom(
        &mut self,
        zoom: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.apply_editor_zoom_value(zoom, true, window, cx);
    }

    pub(super) fn apply_editor_zoom_value(
        &mut self,
        zoom: f32,
        persist: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let clamped = crate::editor_zoom::clamp_zoom(zoom);
        if (clamped - self.editor_zoom).abs() < f32::EPSILON {
            return;
        }
        self.editor_zoom = clamped;
        // Keep the slider thumb in step with zoom changes that came from the
        // keyboard shortcuts / reset (SliderState::set_value emits no event,
        // so this cannot loop back into the change subscription).
        let pct = crate::editor_zoom::zoom_to_percent(clamped) as f32;
        self.zoom_slider.update(cx, |slider, cx| {
            slider.set_value(pct, window, cx);
        });
        self.apply_editor_zoom(cx);
        if persist {
            self.set_status(
                StatusLevel::Info,
                crate::localization::format(
                    "shell.editor.zoom_status",
                    &[(
                        "percent",
                        &crate::editor_zoom::zoom_to_percent(clamped).to_string(),
                    )],
                ),
            );
            self.save_workspace_session(cx);
        }
        cx.notify();
    }

    /// Gesture zoom path: the editor event handler has no `Window`, so the slider is synced on the next frame.
    pub(super) fn apply_editor_zoom_from_gesture(&mut self, zoom: f32, cx: &mut Context<Self>) {
        let clamped = crate::editor_zoom::clamp_zoom(zoom);
        if (clamped - self.editor_zoom).abs() < f32::EPSILON {
            return;
        }
        self.editor_zoom = clamped;
        self.zoom_slider_needs_sync = true;
        self.apply_editor_zoom(cx);
        self.set_status(
            StatusLevel::Info,
            crate::localization::format(
                "shell.editor.gesture_zoom_status",
                &[(
                    "percent",
                    &crate::editor_zoom::zoom_to_percent(clamped).to_string(),
                )],
            ),
        );
        self.save_workspace_session(cx);
        cx.notify();
    }

    /// Drop-up editor zoom control: vertical slider with + / −, shown above
    /// the status-bar percent readout.
    pub(super) fn render_zoom_control(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let track_h = Theme::scaled(ZOOM_TRACK_WIDTH);
        let thumb_pad = Theme::scaled(8.0);
        let icon_btn = |id: &'static str, icon: AppIcon| {
            div()
                .id(id)
                .w(px(Theme::scaled(18.0)))
                .h(px(Theme::scaled(18.0)))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(Theme::scaled(3.0)))
                .cursor(CursorStyle::PointingHand)
                .text_color(Theme::text_dim())
                .hover(|s| s.bg(Theme::border()))
                .child(
                    Icon::empty()
                        .path(icon.path())
                        .size(px(Theme::scaled(12.0))),
                )
        };

        let minus = icon_btn("zoom-out", AppIcon::Minus).on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, _ev, window, cx| {
                let z = crate::editor_zoom::zoom_out(this.editor_zoom);
                this.set_editor_zoom(z, window, cx);
            }),
        );
        let plus = icon_btn("zoom-in", AppIcon::Add).on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, _ev, window, cx| {
                let z = crate::editor_zoom::zoom_in(this.editor_zoom);
                this.set_editor_zoom(z, window, cx);
            }),
        );

        div()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(Theme::scaled(6.0)))
            .p(px(Theme::scaled(8.0)))
            .bg(Theme::popup_bg())
            .border_1()
            .border_color(Theme::popup_border())
            .rounded(px(Theme::scaled(6.0)))
            .child(plus)
            .child(
                div()
                    .h(px(track_h + thumb_pad * 2.0))
                    .w(px(Theme::scaled(24.0)))
                    .flex()
                    .items_center()
                    .justify_center()
                    .py(px(thumb_pad))
                    .child(
                        Slider::new(&self.zoom_slider)
                            .vertical()
                            .h(px(track_h))
                            .w(px(Theme::scaled(16.0))),
                    ),
            )
            .child(minus)
    }

    pub(super) fn handle_minimize_window(
        &mut self,
        _action: &MinimizeWindow,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        window.minimize_window();
    }

    pub(super) fn handle_zoom_window(
        &mut self,
        _action: &ZoomWindow,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        window.zoom_window();
    }

    pub(super) fn handle_zoom_editor_in(
        &mut self,
        _action: &ZoomEditorIn,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let z = crate::editor_zoom::zoom_in(self.editor_zoom);
        self.set_editor_zoom(z, window, cx);
    }

    pub(super) fn handle_zoom_editor_out(
        &mut self,
        _action: &ZoomEditorOut,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let z = crate::editor_zoom::zoom_out(self.editor_zoom);
        self.set_editor_zoom(z, window, cx);
    }

    pub(super) fn handle_zoom_editor_reset(
        &mut self,
        _action: &ZoomEditorReset,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_editor_zoom(crate::editor_zoom::DEFAULT_ZOOM, window, cx);
    }

    pub(super) fn handle_toggle_zoom_popover(
        &mut self,
        _action: &ToggleZoomPopover,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.zoom_popover_open = !self.zoom_popover_open;
        cx.notify();
    }

    pub(super) fn handle_close_zoom_popover(
        &mut self,
        _action: &CloseZoomPopover,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.zoom_popover_open {
            self.zoom_popover_open = false;
            self.save_workspace_session(cx);
            cx.notify();
        }
    }

    pub(super) fn end_pane_resize(&mut self, cx: &mut Context<Self>) {
        let was_resizing = self.is_resizing_sidebar
            || self.is_resizing_ai
            || self.is_resizing_results
            || self.is_resizing_log
            || self.is_resizing_ddl_panel
            || self.is_resizing_history_panel
            || self.is_resizing_split
            || self.is_resizing_cell_detail;
        self.is_resizing_sidebar = false;
        self.is_resizing_ai = false;
        self.is_resizing_results = false;
        self.is_resizing_log = false;
        self.is_resizing_ddl_panel = false;
        self.is_resizing_history_panel = false;
        self.is_resizing_split = false;
        self.is_resizing_cell_detail = false;
        if was_resizing {
            self.save_workspace_session(cx);
            cx.notify();
        }
    }

    pub(super) fn handle_bring_all_to_front(
        &mut self,
        _action: &BringAllToFront,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        window.activate_window();
    }

    /// Switch the appearance mode, persist it to the appearance preferences
    /// (so the choice survives relaunch and stays in sync with the
    /// Preferences → Appearance → Themes pane), and re-theme all live grids.
    pub(super) fn apply_theme_mode(&mut self, kind: ThemeKind, cx: &mut Context<Self>) {
        self.appearance_prefs.theme_mode =
            crate::preferences::ThemeModePreference::from_theme(kind);
        self.persist_and_apply_appearance(cx);
    }

    pub(super) fn persist_and_apply_appearance(&mut self, cx: &mut Context<Self>) {
        self.appearance_prefs.save();
        self.appearance_prefs.apply_theme();
        self.refresh_all_grid_themes(cx);
        cx.notify();
    }

    /// Push the current app theme into every live grid: all result grids in
    /// every tab, plus the object editor overlay's page grids if it is open.
    /// Grids opt out of the datatable crate's own OS-appearance theming, so
    /// every theme-change path must run this to re-theme them.
    pub(super) fn refresh_all_grid_themes(&mut self, cx: &mut Context<Self>) {
        for tab in &self.tabs {
            tab.results.update(cx, |rv, cx| rv.refresh_grid_themes(cx));
        }
        if let Some(editor) = self.overlays.object_editor.clone() {
            editor.update(cx, |ed, cx| ed.refresh_grid_themes(cx));
        }
    }

    pub(super) fn handle_theme_system(
        &mut self,
        _action: &ThemeSystem,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.apply_theme_mode(ThemeKind::System, cx);
    }

    pub(super) fn handle_theme_light(
        &mut self,
        _action: &ThemeLight,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.apply_theme_mode(ThemeKind::Light, cx);
    }

    pub(super) fn handle_theme_dark(
        &mut self,
        _action: &ThemeDark,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.apply_theme_mode(ThemeKind::Dark, cx);
    }

    pub(super) fn handle_theme_high_contrast(
        &mut self,
        _action: &ThemeHighContrast,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.appearance_prefs.apply_high_contrast_shortcut();
        self.persist_and_apply_appearance(cx);
    }

    pub(super) fn handle_split_vertical(
        &mut self,
        _action: &SplitVertical,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.split_editor(crate::editor_groups::SplitAxis::Vertical, window, cx);
    }

    pub(super) fn handle_split_horizontal(
        &mut self,
        _action: &SplitHorizontal,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.split_editor(crate::editor_groups::SplitAxis::Horizontal, window, cx);
    }

    pub(super) fn handle_close_split(
        &mut self,
        _action: &CloseSplit,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_split(cx);
    }

    pub(super) fn handle_split_same_buffer_vertical(
        &mut self,
        _action: &SplitSameBufferVertical,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.split_same_buffer(crate::editor_groups::SplitAxis::Vertical, window, cx);
    }

    pub(super) fn handle_split_same_buffer_horizontal(
        &mut self,
        _action: &SplitSameBufferHorizontal,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.split_same_buffer(crate::editor_groups::SplitAxis::Horizontal, window, cx);
    }

    /// Half the viewport along `axis` — the starting leading-pane size for a
    /// fresh split; divider drags refine it and the clamp keeps restored
    /// sessions honest.
    pub(super) fn default_split_size(
        axis: crate::editor_groups::SplitAxis,
        window: &Window,
    ) -> f32 {
        let viewport = window.viewport_size();
        match axis {
            crate::editor_groups::SplitAxis::Vertical => f32::from(viewport.width) * 0.45,
            crate::editor_groups::SplitAxis::Horizontal => f32::from(viewport.height) * 0.45,
        }
    }

    /// Open (or re-orient) the editor split. Stage 1 semantics with 2+ tabs:
    /// the active tab's SQL is cloned into a new tab that opens focused in
    /// the trailing pane, while the original stays in the leading pane — two
    /// independent editor+results stacks. With a single open tab there is
    /// nothing worth cloning into a second tab, so the same action opens a
    /// same-buffer split (stage 2): both panes edit the active tab's document
    /// through two live views.
    pub(super) fn split_editor(
        &mut self,
        axis: crate::editor_groups::SplitAxis,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.tabs.is_empty() {
            return;
        }
        if let Some(split) = &mut self.split {
            if split.axis != axis {
                split.axis = axis;
                self.save_workspace_session(cx);
                cx.notify();
            }
            return;
        }
        if self.tabs.len() == 1 {
            self.split_same_buffer(axis, window, cx);
            return;
        }
        let Some(active) = self.tabs.get(self.active_tab) else {
            return;
        };
        let text = active.editor.read(cx).text().to_string();
        let original = self.active_tab;
        self.new_tab(cx, Some(text));
        let first_size = Self::default_split_size(axis, window);
        self.split = Some(crate::editor_groups::SplitState::new(
            axis, original, false, first_size,
        ));
        self.save_workspace_session(cx);
        cx.notify();
    }

    /// Open (or re-orient) a same-buffer split: both panes show the active
    /// tab's document through two views sharing one buffer, undo stream,
    /// diagnostics, and dirty state, with independent cursors and scrolling.
    /// An existing two-tab split gives way (its unfocused tab stays in the
    /// tab bar).
    pub(super) fn split_same_buffer(
        &mut self,
        axis: crate::editor_groups::SplitAxis,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.get(self.active_tab) else {
            return;
        };
        if tab.ddl_view.is_some() {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.editor.ddl_split_unavailable"),
            );
            cx.notify();
            return;
        }
        if let Some(split) = &mut self.split {
            if split.is_same_buffer() {
                if split.axis != axis {
                    split.axis = axis;
                    self.save_workspace_session(cx);
                    cx.notify();
                }
                return;
            }
            self.split = None;
        }
        let first_size = Self::default_split_size(axis, window);
        self.split = Some(crate::editor_groups::SplitState::new_same_buffer(
            axis, true, first_size,
        ));
        self.retarget_same_buffer_view(cx);
        self.save_workspace_session(cx);
        cx.notify();
    }

    /// Keep the same-buffer split's second view bound to the ACTIVE tab's
    /// document. Call after anything that changes `active_tab` (or opens the
    /// split). No-op without a same-buffer split; closes the split when the
    /// active tab cannot host one (gone, or a DDL viewer tab).
    pub(super) fn retarget_same_buffer_view(&mut self, cx: &mut Context<Self>) {
        if !self.split.as_ref().is_some_and(|s| s.is_same_buffer()) {
            return;
        }
        let Some(tab) = self.tabs.get(self.active_tab) else {
            self.close_split(cx);
            return;
        };
        if tab.ddl_view.is_some() {
            self.close_split(cx);
            return;
        }
        let source = tab.editor.clone();
        if let Some(view) = &self.same_buffer_view
            && view
                .read(cx)
                .shared_peer_entity()
                .is_some_and(|peer| peer == source)
        {
            return; // already bound to this tab's document
        }
        self.drop_same_buffer_view(cx);
        let view = SqlEditor::new_shared_view(&source, cx);
        // The view reports through the same per-tab event pipeline as the
        // canonical editor; `find_tab_by_editor` resolves it to the active
        // tab, so Execute/FIM/DDL-navigation from the second pane just work.
        cx.subscribe(&view, Self::handle_editor_event).detach();
        self.same_buffer_view = Some(view);
        cx.notify();
    }

    /// Drop the second view and unlink the canonical editor's document from
    /// it. The canonical editor keeps the document exactly as it stands.
    pub(super) fn drop_same_buffer_view(&mut self, cx: &mut Context<Self>) {
        if let Some(view) = self.same_buffer_view.take() {
            if let Some(canonical) = view.read(cx).shared_peer_entity() {
                canonical.update(cx, |e, _| e.detach_shared_peer());
            }
            view.update(cx, |e, _| e.detach_shared_peer());
        }
    }

    /// Drop back to a single pane. Two-tab split: the unfocused pane's tab
    /// stays open in the tab bar. Same-buffer split: the surviving editor
    /// keeps the document, and when the closing (second) view was the
    /// focused one its caret carries over so the user's position survives.
    pub(super) fn close_split(&mut self, cx: &mut Context<Self>) {
        if let Some(split) = self.split.take() {
            if split.is_same_buffer() {
                if !split.active_first
                    && let (Some(view), Some(tab)) =
                        (&self.same_buffer_view, self.tabs.get(self.active_tab))
                {
                    let (cursor, anchor) = view.read(cx).caret_state();
                    tab.editor.update(cx, |e, cx| {
                        e.adopt_caret_state(cursor, anchor, cx);
                        e.request_focus();
                    });
                }
                self.drop_same_buffer_view(cx);
            }
            self.save_workspace_session(cx);
            cx.notify();
        }
    }

    /// Click-to-focus routing for the split panes: focusing the unfocused
    /// pane swaps which tab is active (menus, shortcuts, and the query
    /// toolbar then target that pane) without moving any tab between panes.
    /// In a same-buffer split both panes already show the active tab, so
    /// only keyboard focus moves between the two views.
    pub(super) fn focus_split_pane(&mut self, first: bool, cx: &mut Context<Self>) {
        let Some(split) = &mut self.split else {
            return;
        };
        if split.active_first == first {
            return;
        }
        if split.is_same_buffer() {
            split.focus_other(self.active_tab);
            let target = if first {
                self.tabs.get(self.active_tab).map(|t| t.editor.clone())
            } else {
                self.same_buffer_view.clone()
            };
            if let Some(editor) = target {
                editor.update(cx, |e, _| e.request_focus());
            }
            self.save_workspace_session(cx);
            cx.notify();
            return;
        }
        let new_active = split.focus_other(self.active_tab);
        self.active_tab = new_active;
        self.sync_toolbar_to_active_tab();
        self.sync_connection_to_active_tab(cx);
        if let Some(tab) = self.tabs.get(new_active) {
            tab.editor.update(cx, |e, _| e.request_focus());
        }
        self.save_workspace_session(cx);
        cx.notify();
    }
}
