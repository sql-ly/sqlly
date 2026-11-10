use super::*;

impl OverlayHost {
    #[cfg(any(target_arch = "wasm32", test))]
    pub(super) fn show_about(&mut self, cx: &mut gpui::App) {
        let panel = cx.new(crate::about::AboutPanel::new);
        self.information_panel = Some(("about.window_title", panel.into()));
    }

    #[cfg(any(target_arch = "wasm32", test))]
    pub(super) fn show_open_source_licenses(&mut self, cx: &mut gpui::App) {
        let panel = cx.new(crate::open_source::OpenSourceLicensesPanel::new);
        self.information_panel = Some(("open_source.window_title", panel.into()));
    }
}

/// State for the reviewed rename dialog: the object being renamed and the
/// lazily-materialized name input (renders have the `Window` handlers lack).
pub(super) struct RenameDialogState {
    pub(super) server_id: Option<uuid::Uuid>,
    pub(super) database: Option<String>,
    pub(super) schema: String,
    pub(super) name: String,
    pub(super) kind: crate::table_actions::ScriptObjectKind,
    pub(super) db: sqlly_lang::sqlmodel::DatabaseType,
    pub(super) input: Option<Entity<gpui_component::input::InputState>>,
    pub(super) value: String,
    pub(super) error: Option<String>,
    pub(super) _subscription: Option<gpui::Subscription>,
}

/// State for the New / Rename Pin Group naming dialog.
pub(super) struct PinGroupDialogState {
    pub(super) server_id: uuid::Uuid,
    /// The group being renamed (id, current name); `None` creates one.
    pub(super) rename: Option<(uuid::Uuid, String)>,
    /// The object to put in a newly created group.
    pub(super) pin: Option<crate::schema_tree::FavoriteObject>,
    pub(super) input: Option<Entity<gpui_component::input::InputState>>,
    pub(super) value: String,
    pub(super) error: Option<String>,
    pub(super) _subscription: Option<gpui::Subscription>,
}

