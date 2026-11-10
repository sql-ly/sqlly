//! Bottom status strip of the workspace window.
//!
//! Port of `StatusBarModel.swift` + `StatusBarView.swift`. Renders, for the
//! query tab currently in focus: the connected server, database, and the
//! client / project / environment badges, plus an IntelliSense error
//! indicator.
//!
//! The Swift client backs this with a hierarchical `ConnectionStoreData`
//! (servers containing databases, plus separate clients/projects/environments
//! collections carrying `colorHex`/`imagePath`). The GPUI app uses a flat
//! `ConnectionProfile` list where each profile carries optional `client` and
//! `project` strings and a non-optional `ConnectionEnvironment` enum; the
//! color/icon assigned to each of those entities lives in the connection
//! store's entity lists, keyed by name ([`GroupIdentities`]). A badge picks up
//! whatever identity its name resolves to and degrades to a plain outlined chip
//! when the entity has none. The environment badge is hidden when it is the
//! `Local` default (mirroring Swift's "omit when unassigned").
//!
//! All display rules are pure and unit-tested (per AGENTS.md test mandate).

use crate::tooltip::AppTooltip;
use gpui::{
    CursorStyle, Hsla, IntoElement, MouseButton, SharedString, Styled, div, img, prelude::*, px,
};
use gpui_component::Icon;
use sqlly_core::connection::ConnectionRoute;
use sqlly_core::{ConnectionEnvironment, ConnectionProfile};

use crate::entra_grouping::ServerGroupingMode;
use crate::schema_tree::GroupIdentities;
use crate::theme::Theme;

/// A single labeled entity (client / project / environment) shown in the
/// status bar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusBarBadge {
    pub name: String,
    /// Optional hex color (e.g. "#1E88E5") assigned to the entity in the
    /// connection workspace; painted as the chip's background when present.
    pub color_hex: Option<String>,
    /// Optional icon assigned to the entity, as a path on disk; drawn to the
    /// left of the name inside the chip.
    pub image_path: Option<String>,
}

impl StatusBarBadge {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            color_hex: None,
            image_path: None,
        }
    }

    /// Attach the color/icon the connection store has recorded for this
    /// entity's name. Blank values are dropped so an entity with an empty color
    /// field renders as the plain outlined chip rather than an unparseable one.
    fn with_identity(mut self, identity: (Option<String>, Option<String>)) -> Self {
        let keep = |value: Option<String>| {
            value
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        self.color_hex = keep(identity.0);
        self.image_path = keep(identity.1);
        self
    }

    /// Builds a badge only when the entity has a displayable name; blank names
    /// are treated as "no badge" so the status bar stays clean.
    fn from_optional(name: Option<&str>) -> Option<Self> {
        let trimmed = name.unwrap_or("").trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(Self::new(trimmed))
        }
    }

    #[allow(dead_code)]
    pub fn has_content(&self) -> bool {
        !self.name.trim().is_empty()
    }
}

/// Connection-oriented fields shown at the left of the status bar. The
/// displayed text prefers the friendly name and falls back to the host when
/// the friendly name is blank.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StatusBarConnection {
    pub server_friendly_name: Option<String>,
    pub server_host: Option<String>,
    pub database_friendly_name: Option<String>,
    pub database_host: Option<String>,
}

impl StatusBarConnection {
    /// The text to render for the server line. Prefers the friendly name,
    /// falls back to the host, then "".
    pub fn server_display(&self) -> String {
        prefer_friendly(&self.server_friendly_name, &self.server_host)
    }

    /// The text to render for the database line. Prefers the friendly name,
    /// falls back to the database name, then "".
    pub fn database_display(&self) -> String {
        prefer_friendly(&self.database_friendly_name, &self.database_host)
    }
}

/// The route facts recorded when a Via-Relay session was established: which
/// relay endpoint the session actually landed on and the display name of the
/// bridge answering behind it. Purely presentational — the session itself
/// lives in the `ServiceRouter`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayRouteFacts {
    /// Hostname of the relay endpoint the session was established through.
    pub relay_host: String,
    /// Display name of the bridge (relay connector) behind that relay.
    pub bridge_name: String,
}

/// How the focused connection reaches its server — drives the route indicator
/// at the far right of the status strip.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum StatusBarRoute {
    /// No focused connection: no indicator.
    #[default]
    None,
    /// The app dials the database server itself.
    Direct,
    /// The session rides a public relay to an on-prem bridge.
    ViaRelay {
        relay_host: String,
        bridge_name: String,
    },
}

/// Production-environment chip state for the status bar and banner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProductionChip {
    pub unlocked_remaining: Option<String>,
}

impl ProductionChip {
    pub fn label(&self) -> String {
        match self.unlocked_remaining.as_deref() {
            Some(remaining) => crate::localization::format(
                "status_bar.production.unlocked",
                &[("remaining", remaining)],
            ),
            None => crate::localization::text("status_bar.production.locked"),
        }
    }
}

/// Manual-commit chip for the active tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransactionChip {
    pub pending: u32,
}

impl TransactionChip {
    pub fn label(&self) -> String {
        crate::localization::format(
            "status_bar.transaction.pending",
            &[("count", &self.pending.to_string())],
        )
    }
}

/// Read-only connection chip with optional timed-unlock countdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadOnlyChip {
    pub unlocked_remaining: Option<String>,
}

impl ReadOnlyChip {
    pub fn label(&self) -> String {
        match self.unlocked_remaining.as_deref() {
            Some(remaining) => crate::localization::format(
                "status_bar.read_only.unlocked",
                &[("remaining", remaining)],
            ),
            None => crate::localization::text("status_bar.read_only.locked"),
        }
    }
}

/// Complete status-bar state for the query tab currently in focus.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StatusBarState {
    pub connection: StatusBarConnection,
    pub client: Option<StatusBarBadge>,
    pub project: Option<StatusBarBadge>,
    pub environment: Option<StatusBarBadge>,
    pub production: Option<ProductionChip>,
    pub transaction: Option<TransactionChip>,
    pub read_only: Option<ReadOnlyChip>,
    /// How the focused connection reaches its server (direct vs via relay).
    pub route: StatusBarRoute,
    /// Number of recovered IntelliSense errors this session. Drives the
    /// status-bar error indicator (hidden when zero).
    pub intellisense_error_count: usize,
}

impl StatusBarState {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn with_production(mut self, chip: Option<ProductionChip>) -> Self {
        self.production = chip;
        self
    }

    pub fn with_transaction(mut self, chip: Option<TransactionChip>) -> Self {
        self.transaction = chip;
        self
    }

    pub fn with_read_only(mut self, chip: Option<ReadOnlyChip>) -> Self {
        self.read_only = chip;
        self
    }
}

/// The one text size for everything in the status strip — the same `0.92`
/// ratio the result grid's message bar uses, so the two bands read as one
/// uniform surface.
pub fn bar_font_size() -> f32 {
    Theme::font_size_ratio(0.92)
}

/// Pure presentation rules for the IntelliSense error indicator in the status
/// bar. Factored out so the display text / visibility can be unit-tested
/// without any GPUI views (per AGENTS.md test mandate).
pub mod intellisense_error_indicator {
    /// Whether the indicator should be shown at all.
    pub fn is_visible(count: usize) -> bool {
        count > 0
    }

    /// The label text, e.g. "1 IntelliSense error" / "3 IntelliSense errors".
    /// Returns "" when there is nothing to show.
    pub fn label(count: usize) -> String {
        if !is_visible(count) {
            return String::new();
        }
        let key = if count == 1 {
            "status_bar.intellisense.error_singular"
        } else {
            "status_bar.intellisense.error_plural"
        };
        crate::localization::format(key, &[("count", &count.to_string())])
    }
}

