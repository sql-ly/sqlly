//! `Render` impl and render helpers for `ResultsView`, split from the
//! flat `results` module.

use super::*;
use crate::tooltip::AppTooltip;
use gpui_component::IconName;
use gpui_component::button::Button;
use gpui_component::menu::{DropdownMenu, PopupMenuItem};

#[derive(Clone, Copy)]
pub(super) struct StackedResize {
    pub(super) slot: usize,
    pub(super) start_y: f32,
    pub(super) start_height: f32,
}

impl ResultsView {
    /// Intercept Cmd+C / Cmd+Shift+C on the grid container so copy uses the
    /// user's default format instead of the grid crate's TSV handler.
    fn wrap_grid_copy_capture(
        &self,
        slot_idx: usize,
        grid: Entity<SqllyDataTable>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let capture = div()
            .id(SharedString::from(format!("grid-copy-capture-{slot_idx}")))
            .size_full()
            .capture_key_down(
                cx.listener(move |this, ev: &gpui::KeyDownEvent, _window, cx| {
                    let ks = &ev.keystroke;
                    let m = &ks.modifiers;
                    // Copy is the platform's copy chord: ⌘C on macOS, Ctrl+C
                    // elsewhere — including the browser build, which folds a
                    // Mac's ⌘ into Ctrl before gpui sees the key.
                    if ks.key == "c"
                        && crate::keybindings::primary_modifier(m)
                        && !m.alt
                        && !(m.platform && m.control)
                    {
                        this.copy_selection_default(slot_idx, m.shift, cx);
                        cx.stop_propagation();
                    } else if ks.key == "v" && m.platform && !m.alt && !m.control {
                        this.request_paste(slot_idx, cx);
                        cx.stop_propagation();
                    }
                }),
            )
            .child(grid)
            // Double-click a cell to copy its value (with a brief "Value
            // copied" toast). On mouse UP so the grid has already moved the
            // selection to the cell under the pointer.
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(move |this, ev: &gpui::MouseUpEvent, window, cx| {
                    this.handle_grid_double_click(slot_idx, ev, window, cx);
                }),
            )
            .on_scroll(cx.listener(|this, event, _window, cx| {
                this.handle_grid_zoom_wheel(event, cx);
            }))
            .on_pinch(cx.listener(|this, event: &gpui::PinchEvent, _window, cx| {
                this.handle_grid_pinch(event, cx);
            }));
        let Some(caption) = self.grid_caption(slot_idx) else {
            return capture.into_any_element();
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(Theme::row_height()))
                    .px(px(Theme::scaled(10.0)))
                    .flex()
                    .items_center()
                    .font(Theme::ui_font())
                    .text_size(px(Theme::font_size_ratio(0.84)))
                    .text_color(Theme::text_dim())
                    .border_b_1()
                    .border_color(Theme::border())
                    .child(caption),
            )
            .child(div().flex_1().min_h(px(0.0)).child(capture))
            .into_any_element()
    }

    fn wrap_with_cell_detail(
        &self,
        grid: gpui::AnyElement,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(panel) = self.cell_detail.clone() else {
            return grid;
        };
        let size = self.cell_detail_size;
        let this = cx.entity();
        match self.cell_detail_side {
            crate::cell_detail_model::DockSide::Right => {
                let divider =
                    PaneDivider::vertical("cell-detail-divider", CollapseDirection::Right)
                        .label(crate::localization::text(
                            "results.cell_details.right_divider_label",
                        ))
                        .on_drag_start({
                            let this = this.clone();
                            move |pos: f32, _window, app| {
                                this.update(app, |_this, cx| {
                                    cx.emit(ResultsViewEvent::CellDetailDragStarted { pos });
                                });
                            }
                        });
                div()
                    .flex()
                    .flex_row()
                    .size_full()
                    .child(div().flex_1().min_w_0().min_h_0().child(grid))
                    .child(divider)
                    .child(
                        div()
                            .w(px(size))
                            .h_full()
                            .flex_shrink_0()
                            .border_l_1()
                            .border_color(Theme::border())
                            .child(panel),
                    )
                    .into_any_element()
            }
            crate::cell_detail_model::DockSide::Bottom => {
                let divider =
                    PaneDivider::horizontal("cell-detail-divider", CollapseDirection::Down)
                        .label(crate::localization::text(
                            "results.cell_details.bottom_divider_label",
                        ))
                        .on_drag_start({
                            let this = this.clone();
                            move |pos: f32, _window, app| {
                                this.update(app, |_this, cx| {
                                    cx.emit(ResultsViewEvent::CellDetailDragStarted { pos });
                                });
                            }
                        });
                div()
                    .flex()
                    .flex_col()
                    .size_full()
                    .child(div().flex_1().min_w_0().min_h_0().child(grid))
                    .child(divider)
                    .child(
                        div()
                            .h(px(size))
                            .w_full()
                            .flex_shrink_0()
                            .border_t_1()
                            .border_color(Theme::border())
                            .child(panel),
                    )
                    .into_any_element()
            }
        }
    }

    /// The grid find bar: a value search across every column of the active
    /// result set. Rendered only while open; `None` collapses to nothing.
    fn render_grid_find_bar(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        if !self.grid_find_visible() {
            return None;
        }
        self.ensure_grid_find_input(window, cx);
        let input = self.grid_find.input.clone()?;
        let replace_input = self.grid_find.replace_input.clone()?;
        // Focus the input on the render after a windowless open (context menu).
        if self.grid_find.focus_pending {
            self.grid_find.focus_pending = false;
            input.update(cx, |state, cx| {
                state.set_value(self.grid_find.query.clone(), window, cx);
                state.focus(window, cx);
            });
        }
        let s = Theme::scaled;
        let count = self.grid_find_count_label(cx);
        let has_query = !self.grid_find.query.trim().is_empty();
        let can_replace = has_query && self.grid_find_match_count(cx) > 0;
        Some(
            div()
                .flex()
                .flex_col()
                .gap(px(s(2.0)))
                .px(px(s(8.0)))
                .py(px(s(4.0)))
                .bg(Theme::panel_bg())
                .border_b_1()
                .border_color(Theme::border())
                .on_key_down(
                    cx.listener(|this, event: &gpui::KeyDownEvent, _window, cx| {
                        if event.keystroke.key == "escape" {
                            this.close_grid_find(cx);
                        }
                    }),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(s(6.0)))
                        .child(
                            Icon::empty()
                                .path(crate::icon_catalog::AppIcon::Search.path())
                                .flex_shrink_0(),
                        )
                        .child(div().w(px(s(220.0))).child(Input::new(&input).small()))
                        .child(
                            div()
                                .min_w(px(s(72.0)))
                                .text_size(px(Theme::font_size_ratio(0.85)))
                                .text_color(Theme::text_dim())
                                .child(if has_query { count } else { String::new() }),
                        )
                        .child(
                            Button::new("grid-find-prev")
                                .icon(
                                    Icon::empty()
                                        .path(crate::icon_catalog::AppIcon::ChevronUp.path()),
                                )
                                .ghost()
                                .xsmall()
                                .app_tooltip_text(crate::localization::text(
                                    "results.find_replace.previous_match_tooltip",
                                ))
                                .on_click(cx.listener(|this, _, _window, cx| {
                                    this.grid_find_prev(cx);
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new("grid-find-next")
                                .icon(
                                    Icon::empty()
                                        .path(crate::icon_catalog::AppIcon::ChevronDown.path()),
                                )
                                .ghost()
                                .xsmall()
                                .app_tooltip_text(crate::localization::text(
                                    "results.find_replace.next_match_tooltip",
                                ))
                                .on_click(cx.listener(|this, _, _window, cx| {
                                    this.grid_find_next(cx);
                                    cx.notify();
                                })),
                        )
                        .child(div().flex_1())
                        .child(
                            Button::new("grid-find-close")
                                .icon(
                                    Icon::empty().path(crate::icon_catalog::AppIcon::Close.path()),
                                )
                                .ghost()
                                .xsmall()
                                .app_tooltip_text(crate::localization::text(
                                    "results.find_replace.close_tooltip",
                                ))
                                .on_click(cx.listener(|this, _, _window, cx| {
                                    this.close_grid_find(cx);
                                })),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(s(6.0)))
                        .child(
                            // Spacer aligning the replace field under the find one.
                            div().w(px(s(16.0))).flex_shrink_0(),
                        )
                        .child(
                            div()
                                .w(px(s(220.0)))
                                .child(Input::new(&replace_input).small()),
                        )
                        .child(
                            Button::new("grid-find-replace")
                                .ghost()
                                .xsmall()
                                .label(crate::localization::text(
                                    "results.find_replace.replace_button",
                                ))
                                .disabled(!can_replace)
                                .app_tooltip_text(crate::localization::text(
                                    "results.find_replace.replace_tooltip",
                                ))
                                .on_click(cx.listener(|this, _, _window, cx| {
                                    this.grid_find_replace(false, cx);
                                })),
                        )
                        .child(
                            Button::new("grid-find-replace-all")
                                .ghost()
                                .xsmall()
                                .label(crate::localization::text(
                                    "results.find_replace.replace_all_button",
                                ))
                                .disabled(!can_replace)
                                .app_tooltip_text(crate::localization::text(
                                    "results.find_replace.replace_all_tooltip",
                                ))
                                .on_click(cx.listener(|this, _, _window, cx| {
                                    this.grid_find_replace(true, cx);
                                })),
                        ),
                )
                .into_any_element(),
        )
    }

    fn render_goto_column(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        self.goto_column.as_ref()?;
        self.ensure_goto_column_input(window, cx);
        let popover = self.goto_column.as_ref()?;
        let input = popover.input.clone()?;
        if popover.focus_pending {
            if let Some(popover) = self.goto_column.as_mut() {
                popover.focus_pending = false;
            }
            input.update(cx, |state, cx| {
                state.focus(window, cx);
            });
        }
        let popover = self.goto_column.as_ref()?;
        let matches = popover.model.matches();
        let selected = popover.model.selected;
        let s = Theme::scaled;
        let list = matches
            .into_iter()
            .take(12)
            .enumerate()
            .map(|(i, (_, name))| {
                let highlighted = i == selected;
                div()
                    .id(SharedString::from(format!("goto-col-{i}")))
                    .px(px(s(8.0)))
                    .py(px(s(3.0)))
                    .rounded_sm()
                    .when(highlighted, |el| el.bg(Theme::accent().opacity(0.15)))
                    .child(name)
            });
        Some(
            div()
                .flex()
                .flex_col()
                .gap(px(s(4.0)))
                .px(px(s(8.0)))
                .py(px(s(6.0)))
                .bg(Theme::panel_bg())
                .border_b_1()
                .border_color(Theme::border())
                .on_key_down(
                    cx.listener(|this, event: &gpui::KeyDownEvent, _window, cx| {
                        let key = event.keystroke.key.as_str();
                        if key == "escape" {
                            this.close_goto_column(cx);
                        } else if let Some(popover) = this.goto_column.as_mut() {
                            if key == "down" {
                                popover.model.move_selection(1);
                                cx.notify();
                            } else if key == "up" {
                                popover.model.move_selection(-1);
                                cx.notify();
                            }
                        }
                    }),
                )
                .child(div().w(px(s(280.0))).child(Input::new(&input).small()))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(s(1.0)))
                        .max_h(px(s(240.0)))
                        .children(list),
                )
                .into_any_element(),
        )
    }
}

