//! DDL Navigation pane: presentation choices, popup preview, monitor picker.
//!
//! Split from `preferences_ui.rs`; code moved verbatim.

use super::*;

impl PreferencesWindow {
    // ============================================================
    // DDL Navigation
    // ============================================================

    pub(super) fn set_ddl_field<F>(&self, cx: &mut Context<Self>, f: F)
    where
        F: FnOnce(&mut preferences::DdlNavigationPreferences),
    {
        let mut d = self.store.ddl_navigation();
        f(&mut d);
        self.store.set_ddl_navigation(d);
        cx.notify();
    }

    pub(super) fn toggle_ddl_enabled(&mut self, cx: &mut Context<Self>) {
        let enabled = self.store.ddl_navigation().enabled;
        self.set_ddl_field(cx, |d| d.enabled = !enabled);
    }

    pub(super) fn toggle_ddl_privileges(&mut self, cx: &mut Context<Self>) {
        let on = self.store.ddl_navigation().include_privileges;
        self.set_ddl_field(cx, |d| d.include_privileges = !on);
    }

    pub(super) fn choose_ddl_new_tab(&mut self, cx: &mut Context<Self>) {
        self.set_ddl_field(cx, |d| {
            d.presentation = DdlNavigationPresentation::OpenInNewTab
        });
    }

    pub(super) fn choose_ddl_side_panel(&mut self, cx: &mut Context<Self>) {
        self.set_ddl_field(cx, |d| {
            d.presentation = DdlNavigationPresentation::SidePanel
        });
    }

    pub(super) fn choose_ddl_popup(&mut self, cx: &mut Context<Self>) {
        self.set_ddl_field(cx, |d| d.presentation = DdlNavigationPresentation::Popup);
    }

    /// One of the DDL popup position sliders. The percent → fraction mapping
    /// and store write live in the `SliderEvent::Change` subscriptions set up
    /// in [`Self::new`]; this only renders the element.
    pub(super) fn ddl_percent_slider(&self, state: &Entity<SliderState>) -> AnyElement {
        div()
            .min_w(px(Theme::scaled(260.)))
            .child(Slider::new(state).w(px(Theme::scaled(220.))))
            .into_any_element()
    }

    pub(super) fn ddl_monitor_dropdown(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let current = self.store.ddl_navigation().popup.monitor;
        let mut options: Vec<DropdownOption> = Vec::new();

        let current_selected = current == DdlPopupMonitorPreference::CurrentWindow;
        options.push((
            "Current app monitor".to_string(),
            current_selected,
            std::rc::Rc::new(
                |this: &mut Self, _window: &mut Window, cx: &mut Context<Self>| {
                    this.set_ddl_field(cx, |d| {
                        d.popup.monitor = DdlPopupMonitorPreference::CurrentWindow;
                    });
                },
            ),
        ));

        for monitor in &self.monitors {
            // Only monitors with a stable identifier can be persisted.
            let Some(uuid) = monitor.uuid.clone() else {
                continue;
            };
            let selected = matches!(
                &current,
                DdlPopupMonitorPreference::Monitor { id } if *id == uuid
            );
            options.push((
                monitor.label.clone(),
                selected,
                std::rc::Rc::new(
                    move |this: &mut Self, _window: &mut Window, cx: &mut Context<Self>| {
                        let id = uuid.clone();
                        this.set_ddl_field(cx, move |d| {
                            d.popup.monitor = DdlPopupMonitorPreference::Monitor { id };
                        });
                    },
                ),
            ));
        }

        self.select_dropdown("ddl_popup_monitor", "", options, window, cx)
    }

    /// To-scale preview of the selected monitor with the popup rectangle
    /// placed by the same `popup_bounds` math used at open time (WYSIWYG).
    pub(super) fn render_ddl_popup_preview(&self, cx: &mut Context<Self>) -> AnyElement {
        let _ = cx;
        let d = self.store.ddl_navigation();
        let Some(monitor) =
            crate::ddl_navigation::select_monitor(&d.popup.monitor, &self.monitors, None)
        else {
            return self.hint(&crate::localization::text(
                "settings.ddl.hint.monitor_info_unavailable",
            ));
        };

        const PREVIEW_WIDTH: f32 = 360.0;
        let monitor_width = f32::from(monitor.bounds.size.width).max(1.0);
        let monitor_height = f32::from(monitor.bounds.size.height).max(1.0);
        let scale = PREVIEW_WIDTH / monitor_width;

        let popup = crate::ddl_navigation::popup_bounds(&d.popup, monitor.bounds);
        let rel_x = (f32::from(popup.origin.x) - f32::from(monitor.bounds.origin.x)) * scale;
        let rel_y = (f32::from(popup.origin.y) - f32::from(monitor.bounds.origin.y)) * scale;
        let popup_w = f32::from(popup.size.width) * scale;
        let popup_h = f32::from(popup.size.height) * scale;

        let mut popup_fill = Theme::accent();
        popup_fill.a = 0.25;
        let clamped = d.popup.clamped();

        div()
            .flex()
            .flex_col()
            .gap(px(Theme::scaled(6.)))
            .py(px(Theme::scaled(4.)))
            .child(
                div()
                    .relative()
                    .w(px(PREVIEW_WIDTH))
                    .h(px(monitor_height * scale))
                    .border_1()
                    .border_color(Theme::border())
                    .rounded(px(Theme::scaled(4.)))
                    .bg(Theme::panel_bg())
                    .child(
                        div()
                            .absolute()
                            .left(px(rel_x))
                            .top(px(rel_y))
                            .w(px(popup_w))
                            .h(px(popup_h))
                            .bg(popup_fill)
                            .border_1()
                            .border_color(Theme::accent())
                            .rounded(px(Theme::scaled(2.))),
                    ),
            )
            .child(self.hint(&crate::localization::format(
                "settings.ddl.message.popup_px",
                &[
                    ("label", &(monitor.label).to_string()),
                    ("width", &(clamped.width).to_string()),
                    ("height", &(clamped.height).to_string()),
                ],
            )))
            .into_any_element()
    }

