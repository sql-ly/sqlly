use super::*;

impl ConnectionManagement {
    /// Expand the section roots so the tree is usable on first open.
    pub(super) fn expand_defaults(&mut self) {
        for root in self.tree.children_of(None) {
            self.expanded.insert(root.id.0.clone());
            for child in self.tree.children_of(Some(&root.id)) {
                self.expanded.insert(child.id.0.clone());
            }
        }
    }

    /// Rebuild the management tree from the persisted connection store. Each
    /// saved profile becomes a registered server keyed by its stable profile
    /// id so the tree can route a selection back to the profile.
    pub(super) fn build_tree() -> ConnectionManagementTree {
        Self::build_tree_from(ConnectionStore::global())
    }

    /// Rebuild the management tree, merging saved profiles with Azure
    /// discoveries.
    pub(super) fn build_tree_with_azure(
        discoveries: &[AzureSqlDiscovery],
    ) -> ConnectionManagementTree {
        let mut tree = Self::build_tree_from(ConnectionStore::global());
        if !discoveries.is_empty() {
            let azure_tree = ConnectionManagementTree::from_azure_discoveries(discoveries);
            tree.nodes.extend(azure_tree.nodes);
        }
        localize_tree_labels(&mut tree);
        tree
    }

    /// Build the management tree from a given connection store. Separated from
    /// [`build_tree`](Self::build_tree) so the grouping logic is testable
    /// against a temp-file-backed store without touching the real config dir.
    pub(super) fn build_tree_from(store: &ConnectionStore) -> ConnectionManagementTree {
        let profiles = store.list();
        let mut registered = RegisteredServerStore::new();
        for profile in &profiles {
            let id = profile.id.0;
            let mut server = RegisteredServer::new(profile.name.clone(), profile.clone());
            server.id = id;
            let _ = registered.add_server(server);
        }
        let mut tree = ConnectionManagementTree::from_registered_servers(&registered);
        tag_relay_routes(&mut tree, &profiles);
        tag_connection_health(&mut tree, &profiles);
        localize_tree_labels(&mut tree);
        tree
    }

    pub(super) fn refresh_tree(&mut self, cx: &mut Context<Self>) {
        self.tree = Self::build_tree_with_azure(&self.azure_discoveries);
        cx.notify();
    }

    fn select_node(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = Some(id.clone());

        // Find the clicked node. If it is an Azure database, emit a link event
        // instead of trying to load a saved profile.
        let node = self.tree.nodes.iter().find(|n| n.id.0 == id);
        if let Some(n) = node
            && matches!(
                n.kind,
                sqlly_core::ConnectionManagementNodeKind::AzureDatabase
            )
            && let Some(database_id) = &n.resource_id
            && let Some(d) = self
                .azure_discoveries
                .iter()
                .find(|d| &d.database_id == database_id)
        {
            cx.emit(ConnectionManagementEvent::LinkAzureDatabase(d.clone()));
            return;
        }

        let profile = node
            .and_then(|n| n.profile_id)
            .and_then(|pid| ConnectionStore::global().get(pid));
        if let Some(profile) = profile {
            self.load_profile(profile, window, cx);
        } else {
            cx.notify();
        }
    }

    fn toggle_expand(&mut self, id: String, cx: &mut Context<Self>) {
        if self.expanded.remove(&id) {
            // was present → now collapsed
        } else {
            self.expanded.insert(id);
        }
        cx.notify();
    }

    // ----- workspace panes -----

