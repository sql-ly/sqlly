use super::*;

impl AppView {
    /// Connect to a profile from a UI surface other than the sidebar (here:
    /// the query-bar server dropdown). Mirrors the
    /// `ConnectionManagementEvent::ConnectRequested` path: set active profile,
    /// tell the sidebar to mark the connection active + load its databases,
    /// push the profile to the AI panel, and refresh the completion catalog.
    pub(super) fn connect_profile(&mut self, profile: ConnectionProfile, cx: &mut Context<Self>) {
        self.connect_profile_then(profile, cx, |_, _| {});
    }

    /// Connect and run `then` only once the connection is fully established
    /// (after [`Self::mark_connected`]) — i.e. after any async Entra token
    /// minting or relay session setup completes. On failure `then` never runs,
    /// so callers can gate follow-up work that must not land on the previous
    /// connection (like opening a query tab bound to the target).
    pub(super) fn connect_profile_then(
        &mut self,
        mut profile: ConnectionProfile,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, &mut Context<Self>) + 'static,
    ) {
        if !self.ensure_saved_connection_usable(profile.id.0, cx) {
            cx.notify();
            return;
        }
        if let Err(message) = ConnectionStore::resolve_tunnel_profile(&mut profile) {
            self.set_status(StatusLevel::Error, message);
            cx.notify();
            return;
        }
        // A Via-Relay profile establishes its relay session first (no Azure SQL
        // token is minted app-side — the on-prem bridge owns the DB credential).
        #[cfg(not(target_arch = "wasm32"))]
        if matches!(
            profile.route,
            sqlly_core::connection::ConnectionRoute::ViaRelay { .. }
        ) {
            self.connect_via_relay_then(profile, cx, then);
            return;
        }
        // Visible immediately; an Entra profile can spend seconds minting a
        // SQL token before `mark_connected` overwrites this with Success.
        // The direct path overwrites it in the same update — no flicker.
        self.set_status(
            StatusLevel::Busy,
            crate::localization::format("shell.connections.connecting", &[("name", &profile.name)]),
        );
        self.with_entra_sql_token(profile, cx, move |app, profile, cx| {
            app.mark_connected(profile, cx);
            then(app, cx);
        });
    }

    /// Connect by profile id — the welcome canvas's row click. Hydrates the
    /// stored profile (credentials ride along) and takes the normal connect
    /// path, relay branch included.
    pub(crate) fn connect_profile_id(&mut self, id: &str, cx: &mut Context<Self>) {
        let Ok(uuid) = uuid::Uuid::parse_str(id) else {
            return;
        };
        if let Some(profile) = ConnectionStore::global().get(uuid) {
            self.connect_profile(profile, cx);
        }
        cx.notify();
    }

    /// Ctrl+Shift+N: connect the active tab to the Nth connection in the picker's order.
    /// Goes through `connect_profile_id`, so Entra/relay setup, the production banner and
    /// read-only guards behave exactly as if the user had picked it in the connect picker.
    pub(super) fn handle_quick_switch_connection(&mut self, n: usize, cx: &mut Context<Self>) {
        let profiles = ConnectionStore::global().list();
        match crate::connect_picker::nth_connection_id(&profiles, n) {
            Some(id) => {
                self.connect_profile_id(&id, cx);
            }
            None => self.set_status(
                StatusLevel::Info,
                crate::localization::format(
                    "shell.connections.quick_switch_missing",
                    &[("number", &n.to_string())],
                ),
            ),
        }
        cx.notify();
    }

    /// While a relay proxy provides the connections, every add / duplicate /
    /// delete / import entry point declines with the read-only copy on the
    /// status line. Returns `true` when the caller must stop.
    pub(crate) fn refuse_if_connections_proxied(&mut self, cx: &mut Context<Self>) -> bool {
        if !crate::connection::connections_are_proxied() {
            return false;
        }
        self.set_status(
            StatusLevel::Info,
            crate::localization::text("shell.connections.proxy_read_only"),
        );
        cx.notify();
        true
    }

    /// Open the connection editor on a blank profile. Shared by the sidebar's
    /// add button (`SidebarEvent::AddConnectionRequested`) and the welcome
    /// canvas's New Connection button.
    pub(crate) fn open_new_connection_editor(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.refuse_if_connections_proxied(cx) {
            return;
        }
        if !self.ensure_room_for_saved_connections(1, cx) {
            return;
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = window;
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.connections.add_browser_unavailable"),
            );
            cx.notify();
            return;
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let azure = self.azure_discoveries.clone();
            let editor = cx.new(|cx| {
                let mut editor = ConnectionEditor::new(
                    self.service_router.clone(),
                    self.log.clone(),
                    self.entra_accounts_registry.clone(),
                    window,
                    cx,
                );
                editor.set_azure_discoveries(azure, cx);
                editor.set_entra_sql_token_provider(self.entra_sql_token_provider.clone());
                editor
            });
            cx.subscribe(&editor, Self::handle_editor_modal_event)
                .detach();
            self.overlays.connection_editor = Some(editor);
            cx.notify();
        }
    }

    /// Switch the active database context to `database` for the current
    /// connection. Updates `active_profile.database`, reloads the schema tree
    /// (which re-runs `list_databases` for the updated profile), and refreshes
    /// the completion catalog so editor intellisense targets the new database.
    /// Used by the query-bar database dropdown.
    pub(super) fn switch_database(&mut self, database: String, cx: &mut Context<Self>) {
        let Some(mut profile) = self.active_profile.clone() else {
            return;
        };
        profile.database = Some(database);
        self.active_profile = Some(profile.clone());
        // Per-tab pin follows the database picker too.
        self.stamp_active_tab_connection();
        // Re-mint the Azure SQL token if it has expired before the schema
        // reload / completion refresh (both hit the engine). Synchronous fast
        // path when the cached token is still valid or the auth is non-Entra.
        self.with_entra_sql_token(profile, cx, move |app, profile, cx| {
            if app.active_profile.as_ref().map(|p| p.id) == Some(profile.id) {
                app.active_profile = Some(profile.clone());
            }
            app.sidebar
                .update(cx, |s, cx| s.load_databases(profile.clone(), cx));
            app.load_data_model(profile, false, cx);
            cx.notify();
        });
    }

    /// Chip / breadcrumb / Select confirm all share this path.
    pub(super) fn select_server_by_name(&mut self, name: String, cx: &mut Context<Self>) {
        let store = ConnectionStore::global();
        if let Some(profile) = self
            .sidebar
            .read(cx)
            .connections()
            .iter()
            .find(|p| p.name == name)
            .and_then(|p| store.get(p.id.0))
        {
            self.connect_profile(profile, cx);
        }
        cx.notify();
    }

    pub(super) fn select_database_by_name(&mut self, database: String, cx: &mut Context<Self>) {
        self.switch_database(database, cx);
        cx.notify();
    }

    /// Adopt settings just saved for the *currently connected* profile, without
    /// making the user reconnect (or, as it was, relaunch).
    ///
    /// `active_profile` is a clone taken at connect time, and it is the copy
    /// every query is executed against — `execute_query` hands it to the driver,
    /// which reads `block_all_but_select` and friends straight off it. Editing
    /// those flags used to write only to the store, so the running session went
    /// on enforcing the policy it had connected with.
    ///
    /// Runtime state that never came from disk is preserved: the hydrated
    /// password and any minted Entra token are re-derived by
    /// `with_entra_sql_token`, and a database picked from the query bar survives
    /// unless the editor names one itself.
    pub(super) fn adopt_saved_profile(&mut self, saved: ConnectionProfile, cx: &mut Context<Self>) {
        let Some(active) = self.active_profile.as_ref() else {
            return;
        };
        if active.id != saved.id {
            return;
        }
        // The profile was edited (credentials, Entra account, TLS, …): warm
        // clients from the pre-edit session must never be reused. Evict both
        // the old and the saved identity — the pool key can move when the
        // edit changed the database, account binding, or TLS flags.
        self.services.queries.evict_profile(active);
        self.services.queries.evict_profile(&saved);
        // The edit may have moved the server, credentials, or TLS out from
        // under every database snapshot we cached for this connection, so drop
        // them; the next selection re-pulls fresh.
        self.datamodel_cache
            .retain(|(pid, _), _| *pid != saved.id.0);
        self.datamodel_cache_order
            .retain(|(pid, _)| *pid != saved.id.0);
        let mut next = saved;
        // "All databases" (no database on the profile) leaves the query bar's
        // choice as the only statement of context there is; keep it.
        if next.database.as_deref().unwrap_or("").is_empty() {
            next.database = active.database.clone();
        }
        self.with_entra_sql_token(next, cx, move |app, profile, cx| {
            app.active_profile = Some(profile.clone());
            app.ai_panel.update(cx, |p, cx| {
                p.set_profile(Some(profile), cx);
            });
            cx.notify();
        });
    }

    pub(super) fn handle_editor_modal_event(
        &mut self,
        _editor: Entity<ConnectionEditor>,
        event: &ConnectionEditorEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            ConnectionEditorEvent::Saved(profile) => {
                self.restore_editor_focus(cx);
                self.overlays.connection_editor = None;
                // A profile the explorer did not list before this save (a
                // new connection, or a duplicate) is revealed and selected
                // once the tree has picked it up, so the user sees where it
                // landed; an edit leaves the selection where it was.
                let is_new = !self
                    .sidebar
                    .read(cx)
                    .connections()
                    .iter()
                    .any(|p| p.id == profile.id);
                let node_id = format!("srv:{}", profile.id.0);
                self.sidebar.update(cx, |s, cx| {
                    s.refresh_connections(cx);
                    if is_new {
                        s.reveal_node_id(&node_id, cx);
                    }
                });
                self.adopt_saved_profile(profile.as_ref().clone(), cx);
                self.set_status(
                    StatusLevel::Success,
                    crate::localization::text("shell.connections.saved"),
                );
                cx.notify();
            }
            ConnectionEditorEvent::Cancelled => {
                self.restore_editor_focus(cx);
                self.overlays.connection_editor = None;
                cx.notify();
            }
            ConnectionEditorEvent::RefreshAzureServersRequested => {
                self.trigger_azure_discovery(cx);
            }
            ConnectionEditorEvent::ManageTunnelProfiles => {
                #[cfg(not(target_arch = "wasm32"))]
                {
                    self.overlays.pending_tunnel_profiles = true;
                    cx.notify();
                }
            }
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn open_tunnel_profiles_dialog(
        &mut self,
        probe_profile: Option<ConnectionProfile>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.overlays.tunnel_profiles_dialog.is_none() {
            let d = cx.new(|cx| {
                crate::tunnel_profiles_dialog::TunnelProfilesDialog::new(probe_profile, window, cx)
            });
            cx.subscribe(&d, Self::handle_tunnel_profiles_dialog_event)
                .detach();
            self.overlays.tunnel_profiles_dialog = Some(d);
        }
        cx.notify();
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn handle_tunnel_profiles_dialog_event(
        &mut self,
        _view: Entity<crate::tunnel_profiles_dialog::TunnelProfilesDialog>,
        event: &crate::tool_dialog::ToolDialogEvent,
        cx: &mut Context<Self>,
    ) {
        if matches!(event, crate::tool_dialog::ToolDialogEvent::Close) {
            self.overlays.tunnel_profiles_dialog = None;
            if let Some(editor) = &self.overlays.connection_editor {
                editor.update(cx, |editor, cx| {
                    editor.tunnel_profile_select_stale = true;
                    cx.notify();
                });
            }
            cx.notify();
        }
    }

    pub(super) fn handle_management_event(
        &mut self,
        _view: Entity<ConnectionManagement>,
        event: &ConnectionManagementEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            ConnectionManagementEvent::Saved(profile) => {
                self.sidebar.update(cx, |s, cx| s.refresh_connections(cx));
                self.adopt_saved_profile(profile.clone(), cx);
                self.set_status(
                    StatusLevel::Success,
                    crate::localization::format(
                        "shell.connections.saved_with_name",
                        &[("name", &profile.name)],
                    ),
                );
                cx.notify();
            }
            ConnectionManagementEvent::ConnectRequested(profile) => {
                self.restore_editor_focus(cx);
                self.overlays.connection_management = None;
                #[cfg(not(target_arch = "wasm32"))]
                if matches!(
                    profile.route,
                    sqlly_core::connection::ConnectionRoute::ViaRelay { .. }
                ) {
                    self.connect_via_relay(profile.clone(), cx);
                    return;
                }
                self.with_entra_sql_token(profile.clone(), cx, move |app, profile, cx| {
                    app.mark_connected(profile, cx);
                });
            }
            ConnectionManagementEvent::Deleted(id) => {
                self.sidebar.update(cx, |s, cx| s.refresh_connections(cx));
                // A deleted profile's relay session must die with it — leaving
                // it cached would keep routing (and holding the bridge open)
                // for a connection that no longer exists.
                self.clear_remote_services(&sqlly_core::ConnectionId(*id));
                if self.active_profile.as_ref().map(|p| p.id.0) == Some(*id) {
                    if let Some(profile) = self.active_profile.take() {
                        // Drop warm clients for the deleted connection now —
                        // they could never be legitimately reused.
                        self.services.queries.evict_profile(&profile);
                    }
                    self.sidebar.update(cx, |s, cx| {
                        s.set_bound_server(None, cx);
                    });
                    self.ai_panel.update(cx, |p, cx| {
                        p.set_profile(None, cx);
                    });
                    self.clear_completion_catalogs(cx);
                }
                self.set_status(
                    StatusLevel::Success,
                    crate::localization::text("shell.connections.deleted"),
                );
                cx.notify();
            }
            ConnectionManagementEvent::Cancelled => {
                self.restore_editor_focus(cx);
                self.overlays.connection_management = None;
                cx.notify();
            }
            ConnectionManagementEvent::PoppedOut => {
                // The detached window renders the same entity now — drop only
                // the inline card. Deliberately no `restore_editor_focus`:
                // the dialog is still open, just elsewhere.
                self.overlays.connection_management = None;
                cx.notify();
            }
            ConnectionManagementEvent::LinkAzureDatabase(discovery) => {
                self.restore_editor_focus(cx);
                self.overlays.connection_management = None;
                self.link_azure_database(discovery, cx);
            }
            ConnectionManagementEvent::AddEntraAccountRequested => {
                if let Some(cm) = self.overlays.connection_management.clone() {
                    self.spawn_entra_signin_for_management(cm, cx);
                }
            }
            ConnectionManagementEvent::RefreshAzureServersRequested => {
                self.trigger_azure_discovery(cx);
            }
            ConnectionManagementEvent::EntitiesChanged => {
                self.sidebar.update(cx, |s, cx| s.refresh_connections(cx));
                self.set_status(
                    StatusLevel::Success,
                    crate::localization::text("shell.connections.organization_updated"),
                );
                cx.notify();
            }
            ConnectionManagementEvent::UnlockRequested => {
                let reason = crate::license::saved_connection_limit_now()
                    .map(crate::license::connection_limit_reason);
                self.open_unlock_dialog(reason, cx);
            }
        }
    }

    /// Runs the interactive Entra browser sign-in for the connection
    /// workspace's Accounts pane, threading the outcome back into the
    /// workspace entity (mirrors [`Self::spawn_entra_signin`], which targets
    /// the standalone accounts dialog).
    pub(super) fn spawn_entra_signin_for_management(
        &mut self,
        management: Entity<ConnectionManagement>,
        cx: &mut Context<Self>,
    ) {
        management.update(cx, |cm, cx| {
            cm.begin_entra_signin(cx);
        });

        let management_weak = management.downgrade();
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
                    None,
                    std::time::Duration::from_secs(180),
                )
                .await;
                match result {
                    Ok(outcome) => {
                        let _ = management_weak.update(async_cx, |cm, cx| {
                            cm.on_signin_success(&outcome, cx);
                        });
                        // Re-discover Azure SQL resources for the new account.
                        let _ = app_weak.update(async_cx, |app, cx| {
                            app.trigger_azure_discovery(cx);
                            cx.notify();
                        });
                    }
                    Err(error) => {
                        let _ = management_weak.update(async_cx, |cm, cx| {
                            cm.on_signin_error(error.to_string(), cx);
                        });
                    }
                }
            })
            .detach();
    }

    pub(super) fn handle_entity_management_event(
        &mut self,
        _view: Entity<EntityManagement>,
        event: &EntityManagementEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            EntityManagementEvent::Changed => {
                // Entity edits persist to the connection store; refresh the
                // sidebar (grouping/entity display) and the connection manager
                // so the new organization is reflected immediately.
                self.sidebar.update(cx, |s, cx| s.refresh_connections(cx));
                if let Some(cm) = self.overlays.connection_management.clone() {
                    cm.update(cx, |_, cx| cx.notify());
                }
                self.set_status(
                    StatusLevel::Success,
                    crate::localization::text("shell.connections.entity_organization_updated"),
                );
                cx.notify();
            }
            EntityManagementEvent::Cancelled => {
                self.restore_editor_focus(cx);
                self.overlays.entity_management = None;
                cx.notify();
            }
        }
    }

    pub(super) fn handle_disconnect_action(
        &mut self,
        _action: &Disconnect,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.execute_command("connection.disconnect", window, cx);
    }

    pub(super) fn handle_show_connection_management(
        &mut self,
        _action: &ShowConnectionManagement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        tracing::debug!("action: ShowConnectionManagement -> open connection management");
        if self.overlays.connection_management.is_some() {
            return;
        }
        let azure = self.azure_discoveries.clone();
        let view = cx.new(|cx| {
            let mut cm = ConnectionManagement::new(
                self.service_router.clone(),
                self.log.clone(),
                self.entra_accounts_registry.clone(),
                window,
                cx,
            );
            cm.set_azure_discoveries(azure, cx);
            cm.set_entra_sql_token_provider(self.entra_sql_token_provider.clone());
            cm
        });
        cx.subscribe(&view, Self::handle_management_event).detach();
        self.overlays.connection_management = Some(view);
        cx.notify();
    }

    pub(super) fn handle_show_entity_management(
        &mut self,
        _action: &ShowEntityManagement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        tracing::debug!("action: ShowEntityManagement -> open entity management");
        if self.overlays.entity_management.is_some() {
            return;
        }
        let view = cx.new(|cx| EntityManagement::new(window, cx));
        cx.subscribe(&view, Self::handle_entity_management_event)
            .detach();
        self.overlays.entity_management = Some(view);
        cx.notify();
    }

    pub(super) fn handle_connect(
        &mut self,
        _action: &Connect,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.overlays.connection_management.is_some() {
            return;
        }
        let azure = self.azure_discoveries.clone();
        let view = cx.new(|cx| {
            let mut cm = ConnectionManagement::new(
                self.service_router.clone(),
                self.log.clone(),
                self.entra_accounts_registry.clone(),
                window,
                cx,
            );
            cm.set_azure_discoveries(azure, cx);
            cm.set_entra_sql_token_provider(self.entra_sql_token_provider.clone());
            cm
        });
        cx.subscribe(&view, Self::handle_management_event).detach();
        self.overlays.connection_management = Some(view);
        cx.notify();
    }

    /// Measure baseline round-trip latency by running `SELECT 1` against the
    /// server. Called once on connect. Stores the result in the results view's
    /// `estimated_latency_ms` so it can be subtracted from observed timings.
    pub(super) fn measure_latency(&mut self, profile: ConnectionProfile, cx: &mut Context<Self>) {
        let results_entity = match self.tabs.get(self.active_tab) {
            Some(tab) => tab.results.clone(),
            None => return,
        };
        // Measure the latency of the path this connection actually uses (the
        // relay for a Via-Relay profile, the in-process engine for a Direct one).
        let services = self.services_for_profile(&profile);
        let row_buffer = 1usize;
        let (tx, mut rx) = tokio::sync::mpsc::channel::<u64>(1);

        cx.background_spawn(async move {
            let start = Instant::now();
            if let Ok(Ok(handle)) = crate::tokio_handle()
                .spawn(async move {
                    services
                        .queries
                        .execute(&profile, "SELECT 1", row_buffer)
                        .await
                })
                .await
            {
                let mut events = handle.events;
                if events.recv().await.is_some() {
                    let elapsed = start.elapsed().as_millis() as u64;
                    let _ = tx.send(elapsed).await;
                }
            }
        })
        .detach();

        cx.spawn(async move |_this, cx| {
            if let Some(elapsed) = rx.recv().await {
                results_entity.update(cx, |r, cx| {
                    r.set_estimated_latency(elapsed, cx);
                });
            }
        })
        .detach();
    }

    /// An Auto-detect Redis profile learns its flavor (Valkey, KeyDB,
    /// DragonflyDB, Redis) from `INFO server` once connected, and keeps it so
    /// flavor-specific behavior applies from then on. A managed AWS host
    /// already decided the flavor in the editor, so it is not probed.
    pub(super) fn detect_engine_flavor(
        &mut self,
        profile: &ConnectionProfile,
        cx: &mut Context<Self>,
    ) {
        if !profile.engine_auto_detect
            || !profile.database_type.is_command_engine()
            || sqlly_core::redis_flavor::from_host(&profile.server).is_some()
        {
            return;
        }
        let services = self.services_for_profile(profile);
        let probe_profile = profile.clone();
        let (tx, rx) = tokio::sync::oneshot::channel::<Option<String>>();
        cx.background_spawn(async move {
            let product = crate::tokio_handle()
                .spawn(async move {
                    sqlly_services::collect_server_info(services.queries.as_ref(), &probe_profile)
                        .await
                        .ok()
                        .map(|info| info.product)
                })
                .await
                .ok()
                .flatten();
            let _ = tx.send(product);
        })
        .detach();
        let id = profile.id.0;
        let server = profile.server.clone();
        cx.spawn(async move |this, cx| {
            let Ok(Some(product)) = rx.await else {
                return;
            };
            let engine = sqlly_core::redis_flavor::detected(&server, Some(&product));
            if !ConnectionStore::global().set_detected_engine(id, engine) {
                return;
            }
            this.update(cx, |this, cx| {
                if let Some(active) = this.active_profile.as_mut()
                    && active.id.0 == id
                {
                    active.database_type = engine;
                }
                this.sidebar.update(cx, |s, cx| s.refresh_connections(cx));
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Resolve the [`Services`] that drive engine operations for `profile`.
    ///
    /// * [`ConnectionRoute::Direct`](sqlly_core::ConnectionRoute::Direct) always
    ///   uses the in-process services — identical to before routing existed, so
    ///   the Direct path is byte-for-byte unchanged.
    /// * [`ConnectionRoute::ViaRelay`](sqlly_core::ConnectionRoute::ViaRelay)
    ///   uses the remote-backed services established over the relay. With no
    ///   established session every engine-backed call fails with clear
    ///   "connect the relay session" copy instead of silently dialing the
    ///   database direct (see [`crate::service_router::ServiceRouter`]).
    pub(crate) fn services_for_profile(&self, profile: &ConnectionProfile) -> Services {
        self.service_router.services_for(profile)
    }

    /// Install a remote-backed [`Services`] for a Via-Relay connection, so
    /// subsequent engine operations on that profile route through the relay.
    /// Called by [`Self::connect_via_relay`] once the relay session is open.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn install_remote_services(
        &mut self,
        id: sqlly_core::ConnectionId,
        services: Services,
        route_facts: crate::status_bar::RelayRouteFacts,
        closed: tokio::sync::watch::Receiver<bool>,
        cx: &mut Context<Self>,
    ) {
        self.service_router.install(id, services);
        self.remote_route_facts.insert(id, route_facts);
        let generation = self.bump_remote_session_generation(id);
        self.watch_remote_session_loss(id, generation, closed, cx);
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn bump_remote_session_generation(&mut self, id: sqlly_core::ConnectionId) -> u64 {
        let generation = self.remote_session_generations.entry(id).or_insert(0);
        *generation += 1;
        *generation
    }

    /// Turn the relay transport's own lifecycle into explorer truth: the
    /// moment the session dies while still installed (bridge dropped, relay
    /// gone), the server flips to Unreachable — red means "we know it is not
    /// reachable", and the relay knew before any probe could.
    #[cfg(not(target_arch = "wasm32"))]
    fn watch_remote_session_loss(
        &mut self,
        id: sqlly_core::ConnectionId,
        generation: u64,
        mut closed: tokio::sync::watch::Receiver<bool>,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            // Wait for the transport to die; a dropped sender is the same
            // event (the demux task ended).
            while !*closed.borrow() {
                if closed.changed().await.is_err() {
                    break;
                }
            }
            this.update(cx, |app, cx| {
                // Only the still-current session may mark the server down: an
                // explicit disconnect or a replacement session bumps the
                // generation first.
                if app.remote_session_generations.get(&id) != Some(&generation) {
                    return;
                }
                app.clear_remote_services(&id);
                crate::connection::ConnectionStore::global().record_failure_message(
                    id.0,
                    chrono::Utc::now(),
                    &crate::localization::deferred_text("shell.connections.relay_lost"),
                );
                app.sidebar.update(cx, |sidebar, cx| {
                    sidebar.set_connection_state(
                        id.0,
                        crate::schema_tree::ConnectionState::Unreachable,
                        cx,
                    );
                });
                app.set_status(
                    StatusLevel::Error,
                    crate::localization::text("shell.connections.relay_lost"),
                );
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Drop any cached remote session for `id` (on disconnect or profile
    /// deletion) so a later reconnect re-establishes the relay session cleanly.
    pub(crate) fn clear_remote_services(&mut self, id: &sqlly_core::ConnectionId) {
        self.service_router.remove(id);
        self.remote_route_facts.remove(id);
        #[cfg(not(target_arch = "wasm32"))]
        {
            // Deliberate teardown: invalidate any session-loss watcher so the
            // transport closing under it cannot be reported as a lost
            // connection.
            self.bump_remote_session_generation(*id);
            sqlly_engines::ssh::TunnelManager::global().close(id);
            // Redis Profiler / Pub/Sub captures hold their own sockets; stop
            // them wherever their pane lives (docked or popped out).
            sqlly_services::redis_live::stop_captures_for_profile(
                id.0,
                sqlly_services::redis_live::DISCONNECTED_REASON,
            );
        }
    }

    /// Push the services the active `profile` should drive into every
    /// engine-backed subsystem (schema tree, AI panel, editor tabs), so *all*
    /// DB features — not just the primary query path — route the same way the
    /// connection does. For a Direct profile this is the in-process services
    /// (unchanged behavior); for a Via-Relay profile with an established
    /// session it is the relay-backed services.
    pub(super) fn distribute_services(
        &mut self,
        profile: &ConnectionProfile,
        cx: &mut Context<Self>,
    ) {
        let services = self.services_for_profile(profile);
        self.sidebar
            .update(cx, |s, cx| s.set_services(services.clone(), cx));
        self.ai_panel
            .update(cx, |p, cx| p.set_services(services.clone(), cx));
        for tab in &self.tabs {
            tab.editor
                .update(cx, |e, cx| e.set_services(services.clone(), cx));
        }
    }

    /// Shared "connection is live" finish: adopt `profile` as active, route
    /// every subsystem's services to it, load its databases, prime completion,
    /// and report success. Both the Direct and Via-Relay connect paths funnel
    /// through here so the two behave identically once a session exists.
    pub(super) fn mark_connected(&mut self, profile: ConnectionProfile, cx: &mut Context<Self>) {
        let name = profile.name.clone();
        self.active_profile = Some(profile.clone());
        // A fresh attempt is in flight: a leftover red dot from an earlier
        // failure is stale now. Don't claim green yet — DatabasesLoaded does
        // that once enumeration proves the server is really reachable.
        self.sidebar.update(cx, |s, cx| {
            s.clear_unreachable(profile.id.0, cx);
        });
        // The focused tab owns this connection now: switching back to it
        // later switches the connection back.
        self.stamp_active_tab_connection();
        self.distribute_services(&profile, cx);
        self.sidebar.update(cx, |s, cx| {
            s.set_bound_server(Some(profile.id.0), cx);
            s.load_databases(profile.clone(), cx);
        });
        self.ai_panel.update(cx, |p, cx| {
            p.set_profile(Some(profile.clone()), cx);
        });
        self.load_data_model(profile.clone(), false, cx);
        self.detect_engine_flavor(&profile, cx);
        self.measure_latency(profile, cx);
        // A live session is not a browsable one — the databases still have to
        // enumerate. Say "Connecting…"; DatabasesLoaded/LoadFailed settle it, so
        // a server that connects but can't enumerate never reads green.
        self.set_status(
            StatusLevel::Busy,
            crate::localization::format(
                "shell.connections.active_connection_connecting",
                &[("name", &name)],
            ),
        );
        self.clear_pending_resume_on_manual_connect();
        cx.notify();
    }

    /// Disconnect one connection profile, wherever it appears: roll back its
    /// manual transactions, tear down its relay session, apply the tab
    /// disconnect policy to the tabs bound to it, and return its server node
    /// to the default gray state. Per profile — other connected servers are
    /// untouched. When the profile is also the focused window's connection,
    /// the window-scope state (query bar, AI panel, completion catalogs)
    /// is cleared too.
    pub(crate) fn disconnect_profile(&mut self, profile_id: uuid::Uuid, cx: &mut Context<Self>) {
        if let Some(tab_idx) = self.first_dirty_manual_tab_for_profile(&profile_id) {
            let _ = self.arm_transaction_close_if_dirty(
                tab_idx,
                TransactionCloseReason::Disconnect(profile_id),
                cx,
            );
            return;
        }
        self.rollback_all_for_profile(profile_id, cx);
        let is_active = self
            .active_profile
            .as_ref()
            .is_some_and(|p| p.id.0 == profile_id);
        // Tear down any relay session for the connection being closed so the
        // relay/bridge free it and a later reconnect re-establishes a fresh
        // session instead of reusing a stale cached one.
        self.clear_remote_services(&sqlly_core::ConnectionId(profile_id));
        if is_active {
            self.active_profile = None;
            self.sidebar.update(cx, |s, cx| {
                s.set_bound_server(None, cx);
            });
            self.ai_panel.update(cx, |p, cx| {
                p.set_profile(None, cx);
            });
            self.clear_completion_catalogs(cx);
            self.clear_auto_refresh_on_disconnect(cx);
        }
        // Which tabs this disconnect touches: tabs pinned to this profile —
        // plus, only when it is the focused window's connection, the
        // untargeted tabs that follow that connection.
        let targets_profile = move |tab: &crate::tabs::QueryTab| {
            if is_active {
                crate::tabs::tab_targets_connection(tab, Some(&profile_id))
            } else {
                !tab.snapshot_only
                    && tab.ddl_view.is_none()
                    && tab
                        .connection_target
                        .as_ref()
                        .is_some_and(|t| t.profile_id == profile_id)
            }
        };
        let policy = crate::preferences::TabPreferences::load().disconnect_policy;
        match policy {
            crate::preferences::DisconnectTabPolicy::CloseTabs => {
                self.close_tabs_where(
                    |_, tab| {
                        crate::tabs::disconnect_tab_action(
                            policy,
                            targets_profile(tab),
                            tab.pinned,
                            tab.dirty,
                        ) != crate::tabs::DisconnectTabAction::Close
                    },
                    cx,
                );
                if self.tabs.is_empty() {
                    self.new_tab(cx, None);
                }
            }
            crate::preferences::DisconnectTabPolicy::KeepTabsClearResults => {
                let victims: Vec<usize> = self
                    .tabs
                    .iter()
                    .enumerate()
                    .filter(|(_, tab)| {
                        crate::tabs::disconnect_tab_action(
                            policy,
                            targets_profile(tab),
                            tab.pinned,
                            tab.dirty,
                        ) == crate::tabs::DisconnectTabAction::ClearResults
                    })
                    .map(|(i, _)| i)
                    .collect();
                for i in victims {
                    self.cancel_tab_query(i, cx);
                    if let Some(tab) = self.tabs.get(i) {
                        tab.results.update(cx, |r, cx| r.clear(cx));
                    }
                }
            }
            crate::preferences::DisconnectTabPolicy::KeepTabsKeepResults => {}
        }
        // Back to the default gray dot: disconnected, not failed. This also
        // drops any in-flight database enumeration for the server, so a
        // stale late success cannot flip the dot back to green.
        let name = ConnectionStore::global()
            .get(profile_id)
            .map(|p| p.name)
            .unwrap_or_else(|| crate::localization::text("shell.connections.server_fallback"));
        self.sidebar.update(cx, |s, cx| {
            s.disconnect_server_node(profile_id, cx);
        });
        self.set_status(
            StatusLevel::Success,
            crate::localization::format("shell.connections.disconnected", &[("name", &name)]),
        );
        cx.notify();
    }

    /// Open a Via-Relay connection: establish the relay session (registering
    /// this device's public Noise key, minting a relay token, pinning the
    /// bridge, and running the Noise handshake) off the UI thread, then finish
    /// the connection through [`Self::mark_connected`]. A failure surfaces
    /// UI-ready copy in the status bar and leaves no half-open session.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn connect_via_relay(&mut self, profile: ConnectionProfile, cx: &mut Context<Self>) {
        self.connect_via_relay_then(profile, cx, |_, _| {});
    }

    /// [`Self::connect_via_relay`] with a completion continuation: `then` runs
    /// only after the relay session is open and [`Self::mark_connected`] has
    /// made the target the active connection. Every failure path skips it.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn connect_via_relay_then(
        &mut self,
        profile: ConnectionProfile,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, &mut Context<Self>) + 'static,
    ) {
        // A relay session already established for this profile: just finish.
        if self.service_router.has_session(&profile.id) {
            self.mark_connected(profile, cx);
            then(self, cx);
            return;
        }
        let Some(session) = crate::store::session::load().filter(|s| !s.is_expired()) else {
            self.set_status(
                StatusLevel::Error,
                crate::localization::text("shell.connections.relay_sign_in_required"),
            );
            cx.notify();
            return;
        };
        let Some(device_id) = session.device_id.clone() else {
            self.set_status(
                StatusLevel::Error,
                crate::localization::text("shell.connections.relay_device_unregistered"),
            );
            cx.notify();
            return;
        };
        let access_token = session.access_token.clone();
        let local_key = match crate::remote_session::app_noise_keypair(&device_id) {
            Ok(key) => key,
            Err(_) => {
                self.set_status(
                    StatusLevel::Error,
                    crate::localization::text("shell.connections.relay_key_failed"),
                );
                cx.notify();
                return;
            }
        };
        self.set_status(
            StatusLevel::Busy,
            crate::localization::format(
                "shell.connections.opening_relay",
                &[("name", &profile.name)],
            ),
        );
        cx.notify();

        let profile_for_task = profile.clone();
        let mut async_cx = cx.to_async();
        let weak_self = cx.weak_entity();
        cx.spawn(async move |_this, _cx| {
            // The relay transport (TLS, Noise, reqwest) needs the tokio runtime;
            // run the whole orchestration there and await the outcome.
            let outcome = crate::tokio_handle()
                .spawn(async move {
                    crate::remote_session::open_via_relay(
                        &profile_for_task,
                        &access_token,
                        &device_id,
                        &local_key,
                    )
                    .await
                })
                .await;
            let result = match outcome {
                Ok(inner) => inner,
                Err(_) => Err(crate::localization::deferred_text(
                    "shell.connections.relay_task_failed",
                )),
            };
            weak_self
                .update(&mut async_cx, |app, cx| match result {
                    Ok(opened) => {
                        app.install_remote_services(
                            profile.id,
                            opened.services,
                            crate::status_bar::RelayRouteFacts {
                                relay_host: opened.relay_hostname,
                                bridge_name: opened.bridge_name,
                            },
                            opened.closed,
                            cx,
                        );
                        app.mark_connected(profile, cx);
                        then(app, cx);
                    }
                    Err(message) => {
                        // A failed relay connect is a failed reach attempt:
                        // mark that server red in the explorer.
                        app.sidebar.update(cx, |s, cx| {
                            s.set_connection_state(
                                profile.id.0,
                                crate::schema_tree::ConnectionState::Unreachable,
                                cx,
                            );
                        });
                        app.set_status(StatusLevel::Error, crate::localization::message(message));
                        cx.notify();
                    }
                })
                .ok();
        })
        .detach();
    }

    /// Switch to the tab's remembered connection by looking it up in the
    /// connection store and connecting. Mirrors the Swift
    /// `activateRememberedConnection()` flow.
    /// Resolve a saved connection profile for a remembered resume connection.
    ///
    /// The remembered metadata is snapshotted from the *live* connection, whose
    /// `database` reflects the database actually in use at run time (e.g. after
    /// a `USE`). The *persisted* profile frequently stores `database: None`
    /// (the database is chosen at connect time), so an exact `server + database`
    /// match misses. We therefore match on `server` and prefer an exact
    /// database match, falling back to any server match — then carry the
    /// remembered database onto the returned profile so the resumed connection
    /// targets the right database.
    pub(super) fn resolve_resume_profile(
        server: &str,
        database: &Option<String>,
    ) -> Option<ConnectionProfile> {
        let store = ConnectionStore::global();
        let candidates: Vec<ConnectionProfile> = store
            .list()
            .into_iter()
            .filter(|p| p.server == *server)
            .collect();
        // Prefer an exact database match; otherwise take the first server match.
        let mut profile = candidates
            .iter()
            .find(|p| p.database == *database)
            .or_else(|| candidates.first())
            .cloned()?;
        // Ensure the resumed connection targets the remembered database.
        if database.is_some() {
            profile.database = database.clone();
        }
        Some(profile)
    }

    pub(super) fn activate_remembered_connection(
        &mut self,
        tab_idx: usize,
        cx: &mut Context<Self>,
    ) {
        let conn = self
            .tabs
            .get(tab_idx)
            .and_then(|t| t.pending_resume_connection.clone());
        let Some(conn) = conn else {
            return;
        };
        // Look up the connection profile by server (database is a soft match).
        let matching = Self::resolve_resume_profile(&conn.server, &conn.database);
        if let Some(profile) = matching {
            let server = conn.server.clone();
            let database = conn.database.clone();
            self.with_entra_sql_token(profile, cx, move |app, profile, cx| {
                app.active_profile = Some(profile.clone());
                app.connect_data_model(profile, cx);
                app.set_status(
                    StatusLevel::Success,
                    crate::localization::format(
                        "shell.connections.resumed",
                        &[
                            ("server", &server),
                            (
                                "database",
                                &database.unwrap_or_else(|| {
                                    crate::localization::text(
                                        "shell.connections.resumed_default_database",
                                    )
                                }),
                            ),
                        ],
                    ),
                );
                cx.notify();
            });
        } else {
            // Connection not found — leave the current connection.
            self.set_status(
                StatusLevel::Warning,
                crate::localization::format(
                    "shell.connections.resume_not_found",
                    &[
                        ("server", &conn.server),
                        (
                            "database",
                            &conn.database.clone().unwrap_or_else(|| {
                                crate::localization::text(
                                    "shell.connections.resume_not_found_default_database",
                                )
                            }),
                        ),
                    ],
                ),
            );
        }
        // Clear the pending confirmation flag.
        if let Some(tab) = self.tabs.get_mut(tab_idx) {
            tab.has_pending_resume_confirmation = false;
        }
        cx.notify();
    }

    /// User confirmed the resume connection switch. Activate the remembered
    /// connection and resume the blocked query execution.
    pub(super) fn confirm_resume_connection(&mut self, cx: &mut Context<Self>) {
        if let Some(pending) = self.pending.resume_confirmation.take() {
            self.activate_remembered_connection(pending.tab_idx, cx);
            // Resume the blocked query execution.
            self.execute_query(pending.tab_idx, cx);
        }
    }

    /// User declined the resume connection switch. Clear the pending
    /// confirmation and proceed with the current connection.
    pub(super) fn cancel_resume_connection(&mut self, cx: &mut Context<Self>) {
        if let Some(pending) = self.pending.resume_confirmation.take() {
            if let Some(tab) = self.tabs.get_mut(pending.tab_idx) {
                tab.has_pending_resume_confirmation = false;
            }
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.connections.resume_declined"),
            );
            cx.notify();
        }
    }

    /// Arm the delete-connection confirmation for `id`. Both the `del` link on
    /// the server row and the context menu's "Delete Connection…" land here;
    /// neither touches the store until [`Self::confirm_delete_connection`].
    /// A profile that no longer exists is a no-op rather than an empty dialog.
    pub(super) fn request_delete_connection(&mut self, id: uuid::Uuid, cx: &mut Context<Self>) {
        let Some(profile) = ConnectionStore::global()
            .list()
            .into_iter()
            .find(|p| p.id.0 == id)
        else {
            self.set_status(
                StatusLevel::Warning,
                crate::localization::text("shell.connections.not_found"),
            );
            cx.notify();
            return;
        };
        self.pending.connection_deletion = Some(PendingConnectionDeletion {
            id,
            name: profile.name.clone(),
            server: profile.server.clone(),
            environment: crate::status_bar::localized_environment_label(profile.environment),
        });
        cx.notify();
    }

    /// User confirmed the deletion: remove the profile and drop it as the
    /// active connection if it was one.
    pub(super) fn confirm_delete_connection(&mut self, cx: &mut Context<Self>) {
        let Some(pending) = self.pending.connection_deletion.take() else {
            return;
        };
        if let Some(profile) = ConnectionStore::global().get(pending.id)
            && let Some(tunnel) = &profile.ssh_tunnel
        {
            let _ = crate::sql_credentials::delete_ssh_secrets(tunnel);
        }
        ConnectionStore::global().remove(pending.id);
        // Same eviction as the management workspace's delete: a deleted
        // profile's relay session must not stay cached.
        self.clear_remote_services(&sqlly_core::ConnectionId(pending.id));
        self.sidebar.update(cx, |s, cx| s.refresh_connections(cx));
        if self.active_profile.as_ref().map(|p| p.id.0) == Some(pending.id) {
            self.active_profile = None;
        }
        self.set_status(
            StatusLevel::Success,
            crate::localization::format(
                "shell.connections.deleted_with_name",
                &[("name", &pending.name)],
            ),
        );
        cx.notify();
    }

    /// User backed out. The profile is untouched.
    pub(super) fn cancel_delete_connection(&mut self, cx: &mut Context<Self>) {
        if self.pending.connection_deletion.take().is_some() {
            cx.notify();
        }
    }

    /// Open the connection editor on the stored profile `id`.
    ///
    /// Reads the profile back from the store rather than editing the live
    /// `active_profile`: that copy carries runtime state (a minted Entra token,
    /// a hydrated password, a database switched from the query bar) that has no
    /// business being written back to disk.
    pub(super) fn open_connection_editor(
        &mut self,
        id: uuid::Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = ConnectionStore::global();
        let Some(profile) = store.get(id) else {
            return;
        };
        let azure = self.azure_discoveries.clone();
        let editor = cx.new(|cx| {
            let mut editor = ConnectionEditor::from_profile(
                self.service_router.clone(),
                self.log.clone(),
                self.entra_accounts_registry.clone(),
                profile,
                window,
                cx,
            );
            editor.set_azure_discoveries(azure, cx);
            editor.set_entra_sql_token_provider(self.entra_sql_token_provider.clone());
            editor
        });
        cx.subscribe(&editor, Self::handle_editor_modal_event)
            .detach();
        self.overlays.connection_editor = Some(editor);
        cx.notify();
    }

    /// Open the connection editor on an independent, unsaved copy of the
    /// stored profile `id` (see [`ConnectionEditor::from_duplicate`]). The
    /// store is untouched until the user saves; Cancel leaves the connection
    /// list exactly as it was.
    pub(super) fn open_duplicate_connection_editor(
        &mut self,
        id: uuid::Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.ensure_room_for_saved_connections(1, cx) {
            return;
        }
        let store = ConnectionStore::global();
        let Some(original) = store.get(id) else {
            self.set_status(
                StatusLevel::Warning,
                crate::localization::text("shell.connections.duplicate_not_found"),
            );
            cx.notify();
            return;
        };
        let existing_names: Vec<String> = store.list().into_iter().map(|p| p.name).collect();
        let azure = self.azure_discoveries.clone();
        let editor = cx.new(|cx| {
            let mut editor = ConnectionEditor::from_duplicate(
                self.service_router.clone(),
                self.log.clone(),
                self.entra_accounts_registry.clone(),
                original,
                existing_names,
                window,
                cx,
            );
            editor.set_azure_discoveries(azure, cx);
            editor.set_entra_sql_token_provider(self.entra_sql_token_provider.clone());
            editor
        });
        cx.subscribe(&editor, Self::handle_editor_modal_event)
            .detach();
        self.overlays.connection_editor = Some(editor);
        cx.notify();
    }
}
