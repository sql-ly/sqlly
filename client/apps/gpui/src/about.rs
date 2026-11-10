//! About panel + third-party attributions catalog.
//!
//! Port of `Attributions.swift` (UI-free catalog of credited open-source
//! projects) and `AttributionsController.swift` (the scrollable Acknowledgments
//! view). `AttributionsController` used an `NSTextView` with attributed strings;
//! here we render the same content with GPUI elements inside a dedicated window.
//!
//! The catalog (`Credit`, `Group`, [`header`], [`intro`], [`groups`],
//! [`all_credits`]) is deliberately UI-free so the credited list stays
//! unit-testable and easy to audit as dependencies change (per AGENTS.md test
//! mandate). Only *direct* dependencies of the Rust workspace are listed, plus
//! the bundled visual assets (e.g. the Hugeicons icon set) that ship in the app.

use gpui::{
    App, Bounds, Context, IntoElement, MouseButton, Render, SharedString, Styled, TitlebarOptions,
    Window, WindowBounds, WindowOptions, div, prelude::*, px, size,
};
use gpui_component::Disableable as _;
use gpui_component::button::Button;

use crate::theme::Theme;

/// A single credited open-source project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credit {
    /// Package / crate name as published.
    pub name: &'static str,
    /// Major version line SQLLY depends on (e.g. "1", "0.12").
    pub version: &'static str,
    /// SPDX-style license identifier (e.g. "MIT OR Apache-2.0").
    pub license: &'static str,
    /// Canonical project home (crates.io / repository).
    pub url: &'static str,
    /// One-line description of why SQLLY uses it.
    pub purpose: &'static str,
}

/// A titled grouping of related credits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub title: &'static str,
    pub credits: &'static [Credit],
}

pub fn header() -> String {
    crate::localization::text("about.acknowledgments_header")
}

pub fn intro() -> String {
    crate::localization::text("about.acknowledgments_intro")
}

impl Group {
    pub fn title_key(&self) -> &'static str {
        match self.title {
            "Asynchronous Runtime & Concurrency" => "about.groups.async_runtime",
            "Database Connectivity" => "about.groups.database_connectivity",
            "Serialization & Data" => "about.groups.serialization_data",
            "Utilities" => "about.groups.utilities",
            "Diagnostics" => "about.groups.diagnostics",
            "Platform" => "about.groups.platform",
            "Icons & Assets" => "about.groups.icons_assets",
            _ => unreachable!("every credit group has a localization key"),
        }
    }
}

impl Credit {
    pub fn purpose_key(&self) -> &'static str {
        match self.name {
            "tokio" => "about.purposes.tokio",
            "tokio-util" => "about.purposes.tokio_util",
            "futures" => "about.purposes.futures",
            "async-trait" => "about.purposes.async_trait",
            "parking_lot" => "about.purposes.parking_lot",
            "dashmap" => "about.purposes.dashmap",
            "tiberius" => "about.purposes.tiberius",
            "tokio-postgres" => "about.purposes.tokio_postgres",
            "mysql_async" => "about.purposes.mysql_async",
            "rusqlite" => "about.purposes.rusqlite",
            "duckdb" => "about.purposes.duckdb_crate",
            "oracle" => "about.purposes.oracle",
            "reqwest" => "about.purposes.reqwest",
            "redis" => "about.purposes.redis",
            "serde" => "about.purposes.serde",
            "serde_json" => "about.purposes.serde_json",
            "indexmap" => "about.purposes.indexmap",
            "bitflags" => "about.purposes.bitflags",
            "smallvec" => "about.purposes.smallvec",
            "bytes" => "about.purposes.bytes",
            "uuid" => "about.purposes.uuid",
            "chrono" => "about.purposes.chrono",
            "anyhow" => "about.purposes.anyhow",
            "thiserror" => "about.purposes.thiserror",
            "clap" => "about.purposes.clap",
            "tracing" => "about.purposes.tracing",
            "tracing-subscriber" => "about.purposes.tracing_subscriber",
            "criterion" => "about.purposes.criterion",
            "Rust" => "about.purposes.rust",
            "SQLite" => "about.purposes.sqlite",
            "DuckDB" => "about.purposes.duckdb_platform",
            "ODPI-C" => "about.purposes.odpi_c",
            "Hugeicons" => "about.purposes.hugeicons",
            "GPUI" => "about.purposes.gpui",
            "gpui-component" => "about.purposes.gpui_component",
            "Lucide" => "about.purposes.lucide",
            "JetBrains Mono" => "about.purposes.jetbrains_mono",
            "Fira Code" => "about.purposes.fira_code",
            "Noto Sans" => "about.purposes.noto_sans",
            _ => unreachable!("every credit has a localization key"),
        }
    }

    pub fn localized_version(&self) -> String {
        match (self.name, self.version) {
            ("SQLite", "bundled") => crate::localization::text("about.versions.sqlite_bundled"),
            ("DuckDB", version) => crate::localization::format(
                "about.versions.duckdb_bundled",
                &[("version", version)],
            ),
            ("ODPI-C", "bundled") => crate::localization::text("about.versions.odpi_bundled"),
            ("Hugeicons", "free set") => {
                crate::localization::text("about.versions.hugeicons_free_set")
            }
            _ => self.version.to_string(),
        }
    }
}

