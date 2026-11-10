//! The result-name context menu (Show by Default / Set Number of Rows /
//! Comment Out) and the Result Settings dialog the name's pencil opens.
//!
//! Both only gather intent: they emit
//! [`ResultsViewEvent::ResultSettingsRequested`] and the host, which owns the
//! editor buffer and the executed span, applies it as one undoable edit (see
//! `crate::result_header_actions` for the text logic).

use gpui_component::input::NumberInput;
use gpui_component::menu::{PopupMenu, PopupMenuItem};

use super::render::{
    CARD_FOOTER_ALLOWANCE, CARD_GAP, CARD_HEADER_ALLOWANCE, VIEW_TAB_STRIP_ALLOWANCE,
    adaptive_card_total_height, card_total_height_for_lines,
};
use super::*;
use crate::result_header_actions::{ResultSettingsChange, visible_row_count};

/// An open result-name context menu.
pub(crate) struct ResultNameMenu {
    pub menu: Entity<PopupMenu>,
    pub position: gpui::Point<gpui::Pixels>,
    _dismiss: Subscription,
}

/// The Result Settings dialog's state. Inputs need a `Window`, so they are
/// created on first render (like the snapshot prompt's).
pub(crate) struct ResultSettingsDialog {
    pub slot: usize,
    initial_name: String,
    initial_focus: bool,
    /// What the rows field started with (the directive's value, else the
    /// current visible count) — only a change from it is written back.
    initial_rows: String,
    focus: bool,
    name_input: Option<Entity<InputState>>,
    rows_input: Option<Entity<InputState>>,
    error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

const DIALOG_ID: &str = "result-settings-dialog";

impl ResultSettingsDialog {
    #[cfg(test)]
    pub fn new_for_test(slot: usize, name: &str, focus: bool, rows: &str) -> Self {
        Self {
            slot,
            initial_name: name.to_string(),
            initial_focus: focus,
            initial_rows: rows.to_string(),
            focus,
            name_input: None,
            rows_input: None,
            error: None,
            _subscriptions: Vec::new(),
        }
    }

    #[cfg(test)]
    pub fn name_input(&self) -> Option<Entity<InputState>> {
        self.name_input.clone()
    }

    #[cfg(test)]
    pub fn rows_input(&self) -> Option<Entity<InputState>> {
        self.rows_input.clone()
    }

    #[cfg(test)]
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// The change the dialog describes given the fields' current text: only
    /// what differs from how it opened is written. An invalid row count is a
    /// user-facing error.
    pub fn change_for(&self, name: &str, rows: &str) -> Result<ResultSettingsChange, String> {
        let name = name.trim();
        let rows = rows.trim();
        let lines = if rows == self.initial_rows.trim() {
            None
        } else if rows.is_empty() {
            Some(None)
        } else {
            match crate::result_layout::parse_lines_value(rows) {
                Some(n) => Some(Some(n)),
                None => {
                    return Err(crate::localization::format(
                        "results.result_settings.invalid_row_count",
                        &[("maximum_rows", &crate::result_layout::MAX_LINES.to_string())],
                    ));
                }
            }
        };
        Ok(ResultSettingsChange {
            slot: self.slot,
            name: (!name.is_empty() && name != self.initial_name).then(|| name.to_string()),
            focus: (self.focus != self.initial_focus).then_some(self.focus),
            lines,
            comment_out: false,
        })
    }
}

impl ResultsView {
    /// Rows of result `slot` currently in view, counting a partly visible row
    /// only when more than a quarter of it shows. Read from the grid's own
    /// viewport when it has been painted; otherwise estimated from the
    /// stacked card's height.
    pub(crate) fn visible_row_count_for_slot(&self, slot: usize, cx: &App) -> usize {
        let total = self.row_count_for_slot(slot);
        if let Some(grid) = self.grid_entity_for_slot(slot) {
            let state = grid.read(cx).state.read(cx);
            let viewport = f32::from(state.bounds.size.height) - state.header_height;
            if viewport > 0.0 && state.row_height > 0.0 {
                return visible_row_count(
                    f32::from(state.scroll_handle.offset().y),
                    viewport,
                    state.row_height,
                    state.display_row_count(),
                );
            }
        }
        let card = self
            .stacked_card_heights
            .get(slot)
            .copied()
            .flatten()
            .or_else(|| {
                self.layout_directives
                    .lines_for(slot)
                    .map(|lines| card_total_height_for_lines(lines, self.live_grid_metrics()))
            })
            .unwrap_or_else(|| adaptive_card_total_height(total));
        // Card chrome plus the grid's own column-header row.
        let metrics = self.live_grid_metrics();
        let viewport = card
            - CARD_HEADER_ALLOWANCE
            - VIEW_TAB_STRIP_ALLOWANCE
            - CARD_FOOTER_ALLOWANCE
            - CARD_GAP * 2.0
            - metrics.header_height;
        visible_row_count(0.0, viewport, metrics.row_height, total)
    }

