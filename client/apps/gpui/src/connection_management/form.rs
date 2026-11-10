use super::*;

impl ConnectionManagement {
    /// Load a profile into the form fields for editing.
    pub(super) fn load_profile(
        &mut self,
        profile: ConnectionProfile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editing_id = Some(profile.id.0);
        self.duplicate_of = None;
        self.environment = profile.environment;
        self.database_type = profile.database_type;
        self.engine_auto_detect =
            profile.engine_auto_detect && profile.database_type.is_command_engine();
        self.encrypt = profile.encrypt;
        self.trust = profile.trust_server_certificate;
        // The form always holds a resolved mode; a `Legacy` profile shows the
        // mode its booleans have always meant.
        self.tls_mode = profile.effective_tls_mode();
        self.tls_ca_file = profile
            .tls_ca_file
            .as_deref()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        self.tls_client_cert_file = profile
            .tls_client_cert_file
            .as_deref()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        self.tls_client_key_file = profile
            .tls_client_key_file
            .as_deref()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        self.sync_tls_inputs(window, cx);
        self.block_select = profile.block_all_but_select;
        self.rollback = profile.auto_wrap_rollback;
        self.host_validation = if crate::engine_form::is_file_engine(profile.database_type) {
            HostValidationResult::Valid
        } else {
            validate_host(&profile.server)
        };
        self.entra_account = profile.entra_account.clone();
        self.dirty = false;
        self.close_confirm = false;
        self.database_dropdown_open = false;
        self.database_list = None;
        self.database_list_error = None;

        self.auth_mode = match &profile.authentication {
            AuthenticationType::SqlLogin { .. } => AuthMode::SqlLogin,
            AuthenticationType::Integrated => AuthMode::Integrated,
            AuthenticationType::EntraBrowser => AuthMode::EntraBrowser,
            AuthenticationType::EntraDeviceCode => AuthMode::EntraBrowser,
            AuthenticationType::AwsRdsIam { .. } => AuthMode::AwsRdsIam,
            AuthenticationType::GcpCloudSqlIam { .. } => AuthMode::GcpCloudSqlIam,
            AuthenticationType::AuthToken { .. } => AuthMode::AuthToken,
        };
        if self.auth_mode == AuthMode::EntraBrowser && self.entra_account.is_none() {
            self.reload_entra_accounts();
            self.entra_account = sole_entra_binding(&self.entra_accounts);
        }
        self.apply_iam_auth_mode_side_effects(self.auth_mode, window, cx);

        self.name = profile.name;
        self.server = profile.server;
        self.port = profile.port.to_string();
        let engine_label =
            crate::engine_form::picker_label(self.database_type, self.engine_auto_detect);
        self.engine_select.update(cx, |state, cx| {
            state.set_selected_value(&engine_label, window, cx)
        });
        self.database = profile.database.unwrap_or_default();
        self.client = profile.client.unwrap_or_default();
        self.project = profile.project.unwrap_or_default();
        self.timeout = profile.timeout_seconds.to_string();
        self.statement_timeout = profile
            .statement_timeout_seconds
            .map(|s| s.to_string())
            .unwrap_or_default();
        self.startup_sql = profile.startup_sql.clone().unwrap_or_default();
        self.notes = profile.notes.clone().unwrap_or_default();

        match &profile.authentication {
            AuthenticationType::SqlLogin {
                username,
                secret_ref,
            } => {
                self.username = username.clone();
                self.connection_sql_account =
                    if secret_ref.provider == SecretProvider::MacOsKeychain {
                        Uuid::parse_str(&secret_ref.key).ok()
                    } else {
                        None
                    };
                if secret_ref.provider == SecretProvider::InMemory {
                    let key = secret_ref.key.clone();
                    self.password
                        .update(cx, |f, cx| f.set_value(key, window, cx));
                } else if self.connection_sql_account.is_some() {
                    // The real password is in the keychain and never comes back
                    // here; the field shows a fixed-length stand-in so the row
                    // does not read as "no password saved".
                    self.password.update(cx, |f, cx| {
                        f.set_value(crate::connection_editor::password_mask(), window, cx)
                    });
                } else {
                    self.password
                        .update(cx, |f, cx| f.set_value("", window, cx));
                }
            }
            AuthenticationType::AwsRdsIam {
                username,
                region,
                profile: aws_profile,
            } => {
                self.username = username.clone();
                self.aws_region = region.clone();
                self.aws_profile = aws_profile.clone().unwrap_or_default();
                self.password
                    .update(cx, |f, cx| f.set_value("", window, cx));
            }
            AuthenticationType::GcpCloudSqlIam { username } => {
                self.username = username.clone();
                self.password
                    .update(cx, |f, cx| f.set_value("", window, cx));
            }
            // A libSQL token is shown like a password: its value when it is
            // in memory, a stand-in when it lives in the keychain (the entry
            // id is kept so an untouched field re-saves the same reference).
            AuthenticationType::AuthToken { secret_ref } => {
                self.username.clear();
                self.connection_sql_account =
                    if secret_ref.provider == SecretProvider::MacOsKeychain {
                        Uuid::parse_str(&secret_ref.key).ok()
                    } else {
                        None
                    };
                let shown = if secret_ref.provider == SecretProvider::InMemory {
                    secret_ref.key.clone()
                } else if self.connection_sql_account.is_some() {
                    crate::connection_editor::password_mask()
                } else {
                    String::new()
                };
                self.password
                    .update(cx, |f, cx| f.set_value(shown, window, cx));
            }
            _ => {
                self.username.clear();
                self.password
                    .update(cx, |f, cx| f.set_value("", window, cx));
            }
        }

        // Push the loaded values into the input / select states.
        let name = self.name.clone();
        self.name_input
            .update(cx, |s, cx| s.set_value(name, window, cx));
        let server = self.server.clone();
        self.server_input
            .update(cx, |s, cx| s.set_value(server, window, cx));
        let port = self.port.clone();
        self.port_input
            .update(cx, |s, cx| s.set_value(port, window, cx));
        let database = self.database.clone();
        self.database_input
            .update(cx, |s, cx| s.set_value(database, window, cx));
        let username = self.username.clone();
        self.username_input
            .update(cx, |s, cx| s.set_value(username, window, cx));
        let aws_region = self.aws_region.clone();
        self.aws_region_input
            .update(cx, |s, cx| s.set_value(aws_region, window, cx));
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.aws_profile_select_stale = true;
        }
        let timeout = self.timeout.clone();
        self.timeout_input
            .update(cx, |s, cx| s.set_value(timeout, window, cx));
        let statement_timeout = self.statement_timeout.clone();
        self.statement_timeout_input
            .update(cx, |s, cx| s.set_value(statement_timeout, window, cx));
        let startup_sql = self.startup_sql.clone();
        self.startup_sql_input
            .update(cx, |s, cx| s.set_value(startup_sql, window, cx));
        let notes = self.notes.clone();
        self.notes_input
            .update(cx, |s, cx| s.set_value(notes, window, cx));
        self.sync_entity_selects(window, cx);