/// The application name shown in the About panel.
pub const APP_NAME: &str = "SQLLY";

/// Identifier every window reports to the desktop environment. On Linux this
/// is the Wayland `app_id` / X11 `WM_CLASS`, and it is the only link between a
/// window and its installed desktop entry — it must stay equal to the basename
/// of `io.zebrasoft.sqlly.desktop` and of the hicolor PNGs, or the icon lookup
/// fails. Reverse-DNS per freedesktop convention; the build scripts and the
/// release pipeline hardcode the same string. No-op on macOS and Windows,
/// which take their icon from the bundle and the .exe respectively.
pub const APP_ID: &str = "io.zebrasoft.sqlly";

/// The application version shown in the About panel, taken from the crate
/// manifest at compile time.
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// All credited projects, grouped by role. Ordered most user-visible first.
///
/// Mirrors `Attributions.groups` in `Attributions.swift`. Kept as a `&'static`
/// slice so the catalog is fully UI-free and trivially testable.
pub static GROUPS: &[Group] = &[
    Group {
        title: "Asynchronous Runtime & Concurrency",
        credits: &[
            Credit {
                name: "tokio",
                version: "1",
                license: "MIT",
                url: "https://github.com/tokio-rs/tokio",
                purpose: "Asynchronous runtime powering the query engine and TDS networking.",
            },
            Credit {
                name: "tokio-util",
                version: "0.7",
                license: "MIT",
                url: "https://github.com/tokio-rs/tokio",
                purpose: "Compatibility and codec utilities layered on top of Tokio.",
            },
            Credit {
                name: "futures",
                version: "0.3",
                license: "MIT OR Apache-2.0",
                url: "https://github.com/rust-lang/futures-rs",
                purpose: "Core async abstractions for composing concurrent work.",
            },
            Credit {
                name: "async-trait",
                version: "0.1",
                license: "MIT OR Apache-2.0",
                url: "https://github.com/dtolnay/async-trait",
                purpose: "Ergonomic async methods on traits across the engine.",
            },
            Credit {
                name: "parking_lot",
                version: "0.12",
                license: "MIT OR Apache-2.0",
                url: "https://github.com/Amanieu/parking_lot",
                purpose: "Fast, compact synchronization primitives.",
            },
            Credit {
                name: "dashmap",
                version: "6",
                license: "MIT",
                url: "https://github.com/xacrimon/dashmap",
                purpose: "Concurrent hash map for shared engine state.",
            },
        ],
    },
    Group {
        title: "Database Connectivity",
        credits: &[
            Credit {
                name: "tiberius",
                version: "0.12",
                license: "MIT OR Apache-2.0",
                url: "https://github.com/prisma/tiberius",
                purpose: "Native TDS client for talking to Microsoft SQL Server.",
            },
            Credit {
                name: "tokio-postgres",
                version: "0.7",
                license: "MIT OR Apache-2.0",
                url: "https://github.com/sfackler/rust-postgres",
                purpose: "Native PostgreSQL wire client behind PostgreSQL and its compatible services.",
            },
            Credit {
                name: "mysql_async",
                version: "0.37",
                license: "MIT OR Apache-2.0",
                url: "https://github.com/blackbeam/mysql_async",
                purpose: "Native MySQL wire client behind MySQL, MariaDB and their compatible services.",
            },
            Credit {
                name: "rusqlite",
                version: "0.40",
                license: "MIT",
                url: "https://github.com/rusqlite/rusqlite",
                purpose: "Bundled SQLite bindings for SQLite connections and the local model engine.",
            },
            Credit {
                name: "duckdb",
                version: "1.10505 (DuckDB 1.5.5)",
                license: "MIT",
                url: "https://github.com/duckdb/duckdb-rs",
                purpose: "Bundled DuckDB bindings for in-process DuckDB connections.",
            },
            Credit {
                name: "oracle",
                version: "0.6",
                license: "UPL-1.0 OR Apache-2.0",
                url: "https://github.com/kubo/rust-oracle",
                purpose: "ODPI-C bindings for Oracle Database; loads the free Oracle Instant Client at connect time.",
            },
            Credit {
                name: "reqwest",
                version: "0.13",
                license: "MIT OR Apache-2.0",
                url: "https://github.com/seanmonstar/reqwest",
                purpose: "HTTP client carrying the ClickHouse HTTP interface and the libSQL / Turso Hrana pipeline.",
            },
            Credit {
                name: "redis",
                version: "1.7",
                license: "BSD-3-Clause",
                url: "https://github.com/redis-rs/redis-rs",
                purpose: "RESP client for Redis, Valkey, KeyDB, DragonflyDB, Amazon ElastiCache and MemoryDB connections.",
            },
        ],
    },
    Group {
        title: "Serialization & Data",
        credits: &[
            Credit {
                name: "serde",
                version: "1",
                license: "MIT OR Apache-2.0",
                url: "https://github.com/serde-rs/serde",
                purpose: "Framework for serializing and deserializing engine data.",
            },
            Credit {
                name: "serde_json",
                version: "1",
                license: "MIT OR Apache-2.0",
                url: "https://github.com/serde-rs/json",
                purpose: "JSON support for the client/engine protocol.",
            },
            Credit {
                name: "indexmap",
                version: "2",
                license: "MIT OR Apache-2.0",
                url: "https://github.com/indexmap-rs/indexmap",
                purpose: "Insertion-ordered maps for deterministic results.",
            },
            Credit {
                name: "bitflags",
                version: "2",
                license: "MIT OR Apache-2.0",
                url: "https://github.com/bitflags/bitflags",
                purpose: "Typed bit flag sets used throughout the parser.",
            },
            Credit {
                name: "smallvec",
                version: "1",
                license: "MIT OR Apache-2.0",
                url: "https://github.com/servo/rust-smallvec",
                purpose: "Stack-allocated small vectors on hot tokenizer paths.",
            },
            Credit {
                name: "bytes",
                version: "1",
                license: "MIT",
                url: "https://github.com/tokio-rs/bytes",
                purpose: "Efficient byte buffer handling for network I/O.",
            },
        ],
    },
    Group {
        title: "Utilities",
        credits: &[
            Credit {
                name: "uuid",
                version: "1",
                license: "MIT OR Apache-2.0",
                url: "https://github.com/uuid-rs/uuid",
                purpose: "Unique identifiers for sessions and requests.",
            },
            Credit {
                name: "chrono",
                version: "0.4",
                license: "MIT OR Apache-2.0",
                url: "https://github.com/chronotope/chrono",
                purpose: "Date and time handling for query results and logging.",
            },
            Credit {
                name: "anyhow",
                version: "1",
                license: "MIT OR Apache-2.0",
                url: "https://github.com/dtolnay/anyhow",
                purpose: "Ergonomic error handling across the engine.",
            },
            Credit {
                name: "thiserror",
                version: "2",
                license: "MIT OR Apache-2.0",
                url: "https://github.com/dtolnay/thiserror",
                purpose: "Derive macros for structured library errors.",
            },
            Credit {
                name: "clap",
                version: "4",
                license: "MIT OR Apache-2.0",
                url: "https://github.com/clap-rs/clap",
                purpose: "Command-line argument parsing for the SQLLY CLI.",
            },
        ],
    },
    Group {
        title: "Diagnostics",
        credits: &[
            Credit {
                name: "tracing",
                version: "0.1",
                license: "MIT",
                url: "https://github.com/tokio-rs/tracing",
                purpose: "Structured, contextual logging for the engine.",
            },
            Credit {
                name: "tracing-subscriber",
                version: "0.3",
                license: "MIT",
                url: "https://github.com/tokio-rs/tracing",
                purpose: "Collects and formats engine trace output.",
            },
            Credit {
                name: "criterion",
                version: "0.8",
                license: "MIT OR Apache-2.0",
                url: "https://github.com/bheisler/criterion.rs",
                purpose: "Statistics-driven benchmarking for performance-critical paths.",
            },
        ],
    },
    Group {
        title: "Platform",
        credits: &[
            Credit {
                name: "Rust",
                version: "1.85+",
                license: "MIT OR Apache-2.0",
                url: "https://www.rust-lang.org",
                purpose: "Systems language the SQLLY engine and CLI are written in.",
            },
            Credit {
                name: "SQLite",
                version: "bundled",
                license: "Public Domain",
                url: "https://www.sqlite.org",
                purpose: "Embedded database engine bundled via rusqlite.",
            },
            Credit {
                name: "DuckDB",
                version: "1.5.5 (bundled)",
                license: "MIT",
                url: "https://duckdb.org",
                purpose: "Embedded analytical database engine bundled via the duckdb crate.",
            },
            Credit {
                name: "ODPI-C",
                version: "bundled",
                license: "UPL-1.0 OR Apache-2.0",
                url: "https://github.com/oracle/odpi",
                purpose: "Oracle's C driver layer built into the oracle crate; the Oracle Instant Client it loads is installed separately.",
            },
            Credit {
                name: "GPUI",
                version: "0.3",
                license: "Apache-2.0",
                url: "https://www.gpui.rs",
                purpose: "GPU-accelerated UI framework from Zed Industries that draws every SQLLY window.",
            },
            Credit {
                name: "gpui-component",
                version: "0.9",
                license: "Apache-2.0",
                url: "https://github.com/longbridge/gpui-component",
                purpose: "Longbridge's widget library for GPUI: inputs, menus, dialogs, tables and the editor surface.",
            },
        ],
    },
    Group {
        title: "Icons & Assets",
        credits: &[
            Credit {
                name: "Hugeicons",
                version: "free set",
                license: "MIT",
                url: "https://hugeicons.com",
                purpose: "Icon set every SQLLY app renders, vendored from the free collection.",
            },
            Credit {
                name: "Lucide",
                version: "1.43",
                license: "ISC",
                url: "https://lucide.dev",
                purpose: "Icons drawn by gpui-component's widgets and the browser edition; some derive from Feather (MIT, Cole Bemis).",
            },
            Credit {
                name: "JetBrains Mono",
                version: "2.304",
                license: "OFL-1.1",
                url: "https://github.com/JetBrains/JetBrainsMono",
                purpose: "Bundled monospaced editor font.",
            },
            Credit {
                name: "Fira Code",
                version: "6.002",
                license: "OFL-1.1",
                url: "https://github.com/tonsky/FiraCode",
                purpose: "Bundled monospaced editor font with ligatures.",
            },
            Credit {
                name: "Noto Sans",
                version: "2.015 / SC 2.004",
                license: "OFL-1.1",
                url: "https://notofonts.github.io",
                purpose: "Interface fonts (Latin and Simplified Chinese subsets) embedded in the browser edition.",
            },
        ],
    },
];

