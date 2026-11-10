use super::*;
use sqlly_core::connection::ConnectionRoute;
#[cfg(not(target_arch = "wasm32"))]
use sqlly_services::query::{ManualTxState, TransactionEnd};

/// Pure decision fold for manual-mode availability (unit-tested). Reasons
/// are catalog keys, rendered by [`crate::capabilities::Capability::reason`].
pub(crate) fn manual_mode_decision(
    cap: crate::capabilities::Capability,
    effective_select_only: bool,
    forced_wrap: bool,
    via_relay: bool,
) -> crate::capabilities::Capability {
    if via_relay {
        return crate::capabilities::Capability::Unavailable(
            "shell.transactions.manual_relay_unavailable",
        );
    }
    if !cap.is_available() {
        return cap;
    }
    if forced_wrap {
        return crate::capabilities::Capability::Unavailable("shell.transactions.forced_rollback");
    }
    if effective_select_only {
        return crate::capabilities::Capability::Unavailable("shell.transactions.select_only");
    }
    crate::capabilities::Capability::Available
}

impl AppView {
    pub(super) fn read_only_unlock_active(
        &self,
        profile_id: &uuid::Uuid,
        now: sqlly_intel::query::safety::Instant,
    ) -> bool {
        self.read_only_unlocks
            .get(&profile_id.to_string())
            .is_some_and(|u| u.is_active(now))
    }

    pub(super) fn manual_mode_availability(
        &self,
        _tab_idx: usize,
    ) -> crate::capabilities::Capability {
        let Some(profile) = self.active_profile.as_ref() else {
            return crate::capabilities::Capability::Unavailable(
                "shell.transactions.connect_manual",
            );
        };
        let db = profile.database_type;
        let cap = crate::capabilities::manual_transactions(db);
        let store = ConnectionStore::global();
        let now = sqlly_intel::query::safety::Instant::now();
        let safety = crate::mutation_review::SafetyFeatures::effective_for_profile(profile, store);
        let mut effective_select_only = profile.block_all_but_select;
        if !self.read_only_unlock_active(&profile.id.0, now) {
            effective_select_only |= safety.only_allow_select;
        }
        effective_select_only |= safety
            .production_guard()
            .forces_select_only(self.production_unlock.as_ref(), now);
        let forced_wrap = safety.use_outer_transaction_rollback_scope;
        let via_relay = matches!(profile.route, ConnectionRoute::ViaRelay { .. });
        manual_mode_decision(cap, effective_select_only, forced_wrap, via_relay)
    }

    pub(super) fn toggle_transaction_mode(&mut self, tab_idx: usize, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(tab_idx) else {
            return;
        };
        if tab.transaction_mode == crate::tabs::TransactionMode::Manual
            && tab.manual_tx.as_ref().is_some_and(|tx| tx.dirty())
        {
            self.pending.transaction_close = Some(PendingTransactionClose {
                tab_id: tab.id.clone(),
                tab_name: tab.name.clone(),
                statements: tab.manual_tx.as_ref().map(|tx| tx.statements).unwrap_or(0),
                reason: TransactionCloseReason::SwitchToAuto,
            });
            cx.notify();
            return;
        }
        match tab.transaction_mode {
            crate::tabs::TransactionMode::Auto => {
                if let Some(reason) = self.manual_mode_availability(tab_idx).reason() {
                    self.set_status(StatusLevel::Info, reason);
                    cx.notify();
                    return;
                }
                if let Some(tab) = self.tabs.get_mut(tab_idx) {
                    tab.transaction_mode = crate::tabs::TransactionMode::Manual;
                }
                self.set_status(
                    StatusLevel::Info,
                    crate::localization::text("shell.transactions.manual_enabled"),
                );
            }
            crate::tabs::TransactionMode::Manual => {
                self.silent_rollback_tab(tab_idx, cx);
                if let Some(tab) = self.tabs.get_mut(tab_idx) {
                    tab.transaction_mode = crate::tabs::TransactionMode::Auto;
                    tab.manual_tx = None;
                }
                self.set_status(
                    StatusLevel::Info,
                    crate::localization::text("shell.transactions.auto_enabled"),
                );
            }
        }
        cx.notify();
    }

