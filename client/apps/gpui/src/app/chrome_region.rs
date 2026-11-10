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
//! re-renders it on the next frame; and the app's render arms a one-shot
//! timer so that after any frame the region re-renders within [`MAX_AGE`].
//! That bounds how stale a value read from some other entity (an editor's
//! dirty flag, a picker's selection) can get while the app view itself is
//! not notified. Hover styles and tooltips are unaffected: gpui refreshes
//! the whole window, caches included, when the hovered element changes.

use std::time::{Duration, Instant};

use gpui::{
    App, Context, Entity, IntoElement, Render, Subscription, WeakEntity, Window, div, prelude::*,
};

use super::AppView;

/// Longest a region is drawn from its cache after a frame in which it was
/// not re-rendered. Short enough that a dirty dot or picker label appearing
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
    last_render: Instant,
    /// The `last_render` a refresh timer was armed for: one timer per
    /// render, so an idle window does not re-arm itself forever.
    armed_for: Option<Instant>,
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
            last_render: Instant::now(),
            armed_for: None,
            _app_observer,
        }
    }

    /// Called from the app view's render on every frame. Arms one timer per
    /// region render that notifies the region when [`MAX_AGE`] has passed
    /// since that render, so whatever this frame was drawn for reaches the
    /// region within that bound even when nothing notifies the app view.
    pub(crate) fn keep_fresh(region: &Entity<Self>, cx: &mut App) {
        let (last_render, armed_for) = {
            let r = region.read(cx);
            (r.last_render, r.armed_for)
        };
        if armed_for == Some(last_render) {
            return;
        }
        let delay = MAX_AGE.saturating_sub(last_render.elapsed());
        region.update(cx, |r, cx| {
            r.armed_for = Some(last_render);
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(delay).await;
                let _ = this.update(cx, |_, cx| cx.notify());
            })
            .detach();
        });
    }
}

impl Render for ChromeRegion {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.last_render = Instant::now();
        let kind = self.kind;
        let inner = self.app.upgrade().map(|app| {
            app.update(cx, |app, cx| match kind {
                ChromeRegionKind::TabBar => app.render_tab_bar(window, cx).into_any_element(),
                ChromeRegionKind::StatusBar => app.render_status_bar(cx),
            })
        });
        // `size_full`: a cached view's first layout runs its root standalone
        // rather than stretched by the parent, so the root must claim the
        // slot the app view reserves for it or the row collapses to its
        // content width.
        div().size_full().children(inner)
    }
}