/// All credited projects, flattened in group order. Convenient for tests/search.
pub fn all_credits() -> Vec<&'static Credit> {
    GROUPS.iter().flat_map(|g| g.credits.iter()).collect()
}

/// The about/attributions panel rendered in its own window.
pub struct AboutPanel {
    /// The app-wide shared update status; the panel renders whatever it holds
    /// and asks it to re-check, so this indicator and the workspace window's
    /// chip can never disagree (and never fetch twice).
    update: gpui::Entity<crate::update_status::UpdateStatus>,
}

impl AboutPanel {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let update = crate::update_status::handle(cx);
        cx.observe(&update, |_, _, cx| cx.notify()).detach();
        // Kick off a check as soon as the panel opens so the indicator is
        // fresh without the user having to press the button (coalesced if a
        // check is already in flight).
        if crate::distribution::checks_for_updates() {
            update.update(cx, |status, cx| status.check(cx));
        }
        Self { update }
    }
}

impl Render for AboutPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let body = div()
            .id("about-body")
            .flex()
            .flex_col()
            .w_full()
            .h_full()
            .overflow_y_scroll()
            .px(px(s(32.0)))
            .py(px(s(24.0)))
            .gap(px(s(14.0)))
            .child(self.render_version_block(cx))
            .child(
                div()
                    .text_size(px(Theme::font_size() * 1.5))
                    .text_color(Theme::text())
                    .child(header()),
            )
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.87)))
                    .text_color(Theme::text_dim())
                    .child(intro()),
            )
            .children(GROUPS.iter().map(render_group));

        div()
            .size_full()
            .bg(Theme::bg())
            .font(Theme::ui_font())
            .text_color(Theme::text())
            .child(body)
    }
}

