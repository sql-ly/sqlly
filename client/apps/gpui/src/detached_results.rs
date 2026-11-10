//! Detached results window: bounds preference helpers and the main-window placeholder.

use gpui::{AnyElement, App, Bounds, Pixels, Window, div, point, prelude::*, px, size};
use gpui_component::button::Button;

use crate::preferences::DetachedResultsPreferences;
use crate::theme::Theme;

const DEFAULT_DETACHED_WIDTH: f32 = 900.0;
const DEFAULT_DETACHED_HEIGHT: f32 = 600.0;

/// Pure: pick the window bounds for a detach — remembered per tab, else last used, else a 900×600 window centred on `monitor` (clamped so it never opens offscreen).
pub fn initial_bounds(
    prefs: &DetachedResultsPreferences,
    tab_key: &str,
    monitor: Option<Bounds<Pixels>>,
) -> Bounds<Pixels> {
    let remembered = prefs.bounds_for(tab_key).or(prefs.last_bounds);
    match (remembered, monitor) {
        (Some(tuple), Some(mon)) => clamp_into_monitor(bounds_from_tuple(tuple), mon),
        (Some(tuple), None) => bounds_from_tuple(tuple),
        (None, Some(mon)) => clamp_into_monitor(
            centered_on(mon, DEFAULT_DETACHED_WIDTH, DEFAULT_DETACHED_HEIGHT),
            mon,
        ),
        (None, None) => Bounds::new(
            point(px(0.0), px(0.0)),
            size(px(DEFAULT_DETACHED_WIDTH), px(DEFAULT_DETACHED_HEIGHT)),
        ),
    }
}

/// Bounds → the tuple stored in prefs (rounded to whole logical pixels).
pub fn bounds_tuple(b: &Bounds<Pixels>) -> (f32, f32, f32, f32) {
    (
        f32::from(b.origin.x).round(),
        f32::from(b.origin.y).round(),
        f32::from(b.size.width).round(),
        f32::from(b.size.height).round(),
    )
}

fn bounds_from_tuple(t: (f32, f32, f32, f32)) -> Bounds<Pixels> {
    Bounds::new(
        point(px(t.0), px(t.1)),
        size(px(t.2.max(1.0)), px(t.3.max(1.0))),
    )
}

fn centered_on(monitor: Bounds<Pixels>, width: f32, height: f32) -> Bounds<Pixels> {
    let mon_w = f32::from(monitor.size.width).max(1.0);
    let mon_h = f32::from(monitor.size.height).max(1.0);
    let w = width.min(mon_w).max(1.0);
    let h = height.min(mon_h).max(1.0);
    let x = f32::from(monitor.origin.x) + (mon_w - w) / 2.0;
    let y = f32::from(monitor.origin.y) + (mon_h - h) / 2.0;
    Bounds::new(point(px(x), px(y)), size(px(w), px(h)))
}

fn clamp_into_monitor(bounds: Bounds<Pixels>, monitor: Bounds<Pixels>) -> Bounds<Pixels> {
    let mon_x = f32::from(monitor.origin.x);
    let mon_y = f32::from(monitor.origin.y);
    let mon_w = f32::from(monitor.size.width).max(1.0);
    let mon_h = f32::from(monitor.size.height).max(1.0);
    let w = f32::from(bounds.size.width).clamp(1.0, mon_w);
    let h = f32::from(bounds.size.height).clamp(1.0, mon_h);
    let max_x = mon_x + mon_w - w;
    let max_y = mon_y + mon_h - h;
    let x = f32::from(bounds.origin.x).clamp(mon_x.min(max_x), max_x.max(mon_x));
    let y = f32::from(bounds.origin.y).clamp(mon_y.min(max_y), max_y.max(mon_y));
    Bounds::new(point(px(x), px(y)), size(px(w), px(h)))
}

/// The placeholder rendered in the main window while the tab's results live elsewhere.
pub fn render_placeholder(
    on_reattach: impl Fn(&mut Window, &mut App) + 'static,
    _cx: &App,
) -> AnyElement {
    let s = Theme::scaled;
    div()
        .size_full()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(s(12.0)))
        .child(
            div()
                .text_size(px(Theme::font_size()))
                .text_color(Theme::text_dim())
                .child(crate::localization::text(
                    "results.detached.main_window_placeholder",
                )),
        )
        .child(
            Button::new("reattach-results")
                .label(crate::localization::text(
                    "results.detached.reattach_button",
                ))
                .on_click(move |_, window, cx| on_reattach(window, cx)),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(x: f32, y: f32, w: f32, h: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(x), px(y)), size(px(w), px(h)))
    }

    #[test]
    fn initial_bounds_prefers_tab_then_last_then_default() {
        let mut prefs = DetachedResultsPreferences::default();
        prefs.remember("other.sql", (10.0, 20.0, 400.0, 300.0));
        prefs.remember("mine.sql", (50.0, 60.0, 800.0, 500.0));
        let mon = monitor(0.0, 0.0, 1920.0, 1080.0);
        let tab = initial_bounds(&prefs, "mine.sql", Some(mon));
        assert_eq!(bounds_tuple(&tab), (50.0, 60.0, 800.0, 500.0));
        let last = initial_bounds(&prefs, "unknown.sql", Some(mon));
        assert_eq!(bounds_tuple(&last), (50.0, 60.0, 800.0, 500.0));
        let empty = DetachedResultsPreferences::default();
        let def = initial_bounds(&empty, "x", Some(mon));
        assert_eq!(
            bounds_tuple(&def),
            (
                ((1920.0_f32 - 900.0) / 2.0).round(),
                ((1080.0_f32 - 600.0) / 2.0).round(),
                900.0,
                600.0
            )
        );
    }

    #[test]
    fn initial_bounds_clamps_into_monitor() {
        let mut prefs = DetachedResultsPreferences::default();
        prefs.remember("wide.sql", (-100.0, -50.0, 5000.0, 4000.0));
        let mon = monitor(2560.0, 0.0, 1920.0, 1080.0);
        let b = initial_bounds(&prefs, "wide.sql", Some(mon));
        assert_eq!(f32::from(b.size.width), 1920.0);
        assert_eq!(f32::from(b.size.height), 1080.0);
        assert_eq!(f32::from(b.origin.x), 2560.0);
        assert_eq!(f32::from(b.origin.y), 0.0);
    }

    #[test]
    fn bounds_tuple_rounds() {
        let b = Bounds::new(point(px(10.4), px(20.6)), size(px(300.2), px(400.8)));
        assert_eq!(bounds_tuple(&b), (10.0, 21.0, 300.0, 401.0));
    }

    #[test]
    fn prefs_remember_caps_at_200() {
        let mut prefs = DetachedResultsPreferences::default();
        for i in 0..201 {
            prefs.remember(&format!("tab-{i:03}"), (i as f32, 0.0, 100.0, 100.0));
        }
        assert_eq!(prefs.bounds_by_tab.len(), 200);
        assert!(prefs.bounds_for("tab-200").is_some());
        assert!(prefs.bounds_for("tab-000").is_none());
        assert_eq!(prefs.last_bounds, Some((200.0, 0.0, 100.0, 100.0)));
    }

    // Windowed smoke (`detached_results_window_hosts_the_same_entity`) is skipped:
    // the gpui test harness here cannot open a second OS window (`cx.open_window`
    // is a platform path, not `cx.add_window_view`). Cross-window hosting follows
    // the DDL popup pattern (`Root::new` of an entity created in the main window).
}