/// Open dialog/overlay entities mounted above the workbench.
#[derive(Default)]
pub(super) struct OverlayHost {
    /// Result snapshot browser overlay, when open.
    pub(super) snapshot_browser: Option<Entity<crate::result_snapshots::SnapshotBrowser>>,
    /// Saved Charts library panel, when open.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) saved_charts_panel: Option<Entity<crate::saved_charts_panel::SavedChartsPanel>>,
    /// "Save chart to library…" naming prompt, when open.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) save_chart_dialog: Option<Entity<crate::saved_charts_panel::SaveChartDialog>>,
    /// Chart Dashboard (inline card; cleared on pop-out, when the standalone
    /// window owns the entity).
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) chart_dashboard: Option<Entity<crate::chart_dashboard::ChartDashboard>>,
    /// Connection health dashboard overlay, when open.
    pub(super) connection_health_dialog:
        Option<Entity<crate::connection_health::ConnectionHealthDashboard>>,
    /// Reviewed rename dialog, when open.
    pub(super) rename_dialog: Option<RenameDialogState>,
    /// New / Rename Pin Group naming dialog, when open.
    pub(super) pin_group_dialog: Option<PinGroupDialogState>,
    /// ER relationship graph overlay, when open.
    pub(super) er_diagram: Option<Entity<crate::er_diagram::ErDiagramView>>,
    /// "Improve IntelliSense" feedback dialog, when open as the inline modal.
    /// Cleared on pop-out — the standalone window owns the entity from then
    /// on, and the detached subscription keeps routing its submissions.
    pub(super) intellisense_feedback:
        Option<Entity<crate::intellisense_feedback_dialog::IntelliSenseFeedbackDialog>>,
    /// Visual query builder overlay, when open.
    pub(super) query_builder: Option<Entity<crate::query_builder::QueryBuilderView>>,
    /// Table to add after an explorer retarget that needed async connect.
    pub(super) pending_query_builder: Option<(String, String)>,
    /// Tools-submenu dialog to open once an explorer retarget has finished
    /// connecting — the async connect path has no `Window` to open it with.
    pub(super) pending_explorer_tool: Option<(
        crate::schema_tree::ExplorerTool,
        crate::tool_dialog::ToolScope,
    )>,
    pub(super) connection_editor: Option<Entity<ConnectionEditor>>,
    pub(super) connection_management: Option<Entity<ConnectionManagement>>,
    pub(super) entity_management: Option<Entity<EntityManagement>>,
    pub(super) preferences_window: Option<Entity<crate::preferences_ui::PreferencesWindow>>,
    /// Live metadata editor opened from explorer right-click → Properties for
    /// tables, views, procedures, functions, indexes, statistics, and types.
    pub(super) object_editor: Option<Entity<crate::object_editor::ObjectEditorView>>,
    pub(super) command_palette: Option<Entity<CommandPalette>>,
    /// Custom "Open Project File" dialog (replaces the native open panel).
    pub(super) project_file_picker: Option<Entity<ProjectFilePicker>>,
    pub(super) workspace_dialog: Option<Entity<WorkspaceDialog>>,
    pub(super) tool_output_view: Option<Entity<ToolOutputView>>,
    /// Browser Help panels stay in the workbench: wasm has only one window.
    pub(super) information_panel: Option<(&'static str, gpui::AnyView)>,
    /// Tip of the Day (startup or Help ▸ Tip of the Day…), when open.
    pub(super) tip_of_the_day: Option<Entity<crate::tip_of_the_day::TipOfTheDayDialog>>,
    /// Unlock SQLLY (Mac App Store in-app purchase), when open.
    pub(super) unlock_dialog: Option<Entity<crate::unlock_dialog::UnlockDialog>>,
    /// Help ▸ Mac App Store Edition (also shown once on first launch).
    pub(super) app_store_edition_dialog:
        Option<Entity<crate::app_store_edition_dialog::AppStoreEditionDialog>>,
    pub(super) entra_accounts_dialog:
        Option<Entity<crate::entra_accounts_dialog::EntraAccountsDialog>>,
    pub(super) security_dialog: Option<Entity<crate::security_dialog::SecurityDialog>>,
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) database_search_dialog:
        Option<Entity<crate::database_search_dialog::DatabaseSearchDialog>>,
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) run_sql_file_dialog: Option<Entity<crate::run_sql_file_dialog::RunSqlFileDialog>>,
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) batch_insert_dialog: Option<Entity<crate::batch_insert_dialog::BatchInsertDialog>>,
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) tunnel_profiles_dialog:
        Option<Entity<crate::tunnel_profiles_dialog::TunnelProfilesDialog>>,
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) deep_link_sheet: Option<Entity<crate::deep_link_sheet::DeepLinkSheet>>,
    pub(super) backup_restore_dialog:
        Option<Entity<crate::backup_restore_dialog::BackupRestoreDialog>>,
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) native_restore_dialog:
        Option<Entity<crate::native_restore_dialog::NativeRestoreDialog>>,
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) database_dump_dialog:
        Option<Entity<crate::database_dump_dialog::DatabaseDumpDialog>>,
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) table_export_dialog: Option<Entity<crate::table_export_dialog::TableExportDialog>>,
    /// (schema, table) pairs the next table-export dialog opens with checked —
    /// set by the bulk-operations Export data handoff, consumed by
    /// `open_table_export_dialog` whichever path (direct or post-connect
    /// pending) reaches it.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) table_export_preselect: Option<std::collections::HashSet<(String, String)>>,
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) database_dialog: Option<Entity<crate::database_dialog::DatabaseDialog>>,
    /// Redis key browser prompts (filter, TTL, persist, delete); one at a time.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) redis_key_dialog: Option<Entity<crate::redis_key_dialog::RedisKeyDialog>>,
    /// Redis key rename / copy / export and database naming; one at a time.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) redis_key_action_dialog:
        Option<Entity<crate::redis_key_actions::RedisKeyActionDialog>>,
    /// Run-now confirmation for an explorer Maintenance action (VACUUM, DBCC
    /// CHECKDB, index rebuilds…); one at a time.
    pub(super) maintenance_dialog: Option<Entity<crate::maintenance_dialog::MaintenanceDialog>>,
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) mcp_activity_dialog: Option<Entity<crate::mcp_activity::McpActivityView>>,
    pub(super) activity_monitor_pane: Option<Entity<crate::activity_monitor::ActivityMonitorPane>>,
    /// One Server Dashboard per connection, oldest first — they are non-modal
    /// and stay open together so several servers can be watched at once. The
    /// last entry renders on top.
    pub(super) server_dashboard_panes: Vec<(
        sqlly_core::ConnectionId,
        Entity<crate::server_dashboard::ServerDashboardPane>,
    )>,
    pub(super) template_explorer_dialog:
        Option<Entity<crate::template_explorer::TemplateExplorerDialog>>,
    /// A custom explorer action's parameter prompt, with the clicked
    /// object's context and the template name the answer runs.
    pub(super) template_params_dialog: Option<(
        Entity<crate::template_explorer::TemplateParamsDialog>,
        ExplorerActionContext,
        String,
    )>,
    pub(super) query_store_pane: Option<Entity<crate::query_store::QueryStorePane>>,
    pub(super) disk_usage_pane: Option<Entity<crate::database_disk_usage::DiskUsagePane>>,
    /// The Redis memory analyzer's report pane (one at a time).
    pub(super) redis_memory_pane: Option<Entity<crate::redis_memory_analyzer::RedisMemoryPane>>,
    /// The Security tool's Redis page (ACL users and log).
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) redis_acl_pane: Option<Entity<crate::redis_acl::RedisAclPane>>,
    /// The Redis key view (one at a time; opening another key retargets it).
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) redis_key_view: Option<Entity<crate::redis_key_view::RedisKeyViewPane>>,
    /// The Redis Cluster topology pane (one at a time).
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) redis_cluster_topology:
        Option<Entity<crate::redis_cluster_topology::RedisClusterTopologyPane>>,
    pub(super) object_browser_pane: Option<Entity<crate::object_browser::ObjectBrowserPane>>,
    pub(super) index_analyzer_pane: Option<Entity<crate::index_analyzer::IndexAnalyzerPane>>,
    pub(super) test_data_generator_pane:
        Option<Entity<crate::test_data_generator::TestDataGeneratorPane>>,
    pub(super) tenant_scoping_dialog: Option<Entity<crate::tenant_scoping::TenantScopingDialog>>,
    pub(super) query_history_browser:
        Option<Entity<crate::query_history_browser::QueryHistoryBrowser>>,
    pub(super) object_dependencies_pane:
        Option<Entity<crate::object_dependencies::ObjectDependenciesView>>,
    pub(super) sql_agent_pane: Option<Entity<crate::sql_agent::SqlAgentPane>>,
    pub(super) import_export_dialog: Option<Entity<crate::import_export::ImportExportDialog>>,
    pub(super) schema_compare_pane: Option<Entity<crate::schema_compare::SchemaComparePane>>,
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) schema_history_dialog: Option<Entity<crate::schema_history::SchemaHistoryDialog>>,
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) save_schema_snapshot_dialog:
        Option<Entity<crate::schema_history::SaveSchemaSnapshotDialog>>,
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) schema_compare_deploy_dialog:
        Option<Entity<crate::schema_compare_deploy::SchemaCompareDeployDialog>>,
    pub(super) data_compare_pane: Option<Entity<crate::data_compare::DataComparePane>>,
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) data_transfer_pane: Option<Entity<crate::data_transfer::DataTransferPane>>,
    /// Opened on the next frame after an explorer retarget that needed async connect.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) pending_data_transfer_preselect: Option<(Option<String>, Option<String>, String)>,
    /// Database name for a deferred export dialog open after async connect.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) pending_database_dump: Option<Option<String>>,
    /// Database and optional schema for a deferred table-export dialog.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) pending_table_export: Option<(String, Option<String>)>,
    /// Schema prefill for a deferred clipboard import after async connect.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) pending_import_from_clipboard: Option<Option<String>>,
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) pending_database_search: bool,
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) pending_run_sql_file: bool,
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) pending_run_sql_file_path: Option<std::path::PathBuf>,
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) pending_tunnel_profiles: bool,
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) pending_open_mcp_activity: bool,
    pub(super) xevents_pane: Option<Entity<crate::xevents::ExtendedEventsPane>>,
    pub(super) designer_dialog: Option<Entity<crate::designer::DesignerDialog>>,
    pub(super) structure_editor: Option<Entity<crate::structure_editor::StructureEditorDialog>>,
    pub(super) multi_target_dialog: Option<Entity<crate::multi_target_dialog::MultiTargetDialog>>,
    /// The Export Conn w/ Creds server/database picker, while open.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) credential_export_dialog:
        Option<Entity<crate::credential_export_dialog::CredentialExportDialog>>,
    /// The open "your IP is blocked by the server firewall" (error 40615)
    /// prompt, if any.
    pub(super) firewall_rule_dialog:
        Option<Entity<crate::firewall_rule_dialog::FirewallRuleDialog>>,
    /// SSH host-key trust prompt, when an unknown key is seen during tunnel
    /// setup.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) host_key_dialog: Option<Entity<crate::host_key_dialog::HostKeyDialog>>,
    /// SSH keyboard-interactive prompt (a one-time code), when a tunnel's
    /// server asks for more after the key or password.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) ssh_challenge_dialog:
        Option<Entity<crate::ssh_challenge_dialog::SshChallengeDialog>>,
    /// Open row-editor modal plus the results view it was opened from, so a
    /// committed save can refresh that grid on dismissal.
    pub(super) row_editor: Option<(Entity<crate::row_editor::RowEditor>, Entity<ResultsView>)>,
    /// Row editors popped into their own windows, paired with their source grid
    /// so a committed save still refreshes it after the modal is gone.
    pub(super) popped_out_row_editors:
        Vec<(Entity<crate::row_editor::RowEditor>, Entity<ResultsView>)>,
    /// Run Statements picker overlay.
    pub(super) run_picker: Option<RunPickerOverlay>,
    /// Inline tab-strip rename, when a tab title is being edited.
    pub(super) tab_rename: Option<TabRenameState>,
}