impl Render for ResultsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        // Profile the visible slot's columns through the optional decision
        // model, at most once per slot; a cheap flag check on every other
        // frame.
        self.maybe_spawn_column_profile(cx);
        self.maybe_spawn_chart_suggestion_for_active(cx);
        if self.completed_at.is_some() && self.ui_paint_ms.is_none() {
            if let Some(completed) = self.completed_at {
                self.ui_paint_ms = Some(completed.elapsed().as_millis() as u64);
            }
            if let Some(paint) = self.ui_paint_ms
                && let Some(server_total) = self.server_total_ms()
            {
                self.wall_clock_ms = Some(server_total + paint);
            }
        }

        let tabs_count = self.result_sets.len();
        let layout_mode = self.layout_mode;
        let canvas_spec = if layout_mode == LayoutMode::Canvas {
            self.canvas_layout_spec(tabs_count)
        } else {
            None
        };
        let snapshot_prompt_el = self.render_snapshot_prompt(window, cx);
        let result_settings_el = self.render_result_settings(window, cx);
        let result_name_menu_el = self.render_result_name_menu(cx);
        let filter_builder_el = self.render_filter_builder(window, cx);
        let row_form_el = self.render_row_form(window, cx);
        let conditional_formatting_el = self.render_conditional_formatting(window, cx);
        let grid_find_bar_el = self.render_grid_find_bar(window, cx);
        let formula_bar_el = self.render_formula_bar(window, cx);
        let goto_column_el = self.render_goto_column(window, cx);
        // A result set can outlive its grid — most visibly after a session
        // restore, where the cached set is present but no grid was ever built —
        // which otherwise wedges the pane on "Preparing grid…" forever. Build
        // it here; `ensure_grid_for_active_tab` is a cheap no-op once the grid
        // exists, and falls back to an empty grid ("No rows returned") when the
        // rows can't be materialized, so the pane always resolves.
        if self.grid_entity_for_slot(self.active_tab).is_none()
            && self.active_result_set().is_some()
        {
            self.ensure_grid_for_active_tab(cx);
        }
        let active_rs = self.active_result_set();
        let messages_count = self.messages.len();
        let active_grid = self.grid_entity_for_slot(self.active_tab);

        // Collect all grid views for stacked/canvas modes.
        let all_grids: Vec<(usize, String, Option<Entity<SqllyDataTable>>, bool)> = (0..tabs_count)
            .map(|i| {
                let label = self.result_set_label(i);
                let grid = self.grid_entity_for_slot(i);
                let collapsed = self.collapsed_cards.get(i).copied().unwrap_or(false);
                (i, label, grid, collapsed)
            })
            .collect();

        div()
            .key_context("ResultsPane")
            .flex()
            .flex_col()
            .size_full()
            .relative()
            .bg(Theme::bg())
            .children(row_form_el)
            .children(conditional_formatting_el)
            .children(filter_builder_el)
            .children(snapshot_prompt_el)
            .children(result_settings_el)
            .children(result_name_menu_el)
            .children(self.render_historical_banner(cx))
            .children(grid_find_bar_el)
            .children(formula_bar_el)
            .children(goto_column_el)
            .child(
                // Tab bar — hidden while pristine so the empty glyph owns the
                // region. Chrome appears after the first run (or a message).
                if !self.results_chrome_visible() {
                    div().into_any_element()
                } else if layout_mode == LayoutMode::Tabbed {
                    div()
                        .flex()
                        .flex_row()
                        .items_stretch()
                        .h(px(s(34.0)))
                        .bg(Theme::bg())
                        .border_b_1()
                        .border_color(Theme::border())
                        .children((0..tabs_count).map(|i| {
                            let is_active = i == self.active_tab
                                && self.active_output_tab() == OutputPanelTab::Results
                                && !self.show_result_profile;
                            let tab = chrome_tab_shell(
                                ElementId::NamedInteger("result-set-tab".into(), i as u64),
                                is_active,
                            );
                            let label = self.result_set_label(i);
                            tab.child(
                                div()
                                    .flex()
                                    .flex_1()
                                    .items_center()
                                    .child(self.render_lock_button(i, cx))
                                    .child(label)
                                    .child(
                                        div()
                                            .id(ElementId::NamedInteger(
                                                "result-set-tab-settings".into(),
                                                i as u64,
                                            ))
                                            .debug_selector(move || format!("result-set-tab-settings-{i}"))
                                            .ml(px(s(3.0)))
                                            .text_size(px(Theme::font_size_ratio(0.85)))
                                            .text_color(Theme::text_dim())
                                            .hover(|st| st.text_color(Theme::text()))
                                            .child("\u{270E}")
                                            .on_mouse_down(
                                                gpui::MouseButton::Left,
                                                cx.listener(move |this, _, _window, cx| {
                                                    cx.stop_propagation();
                                                    this.open_result_settings(i, cx);
                                                }),
                                            ),
                                    ),
                            )
                            .child(chrome_tab_underline(is_active))
                            // Right-click the result's name: Show by Default /
                            // Set Number of Rows / Comment Out Result.
                            .on_mouse_down(
                                gpui::MouseButton::Right,
                                cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                                    cx.stop_propagation();
                                    this.open_result_name_menu(i, ev.position, window, cx);
                                }),
                            )
                            .on_mouse_down(
                                gpui::MouseButton::Left,
                                cx.listener(move |this, _, _, cx| {
                                    this.active_tab = i;
                                    this.clear_output_tab_flags();
                                    this.ensure_grid_for_active_tab(cx);
                                    if this.track_difference {
                                        this.diff_key_columns.clear();
                                        this.recompute_detailed_diff(cx);
                                    }
                                    cx.emit(ResultsViewEvent::ActiveResultChanged);
                                    this.refresh_cell_detail(cx);
                                    cx.notify();
                                }),
                            )
                        }))
                        .children(
                            self.unified_output_tabs()
                                .into_iter()
                                .filter(|t| *t != OutputPanelTab::Results)
                                .map(|t| self.render_unified_output_tab(t, cx)),
                        )
                        .child(self.render_result_profile_tab(cx))
                        .children(self.render_staged_edit_controls(cx))
                        // Spacer pushes Export to the right edge.
                        .child(div().flex_1())
                        .children(self.multi_sort_chip(cx.entity()))
                        .children(self.mask_suggestion_chips(cx.entity()))
                        .children(self.chart_suggestion_chip(cx.entity(), cx))
                        .children(self.save_chart_chip(cx.entity(), cx))
                        .children(self.masked_toolbar_chip(cx.entity()))
                        .child(self.render_result_layout_control(cx))
                        .child(self.render_export_control(cx))
                        .child(self.render_pane_layout_buttons(cx))
                        .into_any_element()
                } else {
                    // Stacked/Canvas: a single "Results" tab holds every set,
                    // sitting next to the same aux tabs (Messages / Diff /
                    // Plan). Stacked puts its grouping / filter / sort clear
                    // controls in each card's own header, since they act on
                    // that one result set.
                    let entity = cx.entity();
                    div()
                        .flex()
                        .flex_row()
                        .items_stretch()
                        .h(px(s(34.0)))
                        .bg(Theme::bg())
                        .border_b_1()
                        .border_color(Theme::border())
                        .children(
                            self.unified_output_tabs()
                                .into_iter()
                                .map(|t| self.render_unified_output_tab(t, cx)),
                        )
                        .child(self.render_result_profile_tab(cx))
                        .children(self.render_staged_edit_controls(cx))
                        // Spacer pushes the controls to the right edge.
                        .child(div().flex_1())
                        .children(self.multi_sort_chip(entity.clone()))
                        .children(self.mask_suggestion_chips(entity.clone()))
                        .children(self.chart_suggestion_chip(entity.clone(), cx))
                        .children(self.save_chart_chip(entity.clone(), cx))
                        .children(self.masked_toolbar_chip(entity.clone()))
                        .child(self.render_result_layout_control(cx))
                        .child(self.render_export_control(cx))
                        .child(self.render_pane_layout_buttons(cx))
                        .into_any_element()
                },
            )
            .children(self.render_run_history_strip(cx))
            .child(
                // Export dropdown. `deferred(anchored(...))` paints it above
                // the (later-sibling) content area and keeps it from being
                // clipped; `.occlude()` + `.on_mouse_down_out()` give it
                // click-outside dismissal, matching the schema-tree context
                // menu idiom.
                if self.results_chrome_visible() && self.show_export_menu {
                    let dismiss = cx.entity();
                    // Same preference gating as the grid context menu; the
                    // tiny JSON only loads while the menu is actually open.
                    let export_prefs = crate::preferences::ExportPreferences::load();
                    let (standard, programming) = toolbar_export_formats(&export_prefs);
                    let item = |fmt: ExportFormat, cx: &mut Context<Self>| {
                        div()
                            .px(px(s(12.0)))
                            .py(px(s(4.0)))
                            .font(Theme::ui_font())
                            .text_size(px(Theme::font_size()))
                            .text_color(Theme::text())
                            .cursor_pointer()
                            // `row_hover` is near-identical to the popup
                            // surface in the dark palettes; the accent-tinted
                            // completion highlight reads on every popup.
                            .hover(|s| s.bg(Theme::completion_selected()))
                            .child(fmt.localized_label())
                            .on_mouse_down(
                                gpui::MouseButton::Left,
                                cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                                    this.copy_as(fmt, ev.modifiers.shift, window, cx);
                                }),
                            )
                            .into_any_element()
                    };
                    let mut rows: Vec<gpui::AnyElement> = Vec::new();
                    // "Send this to Excel" without the save dialog: export to
                    // a temp file and open it in the OS default spreadsheet app.
                    rows.push(
                        div()
                            .px(px(s(12.0)))
                            .py(px(s(4.0)))
                            .font(Theme::ui_font())
                            .text_size(px(Theme::font_size()))
                            .text_color(Theme::text())
                            .cursor_pointer()
                            .hover(|s| s.bg(Theme::completion_selected()))
                            .child(crate::localization::text(
                                "results.export_menu.open_spreadsheet",
                            ))
                            .on_mouse_down(
                                gpui::MouseButton::Left,
                                cx.listener(move |this, _ev: &MouseDownEvent, window, cx| {
                                    this.open_active_in_spreadsheet(window, cx);
                                }),
                            )
                            .into_any_element(),
                    );
                    if !standard.is_empty() {
                        rows.push(
                            div()
                                .my(px(s(4.0)))
                                .h(px(1.0))
                                .bg(Theme::popup_border())
                                .into_any_element(),
                        );
                    }
                    for fmt in &standard {
                        rows.push(item(*fmt, cx));
                    }
                    if !standard.is_empty() && !programming.is_empty() {
                        rows.push(
                            div()
                                .my(px(s(4.0)))
                                .h(px(1.0))
                                .bg(Theme::popup_border())
                                .into_any_element(),
                        );
                    }
                    for fmt in &programming {
                        rows.push(item(*fmt, cx));
                    }
                    // Model / type definitions from the column metadata alone
                    // (no row data): same language switches as the snippets.
                    let models = toolbar_model_languages(&export_prefs);
                    if !rows.is_empty() && !models.is_empty() {
                        rows.push(
                            div()
                                .my(px(s(4.0)))
                                .h(px(1.0))
                                .bg(Theme::popup_border())
                                .into_any_element(),
                        );
                    }
                    for lang in models {
                        rows.push(
                            div()
                                .px(px(s(12.0)))
                                .py(px(s(4.0)))
                                .font(Theme::ui_font())
                                .text_size(px(Theme::font_size()))
                                .text_color(Theme::text())
                                .cursor_pointer()
                                .hover(|s| s.bg(Theme::completion_selected()))
                                .child(lang.label().to_string())
                                .on_mouse_down(
                                    gpui::MouseButton::Left,
                                    cx.listener(move |this, _ev: &MouseDownEvent, window, cx| {
                                        this.copy_model_as(lang, window, cx);
                                    }),
                                )
                                .into_any_element(),
                        );
                    }
                    if rows.is_empty() {
                        rows.push(
                            div()
                                .px(px(s(12.0)))
                                .py(px(s(4.0)))
                                .text_size(px(Theme::font_size()))
                                .text_color(Theme::text_dim())
                                .child(crate::localization::text(
                                    "results.export_menu.no_formats_enabled",
                                ))
                                .into_any_element(),
                        );
                    }
                    let menu = div()
                        .occlude()
                        .w(px(s(150.0)))
                        .bg(Theme::popup_bg())
                        .border_1()
                        .border_color(Theme::popup_border())
                        .rounded(px(s(4.0)))
                        .py(px(s(4.0)))
                        .flex()
                        .flex_col()
                        .children(rows)
                        .on_mouse_down_out(move |_: &MouseDownEvent, _window, app| {
                            dismiss.update(app, |this, cx| {
                                if this.show_export_menu {
                                    this.show_export_menu = false;
                                    cx.notify();
                                }
                            });
                        });
                    div()
                        .absolute()
                        .top(px(s(34.0)))
                        .right(px(0.0))
                        .child(
                            deferred(
                                anchored()
                                    .anchor(Anchor::TopRight)
                                    .snap_to_window_with_margin(px(s(8.0)))
                                    .child(menu),
                            )
                            .with_priority(1),
                        )
                        .into_any_element()
                } else {
                    div().into_any_element()
                },
            )
            .child(
                // Content area — dispatches on layout mode
                div()
                    .id("results_content")
                    .flex_1()
                    .overflow_hidden()
                    .child(if self.show_history {
                        self.render_history_pane()
                    } else if self.show_ddl {
                        self.render_ddl_pane()
                    } else if self.show_preview_sql {
                        self.render_preview_sql_pane(cx)
                    } else if self.show_execution_plan {
                        // Explicitly toggled — like Messages and Diff, the plan
                        // pane wins over the grid when asked for.
                        if let Some(view) = &self.execution_plan_view {
                            view.clone().into_any_element()
                        } else {
                            div().into_any_element()
                        }
                    } else if self.show_messages {
                        // The Messages tab wins over the grid whenever selected —
                        // even with nothing to show yet, since it is a permanent
                        // tab. It carries both validation diagnostics and the
                        // run's PRINT / rows-affected / error output, in one
                        // read-only selectable field.
                        self.render_messages_pane(window, cx)
                    } else if self.show_diff {
                        self.render_diff_view(cx)
                    } else if self.show_statistics {
                        // Client Statistics — the last run's metrics, shown in
                        // the content area (not squeezed into the tab strip).
                        self.render_statistics_pane(cx)
                    } else if self.show_result_profile && self.result_profile_grid.is_some() {
                        self.render_result_profile_view(cx)
                    } else if self.is_running && self.result_sets.is_empty() {
                        // Only cover the content area while nothing has
                        // arrived yet. Once the first result set lands, fall
                        // through to the grid so rows paint as they stream in
                        // instead of popping in all at once on completion.
                        let elapsed = self.query_start.map(|start| start.elapsed());
                        loading_placeholder(
                            crate::localization::text("results.loading.query_running"),
                            elapsed,
                        )
                    } else if layout_mode == LayoutMode::Stacked && tabs_count > 0 {
                        // Stacked: scrollable column of collapsible cards.
                        self.render_stacked(&all_grids, cx)
                    } else if layout_mode == LayoutMode::Canvas && tabs_count > 0 {
                        // Canvas: grid placement.
                        self.render_canvas(&all_grids, canvas_spec.as_ref(), cx)
                    } else if active_rs.is_some() {
                        if let Some(grid_view) = active_grid {
                            // A successful query that matched nothing renders as
                            // a header row over blank space, which reads exactly
                            // like a failure or a stuck load. Say so instead —
                            // but keep the grid mounted underneath, because the
                            // column headers are real information about what was
                            // asked. Only speak up once the query has finished:
                            // mid-stream, zero rows just means "not yet".
                            // Mirrors `row_count_for_slot`: a spilled result stops
                            // growing `rows`, so the hint is the honest total.
                            let empty = !self.is_running
                                && active_rs
                                    .is_some_and(|rs| rs.rows.len().max(rs.row_count_hint) == 0);
                            let grid = div()
                                .id("tabbed-results")
                                .relative()
                                .size_full()
                                .on_scroll(cx.listener(|this, event, _window, cx| {
                                    if this.handle_grid_zoom_wheel(event, cx) {
                                        return;
                                    }
                                    this.handle_tabbed_scroll(event, cx);
                                }))
                                .on_pinch(cx.listener(|this, event: &gpui::PinchEvent, _window, cx| {
                                    this.handle_grid_pinch(event, cx);
                                }))
                                .child(self.wrap_grid_copy_capture(
                                    self.active_tab,
                                    grid_view.clone(),
                                    cx,
                                ))
                                .when(empty, |d| {
                                    d.child(
                                        div()
                                            .absolute()
                                            .inset_0()
                                            .top(px(Theme::row_height() * 2.0))
                                            .flex()
                                            .items_start()
                                            .justify_center()
                                            .pt(px(Theme::scaled(24.0)))
                                            .child(
                                                div()
                                                    .text_size(px(Theme::font_size()))
                                                    .text_color(Theme::text_dim())
                                                    .child(crate::localization::text(
                                                        "results.grid.no_rows_returned",
                                                    )),
                                            ),
                                    )
                                })
                                // Keyed by result-set slot: the fade plays when
                                // a grid first lands and again on each set
                                // switch, never on the steady-state repaints
                                // in between.
                                .with_animation(
                                    ElementId::NamedInteger(
                                        "results-grid".into(),
                                        self.active_tab as u64,
                                    ),
                                    Animation::new(Duration::from_millis(120))
                                        .with_easing(cubic_bezier(0.16, 1.0, 0.3, 1.0)),
                                    |this, delta| this.opacity(delta),
                                );
                            self.wrap_with_cell_detail(grid.into_any_element(), cx)
                        } else {
                            loading_placeholder(
                                crate::localization::text("results.loading.preparing_active_grid"),
                                None,
                            )
                        }
                    } else if messages_count > 0 || self.run_started_wall.is_some() {
                        // Even a run that produced no result sets and no server
                        // messages still has its start/finish clock lines to
                        // show — land on the Messages pane, not the pristine
                        // shortcut list.
                        self.render_messages_pane(window, cx)
                    } else {
                        // Shortcut list: action left, dim shortcut right.
                        // Run Query uses the user's actual execute binding.
                        div()
                            .flex()
                            .flex_col()
                            .size_full()
                            .items_center()
                            .justify_center()
                            .gap(px(s(12.0)))
                            .child(
                                Icon::empty()
                                    .path(crate::icon_catalog::AppIcon::Table.path())
                                    .size(px(s(40.0)))
                                    .text_color(Theme::text_dim().opacity(0.45)),
                            )
                            .child(
                                div()
                                    .font(Theme::ui_font())
                                    .text_size(px(Theme::font_size_ratio(0.92)))
                                    .text_color(Theme::text_dim().opacity(0.7))
                                    .child(crate::localization::text(
                                        "results.empty_state.run_query_prompt",
                                    )),
                            )
                            .child(render_empty_state_shortcuts())
                            .into_any_element()
                    }),
            )
            .when(
                self.results_chrome_visible()
                    && layout_mode != LayoutMode::Stacked
                    && !self.show_diff,
                |d| {
                d.children(self.selection_stats_text(cx).map(|text| {
                    div()
                        .flex()
                        .flex_row()
                        .h(px(Theme::bar_height(1.55)))
                        .border_t_1()
                        .border_color(Theme::border())
                        .bg(Theme::bg())
                        .px(px(s(8.0)))
                        .items_center()
                        .font(Theme::ui_font())
                        .text_size(px(Theme::font_size_ratio(0.92)))
                        .text_color(Theme::text_dim())
                        .child(text)
                }))
            })
            .when(self.results_chrome_visible(), |d| {
                d.child({
                    let fmt = self.formatting.clone().unwrap_or_default();
                    div()
                        .flex()
                        .flex_row()
                        .h(px(Theme::bar_height(2.0)))
                        .border_t_1()
                        .border_color(Theme::border())
                        .bg(Theme::bg())
                        .px(px(s(8.0)))
                        .items_center()
                        .gap(px(s(16.0)))
                        .font(Theme::ui_font())
                        .text_size(px(Theme::font_size_ratio(0.92)))
                        .text_color(Theme::text_dim())
                        .children((0..self.result_sets.len()).map(|slot| {
                            let this = cx.entity();
                            let count_state = self.total_row_counts.get(&slot).cloned();
                            let show_link = self.total_count_link_visible(slot);
                            let sql = self.query_sql.clone().unwrap_or_default();
                            let failed_msg = match &count_state {
                                Some(TotalCount::Failed(msg)) => Some(msg.clone()),
                                _ => None,
                            };
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(Theme::scaled(8.0)))
                                .child(self.result_count_label(slot, &fmt))
                                .when(show_link, {
                                    let this = this.clone();
                                    move |row| {
                                        row.child(
                                            Button::new("results-total-count")
                                                .link()
                                                .xsmall()
                                                .label(crate::localization::text(
                                                    "results.status_bar.calculate_total_rows",
                                                ))
                                                .app_tooltip_text(crate::localization::text(
                                                    "results.status_bar.calculate_total_rows_tooltip",
                                                ))
                                                .on_click(move |_ev, _window, app| {
                                                    this.update(app, |_v, cx| {
                                                        cx.emit(
                                                            ResultsViewEvent::TotalRowCountRequested {
                                                                result_set_index: slot,
                                                                sql: sql.clone(),
                                                            },
                                                        );
                                                        cx.notify();
                                                    });
                                                }),
                                        )
                                    }
                                })
                                .when(
                                    matches!(count_state, Some(TotalCount::Pending)),
                                    {
                                        let this = this.clone();
                                        move |row| {
                                            row.child(div().child(crate::localization::text(
                                                "results.status_bar.counting_rows",
                                            )))
                                            .child(
                                                Button::new("results-total-count-cancel")
                                                    .ghost()
                                                    .xsmall()
                                                    .label(crate::localization::text(
                                                        "results.status_bar.cancel_count_button",
                                                    ))
                                                    .on_click(move |_ev, _window, app| {
                                                        this.update(app, |_v, cx| {
                                                            cx.emit(
                                                                ResultsViewEvent::TotalRowCountCancelRequested,
                                                            );
                                                            cx.notify();
                                                        });
                                                    }),
                                            )
                                        }
                                    },
                                )
                                .when(failed_msg.is_some(), |row| {
                                    row.child(
                                        Button::new("results-total-count-failed")
                                            .link()
                                            .xsmall()
                                            .label(crate::localization::text(
                                                "results.status_bar.total_unavailable",
                                            ))
                                            .app_tooltip_text(failed_msg.clone().unwrap_or_default()),
                                    )
                                })
                        }))
                        .when(self.last_sql_is_select_only(), |bar| {
                            let this = cx.entity();
                            let current = self.auto_refresh_secs;
                            bar.child(
                                Button::new("results-auto-refresh")
                                    .ghost()
                                    .xsmall()
                                    .label(crate::auto_refresh::interval_label(current))
                                    .app_tooltip_text(crate::localization::text(
                                        "results.status_bar.auto_refresh_tooltip",
                                    ))
                                    .dropdown_menu(move |menu, _, _| {
                                        let mut menu = menu;
                                        for choice in crate::auto_refresh::INTERVAL_CHOICES {
                                            let this = this.clone();
                                            menu = menu.item(
                                                PopupMenuItem::new(
                                                    crate::auto_refresh::interval_label(choice),
                                                )
                                                .checked(choice == current)
                                                .on_click(move |_ev, _window, app| {
                                                    this.update(app, |v, cx| {
                                                        v.auto_refresh_secs = choice;
                                                        cx.emit(
                                                            ResultsViewEvent::AutoRefreshIntervalChanged(
                                                                choice,
                                                            ),
                                                        );
                                                        cx.notify();
                                                    });
                                                }),
                                            );
                                        }
                                        menu
                                    }),
                            )
                        })
                        .when(
                            (self.grid_zoom - crate::editor_zoom::DEFAULT_ZOOM).abs() > 1e-3,
                            |bar| {
                                bar.child(
                                    div()
                                        .id("grid-zoom-label")
                                        .text_color(Theme::text_dim())
                                        .cursor(CursorStyle::PointingHand)
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(|this, ev: &MouseDownEvent, _window, cx| {
                                                if ev.click_count >= 2 {
                                                    this.reset_grid_zoom(cx);
                                                }
                                            }),
                                        )
                                        .child(
                                            crate::localization::format(
                                                "results.status_bar.grid_zoom",
                                                &[(
                                                    "zoom_percent",
                                                    &crate::editor_zoom::zoom_to_percent(
                                                        self.grid_zoom,
                                                    )
                                                    .to_string(),
                                                )],
                                            ),
                                        ),
                                )
                            },
                        )
                        .when(self.is_running, |bar| {
                            let elapsed = self
                                .query_start
                                .map(|s| s.elapsed())
                                .unwrap_or_default();
                            bar.child(
                                div()
                                    .id("timing-running")
                                    .child(
                                        crate::localization::format(
                                            "results.status_bar.query_running_elapsed",
                                            &[(
                                                "elapsed_time",
                                                &format_running_elapsed(elapsed),
                                            )],
                                        ),
                                    ),
                            )
                        })
                        // The per-query timing breakdown is a diagnostics aid,
                        // hidden unless Results ▸ Performance ▸ "Show detailed
                        // query timing" is on. The row counts above always show;
                        // everything below (First/Query/Server/Paint/Wall/
                        // Latency, the latency-subtract toggle, and the hh:MM:ss
                        // toggle) hides together with it.
                        .when(self.show_detailed_timing, |bar| {
                            let fmt_fn = match self.timing_format {
                                TimingFormat::Duration => format_duration_ms,
                                TimingFormat::Elapsed => format_elapsed,
                            };
                            let timing = |key, value: String| {
                                crate::localization::format(key, &[("duration", &value)])
                            };
                            bar.children({
                                self.display_ms(self.first_response_ms).map(|ms| {
                                    div()
                                        .id("timing-first")
                                        .child(timing(
                                            "results.status_bar.first_response_time",
                                            fmt_fn(ms),
                                        ))
                                        .app_tooltip(move |_window, cx| {
                                            crate::app::tooltip_view(
                                                crate::localization::text(
                                                    "results.status_bar.first_response_tooltip",
                                                ),
                                                cx,
                                            )
                                        })
                                })
                            })
                            .children({
                                self.format_timing(self.elapsed_ms).map(|s| {
                                    div()
                                        .id("timing-query")
                                        .child(timing("results.status_bar.query_time", s))
                                        .app_tooltip(move |_window, cx| {
                                            crate::app::tooltip_view(
                                                crate::localization::text(
                                                    "results.status_bar.query_time_tooltip",
                                                ),
                                                cx,
                                            )
                                        })
                                })
                            })
                            .children({
                                self.format_timing(self.server_total_ms()).map(|s| {
                                    div()
                                        .id("timing-server-total")
                                        .child(timing("results.status_bar.server_total_time", s))
                                        .app_tooltip(move |_window, cx| {
                                            crate::app::tooltip_view(
                                                crate::localization::text(
                                                    "results.status_bar.server_total_tooltip",
                                                ),
                                                cx,
                                            )
                                        })
                                })
                            })
                            .children({
                                self.format_timing(self.ui_paint_ms).map(|s| {
                                    div()
                                        .id("timing-paint")
                                        .child(timing("results.status_bar.paint_time", s))
                                        .app_tooltip(move |_window, cx| {
                                            crate::app::tooltip_view(
                                                crate::localization::text(
                                                    "results.status_bar.paint_time_tooltip",
                                                ),
                                                cx,
                                            )
                                        })
                                })
                            })
                            .children({
                                self.format_timing(self.wall_clock_ms).map(|s| {
                                    div()
                                        .id("timing-wall")
                                        .child(timing("results.status_bar.wall_time", s))
                                        .app_tooltip(move |_window, cx| {
                                            crate::app::tooltip_view(
                                                crate::localization::text(
                                                    "results.status_bar.wall_time_tooltip",
                                                ),
                                                cx,
                                            )
                                        })
                                })
                            })
                            .when(self.estimated_latency_ms.is_some(), |d| {
                                d.child(
                                    div()
                                        .w(px(1.0))
                                        .h(px(s(14.0)))
                                        .bg(Theme::border()),
                                )
                                .child(
                                    div()
                                        .id("timing-latency")
                                        .child(
                                            crate::localization::format(
                                                "results.status_bar.latency_time",
                                                &[(
                                                    "milliseconds",
                                                    &self.estimated_latency_ms.unwrap().to_string(),
                                                )],
                                            ),
                                        )
                                        .app_tooltip(move |_window, cx| {
                                            crate::app::tooltip_view(
                                                crate::localization::text(
                                                    "results.status_bar.latency_tooltip",
                                                ),
                                                cx,
                                            )
                                        }),
                                )
                                .child({
                                    let this = cx.entity();
                                    Checkbox::new("results_subtract_latency")
                                        .checked(self.subtract_latency)
                                        .label(crate::localization::text(
                                            "results.status_bar.subtract_latency",
                                        ))
                                        .tab_stop(true)
                                        .on_click(move |_checked: &bool, _window, app| {
                                            this.update(app, |this, cx| {
                                                this.toggle_subtract_latency(cx);
                                            });
                                        })
                                })
                            })
                            .child(
                                div()
                                    .w(px(1.0))
                                    .h(px(s(14.0)))
                                    .bg(Theme::border()),
                            )
                            .child({
                                let this = cx.entity();
                                Checkbox::new("results_timing_format")
                                    .checked(matches!(self.timing_format, TimingFormat::Elapsed))
                                    .label("hh:MM:ss")
                                    .tab_stop(true)
                                    .on_click(move |_checked: &bool, _window, app| {
                                        this.update(app, |this, cx| {
                                            this.toggle_timing_format(cx);
                                        });
                                    })
                            })
                        })
                })
            })
    }
}

