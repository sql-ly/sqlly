use super::*;

impl AppView {
    /// Populate `azure_discoveries` (and the sidebar / connection-management
    /// tree) from the on-disk cache written by the last successful live
    /// discovery. Display-only: performs no network calls and does not touch
    /// Entra tokens. A missing or corrupt cache yields an empty list.
    pub(super) fn load_cached_azure_discoveries(&mut self, cx: &mut Context<Self>) {
        let discoveries =
            crate::azure_discovery_cache::load(&crate::azure_discovery_cache::cache_file());
        if discoveries.is_empty() {
            return;
        }
        self.azure_discoveries = discoveries.clone();
        self.sidebar.update(cx, |s, cx| {
            s.set_azure_discoveries(discoveries.clone(), cx);
        });
        if let Some(cm) = self.overlays.connection_management.clone() {
            cm.update(cx, |cm, cx| {
                cm.set_azure_discoveries(discoveries, cx);
            });
        }
    }

    /// Spawn an async Azure SQL discovery across all registered Entra accounts.
    /// When complete, the results are pushed to the sidebar schema tree and
    /// the connection management dialog (if open). Replacing the task cancels
    /// any prior in-flight discovery.
    pub(super) fn trigger_azure_discovery(&mut self, cx: &mut Context<Self>) {
        let registry = self.entra_accounts_registry.clone();
        let keyring = self.entra_token_cache.clone();
        let transport = self.entra_http_transport.clone();
        let weak = cx.weak_entity();
        let async_cx = cx.to_async();

        if registry.list().is_empty() {
            // No accounts left — drop any cached/in-memory discoveries so
            // removed accounts' servers don't reappear on the next launch.
            if !self.azure_discoveries.is_empty() {
                self.azure_discoveries.clear();
                self.sidebar.update(cx, |s, cx| {
                    s.set_azure_discoveries(Vec::new(), cx);
                });
                if let Some(cm) = self.overlays.connection_management.clone() {
                    cm.update(cx, |cm, cx| {
                        cm.set_azure_discoveries(Vec::new(), cx);
                    });
                }
            }
            let _ = crate::azure_discovery_cache::save(
                &[],
                &crate::azure_discovery_cache::cache_file(),
            );
            let message = crate::localization::text("shell.azure.sign_in_before_discovery");
            if let Some(cm) = self.overlays.connection_management.clone() {
                cm.update(cx, |cm, cx| cm.set_azure_discovery_error(&message, cx));
            }
            if let Some(editor) = self.overlays.connection_editor.clone() {
                editor.update(cx, |editor, cx| {
                    editor.set_azure_discovery_error(&message, cx)
                });
            }
            self.set_status(StatusLevel::Info, message);
            cx.notify();
            return;
        }

        self.set_status(
            StatusLevel::Busy,
            crate::localization::text("shell.azure.discovering"),
        );
        cx.notify();

        self.azure_discovery_task = Some(cx.spawn(async move |_this, _cx| {
            let outcome = crate::azure_discovery_service::discover_azure_sql_resources(
                &registry, keyring, transport,
            )
            .await;
            let discovery_failed = outcome.failed_completely();
            let discoveries = outcome.discoveries;

            let mut cx = async_cx.clone();
            let _ = weak.update(&mut cx, |this, cx| {
                this.azure_discoveries = discoveries.clone();
                // Persist the fresh snapshot so the next launch can populate the
                // sidebar from cache without a live Azure round-trip.
                if let Err(err) = crate::azure_discovery_cache::save(
                    &discoveries,
                    &crate::azure_discovery_cache::cache_file(),
                ) {
                    eprintln!("failed to cache Azure SQL discoveries: {err}");
                }
                this.sidebar.update(cx, |s, cx| {
                    s.set_azure_discoveries(discoveries.clone(), cx);
                });
                let discovery_error =
                    crate::localization::text("shell.azure.discovery_permission_error");
                if let Some(cm) = this.overlays.connection_management.clone() {
                    cm.update(cx, |cm, cx| {
                        if discovery_failed {
                            cm.set_azure_discovery_error(&discovery_error, cx);
                        } else {
                            cm.set_azure_discoveries(discoveries.clone(), cx);
                        }
                    });
                }
                if let Some(editor) = this.overlays.connection_editor.clone() {
                    editor.update(cx, |editor, cx| {
                        if discovery_failed {
                            editor.set_azure_discovery_error(&discovery_error, cx);
                        } else {
                            editor.set_azure_discoveries(discoveries.clone(), cx);
                        }
                    });
                }
                if discoveries.is_empty() {
                    let (level, message) = if discovery_failed {
                        (
                            StatusLevel::Error,
                            crate::localization::text("shell.azure.discovery_failed"),
                        )
                    } else {
                        (
                            StatusLevel::Info,
                            crate::localization::text("shell.azure.no_servers"),
                        )
                    };
                    this.set_status(level, message);
                } else {
                    this.set_status(
                        StatusLevel::Success,
                        crate::localization::format(
                            "shell.azure.discovered",
                            &[("count", &discoveries.len().to_string())],
                        ),
                    );
                }
                cx.notify();
            });
        }));
    }