    fn render_row(&mut self, plan: RowPlan, idx: usize, cx: &mut Context<Self>) -> AnyElement {
        let s = Theme::scaled;
        let indent = plan.depth as f32 * s(TREE_LEVEL_INDENT);
        let is_selected = self.selected.as_deref() == Some(plan.id.as_str());
        let id = plan.id.clone();
        let has_children = plan.has_children;
        let is_expanded = self.expanded.contains(&plan.id);
        let arrow = has_children.then(|| crate::icons::disclosure_chevron(is_expanded));
        let label = plan.label.clone();
        let badges = plan.badges.clone();

        div()
            .id(("cm-row", idx))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(s(4.0)))
            .h(px(s(22.0)))
            .pl(px(indent))
            .pr(px(s(8.0)))
            .cursor(gpui::CursorStyle::PointingHand)
            .bg(if is_selected {
                Theme::accent().opacity(0.3)
            } else {
                Theme::sidebar_bg()
            })
            .hover(|st| st.bg(Theme::border()))
            .text_size(px(Theme::font_size()))
            .text_color(if is_selected {
                Theme::text()
            } else {
                Theme::text_dim()
            })
            .child(
                div()
                    .w(px(s(17.5)))
                    .flex()
                    .items_center()
                    .justify_center()
                    .children(arrow),
            )
            .child(div().flex_1().child(label))
            .children(badges.iter().map(|b| {
                div()
                    .text_size(px(Theme::font_size_ratio(0.67)))
                    .text_color(match b.as_str() {
                        "healthy" => Theme::success(),
                        "failed" => Theme::error(),
                        _ => Theme::text_dim(),
                    })
                    .px(px(s(4.0)))
                    .border_1()
                    .border_color(Theme::border())
                    .rounded(px(s(2.0)))
                    .child(match b.as_str() {
                        "healthy" => {
                            crate::localization::text("connections.management.healthy_badge")
                        }
                        "failed" => {
                            crate::localization::text("connections.management.failed_badge")
                        }
                        "untested" => {
                            crate::localization::text("connections.management.untested_badge")
                        }
                        "via relay" => {
                            crate::localization::text("connections.management.relay_badge")
                        }
                        "SELECT only" => {
                            crate::localization::text("connections.management.select_only_badge")
                        }
                        "Rollback wrap" => {
                            crate::localization::text("connections.management.rollback_badge")
                        }
                        "Linked" => {
                            crate::localization::text("connections.management.linked_badge")
                        }
                        "Discovered" => {
                            crate::localization::text("connections.management.discovered_badge")
                        }
                        _ => b.clone(),
                    })
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    if has_children {
                        this.toggle_expand(id.clone(), cx);
                    }
                    this.select_node(id.clone(), window, cx);
                }),
            )
            .into_any_element()
    }

    /// The Connections pane: server list + split divider + grouped-card
    /// editor. This is the former single-purpose dialog body, restyled into
    /// System Settings-style inset groups.
    pub(super) fn render_connections_pane(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // A relay proxy's listing is read-only: the footer's add/remove/
        // duplicate all disable, matching every other entry point.
        let proxied = crate::connection::connections_are_proxied();
        #[cfg(not(target_arch = "wasm32"))]
        if self.aws_profile_select_stale {
            self.sync_aws_profile_select(window, cx);
        }
        let s = Theme::scaled;

        // ----- Build the tree render plan under an immutable borrow -----
        let parent_ids: HashSet<String> = self
            .tree
            .nodes
            .iter()
            .filter_map(|n| n.parent.as_ref().map(|p| p.0.clone()))
            .collect();
        let query = self.search.trim().to_lowercase();
        let mut plans: Vec<RowPlan> = Vec::new();
        walk(&self.tree, &self.expanded, None, 0, &parent_ids, &mut plans);
        let searching = !query.is_empty();
        if searching {
            plans.retain(|p| p.label.to_lowercase().contains(&query));
        }
        let tree_content_width = connection_tree_content_width(&plans);

        // ----- Snapshot form state for rendering -----
        let environment = self.environment;
        let auth_mode = self.auth_mode;
        let file_engine = crate::engine_form::is_file_engine(self.database_type);
        let iam_tls = matches!(auth_mode, AuthMode::AwsRdsIam | AuthMode::GcpCloudSqlIam);
        let tls_required = self.database_type.requires_tls() || iam_tls;
        let encrypt = self.encrypt || tls_required;
        let trust = self.trust;
        let block_select = self.block_select;
        let rollback = self.rollback;
        let editing_id = self.editing_id;
        let test_result = self.test_result.clone();
        let test_success = self.test_success;
        let status_message = self.status_message.clone();
        let health_message = editing_id
            .and_then(|id| ConnectionStore::global().get(id))
            .map(|profile| connection_health_description(&profile, chrono::Utc::now()));
        let errors = self.validation_errors(cx);
        let can_save = errors.is_empty();

        let search = Input::new(&self.search_input).cleanable(true);
        let name = Input::new(&self.name_input);
        let server = Input::new(&self.server_input);
        let port = Input::new(&self.port_input).disabled(file_engine);
        let engine = Select::new(&self.engine_select);
        let database = Input::new(&self.database_input);
        // A saved account owns both credentials: the username is the account's
        // and the password lives in the keychain, so neither field is the
        // user's to edit here. Clicking the chosen account again releases them.
        let account_locked = self.connection_sql_account.is_some();
        let username = Input::new(&self.username_input).disabled(account_locked);
        let aws_region = Input::new(&self.aws_region_input);
        #[cfg(not(target_arch = "wasm32"))]
        let aws_profile = Select::new(&self.aws_profile_select);
        let password = Input::new(&self.password)
            .mask_toggle()
            .disabled(account_locked);
        let statement_timeout = Input::new(&self.statement_timeout_input);
        let startup_sql =
            Textarea::new(&self.startup_sql_input).font_family(Theme::editor_font_family());
        let notes = Textarea::new(&self.notes_input);
        let timeout = Input::new(&self.timeout_input);
        let client_select =
            Self::field_control(Select::new(&self.client_select).placeholder(none_choice()));
        let project_select =
            Self::field_control(Select::new(&self.project_select).placeholder(none_choice()));

        // Build the tree rows (mutable borrow needed for cx.listener).
        let rows: Vec<AnyElement> = plans
            .into_iter()
            .enumerate()
            .map(|(i, p)| self.render_row(p, i, cx))
            .collect();

        // ----- Left pane: search + tree + add/remove footer -----
        let left_pane = div()
            .w(px(s(self.split_left_width)))
            .h_full()
            .min_h_0()
            .flex()
            .flex_col()
            .bg(Theme::sidebar_bg())
            .border_r_0()
            .child(div().p(px(s(8.0))).child(search))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_row()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .min_h_0()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .id("cm_tree")
                                    .flex_1()
                                    .min_h_0()
                                    .overflow_scroll()
                                    .track_scroll(&self.connection_tree_scroll)
                                    .on_scroll(cx.listener(|_, _, _, cx| cx.notify()))
                                    .child(if rows.is_empty() {
                                        div()
                                            .p(px(s(12.0)))
                                            .text_size(px(Theme::font_size()))
                                            .text_color(Theme::text_dim())
                                            .child(crate::localization::text(if searching {
                                                "connections.management.no_filtered_connections"
                                            } else {
                                                "connections.management.no_connections"
                                            }))
                                            .into_any_element()
                                    } else {
                                        div()
                                            .min_w(px(s(tree_content_width)))
                                            .flex()
                                            .flex_col()
                                            .children(rows)
                                            .into_any_element()
                                    }),
                            )
                            .child(Self::horizontal_scrollbar(
                                "cm-tree-horizontal-scrollbar",
                                &self.connection_tree_scroll,
                            )),
                    )
                    .child(Self::vertical_scrollbar(
                        "cm-tree-vertical-scrollbar",
                        &self.connection_tree_scroll,
                    )),
            )
            .child(
                // Swift-parity +/− footer: + resets the form for a new
                // connection, − deletes the selected saved connection.
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(4.0)))
                    .p(px(s(6.0)))
                    .border_t_1()
                    .border_color(Theme::border())
                    .child(Self::list_footer_button(
                        "cm-list-add",
                        "+",
                        !proxied,
                        cx,
                        Self::handle_add_server,
                    ))
                    .child(Self::list_footer_button(
                        "cm-list-remove",
                        "\u{2212}",
                        editing_id.is_some() && !proxied,
                        cx,
                        Self::handle_delete,
                    ))
                    // ⧉ loads an unsaved copy of the selected connection into
                    // the form; it follows the same gating as −, since only a
                    // saved connection can be duplicated.
                    .child(Self::list_footer_button(
                        "cm-list-duplicate",
                        "\u{29C9}",
                        editing_id.is_some() && !proxied,
                        cx,
                        Self::handle_duplicate,
                    )),
            );

        // ----- Split divider -----
        let split_divider = div()
            .id("cm-split-handle")
            .w(px(s(6.0)))
            .h_full()
            .flex()
            .flex_none()
            .cursor(gpui::CursorStyle::ResizeLeftRight)
            .bg(Theme::border())
            .hover(|st| st.bg(Theme::text_dim()))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(Self::handle_split_resize_start),
            );

        // ----- Right pane: grouped-card form (System Settings style) -----
        let env_segmented = Self::render_segmented(
            "env",
            &[
                ("PROD", ConnectionEnvironment::Production),
                ("STAGE", ConnectionEnvironment::Staging),
                ("TEST", ConnectionEnvironment::Test),
                ("DEV", ConnectionEnvironment::Dev),
                ("LOCAL", ConnectionEnvironment::Local),
            ],
            environment,
            cx,
        );
        let auth_segmented = Self::render_auth_segmented(cx, auth_mode, self.database_type);

        let host_error = match self.host_validation {
            HostValidationResult::Valid | HostValidationResult::Empty => None,
            HostValidationResult::InvalidHost => Some(crate::localization::text(
                "connections.management.invalid_host",
            )),
            HostValidationResult::InvalidInstance => Some(crate::localization::text(
                "connections.management.invalid_instance",
            )),
        };

        let mut auth_rows: Vec<AnyElement> = if file_engine {
            vec![Self::card_row_stacked(
                &crate::localization::text("connections.management.credentials_section"),
                div()
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text_dim())
                    .child(crate::engine_form::file_engine_credentials_note(
                        self.database_type,
                    ))
                    .into_any_element(),
            )]
        } else {
            vec![Self::card_row_stacked(
                &crate::localization::text("connections.management.auth_mode_label"),
                auth_segmented,
            )]
        };
        if !file_engine && auth_mode == AuthMode::SqlLogin {
            auth_rows.push(self.render_sql_account_choices(cx));
            auth_rows.push(Self::card_row(
                &crate::localization::text("connections.management.username_label"),
                Self::field_control(username),
            ));
            auth_rows.push(Self::card_row(
                &crate::localization::text("connections.management.password_label"),
                Self::field_control(password),
            ));
        } else if !file_engine && auth_mode == AuthMode::AuthToken {
            auth_rows.push(Self::card_row(
                &crate::localization::text("connections.management.auth_token_label"),
                Self::field_control(password),
            ));
            auth_rows.push(
                div()
                    .px(px(s(12.0)))
                    .py(px(s(8.0)))
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text(
                        "connections.management.auth_token_hint",
                    ))
                    .into_any_element(),
            );
        } else if !file_engine && auth_mode == AuthMode::Integrated {
            auth_rows.push(
                div()
                    .px(px(s(12.0)))
                    .py(px(s(8.0)))
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text(
                        "connections.management.windows_credentials_hint",
                    ))
                    .into_any_element(),
            );
        } else if !file_engine && auth_mode == AuthMode::EntraBrowser {
            auth_rows.push(
                div()
                    .px(px(s(12.0)))
                    .py(px(s(8.0)))
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text(
                        "connections.management.entra_credentials_hint",
                    ))
                    .into_any_element(),
            );
            auth_rows.push(self.render_entra_binding_rows(cx));
        } else if !file_engine && auth_mode == AuthMode::AwsRdsIam {
            auth_rows.push(Self::card_row(
                &crate::localization::text("connections.management.database_user_label"),
                Self::field_control(username),
            ));
            auth_rows.push(Self::card_row(
                &crate::localization::text("connections.management.region_label"),
                Self::field_control(aws_region),
            ));
            #[cfg(not(target_arch = "wasm32"))]
            auth_rows.push(Self::card_row(
                &crate::localization::text("connections.management.aws_profile_label"),
                Self::field_control(aws_profile),
            ));
            auth_rows.push(self.render_aws_iam_hint());
        } else if !file_engine && auth_mode == AuthMode::GcpCloudSqlIam {
            auth_rows.push(Self::card_row(
                &crate::localization::text("connections.management.iam_user_label"),
                Self::field_control(username),
            ));
            #[cfg(not(target_arch = "wasm32"))]
            auth_rows.push(self.render_gcp_adc_status_row(cx));
            auth_rows.push(
                div()
                    .px(px(s(12.0)))
                    .py(px(s(8.0)))
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text(
                        "connections.management.gcp_iam_hint",
                    ))
                    .into_any_element(),
            );
        }

        let database_control = self.render_database_control(database, cx);

        // Safety: profile-level toggles, then any flags forced on by the
        // assigned client / project / environment entities (Swift's
        // force-down), shown checked + locked.
        let mut safety_rows: Vec<AnyElement> =
            if crate::engine_form::uses_tls_modes(self.database_type) {
                // PostgreSQL/MySQL: the four-way TLS mode select plus the CA /
                // client-certificate / client-key path rows replace the two
                // legacy checkboxes.
                vec![
                    Self::card_row(
                        &crate::localization::text("connections.management.tls_label"),
                        Select::new(&self.tls_mode_select).into_any_element(),
                    ),
                    self.tls_path_row(
                        &crate::localization::text("connections.management.ca_file_label"),
                        TlsPathField::Ca,
                        "cm-tls-browse-ca",
                        cx,
                    ),
                    self.tls_path_row(
                        &crate::localization::text(
                            "connections.management.client_certificate_label",
                        ),
                        TlsPathField::ClientCert,
                        "cm-tls-browse-cert",
                        cx,
                    ),
                    self.tls_path_row(
                        &crate::localization::text("connections.management.client_key_label"),
                        TlsPathField::ClientKey,
                        "cm-tls-browse-key",
                        cx,
                    ),
                ]
            } else {
                vec![
                    Self::card_row(
                        &crate::localization::text(
                            "connections.management.encrypt_connection_label",
                        ),
                        Self::render_checkbox(
                            "cb-encrypt",
                            "",
                            encrypt,
                            tls_required,
                            cx,
                            Self::toggle_encrypt,
                        ),
                    ),
                    Self::card_row(
                        &crate::localization::text(
                            "connections.management.trust_certificate_label",
                        ),
                        Self::render_checkbox("cb-trust", "", trust, false, cx, Self::toggle_trust),
                    ),
                ]
            };
        safety_rows.extend([
            Self::card_row(
                &crate::localization::text("connections.management.select_only_label"),
                Self::render_checkbox(
                    "cb-block",
                    "",
                    block_select,
                    false,
                    cx,
                    Self::toggle_block_select,
                ),
            ),
            Self::card_row(
                &crate::localization::text("connections.management.rollback_label"),
                Self::render_checkbox(
                    "cb-rollback",
                    "",
                    rollback,
                    false,
                    cx,
                    Self::toggle_rollback,
                ),
            ),
        ]);
        if iam_tls {
            safety_rows.push(
                div()
                    .px(px(s(12.0)))
                    .pb(px(s(6.0)))
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text(
                        "connections.management.iam_tls_note",
                    ))
                    .into_any_element(),
            );
        }
        for (flag, source) in self.inherited_safety() {
            safety_rows.push(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap(px(s(12.0)))
                    .min_h(px(s(32.0)))
                    .px(px(s(12.0)))
                    .child(
                        div()
                            .text_size(px(Theme::font_size()))
                            .text_color(Theme::text_dim())
                            .child(crate::localization::format(
                                "connections.management.inherited_safety",
                                &[("flag_label", &flag), ("source_name", &source)],
                            )),
                    )
                    .child(
                        div()
                            .text_size(px(Theme::font_size()))
                            .text_color(Theme::text_dim())
                            .child(crate::localization::text(
                                "connections.management.enforced_label",
                            )),
                    )
                    .into_any_element(),
            );
        }

        let server_control = if auth_mode == AuthMode::EntraBrowser {
            self.render_azure_server_control(server, cx)
        } else {
            Self::field_control(server)
        };
        let mut server_rows: Vec<AnyElement> = vec![
            div()
                .px(px(s(12.0)))
                .pb(px(s(6.0)))
                .child(
                    Button::new("cm-paste-url")
                        .ghost()
                        .small()
                        .label(crate::localization::text(
                            "connections.management.paste_url_button",
                        ))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.url_paste_open = !this.url_paste_open;
                            cx.notify();
                        })),
                )
                .into_any_element(),
        ];
        if self.url_paste_open {
            server_rows.push(Self::card_row(
                &crate::localization::text("connections.management.connection_string_label"),
                Self::field_control(Input::new(&self.url_paste_input).cleanable(true)),
            ));
        }
        server_rows.push(Self::card_row(
            &crate::localization::text(if file_engine {
                "connections.management.database_file_label"
            } else {
                "connections.management.host_label"
            }),
            server_control,
        ));
        if let Some(msg) = host_error {
            server_rows.push(
                div()
                    .px(px(s(12.0)))
                    .pb(px(s(6.0)))
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::error())
                    .child(msg)
                    .into_any_element(),
            );
        }
        server_rows.push(Self::card_row(
            &crate::localization::text("connections.management.port_label"),
            div().w(px(s(90.0))).child(port).into_any_element(),
        ));
        server_rows.push(Self::card_row(
            &crate::localization::text("connections.management.database_label"),
            database_control,
        ));

        let form_body = div()
            .id("cm_form_scroll")
            .flex_1()
            .min_h_0()
            .overflow_scroll()
            .p(px(s(16.0)))
            .flex()
            .flex_col()
            .gap(px(s(16.0)))
            .child(Self::group(
                &crate::localization::text("connections.management.organization_section"),
                vec![
                    Self::card_row(
                        &crate::localization::text("connections.management.name_label"),
                        Self::field_control(name),
                    ),
                    Self::card_row(
                        &crate::localization::text("connections.management.database_engine_label"),
                        Self::field_control(
                            div().flex().flex_col().gap(px(s(2.0))).child(engine).when(
                                self.engine_auto_detect,
                                |d| {
                                    d.child(
                                        div()
                                            .text_size(px(Theme::font_size()))
                                            .text_color(Theme::text_dim())
                                            .child(crate::engine_form::auto_detect_hint(
                                                self.database_type,
                                            )),
                                    )
                                },
                            ),
                        ),
                    ),
                    Self::card_row(
                        &crate::localization::text("connections.management.client_label"),
                        client_select,
                    ),
                    Self::card_row(
                        &crate::localization::text("connections.management.project_label"),
                        project_select,
                    ),
                    Self::card_row_stacked(
                        &crate::localization::text("connections.management.environment_label"),
                        env_segmented,
                    ),
                ],
            ))
            .child(Self::group(
                &crate::localization::text("connections.management.server_section"),
                server_rows,
            ))
            .child(Self::group(
                &crate::localization::text("connections.management.authentication_section"),
                auth_rows,
            ))
            .child(Self::group(
                &crate::localization::text("connections.management.safety_section"),
                safety_rows,
            ))
            .when_some(health_message, |body, health| {
                body.child(Self::group(
                    &crate::localization::text("connections.management.health_section"),
                    vec![Self::card_row_stacked(
                        &crate::localization::text(
                            "connections.management.last_observed_status_label",
                        ),
                        div()
                            .text_size(px(Theme::font_size()))
                            .text_color(Theme::text_dim())
                            .child(health)
                            .into_any_element(),
                    )],
                ))
            })
            .child(Self::group(
                &crate::localization::text("connections.management.advanced_section"),
                vec![
                    Self::card_row(
                        &crate::localization::text("connections.management.timeout_label"),
                        div().w(px(s(90.0))).child(timeout).into_any_element(),
                    ),
                    Self::card_row(
                        &crate::localization::text("connections.management.query_timeout_label"),
                        div()
                            .w(px(s(90.0)))
                            .child(statement_timeout)
                            .into_any_element(),
                    ),
                    Self::card_row_stacked(
                        &crate::localization::text("connections.management.startup_sql_label"),
                        startup_sql.into_any_element(),
                    ),
                    Self::card_row_stacked(
                        &crate::localization::text("connections.management.notes_label"),
                        notes.into_any_element(),
                    ),
                ],
            ))
            .when(!errors.is_empty(), |st| {
                st.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(s(2.0)))
                        .children(errors.iter().map(|e| {
                            div()
                                .text_size(px(Theme::font_size()))
                                .text_color(Theme::error())
                                .child(e.clone())
                        })),
                )
            })
            .when_some(test_result, |st, result| {
                st.child(
                    div()
                        .text_size(px(Theme::font_size()))
                        .text_color(if test_success {
                            Theme::success()
                        } else {
                            Theme::error()
                        })
                        .child(result),
                )
            });

        // ----- Button bar -----
        let button_bar = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap(px(s(8.0)))
            .p(px(s(12.0)))
            .border_t_1()
            .border_color(Theme::border())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(8.0)))
                    .child(Self::button(
                        crate::localization::text("connections.management.test_button"),
                        can_save,
                        cx,
                        Self::handle_test,
                    ))
                    .child(Self::button(
                        crate::localization::text("connections.management.test_all_button"),
                        !self.testing_all,
                        cx,
                        Self::handle_test_all,
                    ))
                    .child(Self::button(
                        crate::localization::text(
                            if self
                                .editing_id
                                .and_then(|id| ConnectionStore::global().get(id))
                                .is_some_and(|profile| profile.favorite)
                            {
                                "connections.management.unfavorite_button"
                            } else {
                                "connections.management.favorite_button"
                            },
                        ),
                        self.editing_id.is_some(),
                        cx,
                        Self::handle_toggle_favorite,
                    )),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(8.0)))
                    .child(
                        div()
                            .text_size(px(Theme::font_size()))
                            .text_color(Theme::text_dim())
                            .child(status_message),
                    )
                    .child(Self::button(
                        crate::localization::text("connections.management.connect_button"),
                        can_save,
                        cx,
                        Self::handle_connect,
                    ))
                    .child(Self::button(
                        crate::localization::text("connections.management.save_button"),
                        can_save,
                        cx,
                        Self::handle_save,
                    ))
                    .child(Self::button(
                        crate::localization::text("connections.management.close_button"),
                        true,
                        cx,
                        Self::handle_close,
                    )),
            );

        let right_pane = div()
            .flex_1()
            .h_full()
            .min_h_0()
            .flex()
            .flex_col()
            .child(form_body)
            .child(button_bar);

        div()
            .flex_1()
            .h_full()
            .min_h_0()
            .flex()
            .flex_row()
            .child(left_pane)
            .child(split_divider)
            .child(right_pane)
            .into_any_element()
    }
}