impl ResultsView {
    pub(super) fn results_chrome_caption(&self) -> String {
        let n = self.result_sets.len();
        chrome_tab_caption(OutputPanelTab::Results.label(), if n > 1 { n } else { 0 })
    }

    pub(super) fn messages_chrome_caption(&self) -> String {
        chrome_tab_caption(OutputPanelTab::Messages.label(), self.messages_pane_count())
    }

    pub(super) fn diff_chrome_caption(&self) -> String {
        chrome_tab_caption(OutputPanelTab::Diff.label(), 0)
    }

    pub(super) fn plan_chrome_caption(&self, cx: &App) -> String {
        chrome_tab_caption(
            OutputPanelTab::Plan.label(),
            self.execution_plan_finding_count(cx).unwrap_or(0),
        )
    }

    fn unified_output_tabs(&self) -> Vec<OutputPanelTab> {
        OutputPanelTab::visible_tabs(crate::results::chrome::TabVisibility {
            chrome_visible: self.results_chrome_visible(),
            // The plan view is built lazily, the first time something asks for
            // an execution plan — so its existence IS "a plan was requested".
            plan: self.execution_plan_view.is_some(),
            ddl: self.ddl_panel.is_some(),
            preview_sql: !self.pending.is_empty(),
            diff: self.track_difference && !self.retained_diff_runs.is_empty(),
            stats: self.statistics_opened,
        })
    }

