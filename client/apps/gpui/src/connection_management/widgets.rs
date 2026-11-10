use super::*;

impl ConnectionManagement {
    pub(super) fn handle_split_resize_start(
        &mut self,
        ev: &MouseDownEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.is_split_resizing = true;
        self.split_resize_start_pos = ev.position;
        self.split_resize_start_width = self.split_left_width;
        cx.stop_propagation();
        cx.notify();
    }

    pub(super) fn handle_mouse_move(
        &mut self,
        ev: &MouseMoveEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.is_split_resizing {
            let dx = f32::from(ev.position.x) - f32::from(self.split_resize_start_pos.x);
            self.split_left_width =
                compute_split_width(self.split_resize_start_width, dx, Theme::SCALE);
            cx.notify();
        }
    }

    pub(super) fn handle_mouse_up(
        &mut self,
        _: &MouseUpEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.is_split_resizing {
            self.is_split_resizing = false;
            cx.notify();
        }
    }

    pub(super) fn render_segmented(
        id_prefix: &'static str,
        options: &[(&str, ConnectionEnvironment)],
        current: ConnectionEnvironment,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let s = Theme::scaled;
        div()
            .flex()
            .flex_row()
            .gap(px(s(4.0)))
            .children(options.iter().enumerate().map(|(i, (label, val))| {
                let selected = *val == current;
                let val = *val;
                div()
                    .id((id_prefix, i))
                    .px(px(s(8.0)))
                    .h(px(s(24.0)))
                    .flex()
                    .items_center()
                    .cursor(gpui::CursorStyle::PointingHand)
                    .border_1()
                    .border_color(Theme::border())
                    .rounded(px(s(3.0)))
                    .text_size(px(Theme::font_size()))
                    .bg(if selected {
                        Theme::accent()
                    } else {
                        Theme::bg()
                    })
                    .text_color(if selected {
                        Theme::bg()
                    } else {
                        Theme::text_dim()
                    })
                    .hover(|st| st.bg(Theme::border()))
                    .child(label.to_string())
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| this.set_environment(val, cx)),
                    )
            }))
            .into_any_element()
    }

    pub(super) fn render_auth_segmented(
        cx: &mut Context<Self>,
        current: AuthMode,
        engine: DatabaseType,
    ) -> AnyElement {
        let s = Theme::scaled;
        div()
            .flex()
            .flex_row()
            .gap(px(s(4.0)))
            .children(AuthMode::ALL.iter().enumerate().filter_map(|(i, mode)| {
                if !mode.supported_for(engine) {
                    return None;
                }
                let selected = *mode == current;
                let mode = *mode;
                Some(
                    div()
                        .id(("auth", i))
                        .px(px(s(8.0)))
                        .h(px(s(24.0)))
                        .flex()
                        .items_center()
                        .cursor(gpui::CursorStyle::PointingHand)
                        .border_1()
                        .border_color(Theme::border())
                        .rounded(px(s(3.0)))
                        .text_size(px(Theme::font_size()))
                        .bg(if selected {
                            Theme::accent()
                        } else {
                            Theme::bg()
                        })
                        .text_color(if selected {
                            Theme::bg()
                        } else {
                            Theme::text_dim()
                        })
                        .hover(|st| st.bg(Theme::border()))
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(s(3.0)))
                                .when(mode == AuthMode::EntraBrowser, |d| {
                                    d.children(
                                        crate::icons::entra_id_icon()
                                            .map(|icon| img(icon).w(px(s(14.0))).h(px(s(14.0)))),
                                    )
                                })
                                .child(mode.label().to_string()),
                        )
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, window, cx| {
                                this.set_auth_mode(mode, window, cx)
                            }),
                        ),
                )
            }))
            .into_any_element()
    }

    pub(super) fn render_checkbox(
        id: &'static str,
        label: &str,
        checked: bool,
        disabled: bool,
        cx: &mut Context<Self>,
        handler: fn(&mut Self, bool, &mut Window, &mut Context<Self>),
    ) -> AnyElement {
        let this = cx.entity();
        let mut checkbox = Checkbox::new(id).checked(checked).tab_stop(true);
        if !label.is_empty() {
            checkbox = checkbox.label(label.to_string());
        }
        if disabled {
            checkbox = checkbox.disabled(true);
        } else {
            checkbox = checkbox.on_click(move |new_checked: &bool, window, app| {
                let new_checked = *new_checked;
                this.update(app, |this, cx| handler(this, new_checked, window, cx));
            });
        }
        checkbox.into_any_element()
    }

    pub(super) fn button(
        label: String,
        enabled: bool,
        cx: &mut Context<Self>,
        handler: impl Fn(&mut Self, &MouseDownEvent, &mut Window, &mut Context<Self>) + 'static,
    ) -> AnyElement {
        let s = Theme::scaled;
        div()
            .id(label.clone())
            .px(px(s(12.0)))
            .h(px(s(28.0)))
            .flex()
            .items_center()
            .cursor(if enabled {
                gpui::CursorStyle::PointingHand
            } else {
                gpui::CursorStyle::Arrow
            })
            .border_1()
            .border_color(Theme::border())
            .rounded(px(s(3.0)))
            .text_size(px(Theme::font_size()))
            .text_color(if enabled {
                Theme::ink_on(Theme::accent())
            } else {
                Theme::text_disabled()
            })
            .when(enabled, |st| {
                st.bg(Theme::accent()).hover(|h| h.opacity(0.9))
            })
            .when(!enabled, |st| st.opacity(0.5))
            .child(label.to_string())
            .when(enabled, |st| {
                st.on_mouse_down(MouseButton::Left, cx.listener(handler))
            })
            .into_any_element()
    }

    pub(super) fn vertical_scrollbar(id: &'static str, handle: &ScrollHandle) -> AnyElement {
        let s = Theme::scaled;
        let viewport = handle.bounds().size.height;
        let max_offset = handle.max_offset().y;
        let (scrollable, thumb_top, thumb_height) =
            scrollbar_metrics(viewport.into(), max_offset.into(), handle.offset().y.into());
        let track = div()
            .id(id)
            .w(px(s(10.0)))
            .h_full()
            .flex_none()
            .relative()
            .bg(Theme::panel_bg())
            .border_l_1()
            .border_color(Theme::border())
            .child(
                div()
                    .absolute()
                    .top(relative(thumb_top))
                    .left(px(s(2.0)))
                    .right(px(s(2.0)))
                    .h(relative(thumb_height))
                    .rounded(px(s(3.0)))
                    .bg(Theme::text_dim().opacity(if scrollable { 0.5 } else { 0.15 })),
            );
        if !scrollable {
            return track.into_any_element();
        }
        track
            .cursor(gpui::CursorStyle::PointingHand)
            // Press jumps there; holding drags (tracked by the dialog root's
            // `scrollbar_drag_area`).
            .scrollbar_track(handle, gpui::Axis::Vertical)
            .into_any_element()
    }

    pub(super) fn horizontal_scrollbar(id: &'static str, handle: &ScrollHandle) -> AnyElement {
        let s = Theme::scaled;
        let viewport = handle.bounds().size.width;
        let max_offset = handle.max_offset().x;
        let (scrollable, thumb_left, thumb_width) =
            scrollbar_metrics(viewport.into(), max_offset.into(), handle.offset().x.into());
        let track = div()
            .id(id)
            .w_full()
            .h(px(s(10.0)))
            .flex_none()
            .relative()
            .bg(Theme::panel_bg())
            .border_t_1()
            .border_color(Theme::border())
            .child(
                div()
                    .absolute()
                    .left(relative(thumb_left))
                    .top(px(s(2.0)))
                    .bottom(px(s(2.0)))
                    .w(relative(thumb_width))
                    .rounded(px(s(3.0)))
                    .bg(Theme::text_dim().opacity(if scrollable { 0.5 } else { 0.15 })),
            );
        if !scrollable {
            return track.into_any_element();
        }
        track
            .cursor(gpui::CursorStyle::PointingHand)
            // Press jumps there; holding drags (tracked by the dialog root's
            // `scrollbar_drag_area`).
            .scrollbar_track(handle, gpui::Axis::Horizontal)
            .into_any_element()
    }

    /// The System Settings-style navigation sidebar (five sections).
    pub(super) fn render_nav(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let s = Theme::scaled;
        let active = self.pane;
        div()
            .w(px(s(185.0)))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(s(2.0)))
            .p(px(s(8.0)))
            .bg(Theme::sidebar_bg())
            .border_r_1()
            .border_color(Theme::border())
            .children(WorkspacePane::ALL.iter().enumerate().map(|(i, pane)| {
                let pane = *pane;
                let selected = active == pane;
                div()
                    .id(("cw-nav", i))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(8.0)))
                    .h(px(s(30.0)))
                    .px(px(s(8.0)))
                    .rounded(px(s(6.0)))
                    .cursor(gpui::CursorStyle::PointingHand)
                    .when(selected, |d| d.bg(Theme::accent()))
                    .when(!selected, |d| d.hover(|st| st.bg(Theme::border())))
                    .child(
                        div()
                            .w(px(s(20.0)))
                            .h(px(s(20.0)))
                            .flex_none()
                            .rounded(px(s(5.0)))
                            .bg(pane.tint())
                            .flex()
                            .items_center()
                            .justify_center()
                            // Fixed glyph: drawn to fit its 20×20 tile.
                            .text_size(px(s(11.0)))
                            .text_color(Theme::ink_on(pane.tint()))
                            .child(pane.glyph()),
                    )
                    .child(
                        div()
                            .text_size(px(Theme::font_size()))
                            .text_color(if selected { Theme::bg() } else { Theme::text() })
                            .child(pane.label()),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            this.set_pane(pane, window, cx);
                        }),
                    )
            }))
            .into_any_element()
    }

    /// Wraps an input so it sits right-aligned in a card row.
    pub(super) fn field_control(field: impl IntoElement) -> AnyElement {
        div()
            .w(px(Theme::scaled(240.0)))
            .child(field)
            .into_any_element()
    }

    /// Section title + inset card (grouped rows with hairline separators).
    pub(super) fn group(label: &str, rows: Vec<AnyElement>) -> AnyElement {
        let s = Theme::scaled;
        div()
            .flex()
            .flex_col()
            .gap(px(s(6.0)))
            .child(
                div()
                    .pl(px(s(4.0)))
                    .text_size(px(Theme::font_size_ratio(0.92)))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(Theme::text_dim())
                    .child(label.to_string()),
            )
            .child(Self::card(rows))
            .into_any_element()
    }

    /// The inset rounded group box holding rows separated by hairlines.
    fn card(rows: Vec<AnyElement>) -> AnyElement {
        let s = Theme::scaled;
        let count = rows.len();
        div()
            .flex()
            .flex_col()
            .bg(Theme::bg())
            .border_1()
            .border_color(Theme::border())
            .rounded(px(s(8.0)))
            .overflow_hidden()
            .children(rows.into_iter().enumerate().map(move |(i, row)| {
                div()
                    .when(i + 1 < count, |d| {
                        d.border_b_1().border_color(Theme::border().opacity(0.6))
                    })
                    .child(row)
            }))
            .into_any_element()
    }

    /// A TLS certificate/key path row: label above the path input, with a
    /// Browse… button on native builds.
    pub(super) fn tls_path_row(
        &self,
        label: &str,
        field: TlsPathField,
        browse_id: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let input_entity = match field {
            TlsPathField::Ca => &self.tls_ca_input,
            TlsPathField::ClientCert => &self.tls_cert_input,
            TlsPathField::ClientKey => &self.tls_key_input,
        };
        let input = Input::new(input_entity).font_family(Theme::editor_font_family());
        #[cfg(not(target_arch = "wasm32"))]
        let control = {
            let s = Theme::scaled;
            let browse_entity = cx.entity();
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(s(8.0)))
                .child(div().flex_1().child(input))
                .child(
                    Button::new(browse_id)
                        .ghost()
                        .small()
                        .label(crate::localization::text(
                            "connections.management.tls_browse_button",
                        ))
                        .on_click(move |_, window, app| {
                            browse_entity.update(app, |this, cx| {
                                this.handle_browse_tls_file(field, window, cx);
                            });
                        }),
                )
                .into_any_element()
        };
        #[cfg(target_arch = "wasm32")]
        let control = {
            let _ = (browse_id, cx);
            input.into_any_element()
        };
        Self::card_row_stacked(label, control)
    }

    /// Label left, control right — the standard System Settings row.
    pub(super) fn card_row(label: &str, control: AnyElement) -> AnyElement {
        let s = Theme::scaled;
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap(px(s(12.0)))
            .min_h(px(s(38.0)))
            .px(px(s(12.0)))
            .py(px(s(4.0)))
            .child(
                div()
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text())
                    .child(label.to_string()),
            )
            .child(control)
            .into_any_element()
    }

    /// Label above a full-width control, for wide controls like segmented
    /// pickers.
    pub(super) fn card_row_stacked(label: &str, control: impl IntoElement) -> AnyElement {
        let s = Theme::scaled;
        div()
            .flex()
            .flex_col()
            .gap(px(s(6.0)))
            .px(px(s(12.0)))
            .py(px(s(8.0)))
            .child(
                div()
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text())
                    .child(label.to_string()),
            )
            .child(control)
            .into_any_element()
    }

    /// Small square +/− button for list footers (Swift's master-detail
    /// add/remove controls).
    pub(super) fn list_footer_button(
        id: &'static str,
        glyph: &'static str,
        enabled: bool,
        cx: &mut Context<Self>,
        handler: impl Fn(&mut Self, &MouseDownEvent, &mut Window, &mut Context<Self>) + 'static,
    ) -> AnyElement {
        let s = Theme::scaled;
        div()
            .id(id)
            .w(px(s(24.0)))
            .h(px(s(22.0)))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(s(4.0)))
            .border_1()
            .border_color(Theme::border())
            // Fixed glyph: drawn to fit the 24×22 button.
            .text_size(px(s(13.0)))
            .text_color(if enabled {
                Theme::text()
            } else {
                Theme::text_dim()
            })
            .cursor(if enabled {
                gpui::CursorStyle::PointingHand
            } else {
                gpui::CursorStyle::Arrow
            })
            .when(enabled, |d| {
                d.hover(|st| st.bg(Theme::border()))
                    .on_mouse_down(MouseButton::Left, cx.listener(handler))
            })
            .when(!enabled, |d| d.opacity(0.5))
            .child(glyph)
            .into_any_element()
    }
}

pub(super) fn scrollbar_metrics(viewport: f32, max_offset: f32, offset: f32) -> (bool, f32, f32) {
    let scrollable = max_offset > 1.0 && viewport > 0.0;
    if !scrollable {
        return (false, 0.0, 1.0);
    }
    let thumb_size = (viewport / (viewport + max_offset)).clamp(0.08, 1.0);
    let progress = (-offset / max_offset).clamp(0.0, 1.0);
    (true, progress * (1.0 - thumb_size), thumb_size)
}

/// Compute the left-pane width from a split-drag delta. Pure function so
/// the clamping logic is unit-testable without a GPUI context.
pub(super) fn compute_split_width(start_width: f32, dx_px: f32, scale: f32) -> f32 {
    const MIN_W: f32 = 150.0;
    const MAX_W: f32 = 600.0;
    (start_width + dx_px / scale).clamp(MIN_W, MAX_W)
}
