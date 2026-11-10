//! Connection Health dashboard — every saved connection's observed health in
//! one overlay: badge, last connected/tested age, redacted last error, and a
//! client-measured test latency, with per-row Test and a Test All pass.
//!
//! The dialog renders on the unified [`crate::dialog::Dialog`] chrome, emits
//! [`ToolDialogEvent`] (only `Close`), and never touches the workbench. Test outcomes persist through
//! [`ConnectionStore::record_test_success`]/[`ConnectionStore::record_failure`]
//! — the same records the connection manager writes — so the badges here and
//! in the saved-connections tree agree after the dialog closes.
//!
//! Routing: tests resolve services per profile through
//! [`crate::service_router::ServiceRouter`] (Direct → in-process, Via Relay →
//! the established relay session or an honest "session not open" failure).
//! [`ConnectionHealthDashboard::new`] wraps plain default [`Services`] for
//! hosts without a router; prefer [`ConnectionHealthDashboard::with_router`]
//! so Via-Relay profiles test over their real route.

// Browser build: swap tokio's channels/spawn for the reactor-free shim so the
// `tokio::` paths below compile unchanged.
#[cfg(target_arch = "wasm32")]
use crate::wasm_tokio as tokio;

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use gpui::{
    Context, EventEmitter, FocusHandle, IntoElement, ParentElement, Render, Styled, Window, div,
    prelude::*, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::{Disableable as _, Sizable as _};
use sqlly_core::{AuthenticationType, ConnectionProfile, ConnectionStatus, ConnectionTestStatus};
use sqlly_services::Services;
#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;
use uuid::Uuid;
#[cfg(target_arch = "wasm32")]
use web_time::Instant;

use crate::connection::ConnectionStore;
use crate::dialog::{Dialog, DialogFrame};
use crate::schema_tree::ConnectionState;
use crate::service_router::ServiceRouter;
use crate::theme::Theme;
use crate::tool_dialog::ToolDialogEvent;

impl EventEmitter<ToolDialogEvent> for ConnectionHealthDashboard {}

/// Longest error text a row shows. The stored summary is already secret-free;
/// this only keeps rows one line tall.
const ERROR_DISPLAY_CAP: usize = 120;

/// One finished test, handed from the background pass to the view.
struct TestReport {
    id: Uuid,
    /// `None` when the test never dialed (credential hydration failed).
    latency_ms: Option<u64>,
    /// Keyed where the app authored it; persisted with its key so the
    /// dashboard renders it in the active language.
    error: Option<sqlly_core::DisplayMessage>,
}

pub struct ConnectionHealthDashboard {
    router: ServiceRouter,
    /// Saved profiles in display order (failed → untested → healthy, then
    /// alphabetical). Reloaded from the store as test outcomes land.
    profiles: Vec<ConnectionProfile>,
    /// Last measured round-trip per profile id. In-view state only — it
    /// resets on Refresh and when the dialog closes.
    latencies: HashMap<Uuid, u64>,
    /// Profiles with a test in flight (their Test buttons are disabled).
    in_flight: HashSet<Uuid>,
    testing_all: bool,
    /// Freshness token for async completions: bumped by Refresh so results
    /// from before the reload cannot write stale latencies (the
    /// `object_dependencies` generation pattern).
    refresh_generation: u64,
    status_message: Option<String>,
    /// Live explorer connection state per profile — the same map that drives
    /// the tree dots, pushed in by the host at open. One source of connection
    /// truth, two views: the row dot here is the explorer dot, while the
    /// badge/timestamps are the persisted history.
    live_states: HashMap<Uuid, ConnectionState>,
    focus_handle: FocusHandle,
    frame: DialogFrame,
}

impl ConnectionHealthDashboard {
    /// Build over the default (in-process) [`Services`]. Every profile tests
    /// through them — fine for Direct profiles; Via-Relay profiles will fail
    /// honestly only if the caller's default services cannot reach them.
    /// Prefer [`Self::with_router`].
    pub fn new(services: Services, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::with_router(ServiceRouter::new(services), window, cx)
    }

    /// Build over the app's live [`ServiceRouter`] so each profile tests over
    /// its real route (Direct vs. an established Via-Relay session).
    pub fn with_router(
        router: ServiceRouter,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            router,
            profiles: Vec::new(),
            latencies: HashMap::new(),
            in_flight: HashSet::new(),
            testing_all: false,
            refresh_generation: 0,
            status_message: None,
            live_states: HashMap::new(),
            focus_handle: cx.focus_handle(),
            // Fixed frame: the row list manages its own scrolling.
            frame: DialogFrame::new(Theme::scaled(680.0), Theme::scaled(480.0)),
        };
        this.reload_profiles();
        this
    }

    /// Adopt the explorer's live per-server connection states so the row dots
    /// here show the same truth as the tree dots.
    pub fn set_live_states(
        &mut self,
        states: HashMap<Uuid, ConnectionState>,
        cx: &mut Context<Self>,
    ) {
        self.live_states = states;
        cx.notify();
    }

    fn live_state(&self, id: Uuid) -> ConnectionState {
        self.live_states.get(&id).copied().unwrap_or_default()
    }

    /// Re-read the store without invalidating in-flight tests (used as each
    /// outcome lands so badges update mid-pass).
    fn reload_profiles(&mut self) {
        let mut profiles = ConnectionStore::global().list();
        sort_profiles(&mut profiles);
        self.profiles = profiles;
    }

    /// Explicit Refresh: reload and invalidate anything still in flight.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.refresh_generation += 1;
        self.reload_profiles();
        self.latencies.clear();
        self.status_message = None;
        cx.notify();
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        cx.emit(ToolDialogEvent::Close);
    }

    fn test_one(&mut self, id: Uuid, cx: &mut Context<Self>) {
        let Some(profile) = self.profiles.iter().find(|p| p.id.0 == id).cloned() else {
            return;
        };
        if self.in_flight.contains(&id) || !is_testable(&profile) {
            return;
        }
        self.run_tests(vec![profile], cx);
    }

    /// Test every testable saved profile, sequentially, updating rows as each
    /// outcome lands. Interactive sign-in profiles are skipped so the pass
    /// never pops browser/device-code windows (same rule as the connection
    /// manager's Test All).
    fn test_all(&mut self, cx: &mut Context<Self>) {
        if self.testing_all {
            return;
        }
        let profiles: Vec<ConnectionProfile> = self
            .profiles
            .iter()
            .filter(|p| is_testable(p) && !self.in_flight.contains(&p.id.0))
            .cloned()
            .collect();
        let skipped = self.profiles.len() - profiles.len();
        if profiles.is_empty() {
            self.status_message = Some(crate::localization::text(
                "explorer.connection_health.no_testable_connections",
            ));
            cx.notify();
            return;
        }
        self.testing_all = true;
        let count = profiles.len().to_string();
        self.status_message = Some(if skipped > 0 {
            crate::localization::format(
                "explorer.connection_health.testing_with_skipped",
                &[("count", &count), ("skipped", &skipped.to_string())],
            )
        } else {
            crate::localization::format("explorer.connection_health.testing", &[("count", &count)])
        });
        self.run_tests(profiles, cx);
    }

    /// Run one sequential test pass over `profiles` on the tokio runtime,
    /// streaming each outcome back to the view. Outcomes always persist to the
    /// [`ConnectionStore`]; the in-view latency only applies when the capture
    /// generation still matches (Refresh discards stale results).
    fn run_tests(&mut self, profiles: Vec<ConnectionProfile>, cx: &mut Context<Self>) {
        let generation = self.refresh_generation;
        let all_pass = self.testing_all;
        for profile in &profiles {
            self.in_flight.insert(profile.id.0);
        }
        cx.notify();

        let router = self.router.clone();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<TestReport>(8);
        cx.background_spawn(async move {
            crate::tokio_handle()
                .spawn(async move {
                    for profile in profiles {
                        let id = profile.id.0;
                        let report = match crate::sql_credentials::hydrate_profile(profile) {
                            Ok(profile) => {
                                let services = router.services_for(&profile);
                                let started = Instant::now();
                                let outcome = services.connections.test_connection(&profile).await;
                                let latency_ms = started.elapsed().as_millis() as u64;
                                TestReport {
                                    id,
                                    latency_ms: Some(latency_ms),
                                    error: outcome.err().map(|e| e.message()),
                                }
                            }
                            // The failure is persisted with the profile's
                            // health record as key + arguments (plus the
                            // English fallback); the UI language is not
                            // visible on this runtime.
                            Err(error) => TestReport {
                                id,
                                latency_ms: None,
                                error: Some(error),
                            },
                        };
                        // Persist through the same records the connection
                        // manager writes, so badges agree everywhere.
                        match &report.error {
                            None => {
                                ConnectionStore::global()
                                    .record_test_success(report.id, Utc::now());
                            }
                            Some(error) => {
                                ConnectionStore::global().record_failure_message(
                                    report.id,
                                    Utc::now(),
                                    error,
                                );
                            }
                        }
                        if tx.send(report).await.is_err() {
                            return;
                        }
                    }
                })
                .await
                .ok();
        })
        .detach();

        let async_cx = cx.to_async();
        let weak = cx.weak_entity();
        cx.spawn(async move |_this, _cx| {
            let mut passed = 0usize;
            let mut failed = 0usize;
            while let Some(report) = rx.recv().await {
                if report.error.is_none() {
                    passed += 1;
                } else {
                    failed += 1;
                }
                let mut cx = async_cx.clone();
                let _ = weak.update(&mut cx, |this, cx| {
                    this.in_flight.remove(&report.id);
                    if this.refresh_generation == generation
                        && let Some(ms) = report.latency_ms
                    {
                        this.latencies.insert(report.id, ms);
                    }
                    this.reload_profiles();
                    cx.notify();
                });
            }
            let mut cx = async_cx.clone();
            weak.update(&mut cx, |this, cx| {
                if all_pass {
                    this.testing_all = false;
                    this.status_message = Some(crate::localization::format(
                        "explorer.connection_health.test_all_result",
                        &[
                            ("passed", &passed.to_string()),
                            ("failed", &failed.to_string()),
                        ],
                    ));
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

// ---------------------------------------------------------------------------
// Pure helpers (unit-tested below).
// ---------------------------------------------------------------------------

/// The one-word health badge for a profile's observed status.
fn health_badge(status: &ConnectionStatus) -> String {
    crate::localization::text(match status.last_test_status {
        Some(ConnectionTestStatus::Succeeded) => "explorer.connection_health.badge_healthy",
        Some(ConnectionTestStatus::Failed) => "explorer.connection_health.badge_failed",
        Some(ConnectionTestStatus::Untested) | None => "explorer.connection_health.badge_untested",
    })
}

/// Sort rank: failed first (needs attention), then untested, then healthy.
fn health_rank(status: &ConnectionStatus) -> u8 {
    match status.last_test_status {
        Some(ConnectionTestStatus::Failed) => 0,
        Some(ConnectionTestStatus::Untested) | None => 1,
        Some(ConnectionTestStatus::Succeeded) => 2,
    }
}

/// Failed → untested → healthy, alphabetical (case-insensitive) within each
/// group.
fn sort_profiles(profiles: &mut [ConnectionProfile]) {
    profiles.sort_by(|a, b| {
        health_rank(&a.status)
            .cmp(&health_rank(&b.status))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

/// "just now" / "5m ago" / "3h ago" / "2d ago" (same buckets as the saved-
/// connections tree).
pub(crate) fn relative_age(age: chrono::Duration) -> String {
    let seconds = age.num_seconds().max(0);
    if seconds < 60 {
        return crate::localization::text("explorer.connection_health.just_now");
    }
    let (key, value) = if seconds < 3_600 {
        ("explorer.connection_health.minutes_ago", seconds / 60)
    } else if seconds < 86_400 {
        ("explorer.connection_health.hours_ago", seconds / 3_600)
    } else {
        ("explorer.connection_health.days_ago", seconds / 86_400)
    };
    crate::localization::format(key, &[("count", &value.to_string())])
}

/// Relative wording for an optional timestamp: "never" when absent.
pub(crate) fn relative_time(at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> String {
    at.map(|at| relative_age(now.signed_duration_since(at)))
        .unwrap_or_else(|| crate::localization::text("explorer.connection_health.never"))
}

/// One-line summary: badge, tested age, and (when failed) the redacted
/// error. The dashboard renders these pieces as separate row elements; this
/// composed form is the tested contract and is exported for hosts that want
/// a single status line (status bar, tooltips).
pub fn health_description(status: &ConnectionStatus, now: DateTime<Utc>) -> String {
    let badge = health_badge(status);
    let observed = status
        .last_tested_at
        .map(|at| relative_age(now.signed_duration_since(at)))
        .unwrap_or_else(|| crate::localization::text("explorer.connection_health.never_tested"));
    match last_error_text(status).as_deref() {
        Some(error) => crate::localization::format(
            "explorer.connection_health.description_with_error",
            &[
                ("badge", &badge),
                ("observed", &observed),
                ("error", &redact_error(error)),
            ],
        ),
        None => crate::localization::format(
            "explorer.connection_health.description",
            &[("badge", &badge), ("observed", &observed)],
        ),
    }
}

/// The last failure in the active language: an app-authored failure is
/// re-rendered from its stored key and arguments; raw driver text and records
/// written before keys were stored show the persisted summary as is.
pub(crate) fn last_error_text(status: &ConnectionStatus) -> Option<String> {
    let summary = status.last_error_summary.as_deref()?;
    Some(match &status.last_error_message {
        Some(stored) => crate::localization::stored_message(stored, summary),
        None => summary.to_owned(),
    })
}

/// Collapse an error summary to a single bounded line. The stored summary is
/// already secret-free; this folds whitespace and caps the length for display.
fn redact_error(summary: &str) -> String {
    let folded = summary.split_whitespace().collect::<Vec<_>>().join(" ");
    if folded.chars().count() <= ERROR_DISPLAY_CAP {
        return folded;
    }
    let mut capped: String = folded.chars().take(ERROR_DISPLAY_CAP).collect();
    capped.push('\u{2026}');
    capped
}

/// Display text for a measured round-trip. `None` renders as an em dash.
fn format_latency(ms: Option<u64>) -> String {
    match ms {
        None => "\u{2014}".to_string(),
        Some(0) => "<1 ms".to_string(),
        Some(ms) if ms < 1_000 => format!("{ms} ms"),
        Some(ms) => format!("{:.1} s", ms as f64 / 1_000.0),
    }
}

/// Dot color for a live explorer connection state (mirrors the tree dots,
/// stale states dimmed the same way).
fn live_state_color(state: ConnectionState) -> gpui::Hsla {
    match state {
        ConnectionState::Connected => Theme::success(),
        ConnectionState::Unreachable => Theme::error(),
        ConnectionState::StaleConnected => Theme::success().opacity(0.4),
        ConnectionState::StaleUnreachable => Theme::error().opacity(0.4),
        ConnectionState::Disconnected => Theme::text_dim().opacity(0.5),
    }
}

/// One-glance overview of every server's live state:
/// "2 connected · 1 unreachable · 5 not connected".
pub(crate) fn live_summary(
    profiles: &[ConnectionProfile],
    states: &HashMap<Uuid, ConnectionState>,
) -> String {
    let (mut connected, mut unreachable, mut other) = (0usize, 0usize, 0usize);
    for profile in profiles {
        match states.get(&profile.id.0).copied().unwrap_or_default() {
            ConnectionState::Connected => connected += 1,
            ConnectionState::Unreachable => unreachable += 1,
            _ => other += 1,
        }
    }
    crate::localization::format(
        "explorer.connection_health.live_summary",
        &[
            ("connected", &connected.to_string()),
            ("unreachable", &unreachable.to_string()),
            ("not_connected", &other.to_string()),
        ],
    )
}

/// "Direct" vs "Via Relay", from the route the profile carries.
fn route_label(route: &sqlly_core::connection::ConnectionRoute) -> String {
    crate::localization::text(match route {
        sqlly_core::connection::ConnectionRoute::Direct => {
            "explorer.connection_health.route_direct"
        }
        sqlly_core::connection::ConnectionRoute::ViaRelay { .. } => {
            "explorer.connection_health.route_relay"
        }
    })
}

/// Interactive sign-in profiles are not testable from here: dialing them
/// could pop sign-in windows mid-pass.
fn is_testable(profile: &ConnectionProfile) -> bool {
    !matches!(
        profile.authentication,
        AuthenticationType::EntraBrowser
            | AuthenticationType::EntraDeviceCode
            | AuthenticationType::AwsRdsIam { .. }
            | AuthenticationType::GcpCloudSqlIam { .. }
    )
}

impl gpui::Focusable for ConnectionHealthDashboard {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ConnectionHealthDashboard {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let now = Utc::now();
        let any_running = self.testing_all || !self.in_flight.is_empty();

        let mut list = div()
            .id("connection-health-list")
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.0))
            .overflow_scroll()
            .gap(px(s(2.0)));
        if self.profiles.is_empty() {
            list = list.child(div().p(px(s(12.0))).text_color(Theme::text_dim()).child(
                crate::localization::text("explorer.connection_health.empty"),
            ));
        }
        for (ix, profile) in self.profiles.iter().enumerate() {
            let id = profile.id.0;
            let badge = health_badge(&profile.status);
            let badge_color = match profile.status.last_test_status {
                Some(ConnectionTestStatus::Succeeded) => Theme::success(),
                Some(ConnectionTestStatus::Failed) => Theme::error(),
                Some(ConnectionTestStatus::Untested) | None => Theme::text_dim(),
            };
            let testing = self.in_flight.contains(&id);
            let testable = is_testable(profile);
            let engine = profile.database_type.display_name().to_string();
            let route = route_label(&profile.route);
            let tested = relative_time(profile.status.last_tested_at, now);
            let connected = relative_time(profile.status.last_connected_at, now);
            let subline = crate::localization::format(
                "explorer.connection_health.row_subline",
                &[
                    ("engine", &engine),
                    ("route", &route),
                    ("tested", &tested),
                    ("connected", &connected),
                ],
            );
            let error_line = last_error_text(&profile.status)
                .as_deref()
                .map(redact_error);

            let mut detail = div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w(px(0.0))
                .child(
                    div()
                        .text_color(Theme::text())
                        .text_size(px(Theme::font_size_ratio(0.93)))
                        .child(profile.name.clone()),
                )
                .child(
                    div()
                        .text_color(Theme::text_dim())
                        .text_size(px(Theme::font_size_ratio(0.78)))
                        .child(subline),
                );
            if let Some(error) = error_line {
                detail = detail.child(
                    div()
                        .text_color(Theme::error())
                        .text_size(px(Theme::font_size_ratio(0.78)))
                        .child(error),
                );
            }

            list = list.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(10.0)))
                    .px(px(s(8.0)))
                    .py(px(s(6.0)))
                    .rounded(px(s(4.0)))
                    .bg(if ix % 2 == 1 {
                        Theme::row_alt()
                    } else {
                        gpui::transparent_black()
                    })
                    .child(
                        // The live explorer dot — same truth as the tree.
                        // The badge word and timestamps beside it stay the
                        // persisted history.
                        div()
                            .flex_shrink_0()
                            .w(px(s(8.0)))
                            .h(px(s(8.0)))
                            .rounded_full()
                            .bg(live_state_color(self.live_state(id))),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .w(px(s(58.0)))
                            .text_color(badge_color)
                            .text_size(px(Theme::font_size_ratio(0.78)))
                            .child(badge),
                    )
                    .child(detail)
                    .child(
                        div()
                            .flex_shrink_0()
                            .w(px(s(58.0)))
                            .text_color(Theme::text_dim())
                            .text_size(px(Theme::font_size_ratio(0.82)))
                            .child(if testing {
                                "\u{2026}".to_string()
                            } else {
                                format_latency(self.latencies.get(&id).copied())
                            }),
                    )
                    .child(
                        Button::new(("ch-test", ix))
                            .small()
                            .label(crate::localization::text(if testing {
                                "explorer.connection_health.testing_button"
                            } else {
                                "explorer.connection_health.test_button"
                            }))
                            .disabled(testing || self.testing_all || !testable)
                            .on_click(
                                cx.listener(move |this, _, _window, cx| this.test_one(id, cx)),
                            ),
                    ),
            );
        }

        // Bare body: the row list scrolls itself, so the chrome must not wrap
        // it in another scroll container.
        let body = div()
            .flex_1()
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .gap(px(s(8.0)))
            .px(px(s(14.0)))
            .py(px(s(10.0)))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(8.0)))
                    .child(
                        div()
                            .text_color(Theme::text_dim())
                            .text_size(px(Theme::font_size_ratio(0.82)))
                            .child(crate::localization::format(
                                "explorer.connection_health.saved_summary",
                                &[
                                    ("count", &self.profiles.len().to_string()),
                                    (
                                        "live_summary",
                                        &live_summary(&self.profiles, &self.live_states),
                                    ),
                                ],
                            )),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .min_w(px(0.0))
                            .text_color(Theme::text_dim())
                            .text_size(px(Theme::font_size_ratio(0.82)))
                            .child(self.status_message.clone().unwrap_or_default()),
                    ),
            )
            .child(list);

        let focus_handle = self.focus_handle.clone();
        Dialog::new("connection-health-dialog", |this: &mut Self| {
            &mut this.frame
        })
        .title(crate::localization::text(
            "explorer.connection_health.title",
        ))
        .easy_close(true)
        .key_context("ConnectionHealthDashboard")
        .track_focus(&focus_handle)
        .bare_body()
        .on_close(|this, _window, cx| this.close(cx))
        .child(body)
        .leading_button(
            Button::new("ch-refresh")
                .label(crate::localization::text(
                    "explorer.connection_health.refresh_button",
                ))
                .disabled(any_running)
                .on_click(cx.listener(|this, _, _window, cx| this.refresh(cx))),
        )
        .button(
            Button::new("ch-test-all")
                .primary()
                .label(crate::localization::text(if self.testing_all {
                    "explorer.connection_health.testing_button"
                } else {
                    "explorer.connection_health.test_all_button"
                }))
                .disabled(any_running || self.profiles.is_empty())
                .on_click(cx.listener(|this, _, _window, cx| this.test_all(cx))),
        )
        .build(self, window, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::{SecretProvider, SecretRef};

    fn status(test: Option<ConnectionTestStatus>) -> ConnectionStatus {
        ConnectionStatus {
            last_connected_at: None,
            last_tested_at: None,
            last_test_status: test,
            last_error_summary: None,
            last_error_message: None,
        }
    }

    fn profile(name: &str, test: Option<ConnectionTestStatus>) -> ConnectionProfile {
        let mut p = ConnectionProfile::sql_login(
            name,
            "localhost",
            "sa",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: "pw".to_string(),
            },
        );
        p.status = status(test);
        p
    }

    #[test]
    fn badge_maps_test_status_to_health_words() {
        assert_eq!(
            health_badge(&status(Some(ConnectionTestStatus::Succeeded))),
            "healthy"
        );
        assert_eq!(
            health_badge(&status(Some(ConnectionTestStatus::Failed))),
            "failed"
        );
        assert_eq!(
            health_badge(&status(Some(ConnectionTestStatus::Untested))),
            "untested"
        );
        assert_eq!(health_badge(&status(None)), "untested");
    }

    /// The overview line counts live explorer states, with stale/last-known
    /// states counted as not connected (they are claims about last session).
    #[test]
    fn live_summary_counts_connected_unreachable_and_the_rest() {
        let a = profile("a", None);
        let b = profile("b", None);
        let c = profile("c", None);
        let d = profile("d", None);
        let mut states = HashMap::new();
        states.insert(a.id.0, ConnectionState::Connected);
        states.insert(b.id.0, ConnectionState::Unreachable);
        states.insert(c.id.0, ConnectionState::StaleConnected);
        // d has no entry: default Disconnected.
        assert_eq!(
            live_summary(&[a, b, c, d], &states),
            "1 connected \u{b7} 1 unreachable \u{b7} 2 not connected"
        );
    }

    #[test]
    fn relative_age_buckets_minutes_hours_days() {
        assert_eq!(relative_age(chrono::Duration::seconds(5)), "just now");
        assert_eq!(relative_age(chrono::Duration::minutes(5)), "5m ago");
        assert_eq!(relative_age(chrono::Duration::hours(3)), "3h ago");
        assert_eq!(relative_age(chrono::Duration::days(2)), "2d ago");
        // Clock skew never yields negative ages.
        assert_eq!(relative_age(chrono::Duration::seconds(-30)), "just now");
        // The optional wrapper spells out "never".
        let now = Utc::now();
        assert_eq!(relative_time(None, now), "never");
        assert_eq!(
            relative_time(Some(now - chrono::Duration::minutes(7)), now),
            "7m ago"
        );
    }

    #[test]
    fn stored_failures_render_in_the_active_language() {
        // Recorded off the UI thread: only the English fallback is resolved.
        let message = std::thread::spawn(|| {
            crate::localization::deferred_text("shell.connections.relay_lost")
        })
        .join()
        .unwrap();
        let mut st = status(None);
        st.record_failure_message(Utc::now(), &message);
        let english = st.last_error_summary.clone().unwrap();
        let guard = crate::localization::test_language(crate::localization::Language::Spanish);
        let spanish = last_error_text(&st).unwrap();
        drop(guard);
        assert_ne!(spanish, english, "keyed failures follow the UI language");
        assert_eq!(
            spanish,
            sqlly_i18n::format_compiled(
                sqlly_i18n::Language::Spanish,
                "shell.connections.relay_lost",
                &[]
            )
            .unwrap()
        );

        // Records from older builds (summary only) and unknown keys show the
        // persisted text.
        let mut legacy = status(Some(ConnectionTestStatus::Failed));
        legacy.last_error_summary = Some("Login failed".into());
        assert_eq!(last_error_text(&legacy).as_deref(), Some("Login failed"));
        legacy.last_error_message = Some(sqlly_core::StoredMessage {
            key: "retired.key.that_no_longer_exists".into(),
            arguments: Vec::new(),
        });
        assert_eq!(last_error_text(&legacy).as_deref(), Some("Login failed"));
    }

    #[test]
    fn description_appends_redacted_error_and_age() {
        let now = Utc::now();
        let mut st = status(Some(ConnectionTestStatus::Failed));
        st.last_tested_at = Some(now - chrono::Duration::minutes(5));
        st.last_error_summary = Some("login  failed\nfor user".to_string());
        assert_eq!(
            health_description(&st, now),
            "failed \u{b7} 5m ago \u{b7} login failed for user"
        );
        let healthy = status(Some(ConnectionTestStatus::Succeeded));
        assert_eq!(
            health_description(&healthy, now),
            "healthy \u{b7} never tested"
        );
    }

    #[test]
    fn redact_error_folds_whitespace_and_caps_length() {
        assert_eq!(redact_error("a\n b\t\tc"), "a b c");
        let long = "x".repeat(ERROR_DISPLAY_CAP + 40);
        let capped = redact_error(&long);
        assert_eq!(capped.chars().count(), ERROR_DISPLAY_CAP + 1);
        assert!(capped.ends_with('\u{2026}'));
    }

    #[test]
    fn latency_formats_dash_submilli_ms_and_seconds() {
        assert_eq!(format_latency(None), "\u{2014}");
        assert_eq!(format_latency(Some(0)), "<1 ms");
        assert_eq!(format_latency(Some(42)), "42 ms");
        assert_eq!(format_latency(Some(1_500)), "1.5 s");
    }

    #[test]
    fn sort_puts_failed_before_untested_before_healthy_alphabetically() {
        let mut profiles = vec![
            profile("zeta", Some(ConnectionTestStatus::Succeeded)),
            profile("Alpha", Some(ConnectionTestStatus::Succeeded)),
            profile("mid", None),
            profile("Broken B", Some(ConnectionTestStatus::Failed)),
            profile("broken a", Some(ConnectionTestStatus::Failed)),
        ];
        sort_profiles(&mut profiles);
        let names: Vec<&str> = profiles.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["broken a", "Broken B", "mid", "Alpha", "zeta"]);
    }

    #[test]
    fn route_labels_direct_and_via_relay() {
        use sqlly_core::connection::ConnectionRoute;
        assert_eq!(route_label(&ConnectionRoute::Direct), "Direct");
        assert_eq!(
            route_label(&ConnectionRoute::ViaRelay {
                relay_device_id: "dev".into(),
                pair_id: "pair".into(),
                relay_hint: None,
            }),
            "Via Relay"
        );
    }
}
