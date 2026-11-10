//! Explorer power actions: user-template Custom Actions (#13), multi-select
//! bulk operations (#14), and the Deploy Cart entry points (#15).
//!
//! Review-first like every other explorer write: templates open in a tab on
//! the clicked object's connection unless the template itself opted into Run,
//! and even then anything but read-only SQL is shown for confirmation first
//! (with every execute gate still applied); bulk scripts open one tab per
//! source database; the cart only collects and generates, never executes.

use super::*;
use crate::bulk_actions::{self, BulkObjectGroup};
use crate::schema_tree::{BulkObjectRef, BulkTreeAction};

/// The `{schema}` / `{object}` values a node supplies to a template, from its
/// kind (the context's own `schema`/`name` fields are not filled uniformly
/// across node kinds).
pub(super) fn template_object_context(
    context: &ExplorerActionContext,
    server: String,
) -> sqlly_services::TemplateObjectContext {
    let (schema, object) = match &context.kind {
        NodeKind::Table { schema, name }
        | NodeKind::View { schema, name }
        | NodeKind::Procedure { schema, name }
        | NodeKind::Function { schema, name } => (schema.clone(), name.clone()),
        _ => (
            context.schema.clone().unwrap_or_default(),
            context.name.clone().unwrap_or_default(),
        ),
    };
    sqlly_services::TemplateObjectContext {
        server,
        database: context
            .database
            .clone()
            .or_else(|| context.profile.as_ref().and_then(|p| p.database.clone()))
            .unwrap_or_default(),
        schema,
        object,
        kind: crate::schema_tree::explorer_object_kind(&context.kind),
    }
}

#[cfg(not(target_arch = "wasm32"))]
/// The Deploy Cart item a node collects as, or `None` for kinds a cart cannot
/// hold (or a node without a server identity).
pub(super) fn deploy_cart_item(
    context: &ExplorerActionContext,
    server_name: String,
    include_data: bool,
) -> Option<sqlly_services::DeployCartItem> {
    let kind = crate::deploy_cart::deploy_object_kind(&context.kind)?;
    let server_id = context.server_id?;
    let ctx = template_object_context(context, server_name.clone());
    Some(sqlly_services::DeployCartItem {
        server_id,
        server_name,
        database: ctx.database,
        schema: ctx.schema,
        name: ctx.object,
        kind,
        // Only tables carry rows.
        include_data: include_data && kind == sqlly_services::DeployObjectKind::Table,
    })
}

/// Whether a Run-mode custom action may execute without showing its SQL
/// first: only when every statement is read-only (SELECT-only, and nothing the
/// engine's read-only screen flags). Anything else opens the review dialog,
/// whatever the connection's mutation-review setting.
pub(super) fn custom_action_runs_unconfirmed(db: sqlly_core::DatabaseType, sql: &str) -> bool {
    if db.is_command_engine() {
        return !sqlly_intel::query::safety::redis::has_write_command(sql);
    }
    sqlly_intel::query::safety::classify_select_only_for(db, sql).select_only
        && sqlly_intel::query::safety::read_only_violation(db, sql).is_none()
}

/// Whether a custom-action template opted into Run (execute) mode.
fn template_runs(template: &sqlly_services::ScriptTemplate) -> bool {
    template
        .explorer_action
        .as_ref()
        .is_some_and(|spec| spec.mode == sqlly_services::ExplorerActionMode::Run)
}