    fn render_unified_output_tab(
        &self,
        tab: OutputPanelTab,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        match tab {
            OutputPanelTab::Results => self.render_results_tab(cx),
            OutputPanelTab::Messages => self.render_messages_tab(cx),
            OutputPanelTab::Plan => self.render_execution_plan_tab(cx),
            OutputPanelTab::History => self.render_history_tab(cx),
            OutputPanelTab::Ddl => self.render_ddl_tab(cx),
            OutputPanelTab::PreviewSql => self.render_preview_sql_tab(cx),
            OutputPanelTab::Diff => self.render_diff_tab(cx),
            OutputPanelTab::Statistics => self.render_statistics_tab(cx),
        }
    }

    /// The Client Statistics tab-bar button. (The metrics themselves render in
    /// the content area via [`Self::render_statistics_pane`].)
    fn render_statistics_tab(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let is_active = self.active_output_tab() == OutputPanelTab::Statistics;
        self.tab_button(
            "statistics-tab",
            OutputPanelTab::Statistics.label(),
            is_active,
            OutputPanelTab::Statistics.tooltip(),
            |this, cx| {
                this.select_output_tab(OutputPanelTab::Statistics, cx);
            },
            cx,
        )
    }

    /// The Client Statistics pane: the run's metrics as a labelled key/value
    /// list (result sets, rows, and the timing breakdown).
    fn render_statistics_pane(&self, _cx: &mut Context<Self>) -> gpui::AnyElement {
        let s = Theme::scaled;
        let rows = super::client_stats::client_stat_rows(&self.client_stat_inputs());
        div()
            .id("client-statistics-pane")
            .size_full()
            .overflow_y_scroll()
            .p(px(s(12.0)))
            .text_size(px(Theme::font_size()))
            .text_color(Theme::text())
            .child(div().text_color(Theme::text_dim()).pb(px(s(8.0))).child(
                crate::localization::text("results.client_statistics.last_run_caption"),
            ))
            .children(rows.into_iter().map(|row| {
                div()
                    .flex()
                    .flex_row()
                    .items_baseline()
                    .gap(px(s(8.0)))
                    .py(px(s(3.0)))
                    .border_b_1()
                    .border_color(Theme::border())
                    .child(
                        div()
                            .w(px(s(200.0)))
                            .text_color(Theme::text_dim())
                            .child(row.label),
                    )
                    .child(div().min_w(px(s(90.0))).child(row.value))
                    .child(
                        div()
                            .flex_1()
                            .text_color(Theme::text_dim())
                            .text_size(px(Theme::font_size_ratio(0.85)))
                            .child(row.note),
                    )
            }))
            .into_any_element()
    }