    pub(super) fn run_with_manual_transaction_if_needed(
        &mut self,
        tab_idx: usize,
        profile: ConnectionProfile,
        cx: &mut Context<Self>,
        on_ready: impl FnOnce(&mut Self, ConnectionProfile, &mut Context<Self>) + Send + 'static,
    ) {
        if self.tabs.get(tab_idx).is_none_or(|t| t.manual_tx.is_some()) {
            on_ready(self, profile, cx);
            return;
        }
        #[cfg(target_arch = "wasm32")]
        {
            on_ready(self, profile, cx);
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let tab_id = self.tabs[tab_idx].id.clone();
            let profile_for_begin = profile.clone();
            let services = self.services_for_profile(&profile);
            let weak = cx.weak_entity();
            cx.spawn(async move |_this, cx| {
                let result = crate::tokio_handle()
                    .spawn(async move {
                        services
                            .queries
                            .begin_manual_transaction(&profile_for_begin, &tab_id)
                            .await
                    })
                    .await;
                let _ = weak.update(cx, |app, cx| match result {
                    Ok(Ok(_state)) => {
                        let now = Instant::now();
                        if let Some(tab) = app.tabs.get_mut(tab_idx) {
                            tab.manual_tx = Some(crate::tabs::ManualTxTabState {
                                begun_at: now,
                                last_activity: now,
                                statements: 0,
                                lost: false,
                            });
                        }
                        app.schedule_manual_tx_tick(cx);
                        on_ready(app, profile, cx);
                    }
                    Ok(Err(e)) => {
                        app.set_status(
                            StatusLevel::Error,
                            crate::localization::message(e.message()),
                        );
                        cx.notify();
                    }
                    Err(_) => {
                        app.set_status(
                            StatusLevel::Error,
                            crate::localization::text("shell.transactions.begin_failed"),
                        );
                        cx.notify();
                    }
                });
            })
            .detach();
        }
    }

    pub(super) fn commit_transaction(&mut self, tab_idx: usize, cx: &mut Context<Self>) {
        #[cfg(not(target_arch = "wasm32"))]
        self.end_manual_transaction(tab_idx, TransactionEnd::Commit, None, cx);
        #[cfg(target_arch = "wasm32")]
        let _ = (tab_idx, cx);
    }

