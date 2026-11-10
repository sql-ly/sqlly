//! "Server firewall is blocking your IP" dialog — the opt-in prompt raised
//! when Azure SQL rejects a connection with error **40615**.
//!
//! The engine reports the block as a connection error naming the client IP the
//! gateway saw. Both remediations Azure suggests are awkward from a SQL client:
//! the portal is a context switch, and `sp_set_firewall_rule` runs on `master`
//! — a database the firewall is currently refusing to let us reach. So SQLLY
//! offers the ARM control-plane route instead (see
//! [`crate::azure_firewall_flow`]), and this dialog is where the user says yes
//! to it and names the rule.
//!
//! Nothing is sent to Azure until the user presses **Add Firewall Rule** — the
//! dialog opening is a question, never an action. The permission to make the
//! change is exactly the signed-in account's Azure RBAC, so a `403` is
//! reported as "you do not have this permission, ask someone who does" rather
//! than retried or worked around.

use gpui::{
    Entity, EventEmitter, FocusHandle, IntoElement, ParentElement, Render, Styled, Subscription,
    Window, div, prelude::*, px,
};
use gpui_component::Disableable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputEvent, InputState};

use crate::azure_firewall::validate_firewall_request;
use crate::dialog::{Dialog, DialogFrame};
use crate::theme::Theme;

/// What the parent must do on the dialog's behalf.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FirewallRuleDialogEvent {
    /// The user confirmed the rule. The host runs the ARM flow and reports
    /// back through [`FirewallRuleDialog::finish`].
    AddRequested {
        rule_name: String,
        start_ip: String,
        end_ip: String,
    },
    /// The dialog was dismissed; the host clears its reference to it.
    Dismissed,
}

impl EventEmitter<FirewallRuleDialogEvent> for FirewallRuleDialog {}

/// Where the dialog is in its one-shot lifecycle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Editable form. Add is live.
    Prompt,
    /// The ARM call is in flight; the form is locked so a second Add cannot
    /// race the first.
    Working,
    /// Terminal outcome — applied, refused, or failed. Only Close remains.
    Done { ok: bool, message: String },
    /// SQLLY can tell before asking that it cannot make the call at all (the
    /// profile has no Entra account bound, or that account was never
    /// registered on this machine). Explaining that is more use than a form
    /// whose only possible ending is an error.
    Unavailable { message: String },
}

pub struct FirewallRuleDialog {
    /// FQDN of the server that refused the connection.
    server: String,
    /// The signed-in Entra identity the ARM call will be made as, for the
    /// "using your account …" line. `None` when unknown.
    account_label: Option<String>,
    /// The client IP the server named in the 40615 message, if it named one.
    /// Kept apart from the editable field so the prose can say where the
    /// address came from.
    blocked_ip: Option<String>,
    rule_name: String,
    start_ip: String,
    end_ip: String,
    rule_input: Entity<InputState>,
    start_input: Entity<InputState>,
    end_input: Entity<InputState>,
    phase: Phase,
    /// Inline reason the current form contents cannot be submitted.
    validation: Option<String>,
    focus_handle: FocusHandle,
    focus_requested: bool,
    frame: DialogFrame,
    _subscriptions: Vec<Subscription>,
}

