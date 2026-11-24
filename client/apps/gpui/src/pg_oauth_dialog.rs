//! PostgreSQL 18 OAuth device-code prompt: while SQLLY waits for the user to
//! approve the sign-in in their browser, show the code to enter and the page
//! to enter it on. Closing the dialog cancels the sign-in.
#![cfg(not(target_arch = "wasm32"))]

use gpui::{
    ClipboardItem, EventEmitter, FocusHandle, Focusable, IntoElement, ParentElement, Render,
    Styled, Window, div, px,
};
use gpui_component::button::Button;
use tokio_util::sync::CancellationToken;

use crate::dialog::{Dialog, DialogFrame};
use crate::pg_oauth_token::DeviceChallenge;
use crate::theme::Theme;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PgOAuthDialogEvent {
    /// The user cancelled; the pending sign-in stops.
    Cancelled,
}

impl EventEmitter<PgOAuthDialogEvent> for PgOAuthDialog {}

pub struct PgOAuthDialog {
    connection_name: String,
    challenge: DeviceChallenge,
    cancel: CancellationToken,
    copied: bool,
    focus_handle: FocusHandle,
    focus_requested: bool,
    frame: DialogFrame,
}

impl Focusable for PgOAuthDialog {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl PgOAuthDialog {
    pub fn new(
        connection_name: String,
        challenge: DeviceChallenge,
        cancel: CancellationToken,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        Self {
            connection_name,
            challenge,
            cancel,
            copied: false,
            focus_handle: cx.focus_handle(),
            focus_requested: false,
            frame: DialogFrame::new_auto(Theme::scaled(500.0)),
        }
    }

    fn cancel(&mut self, cx: &mut gpui::Context<Self>) {
        self.cancel.cancel();
        cx.emit(PgOAuthDialogEvent::Cancelled);
    }

    fn open_browser(&mut self) {
        if let Err(error) = crate::browser_open::open_in_browser(&self.challenge.browser_url) {
            tracing::warn!(%error, "could not open the browser for PostgreSQL OAuth sign-in");
        }
    }

    fn copy_code(&mut self, cx: &mut gpui::Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(self.challenge.user_code.clone()));
        self.copied = true;
        cx.notify();
    }
}

impl Render for PgOAuthDialog {
    fn render(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        if !self.focus_requested {
            self.focus_requested = true;
            window.focus(&self.focus_handle, cx);
        }
        let s = Theme::scaled;
        let focus_handle = self.focus_handle.clone();
        Dialog::new("pg-oauth", |this: &mut Self| &mut this.frame)
            .title(crate::localization::text(
                "connections.pg_oauth.dialog_title",
            ))
            .confirm(
                crate::localization::text("connections.pg_oauth.open_browser_button"),
                crate::localization::text("connections.pg_oauth.cancel_button"),
            )
            .on_confirm(|this, _window, _cx| this.open_browser())
            .on_reject(|this, _window, cx| this.cancel(cx))
            .on_close(|this, _window, cx| this.cancel(cx))
            .track_focus(&focus_handle)
            .child(
                div()
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text())
                    .child(crate::localization::format(
                        "connections.pg_oauth.instructions",
                        &[
                            ("name", &self.connection_name),
                            ("issuer", &self.challenge.issuer),
                        ],
                    )),
            )
            .child(
                div()
                    .py(px(s(8.0)))
                    .text_size(px(Theme::font_size() * 1.8))
                    .text_color(Theme::text())
                    .font_family(Theme::editor_font_family())
                    .child(self.challenge.user_code.clone()),
            )
            .child(
                div()
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text_dim())
                    .font_family(Theme::editor_font_family())
                    .child(self.challenge.verification_uri.clone()),
            )
            .child(
                div()
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text("connections.pg_oauth.waiting")),
            )
            .leading_button(
                Button::new("pg-oauth-copy-code")
                    .label(if self.copied {
                        crate::localization::text("connections.pg_oauth.code_copied")
                    } else {
                        crate::localization::text("connections.pg_oauth.copy_code_button")
                    })
                    .on_click(cx.listener(|this, _, _window, cx| this.copy_code(cx))),
            )
            .build(self, window, cx)
    }
}