/// Pure resolver turning the focused connection profile into a
/// `StatusBarState`.
///
/// Resolution rules (adapted from `StatusBarResolver` in Swift to the flat
/// GPUI profile model):
/// - Server label prefers the profile's friendly name (`profile.name`), falling
///   back to its host (`profile.server`).
/// - Database label uses the profile's `database` (the flat model has no
///   separate database friendly name, so friendly and host collapse to the same
///   value).
/// - Client badge resolves from `profile.client` when non-empty.
/// - Project badge resolves from `profile.project` when non-empty.
/// - Environment badge resolves from `profile.environment` and is **hidden when
///   `Local`** (the default), mirroring Swift's "omit when unassigned".
///
/// Badges also carry the color/icon each entity was assigned in the connection
/// workspace. Identity is matched by name, case-insensitively — the same lookup
/// the explorer's group headers use, so a client painted blue in the rail is
/// blue in the status bar too.
pub fn resolve_with_identities(
    profile: Option<&ConnectionProfile>,
    identities: &GroupIdentities,
) -> StatusBarState {
    let Some(p) = profile else {
        return StatusBarState::empty();
    };

    let connection = StatusBarConnection {
        server_friendly_name: Some(p.name.clone()),
        server_host: Some(p.server.clone()),
        database_friendly_name: p.database.clone(),
        database_host: p.database.clone(),
    };

    let identify = |badge: Option<StatusBarBadge>, mode: ServerGroupingMode| {
        badge.map(|b| {
            let identity = identities.identity(mode, &b.name);
            b.with_identity(identity)
        })
    };

    StatusBarState {
        connection,
        client: identify(
            StatusBarBadge::from_optional(p.client.as_deref()),
            ServerGroupingMode::ByClient,
        ),
        project: identify(
            StatusBarBadge::from_optional(p.project.as_deref()),
            ServerGroupingMode::ByProject,
        ),
        environment: identify(
            environment_badge(p.environment),
            ServerGroupingMode::ByEnvironment,
        ),
        production: None,
        transaction: None,
        read_only: None,
        route: resolve_route(Some(p), None),
        intellisense_error_count: 0,
    }
}

/// Resolve the production chip for the active profile, if any.
pub fn resolve_production_chip(
    profile: Option<&ConnectionProfile>,
    store: &crate::connection::ConnectionStore,
    unlock: Option<&sqlly_intel::query::safety::WriteUnlock>,
    now: sqlly_intel::query::safety::Instant,
) -> Option<ProductionChip> {
    let profile = profile?;
    let safety = crate::mutation_review::SafetyFeatures::effective_for_profile(profile, store);
    if !safety.is_production {
        return None;
    }
    let unlocked_remaining = unlock
        .filter(|u| u.is_active(now))
        .map(|u| u.remaining_label(now));
    Some(ProductionChip { unlocked_remaining })
}

/// Resolve the manual-transaction chip for the active tab, if any.
pub fn resolve_transaction_chip(tab: Option<&crate::tabs::QueryTab>) -> Option<TransactionChip> {
    let tab = tab?;
    if tab.transaction_mode != crate::tabs::TransactionMode::Manual {
        return None;
    }
    let tx = tab.manual_tx.as_ref()?;
    if tx.lost {
        return None;
    }
    Some(TransactionChip {
        pending: tx.statements,
    })
}

/// Resolve the read-only chip for the active profile.
pub fn resolve_read_only_chip(
    profile: Option<&ConnectionProfile>,
    unlock: Option<&sqlly_intel::query::safety::WriteUnlock>,
    now: sqlly_intel::query::safety::Instant,
) -> Option<ReadOnlyChip> {
    let profile = profile?;
    if !profile.block_all_but_select {
        return None;
    }
    let unlocked_remaining = unlock
        .filter(|u| u.is_active(now))
        .map(|u| u.remaining_label(now));
    Some(ReadOnlyChip { unlocked_remaining })
}

/// Resolve the route indicator for the focused profile.
///
/// A `ViaRelay` profile prefers the live session's [`RelayRouteFacts`] (the
/// relay endpoint actually landed on, the bridge's display name). Before a
/// session exists — or if the facts were never recorded — it degrades to the
/// profile's advisory relay hint (host portion only) and the raw relay device
/// id, so the indicator never lies about the transport even when it cannot
/// name the hops nicely.
pub fn resolve_route(
    profile: Option<&ConnectionProfile>,
    facts: Option<&RelayRouteFacts>,
) -> StatusBarRoute {
    let Some(p) = profile else {
        return StatusBarRoute::None;
    };
    match &p.route {
        ConnectionRoute::Direct => StatusBarRoute::Direct,
        ConnectionRoute::ViaRelay {
            relay_device_id,
            relay_hint,
            ..
        } => match facts {
            Some(f) => StatusBarRoute::ViaRelay {
                relay_host: f.relay_host.clone(),
                bridge_name: f.bridge_name.clone(),
            },
            None => StatusBarRoute::ViaRelay {
                relay_host: relay_hint
                    .as_deref()
                    .map(|h| h.split(':').next().unwrap_or(h).to_string())
                    .filter(|h| !h.trim().is_empty())
                    .unwrap_or_else(|| "relay".to_string()),
                bridge_name: relay_device_id.clone(),
            },
        },
    }
}

/// The tooltip for the route indicator: the full path a query travels.
/// Returns "" when there is no route to describe.
pub fn route_tooltip(state: &StatusBarState) -> String {
    let server = state.connection.server_display();
    match &state.route {
        StatusBarRoute::None => String::new(),
        StatusBarRoute::Direct => {
            crate::localization::format("status_bar.route.direct_tooltip", &[("server", &server)])
        }
        StatusBarRoute::ViaRelay {
            relay_host,
            bridge_name,
        } => crate::localization::format(
            "status_bar.route.relay_tooltip",
            &[
                ("relay_host", relay_host),
                ("bridge_name", bridge_name),
                ("server", &server),
            ],
        ),
    }
}

/// Renders the connection status dot: green when connected, red when not.
/// Hover shows the full path a query travels.
fn render_status_circle(state: &StatusBarState, connected: bool) -> impl IntoElement {
    let bg = if connected {
        Theme::success()
    } else {
        Theme::error()
    };
    let tooltip: SharedString = if connected {
        route_tooltip(state).into()
    } else {
        crate::localization::text("status_bar.route.not_connected_tooltip").into()
    };
    div()
        .id("status-route-indicator")
        .flex_shrink_0()
        .w(px(Theme::scaled(8.0)))
        .h(px(Theme::scaled(8.0)))
        .rounded_full()
        .bg(bg)
        .app_tooltip(move |_window, cx| crate::app::tooltip_view(tooltip.clone(), cx))
}

/// Display label for a `ConnectionEnvironment`.
pub fn environment_label(env: ConnectionEnvironment) -> &'static str {
    match env {
        ConnectionEnvironment::Local => "Local",
        ConnectionEnvironment::Dev => "Dev",
        ConnectionEnvironment::Test => "Test",
        ConnectionEnvironment::Staging => "Staging",
        ConnectionEnvironment::Production => "Production",
        ConnectionEnvironment::Unset => "Unset",
    }
}

pub fn localized_environment_label(env: ConnectionEnvironment) -> String {
    let key = match env {
        ConnectionEnvironment::Local => "status_bar.environment.local",
        ConnectionEnvironment::Dev => "status_bar.environment.dev",
        ConnectionEnvironment::Test => "status_bar.environment.test",
        ConnectionEnvironment::Staging => "status_bar.environment.staging",
        ConnectionEnvironment::Production => "status_bar.environment.production",
        ConnectionEnvironment::Unset => "status_bar.environment.unset",
    };
    crate::localization::text(key)
}

fn environment_badge(env: ConnectionEnvironment) -> Option<StatusBarBadge> {
    match env {
        ConnectionEnvironment::Local | ConnectionEnvironment::Unset => None,
        other => Some(StatusBarBadge::new(localized_environment_label(other))),
    }
}

