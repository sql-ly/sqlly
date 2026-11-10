use super::*;
use crate::tool_dialog::ToolScope;

/// Why a tool that dials the database directly from this machine (a native
/// CLI, a dedicated Redis socket, a native dump tool) cannot run on
/// `profile`: relay connections are reached through the bridge, and the
/// desktop must never silently dial the bridge-side address itself.
/// `None` when the connection is direct.
pub(crate) fn direct_route_refusal(profile: &ConnectionProfile, feature: &str) -> Option<String> {
    matches!(
        profile.route,
        sqlly_core::connection::ConnectionRoute::ViaRelay { .. }
    )
    .then(|| {
        crate::localization::format("shell.tools.direct_route_required", &[("feature", feature)])
    })
}

impl AppView {
    /// True while `profile` is a production connection whose writes are
    /// locked (not unlocked from the status-bar chip). Every tool that can
    /// mutate — bulk actions, imports, config changes, kills — checks this
    /// before offering or running its write path.
    pub(crate) fn production_writes_locked(&self, profile: &ConnectionProfile) -> bool {
        let store = crate::connection::ConnectionStore::global();
        crate::mutation_review::SafetyFeatures::effective_for_profile(profile, store)
            .production_guard()
            .decide(
                self.production_unlock.as_ref(),
                sqlly_intel::query::safety::Instant::now(),
            )
            == sqlly_intel::query::safety::ProductionDecision::ReadOnly
    }

    pub(super) fn handle_search_schema(
        &mut self,
        _action: &SearchSchema,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.ai_panel.update(cx, |p, cx| {
            p.set_active_section(AiSection::Search, cx);
        });
        self.ai_visible = true;
        cx.notify();
    }

    pub(super) fn show_tool_output(
        &mut self,
        title: &str,
        body: String,
        can_open_as_query: bool,
        cx: &mut Context<Self>,
    ) {
        if self.overlays.tool_output_view.is_some() {
            return;
        }
        let view = cx.new(|_cx| ToolOutputView::new(title.to_string(), body, can_open_as_query));
        cx.subscribe(&view, Self::handle_tool_output_event).detach();
        self.overlays.tool_output_view = Some(view);
        self.set_status(
            StatusLevel::Info,
            crate::localization::format("shell.tools.output_ready", &[("title", title)]),
        );
        cx.notify();
    }