        self.test_result = None;
        self.status_message = crate::localization::format(
            "connections.management.editing_profile",
            &[("profile_name", &self.name)],
        );
    }

    /// Reset the form to a blank "new connection" state.
    pub(super) fn add_server(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editing_id = None;
        self.duplicate_of = None;
        self.environment = ConnectionEnvironment::Local;
        self.auth_mode = AuthMode::SqlLogin;
        self.encrypt = true;
        self.trust = false;
        self.tls_mode = TlsMode::VerifyFull;
        self.tls_ca_file = String::new();
        self.tls_client_cert_file = String::new();
        self.tls_client_key_file = String::new();
        self.sync_tls_inputs(window, cx);
        self.block_select = false;
        self.rollback = false;
        self.selected = None;
        self.test_result = None;
        self.host_validation = HostValidationResult::Empty;
        self.entra_account = None;
        self.connection_sql_account = None;
        self.dirty = false;
        self.close_confirm = false;
        self.database_dropdown_open = false;
        self.database_list = None;
        self.database_list_error = None;
        self.status_message =
            crate::localization::text("connections.management.add_connection_status");

        self.name.clear();
        self.server.clear();
        self.port = "1433".to_string();
        self.database_type = DatabaseType::SqlServer;
        self.engine_auto_detect = false;
        self.database.clear();
        self.username.clear();
        self.aws_region.clear();
        self.aws_profile.clear();
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.gcp_adc_status.clear();
            self.aws_profile_select_stale = true;
        }
        self.password
            .update(cx, |f, cx| f.set_value("", window, cx));
        self.client.clear();
        self.project.clear();
        self.timeout = "30".to_string();
        self.statement_timeout = String::new();
        self.startup_sql = String::new();
        self.notes = String::new();
        self.name_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.server_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.port_input
            .update(cx, |s, cx| s.set_value("1433", window, cx));
        self.engine_select.update(cx, |state, cx| {
            state.set_selected_value(
                &DatabaseType::SqlServer.display_name().to_string(),
                window,
                cx,
            )
        });
        self.database_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.username_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.aws_region_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.timeout_input
            .update(cx, |s, cx| s.set_value("30", window, cx));
        self.statement_timeout_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.startup_sql_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.notes_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.sync_entity_selects(window, cx);
        cx.notify();
    }

    pub(super) fn set_environment(&mut self, env: ConnectionEnvironment, cx: &mut Context<Self>) {
        self.environment = env;
        self.mark_dirty();
        // Mirror the Swift safety defaults: Production/Staging force the
        // rollback safety net on; Production also forces SELECT-only.
        if matches!(
            env,
            ConnectionEnvironment::Production | ConnectionEnvironment::Staging
        ) {
            self.rollback = true;
        }
        if env == ConnectionEnvironment::Production {
            self.block_select = true;
        }
        if self.test_result.is_some() {
            self.test_result = None;
        }
        cx.notify();
    }

    pub(super) fn set_auth_mode(
        &mut self,
        mode: AuthMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.auth_mode = mode;
        self.mark_dirty();
        self.apply_iam_auth_mode_side_effects(mode, window, cx);
        if mode == AuthMode::EntraBrowser {
            self.reload_entra_accounts();
            if self.entra_accounts.len() == 1 {
                self.entra_account = sole_entra_binding(&self.entra_accounts);
            }
            // Best-effort discovery: populate the picker but don't open the
            // dropdown or gate manual entry on it.
            self.azure_servers_loading = true;
            self.azure_servers_error = None;
            self.azure_server_dropdown_open = false;
            self.azure_server_help_copied = false;
            cx.emit(ConnectionManagementEvent::RefreshAzureServersRequested);
        } else {
            self.azure_server_dropdown_open = false;
        }
        if self.test_result.is_some() {
            self.test_result = None;
        }
        cx.notify();
    }

    /// Build a validation snapshot from the current form state.
    pub(super) fn build_form(&self, cx: &mut Context<Self>) -> ConnectionOptionsForm {
        let name = self.name.trim().to_string();
        let server = self.server.trim().to_string();
        let port: u16 = if crate::engine_form::is_file_engine(self.database_type) {
            1
        } else {
            self.port.trim().parse().unwrap_or(0)
        };
        let database = {
            let d = self.database.trim().to_string();
            if d.is_empty() { None } else { Some(d) }
        };
        let client = {
            let c = self.client.trim().to_string();
            if c.is_empty() { None } else { Some(c) }
        };
        let project = {
            let p = self.project.trim().to_string();
            if p.is_empty() { None } else { Some(p) }
        };
        let timeout: u64 = self.timeout.trim().parse().unwrap_or(0);
        let username = self.username.trim().to_string();
        let password = self.password.read(cx).value().to_string();

        let authentication = match self.auth_mode {
            AuthMode::SqlLogin => AuthenticationType::SqlLogin {
                username,
                secret_ref: SecretRef {
                    provider: if self.connection_sql_account.is_some() {
                        SecretProvider::MacOsKeychain
                    } else {
                        SecretProvider::InMemory
                    },
                    key: self
                        .connection_sql_account
                        .map(|id| id.to_string())
                        .unwrap_or(password),
                },
            },
            AuthMode::Integrated => AuthenticationType::Integrated,
            AuthMode::EntraBrowser => AuthenticationType::EntraBrowser,
            AuthMode::AwsRdsIam => AuthenticationType::AwsRdsIam {
                username,
                region: self.aws_region.trim().to_string(),
                profile: {
                    let profile = self.aws_profile.trim();
                    if profile.is_empty() {
                        None
                    } else {
                        Some(profile.to_string())
                    }
                },
            },
            AuthMode::GcpCloudSqlIam => AuthenticationType::GcpCloudSqlIam { username },
            AuthMode::AuthToken => AuthenticationType::AuthToken {
                secret_ref: SecretRef {
                    provider: SecretProvider::InMemory,
                    key: password.trim().to_string(),
                },
            },
        };

        let mode = if self.editing_id.is_some() {
            ConnectionOptionsFormMode::EditSaved
        } else {
            ConnectionOptionsFormMode::CreateManual
        };

        ConnectionOptionsForm {
            mode,
            name,
            server,
            port,
            database,
            environment: self.environment,
            client,
            project,
            authentication,
            encrypt: self.encrypt,
            trust_server_certificate: self.trust,
            timeout_seconds: timeout,
            application_name: "SQLLY".to_string(),
            auto_wrap_rollback: self.rollback,
            block_all_but_select: self.block_select,
            validation_errors: Vec::new(),
        }
    }

    pub(super) fn validation_errors(&self, cx: &mut Context<Self>) -> Vec<String> {
        self.build_form(cx)
            .validate()
            .into_iter()
            .map(|error| {
                crate::localization::text(match error.field.as_str() {
                    "name" => "connections.management.validation_name_required",
                    "server" => "connections.management.validation_server_required",
                    "port" => "connections.management.validation_port_required",
                    "timeout_seconds" => "connections.management.validation_timeout_required",
                    _ => return error.message,
                })
            })
            .collect()
    }

    /// Build a full connection profile from the form. Caller should ensure
    /// validation has passed first.
    pub(super) fn build_profile(&self, cx: &mut Context<Self>) -> ConnectionProfile {
        let name = self.name.trim().to_string();
        let server = self.server.trim().to_string();
        let port: u16 = if crate::engine_form::is_file_engine(self.database_type) {
            0
        } else {
            self.port.trim().parse().unwrap_or(0)
        };
        let database = {
            let d = self.database.trim().to_string();
            if d.is_empty() { None } else { Some(d) }
        };
        let client = {
            let c = self.client.trim().to_string();
            if c.is_empty() { None } else { Some(c) }
        };
        let project = {
            let p = self.project.trim().to_string();
            if p.is_empty() { None } else { Some(p) }
        };
        let timeout: u64 = self.timeout.trim().parse().unwrap_or(30);
        let username = self.username.trim().to_string();
        let password = self.password.read(cx).value().to_string();

        let mut profile = ConnectionStore::create_sql_login(
            name,
            server,
            port,
            database,
            username,
            password,
            self.encrypt,
            self.trust,
        );
        if let (AuthMode::SqlLogin, Some(account_id)) =
            (self.auth_mode, self.connection_sql_account)
            && let AuthenticationType::SqlLogin { secret_ref, .. } = &mut profile.authentication
        {
            secret_ref.provider = SecretProvider::MacOsKeychain;
            secret_ref.key = account_id.to_string();
        }
        profile.environment = self.environment;
        profile.database_type = self.database_type;
        profile.engine_auto_detect =
            self.engine_auto_detect && self.database_type.is_command_engine();
        profile.client = client;
        profile.project = project;
        profile.timeout_seconds = if timeout == 0 { 30 } else { timeout };
        profile.statement_timeout_seconds = self
            .statement_timeout
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|s| *s > 0);
        profile.startup_sql = {
            let text = self.startup_sql.trim();
            if text.is_empty() {
                None
            } else {
                Some(text.to_string())
            }
        };
        profile.notes = {
            let text = self.notes.trim();
            (!text.is_empty()).then(|| text.to_string())
        };
        profile.block_all_but_select = self.block_select;
        profile.auto_wrap_rollback = self.rollback;

        if crate::engine_form::uses_tls_modes(self.database_type) {
            profile.tls_mode = self.tls_mode;
            // Keep the two legacy booleans in sync so builds that predate
            // `tls_mode` read a safe equivalent of the selected mode.
            let (encrypt, trust) = self.tls_mode.legacy_flags();
            profile.encrypt = encrypt;
            profile.trust_server_certificate = trust;
        }
        let tls_path = |s: &str| {
            let t = s.trim();
            if t.is_empty() {
                None
            } else {
                Some(std::path::PathBuf::from(t))
            }
        };
        profile.tls_ca_file = tls_path(&self.tls_ca_file);
        profile.tls_client_cert_file = tls_path(&self.tls_client_cert_file);
        profile.tls_client_key_file = tls_path(&self.tls_client_key_file);

        match self.auth_mode {
            AuthMode::SqlLogin => {}
            AuthMode::Integrated => profile.authentication = AuthenticationType::Integrated,
            AuthMode::EntraBrowser => profile.authentication = AuthenticationType::EntraBrowser,
            AuthMode::AwsRdsIam => {
                profile.authentication = AuthenticationType::AwsRdsIam {
                    username: self.username.trim().to_string(),
                    region: self.aws_region.trim().to_string(),
                    profile: {
                        let aws_profile = self.aws_profile.trim();
                        if aws_profile.is_empty() {
                            None
                        } else {
                            Some(aws_profile.to_string())
                        }
                    },
                };
                profile.encrypt = true;
            }
            AuthMode::GcpCloudSqlIam => {
                profile.authentication = AuthenticationType::GcpCloudSqlIam {
                    username: self.username.trim().to_string(),
                };
                profile.encrypt = true;
            }
            // An untouched stand-in keeps the saved keyring entry; a typed
            // token is in memory here and the store moves it into the
            // keyring on save exactly like a typed SQL-login password.
            AuthMode::AuthToken => {
                let typed = self.password.read(cx).value().trim().to_string();
                let secret_ref = match self.connection_sql_account {
                    Some(id) if typed == crate::connection_editor::password_mask() => SecretRef {
                        provider: SecretProvider::MacOsKeychain,
                        key: id.to_string(),
                    },
                    _ => SecretRef {
                        provider: SecretProvider::InMemory,
                        key: typed,
                    },
                };
                profile.authentication = AuthenticationType::AuthToken { secret_ref };
            }
        }

        // Preserve the Entra account binding for Entra auth modes so editing
        // a bound profile here does not silently drop the binding.
        if matches!(self.auth_mode, AuthMode::EntraBrowser) {
            profile.entra_account = self.entra_account.clone();
        }

        if let Some(id) = self.editing_id {
            profile.id.0 = id;
            // This inline form does not expose routing, the confirmation
            // settings, engine type, or provenance/status, so carry the saved
            // values forward rather than silently resetting them on edit (a
            // ViaRelay profile must stay ViaRelay; a profile with
            // "confirm data changes" on must keep confirming). The modal
            // ConnectionEditor owns the full set of controls.
            if let Some(existing) = ConnectionStore::global().get(id) {
                carry_unexposed_fields(&mut profile, &existing);
            }
        } else if let Some(pending) = &self.duplicate_of {
            // A duplicate keeps the copy's own fresh id and the settings this
            // form has no control for — taken from the copy, whose tunnel and
            // proxy secrets already point at re-minted keyring entries, never
            // from the source profile.
            profile.id = pending.copy.profile.id;
            carry_unexposed_fields(&mut profile, &pending.copy.profile);
        }
        profile
    }

    /// Push the canonical TLS values into the select and path inputs.
    pub(super) fn sync_tls_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let label = crate::connection_editor::tls_mode_label(self.tls_mode).to_string();
        self.tls_mode_select.update(cx, |state, cx| {
            state.set_selected_value(&label, window, cx);
        });
        let values = [
            (self.tls_ca_input.clone(), self.tls_ca_file.clone()),
            (
                self.tls_cert_input.clone(),
                self.tls_client_cert_file.clone(),
            ),
            (self.tls_key_input.clone(), self.tls_client_key_file.clone()),
        ];
        for (input, value) in values {
            let w = &mut *window;
            input.update(cx, move |s, cx| s.set_value(value, w, cx));
        }
    }

    /// Switch the form to `engine`: port, TLS and sign-in defaults follow.
    /// An Auto-detect switch onto a managed AWS cache also turns TLS on
    /// (MemoryDB and IAM sign-in need it).
    pub(super) fn apply_engine(
        &mut self,
        engine: DatabaseType,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let old = self.database_type;
        self.database_type = engine;
        self.encrypt = crate::engine_form::encrypt_after_engine_change(self.encrypt, engine);
        let port = crate::engine_form::port_after_engine_change(&self.port, old, engine);
        if port != self.port {
            self.port = port.clone();
            self.port_input
                .update(cx, |state, cx| state.set_value(port, window, cx));
        }
        if crate::engine_form::is_file_engine(engine) {
            self.auth_mode = AuthMode::SqlLogin;
            self.host_validation = HostValidationResult::Valid;
        } else {
            // Moving within the Redis family (Auto-detect following the
            // host) keeps a sign-in the new flavor still accepts, e.g. IAM.
            let same_family_keeps_auth = old.is_command_engine()
                && engine.is_command_engine()
                && self.auth_mode.supported_for(engine);
            if !same_family_keeps_auth
                && (!matches!(
                    engine,
                    DatabaseType::SqlServer | DatabaseType::AzureSqlDatabase
                ) || (engine == DatabaseType::AzureSqlDatabase
                    && self.auth_mode == AuthMode::Integrated))
            {
                self.auth_mode = AuthMode::default_for(engine);
            }
            self.host_validation = validate_host(&self.server);
        }
        if self.engine_auto_detect {
            let mode = crate::engine_form::tls_after_auto_detect(self.tls_mode, old, engine);
            if mode != self.tls_mode {
                self.set_tls_mode(mode, cx);
                self.sync_tls_inputs(window, cx);
            }
        }
        self.test_result = None;
        self.mark_dirty();
        cx.notify();
    }

    /// Writes the selected TLS mode and keeps the two legacy booleans in
    /// sync so builds that predate `tls_mode` read a safe equivalent.
    pub(super) fn set_tls_mode(&mut self, mode: TlsMode, cx: &mut Context<Self>) {
        if self.tls_mode != mode {
            self.mark_dirty();
        }
        self.tls_mode = mode;
        let (encrypt, trust) = mode.legacy_flags();
        self.encrypt = encrypt;
        self.trust = trust;
        if self.test_result.is_some() {
            self.test_result = None;
        }
        cx.notify();
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn handle_browse_tls_file(
        &mut self,
        field: super::TlsPathField,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let file = rfd::FileDialog::new()
            .add_filter(
                crate::localization::text("connections.management.certificate_file_filter"),
                &["pem", "crt", "cer", "key"],
            )
            .set_title(crate::localization::text(
                "connections.management.select_certificate_title",
            ))
            .pick_file();
        if let Some(path) = file {
            crate::sandbox_access::remember(&path);
            let value = path.to_string_lossy().to_string();
            let input = match field {
                super::TlsPathField::Ca => {
                    self.tls_ca_file = value.clone();
                    &self.tls_ca_input
                }
                super::TlsPathField::ClientCert => {
                    self.tls_client_cert_file = value.clone();
                    &self.tls_cert_input
                }
                super::TlsPathField::ClientKey => {
                    self.tls_client_key_file = value.clone();
                    &self.tls_key_input
                }
            };
            input.update(cx, |state, cx| {
                state.set_value(value, window, cx);
            });
            self.mark_dirty();
            cx.notify();
        }
    }

    pub(super) fn toggle_encrypt(&mut self, checked: bool, _: &mut Window, cx: &mut Context<Self>) {
        self.encrypt = checked;
        self.mark_dirty();
        if self.test_result.is_some() {
            self.test_result = None;
        }
        cx.notify();
    }

    pub(super) fn toggle_trust(&mut self, checked: bool, _: &mut Window, cx: &mut Context<Self>) {
        self.trust = checked;
        self.mark_dirty();
        if self.test_result.is_some() {
            self.test_result = None;
        }
        cx.notify();
    }

    pub(super) fn toggle_block_select(
        &mut self,
        checked: bool,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.block_select = checked;
        self.mark_dirty();
        cx.notify();
    }

    pub(super) fn toggle_rollback(
        &mut self,
        checked: bool,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.rollback = checked;
        self.mark_dirty();
        cx.notify();
    }

    pub(super) fn apply_pasted_url(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let raw = self.url_paste_input.read(cx).value().to_string();
        match sqlly_core::parse_connection_url(&raw) {
            Err(error) => {
                let error = crate::localization::message(error.message());
                self.status_message = crate::localization::format(
                    "connections.management.connection_string_read_failed",
                    &[("error", error.as_str())],
                );
            }
            Ok(parsed) => {
                if !crate::engine_form::SELECTABLE_ENGINES.contains(&parsed.engine) {
                    self.status_message = crate::localization::format(
                        "connections.management.unsupported_engine",
                        &[("engine_name", parsed.engine.display_name())],
                    );
                    cx.notify();
                    return;
                }
                // A generic `redis://` paste keeps Auto-detect; a flavored
                // scheme (`valkey://`) or any other engine is an explicit choice.
                self.engine_auto_detect &= parsed.engine == DatabaseType::Redis;
                self.database_type = if self.engine_auto_detect {
                    crate::engine_form::auto_detect_engine(&parsed.server, self.database_type)
                } else {
                    parsed.engine
                };
                let engine_label =
                    crate::engine_form::picker_label(self.database_type, self.engine_auto_detect);
                self.engine_select.update(cx, |state, cx| {
                    state.set_selected_value(&engine_label, window, cx)
                });
                self.server = parsed.server.clone();
                self.server_input
                    .update(cx, |s, cx| s.set_value(parsed.server.clone(), window, cx));
                self.port = if crate::engine_form::is_file_engine(parsed.engine) {
                    String::new()
                } else {
                    parsed.port.map(|p| p.to_string()).unwrap_or_else(|| {
                        crate::engine_form::default_port(parsed.engine).to_string()
                    })
                };
                self.port_input
                    .update(cx, |s, cx| s.set_value(self.port.clone(), window, cx));
                self.database = parsed.database.clone().unwrap_or_default();
                self.database_input
                    .update(cx, |s, cx| s.set_value(self.database.clone(), window, cx));
                if let Some(username) = &parsed.username {
                    self.username = username.clone();
                    self.username_input
                        .update(cx, |s, cx| s.set_value(username.clone(), window, cx));
                }
                let (mode, encrypt, trust) = crate::engine_form::tls_after_paste(
                    (self.tls_mode, self.encrypt, self.trust),
                    &parsed,
                );
                if self.tls_mode != mode {
                    self.mark_dirty();
                }
                self.tls_mode = mode;
                self.encrypt = encrypt;
                self.trust = trust;
                if let Some(path) = &parsed.tls_ca_file {
                    self.tls_ca_file = path.to_string_lossy().to_string();
                }
                if let Some(path) = &parsed.tls_client_cert_file {
                    self.tls_client_cert_file = path.to_string_lossy().to_string();
                }
                if let Some(path) = &parsed.tls_client_key_file {
                    self.tls_client_key_file = path.to_string_lossy().to_string();
                }
                self.sync_tls_inputs(window, cx);
                if parsed.integrated_security
                    && matches!(
                        parsed.engine,
                        DatabaseType::SqlServer | DatabaseType::AzureSqlDatabase
                    )
                {
                    self.set_auth_mode(AuthMode::Integrated, window, cx);
                } else if crate::engine_form::uses_auth_token(parsed.engine) {
                    self.set_auth_mode(AuthMode::AuthToken, window, cx);
                } else if parsed.username.is_some() || parsed.password.is_some() {
                    self.set_auth_mode(AuthMode::SqlLogin, window, cx);
                }
                if self.name.trim().is_empty() {
                    self.name = crate::engine_form::derived_connection_name(&parsed);
                    self.name_input
                        .update(cx, |s, cx| s.set_value(self.name.clone(), window, cx));
                }
                if let Some(password) = parsed.password {
                    self.password
                        .update(cx, |f, cx| f.set_value(password, window, cx));
                    self.connection_sql_account = None;
                }
                self.host_validation = if crate::engine_form::is_file_engine(parsed.engine) {
                    if self.server.trim().is_empty() {
                        HostValidationResult::Empty
                    } else {
                        HostValidationResult::Valid
                    }
                } else {
                    validate_host(&self.server)
                };
                self.url_paste_input
                    .update(cx, |s, cx| s.set_value("", window, cx));
                self.url_paste_open = false;
                self.mark_dirty();
                let ignored = parsed.extra_options.len();
                self.status_message = if ignored == 0 {
                    crate::localization::text("connections.management.connection_string_filled")
                } else {
                    crate::localization::format(
                        "connections.management.connection_string_filled_ignored",
                        &[("ignored_count", &ignored.to_string())],
                    )
                };
            }
        }
        cx.notify();
    }

    /// Mark the connection form as edited (enables the discard-on-close
    /// guard) and clear a pending close confirmation.
    pub(super) fn mark_dirty(&mut self) {
        self.dirty = true;
        self.close_confirm = false;
    }
}

