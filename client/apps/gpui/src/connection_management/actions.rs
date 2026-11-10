use super::*;

/// Copy every keyring secret a duplicate re-minted from its source entry to
/// the copy's own. Returns how many could not be copied (nothing stored
/// under the source, or the keyring refused); the copy's references to those
/// stay dangling until the user re-enters the secret.
pub(super) fn copy_duplicate_secrets(secrets: &[sqlly_core::SecretCopy]) -> usize {
    let mut failures = 0;
    for copy in secrets {
        let (Ok(from), Ok(to)) = (
            Uuid::parse_str(&copy.from.key),
            Uuid::parse_str(&copy.to.key),
        ) else {
            failures += 1;
            continue;
        };
        match crate::sql_credentials::copy_password(from, to) {
            Ok(true) => {}
            Ok(false) | Err(_) => failures += 1,
        }
    }
    failures
}

/// Status-line wording for secrets a duplicate could not take over.
pub(super) fn secrets_not_copied_warning(failures: usize) -> Option<String> {
    match failures {
        0 => None,
        1 => Some(crate::localization::text(
            "connections.management.secret_copy_warning_one",
        )),
        count => Some(crate::localization::format(
            "connections.management.secret_copy_warning_many",
            &[("count", &count.to_string())],
        )),
    }
}

impl ConnectionManagement {
    pub(super) fn handle_save(
        &mut self,
        _: &MouseDownEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.save_profile(cx);
    }

    /// Commit the form to the connection store. Returns `false` (leaving the
    /// form untouched) when validation fails, so callers that want to close
    /// afterwards can keep the dialog open instead.
    pub(super) fn save_profile(&mut self, cx: &mut Context<Self>) -> bool {
        if crate::connection::connections_are_proxied() {
            self.status_message =
                crate::localization::text("connections.management.proxy_save_read_only");
            cx.notify();
            return false;
        }
        if self
            .editing_id
            .is_some_and(crate::connection::connection_is_provisioned)
        {
            self.status_message =
                crate::localization::text("connections.management.provisioned_save_read_only");
            cx.notify();
            return false;
        }
        if !self.build_form(cx).can_save() {
            self.status_message =
                crate::localization::text("connections.management.fix_validation_before_save");
            cx.notify();
            return false;
        }
        if self.editing_id.is_none()
            && let Some(limit) = crate::license::saved_connection_limit_now()
            && !crate::license::fits_saved_connection_limit(
                Some(limit),
                ConnectionStore::global().list().len(),
                1,
            )
        {
            self.status_message = crate::license::connection_limit_reason(limit);
            cx.emit(ConnectionManagementEvent::UnlockRequested);
            cx.notify();
            return false;
        }
        let profile = self.build_profile(cx);
        let secret_warning = self.commit_profile(&profile);
        self.refresh_tree(cx);
        self.dirty = false;
        self.close_confirm = false;
        self.status_message = match secret_warning {
            Some(warning) => crate::localization::format(
                "connections.management.saved_with_secret_warning",
                &[("profile_name", &profile.name), ("warning", &warning)],
            ),
            None => crate::localization::format(
                "connections.management.saved_profile",
                &[("profile_name", &profile.name)],
            ),
        };
        cx.emit(ConnectionManagementEvent::Saved(profile));
        true
    }

    /// Write the form's profile to the store: update in place when editing,
    /// insert beside the source when the form holds a duplicate (copying the
    /// source's keyring secrets into the copy's own entries first), append
    /// otherwise. Leaves the form editing the saved profile. Returns a
    /// warning when a duplicate's secret could not be copied.
    fn commit_profile(&mut self, profile: &ConnectionProfile) -> Option<String> {
        let store = ConnectionStore::global();
        if let Some(id) = self.editing_id {
            store.update(id, profile.clone());
            return None;
        }
        let warning = match self.duplicate_of.take() {
            Some(pending) => {
                let failures = copy_duplicate_secrets(&pending.copy.secrets);
                store.add_after(pending.source, profile.clone());
                self.selected = Some(format!("profile:{}", profile.id.0));
                secrets_not_copied_warning(failures)
            }
            None => {
                store.add(profile.clone());
                None
            }
        };
        self.editing_id = Some(profile.id.0);
        warning
    }

