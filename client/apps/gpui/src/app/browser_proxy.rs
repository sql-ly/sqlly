//! Relay browser-proxy mode (wasm only): the app runs against the servers
//! and databases a `sqlly-relay --proxy-credentials …` or a SQLly Embedded
//! bridge serves, instead of the in-tab samples.
//!
//! The host page hands the proxy URL to [`crate::configure_browser_proxy`]
//! before the UI starts. On the first window the app opens the proxy
//! socket ([`sqlly_services::browser::proxy::connect`]), replaces the
//! connection store with the proxy's read-only listing, and installs the
//! remote-backed services for every proxied profile in the
//! [`ServiceRouter`](crate::service_router::ServiceRouter). From then on
//! every engine-backed feature runs through the proxy exactly like a
//! desktop Via-Relay session, and every add/edit/delete/import affordance
//! is disabled (see [`crate::connection::connections_are_proxied`]).
//!
//! A session that drops (the bridge restarted, the network blipped, the
//! engine stream broke) is reconnected with the same launch token on a short
//! backoff; the token stays valid for a couple of minutes after its session
//! closes, which is what makes this possible. A refusal (token expired,
//! seat or session limit, license not ok) is final and is shown verbatim:
//! only relaunching from the host page can change it.

use std::sync::Mutex;
use std::time::Duration;

use gpui::Context;
use sqlly_services::browser::proxy::{ConnectError, ProxySession};

use super::{AppView, StatusLevel};
use crate::connection::ConnectionStore;

struct ProxyConfig {
    url: String,
    token: Option<String>,
}

static CONFIG: Mutex<Option<ProxyConfig>> = Mutex::new(None);

/// Pauses before each reconnect attempt after a session is lost (or could
/// not be opened at all). About half a minute in total: long enough for a
/// bridge restart, short enough that the user is not left staring.
const RECONNECT_DELAYS_SECS: [u64; 5] = [1, 2, 4, 8, 15];

/// Record the proxy the host page asked for. `None` keeps the standalone
/// preview (sample connections).
pub fn configure(url: Option<String>, token: Option<String>) {
    sqlly_services::browser::set_served(url.is_some());
    *CONFIG.lock().expect("proxy config poisoned") = url.map(|url| ProxyConfig { url, token });
}

/// Whether a relay proxy was configured for this page load.
pub fn configured() -> bool {
    CONFIG.lock().expect("proxy config poisoned").is_some()
}

fn socket_url() -> Option<String> {
    let config = CONFIG.lock().expect("proxy config poisoned");
    config
        .as_ref()
        .map(|c| sqlly_services::browser::proxy::socket_url(&c.url, c.token.as_deref()))
}

impl AppView {
    /// The status line and one toast for a bridge whose license needs
    /// attention. In grace the bridge still serves, so it is a warning with
    /// the renewal countdown; otherwise every query will fail with this same
    /// message until the license recovers.
    fn show_proxy_license_trouble(
        &mut self,
        relay: &str,
        state: &sqlly_core::credential_proxy::ProxyLicense,
        cx: &mut Context<Self>,
    ) {
        use gpui_component::notification::Notification;
        let message = crate::localization::message(state.user_display_message());
        if state.serves() {
            self.set_status(
                StatusLevel::Warning,
                crate::localization::format(
                    "shell.browser_proxy.license_warning",
                    &[("relay", relay), ("message", &message)],
                ),
            );
            Self::toast_update_outcome(Notification::warning(message), cx);
        } else {
            self.set_status(
                StatusLevel::Error,
                crate::localization::format(
                    "shell.browser_proxy.license_paused",
                    &[("relay", relay), ("message", &message)],
                ),
            );
            Self::toast_update_outcome(Notification::error(message), cx);
        }
    }

    /// Open the configured proxy session in the background; a no-op when no
    /// proxy was configured.
    pub(super) fn start_browser_proxy(&mut self, cx: &mut Context<Self>) {
        let Some(url) = socket_url() else {
            return;
        };
        self.set_status(
            StatusLevel::Info,
            crate::localization::text("shell.browser_proxy.connecting"),
        );
        cx.spawn(async move |this, cx| {
            let result = sqlly_services::browser::proxy::connect(&url).await;
            let _ = this.update(cx, |app, cx| match result {
                Ok(session) => app.install_browser_proxy_session(session, false, cx),
                Err(error @ ConnectError::Refused { .. }) => {
                    app.report_proxy_refusal(&error, cx);
                }
                Err(ConnectError::Unavailable(message)) => {
                    app.retry_browser_proxy(message, cx);
                }
            });
        })
        .detach();
    }