fn prefer_friendly(friendly: &Option<String>, fallback: &Option<String>) -> String {
    if let Some(f) = friendly {
        let trimmed = f.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    // Fall back to the host/fallback value.
    if let Some(b) = fallback {
        let trimmed = b.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    String::new()
}

/// Build the connection trail text for the tab-switcher rows.
/// Returns `"client|project|env|server|database"` with only non-empty
/// segments, or `"Disconnected"` when the segment list is empty.
/// Mirrors `EditorConnectionTrailText.text` in `EditorTabSwitcher.swift`.
///
/// `is_connected` should be `false` when no profile is active; in that case
/// the result is always `"Disconnected"`.
#[cfg(test)]
pub fn connection_trail_text(state: &StatusBarState, is_connected: bool) -> String {
    if !is_connected {
        return crate::localization::text("status_bar.connection.disconnected");
    }
    let mut segments: Vec<String> = Vec::new();
    if let Some(c) = &state.client {
        segments.push(c.name.clone());
    }
    if let Some(p) = &state.project {
        segments.push(p.name.clone());
    }
    if let Some(e) = &state.environment {
        segments.push(e.name.clone());
    }
    let server = state.connection.server_display();
    if !server.is_empty() {
        segments.push(server);
    }
    let database = state.connection.database_display();
    if !database.is_empty() {
        segments.push(database);
    }
    if segments.is_empty() {
        crate::localization::text("status_bar.connection.disconnected")
    } else {
        segments.join("|")
    }
}

/// The connection trail as elements rather than one string: the client,
/// project and environment as their real identity chips (logo and colour, the
/// same treatment they get in the status bar), then the server and database as
/// plain text.
///
/// [`connection_trail_text`] remains the flat-string form. This is what the
/// tab switcher draws, because a run-together `a|b|c` string made the
/// separators read as part of the names and threw away the colours that make
/// an environment recognisable at a glance.
///
/// The trail is exactly one line of `line_h` pixels (the caller sizes its row
/// from the same value): it never wraps, and long names shrink with an
/// ellipsis instead of pushing the chips onto a second line that would
/// overlap the next row.
pub fn render_connection_trail(
    state: &StatusBarState,
    is_connected: bool,
    line_h: f32,
) -> impl IntoElement {
    let mut row = div()
        .flex()
        .flex_row()
        .flex_nowrap()
        .items_center()
        .min_w_0()
        .overflow_hidden()
        .whitespace_nowrap()
        .line_height(px(line_h))
        // Generous, because these are separate facts, not one phrase.
        .gap(px(Theme::scaled(4.0)))
        .text_size(px(Theme::font_size_ratio(0.85)))
        .text_color(Theme::text_dim());

    if !is_connected {
        return row.child(crate::localization::text(
            "status_bar.connection.disconnected",
        ));
    }

    // A divider that cannot be mistaken for text: its own breathing room and
    // dimmer than the names on either side of it.
    let divider = || {
        div()
            .flex_shrink_0()
            .px(px(Theme::scaled(4.0)))
            .text_color(Theme::text_dim().opacity(0.5))
            .child("│")
    };

    let mut any = false;
    for badge in [&state.client, &state.project, &state.environment]
        .into_iter()
        .flatten()
    {
        if any {
            row = row.child(divider());
        }
        row = row.child(badge_chip(badge, true));
        any = true;
    }
    for text in [
        state.connection.server_display(),
        state.connection.database_display(),
    ] {
        if text.is_empty() {
            continue;
        }
        if any {
            row = row.child(divider());
        }
        row = row.child(
            div()
                .min_w_0()
                .overflow_hidden()
                .text_ellipsis()
                .child(text),
        );
        any = true;
    }
    if !any {
        row = row.child(crate::localization::text(
            "status_bar.connection.disconnected",
        ));
    }
    row
}

#[allow(dead_code)]
/// The preferred height of the status strip, in pixels.
pub const PREFERRED_HEIGHT: f32 = 50.0;

/// Height of the status strip. Phase 0 token (~26px at default scale).
pub fn strip_height() -> f32 {
    Theme::status_bar_height()
}

/// Left-cluster chrome icons, in display order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChromeIcon {
    Account,
    Settings,
    Security,
    Diagnostics,
}

impl ChromeIcon {
    pub fn app_icon(self) -> crate::icon_catalog::AppIcon {
        match self {
            Self::Account => crate::icon_catalog::AppIcon::AccountUser,
            Self::Settings => crate::icon_catalog::AppIcon::Settings,
            Self::Security => crate::icon_catalog::AppIcon::Security,
            Self::Diagnostics => crate::icon_catalog::AppIcon::Diagnostics,
        }
    }

    fn tooltip(self) -> String {
        let key = match self {
            Self::Account => "status_bar.chrome.account_tooltip",
            Self::Settings => "status_bar.chrome.settings_tooltip",
            Self::Security => "status_bar.chrome.security_tooltip",
            Self::Diagnostics => "status_bar.chrome.diagnostics_tooltip",
        };
        crate::localization::text(key)
    }

    fn element_id(self) -> &'static str {
        match self {
            Self::Account => "status-chrome-account",
            Self::Settings => "status-chrome-settings",
            Self::Security => "status-chrome-security",
            Self::Diagnostics => "status-chrome-diagnostics",
        }
    }
}

/// Account, settings, security, diagnostics — left cluster, dim until hover.
pub fn chrome_icons() -> [ChromeIcon; 4] {
    [
        ChromeIcon::Account,
        ChromeIcon::Settings,
        ChromeIcon::Security,
        ChromeIcon::Diagnostics,
    ]
}

/// Transient status-line severity. Errors paint red; everything else is dim.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusMessageKind {
    Info,
    Busy,
    Success,
    Warning,
    Error,
}

/// Ink the center status line uses. No chip fill — plain text only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusInk {
    Dim,
    Error,
}

pub fn status_message_ink(kind: StatusMessageKind) -> StatusInk {
    match kind {
        StatusMessageKind::Error => StatusInk::Error,
        StatusMessageKind::Info
        | StatusMessageKind::Busy
        | StatusMessageKind::Success
        | StatusMessageKind::Warning => StatusInk::Dim,
    }
}

pub fn status_uses_chip(_kind: StatusMessageKind) -> bool {
    false
}

pub fn status_message_icon(kind: StatusMessageKind) -> crate::icon_catalog::AppIcon {
    match kind {
        StatusMessageKind::Info | StatusMessageKind::Success => crate::icon_catalog::AppIcon::Info,
        StatusMessageKind::Busy => crate::icon_catalog::AppIcon::Loading,
        StatusMessageKind::Warning | StatusMessageKind::Error => {
            crate::icon_catalog::AppIcon::Notification
        }
    }
}

