//! Preferences ▸ Editor ▸ Snippets & Quick Actions.
//!
//! Working copy is held on the window and written on Save. Shortcuts for
//! configured slots are assigned in the existing Keyboard Shortcuts pane.

use super::*;
use crate::snippets::{
    QuickAction, QuickActionTarget, SnippetPreferences, UserSnippet, parse_snippet,
};

impl PreferencesWindow {
    pub(super) fn apply_snippet_prefix(&mut self, value: String, cx: &mut Context<Self>) {
        if let Some(i) = self.selected_snippet
            && let Some(snippet) = self.snippet_prefs.snippets.get_mut(i)
        {
            snippet.prefix = value;
        }
        self.refresh_snippet_field_errors();
        cx.notify();
    }

    pub(super) fn apply_snippet_description(&mut self, value: String, cx: &mut Context<Self>) {
        if let Some(i) = self.selected_snippet
            && let Some(snippet) = self.snippet_prefs.snippets.get_mut(i)
        {
            snippet.description = value;
        }
        cx.notify();
    }

    pub(super) fn apply_snippet_body(&mut self, value: String, cx: &mut Context<Self>) {
        if let Some(i) = self.selected_snippet
            && let Some(snippet) = self.snippet_prefs.snippets.get_mut(i)
        {
            snippet.body = value;
        }
        self.refresh_snippet_field_errors();
        cx.notify();
    }

    pub(super) fn apply_quick_title(&mut self, value: String, cx: &mut Context<Self>) {
        if let Some(i) = self.selected_quick
            && let Some(action) = self.snippet_prefs.quick_actions.get_mut(i)
        {
            action.title = value;
        }
        cx.notify();
    }

    pub(super) fn apply_quick_body(&mut self, value: String, cx: &mut Context<Self>) {
        if let Some(i) = self.selected_quick
            && let Some(action) = self.snippet_prefs.quick_actions.get_mut(i)
        {
            action.body = value;
        }
        cx.notify();
    }

    fn refresh_snippet_field_errors(&mut self) {
        self.field_errors.remove("snippet-prefix");
        self.field_errors.remove("snippet-body");
        self.field_errors.remove("qa-slot");
        if let Some(i) = self.selected_snippet
            && let Some(snippet) = self.snippet_prefs.snippets.get(i)
        {
            if let Some(reason) = prefix_error(snippet, i, &self.snippet_prefs.snippets) {
                self.field_errors.insert("snippet-prefix", reason);
            }
            if let Err(error) = parse_snippet(&snippet.body) {
                self.field_errors.insert("snippet-body", error.to_string());
            }
        }
        if let Some(i) = self.selected_quick
            && let Some(action) = self.snippet_prefs.quick_actions.get(i)
            && let Some(reason) = slot_error(action, i, &self.snippet_prefs.quick_actions)
        {
            self.field_errors.insert("qa-slot", reason);
        }
    }

    fn select_snippet(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.selected_snippet = Some(index);
        let snippet = self.snippet_prefs.snippets.get(index).cloned();
        if let Some(snippet) = snippet {
            self.set_input_value("snippet-prefix", &snippet.prefix, window, cx);
            self.set_input_value("snippet-description", &snippet.description, window, cx);
            self.set_input_value("snippet-body", &snippet.body, window, cx);
        }
        self.refresh_snippet_field_errors();
        cx.notify();
    }

    fn select_quick(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.selected_quick = Some(index);
        let action = self.snippet_prefs.quick_actions.get(index).cloned();
        if let Some(action) = action {
            self.set_input_value("qa-title", &action.title, window, cx);
            self.set_input_value("qa-body", &action.body, window, cx);
        }
        self.refresh_snippet_field_errors();
        cx.notify();
    }

    fn add_snippet(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.snippet_prefs.snippets.push(UserSnippet::default());
        let index = self.snippet_prefs.snippets.len() - 1;
        self.select_snippet(index, window, cx);
    }

    fn delete_selected_snippet(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.selected_snippet else {
            return;
        };
        if index >= self.snippet_prefs.snippets.len() {
            return;
        }
        self.snippet_prefs.snippets.remove(index);
        if self.snippet_prefs.snippets.is_empty() {
            self.selected_snippet = None;
            self.set_input_value("snippet-prefix", "", window, cx);
            self.set_input_value("snippet-description", "", window, cx);
            self.set_input_value("snippet-body", "", window, cx);
        } else {
            self.select_snippet(index.min(self.snippet_prefs.snippets.len() - 1), window, cx);
        }
        self.refresh_snippet_field_errors();
        cx.notify();
    }