fn localize_tree_labels(tree: &mut ConnectionManagementTree) {
    for node in &mut tree.nodes {
        let key = match node.label.as_str() {
            "SQL Servers" => "connections.management.sql_servers_root",
            "Unassigned Client" => "connections.management.unassigned_client",
            "Unassigned Project" => "connections.management.unassigned_project",
            "PRODUCTION" => "connections.management.production_environment",
            "STAGING" => "connections.management.staging_environment",
            "TEST" => "connections.management.test_environment",
            "DEV" => "connections.management.dev_environment",
            "LOCAL" => "connections.management.local_environment",
            "UNSET" => "connections.management.unset_environment",
            _ => continue,
        };
        node.label = crate::localization::text(key);
    }
}

pub(super) fn connection_tree_content_width(plans: &[RowPlan]) -> f32 {
    plans
        .iter()
        .map(|plan| {
            let label = plan.label.chars().count() as f32 * 7.0;
            let badges = plan
                .badges
                .iter()
                .map(|badge| badge.chars().count() as f32 * 6.0 + 12.0)
                .sum::<f32>();
            plan.depth as f32 * TREE_LEVEL_INDENT + DISCLOSURE_ICON_SIZE + 28.0 + label + badges
        })
        .fold(0.0, f32::max)
}

/// Badge shown on a saved-profile row whose profile routes through a relay
/// device rather than connecting straight to `server:port`.
pub(super) const RELAY_BADGE: &str = "via relay";

