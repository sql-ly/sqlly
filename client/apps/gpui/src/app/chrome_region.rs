//! Frame-cached chrome rows.
//!
//! gpui re-runs `render` on every non-cached view each frame, so a
//! results-grid scroll re-laid-out the tab bar, command bar and status bar
//! 120 times a second although nothing in them changed. Each row now lives
//! in a [`ChromeRegion`] view drawn with `Entity::cached`: gpui reuses its
//! layout and paint from the previous frame until the region's own entity
//! is notified.
//!
//! The region still renders the app's own functions (`render_tab_bar`,
//! `render_status_bar`) by updating the [`AppView`] from inside its render,
//! which is sound because a child view renders after its parent's render
//! has returned. Freshness comes from two rules rather than an audit of
//! every input: the region observes the app view, so any app notify
//! re-renders it on the next frame; and every app frame makes sure one
//! refresh timer is pending, which re-renders the regions [`MAX_AGE`] later
//! if any of them sat out a frame in the meantime. That bounds how stale a
//! value read from some other entity (an editor's dirty flag, a picker's
//! selection) can get while the app view itself is not notified. Hover
//! styles and tooltips are unaffected: gpui refreshes the whole window,
//! caches included, when the hovered element changes.
//!
//! The timer is shared and judges staleness by frame number, not per
//! region by time: a refresh re-renders every placed region in one frame,
//! after which the next timer finds nothing stale and the window goes
//! idle. Per-region timers kept each other alive forever, because the
//! frame one region's refresh drew left the other a frame behind.

use std::time::Duration;

use gpui::{
    Context, Entity, IntoElement, Render, Subscription, WeakEntity, Window, div, prelude::*,
};

use super::AppView;

/// Longest a region is drawn from its cache after a frame in which it was
/// not re-rendered (the refresh timer's period). Short enough that a dirty dot or picker label appearing
/// a beat late still reads as immediate; long enough that a scroll rebuilds
/// the chrome five times a second instead of once per frame (each refresh
/// is its own frame when grid repaints are paced, so this also bounds the
/// frames the refresh adds).
pub(crate) const MAX_AGE: Duration = Duration::from_millis(200);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChromeRegionKind {
    /// The query-tab strip and the command bar beneath it.
    TabBar,
    /// The bottom status strip.
    StatusBar,
}

pub(crate) struct ChromeRegion {
    kind: ChromeRegionKind,
    app: WeakEntity<AppView>,
    /// The app frame (`AppView::chrome_frame`) that last put this region in
    /// the element tree; a region that is not on screen is never stale.
    placed_frame: u64,
    /// The app frame this region last rendered in.
    rendered_frame: u64,
    #[cfg(test)]
    renders: usize,
    _app_observer: Subscription,
}

impl ChromeRegion {
    pub(crate) fn new(
        kind: ChromeRegionKind,
        app: &Entity<AppView>,
        cx: &mut Context<Self>,
    ) -> Self {
        let _app_observer = cx.observe(app, |_, _, cx| cx.notify());
        Self {
            kind,
            app: app.downgrade(),
            placed_frame: 0,
            rendered_frame: 0,
            #[cfg(test)]
            renders: 0,
            _app_observer,
        }
    }

    /// Records that app frame `frame` shows this region.
    pub(crate) fn placed(&mut self, frame: u64) {
        self.placed_frame = frame;
    }

    /// Whether app frame `frame` drew this region from its cache.
    fn stale_in(&self, frame: u64) -> bool {
        self.placed_frame == frame && self.rendered_frame != frame
    }
}

impl AppView {
    /// Called at the top of every app render: numbers the frame and makes
    /// sure a refresh timer is pending. When it fires, the regions are
    /// re-rendered only if the latest frame drew one of them from its
    /// cache, so the frame a refresh draws does not schedule another.
    pub(crate) fn begin_chrome_frame(&mut self, cx: &mut Context<Self>) {
        self.chrome_frame = self.chrome_frame.wrapping_add(1);
        if self.chrome_refresh_pending {
            return;
        }
        self.chrome_refresh_pending = true;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(MAX_AGE).await;
            let _ = this.update(cx, |app, cx| {
                app.chrome_refresh_pending = false;
                let frame = app.chrome_frame;
                let regions = [app.tab_bar_region.clone(), app.status_bar_region.clone()];
                let stale = regions
                    .iter()
                    .flatten()
                    .any(|region| region.read(cx).stale_in(frame));
                if stale {
                    // All of them, so they share one frame and stay in step.
                    for region in regions.iter().flatten() {
                        region.update(cx, |_, cx| cx.notify());
                    }
                }
            });
        })
        .detach();
    }
}

