//! The welcome canvas — what the central workbench shows until the app has an
//! active connection.
//!
//! First launch used to land on a seeded editor over a bare "No results yet."
//! strip: nothing named the next step, and the one action that unlocks the
//! app (adding or picking a connection) lived behind a text link in the
//! sidebar. The welcome canvas makes that action the headline. It is not a
//! modal and it never blocks the keyboard: pressing the run binding while
//! disconnected still walks the connection prompt, and the canvas simply
//! swaps for the workbench the moment a connection lands.
//!
//! The model half is pure and unit-tested (per AGENTS.md); the render half is
//! a plain builder called from `AppView::render`, wired through
//! `cx.listener` like the status bar.

use chrono::{DateTime, Utc};
use gpui::{
    Animation, AnimationExt as _, ClickEvent, Context, FontWeight, InteractiveElement as _,
    IntoElement as _, ParentElement as _, StatefulInteractiveElement as _, Styled as _, div,
    prelude::FluentBuilder as _, px,
};
use gpui_component::Icon;
use gpui_component::animation::cubic_bezier;
use gpui_component::kbd::Kbd;
use sqlly_core::{ConnectionEnvironment, ConnectionProfile};
use sqlly_lang::sqlmodel::{DatabaseType, Dialect};
use std::time::Duration;

use crate::app::AppView;
use crate::connect_picker::{ConnectionOption, connection_options};
use crate::theme::Theme;

/// How many saved connections the canvas lists. More than this and the list
/// stops being a welcome and starts being a second sidebar.
pub const MAX_WELCOME_CONNECTIONS: usize = 5;

/// Shortcut captions shown along the bottom edge. Resolved through the
/// keybinding registry so a rebind rewrites the welcome screen too.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WelcomeHints {
    pub run: String,
    pub palette: String,
    pub sidebar: String,
}

/// Everything the canvas needs to render, derived from state the app already
/// owns. Pure data so the shaping logic is testable without a window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WelcomeModel {
    pub connections: Vec<ConnectionOption>,
    pub hints: WelcomeHints,
    /// The connections come from a relay proxy or SQLly Embedded bridge, not
    /// from this browser's in-tab samples or the user's own saved list.
    pub proxied: bool,
}

const ZERO_STATE_INVITATION: &str = "welcome.invitation.zero_connections";
const CHOOSE_CONNECTION_INVITATION: &str = "welcome.invitation.choose_connection";
const WASM_DEMO_INVITATION: &str = "welcome.invitation.browser_demo";
const PROXIED_INVITATION: &str = "welcome.invitation.proxied_connections";

impl WelcomeModel {
    /// Zero-state vs. returning-user state drives the invitation copy.
    pub fn has_connections(&self) -> bool {
        !self.connections.is_empty()
    }

    pub fn headline(&self) -> &'static str {
        "SQLLY"
    }

    /// Whether the welcome canvas offers New Connection (desktop only).
    pub fn offers_new_connection() -> bool {
        !cfg!(target_arch = "wasm32")
    }

    pub fn invitation(&self) -> String {
        let key = if self.proxied {
            PROXIED_INVITATION
        } else if cfg!(target_arch = "wasm32") {
            WASM_DEMO_INVITATION
        } else if self.has_connections() {
            CHOOSE_CONNECTION_INVITATION
        } else {
            ZERO_STATE_INVITATION
        };
        crate::localization::text(key)
    }
}

/// Build the canvas model from the saved profiles and pre-resolved shortcut
/// hints (`crate::keybindings::shortcut_hint`, which owns the on-disk
/// overrides path). Profiles arrive in sidebar order; the canvas takes the
/// first few and reuses the connect picker's tested row shaping (friendly
/// name + server/database/environment detail).
pub fn welcome_model(
    profiles: &[ConnectionProfile],
    hints: WelcomeHints,
    proxied: bool,
) -> WelcomeModel {
    let mut connections = connection_options(profiles);
    connections.truncate(MAX_WELCOME_CONNECTIONS);
    WelcomeModel {
        connections,
        hints,
        proxied,
    }
}

