//! SSH keyboard-interactive prompt — shown when a tunnel's server accepts
//! the key or password only as a first step and then asks for more (a
//! one-time code, a token PIN). One masked or plain field per question the
//! server asked; the answers go straight back to the tunnel and are never
//! kept here once the dialog closes.
#![cfg(not(target_arch = "wasm32"))]

use gpui::{
    AppContext as _, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, ParentElement,
    Render, Styled, Subscription, Window, div, px,
};
use gpui_component::input::{Input, InputEvent, InputState};
use sqlly_engines::ssh::SshChallengePrompt;
use tokio::sync::oneshot;

use crate::dialog::{Dialog, DialogFrame};
use crate::theme::Theme;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SshChallengeDialogEvent {
    /// The answers were sent to the server.
    Answered,
    /// The user cancelled; the connect fails.
    Cancelled,
}

impl EventEmitter<SshChallengeDialogEvent> for SshChallengeDialog {}

pub struct SshChallengeDialog {
    prompt: SshChallengePrompt,
    inputs: Vec<Entity<InputState>>,
    respond: Option<oneshot::Sender<Option<Vec<String>>>>,
    focus_handle: FocusHandle,
    focus_requested: bool,
    frame: DialogFrame,
    _subscriptions: Vec<Subscription>,
}

impl Focusable for SshChallengeDialog {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl SshChallengeDialog {
    pub fn new(
        prompt: SshChallengePrompt,
        respond: oneshot::Sender<Option<Vec<String>>>,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        let inputs: Vec<Entity<InputState>> = prompt
            .fields
            .iter()
            .map(|field| cx.new(|cx| InputState::new(window, cx).masked(!field.echo)))
            .collect();
        let subscriptions = inputs
            .iter()
            .map(|input| {
                cx.subscribe_in(
                    input,
                    window,
                    |this: &mut Self, _input, event: &InputEvent, _window, cx| {
                        if let InputEvent::PressEnter { .. } = event {
                            this.answer(cx);
                        }
                    },
                )
            })
            .collect();
        Self {
            prompt,
            inputs,
            respond: Some(respond),
            focus_handle: cx.focus_handle(),
            focus_requested: false,
            frame: DialogFrame::new_auto(Theme::scaled(480.0)),
            _subscriptions: subscriptions,
        }
    }

    /// Hands the answers to the waiting tunnel, in the server's order.
    fn answer(&mut self, cx: &mut gpui::Context<Self>) {
        let answers = self
            .inputs
            .iter()
            .map(|input| input.read(cx).value().to_string())
            .collect();
        self.finish(Some(answers), cx);
    }

    fn cancel(&mut self, cx: &mut gpui::Context<Self>) {
        self.finish(None, cx);
    }

    fn finish(&mut self, answers: Option<Vec<String>>, cx: &mut gpui::Context<Self>) {
        let answered = answers.is_some();
        if let Some(tx) = self.respond.take() {
            let _ = tx.send(answers);
        }
        cx.emit(if answered {
            SshChallengeDialogEvent::Answered
        } else {
            SshChallengeDialogEvent::Cancelled
        });
    }
}

impl Render for SshChallengeDialog {
    fn render(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        if !self.focus_requested {
            self.focus_requested = true;
            match self.inputs.first() {
                Some(first) => first.update(cx, |state, cx| state.focus(window, cx)),
                None => window.focus(&self.focus_handle, cx),
            }
        }

        let focus_handle = self.focus_handle.clone();
        let header = crate::localization::format(
            "connections.ssh_challenge.server_message",
            &[
                ("username", &self.prompt.username),
                ("host", &self.prompt.host),
                ("port", &self.prompt.port.to_string()),
            ],
        );
        // The server's own title and instructions are shown as sent.
        let server_text: Vec<String> = [&self.prompt.name, &self.prompt.instructions]
            .into_iter()
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty())
            .collect();
        let fields = self
            .prompt
            .fields
            .iter()
            .zip(&self.inputs)
            .map(|(field, input)| {
                let control = if field.echo {
                    Input::new(input)
                } else {
                    Input::new(input).mask_toggle()
                };
                div()
                    .flex()
                    .flex_col()
                    .gap(px(Theme::scaled(4.0)))
                    .child(
                        div()
                            .text_size(px(Theme::font_size()))
                            .text_color(Theme::text_dim())
                            .child(field.prompt.trim().to_string()),
                    )
                    .child(control)
            })
            .collect::<Vec<_>>();

        Dialog::new("ssh-challenge", |this: &mut Self| &mut this.frame)
            .title(crate::localization::text("connections.ssh_challenge.title"))
            .confirm(
                crate::localization::text("connections.ssh_challenge.continue_button"),
                crate::localization::text("connections.ssh_challenge.cancel_button"),
            )
            .on_confirm(|this, _window, cx| this.answer(cx))
            .on_reject(|this, _window, cx| this.cancel(cx))
            .on_close(|this, _window, cx| this.cancel(cx))
            .track_focus(&focus_handle)
            .child(
                div()
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text())
                    .child(header),
            )
            .children_body(server_text.into_iter().map(|text| {
                div()
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text())
                    .child(text)
            }))
            .children_body(fields)
            .child(
                div()
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text(
                        "connections.ssh_challenge.not_saved_note",
                    )),
            )
            .build(self, window, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_engines::ssh::SshChallengeField;

    fn ensure_components_initialized(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            if cx.try_global::<gpui_component::Theme>().is_none() {
                gpui_component::init(cx);
            }
        });
    }

    fn prompt() -> SshChallengePrompt {
        SshChallengePrompt {
            host: "bastion.example.com".into(),
            port: 22,
            username: "deploy".into(),
            name: "Two-factor".into(),
            instructions: "Enter the code from your app".into(),
            fields: vec![
                SshChallengeField {
                    prompt: "Verification code: ".into(),
                    echo: false,
                },
                SshChallengeField {
                    prompt: "Token label: ".into(),
                    echo: true,
                },
            ],
        }
    }

    #[gpui::test]
    fn continue_sends_every_answer_in_order(cx: &mut gpui::TestAppContext) {
        ensure_components_initialized(cx);
        let (tx, mut rx) = oneshot::channel();
        let (view, cx) =
            cx.add_window_view(|window, cx| SshChallengeDialog::new(prompt(), tx, window, cx));
        view.update_in(cx, |dialog, window, cx| {
            assert_eq!(dialog.inputs.len(), 2, "one field per server question");
            dialog.inputs[0].update(cx, |s, cx| s.set_value("246810", window, cx));
            dialog.inputs[1].update(cx, |s, cx| s.set_value("yubikey", window, cx));
            dialog.answer(cx);
        });
        assert_eq!(
            rx.try_recv().unwrap(),
            Some(vec!["246810".to_string(), "yubikey".to_string()])
        );
    }

    #[gpui::test]
    fn cancel_sends_no_answers(cx: &mut gpui::TestAppContext) {
        ensure_components_initialized(cx);
        let (tx, mut rx) = oneshot::channel();
        let (view, cx) =
            cx.add_window_view(|window, cx| SshChallengeDialog::new(prompt(), tx, window, cx));
        view.update(cx, |dialog, cx| dialog.cancel(cx));
        assert_eq!(rx.try_recv().unwrap(), None);
    }
}
