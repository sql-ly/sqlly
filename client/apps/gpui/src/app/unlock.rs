//! Workbench wiring for the Mac App Store edition's unlock: SQLLY ▸ Unlock
//! SQLLY…, the free-tier saved-connection gate, and routing every "upgrade"
//! surface to the in-app purchase window. The window itself lives in
//! `crate::unlock_dialog`; the limits in `crate::license`.

use super::*;
use crate::app_store_edition_dialog::{AppStoreEditionDialog, AppStoreEditionEvent};
use crate::unlock_dialog::{UnlockDialog, UnlockDialogEvent};

impl AppView {
    /// SQLLY ▸ Unlock SQLLY… / command palette.
    pub(super) fn handle_unlock_sqlly(
        &mut self,
        _action: &UnlockSqlly,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !crate::distribution::license_checks_enabled() {
            self.notify_feature_disabled(
                crate::localization::text("license.disabled.standalone_build"),
                cx,
            );
            return;
        }
        self.open_unlock_dialog(None, cx);
    }

    /// Help ▸ Privacy Policy.
    pub(super) fn handle_privacy_policy(
        &mut self,
        _action: &PrivacyPolicy,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        let _ = crate::browser_open::open_in_browser(crate::license::PRIVACY_URL);
    }

    /// Help ▸ Mac App Store Edition / command palette.
    pub(super) fn handle_app_store_edition(
        &mut self,
        _action: &AppStoreEdition,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_app_store_edition_dialog(cx);
    }

    fn open_app_store_edition_dialog(&mut self, cx: &mut Context<Self>) {
        if self.overlays.app_store_edition_dialog.is_some() {
            return;
        }
        let view = cx.new(|_cx| AppStoreEditionDialog::new());
        cx.subscribe(
            &view,
            |this: &mut Self, _view, event: &AppStoreEditionEvent, cx| match event {
                AppStoreEditionEvent::Closed => {
                    this.overlays.app_store_edition_dialog = None;
                    cx.notify();
                }
            },
        )
        .detach();
        self.overlays.app_store_edition_dialog = Some(view);
        cx.notify();
    }

    /// First launch of the App Store edition: say up front what the sandbox
    /// changes, once (never under `cargo test`).
    pub(super) fn show_app_store_edition_notice_once(&mut self, cx: &mut Context<Self>) {
        // Debug builds only: open Unlock SQLLY at launch for screenshots.
        #[cfg(all(debug_assertions, not(test)))]
        if std::env::var_os("SQLLY_DEBUG_OPEN_UNLOCK").is_some() {
            self.open_unlock_dialog(None, cx);
            return;
        }
        if cfg!(test) || !crate::app_store_edition_dialog::first_launch_notice_due() {
            return;
        }
        crate::app_store_edition_dialog::record_first_launch_notice();
        self.open_app_store_edition_dialog(cx);
    }

    /// Open the unlock window, optionally explaining which limit sent the
    /// user there. Re-opening while it is up keeps the existing window.
    pub(super) fn open_unlock_dialog(&mut self, reason: Option<String>, cx: &mut Context<Self>) {
        if self.overlays.unlock_dialog.is_some() {
            return;
        }
        let view = cx.new(|cx| UnlockDialog::new(reason, cx));
        cx.subscribe(
            &view,
            |this: &mut Self, _view, event: &UnlockDialogEvent, cx| match event {
                UnlockDialogEvent::Closed => {
                    this.overlays.unlock_dialog = None;
                    cx.notify();
                }
                UnlockDialogEvent::Unlocked => {
                    this.show_nag_dialog = false;
                    this.set_status(
                        StatusLevel::Success,
                        crate::localization::text("unlock.status.unlocked_short"),
                    );
                    cx.notify();
                }
            },
        )
        .detach();
        self.overlays.unlock_dialog = Some(view);
        cx.notify();
    }

    /// Where every "buy a license" surface goes: the web store in the
    /// direct-download edition, the in-app purchase window in the App Store
    /// edition (which may not link out to other ways to pay).
    pub(super) fn open_upgrade_surface(&mut self, cx: &mut Context<Self>) {
        if crate::distribution::sells_through_web_store() {
            let _ = crate::browser_open::open_in_browser(crate::license::supporter_link_url());
        } else {
            self.open_unlock_dialog(None, cx);
        }
    }

    /// The free-tier gate in front of anything that adds saved connections.
    /// Returns true when `adding` more fit; otherwise opens the unlock window
    /// explaining the limit and returns false.
    pub(super) fn ensure_room_for_saved_connections(
        &mut self,
        adding: usize,
        cx: &mut Context<Self>,
    ) -> bool {
        let limit = crate::license::saved_connection_limit_now();
        let existing = ConnectionStore::global().list().len();
        if crate::license::fits_saved_connection_limit(limit, existing, adding) {
            return true;
        }
        if let Some(limit) = limit {
            self.open_unlock_dialog(Some(crate::license::connection_limit_reason(limit)), cx);
        }
        false
    }

    /// The free-tier gate in front of opening a saved connection: only the
    /// first [`crate::license::FREE_SAVED_CONNECTIONS`] saved connections open
    /// on the free tier (extras can arrive through an import, or remain after
    /// a refund). Unsaved/ad-hoc profiles are never blocked.
    pub(super) fn ensure_saved_connection_usable(
        &mut self,
        profile_id: uuid::Uuid,
        cx: &mut Context<Self>,
    ) -> bool {
        let limit = crate::license::saved_connection_limit_now();
        let Some(position) = ConnectionStore::global()
            .list()
            .iter()
            .position(|p| p.id.0 == profile_id)
        else {
            return true;
        };
        if crate::license::saved_connection_usable(limit, position) {
            return true;
        }
        if let Some(limit) = limit {
            self.open_unlock_dialog(Some(crate::license::connection_limit_reason(limit)), cx);
        }
        false
    }
}

/// For surfaces outside the workbench view (status bar): route an "upgrade"
/// click like [`AppView::open_upgrade_surface`].
pub(crate) fn open_upgrade_surface_from(window: &mut Window, cx: &mut App) {
    if crate::distribution::sells_through_web_store() {
        let _ = crate::browser_open::open_in_browser(crate::license::supporter_link_url());
    } else {
        window.dispatch_action(Box::new(UnlockSqlly), cx);
    }
}
