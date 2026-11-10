//! Keyboard Shortcuts pane: model, capture flow, conflict checks.
//!
//! Split from `preferences_ui.rs`; code moved verbatim.

use super::*;

impl PreferencesWindow {
    // ============================================================
    // Keyboard Shortcuts
    // ============================================================

    /// Lazily build the keybindings editor model on first render so the pane
    /// opens instantly without doing file I/O up front in `new()`.
    pub(super) fn ensure_keybindings_model(&mut self) {
        if self.keybindings_model.is_some() {
            return;
        }
        let settings =
            crate::keybindings::load_keybinding_settings(&crate::keybindings::keybindings_path());
        let registry = crate::command_palette::CommandRegistry::default_registry();
        let model = crate::keybindings::KeybindingsEditorModel::new(
            settings.profile,
            registry.all().to_vec(),
            settings.overrides,
        );
        self.keybindings_model = Some(model);
        self.editor_keybindings_overrides =
            Some(crate::editor_keybindings::load_editor_overrides());
    }

    /// Save the active app shortcut profile and overrides as one startup
    /// snapshot, then expose their activation boundary in the pane.
    pub(super) fn save_keybinding_settings(&mut self) {
        if let Some(model) = &self.keybindings_model {
            let settings = crate::keybindings::KeybindingSettings {
                profile: model.profile,
                overrides: model.overrides.clone(),
            };
            match crate::keybindings::save_keybinding_settings(
                &settings,
                &crate::keybindings::keybindings_path(),
            ) {
                Ok(()) => self.keybindings_restart_required = true,
                Err(error) => {
                    self.keybinding_conflict = Some(crate::localization::format(
                        "settings.keybindings.message.save_app_shortcuts_failed",
                        &[("error", &(error).to_string())],
                    ));
                }
            }
        }
    }

    /// Save editor-level keybinding overrides to disk.
    pub(super) fn save_editor_keybindings_overrides(&mut self) {
        if let Some(overrides) = &self.editor_keybindings_overrides {
            match crate::editor_keybindings::save_editor_overrides(overrides) {
                Ok(()) => self.keybindings_restart_required = true,
                Err(error) => {
                    self.keybinding_conflict = Some(crate::localization::format(
                        "settings.keybindings.message.save_editor_shortcuts_failed",
                        &[("error", &(error).to_string())],
                    ));
                }
            }
        }
    }

    pub(super) fn set_shortcut_profile(
        &mut self,
        profile: crate::keybindings::ShortcutProfile,
        cx: &mut Context<Self>,
    ) {
        let conflicts = self
            .keybindings_model
            .as_ref()
            .map_or_else(Vec::new, |model| {
                let mut prospective = model.clone();
                prospective.set_profile(profile);
                prospective.active_conflicts()
            });
        if let Some(conflict) = conflicts.first() {
            let owners = conflict
                .command_ids
                .iter()
                .map(|id| {
                    self.keybindings_model
                        .as_ref()
                        .and_then(|model| model.commands.iter().find(|command| &command.id == id))
                        .map_or(id.as_str(), |command| command.title.as_str())
                })
                .collect::<Vec<_>>()
                .join(" and ");
            self.keybinding_conflict = Some(crate::localization::format(
                "settings.keybindings.message.already_assigned",
                &[
                    ("value", &(conflict.binding.display_string()).to_string()),
                    ("owners", &(owners).to_string()),
                ],
            ));
            cx.notify();
            return;
        }
        if let Some(model) = &mut self.keybindings_model {
            model.set_profile(profile);
            self.keybinding_conflict = None;
            self.save_keybinding_settings();
        }
        cx.notify();
    }

    /// Begin capturing a keystroke for a specific command + slot.
    pub(super) fn start_keybinding_capture(
        &mut self,
        target: String,
        slot: usize,
        cx: &mut Context<Self>,
    ) {
        self.keybinding_capture = Some((target, slot));
        cx.notify();
    }

    /// Cancel keystroke capture without committing.
    #[allow(dead_code)]
    pub(super) fn cancel_keybinding_capture(&mut self, cx: &mut Context<Self>) {
        self.keybinding_capture = None;
        cx.notify();
    }

    /// Handle a key down event while in capture mode. Converts the gpui
    /// keystroke to the appropriate binding format, saves it, and exits
    /// capture mode.
    pub(super) fn handle_keybinding_key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((target, slot)) = self.keybinding_capture.take() else {
            return;
        };

