//! The "this is illustrative only" notice shown at the top of dialogs whose
//! real work a browser tab cannot do — connecting to SQL Server, signing in to
//! Azure/Entra, reaching your files, opening an Azure firewall rule.
//!
//! On native builds `preview_banner` returns `None`, so the desktop dialogs
//! render exactly as before and the banner code compiles out entirely.

/// An info banner explaining that the surrounding dialog is illustrative in the
/// browser preview. `Some(..)` on the browser build, `None` on desktop — call
/// sites splice it in with `.children(preview_banner(..))` so the desktop
/// layout is untouched.
#[cfg(target_arch = "wasm32")]
pub fn preview_banner(message: impl Into<gpui::SharedString>) -> Option<gpui::AnyElement> {
    use crate::theme::Theme;
    use gpui::prelude::*;
    use gpui::{div, px};

    let message = message.into();
    Some(
        div()
            .flex()
            .flex_row()
            .items_start()
            .gap(px(Theme::scaled(8.0)))
            .w_full()
            .px(px(Theme::scaled(12.0)))
            .py(px(Theme::scaled(8.0)))
            .bg(Theme::info().opacity(0.12))
            .border_b_1()
            .border_color(Theme::info().opacity(0.4))
            .text_size(px(Theme::font_size_ratio(0.9)))
            .text_color(Theme::text())
            .child(
                div()
                    .flex_shrink_0()
                    .text_color(Theme::info())
                    .child(crate::localization::text("browser.preview.badge")),
            )
            .child(div().flex_1().min_w(px(0.0)).child(message))
            .into_any_element(),
    )
}

/// Desktop build: no banner.
#[cfg(not(target_arch = "wasm32"))]
pub fn preview_banner(_message: impl Into<gpui::SharedString>) -> Option<gpui::AnyElement> {
    None
}

/// Finishes a "<something> is not available" sentence for a feature the
/// browser build cannot offer. The standalone preview points at the desktop
/// app; a page served by a relay proxy or SQLly Embedded bridge just says
/// why, since it is somebody's product and the desktop app may not exist.
#[cfg(target_arch = "wasm32")]
pub fn unavailable(what: &str) -> String {
    let key = if served() {
        "browser.preview.unavailable_served"
    } else {
        "browser.preview.unavailable_standalone"
    };
    crate::localization::format(key, &[("feature", what)])
}

/// Desktop builds never call this; it exists so shared code compiles.
#[cfg(not(target_arch = "wasm32"))]
#[allow(dead_code)]
pub fn unavailable(what: &str) -> String {
    crate::localization::format(
        "browser.preview.unavailable_standalone",
        &[("feature", what)],
    )
}

/// Whether this browser page is served by a relay proxy or SQLly Embedded
/// bridge rather than being the standalone preview. Always false on desktop.
pub fn served() -> bool {
    #[cfg(target_arch = "wasm32")]
    {
        sqlly_services::browser::is_served()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        false
    }
}

/// Picks between two fixed messages for a browser-only limitation: the
/// standalone preview's (which may point at the desktop app) or a served
/// page's. For call sites that need a `&'static str`.
pub fn pick(preview: &'static str, served_message: &'static str) -> &'static str {
    if served() { served_message } else { preview }
}