impl gpui::Focusable for FirewallRuleDialog {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl FirewallRuleDialog {
    pub fn new(
        server: String,
        account_label: Option<String>,
        blocked_ip: Option<String>,
        default_rule_name: String,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        let ip = blocked_ip.clone().unwrap_or_default();
        let rule_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("SQLLY-me-20260803")
                .default_value(default_rule_name.clone())
        });
        let start_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("203.0.113.5")
                .default_value(ip.clone())
        });
        let end_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("203.0.113.5")
                .default_value(ip.clone())
        });

        let mut subs = Vec::new();
        subs.push(cx.subscribe_in(
            &rule_input,
            window,
            |this: &mut Self, input, event: &InputEvent, _window, cx| {
                if let InputEvent::Change = event {
                    this.rule_name = input.read(cx).value().to_string();
                    this.validation = None;
                    cx.notify();
                }
            },
        ));
        subs.push(cx.subscribe_in(
            &start_input,
            window,
            |this: &mut Self, input, event: &InputEvent, _window, cx| {
                if let InputEvent::Change = event {
                    this.start_ip = input.read(cx).value().to_string();
                    this.validation = None;
                    cx.notify();
                }
            },
        ));
        subs.push(cx.subscribe_in(
            &end_input,
            window,
            |this: &mut Self, input, event: &InputEvent, _window, cx| {
                if let InputEvent::Change = event {
                    this.end_ip = input.read(cx).value().to_string();
                    this.validation = None;
                    cx.notify();
                }
            },
        ));

        Self {
            server,
            account_label,
            blocked_ip,
            rule_name: default_rule_name,
            start_ip: ip.clone(),
            end_ip: ip,
            rule_input,
            start_input,
            end_input,
            phase: Phase::Prompt,
            validation: None,
            focus_handle: cx.focus_handle(),
            focus_requested: false,
            frame: DialogFrame::new_auto(Theme::scaled(520.0)),
            _subscriptions: subs,
        }
    }

    /// Explain, without offering a form, why SQLLY cannot add the rule itself.
    pub fn set_unavailable(&mut self, message: impl Into<String>, cx: &mut gpui::Context<Self>) {
        self.phase = Phase::Unavailable {
            message: message.into(),
        };
        cx.notify();
    }

    /// Fill the address fields from a public-IP lookup. Only ever writes into
    /// *empty* fields: a value the user typed, or one the server itself named
    /// in the error, outranks anything an echo service says about us.
    pub fn set_looked_up_ip(
        &mut self,
        ip: String,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        if !matches!(self.phase, Phase::Prompt) {
            return;
        }
        if self.start_ip.trim().is_empty() {
            self.start_ip = ip.clone();
            self.start_input
                .update(cx, |s, cx| s.set_value(ip.clone(), window, cx));
        }
        if self.end_ip.trim().is_empty() {
            self.end_ip = ip.clone();
            self.end_input
                .update(cx, |s, cx| s.set_value(ip, window, cx));
        }
        cx.notify();
    }

    /// Lock the form while the ARM call is out.
    pub fn begin_working(&mut self, cx: &mut gpui::Context<Self>) {
        self.phase = Phase::Working;
        self.validation = None;
        cx.notify();
    }

    /// Land the outcome of the ARM call as the dialog's terminal state.
    pub fn finish(&mut self, ok: bool, message: impl Into<String>, cx: &mut gpui::Context<Self>) {
        self.phase = Phase::Done {
            ok,
            message: message.into(),
        };
        cx.notify();
    }

    fn submit(&mut self, cx: &mut gpui::Context<Self>) {
        if !matches!(self.phase, Phase::Prompt) {
            return;
        }
        let rule_name = self.rule_name.trim().to_string();
        let start_ip = self.start_ip.trim().to_string();
        let end_ip = self.end_ip.trim().to_string();
        if let Some(reason) = validate_firewall_request(&rule_name, &start_ip, &end_ip) {
            self.validation = Some(reason.to_string());
            cx.notify();
            return;
        }
        self.validation = None;
        cx.emit(FirewallRuleDialogEvent::AddRequested {
            rule_name,
            start_ip,
            end_ip,
        });
        cx.notify();
    }

    pub fn close(&mut self, cx: &mut gpui::Context<Self>) {
        cx.emit(FirewallRuleDialogEvent::Dismissed);
        cx.notify();
    }

    fn on_key_down(
        &mut self,
        event: &gpui::KeyDownEvent,
        _window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let m = &event.keystroke.modifiers;
        if m.control || m.alt || m.platform || m.function {
            return;
        }
        // Escape is handled by the dialog chrome via `on_close`, whose
        // handler keeps the old guard: never abandon a change that is already
        // in flight — the PUT would still land, with no window left to report
        // it in. Enter keeps its per-phase meaning here.
        if event.keystroke.key.as_str() == "enter" {
            match self.phase {
                Phase::Prompt => self.submit(cx),
                Phase::Done { .. } | Phase::Unavailable { .. } => self.close(cx),
                Phase::Working => {}
            }
        }
    }
}