        let key = event.keystroke.key.as_str();

        // Escape cancels capture.
        if key == "escape" {
            cx.notify();
            return;
        }

        // Ignore bare modifier presses — wait for an actual key.
        if matches!(
            key,
            "cmd" | "shift" | "alt" | "ctrl" | "fn" | "platform" | "super" | "win"
        ) {
            // Put it back so we keep capturing.
            self.keybinding_capture = Some((target, slot));
            return;
        }

        if let Some(rest) = target.strip_prefix("editor:") {
            // Editor-level keybinding.
            let stroke = crate::editor_keybindings::keystroke_to_stroke_string(&event.keystroke);
            let conflicts =
                self.editor_keybindings_overrides
                    .as_ref()
                    .map_or_else(Vec::new, |overrides| {
                        crate::editor_keybindings::conflicts_for_editor_binding(
                            rest, &stroke, overrides,
                        )
                    });
            if !conflicts.is_empty() {
                let titles = conflicts
                    .iter()
                    .map(|name| crate::editor_keybindings::editor_action_display_name(name))
                    .collect::<Vec<_>>()
                    .join(" and ");
                self.keybinding_conflict = Some(crate::localization::format(
                    "settings.keybindings.message.editor_action_conflict",
                    &[
                        ("stroke", &(stroke).to_string()),
                        ("titles", &(titles).to_string()),
                    ],
                ));
                self.keybinding_capture = Some((target, slot));
                cx.notify();
                return;
            }
            if let Some(overrides) = &mut self.editor_keybindings_overrides {
                let entry = overrides.entry(rest.to_string()).or_default();
                if slot < entry.len() {
                    entry[slot] = stroke;
                } else {
                    entry.push(stroke);
                }
                // Dedupe.
                let mut seen = std::collections::HashSet::new();
                entry.retain(|s| seen.insert(s.clone()));
                entry.truncate(3);
                if entry.is_empty() {
                    overrides.remove(rest);
                }
            }
            self.keybinding_conflict = None;
            self.save_editor_keybindings_overrides();
        } else {
            // App-level keybinding.
            if let Some(ks) = crate::keybindings::from_gpui_keystroke(&event.keystroke) {
                let binding = crate::keybindings::KeyBinding::single(ks);
                let conflicts = self
                    .keybindings_model
                    .as_ref()
                    .map_or_else(Vec::new, |model| model.conflicts(&binding, &target));
                if !conflicts.is_empty() {
                    let titles = conflicts
                        .iter()
                        .map(|command| command.title.as_str())
                        .collect::<Vec<_>>()
                        .join(" and ");
                    self.keybinding_conflict = Some(crate::localization::format(
                        "settings.keybindings.message.app_command_conflict",
                        &[
                            ("value", &(binding.display_string()).to_string()),
                            ("titles", &(titles).to_string()),
                        ],
                    ));
                    self.keybinding_capture = Some((target, slot));
                    cx.notify();
                    return;
                }
                if let Some(model) = &mut self.keybindings_model {
                    model.set_binding(binding, &target, slot);
                }
                self.keybinding_conflict = None;
                self.save_keybinding_settings();
            }
        }