    fn add_quick_action(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let used: std::collections::HashSet<u8> = self
            .snippet_prefs
            .quick_actions
            .iter()
            .map(|action| action.slot)
            .collect();
        let Some(slot) = (1u8..=9).find(|slot| !used.contains(slot)) else {
            return;
        };
        self.snippet_prefs.quick_actions.push(QuickAction {
            slot,
            title: String::new(),
            body: String::new(),
            target: QuickActionTarget::ReplaceSelection,
        });
        let index = self.snippet_prefs.quick_actions.len() - 1;
        self.select_quick(index, window, cx);
    }

    fn delete_selected_quick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.selected_quick else {
            return;
        };
        if index >= self.snippet_prefs.quick_actions.len() {
            return;
        }
        self.snippet_prefs.quick_actions.remove(index);
        if self.snippet_prefs.quick_actions.is_empty() {
            self.selected_quick = None;
            self.set_input_value("qa-title", "", window, cx);
            self.set_input_value("qa-body", "", window, cx);
        } else {
            self.select_quick(
                index.min(self.snippet_prefs.quick_actions.len() - 1),
                window,
                cx,
            );
        }
        self.refresh_snippet_field_errors();
        cx.notify();
    }

    fn save_snippet_prefs(&mut self, cx: &mut Context<Self>) {
        self.refresh_snippet_field_errors();
        if let Some(reason) = validate_snippet_prefs(&self.snippet_prefs) {
            self.snippet_save_error = Some(reason);
            cx.notify();
            return;
        }
        self.snippet_save_error = None;
        self.snippet_prefs.version = 1;
        self.snippet_prefs.save();
        if let Some(model) = &mut self.keybindings_model {
            model.commands = crate::command_palette::CommandRegistry::default_registry()
                .all()
                .to_vec();
        }
        cx.emit(PreferencesWindowEvent::SnippetsChanged);
        cx.notify();
    }

    /// Reads a VS Code / Azure Data Studio snippet file into the working copy;
    /// the new snippets are kept once the pane is saved, like any other edit.
    #[cfg(not(target_arch = "wasm32"))]
    fn import_vscode_snippets(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(
                crate::localization::text("settings.snippets.vscode.import_prompt").into(),
            ),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = rx.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let parsed = std::fs::read_to_string(&path)
                .map_err(|error| {
                    crate::localization::format(
                        "settings.snippets.vscode.read_failed",
                        &[
                            ("path", &path.display().to_string()),
                            ("error", &error.to_string()),
                        ],
                    )
                })
                .and_then(|text| crate::snippets::import_vscode_snippets(&text));
            this.update(cx, |this, cx| {
                this.snippet_transfer_status = Some(match parsed {
                    Ok(imported) => {
                        let (added, skipped) = crate::snippets::merge_imported_snippets(
                            &mut this.snippet_prefs.snippets,
                            imported,
                        );
                        (
                            false,
                            crate::localization::format(
                                "settings.snippets.vscode.imported",
                                &[
                                    ("added", &added.to_string()),
                                    ("skipped", &skipped.to_string()),
                                ],
                            ),
                        )
                    }
                    Err(error) => (true, error),
                });
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn export_vscode_snippets(&mut self, cx: &mut Context<Self>) {
        let text = crate::snippets::export_vscode_snippets(&self.snippet_prefs.snippets);
        let count = self.snippet_prefs.snippets.len();
        let rx = cx.prompt_for_new_path(std::path::Path::new("."), Some("sqlly.code-snippets"));
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(path))) = rx.await else {
                return;
            };
            let status = match std::fs::write(&path, text) {
                Ok(()) => (
                    false,
                    crate::localization::format(
                        "settings.snippets.vscode.exported",
                        &[
                            ("count", &count.to_string()),
                            ("path", &path.display().to_string()),
                        ],
                    ),
                ),
                Err(error) => (
                    true,
                    crate::localization::format(
                        "settings.snippets.vscode.write_failed",
                        &[("error", &error.to_string())],
                    ),
                ),
            };
            this.update(cx, |this, cx| {
                this.snippet_transfer_status = Some(status);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn assign_quick_shortcut(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let title = self
            .selected_quick
            .and_then(|i| self.snippet_prefs.quick_actions.get(i))
            .map(|action| {
                if action.title.trim().is_empty() {
                    crate::localization::format(
                        "settings.snippets.message.quick_action_slot",
                        &[("slot", &(action.slot).to_string())],
                    )
                } else {
                    crate::localization::format(
                        "settings.snippets.message.quick_action",
                        &[("title", &(action.title).to_string())],
                    )
                }
            });
        self.select_pane(PreferencesPane::Keybindings, cx);
        if let Some(title) = title {
            self.set_input_value("kb-search", &title, window, cx);
            self.handle_keybindings_search(title, cx);
        }
    }

    pub(super) fn render_editor_snippets(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut items = Vec::new();
        items.push(self.intro(&crate::localization::text(
            "settings.snippets.intro.pane_intro",
        )));
        items.push(self.section_header(&crate::localization::text(
            "settings.snippets.section_header.snippets",
        )));
        items.push(self.snippet_list(cx));
        items.push(
            div()
                .flex()
                .flex_row()
                .gap(px(Theme::scaled(8.)))
                .child(self.quiet_button(
                    "snippet-add",
                    &crate::localization::text("settings.snippets.quiet_button.add"),
                    cx,
                    |this, window, cx| {
                        this.add_snippet(window, cx);
                    },
                ))
                .child(self.quiet_button(
                    "snippet-delete",
                    &crate::localization::text("settings.snippets.quiet_button.delete"),
                    cx,
                    |this, window, cx| this.delete_selected_snippet(window, cx),
                ))
                .into_any_element(),
        );
        #[cfg(not(target_arch = "wasm32"))]
        {
            items.push(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(Theme::scaled(8.)))
                    .child(self.quiet_button(
                        "snippet-import-vscode",
                        &crate::localization::text("settings.snippets.vscode.import_button"),
                        cx,
                        |this, _window, cx| this.import_vscode_snippets(cx),
                    ))
                    .child(self.quiet_button(
                        "snippet-export-vscode",
                        &crate::localization::text("settings.snippets.vscode.export_button"),
                        cx,
                        |this, _window, cx| this.export_vscode_snippets(cx),
                    ))
                    .into_any_element(),
            );
            if let Some((is_error, message)) = &self.snippet_transfer_status {
                items.push(
                    div()
                        .text_color(if *is_error {
                            Theme::error()
                        } else {
                            Theme::text_dim()
                        })
                        .text_size(px(Theme::font_size_ratio(0.92)))
                        .child(message.clone())
                        .into_any_element(),
                );
            }
        }
        if self.selected_snippet.is_some() {
            items.push(self.input_row(
                &crate::localization::text("settings.snippets.input_row.prefix"),
                "snippet-prefix",
            ));
            items.push(self.input_row(
                &crate::localization::text("settings.snippets.input_row.description"),
                "snippet-description",
            ));
            items.push(self.labeled_row(
                &crate::localization::text("settings.snippets.labeled_row.body"),
                self.prefs_input("snippet-body"),
            ));
            if let Some(message) = self.field_errors.get("snippet-body") {
                items.push(
                    div()
                        .text_color(Theme::error())
                        .text_size(px(Theme::font_size_ratio(0.92)))
                        .child(message.clone())
                        .into_any_element(),
                );
            }
        }

        items.push(self.section_header(&crate::localization::text(
            "settings.snippets.section_header.quick_actions",
        )));
        items.push(self.quick_list(cx));
        let slots_full = self.snippet_prefs.quick_actions.len() >= 9;
        items.push(
            div()
                .flex()
                .flex_row()
                .gap(px(Theme::scaled(8.)))
                .child(self.quiet_button(
                    "qa-add",
                    &crate::localization::text("settings.snippets.quiet_button.add_2"),
                    cx,
                    move |this, window, cx| {
                        if this.snippet_prefs.quick_actions.len() < 9 {
                            this.add_quick_action(window, cx);
                        }
                    },
                ))
                .child(self.quiet_button(
                    "qa-delete",
                    &crate::localization::text("settings.snippets.quiet_button.delete_2"),
                    cx,
                    |this, window, cx| {
                        this.delete_selected_quick(window, cx);
                    },
                ))
                .into_any_element(),
        );
        let _ = slots_full;
        if let Some(index) = self.selected_quick
            && let Some(action) = self.snippet_prefs.quick_actions.get(index).cloned()
        {
            items.push(self.labeled_row(
                &crate::localization::text("settings.snippets.labeled_row.slot"),
                self.enum_select(
                    "qa-slot",
                    action.slot,
                    (1u8..=9).collect(),
                    |slot| {
                        crate::localization::format(
                            "settings.snippets.message.slot_slot",
                            &[("slot", &(slot).to_string())],
                        )
                    },
                    |this, slot, cx| {
                        if let Some(i) = this.selected_quick
                            && let Some(current) = this.snippet_prefs.quick_actions.get_mut(i)
                        {
                            current.slot = slot;
                        }
                        this.refresh_snippet_field_errors();
                        cx.notify();
                    },
                    window,
                    cx,
                ),
            ));
            if let Some(message) = self.field_errors.get("qa-slot") {
                items.push(
                    div()
                        .text_color(Theme::error())
                        .text_size(px(Theme::font_size_ratio(0.92)))
                        .child(message.clone())
                        .into_any_element(),
                );
            }
            items.push(self.input_row(
                &crate::localization::text("settings.snippets.input_row.title"),
                "qa-title",
            ));
            items.push(self.labeled_row(
                &crate::localization::text("settings.snippets.labeled_row.target"),
                self.enum_select(
                    "qa-target",
                    action.target,
                    vec![
                        QuickActionTarget::ReplaceSelection,
                        QuickActionTarget::NewTab,
                        QuickActionTarget::RunInNewTab,
                    ],
                    |target| match target {
                        QuickActionTarget::ReplaceSelection => {
                            crate::localization::text("settings.snippets.option.replace_selection")
                        }
                        QuickActionTarget::NewTab => {
                            crate::localization::text("settings.snippets.option.new_tab")
                        }
                        QuickActionTarget::RunInNewTab => {
                            crate::localization::text("settings.snippets.option.run_new_tab")
                        }
                    },
                    |this, target, cx| {
                        if let Some(i) = this.selected_quick
                            && let Some(current) = this.snippet_prefs.quick_actions.get_mut(i)
                        {
                            current.target = target;
                        }
                        cx.notify();
                    },
                    window,
                    cx,
                ),
            ));
            items.push(self.labeled_row(
                &crate::localization::text("settings.snippets.labeled_row.body_2"),
                self.prefs_input("qa-body"),
            ));
            items.push(self.quiet_button(
                "qa-assign",
                &crate::localization::text("settings.snippets.quiet_button.assign_shortcut"),
                cx,
                |this, window, cx| this.assign_quick_shortcut(window, cx),
            ));
        }

        items.push(self.hint(&crate::localization::text(
            "settings.snippets.hint.placeholder_syntax",
        )));
        if let Some(error) = &self.snippet_save_error {
            items.push(
                div()
                    .text_color(Theme::error())
                    .text_size(px(Theme::font_size_ratio(0.92)))
                    .child(error.clone())
                    .into_any_element(),
            );
        }
        items.push(
            div()
                .flex()
                .flex_row()
                .pt(px(Theme::scaled(8.)))
                .child(
                    Button::new("snippet-save")
                        .label(crate::localization::text("settings.snippets.label.save"))
                        .primary()
                        .on_click(cx.listener(|this, _, _window, cx| {
                            this.save_snippet_prefs(cx);
                        })),
                )
                .into_any_element(),
        );
        self.pane_container(items)
    }

    fn snippet_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut list = div()
            .flex()
            .flex_col()
            .border_1()
            .border_color(Theme::border())
            .rounded(px(Theme::scaled(4.)));
        if self.snippet_prefs.snippets.is_empty() {
            list = list.child(
                div()
                    .h(px(Theme::scaled(26.)))
                    .px(px(Theme::scaled(8.)))
                    .flex()
                    .items_center()
                    .text_color(Theme::text_dim())
                    .text_size(px(Theme::font_size()))
                    .child(crate::localization::text(
                        "settings.snippets.child.no_snippets_yet",
                    )),
            );
        }
        for (index, snippet) in self.snippet_prefs.snippets.iter().enumerate() {
            let selected = self.selected_snippet == Some(index);
            let label = if snippet.prefix.is_empty() {
                crate::localization::text("settings.snippets.list.untitled")
            } else {
                snippet.prefix.clone()
            };
            list = list.child(self.prefs_list_row(
                format!("snippet-row-{index}"),
                label,
                selected,
                index > 0,
                cx,
                move |this, window, cx| this.select_snippet(index, window, cx),
            ));
        }
        list.into_any_element()
    }

    fn quick_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut list = div()
            .flex()
            .flex_col()
            .border_1()
            .border_color(Theme::border())
            .rounded(px(Theme::scaled(4.)));
        if self.snippet_prefs.quick_actions.is_empty() {
            list = list.child(
                div()
                    .h(px(Theme::scaled(26.)))
                    .px(px(Theme::scaled(8.)))
                    .flex()
                    .items_center()
                    .text_color(Theme::text_dim())
                    .text_size(px(Theme::font_size()))
                    .child(crate::localization::text(
                        "settings.snippets.child.no_quick_actions_yet",
                    )),
            );
        }
        for (index, action) in self.snippet_prefs.quick_actions.iter().enumerate() {
            let selected = self.selected_quick == Some(index);
            let title = if action.title.trim().is_empty() {
                crate::localization::format(
                    "settings.snippets.message.slot",
                    &[("slot", &(action.slot).to_string())],
                )
            } else {
                crate::localization::format(
                    "settings.snippets.message.slot_2",
                    &[
                        ("slot", &(action.slot).to_string()),
                        ("title", &(action.title).to_string()),
                    ],
                )
            };
            list = list.child(self.prefs_list_row(
                format!("qa-row-{index}"),
                title,
                selected,
                index > 0,
                cx,
                move |this, window, cx| this.select_quick(index, window, cx),
            ));
        }
        list.into_any_element()
    }

    fn prefs_list_row<F>(
        &self,
        id: String,
        label: String,
        selected: bool,
        hairline: bool,
        cx: &mut Context<Self>,
        on_select: F,
    ) -> AnyElement
    where
        F: Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    {
        let mut row = div()
            .id(id)
            .h(px(Theme::scaled(26.)))
            .px(px(Theme::scaled(8.)))
            .flex()
            .items_center()
            .cursor_pointer()
            .text_size(px(Theme::font_size()))
            .text_color(Theme::text())
            .bg(if selected {
                Theme::panel_bg()
            } else {
                Theme::bg()
            });
        if hairline {
            row = row.border_t_1().border_color(Theme::border());
        }
        if selected {
            row = row.border_l_1().border_color(Theme::accent());
        }
        row.child(self.match_text(&label))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, _ev, window, cx| {
                    cx.stop_propagation();
                    on_select(this, window, cx);
                }),
            )
            .into_any_element()
    }
}