    /// A generic tab-bar button (used for the aux tabs that are the same in
    /// every layout mode: Results, Messages, Diff, Plan). Active tabs get an
    /// amber underline; inactive tabs stay dim sans text with no boxed fill.
    pub(super) fn tab_button(
        &self,
        id: &'static str,
        label: impl Into<SharedString>,
        is_active: bool,
        tooltip: impl Into<SharedString>,
        on_click: impl Fn(&mut Self, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let tooltip = tooltip.into();
        chrome_tab_shell(id, is_active)
            .child(div().flex().flex_1().items_center().child(label.into()))
            .child(chrome_tab_underline(is_active))
            .app_tooltip(move |_window, cx| crate::app::tooltip_view(tooltip.clone(), cx))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    on_click(this, cx);
                    cx.notify();
                }),
            )
            .into_any_element()
    }

    /// The single "Results" tab shown in stacked/canvas modes, where all result
    /// sets live in one scrollable body instead of one tab each. Active whenever
    /// none of the aux panes (Messages / Diff / Plan) is showing.
    fn render_results_tab(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let is_active =
            self.active_output_tab() == OutputPanelTab::Results && !self.show_result_profile;
        self.tab_button(
            "results-tab",
            self.results_chrome_caption(),
            is_active,
            OutputPanelTab::Results.tooltip(),
            |this, cx| {
                this.select_output_tab(OutputPanelTab::Results, cx);
            },
            cx,
        )
    }

    fn render_run_history_strip(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if self.retained_runs.is_empty() {
            return None;
        }
        let s = Theme::scaled;
        let current_selected = self.viewing_run.is_none();
        let all_pinned = self.retained_runs.iter().all(|run| run.pinned);
        let full = self.run_history_limit > 0 && self.retained_runs.len() >= self.run_history_limit;
        let show_hint = all_pinned && full;

        let mut chips = div()
            .id("run-history-strip")
            .flex()
            .flex_row()
            .items_center()
            .gap(px(s(6.0)))
            .px(px(s(8.0)))
            .py(px(s(4.0)))
            .border_b_1()
            .border_color(Theme::border())
            .overflow_x_scroll()
            .child(
                div()
                    .id("run-history-icon")
                    .flex()
                    .items_center()
                    .flex_shrink_0()
                    .child(
                        Icon::empty()
                            .path(crate::icon_catalog::AppIcon::History.path())
                            .flex_shrink_0(),
                    )
                    .app_tooltip(move |_window, cx| {
                        crate::app::tooltip_view(
                            crate::localization::text("results.run_history.strip_tooltip"),
                            cx,
                        )
                    }),
            )
            .child(
                run_history_chip(
                    Button::new("run-history-current").label(crate::localization::text(
                        "results.run_history.current_button",
                    )),
                    current_selected,
                )
                .on_click(cx.listener(|this, _, _, cx| this.switch_to_run(None, cx))),
            );

        for run in self.retained_runs.iter().rev() {
            let id = run.id;
            let selected = self.viewing_run == Some(id);
            let pinned = run.pinned;
            let rows: usize = run.sets.iter().map(|set| set.row_count).sum();
            let elapsed = run
                .elapsed_ms
                .map(format_duration_ms)
                .unwrap_or_else(|| "—".to_string());
            let time = format_run_clock(run.completed_at);
            let label = crate::localization::format(
                "results.run_history.run_chip_label",
                &[
                    ("run_time", &time),
                    ("row_count", &format_count(rows)),
                    ("elapsed_time", &elapsed),
                ],
            );
            let tooltip = retained_run_chip_tooltip(run);
            chips = chips.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(2.0)))
                    .child(
                        run_history_chip(
                            Button::new(("run-history-run", id)).label(label),
                            selected,
                        )
                        .app_tooltip_text(tooltip)
                        .when(pinned, |btn| btn.icon(IconName::Star))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.switch_to_run(Some(id), cx);
                        })),
                    )
                    .child(
                        Button::new(("run-history-pin", id))
                            .small()
                            .ghost()
                            .selected(pinned)
                            .icon(IconName::Star)
                            .app_tooltip_text(crate::localization::text(if pinned {
                                "results.run_history.unpin_tooltip"
                            } else {
                                "results.run_history.pin_tooltip"
                            }))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.pin_run(id, !pinned, cx);
                            })),
                    )
                    .child(
                        Button::new(("run-history-close", id))
                            .small()
                            .ghost()
                            .icon(IconName::Close)
                            .app_tooltip_text(crate::localization::text(
                                "results.run_history.close_run_tooltip",
                            ))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.close_run(id, cx);
                            })),
                    ),
            );
        }

        if show_hint {
            chips = chips.child(
                div()
                    .text_color(Theme::text_dim())
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .child(crate::localization::text(
                        "results.run_history.all_pinned_hint",
                    )),
            );
        }

        Some(chips.into_any_element())
    }

    /// The always-present Messages tab. Unlike a per-result-set tab it is never
    /// gated on there being results — PRINT output, diagnostics, and run
    /// messages are worth a home even for a batch that returned no rows.
    fn render_messages_tab(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let is_active = self.active_output_tab() == OutputPanelTab::Messages;
        self.tab_button(
            "messages-tab",
            self.messages_chrome_caption(),
            is_active,
            OutputPanelTab::Messages.tooltip(),
            |this, cx| {
                this.select_output_tab(OutputPanelTab::Messages, cx);
            },
            cx,
        )
    }

    /// The Diff tab — present only while difference tracking is on and a diff
    /// has been computed. Returns an empty element otherwise.
    fn render_diff_tab(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        if !self.track_difference || self.retained_diff_runs.is_empty() {
            return div().into_any_element();
        }
        let is_active = self.active_output_tab() == OutputPanelTab::Diff;
        self.tab_button(
            "diff-tab",
            self.diff_chrome_caption(),
            is_active,
            OutputPanelTab::Diff.tooltip(),
            |this, cx| {
                this.select_output_tab(OutputPanelTab::Diff, cx);
            },
            cx,
        )
    }

    /// The Plan tab — always present once chrome is visible.
    fn render_execution_plan_tab(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let is_active = self.active_output_tab() == OutputPanelTab::Plan;
        self.tab_button(
            "execution-plan-toggle",
            self.plan_chrome_caption(cx),
            is_active,
            OutputPanelTab::Plan.tooltip(),
            |this, cx| {
                this.select_output_tab(OutputPanelTab::Plan, cx);
            },
            cx,
        )
    }

    fn render_history_tab(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let is_active = self.active_output_tab() == OutputPanelTab::History;
        self.tab_button(
            "history-output-tab",
            OutputPanelTab::History.label(),
            is_active,
            OutputPanelTab::History.tooltip(),
            |this, cx| {
                this.select_output_tab(OutputPanelTab::History, cx);
            },
            cx,
        )
    }

    fn render_ddl_tab(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let is_active = self.active_output_tab() == OutputPanelTab::Ddl;
        self.tab_button(
            "ddl-output-tab",
            OutputPanelTab::Ddl.label(),
            is_active,
            OutputPanelTab::Ddl.tooltip(),
            |this, cx| {
                this.select_output_tab(OutputPanelTab::Ddl, cx);
            },
            cx,
        )
    }

    fn render_history_pane(&self) -> gpui::AnyElement {
        #[cfg(not(target_arch = "wasm32"))]
        {
            if let Some(panel) = &self.history_panel {
                panel.clone().into_any_element()
            } else {
                div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text(
                        "results.history.native_unavailable",
                    ))
                    .into_any_element()
            }
        }
        #[cfg(target_arch = "wasm32")]
        {
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(Theme::text_dim())
                .child(crate::browser_preview::unavailable(
                    &crate::localization::text("results.history.browser_unavailable"),
                ))
                .into_any_element()
        }
    }

    fn render_ddl_pane(&self) -> gpui::AnyElement {
        if let Some(panel) = &self.ddl_panel {
            return panel.clone().into_any_element();
        }
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .text_color(Theme::text_dim())
            .child(crate::localization::text(
                "results.ddl.select_object_empty_state",
            ))
            .into_any_element()
    }

    fn render_preview_sql_tab(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let is_active = self.active_output_tab() == OutputPanelTab::PreviewSql;
        self.tab_button(
            "preview-sql-output-tab",
            OutputPanelTab::PreviewSql.label(),
            is_active,
            OutputPanelTab::PreviewSql.tooltip(),
            |this, cx| {
                this.select_output_tab(OutputPanelTab::PreviewSql, cx);
            },
            cx,
        )
    }

    fn render_preview_sql_pane(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        if self.preview_sql_text.is_empty() {
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(Theme::text_dim())
                .child(crate::localization::text("results.preview_sql.empty_state"))
                .into_any_element();
        }
        let s = Theme::scaled;
        div()
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_end()
                    .px(px(s(8.0)))
                    .py(px(s(4.0)))
                    .border_b_1()
                    .border_color(Theme::border())
                    .child(
                        Button::new("preview-sql-copy")
                            .ghost()
                            .xsmall()
                            .label(crate::localization::text("results.preview_sql.copy_button"))
                            .app_tooltip_text(crate::localization::text(
                                "results.preview_sql.copy_tooltip",
                            ))
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.copy_preview_sql(cx);
                            })),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .overflow_hidden()
                    .p(px(Theme::scaled(8.0)))
                    .font_family(Theme::editor_font_family())
                    .text_size(px(Theme::font_size_ratio(0.9)))
                    .text_color(Theme::text())
                    .child(self.preview_sql_text.clone()),
            )
            .into_any_element()
    }

    /// Result layout lives in results chrome (9.11), not on the command bar.
    fn render_pane_layout_buttons(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let maximized = self.pane_maximized;
        let maximize_tip = crate::localization::text(if maximized {
            "results.pane.restore_editor_tooltip"
        } else {
            "results.pane.maximize_results_tooltip"
        });
        let maximize_icon = if maximized {
            crate::icon_catalog::AppIcon::ChevronDown
        } else {
            crate::icon_catalog::AppIcon::ChevronUp
        };
        div()
            .flex()
            .flex_row()
            .items_center()
            .child(
                Button::new("results-maximize")
                    .icon(Icon::empty().path(maximize_icon.path()))
                    .ghost()
                    .xsmall()
                    .app_tooltip_text(maximize_tip)
                    .on_click(cx.listener(|_, _, _, cx| {
                        cx.emit(ResultsViewEvent::MaximizeToggleRequested);
                    })),
            )
            .child(
                Button::new("results-detach")
                    .icon(Icon::empty().path(crate::icon_catalog::AppIcon::Frame.path()))
                    .ghost()
                    .xsmall()
                    .app_tooltip_text(crate::localization::text(
                        "results.pane.detach_results_tooltip",
                    ))
                    .on_click(cx.listener(|_, _, _, cx| {
                        cx.emit(ResultsViewEvent::DetachRequested);
                    })),
            )
            .into_any_element()
    }

    /// Result layout lives in results chrome (9.11), not on the command bar.
    fn render_result_layout_control(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let stacked = self.layout_mode == LayoutMode::Stacked;
        let tip = crate::localization::text(if stacked {
            "results.layout.stacked_tooltip"
        } else {
            "results.layout.tabbed_tooltip"
        });
        div()
            .id("result-layout")
            .h_full()
            .px(px(Theme::scaled(8.0)))
            .flex()
            .items_center()
            .gap(px(Theme::scaled(4.0)))
            .cursor_pointer()
            .font(Theme::ui_font())
            .text_size(px(Theme::font_size()))
            .text_color(Theme::text_dim())
            .hover(|st| st.text_color(Theme::text()).bg(Theme::row_hover()))
            .app_tooltip(move |_window, cx| crate::app::tooltip_view(tip.to_string(), cx))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    let next = if this.layout_mode == LayoutMode::Stacked {
                        LayoutMode::Tabbed
                    } else {
                        LayoutMode::Stacked
                    };
                    this.set_layout_mode(next, cx);
                    cx.emit(ResultsViewEvent::LayoutModeChanged(next));
                }),
            )
            .child(
                Icon::empty()
                    .path(if stacked {
                        crate::icon_catalog::AppIcon::ResultsStacked.path()
                    } else {
                        crate::icon_catalog::AppIcon::ResultsInTabs.path()
                    })
                    .size(px(Theme::toolbar_icon_size())),
            )
            .child(crate::localization::text("results.layout.control_label"))
            .into_any_element()
    }

    /// The Export control that sits at the right edge of the tab bar in every
    /// layout mode — an accent text link, not a boxed button.
    fn render_export_control(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let s = Theme::scaled;
        // Shift-picking a text format saves to a file instead of copying, which
        // is otherwise undiscoverable, so the tooltip carries it.
        let tip = crate::localization::text("results.export_menu.toolbar_tooltip");
        div()
            .id("export-toggle")
            .px(px(s(12.0)))
            .h_full()
            .flex()
            .items_center()
            .cursor_pointer()
            .font(Theme::ui_font())
            .text_size(px(Theme::font_size()))
            .font_weight(FontWeight::MEDIUM)
            .text_color(Theme::text_dim())
            // Matches every other control on this row, and the hover rule in
            // DESIGN.md: a `row_hover` fill, not a bare text-shade shift.
            .hover(|st| st.text_color(Theme::text()).bg(Theme::row_hover()))
            .app_tooltip(move |_window, cx| crate::app::tooltip_view(tip.to_string(), cx))
            .child(crate::localization::text(
                "results.export_menu.toolbar_button",
            ))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    this.toggle_export_menu(cx);
                }),
            )
            .into_any_element()
    }

    /// Render retained-run selection, optional composite keys, and the typed
    /// added/removed/changed detail table.
    /// The messages pane (PRINT output, RAISERROR, cancellation notices).
    ///
    /// Extracted so it can render either as the fallback when a batch returned
    /// no rows, or on demand via the footer's Messages toggle when it returned
    /// both — the case that used to be unreachable.
    fn render_messages_pane(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let s = Theme::scaled;
        self.ensure_messages_input(window, cx);
        self.sync_messages_text(window, cx);
        let input = self
            .messages_input
            .clone()
            .expect("ensure_messages_input just created it");
        let mut pane = div()
            .id("messages-pane")
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .text_size(px(Theme::font_size()))
            .text_color(Theme::text())
            .font_family(Theme::editor_font_family());
        if let Some(offset) = self.execution_error_offset {
            pane = pane.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .px(px(s(8.0)))
                    .py(px(s(4.0)))
                    .gap(px(s(8.0)))
                    .bg(Theme::panel_bg())
                    .border_b_1()
                    .border_color(Theme::border())
                    .font(Theme::ui_font())
                    .child(
                        Button::new("go-to-error")
                            .ghost()
                            .small()
                            .label(crate::localization::text(
                                "results.messages.go_to_error_button",
                            ))
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.emit(ResultsViewEvent::GoToEditorOffset(offset));
                            })),
                    ),
            );
        }
        pane.child(
            div().flex_1().p(px(s(8.0))).child(
                Textarea::new(&input)
                    .h_full()
                    .readonly(true)
                    // No box, no border: the messages are the pane's content,
                    // not a form field sitting inside it.
                    .appearance(false)
                    .bordered(false),
            ),
        )
        .into_any_element()
    }
    /// Cycle the diff baseline through the named snapshots (… → none).
    #[cfg(not(target_arch = "wasm32"))]
    fn cycle_snapshot_baseline(&mut self, cx: &mut Context<Self>) {
        let store = crate::result_snapshots::SnapshotStore::default_store();
        let snapshots = store.list();
        if snapshots.is_empty() {
            self.diff_unavailable_reason = Some(crate::localization::text(
                "results.diff.no_snapshots_available",
            ));
            cx.notify();
            return;
        }
        let next = match self.diff_snapshot_index {
            None => Some(0),
            Some(index) if index + 1 < snapshots.len() => Some(index + 1),
            Some(_) => None,
        };
        self.diff_snapshot_index = next;
        self.diff_snapshot_baseline = next.and_then(|index| {
            let meta = snapshots.get(index)?;
            let text = store.read(&meta.id).ok()?;
            let parsed = crate::import_export::parse_sqlly_export_snapshot(&text).ok()?;
            Some(DiffSnapshotBaseline {
                name: meta.name.clone(),
                columns: parsed.result_set.columns,
                rows: parsed.result_set.rows,
            })
        });
        if next.is_some() && self.diff_snapshot_baseline.is_none() {
            self.diff_unavailable_reason = Some(crate::localization::text(
                "results.diff.snapshot_load_failed",
            ));
            self.diff_snapshot_index = None;
        }
        self.recompute_detailed_diff(cx);
    }

    fn set_diff_class_filter(&mut self, filter: Option<&'static str>, cx: &mut Context<Self>) {
        self.diff_class_filter = filter;
        if let Some(diff) = self.detailed_diff.take() {
            self.detailed_diff_grid = self.build_detailed_diff_grid(&diff, cx);
            self.detailed_diff = Some(diff);
        }
        cx.notify();
    }

    /// Export the (filtered) diff classification table as CSV via a save
    /// dialog.
    #[cfg(not(target_arch = "wasm32"))]
    fn export_diff_csv(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(diff) = &self.detailed_diff else {
            return;
        };
        let (columns, rows) = detailed_diff_table(diff, self.diff_class_filter);
        let opts = adhoc_export_options(ExportFormat::Csv, &self.effective_export_prefs());
        let csv = crate::result_export::csv_with(&columns, &rows, &opts);
        let rx = cx.prompt_for_new_path(std::path::Path::new("."), Some("result-diff.csv"));
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(path))) = rx.await
                && let Err(message) = write_export_bytes(&path, csv.as_bytes(), false)
            {
                let _ = this.update_in(cx, |this, window, cx| {
                    this.note_export_write_failed(message, window, cx);
                });
            }
        })
        .detach();
    }

    fn render_diff_view(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let s = Theme::scaled;
        let mut content = div()
            .id("diff-view")
            .flex()
            .flex_col()
            .size_full()
            .overflow_y_scroll()
            .p(px(s(8.0)))
            .gap(px(s(8.0)));
        content = content.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(s(8.0)))
                .font(Theme::ui_font())
                .child(
                    div()
                        .text_color(Theme::text_dim())
                        .child(crate::localization::text("results.diff.baseline_label")),
                )
                .child(
                    Button::new("diff-baseline-run")
                        .small()
                        .label(self.selected_diff_run_label(true))
                        .on_click(cx.listener(|this, _, _, cx| this.cycle_diff_run(true, cx))),
                )
                .child(
                    div()
                        .text_color(Theme::text_dim())
                        .child(crate::localization::text("results.diff.comparison_label")),
                )
                .child(
                    Button::new("diff-comparison-run")
                        .small()
                        .label(self.selected_diff_run_label(false))
                        .on_click(cx.listener(|this, _, _, cx| this.cycle_diff_run(false, cx))),
                )
                .child(
                    div()
                        .text_color(Theme::text_dim())
                        .child(crate::localization::format(
                            "results.diff.retained_run_count",
                            &[("run_count", &self.retained_diff_runs.len().to_string())],
                        )),
                )
                .child(
                    Button::new("diff-snapshot-baseline")
                        .small()
                        .label(match &self.diff_snapshot_baseline {
                            Some(snapshot) => crate::localization::format(
                                "results.diff.snapshot_named",
                                &[("snapshot_name", &snapshot.name)],
                            ),
                            None => crate::localization::text("results.diff.snapshot_none"),
                        })
                        .on_click(cx.listener(|this, _, _, cx| {
                            #[cfg(not(target_arch = "wasm32"))]
                            this.cycle_snapshot_baseline(cx);
                            #[cfg(target_arch = "wasm32")]
                            {
                                let _ = cx;
                            }
                        })),
                ),
        );

        // Change-class filter chips + export.
        let mut filter_row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(s(6.0)))
            .font(Theme::ui_font())
            .child(
                div()
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text("results.diff.show_label")),
            );
        for (index, (key, value)) in [
            ("results.diff.filter_all", None),
            ("results.diff.filter_added", Some("Added")),
            ("results.diff.filter_removed", Some("Removed")),
            ("results.diff.filter_changed", Some("Changed")),
        ]
        .into_iter()
        .enumerate()
        {
            let selected = self.diff_class_filter == value;
            filter_row = filter_row.child(
                Button::new(("diff-class-filter", index))
                    .xsmall()
                    .label(crate::localization::text(key))
                    .selected(selected)
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.set_diff_class_filter(value, cx)),
                    ),
            );
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            filter_row = filter_row.child(
                Button::new("diff-export-csv")
                    .xsmall()
                    .label(crate::localization::text("results.diff.export_csv_button"))
                    .disabled(self.detailed_diff.is_none())
                    .on_click(cx.listener(|this, _, window, cx| this.export_diff_csv(window, cx))),
            );
        }
        content = content.child(filter_row);

        let key_columns = self
            .diff_baseline_run
            .and_then(|id| self.retained_diff_runs.iter().find(|run| run.id == id))
            .and_then(|run| run.sets.get(self.active_tab))
            .map(|set| set.columns.clone())
            .unwrap_or_default();
        let mut key_row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(s(8.0)))
            .overflow_x_scrollbar()
            .font(Theme::ui_font())
            .child(
                div()
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text("results.diff.key_columns_label")),
            );
        for (index, column) in key_columns.iter().enumerate() {
            let entity = cx.entity();
            key_row = key_row.child(
                Checkbox::new(("diff-key-column", index))
                    .checked(self.diff_key_columns.contains(&index))
                    .label(column.name.clone())
                    .on_click(move |_checked, _window, app| {
                        entity.update(app, |this, cx| this.toggle_diff_key_column(index, cx));
                    }),
            );
        }
        content = content.child(key_row);

        if let Some(diff) = &self.detailed_diff {
            content = content.child(div().text_color(Theme::text()).child(
                crate::localization::format(
                    "results.diff.summary_label",
                    &[("summary", &diff.summary())],
                ),
            ));
            let detail_rows = diff.added.len()
                + diff.removed.len()
                + diff
                    .changed
                    .iter()
                    .map(|row| row.changes.len())
                    .sum::<usize>();
            content = content.child(self.render_diff_slot(
                &crate::localization::text("results.diff.typed_changes_title"),
                detail_rows,
                self.detailed_diff_grid.as_ref(),
                &crate::localization::text(if diff.is_identical() {
                    "results.diff.no_differences"
                } else {
                    "results.diff.preparing_changes"
                }),
                None,
                cx,
            ));
        } else {
            content = content.child(
                div()
                    .border_1()
                    .border_color(Theme::border())
                    .p(px(s(12.0)))
                    .text_color(Theme::warning())
                    .child(self.diff_unavailable_reason.clone().unwrap_or_else(|| {
                        crate::localization::text("results.diff.choose_two_runs")
                    })),
            );
        }
        content.into_any_element()
    }

    /// Render a single diff slot as a bordered card with a header (title +
    /// row count) and either a real grid or a centered placeholder message.
    fn render_diff_slot(
        &self,
        title: &str,
        row_count: usize,
        grid: Option<&Entity<SqllyDataTable>>,
        placeholder: &str,
        _extra: Option<()>,
        _cx: &mut Context<Self>,
    ) -> gpui::Div {
        let grid_h = adaptive_card_grid_height(row_count);
        let mut card = div()
            .flex()
            .flex_col()
            .border_1()
            .border_color(Theme::border())
            .bg(Theme::bg())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .h(px(Theme::scaled(26.0)))
                    .px(px(Theme::scaled(8.0)))
                    .items_center()
                    .bg(Theme::bg())
                    .border_b_1()
                    .border_color(Theme::border())
                    .font(Theme::ui_font())
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::format(
                        "results.diff.slot_row_count_title",
                        &[("title", title), ("row_count", &row_count.to_string())],
                    )),
            );

        if let Some(g) = grid {
            // NOTE: width-only fill here. `.size_full()` (height:100%) inside the
            // enclosing `overflow_y_scroll` diff container collapses the grid's
            // hitbox so no mouse events reach it, killing all selection. The grid
            // already has an explicit fixed height via `.h(grid_h)`.
            card = card.child(div().h(px(grid_h)).w_full().child(g.clone()));
        } else if row_count == 0 {
            card = card.child(
                div()
                    .flex()
                    .h(px(grid_h))
                    .items_center()
                    .justify_center()
                    .text_color(Theme::text_dim())
                    .child(placeholder.to_string()),
            );
        } else {
            card = card.child(
                div()
                    .flex()
                    .h(px(grid_h))
                    .items_center()
                    .justify_center()
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text(
                        "results.loading.preparing_diff_grid",
                    )),
            );
        }
        card
    }

    /// Render stacked layout: a scrollable column of collapsible cards.
    fn render_stacked(
        &self,
        all_grids: &[(usize, String, Option<Entity<SqllyDataTable>>, bool)],
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let s = Theme::scaled;
        let single_result = all_grids.len() == 1;
        let stack = div()
            .id("stacked-results")
            .flex()
            .flex_col()
            .size_full()
            .when(single_result, |stack| stack.overflow_hidden())
            .when(!single_result, |stack| {
                stack
                    .overflow_y_scroll()
                    .track_scroll(&self.stacked_scroll_handle)
            })
            .on_scroll(cx.listener(|this, event, _window, cx| {
                if this.handle_grid_zoom_wheel(event, cx) {
                    return;
                }
                this.handle_stacked_scroll(event, cx);
            }))
            .on_pinch(cx.listener(|this, event: &gpui::PinchEvent, _window, cx| {
                this.handle_grid_pinch(event, cx);
            }))
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _window, cx| {
                let Some(resize) = this.stacked_resize else {
                    return;
                };
                let height = stacked_resize_height(resize, f32::from(event.position.y));
                if let Some(card_height) = this.stacked_card_heights.get_mut(resize.slot) {
                    *card_height = Some(height);
                }
                cx.notify();
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _event: &MouseUpEvent, _window, cx| {
                    if this.stacked_resize.take().is_some() {
                        cx.notify();
                    }
                }),
            );

        stack
            .children(all_grids.iter().flat_map(|(slot, label, grid, collapsed)| {
                let row_count = self.row_count_for_slot(*slot);
                let collapse_arrow = crate::icons::disclosure_chevron(!*collapsed);
                let slot_idx = *slot;

                let header = div()
                    .id(ElementId::NamedInteger(
                        "stacked-result-header".into(),
                        slot_idx as u64,
                    ))
                    .debug_selector(move || format!("stacked-result-header-{slot_idx}"))
                    .flex()
                    .flex_row()
                    .h(px(Theme::scaled(26.0)))
                    .px(px(s(8.0)))
                    .items_center()
                    .bg(Theme::bg())
                    .border_b_1()
                    .border_color(Theme::border())
                    .font(Theme::ui_font())
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text_dim())
                    .hover(|st| st.bg(Theme::row_hover()))
                    // Right-click the result's name: Show by Default / Set
                    // Number of Rows / Comment Out Result.
                    .on_mouse_down(
                        gpui::MouseButton::Right,
                        cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            this.open_result_name_menu(slot_idx, ev.position, window, cx);
                        }),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(s(4.0)))
                            .child(collapse_arrow)
                            .child(self.render_lock_button(slot_idx, cx))
                            .child(label.clone())
                            .on_mouse_down(
                                gpui::MouseButton::Left,
                                cx.listener(move |this, _, _, cx| {
                                    this.toggle_card_collapsed(slot_idx, cx);
                                }),
                            ),
                    )
                    .child(
                        div()
                            .id(ElementId::NamedInteger(
                                "stacked-result-settings".into(),
                                slot_idx as u64,
                            ))
                            .debug_selector(move || format!("stacked-result-settings-{slot_idx}"))
                            .ml(px(s(3.0)))
                            .text_size(px(Theme::font_size() * 1.5))
                            .text_color(Theme::text_dim())
                            .child("\u{270E}")
                            .on_mouse_down(
                                gpui::MouseButton::Left,
                                cx.listener(move |this, _, _window, cx| {
                                    this.open_result_settings(slot_idx, cx);
                                }),
                            ),
                    )
                    .child(div().ml(px(s(8.0))).text_color(Theme::text_dim()).child(
                        crate::localization::format(
                            "results.stacked_cards.row_count",
                            &[("row_count", &row_count.to_string())],
                        ),
                    ))
                    .child(div().flex_1())
                    .child(self.render_stacked_card_controls(slot_idx, cx));

                // A `-- sqlly lines:` allocation sizes the card for that many
                // data rows; otherwise the height adapts to the row count. A
                // dragged divider overrides either.
                let default_height = self
                    .layout_directives
                    .lines_for(slot_idx)
                    .map(|lines| card_total_height_for_lines(lines, self.live_grid_metrics()))
                    .unwrap_or_else(|| adaptive_card_total_height(row_count));
                let card_height = self
                    .stacked_card_heights
                    .get(slot_idx)
                    .and_then(|height| *height)
                    .unwrap_or(default_height);
                let mut card = div().flex().flex_col().bg(Theme::bg()).child(header);
                if !*collapsed {
                    card = if single_result {
                        card.flex_1().min_h(px(0.0))
                    } else {
                        // Keep the allotted height: a shrinking card would
                        // squeeze every set to fit the pane, defeating
                        // `-- sqlly lines:`, divider drags, and scrolling a
                        // `-- sqlly focus:` card into view.
                        card.h(px(card_height)).flex_shrink_0()
                    };
                    if let Some(g) = grid {
                        card = card.child(
                            div()
                                .flex_1()
                                .min_h(px(0.0))
                                .w_full()
                                .overflow_hidden()
                                .child(self.wrap_grid_copy_capture(slot_idx, g.clone(), cx)),
                        );
                    } else {
                        card = card.child(
                            div()
                                .flex()
                                .flex_1()
                                .min_h(px(0.0))
                                .items_center()
                                .justify_center()
                                .text_color(Theme::text_dim())
                                .child(crate::localization::text(
                                    "results.loading.preparing_stacked_grid",
                                )),
                        );
                    }
                    let footer_text = self.selection_stats_text_for_slot(*slot, cx);
                    card = card.child(
                        div()
                            .flex()
                            .flex_row()
                            .h(px(CARD_FOOTER_ALLOWANCE))
                            .border_t_1()
                            .border_color(Theme::border())
                            .bg(Theme::bg())
                            .px(px(s(8.0)))
                            .items_center()
                            .font(Theme::ui_font())
                            .text_size(px(Theme::font_size_ratio(0.92)))
                            .text_color(Theme::text_dim())
                            .child(footer_text.unwrap_or_default()),
                    );
                }

                let mut elements = vec![card.into_any_element()];
                // Render a divider below every card, including the last, so
                // each result set (the last one included) can be resized by
                // dragging its own bottom divider. The divider resizes the
                // card above it (`slot_idx`).
                if !single_result {
                    let entity = cx.entity();
                    elements.push(
                        div()
                            .id(gpui::SharedString::from(format!(
                                "stacked-result-divider-{slot_idx}"
                            )))
                            .debug_selector(|| format!("stacked-result-divider-{slot_idx}"))
                            .child(
                                PaneDivider::horizontal(
                                    format!("stacked-result-divider-control-{slot_idx}"),
                                    CollapseDirection::Down,
                                )
                                .on_drag_start(
                                    move |start_y, _window, app| {
                                        entity.update(app, |this, cx| {
                                            if this.stacked_card_heights.len() <= slot_idx {
                                                this.stacked_card_heights
                                                    .resize(slot_idx + 1, None);
                                            }
                                            let start_height = this
                                                .stacked_card_heights
                                                .get(slot_idx)
                                                .and_then(|height| *height)
                                                .unwrap_or(default_height);
                                            this.stacked_resize = Some(StackedResize {
                                                slot: slot_idx,
                                                start_y,
                                                start_height,
                                            });
                                            cx.notify();
                                        });
                                    },
                                ),
                            )
                            .into_any_element(),
                    );
                }
                elements
            }))
            .into_any_element()
    }

    /// Render canvas layout: result sets placed in a grid based on SQL
    /// comment directives. Falls back to stacked when no layout is parsed.
    ///
    /// The board is a real CSS grid: `rows x columns` equal `minmax(0, 1fr)`
    /// tracks that fill the whole results pane, with every placement pinned
    /// to its explicit grid lines so row and column spans take exactly the
    /// cells they claim. Cells no placement covers are drawn as empty
    /// background, and the 1px gap over a border-colored board draws the
    /// cell separators.
    fn render_canvas(
        &self,
        all_grids: &[(usize, String, Option<Entity<SqllyDataTable>>, bool)],
        spec: Option<&crate::result_layout::QueryResultLayoutSpec>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(spec) = spec else {
            // No SQL → fall back to stacked.
            return self.render_stacked(all_grids, cx);
        };
        // An ignored board (e.g. larger than the 8x8 maximum) carries its
        // dimensions plus an error; honor "ignored" and stack instead.
        if !spec.is_valid() {
            return self.render_stacked(all_grids, cx);
        }
        let s = Theme::scaled;

        let mut canvas = div()
            .id("canvas-results")
            .debug_selector(|| "canvas-results".into())
            .grid()
            .grid_rows(canvas_track_count(spec.rows))
            .grid_cols(canvas_track_count(spec.columns))
            .gap(px(1.0))
            .overflow_hidden()
            .bg(Theme::border());

        for row in 0..spec.rows {
            for col in 0..spec.columns {
                let Some(placement) = spec.placement_at(row, col) else {
                    // Uncovered cell: empty background, pinned to its own
                    // lines so auto-placement can never shuffle it.
                    canvas = canvas.child(canvas_cell_area(
                        div().bg(Theme::sidebar_bg()),
                        (row, row),
                        (col, col),
                    ));
                    continue;
                };
                if placement.row_start != row || placement.column_start != col {
                    // Covered by a spanning placement drawn at its start cell.
                    continue;
                }
                let rows = (placement.row_start, placement.row_end);
                let cols = (placement.column_start, placement.column_end);
                // A placement with no result set yet (its statement has not
                // produced one, or produced none) is an empty cell, never a
                // second copy of some other result set.
                let Some(slot) = placement.result_set_index else {
                    canvas =
                        canvas.child(canvas_cell_area(div().bg(Theme::sidebar_bg()), rows, cols));
                    continue;
                };
                let grid = all_grids
                    .iter()
                    .find(|(grid_slot, _, _, _)| *grid_slot == slot)
                    .and_then(|(_, _, g, _)| g.clone());
                let header = div()
                    .flex()
                    .flex_row()
                    .flex_none()
                    .h(px(Theme::scaled(22.0)))
                    .px(px(s(6.0)))
                    .gap(px(s(8.0)))
                    .items_center()
                    .bg(Theme::bg())
                    .border_b_1()
                    .border_color(Theme::border())
                    .font(Theme::ui_font())
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .text_color(Theme::text_dim())
                    .child(
                        div()
                            .min_w(px(0.0))
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(self.result_set_label(slot)),
                    )
                    .child(crate::localization::format(
                        "results.canvas.row_count",
                        &[("row_count", &self.row_count_for_slot(slot).to_string())],
                    ));
                let body = match grid {
                    Some(g) => div()
                        .flex_1()
                        .min_h(px(0.0))
                        .w_full()
                        .overflow_hidden()
                        .child(self.wrap_grid_copy_capture(slot, g, cx)),
                    None => div()
                        .flex()
                        .flex_1()
                        .min_h(px(0.0))
                        .items_center()
                        .justify_center()
                        .text_color(Theme::text_dim())
                        .child(crate::localization::text(
                            "results.loading.preparing_canvas_grid",
                        )),
                };
                let cell = div()
                    .id(SharedString::from(format!("canvas-cell-{slot}")))
                    .debug_selector(move || format!("canvas-cell-{slot}"))
                    .flex()
                    .flex_col()
                    .bg(Theme::bg())
                    .child(header)
                    .child(body);
                canvas = canvas.child(canvas_cell_area(cell, rows, cols));
            }
        }

        // A result set left with no open cell is otherwise simply missing
        // from the board. Say so under it (SQL-visible problems squiggle in
        // the editor instead).
        let notes = canvas_layout_notes(spec);
        if notes.is_empty() {
            return canvas.size_full().into_any_element();
        }
        div()
            .flex()
            .flex_col()
            .size_full()
            .child(canvas.flex_1().min_h(px(0.0)).w_full())
            .child(
                div()
                    .id("canvas-layout-notes")
                    .debug_selector(|| "canvas-layout-notes".into())
                    .flex()
                    .flex_col()
                    .flex_none()
                    .px(px(s(8.0)))
                    .py(px(s(4.0)))
                    .gap(px(s(2.0)))
                    .border_t_1()
                    .border_color(Theme::border())
                    .bg(Theme::bg())
                    .font(Theme::ui_font())
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .text_color(Theme::warning())
                    .children(notes),
            )
            .into_any_element()
    }

    fn render_staged_edit_controls(&self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        if self.pending.is_empty() {
            return Vec::new();
        }
        let s = Theme::scaled;
        let (edited, inserted, deleted) = self.pending.counts();
        let label = crate::localization::format(
            "results.staged_changes.count_summary",
            &[
                ("edited_count", &edited.to_string()),
                ("inserted_count", &inserted.to_string()),
                ("deleted_count", &deleted.to_string()),
            ],
        );
        // An older relay bridge cannot commit the batch: say so here and
        // disable Save rather than offer a save that will fail.
        let save_blocked = self.staged_save_blocked();
        vec![
            div()
                .flex()
                .items_center()
                .h_full()
                .gap(px(s(6.0)))
                .mr(px(s(8.0)))
                .child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.85)))
                        .text_color(Theme::text_dim())
                        .child(label),
                )
                .when(save_blocked, |d| {
                    d.child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.85)))
                            .text_color(Theme::warning())
                            .child(crate::localization::text(
                                "results.staged_changes.newer_bridge_required",
                            )),
                    )
                })
                .child(
                    Button::new("stage-undo")
                        .small()
                        .label(crate::localization::text(
                            "results.staged_changes.undo_button",
                        ))
                        .on_click(cx.listener(|this, _, _, cx| this.stage_undo(cx))),
                )
                .child(
                    Button::new("stage-redo")
                        .small()
                        .label(crate::localization::text(
                            "results.staged_changes.redo_button",
                        ))
                        .on_click(cx.listener(|this, _, _, cx| this.stage_redo(cx))),
                )
                .child(
                    Button::new("stage-preview")
                        .small()
                        .label(crate::localization::text(
                            "results.staged_changes.preview_sql_button",
                        ))
                        .on_click(cx.listener(|this, _, _, cx| this.preview_staged_sql(cx))),
                )
                .child(
                    Button::new("stage-copy-sql")
                        .small()
                        .label(crate::localization::text(
                            "results.staged_changes.copy_sql_button",
                        ))
                        .app_tooltip_text(crate::localization::text(
                            "results.staged_changes.copy_sql_tooltip",
                        ))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.copy_preview_sql(cx);
                        })),
                )
                .child({
                    let this = cx.entity();
                    Button::new("stage-transform")
                        .small()
                        .label(crate::localization::text(
                            "results.staged_changes.transform_button",
                        ))
                        .app_tooltip_text(crate::localization::text(
                            "results.staged_changes.transform_tooltip",
                        ))
                        .dropdown_menu(move |menu, _, _| {
                            let mut menu = menu;
                            for transform in crate::cell_transform::CellTransform::all() {
                                let transform = *transform;
                                let this = this.clone();
                                menu = menu.item(
                                    PopupMenuItem::new(transform.localized_label()).on_click(
                                        move |_ev, _window, app| {
                                            this.update(app, |this, cx| {
                                                this.transform_staged_values(transform, cx);
                                            });
                                        },
                                    ),
                                );
                            }
                            menu
                        })
                })
                .child(
                    Button::new("stage-save")
                        .small()
                        .label(crate::localization::text(
                            "results.staged_changes.save_button",
                        ))
                        .disabled(save_blocked)
                        .when(save_blocked, |button| {
                            button.app_tooltip_text(
                                crate::cell_actions::localized_relay_staged_save_reason(),
                            )
                        })
                        .on_click(cx.listener(|this, _, _, cx| this.save_staged(cx))),
                )
                .child(
                    Button::new("stage-discard")
                        .small()
                        .label(crate::localization::text(
                            "results.staged_changes.discard_button",
                        ))
                        .on_click(cx.listener(|this, _, _, cx| this.discard_staged(cx))),
                )
                .into_any_element(),
        ]
    }
}