    /// Hand a requested change to the host, which owns the editor buffer.
    /// Nothing is mirrored here: the host calls
    /// [`Self::confirm_result_settings`] once the query was actually
    /// edited, so a refused request (a read-only snapshot tab, a result the
    /// statement mapping can't place) leaves the view showing what the
    /// query really says.
    pub(crate) fn request_result_settings(
        &mut self,
        change: ResultSettingsChange,
        cx: &mut Context<Self>,
    ) {
        cx.emit(ResultsViewEvent::ResultSettingsRequested(change));
    }

    /// Mirror a change the host has written into the query, so the label,
    /// focus and allocation take effect without a re-run.
    pub(crate) fn confirm_result_settings(
        &mut self,
        change: &ResultSettingsChange,
        cx: &mut Context<Self>,
    ) {
        let slot = change.slot;
        if let Some(name) = change.name.as_deref().map(str::trim)
            && !name.is_empty()
        {
            self.result_set_names.insert(slot, name.to_string());
        }
        match change.focus {
            Some(true) => self.layout_directives.focus = Some(slot + 1),
            Some(false) if self.layout_directives.focus == Some(slot + 1) => {
                self.layout_directives.focus = None;
            }
            _ => {}
        }
        match change.lines {
            Some(Some(rows)) => {
                let rows = rows.clamp(1, crate::result_layout::MAX_LINES);
                let lines = &mut self.layout_directives.lines;
                match lines.iter_mut().find(|(index, _)| *index == slot + 1) {
                    Some(entry) => entry.1 = rows,
                    None => lines.push((slot + 1, rows)),
                }
            }
            Some(None) => self
                .layout_directives
                .lines
                .retain(|(index, _)| *index != slot + 1),
            None => {}
        }
        cx.notify();
    }

    pub(crate) fn close_result_name_menu(&mut self, cx: &mut Context<Self>) {
        if self.result_name_menu.take().is_some() {
            cx.notify();
        }
    }

    /// Right-click on a result's name: Show by Default, Set Number of Rows
    /// (to what is visible now), Comment Out Result, and Result Settings….
    pub(crate) fn open_result_name_menu(
        &mut self,
        slot: usize,
        position: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let visible = self.visible_row_count_for_slot(slot, cx).max(1);
        let is_default = self.layout_directives.focus_slot() == Some(slot);
        let this = cx.entity();
        let menu = PopupMenu::build(window, cx, move |menu, _window, _cx| {
            let focus_this = this.clone();
            let rows_this = this.clone();
            let comment_this = this.clone();
            let settings_this = this.clone();
            menu.item(
                PopupMenuItem::new(crate::localization::text(
                    "results.result_name_menu.show_by_default",
                ))
                .checked(is_default)
                .disabled(is_default)
                .on_click(move |_ev, _w, app| {
                    focus_this.update(app, |view, cx| {
                        view.close_result_name_menu(cx);
                        view.request_result_settings(
                            ResultSettingsChange {
                                slot,
                                focus: Some(true),
                                ..Default::default()
                            },
                            cx,
                        );
                    });
                }),
            )
            .item(
                PopupMenuItem::new(crate::localization::format(
                    "results.result_name_menu.set_visible_rows",
                    &[("visible_rows", &visible.to_string())],
                ))
                .on_click(move |_ev, _w, app| {
                    rows_this.update(app, |view, cx| {
                        view.close_result_name_menu(cx);
                        view.request_result_settings(
                            ResultSettingsChange {
                                slot,
                                lines: Some(Some(visible)),
                                ..Default::default()
                            },
                            cx,
                        );
                    });
                }),
            )
            .item(
                PopupMenuItem::new(crate::localization::text(
                    "results.result_name_menu.comment_out",
                ))
                .on_click(move |_ev, _w, app| {
                    comment_this.update(app, |view, cx| {
                        view.close_result_name_menu(cx);
                        view.request_result_settings(
                            ResultSettingsChange {
                                slot,
                                comment_out: true,
                                ..Default::default()
                            },
                            cx,
                        );
                    });
                }),
            )
            .separator()
            .item(
                PopupMenuItem::new(crate::localization::text(
                    "results.result_name_menu.open_settings",
                ))
                .on_click(move |_ev, _w, app| {
                    settings_this.update(app, |view, cx| {
                        view.close_result_name_menu(cx);
                        view.open_result_settings(slot, cx);
                    });
                }),
            )
        });
        let dismiss = cx.subscribe(&menu, |this, _menu, _: &gpui::DismissEvent, cx| {
            this.close_result_name_menu(cx);
        });
        // No tooltips while the menu is up — see `crate::tooltip`.
        crate::tooltip::menu_opened(cx);
        self.result_name_menu = Some(ResultNameMenu {
            menu,
            position,
            _dismiss: dismiss,
        });
        cx.notify();
    }