/// Copy onto `profile` every setting the management form has no control
/// for, so saving from this form never silently resets what the modal
/// connection editor wrote: routing, the confirmation and production
/// settings, provenance/status/favorite, the transport chain (SSH, proxy,
/// Kubernetes, named tunnel), masking rules and the application name.
pub(super) fn carry_unexposed_fields(profile: &mut ConnectionProfile, source: &ConnectionProfile) {
    profile.route = source.route.clone();
    profile.confirm_data_changes = source.confirm_data_changes;
    profile.allow_unbounded_mutations = source.allow_unbounded_mutations;
    profile.production_override = source.production_override;
    profile.rollback_policy_defaulted = source.rollback_policy_defaulted;
    profile.provenance = source.provenance;
    profile.status = source.status.clone();
    profile.favorite = source.favorite;
    profile.application_name = source.application_name.clone();
    profile.ssh_tunnel = source.ssh_tunnel.clone();
    profile.tunnel_profile = source.tunnel_profile;
    profile.transport = source.transport.clone();
    profile.masking = source.masking.clone();
    profile.redis_db_aliases = source.redis_db_aliases.clone();
    profile.redis_visible_databases = source.redis_visible_databases.clone();
    profile.hidden_schemas = source.hidden_schemas.clone();
    profile.display_time_zone = source.display_time_zone.clone();
    profile.redis_entra_user = source.redis_entra_user.clone();
}
