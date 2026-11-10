//! App-wide shared update-check state.
//!
//! One entity holds the latest known answer to "is a newer build available?"
//! and runs the fetch/compare when asked. Both consumers observe it instead of
//! running their own checks — the workspace window (status-bar chip, update
//! dialog, menu action) and the About window's indicator — so the two surfaces
//! can never disagree, and overlapping checks coalesce into one fetch.

use gpui::{App, AppContext as _, Context, Entity, Global};

use crate::app_update::UpdateAvailable;

#[derive(Debug, Clone)]
pub enum UpdateCheckState {
    /// No check has run yet.
    Idle,
    /// A check is in flight.
    Checking,
    /// The installed build is the newest one advertised by the feed.
    UpToDate,
    /// A newer build is available.
    Available(UpdateAvailable),
    /// The feed couldn't be reached (offline, server error, browser build).
    Failed,
}

pub struct UpdateStatus {
    pub state: UpdateCheckState,
}

struct UpdateStatusHandle(Entity<UpdateStatus>);

impl Global for UpdateStatusHandle {}

/// The shared entity, created on first use. Any window can reach it through
/// `&mut App`.
pub fn handle(cx: &mut App) -> Entity<UpdateStatus> {
    if !cx.has_global::<UpdateStatusHandle>() {
        let entity = cx.new(|_| UpdateStatus {
            state: UpdateCheckState::Idle,
        });
        cx.set_global(UpdateStatusHandle(entity));
    }
    cx.global::<UpdateStatusHandle>().0.clone()
}

impl UpdateStatus {
    /// Check for a newer build off the tokio runtime and publish the outcome.
    ///
    /// Two purpose-built endpoints, each doing the least work:
    /// 1. `GET /download/latest/version` — the cheap, authoritative "is a newer
    ///    build downloadable for my platform?" check. On the common up-to-date
    ///    path this is the *only* request, and no changelog prose is fetched.
    /// 2. Only when a newer build exists, `GET /api/changelog/since/{installed}`
    ///    pulls just the diff (releases newer than this build, with their
    ///    changes) for the "what's new" dialog.
    ///
    /// A check already in flight absorbs the request — observers get that check's
    /// result, so a button click during the startup check still surfaces an
    /// answer without a second fetch. How each surface reacts (dialog, toast,
    /// indicator) is the observer's business, tracked on the observer's own state.
    pub fn check(&mut self, cx: &mut Context<Self>) {
        // The App Store edition is updated by the App Store, never by us.
        if !crate::distribution::checks_for_updates()
            || matches!(self.state, UpdateCheckState::Checking)
        {
            return;
        }
        self.state = UpdateCheckState::Checking;
        cx.notify();

        let weak_self = cx.weak_entity();
        let async_cx = cx.to_async();
        cx.spawn(async move |_this, _cx| {
            let outcome = crate::tokio_handle()
                .spawn(async move {
                    let installed = crate::about::APP_VERSION;
                    let latest = crate::store::fetch_latest_version().await?;
                    // No published build for this platform, or nothing newer than
                    // what's installed: definitively up to date.
                    let Some(latest) = latest else {
                        return Ok::<_, crate::store::StoreError>(None);
                    };
                    if !crate::app_update::is_newer(&latest.version, installed) {
                        return Ok(None);
                    }
                    // A newer build exists — fetch only the changes since this
                    // build for the dialog. A changelog hiccup shouldn't hide the
                    // update, so a failure here degrades to "no change list".
                    let since = crate::store::fetch_changelog_since(installed).await.ok();
                    Ok(Some(crate::app_update::build_update(&latest, since)))
                })
                .await;
            let state = match outcome {
                Ok(Ok(Some(update))) => UpdateCheckState::Available(update),
                Ok(Ok(None)) => UpdateCheckState::UpToDate,
                _ => UpdateCheckState::Failed,
            };
            let mut cx = async_cx.clone();
            weak_self
                .update(&mut cx, |this, cx| {
                    this.state = state;
                    cx.notify();
                })
                .ok();
        })
        .detach();
    }
}