/// The explanatory paragraph above the form.
///
/// Pure so the wording is testable: this text is the whole reason the user
/// trusts the dialog enough to press the button, and it has to name the
/// server, the address, and whose credentials the change will be made with.
pub fn prompt_body(server: &str, blocked_ip: Option<&str>, account: Option<&str>) -> String {
    let key = match (blocked_ip.is_some(), account.is_some()) {
        (true, true) => "connections.firewall.prompt_with_ip_and_account",
        (true, false) => "connections.firewall.prompt_with_ip",
        (false, true) => "connections.firewall.prompt_with_account",
        (false, false) => "connections.firewall.prompt_without_ip_or_account",
    };
    let mut arguments = vec![("server", server)];
    if let Some(ip) = blocked_ip {
        arguments.push(("client_ip", ip));
    }
    if let Some(account) = account {
        arguments.push(("account", account));
    }
    crate::localization::format(key, &arguments)
}

/// Success text for an applied rule, including Azure's own propagation caveat
/// — an immediate reconnect can still be refused, and a user who is not told
/// that reads the delay as the rule having failed.
pub fn applied_body(server: &str, rule_name: &str, start_ip: &str, end_ip: &str) -> String {
    let range = if start_ip == end_ip {
        start_ip.to_string()
    } else {
        format!("{start_ip}–{end_ip}")
    };
    crate::localization::format(
        "connections.firewall.applied_body",
        &[
            ("server", server),
            ("range", &range),
            ("rule_name", rule_name),
        ],
    )
}