/// Appends the [`RELAY_BADGE`] to every saved-profile node whose profile uses a
/// [`ConnectionRoute::ViaRelay`](sqlly_core::ConnectionRoute) route. The tree
/// (built in `sqlly-core`) has no notion of routing, so the badge is stamped
/// here from the profiles that seeded it. Node ids for saved profiles are
/// `profile:{uuid}`, matching how the tree builder labels them.
fn tag_relay_routes(tree: &mut ConnectionManagementTree, profiles: &[ConnectionProfile]) {
    use sqlly_core::ConnectionManagementNodeKind;
    use sqlly_core::connection::ConnectionRoute;
    let relay_node_ids: HashSet<String> = profiles
        .iter()
        .filter(|p| matches!(p.route, ConnectionRoute::ViaRelay { .. }))
        .map(|p| format!("profile:{}", p.id.0))
        .collect();
    for node in tree.nodes.iter_mut() {
        if node.kind == ConnectionManagementNodeKind::SavedProfile
            && relay_node_ids.contains(&node.id.0)
            && !node.badges.iter().any(|b| b == RELAY_BADGE)
        {
            node.badges.push(RELAY_BADGE.to_string());
        }
    }
}

fn connection_health_badge(profile: &ConnectionProfile) -> &'static str {
    match profile.status.last_test_status {
        Some(ConnectionTestStatus::Succeeded) => "healthy",
        Some(ConnectionTestStatus::Failed) => "failed",
        Some(ConnectionTestStatus::Untested) | None => "untested",
    }
}

