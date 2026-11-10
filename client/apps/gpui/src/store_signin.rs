//! Sign-in window for the SQLLY store account — browser device pairing.
//!
//! The panel asks the store for a short pairing code (`XXXX-XXXX-XXXX`) as soon
//! as it opens (reusing a stored pairing when one exists), shows the code with a
//! "log in to activate" link that opens `/devices/register?code=…` in the
//! browser, and polls the store until the user finishes signing in on the web.
//! The user can pick any web sign-in option; no password touches the app. The
//! resulting session persists in the OS keychain.
//!
//! Opened via [`open_store_signin_window`] — from the free-tier banner's
//! "Sign in" link today; a menu entry can reuse the same opener.

use std::time::Duration;

use gpui::{
    App, Bounds, Context, IntoElement, Render, SharedString, Styled, TitlebarOptions, Window,
    WindowBounds, WindowOptions, div, prelude::*, px, size,
};
use gpui_component::button::{Button, ButtonVariants as _};

use crate::store::{self, MachineIdentity, PairingPoll, PairingRegistration, StoreSession};
use crate::theme::Theme;

/// How often the app asks the store whether the browser claim has landed.
const POLL_INTERVAL: Duration = Duration::from_secs(3);

/// What the panel is doing right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Requesting (or loading) a pairing code.
    Preparing,
    /// Showing the code and polling for the browser to claim it.
    AwaitingClaim,
    /// The device is claimed and the session is stored.
    SignedIn,
    /// Preparing the pairing failed; offer a retry.
    Failed,
}

