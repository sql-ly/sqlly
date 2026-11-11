//! Themes and Display panes: theme gallery, mode pills, fonts.
//!
//! Split from `preferences_ui.rs`; code moved verbatim.

use super::*;

impl PreferencesWindow {
    // The former `render_pane` dispatch and `render_sidebar` tree renderer were
    // removed in the migration to the gpui-component `Settings` element: pane
    // selection and the sidebar are now owned by the component, and each pane's
    // `render_*` body is invoked through `build_pages` / `pane_group`.

    // ============================================================
    // Appearance → Themes
    // ============================================================

    pub(super) fn render_themes(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let a = self.store.appearance();
        let mode = match a.theme_mode {
            crate::preferences::ThemeModePreference::HighContrast => {
                crate::preferences::ThemeModePreference::Dark
            }
            other => other,
        };
        let light_theme = a.light_theme.sanitize_light();
        let dark_theme = a.dark_theme.sanitize_dark();

        let children = vec![
            self.heading(&crate::localization::text(
                "settings.appearance.heading.themes",
            )),
            self.intro(&crate::localization::text(
                "settings.appearance.intro.mode_and_palette",
            )),
            self.render_mode_pills(mode, cx),
            div()
                .h(px(1.))
                .bg(Theme::border())
                .mt(px(Theme::scaled(8.)))
                .mb(px(Theme::scaled(4.)))
                .into_any_element(),
            self.section_header(&crate::localization::text(
                "settings.appearance.section_header.light_theme",
            )),
            self.render_theme_gallery(&crate::theme::ColorTheme::LIGHT, light_theme, cx),
            self.section_header(&crate::localization::text(
                "settings.appearance.section_header.dark_theme",
            )),
            self.render_theme_gallery(&crate::theme::ColorTheme::DARK, dark_theme, cx),
        ];
        self.pane_container(children)
    }