/// State for inline tab rename: the tab index and the lazily-materialized name input.
pub(crate) struct TabRenameState {
    pub(super) tab_idx: usize,
    pub(super) input: Option<Entity<gpui_component::input::InputState>>,
    pub(super) value: String,
    /// Ignore Blur until the input has actually focused (create+focus can emit Blur first).
    pub(super) focused: bool,
    pub(super) _subscription: Option<gpui::Subscription>,
}

/// State for the Run Statements picker overlay.
pub(crate) struct RunPickerOverlay {
    pub tab_idx: usize,
    pub model: crate::run_picker::RunPickerModel,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn browser_help_panels_replace_close_and_reopen_without_a_second_window(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            // Keep this test local: opening About must reuse an in-flight check.
            crate::update_status::handle(cx).update(cx, |status, _| {
                status.state = crate::update_status::UpdateCheckState::Checking;
            });
            let mut overlays = OverlayHost::default();
            let window_count = cx.windows().len();
            overlays.show_about(cx);
            let (title, panel) = overlays.information_panel.as_ref().unwrap();
            assert_eq!(*title, "about.window_title");
            assert!(panel.clone().downcast::<crate::about::AboutPanel>().is_ok());

            overlays.show_open_source_licenses(cx);
            let (title, panel) = overlays.information_panel.as_ref().unwrap();
            assert_eq!(*title, "open_source.window_title");
            assert!(
                panel
                    .clone()
                    .downcast::<crate::open_source::OpenSourceLicensesPanel>()
                    .is_ok()
            );

            overlays.information_panel = None;
            overlays.show_about(cx);
            assert_eq!(
                overlays.information_panel.as_ref().unwrap().0,
                "about.window_title"
            );
            assert_eq!(cx.windows().len(), window_count);
        });
    }
}
