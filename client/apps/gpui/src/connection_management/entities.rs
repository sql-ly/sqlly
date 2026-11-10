use super::*;

impl ConnectionManagement {
    /// Kind-agnostic rows for the active pane list and detail editor.
    pub(super) fn entity_rows(kind: EntityKind) -> Vec<EntityPaneRow> {
        let store = ConnectionStore::global();
        match kind {
            EntityKind::Client => store
                .clients()
                .into_iter()
                .map(|e| EntityPaneRow {
                    id: e.id,
                    name: e.name,
                    color_hex: e.color_hex,
                    image_path: e.image_path,
                    safety: e.safety,
                })
                .collect(),
            EntityKind::Project => store
                .projects()
                .into_iter()
                .map(|e| EntityPaneRow {
                    id: e.id,
                    name: e.name,
                    color_hex: e.color_hex,
                    image_path: e.image_path,
                    safety: e.safety,
                })
                .collect(),
            EntityKind::Environment => store
                .environments()
                .into_iter()
                .map(|e| EntityPaneRow {
                    id: e.id,
                    name: e.name,
                    color_hex: e.color_hex,
                    image_path: e.image_path,
                    safety: e.safety,
                })
                .collect(),
        }
    }

    pub(super) fn clear_entity_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.entity_selected = None;
        self.entity_name = String::new();
        self.entity_color = String::new();
        self.entity_image_path = String::new();
        self.entity_safety = SafetyFeatures::default();
        self.sync_entity_editor_inputs(window, cx);
    }

    pub(super) fn select_entity(
        &mut self,
        kind: EntityKind,
        id: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(row) = Self::entity_rows(kind).into_iter().find(|row| row.id == id) {
            self.entity_selected = Some(id);
            self.entity_name = row.name;
            self.entity_color = row.color_hex.unwrap_or_default();
            self.entity_image_path = row.image_path.unwrap_or_default();
            self.entity_safety = row.safety;
            self.entity_status = String::new();
            self.sync_entity_editor_inputs(window, cx);
        }
    }

    /// Push the canonical entity-editor values into the name input and color
    /// picker states.
    fn sync_entity_editor_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.entity_name.clone();
        self.entity_name_input
            .update(cx, |s, cx| s.set_value(name, window, cx));
        let color =
            crate::theme::parse_hex_color(self.entity_color.trim()).unwrap_or(Theme::text_dim());
        self.entity_color_picker
            .update(cx, |s, cx| s.set_value(color, window, cx));
    }

    fn add_entity(&mut self, kind: EntityKind, window: &mut Window, cx: &mut Context<Self>) {
        if crate::connection::connections_are_proxied() {
            self.entity_status =
                crate::localization::text("connections.management.proxy_entity_add_read_only");
            cx.notify();
            return;
        }
        self.clear_entity_form(window, cx);
        self.entity_name = crate::localization::text(match kind {
            EntityKind::Client => "connections.management.new_client",
            EntityKind::Project => "connections.management.new_project",
            EntityKind::Environment => "connections.management.new_environment",
        });
        let name = self.entity_name.clone();
        self.entity_name_input
            .update(cx, |s, cx| s.set_value(name, window, cx));
        self.entity_status = crate::localization::text(match kind {
            EntityKind::Client => "connections.management.new_client_status",
            EntityKind::Project => "connections.management.new_project_status",
            EntityKind::Environment => "connections.management.new_environment_status",
        });
        cx.notify();
    }

    fn save_entity(&mut self, kind: EntityKind, window: &mut Window, cx: &mut Context<Self>) {
        if crate::connection::connections_are_proxied() {
            self.entity_status =
                crate::localization::text("connections.management.proxy_entity_save_read_only");
            cx.notify();
            return;
        }
        let name = self.entity_name.trim().to_string();
        if name.is_empty() {
            self.entity_status =
                crate::localization::text("connections.management.entity_name_required");
            cx.notify();
            return;
        }
        let id = self.entity_selected.unwrap_or_else(Uuid::new_v4);
        let color_hex = {
            let c = self.entity_color.trim().to_string();
            if c.is_empty() { None } else { Some(c) }
        };
        let image_path = {
            let path = self.entity_image_path.trim().to_string();
            if path.is_empty() { None } else { Some(path) }
        };
        let safety = self.entity_safety.clone();
        let store = ConnectionStore::global();
        match kind {
            EntityKind::Client => store.upsert_client(ClientEntity {
                id,
                name: name.clone(),
                color_hex,
                image_path,
                safety,
            }),
            EntityKind::Project => store.upsert_project(ProjectEntity {
                id,
                name: name.clone(),
                color_hex,
                image_path,
                safety,
            }),
            EntityKind::Environment => store.upsert_environment(EnvironmentEntity {
                id,
                name: name.clone(),
                color_hex,
                image_path,
                safety,
            }),
        }
        self.entity_selected = Some(id);
        self.entity_status = crate::localization::format(
            "connections.management.entity_saved",
            &[("entity_name", &name)],
        );
        // Entity names may have changed — keep the Connections pane's
        // client / project selects in sync.
        self.sync_entity_selects(window, cx);
        cx.emit(ConnectionManagementEvent::EntitiesChanged);
        cx.notify();
    }

    fn set_entity_icon_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if !is_supported_icon_path(&path) {
            self.entity_status =
                crate::localization::text("connections.management.unsupported_icon");
            cx.notify();
            return;
        }
        self.entity_image_path = path.to_string_lossy().into_owned();
        self.entity_status = crate::localization::format(
            "connections.management.icon_selected",
            &[("path", &path.display().to_string())],
        );
        cx.notify();
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn browse_entity_icon(&mut self, cx: &mut Context<Self>) {
        if let Some(path) = rfd::FileDialog::new()
            .add_filter(
                crate::localization::text("connections.management.image_file_filter"),
                &["png", "jpg", "jpeg", "gif", "svg", "ico", "bmp", "webp"],
            )
            .set_title(crate::localization::text(
                "connections.management.select_icon_title",
            ))
            .pick_file()
        {
            self.set_entity_icon_path(path, cx);
        }
    }

    /// Browser build: there is no native file dialog; report it in the same
    /// status line the icon picker already uses.
    #[cfg(target_arch = "wasm32")]
    fn browse_entity_icon(&mut self, cx: &mut Context<Self>) {
        self.entity_status =
            crate::localization::text("connections.management.browser_icon_picker_unavailable");
        cx.notify();
    }

    fn remove_entity(&mut self, kind: EntityKind, window: &mut Window, cx: &mut Context<Self>) {
        if crate::connection::connections_are_proxied() {
            self.entity_status =
                crate::localization::text("connections.management.proxy_entity_remove_read_only");
            cx.notify();
            return;
        }
        let Some(id) = self.entity_selected else {
            self.entity_status = crate::localization::text(match kind {
                EntityKind::Client => "connections.management.select_client_to_remove",
                EntityKind::Project => "connections.management.select_project_to_remove",
                EntityKind::Environment => "connections.management.select_environment_to_remove",
            });
            cx.notify();
            return;
        };
        let store = ConnectionStore::global();
        match kind {
            EntityKind::Client => store.remove_client(id),
            EntityKind::Project => store.remove_project(id),
            EntityKind::Environment => store.remove_environment(id),
        }
        let first = Self::entity_rows(kind).first().map(|row| row.id);
        match first {
            Some(next) => self.select_entity(kind, next, window, cx),
            None => self.clear_entity_form(window, cx),
        }
        self.entity_status = crate::localization::text("connections.management.entity_removed");
        self.sync_entity_selects(window, cx);
        cx.emit(ConnectionManagementEvent::EntitiesChanged);
        cx.notify();
    }

    fn set_entity_safety(&mut self, field: SafetyField, value: bool, cx: &mut Context<Self>) {
        safety_set(&mut self.entity_safety, field, value);
        cx.notify();
    }

    /// Safety flags forced on by the entities this connection is assigned to
    /// (client / project / environment by name), mirroring Swift's
    /// server-inherits-entity-safety force-down. Returns `(flag label,
    /// source entity name)` pairs, deduped by flag.
    pub(super) fn inherited_safety(&self) -> Vec<(String, String)> {
        let store = ConnectionStore::global();
        let mut sources: Vec<(String, SafetyFeatures)> = Vec::new();
        let client = self.client.trim();
        if !client.is_empty()
            && let Some(e) = store
                .clients()
                .into_iter()
                .find(|e| e.name.eq_ignore_ascii_case(client))
        {
            sources.push((e.name, e.safety));
        }
        let project = self.project.trim();
        if !project.is_empty()
            && let Some(e) = store
                .projects()
                .into_iter()
                .find(|e| e.name.eq_ignore_ascii_case(project))
        {
            sources.push((e.name, e.safety));
        }
        let environments = store.environments();
        if let Some(e) = self.environment.find_entity(&environments, |e| &e.name) {
            sources.push((e.name.clone(), e.safety.clone()));
        }

        let mut out: Vec<(String, String)> = Vec::new();
        for field in SafetyField::all() {
            for (name, safety) in &sources {
                if safety_get(safety, *field) {
                    out.push((field.label(), name.clone()));
                    break;
                }
            }
        }
        out
    }

    // ----- rendering helpers -----

    /// Entity pane (Clients / Projects / Environments): list + grouped-card
    /// editor with name, color, and the full 8-flag safety group.
    pub(super) fn render_entity_pane(
        &mut self,
        kind: EntityKind,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // Labels come from the relay proxy's credential file while proxied.
        let proxied = crate::connection::connections_are_proxied();
        let s = Theme::scaled;
        let rows = Self::entity_rows(kind);
        let selected = self.entity_selected;
        let status = self.entity_status.clone();
        let this = cx.entity();

        let list_rows: Vec<AnyElement> = rows
            .iter()
            .enumerate()
            .map(|(i, row)| {
                let id = row.id;
                let is_selected = selected == Some(id);
                let dot_color = row
                    .color_hex
                    .as_deref()
                    .and_then(crate::theme::parse_hex_color)
                    .unwrap_or(Theme::text_dim());
                div()
                    .id(("cw-entity-row", i))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(6.0)))
                    .h(px(s(26.0)))
                    .px(px(s(8.0)))
                    .rounded(px(s(4.0)))
                    .cursor(gpui::CursorStyle::PointingHand)
                    .bg(if is_selected {
                        Theme::accent().opacity(0.3)
                    } else {
                        Theme::sidebar_bg()
                    })
                    .hover(|st| st.bg(Theme::border()))
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text())
                    .child(
                        if let Some(path) = row
                            .image_path
                            .as_ref()
                            .filter(|path| Path::new(path.as_str()).is_file())
                        {
                            img(PathBuf::from(path))
                                .w(px(s(16.0)))
                                .h(px(s(16.0)))
                                .into_any_element()
                        } else {
                            div()
                                .w(px(s(8.0)))
                                .h(px(s(8.0)))
                                .mx(px(s(4.0)))
                                .flex_none()
                                .rounded(px(s(4.0)))
                                .bg(dot_color)
                                .into_any_element()
                        },
                    )
                    .child(div().flex_1().child(row.name.clone()))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            this.select_entity(kind, id, window, cx);
                            cx.notify();
                        }),
                    )
                    .into_any_element()
            })
            .collect();

        let list_pane = div()
            .w(px(s(220.0)))
            .h_full()
            .min_h_0()
            .flex_none()
            .flex()
            .flex_col()
            .bg(Theme::sidebar_bg())
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_row()
                    .child(
                        div()
                            .id("cw-entity-list")
                            .flex_1()
                            .min_w_0()
                            .min_h_0()
                            .overflow_scroll()
                            .track_scroll(&self.entity_list_scroll)
                            .on_scroll(cx.listener(|_, _, _, cx| cx.notify()))
                            .p(px(s(6.0)))
                            .child(if list_rows.is_empty() {
                                div()
                                    .p(px(s(8.0)))
                                    .text_size(px(Theme::font_size()))
                                    .text_color(Theme::text_dim())
                                    .child(crate::localization::text(match kind {
                                        EntityKind::Client => {
                                            "connections.management.empty_clients"
                                        }
                                        EntityKind::Project => {
                                            "connections.management.empty_projects"
                                        }
                                        EntityKind::Environment => {
                                            "connections.management.empty_environments"
                                        }
                                    }))
                                    .into_any_element()
                            } else {
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(px(s(2.0)))
                                    .children(list_rows)
                                    .into_any_element()
                            }),
                    )
                    .child(Self::vertical_scrollbar(
                        "cw-entity-list-scrollbar",
                        &self.entity_list_scroll,
                    )),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(4.0)))
                    .p(px(s(6.0)))
                    .border_t_1()
                    .border_color(Theme::border())
                    .child(Self::list_footer_button(
                        "cw-entity-add",
                        "+",
                        !proxied,
                        cx,
                        move |this, _, window, cx| this.add_entity(kind, window, cx),
                    ))
                    .child(Self::list_footer_button(
                        "cw-entity-remove",
                        "\u{2212}",
                        selected.is_some() && !proxied,
                        cx,
                        move |this, _, window, cx| this.remove_entity(kind, window, cx),
                    )),
            );

        let name_input = Input::new(&self.entity_name_input);
        let color_hex = self.entity_color.clone();
        let image_path = self.entity_image_path.clone();

        let safety = self.entity_safety.clone();
        let safety_rows: Vec<AnyElement> = SafetyField::all()
            .iter()
            .enumerate()
            .map(|(i, field)| {
                let field = *field;
                let checked = safety_get(&safety, field);
                let this = this.clone();
                Self::card_row(
                    &field.label(),
                    Checkbox::new(("cw-entity-safety", i))
                        .checked(checked)
                        .tab_stop(true)
                        .on_click(move |value: &bool, _window, app| {
                            let value = *value;
                            this.update(app, |this, cx| this.set_entity_safety(field, value, cx));
                        })
                        .into_any_element(),
                )
            })
            .collect();

        let editor = div()
            .id("cw-entity-editor")
            .flex_1()
            .min_h_0()
            .overflow_scroll()
            .p(px(s(16.0)))
            .flex()
            .flex_col()
            .gap(px(s(16.0)))
            .child(Self::group(
                &kind.singular(),
                vec![Self::card_row(
                    &crate::localization::text("connections.management.entity_name_label"),
                    Self::field_control(name_input),
                )],
            ))
            .child(Self::group(
                &crate::localization::text("connections.management.appearance_section"),
                vec![
                    Self::card_row(
                        &crate::localization::text("connections.management.color_label"),
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(s(8.0)))
                            .child(ColorPicker::new(&self.entity_color_picker))
                            .child(
                                div()
                                    .w(px(s(82.0)))
                                    .text_size(px(Theme::font_size()))
                                    .text_color(Theme::text_dim())
                                    .child(if color_hex.is_empty() {
                                        crate::localization::text("connections.management.no_color")
                                    } else {
                                        color_hex
                                    }),
                            )
                            .into_any_element(),
                    ),
                    Self::card_row(
                        &crate::localization::text("connections.management.icon_label"),
                        div()
                            .id("cw-entity-icon-drop-zone")
                            .w_full()
                            .min_h(px(s(76.0)))
                            .px(px(s(12.0)))
                            .py(px(s(10.0)))
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(s(12.0)))
                            .border_1()
                            .border_color(Theme::border())
                            .rounded(px(s(6.0)))
                            .drag_over::<ExternalPaths>(|style, _, _, _| {
                                style
                                    .bg(Theme::accent().opacity(0.16))
                                    .border_color(Theme::accent())
                            })
                            .child(
                                if !image_path.is_empty()
                                    && Path::new(image_path.as_str()).is_file()
                                {
                                    img(PathBuf::from(&image_path))
                                        .w(px(s(48.0)))
                                        .h(px(s(48.0)))
                                        .into_any_element()
                                } else {
                                    div()
                                        .w(px(s(48.0)))
                                        .h(px(s(48.0)))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .rounded(px(s(6.0)))
                                        .bg(Theme::sidebar_bg())
                                        .border_1()
                                        .border_color(Theme::border())
                                        // Fixed glyph: drawn to fit its tile.
                                        .text_size(px(s(20.0)))
                                        .text_color(Theme::text_dim())
                                        .child("+")
                                        .into_any_element()
                                },
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .flex_col()
                                    .gap(px(s(4.0)))
                                    .child(
                                        div()
                                            .text_size(px(Theme::font_size()))
                                            .text_color(Theme::text())
                                            .child(crate::localization::text(
                                                "connections.management.drop_icon_hint",
                                            )),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(Theme::font_size_ratio(0.85)))
                                            .text_color(Theme::text_dim())
                                            .child(if image_path.is_empty() {
                                                crate::localization::text(
                                                    "connections.management.supported_icon_formats",
                                                )
                                            } else {
                                                image_path.clone()
                                            }),
                                    ),
                            )
                            .child(Self::button(
                                crate::localization::text(
                                    "connections.management.choose_file_button",
                                ),
                                true,
                                cx,
                                |this, _, _, cx| this.browse_entity_icon(cx),
                            ))
                            .on_drop::<ExternalPaths>({
                                let this = this.clone();
                                move |paths, _window, app| {
                                    if let Some(path) = paths.paths().first().cloned() {
                                        this.update(app, |this, cx| {
                                            this.set_entity_icon_path(path, cx)
                                        });
                                    }
                                }
                            })
                            .into_any_element(),
                    ),
                ],
            ))
            .child(Self::group(
                &crate::localization::text("connections.management.safety_features_section"),
                safety_rows,
            ));

        let footer = div()
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
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text_dim())
                    .child(status),
            )
            .child(Self::button(
                crate::localization::text("connections.management.save_entity_button"),
                true,
                cx,
                move |this, _, window, cx| this.save_entity(kind, window, cx),
            ));

        div()
            .flex_1()
            .h_full()
            .min_h_0()
            .flex()
            .flex_row()
            .child(list_pane)
            .child(
                div()
                    .flex_1()
                    .h_full()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .child(editor)
                    .child(footer),
            )
            .into_any_element()
    }
}