fn tag_connection_health(tree: &mut ConnectionManagementTree, profiles: &[ConnectionProfile]) {
    use sqlly_core::ConnectionManagementNodeKind;
    let health_by_node: std::collections::HashMap<String, &'static str> = profiles
        .iter()
        .map(|profile| {
            (
                format!("profile:{}", profile.id.0),
                connection_health_badge(profile),
            )
        })
        .collect();
    for node in &mut tree.nodes {
        if node.kind == ConnectionManagementNodeKind::SavedProfile
            && let Some(badge) = health_by_node.get(&node.id.0)
            && !node.badges.iter().any(|existing| existing == badge)
        {
            node.badges.push((*badge).to_string());
        }
    }
}

pub(super) fn connection_health_description(
    profile: &ConnectionProfile,
    now: chrono::DateTime<chrono::Utc>,
) -> String {
    let status = crate::localization::text(match connection_health_badge(profile) {
        "healthy" => "connections.management.healthy_badge",
        "failed" => "connections.management.failed_badge",
        _ => "connections.management.untested_badge",
    });
    let observed = profile
        .status
        .last_tested_at
        .map(|at| relative_health_age(now.signed_duration_since(at)))
        .unwrap_or_else(|| crate::localization::text("connections.management.never_tested"));
    if let Some(error) = crate::connection_health::last_error_text(&profile.status) {
        crate::localization::format(
            "connections.management.health_with_error",
            &[
                ("status", &status),
                ("observed", &observed),
                ("error", &error.to_string()),
            ],
        )
    } else {
        crate::localization::format(
            "connections.management.health_without_error",
            &[("status", &status), ("observed", &observed)],
        )
    }
}

