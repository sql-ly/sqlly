//! One tooltip policy for the whole app: a 2-second dwell before anything
//! appears, nothing at all while a context menu is open, and once shown a
//! tooltip stays up only while the pointer rests near where it opened —
//! moving more than 50px away hides it (see `TooltipView` in `app`).
//!
//! Two tooltip systems reach this app. gpui's own (`Interactivity::tooltip`)
//! shows after 500ms and clears on any mouse-down; gpui-component's managed
//! overlay (`Button::tooltip`) shows after its own 500ms and clears only on a
//! **left** mouse-down, so a right-click left its tooltip sitting on top of the
//! menu it just opened. Everything here routes through gpui's, so there is one
//! dwell time, one dismiss rule, and one visual style ([`crate::app::tooltip_view`]).
//!
//! Neither system cancels a tooltip whose show timer is already running when
//! the mouse goes down: the timer fires regardless and paints the tooltip over
//! the freshly-opened menu. That is what [`menus_open`] is for — while a menu is
//! up, tooltips are simply not attached, which drops the pending timers with
//! them.

use std::cell::Cell;
use std::time::Duration;

use gpui::{AnyView, App, InteractiveElement, SharedString, Window};

/// How long the pointer must rest on something before its tooltip appears.
///
/// Deliberately far longer than either framework default (500ms): tooltips in a
/// dense tree or a toolbar of icon buttons were firing on the way past, so the
/// app flickered chips at a user who was only crossing the sidebar.
pub(crate) const TOOLTIP_SHOW_DELAY: Duration = Duration::from_secs(2);

/// How many context menus are currently open. A counter rather than a flag so
/// a menu opened over a menu (a submenu's owner, a dialog over the tree) can't
/// have the inner one's close re-enable tooltips for the outer.
#[derive(Default)]
struct OpenMenus(Cell<usize>);

impl OpenMenus {
    fn any(&self) -> bool {
        self.0.get() > 0
    }

    fn opened(&self) {
        self.0.set(self.0.get() + 1);
    }

    /// Saturates at zero: an unbalanced close must not wrap around into
    /// "suppressed forever".
    fn closed(&self) {
        self.0.set(self.0.get().saturating_sub(1));
    }
}

thread_local! {
    /// Thread-local, not a process global: every window renders, opens menus and
    /// attaches tooltips on the main thread, so the count belongs to that
    /// thread — and each `#[gpui::test]` gets its own, instead of racing the
    /// rest of the suite for one shared counter.
    static OPEN_MENUS: OpenMenus = OpenMenus::default();
}

/// Whether any context menu is currently open, and so whether tooltips are
/// suppressed. Read by [`AppTooltip::app_tooltip`] as each element renders.
pub(crate) fn menus_open() -> bool {
    OPEN_MENUS.with(OpenMenus::any)
}

/// Register that a context menu just opened, and repaint every window so the
/// elements underneath drop their tooltips — including any whose show timer is
/// mid-flight.
///
/// Call from the same place that stores the menu, guarded so one menu counts
/// once. [`menu_closed`] must mirror it.
pub(crate) fn menu_opened(cx: &mut App) {
    OPEN_MENUS.with(OpenMenus::opened);
    cx.refresh_windows();
}

/// Register that a context menu closed. Saturates at zero, so an unbalanced
/// close can never wedge the app into permanently-suppressed tooltips.
pub(crate) fn menu_closed(cx: &mut App) {
    OPEN_MENUS.with(OpenMenus::closed);
    cx.refresh_windows();
}

/// Attach a tooltip under the app's policy.
///
/// Implemented over [`InteractiveElement`] rather than gpui's
/// `StatefulInteractiveElement` so it also covers `gpui_component::Button`,
/// whose own tooltip belongs to the other system. That widens the bound: gpui
/// only keeps tooltip state for elements with an id, so an element without one
/// silently never shows a tooltip. Every element here has an id already.
pub(crate) trait AppTooltip: InteractiveElement + Sized {
    /// Show `build`'s view after [`TOOLTIP_SHOW_DELAY`], unless a context menu
    /// is open — in which case no tooltip is attached at all.
    fn app_tooltip(mut self, build: impl Fn(&mut Window, &mut App) -> AnyView + 'static) -> Self {
        if menus_open() {
            return self;
        }
        self.interactivity().tooltip_show_delay(TOOLTIP_SHOW_DELAY);
        self.interactivity().tooltip(build);
        self
    }

    /// [`Self::app_tooltip`] for the common case of a plain string, rendered as
    /// the app's own tooltip chip.
    fn app_tooltip_text(self, text: impl Into<SharedString>) -> Self {
        let text = text.into();
        self.app_tooltip(move |_window, cx| crate::app::tooltip_view(text.clone(), cx))
    }

    /// [`Self::app_tooltip_text`] for full-sentence content, wrapped at a
    /// readable width instead of running off the window.
    fn app_tooltip_wrapping(self, text: impl Into<SharedString>) -> Self {
        let text = text.into();
        self.app_tooltip(move |_window, cx| crate::app::wrapping_tooltip_view(text.clone(), cx))
    }
}

impl<E: InteractiveElement + Sized> AppTooltip for E {}

#[cfg(test)]
mod tests {
    use super::*;

    /// The counter gates attachment, so its arithmetic is the whole mechanism:
    /// nested menus must not un-suppress early, and an extra close must not
    /// wrap around into "suppressed forever". Exercised on a local instance —
    /// the real one is process-global and tests run in parallel.
    #[test]
    fn open_menu_count_nests_and_saturates() {
        let menus = OpenMenus::default();
        assert!(!menus.any());

        menus.opened();
        menus.opened();
        assert!(menus.any(), "two menus open");

        menus.closed();
        assert!(menus.any(), "the outer menu is still open");
        menus.closed();
        assert!(!menus.any(), "last menu closed: tooltips are back");
        menus.closed();
        assert!(
            !menus.any(),
            "an unbalanced close saturates at zero rather than wrapping"
        );
    }
}