pub struct StoreSignInPanel {
    /// The persisted session, if the app has signed in before (or just did).
    session: Option<StoreSession>,
    /// This machine's fingerprint identity. Gathered once at open.
    identity: Option<MachineIdentity>,
    /// The active pairing (code + secret) while awaiting a claim.
    pairing: Option<PairingRegistration>,
    phase: Phase,
    /// Bumped whenever we start a fresh pairing so a stale poll loop stops.
    generation: usize,
    /// User-presentable status line (error or progress detail).
    status: Option<(StatusTone, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusTone {
    Error,
    Success,
    Info,
}

impl StoreSignInPanel {
    /// Opens at once in a "checking" state. The stored session (a
    /// keychain / Credential Manager read) and the machine identity (registry
    /// or IOKit reads) are loaded on the background runtime, so none of it
    /// holds up the window's first paint.
    pub fn new(_window: &mut Window, cx: &mut Context<Self>) -> Self {
        let panel = Self {
            session: None,
            identity: None,
            pairing: None,
            phase: Phase::Preparing,
            generation: 0,
            status: Some((
                StatusTone::Info,
                crate::localization::text("store_signin.status.checking_account"),
            )),
        };

        // `generation` is 0 until the first `begin()`; a retry or sign-out
        // that starts one before this read answers wins, and the read is
        // dropped rather than overwriting the newer flow.
        let started_at = panel.generation;
        let mut acx = cx.to_async();
        let weak_self = cx.weak_entity();
        cx.spawn(async move |_, _| {
            let loaded = crate::tokio_handle()
                .spawn_blocking(|| {
                    let session = store::session::load().filter(|s| !s.is_expired());
                    let identity = store::machine_identity().ok();
                    (session, identity)
                })
                .await;
            let (session, identity) = loaded.unwrap_or((None, None));
            let _ = weak_self.update(&mut acx, |this, cx| {
                if this.identity.is_none() {
                    this.identity = identity;
                }
                if this.generation != started_at {
                    cx.notify();
                    return;
                }
                match session {
                    Some(session) => {
                        this.status = Some((
                            StatusTone::Info,
                            signed_in_message(session.email.as_deref()),
                        ));
                        this.session = Some(session);
                        this.phase = Phase::SignedIn;
                        cx.notify();
                    }
                    None => this.begin(cx),
                }
            });
        })
        .detach();
        panel
    }

    /// Loads or creates a pairing, then polls for the browser claim. Safe to
    /// call again (retry / switch account) — it supersedes any running loop.
    fn begin(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        self.pairing = None;
        self.phase = Phase::Preparing;
        self.status = Some((
            StatusTone::Info,
            crate::localization::text("store_signin.status.getting_pairing_code"),
        ));
        cx.notify();

        let mut acx = cx.to_async();
        let weak_self = cx.weak_entity();
        cx.spawn(async move |_, _| {
            // Stage 1: ensure a (non-expired) pairing exists.
            let ensured = crate::tokio_handle()
                .spawn(async move {
                    if let Some(existing) =
                        store::pairing::load().filter(|p| p.expires_at > chrono::Utc::now())
                    {
                        return Ok(existing);
                    }
                    let identity = store::machine_identity()?;
                    let pairing = store::create_pairing(&identity).await?;
                    let _ = store::pairing::save(&pairing);
                    Ok::<_, store::StoreError>(pairing)
                })
                .await;

            let pairing = match ensured {
                Ok(Ok(pairing)) => pairing,
                Ok(Err(e)) => {
                    let _ = weak_self.update(&mut acx, |this, cx| {
                        if this.generation == generation {
                            this.phase = Phase::Failed;
                            this.status = Some((
                                StatusTone::Error,
                                crate::localization::message(e.display_message()),
                            ));
                            cx.notify();
                        }
                    });
                    return;
                }
                Err(e) => {
                    let _ = weak_self.update(&mut acx, |this, cx| {
                        if this.generation == generation {
                            this.phase = Phase::Failed;
                            this.status = Some((
                                StatusTone::Error,
                                crate::localization::format(
                                    "store_signin.status.pairing_task_failed",
                                    &[("error", &e.to_string())],
                                ),
                            ));
                            cx.notify();
                        }
                    });
                    return;
                }
            };

            let published = weak_self
                .update(&mut acx, |this, cx| {
                    if this.generation != generation {
                        return false;
                    }
                    this.pairing = Some(pairing.clone());
                    this.phase = Phase::AwaitingClaim;
                    this.status = Some((
                        StatusTone::Info,
                        crate::localization::text("store_signin.status.awaiting_claim"),
                    ));
                    cx.notify();
                    true
                })
                .unwrap_or(false);
            if !published {
                return;
            }

            // Stage 2: poll until the browser claims the device.
            loop {
                let poll = crate::tokio_handle()
                    .spawn({
                        let pairing = pairing.clone();
                        async move { store::poll_pairing_token(&pairing).await }
                    })
                    .await;

                let stop = weak_self
                    .update(&mut acx, |this, cx| {
                        if this.generation != generation {
                            return true; // superseded — stop this loop
                        }
                        match poll {
                            Ok(Ok(PairingPoll::Ready(session))) => {
                                match store::session::save(&session) {
                                    Ok(()) => {
                                        // App Store edition: link an owned
                                        // unlock to the account just signed in.
                                        crate::app_store::sync_with_account_soon();
                                        this.status = Some((
                                            StatusTone::Success,
                                            signed_in_message(session.email.as_deref()),
                                        ));
                                    }
                                    Err(error) => {
                                        this.status = Some((
                                            StatusTone::Error,
                                            crate::localization::format(
                                                "store_signin.status.keychain_save_failed",
                                                &[("error", &error.to_string())],
                                            ),
                                        ));
                                    }
                                }
                                this.session = Some(session);
                                this.phase = Phase::SignedIn;
                                cx.notify();
                                true
                            }
                            Ok(Ok(PairingPoll::Pending)) => false,
                            Ok(Err(error)) => {
                                this.status = Some((
                                    StatusTone::Info,
                                    crate::localization::format(
                                        "store_signin.status.waiting_with_error",
                                        &[(
                                            "error",
                                            &crate::localization::message(error.display_message()),
                                        )],
                                    ),
                                ));
                                cx.notify();
                                false
                            }
                            Err(error) => {
                                this.status = Some((
                                    StatusTone::Error,
                                    crate::localization::format(
                                        "store_signin.status.poll_task_failed",
                                        &[("error", &error.to_string())],
                                    ),
                                ));
                                cx.notify();
                                true
                            }
                        }
                    })
                    .unwrap_or(true); // entity gone (window closed) — stop

                if stop {
                    break;
                }
                acx.background_executor().timer(POLL_INTERVAL).await;
            }
        })
        .detach();
    }

    /// Signs out and starts a fresh pairing so a different account can claim
    /// this device.
    fn sign_out(&mut self, cx: &mut Context<Self>) {
        store::session::clear();
        store::pairing::clear();
        self.session = None;
        self.begin(cx);
    }

    fn open_register_page(&self, code: &str) {
        let _ = crate::browser_open::open_in_browser(&register_url(&store::base_url(), code));
    }
}

fn signed_in_message(email: Option<&str>) -> String {
    let fallback;
    let email = match email {
        Some(email) => email,
        None => {
            fallback = crate::localization::text("store_signin.account_fallback");
            &fallback
        }
    };
    crate::localization::format("store_signin.status.signed_in", &[("email", email)])
}

/// The store's device-registration page for a pairing code. Pure so the URL
/// shape is testable without opening a browser.
fn register_url(base: &str, code: &str) -> String {
    format!(
        "{base}/devices/register?code={code}{}",
        crate::license::web_edition_query_suffix()
    )
}

impl Render for StoreSignInPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;

        let status_line = self.status.as_ref().map(|(tone, message)| {
            let color = match tone {
                StatusTone::Error => Theme::error(),
                StatusTone::Success => Theme::success(),
                StatusTone::Info => Theme::text_dim(),
            };
            div()
                .text_size(px(Theme::font_size()))
                .text_color(color)
                .child(message.clone())
        });

        let mut body = div()
            .flex()
            .flex_col()
            .size_full()
            .bg(Theme::bg())
            .text_color(Theme::text())
            .p(px(s(20.0)))
            .gap(px(s(12.0)));

        body = body.child(
            div()
                .text_size(px(Theme::font_size_ratio(1.2)))
                .font_weight(gpui::FontWeight::BOLD)
                .child(crate::localization::text("store_signin.dialog.heading")),
        );

        match (&self.session, &self.pairing, self.phase) {
            (Some(session), _, _) => {
                let device = self.identity.as_ref().map(|i| i.name.clone());
                let device = device
                    .unwrap_or_else(|| crate::localization::text("store_signin.device_fallback"));
                let email = session
                    .email
                    .clone()
                    .unwrap_or_else(|| crate::localization::text("store_signin.no_email"));
                let device_id = session
                    .device_id
                    .clone()
                    .unwrap_or_else(|| crate::localization::text("store_signin.paired_fallback"));
                body = body
                    .child(div().text_size(px(Theme::font_size())).child(
                        crate::localization::format(
                            "store_signin.account_row",
                            &[("email", &email)],
                        ),
                    ))
                    .child(
                        div()
                            .text_size(px(Theme::font_size()))
                            .text_color(Theme::text_dim())
                            .child(crate::localization::format(
                                "store_signin.device_row",
                                &[("device", &device), ("device_id", &device_id)],
                            )),
                    )
                    .child(
                        Button::new("store-sign-out")
                            .label(crate::localization::text(
                                "store_signin.actions.sign_out_switch",
                            ))
                            .outline()
                            .on_click(cx.listener(|this, _, _window, cx| this.sign_out(cx))),
                    );
            }
            (None, Some(pairing), Phase::AwaitingClaim) => {
                let code = pairing.code.clone();
                body = body
                    .child(
                        div()
                            .text_size(px(Theme::font_size()))
                            .text_color(Theme::text_dim())
                            .child(crate::localization::text("store_signin.pairing.code_label")),
                    )
                    .child(
                        div()
                            .text_size(px(Theme::font_size_ratio(2.3)))
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_color(Theme::accent())
                            .child(code.clone()),
                    )
                    .child(
                        Button::new("store-open-register")
                            .label(crate::localization::text("store_signin.actions.activate"))
                            .primary()
                            .on_click(cx.listener(move |this, _, _window, _cx| {
                                this.open_register_page(&code);
                            })),
                    )
                    .child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.85)))
                            .text_color(Theme::text_dim())
                            .child(crate::localization::text(
                                "store_signin.pairing.browser_instructions",
                            )),
                    );
            }
            (None, _, Phase::Failed) => {
                body = body.child(
                    Button::new("store-retry")
                        .label(crate::localization::text("store_signin.actions.retry"))
                        .primary()
                        .on_click(cx.listener(|this, _, _window, cx| this.begin(cx))),
                );
            }
            (None, _, _) => {
                body = body.child(
                    div()
                        .text_size(px(Theme::font_size()))
                        .text_color(Theme::text_dim())
                        .child(crate::localization::text(
                            "store_signin.status.getting_pairing_code",
                        )),
                );
            }
        }

        body.children(status_line)
    }
}

/// Opens the store sign-in window. Callable from an app-level action handler
/// (receives `&mut App`), same pattern as `about::open_about_window`.
pub fn open_store_signin_window(cx: &mut App) {
    let bounds = Bounds::centered(None, size(px(460.), px(420.)), cx);
    let _ = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some(SharedString::from(crate::localization::text(
                    "store_signin.dialog.window_title",
                ))),
                ..Default::default()
            }),
            app_id: Some(crate::about::APP_ID.into()),
            is_resizable: true,
            ..Default::default()
        },
        |window, cx| {
            let panel = cx.new(|cx| StoreSignInPanel::new(window, cx));
            cx.new(|cx| gpui_component::Root::new(panel, window, cx))
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_url_carries_base_and_code() {
        assert_eq!(
            register_url("https://store.sqlly.app", "AB12"),
            format!(
                "https://store.sqlly.app/devices/register?code=AB12{}",
                crate::license::web_edition_query_suffix()
            )
        );
    }
}
