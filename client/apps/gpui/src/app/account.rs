use super::*;
use crate::tooltip::AppTooltip;

impl AppView {
    pub(super) fn handle_open_account(
        &mut self,
        _action: &OpenAccount,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if crate::distribution::is_standalone() {
            self.notify_feature_disabled(
                crate::localization::text("account.disabled.standalone_build"),
                cx,
            );
            return;
        }
        self.on_account_button_clicked(cx);
    }

    /// Tell the user, unmissably, that a feature is switched off in this
    /// build: a warning in the status bar plus a toast.
    pub(super) fn notify_feature_disabled(&mut self, message: String, cx: &mut Context<Self>) {
        self.set_status(StatusLevel::Warning, message.clone());
        Self::toast_update_outcome(
            gpui_component::notification::Notification::warning(message),
            cx,
        );
        cx.notify();
    }

    /// The status line, colored by severity.
    ///
    /// `Info` keeps the muted ink it always had — most messages are chatter and
    /// shouldn't compete with the data. Everything else earns a semantic color,
    /// so a failure is no longer typographically identical to "Schema metadata
    /// refreshed". `Busy` gets a static accent dot rather than a spinner:
    /// the design system bans motion, and a dot answers "is it still working?"
    /// just as well while costing nothing per frame.
    /// The status line. Failures are clickable: one click copies the message
    /// and opens it in full.
    ///
    /// A database error is the one message a user actually needs to keep — to
    /// paste into a ticket, a search, or a message to whoever runs the server —
    /// and the status strip shows it on one ellipsized line and cannot be
    /// selected with the mouse. Clicking puts the whole text, however long, on
    /// the clipboard and opens it in a readable dialog.
    pub(super) fn render_status_message(
        level: StatusLevel,
        text: String,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let kind = match level {
            StatusLevel::Info => crate::status_bar::StatusMessageKind::Info,
            StatusLevel::Busy => crate::status_bar::StatusMessageKind::Busy,
            StatusLevel::Success => crate::status_bar::StatusMessageKind::Success,
            StatusLevel::Warning => crate::status_bar::StatusMessageKind::Warning,
            StatusLevel::Error => crate::status_bar::StatusMessageKind::Error,
        };
        let color = match crate::status_bar::status_message_ink(kind) {
            crate::status_bar::StatusInk::Error => Theme::error(),
            crate::status_bar::StatusInk::Dim => Theme::text_dim(),
        };
        let copyable = crate::status_bar::is_copyable_status(kind) && !text.is_empty();
        let to_copy = text.clone();
        let icon = crate::status_bar::status_message_icon(kind);
        div()
            .id("status-message")
            .flex()
            .flex_row()
            .items_center()
            .min_w_0()
            .gap(px(Theme::scaled(4.0)))
            .when(!text.is_empty(), |d| {
                d.child(
                    Icon::empty()
                        .path(icon.path())
                        .size(px(Theme::toolbar_icon_size()))
                        .text_color(color),
                )
            })
            // A long message (a database error can run to paragraphs, and
            // SQL Server's arrive with embedded newlines) must stay on one
            // status line: flatten to a single line, then truncate with an
            // ellipsis instead of wrapping and pushing the bar taller. The
            // full text is still reachable — clicking a copyable message
            // copies it and opens it in a dialog.
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_color(color)
                    .child(crate::status_bar::single_line(&text)),
            )
            .when(copyable, |d| {
                d.cursor_pointer()
                    .app_tooltip(|_window, cx| {
                        tooltip_view(
                            crate::localization::text("status_bar.message.copy_open_tooltip"),
                            cx,
                        )
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _ev, _window, cx| {
                            cx.write_to_clipboard(gpui::ClipboardItem::new_string(to_copy.clone()));
                            this.status_detail = Some((level, to_copy.clone()));
                            cx.notify();
                        }),
                    )
            })
            // A new message fades in rather than popping. Keyed by level +
            // text so identical repeat messages don't replay.
            .with_animation(
                ElementId::Name(format!("status:{level:?}:{text}").into()),
                Animation::new(Duration::from_millis(180))
                    .with_easing(cubic_bezier(0.16, 1.0, 0.3, 1.0)),
                |this, delta| this.opacity(delta),
            )
    }

    /// Free-tier note in the status bar: dim text, accent on hover, opens the
    /// pricing page (or account manage URL for a licensed supporter).
    /// Relocated to the left cluster via `status_bar::render_tagline`; kept for
    /// the nag-dialog sibling until that surface is rewritten.
    #[allow(dead_code)]
    pub(super) fn render_free_tier_status_chip(cx: &mut Context<Self>) -> Option<impl IntoElement> {
        crate::license::should_show_supporter_message_now().then(|| {
            let presentation = crate::status_bar::free_tier_presentation();
            div()
                .id("free-tier-status")
                .cursor_pointer()
                .text_color(Theme::text_dim())
                .hover(|s| {
                    if presentation.link_is_accent {
                        s.text_color(Theme::accent())
                    } else {
                        s.text_color(Theme::text())
                    }
                })
                .child(presentation.text)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _ev, _window, cx| {
                        // A licensed supporter has nothing left to buy: send them
                        // to manage their account instead of the pricing page.
                        this.open_upgrade_surface(cx);
                    }),
                )
        })
    }

    /// Dismiss the free-tier pay dialog ("Remind me tomorrow").
    pub(super) fn dismiss_nag_dialog(&mut self, cx: &mut Context<Self>) {
        self.show_nag_dialog = false;
        cx.notify();
    }

    /// "Okay, fine — take my money": open the pricing page (or, for a licensed
    /// user, their account) and dismiss.
    pub(super) fn accept_nag_dialog(&mut self, cx: &mut Context<Self>) {
        self.open_upgrade_surface(cx);
        self.show_nag_dialog = false;
        cx.notify();
    }

    /// The free-tier pay dialog: the unified dialog chrome carrying the
    /// status-bar/banner messaging with a heavier ask and two choices.
    pub(super) fn render_nag_dialog(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let s = Theme::scaled;
        let heading = crate::license::nag_dialog_heading_now();
        let query_count_line =
            crate::license::nag_dialog_query_count_line(crate::license::load_query_count());
        let body = div()
            .flex()
            .flex_row()
            .gap(px(s(14.0)))
            .child(div().text_size(px(s(30.0))).flex_shrink_0().child("🐷"))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .gap(px(s(8.0)))
                    .child(
                        div()
                            .text_size(px(Theme::font_size()))
                            .text_color(Theme::text_dim())
                            .child(crate::license::nag_dialog_body()),
                    )
                    .child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.9)))
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_color(Theme::text())
                            .child(query_count_line),
                    )
                    .child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.82)))
                            .text_color(Theme::text_dim())
                            .child(crate::license::nag_dialog_removal_note()),
                    ),
            );
        crate::dialog::Dialog::new("nag-dialog", |this: &mut Self| {
            this.dialog_frames.get("nag-dialog", || {
                crate::dialog::DialogFrame::new_auto(Theme::scaled(500.0))
            })
        })
        .title(heading)
        .on_close(|this, _window, cx| this.dismiss_nag_dialog(cx))
        // The two labeled choices replace the stock Close button; the scrim
        // still dismisses (a scrim click and "remind me later" are the same
        // act), so re-enable what dropping the default Close turned off.
        .use_default_close(false)
        .dismiss_on_scrim_click(true)
        .child(body)
        .button(
            Button::new("nag-dialog-later")
                .label(crate::license::nag_dialog_later_label())
                .on_click(cx.listener(|this, _, _, cx| this.dismiss_nag_dialog(cx))),
        )
        .button(
            Button::new("nag-dialog-pay")
                .primary()
                .label(crate::license::nag_dialog_pay_label())
                .on_click(cx.listener(|this, _, _, cx| this.accept_nag_dialog(cx))),
        )
        .build(self, window, cx)
    }

    /// The "update available" status-bar chip. Shown once the startup check
    /// finds a newer build; clicking it opens the changes dialog.
    #[allow(dead_code)]
    pub(super) fn render_update_available_indicator(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement + use<>> {
        let latest = self.update_available.as_ref()?.latest.clone();
        let fill = Theme::accent();
        Some(
            div()
                .id("update-available-chip")
                .cursor_pointer()
                .text_color(Theme::text_dim())
                .hover(|s| s.text_color(fill))
                .child(crate::status_bar::update_chip_label(&latest))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _ev, _window, cx| {
                        this.open_update_dialog(cx);
                    }),
                ),
        )
    }

    /// Open the changes dialog, re-checking the update endpoint first when
    /// the last completed check is more than 30 minutes old (each qualifying
    /// open slides that window forward via the completed check's timestamp).
    pub(super) fn open_update_dialog(&mut self, cx: &mut Context<Self>) {
        const DIALOG_FRESHNESS: Duration = Duration::from_secs(30 * 60);
        let stale = self
            .update_checked_at
            .is_none_or(|checked| checked.elapsed() >= DIALOG_FRESHNESS);
        if stale {
            self.check_for_update(false, cx);
        }
        self.show_update_dialog = true;
        cx.notify();
    }

    /// The status-bar bell: unread count badge, click to open a bounded
    /// dropdown of recent notifications (long query completions, failures,
    /// updates). Opening marks everything read.
    pub(super) fn render_task_chip(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        use crate::background_tasks::{TaskKind, TaskState};
        use gpui_component::button::{Button, ButtonVariants};

        let s = Theme::scaled;
        let running = self.background_tasks.running_count();
        if running == 0 && !self.tasks_open {
            return div().into_any_element();
        }
        let chip_label = crate::status_bar::task_chip_label(running)
            .unwrap_or_else(|| crate::localization::text("background_tasks.chip_fallback"));
        let tip: SharedString = crate::localization::text("background_tasks.tooltip").into();
        let task_rows: Vec<_> = self
            .background_tasks
            .tasks()
            .iter()
            .map(|task| {
                let state_text = match &task.state {
                    TaskState::Running => {
                        crate::localization::text("background_tasks.state.running")
                    }
                    TaskState::Finished => {
                        crate::localization::text("background_tasks.state.finished")
                    }
                    TaskState::Failed(message) => message.clone(),
                    TaskState::Cancelled => {
                        crate::localization::text("background_tasks.state.cancelled")
                    }
                };
                (
                    task.id,
                    match task.kind {
                        TaskKind::Export => "E",
                        TaskKind::Dump => "D",
                        TaskKind::Transfer => "T",
                    },
                    task.label.clone(),
                    crate::background_tasks::progress_label(&task.progress),
                    state_text,
                    task.state == TaskState::Running,
                    task.state == TaskState::Finished,
                    task.output_path.clone(),
                )
            })
            .collect();
        let mut chip = div()
            .id("task-chip")
            .relative()
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .text_color(if running > 0 {
                Theme::text()
            } else {
                Theme::text_dim()
            })
            .hover(|style| style.text_color(Theme::text()))
            .app_tooltip(move |_window, cx| tooltip_view(tip.clone(), cx))
            .child(
                Icon::empty()
                    .path(AppIcon::Loading.path())
                    .size(px(Theme::toolbar_icon_size())),
            )
            .child(chip_label)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _ev, _window, cx| {
                    this.tasks_open = !this.tasks_open;
                    cx.notify();
                }),
            );
        if self.tasks_open {
            chip = chip.child(gpui::deferred(
                gpui::anchored().anchor(gpui::Anchor::BottomRight).child(
                    div()
                        .occlude()
                        .w(px(s(420.0)))
                        .max_h(px(s(340.0)))
                        .mb(px(s(22.0)))
                        .mr(px(s(44.0)))
                        .bg(Theme::popup_bg())
                        .border_1()
                        .border_color(Theme::popup_border())
                        .rounded(px(s(6.0)))
                        .flex()
                        .flex_col()
                        .on_mouse_down_out(cx.listener(|this, _ev, _window, cx| {
                            this.tasks_open = false;
                            cx.notify();
                        }))
                        .child(
                            div()
                                .px(px(s(10.0)))
                                .py(px(s(6.0)))
                                .border_b_1()
                                .border_color(Theme::border())
                                .text_color(Theme::text())
                                .child(crate::localization::text("background_tasks.popover.title")),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_h(px(0.0))
                                .overflow_hidden()
                                .flex()
                                .flex_col()
                                .when(task_rows.is_empty(), |list| {
                                    list.child(
                                        div().p(px(s(10.0))).text_color(Theme::text_dim()).child(
                                            crate::localization::text(
                                                "background_tasks.popover.empty",
                                            ),
                                        ),
                                    )
                                })
                                .children(task_rows.into_iter().map(
                                    |(
                                        id,
                                        glyph,
                                        label,
                                        progress_text,
                                        state_text,
                                        running,
                                        finished,
                                        output_path,
                                    )| {
                                        div()
                                            .px(px(s(10.0)))
                                            .py(px(s(5.0)))
                                            .border_b_1()
                                            .border_color(Theme::border())
                                            .flex()
                                            .flex_col()
                                            .gap(px(s(2.0)))
                                            .child(
                                                div()
                                                    .flex()
                                                    .flex_row()
                                                    .items_center()
                                                    .gap(px(s(6.0)))
                                                    .child(
                                                        div()
                                                            .text_color(Theme::text_dim())
                                                            .child(glyph),
                                                    )
                                                    .child(
                                                        div()
                                                            .text_color(Theme::text())
                                                            .child(label),
                                                    ),
                                            )
                                            .child(
                                                div()
                                                    .text_color(Theme::text_dim())
                                                    .text_size(px(Theme::font_size_ratio(0.78)))
                                                    .child(progress_text),
                                            )
                                            .child(
                                                div()
                                                    .text_color(Theme::text_dim())
                                                    .text_size(px(Theme::font_size_ratio(0.78)))
                                                    .child(state_text),
                                            )
                                            .children(running.then(|| {
                                                let cancel_id = id;
                                                Button::new(("cancel-task", cancel_id.0))
                                                    .ghost()
                                                    .label(crate::localization::text(
                                                        "background_tasks.actions.cancel",
                                                    ))
                                                    .on_click(cx.listener(move |this, _, _, cx| {
                                                        this.background_tasks.cancel(cancel_id);
                                                        cx.notify();
                                                    }))
                                            }))
                                            .children(output_path.and_then(|path| {
                                                finished.then(|| {
                                                    Button::new(("reveal-task", id.0))
                                                        .ghost()
                                                        .label(crate::localization::text(
                                                            "background_tasks.actions.reveal",
                                                        ))
                                                        .on_click(cx.listener(move |_, _, _, _| {
                                                            crate::reveal_path::reveal_file(&path);
                                                        }))
                                                })
                                            }))
                                    },
                                )),
                        )
                        .child(
                            div()
                                .px(px(s(10.0)))
                                .py(px(s(6.0)))
                                .border_t_1()
                                .border_color(Theme::border())
                                .child(
                                    div()
                                        .id("tasks-clear-finished")
                                        .cursor_pointer()
                                        .text_color(Theme::text_dim())
                                        .hover(|style| style.text_color(Theme::text()))
                                        .child(crate::localization::text(
                                            "background_tasks.actions.clear_finished",
                                        ))
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(|this, _ev, _window, cx| {
                                                this.background_tasks.clear_finished();
                                                cx.notify();
                                            }),
                                        ),
                                ),
                        ),
                ),
            ));
        }
        chip.into_any_element()
    }

    pub(super) fn render_notification_bell(
        &self,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let s = Theme::scaled;
        let unread = self.notifications.unread_count();
        let tip: SharedString = crate::localization::text("notifications.tooltip").into();
        div()
            .id("notification-bell")
            .relative()
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .text_color(if unread > 0 {
                Theme::text()
            } else {
                Theme::text_dim()
            })
            .hover(|style| style.text_color(Theme::text()))
            .app_tooltip(move |_window, cx| tooltip_view(tip.clone(), cx))
            .child(
                Icon::empty()
                    .path(AppIcon::Notification.path())
                    .size(px(Theme::toolbar_icon_size())),
            )
            .children((unread > 0).then(|| {
                div()
                    .absolute()
                    .top(px(s(-4.0)))
                    .right(px(s(-6.0)))
                    .min_w(px(s(12.0)))
                    .px(px(s(3.0)))
                    .h(px(s(12.0)))
                    .rounded_full()
                    .bg(Theme::error())
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(Theme::font_size_ratio(0.62)))
                    .text_color(Theme::ink_on(Theme::error()))
                    .child(format!("{unread}"))
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _ev, _window, cx| {
                    this.notifications_open = !this.notifications_open;
                    if this.notifications_open {
                        this.notifications.mark_all_read();
                    }
                    cx.notify();
                }),
            )
    }

    /// The notifications list, rendered with the shared [`Dialog`] chrome so
    /// it matches every other modal. `None` while closed.
    /// The clicked status message, in full.
    ///
    /// The strip gives a message one ellipsized line; a database error is
    /// routinely longer than that and is the one message worth reading whole.
    /// Clicking has already put it on the clipboard — this is the readable
    /// half of the same gesture, so the text wraps and scrolls here rather
    /// than truncating.
    pub(super) fn render_status_detail_dialog(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Div> {
        let (level, text) = self.status_detail.clone()?;
        let s = Theme::scaled;
        let color = match level {
            StatusLevel::Error => Theme::error(),
            StatusLevel::Warning => Theme::warning(),
            _ => Theme::text(),
        };
        let title = match level {
            StatusLevel::Error => crate::localization::text("status_detail.error_title"),
            StatusLevel::Warning => crate::localization::text("status_detail.warning_title"),
            _ => crate::localization::text("status_detail.message_title"),
        };
        let to_copy = text.clone();
        let body = div()
            .id("status-detail-body")
            .max_h(px(s(320.0)))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(s(6.0)))
            // The engine's own line breaks are meaningful here — SQL Server
            // puts the `Msg …, Level …` header on its own line — so keep them
            // instead of flattening as the strip does.
            .children(text.lines().map(|line| {
                div()
                    .whitespace_normal()
                    .text_color(color)
                    .child(line.to_string())
            }));
        Some(
            Dialog::new("status-detail-dialog", |this: &mut Self| {
                this.dialog_frames.get("status-detail-dialog", || {
                    DialogFrame::new_auto(Theme::scaled(520.0))
                })
            })
            .title(title)
            .dismiss_on_scrim_click(true)
            .on_close(|this, _window, cx| {
                this.status_detail = None;
                cx.notify();
            })
            .child(body)
            .button(
                Button::new("status-detail-copy")
                    .label(crate::localization::text("status_detail.copy_button"))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(to_copy.clone()));
                        this.set_status(
                            StatusLevel::Success,
                            crate::localization::text("status_detail.copied_status"),
                        );
                        cx.notify();
                    })),
            )
            .build(self, window, cx),
        )
    }

    pub(super) fn render_notifications_dialog(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Div> {
        if !self.notifications_open {
            return None;
        }
        let s = Theme::scaled;
        let items: Vec<_> = self
            .notifications
            .items()
            .iter()
            .take(30)
            .map(|item| {
                (
                    item.kind.label(),
                    item.message.clone(),
                    item.timestamp
                        .with_timezone(&chrono::Local)
                        .format("%H:%M:%S")
                        .to_string(),
                    item.reveal_path.clone(),
                )
            })
            .collect();
        let empty = items.is_empty();
        let os_notify = self.os_notify.enabled;
        let list = div()
            .id("notifications-list")
            .max_h(px(s(360.0)))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .when(empty, |list| {
                list.child(
                    div()
                        .text_color(Theme::text_dim())
                        .child(crate::localization::text("notifications.empty")),
                )
            })
            .children(items.into_iter().enumerate().map(
                |(idx, (label, message, time, reveal_path))| {
                    div()
                        .py(px(s(5.0)))
                        .border_b_1()
                        .border_color(Theme::border())
                        .flex()
                        .flex_col()
                        .gap(px(s(1.0)))
                        .child(
                            div()
                                .text_color(Theme::text_dim())
                                .text_size(px(Theme::font_size_ratio(0.78)))
                                .child(crate::localization::format(
                                    "notifications.item_timestamp",
                                    &[("label", &label), ("time", &time)],
                                )),
                        )
                        .child(
                            div()
                                .text_color(Theme::text())
                                .whitespace_normal()
                                .child(message),
                        )
                        .children(reveal_path.map(|path| {
                            Button::new(("reveal", idx))
                                .ghost()
                                .label(crate::localization::text("notifications.actions.reveal"))
                                .on_click(cx.listener(move |_, _, _, _| {
                                    crate::reveal_path::reveal_file(&path);
                                }))
                        }))
                },
            ));
        Some(
            Dialog::new("notifications-dialog", |this: &mut Self| {
                this.dialog_frames.get("notifications-dialog", || {
                    DialogFrame::new_auto(Theme::scaled(420.0))
                })
            })
            .title(crate::localization::text("notifications.dialog.title"))
            .dismiss_on_scrim_click(true)
            .on_close(|this, _window, cx| {
                this.notifications_open = false;
                cx.notify();
            })
            .child(list)
            .leading_button(
                gpui_component::checkbox::Checkbox::new("os-notify-toggle")
                    .checked(os_notify)
                    .label(crate::localization::text("notifications.system_toggle"))
                    .on_click(cx.listener(|this, checked: &bool, _window, cx| {
                        this.os_notify.enabled = *checked;
                        this.os_notify.save();
                        cx.notify();
                    })),
            )
            .button(
                Button::new("notifications-clear")
                    .label(crate::localization::text("notifications.actions.clear_all"))
                    .disabled(empty)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.notifications.clear();
                        cx.notify();
                    })),
            )
            .build(self, window, cx),
        )
    }

    pub(super) fn dismiss_update_dialog(&mut self, cx: &mut Context<Self>) {
        self.show_update_dialog = false;
        cx.notify();
    }

    pub(super) fn skip_update_version(&mut self, cx: &mut Context<Self>) {
        let Some(latest) = self
            .update_available
            .as_ref()
            .map(|update| update.latest.clone())
        else {
            return;
        };
        let mut prefs = crate::preferences::UpdatePreferences::load();
        prefs.skipped_version = Some(latest.clone());
        prefs.snoozed_until = None;
        prefs.save();
        self.update_available = None;
        self.show_update_dialog = false;
        self.set_status(
            StatusLevel::Info,
            crate::localization::format("updates.status.skipped", &[("version", &latest)]),
        );
        cx.notify();
    }

    /// The update dialog's "Show at startup" checkbox.
    pub(super) fn set_update_startup_notice(&mut self, enabled: bool, cx: &mut Context<Self>) {
        let mut prefs = crate::preferences::UpdatePreferences::load();
        prefs.startup_notice_disabled = !enabled;
        prefs.save();
        cx.notify();
    }

    pub(super) fn snooze_update(&mut self, cx: &mut Context<Self>) {
        let mut prefs = crate::preferences::UpdatePreferences::load();
        prefs.snoozed_until =
            Some(chrono::Utc::now() + chrono::Duration::hours(crate::app_update::SNOOZE_HOURS));
        prefs.save();
        self.update_available = None;
        self.show_update_dialog = false;
        self.set_status(
            StatusLevel::Info,
            crate::localization::text("updates.status.snoozed"),
        );
        cx.notify();
    }

    pub(super) fn cancel_update_idle_download(&mut self, cx: &mut Context<Self>) {
        self.update_install_when_idle = false;
        self.update_restart_when_ready = false;
        self.update_idle_poll = None;
        cx.notify();
    }

    pub(super) fn install_update_when_idle(&mut self, cx: &mut Context<Self>) {
        if self.update_install_when_idle {
            self.cancel_update_idle_download(cx);
            return;
        }

        // With an in-app install, download now and restart once idle; without
        // one, open the browser download once idle.
        let self_install = crate::self_update::unsupported_reason().is_none();
        if self_install {
            self.update_restart_when_ready = true;
            self.start_update_install(cx);
        }

        let work = self.in_flight_work(cx);
        if crate::app_update::idle_install_ready(&work) {
            if self_install {
                self.restart_into_update_if_ready(cx);
            } else {
                self.download_update(cx);
            }
            return;
        }

        self.update_install_when_idle = true;
        self.show_update_dialog = false;
        let message = if self_install {
            crate::localization::format(
                "updates.status.restart_when_idle",
                &[("work", &work.summary())],
            )
        } else {
            crate::localization::format(
                "updates.status.download_when_idle",
                &[("work", &work.summary())],
            )
        };
        self.set_status(StatusLevel::Info, message);

        if self.update_idle_poll.is_some() {
            cx.notify();
            return;
        }

        let weak = cx.weak_entity();
        let async_cx = cx.to_async();
        let task = cx.spawn(async move |_this, _cx| {
            loop {
                async_cx
                    .background_executor()
                    .timer(Duration::from_secs(2))
                    .await;
                let mut cx = async_cx.clone();
                match weak.update(&mut cx, |this, cx| {
                    if !this.update_install_when_idle {
                        this.update_idle_poll = None;
                        return false;
                    }
                    let work = this.in_flight_work(cx);
                    if !work.is_idle() {
                        return true;
                    }
                    if self_install {
                        // Idle, but the update may still be downloading.
                        return match this.update_install {
                            crate::self_update::InstallState::Ready(_) => {
                                this.update_install_when_idle = false;
                                this.update_idle_poll = None;
                                this.restart_into_update_if_ready(cx);
                                false
                            }
                            crate::self_update::InstallState::Failed(_) => {
                                this.update_install_when_idle = false;
                                this.update_restart_when_ready = false;
                                this.update_idle_poll = None;
                                cx.notify();
                                false
                            }
                            _ => true,
                        };
                    }
                    if let Some(latest) = this.update_available.as_ref().map(|u| u.latest.clone()) {
                        this.notifications.push(
                            crate::notification_center::NotificationKind::UpdateAvailable,
                            crate::localization::format(
                                "updates.notification.idle_download",
                                &[("version", &latest)],
                            ),
                            chrono::Utc::now(),
                        );
                    }
                    this.update_install_when_idle = false;
                    this.update_idle_poll = None;
                    this.download_update(cx);
                    false
                }) {
                    Ok(keep_polling) => {
                        if !keep_polling {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        self.update_idle_poll = Some(task);
        cx.notify();
    }

    /// The dialog's primary button. Downloads and installs in-app when this
    /// copy can replace itself (restarting once the update is staged);
    /// otherwise opens the download for this machine's OS + architecture in
    /// the browser and closes the dialog.
    pub(super) fn download_update(&mut self, cx: &mut Context<Self>) {
        let mut prefs = crate::preferences::UpdatePreferences::load();
        prefs.skipped_version = None;
        prefs.snoozed_until = None;
        prefs.save();
        if crate::self_update::unsupported_reason().is_none() {
            match self.update_install {
                crate::self_update::InstallState::Ready(_) => self.restart_into_update_if_ready(cx),
                _ => self.start_update_install(cx),
            }
            return;
        }
        let _ = crate::browser_open::open_in_browser(&crate::store::download_latest_url());
        self.update_install_when_idle = false;
        self.update_idle_poll = None;
        self.show_update_dialog = false;
        cx.notify();
    }

    /// Download the update and stage it beside the installed copy, keeping the
    /// dialog's progress current. A no-op while one is running or staged.
    pub(super) fn start_update_install(&mut self, cx: &mut Context<Self>) {
        use crate::self_update::InstallState;
        if self.update_install.is_busy() || matches!(self.update_install, InstallState::Ready(_)) {
            return;
        }
        // The browser build has no installer to hand a download to, and no
        // state to park in: a failure here would only offer a Try Again that
        // fails the same way. Open the download page, as the manual path
        // does, and leave the dialog idle.
        #[cfg(target_arch = "wasm32")]
        {
            let _ = crate::browser_open::open_in_browser(&crate::store::download_latest_url());
            self.update_install = InstallState::Idle;
            self.update_install_when_idle = false;
            self.update_idle_poll = None;
            self.show_update_dialog = false;
            cx.notify();
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            // A job cancelled a moment ago may still be stopping its download
            // or removing what it staged, in the same folders this one is
            // about to use; let it finish first so its cleanup can't take
            // this attempt's files with it. The dialog says so from the
            // first frame rather than showing an empty download until the
            // poll's first tick.
            let previous = self.update_install_draining.take();
            let progress = std::sync::Arc::new(crate::self_update::DownloadProgress::default());
            // The progress says so too, from now rather than from whenever
            // the job gets scheduled, so the poll's first tick (which reads
            // the progress) agrees with this frame.
            let queued = previous.as_ref().is_some_and(|p| !p.is_finished());
            if queued {
                progress.announce_waiting_for_previous();
            }
            self.update_install = if queued {
                InstallState::CleaningUp
            } else {
                InstallState::Downloading {
                    received: 0,
                    total: None,
                }
            };
            cx.notify();
            self.update_install_progress = Some(progress.clone());
            // One handle for the stage's own work, one for the job around it.
            let stage = crate::self_update::stage_update(progress.clone());
            let job_progress = progress.clone();
            crate::tokio_handle().spawn(async move {
                crate::self_update::run_staging(job_progress, previous, stage).await;
            });
            let weak = cx.weak_entity();
            let async_cx = cx.to_async();
            // The handle cancels the job if this poll is dropped before it
            // collected the result (the view torn down, the task replaced),
            // so a parked staging is never left waiting for a poll that is
            // gone.
            let handle = crate::self_update::InstallHandle::new(progress);
            let task = cx.spawn(async move |_this, _cx| {
                let outcome = loop {
                    async_cx
                        .background_executor()
                        .timer(Duration::from_millis(250))
                        .await;
                    match handle.verdict() {
                        crate::self_update::Verdict::Outcome(outcome) => break outcome,
                        crate::self_update::Verdict::Cancelled => {
                            // The dialog's own Cancel resets the view and
                            // drops this poll in the same call, so this is
                            // only seen when something else cancelled the
                            // progress. The job removes its own files either
                            // way; the dialog is reset here too, so it is
                            // never left busy, but only while this poll's
                            // attempt is still the dialog's current one (a
                            // newer attempt owns the dialog otherwise).
                            let mut cx = async_cx.clone();
                            let _ = weak.update(&mut cx, |this, cx| {
                                let still_ours =
                                    this.update_install_progress
                                        .as_ref()
                                        .is_some_and(|current| {
                                            std::ptr::eq(current.as_ref(), handle.progress())
                                        });
                                if !still_ours {
                                    return;
                                }
                                this.update_install_draining = this.update_install_progress.take();
                                this.update_install_task = None;
                                this.update_install = crate::self_update::InstallState::Idle;
                                cx.notify();
                            });
                            return;
                        }
                        crate::self_update::Verdict::Vanished => {
                            // The job catches its own panics and parks them
                            // as failures, so only an aborted job ends here.
                            tracing::error!("the in-app update job ended without a result");
                            break Err(crate::self_update::stopped_unexpectedly());
                        }
                        crate::self_update::Verdict::Pending => {}
                    }
                    let progress = handle.progress();
                    let (received, total) = progress.snapshot();
                    let mut cx = async_cx.clone();
                    let alive = weak.update(&mut cx, |this, cx| {
                        // Once every byte is in, the job is unpacking.
                        let next = if progress.is_waiting_for_previous() {
                            InstallState::CleaningUp
                        } else if total.is_some_and(|t| received >= t) {
                            InstallState::Preparing
                        } else {
                            InstallState::Downloading { received, total }
                        };
                        this.update_install = next;
                        cx.notify();
                    });
                    if alive.is_err() {
                        // Nobody is left to collect the result; dropping
                        // the handle tells the job, which reclaims and
                        // removes whatever it staged.
                        return;
                    }
                };
                let mut cx = async_cx.clone();
                // Handed to the view only if it is still there; otherwise it
                // stays here to be cleaned up below.
                let mut slot = Some(outcome);
                let shown = weak.update(&mut cx, |this, cx| {
                    let Some(outcome) = slot.take() else {
                        return;
                    };
                    this.update_install_task = None;
                    this.update_install_progress = None;
                    match outcome {
                        Ok(prepared) => {
                            let version = prepared.version.clone();
                            this.update_install = InstallState::Ready(prepared);
                            let message = crate::localization::format(
                                "updates.status.ready_to_restart",
                                &[("version", &version)],
                            );
                            this.notifications.push(
                                crate::notification_center::NotificationKind::UpdateAvailable,
                                message.clone(),
                                chrono::Utc::now(),
                            );
                            this.set_status(StatusLevel::Success, message);
                            if this.update_restart_when_ready
                                && !this.update_install_when_idle
                                && this.in_flight_work(cx).is_idle()
                            {
                                this.restart_into_update_if_ready(cx);
                            }
                        }
                        Err(message) => {
                            let message = crate::self_update::localized_error(&message);
                            tracing::warn!(%message, "in-app update failed");
                            this.update_restart_when_ready = false;
                            this.set_status(
                                StatusLevel::Error,
                                crate::localization::format(
                                    "updates.status.failed",
                                    &[("error", &message)],
                                ),
                            );
                            this.update_install = InstallState::Failed(message);
                        }
                    }
                    cx.notify();
                });
                // The view went away between collecting and showing the
                // result; a staged update nobody will install is removed.
                if shown.is_err()
                    && let Some(Ok(prepared)) = slot.take()
                {
                    let _ = crate::tokio_handle()
                        .spawn_blocking(move || crate::self_update::discard(&prepared))
                        .await;
                }
            });
            self.update_install_task = Some(task);
        }
    }

    /// The dialog's Cancel while an update downloads or unpacks: stop the job
    /// and put the dialog back to its idle buttons. The job notices at its
    /// next chunk (or once unpacking ends) and removes what it staged.
    pub(super) fn cancel_update_install(&mut self, cx: &mut Context<Self>) {
        if !self.update_install.is_busy() {
            return;
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(progress) = self.update_install_progress.take() {
            // The job removes whatever it downloaded or staged, including a
            // staging it had already parked for us, before it reports
            // finished. Kept until the next install starts, which waits for
            // that before touching the same folders.
            progress.cancel();
            self.update_install_draining = Some(progress);
        }
        // Dropping the poll task stops the dialog's progress updates and
        // drops the job's outcome; the job itself winds down on its own.
        self.update_install_task = None;
        // A cancelled download also ends any wait to install it when idle.
        self.update_install_when_idle = false;
        self.update_idle_poll = None;
        self.update_restart_when_ready = false;
        self.update_install = crate::self_update::InstallState::Idle;
        self.set_status(
            StatusLevel::Info,
            crate::localization::text("updates.status.cancelled"),
        );
        cx.notify();
    }

    /// Hand the staged update to the installer helper and quit; the helper
    /// swaps it in once SQLLY exits and relaunches it. The workspace session
    /// (unsaved editors included) is saved first, as on a normal quit.
    pub(super) fn restart_into_update_if_ready(&mut self, cx: &mut Context<Self>) {
        let crate::self_update::InstallState::Ready(prepared) = &self.update_install else {
            return;
        };
        #[cfg(not(target_arch = "wasm32"))]
        match crate::self_update::launch_installer(prepared) {
            Ok(()) => {
                self.save_workspace_session(cx);
                cx.quit();
            }
            Err(message) => {
                let message = crate::self_update::localized_error(&message);
                self.update_restart_when_ready = false;
                self.set_status(
                    StatusLevel::Error,
                    crate::localization::format(
                        "updates.status.installer_launch_failed",
                        &[("error", &message)],
                    ),
                );
                self.update_install = crate::self_update::InstallState::Failed(message);
                cx.notify();
            }
        }
        #[cfg(target_arch = "wasm32")]
        let _ = (prepared, cx);
    }

    /// Report, once, a previous restart whose install had to be rolled back.
    pub(super) fn report_previous_update_failure(&mut self, cx: &mut Context<Self>) {
        if let Some(message) = crate::self_update::take_install_failure() {
            self.notifications.push(
                crate::notification_center::NotificationKind::UpdateAvailable,
                message.clone(),
                chrono::Utc::now(),
            );
            self.set_status(StatusLevel::Error, message);
            cx.notify();
        }
    }

    /// The "update available" dialog: a scrollable list of everything new since
    /// this build, and a button that downloads, installs and restarts in-app
    /// (or, where this copy can't replace itself, opens the download in the
    /// browser). Rendered on the unified dialog chrome.
    pub(super) fn render_update_dialog(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let s = Theme::scaled;
        let installed = crate::about::APP_VERSION;
        let update = self.update_available.clone();
        let latest = update
            .as_ref()
            .map(|u| u.latest.clone())
            .unwrap_or_default();
        let versions = update.map(|u| u.versions).unwrap_or_default();
        let update_prefs = crate::preferences::UpdatePreferences::load();
        let skipped_note = update_prefs.skipped_version.as_ref().map(|skipped| {
            crate::localization::format("updates.dialog.skipped_note", &[("version", skipped)])
        });
        let in_flight = self.in_flight_work(cx);
        let waiting_for_idle = self.update_install_when_idle;
        let download_page = crate::app_update::download_page_url(&crate::store::base_url());
        let download_page_label = crate::app_update::link_label(&download_page);
        let self_install_blocker = crate::self_update::unsupported_reason();
        let install = self.update_install.clone();
        let install_error = match &install {
            crate::self_update::InstallState::Failed(message) => Some(message.clone()),
            _ => None,
        };
        let install_note = match &self_install_blocker {
            None => crate::localization::text("updates.dialog.automatic_install_note"),
            Some(reason) => crate::localization::format(
                "updates.dialog.manual_install_note",
                &[("reason", reason)],
            ),
        };

        let body = div()
            .flex()
            .flex_col()
            .gap(px(s(10.0)))
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(1.15)))
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(Theme::text())
                    .child(crate::localization::format(
                        "updates.dialog.available_title",
                        &[("version", &latest)],
                    )),
            )
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.9)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::format(
                        "updates.dialog.installed_version",
                        &[("version", installed)],
                    )),
            )
            .children(skipped_note.into_iter().map(|note| {
                div()
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .text_color(Theme::text_dim())
                    .child(note)
            }))
            .when(waiting_for_idle, |el| {
                el.child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.85)))
                        .text_color(Theme::text_dim())
                        .child(crate::localization::text("updates.dialog.waiting_for_idle")),
                )
            })
            // Scrollable list of what's new across every
            // version between the installed build and the
            // latest, with a visible scrollbar so it's clear
            // there is more to read.
            .child(
                div()
                    .relative()
                    .max_h(px(s(300.0)))
                    .child(
                        div()
                            .id("update-dialog-changes")
                            .flex()
                            .flex_col()
                            .gap(px(s(12.0)))
                            .max_h(px(s(300.0)))
                            .overflow_y_scroll()
                            .track_scroll(&self.update_dialog_scroll)
                            .pr(px(s(12.0)))
                            .children(versions.into_iter().map(|v| {
                                let header = if v.date.is_empty() {
                                    format!("v{}", v.version)
                                } else {
                                    format!("v{}  \u{00b7}  {}", v.version, v.date)
                                };
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(px(s(4.0)))
                                    .child(
                                        div()
                                            .text_size(px(Theme::font_size()))
                                            .font_weight(gpui::FontWeight::BOLD)
                                            .text_color(Theme::accent())
                                            .child(header),
                                    )
                                    .children(v.sections.into_iter().map(|section| {
                                        div()
                                            .flex()
                                            .flex_col()
                                            .gap(px(s(3.0)))
                                            .child(
                                                div()
                                                    .text_size(px(Theme::font_size_ratio(0.92)))
                                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                                    .text_color(Theme::text())
                                                    .child(section.title),
                                            )
                                            .children(section.details.into_iter().map(|d| {
                                                if d.is_bullet {
                                                    div()
                                                        .flex()
                                                        .flex_row()
                                                        .gap(px(s(6.0)))
                                                        .text_size(px(Theme::font_size_ratio(0.88)))
                                                        .text_color(Theme::text_dim())
                                                        .child(
                                                            div().flex_shrink_0().child("\u{2022}"),
                                                        )
                                                        .child(div().flex_1().child(d.text))
                                                } else {
                                                    div()
                                                        .text_size(px(Theme::font_size_ratio(0.88)))
                                                        .text_color(Theme::text_dim())
                                                        .child(d.text)
                                                }
                                            }))
                                    }))
                            })),
                    )
                    .child(gpui_component::scroll::Scrollbar::vertical(
                        &self.update_dialog_scroll,
                    )),
            )
            .children(install_error.map(|message| {
                div()
                    .text_size(px(Theme::font_size_ratio(0.88)))
                    .text_color(Theme::error())
                    .child(crate::localization::format(
                        "updates.dialog.install_failed_with_download",
                        &[("error", &message)],
                    ))
            }))
            // Small print: how the install works, or why it's manual here.
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.78)))
                    .text_color(Theme::text_dim())
                    .child(install_note),
            )
            // A visible download link to the store's download page (every
            // platform and package), next to the per-platform button below.
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap(px(s(4.0)))
                    .text_size(px(Theme::font_size_ratio(0.88)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text("updates.dialog.download_label"))
                    .child(
                        div()
                            .id("update-dialog-download-link")
                            .text_color(Theme::accent())
                            .underline()
                            .cursor_pointer()
                            .on_click(move |_, _, _| {
                                let _ = crate::browser_open::open_in_browser(&download_page);
                            })
                            .child(download_page_label),
                    ),
            );

        let mut dialog =
            crate::dialog::Dialog::new("update-available-dialog", |this: &mut Self| {
                this.dialog_frames.get("update-available-dialog", || {
                    crate::dialog::DialogFrame::new_auto(Theme::scaled(800.0))
                })
            })
            .title(crate::localization::text("updates.dialog.title"))
            .on_close(|this, _window, cx| this.dismiss_update_dialog(cx))
            .child(body)
            .leading_button(
                gpui_component::checkbox::Checkbox::new("update-dialog-startup-notice")
                    .checked(!update_prefs.startup_notice_disabled)
                    .label(crate::localization::text(
                        "updates.dialog.startup_notice_toggle",
                    ))
                    .on_click(cx.listener(|this, checked: &bool, _window, cx| {
                        this.set_update_startup_notice(*checked, cx);
                    })),
            );
        // While the update downloads or unpacks, Cancel is the only choice
        // beside the (disabled) primary button; it stops the job and brings
        // the usual buttons back.
        let busy = install.is_busy();
        if busy {
            dialog = dialog.button(
                Button::new("update-dialog-cancel-install")
                    .label(crate::localization::text(
                        "updates.dialog.cancel_install_button",
                    ))
                    .on_click(cx.listener(|this, _, _, cx| this.cancel_update_install(cx))),
            );
        } else {
            dialog = dialog
                .button(
                    Button::new("update-dialog-skip")
                        .label(crate::localization::text("updates.dialog.skip_button"))
                        .on_click(cx.listener(|this, _, _, cx| this.skip_update_version(cx))),
                )
                .button(
                    Button::new("update-dialog-snooze")
                        .label(crate::localization::text("updates.dialog.snooze_button"))
                        .on_click(cx.listener(|this, _, _, cx| this.snooze_update(cx))),
                );
        }
        let ready = matches!(install, crate::self_update::InstallState::Ready(_));
        if !busy && (waiting_for_idle || !in_flight.is_idle()) {
            let idle_label = match (waiting_for_idle, self_install_blocker.is_none()) {
                (true, true) => crate::localization::text("updates.dialog.cancel_idle_install"),
                (true, false) => crate::localization::text("updates.dialog.cancel_idle_download"),
                (false, true) if ready => crate::localization::format(
                    "updates.dialog.restart_when_idle",
                    &[("work", &in_flight.summary())],
                ),
                (false, true) => crate::localization::format(
                    "updates.dialog.install_when_idle",
                    &[("work", &in_flight.summary())],
                ),
                (false, false) => crate::localization::format(
                    "updates.dialog.download_when_idle",
                    &[("work", &in_flight.summary())],
                ),
            };
            dialog = dialog.button(
                Button::new("update-dialog-idle")
                    .label(idle_label)
                    .on_click(cx.listener(|this, _, _, cx| this.install_update_when_idle(cx))),
            );
        }
        if !busy {
            dialog = dialog.button(
                Button::new("update-dialog-check-now")
                    .label(crate::localization::text(
                        "updates.dialog.check_again_button",
                    ))
                    .on_click(cx.listener(|this, _, _, cx| this.check_for_update(true, cx))),
            );
        }
        dialog
            .button({
                let label =
                    Self::update_install_label(self_install_blocker.is_some(), &install, &latest);
                Button::new("update-dialog-download")
                    .primary()
                    .label(label)
                    .disabled(busy)
                    .on_click(cx.listener(|this, _, _, cx| {
                        if matches!(
                            this.update_install,
                            crate::self_update::InstallState::Failed(_)
                        ) {
                            this.update_install = crate::self_update::InstallState::Idle;
                        }
                        this.download_update(cx)
                    }))
            })
            .build(self, window, cx)
            .into_any_element()
    }

    /// The update dialog's primary button label for this install state.
    fn update_install_label(
        manual_download: bool,
        install: &crate::self_update::InstallState,
        latest: &str,
    ) -> String {
        use crate::self_update::InstallState;
        match (manual_download, install) {
            (true, _) => crate::localization::format(
                "updates.actions.download_version",
                &[("version", latest)],
            ),
            (false, InstallState::Idle) => crate::localization::format(
                "updates.actions.install_version",
                &[("version", latest)],
            ),
            (false, InstallState::Downloading { received, total }) => crate::localization::format(
                "updates.actions.downloading",
                &[(
                    "progress",
                    &crate::self_update::progress_label(*received, *total),
                )],
            ),
            (false, InstallState::Preparing) => {
                crate::localization::text("updates.actions.verifying")
            }
            (false, InstallState::CleaningUp) => {
                crate::localization::text("updates.actions.cleaning_up")
            }
            (false, InstallState::Ready(_)) => crate::localization::text("updates.actions.restart"),
            (false, InstallState::Failed(_)) => {
                crate::localization::text("updates.actions.try_again")
            }
        }
    }

    /// Re-read the stored session in the background so the account control
    /// reflects a sign-in or sign-out that happened elsewhere (the sign-in
    /// window is a separate OS window). The read is a keychain / Windows
    /// Credential Manager call, which can take long enough to stall a frame,
    /// so it never runs on the UI thread; the control updates when it
    /// answers, and only notifies when the signed-in identity changed.
    pub(super) fn refresh_account_session(&mut self, cx: &mut Context<Self>) {
        self.load_account_session_then(cx, |_, _| {});
    }

    /// Read the stored session off the UI thread, store it, then run `then`
    /// with the fresh value in place. A failed read (the blocking task was
    /// dropped) leaves the cached session as it was and skips `then`.
    fn load_account_session_then(
        &mut self,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, &mut Context<Self>) + 'static,
    ) {
        let mut async_cx = cx.to_async();
        let weak_self = cx.weak_entity();
        cx.spawn(async move |_this, _cx| {
            let loaded = crate::tokio_handle()
                .spawn_blocking(|| crate::store::session::load().filter(|s| !s.is_expired()))
                .await;
            let Ok(session) = loaded else {
                return;
            };
            let _ = weak_self.update(&mut async_cx, |this, cx| {
                if session != this.account_session {
                    this.account_session = session;
                    cx.notify();
                }
                then(this, cx);
            });
        })
        .detach();
    }

    /// Account control clicked: signed in → open the subscription dialog and
    /// (re)load the subscription; signed out → open the sign-in window.
    ///
    /// Decides on a fresh read of the stored session (taken off the UI
    /// thread), so a sign-in or sign-out in the other window, or an expired
    /// token, is honoured on this click. Further clicks while that read is in
    /// flight are ignored.
    pub(super) fn on_account_button_clicked(&mut self, cx: &mut Context<Self>) {
        if self.account_click_pending {
            return;
        }
        self.account_click_pending = true;
        self.load_account_session_then(cx, |this, cx| {
            this.account_click_pending = false;
            this.open_account_for_session(cx);
        });
    }

    /// The click's decision, once the session read has answered.
    fn open_account_for_session(&mut self, cx: &mut Context<Self>) {
        if self.account_session.is_some() {
            self.show_account_dialog = true;
            self.account_device_id_copied = false;
            self.load_account_subscription(cx);
            cx.notify();
        } else {
            crate::store_signin::open_store_signin_window(cx);
        }
    }

    /// Dismiss the account / subscription dialog.
    pub(super) fn dismiss_account_dialog(&mut self, cx: &mut Context<Self>) {
        self.show_account_dialog = false;
        self.account_device_id_copied = false;
        cx.notify();
    }

    /// Copy the signed-in device id to the clipboard and flip the inline
    /// "copied" confirmation in the account dialog.
    pub(super) fn copy_account_device_id(&mut self, cx: &mut Context<Self>) {
        let Some(device_id) = self
            .account_session
            .as_ref()
            .and_then(|se| se.device_id.clone())
        else {
            return;
        };
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(device_id));
        self.account_device_id_copied = true;
        cx.notify();
    }

    /// Sign out from the account dialog: clear the keychain session + pairing,
    /// drop the cached session, and close the dialog. Mirrors the sign-out in
    /// the sign-in window ([`crate::store_signin`]).
    pub(super) fn account_sign_out(&mut self, cx: &mut Context<Self>) {
        crate::store::session::clear();
        crate::store::pairing::clear();
        self.account_session = None;
        self.account_subscription = AccountSubscriptionState::Idle;
        self.show_account_dialog = false;
        cx.notify();
    }

    /// Fetch the signed-in account's subscription for the account dialog. A
    /// `404` from the store means "signed in, free tier" and is surfaced as
    /// `Loaded(None)` rather than an error.
    pub(super) fn load_account_subscription(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.account_session.clone() else {
            return;
        };
        self.account_subscription = AccountSubscriptionState::Loading;
        let token = session.access_token.clone();
        let async_cx = cx.to_async();
        let weak_self = cx.weak_entity();
        cx.spawn(async move |_this, _cx| {
            let result = crate::tokio_handle()
                .spawn(async move { crate::store::fetch_subscription(&token).await })
                .await;
            let mut cx = async_cx.clone();
            weak_self
                .update(&mut cx, |this, cx| {
                    this.account_subscription = match result {
                        Ok(Ok(subscription)) => {
                            // Fresh entitlement answer: keep the licensed-user
                            // marker (which silences the supporter message and
                            // pay dialog) in step with the store.
                            crate::license::record_entitlement(
                                crate::license::entitlement_from_subscription(
                                    subscription.as_ref(),
                                ),
                            );
                            if crate::license::is_paying() {
                                this.show_nag_dialog = false;
                            }
                            AccountSubscriptionState::Loaded(subscription)
                        }
                        Ok(Err(error)) => AccountSubscriptionState::Failed(
                            crate::localization::message(error.display_message()),
                        ),
                        Err(error) => {
                            AccountSubscriptionState::Failed(crate::localization::format(
                                "account.subscription.task_failed",
                                &[("error", &error.to_string())],
                            ))
                        }
                    };
                    cx.notify();
                })
                .ok();
        })
        .detach();
    }

    /// Background entitlement refresh at startup: if a store session exists,
    /// fetch the subscription and update the persisted licensed-until marker —
    /// so a licensed user's nag surfaces stay off without them ever opening
    /// the account dialog. If the fresh answer says "licensed", any pay dialog
    /// decided at launch (off the stale marker) is retracted. Best-effort:
    /// failures leave the persisted marker as-is (a licensed user does not
    /// lose recognition by being offline).
    pub(super) fn refresh_entitlement(&self, cx: &mut Context<Self>) {
        let Some(session) = self.account_session.clone() else {
            return;
        };
        let token = session.access_token.clone();
        let async_cx = cx.to_async();
        let weak_self = cx.weak_entity();
        cx.spawn(async move |_this, _cx| {
            let Ok(Ok(subscription)) = crate::tokio_handle()
                .spawn(async move { crate::store::fetch_subscription(&token).await })
                .await
            else {
                return; // offline/unreachable: keep the persisted marker
            };
            crate::license::record_entitlement(crate::license::entitlement_from_subscription(
                subscription.as_ref(),
            ));
            let mut cx = async_cx.clone();
            weak_self
                .update(&mut cx, |this, cx| {
                    if crate::license::is_paying() && this.show_nag_dialog {
                        this.show_nag_dialog = false;
                    }
                    // The supporter chip/banner re-evaluate on render.
                    cx.notify();
                })
                .ok();
        })
        .detach();
    }

    /// Best-effort check for a newer desktop build at startup: fetch the store's
    /// changelog feed and, if its latest version is newer than this build, stash
    /// the update so the status bar can surface it. Silent on any failure
    /// (offline, browser build, older-or-equal version).
    /// The Help ▸ Check for Updates menu action. Forces a fresh fetch (there is
    /// no client-side cache to bypass — the feed is fetched anew each call) and,
    /// unlike the silent launch check, always tells the user the outcome: opens
    /// the dialog when newer, or reports "up to date" / a failure in the status
    /// bar when not.
    pub(super) fn handle_check_for_updates(
        &mut self,
        _action: &CheckForUpdates,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if crate::distribution::is_standalone() {
            self.notify_feature_disabled(
                crate::localization::text("updates.status.standalone_build"),
                cx,
            );
            return;
        }
        if let Some(store) = crate::distribution::updating_store_name() {
            self.set_status(
                StatusLevel::Info,
                crate::localization::format(
                    "updates.status.store_managed",
                    &[("version", crate::about::APP_VERSION), ("store", store)],
                ),
            );
            cx.notify();
            return;
        }
        if self.update_available.is_some() {
            self.show_update_dialog = true;
            cx.notify();
            return;
        }
        self.set_status(
            StatusLevel::Busy,
            crate::localization::text("updates.status.checking"),
        );
        cx.notify();
        self.check_for_update(true, cx);
    }

    /// Ask the shared update-status entity for a check. `manual` (the menu
    /// action) latches a flag so the observer surfaces this check's outcome —
    /// dialog on a hit, status message plus toast otherwise. The silent launch
    /// check leaves the status bar untouched so a background check never talks
    /// over the user.
    pub(super) fn check_for_update(&mut self, manual: bool, cx: &mut Context<Self>) {
        if manual {
            self.awaiting_manual_update_check = true;
        }
        let status = crate::update_status::handle(cx);
        status.update(cx, |status, cx| status.check(cx));
    }

    /// React to the shared update status changing, no matter which surface
    /// started the check: adopt an available update into the status-bar chip,
    /// and — only when this view latched a manual check — surface the outcome
    /// with the dialog or a status message plus toast. (The status bar is easy
    /// to miss and other work can overwrite it; a manual check deserves an
    /// unmissable answer.)
    pub(super) fn on_update_status_changed(
        &mut self,
        status: gpui::Entity<crate::update_status::UpdateStatus>,
        cx: &mut Context<Self>,
    ) {
        use crate::update_status::UpdateCheckState;
        let state = status.read(cx).state.clone();
        // The launch check announces once: whatever its first outcome, later
        // (periodic) checks only update the status-bar chip.
        let mut startup = false;
        if !matches!(state, UpdateCheckState::Checking | UpdateCheckState::Idle) {
            self.update_checked_at = Some(Instant::now());
            startup = std::mem::take(&mut self.startup_update_notice_pending);
        }
        match state {
            UpdateCheckState::Available(update) => {
                let prefs = crate::preferences::UpdatePreferences::load();
                let manual = self.awaiting_manual_update_check;
                let announce_at_startup = startup && !prefs.startup_notice_disabled;
                let surface = crate::app_update::should_surface(
                    &update,
                    prefs.skipped_version.as_deref(),
                    prefs.snoozed_until,
                    chrono::Utc::now(),
                );
                if surface {
                    let is_new = self
                        .update_available
                        .as_ref()
                        .is_none_or(|known| known.latest != update.latest);
                    if is_new {
                        self.notifications.push(
                            crate::notification_center::NotificationKind::UpdateAvailable,
                            crate::localization::format(
                                "updates.notification.available",
                                &[("version", &update.latest)],
                            ),
                            chrono::Utc::now(),
                        );
                    }
                    self.update_available = Some(update);
                    if std::mem::take(&mut self.awaiting_manual_update_check) || announce_at_startup
                    {
                        self.show_update_dialog = true;
                    }
                } else if manual {
                    self.update_available = Some(update);
                    if std::mem::take(&mut self.awaiting_manual_update_check) {
                        self.show_update_dialog = true;
                    }
                } else {
                    self.update_available = None;
                }
                cx.notify();
            }
            UpdateCheckState::UpToDate => {
                // A fresh check says nothing newer exists: retire any stale
                // chip/dialog rather than keep advertising an update that a
                // newer check no longer confirms.
                if self.update_available.take().is_some() {
                    self.show_update_dialog = false;
                    cx.notify();
                }
                if std::mem::take(&mut self.awaiting_manual_update_check) {
                    self.set_status(
                        StatusLevel::Success,
                        crate::localization::format(
                            "updates.status.up_to_date",
                            &[("version", crate::about::APP_VERSION)],
                        ),
                    );
                    Self::toast_update_outcome(
                        gpui_component::notification::Notification::success(
                            crate::localization::format(
                                "updates.toast.up_to_date",
                                &[("version", crate::about::APP_VERSION)],
                            ),
                        ),
                        cx,
                    );
                    cx.notify();
                }
            }
            UpdateCheckState::Failed => {
                if std::mem::take(&mut self.awaiting_manual_update_check) {
                    self.set_status(
                        StatusLevel::Warning,
                        crate::localization::text("updates.status.check_failed"),
                    );
                    Self::toast_update_outcome(
                        gpui_component::notification::Notification::warning(
                            crate::localization::text("updates.toast.check_failed"),
                        ),
                        cx,
                    );
                    cx.notify();
                }
            }
            UpdateCheckState::Checking | UpdateCheckState::Idle => {}
        }
    }

    /// Show a toast in the frontmost workbench window for a manual
    /// update-check outcome. Deferred out of the current update pass because
    /// pushing a notification needs `&mut Window`, which the async
    /// continuation does not hold. (`push_notification` updates the window's
    /// `Root` entity — safe only because `update_active_app_view` does not
    /// hold a `Root` lease while running the callback.)
    pub(super) fn toast_update_outcome(
        note: gpui_component::notification::Notification,
        cx: &mut Context<Self>,
    ) {
        cx.defer(move |app| {
            Self::update_active_app_view(app, |_view, window, cx| {
                use gpui_component::WindowExt as _;
                window.push_notification(note, cx);
            });
        });
    }

    /// The account control. Signed in → success-tinted person glyph with a
    /// status dot; click opens the subscription dialog. Signed out → dim glyph;
    /// click opens sign-in. Either way the tooltip names the current state.
    /// Kept for the account dialog chrome; the status-bar cluster now
    /// dispatches [`OpenAccount`].
    #[allow(dead_code)]
    pub(super) fn render_account_button(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let signed_in = self.account_session.is_some();
        let email = self.account_session.as_ref().and_then(|s| s.email.clone());
        let tip = if signed_in {
            match &email {
                Some(email) => {
                    crate::localization::format("account.tooltip.signed_in_as", &[("email", email)])
                }
                None => crate::localization::text("account.tooltip.signed_in"),
            }
        } else {
            crate::localization::text("account.tooltip.signed_out")
        };
        let icon_color = if signed_in {
            Theme::success()
        } else {
            Theme::text_dim()
        };
        div()
            .id("qbar-account")
            .relative()
            .w(px(Theme::control_height()))
            .h(px(Theme::control_height()))
            .flex()
            .flex_shrink_0()
            .items_center()
            .justify_center()
            .rounded(px(Theme::scaled(4.0)))
            .cursor_pointer()
            .text_color(icon_color)
            .hover(|s| s.bg(Theme::border()))
            .child(ToolbarIcon(AppIcon::AccountUser).render(Theme::toolbar_icon_size()))
            // Signed-in status dot, tucked into the bottom-right of the glyph.
            .when(signed_in, |d| {
                d.child(
                    div()
                        .absolute()
                        .bottom(px(Theme::scaled(2.0)))
                        .right(px(Theme::scaled(2.0)))
                        .w(px(Theme::scaled(7.0)))
                        .h(px(Theme::scaled(7.0)))
                        .rounded_full()
                        .bg(Theme::success())
                        .border_1()
                        .border_color(Theme::sidebar_bg()),
                )
            })
            .app_tooltip(move |_window, cx| tooltip_view(tip.clone(), cx))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _ev, _window, cx| {
                    this.on_account_button_clicked(cx);
                }),
            )
    }

    /// The account / subscription details dialog. Only shown while signed in
    /// (the control opens the sign-in window otherwise), so it always leads with
    /// the signed-in identity, then the subscription, then account actions.
    /// Rendered on the unified dialog chrome.
    pub(super) fn render_account_dialog(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let s = Theme::scaled;
        let email = self
            .account_session
            .as_ref()
            .and_then(|se| se.email.clone());
        let device = self
            .account_session
            .as_ref()
            .and_then(|se| se.device_id.clone());
        let device_copied = self.account_device_id_copied;
        let body = div()
            .flex()
            .flex_col()
            .gap(px(s(14.0)))
            // Signed-in identity.
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(s(4.0)))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(s(6.0)))
                            .child(
                                div()
                                    .w(px(s(8.0)))
                                    .h(px(s(8.0)))
                                    .rounded_full()
                                    .bg(Theme::success()),
                            )
                            .child(
                                div()
                                    .text_size(px(Theme::font_size()))
                                    .font_weight(gpui::FontWeight::BOLD)
                                    .text_color(Theme::text())
                                    .child(crate::localization::text("account.identity.signed_in")),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(Theme::font_size()))
                            .text_color(Theme::text_dim())
                            .child(email.unwrap_or_else(|| {
                                crate::localization::text("account.identity.no_email")
                            })),
                    )
                    .when_some(device, |d, id| {
                        d.child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(s(6.0)))
                                .child(
                                    div()
                                        .text_size(px(Theme::font_size_ratio(0.85)))
                                        .text_color(Theme::text_dim())
                                        .child(crate::localization::format(
                                            "account.identity.device",
                                            &[("device_id", &id)],
                                        )),
                                )
                                .child(
                                    div()
                                        .id("account-device-copy")
                                        .flex()
                                        .flex_shrink_0()
                                        .items_center()
                                        .justify_center()
                                        .w(px(s(18.0)))
                                        .h(px(s(18.0)))
                                        .rounded(px(s(4.0)))
                                        .cursor_pointer()
                                        .text_color(Theme::text_dim())
                                        .hover(|st| st.text_color(Theme::text()).bg(Theme::bg()))
                                        .app_tooltip(|_window, cx| {
                                            tooltip_view(
                                                crate::localization::text(
                                                    "account.identity.copy_device_tooltip",
                                                ),
                                                cx,
                                            )
                                        })
                                        .child(
                                            ToolbarIcon(AppIcon::Copy)
                                                .render(Theme::font_size_ratio(0.9)),
                                        )
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(|this, _, _, cx| {
                                                this.copy_account_device_id(cx);
                                            }),
                                        ),
                                )
                                .when(device_copied, |row| {
                                    row.child(
                                        div()
                                            .text_size(px(Theme::font_size_ratio(0.8)))
                                            .text_color(Theme::success())
                                            .child(crate::localization::text(
                                                "account.identity.copied",
                                            )),
                                    )
                                }),
                        )
                    }),
            )
            // Divider.
            .child(div().h(px(1.0)).w_full().bg(Theme::border()))
            // Subscription section.
            .child(self.render_subscription_section(cx))
            // Policies and self-serve deletion (App Store guideline 5.1.1).
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap(px(s(12.0)))
                    .pt(px(s(4.0)))
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .child(account_link(
                        "account-privacy",
                        "account.links.privacy",
                        || crate::license::PRIVACY_URL.to_string(),
                    ))
                    .child(account_link("account-terms", "account.links.terms", || {
                        crate::license::TERMS_URL.to_string()
                    }))
                    .child(account_link(
                        "account-delete",
                        "account.links.delete",
                        crate::license::delete_account_url,
                    )),
            );

        crate::dialog::Dialog::new("account-dialog", |this: &mut Self| {
            this.dialog_frames.get("account-dialog", || {
                crate::dialog::DialogFrame::new_auto(Theme::scaled(460.0))
            })
        })
        .title(crate::localization::text("account.dialog.title"))
        .on_close(|this, _window, cx| this.dismiss_account_dialog(cx))
        .child(body)
        .leading_button(
            Button::new("account-manage")
                .label(crate::localization::text("account.actions.manage"))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.show_account_dialog = false;
                    crate::store_signin::open_store_signin_window(cx);
                    cx.notify();
                })),
        )
        .leading_button(
            Button::new("account-sign-out")
                .label(crate::localization::text("account.actions.sign_out"))
                .on_click(cx.listener(|this, _, _, cx| this.account_sign_out(cx))),
        )
        .build(self, window, cx)
    }

    /// The subscription block inside the account dialog, driven by
    /// [`AccountSubscriptionState`].
    pub(super) fn render_subscription_section(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let s = Theme::scaled;
        match &self.account_subscription {
            AccountSubscriptionState::Idle | AccountSubscriptionState::Loading => div()
                .text_size(px(Theme::font_size()))
                .text_color(Theme::text_dim())
                .child(crate::localization::text("account.subscription.loading"))
                .into_any_element(),
            AccountSubscriptionState::Failed(msg) => div()
                .flex()
                .flex_col()
                .gap(px(s(8.0)))
                .child(
                    div()
                        .text_size(px(Theme::font_size()))
                        .text_color(Theme::error())
                        .child(crate::localization::format(
                            "account.subscription.load_failed",
                            &[("error", msg)],
                        )),
                )
                .child(
                    div()
                        .id("account-subscription-retry")
                        .flex_shrink_0()
                        .px(px(s(12.0)))
                        .py(px(s(6.0)))
                        .rounded(px(s(6.0)))
                        .border_1()
                        .border_color(Theme::border())
                        .text_color(Theme::text())
                        .cursor_pointer()
                        .hover(|st| st.bg(Theme::bg()))
                        .child(crate::localization::text("account.subscription.retry"))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| {
                                this.load_account_subscription(cx);
                            }),
                        ),
                )
                .into_any_element(),
            AccountSubscriptionState::Loaded(None) => div()
                .flex()
                .flex_col()
                .gap(px(s(6.0)))
                .child(
                    div()
                        .text_size(px(Theme::font_size_ratio(1.1)))
                        .font_weight(gpui::FontWeight::BOLD)
                        .text_color(Theme::text())
                        .child(crate::localization::text("account.subscription.free_tier")),
                )
                .child(
                    div()
                        .text_size(px(Theme::font_size()))
                        .text_color(Theme::text_dim())
                        .child(crate::localization::text("account.subscription.no_active")),
                )
                .child(self.render_account_upgrade_button(cx))
                .into_any_element(),
            AccountSubscriptionState::Loaded(Some(subscription)) => {
                self.render_subscription_details(subscription)
            }
        }
    }

    /// Render an active subscription's plan, status, and renewal detail.
    pub(super) fn render_subscription_details(
        &self,
        subscription: &crate::store::Subscription,
    ) -> gpui::AnyElement {
        use chrono::Datelike;
        let s = Theme::scaled;
        // A far-future period end (year 9999) is the server's sentinel for "no
        // expiry" — never render it as a date.
        let far_future = subscription.current_period_end.year() >= 9999;
        let (status_label, status_color) = if subscription.has_access {
            (
                crate::localization::text("account.subscription.active"),
                Theme::success(),
            )
        } else if subscription.status.eq_ignore_ascii_case("PastDue") {
            (
                crate::localization::text("account.subscription.past_due"),
                Theme::error(),
            )
        } else {
            (
                crate::localization::text("account.subscription.inactive"),
                Theme::warning(),
            )
        };
        let mut col = div().flex().flex_col().gap(px(s(6.0))).child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(s(8.0)))
                .child(
                    div()
                        .text_size(px(Theme::font_size_ratio(1.1)))
                        .font_weight(gpui::FontWeight::BOLD)
                        .text_color(Theme::text())
                        .child(subscription.plan_name.clone()),
                )
                .child(
                    div()
                        .px(px(s(6.0)))
                        .py(px(s(1.0)))
                        .rounded(px(s(3.0)))
                        .bg(status_color)
                        .text_color(Theme::ink_on(status_color))
                        .text_size(px(Theme::font_size_ratio(0.8)))
                        .child(status_label),
                ),
        );
        if subscription.is_comped {
            col = col.child(
                div()
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text_dim())
                    .child(match &subscription.comp_reason {
                        Some(reason) if !reason.trim().is_empty() => crate::localization::format(
                            "account.subscription.comped_with_reason",
                            &[("reason", reason)],
                        ),
                        _ => crate::localization::text("account.subscription.comped"),
                    }),
            );
        } else if !far_future {
            let end = subscription
                .current_period_end
                .format("%b %-d, %Y")
                .to_string();
            let line = if subscription.will_renew {
                // The App Store edition shows no prices for purchases made
                // outside the App Store (guideline 3.1.1).
                match subscription
                    .next_renewal_amount
                    .filter(|_| crate::distribution::sells_through_web_store())
                {
                    Some(amount) => crate::localization::format(
                        "account.subscription.renews_with_price",
                        &[("date", &end), ("amount", &format!("${amount:.2}"))],
                    ),
                    None => crate::localization::format(
                        "account.subscription.renews",
                        &[("date", &end)],
                    ),
                }
            } else {
                crate::localization::format("account.subscription.access_until", &[("date", &end)])
            };
            col = col.child(
                div()
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text_dim())
                    .child(line),
            );
        }
        if let Some(note) = licensed_platforms_note(subscription.licensed_platforms.as_deref()) {
            col = col.child(
                div()
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text_dim())
                    .child(note),
            );
        }
        col.into_any_element()
    }

    /// The "View plans" call-to-action shown for free-tier accounts in the
    /// account dialog; opens the pricing page (or the account page if the
    /// entitlement has since gone paid).
    pub(super) fn render_account_upgrade_button(
        &self,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let s = Theme::scaled;
        let accent = Theme::accent();
        div()
            .id("account-upgrade")
            .flex_shrink_0()
            .mt(px(s(4.0)))
            .px(px(s(14.0)))
            .py(px(s(7.0)))
            .rounded(px(s(6.0)))
            .bg(accent)
            .text_color(Theme::ink_on(accent))
            .font_weight(gpui::FontWeight::BOLD)
            .cursor_pointer()
            .hover(|st| st.opacity(0.9))
            .child(if crate::distribution::sells_through_web_store() {
                crate::localization::text("account.actions.view_plans")
            } else {
                crate::localization::text("account.actions.unlock")
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.open_upgrade_surface(cx)),
            )
    }
}

