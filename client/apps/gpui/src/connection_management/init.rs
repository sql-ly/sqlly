use super::*;

/// Format an `Hsla` as a lowercase `#rrggbb` hex string (the canonical
/// entity-color representation persisted by the store). One shared
/// implementation lives in `crate::theme`.
use crate::theme::hsla_to_hex_string;

impl ConnectionManagement {
    pub fn new(
        router: crate::service_router::ServiceRouter,
        log: crate::log_console::DiagnosticLog,
        entra_registry: EntraAccountRegistryStore,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut view = Self::empty(router, log, entra_registry, window, cx);
        view.expand_defaults();
        view
    }

    /// Inject the shared SQL-token provider so Test Connection and the Databases
    /// dropdown re-mint an expired Entra token before dialing.
    pub fn set_entra_sql_token_provider(
        &mut self,
        provider: crate::azure_sql_token::EntraSqlTokenProvider,
    ) {
        self.entra_sql_token_provider = Some(provider);
    }

    fn empty(
        router: crate::service_router::ServiceRouter,
        log: crate::log_console::DiagnosticLog,
        entra_registry: EntraAccountRegistryStore,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let password = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(crate::localization::input_placeholder(
                    window,
                    cx,
                    "connections.management.password_placeholder",
                ))
                .masked(true)
        });
        let entra_accounts = entra_registry.list();
        let sql_account_password = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(crate::localization::input_placeholder(
                    window,
                    cx,
                    "connections.management.account_password_placeholder",
                ))
                .masked(true)
        });

        let search_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "connections.management.filter_connections_placeholder",
            ))
        });
        let name_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "connections.management.connection_name_placeholder",
            ))
        });
        let server_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("server.example.com"));
        let port_input = cx.new(|cx| InputState::new(window, cx).default_value("1433"));
        let engine_select = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(crate::engine_form::picker_labels()),
                None,
                window,
                cx,
            )
            .searchable(true)
        });
        engine_select.update(cx, |state, cx| {
            state.set_selected_value(
                &DatabaseType::SqlServer.display_name().to_string(),
                window,
                cx,
            )
        });
        let database_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "connections.management.optional_database_placeholder",
            ))
        });
        let username_input = cx.new(|cx| InputState::new(window, cx).placeholder("sa"));
        let aws_region_input = cx.new(|cx| InputState::new(window, cx).placeholder("us-west-2"));
        #[cfg(not(target_arch = "wasm32"))]
        let aws_profile_select = cx.new(|cx| {
            SelectState::new(
                vec![crate::cloud_iam_token::aws_profile_default_label()],
                Some(gpui_component::IndexPath::new(0)),
                window,
                cx,
            )
        });
        let tls_mode_select = cx.new(|cx| {
            SelectState::new(
                crate::connection_editor::tls_mode_labels(),
                None,
                window,
                cx,
            )
        });
        tls_mode_select.update(cx, |state, cx| {
            state.set_selected_value(
                &crate::connection_editor::tls_mode_label(TlsMode::VerifyFull).to_string(),
                window,
                cx,
            )
        });
        let tls_ca_input = cx.new(|cx| InputState::new(window, cx).placeholder("/path/to/ca.pem"));
        let tls_cert_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("/path/to/client-cert.pem"));
        let tls_key_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("/path/to/client-key.pem"));
        let timeout_input = cx.new(|cx| InputState::new(window, cx).default_value("30"));
        let statement_timeout_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "connections.management.no_query_timeout_placeholder",
            ))
        });
        let startup_sql_input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .soft_wrap(true)
                .auto_grow(3, 8)
                .placeholder(crate::localization::input_placeholder(
                    window,
                    cx,
                    "connections.management.startup_sql_placeholder",
                ))
        });
        let notes_input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .soft_wrap(true)
                .auto_grow(3, 8)
                .placeholder(crate::localization::input_placeholder(
                    window,
                    cx,
                    "connections.management.notes_placeholder",
                ))
        });
        let url_paste_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("postgres://user@host:5432/db  ·  Server=…;Database=…  ·  jdbc:…")
        });
        let entity_name_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "connections.management.entity_name_placeholder",
            ))
        });
        let sql_account_name_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "connections.management.sql_account_friendly_name_placeholder",
            ))
        });
        let sql_account_username_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "connections.management.sql_account_username_placeholder",
            ))
        });

        let store = ConnectionStore::global();
        let client_select = cx.new(|cx| {
            SelectState::new(
                Self::select_items(store.clients().into_iter().map(|e| e.name).collect()),
                None,
                window,
                cx,
            )
        });
        let project_select = cx.new(|cx| {
            SelectState::new(
                Self::select_items(store.projects().into_iter().map(|e| e.name).collect()),
                None,
                window,
                cx,
            )
        });

        let entity_color_picker = cx.new(|cx| ColorPickerState::new(window, cx));

        let mut subscriptions = vec![
            // The dirty-marking syncs are equality-guarded: `set_value` also
            // emits `InputEvent::Change`, and a programmatic fill (which sets
            // the canonical String first) must not mark the form edited.
            Self::text_sync(&search_input, window, cx, |this, text| this.search = text),
            Self::text_sync(&name_input, window, cx, |this, text| {
                if this.name != text {
                    this.name = text;
                    this.mark_dirty();
                }
            }),
            cx.subscribe_in(
                &server_input,
                window,
                |this: &mut Self, state, event: &InputEvent, window, cx| {
                    if let InputEvent::Change = event {
                        let text = state.read(cx).value().to_string();
                        if this.server != text {
                            this.host_validation =
                                if crate::engine_form::is_file_engine(this.database_type) {
                                    if text.trim().is_empty() {
                                        HostValidationResult::Empty
                                    } else {
                                        HostValidationResult::Valid
                                    }
                                } else {
                                    validate_host(&text)
                                };
                            this.server = text;
                            this.mark_dirty();
                            this.maybe_prefill_aws_region_from_server(window, cx);
                            if this.engine_auto_detect {
                                let engine = crate::engine_form::auto_detect_engine(
                                    &this.server,
                                    this.database_type,
                                );
                                if engine != this.database_type {
                                    this.apply_engine(engine, window, cx);
                                }
                            }
                        }
                        cx.notify();
                    }
                },
            ),
            Self::text_sync(&aws_region_input, window, cx, |this, text| {
                if this.aws_region != text {
                    this.aws_region = text;
                    this.mark_dirty();
                }
            }),
            Self::text_sync(&port_input, window, cx, |this, text| {
                if this.port != text {
                    this.port = text;
                    this.mark_dirty();
                }
            }),
            Self::text_sync(&database_input, window, cx, |this, text| {
                if this.database != text {
                    this.database = text;
                    this.mark_dirty();
                }
            }),
            Self::text_sync(&username_input, window, cx, |this, text| {
                if this.username != text {
                    this.username = text;
                    this.mark_dirty();
                }
            }),
            #[cfg(not(target_arch = "wasm32"))]
            cx.subscribe_in(
                &aws_profile_select,
                window,
                |this: &mut Self, select, ev: &SelectEvent<Vec<String>>, _window, cx| {
                    let SelectEvent::Confirm(value) = ev;
                    let profile = value
                        .as_ref()
                        .and_then(|_| crate::connection_editor::select_row(select, cx))
                        .map(|row| {
                            crate::cloud_iam_token::aws_profile_from_row(
                                row,
                                &this.aws_profile_options,
                            )
                        })
                        .unwrap_or_default();
                    if this.aws_profile != profile {
                        this.aws_profile = profile;
                        this.mark_dirty();
                    }
                    cx.notify();
                },
            ),
            cx.subscribe_in(
                &tls_mode_select,
                window,
                |this: &mut Self, select, ev: &SelectEvent<Vec<String>>, _window, cx| {
                    let SelectEvent::Confirm(value) = ev;
                    let Some(mode) = value
                        .as_ref()
                        .and_then(|_| crate::connection_editor::select_row(select, cx))
                        .and_then(crate::connection_editor::tls_mode_from_row)
                    else {
                        return;
                    };
                    this.set_tls_mode(mode, cx);
                },
            ),
            Self::text_sync(&tls_ca_input, window, cx, |this, text| {
                if this.tls_ca_file != text {
                    this.tls_ca_file = text;
                    this.mark_dirty();
                }
            }),
            Self::text_sync(&tls_cert_input, window, cx, |this, text| {
                if this.tls_client_cert_file != text {
                    this.tls_client_cert_file = text;
                    this.mark_dirty();
                }
            }),
            Self::text_sync(&tls_key_input, window, cx, |this, text| {
                if this.tls_client_key_file != text {
                    this.tls_client_key_file = text;
                    this.mark_dirty();
                }
            }),
            Self::text_sync(&timeout_input, window, cx, |this, text| {
                if this.timeout != text {
                    this.timeout = text;
                    this.mark_dirty();
                }
            }),
            Self::text_sync(&statement_timeout_input, window, cx, |this, text| {
                if this.statement_timeout != text {
                    this.statement_timeout = text;
                    this.mark_dirty();
                }
            }),
            cx.subscribe_in(
                &startup_sql_input,
                window,
                |this: &mut Self, input, event: &InputEvent, _window, cx| {
                    if let InputEvent::Change = event {
                        let text = input.read(cx).value().to_string();
                        if this.startup_sql != text {
                            this.startup_sql = text;
                            this.mark_dirty();
                        }
                    }
                },
            ),
            cx.subscribe_in(
                &notes_input,
                window,
                |this: &mut Self, input, event: &InputEvent, _window, cx| {
                    if let InputEvent::Change = event {
                        let text = input.read(cx).value().to_string();
                        if this.notes != text {
                            this.notes = text;
                            this.mark_dirty();
                        }
                    }
                },
            ),
            Self::text_sync(&entity_name_input, window, cx, |this, text| {
                this.entity_name = text;
            }),
            Self::text_sync(&sql_account_name_input, window, cx, |this, text| {
                this.sql_account_name = text;
            }),
            Self::text_sync(&sql_account_username_input, window, cx, |this, text| {
                this.sql_account_username = text;
            }),
            cx.subscribe_in(
                &client_select,
                window,
                |this: &mut Self, _, ev: &SelectEvent<Vec<String>>, _window, cx| {
                    let SelectEvent::Confirm(value) = ev;
                    let choice = Self::entity_choice(value);
                    if this.client != choice {
                        this.client = choice;
                        this.mark_dirty();
                    }
                    cx.notify();
                },
            ),
            cx.subscribe_in(
                &project_select,
                window,
                |this: &mut Self, _, ev: &SelectEvent<Vec<String>>, _window, cx| {
                    let SelectEvent::Confirm(value) = ev;
                    let choice = Self::entity_choice(value);
                    if this.project != choice {
                        this.project = choice;
                        this.mark_dirty();
                    }
                    cx.notify();
                },
            ),
            cx.subscribe_in(
                &entity_color_picker,
                window,
                |this: &mut Self, _, ev: &ColorPickerEvent, _window, cx| {
                    let ColorPickerEvent::Change(color) = ev;
                    let hex = color.map(hsla_to_hex_string).unwrap_or_default();
                    if this.entity_color != hex {
                        this.entity_color = hex;
                        cx.notify();
                    }
                },
            ),
        ];
        subscriptions.push(cx.subscribe_in(
            &url_paste_input,
            window,
            |this: &mut Self, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.apply_pasted_url(window, cx);
                }
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &engine_select,
            window,
            |this: &mut Self, _, ev: &SelectEvent<SearchableVec<String>>, window, cx| {
                // Typing in the picker filters the list, so the confirmed row
                // no longer indexes the engine table; resolve by label instead.
                let SelectEvent::Confirm(value) = ev;
                match value
                    .as_deref()
                    .and_then(crate::engine_form::choice_from_label)
                {
                    Some(crate::engine_form::EngineChoice::AutoDetect) => {
                        this.engine_auto_detect = true;
                        let engine = crate::engine_form::auto_detect_engine(
                            &this.server,
                            this.database_type,
                        );
                        this.apply_engine(engine, window, cx);
                    }
                    Some(crate::engine_form::EngineChoice::Engine(engine)) => {
                        this.engine_auto_detect = false;
                        this.apply_engine(engine, window, cx);
                    }
                    None => {}
                }
                this.mark_dirty();
                cx.notify();
            },
        ));

        subscriptions.push(
            cx.observe_global_in::<crate::localization::LocalizationChanged>(
                window,
                |this, window, cx| {
                    this.sync_entity_selects(window, cx);
                    let selected = crate::connection_editor::tls_mode_label(this.tls_mode);
                    this.tls_mode_select.update(cx, |state, cx| {
                        state.set_items(crate::connection_editor::tls_mode_labels(), window, cx);
                        state.set_selected_value(&selected, window, cx);
                    });
                    #[cfg(not(target_arch = "wasm32"))]
                    {
                        if let Some(first) = this.aws_profile_options.first_mut() {
                            *first = crate::cloud_iam_token::aws_profile_default_label();
                        }
                        this.aws_profile_select_stale = true;
                    }
                    cx.notify();
                },
            ),
        );
        Self {
            router,
            log,
            pane: WorkspacePane::Connections,
            dirty: false,
            close_confirm: false,
            windowed: false,
            host_validation: HostValidationResult::Empty,
            entra_account: None,
            entra_registry,
            entra_sql_token_provider: None,
            entra_accounts,
            entra_selected: None,
            entra_status: crate::localization::text("connections.management.entra_initial_status"),
            entra_in_flight: false,
            sql_accounts: store.sql_auth_accounts(),
            sql_selected: None,
            connection_sql_account: None,
            sql_account_name: String::new(),
            sql_account_username: String::new(),
            sql_account_name_input,
            sql_account_username_input,
            sql_account_password,
            sql_account_status: crate::localization::text(
                "connections.management.sql_account_initial_status",
            ),
            database_dropdown_open: false,
            database_list: None,
            database_loading: false,
            database_list_error: None,
            entity_selected: None,
            entity_name: String::new(),
            entity_color: String::new(),
            entity_image_path: String::new(),
            entity_name_input,
            entity_color_picker,
            entity_safety: SafetyFeatures::default(),
            entity_status: String::new(),
            search: String::new(),
            name: String::new(),
            server: String::new(),
            port: "1433".to_string(),
            database_type: DatabaseType::SqlServer,
            engine_auto_detect: false,
            database: String::new(),
            username: String::new(),
            aws_region: String::new(),
            aws_profile: String::new(),
            #[cfg(not(target_arch = "wasm32"))]
            gcp_adc_status: String::new(),
            password,
            client: String::new(),
            project: String::new(),
            timeout: "30".to_string(),
            statement_timeout: String::new(),
            startup_sql: String::new(),
            notes: String::new(),
            environment: ConnectionEnvironment::Local,
            auth_mode: AuthMode::SqlLogin,
            encrypt: true,
            trust: false,
            tls_mode: TlsMode::VerifyFull,
            tls_ca_file: String::new(),
            tls_client_cert_file: String::new(),
            tls_client_key_file: String::new(),
            tls_ca_input,
            tls_cert_input,
            tls_key_input,
            tls_mode_select,
            block_select: false,
            rollback: false,
            editing_id: None,
            duplicate_of: None,
            expanded: HashSet::new(),
            selected: None,
            tree: Self::build_tree(),
            azure_discoveries: Vec::new(),
            azure_servers_loading: false,
            azure_servers_error: None,
            azure_server_dropdown_open: false,
            azure_server_help_copied: false,
            testing: false,
            testing_all: false,
            test_result: None,
            test_success: false,
            status_message: crate::localization::text("connections.management.initial_status"),
            // Same resize minimum the old hand-rolled resize handle enforced
            // (design units, scaled for the frame).
            frame: DialogFrame::new(Theme::scaled(DIALOG_WIDTH), Theme::scaled(DIALOG_HEIGHT))
                .with_min_size(Theme::scaled(600.0), Theme::scaled(400.0)),
            split_left_width: 250.0,
            connection_tree_scroll: ScrollHandle::new(),
            account_list_scroll: ScrollHandle::new(),
            entity_list_scroll: ScrollHandle::new(),
            is_split_resizing: false,
            split_resize_start_pos: Point::default(),
            split_resize_start_width: 0.0,
            search_input,
            name_input,
            server_input,
            port_input,
            engine_select,
            database_input,
            username_input,
            aws_region_input,
            #[cfg(not(target_arch = "wasm32"))]
            aws_profile_select,
            #[cfg(not(target_arch = "wasm32"))]
            aws_profile_options: vec![crate::cloud_iam_token::aws_profile_default_label()],
            #[cfg(not(target_arch = "wasm32"))]
            aws_profiles_loaded: false,
            #[cfg(not(target_arch = "wasm32"))]
            aws_profile_select_stale: false,
            timeout_input,
            statement_timeout_input,
            startup_sql_input,
            notes_input,
            url_paste_input,
            url_paste_open: false,
            client_select,
            project_select,
            _subscriptions: subscriptions,
        }
    }

    /// One `InputEvent::Change` subscription mirroring an input's text into
    /// its canonical `String` field (plus any side effects in `apply`).
    fn text_sync(
        input: &Entity<InputState>,
        window: &mut Window,
        cx: &mut Context<Self>,
        apply: impl Fn(&mut Self, String) + 'static,
    ) -> Subscription {
        cx.subscribe_in(
            input,
            window,
            move |this: &mut Self, state, event: &InputEvent, _window, cx| {
                if let InputEvent::Change = event {
                    apply(this, state.read(cx).value().to_string());
                    cx.notify();
                }
            },
        )
    }

    /// Item list for the client / project selects: a leading "None" sentinel
    /// followed by the stored entity names (Swift's entity popups).
    fn select_items(mut names: Vec<String>) -> Vec<String> {
        let mut items = vec![none_choice()];
        items.append(&mut names);
        items
    }

    /// Map a confirmed select value back to the canonical field value (the
    /// "None" sentinel clears the assignment).
    fn entity_choice(value: &Option<String>) -> String {
        match value.as_deref() {
            None => String::new(),
            Some(value) if value == none_choice() => String::new(),
            Some(v) => v.to_string(),
        }
    }

    /// The select-item value representing a canonical field value.
    fn select_value(current: &str) -> String {
        let trimmed = current.trim();
        if trimmed.is_empty() {
            none_choice()
        } else {
            trimmed.to_string()
        }
    }

    /// Refresh the client / project select items from the store and re-apply
    /// the current selections. Call after entity edits or programmatic form
    /// fills.
    pub(super) fn sync_entity_selects(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let store = ConnectionStore::global();
        let clients = Self::select_items(store.clients().into_iter().map(|e| e.name).collect());
        let projects = Self::select_items(store.projects().into_iter().map(|e| e.name).collect());
        let client_value = Self::select_value(&self.client);
        let project_value = Self::select_value(&self.project);
        self.client_select.update(cx, |state, cx| {
            state.set_items(clients, window, cx);
            state.set_selected_value(&client_value, window, cx);
        });
        self.project_select.update(cx, |state, cx| {
            state.set_items(projects, window, cx);
            state.set_selected_value(&project_value, window, cx);
        });
    }
}