    /// The open name menu as a window-level overlay.
    pub(crate) fn render_result_name_menu(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let open = self.result_name_menu.as_ref()?;
        let entity = cx.entity();
        Some(
            deferred(
                anchored()
                    .position(point(open.position.x, open.position.y))
                    .child(div().occlude().child(open.menu.clone()).on_mouse_down_out(
                        move |_ev: &MouseDownEvent, _window, app| {
                            entity.update(app, |this, cx| this.close_result_name_menu(cx));
                        },
                    )),
            )
            .with_priority(1)
            .into_any_element(),
        )
    }

    /// Open the Result Settings dialog for `slot` (the name's pencil).
    pub fn open_result_settings(&mut self, slot: usize, cx: &mut Context<Self>) {
        let focus = self.layout_directives.focus_slot() == Some(slot);
        let rows = self
            .layout_directives
            .lines
            .iter()
            .find(|(index, _)| *index == slot + 1)
            .map(|(_, rows)| *rows)
            .unwrap_or_else(|| self.visible_row_count_for_slot(slot, cx).max(1));
        self.result_settings = Some(ResultSettingsDialog {
            slot,
            initial_name: self.result_set_label(slot),
            initial_focus: focus,
            initial_rows: rows.to_string(),
            focus,
            name_input: None,
            rows_input: None,
            error: None,
            _subscriptions: Vec::new(),
        });
        cx.notify();
    }

    pub(crate) fn close_result_settings(&mut self, cx: &mut Context<Self>) {
        if self.result_settings.take().is_some() {
            cx.notify();
        }
    }

    /// The change the dialog's current values describe, or a user-facing
    /// error for an invalid row count.
    fn result_settings_change(&self, cx: &App) -> Result<ResultSettingsChange, String> {
        let Some(dialog) = self.result_settings.as_ref() else {
            return Err(crate::localization::text(
                "results.result_settings.no_result_editing",
            ));
        };
        let read = |input: &Option<Entity<InputState>>, fallback: &str| {
            input
                .as_ref()
                .map(|i| i.read(cx).value().to_string())
                .unwrap_or_else(|| fallback.to_string())
        };
        let name = read(&dialog.name_input, &dialog.initial_name);
        let rows = read(&dialog.rows_input, &dialog.initial_rows);
        dialog.change_for(&name, &rows)
    }

    pub(crate) fn set_result_settings_focus(&mut self, focus: bool, cx: &mut Context<Self>) {
        if let Some(dialog) = &mut self.result_settings {
            dialog.focus = focus;
        }
        cx.notify();
    }

    pub(crate) fn save_result_settings(&mut self, cx: &mut Context<Self>) {
        match self.result_settings_change(cx) {
            Ok(change) => {
                self.close_result_settings(cx);
                let untouched =
                    change.name.is_none() && change.focus.is_none() && change.lines.is_none();
                if !untouched {
                    self.request_result_settings(change, cx);
                }
            }
            Err(error) => {
                if let Some(dialog) = &mut self.result_settings {
                    dialog.error = Some(error);
                }
                cx.notify();
            }
        }
    }

    fn comment_out_from_settings(&mut self, cx: &mut Context<Self>) {
        let Some(slot) = self.result_settings.as_ref().map(|d| d.slot) else {
            return;
        };
        self.close_result_settings(cx);
        self.request_result_settings(
            ResultSettingsChange {
                slot,
                comment_out: true,
                ..Default::default()
            },
            cx,
        );
    }