/// Most layout notes shown under the canvas board before collapsing the
/// rest into a count, so a badly broken layout cannot crowd out the grids.
const MAX_CANVAS_LAYOUT_NOTES: usize = 3;

/// One line per runtime layout problem (a result set left with no open
/// cell), capped at `MAX_CANVAS_LAYOUT_NOTES`. Problems visible in the SQL
/// itself are squiggled in the editor by `directive_diagnostics` instead.
fn canvas_layout_notes(spec: &crate::result_layout::QueryResultLayoutSpec) -> Vec<String> {
    let runtime: Vec<&str> = spec
        .runtime_diagnostics()
        .map(|d| d.message.as_str())
        .collect();
    let mut notes: Vec<String> = runtime
        .iter()
        .take(MAX_CANVAS_LAYOUT_NOTES)
        .map(|message| (*message).to_string())
        .collect();
    let hidden = runtime.len().saturating_sub(MAX_CANVAS_LAYOUT_NOTES);
    if hidden > 0 {
        notes.push(crate::localization::format(
            "results.layout.hidden_problem_count",
            &[("hidden_count", &hidden.to_string())],
        ));
    }
    notes
}

/// Grid track count for one canvas axis. The parser caps boards at
/// `MAX_CANVAS_SIZE`, so the clamp only guards the `u16` conversion.
fn canvas_track_count(tracks: usize) -> u16 {
    u16::try_from(tracks.max(1)).unwrap_or(u16::MAX)
}