/// Collapses a message to a single line for the strip.
///
/// Engine errors arrive with embedded newlines — SQL Server sends
/// `Msg 8134, Level 16, State 1, Line 1\nDivide by zero error encountered` —
/// and a newline defeats `truncate()`: the strip grows a second row instead of
/// ellipsizing, pushing the whole status bar taller. Only the *display* text
/// is flattened; the clipboard and the expand dialog get the original.
pub fn single_line(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for word in text.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

pub fn is_copyable_status(kind: StatusMessageKind) -> bool {
    matches!(kind, StatusMessageKind::Error | StatusMessageKind::Warning)
}

fn status_text_color(kind: StatusMessageKind) -> gpui::Hsla {
    debug_assert!(!status_uses_chip(kind));
    match status_message_ink(kind) {
        StatusInk::Dim => Theme::text_dim(),
        StatusInk::Error => Theme::error(),
    }
}

/// Free-tier note: dim copy with an accent link, never a filled chip.
pub struct FreeTierPresentation {
    pub text: String,
    pub is_chip: bool,
    pub link_is_accent: bool,
}

pub fn free_tier_presentation() -> FreeTierPresentation {
    FreeTierPresentation {
        text: crate::license::status_bar_message(),
        is_chip: false,
        link_is_accent: true,
    }
}

pub fn zoom_percent_label(percent: u32) -> String {
    format!("{percent}%")
}

/// Label for the status-bar background-task chip (`None` hides the chip).
pub fn task_chip_label(running: usize) -> Option<String> {
    match running {
        0 => None,
        1 => Some(crate::localization::text("status_bar.tasks.one")),
        n => Some(crate::localization::format(
            "status_bar.tasks.many",
            &[("count", &n.to_string())],
        )),
    }
}

/// Text for the Vim mode slot at the right of the status strip. `None` when
/// Vim mode is off (the slot collapses). Plain dim text, no chip — same rule
/// as the center status line (`status_uses_chip` is always false).
pub fn vim_mode_badge_label(mode: Option<&'static str>) -> Option<String> {
    mode.map(|m| format!("-- {m} --"))
}

/// Ink for the Vim mode badge. Dim text only — never accent.
pub fn vim_mode_badge_ink() -> Hsla {
    Theme::text_dim()
}

/// Fixed slot so 50% and 200% do not shift neighbors.
pub fn zoom_percent_slot_width() -> f32 {
    Theme::scaled(40.0)
}

/// Fixed slot so version length does not shift neighbors.
pub fn version_slot_width() -> f32 {
    Theme::scaled(56.0)
}

pub fn version_label() -> String {
    format!("v{}", crate::about::APP_VERSION)
}

/// Text of the status-bar chip shown while a newer build is available for this
/// platform. Names the version so it reads as an alert, not decoration.
pub fn update_chip_label(latest: &str) -> String {
    crate::localization::format("status_bar.update.available", &[("version", latest)])
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionDotKind {
    Connected,
    Disconnected,
}

pub fn connection_dot_kind(state: &StatusBarState, connection_healthy: bool) -> ConnectionDotKind {
    if !state.connection.server_display().is_empty() && connection_healthy {
        ConnectionDotKind::Connected
    } else {
        ConnectionDotKind::Disconnected
    }
}

pub fn connection_label(state: &StatusBarState, connection_healthy: bool) -> String {
    match connection_dot_kind(state, connection_healthy) {
        ConnectionDotKind::Connected => state.connection.server_display(),
        ConnectionDotKind::Disconnected => {
            crate::localization::text("status_bar.connection.not_connected")
        }
    }
}

pub fn right_cluster_items() -> [&'static str; 3] {
    ["Commands", "zoom", "version"]
}

fn render_chrome_cluster(log_badge: Option<usize>, account_signed_in: bool) -> impl IntoElement {
    let mut row = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(Theme::scaled(6.0)));
    for icon in chrome_icons() {
        let badge = if icon == ChromeIcon::Diagnostics {
            log_badge
        } else {
            None
        };
        row = row.child(render_chrome_icon(icon, badge, account_signed_in));
    }
    row
}

/// Tagline sits in the left cluster after the chrome icons. Hidden when
/// the supporter message is not shown; the live message then occupies that slot.
#[cfg(test)]
pub fn tagline_belongs_in_left_cluster() -> bool {
    true
}

/// Left cluster order after the icon row: connection, then tagline.
#[cfg(test)]
pub fn left_cluster_connection_precedes_tagline() -> bool {
    true
}

pub fn render_tagline() -> Option<impl IntoElement> {
    crate::license::should_show_supporter_message_now().then(|| {
        let presentation = free_tier_presentation();
        div()
            .id("status-tagline")
            .flex()
            .flex_row()
            .items_center()
            .gap(px(Theme::scaled(4.0)))
            .cursor(CursorStyle::PointingHand)
            .text_size(px(bar_font_size()))
            .text_color(Theme::love())
            .hover(|s| s.text_color(Theme::text()))
            .child(
                Icon::empty()
                    .path(crate::icon_catalog::AppIcon::Heart.path())
                    .size(px(Theme::toolbar_icon_size())),
            )
            .child(presentation.text)
            .on_mouse_down(MouseButton::Left, |_, window, cx| {
                crate::app::open_upgrade_surface_from(window, cx);
            })
    })
}

fn render_chrome_icon(
    icon: ChromeIcon,
    badge: Option<usize>,
    account_signed_in: bool,
) -> impl IntoElement {
    // The account glyph turns green while signed in (matching the toolbar
    // account button); every other chrome icon stays dim-until-hover.
    let signed_in_account = icon == ChromeIcon::Account && account_signed_in;
    let tooltip: SharedString = if signed_in_account {
        crate::localization::text("status_bar.chrome.account_signed_in_tooltip").into()
    } else {
        icon.tooltip().into()
    };
    let (base_color, hover_color) = if signed_in_account {
        (Theme::success(), Theme::success())
    } else {
        (Theme::text_dim(), Theme::text())
    };
    div()
        .id(icon.element_id())
        .relative()
        .flex()
        .items_center()
        .justify_center()
        .text_color(base_color)
        .hover(move |s| s.text_color(hover_color))
        .cursor(CursorStyle::PointingHand)
        .app_tooltip(move |_window, cx| crate::app::tooltip_view(tooltip.clone(), cx))
        .on_mouse_down(MouseButton::Left, move |_, window, cx| match icon {
            ChromeIcon::Settings => {
                window.dispatch_action(Box::new(crate::app::OpenPreferences), cx);
            }
            ChromeIcon::Security => {
                window.dispatch_action(Box::new(crate::app::Security), cx);
            }
            ChromeIcon::Diagnostics => {
                window.dispatch_action(Box::new(crate::app::ToggleLogConsole), cx);
            }
            ChromeIcon::Account => {
                window.dispatch_action(Box::new(crate::app::OpenAccount), cx);
            }
        })
        .child(
            Icon::empty()
                .path(icon.app_icon().path())
                .size(px(Theme::toolbar_icon_size())),
        )
        // Signed-in status dot, tucked into the bottom-right of the glyph.
        .when(signed_in_account, |d| {
            d.child(
                div()
                    .absolute()
                    .bottom(px(Theme::scaled(-1.0)))
                    .right(px(Theme::scaled(-1.0)))
                    .w(px(Theme::scaled(6.0)))
                    .h(px(Theme::scaled(6.0)))
                    .rounded_full()
                    .bg(Theme::success())
                    .border_1()
                    .border_color(Theme::bg()),
            )
        })
        .children(badge.map(|n| {
            div()
                .absolute()
                .top(px(Theme::scaled(-4.0)))
                .right(px(Theme::scaled(-6.0)))
                .min_w(px(Theme::scaled(12.0)))
                .px(px(Theme::scaled(3.0)))
                .h(px(Theme::scaled(12.0)))
                .rounded_full()
                .bg(Theme::error())
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(Theme::font_size_ratio(0.62)))
                .text_color(Theme::ink_on(Theme::error()))
                .child(format!("{n}"))
        }))
}

fn render_plain_status(kind: StatusMessageKind, text: &str) -> impl IntoElement {
    let copyable = is_copyable_status(kind) && !text.is_empty();
    div()
        .id("status-message")
        .flex()
        .flex_row()
        .items_center()
        .min_w_0()
        .gap(px(Theme::scaled(4.0)))
        .text_color(status_text_color(kind))
        .when(copyable, |d| d.cursor(CursorStyle::PointingHand))
        .when(!text.is_empty(), |d| {
            d.child(
                Icon::empty()
                    .path(status_message_icon(kind).path())
                    .size(px(Theme::toolbar_icon_size())),
            )
        })
        // One line only: a long message truncates with an ellipsis rather
        // than wrapping the ~26px strip onto extra lines. `single_line` first,
        // because `truncate` cannot ellipsize away an embedded newline.
        .child(div().min_w_0().truncate().child(single_line(text)))
}

pub fn show_status_connection(state: &StatusBarState, connection_healthy: bool) -> bool {
    connection_dot_kind(state, connection_healthy) == ConnectionDotKind::Connected
}