pub(super) fn is_supported_icon_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "gif" | "svg" | "ico" | "bmp" | "webp"
            )
        })
}

/// Read one [`SafetyField`] flag out of a [`SafetyFeatures`].
fn safety_get(s: &SafetyFeatures, field: SafetyField) -> bool {
    match field {
        SafetyField::IsProduction => s.is_production,
        SafetyField::OuterTransactionRollback => s.use_outer_transaction_rollback_scope,
        SafetyField::OnlyAllowSelect => s.only_allow_select,
        SafetyField::ConfirmationDialog => s.use_confirmation_dialog,
        SafetyField::RequireAiReview => s.require_ai_review_for_data_changes,
        SafetyField::AllowUnconstrainedUpdate => s.allow_unconstrained_update,
        SafetyField::AllowUnconstrainedDelete => s.allow_unconstrained_delete,
        SafetyField::RequireTouchIdToConnect => s.require_touch_id_to_connect,
        SafetyField::RequireTouchIdForNonSelect => s.require_touch_id_for_non_select,
    }
}

/// Write one [`SafetyField`] flag into a [`SafetyFeatures`].
fn safety_set(s: &mut SafetyFeatures, field: SafetyField, value: bool) {
    match field {
        SafetyField::IsProduction => s.is_production = value,
        SafetyField::OuterTransactionRollback => s.use_outer_transaction_rollback_scope = value,
        SafetyField::OnlyAllowSelect => s.only_allow_select = value,
        SafetyField::ConfirmationDialog => s.use_confirmation_dialog = value,
        SafetyField::RequireAiReview => s.require_ai_review_for_data_changes = value,
        SafetyField::AllowUnconstrainedUpdate => s.allow_unconstrained_update = value,
        SafetyField::AllowUnconstrainedDelete => s.allow_unconstrained_delete = value,
        SafetyField::RequireTouchIdToConnect => s.require_touch_id_to_connect = value,
        SafetyField::RequireTouchIdForNonSelect => s.require_touch_id_for_non_select = value,
    }
}