    pub(super) fn handle_tool_output_event(
        &mut self,
        _emitter: Entity<ToolOutputView>,
        event: &ToolOutputEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            ToolOutputEvent::OpenQuery(sql) => {
                self.new_tab(cx, Some(sql.clone()));
                self.overlays.tool_output_view = None;
                cx.notify();
            }
            ToolOutputEvent::Closed => {
                self.overlays.tool_output_view = None;
                cx.notify();
            }
        }
    }

    pub(super) fn handle_template_explorer(
        &mut self,
        _action: &TemplateExplorer,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.overlays.template_explorer_dialog.is_none() {
            let d = cx.new(|cx| crate::template_explorer::TemplateExplorerDialog::new(window, cx));
            cx.subscribe(&d, Self::handle_template_explorer_event)
                .detach();
            self.overlays.template_explorer_dialog = Some(d);
        }
        cx.notify();
    }

    pub(super) fn handle_template_explorer_event(
        &mut self,
        _view: Entity<crate::template_explorer::TemplateExplorerDialog>,
        event: &crate::tool_dialog::ToolDialogEvent,
        cx: &mut Context<Self>,
    ) {
        if self.apply_tool_dialog_event(event, cx) {
            self.overlays.template_explorer_dialog = None;
            self.restore_editor_focus(cx);
            cx.notify();
        }
    }

    /// Resolve the active connection and apply the centralized engine gate
    /// before an admin pane is constructed. Menu actions and palette commands
    /// both land in these guarded handlers, so neither path can bypass the
    /// matrix in `capabilities`.
    pub(super) fn require_admin_tool(
        &mut self,
        tool: crate::capabilities::AdminTool,
        cx: &mut Context<Self>,
    ) -> Option<sqlly_core::ConnectionProfile> {
        let Some(profile) = self.active_profile.clone() else {
            self.set_status(
                StatusLevel::Info,
                crate::localization::format(
                    "shell.tools.connect_admin_tool",
                    &[("tool", tool.label())],
                ),
            );
            cx.notify();
            return None;
        };
        if let Some(reason) = crate::capabilities::admin_tool(profile.database_type, tool).reason()
        {
            self.set_status(StatusLevel::Info, reason.to_string());
            cx.notify();
            return None;
        }
        Some(profile)
    }

    pub(super) fn handle_schema_compare(
        &mut self,
        _action: &SchemaCompare,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) =
            self.require_admin_tool(crate::capabilities::AdminTool::SchemaCompare, cx)
        else {
            return;
        };
        if self.overlays.schema_compare_pane.is_none() {
            let context = crate::tool_dialog::ToolDialogContext::new(
                profile.clone(),
                self.services_for_profile(&profile),
            );
            let router = self.service_router.clone();
            let pane = cx.new(|cx| {
                crate::schema_compare::SchemaComparePane::new(context, router, window, cx)
            });
            cx.subscribe(&pane, Self::handle_schema_compare_event)
                .detach();
            self.overlays.schema_compare_pane = Some(pane);
        }
        cx.notify();
    }

    pub(super) fn handle_schema_compare_event(
        &mut self,
        view: Entity<crate::schema_compare::SchemaComparePane>,
        event: &crate::tool_dialog::ToolDialogEvent,
        cx: &mut Context<Self>,
    ) {
        #[cfg(not(target_arch = "wasm32"))]
        #[allow(clippy::collapsible_if)]
        if matches!(
            event,
            crate::tool_dialog::ToolDialogEvent::Status { message, .. } if message == "deploy-requested"
        ) {
            if let Some(request) = view.update(cx, |pane, _| pane.take_deploy_request()) {
                let store = crate::connection::ConnectionStore::global();
                let safety = crate::mutation_review::SafetyFeatures::effective_for_profile(
                    &request.profile,
                    store,
                );
                let production = safety
                    .production_guard()
                    .decide(self.production_unlock.as_ref(), std::time::Instant::now());
                if production == sqlly_intel::query::safety::ProductionDecision::ReadOnly {
                    self.set_status(
                        StatusLevel::Error,
                        crate::localization::text("shell.tools.schema_deploy_production_locked"),
                    );
                    cx.notify();
                    return;
                }
                if self.overlays.schema_compare_deploy_dialog.is_none() {
                    let profile = request.profile.clone();
                    let context = crate::tool_dialog::ToolDialogContext::new(
                        profile.clone(),
                        self.services_for_profile(&profile),
                    );
                    let deploy_request = crate::schema_compare_deploy::DeployRequest {
                        script: request.script,
                        model: request.model,
                    };
                    let app_view = cx.entity();
                    cx.defer(move |app| {
                        let Some(win) = app.active_window() else {
                            return;
                        };
                        let _ = win.update(app, |_root, window, app| {
                            app_view.update(app, |this, cx| {
                                if this.overlays.schema_compare_deploy_dialog.is_some() {
                                    return;
                                }
                                let dialog = cx.new(|cx| {
                                    crate::schema_compare_deploy::SchemaCompareDeployDialog::new(
                                        context,
                                        deploy_request,
                                        window,
                                        cx,
                                    )
                                });
                                cx.subscribe(&dialog, AppView::handle_schema_compare_deploy_event)
                                    .detach();
                                this.overlays.schema_compare_deploy_dialog = Some(dialog);
                                cx.notify();
                            });
                        });
                    });
                }
            }
        }
        if self.apply_tool_dialog_event(event, cx) {
            self.overlays.schema_compare_pane = None;
            self.restore_editor_focus(cx);
            cx.notify();
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn handle_schema_compare_deploy_event(
        &mut self,
        view: Entity<crate::schema_compare_deploy::SchemaCompareDeployDialog>,
        event: &crate::tool_dialog::ToolDialogEvent,
        cx: &mut Context<Self>,
    ) {
        let close = self.apply_tool_dialog_event(event, cx);
        #[allow(clippy::collapsible_if)]
        if matches!(event, crate::tool_dialog::ToolDialogEvent::Status { .. }) {
            if let Some(body) = view.update(cx, |dialog, _| dialog.take_last_summary()) {
                let level = match event {
                    crate::tool_dialog::ToolDialogEvent::Status { level, .. } => *level,
                    _ => StatusLevel::Info,
                };
                let message = match event {
                    crate::tool_dialog::ToolDialogEvent::Status { message, .. } => message.clone(),
                    _ => String::new(),
                };
                let kind = if level == StatusLevel::Error {
                    crate::notification_center::NotificationKind::QueryFailed
                } else {
                    crate::notification_center::NotificationKind::QueryCompleted
                };
                self.notifications.push(kind, message, chrono::Utc::now());
                self.show_tool_output(
                    &crate::localization::text("shell.tools.schema_compare_deploy_title"),
                    body,
                    false,
                    cx,
                );
            }
        }
        if close {
            self.overlays.schema_compare_deploy_dialog = None;
            self.restore_editor_focus(cx);
            cx.notify();
        }
    }

    /// Schema History from the palette: the active connection's current
    /// database.
    pub(super) fn handle_schema_history(
        &mut self,
        _action: &SchemaHistory,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) =
            self.require_admin_tool(crate::capabilities::AdminTool::SchemaHistory, cx)
        else {
            return;
        };
        if profile.database.is_none() {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.tools.schema_history_select_database"),
            );
            cx.notify();
            return;
        }
        self.open_schema_history(profile, window, cx);
    }

    /// Save Schema Snapshot from the palette: the active connection's
    /// current database.
    pub(super) fn handle_save_schema_snapshot(
        &mut self,
        _action: &SaveSchemaSnapshot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) =
            self.require_admin_tool(crate::capabilities::AdminTool::SchemaHistory, cx)
        else {
            return;
        };
        if profile.database.is_none() {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.tools.snapshot_select_database"),
            );
            cx.notify();
            return;
        }
        self.open_save_schema_snapshot(profile, window, cx);
    }

    /// Open the snapshot timeline for `profile.database`. Snapshots live on
    /// disk, so the browser preview reports instead of opening.
    pub(super) fn open_schema_history(
        &mut self,
        profile: sqlly_core::ConnectionProfile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        #[cfg(target_arch = "wasm32")]
        {
            let _ = (profile, window);
            self.set_status(
                StatusLevel::Info,
                crate::browser_preview::unavailable(&crate::localization::text(
                    "shell.tools.snapshots_unavailable",
                ))
                .to_string(),
            );
            cx.notify();
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            if let Some(reason) = crate::capabilities::admin_tool(
                profile.database_type,
                crate::capabilities::AdminTool::SchemaHistory,
            )
            .reason()
            {
                self.set_status(StatusLevel::Info, reason.to_string());
                cx.notify();
                return;
            }
            // Reopen on the requested database even if a history for another
            // one is showing.
            self.overlays.schema_history_dialog = None;
            let store = crate::schema_snapshots::SchemaSnapshotStore::default_store();
            let dialog = cx.new(|cx| {
                crate::schema_history::SchemaHistoryDialog::new(profile, store, window, cx)
            });
            cx.subscribe(&dialog, Self::handle_schema_history_event)
                .detach();
            self.overlays.schema_history_dialog = Some(dialog);
            cx.notify();
        }
    }

    /// Open the label prompt that captures `profile.database`.
    pub(super) fn open_save_schema_snapshot(
        &mut self,
        profile: sqlly_core::ConnectionProfile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        #[cfg(target_arch = "wasm32")]
        {
            let _ = (profile, window);
            self.set_status(
                StatusLevel::Info,
                crate::browser_preview::unavailable(&crate::localization::text(
                    "shell.tools.save_snapshot_unavailable",
                ))
                .to_string(),
            );
            cx.notify();
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            if self.overlays.save_schema_snapshot_dialog.is_some() {
                return;
            }
            if let Some(reason) = crate::capabilities::admin_tool(
                profile.database_type,
                crate::capabilities::AdminTool::SchemaHistory,
            )
            .reason()
            {
                self.set_status(StatusLevel::Info, reason.to_string());
                cx.notify();
                return;
            }
            let context = crate::tool_dialog::ToolDialogContext::new(
                profile.clone(),
                self.services_for_profile(&profile),
            );
            let store = crate::schema_snapshots::SchemaSnapshotStore::default_store();
            let dialog = cx.new(|cx| {
                crate::schema_history::SaveSchemaSnapshotDialog::new(context, store, window, cx)
            });
            cx.subscribe(&dialog, Self::handle_save_schema_snapshot_event)
                .detach();
            self.overlays.save_schema_snapshot_dialog = Some(dialog);
            cx.notify();
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    /// Open Schema Compare with both sides preset and the comparison running
    /// (Schema History's Compare). Replaces any pane already open so the
    /// requested pair is what shows.
    pub(super) fn open_schema_compare_with_sources(
        &mut self,
        source: crate::schema_snapshots::CompareSource,
        target: crate::schema_snapshots::CompareSource,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let anchor = match (&source, &target) {
            (crate::schema_snapshots::CompareSource::Live { profile, .. }, _)
            | (_, crate::schema_snapshots::CompareSource::Live { profile, .. }) => {
                profile.as_ref().clone()
            }
            (crate::schema_snapshots::CompareSource::Snapshot(meta), _) => {
                match crate::connection::ConnectionStore::global().get(meta.connection_id) {
                    Some(profile) => profile,
                    None => match self.active_profile.clone() {
                        Some(profile) => profile,
                        None => {
                            self.set_status(
                                StatusLevel::Info,
                                crate::localization::text("shell.tools.schema_compare_connect"),
                            );
                            cx.notify();
                            return;
                        }
                    },
                }
            }
        };
        let context = crate::tool_dialog::ToolDialogContext::new(
            anchor.clone(),
            self.services_for_profile(&anchor),
        );
        let router = self.service_router.clone();
        self.overlays.schema_compare_pane = None;
        let pane = cx.new(|cx| {
            crate::schema_compare::SchemaComparePane::new_with_sources(
                context, router, source, target, window, cx,
            )
        });
        cx.subscribe(&pane, Self::handle_schema_compare_event)
            .detach();
        self.overlays.schema_compare_pane = Some(pane);
        cx.notify();
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn handle_schema_history_event(
        &mut self,
        _view: Entity<crate::schema_history::SchemaHistoryDialog>,
        event: &crate::schema_history::SchemaHistoryEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            crate::schema_history::SchemaHistoryEvent::Compare { source, target } => {
                let (source, target) = (source.as_ref().clone(), target.as_ref().clone());
                let app_view = cx.entity();
                cx.defer(move |app| {
                    let Some(win) = app.active_window() else {
                        return;
                    };
                    let _ = win.update(app, |_root, window, app| {
                        app_view.update(app, |this, cx| {
                            this.open_schema_compare_with_sources(source, target, window, cx);
                        });
                    });
                });
            }
            crate::schema_history::SchemaHistoryEvent::TakeSnapshot { profile } => {
                let profile = profile.as_ref().clone();
                let app_view = cx.entity();
                cx.defer(move |app| {
                    let Some(win) = app.active_window() else {
                        return;
                    };
                    let _ = win.update(app, |_root, window, app| {
                        app_view.update(app, |this, cx| {
                            this.open_save_schema_snapshot(profile, window, cx);
                        });
                    });
                });
            }
            crate::schema_history::SchemaHistoryEvent::Closed => {
                self.overlays.schema_history_dialog = None;
                self.restore_editor_focus(cx);
                cx.notify();
            }
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn handle_save_schema_snapshot_event(
        &mut self,
        _view: Entity<crate::schema_history::SaveSchemaSnapshotDialog>,
        event: &crate::schema_history::SchemaSnapshotDialogEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            crate::schema_history::SchemaSnapshotDialogEvent::Saved(meta) => {
                self.set_status(
                    StatusLevel::Success,
                    crate::localization::format(
                        "shell.tools.snapshot_saved",
                        &[
                            ("title", &crate::schema_snapshots::snapshot_title(meta)),
                            ("count", &meta.object_count.to_string()),
                        ],
                    ),
                );
                // Surfaces that list snapshots pick the new one up right away.
                if let Some(dialog) = self.overlays.schema_history_dialog.clone() {
                    dialog.update(cx, |dialog, cx| dialog.reload(cx));
                }
                if let Some(pane) = self.overlays.schema_compare_pane.clone() {
                    pane.update(cx, |pane, cx| pane.reload_snapshots(cx));
                }
                cx.notify();
            }
            crate::schema_history::SchemaSnapshotDialogEvent::Closed => {
                self.overlays.save_schema_snapshot_dialog = None;
                if self.overlays.schema_history_dialog.is_none()
                    && self.overlays.schema_compare_pane.is_none()
                {
                    self.restore_editor_focus(cx);
                }
                cx.notify();
            }
        }
    }

    pub(super) fn handle_data_compare(
        &mut self,
        _action: &DataCompare,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) =
            self.require_admin_tool(crate::capabilities::AdminTool::DataCompare, cx)
        else {
            return;
        };
        if self.overlays.data_compare_pane.is_none() {
            let context = crate::tool_dialog::ToolDialogContext::new(
                profile.clone(),
                self.services_for_profile(&profile),
            );
            let router = self.service_router.clone();
            let pane =
                cx.new(|cx| crate::data_compare::DataComparePane::new(context, router, window, cx));
            cx.subscribe(&pane, Self::handle_data_compare_event)
                .detach();
            self.overlays.data_compare_pane = Some(pane);
        }
        cx.notify();
    }

    pub(super) fn handle_data_compare_event(
        &mut self,
        _view: Entity<crate::data_compare::DataComparePane>,
        event: &crate::tool_dialog::ToolDialogEvent,
        cx: &mut Context<Self>,
    ) {
        if self.apply_tool_dialog_event(event, cx) {
            self.overlays.data_compare_pane = None;
            self.restore_editor_focus(cx);
            cx.notify();
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn handle_data_transfer(
        &mut self,
        _action: &DataTransfer,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) =
            self.require_admin_tool(crate::capabilities::AdminTool::DataTransfer, cx)
        else {
            return;
        };
        if self.overlays.data_transfer_pane.is_none() {
            let context = crate::tool_dialog::ToolDialogContext::new(
                profile.clone(),
                self.services_for_profile(&profile),
            );
            let router = self.service_router.clone();
            let pane = cx.new(|cx| {
                crate::data_transfer::DataTransferPane::new(context, router, None, window, cx)
            });
            cx.subscribe(&pane, Self::handle_data_transfer_event)
                .detach();
            self.overlays.data_transfer_pane = Some(pane);
        }
        cx.notify();
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn open_data_transfer_for(
        &mut self,
        preselect: (Option<String>, Option<String>, String),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) =
            self.require_admin_tool(crate::capabilities::AdminTool::DataTransfer, cx)
        else {
            return;
        };
        if self.overlays.data_transfer_pane.is_none() {
            let context = crate::tool_dialog::ToolDialogContext::new(
                profile.clone(),
                self.services_for_profile(&profile),
            );
            let router = self.service_router.clone();
            let pane = cx.new(|cx| {
                crate::data_transfer::DataTransferPane::new(
                    context,
                    router,
                    Some(preselect),
                    window,
                    cx,
                )
            });
            cx.subscribe(&pane, Self::handle_data_transfer_event)
                .detach();
            self.overlays.data_transfer_pane = Some(pane);
        }
        cx.notify();
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn handle_data_transfer_event(
        &mut self,
        view: Entity<crate::data_transfer::DataTransferPane>,
        event: &crate::tool_dialog::ToolDialogEvent,
        cx: &mut Context<Self>,
    ) {
        let summary = view.update(cx, |pane, _| pane.take_last_summary());
        if let (crate::tool_dialog::ToolDialogEvent::Status { level, message }, Some(body)) =
            (event, summary)
        {
            let kind = match level {
                StatusLevel::Error => crate::notification_center::NotificationKind::QueryFailed,
                _ => crate::notification_center::NotificationKind::QueryCompleted,
            };
            self.notifications
                .push(kind, message.clone(), chrono::Utc::now());
            self.show_tool_output(
                &crate::localization::text("shell.tools.data_transfer_title"),
                body,
                false,
                cx,
            );
        }
        if self.apply_tool_dialog_event(event, cx) {
            self.overlays.data_transfer_pane = None;
            self.restore_editor_focus(cx);
            cx.notify();
        }
    }

    pub(super) fn handle_query_store(
        &mut self,
        _action: &QueryStore,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_query_store(cx);
    }

    pub(super) fn open_query_store(&mut self, cx: &mut Context<Self>) {
        let Some(profile) =
            self.require_admin_tool(crate::capabilities::AdminTool::QueryInsights, cx)
        else {
            return;
        };
        if self.overlays.query_store_pane.is_none() {
            let context = crate::tool_dialog::ToolDialogContext::new(
                profile.clone(),
                self.services_for_profile(&profile),
            );
            let pane = cx.new(|cx| crate::query_store::QueryStorePane::new(context, cx));
            cx.subscribe(&pane, Self::handle_query_store_event).detach();
            self.overlays.query_store_pane = Some(pane);
        }
        cx.notify();
    }

    pub(super) fn handle_query_store_event(
        &mut self,
        _view: Entity<crate::query_store::QueryStorePane>,
        event: &crate::tool_dialog::ToolDialogEvent,
        cx: &mut Context<Self>,
    ) {
        if self.apply_tool_dialog_event(event, cx) {
            self.overlays.query_store_pane = None;
            self.restore_editor_focus(cx);
            cx.notify();
        }
    }

    pub(super) fn handle_disk_usage(
        &mut self,
        _action: &DiskUsageTool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Action-dispatched entry points (command palette, dashboard footer,
        // keyboard) target the whole server; the per-database scope only arrives
        // from a right-clicked database node via `dispatch_explorer_tool`.
        self.open_disk_usage(ToolScope::Server, cx);
    }

    pub(super) fn open_disk_usage(&mut self, scope: ToolScope, cx: &mut Context<Self>) {
        let Some(profile) = self.require_admin_tool(crate::capabilities::AdminTool::DiskUsage, cx)
        else {
            return;
        };
        if self.overlays.disk_usage_pane.is_none() {
            let context = crate::tool_dialog::ToolDialogContext::new(
                profile.clone(),
                self.services_for_profile(&profile),
            );
            let pane =
                cx.new(|cx| crate::database_disk_usage::DiskUsagePane::new(context, scope, cx));
            cx.subscribe(&pane, Self::handle_disk_usage_event).detach();
            self.overlays.disk_usage_pane = Some(pane);
        }
        cx.notify();
    }

    pub(super) fn handle_disk_usage_event(
        &mut self,
        _view: Entity<crate::database_disk_usage::DiskUsagePane>,
        event: &crate::tool_dialog::ToolDialogEvent,
        cx: &mut Context<Self>,
    ) {
        if self.apply_tool_dialog_event(event, cx) {
            self.overlays.disk_usage_pane = None;
            self.restore_editor_focus(cx);
            cx.notify();
        }
    }

    pub(super) fn handle_redis_memory_analyzer(
        &mut self,
        _action: &RedisMemoryAnalyzerTool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_redis_memory_analyzer(cx);
    }

    pub(super) fn open_redis_memory_analyzer(&mut self, cx: &mut Context<Self>) {
        let Some(profile) =
            self.require_admin_tool(crate::capabilities::AdminTool::RedisMemoryAnalyzer, cx)
        else {
            return;
        };
        let context = crate::tool_dialog::ToolDialogContext::new(
            profile.clone(),
            self.services_for_profile(&profile),
        );
        if let Some(pane) = &self.overlays.redis_memory_pane {
            // An open pane asked for another connection or logical database
            // resamples that target instead of showing the old one's reports.
            pane.update(cx, |pane, cx| {
                let current = pane.profile();
                if current.id != profile.id || current.database != profile.database {
                    pane.retarget(context, cx);
                }
            });
        } else {
            let pane = cx.new(|cx| crate::redis_memory_analyzer::RedisMemoryPane::new(context, cx));
            cx.subscribe(&pane, Self::handle_redis_memory_analyzer_event)
                .detach();
            self.overlays.redis_memory_pane = Some(pane);
        }
        cx.notify();
    }

    pub(super) fn handle_redis_memory_analyzer_event(
        &mut self,
        _view: Entity<crate::redis_memory_analyzer::RedisMemoryPane>,
        event: &crate::tool_dialog::ToolDialogEvent,
        cx: &mut Context<Self>,
    ) {
        if self.apply_tool_dialog_event(event, cx) {
            self.overlays.redis_memory_pane = None;
            self.restore_editor_focus(cx);
            cx.notify();
        }
    }

    /// Open (or retarget) the Redis Cluster topology pane on the active
    /// connection.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn open_redis_cluster_topology(&mut self, cx: &mut Context<Self>) {
        let Some(profile) =
            self.require_admin_tool(crate::capabilities::AdminTool::RedisClusterTopology, cx)
        else {
            return;
        };
        let context = crate::tool_dialog::ToolDialogContext::new(
            profile.clone(),
            self.services_for_profile(&profile),
        );
        if let Some(pane) = &self.overlays.redis_cluster_topology {
            pane.update(cx, |pane, cx| {
                if pane.profile().id != profile.id {
                    pane.retarget(context, cx);
                }
            });
        } else {
            let pane = cx.new(|cx| {
                crate::redis_cluster_topology::RedisClusterTopologyPane::new(context, cx)
            });
            cx.subscribe(&pane, Self::handle_redis_cluster_topology_event)
                .detach();
            self.overlays.redis_cluster_topology = Some(pane);
        }
        cx.notify();
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn handle_redis_cluster_topology_event(
        &mut self,
        _view: Entity<crate::redis_cluster_topology::RedisClusterTopologyPane>,
        event: &crate::tool_dialog::ToolDialogEvent,
        cx: &mut Context<Self>,
    ) {
        if self.apply_tool_dialog_event(event, cx) {
            self.overlays.redis_cluster_topology = None;
            self.restore_editor_focus(cx);
            cx.notify();
        }
    }

    /// The command palette's Redis Key View: opens on the active connection
    /// with a key-name prompt (the explorer opens it on a key directly).
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn handle_redis_key_view_palette(&mut self, cx: &mut Context<Self>) {
        let Some(profile) =
            self.require_admin_tool(crate::capabilities::AdminTool::RedisKeyView, cx)
        else {
            return;
        };
        self.open_redis_key_view(profile, None, cx);
    }

    /// Open (or retarget) the key view on `profile` — the key's own server
    /// and logical database — for `key`, byte-exact.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn open_redis_key_view(
        &mut self,
        profile: sqlly_core::ConnectionProfile,
        key: Option<Vec<u8>>,
        cx: &mut Context<Self>,
    ) {
        let context = crate::tool_dialog::ToolDialogContext::new(
            profile.clone(),
            self.services_for_profile(&profile),
        );
        if let Some(pane) = &self.overlays.redis_key_view {
            pane.update(cx, |pane, cx| pane.retarget(context, key, cx));
        } else {
            // The view writes through `tool_dialog::run_query`, which skips
            // the editor's run gate — hand it the production guard so a
            // locked production connection opens with writes disabled and
            // an unlock that expires is honoured at run time.
            let (production, production_check) = self.production_check(&profile, cx);
            let pane = cx.new(|cx| {
                crate::redis_key_view::RedisKeyViewPane::new(
                    context,
                    key,
                    production,
                    production_check,
                    cx,
                )
            });
            cx.subscribe(&pane, Self::handle_redis_key_view_event)
                .detach();
            self.overlays.redis_key_view = Some(pane);
        }
        cx.notify();
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn handle_redis_key_view_event(
        &mut self,
        _view: Entity<crate::redis_key_view::RedisKeyViewPane>,
        event: &crate::tool_dialog::ToolDialogEvent,
        cx: &mut Context<Self>,
    ) {
        if self.apply_tool_dialog_event(event, cx) {
            self.overlays.redis_key_view = None;
            self.restore_editor_focus(cx);
            cx.notify();
        }
    }

    pub(super) fn handle_object_browser(
        &mut self,
        _action: &ObjectBrowserTool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) =
            self.require_admin_tool(crate::capabilities::AdminTool::ObjectBrowser, cx)
        else {
            return;
        };
        let database = profile.database.clone().unwrap_or_default();
        self.open_object_browser_for(profile, database, None, window, cx);
    }

    pub(super) fn open_object_browser_for(
        &mut self,
        mut profile: sqlly_core::ConnectionProfile,
        database: String,
        schema: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        profile.database = Some(database.clone());
        // Explorer can open a different database while a pane is already up;
        // replace rather than skip so the inventory always matches the node.
        self.overlays.object_browser_pane = None;
        let context = crate::tool_dialog::ToolDialogContext::new(
            profile.clone(),
            self.services_for_profile(&profile),
        );
        let pane = cx.new(|cx| {
            crate::object_browser::ObjectBrowserPane::new(context, database, schema, window, cx)
        });
        cx.subscribe(&pane, Self::handle_object_browser_event)
            .detach();
        self.overlays.object_browser_pane = Some(pane);
        cx.notify();
    }

    pub(super) fn handle_object_browser_event(
        &mut self,
        _view: Entity<crate::object_browser::ObjectBrowserPane>,
        event: &crate::tool_dialog::ToolDialogEvent,
        cx: &mut Context<Self>,
    ) {
        if self.apply_tool_dialog_event(event, cx) {
            self.overlays.object_browser_pane = None;
            self.restore_editor_focus(cx);
            cx.notify();
        }
    }

    pub(super) fn handle_index_analyzer(
        &mut self,
        _action: &IndexAnalyzerTool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) =
            self.require_admin_tool(crate::capabilities::AdminTool::IndexAnalyzer, cx)
        else {
            return;
        };
        if self.overlays.index_analyzer_pane.is_none() {
            let context = crate::tool_dialog::ToolDialogContext::new(
                profile.clone(),
                self.services_for_profile(&profile),
            );
            let pane = cx.new(|cx| crate::index_analyzer::IndexAnalyzerPane::new(context, cx));
            cx.subscribe(&pane, Self::handle_index_analyzer_event)
                .detach();
            self.overlays.index_analyzer_pane = Some(pane);
        }
        cx.notify();
    }

    pub(super) fn handle_index_analyzer_event(
        &mut self,
        _view: Entity<crate::index_analyzer::IndexAnalyzerPane>,
        event: &crate::tool_dialog::ToolDialogEvent,
        cx: &mut Context<Self>,
    ) {
        if self.apply_tool_dialog_event(event, cx) {
            self.overlays.index_analyzer_pane = None;
            self.restore_editor_focus(cx);
            cx.notify();
        }
    }

    pub(super) fn handle_test_data_generator(
        &mut self,
        _action: &TestDataGeneratorTool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) =
            self.require_admin_tool(crate::capabilities::AdminTool::TestDataGenerator, cx)
        else {
            return;
        };
        if self.overlays.test_data_generator_pane.is_none() {
            let context = crate::tool_dialog::ToolDialogContext::new(
                profile.clone(),
                self.services_for_profile(&profile),
            );
            let pane = cx.new(|cx| {
                crate::test_data_generator::TestDataGeneratorPane::new(context, window, cx)
            });
            cx.subscribe(&pane, Self::handle_test_data_generator_event)
                .detach();
            self.overlays.test_data_generator_pane = Some(pane);
        }
        cx.notify();
    }

    pub(super) fn handle_test_data_generator_event(
        &mut self,
        _view: Entity<crate::test_data_generator::TestDataGeneratorPane>,
        event: &crate::tool_dialog::ToolDialogEvent,
        cx: &mut Context<Self>,
    ) {
        if self.apply_tool_dialog_event(event, cx) {
            self.overlays.test_data_generator_pane = None;
            self.restore_editor_focus(cx);
            cx.notify();
        }
    }

    /// Open the per-database tenant-scoping settings for the active
    /// connection's current database.
    pub(super) fn handle_tenant_scoping(
        &mut self,
        _action: &TenantScoping,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self.active_profile.clone() else {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.tools.tenant_scoping_connect"),
            );
            cx.notify();
            return;
        };
        if self.overlays.tenant_scoping_dialog.is_none() {
            let dialog = cx.new(|cx| {
                crate::tenant_scoping::TenantScopingDialog::new(
                    profile.server.clone(),
                    profile.database.clone().unwrap_or_default(),
                    window,
                    cx,
                )
            });
            cx.subscribe(&dialog, Self::handle_tenant_scoping_event)
                .detach();
            self.overlays.tenant_scoping_dialog = Some(dialog);
        }
        cx.notify();
    }

    pub(super) fn handle_tenant_scoping_event(
        &mut self,
        _view: Entity<crate::tenant_scoping::TenantScopingDialog>,
        event: &crate::tenant_scoping::TenantScopingEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            crate::tenant_scoping::TenantScopingEvent::Closed => {
                self.overlays.tenant_scoping_dialog = None;
                self.restore_editor_focus(cx);
                cx.notify();
            }
        }
    }

    /// Open the query-history browser: search, pin, and re-run past queries.
    /// Local-file surface — no connection required.
    pub(super) fn handle_show_query_history(
        &mut self,
        _action: &ShowQueryHistory,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.overlays.query_history_browser.is_none() {
            let browser =
                cx.new(|cx| crate::query_history_browser::QueryHistoryBrowser::new(window, cx));
            browser.update(cx, |b, cx| b.focus_filter(window, cx));
            cx.subscribe(&browser, Self::handle_query_history_browser_event)
                .detach();
            self.overlays.query_history_browser = Some(browser);
        }
        cx.notify();
    }

    pub(super) fn handle_query_history_browser_event(
        &mut self,
        _view: Entity<crate::query_history_browser::QueryHistoryBrowser>,
        event: &crate::tool_dialog::ToolDialogEvent,
        cx: &mut Context<Self>,
    ) {
        if self.apply_tool_dialog_event(event, cx) {
            self.overlays.query_history_browser = None;
            self.restore_editor_focus(cx);
            cx.notify();
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn open_mcp_activity(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.overlays.mcp_activity_dialog.is_none() {
            let dialog = cx.new(|cx| crate::mcp_activity::McpActivityView::new(window, cx));
            cx.subscribe(&dialog, Self::handle_mcp_activity_event)
                .detach();
            self.overlays.mcp_activity_dialog = Some(dialog);
        }
        cx.notify();
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn handle_mcp_activity_event(
        &mut self,
        _view: Entity<crate::mcp_activity::McpActivityView>,
        event: &crate::tool_dialog::ToolDialogEvent,
        cx: &mut Context<Self>,
    ) {
        if self.apply_tool_dialog_event(event, cx) {
            self.overlays.mcp_activity_dialog = None;
            self.restore_editor_focus(cx);
            cx.notify();
        }
    }

    pub(super) fn handle_extended_events(
        &mut self,
        _action: &ExtendedEvents,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_extended_events(ToolScope::Server, window, cx);
    }

    pub(super) fn open_extended_events(
        &mut self,
        scope: ToolScope,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) =
            self.require_admin_tool(crate::capabilities::AdminTool::ExtendedEvents, cx)
        else {
            return;
        };
        if self.overlays.xevents_pane.is_none() {
            let context = crate::tool_dialog::ToolDialogContext::new(
                profile.clone(),
                self.services_for_profile(&profile),
            );
            let pane = cx.new(|cx| crate::xevents::ExtendedEventsPane::new(context, scope, cx));
            cx.subscribe(&pane, Self::handle_xevents_event).detach();
            self.overlays.xevents_pane = Some(pane);
        }
        cx.notify();
    }

    pub(super) fn handle_xevents_event(
        &mut self,
        _view: Entity<crate::xevents::ExtendedEventsPane>,
        event: &crate::tool_dialog::ToolDialogEvent,
        cx: &mut Context<Self>,
    ) {
        if self.apply_tool_dialog_event(event, cx) {
            self.overlays.xevents_pane = None;
            self.restore_editor_focus(cx);
            cx.notify();
        }
    }

    pub(super) fn handle_import_export(
        &mut self,
        _action: &ImportExport,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.overlays.import_export_dialog.is_none() {
            let database_type = self.active_db_type();
            let profile = self.active_profile.clone();
            let d = cx.new(|cx| {
                crate::import_export::ImportExportDialog::new_for_database(
                    database_type,
                    profile.as_ref(),
                    window,
                    cx,
                )
            });
            cx.subscribe(&d, Self::handle_import_export_event).detach();
            self.overlays.import_export_dialog = Some(d);
        }
        cx.notify();
    }

    pub(super) fn handle_import_from_clipboard(
        &mut self,
        _action: &ImportFromClipboard,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_import_from_clipboard(None, window, cx);
    }

    pub(super) fn open_import_from_clipboard(
        &mut self,
        schema: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        #[cfg(target_arch = "wasm32")]
        {
            self.set_status(
                StatusLevel::Error,
                crate::browser_preview::unavailable(&crate::localization::text(
                    "shell.tools.import_clipboard_unavailable",
                ))
                .to_string(),
            );
            cx.notify();
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
                self.set_status(
                    StatusLevel::Info,
                    crate::localization::text("shell.tools.import_clipboard_empty"),
                );
                cx.notify();
                return;
            };
            let database_type = self.active_db_type();
            let profile = self.active_profile.clone();
            let d = cx.new(|cx| {
                crate::import_export::ImportExportDialog::new_from_clipboard(
                    database_type,
                    text,
                    schema,
                    profile.as_ref(),
                    window,
                    cx,
                )
            });
            cx.subscribe(&d, Self::handle_import_export_event).detach();
            self.overlays.import_export_dialog = Some(d);
            cx.notify();
        }
    }

    pub(super) fn handle_import_export_event(
        &mut self,
        _view: Entity<crate::import_export::ImportExportDialog>,
        event: &crate::tool_dialog::ToolDialogEvent,
        cx: &mut Context<Self>,
    ) {
        if self.apply_tool_dialog_event(event, cx) {
            self.overlays.import_export_dialog = None;
            self.restore_editor_focus(cx);
            cx.notify();
        }
    }

    pub(super) fn handle_open_keybindings(
        &mut self,
        _action: &OpenKeybindings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(win) = &self.overlays.preferences_window {
            win.update(cx, |view, cx| {
                view.select_pane(crate::preferences::PreferencesPane::Keybindings, cx);
            });
            return;
        }
        let store = crate::preferences::PreferencesStore::new();
        let win = cx.new(|cx| crate::preferences_ui::PreferencesWindow::new(store, window, cx));
        cx.subscribe(&win, Self::handle_preferences_event).detach();
        win.update(cx, |view, cx| {
            view.select_pane(crate::preferences::PreferencesPane::Keybindings, cx);
        });
        self.overlays.preferences_window = Some(win);
        cx.notify();
    }

    pub(super) fn handle_open_mcp_server_settings(
        &mut self,
        _action: &OpenMcpServerSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(win) = &self.overlays.preferences_window {
            win.update(cx, |view, cx| {
                view.select_pane(crate::preferences::PreferencesPane::AiMcp, cx);
            });
            return;
        }
        let store = crate::preferences::PreferencesStore::new();
        let win = cx.new(|cx| crate::preferences_ui::PreferencesWindow::new(store, window, cx));
        cx.subscribe(&win, Self::handle_preferences_event).detach();
        win.update(cx, |view, cx| {
            view.select_pane(crate::preferences::PreferencesPane::AiMcp, cx);
        });
        self.overlays.preferences_window = Some(win);
        cx.notify();
    }

    /// Help ▸ Formatting Directives — opens the same reference window the
    /// editor's `-- sqlly …` hover popup links to, so the directive language
    /// is discoverable without already knowing its syntax.
    pub(super) fn handle_format_directives_reference(
        &mut self,
        _action: &FormatDirectivesReference,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        crate::format_directives::open_format_directives_window(cx);
    }

    #[cfg(target_arch = "wasm32")]
    pub(super) fn handle_about(
        &mut self,
        _action: &About,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.overlays.show_about(cx);
        cx.notify();
    }

    #[cfg(target_arch = "wasm32")]
    pub(super) fn handle_open_source_licenses(
        &mut self,
        _action: &OpenSourceLicenses,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.overlays.show_open_source_licenses(cx);
        cx.notify();
    }

    pub(super) fn handle_sqlly_help(
        &mut self,
        _action: &SqllyHelp,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Generated from the live command registry + keybinding maps (with
        // user overrides), so this window can never drift from real behavior.
        let body = crate::help_content::build_help_body();
        let view = cx.new(|_cx| {
            ToolOutputView::new(
                crate::localization::text("shell.tools.help_title"),
                body,
                false,
            )
        });
        cx.subscribe(&view, Self::handle_tool_output_event).detach();
        self.overlays.tool_output_view = Some(view);
        cx.notify();
    }

    pub(super) fn handle_search_database_value(
        &mut self,
        _action: &SearchDatabaseValue,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        #[cfg(target_arch = "wasm32")]
        {
            self.set_status(
                StatusLevel::Error,
                crate::browser_preview::unavailable(&crate::localization::text(
                    "shell.tools.database_search_unavailable",
                ))
                .to_string(),
            );
            cx.notify();
            return;
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let Some(profile) = self.active_profile.clone() else {
                self.set_status(
                    StatusLevel::Error,
                    crate::localization::text("shell.tools.no_active_connection_search"),
                );
                cx.notify();
                return;
            };
            if self.overlays.database_search_dialog.is_none() {
                let context = crate::tool_dialog::ToolDialogContext::new(
                    profile.clone(),
                    self.services_for_profile(&profile),
                );
                let d = cx.new(|cx| {
                    crate::database_search_dialog::DatabaseSearchDialog::new(context, window, cx)
                });
                cx.subscribe(&d, Self::handle_database_search_dialog_event)
                    .detach();
                self.overlays.database_search_dialog = Some(d);
            }
            cx.notify();
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn handle_database_search_dialog_event(
        &mut self,
        _view: Entity<crate::database_search_dialog::DatabaseSearchDialog>,
        event: &crate::tool_dialog::ToolDialogEvent,
        cx: &mut Context<Self>,
    ) {
        let close = self.apply_tool_dialog_event(event, cx);
        if close {
            self.overlays.database_search_dialog = None;
            self.restore_editor_focus(cx);
            cx.notify();
        }
    }

    pub(super) fn handle_run_sql_file(
        &mut self,
        _action: &RunSqlFile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        #[cfg(target_arch = "wasm32")]
        {
            self.set_status(
                StatusLevel::Error,
                crate::browser_preview::unavailable(&crate::localization::text(
                    "shell.tools.run_sql_file_unavailable",
                ))
                .to_string(),
            );
            cx.notify();
            return;
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let Some(profile) = self.active_profile.clone() else {
                self.set_status(
                    StatusLevel::Error,
                    crate::localization::text("shell.tools.no_active_connection_run_file"),
                );
                cx.notify();
                return;
            };
            let store = crate::connection::ConnectionStore::global();
            let safety =
                crate::mutation_review::SafetyFeatures::effective_for_profile(&profile, store);
            let production = safety
                .production_guard()
                .decide(self.production_unlock.as_ref(), std::time::Instant::now());
            if production == sqlly_intel::query::safety::ProductionDecision::ReadOnly {
                self.set_status(
                    StatusLevel::Error,
                    crate::localization::text("shell.tools.run_file_production_locked"),
                );
                cx.notify();
                return;
            }
            if self.overlays.run_sql_file_dialog.is_none() {
                let context = crate::tool_dialog::ToolDialogContext::new(
                    profile.clone(),
                    self.services_for_profile(&profile),
                );
                let d = cx.new(|cx| {
                    crate::run_sql_file_dialog::RunSqlFileDialog::new(
                        context,
                        self.overlays.pending_run_sql_file_path.take(),
                        window,
                        cx,
                    )
                });
                cx.subscribe(&d, Self::handle_run_sql_file_dialog_event)
                    .detach();
                self.overlays.run_sql_file_dialog = Some(d);
            }
            cx.notify();
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn handle_run_sql_file_dialog_event(
        &mut self,
        view: Entity<crate::run_sql_file_dialog::RunSqlFileDialog>,
        event: &crate::tool_dialog::ToolDialogEvent,
        cx: &mut Context<Self>,
    ) {
        let close = self.apply_tool_dialog_event(event, cx);
        if matches!(event, crate::tool_dialog::ToolDialogEvent::Status { .. }) {
            let body = view.update(cx, |dialog, _| dialog.take_last_summary());
            if let Some(body) = body {
                let level = match event {
                    crate::tool_dialog::ToolDialogEvent::Status { level, .. } => *level,
                    _ => StatusLevel::Info,
                };
                let message = match event {
                    crate::tool_dialog::ToolDialogEvent::Status { message, .. } => message.clone(),
                    _ => String::new(),
                };
                let kind = if level == StatusLevel::Error {
                    crate::notification_center::NotificationKind::QueryFailed
                } else {
                    crate::notification_center::NotificationKind::QueryCompleted
                };
                self.notifications.push(kind, message, chrono::Utc::now());
                self.show_tool_output(
                    &crate::localization::text("shell.tools.run_sql_file_title"),
                    body,
                    false,
                    cx,
                );
            }
        }
        if close {
            self.overlays.run_sql_file_dialog = None;
            self.restore_editor_focus(cx);
            cx.notify();
        }
    }

    pub(super) fn handle_backup_restore(
        &mut self,
        _action: &BackupRestore,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) =
            self.require_admin_tool(crate::capabilities::AdminTool::BackupRestore, cx)
        else {
            return;
        };
        let kind = profile.database_type.connection_kind();
        if matches!(
            kind,
            sqlly_lang::sqlmodel::ConnectionKind::PostgresWire
                | sqlly_lang::sqlmodel::ConnectionKind::MySqlWire
        ) {
            #[cfg(not(target_arch = "wasm32"))]
            {
                use sqlly_services::{NativeTool, find_tool};
                let tool = match kind {
                    sqlly_lang::sqlmodel::ConnectionKind::PostgresWire => NativeTool::PgRestore,
                    sqlly_lang::sqlmodel::ConnectionKind::MySqlWire => NativeTool::Mysql,
                    _ => NativeTool::PgRestore,
                };
                if find_tool(&crate::preferences::native_tool_locations(), tool).is_none() {
                    let name = tool.label();
                    self.set_status(
                        StatusLevel::Info,
                        crate::localization::format(
                            "shell.tools.native_restore_tool_missing",
                            &[("tool", name)],
                        ),
                    );
                    cx.notify();
                    return;
                }
                if self.overlays.native_restore_dialog.is_none() {
                    let context = crate::tool_dialog::ToolDialogContext::new(
                        profile.clone(),
                        self.services_for_profile(&profile),
                    );
                    let d = cx.new(|cx| {
                        crate::native_restore_dialog::NativeRestoreDialog::new(context, window, cx)
                    });
                    cx.subscribe(&d, Self::handle_native_restore_dialog_event)
                        .detach();
                    self.overlays.native_restore_dialog = Some(d);
                }
                cx.notify();
                return;
            }
        }
        if self.overlays.backup_restore_dialog.is_none() {
            // Route-aware services captured at open — a later profile switch
            // must not redirect the dialog's queries or scripts.
            let context = crate::tool_dialog::ToolDialogContext::new(
                profile.clone(),
                self.services_for_profile(&profile),
            );
            let d = cx.new(|cx| {
                crate::backup_restore_dialog::BackupRestoreDialog::new(context, window, cx)
            });
            cx.subscribe(&d, Self::handle_backup_restore_dialog_event)
                .detach();
            self.overlays.backup_restore_dialog = Some(d);
        }
        cx.notify();
    }

    /// Shared application of the tool-dialog contract: script lands in a new
    /// tab, status goes to the status bar. Returns true when the event asks
    /// the host to close the dialog.
    pub(super) fn apply_tool_dialog_event(
        &mut self,
        event: &crate::tool_dialog::ToolDialogEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        match event {
            crate::tool_dialog::ToolDialogEvent::ScriptToEditor { sql } => {
                self.new_tab(cx, Some(sql.clone()));
                self.set_status(
                    StatusLevel::Info,
                    crate::localization::text("shell.tools.scripted_new_tab"),
                );
                cx.notify();
                false
            }
            crate::tool_dialog::ToolDialogEvent::ScriptToEditorForProfile { sql, profile } => {
                let sql = sql.clone();
                self.with_explorer_target_active(
                    Some(profile.as_ref().clone()),
                    cx,
                    move |app, cx| {
                        app.new_tab(cx, Some(sql));
                        app.set_status(
                            StatusLevel::Info,
                            crate::localization::text("shell.tools.scripted_comparison_target"),
                        );
                        cx.notify();
                    },
                );
                false
            }
            crate::tool_dialog::ToolDialogEvent::ScriptToEditorForProfileAndRun {
                sql,
                profile,
            } => {
                let sql = sql.clone();
                self.refresh_explorer_after_run = true;
                self.with_explorer_target_active(
                    Some(profile.as_ref().clone()),
                    cx,
                    move |app, cx| {
                        app.new_tab(cx, Some(sql));
                        let tab_idx = app.active_tab;
                        app.execute_query(tab_idx, cx);
                        cx.notify();
                    },
                );
                false
            }
            crate::tool_dialog::ToolDialogEvent::ScriptToEditorAndRun { sql } => {
                // One-click re-run from the history browser: open the SQL in
                // a new tab and execute it immediately on the active
                // connection (every execution gate — params, safety review,
                // proc mode — still applies).
                self.new_tab(cx, Some(sql.clone()));
                let tab_idx = self.active_tab;
                self.execute_query(tab_idx, cx);
                cx.notify();
                false
            }
            crate::tool_dialog::ToolDialogEvent::AnalyzeWithAi { prompt } => {
                self.ai_panel.update(cx, |panel, cx| {
                    panel.open_ask_with_prompt(prompt.clone(), cx)
                });
                self.ai_visible = true;
                cx.notify();
                false
            }
            crate::tool_dialog::ToolDialogEvent::Status { level, message } => {
                self.set_status(*level, message.clone());
                cx.notify();
                false
            }
            crate::tool_dialog::ToolDialogEvent::DumpStarted { output_path } => {
                #[cfg(not(target_arch = "wasm32"))]
                {
                    let name = output_path
                        .file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_else(|| output_path.display().to_string());
                    let (id, progress) = self.background_tasks.start(
                        crate::background_tasks::TaskKind::Dump,
                        crate::localization::format(
                            "shell.tools.dump_task_label",
                            &[("name", &name)],
                        ),
                        Some(output_path.clone()),
                    );
                    self.dump_task_id = Some(id);
                    if let Some(dialog) = self.overlays.database_dump_dialog.as_ref() {
                        dialog.update(cx, |d, _| d.attach_task(progress));
                    }
                    cx.notify();
                }
                #[cfg(target_arch = "wasm32")]
                let _ = output_path;
                false
            }
            crate::tool_dialog::ToolDialogEvent::DumpCompleted {
                message,
                level,
                primary_path: _,
                cancelled,
            } => {
                #[cfg(not(target_arch = "wasm32"))]
                if let Some(id) = self.dump_task_id.take() {
                    let state = if *cancelled {
                        crate::background_tasks::TaskState::Cancelled
                    } else {
                        crate::background_tasks::TaskState::Finished
                    };
                    self.background_tasks.finish(id, state);
                }
                #[cfg(target_arch = "wasm32")]
                let _ = cancelled;
                self.set_status(*level, message.clone());
                if *level == StatusLevel::Success || *level == StatusLevel::Info {
                    self.record_export_saved(message.clone(), cx);
                }
                cx.notify();
                false
            }
            crate::tool_dialog::ToolDialogEvent::TransferStarted => {
                #[cfg(not(target_arch = "wasm32"))]
                {
                    let (id, progress) = self.background_tasks.start(
                        crate::background_tasks::TaskKind::Transfer,
                        crate::localization::text("shell.tools.data_transfer_task_label"),
                        None,
                    );
                    self.transfer_task_id = Some(id);
                    if let Some(pane) = self.overlays.data_transfer_pane.as_ref() {
                        pane.update(cx, |p, _| p.attach_task(progress));
                    }
                    cx.notify();
                }
                false
            }
            crate::tool_dialog::ToolDialogEvent::TransferFinished { cancelled, failed } => {
                #[cfg(not(target_arch = "wasm32"))]
                if let Some(id) = self.transfer_task_id.take() {
                    let state = if *failed {
                        crate::background_tasks::TaskState::Failed(crate::localization::text(
                            "shell.tools.data_transfer_failed",
                        ))
                    } else if *cancelled {
                        crate::background_tasks::TaskState::Cancelled
                    } else {
                        crate::background_tasks::TaskState::Finished
                    };
                    self.background_tasks.finish(id, state);
                }
                #[cfg(target_arch = "wasm32")]
                let _ = (cancelled, failed);
                false
            }
            crate::tool_dialog::ToolDialogEvent::SaveDumpSchedule { schedule } => {
                #[cfg(not(target_arch = "wasm32"))]
                {
                    let mut schedules = crate::dump_schedule::DumpSchedules::load();
                    if let Some(existing) =
                        schedules.schedules.iter_mut().find(|s| s.id == schedule.id)
                    {
                        existing.database = schedule.database.clone();
                        existing.output_dir = schedule.output_dir.clone();
                        existing.cadence = schedule.cadence;
                        existing.retention = schedule.retention;
                        existing.options = schedule.options.clone();
                        existing.kind = schedule.kind.clone();
                        existing.enabled = schedule.enabled;
                    } else {
                        schedules
                            .schedules
                            .push(crate::dump_schedule::DumpSchedule::clone(schedule));
                    }
                    if let Err(err) = schedules.save() {
                        self.set_status(StatusLevel::Error, err);
                        cx.notify();
                        return false;
                    }
                    self.dump_schedules = schedules;
                }
                self.set_status(
                    StatusLevel::Success,
                    crate::localization::format(
                        "shell.tools.schedule_saved",
                        &[("database", &schedule.database)],
                    ),
                );
                cx.notify();
                false
            }
            crate::tool_dialog::ToolDialogEvent::OpenTool(tool) => {
                use crate::tool_dialog::AdminToolLink;
                match tool {
                    AdminToolLink::ActivityMonitor => {
                        // The pane builds input fields, which need a
                        // window; open it on the next frame from the
                        // active one.
                        let app_view = cx.entity();
                        cx.defer(move |app| {
                            let Some(win) = app.active_window() else {
                                return;
                            };
                            let _ = win.update(app, |_root, window, app| {
                                app_view.update(app, |this, cx| {
                                    this.open_activity_monitor(ToolScope::Server, window, cx);
                                });
                            });
                        });
                    }
                    AdminToolLink::QueryInsights => self.open_query_store(cx),
                    AdminToolLink::DiskUsage => self.open_disk_usage(ToolScope::Server, cx),
                    AdminToolLink::RedisMemoryAnalyzer => self.open_redis_memory_analyzer(cx),
                    #[cfg(not(target_arch = "wasm32"))]
                    AdminToolLink::RedisClusterTopology => self.open_redis_cluster_topology(cx),
                    #[cfg(target_arch = "wasm32")]
                    AdminToolLink::RedisClusterTopology => {}
                }
                false
            }
            crate::tool_dialog::ToolDialogEvent::SaveSchemaSnapshot { profile } => {
                // The label prompt needs a window; open it on the next frame
                // from the active one (the pane may sit in a popped-out
                // window where dispatch would not reach here otherwise).
                let profile = profile.as_ref().clone();
                let app_view = cx.entity();
                cx.defer(move |app| {
                    let Some(win) = app.active_window() else {
                        return;
                    };
                    let _ = win.update(app, |_root, window, app| {
                        app_view.update(app, |this, cx| {
                            this.open_save_schema_snapshot(profile, window, cx);
                        });
                    });
                });
                false
            }
            crate::tool_dialog::ToolDialogEvent::Close => true,
            #[cfg(not(target_arch = "wasm32"))]
            crate::tool_dialog::ToolDialogEvent::RunSqlFile { path } => {
                self.overlays.pending_run_sql_file_path = Some(path.clone());
                self.overlays.pending_run_sql_file = true;
                false
            }
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn handle_native_restore_dialog_event(
        &mut self,
        _view: Entity<crate::native_restore_dialog::NativeRestoreDialog>,
        event: &crate::tool_dialog::ToolDialogEvent,
        cx: &mut Context<Self>,
    ) {
        if self.apply_tool_dialog_event(event, cx) {
            self.overlays.native_restore_dialog = None;
            self.restore_editor_focus(cx);
            cx.notify();
        }
    }

    pub(super) fn handle_backup_restore_dialog_event(
        &mut self,
        _view: Entity<crate::backup_restore_dialog::BackupRestoreDialog>,
        event: &crate::tool_dialog::ToolDialogEvent,
        cx: &mut Context<Self>,
    ) {
        if self.apply_tool_dialog_event(event, cx) {
            self.overlays.backup_restore_dialog = None;
            self.restore_editor_focus(cx);
            cx.notify();
        }
    }

    pub(super) fn handle_export_database_sql(
        &mut self,
        _action: &ExportDatabaseSql,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        #[cfg(target_arch = "wasm32")]
        {
            self.set_status(
                StatusLevel::Error,
                crate::browser_preview::unavailable(&crate::localization::text(
                    "shell.tools.dump_export_unavailable",
                ))
                .to_string(),
            );
            cx.notify();
            return;
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let database = self
                .active_profile
                .as_ref()
                .and_then(|p| p.database.clone());
            self.open_database_dump_dialog(database, window, cx);
        }
    }

    pub(super) fn handle_add_scheduled_dump(
        &mut self,
        _action: &AddScheduledDump,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        #[cfg(target_arch = "wasm32")]
        {
            let _ = window;
            self.set_status(
                StatusLevel::Error,
                crate::browser_preview::unavailable(&crate::localization::text(
                    "shell.tools.scheduled_dumps_unavailable",
                ))
                .to_string(),
            );
            cx.notify();
        }
        #[cfg(not(target_arch = "wasm32"))]
        self.open_database_dump_schedule_dialog(None, window, cx);
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn open_database_dump_dialog(
        &mut self,
        database: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self.active_profile.clone() else {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.tools.dump_connect"),
            );
            cx.notify();
            return;
        };
        if self.overlays.database_dump_dialog.is_none() {
            let context = crate::tool_dialog::ToolDialogContext::new(
                profile.clone(),
                self.services_for_profile(&profile),
            );
            let d = cx.new(|cx| {
                crate::database_dump_dialog::DatabaseDumpDialog::new_export(
                    context, database, window, cx,
                )
            });
            cx.subscribe(&d, Self::handle_database_dump_dialog_event)
                .detach();
            self.overlays.database_dump_dialog = Some(d);
        }
        cx.notify();
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn open_database_dump_schedule_dialog(
        &mut self,
        existing: Option<crate::dump_schedule::DumpSchedule>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let profile = if let Some(schedule) = existing.as_ref() {
            crate::connection::ConnectionStore::global().get(schedule.profile_id)
        } else {
            self.active_profile.clone()
        };
        let Some(profile) = profile else {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.tools.schedule_dump_connect"),
            );
            cx.notify();
            return;
        };
        if self.overlays.database_dump_dialog.is_none() {
            let context = crate::tool_dialog::ToolDialogContext::new(
                profile.clone(),
                self.services_for_profile(&profile),
            );
            let d = cx.new(|cx| {
                crate::database_dump_dialog::DatabaseDumpDialog::new_schedule(
                    context, existing, window, cx,
                )
            });
            cx.subscribe(&d, Self::handle_database_dump_dialog_event)
                .detach();
            self.overlays.database_dump_dialog = Some(d);
        }
        cx.notify();
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn handle_database_dump_dialog_event(
        &mut self,
        _view: Entity<crate::database_dump_dialog::DatabaseDumpDialog>,
        event: &crate::tool_dialog::ToolDialogEvent,
        cx: &mut Context<Self>,
    ) {
        let close = self.apply_tool_dialog_event(event, cx);
        if close {
            self.overlays.database_dump_dialog = None;
            self.restore_editor_focus(cx);
            cx.notify();
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn open_table_export_dialog(
        &mut self,
        database: String,
        schema: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self.active_profile.clone() else {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.tools.table_export_connect"),
            );
            cx.notify();
            return;
        };
        // A preselect staged by the bulk-operations handoff applies to a fresh
        // dialog (read when its listing lands) and to one already open; `take`
        // so a later standalone export never inherits it.
        let preselect = self.overlays.table_export_preselect.take();
        let context = crate::tool_dialog::ToolDialogContext::new(
            profile.clone(),
            self.services_for_profile(&profile),
        );
        if let Some(d) = self.overlays.table_export_dialog.as_ref() {
            // Already open — possibly on another connection or database:
            // retarget it to where the selection lives.
            d.update(cx, |d, cx| {
                d.retarget(context, database, schema, preselect, cx)
            });
        } else {
            let d = cx.new(|cx| {
                crate::table_export_dialog::TableExportDialog::new(
                    context, database, schema, preselect, window, cx,
                )
            });
            cx.subscribe(&d, Self::handle_table_export_dialog_event)
                .detach();
            self.overlays.table_export_dialog = Some(d);
        }
        cx.notify();
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn handle_table_export_dialog_event(
        &mut self,
        _view: Entity<crate::table_export_dialog::TableExportDialog>,
        event: &crate::table_export_dialog::TableExportDialogEvent,
        cx: &mut Context<Self>,
    ) {
        use crate::background_tasks::{TaskKind, TaskState};
        use crate::export_stream::{self, CANCELLED};
        use crate::table_export::plan_targets;

        match event {
            crate::table_export_dialog::TableExportDialogEvent::Close => {
                self.overlays.table_export_dialog = None;
                self.restore_editor_focus(cx);
                cx.notify();
            }
            crate::table_export_dialog::TableExportDialogEvent::Run { plan, profile } => {
                // The dialog's own connection: the active one may have moved
                // since it opened (or been retargeted by a bulk handoff).
                let profile = (**profile).clone();
                let db = profile.database_type;
                let date = chrono::Local::now().format("%Y%m%d").to_string();
                let targets = plan_targets(db, plan, &date);
                let n = targets.len();
                let folder = plan.folder.clone();
                let format = plan.format;
                let opts = plan.options.clone();
                let database = plan.database.clone();
                let services = self.services_for_profile(&profile);
                let label = crate::localization::format(
                    "shell.tools.table_export_task_label",
                    &[
                        ("count", &n.to_string()),
                        ("path", &folder.display().to_string()),
                    ],
                );
                let (id, progress) =
                    self.background_tasks
                        .start(TaskKind::Export, label, Some(folder.clone()));
                let (tx, mut rx) = tokio::sync::mpsc::channel::<
                    Vec<(
                        crate::table_export::TableExportTarget,
                        Result<u64, sqlly_core::DisplayMessage>,
                    )>,
                >(1);
                cx.background_spawn(async move {
                    crate::tokio_handle()
                        .spawn(async move {
                            let results = export_stream::run_table_exports(
                                services,
                                profile,
                                database,
                                db,
                                targets,
                                format,
                                opts,
                                progress.clone(),
                                |_index, _table| {},
                            )
                            .await;
                            let _ = tx.send(results).await;
                        })
                        .await
                        .ok();
                })
                .detach();
                let app = cx.entity();
                let async_cx = cx.to_async();
                cx.spawn(async move |_this, _cx| {
                    if let Some(results) = rx.recv().await {
                        let mut cx = async_cx.clone();
                        app.update(&mut cx, |this, cx| {
                            let mut ok = 0usize;
                            let mut failed = Vec::new();
                            let mut cancelled = false;
                            for (_target, outcome) in results {
                                // Worded here: the exports ran on the runtime.
                                let outcome = outcome.map_err(crate::localization::message);
                                match outcome {
                                    Ok(_) => ok += 1,
                                    Err(e) if e == CANCELLED => {
                                        cancelled = true;
                                        break;
                                    }
                                    Err(e) => failed.push(e),
                                }
                            }
                            let state = if cancelled {
                                TaskState::Cancelled
                            } else if failed.is_empty() {
                                TaskState::Finished
                            } else {
                                TaskState::Failed(crate::localization::format(
                                    "shell.tools.table_export_failures",
                                    &[("count", &failed.len().to_string())],
                                ))
                            };
                            this.background_tasks.finish(id, state);
                            let message = if cancelled {
                                crate::localization::text("shell.tools.table_export_cancelled")
                            } else if failed.is_empty() {
                                crate::localization::format(
                                    "shell.tools.table_export_succeeded",
                                    &[
                                        ("count", &ok.to_string()),
                                        ("path", &folder.display().to_string()),
                                    ],
                                )
                            } else {
                                crate::localization::format(
                                    "shell.tools.table_export_partial",
                                    &[
                                        ("succeeded", &ok.to_string()),
                                        ("total", &n.to_string()),
                                        ("path", &folder.display().to_string()),
                                        ("failed", &failed.len().to_string()),
                                    ],
                                )
                            };
                            let level = if failed.is_empty() && !cancelled {
                                StatusLevel::Success
                            } else if cancelled {
                                StatusLevel::Info
                            } else {
                                StatusLevel::Error
                            };
                            this.set_status(level, message.clone());
                            if level == StatusLevel::Success {
                                this.record_export_saved_with_path(message, folder, cx);
                            }
                            if !failed.is_empty() {
                                this.notifications.push(
                                    crate::notification_center::NotificationKind::QueryFailed,
                                    failed.join("; "),
                                    chrono::Utc::now(),
                                );
                            }
                            this.overlays.table_export_dialog = None;
                            this.restore_editor_focus(cx);
                            cx.notify();
                        });
                    }
                })
                .detach();
            }
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn open_database_dialog(
        &mut self,
        mode: crate::database_dialog::DatabaseDialogMode,
        server_id: Option<uuid::Uuid>,
        database: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let profile = self
            .resolve_explorer_target(server_id.as_ref(), database, cx)
            .or_else(|| self.active_profile.clone());
        let Some(profile) = profile else {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.tools.manage_databases_connect"),
            );
            cx.notify();
            return;
        };
        if let Some(reason) = crate::capabilities::database_ddl(profile.database_type).reason() {
            self.set_status(StatusLevel::Error, reason.to_string());
            cx.notify();
            return;
        }
        let store = crate::connection::ConnectionStore::global();
        let production_read_only =
            crate::mutation_review::SafetyFeatures::effective_for_profile(&profile, store)
                .production_guard()
                .decide(self.production_unlock.as_ref(), std::time::Instant::now())
                == sqlly_intel::query::safety::ProductionDecision::ReadOnly;
        if self.overlays.database_dialog.is_none() {
            let context = crate::tool_dialog::ToolDialogContext::new(
                profile.clone(),
                self.services_for_profile(&profile),
            );
            let d = cx.new(|cx| match mode {
                crate::database_dialog::DatabaseDialogMode::Create => {
                    crate::database_dialog::DatabaseDialog::new_create(
                        context,
                        production_read_only,
                        window,
                        cx,
                    )
                }
                crate::database_dialog::DatabaseDialogMode::Drop { name, is_active } => {
                    crate::database_dialog::DatabaseDialog::new_drop(
                        context,
                        name,
                        is_active,
                        production_read_only,
                        window,
                        cx,
                    )
                }
            });
            cx.subscribe(&d, Self::handle_database_dialog_event)
                .detach();
            self.overlays.database_dialog = Some(d);
        }
        cx.notify();
    }

    /// The production guard's decision for `profile` now, plus a check a
    /// tool can re-run right before it writes (the unlock may have expired
    /// since it opened). A dropped workbench reads as locked.
    pub(super) fn production_check(
        &self,
        profile: &ConnectionProfile,
        cx: &mut Context<Self>,
    ) -> (
        sqlly_intel::query::safety::ProductionDecision,
        crate::tool_dialog::ProductionCheck,
    ) {
        fn decide(
            profile: &ConnectionProfile,
            unlock: Option<&sqlly_intel::query::safety::WriteUnlock>,
        ) -> sqlly_intel::query::safety::ProductionDecision {
            crate::mutation_review::SafetyFeatures::effective_for_profile(
                profile,
                crate::connection::ConnectionStore::global(),
            )
            .production_guard()
            .decide(unlock, sqlly_intel::query::safety::Instant::now())
        }
        let now = decide(profile, self.production_unlock.as_ref());
        let app = cx.weak_entity();
        let profile = profile.clone();
        let check: crate::tool_dialog::ProductionCheck = std::rc::Rc::new(move |cx: &gpui::App| {
            app.upgrade().map_or(
                sqlly_intel::query::safety::ProductionDecision::ReadOnly,
                |app| decide(&profile, app.read(cx).production_unlock.as_ref()),
            )
        });
        (now, check)
    }

    /// Open one of the Redis key browser's prompts on the node's own
    /// connection (the node's server and database, never the active tab's).
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn open_redis_key_dialog(
        &mut self,
        mode: crate::redis_key_dialog::RedisKeyDialogMode,
        context: &crate::schema_tree::ExplorerActionContext,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let profile = self
            .resolve_explorer_target(context.server_id.as_ref(), context.database.as_deref(), cx)
            .or_else(|| context.profile.as_deref().cloned());
        let Some(profile) = profile else {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.tools.redis_connect_dialog"),
            );
            cx.notify();
            return;
        };
        if self.overlays.redis_key_dialog.is_none() {
            let dialog_context = crate::tool_dialog::ToolDialogContext::new(
                profile.clone(),
                self.services_for_profile(&profile),
            );
            // The dialog writes through `tool_dialog::run_query`, which skips
            // the editor's run gate — hand it the production guard so a
            // locked production connection opens with writes disabled and
            // an unlock that expires mid-dialog is honoured at apply time.
            let (production, production_check) = self.production_check(&profile, cx);
            let d = cx.new(|cx| {
                crate::redis_key_dialog::RedisKeyDialog::new(
                    dialog_context,
                    mode,
                    production,
                    production_check,
                    window,
                    cx,
                )
            });
            cx.subscribe(&d, Self::handle_redis_key_dialog_event)
                .detach();
            self.overlays.redis_key_dialog = Some(d);
        }
        cx.notify();
    }

    /// Open the Redis key dialog on keys selected in a results grid (a key
    /// listing such as `SQLLY.KEYS`): delete them, set their TTL, or remove
    /// their expiry. The keys act on the connection and database the active
    /// tab runs against — the grid must belong to that tab, so a background
    /// tab's results can never be applied to another connection.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn open_redis_selected_keys_dialog(
        &mut self,
        results: &gpui::WeakEntity<crate::results::ResultsView>,
        keys: Vec<Vec<u8>>,
        cluster: Option<bool>,
        action: crate::result_grid_menu::SelectedKeysAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::result_grid_menu::SelectedKeysAction as Action;
        let in_active_tab = self.tabs.get(self.active_tab).is_some_and(|tab| {
            results
                .upgrade()
                .is_some_and(|r| r.entity_id() == tab.results.entity_id())
        });
        let profile = self
            .active_profile
            .clone()
            .filter(|p| in_active_tab && p.database_type.is_command_engine());
        let Some(profile) = profile else {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.tools.redis_selected_keys_no_connection"),
            );
            cx.notify();
            return;
        };
        if keys.is_empty() {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.tools.redis_selected_keys_none"),
            );
            cx.notify();
            return;
        }
        if self.overlays.redis_key_dialog.is_some() {
            return;
        }
        // A listing that cannot say whether it came from a cluster batches
        // deletes by hash slot unless the connection is pinned standalone.
        let cluster = cluster.unwrap_or_else(|| {
            profile.redis_cluster.as_ref().map(|c| c.mode)
                != Some(sqlly_core::redis_cluster::RedisClusterMode::Standalone)
        });
        let mode = match action {
            Action::Delete => crate::redis_key_dialog::RedisKeyDialogMode::BulkDelete {
                node_id: String::new(),
                pattern: String::new(),
            },
            Action::SetTtl | Action::Persist => {
                crate::redis_key_dialog::RedisKeyDialogMode::BatchTtl {
                    node_id: String::new(),
                    pattern: String::new(),
                }
            }
        };
        let persist = action == Action::Persist;
        let dialog_context = crate::tool_dialog::ToolDialogContext::new(
            profile.clone(),
            self.services_for_profile(&profile),
        );
        let (production, production_check) = self.production_check(&profile, cx);
        let d = cx.new(|cx| {
            crate::redis_key_dialog::RedisKeyDialog::new(
                dialog_context,
                mode,
                production,
                production_check,
                window,
                cx,
            )
            .with_selected_keys(keys, cluster, persist)
        });
        cx.subscribe(&d, Self::handle_redis_key_dialog_event)
            .detach();
        self.overlays.redis_key_dialog = Some(d);
        cx.notify();
    }

    /// Open the key browser's rename / copy / export / name-database dialog
    /// on the node's own connection.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn open_redis_key_action(
        &mut self,
        action: &crate::schema_tree::ExplorerMenuAction,
        context: &crate::schema_tree::ExplorerActionContext,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::redis_key_actions::RedisKeyActionMode as Mode;
        use crate::schema_tree::ExplorerMenuAction as A;
        use crate::schema_tree::NodeKind;
        let profile = self
            .resolve_explorer_target(context.server_id.as_ref(), context.database.as_deref(), cx)
            .or_else(|| context.profile.as_deref().cloned());
        let Some(profile) = profile else {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.tools.redis_connect_action"),
            );
            cx.notify();
            return;
        };
        // A key's listing to reload is its parent (folder or database).
        let parent = context
            .node_id
            .rsplit_once(":k:")
            .map(|(parent, _)| parent.to_string())
            .unwrap_or_else(|| context.node_id.clone());
        let key = match &context.kind {
            NodeKind::RedisKey { key, .. } => Some(key.clone()),
            _ => None,
        };
        let current_db = sqlly_engines::redis::database_index(&profile).unwrap_or(0);
        let mode = match action {
            A::RedisRenameKey => Mode::Rename {
                node_id: parent,
                key: match key {
                    Some(key) => key,
                    None => return,
                },
            },
            A::RedisCopyToDatabase => Mode::CopyToDatabase {
                node_id: parent,
                key: match key {
                    Some(key) => key,
                    None => return,
                },
                current_db,
                aliases: profile.redis_db_aliases.clone(),
            },
            A::RedisCopyToConnection => {
                let targets: Vec<ConnectionProfile> = crate::connection::ConnectionStore::global()
                    .list()
                    .into_iter()
                    .filter(|p| p.database_type.is_command_engine())
                    .collect();
                let app = cx.weak_entity();
                let fallback = self.services_for_profile(&profile);
                let resolver: crate::redis_key_actions::TargetResolver =
                    std::rc::Rc::new(move |target: &ConnectionProfile, cx: &gpui::App| {
                        let decide = |unlock: Option<&sqlly_intel::query::safety::WriteUnlock>| {
                            crate::mutation_review::SafetyFeatures::effective_for_profile(
                                target,
                                crate::connection::ConnectionStore::global(),
                            )
                            .production_guard()
                            .decide(unlock, sqlly_intel::query::safety::Instant::now())
                        };
                        match app.upgrade() {
                            Some(app) => {
                                let app = app.read(cx);
                                (
                                    crate::tool_dialog::ToolDialogContext::new(
                                        target.clone(),
                                        app.services_for_profile(target),
                                    ),
                                    decide(app.production_unlock.as_ref()),
                                )
                            }
                            None => (
                                crate::tool_dialog::ToolDialogContext::new(
                                    target.clone(),
                                    fallback.clone(),
                                ),
                                sqlly_intel::query::safety::ProductionDecision::ReadOnly,
                            ),
                        }
                    });
                Mode::CopyToConnection {
                    node_id: parent,
                    key: match key {
                        Some(key) => key,
                        None => return,
                    },
                    targets,
                    resolver,
                }
            }
            A::RedisExportCommands => {
                let pattern = match &context.kind {
                    NodeKind::RedisKeyFolder { prefix } => {
                        crate::schema_tree::redis::prefix_pattern(prefix)
                    }
                    NodeKind::Database(_) => self
                        .sidebar
                        .read(cx)
                        .redis_key_filter(&context.node_id, cx)
                        .unwrap_or_else(|| "*".to_string()),
                    _ => return,
                };
                Mode::ExportFolder {
                    node_id: context.node_id.clone(),
                    pattern,
                }
            }
            A::RedisNameDatabase => {
                let (NodeKind::Database(name), Some(server_id)) =
                    (&context.kind, context.server_id)
                else {
                    return;
                };
                let Ok(db) = name.trim().parse::<i64>() else {
                    return;
                };
                let saved = crate::connection::ConnectionStore::global().get(server_id);
                let current = saved
                    .as_ref()
                    .unwrap_or(&profile)
                    .redis_db_aliases
                    .get(&db)
                    .cloned();
                Mode::NameDatabase {
                    node_id: context.node_id.clone(),
                    server_id,
                    db,
                    current,
                }
            }
            A::RedisVisibleDatabases => {
                let Some(server_id) = context.server_id else {
                    return;
                };
                let current = crate::connection::ConnectionStore::global()
                    .get(server_id)
                    .unwrap_or_else(|| profile.clone())
                    .redis_visible_databases;
                Mode::VisibleDatabases {
                    node_id: context.node_id.clone(),
                    server_id,
                    current,
                }
            }
            _ => return,
        };
        let dialog_context = crate::tool_dialog::ToolDialogContext::new(
            profile.clone(),
            self.services_for_profile(&profile),
        );
        let (production, production_check) = self.production_check(&profile, cx);
        let d = cx.new(|cx| {
            crate::redis_key_actions::RedisKeyActionDialog::new(
                dialog_context,
                mode,
                production,
                production_check,
                window,
                cx,
            )
        });
        cx.subscribe(&d, Self::handle_redis_key_action_event)
            .detach();
        self.overlays.redis_key_action_dialog = Some(d);
        cx.notify();
    }

    /// Change a saved Redis profile's explorer settings (database names,
    /// visible databases), keep the active copy in step, and reload the
    /// server node that lists the databases.
    #[cfg(not(target_arch = "wasm32"))]
    fn update_redis_profile_view(
        &mut self,
        server_id: uuid::Uuid,
        database_node: &str,
        message: String,
        cx: &mut Context<Self>,
        change: impl Fn(&mut ConnectionProfile),
    ) {
        let store = crate::connection::ConnectionStore::global();
        let Some(mut saved) = store.get(server_id) else {
            self.set_status(
                StatusLevel::Error,
                crate::localization::text("shell.tools.redis_save_before_database_settings"),
            );
            return;
        };
        change(&mut saved);
        store.update(server_id, saved);
        if let Some(active) = self.active_profile.as_mut().filter(|p| p.id.0 == server_id) {
            change(active);
        }
        let server_node = database_node
            .rsplit_once(":db:")
            .map(|(server, _)| server.to_string())
            .unwrap_or_else(|| database_node.to_string());
        self.sidebar.update(cx, |s, cx| {
            s.refresh_connections(cx);
            s.refresh_schema_node(&server_node, cx);
        });
        self.set_status(StatusLevel::Success, message);
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn handle_redis_key_action_event(
        &mut self,
        _view: Entity<crate::redis_key_actions::RedisKeyActionDialog>,
        event: &crate::redis_key_actions::RedisKeyActionEvent,
        cx: &mut Context<Self>,
    ) {
        use crate::redis_key_actions::RedisKeyActionEvent;
        match event {
            RedisKeyActionEvent::Changed { node_id, message } => {
                let node_id = node_id.clone();
                self.sidebar.update(cx, |s, cx| {
                    s.refresh_schema_node(&node_id, cx);
                });
                self.set_status(StatusLevel::Success, message.clone());
            }
            RedisKeyActionEvent::NameDatabase {
                node_id,
                server_id,
                db,
                alias,
            } => {
                let (db, alias) = (*db, alias.clone());
                let message = match &alias {
                    Some(alias) => crate::localization::format(
                        "shell.tools.redis_database_named",
                        &[("database", &db.to_string()), ("alias", alias)],
                    ),
                    None => crate::localization::format(
                        "shell.tools.redis_database_name_removed",
                        &[("database", &db.to_string())],
                    ),
                };
                self.update_redis_profile_view(
                    *server_id,
                    node_id,
                    message,
                    cx,
                    |p| match &alias {
                        Some(alias) => {
                            p.redis_db_aliases.insert(db, alias.clone());
                        }
                        None => {
                            p.redis_db_aliases.remove(&db);
                        }
                    },
                );
            }
            RedisKeyActionEvent::VisibleDatabases {
                node_id,
                server_id,
                visible,
            } => {
                let visible = visible.clone();
                let message = match &visible {
                    Some(list) => crate::localization::format(
                        "shell.tools.redis_visible_databases",
                        &[(
                            "databases",
                            &crate::schema_tree::redis::visible_databases_text(Some(list)),
                        )],
                    ),
                    None => crate::localization::text("shell.tools.redis_all_databases"),
                };
                self.update_redis_profile_view(*server_id, node_id, message, cx, |p| {
                    p.redis_visible_databases = visible.clone();
                });
            }
            RedisKeyActionEvent::Close => {
                self.overlays.redis_key_action_dialog = None;
                self.restore_editor_focus(cx);
            }
        }
        cx.notify();
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn handle_redis_key_dialog_event(
        &mut self,
        _view: Entity<crate::redis_key_dialog::RedisKeyDialog>,
        event: &crate::redis_key_dialog::RedisKeyDialogEvent,
        cx: &mut Context<Self>,
    ) {
        use crate::redis_key_dialog::RedisKeyDialogEvent;
        match event {
            RedisKeyDialogEvent::Filter { node_id, pattern } => {
                let node_id = node_id.clone();
                let pattern = pattern.clone();
                self.sidebar.update(cx, |s, cx| {
                    s.set_redis_key_filter(&node_id, pattern, cx);
                });
                self.set_status(
                    StatusLevel::Info,
                    crate::localization::text("shell.tools.redis_filter_applied"),
                );
            }
            RedisKeyDialogEvent::Changed { node_id, message } => {
                // Selected-keys runs from a results grid have no node.
                if !node_id.is_empty() {
                    let node_id = node_id.clone();
                    self.sidebar.update(cx, |s, cx| {
                        s.refresh_schema_node(&node_id, cx);
                    });
                }
                self.set_status(StatusLevel::Success, message.clone());
            }
            RedisKeyDialogEvent::SearchValues { script, profile } => {
                // Pinned to the dialog's own connection, never the active tab's.
                let script = script.clone();
                self.with_explorer_target_active(
                    Some(profile.as_ref().clone()),
                    cx,
                    move |app, cx| {
                        app.new_tab(cx, Some(script));
                        app.set_status(
                            StatusLevel::Info,
                            crate::localization::text("shell.tools.redis_value_search_opened"),
                        );
                        cx.notify();
                    },
                );
            }
            RedisKeyDialogEvent::Close => {
                self.overlays.redis_key_dialog = None;
                self.restore_editor_focus(cx);
            }
        }
        cx.notify();
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn handle_database_dialog_event(
        &mut self,
        _view: Entity<crate::database_dialog::DatabaseDialog>,
        event: &crate::tool_dialog::ToolDialogEvent,
        cx: &mut Context<Self>,
    ) {
        let close = self.apply_tool_dialog_event(event, cx);
        if close {
            self.overlays.database_dialog = None;
            self.restore_editor_focus(cx);
            cx.notify();
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn tick_dump_schedules(&mut self, cx: &mut Context<Self>) {
        if self.dump_schedule_active_id.is_some() {
            return;
        }
        let now = chrono::Utc::now();
        let due_id = self
            .dump_schedules
            .schedules
            .iter()
            .filter(|s| crate::dump_schedule::is_due(s, now))
            .find(|s| !self.dump_runs.contains_key(&s.id))
            .map(|s| s.id);
        if let Some(id) = due_id {
            // Production, missing-connection and setup problems are recorded
            // on the schedule (last run + result), so a skipped slot is
            // visible in Settings and retried at the next cadence slot
            // instead of being re-checked every tick forever.
            self.start_scheduled_run(id, false, cx);
        }
    }

    /// Persist the in-memory schedules; a refused or failed save (for
    /// example a corrupt `dump-schedules.json` on disk) surfaces as an error
    /// instead of vanishing.
    #[cfg(not(target_arch = "wasm32"))]
    fn persist_dump_schedules(&mut self) {
        if let Err(err) = self.dump_schedules.save() {
            self.set_status(StatusLevel::Error, err);
        }
    }

    /// Stamp a run's outcome on the schedule and persist it.
    #[cfg(not(target_arch = "wasm32"))]
    fn record_schedule_result(&mut self, id: uuid::Uuid, result: String) {
        if let Some(schedule) = self
            .dump_schedules
            .schedules
            .iter_mut()
            .find(|s| s.id == id)
        {
            schedule.last_run = Some(chrono::Utc::now());
            schedule.last_result = Some(result);
        }
        self.persist_dump_schedules();
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn disable_dump_schedule(&mut self, id: uuid::Uuid, reason: &str, cx: &mut Context<Self>) {
        if let Some(schedule) = self
            .dump_schedules
            .schedules
            .iter_mut()
            .find(|s| s.id == id)
        {
            schedule.enabled = false;
            schedule.last_result = Some(reason.to_string());
        }
        self.persist_dump_schedules();
        cx.notify();
    }

    /// Settings ▸ Scheduled tasks ▸ Run now.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn run_scheduled_dump_now(
        &mut self,
        schedule_id: uuid::Uuid,
        cx: &mut Context<Self>,
    ) {
        self.start_scheduled_run(schedule_id, true, cx);
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn start_scheduled_run(
        &mut self,
        schedule_id: uuid::Uuid,
        manual: bool,
        cx: &mut Context<Self>,
    ) {
        use crate::dump_schedule::{QueryExportFormat, ScheduleKind};
        if self.dump_runs.contains_key(&schedule_id) || self.dump_schedule_active_id.is_some() {
            if manual {
                let message = if self.dump_runs.contains_key(&schedule_id) {
                    crate::localization::text("shell.tools.schedule_already_running")
                } else {
                    crate::localization::text("shell.tools.schedule_another_running")
                };
                self.set_status(StatusLevel::Info, message);
                cx.notify();
            }
            return;
        }
        let schedule = self
            .dump_schedules
            .schedules
            .iter()
            .find(|s| s.id == schedule_id)
            .cloned();
        let Some(schedule) = schedule else {
            return;
        };
        let store = crate::connection::ConnectionStore::global();
        let Some(profile) = store.get(schedule.profile_id) else {
            self.disable_dump_schedule(
                schedule_id,
                &crate::localization::text("shell.tools.schedule_connection_missing"),
                cx,
            );
            return;
        };
        let mut profile = profile;
        profile.database = Some(schedule.database.clone());
        let safety = crate::mutation_review::SafetyFeatures::effective_for_profile(&profile, store);
        if safety.is_production {
            let reason = crate::localization::text("shell.tools.schedule_production_skipped");
            self.record_schedule_result(schedule_id, reason.clone());
            self.set_status(
                StatusLevel::Info,
                crate::localization::format(
                    "shell.tools.schedule_production_skipped_database",
                    &[("database", &schedule.database)],
                ),
            );
            cx.notify();
            return;
        }

        let now = chrono::Utc::now();
        let conn_kind = profile.database_type.connection_kind();
        let output = schedule
            .output_dir
            .join(crate::dump_schedule::scheduled_file_name(
                &schedule, now, conn_kind,
            ));

        enum ScheduledJob {
            Builtin,
            // Boxed: the request carries a whole connection profile and would
            // otherwise make every variant (and `Prepared`) that large.
            Native {
                tool: sqlly_services::ToolLocation,
                req: Box<sqlly_services::NativeDumpRequest>,
            },
            Query {
                sql: String,
                format: crate::results::ExportFormat,
            },
        }
        enum Prepared {
            Run(ScheduledJob),
            /// Transient setup problem (missing tool, non-SELECT query):
            /// recorded as the run's error, retried at the next cadence slot.
            Fail(String),
            /// Permanent setup loss (the saved query was deleted): the
            /// schedule disables itself and says why.
            Disable(String),
        }

        let prepared = match &schedule.kind {
            ScheduleKind::BuiltinDump => Prepared::Run(ScheduledJob::Builtin),
            ScheduleKind::NativeBackup { .. } => {
                let req = crate::dump_schedule::native_dump_request(&schedule, output.clone())
                    .expect("native kind yields a request");
                let tool_kind = match conn_kind {
                    sqlly_lang::sqlmodel::ConnectionKind::PostgresWire => {
                        Some(sqlly_services::NativeTool::PgDump)
                    }
                    sqlly_lang::sqlmodel::ConnectionKind::MySqlWire => {
                        Some(sqlly_services::NativeTool::MysqlDump)
                    }
                    _ => None,
                };
                if let Some(reason) = direct_route_refusal(
                    &profile,
                    &crate::localization::text("shell.tools.native_backup_feature"),
                ) {
                    Prepared::Fail(reason)
                } else {
                    match tool_kind {
                        None => Prepared::Fail(crate::localization::format(
                            "shell.tools.schedule_native_backup_unsupported",
                            &[("engine", profile.database_type.display_name())],
                        )),
                        Some(tool_kind) => {
                            match sqlly_services::find_tool(
                                &crate::preferences::native_tool_locations(),
                                tool_kind,
                            ) {
                                Some(tool) => Prepared::Run(ScheduledJob::Native {
                                    tool: tool.clone(),
                                    req: Box::new(req),
                                }),
                                None => Prepared::Fail(crate::localization::format(
                                    "shell.tools.schedule_native_tool_missing",
                                    &[("tool", tool_kind.binary_name())],
                                )),
                            }
                        }
                    }
                }
            }
            ScheduleKind::QueryExport {
                query_path,
                query_name,
                format,
            } => {
                // The user's effective library folder: Preferences can move
                // it away from the default location.
                let full_path = crate::preferences::SqlLibraryPreferences::load()
                    .effective_root()
                    .join(query_path);
                match std::fs::read_to_string(&full_path) {
                    Err(_) => Prepared::Disable(crate::localization::format(
                        "shell.tools.schedule_query_missing",
                        &[("query", query_name)],
                    )),
                    Ok(text) => {
                        // The library's `-- sqlly:` front-matter line is a
                        // comment the server tolerates, but strip it so the
                        // read-only gate classifies the query itself.
                        let sql = if text.starts_with(crate::sql_library_model::FRONT_MATTER_PREFIX)
                        {
                            text.split_once('\n')
                                .map(|(_, rest)| rest.to_string())
                                .unwrap_or_default()
                        } else {
                            text
                        };
                        let class = sqlly_intel::query::safety::classify_select_only_for(
                            profile.database_type,
                            &sql,
                        );
                        if !class.select_only {
                            Prepared::Fail(crate::localization::format(
                                "shell.tools.schedule_query_not_read_only",
                                &[
                                    ("query", query_name),
                                    (
                                        "reason",
                                        &class.reason.unwrap_or_else(|| {
                                            crate::localization::text(
                                                "shell.tools.schedule_not_read_only_fallback",
                                            )
                                        }),
                                    ),
                                ],
                            ))
                        } else {
                            Prepared::Run(ScheduledJob::Query {
                                sql,
                                format: match format {
                                    QueryExportFormat::Csv => crate::results::ExportFormat::Csv,
                                    QueryExportFormat::JsonLines => {
                                        crate::results::ExportFormat::JsonLines
                                    }
                                },
                            })
                        }
                    }
                }
            }
        };
        // Every runner writes into the folder; none of the vendor tools or
        // the export sink creates it.
        let prepared = match prepared {
            Prepared::Run(job) => match std::fs::create_dir_all(&schedule.output_dir) {
                Ok(()) => Prepared::Run(job),
                Err(err) => Prepared::Fail(crate::localization::format(
                    "shell.tools.schedule_output_folder_failed",
                    &[
                        ("path", &schedule.output_dir.display().to_string()),
                        ("error", &err.to_string()),
                    ],
                )),
            },
            other => other,
        };

        let job = match prepared {
            Prepared::Run(job) => job,
            Prepared::Fail(reason) => {
                self.record_schedule_result(
                    schedule_id,
                    crate::localization::format(
                        "shell.tools.schedule_result_error",
                        &[("error", &reason)],
                    ),
                );
                self.set_status(
                    StatusLevel::Error,
                    crate::localization::format(
                        "shell.tools.schedule_failed",
                        &[("database", &schedule.database), ("error", &reason)],
                    ),
                );
                cx.notify();
                return;
            }
            Prepared::Disable(reason) => {
                self.disable_dump_schedule(schedule_id, &reason, cx);
                return;
            }
        };

        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.dump_runs.insert(schedule_id, cancel.clone());
        self.dump_schedule_active_id = Some(schedule_id);

        let services = self.services_for_profile(&profile);
        let opts = schedule.options.clone();
        let database = schedule.database.clone();
        let kind_label = match &schedule.kind {
            ScheduleKind::BuiltinDump => {
                crate::localization::text("shell.tools.schedule_kind_dump")
            }
            ScheduleKind::NativeBackup { .. } => {
                crate::localization::text("shell.tools.schedule_kind_backup")
            }
            ScheduleKind::QueryExport { .. } => {
                crate::localization::text("shell.tools.schedule_kind_export")
            }
        };
        let schedule_for_retention = schedule.clone();
        let weak = cx.weak_entity();
        let async_cx = cx.to_async();

        cx.spawn(async move |_this, _cx| {
            let mut progress_fn = |_p: sqlly_services::DumpProgress| {};
            let result: Result<sqlly_services::DumpReport, String> = match job {
                ScheduledJob::Builtin => sqlly_services::run_database_dump(
                    &services,
                    &profile,
                    &database,
                    &output,
                    &opts,
                    cancel,
                    &mut progress_fn,
                )
                .await
                .map_err(|err| crate::localization::message(err.message())),
                ScheduledJob::Native { tool, req } => sqlly_services::run_native_dump(
                    &services,
                    &profile,
                    &tool,
                    &req,
                    cancel,
                    &mut progress_fn,
                )
                .await
                .map_err(|err| crate::localization::message(err.message())),
                ScheduledJob::Query { sql, format } => {
                    let started = std::time::Instant::now();
                    let mut query_profile = profile;
                    query_profile.block_all_but_select = true;
                    query_profile.enforce_read_only = true;
                    let db_type = query_profile.database_type;
                    let export_opts = crate::export_options::ExportOptions::default_for(format);
                    match crate::export_stream::export_sql_to_file(
                        &services,
                        &query_profile,
                        &sql,
                        &output,
                        format,
                        db_type,
                        &export_opts,
                    )
                    .await
                    {
                        Ok(rows) => Ok(sqlly_services::DumpReport {
                            files: vec![output.clone()],
                            rows_written: rows,
                            bytes_written: std::fs::metadata(&output)
                                .map(|meta| meta.len())
                                .unwrap_or(0),
                            warnings: Vec::new(),
                            elapsed_ms: started.elapsed().as_millis(),
                            cancelled: false,
                        }),
                        Err(err) => Err(crate::localization::message(err)),
                    }
                }
            };

            let mut cx = async_cx.clone();
            weak.update(&mut cx, |app, cx| {
                app.dump_runs.remove(&schedule_id);
                app.dump_schedule_active_id = None;
                let now = chrono::Utc::now();
                match result {
                    Ok(report) => {
                        // Prune only after a complete run: a cancelled or
                        // partial run must never cost an older good copy.
                        if !report.cancelled {
                            let _ = crate::dump_schedule::apply_retention(
                                &schedule_for_retention.output_dir,
                                &schedule_for_retention,
                                schedule_for_retention.retention,
                            );
                        }
                        let result_line = if report.cancelled {
                            crate::localization::format(
                                "shell.tools.schedule_result_cancelled",
                                &[("path", &output.display().to_string())],
                            )
                        } else {
                            crate::localization::format(
                                "shell.tools.schedule_result_success",
                                &[
                                    ("count", &report.rows_written.to_string()),
                                    ("path", &output.display().to_string()),
                                ],
                            )
                        };
                        app.record_schedule_result(schedule_id, result_line);
                        let message = if report.cancelled {
                            crate::localization::format(
                                "shell.tools.schedule_cancelled",
                                &[("kind", &kind_label), ("database", &database)],
                            )
                        } else {
                            crate::localization::format(
                                "shell.tools.schedule_finished",
                                &[("kind", &kind_label), ("database", &database)],
                            )
                        };
                        let level = if report.cancelled {
                            StatusLevel::Info
                        } else {
                            StatusLevel::Success
                        };
                        app.set_status(level, message.clone());
                        app.notifications.push(
                            crate::notification_center::NotificationKind::ExportSaved,
                            message,
                            now,
                        );
                    }
                    Err(err) => {
                        app.record_schedule_result(
                            schedule_id,
                            crate::localization::format(
                                "shell.tools.schedule_runtime_result_error",
                                &[("error", &err)],
                            ),
                        );
                        let message = crate::localization::format(
                            "shell.tools.schedule_kind_failed",
                            &[
                                ("kind", &kind_label),
                                ("database", &database),
                                ("error", &err),
                            ],
                        );
                        app.set_status(StatusLevel::Error, message.clone());
                        app.notifications.push(
                            crate::notification_center::NotificationKind::QueryFailed,
                            message,
                            now,
                        );
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn remove_scheduled_dump(
        &mut self,
        schedule_id: uuid::Uuid,
        cx: &mut Context<Self>,
    ) {
        self.dump_schedules
            .schedules
            .retain(|s| s.id != schedule_id);
        self.persist_dump_schedules();
        self.dump_runs.remove(&schedule_id);
        cx.notify();
    }

    /// Feedback for Settings ▸ Scheduled tasks ▸ Copy CLI command.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn note_schedule_cli_copied(&mut self, cx: &mut Context<Self>) {
        self.set_status(
            StatusLevel::Success,
            crate::cli_command::copied_status().to_string(),
        );
        cx.notify();
    }

    /// Reopen a saved schedule in the schedule editor (Settings ▸ Scheduled
    /// tasks ▸ Edit…). Saving replaces the schedule by id.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn edit_scheduled_dump(
        &mut self,
        schedule_id: uuid::Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let existing = self
            .dump_schedules
            .schedules
            .iter()
            .find(|s| s.id == schedule_id)
            .cloned();
        if existing.is_some() {
            self.open_database_dump_schedule_dialog(existing, window, cx);
        }
    }

    #[cfg(target_arch = "wasm32")]
    pub(crate) fn edit_scheduled_dump(
        &mut self,
        _schedule_id: uuid::Uuid,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.notify();
    }

    #[cfg(target_arch = "wasm32")]
    pub(crate) fn run_scheduled_dump_now(
        &mut self,
        _schedule_id: uuid::Uuid,
        cx: &mut Context<Self>,
    ) {
        cx.notify();
    }

    #[cfg(target_arch = "wasm32")]
    pub(crate) fn remove_scheduled_dump(
        &mut self,
        _schedule_id: uuid::Uuid,
        cx: &mut Context<Self>,
    ) {
        cx.notify();
    }

    pub(super) fn handle_server_dashboard(
        &mut self,
        _action: &ServerDashboard,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) =
            self.require_admin_tool(crate::capabilities::AdminTool::ServerDashboard, cx)
        else {
            return;
        };
        // One dashboard per connection, all open at once: watching two servers
        // side by side is the point, and a second request for a server that
        // already has one raises that dashboard instead of duplicating it.
        if let Some(index) = self
            .overlays
            .server_dashboard_panes
            .iter()
            .position(|(id, _)| *id == profile.id)
        {
            let existing = self.overlays.server_dashboard_panes.remove(index);
            self.overlays.server_dashboard_panes.push(existing);
            cx.notify();
            return;
        }
        let context = crate::tool_dialog::ToolDialogContext::new(
            profile.clone(),
            self.services_for_profile(&profile),
        );
        let cascade = self.overlays.server_dashboard_panes.len();
        let pane =
            cx.new(|cx| crate::server_dashboard::ServerDashboardPane::new(context, cascade, cx));
        cx.subscribe(&pane, Self::handle_server_dashboard_event)
            .detach();
        self.overlays
            .server_dashboard_panes
            .push((profile.id, pane));
        cx.notify();
    }

    pub(super) fn handle_server_dashboard_event(
        &mut self,
        view: Entity<crate::server_dashboard::ServerDashboardPane>,
        event: &crate::tool_dialog::ToolDialogEvent,
        cx: &mut Context<Self>,
    ) {
        if self.apply_tool_dialog_event(event, cx) {
            // Close only the dashboard that asked to close; its siblings keep
            // polling their own servers.
            self.overlays
                .server_dashboard_panes
                .retain(|(_, pane)| pane != &view);
            self.restore_editor_focus(cx);
            cx.notify();
        }
    }

    pub(super) fn handle_activity_monitor(
        &mut self,
        _action: &ActivityMonitor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_activity_monitor(ToolScope::Server, window, cx);
    }

    pub(super) fn open_activity_monitor(
        &mut self,
        scope: ToolScope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_activity_monitor_inner(scope, None, window, cx);
    }

    /// The Redis live tools (Profiler, Pub/Sub) open the Activity Monitor
    /// straight onto their tab; the capability gate keeps them to RESP
    /// engines. The live lane dials the server directly, so a relay-routed
    /// connection is refused: the status bar says so and the tab opens
    /// showing the same refusal with Start disabled (the pane re-checks the
    /// route before every capture, so a tab click cannot bypass it).
    pub(super) fn open_redis_live_tab(
        &mut self,
        tab: crate::activity_monitor::ActivityTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (tool, feature) = match tab {
            crate::activity_monitor::ActivityTab::Profiler => (
                crate::capabilities::AdminTool::RedisProfiler,
                crate::localization::text("shell.tools.redis_profiler_feature"),
            ),
            crate::activity_monitor::ActivityTab::PubSub => (
                crate::capabilities::AdminTool::RedisPubSub,
                crate::localization::text("shell.tools.redis_pubsub_feature"),
            ),
            _ => (
                crate::capabilities::AdminTool::ActivityMonitor,
                crate::localization::text("shell.tools.activity_monitor_feature"),
            ),
        };
        let Some(profile) = self.require_admin_tool(tool, cx) else {
            return;
        };
        let refusal = tab
            .is_live()
            .then(|| direct_route_refusal(&profile, &feature))
            .flatten();
        self.open_activity_monitor_inner(ToolScope::Server, Some(tab), window, cx);
        if let Some(refusal) = refusal {
            self.set_status(StatusLevel::Warning, refusal);
            cx.notify();
        }
    }

    fn open_activity_monitor_inner(
        &mut self,
        scope: ToolScope,
        tab: Option<crate::activity_monitor::ActivityTab>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) =
            self.require_admin_tool(crate::capabilities::AdminTool::ActivityMonitor, cx)
        else {
            return;
        };
        if let Some(pane) = &self.overlays.activity_monitor_pane {
            // An explicit tab request (the palette's Redis live tools)
            // switches an already-open monitor; a plain re-invocation
            // leaves it where the user put it.
            if let Some(tab) = tab {
                pane.update(cx, |pane, cx| pane.show_tab(tab, cx));
            }
        } else {
            let context = crate::tool_dialog::ToolDialogContext::new(
                profile.clone(),
                self.services_for_profile(&profile),
            );
            let pane = cx.new(|cx| {
                crate::activity_monitor::ActivityMonitorPane::new(
                    context,
                    scope,
                    tab.unwrap_or(crate::activity_monitor::ActivityTab::Sessions),
                    window,
                    cx,
                )
            });
            cx.subscribe(&pane, Self::handle_activity_monitor_event)
                .detach();
            self.overlays.activity_monitor_pane = Some(pane);
        }
        // The slow-log strip's CONFIG SET / REWRITE skip the editor's run
        // gate; hand the pane the production guard for ITS connection.
        if let Some(pane) = self.overlays.activity_monitor_pane.clone() {
            let pane_profile = pane.read(cx).slowlog_profile().clone();
            let (production, production_check) = self.production_check(&pane_profile, cx);
            pane.update(cx, |pane, cx| {
                pane.set_production_check(production, production_check, cx)
            });
        }
        cx.notify();
    }

    pub(super) fn handle_activity_monitor_event(
        &mut self,
        _view: Entity<crate::activity_monitor::ActivityMonitorPane>,
        event: &crate::tool_dialog::ToolDialogEvent,
        cx: &mut Context<Self>,
    ) {
        if self.apply_tool_dialog_event(event, cx) {
            self.overlays.activity_monitor_pane = None;
            self.restore_editor_focus(cx);
            cx.notify();
        }
    }

    pub(super) fn handle_sql_agent(
        &mut self,
        _action: &SqlAgent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self.require_admin_tool(crate::capabilities::AdminTool::SqlAgent, cx)
        else {
            return;
        };
        if self.overlays.sql_agent_pane.is_none() {
            let context = crate::tool_dialog::ToolDialogContext::new(
                profile.clone(),
                self.services_for_profile(&profile),
            );
            let pane = cx.new(|cx| crate::sql_agent::SqlAgentPane::new(context, _window, cx));
            cx.subscribe(&pane, Self::handle_sql_agent_event).detach();
            self.overlays.sql_agent_pane = Some(pane);
        }
        cx.notify();
    }

    pub(super) fn handle_sql_agent_event(
        &mut self,
        _view: Entity<crate::sql_agent::SqlAgentPane>,
        event: &crate::tool_dialog::ToolDialogEvent,
        cx: &mut Context<Self>,
    ) {
        if self.apply_tool_dialog_event(event, cx) {
            self.overlays.sql_agent_pane = None;
            self.restore_editor_focus(cx);
            cx.notify();
        }
    }

    pub(super) fn handle_security(
        &mut self,
        _action: &Security,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self.require_admin_tool(crate::capabilities::AdminTool::Security, cx)
        else {
            return;
        };
        // Redis has no SQL catalog of principals: its Security page is the
        // ACL user list and log.
        #[cfg(not(target_arch = "wasm32"))]
        if profile.database_type.connection_kind() == sqlly_lang::sqlmodel::ConnectionKind::Resp {
            let context = crate::tool_dialog::ToolDialogContext::new(
                profile.clone(),
                self.services_for_profile(&profile),
            );
            let pane = cx.new(|cx| crate::redis_acl::RedisAclPane::new(context, window, cx));
            cx.subscribe(&pane, Self::handle_redis_acl_event).detach();
            self.overlays.redis_acl_pane = Some(pane);
            cx.notify();
            return;
        }
        if self.overlays.security_dialog.is_none() {
            // The dialog runs live security queries against `profile`, so it
            // must use that profile's route (relay or direct).
            let services = self.services_for_profile(&profile);
            let store = crate::connection::ConnectionStore::global();
            let now = sqlly_intel::query::safety::Instant::now();
            let production_read_only =
                crate::mutation_review::SafetyFeatures::effective_for_profile(&profile, store)
                    .production_guard()
                    .forces_select_only(self.production_unlock.as_ref(), now);
            let d = cx.new(|cx| {
                crate::security_dialog::SecurityDialog::new(
                    services,
                    profile,
                    production_read_only,
                    window,
                    cx,
                )
            });
            cx.subscribe(&d, Self::handle_security_dialog_event)
                .detach();
            self.overlays.security_dialog = Some(d);
        }
        cx.notify();
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn handle_redis_acl_event(
        &mut self,
        _view: Entity<crate::redis_acl::RedisAclPane>,
        event: &crate::tool_dialog::ToolDialogEvent,
        cx: &mut Context<Self>,
    ) {
        if self.apply_tool_dialog_event(event, cx) {
            self.overlays.redis_acl_pane = None;
            self.restore_editor_focus(cx);
            cx.notify();
        }
    }

    pub(super) fn handle_security_dialog_event(
        &mut self,
        _view: Entity<crate::security_dialog::SecurityDialog>,
        event: &crate::security_dialog::SecurityDialogEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            crate::security_dialog::SecurityDialogEvent::Dismissed => {
                self.restore_editor_focus(cx);
                self.overlays.security_dialog = None;
                cx.notify();
            }
            // ClickHouse principals are scripted for review, not applied.
            crate::security_dialog::SecurityDialogEvent::ScriptToEditor { sql } => {
                self.apply_tool_dialog_event(
                    &crate::tool_dialog::ToolDialogEvent::ScriptToEditor { sql: sql.clone() },
                    cx,
                );
                self.restore_editor_focus(cx);
                self.overlays.security_dialog = None;
                cx.notify();
            }
        }
    }

    pub(super) fn handle_designers(
        &mut self,
        _action: &Designers,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .require_admin_tool(crate::capabilities::AdminTool::Designers, cx)
            .is_none()
        {
            return;
        }
        if self.overlays.designer_dialog.is_none() {
            let d = cx.new(|cx| crate::designer::DesignerDialog::new(window, cx));
            cx.subscribe(&d, Self::handle_designer_event).detach();
            self.overlays.designer_dialog = Some(d);
        }
        cx.notify();
    }

    pub(super) fn handle_structure_editor_palette(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let context = self
            .sidebar
            .read(cx)
            .explorer_action_context_for_selection(cx);
        let Some(context) =
            context.filter(|ctx| matches!(ctx.kind, crate::schema_tree::NodeKind::Table { .. }))
        else {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.tools.structure_palette_select_table"),
            );
            cx.notify();
            return;
        };
        self.open_structure_editor(&context, window, cx);
    }

    pub(super) fn handle_designer_event(
        &mut self,
        _view: Entity<crate::designer::DesignerDialog>,
        event: &crate::tool_dialog::ToolDialogEvent,
        cx: &mut Context<Self>,
    ) {
        if self.apply_tool_dialog_event(event, cx) {
            self.overlays.designer_dialog = None;
            self.restore_editor_focus(cx);
            cx.notify();
        }
    }

    pub(super) fn open_structure_editor(
        &mut self,
        context: &crate::schema_tree::ExplorerActionContext,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(table) = context.name.clone().filter(|n| !n.is_empty()) else {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.tools.structure_select_table"),
            );
            cx.notify();
            return;
        };
        let Some(profile) = self
            .resolve_explorer_target(context.server_id.as_ref(), context.database.as_deref(), cx)
            .or_else(|| self.active_profile.clone())
        else {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.tools.structure_connect"),
            );
            cx.notify();
            return;
        };
        if let Some(reason) = crate::capabilities::admin_tool(
            profile.database_type,
            crate::capabilities::AdminTool::StructureEditor,
        )
        .reason()
        {
            self.set_status(StatusLevel::Info, reason.to_string());
            cx.notify();
            return;
        }
        if self.overlays.structure_editor.is_some() {
            return;
        }
        let database = context
            .database
            .clone()
            .or_else(|| profile.database.clone())
            .unwrap_or_else(|| "master".to_string());
        let schema = context.schema.clone();
        let services = self.services_for_profile(&profile);
        self.set_status(
            StatusLevel::Info,
            crate::localization::format("shell.tools.structure_loading", &[("table", &table)]),
        );
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let hydrated = cx
                .background_executor()
                .spawn(async move { crate::sql_credentials::hydrate_profile(profile) })
                .await;
            let result = match hydrated {
                Ok(profile) => sqlly_services::load_table_snapshot(
                    &*services.queries,
                    &profile,
                    &database,
                    schema.as_deref(),
                    &table,
                )
                .await
                .map(|snapshot| (profile, snapshot))
                .map_err(|e| crate::localization::message(e.message())),
                Err(reason) => Err(crate::localization::format(
                    "shell.tools.structure_credentials_failed",
                    &[("error", &crate::localization::message(reason))],
                )),
            };
            let _ = this.update_in(cx, |app, window, cx| {
                match result {
                    Ok((profile, snapshot)) => {
                        let dialog = cx.new(|cx| {
                            crate::structure_editor::StructureEditorDialog::new(
                                snapshot, profile, window, cx,
                            )
                        });
                        cx.subscribe(&dialog, Self::handle_structure_editor_event)
                            .detach();
                        app.overlays.structure_editor = Some(dialog);
                        app.set_status(
                            StatusLevel::Success,
                            crate::localization::text("shell.tools.structure_ready"),
                        );
                    }
                    Err(message) => app.set_status(StatusLevel::Info, message),
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn handle_structure_editor_event(
        &mut self,
        _view: Entity<crate::structure_editor::StructureEditorDialog>,
        event: &crate::tool_dialog::ToolDialogEvent,
        cx: &mut Context<Self>,
    ) {
        if self.apply_tool_dialog_event(event, cx) {
            self.overlays.structure_editor = None;
            self.restore_editor_focus(cx);
            cx.notify();
        }
    }
}