pub fn render_right_cluster(
    zoom_percent: u32,
    zoom_expanded: bool,
    zoom_control: impl IntoElement,
    update_version: Option<&str>,
    vim_mode: Option<&'static str>,
) -> impl IntoElement {
    let update_available = update_version.is_some();
    let update_chip = update_version.map(|latest| {
        let fill = Theme::accent();
        div()
            .id("status-update")
            .flex()
            .items_center()
            .px(px(Theme::scaled(7.0)))
            .py(px(Theme::scaled(1.0)))
            .rounded(px(Theme::scaled(3.0)))
            .bg(fill)
            .text_color(Theme::ink_on(fill))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .cursor(CursorStyle::PointingHand)
            .hover(|s| s.opacity(0.85))
            .app_tooltip(|_window, cx| {
                crate::app::tooltip_view(
                    crate::localization::text("status_bar.update.download_tooltip"),
                    cx,
                )
            })
            .on_mouse_down(MouseButton::Left, |_, window, cx| {
                window.dispatch_action(Box::new(crate::app::CheckForUpdates), cx);
            })
            .child(update_chip_label(latest))
    });
    let zoom_control = zoom_control.into_any_element();
    let _ = right_cluster_items();
    let search_tooltip: SharedString =
        crate::localization::text("status_bar.commands.tooltip").into();
    let zoom_tooltip: SharedString = crate::localization::text("status_bar.zoom.tooltip").into();
    let version_tooltip: SharedString = if update_available {
        crate::localization::text("status_bar.update.available_tooltip").into()
    } else if crate::distribution::checks_for_updates() {
        crate::localization::text("status_bar.update.check_tooltip").into()
    } else {
        crate::localization::text("status_bar.update.app_store_tooltip").into()
    };
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(Theme::scaled(10.0)))
        .children(vim_mode_badge_label(vim_mode).map(|label| {
            div()
                .id("status-vim-mode")
                .text_color(vim_mode_badge_ink())
                .child(label)
        }))
        .child(
            div()
                .id("status-search")
                .flex()
                .flex_row()
                .items_center()
                .gap(px(Theme::scaled(4.0)))
                .text_color(Theme::text_dim())
                .hover(|s| s.text_color(Theme::text()))
                .cursor(CursorStyle::PointingHand)
                .app_tooltip(move |_window, cx| {
                    crate::app::tooltip_view(search_tooltip.clone(), cx)
                })
                .on_mouse_down(MouseButton::Left, |_, window, cx| {
                    window.dispatch_action(Box::new(crate::app::ToggleCommandPalette), cx);
                })
                .child(
                    Icon::empty()
                        .path(crate::icon_catalog::AppIcon::Command.path())
                        .size(px(Theme::toolbar_icon_size())),
                )
                .child(crate::localization::text("status_bar.commands.label")),
        )
        .child(
            div()
                .id("status-zoom")
                .relative()
                .w(px(zoom_percent_slot_width()))
                .flex()
                .justify_end()
                .text_color(Theme::text_dim())
                .hover(|s| s.text_color(Theme::text()))
                .cursor(CursorStyle::PointingHand)
                .app_tooltip(move |_window, cx| crate::app::tooltip_view(zoom_tooltip.clone(), cx))
                .on_mouse_down(MouseButton::Left, |_, window, cx| {
                    window.dispatch_action(Box::new(crate::app::ToggleZoomPopover), cx);
                })
                .child(zoom_percent_label(zoom_percent))
                .when(zoom_expanded, |d| {
                    d.child(
                        div()
                            .absolute()
                            .bottom_full()
                            .right_0()
                            .mb(px(Theme::scaled(6.0)))
                            .occlude()
                            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                cx.stop_propagation();
                            })
                            .on_mouse_down_out(|_, window, cx| {
                                window.dispatch_action(Box::new(crate::app::CloseZoomPopover), cx);
                            })
                            .child(zoom_control),
                    )
                }),
        )
        .children(update_chip)
        .child(
            div()
                .id("status-version")
                .w(px(version_slot_width()))
                .flex()
                .flex_row()
                .items_center()
                .justify_end()
                .gap(px(Theme::scaled(4.0)))
                .text_color(Theme::text_dim())
                .hover(|s| s.text_color(Theme::text()))
                .cursor(CursorStyle::PointingHand)
                .app_tooltip(move |_window, cx| {
                    crate::app::tooltip_view(version_tooltip.clone(), cx)
                })
                .on_mouse_down(MouseButton::Left, |_, window, cx| {
                    window.dispatch_action(Box::new(crate::app::CheckForUpdates), cx);
                })
                .child(version_label()),
        )
}

/// Renders the left group of the status strip: chrome icons, then connection
/// when healthy. The tagline sits beside the live status message, not here.
pub fn render_left(
    state: &StatusBarState,
    connection_healthy: bool,
    log_badge: Option<usize>,
    account_signed_in: bool,
) -> impl IntoElement {
    let connected = show_status_connection(state, connection_healthy);
    let database = state.connection.database_display();

    let mut left = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(Theme::scaled(8.0)));
    left = left.child(render_chrome_cluster(log_badge, account_signed_in));

    if connected {
        left = left.child(render_status_circle(state, true)).child(
            div()
                .id("status-connection-label")
                .text_size(px(bar_font_size()))
                .text_color(Theme::text_dim())
                .hover(|s| s.text_color(Theme::text()))
                .child(connection_label(state, connection_healthy)),
        );
        if !database.is_empty() {
            left = left.child(
                div()
                    .text_size(px(bar_font_size()))
                    .text_color(Theme::text_dim())
                    .child(format!("› {}", database)),
            );
        }
    }

    if intellisense_error_indicator::is_visible(state.intellisense_error_count) {
        left = left.child(
            div()
                .text_size(px(bar_font_size()))
                .text_color(Theme::text_dim())
                .child(intellisense_error_indicator::label(
                    state.intellisense_error_count,
                )),
        );
    }

    left
}

/// Renders the right group of the status strip: client / project /
/// environment badges. Pure element builder.
pub fn render_badges(state: &StatusBarState) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(Theme::scaled(6.0)))
        .children(state.client.as_ref().map(render_badge))
        .children(state.project.as_ref().map(render_badge))
        .children(state.transaction.as_ref().map(render_transaction_badge))
        .children(state.read_only.as_ref().map(render_read_only_badge))
        .children(if let Some(chip) = state.production.as_ref() {
            let env_name = state
                .environment
                .as_ref()
                .map(|b| b.name.as_str())
                .unwrap_or("Production");
            Some(render_production_badge(chip, env_name).into_any_element())
        } else {
            state
                .environment
                .as_ref()
                .map(|badge| render_badge(badge).into_any_element())
        })
}

fn render_production_badge(chip: &ProductionChip, _env_name: &str) -> impl IntoElement {
    let fill = Theme::warning();
    let ink = Theme::ink_on(fill);
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(Theme::scaled(4.0)))
        .h(px(Theme::row_height() - Theme::scaled(4.0)))
        .px(px(Theme::scaled(8.0)))
        .rounded(px(Theme::scaled(4.0)))
        .bg(fill)
        .text_size(px(bar_font_size()))
        .text_color(ink)
        .child(chip.label())
}

fn render_transaction_badge(chip: &TransactionChip) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(Theme::scaled(4.0)))
        .h(px(Theme::row_height() - Theme::scaled(4.0)))
        .px(px(Theme::scaled(8.0)))
        .rounded(px(Theme::scaled(4.0)))
        .border_1()
        .border_color(Theme::border())
        .text_size(px(bar_font_size()))
        .text_color(Theme::text_dim())
        .child(chip.label())
}

fn render_read_only_badge(chip: &ReadOnlyChip) -> impl IntoElement {
    let fill = Theme::warning();
    let ink = Theme::ink_on(fill);
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(Theme::scaled(4.0)))
        .h(px(Theme::row_height() - Theme::scaled(4.0)))
        .px(px(Theme::scaled(8.0)))
        .rounded(px(Theme::scaled(4.0)))
        .bg(fill)
        .text_size(px(bar_font_size()))
        .text_color(ink)
        .child(chip.label())
}

#[allow(dead_code)]
/// Renders the full status strip from `state`. Pure element builder — no
/// callbacks; the parent wires any click handling.
pub fn render(state: &StatusBarState) -> impl IntoElement {
    let free = free_tier_presentation();
    debug_assert!(!free.is_chip);
    let _ = free;

    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .h(px(strip_height()))
        .px(px(Theme::scaled(12.0)))
        .border_t_1()
        .border_color(Theme::border())
        .bg(Theme::bg())
        .text_size(px(bar_font_size()))
        .text_color(Theme::text_dim())
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(Theme::scaled(6.0)))
                .child(render_left(state, true, None, false))
                .children(render_tagline())
                .child(render_plain_status(StatusMessageKind::Info, "")),
        )
        .child(div().flex_1())
        .child(render_right_cluster(100, false, div(), None, None))
}