/// Pin a canvas cell to its explicit (0-based, inclusive) row and column
/// range on the board's grid lines, and let it shrink below its content so
/// wide result sets never push their track wider than its `1fr` share.
fn canvas_cell_area<E: Styled + IntoElement>(
    cell: E,
    (row_start, row_end): (usize, usize),
    (col_start, col_end): (usize, usize),
) -> gpui::AnyElement {
    let line = |index: usize| i16::try_from(index + 1).unwrap_or(i16::MAX);
    cell.row_start(line(row_start))
        .row_end(line(row_end + 1))
        .col_start(line(col_start))
        .col_end(line(col_end + 1))
        .min_w(px(0.0))
        .min_h(px(0.0))
        .overflow_hidden()
        .into_any_element()
}

pub(super) const MIN_CARD_HEIGHT: f32 = 160.0;
pub(super) const MAX_CARD_HEIGHT: f32 = 420.0;
pub(super) const CARD_HEADER_ALLOWANCE: f32 = 24.0;
pub(super) const CARD_FOOTER_ALLOWANCE: f32 = 31.2;
pub(super) const ESTIMATED_ROW_HEIGHT: f32 = 22.0;
pub(super) const CARD_GAP: f32 = 8.0;
/// The grid widget's Grid / Pivot / Chart view-tab strip above the column
/// headers.
pub(super) const VIEW_TAB_STRIP_ALLOWANCE: f32 = 34.0;
pub(super) const MIN_RESIZABLE_CARD_HEIGHT: f32 = CARD_HEADER_ALLOWANCE + CARD_FOOTER_ALLOWANCE;

pub(super) fn adaptive_card_total_height(row_count: usize) -> f32 {
    adaptive_card_grid_height(row_count) + CARD_HEADER_ALLOWANCE + CARD_FOOTER_ALLOWANCE
}

/// Total height of a stacked card allotted `lines` data rows by a
/// `-- sqlly lines:` directive: the card chrome, the grid's view-tab strip
/// and column-header row, and the rows themselves, at the grid's real row
/// and header heights (density, results font size, zoom) so exactly that
/// many rows show. Not clamped to the adaptive maximum — the directive is
/// the author asking for exactly this much.
pub(super) fn card_total_height_for_lines(
    lines: usize,
    metrics: crate::preferences::GridDensityMetrics,
) -> f32 {
    CARD_HEADER_ALLOWANCE
        + VIEW_TAB_STRIP_ALLOWANCE
        + metrics.header_height
        + lines.max(1) as f32 * metrics.row_height
        + CARD_FOOTER_ALLOWANCE
        + CARD_GAP * 2.0
}

/// A small glyph button bound to one result set: the stacked card header's
/// clear / filter controls act on their own card, not every grid.
pub(super) fn result_slot_button(
    id: &'static str,
    slot: usize,
    icon: &'static str,
    tooltip: impl Into<SharedString>,
    enabled: bool,
    entity: Entity<ResultsView>,
    action: fn(&mut ResultsView, usize, &mut Context<ResultsView>),
) -> gpui::AnyElement {
    let tooltip = tooltip.into();
    div()
        .id(ElementId::NamedInteger(id.into(), slot as u64))
        .debug_selector(move || format!("{id}-{slot}"))
        .flex()
        .items_center()
        .justify_center()
        .w(px(Theme::scaled(28.0)))
        .h(px(Theme::scaled(22.0)))
        .rounded(px(Theme::scaled(4.0)))
        .font(Theme::ui_font())
        .text_size(px(Theme::font_size_ratio(0.80)))
        .text_color(Theme::text_dim())
        .cursor(if enabled {
            CursorStyle::PointingHand
        } else {
            CursorStyle::Arrow
        })
        .when(!enabled, |button| button.opacity(0.4))
        .when(enabled, |button| {
            button
                .hover(|style| style.bg(Theme::row_hover()).text_color(Theme::text()))
                .on_mouse_down(MouseButton::Left, move |_event, _window, app| {
                    app.stop_propagation();
                    entity.update(app, |view, cx| action(view, slot, cx));
                })
        })
        .app_tooltip(move |_window, cx| crate::app::tooltip_view(tooltip.clone(), cx))
        .child(icon)
        .into_any_element()
}

pub(super) fn result_toolbar_chip(
    id: &'static str,
    text: SharedString,
    tooltip: impl Into<SharedString>,
    enabled: bool,
    entity: Entity<ResultsView>,
    action: fn(&mut ResultsView, &mut Context<ResultsView>),
) -> gpui::AnyElement {
    let tooltip = tooltip.into();
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .px(px(Theme::scaled(8.0)))
        .h(px(Theme::scaled(22.0)))
        .rounded(px(Theme::scaled(4.0)))
        .font(Theme::ui_font())
        .text_size(px(Theme::font_size_ratio(0.80)))
        .text_color(Theme::text_dim())
        .cursor(if enabled {
            CursorStyle::PointingHand
        } else {
            CursorStyle::Arrow
        })
        .when(!enabled, |button| button.opacity(0.4))
        .when(enabled, |button| {
            button
                .hover(|style| style.bg(Theme::row_hover()).text_color(Theme::text()))
                .on_mouse_down(MouseButton::Left, move |_event, _window, app| {
                    entity.update(app, action);
                })
        })
        .app_tooltip(move |_window, cx| crate::app::tooltip_view(tooltip.clone(), cx))
        .child(text)
        .into_any_element()
}