fn prefix_error(snippet: &UserSnippet, index: usize, all: &[UserSnippet]) -> Option<String> {
    let prefix = snippet.prefix.trim();
    if prefix.is_empty() {
        return Some(
            crate::localization::text("settings.snippets.static.prefix_cannot_be_empty")
                .to_string(),
        );
    }
    if prefix.chars().any(char::is_whitespace) {
        return Some(
            crate::localization::text("settings.snippets.static.prefix_cannot_contain_spaces")
                .to_string(),
        );
    }
    let duplicate = all.iter().enumerate().any(|(other, candidate)| {
        other != index && candidate.prefix.eq_ignore_ascii_case(&snippet.prefix)
    });
    if duplicate {
        return Some(
            crate::localization::text("settings.snippets.static.prefix_must_be_unique").to_string(),
        );
    }
    None
}

fn slot_error(action: &QuickAction, index: usize, all: &[QuickAction]) -> Option<String> {
    if !(1..=9).contains(&action.slot) {
        return Some(
            crate::localization::text("settings.snippets.static.slot_must_be_1_through_9")
                .to_string(),
        );
    }
    let duplicate = all
        .iter()
        .enumerate()
        .any(|(other, candidate)| other != index && candidate.slot == action.slot);
    if duplicate {
        return Some(
            crate::localization::text("settings.snippets.static.duplicate_slot").to_string(),
        );
    }
    None
}

fn validate_snippet_prefs(prefs: &SnippetPreferences) -> Option<String> {
    for (index, snippet) in prefs.snippets.iter().enumerate() {
        if let Some(reason) = prefix_error(snippet, index, &prefs.snippets) {
            return Some(reason);
        }
        if let Err(error) = parse_snippet(&snippet.body) {
            return Some(error.to_string());
        }
    }
    for (index, action) in prefs.quick_actions.iter().enumerate() {
        if let Some(reason) = slot_error(action, index, &prefs.quick_actions) {
            return Some(reason);
        }
        if action.title.trim().is_empty() {
            return Some(
                crate::localization::text("settings.snippets.static.empty_quick_action_title")
                    .to_string(),
            );
        }
    }
    None
}