/// A zero-state feature tile: icon + title.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WelcomeTile {
    pub icon: crate::icon_catalog::AppIcon,
    pub title: String,
}

/// Tiles shown instead of the connection list when nothing is saved yet —
/// what the app does, not just what to click.
pub fn zero_state_tiles() -> Vec<WelcomeTile> {
    vec![
        WelcomeTile {
            icon: crate::icon_catalog::AppIcon::Terminal,
            title: crate::localization::text("welcome.tiles.notebooks"),
        },
        WelcomeTile {
            icon: crate::icon_catalog::AppIcon::Statistics,
            title: crate::localization::text("welcome.tiles.charts"),
        },
        WelcomeTile {
            icon: crate::icon_catalog::AppIcon::AiAssistant,
            title: if crate::distribution::can_launch_user_programs() {
                "MCP".to_string()
            } else {
                crate::localization::text("welcome.tiles.ai_assistant")
            },
        },
    ]
}

/// Short engine badge for a connection row — the parser family, which is what
/// actually changes the SQL you write. The first-party engines that borrow
/// another family's grammar (DuckDB parses as PostgreSQL, ClickHouse as
/// MySQL, libSQL as SQLite, Oracle shares its family with the profiled-only
/// engines) are their own products with their own drivers, so each keeps
/// its own badge; wire-compatible labels keep the family badge.
pub fn engine_label(database_type: DatabaseType) -> &'static str {
    match database_type {
        DatabaseType::DuckDb => return "DuckDB",
        DatabaseType::ClickHouse => return "ClickHouse",
        DatabaseType::Oracle => return "Oracle",
        DatabaseType::LibSql => return "libSQL",
        _ => {}
    }
    // Redis borrows a parser family only for highlighting; its badge says
    // what it is.
    if database_type.is_command_engine() {
        return "Redis";
    }
    match database_type.family() {
        Dialect::SqlServer => "MSSQL",
        Dialect::Postgres => "PG",
        Dialect::MySql => "MySQL",
        Dialect::Oracle => "Oracle",
        Dialect::Sqlite => "SQLite",
    }
}

/// Environment chip colour. Local/Unset get no chip — they are the quiet
/// default and a chip on every row is just noise.
fn environment_chip_color(env: ConnectionEnvironment) -> Option<gpui::Hsla> {
    if !environment_has_chip(env) {
        return None;
    }
    match env {
        ConnectionEnvironment::Production => Some(Theme::error()),
        ConnectionEnvironment::Staging => Some(Theme::warning()),
        ConnectionEnvironment::Dev | ConnectionEnvironment::Test => Some(Theme::success()),
        _ => None,
    }
}

/// Whether an environment earns a chip on the welcome canvas.
pub fn environment_has_chip(env: ConnectionEnvironment) -> bool {
    !matches!(
        env,
        ConnectionEnvironment::Local | ConnectionEnvironment::Unset
    )
}

/// Split a resolved shortcut hint into individual keycaps. Hints arrive as
/// `"Cmd+Enter"` or `"Cmd+Enter, F5"` — each alternative becomes its own chip.
pub fn split_keycaps(hint: &str) -> Vec<String> {
    hint.split(", ")
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect()
}

/// Convert one keycap alternative from `split_keycaps` (e.g. "Cmd+Enter",
/// "F5", "Option+←", "fn+↑", "Cmd+Shift+P") into the gpui keystrokes the kit
/// [`Kbd`] chip renders. A two-stroke chord ("Cmd+K Cmd+C") yields one
/// keystroke per stroke. `None` when any part fails to parse — the caller
/// falls back to a plain text chip instead.
pub fn keycap_keystrokes(cap: &str) -> Option<Vec<gpui::Keystroke>> {
    // Rewrite multi-word and fn-prefixed key labels first, so the whitespace
    // chord split below cannot cut them in half. The fn+arrow forms are the
    // macOS display labels for Home/End/Page Up/Page Down.
    let normalized = cap
        .replace("fn+←", "home")
        .replace("fn+→", "end")
        .replace("fn+↑", "pageup")
        .replace("fn+↓", "pagedown")
        .replace("Page Up", "pageup")
        .replace("Page Down", "pagedown");
    let strokes: Option<Vec<_>> = normalized.split_whitespace().map(keycap_stroke).collect();
    strokes.filter(|strokes| !strokes.is_empty())
}