pub(super) fn relative_health_age(age: chrono::Duration) -> String {
    let seconds = age.num_seconds().max(0);
    let (key, count) = if seconds < 60 {
        ("connections.management.health_just_now", 0)
    } else if seconds < 3_600 {
        ("connections.management.health_minutes_ago", seconds / 60)
    } else if seconds < 86_400 {
        ("connections.management.health_hours_ago", seconds / 3_600)
    } else {
        ("connections.management.health_days_ago", seconds / 86_400)
    };
    if count == 0 {
        crate::localization::text(key)
    } else {
        crate::localization::format(key, &[("count", &count.to_string())])
    }
}

/// Recursive walk producing owned row plans. Kept as a free function so the
/// immutable `ConnectionManagementTree` borrow stays self-contained and does
/// not conflict with the mutable `cx.listener` row construction in `render`.
fn walk(
    tree: &ConnectionManagementTree,
    expanded: &HashSet<String>,
    parent: Option<&sqlly_core::ConnectionManagementNodeId>,
    depth: usize,
    parent_ids: &HashSet<String>,
    out: &mut Vec<RowPlan>,
) {
    for node in tree.children_of(parent) {
        let has_children = parent_ids.contains(&node.id.0);
        out.push(RowPlan {
            id: node.id.0.clone(),
            label: node.label.clone(),
            depth,
            has_children,
            badges: node.badges.clone(),
        });
        if has_children && expanded.contains(&node.id.0) {
            walk(tree, expanded, Some(&node.id), depth + 1, parent_ids, out);
        }
    }
}