    pub(super) fn render_mode_pills(
        &self,
        current: crate::preferences::ThemeModePreference,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use crate::icon_catalog::AppIcon;
        use crate::preferences::ThemeModePreference as Mode;

        let mut row = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap(px(Theme::scaled(8.)))
            .py(px(Theme::scaled(4.)));

        for &mode in &Mode::MODE_PILLS {
            let selected = current == mode;
            let label = mode.label();
            let icon = match mode {
                Mode::Light => AppIcon::ThemeLight,
                Mode::Dark => AppIcon::ThemeDark,
                Mode::System => AppIcon::ThemeSystem,
                Mode::HighContrast => AppIcon::ThemeDark,
            };
            let border = if selected {
                Theme::accent()
            } else {
                Theme::border()
            };
            let fill = if selected {
                Theme::row_hover()
            } else {
                gpui::transparent_black()
            };
            let pill_id = match mode {
                Mode::Light => {
                    crate::localization::text("settings.appearance.option.theme_mode_light")
                }
                Mode::Dark => {
                    crate::localization::text("settings.appearance.option.theme_mode_dark")
                }
                Mode::System => {
                    crate::localization::text("settings.appearance.option.theme_mode_system")
                }
                Mode::HighContrast => {
                    crate::localization::text("settings.appearance.option.theme_mode_hc")
                }
            };
            row = row.child(
                div()
                    .id(pill_id)
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(Theme::scaled(6.)))
                    .px(px(Theme::scaled(12.)))
                    .py(px(Theme::scaled(6.)))
                    .rounded(px(Theme::scaled(16.)))
                    .border_1()
                    .border_color(border)
                    .bg(fill)
                    .cursor_pointer()
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            this.handle_theme_mode_changed(mode, cx);
                        }),
                    )
                    .child(gpui_component::Icon::empty().path(icon.path()).text_color(
                        if selected {
                            Theme::text()
                        } else {
                            Theme::text_dim()
                        },
                    ))
                    .child(
                        div()
                            .text_size(px(Theme::font_size()))
                            .text_color(if selected {
                                Theme::text()
                            } else {
                                Theme::text_dim()
                            })
                            .child(label.to_string()),
                    ),
            );
        }

        row.into_any_element()
    }

    pub(super) fn render_theme_gallery(
        &self,
        themes: &[crate::theme::ColorTheme],
        selected: crate::theme::ColorTheme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut row = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap(px(Theme::scaled(12.)))
            .py(px(Theme::scaled(4.)));
        for &theme in themes {
            row = row.child(self.render_theme_card(theme, theme == selected, cx));
        }
        row.into_any_element()
    }

    pub(super) fn render_theme_card(
        &self,
        theme: crate::theme::ColorTheme,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = theme.preview();
        let border = if selected {
            Theme::accent()
        } else {
            Theme::border()
        };
        let is_dark = theme.is_dark();
        // GPUI `overflow_hidden` clips to an axis-aligned rectangle, not the
        // rounded path, so a square child fill leaks past the border in the
        // corner pockets. Round the sidebar and canvas fills themselves.
        let radius = px(Theme::scaled(8.));
        div()
            .id(("theme-card", theme.to_u8() as u32))
            .flex()
            .flex_col()
            .gap(px(Theme::scaled(6.)))
            .w(px(Theme::scaled(132.)))
            .cursor_pointer()
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    if is_dark {
                        this.handle_dark_theme_changed(theme, cx);
                    } else {
                        this.handle_light_theme_changed(theme, cx);
                    }
                }),
            )
            .child(
                div()
                    .h(px(Theme::scaled(80.)))
                    .rounded(radius)
                    .border_1()
                    .border_color(border)
                    .overflow_hidden()
                    .flex()
                    .flex_row()
                    .child(
                        div()
                            .w(px(Theme::scaled(36.)))
                            .h_full()
                            .rounded_l(radius)
                            .bg(p.sidebar),
                    )
                    .child(
                        div()
                            .flex_1()
                            .h_full()
                            .rounded_r(radius)
                            .bg(p.bg)
                            .px(px(Theme::scaled(8.)))
                            .py(px(Theme::scaled(12.)))
                            .flex()
                            .flex_col()
                            .gap(px(Theme::scaled(6.)))
                            .justify_center()
                            .child(
                                div()
                                    .h(px(Theme::scaled(6.)))
                                    .w(relative(0.9))
                                    .rounded(px(Theme::scaled(3.)))
                                    .bg(p.accent),
                            )
                            .child(
                                div()
                                    .h(px(Theme::scaled(5.)))
                                    .w(relative(0.72))
                                    .rounded(px(Theme::scaled(3.)))
                                    .bg(p.text),
                            )
                            .child(
                                div()
                                    .h(px(Theme::scaled(5.)))
                                    .w(relative(0.55))
                                    .rounded(px(Theme::scaled(3.)))
                                    .bg(p.muted),
                            ),
                    ),
            )
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.92)))
                    .text_color(if selected {
                        Theme::text()
                    } else {
                        Theme::text_dim()
                    })
                    .child(theme.label().to_string()),
            )
            .into_any_element()
    }

    // ============================================================
    // Appearance → Display
    // ============================================================

    pub(super) fn render_display(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let a = self.store.appearance();

        let interface_size = a.interface_font_size as i32;
        let editor_size = a.editor_font_size as i32;

        let interface_font_options = self.interface_font_dropdown_options(
            &a.interface_font,
            Self::handle_interface_font_changed,
        );
        let editor_font_options =
            self.font_dropdown_options(&a.editor_font, Self::handle_editor_font_changed);
        let density_font_size = self.store.result_formatting().density.metrics().font_size;
        let results_size = a.results_font_size.unwrap_or(density_font_size) as i32;
        let results_font_options = self.results_font_dropdown_options(&a.results_font);

        self.pane_container(vec![
            self.heading(&crate::localization::text(
                "settings.appearance.heading.display",
            )),
            self.intro(&crate::localization::text(
                "settings.appearance.intro.fonts_and_sizes",
            )),
            self.section_header(&crate::localization::text(
                "settings.appearance.section_header.interface_font",
            )),
            self.hint(&crate::localization::text(
                "settings.appearance.hint.interface_font_scope",
            )),
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(Theme::scaled(8.)))
                .py(px(Theme::scaled(4.)))
                .child(
                    div()
                        .w(px(Theme::scaled(160.)))
                        .text_size(px(Theme::font_size()))
                        .text_color(Theme::text())
                        .child(self.match_text(&crate::localization::text(
                            "settings.appearance.match_text.font_family",
                        ))),
                )
                .child(self.searchable_dropdown(
                    "appearance-interface-font",
                    &a.interface_font,
                    interface_font_options,
                    crate::localization::text(
                        "settings.appearance.searchable_dropdown.search_fonts",
                    ),
                    window,
                    cx,
                ))
                .into_any_element(),
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(Theme::scaled(8.)))
                .py(px(Theme::scaled(4.)))
                .child(
                    div()
                        .w(px(Theme::scaled(160.)))
                        .text_size(px(Theme::font_size()))
                        .text_color(Theme::text())
                        .child(self.match_text(&crate::localization::text(
                            "settings.appearance.match_text.size",
                        ))),
                )
                .child(self.stepper(
                    "appearance-if-size-dec",
                    "appearance-if-size-inc",
                    interface_size,
                    cx,
                    Self::decrement_interface_font_size,
                    Self::increment_interface_font_size,
                ))
                .into_any_element(),
            self.section_header(&crate::localization::text(
                "settings.appearance.section_header.code_font",
            )),
            self.hint(&crate::localization::text(
                "settings.appearance.hint.sql_editor_ddl_plans_logs",
            )),
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(Theme::scaled(8.)))
                .py(px(Theme::scaled(4.)))
                .child(
                    div()
                        .w(px(Theme::scaled(160.)))
                        .text_size(px(Theme::font_size()))
                        .text_color(Theme::text())
                        .child(self.match_text(&crate::localization::text(
                            "settings.appearance.match_text.font_family_2",
                        ))),
                )
                .child(self.searchable_dropdown(
                    "appearance-editor-font",
                    &a.editor_font,
                    editor_font_options,
                    crate::localization::text(
                        "settings.appearance.searchable_dropdown.search_fonts_2",
                    ),
                    window,
                    cx,
                ))
                .into_any_element(),
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(Theme::scaled(8.)))
                .py(px(Theme::scaled(4.)))
                .child(
                    div()
                        .w(px(Theme::scaled(160.)))
                        .text_size(px(Theme::font_size()))
                        .text_color(Theme::text())
                        .child(self.match_text(&crate::localization::text(
                            "settings.appearance.match_text.size_2",
                        ))),
                )
                .child(self.stepper(
                    "appearance-ef-size-dec",
                    "appearance-ef-size-inc",
                    editor_size,
                    cx,
                    Self::decrement_editor_font_size,
                    Self::increment_editor_font_size,
                ))
                .into_any_element(),
            self.section_header(&crate::localization::text(
                "settings.appearance.section_header.results_font",
            )),
            self.hint(&crate::localization::text(
                "settings.appearance.hint.results_font_scope",
            )),
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(Theme::scaled(8.)))
                .py(px(Theme::scaled(4.)))
                .child(
                    div()
                        .w(px(Theme::scaled(160.)))
                        .text_size(px(Theme::font_size()))
                        .text_color(Theme::text())
                        .child(self.match_text(&crate::localization::text(
                            "settings.appearance.match_text.font_family_3",
                        ))),
                )
                .child(self.searchable_dropdown(
                    "appearance-results-font",
                    &a.results_font,
                    results_font_options,
                    crate::localization::text(
                        "settings.appearance.searchable_dropdown.search_fonts_3",
                    ),
                    window,
                    cx,
                ))
                .into_any_element(),
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(Theme::scaled(8.)))
                .py(px(Theme::scaled(4.)))
                .child(
                    div()
                        .w(px(Theme::scaled(160.)))
                        .text_size(px(Theme::font_size()))
                        .text_color(Theme::text())
                        .child(self.match_text(&crate::localization::text(
                            "settings.appearance.match_text.size_3",
                        ))),
                )
                .child(self.stepper(
                    "appearance-rf-size-dec",
                    "appearance-rf-size-inc",
                    results_size,
                    cx,
                    Self::decrement_results_font_size,
                    Self::increment_results_font_size,
                ))
                .children(a.results_font_size.is_some().then(|| {
                    self.quiet_button(
                        "appearance-rf-size-reset",
                        &crate::localization::text(
                            "settings.appearance.quiet_button.use_density_size",
                        ),
                        cx,
                        |this, _window, cx| this.set_results_font_size(None, cx),
                    )
                }))
                .into_any_element(),
            self.hint(&crate::localization::text(
                "settings.appearance.hint.results_font_size_default",
            )),
            self.section_header(&crate::localization::text(
                "settings.appearance.section_header.free_tier",
            )),
            self.checkbox(
                "appearance-keep-supporter-message",
                &crate::localization::text("settings.appearance.checkbox.keep_love_message_paid"),
                a.keep_supporter_message,
                cx,
                Self::toggle_keep_supporter_message,
            ),
            self.hint(&crate::localization::text(
                "settings.appearance.hint.love_message_free_tier",
            )),
            self.section_header(&crate::localization::text(
                "settings.appearance.section_header.startup",
            )),
            self.checkbox(
                "appearance-show-tips-on-startup",
                &crate::localization::text("settings.appearance.checkbox.show_tips_startup"),
                crate::tip_of_the_day::prefs().show_on_startup,
                cx,
                Self::toggle_show_tips_on_startup,
            ),
            self.hint(&crate::localization::text(
                "settings.appearance.hint.tip_of_the_day",
            )),
        ])
    }

    /// Build `DropdownOption` entries for a font-picker dropdown from the
    /// cached system font list. The currently-selected font gets a checkmark;
    /// if the current font isn't in the system list it's prepended so it
    /// remains visible and selectable.
    pub(super) fn interface_font_dropdown_options(
        &self,
        current: &str,
        handler: fn(&mut Self, String, &mut Context<Self>),
    ) -> Vec<DropdownOption> {
        let mut fonts = self.system_fonts.clone();
        if !fonts.iter().any(|f| f.eq_ignore_ascii_case("System")) {
            fonts.insert(0, "System".to_string());
        }
        if !fonts.iter().any(|f| f.eq_ignore_ascii_case(current)) {
            fonts.insert(0, current.to_string());
        }
        fonts
            .into_iter()
            .map(|font| {
                let selected = font.eq_ignore_ascii_case(current);
                let label = font.clone();
                let on: DropdownHandler = std::rc::Rc::new(move |this, _window, cx| {
                    handler(this, font.clone(), cx);
                });
                (label, selected, on)
            })
            .collect()
    }

    pub(super) fn font_dropdown_options(
        &self,
        current: &str,
        handler: fn(&mut Self, String, &mut Context<Self>),
    ) -> Vec<DropdownOption> {
        let mut fonts = self.system_fonts.clone();
        if !fonts.iter().any(|f| f.eq_ignore_ascii_case(current)) {
            fonts.insert(0, current.to_string());
        }
        fonts
            .into_iter()
            .map(|font| {
                let selected = font.eq_ignore_ascii_case(current);
                let label = font.clone();
                let on: DropdownHandler = std::rc::Rc::new(move |this, _window, cx| {
                    handler(this, font.clone(), cx);
                });
                (label, selected, on)
            })
            .collect()
    }

    /// Font-picker options for the results font: [`RESULTS_FONT_DEFAULT`]
    /// (the grid's platform monospace) first, then the system fonts.
    ///
    /// [`RESULTS_FONT_DEFAULT`]: crate::preferences::RESULTS_FONT_DEFAULT
    fn results_font_dropdown_options(&self, current: &str) -> Vec<DropdownOption> {
        let default = crate::preferences::RESULTS_FONT_DEFAULT;
        let mut options = self.font_dropdown_options(current, Self::handle_results_font_changed);
        options.retain(|(label, _, _)| !label.eq_ignore_ascii_case(default));
        let on: DropdownHandler = std::rc::Rc::new(move |this, _window, cx| {
            this.handle_results_font_changed(default.to_string(), cx);
        });
        options.insert(
            0,
            (
                default.to_string(),
                current.eq_ignore_ascii_case(default),
                on,
            ),
        );
        options
    }

    pub(super) fn handle_results_font_changed(&mut self, text: String, cx: &mut Context<Self>) {
        let mut a = self.store.appearance();
        a.results_font = text;
        self.apply_results_font(a, cx);
    }

    fn increment_results_font_size(&mut self, cx: &mut Context<Self>) {
        let size = self.effective_results_font_size();
        self.set_results_font_size(Some((size + 1.0).min(32.0)), cx);
    }

    fn decrement_results_font_size(&mut self, cx: &mut Context<Self>) {
        let size = self.effective_results_font_size();
        self.set_results_font_size(Some((size - 1.0).max(8.0)), cx);
    }

    /// The results size the stepper shows: the custom size, or the one the
    /// grid density preset implies.
    fn effective_results_font_size(&self) -> f32 {
        self.store
            .appearance()
            .results_font_size
            .unwrap_or_else(|| self.store.result_formatting().density.metrics().font_size)
    }

    fn set_results_font_size(&mut self, size: Option<f32>, cx: &mut Context<Self>) {
        let mut a = self.store.appearance();
        a.results_font_size = size;
        self.apply_results_font(a, cx);
    }

    /// Save a results font change and repaint open grids right away, the way
    /// the interface and code fonts apply while the dialog stays open.
    fn apply_results_font(
        &mut self,
        a: crate::preferences::AppearancePreferences,
        cx: &mut Context<Self>,
    ) {
        self.store.set_appearance(a.clone());
        crate::theme::Theme::set_results_font_prefs(&a.results_font, a.results_font_size);
        cx.emit(PreferencesWindowEvent::ResultsFontChanged);
        cx.notify();
    }

    pub(super) fn toggle_show_tips_on_startup(&mut self, cx: &mut Context<Self>) {
        crate::tip_of_the_day::update_prefs(|p| p.show_on_startup = !p.show_on_startup);
        cx.notify();
    }

    pub(super) fn toggle_keep_supporter_message(&mut self, cx: &mut Context<Self>) {
        let mut a = self.store.appearance();
        a.keep_supporter_message = !a.keep_supporter_message;
        self.store.set_appearance(a);
        cx.notify();
    }

    pub(super) fn handle_theme_mode_changed(
        &mut self,
        mode: crate::preferences::ThemeModePreference,
        cx: &mut Context<Self>,
    ) {
        let mut a = self.store.appearance();
        a.theme_mode = mode;
        self.store.set_appearance(a.clone());
        a.apply_theme();
        cx.emit(PreferencesWindowEvent::ThemeChanged);
        cx.notify();
    }

    pub(super) fn handle_light_theme_changed(
        &mut self,
        theme: crate::theme::ColorTheme,
        cx: &mut Context<Self>,
    ) {
        let mut a = self.store.appearance();
        a.light_theme = theme.sanitize_light();
        self.store.set_appearance(a.clone());
        a.apply_theme();
        cx.emit(PreferencesWindowEvent::ThemeChanged);
        cx.notify();
    }

    pub(super) fn handle_dark_theme_changed(
        &mut self,
        theme: crate::theme::ColorTheme,
        cx: &mut Context<Self>,
    ) {
        let mut a = self.store.appearance();
        a.dark_theme = theme.sanitize_dark();
        self.store.set_appearance(a.clone());
        a.apply_theme();
        cx.emit(PreferencesWindowEvent::ThemeChanged);
        cx.notify();
    }

    pub(super) fn handle_interface_font_changed(&mut self, text: String, cx: &mut Context<Self>) {
        let mut a = self.store.appearance();
        a.interface_font = text;
        self.store.set_appearance(a.clone());
        crate::theme::Theme::set_font_prefs(
            &a.interface_font,
            a.interface_font_size,
            &a.editor_font,
            a.editor_font_size,
        );
        cx.notify();
    }

    pub(super) fn handle_editor_font_changed(&mut self, text: String, cx: &mut Context<Self>) {
        let mut a = self.store.appearance();
        a.editor_font = text;
        self.store.set_appearance(a.clone());
        crate::theme::Theme::set_font_prefs(
            &a.interface_font,
            a.interface_font_size,
            &a.editor_font,
            a.editor_font_size,
        );
        cx.notify();
    }

    pub(super) fn increment_interface_font_size(&mut self, cx: &mut Context<Self>) {
        let mut a = self.store.appearance();
        a.interface_font_size = (a.interface_font_size + 1.0).min(32.0);
        self.store.set_appearance(a.clone());
        crate::theme::Theme::set_font_prefs(
            &a.interface_font,
            a.interface_font_size,
            &a.editor_font,
            a.editor_font_size,
        );
        cx.notify();
    }

    pub(super) fn decrement_interface_font_size(&mut self, cx: &mut Context<Self>) {
        let mut a = self.store.appearance();
        a.interface_font_size = (a.interface_font_size - 1.0).max(8.0);
        self.store.set_appearance(a.clone());
        crate::theme::Theme::set_font_prefs(
            &a.interface_font,
            a.interface_font_size,
            &a.editor_font,
            a.editor_font_size,
        );
        cx.notify();
    }

    pub(super) fn increment_editor_font_size(&mut self, cx: &mut Context<Self>) {
        let mut a = self.store.appearance();
        a.editor_font_size = (a.editor_font_size + 1.0).min(32.0);
        self.store.set_appearance(a.clone());
        crate::theme::Theme::set_font_prefs(
            &a.interface_font,
            a.interface_font_size,
            &a.editor_font,
            a.editor_font_size,
        );
        cx.notify();
    }

    pub(super) fn decrement_editor_font_size(&mut self, cx: &mut Context<Self>) {
        let mut a = self.store.appearance();
        a.editor_font_size = (a.editor_font_size - 1.0).max(8.0);
        self.store.set_appearance(a.clone());
        crate::theme::Theme::set_font_prefs(
            &a.interface_font,
            a.interface_font_size,
            &a.editor_font,
            a.editor_font_size,
        );
        cx.notify();
    }

    pub(super) fn render_language(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selected = self.pending_language;
        let options = crate::localization::LANGUAGES
            .iter()
            .map(|info| {
                let language = info.language;
                let handler: DropdownHandler = std::rc::Rc::new(move |this, _window, cx| {
                    this.pending_language = language;
                    this.language_status = None;
                    this.language_error = None;
                    cx.notify();
                });
                (info.native_name.to_string(), language == selected, handler)
            })
            .collect();
        #[cfg(not(target_arch = "wasm32"))]
        let file_label = self
            .pending_translation_file
            .clone()
            .unwrap_or_else(|| crate::localization::text("settings.appearance.language.no_file"));
        let mut children = vec![
            self.heading(&crate::localization::text(
                "settings.appearance.language.title",
            )),
            self.intro(&crate::localization::text(
                "settings.appearance.language.intro",
            )),
            self.labeled_row(
                &crate::localization::text("settings.appearance.language.language_label"),
                self.select_dropdown("appearance-language", "", options, window, cx),
            ),
            div()
                .flex()
                .flex_row()
                .justify_end()
                .child(
                    Button::new("appearance-language-apply")
                        .label(crate::localization::text(
                            "settings.appearance.language.apply_button",
                        ))
                        .primary()
                        .on_click(cx.listener(|this, _, _window, cx| {
                            this.apply_language(cx);
                        })),
                )
                .into_any_element(),
            self.section_header(&crate::localization::text(
                "settings.appearance.language.custom_file_title",
            )),
            self.hint(&crate::localization::text(
                "settings.appearance.language.custom_file_description",
            )),
        ];
        #[cfg(not(target_arch = "wasm32"))]
        {
            children.push(self.labeled_row(
                &crate::localization::text("settings.appearance.language.file_label"),
                self.read_only_value(&file_label),
            ));
            children.push(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(Theme::scaled(8.)))
                    .child(
                        Button::new("appearance-language-browse")
                            .label(crate::localization::text(
                                "settings.appearance.language.browse_button",
                            ))
                            .outline()
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.browse_translation_file(cx);
                            })),
                    )
                    .child(
                        Button::new("appearance-language-clear")
                            .label(crate::localization::text(
                                "settings.appearance.language.clear_button",
                            ))
                            .outline()
                            .disabled(self.pending_translation_file.is_none())
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.pending_translation_file = None;
                                this.language_status = None;
                                this.language_error = None;
                                cx.notify();
                            })),
                    )
                    .into_any_element(),
            );
        }
        #[cfg(target_arch = "wasm32")]
        children.push(self.hint(&crate::localization::text(
            "settings.appearance.language.browser_file_hint",
        )));
        if let Some(error) = &self.language_error {
            children.push(self.warning(error));
        } else if let Some(warning) = crate::localization::startup_custom_file_warning() {
            children.push(self.warning(&warning));
        }
        if let Some(status) = &self.language_status {
            children.push(self.hint(status));
        }
        self.pane_container(children)
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn browse_translation_file(&mut self, cx: &mut Context<Self>) {
        let weak = cx.weak_entity();
        let async_cx = cx.to_async();
        let receiver = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: None,
        });
        cx.spawn(async move |_this, _cx| {
            if let Ok(Ok(Some(paths))) = receiver.await
                && let Some(path) = paths.into_iter().next()
            {
                let mut cx = async_cx.clone();
                weak.update(&mut cx, |this, cx| {
                    this.pending_translation_file = Some(path.display().to_string());
                    this.language_status = None;
                    this.language_error = None;
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    fn apply_language(&mut self, cx: &mut Context<Self>) {
        // Activation is atomic, but saving can still fail after it: put back
        // exactly what was showing (including a browser host's live-preview
        // overrides) instead of re-reading the saved preferences.
        let previous = crate::localization::snapshot();
        let next = activate_language_preferences(
            self.store.appearance(),
            self.pending_language,
            self.pending_translation_file.clone(),
        )
        .and_then(|preferences| preferences.try_save().map(|()| preferences));
        match next {
            Ok(preferences) => {
                crate::localization::clear_startup_custom_file_issue();
                self.store.update_appearance(preferences);
                self.tree = build_preferences_tree();
                self.filtered_tree = self.tree.clone();
                self.expand_to_selected();
                self.mark_search_index_stale();
                self.search_dirty = true;
                self.language_error = None;
                self.language_status = Some(crate::localization::text(
                    "settings.appearance.language.applied",
                ));
                cx.emit(language_applied_event());
                cx.notify();
            }
            Err(error) => {
                crate::localization::restore(previous);
                crate::localization::refresh(cx);
                self.language_status = None;
                self.language_error = Some(crate::localization::format(
                    "settings.appearance.language.apply_error",
                    &[("error", &error)],
                ));
                cx.notify();
            }
        }
    }
}

fn language_applied_event() -> PreferencesWindowEvent {
    PreferencesWindowEvent::LanguageChanged
}

fn activate_language_preferences(
    mut preferences: crate::preferences::AppearancePreferences,
    language: crate::localization::Language,
    custom_file: Option<String>,
) -> Result<crate::preferences::AppearancePreferences, String> {
    #[cfg(not(target_arch = "wasm32"))]
    let overrides = custom_file
        .as_deref()
        .map(std::fs::read_to_string)
        .transpose()
        .map_err(|error| error.to_string())?;
    #[cfg(target_arch = "wasm32")]
    let overrides: Option<String> = None;
    crate::localization::activate(language, overrides.as_deref())?;
    preferences.language = language.code().to_string();
    preferences.custom_translation_file = custom_file;
    Ok(preferences)
}

#[cfg(test)]
mod language_tests {
    use super::*;
    use crate::localization::Language;

    fn reset() -> crate::localization::TestLanguageGuard {
        crate::localization::test_language(Language::English)
    }

    #[test]
    fn language_choices_use_native_names() {
        let names: Vec<_> = crate::localization::LANGUAGES
            .iter()
            .map(|info| info.native_name)
            .collect();
        assert_eq!(
            names,
            [
                "English",
                "Español",
                "简体中文",
                "日本語",
                "Português (Brasil)",
                "한국어",
                "Deutsch",
                "Русский",
                "Français",
                "繁體中文",
            ]
        );
    }

    #[test]
    fn selection_is_pending_until_apply_activates_it() {
        let _guard = reset();
        let preferences = crate::preferences::AppearancePreferences::default();
        let pending = Language::Spanish;
        assert_eq!(sqlly_i18n::current_language(), Language::English);
        assert_eq!(preferences.language, "en");
        let applied = activate_language_preferences(preferences, pending, None).unwrap();
        assert_eq!(applied.language, "es");
        assert_eq!(sqlly_i18n::current_language(), Language::Spanish);
    }

    #[test]
    fn successful_apply_emits_language_changed() {
        assert!(matches!(
            language_applied_event(),
            PreferencesWindowEvent::LanguageChanged
        ));
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn custom_file_clear_and_invalid_apply_are_atomic() {
        let _guard = reset();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("custom.json");
        std::fs::write(
            &path,
            r#"{"settings":{"appearance":{"language":{"title":{"value":"Custom Language","location":"Settings → Appearance → Language: test override for the language title."}}}}}"#,
        )
        .unwrap();
        let path_string = path.display().to_string();
        let custom = activate_language_preferences(
            crate::preferences::AppearancePreferences::default(),
            Language::English,
            Some(path_string.clone()),
        )
        .unwrap();
        assert_eq!(
            custom.custom_translation_file.as_deref(),
            Some(path_string.as_str())
        );
        assert_eq!(
            crate::localization::text("settings.appearance.language.title"),
            "Custom Language"
        );

        activate_language_preferences(custom.clone(), Language::Spanish, None).unwrap();
        assert_eq!(sqlly_i18n::current_language(), Language::Spanish);
        assert_eq!(
            crate::localization::text("settings.appearance.language.title"),
            "Idioma"
        );

        std::fs::write(&path, "not json").unwrap();
        let before = sqlly_i18n::current_language();
        let result = activate_language_preferences(custom, Language::English, Some(path_string));
        assert!(result.is_err());
        assert_eq!(sqlly_i18n::current_language(), before);
        assert_eq!(
            crate::localization::text("settings.appearance.language.title"),
            "Idioma"
        );
    }
}