/// One space-free stroke of a keycap hint ("Cmd+Enter", "F5", "Cmd++") →
/// a gpui keystroke.
fn keycap_stroke(part: &str) -> Option<gpui::Keystroke> {
    let (mods, key) = match part.rfind('+') {
        Some(i) if i + 1 < part.len() => (&part[..i], &part[i + 1..]),
        // A trailing '+' means the key itself is '+' ("Cmd++").
        Some(i) => (&part[..i], "+"),
        None => ("", part),
    };
    let mut descriptor = String::new();
    for modifier in mods.split('+').filter(|m| !m.is_empty()) {
        descriptor.push_str(match modifier.to_lowercase().as_str() {
            "cmd" => "cmd",
            "ctrl" => "ctrl",
            "shift" => "shift",
            "option" | "alt" => "alt",
            _ => return None,
        });
        descriptor.push('-');
    }
    let key = match key {
        "←" => "left".to_string(),
        "→" => "right".to_string(),
        "↑" => "up".to_string(),
        "↓" => "down".to_string(),
        "Esc" | "esc" => "escape".to_string(),
        other => other.to_lowercase(),
    };
    descriptor.push_str(&key);
    crate::keybindings::parse_keystroke(&descriptor).ok()
}

/// One saved-connection row, rendered as a card. Clicking connects — same path
/// as the query-bar server picker (hydrate from the store, then
/// `connect_profile`).
fn render_connection_row(
    option: &ConnectionOption,
    index: usize,
    now: DateTime<Utc>,
    cx: &mut Context<AppView>,
) -> impl gpui::IntoElement {
    let s = Theme::scaled;
    let id = option.id.clone();
    let row_id: gpui::SharedString = format!("welcome-row-{id}").into();
    let env_chip = environment_chip_color(option.environment).map(|color| {
        div()
            .px(px(s(5.0)))
            .py(px(s(1.0)))
            .rounded(px(s(3.0)))
            .bg(color.opacity(0.12))
            .font(Theme::ui_font())
            .text_size(px(Theme::font_size_ratio(0.85)))
            .text_color(color)
            .child(crate::status_bar::localized_environment_label(
                option.environment,
            ))
    });

    let card = div()
        .id(row_id.clone())
        .w_full()
        .px(px(s(10.0)))
        .py(px(s(6.0)))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(s(10.0)))
        .rounded(px(s(6.0)))
        .bg(Theme::panel_bg())
        .border_1()
        .border_color(Theme::border())
        .cursor_pointer()
        .hover(|style| {
            style
                .bg(Theme::row_hover())
                .border_color(Theme::border_hover())
        })
        .child(
            Icon::empty()
                .path(crate::icon_catalog::AppIcon::Database.path())
                .size(px(s(14.0)))
                .text_color(Theme::text_dim()),
        )
        .child(
            div()
                .flex_grow(1.0)
                .min_w(px(0.0))
                .flex()
                .flex_col()
                .gap(px(s(2.0)))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(s(6.0)))
                        .child(
                            div()
                                .truncate()
                                .font(Theme::ui_font())
                                .text_size(px(Theme::font_size()))
                                .text_color(Theme::text())
                                .child(option.display_name.clone()),
                        )
                        .children(env_chip),
                )
                .when(!option.detail.is_empty(), |col| {
                    col.child(
                        div()
                            .w_full()
                            .truncate()
                            .font(Theme::ui_font())
                            .text_size(px(Theme::font_size_ratio(0.85)))
                            .text_color(Theme::text_dim())
                            .child(format!(
                                "{} · {}",
                                engine_label(option.database_type),
                                option.detail
                            )),
                    )
                }),
        )
        .when(option.favorite, |row| {
            row.child(
                div()
                    .font(Theme::ui_font())
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .text_color(Theme::accent())
                    .child("★"),
            )
        })
        .when(option.last_connected_at.is_some(), |row| {
            row.child(
                div()
                    .font(Theme::ui_font())
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .text_color(Theme::text_dim())
                    .child(crate::connection_health::relative_time(
                        option.last_connected_at,
                        now,
                    )),
            )
        })
        .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
            this.connect_profile_id(&id, cx);
        }));

    // gpui animations have no delay knob, so the stagger is a remapped delta:
    // each row starts `index` steps into a shared, longer timeline.
    let anim_id: gpui::SharedString = format!("{row_id}-in").into();
    card.with_animation(
        anim_id,
        Animation::new(Duration::from_millis(
            (220 + index as u64 * ROW_STAGGER_MS).max(220),
        ))
        .with_easing(cubic_bezier(0.16, 1.0, 0.3, 1.0)),
        move |this, delta| {
            let progress = row_stagger_progress(delta, index);
            this.opacity(progress)
                .mt(px(Theme::scaled(4.0) * (1.0 - progress)))
        },
    )
}

