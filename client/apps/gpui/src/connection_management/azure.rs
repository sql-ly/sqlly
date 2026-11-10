use super::*;
use crate::tooltip::AppTooltip;

impl ConnectionManagement {
    /// Push discovered Azure SQL resources into the management tree so they
    /// appear as an account → subscription → server → database hierarchy.
    pub fn set_azure_discoveries(
        &mut self,
        discoveries: Vec<AzureSqlDiscovery>,
        cx: &mut Context<Self>,
    ) {
        self.azure_discoveries = discoveries;
        self.azure_servers_loading = false;
        self.azure_servers_error = None;
        self.azure_server_help_copied = false;
        self.tree = Self::build_tree_with_azure(&self.azure_discoveries);
        self.expand_defaults();
        cx.notify();
    }

    pub fn set_azure_discovery_error(
        &mut self,
        message: impl Into<String>,
        cx: &mut Context<Self>,
    ) {
        self.azure_servers_loading = false;
        self.azure_servers_error = Some(message.into());
        // A permission failure is not a dead-end: keep the server field
        // front-and-center with a "type it yourself" hint instead of opening
        // the (empty) dropdown over it.
        self.azure_server_dropdown_open = false;
        self.azure_server_help_copied = false;
        cx.notify();
    }

    /// Copies an admin-ready request for the Reader role to the clipboard, so a
    /// user who lacks permission to enumerate Azure SQL servers can ask for it
    /// instead of typing every server by hand.
    fn copy_azure_permission_request(&mut self, cx: &mut Context<Self>) {
        let account = self.azure_account_label();
        let message = crate::localization::format(
            "connections.management.azure_permission_request",
            &[("account_label", &account)],
        );
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(message));
        self.azure_server_help_copied = true;
        cx.notify();
    }

    /// A human label for the Entra account this connection will authenticate as,
    /// for the admin-request message. Named from the SELECTED binding only, so a
    /// user with multiple accounts never sees the wrong identity.
    fn azure_account_label(&self) -> String {
        crate::azure_sql_token::permission_account_label(self.entra_account.as_ref())
    }

    fn azure_server_options(&self) -> Vec<(String, String)> {
        let selected_account = self
            .entra_account
            .as_ref()
            .map(|account| account.account_id.as_str());
        let mut servers = BTreeMap::new();
        for discovery in &self.azure_discoveries {
            if selected_account.is_some_and(|id| id != discovery.account_id) {
                continue;
            }
            servers
                .entry(discovery.server_fqdn.clone())
                .or_insert_with(|| discovery.server_name.clone());
        }
        servers.into_iter().collect()
    }

    fn select_azure_server(&mut self, server: String, window: &mut Window, cx: &mut Context<Self>) {
        self.server = server.clone();
        self.host_validation = validate_host(&server);
        self.azure_server_dropdown_open = false;
        self.database.clear();
        self.database_list = None;
        self.server_input
            .update(cx, |s, cx| s.set_value(server, window, cx));
        self.database_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.mark_dirty();
        cx.notify();
    }

    // ----- database dropdown (lazy list from the target server) -----

    /// Opens/closes the database dropdown. `open` comes from the render-time
    /// snapshot so the popup's capture-phase `on_mouse_down_out` dismissal
    /// and a caret click compose to "closed" instead of fighting.
    fn set_database_dropdown_open(&mut self, open: bool, cx: &mut Context<Self>) {
        self.database_dropdown_open = open;
        if open && self.database_list.is_none() && !self.database_loading {
            self.load_databases(cx);
        }
        cx.notify();
    }

    fn select_database(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        self.database = name.clone();
        self.database_dropdown_open = false;
        self.database_input
            .update(cx, |s, cx| s.set_value(name, window, cx));
        self.mark_dirty();
        cx.notify();
    }

    fn load_databases(&mut self, cx: &mut Context<Self>) {
        if self.server.trim().is_empty() {
            self.database_list_error = Some(crate::localization::text(
                "connections.management.enter_server_first",
            ));
            cx.notify();
            return;
        }
        let profile = match crate::sql_credentials::hydrate_profile(self.build_profile(cx)) {
            Ok(profile) => profile,
            Err(error) => {
                self.database_list_error = Some(crate::localization::format(
                    "connections.management.database_password_load_failed",
                    &[("error", &crate::localization::message(error))],
                ));
                cx.notify();
                return;
            }
        };
        self.database_loading = true;
        self.database_list_error = None;
        cx.notify();
        // Same route-aware resolution as the test button above.
        let services = self.router.services_for(&profile);
        let provider = self.entra_sql_token_provider.clone();
        let async_cx = cx.to_async();
        let weak_self = cx.weak_entity();
        cx.spawn(async move |_this, _cx| {
            // Re-mint the Entra SQL token before listing databases, so the
            // dropdown refreshes like a query window instead of failing on a
            // stale token.
            let profile = match &provider {
                Some(provider) => {
                    match crate::azure_sql_token::hydrate_entra_token(provider, profile).await {
                        Ok(profile) => profile,
                        Err(message) => {
                            let mut cx = async_cx.clone();
                            weak_self
                                .update(&mut cx, |this, cx| {
                                    this.database_loading = false;
                                    this.database_list_error = Some(message);
                                    this.database_list = None;
                                    cx.notify();
                                })
                                .ok();
                            return;
                        }
                    }
                }
                None => profile,
            };
            let result = services.connections.list_databases(&profile).await;
            let mut cx = async_cx.clone();
            weak_self
                .update(&mut cx, |this, cx| {
                    this.database_loading = false;
                    match result {
                        Ok(dbs) => {
                            this.database_list = Some(dbs.into_iter().map(|d| d.name).collect());
                            this.database_list_error = None;
                        }
                        Err(e) => {
                            this.database_list_error =
                                Some(crate::localization::message(e.message()));
                            this.database_list = None;
                        }
                    }
                    cx.notify();
                })
                .ok();
        })
        .detach();
    }

    // ----- entity panes (Clients / Projects / Environments) -----

    pub(super) fn render_azure_server_control(
        &mut self,
        server: Input,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let s = Theme::scaled;
        let open = self.azure_server_dropdown_open;
        let loading = self.azure_servers_loading;
        let error = self.azure_servers_error.clone();
        let has_error = error.is_some();
        let help_copied = self.azure_server_help_copied;
        let options = self.azure_server_options();

        let body = if loading {
            div()
                .p(px(s(8.0)))
                .text_size(px(Theme::font_size()))
                .text_color(Theme::text_dim())
                .child(crate::localization::text(
                    "connections.management.azure_servers_loading",
                ))
                .into_any_element()
        } else if let Some(error) = error {
            div()
                .p(px(s(8.0)))
                .text_size(px(Theme::font_size()))
                .text_color(Theme::error())
                .child(error)
                .into_any_element()
        } else if options.is_empty() {
            div()
                .p(px(s(8.0)))
                .text_size(px(Theme::font_size()))
                .text_color(Theme::text_dim())
                .child(crate::localization::text(
                    "connections.management.no_azure_servers",
                ))
                .into_any_element()
        } else {
            div()
                .id("cm-azure-server-list")
                .flex()
                .flex_col()
                .max_h(px(s(190.0)))
                .overflow_y_scroll()
                .children(
                    options
                        .into_iter()
                        .enumerate()
                        .map(|(index, (fqdn, name))| {
                            let selected = fqdn.clone();
                            div()
                                .id(("cm-azure-server", index))
                                .px(px(s(8.0)))
                                .py(px(s(5.0)))
                                .flex()
                                .flex_col()
                                .text_size(px(Theme::font_size()))
                                .text_color(Theme::text())
                                .hover(|style| style.bg(Theme::row_hover()))
                                .child(name)
                                .child(
                                    div()
                                        .text_size(px(Theme::font_size_ratio(0.85)))
                                        .text_color(Theme::text_dim())
                                        .child(fqdn),
                                )
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _, window, cx| {
                                        this.select_azure_server(selected.clone(), window, cx)
                                    }),
                                )
                        }),
                )
                .into_any_element()
        };

        let dropdown = if open {
            let dismiss = cx.entity();
            Some(
                div().absolute().top(px(s(30.0))).left_0().child(deferred(
                    anchored().snap_to_window_with_margin(px(s(8.0))).child(
                        div()
                            .occlude()
                            .w(px(s(360.0)))
                            .bg(Theme::popup_bg())
                            .border_1()
                            .border_color(Theme::popup_border())
                            .rounded(px(s(4.0)))
                            .on_mouse_down_out(move |_: &MouseDownEvent, _window, app| {
                                dismiss.update(app, |this, cx| {
                                    this.azure_server_dropdown_open = false;
                                    cx.notify();
                                });
                            })
                            .child(body),
                    ),
                )),
            )
        } else {
            None
        };

        let field_row = div()
            .relative()
            .flex()
            .flex_row()
            .gap(px(s(6.0)))
            .child(div().flex_1().child(server))
            .child(
                div()
                    .id("cm-azure-server-caret")
                    .h(px(s(26.0)))
                    .px(px(s(8.0)))
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor(gpui::CursorStyle::PointingHand)
                    .border_1()
                    .border_color(Theme::border())
                    .rounded(px(s(4.0)))
                    .text_color(Theme::text_dim())
                    .hover(|style| style.bg(Theme::border()))
                    .child("\u{25be}")
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            this.azure_server_dropdown_open = !open;
                            cx.notify();
                        }),
                    ),
            )
            .children(dropdown);

        // On a discovery failure (typically a 403 for accounts that can't list
        // servers), keep the field usable and reassure the user they can type
        // the server name — with an info affordance that copies an admin-ready
        // request for the Reader role.
        let note = has_error.then(|| {
            div()
                .flex()
                .flex_col()
                .gap(px(s(3.0)))
                .mt(px(s(4.0)))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(s(5.0)))
                        .text_size(px(Theme::font_size_ratio(0.9)))
                        .text_color(Theme::text_dim())
                        .child(crate::localization::text(
                            "connections.management.azure_listing_manual_hint",
                        ))
                        .child(
                            Button::new("cm-azure-perm-info")
                                .ghost()
                                .xsmall()
                                .icon(
                                    gpui_component::Icon::empty()
                                        .path(crate::icon_catalog::AppIcon::Info.path()),
                                )
                                .app_tooltip_text(crate::localization::text(
                                    "connections.management.copy_azure_permission_tooltip",
                                ))
                                .on_click(cx.listener(|this, _ev, _window, cx| {
                                    this.copy_azure_permission_request(cx);
                                })),
                        ),
                )
                .children(help_copied.then(|| {
                    div()
                        .text_size(px(Theme::font_size_ratio(0.85)))
                        .text_color(Theme::text_dim())
                        .child(crate::localization::text(
                            "connections.management.azure_permission_copied",
                        ))
                }))
        });

        div()
            .flex()
            .flex_col()
            .child(field_row)
            .children(note)
            .into_any_element()
    }

    /// The Database field's control: input + caret + lazily-loaded dropdown.
    /// The popup is `deferred(anchored(...))` so the scrolling form cannot
    /// clip it, with `.occlude()` + `.on_mouse_down_out()` dismissal.
    pub(super) fn render_database_control(
        &mut self,
        database: Input,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let s = Theme::scaled;
        let open = self.database_dropdown_open;

        let caret = div()
            .id("cm-db-caret")
            .h(px(s(26.0)))
            .px(px(s(8.0)))
            .flex()
            .items_center()
            .justify_center()
            .cursor(gpui::CursorStyle::PointingHand)
            .border_1()
            .border_color(Theme::border())
            .rounded(px(s(4.0)))
            .text_size(px(Theme::font_size()))
            .text_color(Theme::text_dim())
            .hover(|st| st.bg(Theme::border()))
            .child("\u{25be}")
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    this.set_database_dropdown_open(!open, cx);
                }),
            );

        let dropdown: Option<AnyElement> = if open {
            let body: AnyElement = if self.database_loading {
                div()
                    .p(px(s(8.0)))
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text(
                        "connections.management.databases_loading",
                    ))
                    .into_any_element()
            } else if let Some(e) = self.database_list_error.clone() {
                div()
                    .p(px(s(8.0)))
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::error())
                    .child(e)
                    .into_any_element()
            } else if let Some(dbs) = self.database_list.clone() {
                if dbs.is_empty() {
                    div()
                        .p(px(s(8.0)))
                        .text_size(px(Theme::font_size()))
                        .text_color(Theme::text_dim())
                        .child(crate::localization::text(
                            "connections.management.no_databases",
                        ))
                        .into_any_element()
                } else {
                    div()
                        .id("cm-db-dropdown-scroll")
                        .flex()
                        .flex_col()
                        .max_h(px(s(160.0)))
                        .overflow_y_scroll()
                        .children(dbs.into_iter().enumerate().map(|(i, name)| {
                            let n = name.clone();
                            div()
                                .id(("cm-db-item", i))
                                .px(px(s(8.0)))
                                .h(px(s(24.0)))
                                .flex()
                                .items_center()
                                .text_size(px(Theme::font_size()))
                                .text_color(Theme::text())
                                .hover(|st| st.bg(Theme::row_hover()))
                                .child(name)
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _, window, cx| {
                                        this.select_database(n.clone(), window, cx)
                                    }),
                                )
                        }))
                        .into_any_element()
                }
            } else {
                div()
                    .p(px(s(8.0)))
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text(
                        "connections.management.databases_loading",
                    ))
                    .into_any_element()
            };
            let dismiss = cx.entity();
            Some(
                div()
                    .absolute()
                    .top(px(s(30.0)))
                    .left(px(0.0))
                    .child(
                        deferred(
                            anchored().snap_to_window_with_margin(px(s(8.0))).child(
                                div()
                                    .occlude()
                                    .w(px(s(280.0)))
                                    .bg(Theme::popup_bg())
                                    .border_1()
                                    .border_color(Theme::popup_border())
                                    .rounded(px(s(4.0)))
                                    .p(px(s(4.0)))
                                    .child(body)
                                    .on_mouse_down_out(move |_: &MouseDownEvent, _window, app| {
                                        dismiss.update(app, |this, cx| {
                                            if this.database_dropdown_open {
                                                this.database_dropdown_open = false;
                                                cx.notify();
                                            }
                                        });
                                    }),
                            ),
                        )
                        .with_priority(1),
                    )
                    .into_any_element(),
            )
        } else {
            None
        };

        div()
            .relative()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(s(6.0)))
            .child(div().w(px(s(200.0))).child(database))
            .child(caret)
            .children(dropdown)
            .into_any_element()
    }

    // ----- System Settings-style card helpers -----
}
