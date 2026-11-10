//! Open Source Licenses viewer.
//!
//! Help > Open Source Licenses… opens a dedicated window listing every
//! third-party open-source project SQLLY depends on, together with the full
//! verbatim text of the license(s) under which it is distributed.
//!
//! The project catalog is reused from [`crate::about`] (`Credit`, `Group`,
//! [`about::GROUPS`], [`about::all_credits`]) so the credits list and the
//! licenses view can never drift — there is exactly one place to maintain the
//! roster of dependencies. This module owns only the license texts themselves
//! and the rendering of them. Kept UI-free at the data layer (the registry and
//! [`licenses_used`] are pure and unit-tested) per the AGENTS.md test mandate.

use gpui::{
    AnyElement, App, Bounds, Context, CursorStyle, IntoElement, MouseButton, Render, ScrollHandle,
    SharedString, Styled, TitlebarOptions, Window, WindowBounds, WindowOptions, div, prelude::*,
    px, size,
};

use crate::about::{Credit, GROUPS, Group, all_credits};
use crate::theme::Theme;

/// Full text of the MIT License (SPDX: `MIT`).
const MIT_TEXT: &str = include_str!("../resources/licenses/MIT.txt");
/// Full text of the Apache License, Version 2.0 (SPDX: `Apache-2.0`).
const APACHE_2_TEXT: &str = include_str!("../resources/licenses/Apache-2.0.txt");
/// SQLite's public-domain blessing — SQLite is not MIT/Apache licensed.
const SQLITE_BLESSING_TEXT: &str = include_str!("../resources/licenses/SQLite-blessing.txt");
/// Full text of the BSD 3-Clause License (SPDX: `BSD-3-Clause`), as shipped by
/// the redis driver.
const BSD_3_CLAUSE_TEXT: &str = include_str!("../resources/licenses/BSD-3-Clause.txt");
/// Full text of the ISC License (SPDX: `ISC`), as shipped by Lucide.
const ISC_TEXT: &str = include_str!("../resources/licenses/ISC.txt");
/// Full text of the SIL Open Font License 1.1 (SPDX: `OFL-1.1`) the bundled
/// fonts are distributed under.
const OFL_1_1_TEXT: &str = include_str!("../resources/licenses/OFL-1.1.txt");

/// A named license and its verbatim text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LicenseText {
    /// Human-readable license name (e.g. "MIT License").
    pub name: &'static str,
    /// The SPDX identifier this text corresponds to (e.g. "Apache-2.0").
    pub spdx: &'static str,
    /// Full, verbatim license text.
    pub text: &'static str,
}

/// Look up the verbatim license text(s) for an SPDX identifier string.
///
/// Handles the SPDX expressions used by the credited deps: bare identifiers
/// (`MIT`, `Public Domain`) and the `MIT OR Apache-2.0` dual-license form. For
/// a dual-licensed dependency both texts are returned so the UI can show each
/// license the project may be used under. Unknown identifiers yield an empty
/// `Vec` so the UI degrades gracefully instead of panicking.
pub fn license_texts(spdx: &str) -> Vec<LicenseText> {
    fn lookup(token: &str) -> Option<LicenseText> {
        match token.trim() {
            "MIT" => Some(LicenseText {
                name: "MIT License",
                spdx: "MIT",
                text: MIT_TEXT,
            }),
            "Apache-2.0" => Some(LicenseText {
                name: "Apache License 2.0",
                spdx: "Apache-2.0",
                text: APACHE_2_TEXT,
            }),
            "Public Domain" => Some(LicenseText {
                name: "Public Domain (SQLite Blessing)",
                spdx: "Public Domain",
                text: SQLITE_BLESSING_TEXT,
            }),
            "BSD-3-Clause" => Some(LicenseText {
                name: "BSD 3-Clause License",
                spdx: "BSD-3-Clause",
                text: BSD_3_CLAUSE_TEXT,
            }),
            "ISC" => Some(LicenseText {
                name: "ISC License",
                spdx: "ISC",
                text: ISC_TEXT,
            }),
            "OFL-1.1" => Some(LicenseText {
                name: "SIL Open Font License 1.1",
                spdx: "OFL-1.1",
                text: OFL_1_1_TEXT,
            }),
            _ => None,
        }
    }
    spdx.split(" OR ").filter_map(lookup).collect()
}

/// Every unique license used by the credited deps, in first-appearance order,
/// paired with the list of credits distributed under it.
///
/// A dual-licensed credit (e.g. `MIT OR Apache-2.0`) appears under *both*
/// licenses, since the user may exercise either. No credit is silently dropped:
/// every credit resolves to at least one [`LicenseText`] (verified by tests).
pub fn licenses_used() -> Vec<(LicenseText, Vec<&'static Credit>)> {
    let mut order: Vec<&'static str> = Vec::new();
    let mut texts: std::collections::HashMap<&'static str, LicenseText> = Default::default();
    let mut deps: std::collections::HashMap<&'static str, Vec<&'static Credit>> =
        Default::default();
    for credit in all_credits() {
        for lt in license_texts(credit.license) {
            let name = lt.name;
            if !texts.contains_key(name) {
                order.push(name);
                texts.insert(name, lt);
                deps.insert(name, Vec::new());
            }
            deps.get_mut(name).unwrap().push(credit);
        }
    }
    order
        .into_iter()
        .filter_map(|name| Some((texts.remove(name)?, deps.remove(name)?)))
        .collect()
}