        cx.notify();
    }

    /// Clear a binding at a specific slot for an editor action.
    pub(super) fn clear_editor_keybinding(
        &mut self,
        action_name: String,
        slot: usize,
        cx: &mut Context<Self>,
    ) {
        if let Some(overrides) = &mut self.editor_keybindings_overrides
            && let Some(entry) = overrides.get_mut(&action_name)
        {
            if slot < entry.len() {
                entry.remove(slot);
            }
            if entry.is_empty() {
                overrides.remove(&action_name);
            }
        }
        self.keybinding_conflict = None;
        self.save_editor_keybindings_overrides();
        cx.notify();
    }

    /// Reset all overrides to defaults, behind the two-click confirm this
    /// function's doc comment has always described.
    ///
    /// It was never implemented: the button ran this on mouse-down and wiped
    /// both `keybindings.json` and `editor-keybindings.json` — every custom
    /// binding the user had, gone on one click, no undo, no prompt. The first
    /// click now arms; the second commits.
    pub(super) fn reset_all_keybindings(&mut self, cx: &mut Context<Self>) {
        if !self.reset_all_armed {
            self.reset_all_armed = true;
            cx.notify();
            return;
        }
        self.reset_all_armed = false;
        if let Some(model) = &mut self.keybindings_model {
            model.reset_all();
        }
        self.save_keybinding_settings();
        if let Some(overrides) = &mut self.editor_keybindings_overrides {
            overrides.clear();
        }
        self.keybinding_conflict = None;
        self.save_editor_keybindings_overrides();
        cx.notify();
    }

    /// Reset a single command's overrides back to defaults.
    pub(super) fn reset_keybinding(&mut self, command_id: String, cx: &mut Context<Self>) {
        if let Some(model) = &mut self.keybindings_model {
            model.reset(&command_id);
        }
        self.keybinding_conflict = None;
        self.save_keybinding_settings();
        cx.notify();
    }

    pub(super) fn handle_keybindings_search(&mut self, text: String, cx: &mut Context<Self>) {
        self.keybindings_search = text;
        cx.notify();
    }

    pub(super) fn render_keybindings(&self, cx: &mut Context<Self>) -> AnyElement {
        // The search text routing lives in the "kb-search" input's Change
        // subscription (see the constructor's spec table).
        let search_input = self.prefs_input("kb-search");

        let model = self.keybindings_model.as_ref();
        let total_count = model.map(|m| m.rows("").len()).unwrap_or(0);
        let override_count = model
            .map(|m| m.rows("").iter().filter(|r| r.has_overrides()).count())
            .unwrap_or(0);
        let profile_label = model
            .map(|m| m.profile.label().to_string())
            .unwrap_or_default();
        let active_profile = model.map(|model| model.profile);
        let profile_buttons = [
            (
                "kb-profile-native",
                crate::keybindings::ShortcutProfile::MacOSNative,
            ),
            ("kb-profile-ssms", crate::keybindings::ShortcutProfile::Ssms),
            (
                "kb-profile-custom",
                crate::keybindings::ShortcutProfile::Custom,
            ),
        ]
        .into_iter()
        .map(|(id, profile)| {
            Button::new(id)
                .label(profile.label())
                .ghost()
                .selected(active_profile == Some(profile))
                .on_click(cx.listener(move |this, _, _window, cx| {
                    this.set_shortcut_profile(profile, cx);
                }))
        })
        .collect::<Vec<_>>();

        // Build the filtered rows list for app-level commands.
        let rows: Vec<_> = match model {
            Some(m) => m.rows(&self.keybindings_search),
            None => Vec::new(),
        };
        let visible = rows.len();

        let capture_target = self.keybinding_capture.clone();

        // Build row elements for app-level commands.
        let mut row_elements: Vec<AnyElement> = Vec::with_capacity(rows.len());
        for row in &rows {
            let command_id = row.command.id.clone();
            let has_overrides = row.has_overrides();
            let command_display = row.command.title.clone();
            let active = row.active_display();
            let default = row.default_display();
            let active_display = if active.is_empty() {
                "—".to_string()
            } else {
                active
            };

            let is_capturing = capture_target
                .as_ref()
                .is_some_and(|(t, _)| t == &command_id);

            let this = cx.entity();
            let cmd_id = command_id.clone();
            row_elements.push(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(Theme::scaled(8.)))
                    .py(px(Theme::scaled(3.)))
                    .child(
                        div()
                            .min_w(px(Theme::scaled(200.)))
                            .text_size(px(Theme::font_size()))
                            .text_color(Theme::text())
                            .child(self.match_text(&command_display)),
                    )
                    .child(if is_capturing {
                        div()
                            .flex_1()
                            .text_size(px(Theme::font_size()))
                            .text_color(Theme::accent())
                            .child(crate::localization::text(
                                "settings.keybindings.child.press_keys_esc_cancel",
                            ))
                            .into_any_element()
                    } else {
                        let this = this.clone();
                        let cmd_id = cmd_id.clone();
                        div()
                            .flex_1()
                            .text_size(px(Theme::font_size()))
                            .text_color(if has_overrides {
                                Theme::accent()
                            } else {
                                Theme::text_dim()
                            })
                            .child(active_display)
                            .cursor(gpui::CursorStyle::PointingHand)
                            .on_mouse_down(gpui::MouseButton::Left, move |_ev, _window, app| {
                                this.update(app, |this, cx| {
                                    this.start_keybinding_capture(cmd_id.clone(), 0, cx);
                                });
                            })
                            .into_any_element()
                    })
                    .child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.92)))
                            .text_color(Theme::text_dim())
                            .child(self.match_text(&crate::localization::format(
                                "settings.keybindings.message.default_default",
                                &[("default", &(default).to_string())],
                            ))),
                    )
                    // Reset lives on its own element with its own hit area.
                    // It used to be `.when(has_overrides, |d| d.on_mouse_down(reset))`
                    // on the *row*, while the binding cell carried its own
                    // mouse-down to start capture — so clicking the binding you
                    // wanted to change started a capture and reset it in the
                    // same click. `stop_propagation` is what keeps the two
                    // handlers from ever stacking again.
                    .when(has_overrides, |d| {
                        d.child(self.quiet_button(
                            SharedString::from(format!("kb_reset_{command_id}")),
                            &crate::localization::text("settings.keybindings.quiet_button.reset"),
                            cx,
                            move |this, _window, cx| {
                                this.reset_keybinding(command_id.clone(), cx);
                            },
                        ))
                    })
                    .into_any_element(),
            );
        }

        // Build editor keybinding rows.
        let editor_defaults = crate::editor_keybindings::default_bindings();
        let editor_overrides = self.editor_keybindings_overrides.as_ref();
        let editor_search = self.keybindings_search.to_lowercase();
        let editor_rows: Vec<(&str, Vec<String>, Vec<String>, bool)> =
            crate::editor_keybindings::KNOWN_ACTIONS
                .iter()
                .copied()
                .filter(|name| {
                    editor_search.is_empty()
                        || crate::editor_keybindings::editor_action_display_name(name)
                            .to_lowercase()
                            .contains(&editor_search)
                        || name.to_lowercase().contains(&editor_search)
                })
                .map(|name| {
                    let defaults = editor_defaults.get(name).cloned().unwrap_or_default();
                    let active = editor_overrides
                        .and_then(|m| m.get(name).cloned())
                        .unwrap_or_else(|| defaults.clone());
                    let has_override = editor_overrides.is_some_and(|m| m.contains_key(name));
                    (name, defaults, active, has_override)
                })
                .collect();
        let editor_visible = editor_rows.len();

        let mut editor_row_elements: Vec<AnyElement> = Vec::with_capacity(editor_rows.len());
        for (action_name, defaults, active, has_override) in &editor_rows {
            let display_name = crate::editor_keybindings::editor_action_display_name(action_name);
            let active_display = if active.is_empty() {
                "—".to_string()
            } else {
                active.join(", ")
            };
            let default_display = if defaults.is_empty() {
                "—".to_string()
            } else {
                defaults.join(", ")
            };

            let is_capturing = capture_target
                .as_ref()
                .is_some_and(|(t, _)| t == &format!("editor:{action_name}"));

            let this = cx.entity();
            let act_name = action_name.to_string();
            editor_row_elements.push(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(Theme::scaled(8.)))
                    .py(px(Theme::scaled(3.)))
                    .child(
                        div()
                            .min_w(px(Theme::scaled(200.)))
                            .text_size(px(Theme::font_size()))
                            .text_color(Theme::text())
                            .child(self.match_text(&display_name)),
                    )
                    .child(if is_capturing {
                        div()
                            .flex_1()
                            .text_size(px(Theme::font_size()))
                            .text_color(Theme::accent())
                            .child(crate::localization::text(
                                "settings.keybindings.child.press_keys_esc_cancel_2",
                            ))
                            .into_any_element()
                    } else {
                        let this = this.clone();
                        let act = act_name.clone();
                        div()
                            .flex_1()
                            .text_size(px(Theme::font_size()))
                            .text_color(if *has_override {
                                Theme::accent()
                            } else {
                                Theme::text_dim()
                            })
                            .child(active_display.clone())
                            .cursor(gpui::CursorStyle::PointingHand)
                            .on_mouse_down(gpui::MouseButton::Left, move |_ev, _window, app| {
                                this.update(app, |this, cx| {
                                    this.start_keybinding_capture(format!("editor:{act}"), 0, cx);
                                });
                            })
                            .into_any_element()
                    })
                    .child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.92)))
                            .text_color(Theme::text_dim())
                            .child(self.match_text(&crate::localization::format(
                                "settings.keybindings.message.default_default_display",
                                &[("default_display", &(default_display).to_string())],
                            ))),
                    )
                    // Same row-level handler collision as the app-command rows
                    // above; same fix.
                    .when(*has_override, |d| {
                        let act = act_name.clone();
                        d.child(self.quiet_button(
                            SharedString::from(format!("kb_reset_editor_{act_name}")),
                            &crate::localization::text("settings.keybindings.quiet_button.reset_2"),
                            cx,
                            move |this, _window, cx| {
                                this.clear_editor_keybinding(act.clone(), 0, cx);
                            },
                        ))
                    })
                    .into_any_element(),
            );
        }

        let this = cx.entity();
        let this_for_capture = this.clone();
        let has_capture = capture_target.is_some();
        let pane =
            self.pane_container(vec![
                self.heading(&crate::localization::text(
                    "settings.keybindings.heading.keyboard_shortcuts",
                )),
                self.intro(&crate::localization::format(
                    "settings.keybindings.intro.summary",
                    &[
                        ("profile_label", profile_label.to_string()),
                        ("total_count", total_count.to_string()),
                        ("override_count", override_count.to_string()),
                        (
                            "editor_action_count",
                            crate::editor_keybindings::KNOWN_ACTIONS.len().to_string(),
                        ),
                    ],
                )),
                self.section_header(&crate::localization::text(
                    "settings.keybindings.section_header.profile",
                )),
                div()
                    .flex()
                    .flex_row()
                    .gap(px(Theme::scaled(6.)))
                    .children(profile_buttons)
                    .into_any_element(),
                div()
                    .flex()
                    .flex_col()
                    .gap(px(Theme::scaled(4.)))
                    .children(
                        self.keybinding_conflict
                            .as_ref()
                            .map(|message| self.warning(message)),
                    )
                    .when(self.keybindings_restart_required, |container| {
                        container.child(self.warning(&crate::localization::text(
                            "settings.keybindings.warning.restart_required",
                        )))
                    })
                    .into_any_element(),
                self.section_header(&crate::localization::text(
                    "settings.keybindings.section_header.search",
                )),
                div().child(search_input).into_any_element(),
                self.hint(&crate::localization::format(
                    "settings.keybindings.message.search_hint",
                    &[
                        ("visible", &(visible).to_string()),
                        ("total_count", &(total_count).to_string()),
                        ("editor_visible", &(editor_visible).to_string()),
                    ],
                )),
                self.section_header(&crate::localization::text(
                    "settings.keybindings.section_header.app_commands",
                )),
                div()
                    .id("kb-commands-list")
                    .flex()
                    .flex_col()
                    .gap(px(1.))
                    .max_h(px(Theme::scaled(300.)))
                    .overflow_y_scroll()
                    .children(row_elements)
                    .into_any_element(),
                self.section_header(&crate::localization::text(
                    "settings.keybindings.section_header.editor_actions",
                )),
                div()
                    .id("kb-editor-list")
                    .flex()
                    .flex_col()
                    .gap(px(1.))
                    .max_h(px(Theme::scaled(300.)))
                    .overflow_y_scroll()
                    .children(editor_row_elements)
                    .into_any_element(),
                div()
                    .flex()
                    .flex_col()
                    .gap(px(Theme::scaled(4.)))
                    .pt(px(Theme::scaled(12.)))
                    .child(div().flex().flex_row().gap(px(Theme::scaled(8.))).child(
                        self.quiet_button(
                            "kb-reset-all",
                            &if self.reset_all_armed {
                                crate::localization::text(
                                    "settings.keybindings.reset_all.confirm_button",
                                )
                            } else {
                                crate::localization::text("settings.keybindings.reset_all.button")
                            },
                            cx,
                            |this, _window, cx| this.reset_all_keybindings(cx),
                        ),
                    ))
                    .when(self.reset_all_armed, |d| {
                        d.child(self.warning(&crate::localization::text(
                            "settings.keybindings.warning.reset_all_warning",
                        )))
                    })
                    .into_any_element(),
            ]);

        // Key capture used to be attached to a plain `div().id("kb-capture")`
        // here. GPUI delivers key events along the *focus path*, and nothing in
        // this window ever held a `FocusHandle`, so "Press keys…" could sit
        // there forever while the keystrokes went to whatever had focus behind
        // the backdrop — most likely the SQL editor. The capture handler now
        // lives on the dialog root, which does hold focus; this function just
        // renders the pane.
        let _ = (has_capture, this_for_capture, this);
        pane
    }
}