    /// A refusal is final: the token, seat or license has to change on the
    /// host side, so say exactly what the proxy said and how to recover.
    fn report_proxy_refusal(&mut self, error: &ConnectError, cx: &mut Context<Self>) {
        use gpui_component::notification::Notification;
        tracing::error!(%error, "relay proxy refused the session");
        let message = crate::localization::message(error.display_message());
        self.service_router.clear_proxy();
        self.set_status(
            StatusLevel::Error,
            crate::localization::format(
                "shell.browser_proxy.refusal_status",
                &[("message", &message)],
            ),
        );
        Self::toast_update_outcome(Notification::error(message), cx);
        cx.notify();
    }

    /// Keep trying to open the proxy session on a short backoff, then give
    /// up with the last reason. Used at start when the proxy could not be
    /// reached and after an established session dropped.
    fn retry_browser_proxy(&mut self, reason: String, cx: &mut Context<Self>) {
        let Some(url) = socket_url() else {
            return;
        };
        self.set_status(
            StatusLevel::Warning,
            crate::localization::format("shell.browser_proxy.reconnecting", &[("reason", &reason)]),
        );
        cx.notify();
        cx.spawn(async move |this, cx| {
            let mut last = reason;
            for (attempt, delay) in RECONNECT_DELAYS_SECS.into_iter().enumerate() {
                cx.background_executor()
                    .timer(Duration::from_secs(delay))
                    .await;
                match sqlly_services::browser::proxy::connect(&url).await {
                    Ok(session) => {
                        let _ = this.update(cx, |app, cx| {
                            app.install_browser_proxy_session(session, true, cx);
                        });
                        return;
                    }
                    Err(error @ ConnectError::Refused { .. }) => {
                        let _ = this.update(cx, |app, cx| app.report_proxy_refusal(&error, cx));
                        return;
                    }
                    Err(ConnectError::Unavailable(message)) => {
                        tracing::warn!(attempt = attempt + 1, %message, "relay proxy reconnect failed");
                        last = message;
                        let left = RECONNECT_DELAYS_SECS.len() - attempt - 1;
                        let _ = this.update(cx, |app, cx| {
                            app.set_status(
                                StatusLevel::Warning,
                                crate::localization::format(
                                    if left == 1 {
                                        "shell.browser_proxy.still_unreachable_singular"
                                    } else {
                                        "shell.browser_proxy.still_unreachable_plural"
                                    },
                                    &[("reason", &last), ("count", &left.to_string())],
                                ),
                            );
                            cx.notify();
                        });
                    }
                }
            }
            let _ = this.update(cx, |app, cx| {
                app.set_status(
                    StatusLevel::Error,
                    crate::localization::format(
                        "shell.browser_proxy.reconnect_failed",
                        &[("reason", &last)],
                    ),
                );
                cx.notify();
            });
        })
        .detach();
    }

    /// An established session closed: stop routing to it and try to get it
    /// back. The reason the proxy gave (its last `error` message, or the
    /// close frame's) is shown so the user learns why queries just failed.
    fn on_browser_proxy_lost(&mut self, reason: Option<String>, cx: &mut Context<Self>) {
        self.service_router.clear_proxy();
        let reason = reason
            .unwrap_or_else(|| crate::localization::text("shell.browser_proxy.session_closed"));
        tracing::warn!(%reason, "relay proxy session lost");
        self.retry_browser_proxy(reason, cx);
    }