/// The parameter prompt's "where" line: server, database, and the object the
/// action was invoked on, skipping any part that is empty.
pub(super) fn custom_action_target_label(
    server: &str,
    ctx: &sqlly_services::TemplateObjectContext,
) -> String {
    let object = match (ctx.schema.as_str(), ctx.object.as_str()) {
        (_, "") => String::new(),
        ("", object) => object.to_string(),
        (schema, object) => format!("{schema}.{object}"),
    };
    let parts: Vec<&str> = [server, ctx.database.as_str(), object.as_str()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect();
    if parts.is_empty() {
        crate::localization::text("shell.custom_actions.active_connection")
    } else {
        parts.join(" \u{b7} ")
    }
}

impl AppView {
    /// Hide Schema / Show Hidden Schemas: change the saved connection's
    /// explorer-only hidden schemas for the clicked database, keep the active
    /// copy in step, and update the tree (hiding prunes in place, showing
    /// reloads the database's level).
    pub(super) fn handle_hidden_schema_action(
        &mut self,
        action: &ExplorerMenuAction,
        context: &ExplorerActionContext,
        cx: &mut Context<Self>,
    ) {
        let (Some(server_id), Some(database)) = (context.server_id, context.database.clone())
        else {
            return;
        };
        let store = crate::connection::ConnectionStore::global();
        let Some(mut saved) = store.get(server_id) else {
            self.set_status(
                StatusLevel::Error,
                crate::localization::text("shell.explorer.hidden_schemas_save_first"),
            );
            return;
        };
        let hiding = match action {
            ExplorerMenuAction::HideSchema => {
                let Some(schema) = context.schema.clone() else {
                    return;
                };
                saved.hide_schema(&database, &schema);
                Some(schema)
            }
            ExplorerMenuAction::ShowHiddenSchemas => None,
            _ => return,
        };
        let shown = if hiding.is_none() {
            saved.show_hidden_schemas(&database)
        } else {
            0
        };
        let hidden_schemas = saved.hidden_schemas.clone();
        store.update(server_id, saved);
        if let Some(active) = self.active_profile.as_mut().filter(|p| p.id.0 == server_id) {
            active.hidden_schemas = hidden_schemas;
        }
        // A schema folder's id is its database's id plus `:sch:{schema}`.
        let db_node = context
            .node_id
            .rsplit_once(":sch:")
            .map_or(context.node_id.clone(), |(db, _)| db.to_string());
        let message = match &hiding {
            Some(schema) => crate::localization::format(
                "shell.explorer.schema_hidden",
                &[("schema", schema), ("database", &database)],
            ),
            None => crate::localization::format(
                "shell.explorer.hidden_schemas_shown",
                &[("count", &shown.to_string()), ("database", &database)],
            ),
        };
        self.sidebar.update(cx, |s, cx| {
            s.refresh_connections(cx);
            if hiding.is_some() {
                s.apply_hidden_schemas(server_id, &database, cx);
            } else {
                s.refresh_schema_node(&db_node, cx);
            }
        });
        self.set_status(StatusLevel::Success, message);
        cx.notify();
    }

    /// The connection display name for a server id, for `{server}` and cart
    /// grouping headers.
    fn server_display_name(&self, server_id: Option<uuid::Uuid>, cx: &Context<Self>) -> String {
        server_id
            .and_then(|id| self.resolve_explorer_target(Some(&id), None, cx))
            .map(|p| p.name)
            .unwrap_or_default()
    }

    /// Custom Actions ▸ <template>: render the user template against the
    /// clicked object and open it (Script) or open-and-run it (Run) on the
    /// object's own connection and database. A template with
    /// `<name,type,default>` parameters first asks for their values in a
    /// small prompt (prefilled with the defaults); the action continues from
    /// [`Self::handle_template_params_event`] once the user confirms.
    pub(super) fn handle_run_template_action(
        &mut self,
        context: &ExplorerActionContext,
        name: &str,
        cx: &mut Context<Self>,
    ) {
        // Re-resolve by name at click time so an edit since the menu opened
        // can never fire a stale body.
        let Some(template) = self.resolve_custom_action_template(name, cx) else {
            return;
        };
        if template.parameters.is_empty() {
            self.run_template_action(context, &template, &HashMap::new(), cx);
            return;
        }
        let run = template_runs(&template);
        let server = self.server_display_name(context.server_id, cx);
        let object_ctx = template_object_context(context, server.clone());
        let target = custom_action_target_label(&server, &object_ctx);
        let dialog = cx
            .new(|_cx| crate::template_explorer::TemplateParamsDialog::new(&template, target, run));
        cx.subscribe(&dialog, Self::handle_template_params_event)
            .detach();
        self.overlays.template_params_dialog = Some((dialog, context.clone(), template.name));
        cx.notify();
    }

    /// The parameter prompt's answer: render and dispatch the action with the
    /// typed values, or just close on cancel.
    pub(super) fn handle_template_params_event(
        &mut self,
        dialog: Entity<crate::template_explorer::TemplateParamsDialog>,
        event: &crate::template_explorer::TemplateParamsEvent,
        cx: &mut Context<Self>,
    ) {
        // A stale prompt (replaced by a newer one) must not fire its action.
        let Some((open, context, name)) = self.overlays.template_params_dialog.take() else {
            return;
        };
        if open != dialog {
            self.overlays.template_params_dialog = Some((open, context, name));
            return;
        }
        self.restore_editor_focus(cx);
        if let crate::template_explorer::TemplateParamsEvent::Submit(values) = event
            && let Some(template) = self.resolve_custom_action_template(&name, cx)
        {
            self.run_template_action(&context, &template, values, cx);
        }
        cx.notify();
    }

    /// The named user template, looked up in the (cached) user catalog. A
    /// missing template or an unreadable catalog is reported on the status
    /// bar.
    fn resolve_custom_action_template(
        &mut self,
        name: &str,
        cx: &mut Context<Self>,
    ) -> Option<sqlly_services::ScriptTemplate> {
        let templates = match crate::template_explorer::try_load_user_templates() {
            Ok(templates) => templates,
            Err(error) => {
                self.set_status(StatusLevel::Error, error);
                cx.notify();
                return None;
            }
        };
        let found = templates.iter().find(|t| t.name == name).cloned();
        if found.is_none() {
            self.set_status(
                StatusLevel::Warning,
                crate::localization::format(
                    "shell.custom_actions.template_missing",
                    &[("name", name)],
                ),
            );
            cx.notify();
        }
        found
    }

    /// Render `template` against the clicked object with `values` for its
    /// SSMS-style parameters (defaults for any not supplied) and script or
    /// run it on the object's own connection and database.
    fn run_template_action(
        &mut self,
        context: &ExplorerActionContext,
        template: &sqlly_services::ScriptTemplate,
        values: &HashMap<String, String>,
        cx: &mut Context<Self>,
    ) {
        let server = self.server_display_name(context.server_id, cx);
        let object_ctx = template_object_context(context, server);
        let run = template_runs(template);
        let target = self.resolve_explorer_target(
            context.server_id.as_ref(),
            context.database.as_deref(),
            cx,
        );
        // Identifiers are quoted in the dialect of the connection the SQL
        // will run on.
        let Some(db) = target
            .as_ref()
            .or(self.active_profile.as_ref())
            .map(|p| p.database_type)
        else {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.custom_actions.connect"),
            );
            cx.notify();
            return;
        };
        let sql = template.render_for_object(values, &object_ctx, db);
        let label = template.name.clone();
        self.with_explorer_target_active(target, cx, move |app, cx| {
            app.new_tab(cx, Some(sql.clone()));
            let tab_idx = app.active_tab;
            if !run {
                app.set_status(
                    StatusLevel::Info,
                    crate::localization::format(
                        "shell.custom_actions.scripted",
                        &[("name", &label)],
                    ),
                );
            } else if custom_action_runs_unconfirmed(db, &sql) {
                app.execute_query(tab_idx, cx);
                app.set_status(
                    StatusLevel::Info,
                    crate::localization::format(
                        "shell.custom_actions.running",
                        &[("name", &label)],
                    ),
                );
            } else {
                app.arm_custom_action_review(tab_idx, sql, &label, cx);
            }
            cx.notify();
        });
    }

    /// Show a Run-mode custom action's SQL in the review dialog before it
    /// executes. When the normal run gate would already review, block, or
    /// refuse this SQL (production lock, tenant scope, a pending review), the
    /// gate handles it instead — confirming here must never bypass it.
    fn arm_custom_action_review(
        &mut self,
        tab_idx: usize,
        sql: String,
        label: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(tab_id) = self.tabs.get(tab_idx).map(|t| t.id.clone()) else {
            return;
        };
        let decision = super::execute::run_gate_decision(
            &sql,
            &super::execute::ExecuteOrigin::UserRun,
            &tab_id,
            self.pending.mutation_review.as_ref(),
            self.active_profile.as_ref(),
            None,
            ConnectionStore::global(),
            self.production_unlock.as_ref(),
            sqlly_intel::query::safety::Instant::now(),
        );
        if decision != super::execute::RunGateDecision::Proceed {
            self.execute_query(tab_idx, cx);
            return;
        }
        self.mutation_review_generation += 1;
        self.pending.mutation_review = Some(super::pending_dialogs::PendingMutationReview {
            sql,
            tab_id,
            bottom_line: crate::localization::format(
                "shell.custom_actions.review_detail",
                &[("name", label)],
            ),
            has_unbounded: false,
            ai_review_required: false,
            production: false,
            ai_explanation: None,
            ai_generation: self.mutation_review_generation,
            notebook_cell: None,
            multi_targets: None,
        });
        self.set_status(
            StatusLevel::Info,
            crate::localization::format("shell.custom_actions.review_status", &[("name", label)]),
        );
        cx.notify();
    }

    /// Deploy Cart ▸ Add: append the node to the active cart and persist.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn add_to_deploy_cart(
        &mut self,
        context: &ExplorerActionContext,
        include_data: bool,
        cx: &mut Context<Self>,
    ) {
        let server_name = self.server_display_name(context.server_id, cx);
        let Some(item) = deploy_cart_item(context, server_name, include_data) else {
            self.set_status(
                StatusLevel::Warning,
                crate::localization::text("shell.deploy_cart.unsupported_object"),
            );
            cx.notify();
            return;
        };
        let label = item.label();
        match crate::deploy_cart::add_item_to_active_cart(item, cx) {
            Ok((cart, outcome)) => {
                let message = match outcome {
                    sqlly_services::AddOutcome::Added => crate::localization::format(
                        "shell.deploy_cart.added",
                        &[("object", &label), ("cart", &cart)],
                    ),
                    sqlly_services::AddOutcome::UpgradedWithData => crate::localization::format(
                        "shell.deploy_cart.data_included",
                        &[("object", &label), ("cart", &cart)],
                    ),
                    sqlly_services::AddOutcome::Duplicate => crate::localization::format(
                        "shell.deploy_cart.duplicate",
                        &[("object", &label), ("cart", &cart)],
                    ),
                };
                self.set_status(StatusLevel::Success, message);
            }
            Err(error) => {
                self.set_status(
                    StatusLevel::Error,
                    crate::localization::format("shell.deploy_cart.failed", &[("error", &error)]),
                );
            }
        }
        cx.notify();
    }

    /// Open (or raise) the Deploy Cart window.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn open_deploy_cart(&mut self, cx: &mut Context<Self>) {
        let router = self.service_router.clone();
        let weak = cx.weak_entity();
        // Opening a window renders into it at once; never inside our update.
        cx.defer(move |cx| {
            crate::deploy_cart::open_window(router, weak, cx);
        });
    }

    /// Route a Deploy Cart window event into the app (scripts to tabs, status).
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn handle_deploy_cart_event(
        &mut self,
        event: &crate::tool_dialog::ToolDialogEvent,
        cx: &mut Context<Self>,
    ) {
        // The window closes itself; Close needs no overlay bookkeeping here.
        if !matches!(event, crate::tool_dialog::ToolDialogEvent::Close) {
            self.apply_tool_dialog_event(event, cx);
        }
    }

    /// The multi-selection banner's actions, one reviewed script (or export
    /// handoff) per source database.
    pub(super) fn handle_bulk_tree_action(
        &mut self,
        action: BulkTreeAction,
        objects: Vec<BulkObjectRef>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let groups = bulk_actions::group_objects(&objects);
        if groups.is_empty() {
            return;
        }
        match action {
            BulkTreeAction::ScriptCreate | BulkTreeAction::ScriptDrop => {
                for group in groups {
                    self.bulk_script_group(action, group, cx);
                }
            }
            BulkTreeAction::Maintenance { op } => {
                for group in groups {
                    self.bulk_maintenance_group(op, group, cx);
                }
            }
            BulkTreeAction::ExportData => {
                // The export dialog is single-database; hand it the first
                // group and say so when the selection spanned more.
                let skipped = groups.len() - 1;
                let group = groups.into_iter().next().expect("non-empty");
                self.bulk_export_group(group, window, cx);
                if skipped > 0 {
                    self.set_status(
                        StatusLevel::Warning,
                        crate::localization::format(
                            "shell.bulk_actions.export_databases_skipped",
                            &[("count", &skipped.to_string())],
                        ),
                    );
                    cx.notify();
                }
            }
        }
    }

    fn bulk_group_profile(
        &mut self,
        group: &BulkObjectGroup,
        cx: &mut Context<Self>,
    ) -> Option<ConnectionProfile> {
        // Only a selection with no server identity may fall back to the
        // active connection. A known server that no longer resolves (the
        // connection was deleted) must never be scripted on some other one.
        let profile = match group.server_id {
            Some(_) => self.resolve_explorer_target(
                group.server_id.as_ref(),
                group.database.as_deref(),
                cx,
            ),
            None => self.active_profile.clone(),
        };
        if profile.is_none() {
            self.set_status(
                StatusLevel::Info,
                if group.server_id.is_some() {
                    crate::localization::text("shell.bulk_actions.connection_missing")
                } else {
                    crate::localization::text("shell.bulk_actions.connect_script")
                },
            );
            cx.notify();
        }
        profile
    }

    fn bulk_script_group(
        &mut self,
        action: BulkTreeAction,
        group: BulkObjectGroup,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self.bulk_group_profile(&group, cx) else {
            return;
        };
        let database = group
            .database
            .clone()
            .or_else(|| profile.database.clone())
            .unwrap_or_default();
        let objects = bulk_actions::script_objects(&group);
        let services = self.services_for_profile(&profile);
        let query_profile = profile.clone();
        let drop = matches!(action, BulkTreeAction::ScriptDrop);
        let verb = if drop { "DROP" } else { "CREATE" };
        let ddl_options = crate::preferences::DdlNavigationPreferences::load().ddl_options();
        self.set_status(
            StatusLevel::Busy,
            crate::localization::format(
                "shell.bulk_actions.scripting",
                &[("verb", verb), ("count", &objects.len().to_string())],
            ),
        );
        cx.notify();

        let (tx, rx) =
            tokio::sync::oneshot::channel::<Result<sqlly_services::ScriptSetReport, String>>();
        cx.background_spawn(async move {
            crate::tokio_handle()
                .spawn(async move {
                    let result = if drop {
                        sqlly_services::render_drop_script_live(
                            &services,
                            &query_profile,
                            &database,
                            &objects,
                        )
                        .await
                        .map_err(|e| e.to_string())
                    } else {
                        sqlly_services::render_create_script_with(
                            &services,
                            &query_profile,
                            &database,
                            &objects,
                            false,
                            ddl_options,
                        )
                        .await
                        .map_err(|e| e.to_string())
                    };
                    let _ = tx.send(result);
                })
                .await
                .ok();
        })
        .detach();

        let async_cx = cx.to_async();
        let weak_self = cx.weak_entity();
        cx.spawn(async move |_this, _cx| {
            let Ok(result) = rx.await else {
                return;
            };
            let mut cx = async_cx.clone();
            weak_self
                .update(&mut cx, |this, cx| match result {
                    Ok(report) => {
                        let status = match report.warnings.len() {
                            0 => crate::localization::format(
                                "shell.bulk_actions.script_opened",
                                &[("verb", verb)],
                            ),
                            1 => crate::localization::format(
                                "shell.bulk_actions.script_opened_one_warning",
                                &[("verb", verb), ("warning", &report.warnings[0])],
                            ),
                            count => crate::localization::format(
                                "shell.bulk_actions.script_opened_warnings",
                                &[("verb", verb), ("count", &count.to_string())],
                            ),
                        };
                        let level = if report.warnings.is_empty() {
                            StatusLevel::Info
                        } else {
                            StatusLevel::Warning
                        };
                        this.with_explorer_target_active(Some(profile), cx, move |app, cx| {
                            app.new_tab(cx, Some(report.sql));
                            app.set_status(level, status);
                            cx.notify();
                        });
                    }
                    Err(error) => {
                        this.set_status(
                            StatusLevel::Error,
                            crate::localization::format(
                                "shell.bulk_actions.script_failed",
                                &[("verb", verb), ("error", &error)],
                            ),
                        );
                        cx.notify();
                    }
                })
                .ok();
        })
        .detach();
    }

    fn bulk_maintenance_group(
        &mut self,
        op: sqlly_services::maintenance::MaintenanceOp,
        group: BulkObjectGroup,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self.bulk_group_profile(&group, cx) else {
            return;
        };
        if let Some(reason) = crate::capabilities::maintenance(profile.database_type).reason() {
            self.set_status(StatusLevel::Error, reason.to_string());
            cx.notify();
            return;
        }
        match bulk_actions::maintenance_script_for(profile.database_type, op, &group) {
            Ok(script) => {
                let status = if script.skipped.is_empty() {
                    crate::localization::text("shell.bulk_actions.maintenance_scripted")
                } else {
                    crate::localization::format(
                        "shell.bulk_actions.maintenance_scripted_skipped",
                        &[("objects", &script.skipped.join(", "))],
                    )
                };
                self.with_explorer_target_active(Some(profile), cx, move |app, cx| {
                    app.new_tab(cx, Some(script.sql));
                    app.set_status(StatusLevel::Info, status);
                    cx.notify();
                });
            }
            Err(error) => {
                self.set_status(StatusLevel::Error, error);
                cx.notify();
            }
        }
    }

    #[allow(unused_variables)]
    fn bulk_export_group(
        &mut self,
        group: BulkObjectGroup,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        #[cfg(target_arch = "wasm32")]
        {
            self.set_status(
                StatusLevel::Info,
                crate::browser_preview::unavailable(&crate::localization::text(
                    "shell.bulk_actions.export_unavailable",
                )),
            );
            cx.notify();
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let preselect = bulk_actions::export_table_preselect(&group);
            if preselect.is_empty() {
                self.set_status(
                    StatusLevel::Info,
                    crate::localization::text("shell.bulk_actions.no_tables_to_export"),
                );
                cx.notify();
                return;
            }
            let Some(profile) = self.bulk_group_profile(&group, cx) else {
                return;
            };
            let database = group
                .database
                .clone()
                .or_else(|| profile.database.clone())
                .unwrap_or_default();
            // Staged only on the path that opens the dialog (now, or once the
            // connect succeeds and the next render replays the pending open),
            // so a failed connect never leaves a stale set for a later
            // standalone export to inherit.
            match retarget_decision(self.active_profile.as_ref(), &profile) {
                RetargetDecision::Connect => {
                    self.connect_profile_then(profile, cx, move |app, cx| {
                        app.overlays.table_export_preselect = Some(preselect);
                        app.overlays.pending_table_export = Some((database, None));
                        cx.notify();
                    });
                }
                RetargetDecision::SwitchDatabase(db) => {
                    self.switch_database(db, cx);
                    self.overlays.table_export_preselect = Some(preselect);
                    self.open_table_export_dialog(database, None, window, cx);
                }
                RetargetDecision::AlreadyActive => {
                    self.overlays.table_export_preselect = Some(preselect);
                    self.open_table_export_dialog(database, None, window, cx);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(kind: NodeKind, database: Option<&str>) -> ExplorerActionContext {
        ExplorerActionContext {
            node_id: "n".into(),
            title: "t".into(),
            kind,
            server_id: Some(uuid::Uuid::nil()),
            database: database.map(str::to_string),
            schema: None,
            name: None,
            table: None,
            copy_name: None,
            copy_qualified: None,
            columns: Vec::new(),
            profile: None,
            position: gpui::Point::default(),
        }
    }

    #[test]
    fn template_context_reads_schema_and_object_from_the_node_kind() {
        let c = ctx(
            NodeKind::Procedure {
                schema: "dbo".into(),
                name: "usp_refresh".into(),
            },
            Some("shop"),
        );
        let t = template_object_context(&c, "prod".into());
        assert_eq!(t.server, "prod");
        assert_eq!(t.database, "shop");
        assert_eq!(t.schema, "dbo");
        assert_eq!(t.object, "usp_refresh");
        assert_eq!(t.kind, Some(sqlly_services::ExplorerObjectKind::Procedure));
    }

    #[test]
    fn template_context_falls_back_to_context_fields_for_other_kinds() {
        let mut c = ctx(NodeKind::TablesFolder, None);
        c.schema = Some("s".into());
        c.name = Some("n".into());
        let t = template_object_context(&c, String::new());
        assert_eq!((t.schema.as_str(), t.object.as_str()), ("s", "n"));
        assert_eq!(t.database, "");
        assert_eq!(t.kind, None);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn cart_item_only_tables_carry_data() {
        let table = ctx(
            NodeKind::Table {
                schema: "dbo".into(),
                name: "orders".into(),
            },
            Some("shop"),
        );
        let item = deploy_cart_item(&table, "prod".into(), true).expect("tables collect");
        assert!(item.include_data);
        assert_eq!(item.label(), "dbo.orders");
        assert_eq!(item.database, "shop");
        let view = ctx(
            NodeKind::View {
                schema: "dbo".into(),
                name: "v".into(),
            },
            Some("shop"),
        );
        let item = deploy_cart_item(&view, "prod".into(), true).expect("views collect");
        assert!(!item.include_data, "views never carry data");
        assert_eq!(item.kind, sqlly_services::DeployObjectKind::View);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn cart_item_refuses_uncollectable_nodes() {
        assert!(
            deploy_cart_item(&ctx(NodeKind::TablesFolder, None), String::new(), false).is_none()
        );
        let mut serverless = ctx(
            NodeKind::Table {
                schema: "dbo".into(),
                name: "t".into(),
            },
            None,
        );
        serverless.server_id = None;
        assert!(deploy_cart_item(&serverless, String::new(), false).is_none());
    }

    #[test]
    fn only_read_only_custom_actions_run_without_confirmation() {
        use sqlly_core::DatabaseType as Db;
        assert!(custom_action_runs_unconfirmed(
            Db::SqlServer,
            "SELECT COUNT(*) FROM [dbo].[Orders]"
        ));
        assert!(!custom_action_runs_unconfirmed(
            Db::SqlServer,
            "UPDATE [dbo].[Orders] SET x = 1"
        ));
        assert!(!custom_action_runs_unconfirmed(
            Db::Postgres,
            "SELECT 1; DROP TABLE \"public\".\"t\""
        ));
        assert!(!custom_action_runs_unconfirmed(
            Db::SqlServer,
            "EXEC dbo.usp_rebuild"
        ));
    }

    #[test]
    fn a_hostile_name_renders_as_one_quoted_identifier_and_stays_read_only() {
        let body = "SELECT COUNT(*) FROM {schema}.{object}";
        let template = sqlly_services::ScriptTemplate {
            name: "Row count".into(),
            category: String::new(),
            description: String::new(),
            body: body.into(),
            parameters: sqlly_services::parameters_from_body(body),
            origin: sqlly_services::TemplateOrigin::User,
            explorer_action: Some(sqlly_services::ExplorerActionSpec {
                kinds: vec![sqlly_services::ExplorerObjectKind::Table],
                mode: sqlly_services::ExplorerActionMode::Run,
            }),
        };
        let c = ctx(
            NodeKind::Table {
                schema: "dbo".into(),
                name: "x]; DROP TABLE y;--".into(),
            },
            Some("shop"),
        );
        let sql = template.render_for_object(
            &std::collections::HashMap::new(),
            &template_object_context(&c, "prod".into()),
            sqlly_core::DatabaseType::SqlServer,
        );
        assert_eq!(sql, "SELECT COUNT(*) FROM [dbo].[x]]; DROP TABLE y;--]");
        assert!(custom_action_runs_unconfirmed(
            sqlly_core::DatabaseType::SqlServer,
            &sql
        ));
    }

    #[test]
    fn template_context_ignores_materialized_views() {
        let t = template_object_context(
            &ctx(
                NodeKind::MaterializedView {
                    schema: "public".into(),
                    name: "mv".into(),
                },
                Some("db"),
            ),
            String::new(),
        );
        assert_eq!(
            t.kind, None,
            "materialized views are not explorer-action targets"
        );
    }

    /// Parameter values are raw text (SSMS semantics): a value can turn a
    /// read-only Run template into a write, and the rendered SQL — not the
    /// template — is what the read-only check sees, so that write still goes
    /// to review instead of running unconfirmed.
    #[test]
    fn typed_parameter_values_render_raw_and_still_face_the_read_only_check() {
        let body = "SELECT TOP (<top,int,10>) * FROM {schema}.{object} WHERE <filter,,1=1>";
        let template = sqlly_services::ScriptTemplate {
            name: "Peek".into(),
            category: String::new(),
            description: String::new(),
            body: body.into(),
            parameters: sqlly_services::parameters_from_body(body),
            origin: sqlly_services::TemplateOrigin::User,
            explorer_action: Some(sqlly_services::ExplorerActionSpec {
                kinds: vec![sqlly_services::ExplorerObjectKind::Table],
                mode: sqlly_services::ExplorerActionMode::Run,
            }),
        };
        assert!(template_runs(&template));
        let object = template_object_context(
            &ctx(
                NodeKind::Table {
                    schema: "dbo".into(),
                    name: "Orders".into(),
                },
                Some("shop"),
            ),
            "prod".into(),
        );
        let db = sqlly_core::DatabaseType::SqlServer;
        // Defaults when nothing is typed: read-only, runs straight away.
        let defaults = template.render_for_object(&HashMap::new(), &object, db);
        assert_eq!(defaults, "SELECT TOP (10) * FROM [dbo].[Orders] WHERE 1=1");
        assert!(custom_action_runs_unconfirmed(db, &defaults));
        // Typed values substitute verbatim, every occurrence, and a partial
        // map keeps the default for the rest.
        let values: HashMap<String, String> =
            [("filter".to_string(), "Freight > 5".to_string())].into();
        let typed = template.render_for_object(&values, &object, db);
        assert_eq!(
            typed,
            "SELECT TOP (10) * FROM [dbo].[Orders] WHERE Freight > 5"
        );
        assert!(custom_action_runs_unconfirmed(db, &typed));
        // A value that smuggles in a write is caught on the rendered SQL.
        let values: HashMap<String, String> = [(
            "filter".to_string(),
            "1=1; DELETE FROM [dbo].[Orders]".to_string(),
        )]
        .into();
        let hostile = template.render_for_object(&values, &object, db);
        assert!(hostile.contains("DELETE FROM [dbo].[Orders]"));
        assert!(!custom_action_runs_unconfirmed(db, &hostile));
    }

    #[test]
    fn parameter_prompt_target_names_server_database_and_object() {
        let object = template_object_context(
            &ctx(
                NodeKind::View {
                    schema: "sales".into(),
                    name: "v_top".into(),
                },
                Some("shop"),
            ),
            "prod".into(),
        );
        assert_eq!(
            custom_action_target_label("prod", &object),
            "prod \u{b7} shop \u{b7} sales.v_top"
        );
        let empty = template_object_context(&ctx(NodeKind::TablesFolder, None), String::new());
        assert_eq!(
            custom_action_target_label("", &empty),
            "the active connection"
        );
    }
}