/// Milliseconds of extra runway each successive row waits before it moves.
const ROW_STAGGER_MS: u64 = 60;

/// Remaps a 0..1 animation delta so row `index` stays still for its share of
/// the timeline and then plays out over the remainder.
fn row_stagger_progress(delta: f32, index: usize) -> f32 {
    let total = 220.0 + index as f32 * ROW_STAGGER_MS as f32;
    let start = index as f32 * ROW_STAGGER_MS as f32 / total;
    if delta <= start {
        return 0.0;
    }
    ((delta - start) / (1.0 - start)).clamp(0.0, 1.0)
}

/// One row in the welcome shortcut table (label left, shortcut right).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WelcomeShortcut {
    pub label: String,
    pub keycap: String,
}

/// Shortcut rows shown on the welcome canvas. Bindings come from the
/// already-resolved [`WelcomeHints`] so a rebind rewrites the table too.
pub fn welcome_shortcuts(hints: &WelcomeHints) -> Vec<WelcomeShortcut> {
    vec![
        WelcomeShortcut {
            label: crate::localization::text("welcome.shortcuts.run_query"),
            keycap: hints.run.clone(),
        },
        WelcomeShortcut {
            label: crate::localization::text("welcome.shortcuts.command_palette"),
            keycap: hints.palette.clone(),
        },
        WelcomeShortcut {
            label: crate::localization::text("welcome.shortcuts.toggle_sidebar"),
            keycap: hints.sidebar.clone(),
        },
    ]
}

/// Welcome shortcuts render as mono keycap chips, not plain text.
fn shortcut_uses_keycap_chrome() -> bool {
    true
}

/// Keycap chips for one resolved shortcut hint, shared by the welcome canvas
/// and the results empty state. Each alternative from [`split_keycaps`]
/// becomes a kit [`Kbd`] chip (platform-correct symbols, themed chrome). An
/// alternative that does not parse into a keystroke keeps the old hand-built
/// text chip so a hint never disappears; when keycap chrome is off the text
/// renders without the chip dressing.
pub fn render_keycap_chips(text: &str) -> gpui::Div {
    let s = Theme::scaled;
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(s(4.0)))
        .children(split_keycaps(text).into_iter().map(move |cap| {
            if !shortcut_uses_keycap_chrome() {
                return fallback_keycap_chip(cap, false).into_any_element();
            }
            match keycap_keystrokes(&cap) {
                Some(strokes) => div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(2.0)))
                    .children(strokes.into_iter().map(|stroke| {
                        Kbd::new(stroke)
                            .outline()
                            .font_family(Theme::editor_font_family())
                            .text_size(px(Theme::font_size_ratio(0.85)))
                    }))
                    .into_any_element(),
                None => fallback_keycap_chip(cap, true).into_any_element(),
            }
        }))
}

