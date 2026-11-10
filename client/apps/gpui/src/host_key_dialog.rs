//! SSH host-key trust prompt — shown when a tunnel reaches a server whose key
//! is not yet in `known_hosts`. First surface migrated onto the unified
//! [`crate::dialog::Dialog`] chrome: a movable, resizable confirm dialog.

#[cfg(not(target_arch = "wasm32"))]
use gpui::{
    EventEmitter, FocusHandle, Focusable, IntoElement, ParentElement, Render, Styled, Subscription,
    Window, div, px,
};
#[cfg(not(target_arch = "wasm32"))]
use sqlly_engines::ssh::known_hosts::HostKeyPrompt;
#[cfg(not(target_arch = "wasm32"))]
use tokio::sync::oneshot;

#[cfg(not(target_arch = "wasm32"))]
use crate::dialog::{Dialog, DialogFrame};
#[cfg(not(target_arch = "wasm32"))]
use crate::theme::Theme;

#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostKeyDialogEvent {
    /// User chose to trust the key and continue connecting.
    Trusted,
    /// User dismissed the prompt without trusting.
    Dismissed,
}

#[cfg(not(target_arch = "wasm32"))]
impl EventEmitter<HostKeyDialogEvent> for HostKeyDialog {}

#[cfg(not(target_arch = "wasm32"))]
pub struct HostKeyDialog {
    prompt: HostKeyPrompt,
    respond: Option<oneshot::Sender<bool>>,
    focus_handle: FocusHandle,
    focus_requested: bool,
    frame: DialogFrame,
    _subscriptions: Vec<Subscription>,
}

#[cfg(not(target_arch = "wasm32"))]
impl Focusable for HostKeyDialog {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl HostKeyDialog {
    pub fn new(
        prompt: HostKeyPrompt,
        respond: oneshot::Sender<bool>,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        Self {
            prompt,
            respond: Some(respond),
            focus_handle: cx.focus_handle(),
            focus_requested: false,
            frame: DialogFrame::new_auto(Theme::scaled(520.0)),
            _subscriptions: Vec::new(),
        }
    }

    fn finish(&mut self, trusted: bool, cx: &mut gpui::Context<Self>) {
        if let Some(tx) = self.respond.take() {
            let _ = tx.send(trusted);
        }
        cx.emit(if trusted {
            HostKeyDialogEvent::Trusted
        } else {
            HostKeyDialogEvent::Dismissed
        });
    }

    fn trust(&mut self, cx: &mut gpui::Context<Self>) {
        self.finish(true, cx);
    }

    fn cancel(&mut self, cx: &mut gpui::Context<Self>) {
        self.finish(false, cx);
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Render for HostKeyDialog {
    fn render(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        if !self.focus_requested {
            self.focus_requested = true;
            window.focus(&self.focus_handle, cx);
        }

        let host = self.prompt.host.clone();
        let port = self.prompt.port;
        let key_type = self.prompt.key_type.clone();
        let fingerprint = self.prompt.fingerprint_sha256.clone();
        let focus_handle = self.focus_handle.clone();

        Dialog::new("host-key", |this: &mut Self| &mut this.frame)
            .title(crate::localization::text("connections.host_key.title"))
            .confirm(
                crate::localization::text("connections.host_key.trust_button"),
                crate::localization::text("connections.host_key.cancel_button"),
            )
            .on_confirm(|this, _window, cx| this.trust(cx))
            .on_reject(|this, _window, cx| this.cancel(cx))
            .on_close(|this, _window, cx| this.cancel(cx))
            .track_focus(&focus_handle)
            .child(
                div()
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text())
                    .child(crate::localization::format(
                        "connections.host_key.authenticity_message",
                        &[("host", &host), ("port", &port.to_string())],
                    )),
            )
            .child(
                div()
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text_dim())
                    .font_family(Theme::editor_font_family())
                    .child(crate::localization::format(
                        "connections.host_key.fingerprint_message",
                        &[("key_type", &key_type), ("fingerprint", &fingerprint)],
                    )),
            )
            .build(self, window, cx)
    }
}