/// A small text link in the account dialog that opens `url()` in the browser.
fn account_link(id: &'static str, label: &'static str, url: fn() -> String) -> impl IntoElement {
    div()
        .id(id)
        .cursor_pointer()
        .text_color(Theme::text_dim())
        .hover(|st| st.text_color(Theme::accent()))
        .child(crate::localization::text(label))
        .on_mouse_down(MouseButton::Left, move |_, _window, _cx| {
            let _ = crate::browser_open::open_in_browser(&url());
        })
}

/// "Licenses SQLLY on your Macs only." for a plan limited to macOS (the Mac
/// App Store purchase); a generic list otherwise; `None` for every platform.
/// Pure.
pub(super) fn licensed_platforms_note(platforms: Option<&[String]>) -> Option<String> {
    let platforms = platforms.filter(|p| !p.is_empty())?;
    if platforms.len() == 1 && platforms[0].eq_ignore_ascii_case("macos") {
        return Some(crate::localization::text("account.subscription.macos_only"));
    }
    let names: Vec<&str> = platforms
        .iter()
        .map(|p| match p.to_ascii_lowercase().as_str() {
            "macos" => "macOS",
            "windows" => "Windows",
            "linux" => "Linux",
            _ => p.as_str(),
        })
        .collect();
    Some(crate::localization::format(
        "account.subscription.platforms_only",
        &[("platforms", &names.join(", "))],
    ))
}

#[cfg(test)]
mod licensed_platforms_tests {
    use super::licensed_platforms_note;

    #[test]
    fn mac_only_plans_say_so() {
        let mac = vec!["macos".to_string()];
        assert_eq!(
            licensed_platforms_note(Some(&mac)).as_deref(),
            Some("Licenses SQLLY on your Macs only.")
        );
        let two = vec!["macos".to_string(), "linux".to_string()];
        assert_eq!(
            licensed_platforms_note(Some(&two)).as_deref(),
            Some("Licenses SQLLY on macOS, Linux only.")
        );
        assert_eq!(licensed_platforms_note(None), None);
        assert_eq!(licensed_platforms_note(Some(&[])), None);
    }
}