impl ResultsView {
    /// Grouping / filter / sort controls for one stacked card's header.
    fn render_stacked_card_controls(
        &self,
        slot: usize,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let (has_grouping, has_filters, has_sort) = self.slot_toolbar_state(slot, cx);
        let entity = cx.entity();
        div()
            .flex()
            .items_center()
            .h_full()
            .child(result_slot_button(
                "results-clear-grouping",
                slot,
                "☷×",
                crate::localization::text("results.stacked_cards.clear_grouping_tooltip"),
                has_grouping,
                entity.clone(),
                Self::clear_slot_grouping,
            ))
            .child(result_slot_button(
                "results-filter-builder",
                slot,
                "▽",
                crate::localization::text("results.stacked_cards.filter_rows_tooltip"),
                true,
                entity.clone(),
                Self::open_filter_builder,
            ))
            .child(result_slot_button(
                "results-clear-filter",
                slot,
                "▽×",
                crate::localization::text("results.stacked_cards.clear_filters_tooltip"),
                has_filters,
                entity.clone(),
                Self::clear_slot_filters,
            ))
            .child(result_slot_button(
                "results-clear-sort",
                slot,
                "⇅×",
                crate::localization::text("results.stacked_cards.clear_sort_tooltip"),
                has_sort,
                entity,
                Self::clear_slot_sort,
            ))
            .into_any_element()
    }

    fn multi_sort_chip(&self, entity: Entity<ResultsView>) -> Option<gpui::AnyElement> {
        let slot = self.active_tab;
        let spec = self.sort_specs.get(slot)?;
        if spec.keys.len() <= 1 {
            return None;
        }
        let names: Vec<String> = self
            .result_sets
            .get(slot)
            .map(|rs| rs.columns.iter().map(|c| c.name.clone()).collect())
            .unwrap_or_default();
        Some(result_toolbar_chip(
            "results-sort-spec",
            SharedString::from(spec.chip_text(&names)),
            crate::localization::text("results.toolbar.multi_sort_tooltip"),
            true,
            entity,
            Self::clear_grid_sort,
        ))
    }

    /// The decision-model mask suggestion: a chip naming how many unmasked
    /// columns look personal (click to mask them for this result), an
    /// "Always" chip that appends global masking rules for them, and a
    /// dismiss chip. Absent while no profile exists (no provider
    /// configured), after a dismissal, and once nothing is left to suggest.
    fn mask_suggestion_chips(&self, entity: Entity<ResultsView>) -> Vec<gpui::AnyElement> {
        let targets = self.mask_suggestion_columns(self.active_tab);
        if targets.is_empty() {
            return Vec::new();
        }
        let n = targets.len();
        let label = crate::localization::format(
            if n == 1 {
                "results.toolbar.mask_one_personal_column"
            } else {
                "results.toolbar.mask_many_personal_columns"
            },
            &[("column_count", &n.to_string())],
        );
        vec![
            result_toolbar_chip(
                "results-mask-suggest",
                SharedString::from(label),
                crate::localization::text("results.toolbar.mask_suggestion_tooltip"),
                true,
                entity.clone(),
                Self::accept_mask_suggestions,
            ),
            result_toolbar_chip(
                "results-mask-suggest-always",
                SharedString::from(crate::localization::text(
                    "results.toolbar.always_mask_button",
                )),
                crate::localization::text("results.toolbar.always_mask_tooltip"),
                true,
                entity.clone(),
                Self::always_mask_suggestions,
            ),
            result_toolbar_chip(
                "results-mask-suggest-dismiss",
                SharedString::from("×"),
                crate::localization::text("results.toolbar.dismiss_mask_suggestion_tooltip"),
                true,
                entity,
                Self::dismiss_mask_suggestion,
            ),
        ]
    }

    /// The decision-model chart caption: while the active slot's Chart tab
    /// is in front and its layout came from a suggestion, one ghost chip
    /// reading "Suggested chart · Revert". Clicking it drops the suggestion,
    /// pins "no suggestion" for the slot, and restores the chart's own
    /// auto-detection.
    fn chart_suggestion_chip(
        &self,
        entity: Entity<ResultsView>,
        cx: &gpui::App,
    ) -> Option<gpui::AnyElement> {
        let slot = self.active_tab;
        if !self.chart_suggestion_active(slot) {
            return None;
        }
        let chart_in_front = self
            .grid_views
            .get(slot)
            .and_then(|view| view.as_ref())
            .is_some_and(|view| view.read(cx).active_tab() == GridTab::Chart);
        if !chart_in_front {
            return None;
        }
        Some(result_toolbar_chip(
            "results-chart-suggested",
            SharedString::from(crate::localization::text(
                "results.toolbar.suggested_chart_revert",
            )),
            crate::localization::text("results.toolbar.suggested_chart_tooltip"),
            true,
            entity,
            Self::revert_chart_suggestion,
        ))
    }

    /// "Save chart to library…" — shown while the active slot's Chart tab is
    /// in front (the chart tab's own header belongs to the grid widget, so
    /// the library action lives beside the other result-toolbar chips).
    fn save_chart_chip(
        &self,
        entity: Entity<ResultsView>,
        cx: &gpui::App,
    ) -> Option<gpui::AnyElement> {
        let slot = self.active_tab;
        if !self.chart_tab_in_front(slot, cx) {
            return None;
        }
        let enabled = self.query_sql.is_some() && self.result_sets.get(slot).is_some();
        Some(result_toolbar_chip(
            "results-chart-save-library",
            SharedString::from(crate::localization::text(
                "results.toolbar.save_chart_library",
            )),
            crate::localization::text("results.toolbar.save_chart_library_tooltip"),
            enabled,
            entity,
            Self::request_save_active_chart,
        ))
    }

    fn masked_toolbar_chip(&self, entity: Entity<ResultsView>) -> Option<gpui::AnyElement> {
        if self.masked_columns(self.active_tab).is_empty() {
            return None;
        }
        // The app's own icon set (not an emoji) so the chip matches every
        // other toolbar glyph in weight and color.
        Some(
            div()
                .id("results-masked")
                .flex()
                .items_center()
                .justify_center()
                .w(px(Theme::scaled(28.0)))
                .h(px(Theme::scaled(22.0)))
                .rounded(px(Theme::scaled(4.0)))
                .text_color(Theme::text_dim())
                .cursor(CursorStyle::PointingHand)
                .hover(|style| style.bg(Theme::row_hover()).text_color(Theme::text()))
                .on_mouse_down(MouseButton::Left, move |_event, _window, app| {
                    entity.update(app, Self::reveal_all_active);
                })
                .app_tooltip(move |_window, cx| {
                    crate::app::tooltip_view(
                        crate::localization::text("results.toolbar.masked_columns_tooltip"),
                        cx,
                    )
                })
                .child(
                    Icon::empty()
                        .path(crate::icon_catalog::AppIcon::Security.path())
                        .size(px(Theme::scaled(14.0))),
                )
                .into_any_element(),
        )
    }
}

/// New height for the card above a dragged divider. Only that card resizes;
/// cards below keep their heights and the outer stacked scroll container
/// absorbs the change in total height.
pub(super) fn stacked_resize_height(resize: StackedResize, current_y: f32) -> f32 {
    let delta = current_y - resize.start_y;
    (resize.start_height + delta).max(MIN_RESIZABLE_CARD_HEIGHT)
}

/// Adaptive grid-content height for a stacked card, clamped to
/// [MIN_CARD_HEIGHT, MAX_CARD_HEIGHT] based on the result set's row count.
/// Mirrors `StackedResultGridsView.cardHeight(for:)` in the Swift app:
/// small results stay compact, large results cap out and scroll inside the
/// grid. Returns the height for the grid container (total card height minus
/// header and footer chrome).
pub(super) fn adaptive_card_grid_height(row_count: usize) -> f32 {
    let rows = row_count.max(1) as f32;
    let desired = CARD_HEADER_ALLOWANCE
        + rows * ESTIMATED_ROW_HEIGHT
        + CARD_FOOTER_ALLOWANCE
        + CARD_GAP * 2.0;
    let total = MAX_CARD_HEIGHT.min(MIN_CARD_HEIGHT.max(desired));
    total - CARD_HEADER_ALLOWANCE - CARD_FOOTER_ALLOWANCE
}

/// Live elapsed label for a running query: whole seconds under a minute
/// ("3s"), then minutes ("1:07"). Millisecond precision is noise on a ticker
/// that exists to reassure, not to measure.
pub(super) fn format_running_elapsed(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}:{:02}", secs / 60, secs % 60)
    }
}

/// Shared "working on it" placeholder for the results pane: a slowly
/// breathing loader glyph over a dim label. The repeating animation doubles
/// as the repaint driver that keeps `elapsed` ticking while a query runs.
pub(super) fn loading_placeholder(label: String, elapsed: Option<Duration>) -> gpui::AnyElement {
    let label = match elapsed {
        Some(elapsed) => crate::localization::format(
            "results.loading.with_elapsed_time",
            &[
                ("loading_message", &label),
                ("elapsed_time", &format_running_elapsed(elapsed)),
            ],
        ),
        None => label,
    };
    div()
        .flex()
        .flex_col()
        .size_full()
        .items_center()
        .justify_center()
        .gap(px(Theme::scaled(10.0)))
        .child(
            Icon::empty()
                .path(crate::icon_catalog::AppIcon::Loading.path())
                .size(px(Theme::scaled(18.0)))
                .text_color(Theme::text_dim())
                .with_animation(
                    ElementId::Name("results-loading-pulse".into()),
                    Animation::new(Duration::from_millis(1200)).repeat(),
                    |this, delta| {
                        // Sine phase keeps the loop seam invisible: opacity is
                        // equal at delta 0 and 1.
                        let phase = (delta * std::f32::consts::TAU).sin() * 0.5 + 0.5;
                        this.opacity(0.3 + 0.7 * phase)
                    },
                ),
        )
        .child(
            div()
                .text_size(px(Theme::font_size()))
                .text_color(Theme::text_dim())
                .child(label),
        )
        .into_any_element()
}

/// Format an elapsed time in milliseconds as `hh:MM:ss.SSS`.
pub(super) fn format_elapsed(ms: u64) -> String {
    let total_secs = ms / 1000;
    let millis = ms % 1000;
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;
    format!("{}:{:02}:{:02}.{:03}", hours, minutes, seconds, millis)
}

/// Format a count with thousands separators: `1234567` → `"1,234,567"`.
/// Row counts read as magnitudes; ungrouped digits make 1234567 and 123456
/// near-indistinguishable in a status strip glanced at, not read.
pub(crate) fn format_count(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Style one run-history chip so the run you are looking at is unmistakable.
///
/// Every chip used to be a ghost button that merely carried `selected`, which
/// on this strip amounted to a slightly different background — with six chips
/// in a row it was not possible to tell at a glance which run was on screen.
/// The active chip is filled; the rest stay quiet.
fn run_history_chip(button: Button, selected: bool) -> Button {
    let button = button.small();
    if selected {
        button.primary().selected(true)
    } else {
        button.ghost()
    }
}

pub(crate) fn format_run_clock(at: SystemTime) -> String {
    local_run_time(at).format("%H:%M:%S").to_string()
}

/// Date-carrying variant of [`format_run_clock`] for the Messages pane's
/// "Query started" / "Completed" stamps, where the run may be revisited days
/// later via the run-history strip and the bare clock time would be ambiguous.
pub(crate) fn format_run_stamp(at: SystemTime) -> String {
    local_run_time(at).format("%Y-%m-%d %H:%M:%S").to_string()
}

fn local_run_time(at: SystemTime) -> chrono::DateTime<chrono::Local> {
    // Go through the unix epoch rather than chrono's `From<std::time::SystemTime>`
    // so the wasm build's `web_time::SystemTime` (std's panics there) works too.
    let unix_ms = at
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    chrono::DateTime::from_timestamp_millis(unix_ms)
        .unwrap_or_default()
        .with_timezone(&chrono::Local)
}

fn retained_run_chip_tooltip(run: &RetainedRun) -> String {
    let mut tip = run.label.clone();
    let sql = run.sql.trim();
    if !sql.is_empty() {
        let truncated: String = sql.chars().take(80).collect();
        tip.push_str(" — ");
        tip.push_str(&truncated);
        if sql.chars().count() > 80 {
            tip.push('…');
        }
    }
    tip
}

/// Format a duration in milliseconds as a compact human-readable string:
/// `< 1s` shows `Nms`, `≥ 1s` shows `N.NNs`.
pub(crate) fn format_duration_ms(ms: u64) -> String {
    if ms < 1000 {
        format!("{}ms", ms)
    } else {
        format!("{:.2}s", ms as f64 / 1000.0)
    }
}
