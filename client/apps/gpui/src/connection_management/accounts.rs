use super::*;

impl ConnectionManagement {
    pub fn reload_entra_accounts(&mut self) {
        self.entra_accounts = self.entra_registry.list();
        if let Some(idx) = self.entra_selected
            && idx >= self.entra_accounts.len()
        {
            self.entra_selected = None;
        }
    }

    /// Marks the Accounts pane busy while the browser sign-in runs.
    pub fn begin_entra_signin(&mut self, cx: &mut Context<Self>) {
        self.entra_in_flight = true;
        self.entra_status =
            crate::localization::text("connections.management.entra_complete_browser_signin");
        cx.notify();
    }

    pub fn on_signin_success(
        &mut self,
        outcome: &crate::azure_signin::SigninOutcome,
        cx: &mut Context<Self>,
    ) {
        self.entra_in_flight = false;
        self.reload_entra_accounts();
        let label = entra_display_label(&outcome.profile);
        self.entra_selected = self
            .entra_accounts
            .iter()
            .position(|account| account.id == outcome.profile.id);
        self.sql_selected = None;
        self.entra_status = crate::localization::format(
            "connections.management.entra_signed_in",
            &[("account_label", &label)],
        );
        cx.notify();
    }

    pub fn on_signin_error(&mut self, message: impl Into<String>, cx: &mut Context<Self>) {
        self.entra_in_flight = false;
        self.entra_status = crate::localization::format(
            "connections.management.entra_signin_failed",
            &[("error", &message.into())],
        );
        cx.notify();
    }

    fn remove_selected_entra_account(&mut self, cx: &mut Context<Self>) {
        let Some(idx) = self.entra_selected else {
            return;
        };
        let Some(account) = self.entra_accounts.get(idx) else {
            return;
        };
        let label = entra_display_label(account);
        let id = account.id.clone();
        self.entra_registry.remove(&id);
        self.entra_status = crate::localization::format(
            "connections.management.entra_removed",
            &[("account_label", &label)],
        );
        self.reload_entra_accounts();
        self.entra_selected = (!self.entra_accounts.is_empty()).then_some(0);
        cx.notify();
    }

    pub(super) fn reload_sql_accounts(&mut self) {
        self.sql_accounts = ConnectionStore::global().sql_auth_accounts();
        if self
            .sql_selected
            .is_some_and(|id| !self.sql_accounts.iter().any(|account| account.id == id))
        {
            self.sql_selected = None;
        }
    }

    fn begin_sql_account(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.entra_selected = None;
        self.sql_selected = None;
        self.sql_account_name.clear();
        self.sql_account_username.clear();
        self.sql_account_name_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.sql_account_username_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.sql_account_password
            .update(cx, |field, cx| field.set_value("", window, cx));
        self.sql_account_status =
            crate::localization::text("connections.management.sql_account_create_help");
        cx.notify();
    }

    fn select_sql_account(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(account) = self
            .sql_accounts
            .iter()
            .find(|account| account.id == id)
            .cloned()
        else {
            return;
        };
        self.sql_selected = Some(id);
        self.entra_selected = None;
        self.sql_account_name = account.name.clone();
        self.sql_account_username = account.username.clone();
        self.sql_account_name_input
            .update(cx, |s, cx| s.set_value(account.name, window, cx));
        self.sql_account_username_input
            .update(cx, |s, cx| s.set_value(account.username, window, cx));
        self.sql_account_password
            .update(cx, |field, cx| field.set_value("", window, cx));
        self.sql_account_status =
            crate::localization::text("connections.management.sql_account_keep_password_help");
        cx.notify();
    }

    fn save_sql_account(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.sql_account_name.trim().to_string();
        let username = self.sql_account_username.trim().to_string();
        let password = self.sql_account_password.read(cx).value().to_string();
        if name.is_empty() || username.is_empty() {
            self.sql_account_status =
                crate::localization::text("connections.management.sql_account_name_user_required");
            cx.notify();
            return;
        }
        let id = self.sql_selected.unwrap_or_else(Uuid::new_v4);
        if self.sql_selected.is_none() && password.is_empty() {
            self.sql_account_status =
                crate::localization::text("connections.management.sql_account_password_required");
            cx.notify();
            return;
        }
        if !password.is_empty()
            && let Err(error) = crate::sql_credentials::store_password(id, &password)
        {
            self.sql_account_status = crate::localization::format(
                "connections.management.sql_account_save_password_failed",
                &[("error", &error)],
            );
            cx.notify();
            return;
        }
        ConnectionStore::global().upsert_sql_auth_account(SqlAuthAccount {
            id,
            name: name.clone(),
            username,
        });
        self.sql_selected = Some(id);
        self.sql_account_password
            .update(cx, |field, cx| field.set_value("", window, cx));
        self.reload_sql_accounts();
        self.sql_account_status = crate::localization::format(
            "connections.management.sql_account_saved",
            &[("account_name", &name)],
        );
        cx.notify();
    }