    /// Load an independent copy of the selected saved connection into the
    /// form as a new, unsaved profile: `"<name> (copy)"`, every setting
    /// carried over, its own fresh keyring references. The original's SQL
    /// password is verified readable up front; when it is not, the copy starts
    /// with an empty password field and the status line says so. Nothing is
    /// written until Save.
    pub(super) fn handle_duplicate(
        &mut self,
        _: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if crate::connection::connections_are_proxied() {
            self.status_message =
                crate::localization::text("connections.management.proxy_duplicate_read_only");
            cx.notify();
            return;
        }
        let Some(id) = self.editing_id else {
            self.status_message =
                crate::localization::text("connections.management.select_connection_to_duplicate");
            cx.notify();
            return;
        };
        let store = ConnectionStore::global();
        let Some(original) = store.get(id) else {
            self.status_message =
                crate::localization::text("connections.management.connection_not_found");
            cx.notify();
            return;
        };
        let taken: Vec<String> = store.list().into_iter().map(|p| p.name).collect();
        let mut copy =
            original.duplicate_named(sqlly_core::unique_copy_name(&original.name, taken));
        self.load_profile(copy.profile.clone(), window, cx);
        self.editing_id = None;
        self.selected = None;

        // The SQL password rides on the account reference this form shows as
        // a masked field. Confirm the original's entry can be read now, so a
        // copy that would save without one says so instead of failing later;
        // the form then holds a plain password field for the user to fill,
        // and the unreadable pair is dropped so Save does not retry it.
        let mut password_missing = false;
        if let Some(fresh) = self.connection_sql_account {
            let fresh_key = fresh.to_string();
            let source = copy
                .secrets
                .iter()
                .find(|s| s.to.key == fresh_key)
                .and_then(|s| Uuid::parse_str(&s.from.key).ok());
            if let Some(source) = source
                && !matches!(crate::sql_credentials::load_password(source), Ok(Some(_)))
            {
                password_missing = true;
                self.connection_sql_account = None;
                self.password
                    .update(cx, |f, cx| f.set_value("", window, cx));
                copy.secrets.retain(|s| s.to.key != fresh_key);
            }
        }
        self.duplicate_of = Some(PendingDuplicate { source: id, copy });
        self.mark_dirty();
        self.status_message = if password_missing {
            crate::localization::format(
                "connections.management.duplicate_password_missing",
                &[("profile_name", &original.name)],
            )
        } else {
            crate::localization::format(
                "connections.management.duplicate_created",
                &[("profile_name", &original.name)],
            )
        };
        cx.notify();
    }

    pub(super) fn handle_connect(
        &mut self,
        _: &MouseDownEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.build_form(cx).can_save() {
            self.status_message =
                crate::localization::text("connections.management.fix_validation_before_connect");
            cx.notify();
            return;
        }
        let profile = self.build_profile(cx);
        if let Some(warning) = self.commit_profile(&profile) {
            self.status_message = warning;
        }
        self.refresh_tree(cx);
        self.dirty = false;
        self.close_confirm = false;
        cx.emit(ConnectionManagementEvent::ConnectRequested(profile));
    }