    /// Create a saved connection profile from a discovered Azure SQL database,
    /// save it (unless an equivalent profile already exists), refresh the
    /// sidebar, and connect.
    /// Ensures an `EntraBrowser` connection carries a **fresh** Azure SQL
    /// access token on `runtime_entra_access_token` before the actual connect
    /// or query work (`then`) runs. Connections that are not `EntraBrowser`
    /// (or have no bound Entra account) skip straight to `then` synchronously.
    ///
    /// Azure SQL access tokens live only ~1h, so this gate caches the minted
    /// token per Entra account with its expiry:
    ///
    /// * **Fast path** — a cached token that is still comfortably valid (more
    ///   than a 5-minute safety skew from expiry) is reused synchronously with
    ///   no network round-trip, so repeat queries/connects on a live session
    ///   are instant.
    /// * **Slow path** — no cached token, or one at/near expiry, triggers an
    ///   async refresh-token exchange against the SQL scope (a *re-mint*). The
    ///   result is cached and stamped onto the profile, so a long-lived session
    ///   that outlives the first token keeps working without a manual re-sign-in
    ///   (mirrors the macOS `validSQLAccessToken` refresh step).
    ///
    /// The mint is async, so `then` — the caller's original synchronous connect
    /// or query tail — is deferred into the async continuation on the slow path
    /// and receives the token-enriched profile. The token is also pushed into
    /// the schema tree so its lazy schema loads authenticate. This is the single
    /// choke point that makes discovered and saved Entra connections usable.
    pub(super) fn with_entra_sql_token(
        &mut self,
        profile: ConnectionProfile,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, ConnectionProfile, &mut Context<Self>) + 'static,
    ) {
        // Free-tier gate (App Store edition): saved connections beyond the
        // free allowance stay locked until the unlock is bought.
        if !self.ensure_saved_connection_usable(profile.id.0, cx) {
            cx.notify();
            return;
        }
        let profile = match crate::sql_credentials::hydrate_profile(profile) {
            Ok(profile) => profile,
            Err(error) => {
                self.set_status(
                    StatusLevel::Error,
                    crate::localization::format(
                        "shell.azure.password_load_failed",
                        &[("error", &crate::localization::message(error))],
                    ),
                );
                cx.notify();
                return;
            }
        };
        #[cfg(not(target_arch = "wasm32"))]
        if profile.authentication.is_cloud_iam() {
            let cache_key = crate::cloud_iam_token::CloudIamTokenProvider::cache_key(&profile);
            if let Some(token) = self
                .cloud_iam_token_provider
                .cached(&cache_key, &profile.authentication)
            {
                let mut enriched = profile;
                enriched.runtime_cloud_iam_token = Some(token.clone());
                let id = enriched.id.0;
                self.sidebar.update(cx, |s, cx| {
                    s.set_runtime_cloud_iam_token(id, token, cx);
                });
                then(self, enriched, cx);
                return;
            }
            self.set_status(
                StatusLevel::Busy,
                crate::localization::format(
                    "shell.azure.minting_cloud_token",
                    &[("name", &profile.name)],
                ),
            );
            cx.notify();
            let provider = self.cloud_iam_token_provider.clone();
            let app = cx.weak_entity();
            let async_cx = cx.to_async();
            async_cx
                .spawn(async move |async_cx: &mut gpui::AsyncApp| {
                    let outcome = provider.ensure(&profile).await;
                    let _ = app.update(async_cx, |app, cx| match outcome {
                        Ok(token) => {
                            let mut enriched = profile;
                            enriched.runtime_cloud_iam_token = Some(token.clone());
                            app.sidebar.update(cx, |s, cx| {
                                s.set_runtime_cloud_iam_token(enriched.id.0, token, cx);
                            });
                            then(app, enriched, cx);
                        }
                        Err(message) => {
                            app.set_status(StatusLevel::Error, message);
                            cx.notify();
                        }
                    });
                })
                .detach();
            return;
        }
        let binding = match (&profile.authentication, &profile.entra_account) {
            // Both interactive Entra methods bind the same account and mint the
            // SQL token through the same refresh-token exchange — device code is
            // not a special case here (mirrors the schema-tree token path).
            (
                sqlly_core::AuthenticationType::EntraBrowser
                | sqlly_core::AuthenticationType::EntraDeviceCode,
                Some(b),
            ) => b.clone(),
            // Non-Entra auth (SQL login, integrated) or an Entra profile with
            // no bound account: no SQL token to mint, connect directly.
            _ => {
                then(self, profile, cx);
                return;
            }
        };
        // Fast path: reuse a cached SQL token that is still comfortably valid.
        // The shared provider guards a 5-minute skew so the token never lapses
        // mid-operation.
        // Redis profiles sign in for the Redis audience, everything else for
        // Azure SQL; the provider caches each audience separately.
        let scope = crate::azure_sql_token::scope_for_profile(&profile);
        if let Some((token, _expires_at)) =
            self.entra_sql_token_provider
                .cached(&crate::azure_sql_token::scoped_cache_key(
                    &binding.account_id,
                    scope,
                ))
        {
            let mut enriched = profile;
            crate::azure_sql_token::stamp_entra_token(&mut enriched, token.clone());
            let id = enriched.id.0;
            self.sidebar.update(cx, |s, cx| {
                s.set_runtime_entra_token(id, token, cx);
            });
            then(self, enriched, cx);
            return;
        }

        // Slow path: mint (first connect) or re-mint (token expired) the SQL
        // token via an async refresh-token exchange through the shared provider,
        // which caches the result for every other connection surface to reuse.
        self.set_status(
            StatusLevel::Busy,
            crate::localization::format("shell.azure.signing_in", &[("name", &profile.name)]),
        );
        cx.notify();

        let provider = self.entra_sql_token_provider.clone();
        let app = cx.weak_entity();
        let async_cx = cx.to_async();
        async_cx
            .spawn(async move |async_cx: &mut gpui::AsyncApp| {
                let outcome = provider.ensure_token_for_scope(&binding, scope).await;
                let _ = app.update(async_cx, |app, cx| match outcome {
                    crate::azure_sql_token::SqlTokenOutcome::Token {
                        access_token,
                        expires_at: _,
                    } => {
                        let mut enriched = profile;
                        crate::azure_sql_token::stamp_entra_token(
                            &mut enriched,
                            access_token.clone(),
                        );
                        // Propagate to the schema tree so drilling into the
                        // server node authenticates on its lazy loads.
                        app.sidebar.update(cx, |s, cx| {
                            s.set_runtime_entra_token(enriched.id.0, access_token, cx);
                        });
                        then(app, enriched, cx);
                    }
                    crate::azure_sql_token::SqlTokenOutcome::SignInRequired => {
                        app.set_status(
                            StatusLevel::Error,
                            crate::localization::format(
                                "shell.azure.session_expired",
                                &[("name", &profile.name)],
                            ),
                        );
                        cx.notify();
                    }
                    crate::azure_sql_token::SqlTokenOutcome::AccountNotRegistered => {
                        app.set_status(
                            StatusLevel::Error,
                            crate::localization::format(
                                "shell.azure.account_not_signed_in",
                                &[("name", &profile.name)],
                            ),
                        );
                        cx.notify();
                    }
                    crate::azure_sql_token::SqlTokenOutcome::Failed(msg) => {
                        app.set_status(StatusLevel::Error, msg);
                        cx.notify();
                    }
                });
            })
            .detach();
    }

    /// Turn a discovered Azure SQL database into a saved connection profile —
    /// without connecting to it. Returns the profile, existing or new.
    ///
    /// Split out from [`Self::link_azure_database`] because adopting and
    /// connecting are two different intentions: clicking a discovered row in the
    /// explorer adopts it, and only the explicit "Link & Connect" menu item
    /// moves the query tab onto it.
    pub(super) fn adopt_azure_database(
        &mut self,
        discovery: &AzureSqlDiscovery,
        cx: &mut Context<Self>,
    ) -> ConnectionProfile {
        let name = format!("{}/{}", discovery.server_name, discovery.database_name);

        // Check for an existing profile that matches this discovered database
        // (same server FQDN + database name + Entra Browser auth). If one
        // exists, reuse it instead of creating a duplicate.
        let store = ConnectionStore::global();
        let existing = store.list().into_iter().find(|p| {
            p.server == discovery.server_fqdn
                && p.database.as_deref() == Some(&discovery.database_name)
                && matches!(
                    p.authentication,
                    sqlly_core::AuthenticationType::EntraBrowser
                )
        });

        match existing {
            Some(p) => p,
            None => {
                let mut p = ConnectionProfile::azure_sql_entra_browser(
                    &name,
                    &discovery.server_fqdn,
                    &discovery.database_name,
                );
                p.entra_account = Some(EntraAccountBinding {
                    account_id: discovery.account_id.clone(),
                    tenant_id: discovery.tenant_id.clone(),
                    username: Some(discovery.username.clone()),
                });
                p.provenance = ConnectionProvenance::AzureDiscovered;
                store.add(p.clone());
                self.sidebar.update(cx, |s, cx| s.refresh_connections(cx));
                p
            }
        }
    }

    pub(super) fn link_azure_database(
        &mut self,
        discovery: &AzureSqlDiscovery,
        cx: &mut Context<Self>,
    ) {
        let name = format!("{}/{}", discovery.server_name, discovery.database_name);
        let profile = self.adopt_azure_database(discovery, cx);

        self.set_status(
            StatusLevel::Busy,
            crate::localization::format("shell.azure.connecting", &[("name", &name)]),
        );
        let name_done = name.clone();
        self.with_entra_sql_token(profile, cx, move |app, profile, cx| {
            app.active_profile = Some(profile.clone());
            app.sidebar.update(cx, |s, cx| {
                s.set_bound_server(Some(profile.id.0), cx);
                s.load_databases(profile.clone(), cx);
            });
            app.ai_panel.update(cx, |p, cx| {
                p.set_profile(Some(profile.clone()), cx);
            });
            app.connect_data_model(profile.clone(), cx);
            app.set_status(
                StatusLevel::Busy,
                crate::localization::format("shell.azure.connecting", &[("name", &name_done)]),
            );
            cx.notify();
        });
        cx.notify();
    }

    pub(super) fn handle_entra_accounts_dialog_event(
        &mut self,
        _view: Entity<crate::entra_accounts_dialog::EntraAccountsDialog>,
        event: &crate::entra_accounts_dialog::EntraAccountsEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            crate::entra_accounts_dialog::EntraAccountsEvent::AddUserRequested => {
                // The dialog's "Add User" button was clicked. Start the same
                // interactive browser sign-in the menu entry point uses.
                if let Some(dlg) = self.overlays.entra_accounts_dialog.clone() {
                    self.spawn_entra_signin(dlg, cx);
                }
            }
            crate::entra_accounts_dialog::EntraAccountsEvent::RefreshCredentialsRequested(
                account,
            ) => {
                if let Some(dlg) = self.overlays.entra_accounts_dialog.clone() {
                    self.spawn_entra_credential_refresh(dlg, account.as_ref().clone(), cx);
                }
            }
            crate::entra_accounts_dialog::EntraAccountsEvent::Changed => {
                if let Some(ed) = self.overlays.connection_editor.clone() {
                    ed.update(cx, |editor, cx| {
                        editor.reload_entra_accounts();
                        cx.notify();
                    });
                }
                if let Some(cm) = self.overlays.connection_management.clone() {
                    cm.update(cx, |cm, cx| {
                        cm.reload_entra_accounts();
                        cx.notify();
                    });
                }
                if let Some(dlg) = self.overlays.entra_accounts_dialog.clone() {
                    dlg.update(cx, |dlg, cx| {
                        dlg.reload(cx);
                    });
                }
                // Re-discover Azure SQL resources for the updated account set.
                self.trigger_azure_discovery(cx);
                cx.notify();
            }
            crate::entra_accounts_dialog::EntraAccountsEvent::Dismissed => {
                // Restore while the dialog is still mounted so its inputs can
                // blur (and any open popover can unregister) before the entity
                // drops. Then drop it.
                self.restore_editor_focus(cx);
                self.overlays.entra_accounts_dialog = None;
                cx.notify();
            }
        }
    }

    pub(super) fn handle_azure_sql_connection(
        &mut self,
        _action: &AzureSqlConnection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Served connections are read-only, whatever the build.
        if self.refuse_if_connections_proxied(cx) {
            return;
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = window;
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.azure.browser_unavailable"),
            );
            cx.notify();
            return;
        }
        // Swift parity: the menu opens a dialog explaining that browser sign-in
        // is required. We open the Entra Account dialog directly so the user
        // can immediately click "Add User" and start signing in.
        #[cfg(not(target_arch = "wasm32"))]
        self.handle_show_entra_account_dialog(&ShowEntraAccountDialog, window, cx);
    }

    pub(super) fn handle_show_entra_account_dialog(
        &mut self,
        _action: &ShowEntraAccountDialog,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Build the dialog entity if absent and show it. Opening the dialog
        // must NOT start a browser sign-in on its own — the interactive flow
        // begins only when the user clicks "Add User", which emits
        // `EntraAccountsEvent::AddUserRequested` -> `spawn_entra_signin`.
        // (Mirrors the macOS app, where opening the account sheet shows the
        // list and the browser opens only on the explicit Add action.)
        if self.overlays.entra_accounts_dialog.is_none() {
            let registry = self.entra_accounts_registry.clone();
            let d =
                cx.new(|cx| crate::entra_accounts_dialog::EntraAccountsDialog::new(registry, cx));
            cx.subscribe(&d, Self::handle_entra_accounts_dialog_event)
                .detach();
            self.overlays.entra_accounts_dialog = Some(d);
        }
        cx.notify();
    }

    /// Drives an interactive Entra browser sign-in for the given dialog and
    /// threads the result back into it. Shared by the menu entry point
    /// ([`Self::handle_show_entra_account_dialog`]) and the dialog's own
    /// "Add User" button (via [`EntraAccountsEvent::AddUserRequested`]) so
    /// both surfaces start the same OAuth orchestration.
    pub(super) fn spawn_entra_signin(
        &mut self,
        dialog: Entity<crate::entra_accounts_dialog::EntraAccountsDialog>,
        cx: &mut Context<Self>,
    ) {
        if dialog.read(cx).is_in_flight() {
            return;
        }
        dialog.update(cx, |dlg, cx| {
            dlg.begin_signin(cx);
        });
        self.spawn_entra_signin_for_tenant(dialog, None, cx);
    }

    pub(super) fn spawn_entra_credential_refresh(
        &mut self,
        dialog: Entity<crate::entra_accounts_dialog::EntraAccountsDialog>,
        account: sqlly_services::security::azure::EntraAccountProfile,
        cx: &mut Context<Self>,
    ) {
        if dialog.read(cx).is_in_flight() {
            return;
        }
        dialog.update(cx, |dlg, cx| {
            dlg.begin_credential_refresh(&account, cx);
        });
        self.spawn_entra_signin_for_tenant(dialog, Some(account.tenant_id), cx);
    }

    pub(super) fn spawn_entra_signin_for_tenant(
        &mut self,
        dialog: Entity<crate::entra_accounts_dialog::EntraAccountsDialog>,
        tenant_id: Option<String>,
        cx: &mut Context<Self>,
    ) {
        // Run the orchestration off the UI thread. Capture the foreground
        // weak references synchronously (cx still borrows), then enter the
        // async block where AsyncApp gives us the AppContext required to
        // thread the result back into both the dialog entity and the
        // AppView's status bar. Mirrors the pattern in
        // `connection_management.rs::handle_test`.
        let dialog_weak = dialog.downgrade();
        let app_weak = cx.weak_entity();
        let transport = self.entra_http_transport.clone();
        let cache = self.entra_token_cache.clone();
        let registry = self.entra_accounts_registry.clone();
        let async_cx = cx.to_async();
        async_cx
            .spawn(async move |async_cx: &mut gpui::AsyncApp| {
                let result = crate::azure_signin::run_signin(
                    transport,
                    cache,
                    registry,
                    tenant_id,
                    std::time::Duration::from_secs(180),
                )
                .await;
                // Surface the result back to the dialog; the AppView status
                // bar is updated when the dialog forwards on_signin_* —
                // keeping the AppView's status_message in sync from two
                // places would race.
                match result {
                    Ok(outcome) => {
                        let _ = dialog_weak.update(async_cx, |dlg, cx| {
                            dlg.on_signin_success(&outcome, cx);
                        });
                    }
                    Err(error) => {
                        let _ = dialog_weak.update(async_cx, |dlg, cx| {
                            dlg.on_signin_error(error.to_string(), cx);
                        });
                    }
                }
                // Tell the AppView to redraw so the status bar / updated
                // picker pick up the new account list.
                let _ = app_weak.update(async_cx, |_app, cx| {
                    cx.notify();
                });
            })
            .detach();
    }

    /// Recognise an Azure SQL firewall block (error 40615) in an error the
    /// engine just reported and arm the opt-in prompt for it.
    ///
    /// The block is the one connection failure the user cannot fix from inside
    /// a SQL client: `sp_set_firewall_rule` lives on `master`, which is exactly
    /// what the firewall is refusing to let us reach. Rather than leaving the
    /// message in the results pane for the user to carry to the Azure portal,
    /// SQLLY offers the control-plane route it *can* still reach.
    ///
    /// `profile` names the connection that failed when the caller knows it
    /// (the schema tree loads against a server that need not be the active
    /// one); otherwise the active profile is assumed.
    pub(super) fn arm_firewall_prompt(
        &mut self,
        message: &str,
        profile: Option<ConnectionProfile>,
        cx: &mut Context<Self>,
    ) {
        use crate::azure_firewall::AzureSqlFirewallError;
        if !AzureSqlFirewallError::is_firewall_block(message) {
            return;
        }
        // One prompt at a time. A blocked connection usually fails several
        // operations at once (the query, the schema tree, the catalog refresh),
        // and three identical dialogs stacked on each other is not three times
        // the help.
        if self.overlays.firewall_rule_dialog.is_some() || self.pending.firewall_prompt.is_some() {
            return;
        }
        let Some(profile) = profile.or_else(|| self.active_profile.clone()) else {
            return;
        };
        // 40615 is an Azure SQL gateway error; anything else carrying that
        // text is not something an ARM firewall rule can fix.
        if !profile.is_azure_endpoint() {
            return;
        }
        let plan = self.firewall_prompt_plan(&profile);
        self.pending.firewall_prompt = Some(PendingFirewallPrompt {
            server: profile.server.clone(),
            blocked_ip: AzureSqlFirewallError::extract_blocked_ip(message),
            plan,
        });
        cx.notify();
    }

    /// Decide what the prompt can offer for `profile` — the ARM call, or an
    /// explanation of why SQLLY cannot make it.
    pub(super) fn firewall_prompt_plan(&self, profile: &ConnectionProfile) -> FirewallPromptPlan {
        #[cfg(target_arch = "wasm32")]
        {
            let _ = profile;
            return FirewallPromptPlan::Unavailable(if crate::browser_preview::served() {
                "SQLLY in the browser can't add an Azure firewall rule. Ask your \
                     administrator to allow the server SQLLY connects from."
                    .to_string()
            } else {
                "Illustrative only — the web preview can't reach Azure to add a firewall \
                     rule. Use the desktop app to open your Azure SQL server's firewall to your \
                     IP address."
                    .to_string()
            });
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            // The ARM call is made with the user's own Azure credential; a SQL
            // login has none, and no amount of retrying invents one.
            let Some(binding) = profile.entra_account.as_ref() else {
                return FirewallPromptPlan::Unavailable(format!(
                    "This connection signs in to {} with SQL authentication, so SQLLY has no \
                     Azure credential to change the server firewall with.\n\nReconnect with a \
                     Microsoft Entra account that is allowed to modify the server, or ask \
                     someone with access to the Azure portal to add a firewall rule for your \
                     IP address.",
                    profile.server
                ));
            };
            let account_id = firewall_flow::account_id_from_binding(&binding.account_id);
            let Some(registered) = self.entra_accounts_registry.find(&account_id) else {
                return FirewallPromptPlan::Unavailable(
                    firewall_flow::FirewallFlowOutcome::AccountNotRegistered
                        .render(&profile.server),
                );
            };
            let username = binding
                .username
                .clone()
                .unwrap_or_else(|| registered.username.clone());
            let default_rule_name = crate::azure_firewall::firewall_rule_name(
                &username,
                &firewall_flow::yyyymmdd_stamp(chrono::Utc::now()),
            );
            FirewallPromptPlan::Ready {
                account_id,
                account_label: (!username.trim().is_empty()).then_some(username),
                default_rule_name,
            }
        }
    }

    /// Turn an armed prompt into the actual dialog. Called from `render`,
    /// which is the first place after detection that has a `&mut Window` — the
    /// dialog's text fields are `InputState` entities and cannot be built
    /// without one.
    pub(super) fn materialize_firewall_prompt(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.overlays.firewall_rule_dialog.is_some() {
            return;
        }
        let Some(pending) = self.pending.firewall_prompt.take() else {
            return;
        };
        let (account_label, default_rule_name, target) = match &pending.plan {
            FirewallPromptPlan::Ready {
                account_id,
                account_label,
                default_rule_name,
            } => (
                account_label.clone(),
                default_rule_name.clone(),
                Some(account_id.clone()),
            ),
            FirewallPromptPlan::Unavailable(_) => (None, String::new(), None),
        };
        let server = pending.server.clone();
        let blocked_ip = pending.blocked_ip.clone();
        let dialog = cx.new(|cx| {
            crate::firewall_rule_dialog::FirewallRuleDialog::new(
                server.clone(),
                account_label,
                blocked_ip.clone(),
                default_rule_name,
                window,
                cx,
            )
        });
        if let FirewallPromptPlan::Unavailable(message) = &pending.plan {
            dialog.update(cx, |d, cx| d.set_unavailable(message.clone(), cx));
        }
        cx.subscribe(&dialog, Self::handle_firewall_dialog_event)
            .detach();
        self.firewall_prompt_target = target.map(|id| (server, id));
        self.overlays.firewall_rule_dialog = Some(dialog.clone());

        // The gateway names the blocked address in nearly every 40615, but not
        // in every one. When it did not, ask the echo service what the world
        // sees us as rather than handing the user an empty field.
        #[cfg(not(target_arch = "wasm32"))]
        if blocked_ip.is_none() && self.firewall_prompt_target.is_some() {
            let transport = self.entra_http_transport.clone();
            let weak = dialog.downgrade();
            cx.spawn_in(window, async move |_this, cx| {
                if let Ok(ip) = firewall_flow::lookup_public_ip(&transport).await {
                    let _ = weak.update_in(cx, |d, window, cx| d.set_looked_up_ip(ip, window, cx));
                }
            })
            .detach();
        }
        cx.notify();
    }

    pub(super) fn handle_firewall_dialog_event(
        &mut self,
        view: Entity<crate::firewall_rule_dialog::FirewallRuleDialog>,
        event: &crate::firewall_rule_dialog::FirewallRuleDialogEvent,
        cx: &mut Context<Self>,
    ) {
        use crate::firewall_rule_dialog::FirewallRuleDialogEvent as Ev;
        match event {
            Ev::Dismissed => {
                self.restore_editor_focus(cx);
                self.overlays.firewall_rule_dialog = None;
                self.firewall_prompt_target = None;
                cx.notify();
            }
            Ev::AddRequested {
                rule_name,
                start_ip,
                end_ip,
            } => {
                #[cfg(target_arch = "wasm32")]
                {
                    let _ = (rule_name, start_ip, end_ip);
                    view.update(cx, |d, cx| {
                        d.finish(
                            false,
                            crate::localization::text("shell.azure.firewall_web_unavailable"),
                            cx,
                        )
                    });
                }
                #[cfg(not(target_arch = "wasm32"))]
                {
                    let Some((server, account_id)) = self.firewall_prompt_target.clone() else {
                        return;
                    };
                    view.update(cx, |d, cx| d.begin_working(cx));
                    let transport = self.entra_http_transport.clone();
                    let keyring = self.entra_token_cache.clone();
                    let registry = self.entra_accounts_registry.clone();
                    let rule_name = rule_name.clone();
                    let start_ip = start_ip.clone();
                    let end_ip = end_ip.clone();
                    let weak = view.downgrade();
                    cx.to_async()
                        .spawn(async move |async_cx| {
                            let outcome = firewall_flow::run_firewall_flow_with_rule(
                                transport,
                                keyring,
                                registry,
                                &account_id,
                                &server,
                                &rule_name,
                                &start_ip,
                                &end_ip,
                                chrono::Utc::now(),
                            )
                            .await;
                            let (ok, body) = match &outcome {
                                firewall_flow::FirewallFlowOutcome::Applied {
                                    rule_name,
                                    start_ip,
                                    end_ip,
                                } => (
                                    true,
                                    crate::firewall_rule_dialog::applied_body(
                                        &server, rule_name, start_ip, end_ip,
                                    ),
                                ),
                                // Everything else — including the 403 that means
                                // "you do not have this permission" — is reported
                                // as it came back, never retried or worked around.
                                other => (false, other.render(&server)),
                            };
                            let _ = weak.update(async_cx, |d, cx| d.finish(ok, body, cx));
                        })
                        .detach();
                }
            }
        }
    }

    pub(super) fn handle_add_firewall_rule(
        &mut self,
        _action: &AddFirewallRule,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Browser preview: opening an Azure firewall rule needs a real Azure
        // ARM call a browser tab can't make. Say so plainly rather than walking
        // the user into a flow that can only fail.
        #[cfg(target_arch = "wasm32")]
        {
            self.show_tool_output(
                &crate::localization::text("shell.azure.firewall_title"),
                if crate::browser_preview::served() {
                    crate::localization::text("shell.azure.firewall_served_unavailable")
                } else {
                    crate::localization::text("shell.azure.firewall_preview_unavailable")
                },
                false,
                cx,
            );
            return;
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            // Resolve the active connection to its Azure context. The flow is
            // Entra-only: SQL login or Integrated profiles fall through with a
            // clear instruction so the user can pick the right authentication
            // mode.
            let active_snapshot = self.active_profile.as_ref().map(|p| {
                (
                    p.server.clone(),
                    p.entra_account.clone(),
                    p.preview().is_azure,
                )
            });
            let (server, binding, is_azure) = match active_snapshot {
                Some((server, binding, is_azure)) => (server, binding, is_azure),
                None => {
                    self.show_tool_output(
                        &crate::localization::text("shell.azure.firewall_title"),
                        crate::localization::text("shell.azure.firewall_connect_first"),
                        false,
                        cx,
                    );
                    return;
                }
            };
            if !is_azure {
                let body = crate::localization::format(
                    "shell.azure.firewall_not_azure",
                    &[("server", &server)],
                );
                self.show_tool_output(
                    &crate::localization::text("shell.azure.firewall_title"),
                    body,
                    false,
                    cx,
                );
                return;
            }
            let binding = match binding {
                Some(b) => b,
                None => {
                    let body = firewall_flow::FirewallFlowOutcome::NoEntraBinding.render(&server);
                    self.show_tool_output(
                        &crate::localization::text("shell.azure.firewall_title"),
                        body,
                        false,
                        cx,
                    );
                    return;
                }
            };
            // Confirm the binding has been registered locally (signed in earlier).
            // If not, send the user to the sign-in flow rather than attempting
            // an ARM call that will only fail with sign-in required.
            let registry = self.entra_accounts_registry.clone();
            let account_id = firewall_flow::account_id_from_binding(&binding.account_id);
            if registry.find(&account_id).is_none() {
                let body = firewall_flow::FirewallFlowOutcome::AccountNotRegistered.render(&server);
                self.show_tool_output(
                    &crate::localization::text("shell.azure.firewall_title"),
                    body,
                    false,
                    cx,
                );
                return;
            }

            // Show a pending state immediately; the async task will REPLACE this
            // content with the outcome when the ARM call returns.
            let pending = crate::localization::format(
                "shell.azure.firewall_contacting",
                &[("server", &server)],
            );
            self.show_tool_output(
                &crate::localization::text("shell.azure.firewall_title"),
                pending,
                false,
                cx,
            );

            // Spawn the fire-and-forget task: do the IP lookup, refresh the
            // cached token silently, run the ARM provisioner, then re-render the
            // tool-output panel with the outcome.
            let transport = self.entra_http_transport.clone();
            let keyring = self.entra_token_cache.clone();
            let username = binding.username.clone().unwrap_or_else(|| {
                registry
                    .list()
                    .into_iter()
                    .find(|p| p.id == account_id)
                    .map(|p| p.username.clone())
                    .unwrap_or_else(|| "user".to_string())
            });
            let app_weak = cx.weak_entity();
            cx.to_async()
                .spawn(async move |async_cx| {
                    let now = chrono::Utc::now();
                    let outcome = firewall_flow::run_firewall_flow(
                        transport,
                        keyring,
                        registry,
                        &account_id,
                        &server,
                        &username,
                        now,
                    )
                    .await;
                    let body = outcome.render(&server);
                    let title = crate::localization::text("shell.azure.firewall_title");
                    let _ = app_weak.update(async_cx, |app, cx| {
                        app.show_tool_output(&title, body, false, cx);
                    });
                })
                .detach();
        }
    }
}
