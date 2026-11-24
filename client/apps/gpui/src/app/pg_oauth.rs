//! PostgreSQL 18 OAuth (device-code) sign-in on the connect path: reuse a
//! fresh token, refresh a stale one quietly, or show the device code while
//! the user approves the sign-in in their browser.

use super::*;

impl AppView {
    /// Stamp a device-code profile with its OAuth access token, then run
    /// `then`. Called from [`Self::with_entra_sql_token`], the single choke
    /// point every connect surface goes through.
    pub(super) fn with_pg_oauth_token(
        &mut self,
        profile: ConnectionProfile,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, ConnectionProfile, &mut Context<Self>) + 'static,
    ) {
        if let Some(token) = crate::pg_oauth_token::fresh_access_token(&profile) {
            self.continue_with_pg_oauth_token(profile, token, cx, then);
            return;
        }
        self.set_status(
            StatusLevel::Busy,
            crate::localization::format("shell.pg_oauth.signing_in", &[("name", &profile.name)]),
        );
        cx.notify();

        let cancel = tokio_util::sync::CancellationToken::new();
        // Set once the sign-in ends, so a code that arrives late never opens
        // a dialog nobody will close.
        let done = tokio_util::sync::CancellationToken::new();
        let (challenge_tx, challenge_rx) = tokio::sync::oneshot::channel();
        let on_challenge: Box<dyn FnOnce(crate::pg_oauth_token::DeviceChallenge) + Send> = Box::new(
            move |challenge| {
                if let Err(error) = crate::browser_open::open_in_browser(&challenge.browser_url) {
                    tracing::warn!(%error, "could not open the browser for PostgreSQL OAuth sign-in");
                }
                let _ = challenge_tx.send(challenge);
            },
        );

        // Show the code once the provider hands it out (only when a new
        // browser sign-in is needed; refreshes never get here).
        let name = profile.name.clone();
        let dialog_cancel = cancel.clone();
        let dialog_done = done.clone();
        let app = cx.weak_entity();
        cx.spawn(async move |_, cx: &mut gpui::AsyncApp| {
            let Ok(challenge) = challenge_rx.await else {
                return;
            };
            if dialog_done.is_cancelled() {
                return;
            }
            let _ = app.update(cx, |app, cx| {
                app.open_pg_oauth_dialog(name, challenge, dialog_cancel, cx)
            });
        })
        .detach();

        let app = cx.weak_entity();
        cx.spawn(async move |_, cx: &mut gpui::AsyncApp| {
            let outcome =
                crate::pg_oauth_token::ensure_token(profile.clone(), Some(on_challenge), cancel)
                    .await;
            done.cancel();
            let _ = app.update(cx, |app, cx| {
                app.overlays.pg_oauth_dialog = None;
                match outcome {
                    Ok(token) => app.continue_with_pg_oauth_token(profile, token, cx, then),
                    Err(message) => {
                        app.set_status(StatusLevel::Error, crate::localization::message(message));
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    fn continue_with_pg_oauth_token(
        &mut self,
        mut profile: ConnectionProfile,
        token: String,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, ConnectionProfile, &mut Context<Self>) + 'static,
    ) {
        profile.runtime_cloud_iam_token = Some(token.clone());
        // The schema tree's lazy loads authenticate with the same token.
        let id = profile.id.0;
        self.sidebar.update(cx, |s, cx| {
            s.set_runtime_cloud_iam_token(id, token, cx);
        });
        then(self, profile, cx);
    }

    fn open_pg_oauth_dialog(
        &mut self,
        connection_name: String,
        challenge: crate::pg_oauth_token::DeviceChallenge,
        cancel: tokio_util::sync::CancellationToken,
        cx: &mut Context<Self>,
    ) {
        let dialog = cx.new(|cx| {
            crate::pg_oauth_dialog::PgOAuthDialog::new(connection_name, challenge, cancel, cx)
        });
        cx.subscribe(
            &dialog,
            |this: &mut Self, _view, _event: &crate::pg_oauth_dialog::PgOAuthDialogEvent, cx| {
                this.overlays.pg_oauth_dialog = None;
                cx.notify();
            },
        )
        .detach();
        self.overlays.pg_oauth_dialog = Some(dialog);
        cx.notify();
    }
}