    pub(super) fn rollback_transaction(&mut self, tab_idx: usize, cx: &mut Context<Self>) {
        #[cfg(not(target_arch = "wasm32"))]
        self.end_manual_transaction(tab_idx, TransactionEnd::Rollback, None, cx);
        #[cfg(target_arch = "wasm32")]
        let _ = (tab_idx, cx);
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn end_manual_transaction(
        &mut self,
        tab_idx: usize,
        end: TransactionEnd,
        followup: Option<PendingTransactionClose>,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.get(tab_idx) else {
            return;
        };
        if tab.active_query_id.is_some() {
            self.set_status(
                StatusLevel::Error,
                crate::localization::text("shell.transactions.wait_for_statement"),
            );
            cx.notify();
            return;
        }
        if end == TransactionEnd::Commit {
            let store = ConnectionStore::global();
            let now = sqlly_intel::query::safety::Instant::now();
            if let Some(profile) = self.active_profile.as_ref() {
                let safety =
                    crate::mutation_review::SafetyFeatures::effective_for_profile(profile, store);
                if safety
                    .production_guard()
                    .forces_select_only(self.production_unlock.as_ref(), now)
                {
                    self.set_status(
                        StatusLevel::Error,
                        crate::localization::text("shell.transactions.production_commit_locked"),
                    );
                    cx.notify();
                    return;
                }
            }
        }
        let Some(profile) = self.active_profile.clone() else {
            return;
        };
        if tab.manual_tx.is_none() {
            return;
        }
        let tab_id = tab.id.clone();
        let statements = tab.manual_tx.as_ref().map(|tx| tx.statements).unwrap_or(0);
        let services = self.services_for_profile(&profile);
        let weak = cx.weak_entity();
        cx.spawn(async move |_this, cx| {
            let result = crate::tokio_handle()
                .spawn(async move {
                    services
                        .queries
                        .end_manual_transaction(&profile, &tab_id, end)
                        .await
                })
                .await;
            let _ = weak.update(cx, |app, cx| match result {
                Ok(Ok(())) => {
                    if let Some(tab) = app.tabs.get_mut(tab_idx) {
                        tab.manual_tx = None;
                    }
                    if let Some(pending) = followup {
                        app.finish_transaction_close(pending, cx);
                    } else {
                        let message = match end {
                            TransactionEnd::Commit => crate::localization::format(
                                "shell.transactions.committed",
                                &[("count", &statements.to_string())],
                            ),
                            TransactionEnd::Rollback => {
                                crate::localization::text("shell.transactions.rolled_back")
                            }
                        };
                        app.set_status(StatusLevel::Success, message);
                        cx.notify();
                    }
                }
                Ok(Err(e)) => {
                    app.set_status(
                        StatusLevel::Error,
                        crate::localization::message(e.message()),
                    );
                    cx.notify();
                }
                Err(_) => {
                    app.set_status(
                        StatusLevel::Error,
                        crate::localization::text("shell.transactions.end_failed"),
                    );
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(super) fn touch_manual_tx(&mut self, tab_idx: usize) {
        #[cfg(target_arch = "wasm32")]
        let _ = tab_idx;
        #[cfg(not(target_arch = "wasm32"))]
        {
            let Some(profile) = self.active_profile.as_ref() else {
                return;
            };
            let Some(tab) = self.tabs.get(tab_idx) else {
                return;
            };
            let services = self.services_for_profile(profile);
            let state = services.queries.manual_transaction_state(profile, &tab.id);
            if let Some(tab) = self.tabs.get_mut(tab_idx)
                && let Some(tx) = tab.manual_tx.as_mut()
            {
                tx.last_activity = Instant::now();
                if let Some(ManualTxState::Open { statements })
                | Some(ManualTxState::Busy { statements }) = state
                {
                    tx.statements = statements;
                }
                if state == Some(ManualTxState::Lost) {
                    tx.lost = true;
                }
            }
        }
    }

    pub(super) fn schedule_manual_tx_tick(&mut self, cx: &mut Context<Self>) {
        if !self.tabs.iter().any(|t| t.manual_tx.is_some()) {
            self.manual_tx_tick = None;
            return;
        }
        if self.manual_tx_tick.is_some() {
            return;
        }
        let this = cx.entity();
        self.manual_tx_tick = Some(cx.spawn(async move |_this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_secs(1))
                .await;
            this.update(cx, |this, cx| {
                this.manual_tx_tick = None;
                #[cfg(not(target_arch = "wasm32"))]
                this.poll_manual_transactions(cx);
                if this.tabs.iter().any(|t| t.manual_tx.is_some()) {
                    this.schedule_manual_tx_tick(cx);
                }
                cx.notify();
            });
        }));
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn poll_manual_transactions(&mut self, cx: &mut Context<Self>) {
        let idle_secs = self.transaction_prefs.idle_rollback_secs();
        let idle_limit = std::time::Duration::from_secs(idle_secs);
        let profile = self.active_profile.clone();
        let mut lost_messages = Vec::new();
        let mut idle_rollbacks: Vec<(usize, String, u64)> = Vec::new();
        let mut lost_indices = Vec::new();

        for (idx, tab) in self.tabs.iter().enumerate() {
            if tab.manual_tx.is_none() {
                continue;
            }
            if let Some(profile) = profile.as_ref() {
                let services = self.services_for_profile(profile);
                if services.queries.manual_transaction_state(profile, &tab.id)
                    == Some(ManualTxState::Lost)
                {
                    lost_indices.push(idx);
                    lost_messages.push(tab.name.clone());
                }
            }
            if let Some(tx) = tab.manual_tx.as_ref() {
                if tx.lost {
                    continue;
                }
                if tab.active_query_id.is_none()
                    && tx.dirty()
                    && tx.last_activity.elapsed() >= idle_limit
                {
                    let mins = idle_secs / 60;
                    idle_rollbacks.push((idx, tab.name.clone(), mins.max(1)));
                }
            }
        }

        for idx in lost_indices {
            if let Some(tx) = self.tabs[idx].manual_tx.as_mut() {
                tx.lost = true;
            }
        }

        for (idx, name, mins) in &idle_rollbacks {
            self.rollback_transaction(*idx, cx);
            let note =
                gpui_component::notification::Notification::warning(crate::localization::format(
                    "shell.transactions.idle_rollback_notification",
                    &[("tab", name), ("minutes", &mins.to_string())],
                ));
            Self::toast_update_outcome(note, cx);
        }

        if !lost_messages.is_empty() {
            self.set_status(
                StatusLevel::Error,
                crate::localization::text("shell.transactions.lost"),
            );
        }
    }

    pub(super) fn rollback_all_for_profile(
        &mut self,
        profile_id: uuid::Uuid,
        cx: &mut Context<Self>,
    ) {
        let indices: Vec<usize> = self
            .tabs
            .iter()
            .enumerate()
            .filter(|(_, tab)| {
                tab.manual_tx.is_some()
                    && crate::tabs::tab_targets_connection(tab, Some(&profile_id))
            })
            .map(|(i, _)| i)
            .collect();
        for idx in indices {
            self.silent_rollback_tab(idx, cx);
        }
    }

    pub(super) fn silent_rollback_tab(&mut self, tab_idx: usize, cx: &mut Context<Self>) {
        #[cfg(target_arch = "wasm32")]
        let _ = (tab_idx, cx);
        #[cfg(not(target_arch = "wasm32"))]
        {
            let Some(tab) = self.tabs.get(tab_idx) else {
                return;
            };
            if tab.manual_tx.is_none() {
                return;
            }
            if tab.active_query_id.is_some() {
                return;
            }
            let Some(profile) = self.active_profile.clone() else {
                return;
            };
            let tab_id = tab.id.clone();
            let services = self.services_for_profile(&profile);
            if let Some(tab) = self.tabs.get_mut(tab_idx) {
                tab.manual_tx = None;
            }
            cx.background_spawn(async move {
                let _ = crate::tokio_handle()
                    .spawn(async move {
                        let _ = services
                            .queries
                            .end_manual_transaction(&profile, &tab_id, TransactionEnd::Rollback)
                            .await;
                    })
                    .await;
            })
            .detach();
        }
    }

    pub(super) fn first_dirty_manual_tab_for_profile(
        &self,
        profile_id: &uuid::Uuid,
    ) -> Option<usize> {
        self.tabs.iter().position(|tab| {
            tab.manual_tx.as_ref().is_some_and(|tx| tx.dirty())
                && crate::tabs::tab_targets_connection(tab, Some(profile_id))
        })
    }

    pub(super) fn arm_transaction_close_if_dirty(
        &mut self,
        tab_idx: usize,
        reason: TransactionCloseReason,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(tab) = self.tabs.get(tab_idx) else {
            return false;
        };
        if !tab.manual_tx.as_ref().is_some_and(|tx| tx.dirty()) {
            return false;
        }
        self.pending.transaction_close = Some(PendingTransactionClose {
            tab_id: tab.id.clone(),
            tab_name: tab.name.clone(),
            statements: tab.manual_tx.as_ref().map(|tx| tx.statements).unwrap_or(0),
            reason,
        });
        cx.notify();
        true
    }

    pub(super) fn cancel_transaction_close(&mut self, cx: &mut Context<Self>) {
        self.pending.transaction_close = None;
        cx.notify();
    }

    pub(super) fn confirm_transaction_close_commit(&mut self, cx: &mut Context<Self>) {
        #[cfg(target_arch = "wasm32")]
        let _ = cx;
        #[cfg(not(target_arch = "wasm32"))]
        {
            let Some(pending) = self.pending.transaction_close.take() else {
                return;
            };
            let Some(tab_idx) = self.tab_index_by_id(&pending.tab_id) else {
                return;
            };
            self.end_manual_transaction(tab_idx, TransactionEnd::Commit, Some(pending), cx);
        }
    }

    pub(super) fn confirm_transaction_close_rollback(&mut self, cx: &mut Context<Self>) {
        #[cfg(target_arch = "wasm32")]
        let _ = cx;
        #[cfg(not(target_arch = "wasm32"))]
        {
            let Some(pending) = self.pending.transaction_close.take() else {
                return;
            };
            let Some(tab_idx) = self.tab_index_by_id(&pending.tab_id) else {
                return;
            };
            self.end_manual_transaction(tab_idx, TransactionEnd::Rollback, Some(pending), cx);
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn finish_transaction_close(
        &mut self,
        pending: PendingTransactionClose,
        cx: &mut Context<Self>,
    ) {
        match pending.reason {
            TransactionCloseReason::CloseTab => {
                if let Some(tab_idx) = self.tab_index_by_id(&pending.tab_id) {
                    self.close_tab_impl(tab_idx, cx);
                }
            }
            TransactionCloseReason::Disconnect(profile_id) => {
                // Resume the disconnect for the profile it was invoked on —
                // the focused connection may have changed while the dialog
                // was up.
                cx.defer(move |app| {
                    Self::update_active_app_view(app, move |view, _window, cx| {
                        view.disconnect_profile(profile_id, cx);
                    });
                });
            }
            TransactionCloseReason::SwitchToAuto => {
                if let Some(tab_idx) = self.tab_index_by_id(&pending.tab_id) {
                    if let Some(tab) = self.tabs.get_mut(tab_idx) {
                        tab.transaction_mode = crate::tabs::TransactionMode::Auto;
                        tab.manual_tx = None;
                    }
                    self.set_status(
                        StatusLevel::Info,
                        crate::localization::text("shell.transactions.switch_to_auto_enabled"),
                    );
                    cx.notify();
                }
            }
        }
    }

    pub(super) fn unlock_read_only(
        &mut self,
        window: sqlly_intel::query::safety::UnlockWindow,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self.active_profile.as_ref() else {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.transactions.no_active_connection"),
            );
            cx.notify();
            return;
        };
        if !profile.block_all_but_select {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.transactions.not_read_only"),
            );
            cx.notify();
            return;
        }
        let now = sqlly_intel::query::safety::Instant::now();
        let unlock = sqlly_intel::query::safety::WriteUnlock::start(window, now);
        self.read_only_unlocks
            .insert(profile.id.0.to_string(), unlock);
        self.set_status(
            StatusLevel::Info,
            crate::localization::format(
                "shell.transactions.read_only_unlocked",
                &[("duration", window.label())],
            ),
        );
        self.schedule_read_only_unlock_tick(cx);
        cx.notify();
    }

    pub(super) fn lock_read_only(&mut self, cx: &mut Context<Self>) {
        if let Some(profile) = self.active_profile.as_ref() {
            self.read_only_unlocks.remove(&profile.id.0.to_string());
        }
        self.read_only_unlock_tick = None;
        self.set_status(
            StatusLevel::Info,
            crate::localization::text("shell.transactions.read_only_locked"),
        );
        cx.notify();
    }

    pub(super) fn schedule_read_only_unlock_tick(&mut self, cx: &mut Context<Self>) {
        if self.read_only_unlocks.is_empty() {
            self.read_only_unlock_tick = None;
            return;
        }
        if self.read_only_unlock_tick.is_some() {
            return;
        }
        let this = cx.entity();
        self.read_only_unlock_tick = Some(cx.spawn(async move |_this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_secs(1))
                .await;
            this.update(cx, |this, cx| {
                this.read_only_unlock_tick = None;
                let now = sqlly_intel::query::safety::Instant::now();
                this.read_only_unlocks
                    .retain(|_, unlock| unlock.is_active(now));
                if this.read_only_unlocks.is_empty() {
                    this.set_status(
                        StatusLevel::Info,
                        crate::localization::text("shell.transactions.read_only_unlock_expired"),
                    );
                } else {
                    this.schedule_read_only_unlock_tick(cx);
                }
                cx.notify();
            });
        }));
    }

    pub(super) fn note_manual_tx_cancel(&mut self, index: usize, cx: &mut Context<Self>) {
        #[cfg(target_arch = "wasm32")]
        let _ = (index, cx);
        #[cfg(not(target_arch = "wasm32"))]
        {
            let Some(tab) = self.tabs.get(index) else {
                return;
            };
            if tab.transaction_mode != crate::tabs::TransactionMode::Manual
                || tab.manual_tx.is_none()
            {
                return;
            }
            let Some(profile) = self.active_profile.clone() else {
                return;
            };
            let tab_id = tab.id.clone();
            let services = self.services_for_profile(&profile);
            let weak = cx.weak_entity();
            cx.spawn(async move |_this, cx| {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                let _ = weak.update(cx, |app, cx| {
                    if services.queries.manual_transaction_state(&profile, &tab_id)
                        == Some(ManualTxState::Lost)
                        && let Some(tab_idx) = app.tab_index_by_id(&tab_id)
                        && let Some(tx) = app.tabs[tab_idx].manual_tx.as_mut()
                    {
                        tx.lost = true;
                        app.set_status(
                            StatusLevel::Error,
                            crate::localization::text("shell.transactions.cancel_lost_transaction"),
                        );
                    }
                    cx.notify();
                });
            })
            .detach();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::manual_mode_decision;
    use crate::capabilities::Capability;

    #[test]
    fn manual_mode_available_when_all_gates_pass() {
        assert!(manual_mode_decision(Capability::Available, false, false, false).is_available());
    }

    #[test]
    fn manual_mode_refuses_relay() {
        assert_eq!(
            manual_mode_decision(Capability::Available, false, false, true).reason_key(),
            Some("shell.transactions.manual_relay_unavailable")
        );
    }

    #[test]
    fn manual_mode_refuses_forced_wrap() {
        assert_eq!(
            manual_mode_decision(Capability::Available, false, true, false).reason_key(),
            Some("shell.transactions.forced_rollback")
        );
    }

    #[test]
    fn manual_mode_refuses_select_only() {
        assert_eq!(
            manual_mode_decision(Capability::Available, true, false, false).reason_key(),
            Some("shell.transactions.select_only")
        );
    }

    #[test]
    fn manual_mode_refuses_unsupported_engine() {
        assert_eq!(
            manual_mode_decision(
                Capability::Unavailable("explorer.capabilities.reason_manual_transactions"),
                false,
                false,
                false
            )
            .reason_key(),
            Some("explorer.capabilities.reason_manual_transactions")
        );
    }
}