    fn remove_selected_sql_account(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.sql_selected else {
            return;
        };
        if let Err(error) = crate::sql_credentials::delete_password(id) {
            self.sql_account_status = crate::localization::format(
                "connections.management.sql_account_remove_password_failed",
                &[("error", &error)],
            );
            cx.notify();
            return;
        }
        ConnectionStore::global().remove_sql_auth_account(id);
        self.begin_sql_account(window, cx);
        self.reload_sql_accounts();
        self.sql_account_status =
            crate::localization::text("connections.management.sql_account_removed");
        cx.notify();
    }

    fn apply_sql_account_to_connection(
        &mut self,
        id: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Clicking the account that is already chosen releases the credential
        // fields — the only way back to typing a one-off username/password once
        // an account has claimed them.
        if self.connection_sql_account == Some(id) {
            self.connection_sql_account = None;
            self.password
                .update(cx, |field, cx| field.set_value("", window, cx));
            self.status_message =
                crate::localization::text("connections.management.entering_credentials_directly");
            self.mark_dirty();
            cx.notify();
            return;
        }
        let Some(account) = ConnectionStore::global()
            .sql_auth_accounts()
            .into_iter()
            .find(|account| account.id == id)
        else {
            return;
        };
        match crate::sql_credentials::load_password(id) {
            Ok(Some(_)) => {
                self.username = account.username;
                let username = self.username.clone();
                self.username_input
                    .update(cx, |s, cx| s.set_value(username, window, cx));
                self.password.update(cx, |field, cx| {
                    field.set_value(crate::connection_editor::password_mask(), window, cx)
                });
                self.connection_sql_account = Some(id);
                self.status_message = crate::localization::format(
                    "connections.management.using_sql_account",
                    &[("account_name", &account.name)],
                );
                self.mark_dirty();
            }
            Ok(None) => {
                self.status_message = crate::localization::format(
                    "connections.management.sql_account_password_missing",
                    &[("account_name", &account.name)],
                );
            }
            Err(error) => {
                self.status_message = crate::localization::format(
                    "connections.management.sql_account_password_read_failed",
                    &[("error", &error)],
                );
            }
        }
        cx.notify();
    }

    /// Bind (or clear) the Entra account saved into the profile on the next
    /// save for Entra auth modes.
    fn select_entra_binding(
        &mut self,
        binding: Option<EntraAccountBinding>,
        cx: &mut Context<Self>,
    ) {
        self.entra_account = binding;
        self.azure_server_dropdown_open = true;
        self.mark_dirty();
        cx.notify();
    }