pub fn header() -> String {
    crate::localization::text("open_source.header")
}

pub fn intro() -> String {
    crate::localization::text("open_source.intro")
}

/// The scrollable Open Source Licenses panel, rendered in its own window.
pub struct OpenSourceLicensesPanel {
    licenses: Vec<(LicenseText, Vec<&'static Credit>)>,
    scroll: ScrollHandle,
}

impl OpenSourceLicensesPanel {
    pub fn new(_cx: &mut Context<Self>) -> Self {
        Self {
            licenses: licenses_used(),
            scroll: ScrollHandle::default(),
        }
    }

    fn copy_license(&self, idx: usize, cx: &mut Context<Self>) {
        if let Some((lt, _)) = self.licenses.get(idx) {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(lt.text.to_string()));
        }
    }
}

impl Render for OpenSourceLicensesPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let deps_section = div()
            .flex()
            .flex_col()
            .gap(px(s(14.0)))
            .child(
                div()
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text())
                    .child(crate::localization::text(
                        "open_source.dependencies_heading",
                    )),
            )
            .children(GROUPS.iter().map(render_dep_group));

        // Build each license block inline so `cx.listener` is invoked directly
        // in render scope (its result cannot escape a capturing map closure).
        // `into_any_element()` erases the element lifetime so the blocks are
        // owned and 'static.
        let mut license_blocks: Vec<AnyElement> = Vec::new();
        for (idx, (lt, deps)) in self.licenses.iter().enumerate() {
            let used_by = deps.iter().map(|c| c.name).collect::<Vec<_>>().join(", ");
            let copy_button = div()
                .px(px(s(8.0)))
                .py(px(s(4.0)))
                .bg(Theme::panel_bg())
                .border_1()
                .border_color(Theme::border())
                .text_color(Theme::text())
                .text_size(px(Theme::font_size_ratio(0.74)))
                .rounded(px(s(4.0)))
                .cursor(CursorStyle::PointingHand)
                .child(crate::localization::text("open_source.copy_license_button"))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _ev, _window, cx| {
                        this.copy_license(idx, cx);
                    }),
                );
            let block = div()
                .flex()
                .flex_col()
                .gap(px(s(4.0)))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(s(8.0)))
                        .items_center()
                        .child(
                            div()
                                .text_size(px(Theme::font_size_ratio(0.92)))
                                .text_color(Theme::text())
                                .child(lt.name.to_string()),
                        )
                        .child(
                            div()
                                .text_size(px(Theme::font_size_ratio(0.74)))
                                .text_color(Theme::text_dim())
                                .child(format!("(SPDX: {})", lt.spdx)),
                        )
                        .child(copy_button),
                )
                .child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.74)))
                        .text_color(Theme::text_dim())
                        .child(crate::localization::format(
                            "open_source.used_by",
                            &[("dependencies", &used_by)],
                        )),
                )
                .child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.74)))
                        .text_color(Theme::text())
                        .font_family(Theme::editor_font_family())
                        .py(px(s(8.0)))
                        .child(lt.text.to_string()),
                );
            license_blocks.push(block.into_any_element());
        }

        let licenses_section = div()
            .flex()
            .flex_col()
            .gap(px(s(16.0)))
            .child(
                div()
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text())
                    .child(crate::localization::text("open_source.licenses_heading")),
            )
            .children(license_blocks);

        let body = div()
            .id("open-source-scroll")
            .flex()
            .flex_col()
            .w_full()
            .h_full()
            .overflow_scroll()
            .track_scroll(&self.scroll)
            .px(px(s(32.0)))
            .py(px(s(24.0)))
            .gap(px(s(20.0)))
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
            .child(deps_section)
            .child(licenses_section);

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(Theme::bg())
            .text_color(Theme::text())
            .child(body)
    }
}

/// Render one dependency group (title + its credits) for the deps list.
fn render_dep_group(group: &Group) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(Theme::scaled(4.0)))
        .child(
            div()
                .text_size(px(Theme::font_size_ratio(0.87)))
                .text_color(Theme::text())
                .child(crate::localization::text(group.title_key())),
        )
        .children(group.credits.iter().map(render_dep))
}

/// Render one credit: name, version, SPDX license id, project url, purpose.
fn render_dep(credit: &Credit) -> impl IntoElement {
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
                .text_size(px(Theme::font_size_ratio(0.74)))
                .text_color(Theme::accent())
                .child(credit.url.to_string()),
        )
        .child(
            div()
                .text_size(px(Theme::font_size_ratio(0.80)))
                .text_color(Theme::text())
                .child(crate::localization::text(credit.purpose_key())),
        )
}