fn render_badge(badge: &StatusBarBadge) -> impl IntoElement {
    badge_chip(badge, false)
}

/// An identity chip. `shrink` lets it give up width with an ellipsis on its
/// name (the tab switcher's single-line trail); the status bar keeps its
/// chips whole.
fn badge_chip(badge: &StatusBarBadge, shrink: bool) -> gpui::Div {
    // The fallback is the caller's call, not the parser's: an unparseable color
    // means "this badge has no color", which is the same outline treatment as a
    // badge that never had one. The local parser this replaced answered a bad
    // digit with black, silently painting a black chip.
    let fill = badge
        .color_hex
        .as_deref()
        .filter(|hex| !hex.is_empty())
        .and_then(crate::theme::parse_hex_color);

    // On a filled chip the label rides the identity color, so it takes whichever
    // ink that color can carry rather than the theme's body text — a pale
    // client color would otherwise print white-on-white.
    let ink = match fill {
        Some(color) => Theme::ink_on(color),
        None => Theme::text(),
    };

    let base = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(Theme::scaled(4.0)))
        .px(px(Theme::scaled(6.0)))
        .py(px(Theme::scaled(1.0)))
        .rounded(px(Theme::scaled(3.0)))
        .text_size(px(bar_font_size()))
        .text_color(ink)
        // The entity's icon, when it has one. Sized off the interface font like
        // the explorer's group-header icon so both scale with the user's text.
        .children(badge.image_path.as_ref().map(|path| {
            img(std::path::PathBuf::from(path))
                .flex_shrink_0()
                .w(px(bar_font_size() * 1.1))
                .h(px(bar_font_size() * 1.1))
        }))
        .child(if shrink {
            div()
                .min_w_0()
                .overflow_hidden()
                .text_ellipsis()
                .child(badge.name.clone())
        } else {
            div().child(badge.name.clone())
        });
    let base = if shrink {
        base.min_w_0().overflow_hidden()
    } else {
        base
    };

    match fill {
        Some(color) => base.bg(color),
        None => base.border_1().border_color(Theme::border()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::{ConnectionEnvironment, ConnectionProfile, SecretProvider, SecretRef};

    /// Resolution with no entity identities on hand — the shape most of these
    /// cases care about.
    fn resolve(profile: Option<&ConnectionProfile>) -> StatusBarState {
        resolve_with_identities(profile, &GroupIdentities::default())
    }

    fn profile(name: &str, server: &str, database: Option<&str>) -> ConnectionProfile {
        let secret_ref = SecretRef {
            provider: SecretProvider::InMemory,
            key: "k".to_string(),
        };
        let mut p = ConnectionProfile::sql_login(name, server, "sa", secret_ref);
        p.database = database.map(|s| s.to_string());
        p
    }

    #[test]
    fn transaction_chip_reflects_pending_count() {
        use std::time::Instant;

        use crate::tabs::ManualTxTabState;

        let tx = ManualTxTabState {
            begun_at: Instant::now(),
            last_activity: Instant::now(),
            statements: 2,
            lost: false,
        };
        assert!(tx.dirty());
        let chip = TransactionChip {
            pending: tx.statements,
        };
        assert_eq!(chip.label(), "Manual · 2 pending");
    }

    #[test]
    fn read_only_chip_counts_down() {
        use sqlly_intel::query::safety::{UnlockWindow, WriteUnlock};

        let mut profile = profile("My DB", "host", None);
        profile.block_all_but_select = true;
        let unlock = WriteUnlock::start(UnlockWindow::FiveMinutes, std::time::Instant::now());
        let chip = resolve_read_only_chip(Some(&profile), Some(&unlock), std::time::Instant::now())
            .expect("chip");
        assert!(chip.label().starts_with("Read-only unlocked "));
        let locked =
            resolve_read_only_chip(Some(&profile), None, std::time::Instant::now()).expect("chip");
        assert_eq!(locked.label(), "Read-only");
    }

    #[test]
    fn production_chip_label_locked() {
        let chip = ProductionChip {
            unlocked_remaining: None,
        };
        assert_eq!(chip.label(), "PRODUCTION");
    }

    #[test]
    fn production_chip_label_unlocked_shows_countdown() {
        let chip = ProductionChip {
            unlocked_remaining: Some("4:59".to_string()),
        };
        assert_eq!(chip.label(), "PRODUCTION · 4:59");
    }

    #[test]
    fn non_production_profile_has_no_production_chip() {
        let p = profile("My DB", "host", None);
        let store = crate::connection::ConnectionStore::from_path(
            tempfile::tempdir().expect("tempdir").path().join("c.json"),
        );
        assert!(
            resolve_production_chip(Some(&p), &store, None, std::time::Instant::now(),).is_none()
        );
    }

    #[test]
    fn empty_state_when_no_profile() {
        let state = resolve(None);
        assert_eq!(state.connection.server_display(), "");
        assert_eq!(state.connection.database_display(), "");
        assert_eq!(state.client, None);
        assert_eq!(state.project, None);
        assert_eq!(state.environment, None);
    }

    #[test]
    fn server_display_prefers_friendly_name() {
        let mut p = profile("My Prod DB", "prod-sql.example.com", None);
        p.server = "prod-sql.example.com".to_string();
        let state = resolve(Some(&p));
        assert_eq!(state.connection.server_display(), "My Prod DB");
    }

    #[test]
    fn server_display_falls_back_to_host_when_name_blank() {
        let p = profile("   ", "prod-sql.example.com", None);
        let state = resolve(Some(&p));
        assert_eq!(state.connection.server_display(), "prod-sql.example.com");
    }

    #[test]
    fn database_display_uses_database_field() {
        let p = profile("My DB", "host", Some("Orders"));
        let state = resolve(Some(&p));
        assert_eq!(state.connection.database_display(), "Orders");
    }

    #[test]
    fn database_display_empty_when_none() {
        let p = profile("My DB", "host", None);
        let state = resolve(Some(&p));
        assert_eq!(state.connection.database_display(), "");
    }

    #[test]
    fn client_badge_present_when_set() {
        let mut p = profile("My DB", "host", None);
        p.client = Some("Acme Corp".to_string());
        let state = resolve(Some(&p));
        assert_eq!(state.client.as_ref().unwrap().name, "Acme Corp");
    }

    #[test]
    fn badges_carry_the_entity_color_and_icon_assigned_to_their_name() {
        let mut p = profile("My DB", "host", None);
        p.client = Some("Acme Corp".to_string());
        p.project = Some("Billing".to_string());
        p.environment = ConnectionEnvironment::Production;

        // Names are matched case-insensitively, as the explorer's group headers
        // do — the store keeps whatever casing the entity was created with.
        let identities = GroupIdentities::default()
            .with_entity(
                ServerGroupingMode::ByClient,
                "acme corp",
                Some("#2E8B57"),
                Some("/icons/acme.png"),
            )
            .with_entity(
                ServerGroupingMode::ByEnvironment,
                "Production",
                Some("#B51F1F"),
                None,
            );

        let state = resolve_with_identities(Some(&p), &identities);
        let client = state.client.unwrap();
        assert_eq!(client.color_hex.as_deref(), Some("#2E8B57"));
        assert_eq!(client.image_path.as_deref(), Some("/icons/acme.png"));
        assert_eq!(
            state.environment.unwrap().color_hex.as_deref(),
            Some("#B51F1F")
        );
        // An entity with no identity recorded stays a plain chip.
        let project = state.project.unwrap();
        assert_eq!(project.color_hex, None);
        assert_eq!(project.image_path, None);
    }

    #[test]
    fn blank_identity_values_do_not_become_chip_colors() {
        // An entity saved with an empty color field must render as the outlined
        // chip, not as an unparseable one the renderer has to guess at.
        let mut p = profile("My DB", "host", None);
        p.client = Some("Acme".to_string());
        let identities = GroupIdentities::default().with_entity(
            ServerGroupingMode::ByClient,
            "acme",
            Some("  "),
            Some(""),
        );

        let client = resolve_with_identities(Some(&p), &identities)
            .client
            .unwrap();
        assert_eq!(client.color_hex, None);
        assert_eq!(client.image_path, None);
    }

    #[test]
    fn client_badge_absent_when_blank() {
        let mut p = profile("My DB", "host", None);
        p.client = Some("   ".to_string());
        let state = resolve(Some(&p));
        assert_eq!(state.client, None);
    }

    #[test]
    fn project_badge_present_when_set() {
        let mut p = profile("My DB", "host", None);
        p.project = Some("Apollo".to_string());
        let state = resolve(Some(&p));
        assert_eq!(state.project.as_ref().unwrap().name, "Apollo");
    }

    #[test]
    fn environment_badge_hidden_for_local_default() {
        let p = profile("My DB", "host", None);
        let state = resolve(Some(&p));
        assert_eq!(state.environment, None);
    }

    #[test]
    fn environment_badge_shown_for_non_local() {
        let mut p = profile("My DB", "host", None);
        p.environment = ConnectionEnvironment::Production;
        let state = resolve(Some(&p));
        assert_eq!(state.environment.as_ref().unwrap().name, "Production");
    }

    #[test]
    fn environment_label_covers_all_variants() {
        assert_eq!(environment_label(ConnectionEnvironment::Local), "Local");
        assert_eq!(environment_label(ConnectionEnvironment::Dev), "Dev");
        assert_eq!(environment_label(ConnectionEnvironment::Test), "Test");
        assert_eq!(environment_label(ConnectionEnvironment::Staging), "Staging");
        assert_eq!(
            environment_label(ConnectionEnvironment::Production),
            "Production"
        );
    }

    #[test]
    fn intellisense_indicator_visibility() {
        assert!(!intellisense_error_indicator::is_visible(0));
        assert!(intellisense_error_indicator::is_visible(1));
        assert!(intellisense_error_indicator::is_visible(7));
    }

    #[test]
    fn intellisense_indicator_label_singular_and_plural() {
        assert_eq!(intellisense_error_indicator::label(0), "");
        assert_eq!(
            intellisense_error_indicator::label(1),
            "1 IntelliSense error"
        );
        assert_eq!(
            intellisense_error_indicator::label(3),
            "3 IntelliSense errors"
        );
    }

    #[test]
    fn badge_from_optional_omits_blank() {
        assert!(StatusBarBadge::from_optional(None).is_none());
        assert!(StatusBarBadge::from_optional(Some("   ")).is_none());
        let b = StatusBarBadge::from_optional(Some("  Acme  ")).unwrap();
        assert_eq!(b.name, "Acme");
    }

    #[test]
    fn prefer_friendly_rules() {
        assert_eq!(prefer_friendly(&Some("a".into()), &Some("b".into())), "a");
        assert_eq!(prefer_friendly(&Some("  ".into()), &Some("b".into())), "b");
        assert_eq!(prefer_friendly(&None, &Some("b".into())), "b");
        assert_eq!(prefer_friendly(&None, &None), "");
    }

    fn via_relay(mut p: ConnectionProfile, hint: Option<&str>) -> ConnectionProfile {
        p.route = ConnectionRoute::ViaRelay {
            relay_device_id: "bridge-dev-1".to_string(),
            pair_id: "pair-1".to_string(),
            relay_hint: hint.map(|h| h.to_string()),
        };
        p
    }

    #[test]
    fn route_none_without_a_profile() {
        assert_eq!(resolve_route(None, None), StatusBarRoute::None);
        let state = resolve(None);
        assert_eq!(state.route, StatusBarRoute::None);
        assert_eq!(route_tooltip(&state), "");
    }

    #[test]
    fn route_direct_for_direct_profiles() {
        let p = profile("My Prod DB", "prod-sql.example.com", None);
        let state = resolve(Some(&p));
        assert_eq!(state.route, StatusBarRoute::Direct);
        assert_eq!(route_tooltip(&state), "Directly connected to My Prod DB");
    }

    #[test]
    fn route_via_relay_prefers_live_session_facts() {
        let p = via_relay(profile("Client DB", "10.0.0.5", None), None);
        let facts = RelayRouteFacts {
            relay_host: "relay-us.sqlly.app".to_string(),
            bridge_name: "office-bridge".to_string(),
        };
        assert_eq!(
            resolve_route(Some(&p), Some(&facts)),
            StatusBarRoute::ViaRelay {
                relay_host: "relay-us.sqlly.app".to_string(),
                bridge_name: "office-bridge".to_string(),
            }
        );
    }

    #[test]
    fn route_via_relay_degrades_to_hint_host_and_device_id() {
        // Without session facts the indicator still tells the truth about the
        // transport: hint host (port stripped) and the raw device id.
        let p = via_relay(
            profile("Client DB", "10.0.0.5", None),
            Some("relay-eu.sqlly.app:443"),
        );
        assert_eq!(
            resolve_route(Some(&p), None),
            StatusBarRoute::ViaRelay {
                relay_host: "relay-eu.sqlly.app".to_string(),
                bridge_name: "bridge-dev-1".to_string(),
            }
        );
        // No hint at all: a generic "relay" placeholder rather than a blank.
        let p = via_relay(profile("Client DB", "10.0.0.5", None), None);
        assert_eq!(
            resolve_route(Some(&p), None),
            StatusBarRoute::ViaRelay {
                relay_host: "relay".to_string(),
                bridge_name: "bridge-dev-1".to_string(),
            }
        );
    }

    #[test]
    fn route_tooltip_shows_the_full_relay_path() {
        let mut state = resolve(Some(&via_relay(
            profile("Client DB", "10.0.0.5", None),
            None,
        )));
        state.route = StatusBarRoute::ViaRelay {
            relay_host: "relay-us.sqlly.app".to_string(),
            bridge_name: "office-bridge".to_string(),
        };
        assert_eq!(
            route_tooltip(&state),
            "Connected via relay-us.sqlly.app → office-bridge → Client DB"
        );
    }

    #[test]
    fn connection_trail_disconnected_when_no_profile() {
        let state = StatusBarState::empty();
        assert_eq!(connection_trail_text(&state, false), "Disconnected");
    }

    #[test]
    fn connection_trail_disconnected_when_connected_but_empty_segments() {
        let state = StatusBarState::empty();
        assert_eq!(connection_trail_text(&state, true), "Disconnected");
    }

    #[test]
    fn connection_trail_full_segments() {
        let state = StatusBarState {
            connection: StatusBarConnection {
                server_friendly_name: Some("MyServer".into()),
                server_host: Some("host:1433".into()),
                database_friendly_name: Some("MyDB".into()),
                database_host: Some("MyDB".into()),
            },
            client: Some(StatusBarBadge::new("Acme")),
            project: Some(StatusBarBadge::new("Portal")),
            environment: Some(StatusBarBadge::new("Production")),
            production: None,
            transaction: None,
            read_only: None,
            route: StatusBarRoute::Direct,
            intellisense_error_count: 0,
        };
        assert_eq!(
            connection_trail_text(&state, true),
            "Acme|Portal|Production|MyServer|MyDB"
        );
    }

    #[test]
    fn connection_trail_partial_segments() {
        let state = StatusBarState {
            connection: StatusBarConnection {
                server_friendly_name: Some("MyServer".into()),
                server_host: Some("host:1433".into()),
                database_friendly_name: None,
                database_host: None,
            },
            client: None,
            project: None,
            environment: Some(StatusBarBadge::new("Dev")),
            production: None,
            transaction: None,
            read_only: None,
            route: StatusBarRoute::Direct,
            intellisense_error_count: 0,
        };
        assert_eq!(connection_trail_text(&state, true), "Dev|MyServer");
    }

    #[test]
    fn connection_trail_server_only() {
        let state = StatusBarState {
            connection: StatusBarConnection {
                server_friendly_name: Some("srv".into()),
                server_host: Some("srv:1433".into()),
                database_friendly_name: None,
                database_host: None,
            },
            client: None,
            project: None,
            environment: None,
            production: None,
            transaction: None,
            read_only: None,
            route: StatusBarRoute::Direct,
            intellisense_error_count: 0,
        };
        assert_eq!(connection_trail_text(&state, true), "srv");
    }

    #[test]
    fn strip_height_uses_phase_0_status_bar_token() {
        assert_eq!(strip_height(), Theme::status_bar_height());
        assert!(strip_height() > 0.0);
        assert!(strip_height() < 50.0);
    }

    #[test]
    fn chrome_cluster_is_account_settings_security_diagnostics() {
        assert_eq!(
            chrome_icons(),
            [
                ChromeIcon::Account,
                ChromeIcon::Settings,
                ChromeIcon::Security,
                ChromeIcon::Diagnostics,
            ]
        );
        assert_eq!(
            ChromeIcon::Settings.app_icon(),
            crate::icon_catalog::AppIcon::Settings
        );
        assert_eq!(
            ChromeIcon::Security.app_icon(),
            crate::icon_catalog::AppIcon::Security
        );
        assert_eq!(
            ChromeIcon::Diagnostics.app_icon(),
            crate::icon_catalog::AppIcon::Diagnostics
        );
        assert_eq!(
            ChromeIcon::Account.app_icon(),
            crate::icon_catalog::AppIcon::AccountUser
        );
    }

    #[test]
    fn status_messages_are_plain_dim_text_not_a_chip() {
        for kind in [
            StatusMessageKind::Info,
            StatusMessageKind::Busy,
            StatusMessageKind::Success,
            StatusMessageKind::Warning,
        ] {
            assert_eq!(status_message_ink(kind), StatusInk::Dim);
            assert!(!status_uses_chip(kind));
        }
        assert_eq!(
            status_message_ink(StatusMessageKind::Error),
            StatusInk::Error
        );
        assert!(!status_uses_chip(StatusMessageKind::Error));
    }

    #[test]
    fn errors_and_warnings_remain_copyable() {
        assert!(is_copyable_status(StatusMessageKind::Error));
        assert!(is_copyable_status(StatusMessageKind::Warning));
        assert!(!is_copyable_status(StatusMessageKind::Info));
        assert!(!is_copyable_status(StatusMessageKind::Busy));
        assert!(!is_copyable_status(StatusMessageKind::Success));
    }

    #[test]
    fn single_line_flattens_engine_errors_onto_one_row() {
        // The shape SQL Server actually sends — a newline defeats `truncate`,
        // so the strip grew a second row instead of ellipsizing.
        assert_eq!(
            single_line("Msg 8134, Level 16, State 1, Line 1\nDivide by zero error encountered."),
            "Msg 8134, Level 16, State 1, Line 1 Divide by zero error encountered."
        );
        assert_eq!(single_line("  padded \r\n\t text  "), "padded text");
        assert_eq!(single_line("already one line"), "already one line");
        assert_eq!(single_line(""), "");
        assert_eq!(single_line("   \n  "), "");
    }

    #[test]
    fn single_line_never_grows_the_message() {
        // Flattening may only collapse: the truncation budget is a width, so
        // a "flattened" string that got longer would defeat the point.
        for text in [
            "a\nb",
            "one   two",
            "trailing\n",
            "\nleading",
            "mixed \t\r\n whitespace",
        ] {
            assert!(single_line(text).len() <= text.len(), "{text:?}");
            assert!(!single_line(text).contains('\n'), "{text:?}");
        }
    }

    #[test]
    fn free_tier_is_dim_text_with_accent_link_not_a_chip() {
        let presentation = free_tier_presentation();
        assert_eq!(presentation.text, crate::license::status_bar_message());
        assert!(!presentation.is_chip);
        assert!(presentation.link_is_accent);
    }

    #[test]
    fn tagline_lives_in_the_left_cluster_not_the_center() {
        assert!(tagline_belongs_in_left_cluster());
        assert!(left_cluster_connection_precedes_tagline());
        assert_eq!(
            free_tier_presentation().text,
            crate::license::status_bar_message()
        );
        assert!(
            right_cluster_items()
                .iter()
                .all(|item| !item.contains("SQLLY") && !item.contains("love")),
            "tagline must not duplicate on the right of the status bar"
        );
    }

    #[test]
    fn tagline_uses_love_ink_not_the_accent() {
        assert_ne!(Theme::love(), Theme::accent());
        assert_ne!(Theme::love(), Theme::error());
        let h = Theme::love().h;
        assert!(
            !(0.08..=0.85).contains(&h),
            "tagline love must be pink-red (h={h})"
        );
    }

    #[test]
    fn zoom_percent_label_is_plain_percent() {
        assert_eq!(zoom_percent_label(100), "100%");
        assert_eq!(zoom_percent_label(50), "50%");
        assert_eq!(zoom_percent_label(200), "200%");
    }

    #[test]
    fn task_chip_label_pluralizes_and_hides_zero() {
        assert_eq!(task_chip_label(0), None);
        assert_eq!(task_chip_label(1).as_deref(), Some("1 task"));
        assert_eq!(task_chip_label(3).as_deref(), Some("3 tasks"));
    }

    #[test]
    fn vim_badge_hidden_when_off() {
        assert_eq!(vim_mode_badge_label(None), None);
    }

    #[test]
    fn vim_badge_labels_modes() {
        assert_eq!(
            vim_mode_badge_label(Some("NORMAL")).as_deref(),
            Some("-- NORMAL --")
        );
        assert_eq!(
            vim_mode_badge_label(Some("INSERT")).as_deref(),
            Some("-- INSERT --")
        );
        assert_eq!(
            vim_mode_badge_label(Some("VISUAL")).as_deref(),
            Some("-- VISUAL --")
        );
    }

    #[test]
    fn vim_badge_uses_no_accent() {
        assert_eq!(vim_mode_badge_ink(), Theme::text_dim());
        assert_ne!(vim_mode_badge_ink(), Theme::accent());
    }

    #[test]
    fn update_chip_names_the_version() {
        assert_eq!(update_chip_label("0.97.1"), "Update available: v0.97.1");
    }

    #[test]
    fn version_label_includes_the_build_version() {
        let label = version_label();
        assert!(label.contains(crate::about::APP_VERSION));
        assert!(label.starts_with('v'));
    }

    #[test]
    fn connection_dot_and_label_follow_health() {
        let mut state = StatusBarState::empty();
        state.connection.server_friendly_name = Some("Prod".into());
        assert_eq!(
            connection_dot_kind(&state, true),
            ConnectionDotKind::Connected
        );
        assert_eq!(connection_label(&state, true), "Prod");
        assert_eq!(
            connection_dot_kind(&state, false),
            ConnectionDotKind::Disconnected
        );
        assert_eq!(connection_label(&state, false), "Not connected");
        assert!(!show_status_connection(&state, false));
        assert!(show_status_connection(&state, true));
        assert_eq!(
            connection_dot_kind(&StatusBarState::empty(), true),
            ConnectionDotKind::Disconnected
        );
        assert!(!show_status_connection(&StatusBarState::empty(), true));
    }

    #[test]
    fn right_cluster_is_commands_zoom_and_version() {
        assert_eq!(right_cluster_items(), ["Commands", "zoom", "version"]);
    }

    #[test]
    fn version_slot_does_not_depend_on_label_length() {
        assert_eq!(version_slot_width(), Theme::scaled(56.0));
        assert!(!version_label().is_empty());
    }

    #[test]
    fn status_message_icon_follows_kind() {
        assert_eq!(
            status_message_icon(StatusMessageKind::Info),
            crate::icon_catalog::AppIcon::Info
        );
        assert_eq!(
            status_message_icon(StatusMessageKind::Busy),
            crate::icon_catalog::AppIcon::Loading
        );
        assert_eq!(
            status_message_icon(StatusMessageKind::Error),
            crate::icon_catalog::AppIcon::Notification
        );
    }

    #[test]
    fn zoom_percent_slot_does_not_depend_on_digit_count() {
        assert_eq!(zoom_percent_slot_width(), Theme::scaled(40.0));
        assert!(zoom_percent_label(50).len() < zoom_percent_label(200).len());
    }
}