    /// Accounts pane: registered Microsoft Entra accounts with add/remove,
    /// mirroring the Swift workspace's Accounts tab (Entra part).
    pub(super) fn render_accounts_pane(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let s = Theme::scaled;
        let accounts = self.entra_accounts.clone();
        let sql_accounts = self.sql_accounts.clone();
        let sql_selected = self.sql_selected;
        let selected = self.entra_selected;
        let status = self.entra_status.clone();
        let sql_status = self.sql_account_status.clone();
        let in_flight = self.entra_in_flight;
        let sql_name = Input::new(&self.sql_account_name_input);
        let sql_username = Input::new(&self.sql_account_username_input);
        let sql_password = Input::new(&self.sql_account_password).mask_toggle();

        let sql_rows: Vec<AnyElement> = if sql_accounts.is_empty() {
            vec![
                div()
                    .px(px(s(12.0)))
                    .py(px(s(10.0)))
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text(
                        "connections.management.no_sql_accounts",
                    ))
                    .into_any_element(),
            ]
        } else {
            sql_accounts
                .into_iter()
                .enumerate()
                .map(|(index, account)| {
                    let id = account.id;
                    div()
                        .id(("cw-sql-account-row", index))
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .gap(px(s(12.0)))
                        .min_h(px(s(38.0)))
                        .px(px(s(12.0)))
                        .cursor(gpui::CursorStyle::PointingHand)
                        .when(sql_selected == Some(id), |row| {
                            row.bg(Theme::accent().opacity(0.25))
                        })
                        .hover(|style| style.bg(Theme::border().opacity(0.4)))
                        .child(
                            div()
                                .text_size(px(Theme::font_size()))
                                .text_color(Theme::text())
                                .child(account.name),
                        )
                        .child(
                            div()
                                .text_size(px(Theme::font_size()))
                                .text_color(Theme::text_dim())
                                .child(account.username),
                        )
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, window, cx| {
                                this.select_sql_account(id, window, cx)
                            }),
                        )
                        .into_any_element()
                })
                .collect()
        };

        let account_rows: Vec<AnyElement> = if accounts.is_empty() {
            vec![
                div()
                    .px(px(s(12.0)))
                    .py(px(s(10.0)))
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text(
                        "connections.management.no_entra_accounts",
                    ))
                    .into_any_element(),
            ]
        } else {
            accounts
                .iter()
                .enumerate()
                .map(|(i, account)| {
                    let is_selected = selected == Some(i);
                    let label = entra_display_label(account);
                    let tenant = account.tenant_id.clone();
                    div()
                        .id(("cw-entra-row", i))
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .gap(px(s(12.0)))
                        .min_h(px(s(38.0)))
                        .px(px(s(12.0)))
                        .cursor(gpui::CursorStyle::PointingHand)
                        .when(is_selected, |d| d.bg(Theme::accent().opacity(0.25)))
                        .hover(|st| st.bg(Theme::border().opacity(0.4)))
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(s(8.0)))
                                .children(
                                    crate::icons::entra_id_icon()
                                        .map(|icon| img(icon).w(px(s(16.0))).h(px(s(16.0)))),
                                )
                                .child(
                                    div()
                                        .text_size(px(Theme::font_size()))
                                        .text_color(Theme::text())
                                        .child(label),
                                ),
                        )
                        .child(
                            div()
                                .text_size(px(Theme::font_size()))
                                .text_color(Theme::text_dim())
                                .child(tenant),
                        )
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, _, cx| {
                                this.entra_selected = Some(i);
                                this.sql_selected = None;
                                cx.notify();
                            }),
                        )
                        .into_any_element()
                })
                .collect()
        };

        let add_button = Self::button(
            crate::localization::text(if in_flight {
                "connections.management.entra_signing_in_button"
            } else {
                "connections.management.add_entra_account_button"
            }),
            !in_flight,
            cx,
            |this, _, _, cx| {
                if !this.entra_in_flight {
                    cx.emit(ConnectionManagementEvent::AddEntraAccountRequested);
                }
            },
        );
        let remove_button = Self::button(
            crate::localization::text("connections.management.remove_entra_account_button"),
            selected.is_some(),
            cx,
            |this, _, _, cx| this.remove_selected_entra_account(cx),
        );
        let selected_entra = selected.and_then(|index| accounts.get(index)).cloned();

        div()
            .flex_1()
            .h_full()
            .min_h_0()
            .flex()
            .flex_row()
            .child(
                div()
                    .w(px(s(280.0)))
                    .h_full()
                    .min_h_0()
                    .flex_none()
                    .flex()
                    .flex_col()
                    .bg(Theme::sidebar_bg())
                    .border_r_1()
                    .border_color(Theme::border())
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .flex()
                            .flex_row()
                            .child(
                                div()
                                    .id("cw-account-list")
                                    .flex_1()
                                    .min_w_0()
                                    .min_h_0()
                                    .overflow_scroll()
                                    .track_scroll(&self.account_list_scroll)
                                    .on_scroll(cx.listener(|_, _, _, cx| cx.notify()))
                                    .py(px(s(6.0)))
                                    .child(
                                        div()
                                            .px(px(s(12.0)))
                                            .py(px(s(5.0)))
                                            .text_size(px(Theme::font_size_ratio(0.85)))
                                            .font_weight(gpui::FontWeight::SEMIBOLD)
                                            .text_color(Theme::text_dim())
                                            .child(crate::localization::text(
                                                "connections.management.entra_list_header",
                                            )),
                                    )
                                    .children(account_rows)
                                    .child(
                                        div()
                                            .px(px(s(12.0)))
                                            .pt(px(s(14.0)))
                                            .pb(px(s(5.0)))
                                            .text_size(px(Theme::font_size_ratio(0.85)))
                                            .font_weight(gpui::FontWeight::SEMIBOLD)
                                            .text_color(Theme::text_dim())
                                            .child(crate::localization::text(
                                                "connections.management.sql_list_header",
                                            )),
                                    )
                                    .children(sql_rows),
                            )
                            .child(Self::vertical_scrollbar(
                                "cw-account-list-scrollbar",
                                &self.account_list_scroll,
                            )),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(s(6.0)))
                            .p(px(s(6.0)))
                            .border_t_1()
                            .border_color(Theme::border())
                            .child(add_button)
                            .child(Self::button(
                                crate::localization::text(
                                    "connections.management.add_sql_account_button",
                                ),
                                true,
                                cx,
                                |this, _, window, cx| this.begin_sql_account(window, cx),
                            )),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .h_full()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .child(if let Some(account) = selected_entra {
                        let label = entra_display_label(&account);
                        div()
                            .id("cw-account-detail")
                            .flex_1()
                            .min_h_0()
                            .overflow_scroll()
                            .p(px(s(16.0)))
                            .flex()
                            .flex_col()
                            .gap(px(s(16.0)))
                            .child(Self::group(
                                &crate::localization::text(
                                    "connections.management.entra_account_detail_section",
                                ),
                                vec![
                                    Self::card_row(
                                        &crate::localization::text(
                                            "connections.management.entra_account_name_label",
                                        ),
                                        div().child(label).into_any_element(),
                                    ),
                                    Self::card_row(
                                        &crate::localization::text(
                                            "connections.management.entra_account_username_label",
                                        ),
                                        div().child(account.username).into_any_element(),
                                    ),
                                    Self::card_row(
                                        &crate::localization::text(
                                            "connections.management.entra_account_tenant_label",
                                        ),
                                        div().child(account.tenant_id).into_any_element(),
                                    ),
                                ],
                            ))
                            .child(
                                div()
                                    .text_size(px(Theme::font_size()))
                                    .text_color(Theme::text_dim())
                                    .child(status),
                            )
                            .into_any_element()
                    } else {
                        div()
                            .id("cw-account-detail")
                            .flex_1()
                            .min_h_0()
                            .overflow_scroll()
                            .p(px(s(16.0)))
                            .flex()
                            .flex_col()
                            .gap(px(s(16.0)))
                            .child(Self::group(
                                &crate::localization::text(if sql_selected.is_some() {
                                    "connections.management.sql_account_detail_section"
                                } else {
                                    "connections.management.new_sql_account_detail_section"
                                }),
                                vec![
                                    Self::card_row(
                                        &crate::localization::text(
                                            "connections.management.sql_account_name_label",
                                        ),
                                        Self::field_control(sql_name),
                                    ),
                                    Self::card_row(
                                        &crate::localization::text(
                                            "connections.management.sql_account_username_label",
                                        ),
                                        Self::field_control(sql_username),
                                    ),
                                    Self::card_row(
                                        &crate::localization::text(
                                            "connections.management.sql_account_password_label",
                                        ),
                                        Self::field_control(sql_password),
                                    ),
                                ],
                            ))
                            .child(
                                div()
                                    .text_size(px(Theme::font_size()))
                                    .text_color(Theme::text_dim())
                                    .child(sql_status),
                            )
                            .into_any_element()
                    })
                    .child(if selected.is_some() {
                        div()
                            .flex()
                            .items_center()
                            .justify_end()
                            .p(px(s(12.0)))
                            .border_t_1()
                            .border_color(Theme::border())
                            .child(remove_button)
                            .into_any_element()
                    } else {
                        div()
                            .flex()
                            .items_center()
                            .justify_end()
                            .gap(px(s(8.0)))
                            .p(px(s(12.0)))
                            .border_t_1()
                            .border_color(Theme::border())
                            .child(Self::button(
                                crate::localization::text(
                                    "connections.management.remove_sql_account_button",
                                ),
                                sql_selected.is_some(),
                                cx,
                                |this, _, window, cx| this.remove_selected_sql_account(window, cx),
                            ))
                            .child(Self::button(
                                crate::localization::text(
                                    "connections.management.save_sql_account_button",
                                ),
                                true,
                                cx,
                                |this, _, window, cx| this.save_sql_account(window, cx),
                            ))
                            .into_any_element()
                    }),
            )
            .into_any_element()
    }

    /// Rows for binding an Entra-auth profile to a registered account.
    pub(super) fn render_entra_binding_rows(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let s = Theme::scaled;
        let accounts = self.entra_accounts.clone();
        let selected = self.entra_account.as_ref().map(|b| b.account_id.clone());

        if accounts.is_empty() {
            return div()
                .px(px(s(12.0)))
                .py(px(s(8.0)))
                .text_size(px(Theme::font_size()))
                .text_color(Theme::text_dim())
                .child(crate::localization::text(
                    "connections.management.no_entra_bindings",
                ))
                .into_any_element();
        }

        div()
            .flex()
            .flex_col()
            .child(
                div()
                    .px(px(s(12.0)))
                    .pt(px(s(6.0)))
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text(
                        "connections.management.binding_account_label",
                    )),
            )
            .children(accounts.into_iter().enumerate().map(|(i, account)| {
                let is_selected = selected.as_deref() == Some(account.id.0.as_str());
                let label = entra_display_label(&account);
                let binding = EntraAccountBinding {
                    account_id: account.id.0.clone(),
                    tenant_id: account.tenant_id.clone(),
                    username: Some(account.username.clone()),
                };
                div()
                    .id(("cw-entra-binding", i))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(8.0)))
                    .mx(px(s(8.0)))
                    .my(px(s(2.0)))
                    .px(px(s(8.0)))
                    .py(px(s(4.0)))
                    .rounded(px(s(4.0)))
                    .cursor(gpui::CursorStyle::PointingHand)
                    .bg(if is_selected {
                        Theme::accent().opacity(0.3)
                    } else {
                        Theme::bg()
                    })
                    .hover(|st| st.bg(Theme::border()))
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text())
                    .child(div().flex_1().child(label))
                    .child(
                        div()
                            .text_size(px(Theme::font_size()))
                            .text_color(Theme::text_dim())
                            .child(account.tenant_id.clone()),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            this.select_entra_binding(Some(binding.clone()), cx);
                        }),
                    )
            }))
            .into_any_element()
    }

    pub(super) fn render_sql_account_choices(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let s = Theme::scaled;
        let accounts = ConnectionStore::global().sql_auth_accounts();
        let selected = self.connection_sql_account;
        if accounts.is_empty() {
            return div()
                .px(px(s(12.0)))
                .py(px(s(6.0)))
                .text_size(px(Theme::font_size()))
                .text_color(Theme::text_dim())
                .child(crate::localization::text(
                    "connections.management.no_saved_sql_accounts",
                ))
                .into_any_element();
        }
        div()
            .px(px(s(12.0)))
            .py(px(s(6.0)))
            .flex()
            .flex_row()
            .flex_wrap()
            .gap(px(s(6.0)))
            .children(accounts.into_iter().enumerate().map(|(index, account)| {
                let id = account.id;
                div()
                    .id(("cm-sql-account-choice", index))
                    .px(px(s(8.0)))
                    .py(px(s(4.0)))
                    .rounded(px(s(4.0)))
                    .border_1()
                    .border_color(if selected == Some(id) {
                        Theme::accent()
                    } else {
                        Theme::border()
                    })
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text())
                    .cursor(gpui::CursorStyle::PointingHand)
                    .hover(|style| style.bg(Theme::border()))
                    .child(account.name)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            this.apply_sql_account_to_connection(id, window, cx)
                        }),
                    )
            }))
            .into_any_element()
    }
}

/// Friendly display label for an Entra account (mirrors the accounts dialog).
fn entra_display_label(account: &EntraAccountProfile) -> String {
    let friendly = account.display_name.as_deref().unwrap_or("").trim();
    if !friendly.is_empty() {
        return friendly.to_string();
    }
    let username = account.username.trim();
    if !username.is_empty() {
        return username.to_string();
    }
    account.id.0.clone()
}

pub(super) fn sole_entra_binding(accounts: &[EntraAccountProfile]) -> Option<EntraAccountBinding> {
    let [account] = accounts else {
        return None;
    };
    Some(EntraAccountBinding {
        account_id: account.id.0.clone(),
        tenant_id: account.tenant_id.clone(),
        username: Some(account.username.clone()),
    })
}