impl AboutPanel {
    /// Version, live update indicator (with a re-check button), and a link to
    /// the full license breakdown, shown at the top of the About panel.
    fn render_version_block(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        div()
            .flex()
            .flex_col()
            .gap(px(s(8.0)))
            .pb(px(s(10.0)))
            .border_b_1()
            .border_color(Theme::border())
            .child(
                div()
                    .text_size(px(Theme::font_size() * 1.5))
                    .text_color(Theme::text())
                    .child(crate::localization::format(
                        "about.version",
                        &[("app_name", APP_NAME), ("version", APP_VERSION)],
                    )),
            )
            .child(self.render_update_status(cx))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_baseline()
                    .gap(px(s(4.0)))
                    .text_size(px(Theme::font_size_ratio(0.87)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text("about.license_breakdown_prefix"))
                    .child(render_external_link(
                        "about-license-link",
                        "sqlly.app/licenses",
                        crate::license::LICENSES_URL,
                        cx,
                    )),
            )
    }

    /// The update indicator row: a status message reflecting the last check plus
    /// a button to force a fresh one. Download link appears when a newer build
    /// is available.
    fn render_update_status(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        use crate::update_status::UpdateCheckState;
        let s = Theme::scaled;
        // A store edition is updated by its store: name the edition instead
        // of offering an update check or a download.
        if let Some(store) = crate::distribution::updating_store_name() {
            return div()
                .text_size(px(Theme::font_size_ratio(0.87)))
                .text_color(Theme::text_dim())
                .child(crate::localization::format(
                    "about.store_update_status",
                    &[("store_name", store)],
                ))
                .into_any_element();
        }
        let state = self.update.read(cx).state.clone();
        let status: gpui::AnyElement = match &state {
            // Idle only lasts until the constructor's kick-off check registers.
            UpdateCheckState::Idle | UpdateCheckState::Checking => div()
                .text_color(Theme::text_dim())
                .child(crate::localization::text("about.checking_updates"))
                .into_any_element(),
            UpdateCheckState::UpToDate => div()
                .text_color(Theme::success())
                .child(crate::localization::format(
                    "about.up_to_date",
                    &[("version", APP_VERSION)],
                ))
                .into_any_element(),
            UpdateCheckState::Available(update) => div()
                .flex()
                .flex_row()
                .items_baseline()
                .gap(px(s(6.0)))
                .child(
                    div()
                        .text_color(Theme::warning())
                        .child(crate::localization::format(
                            "about.update_available",
                            &[("version", &update.latest)],
                        )),
                )
                .child(render_download_action("about-download-update", cx))
                .into_any_element(),
            UpdateCheckState::Failed => div()
                .text_color(Theme::text_dim())
                .child(crate::localization::text(
                    "about.update_service_unreachable",
                ))
                .into_any_element(),
        };
        let checking = matches!(state, UpdateCheckState::Idle | UpdateCheckState::Checking);
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(s(10.0)))
            .text_size(px(Theme::font_size_ratio(0.87)))
            .child(status)
            .child(
                Button::new("about-check-update")
                    .label(if checking {
                        crate::localization::text("about.checking_updates_button")
                    } else {
                        crate::localization::text("about.check_updates_button")
                    })
                    .disabled(checking)
                    .on_click(cx.listener(|this, _ev, _window, cx| {
                        this.update.update(cx, |status, cx| status.check(cx));
                    })),
            )
            .into_any_element()
    }
}