impl Render for FirewallRuleDialog {
    fn render(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        // Take the keyboard on first paint so Escape and Enter work without a
        // click landing somewhere in the dialog first.
        if !self.focus_requested {
            self.focus_requested = true;
            window.focus(&self.focus_handle, cx);
        }

        let s = Theme::scaled;
        let gap = s(10.0);
        let phase = self.phase.clone();
        let busy = matches!(phase, Phase::Working);

        let subtitle = div()
            .text_color(Theme::text_dim())
            .text_size(px(Theme::font_size_ratio(0.80)))
            .child(self.server.clone());

        let field = |label: &str, control: gpui::AnyElement| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(s(8.0)))
                .child(
                    div()
                        .w(px(s(78.0)))
                        .flex_shrink_0()
                        .text_size(px(Theme::font_size_ratio(0.87)))
                        .text_color(Theme::text_dim())
                        .child(label.to_string()),
                )
                .child(div().flex_1().min_w(px(0.0)).child(control))
        };

        let body = match &phase {
            Phase::Prompt | Phase::Working => div()
                .flex()
                .flex_col()
                .gap(px(gap))
                .child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.87)))
                        .text_color(Theme::text())
                        .child(prompt_body(
                            &self.server,
                            self.blocked_ip.as_deref(),
                            self.account_label.as_deref(),
                        )),
                )
                .child(field(
                    &crate::localization::text("connections.firewall.rule_name_label"),
                    Input::new(&self.rule_input)
                        .disabled(busy)
                        .into_any_element(),
                ))
                .child(field(
                    &crate::localization::text("connections.firewall.start_ip_label"),
                    Input::new(&self.start_input)
                        .disabled(busy)
                        .into_any_element(),
                ))
                .child(field(
                    &crate::localization::text("connections.firewall.end_ip_label"),
                    Input::new(&self.end_input)
                        .disabled(busy)
                        .into_any_element(),
                ))
                .child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.80)))
                        .text_color(Theme::text_dim())
                        .child(crate::localization::text(
                            "connections.firewall.address_help",
                        )),
                )
                .into_any_element(),
            Phase::Done { ok, message } => div()
                .text_size(px(Theme::font_size_ratio(0.87)))
                .text_color(if *ok { Theme::text() } else { Theme::error() })
                .child(message.clone())
                .into_any_element(),
            Phase::Unavailable { message } => div()
                .text_size(px(Theme::font_size_ratio(0.87)))
                .text_color(Theme::text())
                .child(message.clone())
                .into_any_element(),
        };

        let status_line = match (&phase, &self.validation) {
            (Phase::Working, _) => Some(
                div()
                    .text_size(px(Theme::font_size_ratio(0.80)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text(
                        "connections.firewall.contacting_azure",
                    ))
                    .into_any_element(),
            ),
            (_, Some(reason)) => Some(
                div()
                    .text_size(px(Theme::font_size_ratio(0.80)))
                    .text_color(Theme::error())
                    .child(reason.clone())
                    .into_any_element(),
            ),
            _ => None,
        };

        let focus_handle = self.focus_handle.clone();
        let dialog = Dialog::new("firewall-rule-dialog", |this: &mut Self| &mut this.frame)
            .title(crate::localization::text(
                "connections.firewall.dialog_title",
            ))
            .key_context("FirewallRuleDialog")
            .track_focus(&focus_handle)
            .use_default_close(false)
            .dismiss_on_scrim_click(false)
            // The ✕, Escape, and Close all funnel here; never let them
            // abandon a PUT that is already on its way to Azure.
            .on_close(|this, _window, cx| {
                if !matches!(this.phase, Phase::Working) {
                    this.close(cx);
                }
            })
            .child(subtitle)
            .child(body)
            .children_body(status_line);
        let dialog = match phase {
            Phase::Prompt | Phase::Working => dialog
                .button(
                    Button::new("firewall-cancel")
                        .label(crate::localization::text(
                            "connections.firewall.cancel_button",
                        ))
                        .disabled(busy)
                        .on_click(cx.listener(|this, _, _window, cx| this.close(cx))),
                )
                .button(
                    Button::new("firewall-add")
                        .label(crate::localization::text("connections.firewall.add_button"))
                        .primary()
                        .disabled(busy)
                        .on_click(cx.listener(|this, _, _window, cx| this.submit(cx))),
                ),
            Phase::Done { .. } | Phase::Unavailable { .. } => dialog.button(
                Button::new("firewall-close")
                    .label(crate::localization::text(
                        "connections.firewall.close_button",
                    ))
                    .primary()
                    .on_click(cx.listener(|this, _, _window, cx| this.close(cx))),
            ),
        };
        dialog
            .build(self, window, cx)
            // Key events bubble from the focused card to this overlay, so
            // Enter keeps its per-phase behavior alongside the chrome's keys.
            .on_key_down(cx.listener(Self::on_key_down))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_body_names_server_ip_and_account() {
        let body = prompt_body(
            "srv1.database.windows.net",
            Some("203.0.113.7"),
            Some("support@sqlly.app"),
        );
        assert!(body.contains("srv1.database.windows.net"));
        assert!(body.contains("203.0.113.7"));
        assert!(body.contains("support@sqlly.app"));
        assert!(body.contains("40615"));
    }

    #[test]
    fn prompt_body_promises_nothing_happens_until_confirmed() {
        let body = prompt_body("srv", Some("1.2.3.4"), None);
        assert!(body.contains("Nothing is sent to Azure until you choose Add Firewall Rule"));
    }

    #[test]
    fn prompt_body_survives_a_message_with_no_ip() {
        let body = prompt_body("srv", None, None);
        assert!(!body.contains("()"));
        assert!(body.contains("not in its firewall allow-list"));
    }

    #[test]
    fn applied_body_collapses_a_single_address() {
        let body = applied_body("srv", "SQLLY-matt-20260803", "1.2.3.4", "1.2.3.4");
        assert!(body.contains("Address: 1.2.3.4"));
        assert!(!body.contains('–'));
        assert!(body.contains("five minutes"));
    }

    #[test]
    fn applied_body_shows_a_range() {
        let body = applied_body("srv", "office", "1.2.3.4", "1.2.3.9");
        assert!(body.contains("1.2.3.4–1.2.3.9"));
        assert!(body.contains("office"));
    }
}