/// Opens the Open Source Licenses panel in its own centered, resizable window.
/// Callable from an app-level action handler (receives `&mut App`).
pub fn open_open_source_window(cx: &mut App) {
    let bounds = Bounds::centered(None, size(px(720.), px(780.)), cx);
    let _ = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some(SharedString::from(crate::localization::text(
                    "open_source.window_title",
                ))),
                ..Default::default()
            }),
            app_id: Some(crate::about::APP_ID.into()),
            is_resizable: true,
            ..Default::default()
        },
        |window, cx| {
            crate::localization::register_window_title_key(
                window.window_handle(),
                "open_source.window_title",
            );
            let panel = cx.new(OpenSourceLicensesPanel::new);
            cx.new(|cx| gpui_component::Root::new(panel, window, cx))
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_and_intro_are_non_empty() {
        assert!(!header().is_empty());
        assert!(intro().contains("SQLLY"));
        assert!(intro().contains("license"));
    }

    #[test]
    fn license_texts_resolves_bare_mit() {
        let texts = license_texts("MIT");
        assert_eq!(texts.len(), 1);
        assert_eq!(texts[0].name, "MIT License");
        assert_eq!(texts[0].spdx, "MIT");
        assert!(texts[0].text.contains("Permission is hereby granted"));
    }

    #[test]
    fn license_texts_resolves_bare_apache() {
        let texts = license_texts("Apache-2.0");
        assert_eq!(texts.len(), 1);
        assert_eq!(texts[0].name, "Apache License 2.0");
        assert_eq!(texts[0].spdx, "Apache-2.0");
        assert!(texts[0].text.contains("Apache License"));
        assert!(texts[0].text.contains("Version 2.0"));
    }

    #[test]
    fn license_texts_resolves_public_domain() {
        let texts = license_texts("Public Domain");
        assert_eq!(texts.len(), 1);
        assert_eq!(texts[0].name, "Public Domain (SQLite Blessing)");
        assert!(texts[0].text.contains("May you do good and not evil"));
    }

    #[test]
    fn license_texts_splits_dual_license() {
        let texts = license_texts("MIT OR Apache-2.0");
        assert_eq!(texts.len(), 2);
        assert_eq!(texts[0].spdx, "MIT");
        assert_eq!(texts[1].spdx, "Apache-2.0");
    }

    #[test]
    fn license_texts_unknown_yields_empty() {
        assert!(license_texts("GPL-3.0-only").is_empty());
        assert!(license_texts("").is_empty());
    }

    #[test]
    fn embedded_license_texts_are_non_empty() {
        assert!(!MIT_TEXT.is_empty());
        assert!(!APACHE_2_TEXT.is_empty());
        assert!(!SQLITE_BLESSING_TEXT.is_empty());
        assert!(ISC_TEXT.starts_with("ISC License"));
        assert!(OFL_1_1_TEXT.starts_with("SIL OPEN FONT LICENSE Version 1.1"));
    }

    #[test]
    fn every_credit_license_resolves() {
        for c in all_credits() {
            let texts = license_texts(c.license);
            assert!(
                !texts.is_empty(),
                "{} has unresolvable license: {}",
                c.name,
                c.license
            );
        }
    }

    #[test]
    fn licenses_used_is_non_empty() {
        let used = licenses_used();
        assert!(!used.is_empty());
        for (lt, deps) in &used {
            assert!(!lt.text.is_empty(), "empty text for {}", lt.name);
            assert!(!deps.is_empty(), "no deps for {}", lt.name);
        }
    }

    #[test]
    fn licenses_used_covers_all_credits() {
        let used = licenses_used();
        let mut covered: Vec<&str> = Vec::new();
        for (_, deps) in &used {
            for d in deps {
                covered.push(d.name);
            }
        }
        for c in all_credits() {
            assert!(
                covered.contains(&c.name),
                "{} not covered by any license block",
                c.name
            );
        }
    }

    #[test]
    fn dual_licensed_credit_appears_under_both_licenses() {
        // `futures` is credited as "MIT OR Apache-2.0".
        let used = licenses_used();
        let mit_deps: Vec<&str> = used
            .iter()
            .find(|(lt, _)| lt.spdx == "MIT")
            .map(|(_, d)| d.iter().map(|c| c.name).collect())
            .unwrap_or_default();
        let apache_deps: Vec<&str> = used
            .iter()
            .find(|(lt, _)| lt.spdx == "Apache-2.0")
            .map(|(_, d)| d.iter().map(|c| c.name).collect())
            .unwrap_or_default();
        assert!(mit_deps.contains(&"futures"), "futures missing from MIT");
        assert!(
            apache_deps.contains(&"futures"),
            "futures missing from Apache-2.0"
        );
    }

    #[test]
    fn mit_and_apache_blocks_both_present() {
        let spdxs: Vec<&str> = licenses_used().iter().map(|(lt, _)| lt.spdx).collect();
        assert!(spdxs.contains(&"MIT"));
        assert!(spdxs.contains(&"Apache-2.0"));
    }
}