    pub(super) fn render_ddl_navigation(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let d = self.store.ddl_navigation();

        let mut children = vec![
            self.heading(&crate::localization::text(
                "settings.ddl.heading.ddl_navigation",
            )),
            self.intro(&crate::localization::text("settings.ddl.intro.intro")),
            self.checkbox(
                "ddl_nav_enabled",
                &crate::localization::text("settings.ddl.checkbox.modifier_click_navigation"),
                d.enabled,
                cx,
                Self::toggle_ddl_enabled,
            ),
            self.labeled_row(
                &crate::localization::text("settings.ddl.labeled_row.activation_modifier"),
                self.enum_select(
                    "ddl_nav_modifier",
                    d.activation_modifier,
                    DdlNavigationModifier::all().to_vec(),
                    |m| m.display_title().to_string(),
                    |this, m, cx| this.set_ddl_field(cx, move |d| d.activation_modifier = m),
                    window,
                    cx,
                ),
            ),
            self.section_header(&crate::localization::text(
                "settings.ddl.section_header.show_definitions",
            )),
            self.radio(
                "ddl_pres_tab",
                &DdlNavigationPresentation::OpenInNewTab.display_title(),
                d.presentation == DdlNavigationPresentation::OpenInNewTab,
                cx,
                Self::choose_ddl_new_tab,
            ),
            self.radio(
                "ddl_pres_panel",
                &DdlNavigationPresentation::SidePanel.display_title(),
                d.presentation == DdlNavigationPresentation::SidePanel,
                cx,
                Self::choose_ddl_side_panel,
            ),
            self.radio(
                "ddl_pres_popup",
                &DdlNavigationPresentation::Popup.display_title(),
                d.presentation == DdlNavigationPresentation::Popup,
                cx,
                Self::choose_ddl_popup,
            ),
        ];

        children.push(self.section_header(&crate::localization::text(
            "settings.ddl.section_header.definition_contents",
        )));
        children.push(self.checkbox(
            "ddl_include_privileges",
            &crate::localization::text("settings.ddl.checkbox.include_pg_privileges"),
            d.include_privileges,
            cx,
            Self::toggle_ddl_privileges,
        ));
        children.push(self.hint(&crate::localization::text(
            "settings.ddl.hint.privileges_explained",
        )));

        if d.presentation == DdlNavigationPresentation::Popup {
            children.push(self.section_header(&crate::localization::text(
                "settings.ddl.section_header.popup_window",
            )));
            children.push(self.labeled_row(
                &crate::localization::text("settings.ddl.labeled_row.width_px"),
                self.prefs_input("ddl_popup_width"),
            ));
            children.push(self.labeled_row(
                &crate::localization::text("settings.ddl.labeled_row.height_px"),
                self.prefs_input("ddl_popup_height"),
            ));
            children.push(self.labeled_row(
                &crate::localization::text("settings.ddl.labeled_row.horizontal_position"),
                self.ddl_percent_slider(&self.ddl_popup_x_slider),
            ));
            children.push(self.labeled_row(
                &crate::localization::text("settings.ddl.labeled_row.vertical_position"),
                self.ddl_percent_slider(&self.ddl_popup_y_slider),
            ));
            children.push(self.labeled_row(
                &crate::localization::text("settings.ddl.labeled_row.monitor"),
                self.ddl_monitor_dropdown(window, cx),
            ));
            children.push(self.section_header(&crate::localization::text(
                "settings.ddl.section_header.preview",
            )));
            children.push(self.render_ddl_popup_preview(cx));
            children.push(self.hint(&crate::localization::text(
                "settings.ddl.hint.popup_clamped",
            )));
        }

        self.pane_container(children)
    }
}