/// Hand-built text chip for a hint that has no keystroke form; `chrome` adds
/// the keycap dressing (panel bg + hairline border) so it sits next to the
/// [`Kbd`] chips without looking broken.
fn fallback_keycap_chip(cap: String, chrome: bool) -> gpui::Div {
    let s = Theme::scaled;
    let chip = div()
        .px(px(s(6.0)))
        .py(px(s(2.0)))
        .rounded(px(s(4.0)))
        .font_family(Theme::editor_font_family())
        .text_size(px(Theme::font_size_ratio(0.85)))
        .text_color(Theme::text_dim())
        .child(cap);
    if chrome {
        chip.bg(Theme::panel_bg())
            .border_1()
            .border_color(Theme::border())
    } else {
        chip
    }
}

fn render_shortcut_table(hints: &WelcomeHints) -> impl gpui::IntoElement {
    let s = Theme::scaled;
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_center()
        .gap(px(s(20.0)))
        .children(welcome_shortcuts(hints).into_iter().map(|row| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(s(6.0)))
                .child(
                    div()
                        .font(Theme::ui_font())
                        .text_size(px(Theme::font_size_ratio(0.85)))
                        .text_color(Theme::text_dim())
                        .child(row.label),
                )
                .child(render_keycap_chips(&row.keycap))
        }))
}

/// Zero-state feature tiles, shown where the connection list would be.
fn render_zero_state_tiles() -> impl gpui::IntoElement {
    let s = Theme::scaled;
    div()
        .flex()
        .flex_row()
        .justify_center()
        .gap(px(s(8.0)))
        .children(zero_state_tiles().into_iter().map(|tile| {
            div()
                .px(px(s(10.0)))
                .py(px(s(6.0)))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(s(6.0)))
                .rounded(px(s(6.0)))
                .bg(Theme::panel_bg())
                .border_1()
                .border_color(Theme::border())
                .child(
                    Icon::empty()
                        .path(tile.icon.path())
                        .size(px(s(14.0)))
                        .text_color(Theme::accent().opacity(0.85)),
                )
                .child(
                    div()
                        .font(Theme::ui_font())
                        .text_size(px(Theme::font_size_ratio(0.85)))
                        .text_color(Theme::text_dim())
                        .child(tile.title),
                )
        }))
}

/// The accent glyph resting on one soft translucent disc.
fn render_hero_glyph() -> impl gpui::IntoElement {
    let s = Theme::scaled;
    div()
        .size(px(s(56.0)))
        .rounded_full()
        .bg(Theme::accent().opacity(0.08))
        .flex()
        .items_center()
        .justify_center()
        .child(
            Icon::empty()
                .path(crate::icon_catalog::AppIcon::Database.path())
                .size(px(s(28.0)))
                .text_color(Theme::accent().opacity(0.85)),
        )
}