impl Render for ChromeRegion {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        #[cfg(test)]
        {
            self.renders += 1;
        }
        let kind = self.kind;
        let inner = self.app.upgrade().map(|app| {
            app.update(cx, |app, cx| {
                self.rendered_frame = app.chrome_frame;
                match kind {
                    ChromeRegionKind::TabBar => app.render_tab_bar(window, cx).into_any_element(),
                    ChromeRegionKind::StatusBar => app.render_status_bar(cx),
                }
            })
        });
        // `size_full`: a cached view's first layout runs its root standalone
        // rather than stretched by the parent, so the root must claim the
        // slot the app view reserves for it or the row collapses to its
        // content width.
        div().size_full().children(inner)
    }
}

#[cfg(test)]
mod tests {
    use super::super::AppView;
    use gpui::{AppContext as _, TestAppContext};
    use std::time::Duration;

    fn open_app(cx: &mut TestAppContext) -> (gpui::Entity<AppView>, &mut gpui::VisualTestContext) {
        crate::ensure_test_tokio_runtime();
        cx.dispatcher.allow_parking();
        cx.update(|cx| {
            if cx.try_global::<gpui_component::Theme>().is_none() {
                gpui_component::init(cx);
            }
            let status = crate::update_status::handle(cx);
            status.update(cx, |s, _| {
                s.state = crate::update_status::UpdateCheckState::Checking;
            });
        });
        let (root, vcx) = cx.add_window_view(|window, cx| {
            let services = sqlly_services::Services::new();
            let app = cx.new(|cx| AppView::new(services, window, cx));
            gpui_component::Root::new(app, window, cx)
        });
        let app = vcx.update(|_, cx| {
            root.read(cx)
                .view()
                .clone()
                .downcast::<AppView>()
                .expect("root hosts the AppView")
        });
        (app, vcx)
    }

    fn renders(app: &gpui::Entity<AppView>, cx: &mut gpui::VisualTestContext) -> (usize, usize) {
        cx.update(|_, cx| {
            let a = app.read(cx);
            (
                a.tab_bar_region.as_ref().map_or(0, |r| r.read(cx).renders),
                a.status_bar_region
                    .as_ref()
                    .map_or(0, |r| r.read(cx).renders),
            )
        })
    }

    fn idle(cx: &mut gpui::VisualTestContext, ms: u64) {
        for _ in 0..ms / 10 {
            cx.executor().advance_clock(Duration::from_millis(10));
            cx.run_until_parked();
        }
    }

    /// An idle window must stop drawing: the refresh timer may not keep
    /// itself (or the other region's) alive once every region is current.
    #[gpui::test]
    fn idle_window_stops_rendering_the_regions(cx: &mut TestAppContext) {
        let (app, cx) = open_app(cx);
        cx.run_until_parked();
        idle(cx, 2_000);
        let settled = renders(&app, cx);
        assert!(settled.0 > 0 && settled.1 > 0, "both regions rendered");
        idle(cx, 5_000);
        assert_eq!(renders(&app, cx), settled, "no renders while idle");
    }

    /// A frame drawn for some other view leaves the regions on their cache;
    /// the timer brings them up to date within `MAX_AGE`, once.
    #[gpui::test]
    fn a_frame_that_skips_the_regions_is_refreshed_once(cx: &mut TestAppContext) {
        let (app, cx) = open_app(cx);
        cx.run_until_parked();
        idle(cx, 2_000);
        let settled = renders(&app, cx);
        cx.update(|_, cx| {
            let menu_bar = app.read(cx).window_menu_bar.clone();
            menu_bar.update(cx, |_, cx| cx.notify());
        });
        cx.run_until_parked();
        assert_eq!(renders(&app, cx), settled, "served from the cache");
        idle(cx, super::MAX_AGE.as_millis() as u64 + 20);
        let refreshed = renders(&app, cx);
        assert_eq!(refreshed, (settled.0 + 1, settled.1 + 1));
        idle(cx, 5_000);
        assert_eq!(renders(&app, cx), refreshed, "and then idle again");
    }
}