/// A clickable "Download" action that opens the OS/arch-specific installer for
/// the newest build in the user's browser.
fn render_download_action<T: 'static>(id: &'static str, cx: &mut Context<T>) -> impl IntoElement {
    div()
        .id(id)
        .cursor_pointer()
        .text_color(Theme::accent())
        .child(crate::localization::text("about.download_update"))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|_this, _ev, _window, _cx| {
                let _ = crate::browser_open::open_in_browser(&crate::store::download_latest_url());
            }),
        )
}

/// An inline external link that opens `url` in the user's browser on click,
/// showing `label` as the accented, clickable text.
fn render_external_link<T: 'static>(
    id: &'static str,
    label: &'static str,
    url: &'static str,
    cx: &mut Context<T>,
) -> impl IntoElement {
    div()
        .id(id)
        .cursor_pointer()
        .text_color(Theme::accent())
        .child(label.to_string())
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |_this, _ev, _window, _cx| {
                let _ = crate::browser_open::open_in_browser(url);
            }),
        )
}

fn render_group(group: &Group) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(Theme::scaled(4.0)))
        .child(
            div()
                .text_size(px(Theme::font_size()))
                .text_color(Theme::text())
                .child(crate::localization::text(group.title_key())),
        )
        .children(group.credits.iter().map(render_credit))
}

