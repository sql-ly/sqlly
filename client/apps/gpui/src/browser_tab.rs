//! Browser (wasm) build: leaving the app means leaving the tab.
//!
//! gpui's web platform cannot quit (`quit` only logs), and removing the one
//! window just blanks the page. So Quit and the title bar's close button ask
//! the browser to close the tab instead. Browsers honor `window.close()` only
//! for tabs a script opened; when the tab stays open, the app keeps running
//! and a toast says how to close it.

use gpui::{App, Window};
use gpui_component::WindowExt as _;

/// Ask the browser to close this tab, explaining in a toast when it refuses.
pub fn close_tab(window: &mut Window, cx: &mut App) {
    if let Some(dom) = web_sys::window() {
        let _ = dom.close();
    }
    // A closed tab never paints this; one the browser kept open does.
    window.push_notification(
        gpui_component::notification::Notification::info(crate::localization::text(
            "shell.browser.close_tab.manual_close_hint",
        )),
        cx,
    );
}