    /// The Result Settings dialog, lifted to the window layer.
    pub(crate) fn render_result_settings(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let dialog = self.result_settings.as_mut()?;
        if dialog.name_input.is_none() {
            let initial_name = dialog.initial_name.clone();
            let initial_rows = dialog.initial_rows.clone();
            let name = cx.new(|cx| InputState::new(window, cx).default_value(initial_name));
            let rows = cx.new(|cx| {
                InputState::new(window, cx)
                    .validate(|text, _| text.bytes().all(|b| b.is_ascii_digit()))
                    .step(1.0)
                    .min(1.0)
                    .max(crate::result_layout::MAX_LINES as f64)
                    .default_value(initial_rows)
            });
            fn enter(
                this: &mut ResultsView,
                _: Entity<InputState>,
                event: &InputEvent,
                cx: &mut Context<ResultsView>,
            ) {
                if let InputEvent::PressEnter { .. } = event {
                    this.save_result_settings(cx);
                }
            }
            let subs = vec![cx.subscribe(&name, enter), cx.subscribe(&rows, enter)];
            name.update(cx, |state, cx| state.focus(window, cx));
            if let Some(dialog) = &mut self.result_settings {
                dialog.name_input = Some(name);
                dialog.rows_input = Some(rows);
                dialog._subscriptions = subs;
            }
        }
        let dialog = self.result_settings.as_ref()?;
        let name_input = dialog.name_input.clone()?;
        let rows_input = dialog.rows_input.clone()?;
        let focus = dialog.focus;
        let slot = dialog.slot;
        let error = dialog.error.clone();
        let s = Theme::scaled;
        let label = |text: String| {
            div()
                .text_size(px(Theme::font_size_ratio(0.85)))
                .text_color(Theme::text_dim())
                .child(text)
        };
        let hint = |text: String| {
            div()
                .text_size(px(Theme::font_size_ratio(0.8)))
                .text_color(Theme::text_dim())
                .child(text)
        };
        let built = crate::dialog::Dialog::new(DIALOG_ID, |this: &mut Self| {
            this.dialog_frames.get(DIALOG_ID, || {
                crate::dialog::DialogFrame::new_auto(Theme::scaled(440.0))
            })
        })
        .title(crate::localization::format(
            "results.result_settings.dialog_title",
            &[("result_number", &(slot + 1).to_string())],
        ))
        .use_default_close(false)
        .on_close(|this, _window, cx| this.close_result_settings(cx))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(s(6.0)))
                .child(label(crate::localization::text(
                    "results.result_settings.name_label",
                )))
                .child(Input::new(&name_input)),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(s(4.0)))
                .child(
                    Checkbox::new("result-settings-default")
                        .label(crate::localization::text(
                            "results.result_settings.show_by_default",
                        ))
                        .checked(focus)
                        .on_click(cx.listener(|this, checked: &bool, _w, cx| {
                            this.set_result_settings_focus(*checked, cx);
                        })),
                )
                .child(hint(crate::localization::text(
                    "results.result_settings.show_by_default_hint",
                ))),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(s(6.0)))
                .child(label(crate::localization::text(
                    "results.result_settings.visible_rows_label",
                )))
                .child(NumberInput::new(&rows_input))
                .child(hint(crate::localization::format(
                    "results.result_settings.visible_rows_hint",
                    &[("maximum_rows", &crate::result_layout::MAX_LINES.to_string())],
                ))),
        )
        .children_body(error.map(|error| {
            div()
                .text_size(px(Theme::font_size_ratio(0.82)))
                .text_color(Theme::error())
                .child(error)
        }))
        .leading_button(
            Button::new("result-settings-comment-out")
                .label(crate::localization::text(
                    "results.result_settings.comment_out_button",
                ))
                .tooltip(crate::localization::text(
                    "results.result_settings.comment_out_tooltip",
                ))
                .on_click(cx.listener(|this, _ev, _w, cx| this.comment_out_from_settings(cx))),
        )
        .button(
            Button::new("result-settings-cancel")
                .label(crate::localization::text(
                    "results.result_settings.cancel_button",
                ))
                .on_click(cx.listener(|this, _ev, _w, cx| this.close_result_settings(cx))),
        )
        .button(
            Button::new("result-settings-save")
                .primary()
                .label(crate::localization::text(
                    "results.result_settings.save_button",
                ))
                .on_click(cx.listener(|this, _ev, _w, cx| this.save_result_settings(cx))),
        )
        .build(self, window, cx);
        Some(crate::dialog::window_layer(built, window))
    }
}
