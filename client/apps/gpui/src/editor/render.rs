use super::*;

use crate::dialog::{Dialog, DialogFrame};
use crate::scrollbars::DraggableScrollbar;

impl Focusable for SqlEditor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SqlEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.needs_focus {
            self.focus_handle.focus(window, cx);
            self.needs_focus = false;
        }

        // Scrollbar geometry (same indicator style as the schema tree): the
        // tracks are ALWAYS visible; a thumb renders only when the content
        // actually overflows on that axis, so an empty track reads as a
        // disabled scrollbar.
        let scrollbar_w = 12.0_f32;
        let viewport_h = f32::from(self.scroll_handle.bounds().size.height);
        let max_scroll_y = f32::from(self.scroll_handle.max_offset().y);
        let offset_y = f32::from(self.scroll_handle.offset().y);
        let (v_scrollable, v_thumb_h, v_thumb_y) = if max_scroll_y > 0.0 && viewport_h > 0.0 {
            let content_total = max_scroll_y + viewport_h;
            let thumb_h = ((viewport_h * viewport_h) / content_total).max(24.0);
            let pct = (-offset_y / max_scroll_y).clamp(0.0, 1.0);
            (true, thumb_h, pct * (viewport_h - thumb_h))
        } else {
            (false, 0.0, 0.0)
        };
        let viewport_w = f32::from(self.scroll_handle.bounds().size.width);
        let max_scroll_x = f32::from(self.scroll_handle.max_offset().x);
        let offset_x = f32::from(self.scroll_handle.offset().x);
        let (h_scrollable, h_thumb_w, h_thumb_x) = if max_scroll_x > 0.0 && viewport_w > 0.0 {
            let content_total = max_scroll_x + viewport_w;
            let thumb_w = ((viewport_w * viewport_w) / content_total).max(24.0);
            let pct = (-offset_x / max_scroll_x).clamp(0.0, 1.0);
            (true, thumb_w, pct * (viewport_w - thumb_w))
        } else {
            (false, 0.0, 0.0)
        };

        let scroll_area = div()
            .key_context("SqlEditor")
            .id("sql-editor-scroll")
            .flex()
            .flex_col()
            .size_full()
            .overflow_scroll()
            .track_scroll(&self.scroll_handle)
            // Keep the last line / end of long lines clear of the scrollbar
            // tracks overlaid by the wrapper below.
            .pr(px(scrollbar_w))
            .pb(px(scrollbar_w))
            .bg(Theme::bg())
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::handle_backspace))
            .on_action(cx.listener(Self::handle_delete))
            .on_action(cx.listener(Self::handle_left))
            .on_action(cx.listener(Self::handle_right))
            .on_action(cx.listener(Self::handle_up))
            .on_action(cx.listener(Self::handle_down))
            .on_action(cx.listener(Self::handle_home))
            .on_action(cx.listener(Self::handle_end))
            .on_action(cx.listener(Self::handle_select_left))
            .on_action(cx.listener(Self::handle_select_right))
            .on_action(cx.listener(Self::handle_select_up))
            .on_action(cx.listener(Self::handle_select_down))
            .on_action(cx.listener(Self::handle_select_home))
            .on_action(cx.listener(Self::handle_select_end))
            .on_action(cx.listener(Self::handle_select_all))
            .on_action(cx.listener(Self::handle_enter))
            .on_action(cx.listener(Self::handle_shift_enter))
            .on_action(cx.listener(Self::handle_tab))
            .on_action(cx.listener(Self::handle_shift_tab))
            .on_action(cx.listener(Self::handle_execute))
            .on_action(cx.listener(Self::handle_cancel))
            .on_action(cx.listener(Self::handle_dismiss))
            .on_action(cx.listener(Self::handle_open_context_menu))
            .on_action(cx.listener(Self::handle_trigger_completions))
            .on_action(cx.listener(Self::handle_trigger_value_lookup))
            .on_action(cx.listener(Self::handle_refresh_dates))
            .on_action(cx.listener(Self::handle_trigger_ai_completion))
            .on_action(cx.listener(Self::handle_fold))
            .on_action(cx.listener(Self::handle_unfold))
            .on_action(cx.listener(Self::handle_fold_all))
            .on_action(cx.listener(Self::handle_unfold_all))
            .on_action(cx.listener(Self::handle_capture_intellisense_feedback))
            .on_action(cx.listener(Self::handle_insert_distinct_values_as_in_list))
            .on_action(cx.listener(Self::handle_paste_as_in_list))
            .on_action(cx.listener(Self::handle_convert_delimited_list))
            .on_action(cx.listener(Self::handle_validate))
            .on_action(cx.listener(Self::handle_format_sql))
            .on_action(cx.listener(Self::handle_format_document))
            .on_action(cx.listener(Self::handle_minify_sql))
            .on_action(cx.listener(Self::handle_page_down))
            .on_action(cx.listener(Self::handle_page_up))
            .on_action(cx.listener(Self::handle_select_page_down))
            .on_action(cx.listener(Self::handle_select_page_up))
            .on_action(cx.listener(Self::handle_toggle_line_comment))
            .on_action(cx.listener(Self::handle_add_line_comment))
            .on_action(cx.listener(Self::handle_remove_line_comment))
            .on_action(cx.listener(Self::handle_move_word_left))
            .on_action(cx.listener(Self::handle_select_word_left))
            .on_action(cx.listener(Self::handle_move_word_right))
            .on_action(cx.listener(Self::handle_select_word_right))
            .on_action(cx.listener(Self::handle_delete_word_backward))
            .on_action(cx.listener(Self::handle_delete_word_forward))
            .on_action(cx.listener(Self::handle_move_hump_left))
            .on_action(cx.listener(Self::handle_select_hump_left))
            .on_action(cx.listener(Self::handle_move_hump_right))
            .on_action(cx.listener(Self::handle_select_hump_right))
            .on_action(cx.listener(Self::handle_move_to_beginning_of_document))
            .on_action(cx.listener(Self::handle_select_to_beginning_of_document))
            .on_action(cx.listener(Self::handle_move_to_end_of_document))
            .on_action(cx.listener(Self::handle_select_to_end_of_document))
            .on_action(cx.listener(Self::handle_delete_to_beginning_of_line))
            .on_action(cx.listener(Self::handle_delete_to_end_of_line))
            .on_action(cx.listener(Self::handle_copy))
            .on_action(cx.listener(Self::handle_cut))
            .on_action(cx.listener(Self::handle_paste))
            .on_action(cx.listener(Self::handle_undo))
            .on_action(cx.listener(Self::handle_redo))
            .on_action(cx.listener(Self::handle_find))
            .on_action(cx.listener(Self::handle_replace))
            .on_action(cx.listener(Self::handle_find_next))
            .on_action(cx.listener(Self::handle_find_prev))
            .on_action(cx.listener(Self::handle_add_cursor_for_next_occurrence))
            .on_action(cx.listener(Self::handle_select_all_occurrences))
            .on_action(cx.listener(Self::handle_add_cursor_above))
            .on_action(cx.listener(Self::handle_add_cursor_below))
            .on_action(cx.listener(Self::handle_column_select_up))
            .on_action(cx.listener(Self::handle_column_select_down))
            .on_action(cx.listener(Self::handle_go_to_line))
            .on_action(cx.listener(Self::handle_go_to_next_error))
            .on_action(cx.listener(Self::handle_qualify_identifier))
            .on_action(cx.listener(Self::handle_move_line_up))
            .on_action(cx.listener(Self::handle_move_line_down))
            .on_action(cx.listener(Self::handle_duplicate_line))
            .on_action(cx.listener(Self::handle_join_lines))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(Self::handle_mouse_down),
            )
            .on_mouse_down(
                gpui::MouseButton::Right,
                cx.listener(Self::handle_context_mouse_down),
            )
            .on_mouse_down(
                gpui::MouseButton::Middle,
                cx.listener(Self::handle_middle_click),
            )
            .on_mouse_move(cx.listener(Self::handle_mouse_move))
            .on_hover(cx.listener(|this, hovered: &bool, _window, cx| {
                if !*hovered {
                    this.handle_pointer_left(cx);
                }
            }))
            .on_scroll(cx.listener(Self::handle_scroll_wheel))
            .on_pinch(cx.listener(Self::handle_pinch))
            .on_mouse_up(gpui::MouseButton::Left, cx.listener(Self::handle_mouse_up))
            .on_modifiers_changed(cx.listener(Self::handle_modifiers_changed))
            // Text surface: show the I-beam so the mouse reads as "click to
            // place the caret", not the default arrow.
            .cursor(gpui::CursorStyle::IBeam)
            // While the DDL-navigation modifier is held over a navigable
            // symbol, show the pointing hand so the "link" reads as
            // clickable.
            .when(self.link_hover_range.is_some(), |d| {
                d.cursor(gpui::CursorStyle::PointingHand)
            })
            .child(EditorElement {
                editor: cx.entity(),
            });

        let context_overlay = self.context_menu.as_ref().map(|menu| {
            let popup = menu.menu.clone();
            let position = menu.position;
            let dismiss = cx.entity();
            deferred(anchored().position(point(position.x, position.y)).child(
                div().occlude().child(popup).on_mouse_down_out(
                    move |_event: &MouseDownEvent, _window, app| {
                        dismiss.update(app, |editor, cx| {
                            editor.context_menu = None;
                            cx.notify();
                        });
                    },
                ),
            ))
            .with_priority(2)
            .into_any_element()
        });

        let format_overlay = self.format_dialog.clone().map(|dialog| {
            let entity = cx.entity();
            let choices = dialog.choices.iter().enumerate().fold(
                div().flex().flex_col().gap_2().text_color(Theme::text()),
                |choices, (index, choice)| {
                    let entity = entity.clone();
                    choices.child(
                        div()
                            .id(("format-choice", index))
                            .px_3()
                            .py_2()
                            .rounded_md()
                            .border_1()
                            .border_color(Theme::border())
                            // Keyboard selection takes the accent-based selected
                            // fill; hover is the subtler row-hover wash, so a
                            // hovered choice never reads as the selected one.
                            .when(index == dialog.selected, |d| {
                                d.bg(Theme::completion_selected())
                            })
                            .hover(|style| style.bg(Theme::row_hover()))
                            .cursor_pointer()
                            .child(choice.label.clone())
                            .on_mouse_down(gpui::MouseButton::Left, move |_event, window, app| {
                                entity.update(app, |editor, cx| {
                                    editor.apply_format_choice(index, window, cx)
                                });
                            }),
                    )
                },
            );
            // Up/Down/Enter selection stays on the editor's own action
            // handlers: focus never moves into the card, exactly as before.
            Dialog::new("editor-column-format", |this: &mut Self| {
                this.dialog_frames.get("editor-column-format", || {
                    DialogFrame::new_auto(Theme::scaled(420.0))
                })
            })
            .title(crate::localization::format(
                "editor.render.format_dialog_title",
                &[("column", &dialog.column)],
            ))
            .dismiss_on_scrim_click(false)
            .on_close(|this, _window, cx| {
                this.format_dialog = None;
                cx.notify();
            })
            .child(choices)
            .build(self, window, cx)
            .into_any_element()
        });

        let picker_overlay = self
            .formatter_picker
            .clone()
            .map(|dialog| dialog.into_any_element());

        // Built in two stages: the body and footer buttons first (immutably
        // borrowing the dialog state), then the chrome, which needs `self`
        // mutably for the frame accessor.
        let convert_parts =
            self.convert_dialog.as_ref().map(|dialog| {
                use crate::list_tools::{InputDelimiter, Quote};

                let s = Theme::scaled;
                let entity = cx.entity();
                let preview_result = dialog.preview(cx);
                let preview_is_err = preview_result.is_err();
                let preview_text = match preview_result {
                    Ok(text) if text.chars().count() > 2000 => {
                        format!("{}…", text.chars().take(2000).collect::<String>())
                    }
                    Ok(text) => text,
                    Err(err) => err,
                };
                let preview_color = if preview_is_err {
                    Theme::warning()
                } else {
                    Theme::text()
                };
                let _replace_enabled = !preview_is_err;

                let delim_row = [
                    (
                        InputDelimiter::Auto,
                        crate::localization::text("editor.render.delimiter_auto"),
                    ),
                    (
                        InputDelimiter::Comma,
                        crate::localization::text("editor.render.delimiter_comma"),
                    ),
                    (
                        InputDelimiter::Newline,
                        crate::localization::text("editor.render.delimiter_newline"),
                    ),
                    (
                        InputDelimiter::Tab,
                        crate::localization::text("editor.render.delimiter_tab"),
                    ),
                    (
                        InputDelimiter::Semicolon,
                        crate::localization::text("editor.render.delimiter_semicolon"),
                    ),
                    (
                        InputDelimiter::Space,
                        crate::localization::text("editor.render.delimiter_space"),
                    ),
                ]
                .into_iter()
                .fold(
                    div().flex().flex_row().gap(px(s(4.0))),
                    |row, (delim, label)| {
                        let entity = entity.clone();
                        let selected = dialog.input_delimiter == delim;
                        row.child(
                            Button::new(label.clone())
                                .label(label)
                                .ghost()
                                .xsmall()
                                .selected(selected)
                                .on_click(move |_, _, cx| {
                                    entity.update(cx, |editor, cx| {
                                        if let Some(dialog) = editor.convert_dialog.as_mut() {
                                            dialog.input_delimiter = delim;
                                            cx.notify();
                                        }
                                    });
                                }),
                        )
                    },
                );

                let quote_row = [Quote::None, Quote::Single, Quote::Double]
                    .into_iter()
                    .fold(div().flex().flex_row().gap(px(s(4.0))), |row, quote| {
                        let entity = entity.clone();
                        let label = match quote {
                            Quote::None => crate::localization::text("editor.render.quote_none"),
                            Quote::Single => {
                                crate::localization::text("editor.render.quote_single")
                            }
                            Quote::Double => {
                                crate::localization::text("editor.render.quote_double")
                            }
                        };
                        let selected = dialog.quote == quote;
                        row.child(
                            Button::new(label.clone())
                                .label(label)
                                .ghost()
                                .xsmall()
                                .selected(selected)
                                .on_click(move |_, _, cx| {
                                    entity.update(cx, |editor, cx| {
                                        if let Some(dialog) = editor.convert_dialog.as_mut() {
                                            dialog.quote = quote;
                                            cx.notify();
                                        }
                                    });
                                }),
                        )
                    });

                let dedupe = dialog.dedupe;
                let dedupe_entity = entity.clone();

                let body =
                    div()
                        .text_color(Theme::text())
                        .flex()
                        .flex_col()
                        .gap(px(s(8.0)))
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(s(8.0)))
                                .child(div().w(px(s(140.0))).text_color(Theme::text_dim()).child(
                                    crate::localization::text(
                                        "editor.render.output_delimiter_label",
                                    ),
                                ))
                                .child(Input::new(&dialog.output_delimiter)),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(s(8.0)))
                                .child(
                                    div().w(px(s(140.0))).text_color(Theme::text_dim()).child(
                                        crate::localization::text("editor.render.prefix_label"),
                                    ),
                                )
                                .child(Input::new(&dialog.prefix)),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(s(8.0)))
                                .child(
                                    div().w(px(s(140.0))).text_color(Theme::text_dim()).child(
                                        crate::localization::text("editor.render.suffix_label"),
                                    ),
                                )
                                .child(Input::new(&dialog.suffix)),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(s(8.0)))
                                .child(div().w(px(s(140.0))).text_color(Theme::text_dim()).child(
                                    crate::localization::text("editor.render.wrap_width_label"),
                                ))
                                .child(Input::new(&dialog.wrap_width)),
                        )
                        .child(div().text_color(Theme::text_dim()).child(
                            crate::localization::text("editor.render.input_delimiter_label"),
                        ))
                        .child(delim_row)
                        .child(
                            div()
                                .text_color(Theme::text_dim())
                                .child(crate::localization::text("editor.render.quote_label")),
                        )
                        .child(quote_row)
                        .child(
                            Checkbox::new("convert-dedupe")
                                .label(crate::localization::text("editor.render.dedupe_label"))
                                .checked(dedupe)
                                .on_click(move |checked: &bool, _, cx| {
                                    let checked = *checked;
                                    dedupe_entity.update(cx, |editor, cx| {
                                        if let Some(dialog) = editor.convert_dialog.as_mut() {
                                            dialog.dedupe = checked;
                                            cx.notify();
                                        }
                                    });
                                }),
                        )
                        .child(
                            div()
                                .text_color(Theme::text_dim())
                                .child(crate::localization::text("editor.render.preview_label")),
                        )
                        .child(
                            div()
                                .font_family(Theme::editor_font_family())
                                .text_size(px(Theme::font_size_ratio(0.85)))
                                .text_color(preview_color)
                                .max_h(px(s(160.0)))
                                .child(preview_text),
                        )
                        .into_any_element();

                let copy_button = Button::new("convert-copy")
                    .label(crate::localization::text(
                        "editor.render.convert_copy_button",
                    ))
                    .ghost()
                    .on_click({
                        let entity = entity.clone();
                        move |_, window, cx| {
                            entity.update(cx, |editor, cx| {
                                if let Some(dialog) = editor.convert_dialog.as_ref()
                                    && let Ok(out) = dialog.preview(cx)
                                {
                                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(out));
                                }
                                let _ = window;
                            });
                        }
                    })
                    .into_any_element();
                let replace_button = Button::new("convert-replace")
                    .label(crate::localization::text(
                        "editor.render.convert_replace_button",
                    ))
                    .on_click({
                        let entity = entity.clone();
                        move |_, _, cx| {
                            entity.update(cx, |editor, cx| {
                                editor.apply_convert_list_replace(cx);
                            });
                        }
                    })
                    .into_any_element();
                (body, copy_button, replace_button)
            });
        let convert_overlay = convert_parts.map(|(body, copy_button, replace_button)| {
            Dialog::new("convert-list-dialog", |this: &mut Self| {
                this.dialog_frames.get("convert-list-dialog", || {
                    DialogFrame::new_auto(Theme::scaled(560.0))
                })
            })
            .title(crate::localization::text(
                "editor.render.convert_dialog_title",
            ))
            .dismiss_on_scrim_click(false)
            .on_close(|this, _window, cx| {
                this.convert_dialog = None;
                cx.notify();
            })
            .leading_button(copy_button)
            .child(body)
            .button(replace_button)
            .build(self, window, cx)
            .into_any_element()
        });

        // ---- Find/replace bar: overlay strip pinned to the editor's top
        // right, clear of the vertical scrollbar track. ----
        let goto_line_overlay = self
            .goto_line_bar
            .as_ref()
            .filter(|bar| bar.visible)
            .map(|bar| {
                let s = Theme::scaled;
                let line_count = self.content.split('\n').count();
                div()
                    .id("goto-line-bar")
                    .absolute()
                    .top(px(s(6.0)))
                    .right(px(s(24.0)))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(6.0)))
                    .p(px(s(6.0)))
                    .rounded_md()
                    .border_1()
                    .border_color(Theme::border())
                    .bg(Theme::popup_bg())
                    .occlude()
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, _window, cx| {
                        if event.keystroke.key.as_str() == "escape" {
                            this.close_goto_line_bar(cx);
                            cx.stop_propagation();
                        }
                    }))
                    .child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.85)))
                            .text_color(Theme::text_dim())
                            .child(crate::localization::format(
                                "editor.render.goto_line_label",
                                &[("line_count", &line_count.to_string())],
                            )),
                    )
                    .child(div().w(px(s(90.0))).child(Input::new(&bar.input).small()))
                    .into_any_element()
            });

        let find_bar_overlay = self.find_bar.as_ref().filter(|bar| bar.visible).map(|bar| {
            let s = Theme::scaled;
            let count = find_replace::count_label(bar.current, bar.matches.len(), bar.truncated);
            let compile_error = bar.compile_error.clone();
            let find_row = div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(s(4.0)))
                .child(div().w(px(s(200.0))).child(Input::new(&bar.find).small()))
                .child(
                    Button::new("find-bar-case")
                        .label("Aa")
                        .ghost()
                        .xsmall()
                        .selected(bar.case_sensitive)
                        .on_click(
                            cx.listener(|this, _, _window, cx| this.toggle_find_case_sensitive(cx)),
                        ),
                )
                .child(
                    Button::new("find-bar-word")
                        .label(crate::localization::text("editor.render.find_word_button"))
                        .ghost()
                        .xsmall()
                        .selected(bar.whole_word)
                        .on_click(
                            cx.listener(|this, _, _window, cx| this.toggle_find_whole_word(cx)),
                        ),
                )
                .child(
                    Button::new("find-bar-regex")
                        .label(".*")
                        .ghost()
                        .xsmall()
                        .selected(bar.regex)
                        .on_click(cx.listener(|this, _, _window, cx| this.toggle_find_regex(cx))),
                )
                .child(
                    div()
                        .min_w(px(s(72.0)))
                        .text_size(px(Theme::font_size_ratio(0.85)))
                        .text_color(Theme::text_dim())
                        .child(count),
                )
                .child(
                    Button::new("find-bar-prev")
                        .label("‹")
                        .ghost()
                        .xsmall()
                        .on_click(cx.listener(|this, _, _window, cx| this.find_prev(cx))),
                )
                .child(
                    Button::new("find-bar-next")
                        .label("›")
                        .ghost()
                        .xsmall()
                        .on_click(cx.listener(|this, _, _window, cx| this.find_next(cx))),
                )
                .child(
                    Button::new("find-bar-close")
                        .label("✕")
                        .ghost()
                        .xsmall()
                        .on_click(cx.listener(|this, _, _window, cx| this.close_find_bar(cx))),
                );
            let replace_row = bar.show_replace.then(|| {
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(4.0)))
                    .child(
                        div()
                            .w(px(s(200.0)))
                            .child(Input::new(&bar.replace).small()),
                    )
                    .child(
                        Button::new("find-bar-replace")
                            .label(crate::localization::text(
                                "editor.render.find_replace_button",
                            ))
                            .ghost()
                            .xsmall()
                            .on_click(cx.listener(|this, _, _window, cx| this.replace_current(cx))),
                    )
                    .child(
                        Button::new("find-bar-replace-all")
                            .label(crate::localization::text(
                                "editor.render.find_replace_all_button",
                            ))
                            .ghost()
                            .xsmall()
                            .on_click(
                                cx.listener(|this, _, _window, cx| this.replace_all_matches(cx)),
                            ),
                    )
            });
            div()
                .id("find-bar")
                .absolute()
                .top(px(s(6.0)))
                .right(px(s(24.0)))
                .flex()
                .flex_col()
                .gap(px(s(4.0)))
                .p(px(s(6.0)))
                .rounded_md()
                .border_1()
                .border_color(Theme::border())
                .bg(Theme::popup_bg())
                .occlude()
                // Escape is unbound inside gpui-component inputs, so it
                // bubbles here as a raw key (same pattern as the result
                // card name editor).
                .on_key_down(cx.listener(|this, event: &KeyDownEvent, _window, cx| {
                    if event.keystroke.key.as_str() == "escape" {
                        this.close_find_bar(cx);
                        cx.stop_propagation();
                    }
                }))
                .child(find_row)
                .children(compile_error.map(|error| {
                    div()
                        .max_w(px(s(420.0)))
                        .text_size(px(Theme::font_size_ratio(0.82)))
                        .text_color(Theme::error())
                        .child(crate::localization::format(
                            "editor.render.invalid_regex",
                            &[("error", &error)],
                        ))
                }))
                .children(replace_row)
                .into_any_element()
        });

        div()
            .relative()
            .size_full()
            .on_key_down(cx.listener(Self::handle_key_down))
            // Container-level drag tracking so a scrollbar drag survives the
            // pointer wandering off the narrow track.
            .scrollbar_drag_area()
            .child(scroll_area)
            // Vertical scrollbar track (always visible) + thumb (only when
            // the content overflows vertically). Pressing the track jumps
            // there; holding drags.
            .child(
                div()
                    .id("sql-editor-vertical-scrollbar-track")
                    .debug_selector(|| "sql-editor-vertical-scrollbar-track".into())
                    .absolute()
                    .top_0()
                    .right_0()
                    .w(px(scrollbar_w))
                    .h_full()
                    .bg(Theme::border().opacity(if v_scrollable { 0.3 } else { 0.15 }))
                    .when(v_scrollable, |track| {
                        track.scrollbar_track(&self.scroll_handle, gpui::Axis::Vertical)
                    }),
            )
            .when(v_scrollable, |d| {
                d.child(
                    div()
                        .id("sql-editor-vertical-scrollbar-thumb")
                        .debug_selector(|| "sql-editor-vertical-scrollbar-thumb".into())
                        .absolute()
                        .top(px(v_thumb_y))
                        .right_0()
                        .w(px(scrollbar_w))
                        .h(px(v_thumb_h))
                        .bg(Theme::text_dim().opacity(0.5))
                        .scrollbar_track(&self.scroll_handle, gpui::Axis::Vertical),
                )
            })
            // Horizontal scrollbar track (always visible) + thumb (only when
            // the content overflows horizontally).
            .child(
                div()
                    .id("sql-editor-horizontal-scrollbar-track")
                    .debug_selector(|| "sql-editor-horizontal-scrollbar-track".into())
                    .absolute()
                    .bottom_0()
                    .left_0()
                    .right(px(scrollbar_w))
                    .h(px(scrollbar_w))
                    .bg(Theme::border().opacity(if h_scrollable { 0.3 } else { 0.15 }))
                    .when(h_scrollable, |track| {
                        track.scrollbar_track(&self.scroll_handle, gpui::Axis::Horizontal)
                    }),
            )
            .when(h_scrollable, |d| {
                d.child(
                    div()
                        .id("sql-editor-horizontal-scrollbar-thumb")
                        .debug_selector(|| "sql-editor-horizontal-scrollbar-thumb".into())
                        .absolute()
                        .bottom_0()
                        .left(px(h_thumb_x))
                        .w(px(h_thumb_w))
                        .h(px(scrollbar_w))
                        .bg(Theme::text_dim().opacity(0.5))
                        .scrollbar_track(&self.scroll_handle, gpui::Axis::Horizontal),
                )
            })
            .when_some(find_bar_overlay, |container, overlay| {
                container.child(overlay)
            })
            .when_some(goto_line_overlay, |container, overlay| {
                container.child(overlay)
            })
            .when_some(context_overlay, |container, overlay| {
                container.child(overlay)
            })
            .when_some(format_overlay, |container, overlay| {
                container.child(overlay)
            })
            .when_some(picker_overlay, |container, overlay| {
                container.child(overlay)
            })
            .when_some(convert_overlay, |container, overlay| {
                container.child(overlay)
            })
    }
}