    /// Test every saved connection, one at a time, on demand only. Entra
    /// sign-in and cloud IAM profiles are skipped — they need a minted token,
    /// not a stored password — and each outcome persists through the same
    /// status records a single test writes, so the health badges update as the
    /// pass walks the list.
    pub(super) fn handle_test_all(
        &mut self,
        _: &MouseDownEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use sqlly_core::AuthenticationType;
        if self.testing_all {
            return;
        }
        let all = ConnectionStore::global().list();
        let total_saved = all.len();
        let profiles: Vec<_> = all
            .into_iter()
            .filter(|profile| {
                !matches!(
                    profile.authentication,
                    AuthenticationType::EntraBrowser
                        | AuthenticationType::EntraDeviceCode
                        | AuthenticationType::AwsRdsIam { .. }
                        | AuthenticationType::GcpCloudSqlIam { .. }
                )
            })
            .collect();
        let skipped = total_saved - profiles.len();
        if profiles.is_empty() {
            self.status_message =
                crate::localization::text("connections.management.no_testable_connections");
            cx.notify();
            return;
        }
        self.testing_all = true;
        self.status_message = crate::localization::format(
            "connections.management.testing_all_progress",
            &[("count", &profiles.len().to_string())],
        );
        cx.notify();
        let router = self.router.clone();
        let log = self.log.clone();
        let async_cx = cx.to_async();
        let weak = cx.weak_entity();
        cx.spawn(async move |_this, _cx| {
            let mut passed = 0usize;
            let mut failed = 0usize;
            for profile in profiles {
                let id = profile.id.0;
                let label = profile.name.clone();
                let outcome = match crate::sql_credentials::hydrate_profile(profile) {
                    Ok(profile) => {
                        let services = router.services_for(&profile);
                        services.connections.test_connection(&profile).await
                    }
                    // Keyed: resolved on the UI thread (and persisted with
                    // its key), never on this runtime.
                    Err(error) => Err(sqlly_core::StudioError::ConnectionMessage(error)),
                };
                match outcome {
                    Ok(()) => {
                        passed += 1;
                        ConnectionStore::global().record_test_success(id, chrono::Utc::now());
                        log.log_verbose(
                            "connection_test",
                            &format!("Test All: \"{label}\" succeeded"),
                        );
                    }
                    Err(error) => {
                        failed += 1;
                        ConnectionStore::global().record_failure_message(
                            id,
                            chrono::Utc::now(),
                            &error.message(),
                        );
                        log.log_error(
                            "connection_test",
                            &format!("Test All: \"{label}\" failed: {error}"),
                        );
                    }
                }
                // Repaint the tree so badges update as the pass advances.
                let mut cx = async_cx.clone();
                let _ = weak.update(&mut cx, |this, cx| {
                    this.refresh_tree(cx);
                    cx.notify();
                });
            }
            let mut cx = async_cx.clone();
            weak.update(&mut cx, |this, cx| {
                this.testing_all = false;
                this.status_message = if skipped > 0 {
                    crate::localization::format(
                        "connections.management.test_all_summary_skipped",
                        &[
                            ("passed", &passed.to_string()),
                            ("failed", &failed.to_string()),
                            ("skipped", &skipped.to_string()),
                        ],
                    )
                } else {
                    crate::localization::format(
                        "connections.management.test_all_summary",
                        &[
                            ("passed", &passed.to_string()),
                            ("failed", &failed.to_string()),
                        ],
                    )
                };
                this.refresh_tree(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Toggle the selected saved connection's favorite star. Pickers list
    /// favorites first.
    pub(super) fn handle_toggle_favorite(
        &mut self,
        _: &MouseDownEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = self.editing_id else {
            self.status_message =
                crate::localization::text("connections.management.select_connection_to_favorite");
            cx.notify();
            return;
        };
        let store = ConnectionStore::global();
        let Some(profile) = store.get(id) else {
            self.status_message =
                crate::localization::text("connections.management.save_connection_first");
            cx.notify();
            return;
        };
        let favorite = !profile.favorite;
        store.set_favorite(id, favorite);
        self.status_message = if favorite {
            crate::localization::format(
                "connections.management.favorite_added",
                &[("profile_name", &profile.name)],
            )
        } else {
            crate::localization::format(
                "connections.management.favorite_removed",
                &[("profile_name", &profile.name)],
            )
        };
        self.refresh_tree(cx);
        cx.notify();
    }

    pub(super) fn handle_test(
        &mut self,
        _: &MouseDownEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.build_form(cx).can_save() {
            self.status_message =
                crate::localization::text("connections.management.fix_validation_before_test");
            cx.notify();
            return;
        }
        let profile = match crate::sql_credentials::hydrate_profile(self.build_profile(cx)) {
            Ok(profile) => profile,
            Err(error) => {
                self.status_message = crate::localization::format(
                    "connections.management.test_password_load_failed",
                    &[("error", &crate::localization::message(error))],
                );
                self.test_result = Some(self.status_message.clone());
                self.test_success = false;
                cx.notify();
                return;
            }
        };
        self.testing = true;
        self.test_result = None;
        self.test_success = false;
        self.status_message =
            crate::localization::text("connections.management.testing_connection");
        cx.notify();

        // Test over the route the profile carries (the form preserves a saved
        // ViaRelay route), matching the modal editor's `handle_test`.
        let services = self.router.services_for(&profile);
        let log = self.log.clone();
        let provider = self.entra_sql_token_provider.clone();
        let profile_label = profile.name.clone();
        let profile_id = profile.id.0;
        let async_cx = cx.to_async();
        let weak = cx.weak_entity();
        cx.spawn(async move |_this, _cx| {
            log.log_verbose(
                "connection_test",
                &format!("Testing connection \"{profile_label}\""),
            );
            // Re-mint the Entra SQL token before dialing so Test Connection
            // refreshes like a query window instead of using a stale token.
            let profile = match &provider {
                Some(provider) => {
                    match crate::azure_sql_token::hydrate_entra_token(provider, profile).await {
                        Ok(profile) => profile,
                        Err(message) => {
                            ConnectionStore::global().record_failure(
                                profile_id,
                                chrono::Utc::now(),
                                &message,
                            );
                            let mut cx = async_cx.clone();
                            weak.update(&mut cx, |this, cx| {
                                this.testing = false;
                                this.test_success = false;
                                this.test_result = Some(crate::localization::format(
                                    "connections.management.connection_failed",
                                    &[("error", &message)],
                                ));
                                this.status_message = message;
                                this.refresh_tree(cx);
                                cx.notify();
                            })
                            .ok();
                            return;
                        }
                    }
                }
                None => profile,
            };
            let result = services.connections.test_connection(&profile).await;
            match &result {
                Ok(()) => log.log_verbose(
                    "connection_test",
                    &format!("Connection test succeeded for \"{profile_label}\""),
                ),
                Err(e) => log.log_error(
                    "connection_test",
                    &format!("Connection test failed for \"{profile_label}\": {e}"),
                ),
            }
            let mut cx = async_cx.clone();
            weak.update(&mut cx, |this, cx| {
                this.testing = false;
                match result {
                    Ok(()) => {
                        ConnectionStore::global()
                            .record_test_success(profile_id, chrono::Utc::now());
                        this.test_success = true;
                        this.test_result = Some(crate::localization::text(
                            "connections.management.connection_successful",
                        ));
                        this.status_message =
                            crate::localization::text("connections.management.test_succeeded");
                    }
                    Err(e) => {
                        ConnectionStore::global().record_failure_message(
                            profile_id,
                            chrono::Utc::now(),
                            &e.message(),
                        );
                        this.test_success = false;
                        let error = crate::localization::message(e.message());
                        this.test_result = Some(crate::localization::format(
                            "connections.management.connection_failed",
                            &[("error", error.as_str())],
                        ));
                        this.status_message =
                            crate::localization::text("connections.management.test_failed");
                    }
                }
                this.refresh_tree(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn handle_delete(
        &mut self,
        _: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if crate::connection::connections_are_proxied() {
            self.status_message =
                crate::localization::text("connections.management.proxy_delete_read_only");
            cx.notify();
            return;
        }
        // An unsaved duplicate has no `editing_id`, so the minus button is
        // disabled for it and its source can never be deleted from here.
        let Some(id) = self.editing_id else {
            self.status_message =
                crate::localization::text("connections.management.select_connection_to_delete");
            cx.notify();
            return;
        };
        if crate::connection::connection_is_provisioned(id) {
            self.status_message =
                crate::localization::text("connections.management.provisioned_delete_read_only");
            cx.notify();
            return;
        }
        let store = ConnectionStore::global();
        let name = store.get(id).map(|p| p.name);
        if let Some(profile) = store.get(id)
            && let Some(tunnel) = &profile.ssh_tunnel
        {
            let _ = crate::sql_credentials::delete_ssh_secrets(tunnel);
        }
        if let Some(profile) = store.get(id)
            && let Some(secret_ref) = profile
                .redis_sentinel
                .as_ref()
                .and_then(|s| s.sentinel_password_ref.as_ref())
            && secret_ref.provider == sqlly_core::SecretProvider::MacOsKeychain
            && let Ok(secret_id) = uuid::Uuid::parse_str(&secret_ref.key)
        {
            let _ = crate::sql_credentials::delete_password(secret_id);
        }
        store.remove(id);
        self.refresh_tree(cx);
        self.add_server(window, cx);
        if let Some(n) = name {
            self.status_message = crate::localization::format(
                "connections.management.deleted_profile",
                &[("profile_name", &n)],
            );
        }
        cx.emit(ConnectionManagementEvent::Deleted(id));
    }

    /// Turn the dialog footer into the unsaved-changes prompt: Save commits
    /// and closes, Discard closes and throws the edits away, Keep editing
    /// (also Escape) returns to the form. The prompt replaces the old
    /// "click Close again to discard" second-chance, which was easy to
    /// trigger accidentally and never offered to save.
    pub(super) fn unsaved_prompt(dialog: Dialog<Self>, cx: &mut Context<Self>) -> Dialog<Self> {
        let entity = cx.entity();
        dialog
            .leading_button(
                div()
                    .text_size(px(Theme::font_size_ratio(0.82)))
                    .text_color(Theme::text())
                    .child(crate::localization::text(
                        "connections.management.unsaved_changes_prompt",
                    ))
                    .into_any_element(),
            )
            .button(
                Button::new("cm-discard-changes")
                    .label(crate::localization::text(
                        "connections.management.discard_button",
                    ))
                    .danger()
                    .on_click(move |_: &gpui::ClickEvent, window, app| {
                        entity.update(app, |this, cx| this.discard_and_close(window, cx));
                    })
                    .into_any_element(),
            )
            .confirm(
                crate::localization::text("connections.management.save_unsaved_button"),
                crate::localization::text("connections.management.keep_editing_button"),
            )
            .on_confirm(|this, window, cx| this.save_and_close(window, cx))
            .on_reject(|this, _window, cx| this.cancel_close(cx))
    }

    /// Single close path: the chrome's ✕ and the pane's Close button both
    /// funnel here. (Escape and scrim clicks do not close this dialog at all
    /// — see the `easy_close(false)` note where the chrome is built.)
    ///
    /// With unsaved edits this does not close: it raises the Save / Discard /
    /// Keep-editing prompt in the footer and waits for an answer.
    pub(super) fn request_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.dirty && !self.close_confirm {
            self.close_confirm = true;
            self.status_message =
                crate::localization::text("connections.management.unsaved_changes_status");
            cx.notify();
            return;
        }
        self.finish_close(window, cx);
    }

    /// Answer the unsaved-changes prompt with Save. A failed validation keeps
    /// the dialog (and the prompt) up rather than closing over a bad form.
    pub(super) fn save_and_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.save_profile(cx) {
            self.finish_close(window, cx);
        }
    }

    /// Answer the unsaved-changes prompt with Discard: throw the edits away
    /// and close.
    pub(super) fn discard_and_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.dirty = false;
        self.finish_close(window, cx);
    }

    /// Answer the unsaved-changes prompt with Keep editing (also Escape).
    pub(super) fn cancel_close(&mut self, cx: &mut Context<Self>) {
        self.close_confirm = false;
        self.status_message =
            crate::localization::text("connections.management.keep_editing_status");
        cx.notify();
    }

    /// Actually go away. Popped out, the window IS the dialog, so closing
    /// means closing the window; inline, the host clears its overlay slot.
    fn finish_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_confirm = false;
        if self.windowed {
            window.remove_window();
        } else {
            cx.emit(ConnectionManagementEvent::Cancelled);
        }
    }

    /// Move the dialog into its own OS window, keeping this same entity (and
    /// therefore every in-progress edit) alive.
    pub(super) fn pop_out(&mut self, cx: &mut Context<Self>) {
        let entity = cx.entity();
        // Opening the window renders this dialog into it immediately, which
        // must not happen inside its own update — defer past it.
        cx.defer(move |cx| {
            let build_entity = entity.clone();
            let opened = crate::dialog::open_detached(
                crate::dialog::DetachedOptions::localized(
                    "connections.management.detached_window_title",
                    Theme::scaled(DIALOG_WIDTH),
                    Theme::scaled(DIALOG_HEIGHT),
                ),
                None,
                cx,
                move |_, cx| {
                    build_entity.update(cx, |dialog, _| dialog.windowed = true);
                    build_entity
                },
            );
            if opened.is_some() {
                // The window owns the dialog now; drop the inline card.
                entity.update(cx, |_, cx| {
                    cx.emit(ConnectionManagementEvent::PoppedOut);
                });
            }
        });
    }

    pub(super) fn handle_close(
        &mut self,
        _: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.request_close(window, cx);
    }

    pub(super) fn handle_add_server(
        &mut self,
        _: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.add_server(window, cx);
    }

    pub(super) fn set_pane(
        &mut self,
        pane: WorkspacePane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.pane == pane {
            return;
        }
        self.pane = pane;
        self.database_dropdown_open = false;
        if pane == WorkspacePane::Connections {
            // Entities may have been renamed / added while another pane was
            // active — refresh the client / project select items.
            self.sync_entity_selects(window, cx);
        }
        if pane == WorkspacePane::Accounts {
            self.reload_entra_accounts();
            self.reload_sql_accounts();
            if self.entra_selected.is_none() && self.sql_selected.is_none() {
                if !self.entra_accounts.is_empty() {
                    self.entra_selected = Some(0);
                } else if let Some(account) = self.sql_accounts.first().cloned() {
                    self.sql_selected = Some(account.id);
                    self.sql_account_name = account.name.clone();
                    self.sql_account_username = account.username.clone();
                    self.sql_account_name_input
                        .update(cx, |s, cx| s.set_value(account.name, window, cx));
                    self.sql_account_username_input
                        .update(cx, |s, cx| s.set_value(account.username, window, cx));
                }
            }
        }
        if let Some(kind) = pane.entity_kind() {
            // Select the first entity of the kind so the editor is populated.
            self.entity_status = String::new();
            let first = Self::entity_rows(kind).first().map(|row| row.id);
            match first {
                Some(id) => self.select_entity(kind, id, window, cx),
                None => self.clear_entity_form(window, cx),
            }
        }
        cx.notify();
    }

    // ----- Entra accounts (Accounts pane + auth binding picker) -----
}