    fn install_browser_proxy_session(
        &mut self,
        mut session: ProxySession,
        reconnected: bool,
        cx: &mut Context<Self>,
    ) {
        let count = session.catalog.profiles.len();
        ConnectionStore::global().replace_with_proxied(&session.catalog);
        self.service_router.install_proxy(session.services);
        self.sidebar.update(cx, |s, cx| s.refresh_connections(cx));
        let relay = session.relay.clone();
        let connected = crate::localization::format(
            match (reconnected, count) {
                (true, 1) => "shell.browser_proxy.reconnected_singular",
                (true, _) => "shell.browser_proxy.reconnected_plural",
                (false, 1) => "shell.browser_proxy.connected_singular",
                (false, _) => "shell.browser_proxy.connected_plural",
            },
            &[("relay", &relay), ("count", &count.to_string())],
        );
        let mut license = session.license;
        match license.borrow_and_update().clone() {
            Some(state) if !state.is_ok() => {
                self.show_proxy_license_trouble(&relay, &state, cx);
            }
            _ => self.set_status(
                if reconnected {
                    StatusLevel::Success
                } else {
                    StatusLevel::Info
                },
                connected.clone(),
            ),
        }
        cx.notify();

        // Launch context (SQLly Embedded): the tabs the host app asked for.
        // Only the first session of a launch token carries any, so a
        // reconnect never reopens them.
        if !session.open.is_empty() {
            self.open_launch_tabs(std::mem::take(&mut session.open), cx);
        }

        // Follow the embedded license: trouble is announced the moment the
        // bridge pushes it, and recovery restores the normal status line.
        cx.spawn(async move |this, cx| {
            while license.changed().await.is_ok() {
                let state = license.borrow_and_update().clone();
                let relay = relay.clone();
                let connected = connected.clone();
                let _ = this.update(cx, |app, cx| {
                    match state {
                        Some(state) if !state.is_ok() => {
                            app.show_proxy_license_trouble(&relay, &state, cx);
                        }
                        _ => app.set_status(
                            StatusLevel::Success,
                            crate::localization::format(
                                "shell.browser_proxy.license_confirmed",
                                &[("message", &connected)],
                            ),
                        ),
                    }
                    cx.notify();
                });
            }
        })
        .detach();

        // Watch for the socket dropping so the user learns why every query
        // suddenly fails, no stale services stay installed, and a reconnect
        // is attempted.
        let mut closed = session.closed;
        let close_reason = session.close_reason;
        cx.spawn(async move |this, cx| {
            loop {
                if *closed.borrow() {
                    break;
                }
                if closed.changed().await.is_err() {
                    break;
                }
            }
            let reason = close_reason.lock().ok().and_then(|r| r.clone());
            let _ = this.update(cx, |app, cx| app.on_browser_proxy_lost(reason, cx));
        })
        .detach();
    }

    /// Open the launch context's tabs: each loads its script with the
    /// host's parameter values in the strip, takes its title and caret, and
    /// is pinned to its connection and database. Focusing the first one
    /// then connects to it through the normal tab-switch path (so a server
    /// that fails to connect still leaves every tab open, SQL intact).
    /// Nothing runs: the user reviews each query and presses Run.
    fn open_launch_tabs(
        &mut self,
        opens: Vec<sqlly_core::credential_proxy::LaunchOpen>,
        cx: &mut Context<Self>,
    ) {
        use gpui_component::notification::Notification;
        let first = self.tabs.len();
        let single_connection = match opens.as_slice() {
            [only] => only.connection.clone(),
            _ => None,
        };
        let count = opens.len();
        for open in opens {
            let tab = crate::launch_tabs::launch_tab(&open);
            self.new_tab(cx, Some(tab.text));
            let index = self.active_tab;
            if let Some(query_tab) = self.tabs.get_mut(index) {
                query_tab.query_params =
                    crate::query_params_strip::ParamStripState::seeded(tab.params);
                query_tab.connection_target =
                    open.connection_id
                        .map(|id| crate::tabs::TabConnectionTarget {
                            profile_id: id.0,
                            database: open.database.clone().filter(|d| !d.is_empty()),
                        });
                if let Some(offset) = tab.cursor {
                    query_tab
                        .editor
                        .update(cx, |editor, cx| editor.place_cursor(offset, cx));
                }
            }
            if let Some(title) = open.title {
                self.rename_tab(index, title, cx);
            }
        }
        self.select_tab(first, cx);
        let message = crate::launch_tabs::opened_message(count, single_connection.as_deref());
        Self::toast_update_outcome(Notification::info(message), cx);
        self.save_workspace_session(cx);
        cx.notify();
    }
}
