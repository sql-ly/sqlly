//! Bridges SSH host-key prompts and keyboard-interactive challenges (one-time
//! codes) from tokio tunnel tasks onto the GPUI main thread.

use std::sync::{Arc, Mutex, OnceLock, mpsc};

use gpui::{AppContext, Context, Entity, Window};
use sqlly_engines::ssh::SshChallengePrompt;
use sqlly_engines::ssh::known_hosts::{self, HostKeyPrompt};
use tokio::sync::oneshot;

use super::AppView;

/// A host-key review waiting for the UI to open a dialog.
pub(crate) struct PendingHostKeyPrompt {
    pub(crate) prompt: HostKeyPrompt,
    pub(crate) respond: oneshot::Sender<bool>,
}

static HOST_KEY_PROMPT_RX: OnceLock<Mutex<mpsc::Receiver<PendingHostKeyPrompt>>> = OnceLock::new();

/// Install the process-wide SSH host-key decider. Must run before any tunnel
/// can open (native startup, before `Application::run`).
pub(crate) fn install_ssh_host_key_decider() {
    let (tx, rx) = mpsc::channel();
    let _ = HOST_KEY_PROMPT_RX.set(Mutex::new(rx));
    known_hosts::install_host_key_decider(Arc::new(move |prompt| {
        let (respond_tx, respond_rx) = oneshot::channel();
        if tx
            .send(PendingHostKeyPrompt {
                prompt,
                respond: respond_tx,
            })
            .is_err()
        {
            return false;
        }
        respond_rx.blocking_recv().unwrap_or(false)
    }));
}

/// A keyboard-interactive round waiting for the UI to open a dialog.
pub(crate) struct PendingSshChallenge {
    pub(crate) prompt: SshChallengePrompt,
    pub(crate) respond: oneshot::Sender<Option<Vec<String>>>,
}

static SSH_CHALLENGE_RX: OnceLock<Mutex<mpsc::Receiver<PendingSshChallenge>>> = OnceLock::new();

/// Install the process-wide SSH challenge responder, next to the host-key
/// decider. The answers pass straight through to the tunnel task; nothing
/// here keeps them.
pub(crate) fn install_ssh_challenge_responder() {
    let (tx, rx) = mpsc::channel();
    let _ = SSH_CHALLENGE_RX.set(Mutex::new(rx));
    sqlly_engines::ssh::install_ssh_challenge_responder(Arc::new(move |prompt| {
        let (respond_tx, respond_rx) = oneshot::channel();
        if tx
            .send(PendingSshChallenge {
                prompt,
                respond: respond_tx,
            })
            .is_err()
        {
            return None;
        }
        respond_rx.blocking_recv().unwrap_or(None)
    }));
}

fn take_pending_ssh_challenge() -> Option<PendingSshChallenge> {
    let rx = SSH_CHALLENGE_RX.get()?.lock().ok()?;
    rx.try_recv().ok()
}

/// Drain one queued prompt, if any. Called from `AppView::render` on the UI
/// thread.
fn take_pending_host_key_prompt() -> Option<PendingHostKeyPrompt> {
    let rx = HOST_KEY_PROMPT_RX.get()?.lock().ok()?;
    rx.try_recv().ok()
}

impl AppView {
    /// Turn a queued host-key review into a modal. Called from `render` — the
    /// first point after detection with a `&mut Window`.
    pub(super) fn materialize_host_key_prompt(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.overlays.host_key_dialog.is_some() {
            return;
        }
        let Some(pending) = take_pending_host_key_prompt() else {
            return;
        };
        let dialog = cx.new(|cx| {
            crate::host_key_dialog::HostKeyDialog::new(pending.prompt, pending.respond, cx)
        });
        cx.subscribe(&dialog, Self::handle_host_key_dialog_event)
            .detach();
        self.overlays.host_key_dialog = Some(dialog);
        cx.notify();
    }

    /// Turn a queued keyboard-interactive round into a modal. Waits while a
    /// host-key review is open: that review comes first in the sign-in.
    pub(super) fn materialize_ssh_challenge_prompt(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.overlays.ssh_challenge_dialog.is_some() || self.overlays.host_key_dialog.is_some() {
            return;
        }
        let Some(pending) = take_pending_ssh_challenge() else {
            return;
        };
        let dialog = cx.new(|cx| {
            crate::ssh_challenge_dialog::SshChallengeDialog::new(
                pending.prompt,
                pending.respond,
                window,
                cx,
            )
        });
        cx.subscribe(
            &dialog,
            |this: &mut Self,
             _view,
             _event: &crate::ssh_challenge_dialog::SshChallengeDialogEvent,
             cx| {
                this.overlays.ssh_challenge_dialog = None;
                cx.notify();
            },
        )
        .detach();
        self.overlays.ssh_challenge_dialog = Some(dialog);
        cx.notify();
    }

    pub(super) fn handle_host_key_dialog_event(
        &mut self,
        _view: Entity<crate::host_key_dialog::HostKeyDialog>,
        event: &crate::host_key_dialog::HostKeyDialogEvent,
        cx: &mut Context<Self>,
    ) {
        use crate::host_key_dialog::HostKeyDialogEvent;
        match event {
            HostKeyDialogEvent::Trusted | HostKeyDialogEvent::Dismissed => {
                self.overlays.host_key_dialog = None;
                cx.notify();
            }
        }
    }
}
