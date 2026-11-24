//! The trust banner (why nothing runs yet, which connections still need
//! allowing) and the trust dialog that grants a notebook its connections.

use gpui::{
    AnyElement, Context, InteractiveElement as _, IntoElement, ParentElement as _, Styled as _,
    div, prelude::FluentBuilder as _, px,
};
use gpui_component::Disableable as _;
use gpui_component::Icon;
use gpui_component::checkbox::Checkbox;
use gpui_component::radio::Radio;
use sqlly_services::notebook_security::TrustLevel;

use super::text_button;
use crate::icon_catalog::AppIcon;
use crate::notebook::connection::environment_badge;
use crate::notebook::view::NotebookView;
use crate::theme::Theme;

fn t(key: &str) -> String {
    crate::localization::text(key)
}

fn small_text(text: String) -> gpui::Div {
    div()
        .text_size(px(Theme::font_size_ratio(0.85)))
        .text_color(Theme::text_dim())
        .child(text)
}

impl NotebookView {
    pub(in crate::notebook) fn render_trust_banner(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let live = self.live_trust();
        if !live.needs_banner() && self.trust_notice.is_none() {
            return None;
        }
        let (message, button) = if live.level == TrustLevel::Untrusted {
            (
                crate::localization::message(live.reason.message()),
                t("notebook.shell.trust.trust_button"),
            )
        } else if !live.pending_connections.is_empty() {
            let names: Vec<String> = live
                .pending_connections
                .iter()
                .map(|slot| self.slot_label(slot))
                .collect();
            (
                crate::localization::format(
                    "notebook.shell.trust.pending",
                    &[("connections", names.join(", "))],
                ),
                t("notebook.shell.trust.allow_button"),
            )
        } else if live.level == TrustLevel::Restricted {
            (
                t("notebook.shell.trust.read_only_banner"),
                t("notebook.shell.trust.change_button"),
            )
        } else {
            (
                self.trust_notice.clone().unwrap_or_default(),
                t("notebook.shell.trust.change_button"),
            )
        };
        let notice = self.trust_notice.clone().filter(|n| *n != message);
        let s = Theme::scaled;
        Some(
            div()
                .id("nb-trust-banner")
                .flex()
                .flex_row()
                .items_center()
                .gap(px(s(8.0)))
                .px(px(s(12.0)))
                .py(px(s(6.0)))
                .border_b_1()
                .border_color(if self.trust_flash {
                    Theme::error()
                } else {
                    Theme::warning()
                })
                .bg(Theme::panel_bg())
                .child(
                    Icon::empty()
                        .path(AppIcon::Lock.path())
                        .text_color(Theme::warning()),
                )
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .gap(px(s(2.0)))
                        .text_size(px(Theme::font_size_ratio(0.9)))
                        .text_color(Theme::text())
                        .child(message)
                        .when_some(notice, |d, notice| {
                            d.child(
                                div()
                                    .text_size(px(Theme::font_size_ratio(0.85)))
                                    .text_color(Theme::error())
                                    .child(notice),
                            )
                        })
                        .when_some(self.trust_problem.clone(), |d, problem| {
                            d.child(small_text(problem))
                        }),
                )
                .child(
                    text_button("nb-trust", button)
                        .on_click(cx.listener(|this, _, _, cx| this.trust_notebook(cx))),
                )
                .into_any_element(),
        )
    }

    /// Show what the file scan flagged when the notebook opened, and note
    /// it in the activity panel.
    pub fn show_scan_warnings(&mut self, warnings: Vec<String>, cx: &mut Context<Self>) {
        for warning in &warnings {
            self.log(
                None,
                crate::localization::format(
                    "notebook.shell.scan.activity",
                    &[("warning", warning.as_str())],
                ),
                false,
                None,
            );
        }
        self.scan_warnings = warnings;
        cx.notify();
    }

    pub(in crate::notebook) fn render_scan_banner(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if self.scan_warnings.is_empty() {
            return None;
        }
        let s = Theme::scaled;
        Some(
            div()
                .id("nb-scan-banner")
                .flex()
                .flex_row()
                .items_start()
                .gap(px(s(8.0)))
                .px(px(s(12.0)))
                .py(px(s(6.0)))
                .border_b_1()
                .border_color(Theme::warning())
                .bg(Theme::panel_bg())
                .child(
                    Icon::empty()
                        .path(AppIcon::Security.path())
                        .text_color(Theme::warning()),
                )
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .gap(px(s(2.0)))
                        .text_size(px(Theme::font_size_ratio(0.9)))
                        .text_color(Theme::text())
                        .child(t("notebook.shell.scan.title"))
                        .children(self.scan_warnings.iter().map(|w| small_text(w.clone()))),
                )
                .child(
                    text_button("nb-scan-dismiss", t("notebook.shell.scan.dismiss")).on_click(
                        cx.listener(|this, _, _, cx| {
                            this.scan_warnings.clear();
                            cx.notify();
                        }),
                    ),
                )
                .into_any_element(),
        )
    }

    pub(in crate::notebook) fn render_grant_dialog(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let draft = self.grant.clone()?;
        let s = Theme::scaled;
        let this = cx.entity();
        let slots = self.grant_slots();
        let mut rows = div().flex().flex_col().gap(px(s(8.0)));
        if slots.is_empty() {
            rows = rows.child(small_text(t("notebook.shell.trust.no_connections")));
        }
        for (n, slot) in slots.into_iter().enumerate() {
            let mapped = self.slot_choice(&slot);
            let target = match &mapped {
                Some(choice) => crate::localization::format(
                    "notebook.shell.trust.mapped_to",
                    &[("connection_name", choice.label.as_str())],
                ),
                None => t("notebook.shell.trust.not_mapped"),
            };
            let this = this.clone();
            let toggled = slot.clone();
            rows = rows.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(s(2.0)))
                    .child(
                        Checkbox::new(("nb-grant-slot", n))
                            .checked(draft.connections.contains(&slot))
                            .label(self.slot_label(&slot))
                            .on_click(move |checked: &bool, _, app| {
                                let checked = *checked;
                                this.update(app, |v, cx| {
                                    v.set_grant_connection(&toggled, checked, cx)
                                });
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(s(6.0)))
                            .pl(px(s(24.0)))
                            .child(small_text(target))
                            .when_some(mapped, |d, choice| {
                                d.child(environment_badge(choice.environment))
                            }),
                    ),
            );
        }
        let read_only = draft.level == TrustLevel::Restricted;
        let level_radio = |id: &'static str, level: TrustLevel, label: &str| {
            let this = this.clone();
            Radio::new(id)
                .checked(draft.level == level)
                .label(t(label))
                .on_click(move |_: &bool, _, app| {
                    this.update(app, |v, cx| v.set_grant_level(level, cx));
                })
        };
        let writes_this = this.clone();
        Some(
            div()
                .id("nb-grant-scrim")
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(Theme::scrim())
                .child(
                    div()
                        .id("nb-grant")
                        .w(px(s(480.0)))
                        .flex()
                        .flex_col()
                        .gap(px(s(10.0)))
                        .p(px(s(16.0)))
                        .rounded(px(s(6.0)))
                        .border_1()
                        .border_color(Theme::popup_border())
                        .bg(Theme::popup_bg())
                        .child(
                            div()
                                .text_color(Theme::text())
                                .child(t("notebook.shell.trust.dialog_title")),
                        )
                        .child(small_text(t("notebook.shell.trust.dialog_intro")))
                        .child(
                            div()
                                .text_size(px(Theme::font_size_ratio(0.9)))
                                .text_color(Theme::text())
                                .child(t("notebook.shell.trust.connections_heading")),
                        )
                        .child(rows)
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(s(4.0)))
                                .child(level_radio(
                                    "nb-grant-trusted",
                                    TrustLevel::Trusted,
                                    "notebook.shell.trust.level_trusted",
                                ))
                                .child(level_radio(
                                    "nb-grant-read-only",
                                    TrustLevel::Restricted,
                                    "notebook.shell.trust.level_read_only",
                                )),
                        )
                        .child(
                            Checkbox::new("nb-grant-allow-writes")
                                .checked(draft.allow_writes && !read_only)
                                .disabled(read_only)
                                .label(t("notebook.shell.trust.allow_writes"))
                                .on_click(move |checked: &bool, _, app| {
                                    let checked = *checked;
                                    writes_this
                                        .update(app, |v, cx| v.set_grant_allow_writes(checked, cx));
                                }),
                        )
                        .when_some(draft.error, |d, error| {
                            d.child(
                                div()
                                    .text_size(px(Theme::font_size_ratio(0.9)))
                                    .text_color(Theme::error())
                                    .child(error),
                            )
                        })
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .justify_end()
                                .gap(px(s(8.0)))
                                .child(
                                    text_button(
                                        "nb-grant-cancel",
                                        t("notebook.shell.trust.cancel_button"),
                                    )
                                    .on_click(cx.listener(|this, _, _, cx| this.cancel_grant(cx))),
                                )
                                .child(
                                    text_button(
                                        "nb-grant-confirm",
                                        t("notebook.shell.trust.confirm_button"),
                                    )
                                    .on_click(cx.listener(|this, _, _, cx| this.confirm_grant(cx))),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }
}