/// Render the canvas into whatever region the caller gives it (the whole
/// central column in practice). Row clicks connect through
/// `AppView::connect_profile_id`; the New Connection button opens the same
/// editor the sidebar's add flow uses.
pub fn render_welcome(
    model: &WelcomeModel,
    cx: &mut Context<AppView>,
) -> impl gpui::IntoElement + use<> {
    let s = Theme::scaled;
    let base = px(Theme::font_size());
    let accent_ink = Theme::ink_on(Theme::accent());
    let now = Utc::now();

    let headline = div()
        .font(Theme::ui_font())
        .text_size(px(Theme::font_size_ratio(1.38)))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(Theme::text())
        .child(model.headline());

    let invitation = div()
        .font(Theme::ui_font())
        .text_size(base)
        .text_color(Theme::text_dim())
        .child(model.invitation());

    let rows: Vec<_> = model
        .connections
        .iter()
        .enumerate()
        .map(|(index, option)| render_connection_row(option, index, now, cx).into_any_element())
        .collect();
    let body = if rows.is_empty() {
        div()
            .w_full()
            .mt(px(s(12.0)))
            .child(render_zero_state_tiles())
            .into_any_element()
    } else {
        div()
            .w_full()
            .mt(px(s(12.0)))
            .flex()
            .flex_col()
            .gap(px(s(4.0)))
            .children(rows)
            .into_any_element()
    };

    let actions = WelcomeModel::offers_new_connection().then(|| {
        div()
            .mt(px(s(12.0)))
            .flex()
            .justify_center()
            .child(
                div()
                    .id("welcome-add-connection")
                    .h(px(Theme::control_height()))
                    .px(px(s(14.0)))
                    .flex()
                    .items_center()
                    .justify_center()
                    .gap(px(s(6.0)))
                    .rounded(px(s(4.0)))
                    .cursor_pointer()
                    .text_size(base)
                    .font(Theme::ui_font())
                    // The welcome canvas is the one place a second accent-filled
                    // control could coexist with Run — but Run is hidden while the
                    // canvas shows, so the "one filled control" rule still holds.
                    .bg(Theme::accent())
                    .text_color(accent_ink)
                    .hover(|style| style.bg(Theme::accent().opacity(0.9)))
                    .child(
                        Icon::empty()
                            .path(crate::icon_catalog::AppIcon::Add.path())
                            .size(px(s(14.0)))
                            .text_color(accent_ink),
                    )
                    .child(crate::localization::text("welcome.actions.new_connection"))
                    .on_click(cx.listener(|this, _event: &ClickEvent, window, cx| {
                        this.open_new_connection_editor(window, cx);
                    })),
            )
            .into_any_element()
    });

    let shortcuts = div()
        .mt(px(s(20.0)))
        .child(render_shortcut_table(&model.hints));

    div()
        .size_full()
        .bg(Theme::bg())
        .flex()
        .items_center()
        .justify_center()
        .child(
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(s(6.0)))
                .w(px(s(420.0)))
                .child(render_hero_glyph())
                .child(headline)
                .child(invitation)
                .child(body)
                .children(actions)
                .child(shortcuts),
        )
        // The canvas mounts and unmounts with the connection state, so this
        // plays exactly once per appearance — a soft landing, not a loop.
        .with_animation(
            "welcome-canvas",
            Animation::new(Duration::from_millis(150))
                .with_easing(cubic_bezier(0.16, 1.0, 0.3, 1.0)),
            |this, delta| this.opacity(delta),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::{SecretProvider, SecretRef};

    fn profile(name: &str) -> ConnectionProfile {
        let secret = SecretRef {
            provider: SecretProvider::InMemory,
            key: "k".to_string(),
        };
        ConnectionProfile::sql_login(name, "host.example.com", "sa", secret)
    }

    fn hints() -> WelcomeHints {
        WelcomeHints {
            run: "Cmd+Enter".to_string(),
            palette: "Cmd+Shift+P".to_string(),
            sidebar: "Cmd+B".to_string(),
        }
    }

    #[test]
    fn no_profiles_means_zero_state() {
        let model = welcome_model(&[], hints(), false);
        assert!(!model.has_connections());
        assert_eq!(
            model.invitation(),
            if cfg!(target_arch = "wasm32") {
                crate::localization::text(WASM_DEMO_INVITATION)
            } else {
                crate::localization::text(ZERO_STATE_INVITATION)
            }
        );
    }

    #[test]
    fn profiles_fill_the_list() {
        let profiles = vec![profile("Dev"), profile("Prod")];
        let model = welcome_model(&profiles, hints(), false);
        assert!(model.has_connections());
        assert_eq!(
            model.invitation(),
            if cfg!(target_arch = "wasm32") {
                crate::localization::text(WASM_DEMO_INVITATION)
            } else {
                crate::localization::text(CHOOSE_CONNECTION_INVITATION)
            }
        );
        assert_eq!(model.connections[0].display_name, "Dev");
        assert_eq!(model.connections[1].display_name, "Prod");
    }

    #[test]
    fn list_is_capped() {
        let profiles: Vec<_> = (0..9).map(|i| profile(&format!("conn-{i}"))).collect();
        let model = welcome_model(&profiles, hints(), false);
        assert_eq!(model.connections.len(), MAX_WELCOME_CONNECTIONS);
    }

    #[test]
    fn hints_pass_through() {
        let model = welcome_model(&[], hints(), false);
        assert_eq!(model.hints.run, "Cmd+Enter");
        assert_eq!(model.hints.palette, "Cmd+Shift+P");
        assert_eq!(model.hints.sidebar, "Cmd+B");
    }

    #[test]
    fn headline_is_the_product_name() {
        let model = welcome_model(&[], hints(), false);
        assert_eq!(model.headline(), "SQLLY");
    }

    #[test]
    fn shortcut_table_uses_resolved_hints() {
        let rows = welcome_shortcuts(&hints());
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].label, "Run query");
        assert_eq!(rows[0].keycap, "Cmd+Enter");
        assert_eq!(rows[1].label, "Command palette");
        assert_eq!(rows[1].keycap, "Cmd+Shift+P");
        assert_eq!(rows[2].label, "Toggle sidebar");
        assert_eq!(rows[2].keycap, "Cmd+B");
        assert!(
            shortcut_uses_keycap_chrome(),
            "welcome shortcuts render as keycap chips"
        );
    }

    #[test]
    fn keycaps_split_on_alternatives() {
        assert_eq!(split_keycaps("Cmd+Enter"), vec!["Cmd+Enter".to_string()]);
        assert_eq!(
            split_keycaps("Cmd+Enter, F5"),
            vec!["Cmd+Enter".to_string(), "F5".to_string()]
        );
        assert!(split_keycaps("").is_empty());
    }

    #[test]
    fn keycap_hints_parse_into_keystrokes() {
        let run = keycap_keystrokes("Cmd+Enter").unwrap();
        assert_eq!(run.len(), 1);
        assert!(run[0].modifiers.platform);
        assert_eq!(run[0].key, "enter");

        assert_eq!(keycap_keystrokes("F5").unwrap()[0].key, "f5");

        let palette = keycap_keystrokes("Cmd+Shift+P").unwrap();
        assert!(palette[0].modifiers.platform && palette[0].modifiers.shift);
        assert_eq!(palette[0].key, "p");

        // macOS fn+arrow labels map to the navigation keys they stand for.
        assert_eq!(keycap_keystrokes("fn+↑").unwrap()[0].key, "pageup");
        assert_eq!(keycap_keystrokes("Cmd+fn+←").unwrap()[0].key, "home");
        // Bare arrows and Option map too.
        let opt_left = keycap_keystrokes("Option+←").unwrap();
        assert!(opt_left[0].modifiers.alt);
        assert_eq!(opt_left[0].key, "left");
        // Non-mac multi-word labels survive the chord split.
        assert_eq!(keycap_keystrokes("Shift+Page Up").unwrap()[0].key, "pageup");
        // Two-stroke chords yield one keystroke per stroke.
        assert_eq!(keycap_keystrokes("Cmd+K Cmd+C").unwrap().len(), 2);
        // A trailing '+' is the '+' key itself.
        assert_eq!(keycap_keystrokes("Cmd++").unwrap()[0].key, "+");
        assert_eq!(keycap_keystrokes("Esc").unwrap()[0].key, "escape");

        // Unknown pieces fail whole — the renderer falls back to a text chip.
        assert!(keycap_keystrokes("Cmd+Nope+X").is_none());
        assert!(keycap_keystrokes("").is_none());
    }

    #[test]
    fn environment_chips_skip_the_quiet_defaults() {
        assert!(environment_has_chip(ConnectionEnvironment::Production));
        assert!(environment_has_chip(ConnectionEnvironment::Staging));
        assert!(environment_has_chip(ConnectionEnvironment::Dev));
        assert!(environment_has_chip(ConnectionEnvironment::Test));
        assert!(!environment_has_chip(ConnectionEnvironment::Local));
        assert!(!environment_has_chip(ConnectionEnvironment::Unset));
    }

    #[test]
    fn zero_state_names_three_capabilities() {
        let tiles = zero_state_tiles();
        assert_eq!(tiles.len(), 3);
        assert_eq!(tiles[0].title, "Notebooks");
        assert_eq!(tiles[1].title, "Charts");
        let third = if crate::distribution::can_launch_user_programs() {
            "MCP"
        } else {
            "AI Assistant"
        };
        assert_eq!(tiles[2].title, third);
    }

    #[test]
    fn engine_labels_follow_the_parser_family() {
        assert_eq!(engine_label(DatabaseType::Postgres), "PG");
        assert_eq!(engine_label(DatabaseType::MySql), "MySQL");
        assert_eq!(engine_label(DatabaseType::SqlServer), "MSSQL");
        assert_eq!(engine_label(DatabaseType::Sqlite), "SQLite");
        assert_eq!(engine_label(DatabaseType::DuckDb), "DuckDB");
        assert_eq!(engine_label(DatabaseType::CockroachDb), "PG");
        assert_eq!(engine_label(DatabaseType::Redis), "Redis");
        assert_eq!(engine_label(DatabaseType::Valkey), "Redis");
        assert_eq!(engine_label(DatabaseType::TiDb), "MySQL");
    }

    /// The first-party engines that borrow another family's grammar still
    /// get their own badge: a ClickHouse row must not read "MySQL", nor a
    /// Turso row "SQLite", nor a Redis row "MySQL".
    #[test]
    fn first_party_engines_keep_their_own_badge() {
        assert_eq!(engine_label(DatabaseType::ClickHouse), "ClickHouse");
        assert_eq!(engine_label(DatabaseType::Oracle), "Oracle");
        assert_eq!(engine_label(DatabaseType::LibSql), "libSQL");
        assert_eq!(engine_label(DatabaseType::Redis), "Redis");
        for db in DatabaseType::ALL
            .into_iter()
            .filter(|db| db.has_native_driver())
        {
            let label = engine_label(db);
            assert!(!label.is_empty(), "{db:?}");
            assert!(label.len() <= 10, "{db:?} badge too long: {label}");
        }
    }

    #[test]
    fn row_stagger_delays_later_rows() {
        assert_eq!(row_stagger_progress(0.0, 0), 0.0);
        assert_eq!(row_stagger_progress(1.0, 0), 1.0);
        assert_eq!(row_stagger_progress(0.05, 2), 0.0);
        assert_eq!(row_stagger_progress(1.0, 2), 1.0);
    }

    #[test]
    fn new_connection_is_desktop_only() {
        assert_eq!(
            WelcomeModel::offers_new_connection(),
            !cfg!(target_arch = "wasm32")
        );
    }

    #[test]
    fn proxied_connections_never_claim_to_be_a_demo() {
        let profiles = vec![profile("Northwind (production)")];
        let model = welcome_model(&profiles, hints(), true);
        assert_eq!(
            model.invitation(),
            crate::localization::text(PROXIED_INVITATION)
        );
        assert!(!model.invitation().contains("demo"));
        assert!(!model.invitation().contains("not your servers"));
        // Even with nothing listed yet, a proxied session is never the demo.
        assert_eq!(
            welcome_model(&[], hints(), true).invitation(),
            crate::localization::text(PROXIED_INVITATION)
        );
    }

    #[test]
    fn wasm_demo_invitation_is_labeled_not_production() {
        let invitation = crate::localization::text(WASM_DEMO_INVITATION);
        assert!(
            invitation.contains("in-tab demo"),
            "demo invitation must name the in-tab demo"
        );
        assert!(
            invitation.contains("not your servers"),
            "demo invitation must not imply production servers"
        );
    }
}