fn render_credit(credit: &Credit) -> impl IntoElement {
    let s = Theme::scaled;
    div()
        .flex()
        .flex_col()
        .gap(px(s(1.0)))
        .pl(px(s(14.0)))
        .child(
            div()
                .flex()
                .flex_row()
                .gap(px(s(6.0)))
                .items_baseline()
                .child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.87)))
                        .text_color(Theme::text())
                        .child(credit.name.to_string()),
                )
                .child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.74)))
                        .text_color(Theme::text_dim())
                        .child(format!(
                            "{} · {}",
                            credit.localized_version(),
                            credit.license
                        )),
                ),
        )
        .child(
            div()
                .text_size(px(Theme::font_size_ratio(0.80)))
                .text_color(Theme::text())
                .child(crate::localization::text(credit.purpose_key())),
        )
        .child(
            div()
                .text_size(px(Theme::font_size_ratio(0.74)))
                .text_color(Theme::accent())
                .child(credit.url.to_string()),
        )
}

/// Opens the About / Acknowledgments panel in its own centered window.
/// Callable from an app-level action handler (receives `&mut App`).
pub fn open_about_window(cx: &mut App) {
    let bounds = Bounds::centered(None, size(px(560.), px(620.)), cx);
    let _ = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some(SharedString::from(crate::localization::text(
                    "about.window_title",
                ))),
                ..Default::default()
            }),
            app_id: Some(APP_ID.into()),
            is_resizable: true,
            ..Default::default()
        },
        |window, cx| {
            crate::localization::register_window_title_key(
                window.window_handle(),
                "about.window_title",
            );
            let panel = cx.new(AboutPanel::new);
            cx.new(|cx| gpui_component::Root::new(panel, window, cx))
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_credit_and_group_has_a_unique_localization_key() {
        let mut keys = std::collections::HashSet::new();
        for group in GROUPS {
            assert!(keys.insert(group.title_key()));
            for credit in group.credits {
                assert!(keys.insert(credit.purpose_key()));
            }
        }
    }

    #[test]
    fn app_version_matches_crate_manifest() {
        // env!("CARGO_PKG_VERSION") is never empty and is semver-ish.
        assert!(!APP_VERSION.is_empty());
        assert!(APP_VERSION.contains('.'));
    }

    #[test]
    fn groups_are_non_empty_and_ordered() {
        assert!(GROUPS.len() >= 5);
        for g in GROUPS {
            assert!(!g.title.is_empty());
            assert!(!g.credits.is_empty());
        }
    }

    #[test]
    fn every_credit_has_populated_fields() {
        for c in all_credits() {
            assert!(!c.name.is_empty(), "empty name");
            assert!(!c.version.is_empty(), "empty version for {}", c.name);
            assert!(!c.license.is_empty(), "empty license for {}", c.name);
            assert!(!c.url.is_empty(), "empty url for {}", c.name);
            assert!(!c.purpose.is_empty(), "empty purpose for {}", c.name);
        }
    }

    #[test]
    fn every_credit_url_is_https() {
        for c in all_credits() {
            assert!(
                c.url.starts_with("https://"),
                "non-https url for {}: {}",
                c.name,
                c.url
            );
        }
    }

    #[test]
    fn no_duplicate_credits_within_a_group() {
        for g in GROUPS {
            let mut seen = std::collections::HashSet::new();
            for c in g.credits {
                assert!(
                    seen.insert(c.name),
                    "duplicate credit {} in {}",
                    c.name,
                    g.title
                );
            }
        }
    }

    #[test]
    fn all_credits_flattens_in_group_order() {
        let flat = all_credits();
        let total: usize = GROUPS.iter().map(|g| g.credits.len()).sum();
        assert_eq!(flat.len(), total);
        // First credit is the first group's first credit.
        assert_eq!(flat[0].name, GROUPS[0].credits[0].name);
    }

    #[test]
    fn known_credits_are_present() {
        let names: Vec<&str> = all_credits().iter().map(|c| c.name).collect();
        assert!(names.contains(&"tiberius"));
        assert!(names.contains(&"tokio"));
        assert!(names.contains(&"rusqlite"));
        assert!(names.contains(&"serde"));
    }

    /// Every native driver crate is credited, so the About panel matches the
    /// engine roster the connection editor offers.
    #[test]
    fn every_native_driver_crate_is_credited() {
        let names: Vec<&str> = all_credits().iter().map(|c| c.name).collect();
        for driver in [
            "tiberius",
            "tokio-postgres",
            "mysql_async",
            "rusqlite",
            "duckdb",
            "oracle",
            "reqwest",
            "redis",
        ] {
            assert!(names.contains(&driver), "missing credit for {driver}");
        }
        assert!(names.contains(&"DuckDB"));
        assert!(names.contains(&"ODPI-C"));
        let reqwest = all_credits()
            .into_iter()
            .find(|c| c.name == "reqwest")
            .unwrap();
        assert!(reqwest.purpose.contains("ClickHouse"));
        assert!(reqwest.purpose.contains("libSQL"));
    }
}
