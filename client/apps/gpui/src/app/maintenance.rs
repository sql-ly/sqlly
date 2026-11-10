//! Explorer Maintenance actions: VACUUM / ANALYZE / REINDEX / UPDATE
//! STATISTICS / DBCC CHECKDB / OPTIMIZE TABLE and friends, from the database,
//! table, and index context menus.
//!
//! Script-first like every other explorer write: the Script form opens the
//! statement in a new tab bound to the node's connection; the Run form goes
//! through [`crate::maintenance_dialog::MaintenanceDialog`] (object +
//! statement + lock warning) and then the same tab-and-execute path, so every
//! execute gate — SELECT-only policy, production lock, confirmation — still
//! applies. Database-wide ops that an engine has no single statement for
//! (MySQL `ANALYZE/OPTIMIZE/CHECK TABLE`, SQL Server rebuild-all-indexes)
//! read the table list from the catalog first and expand to one statement
//! per table.

use super::*;
use crate::maintenance_dialog::{MaintenanceDialog, MaintenanceRequest, RunBlock};
use crate::schema_tree::{MaintenanceMode, maintenance_scope};
use sqlly_services::maintenance::{
    MaintenanceOp, MaintenanceTarget, TableRef, maintenance_script, maintenance_sql,
};
use sqlly_services::tool_dialects::maintenance_dialect;

/// The maintenance target an explorer node denotes, from its kind.
pub(super) fn maintenance_target(context: &ExplorerActionContext) -> Option<MaintenanceTarget> {
    let non_empty = |s: &str| (!s.is_empty()).then(|| s.to_string());
    match &context.kind {
        NodeKind::Database(name) => Some(MaintenanceTarget::Database { name: name.clone() }),
        NodeKind::Table { schema, name } => Some(MaintenanceTarget::Table {
            schema: non_empty(schema),
            name: name.clone(),
        }),
        NodeKind::Index {
            schema,
            table,
            name,
        } => Some(MaintenanceTarget::Index {
            schema: non_empty(schema),
            table: table.clone(),
            name: name.clone(),
        }),
        _ => None,
    }
}

/// Why Run must be refused for a profile, from its effective safety policy:
/// the SELECT-only lock (own or inherited) or a locked production
/// connection. `None` when the connection may run writes.
pub(super) fn maintenance_run_block(
    profile: &sqlly_core::ConnectionProfile,
    store: &crate::connection::ConnectionStore,
    unlock: Option<&sqlly_intel::query::safety::WriteUnlock>,
    now: sqlly_intel::query::safety::Instant,
) -> Option<RunBlock> {
    let safety = crate::mutation_review::SafetyFeatures::effective_for_profile(profile, store);
    if safety.only_allow_select {
        return Some(RunBlock::SelectOnly);
    }
    if safety.production_guard().decide(unlock, now)
        == sqlly_intel::query::safety::ProductionDecision::ReadOnly
    {
        return Some(RunBlock::ProductionReadOnly);
    }
    None
}

/// Catalog rows from `MetadataQuery::Tables` (`schema_name, name, …`) as
/// table references, in catalog order.
pub(super) fn table_refs_from_rows(rows: Vec<Vec<sqlly_core::CellValue>>) -> Vec<TableRef> {
    use sqlly_core::CellValue;
    fn text(cell: Option<&CellValue>) -> Option<&str> {
        match cell? {
            CellValue::String(s) => Some(s.as_str()),
            _ => None,
        }
    }
    rows.iter()
        .filter_map(|row| {
            let name = text(row.get(1)).filter(|n| !n.is_empty())?;
            Some(TableRef::new(text(row.first()), name))
        })
        .collect()
}

impl AppView {
    /// Entry point from the explorer context menu.
    pub(super) fn handle_maintenance(
        &mut self,
        context: &ExplorerActionContext,
        op: MaintenanceOp,
        mode: MaintenanceMode,
        cx: &mut Context<Self>,
    ) {
        let (Some(scope), Some(target)) = (
            maintenance_scope(&context.kind),
            maintenance_target(context),
        ) else {
            return;
        };
        let Some(profile) = self
            .resolve_explorer_target(context.server_id.as_ref(), context.database.as_deref(), cx)
            .or_else(|| self.active_profile.clone())
        else {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.maintenance.connect"),
            );
            cx.notify();
            return;
        };
        if let Some(reason) = crate::capabilities::maintenance(profile.database_type).reason() {
            self.set_status(StatusLevel::Error, reason.to_string());
            cx.notify();
            return;
        }
        let Some(text) = maintenance_dialect(profile.database_type)
            .and_then(|dialect| op.describe(dialect, scope))
        else {
            self.set_status(
                StatusLevel::Error,
                crate::localization::text("shell.maintenance.unsupported"),
            );
            cx.notify();
            return;
        };

        if !text.expands_tables {
            self.finish_maintenance(
                profile,
                op,
                target,
                text.label,
                text.warning,
                Vec::new(),
                mode,
                cx,
            );
            return;
        }

        // Database-wide op with no single statement on this engine: list the
        // tables first, then expand to one statement per table.
        let database = context
            .database
            .clone()
            .or_else(|| profile.database.clone())
            .unwrap_or_default();
        let services = self.services_for_profile(&profile);
        let query_profile = profile.clone();
        self.set_status(
            StatusLevel::Busy,
            crate::localization::format(
                "shell.maintenance.listing_tables",
                &[("operation", text.label)],
            ),
        );
        cx.notify();

        let (tx, rx) = tokio::sync::oneshot::channel::<Result<Vec<TableRef>, String>>();
        cx.background_spawn(async move {
            crate::tokio_handle()
                .spawn(async move {
                    let result = services
                        .metadata
                        .run_metadata_query(
                            &query_profile,
                            &database,
                            &sqlly_intel::metadata::MetadataQuery::Tables { schema: None },
                        )
                        .await
                        .map(table_refs_from_rows)
                        .map_err(|e| e.to_string());
                    let _ = tx.send(result);
                })
                .await
                .ok();
        })
        .detach();

        let async_cx = cx.to_async();
        let weak_self = cx.weak_entity();
        let label = text.label;
        let warning = text.warning;
        cx.spawn(async move |_this, _cx| {
            let Ok(result) = rx.await else {
                return;
            };
            let mut cx = async_cx.clone();
            weak_self
                .update(&mut cx, |this, cx| match result {
                    Ok(tables) => this
                        .finish_maintenance(profile, op, target, label, warning, tables, mode, cx),
                    Err(error) => {
                        this.set_status(
                            StatusLevel::Error,
                            crate::localization::format(
                                "shell.maintenance.list_failed",
                                &[("operation", label), ("error", &error.to_string())],
                            ),
                        );
                        cx.notify();
                    }
                })
                .ok();
        })
        .detach();
    }

    /// Render the SQL and either script it to a tab on the node's connection
    /// or open the Run confirmation.
    #[allow(clippy::too_many_arguments)]
    fn finish_maintenance(
        &mut self,
        profile: sqlly_core::ConnectionProfile,
        op: MaintenanceOp,
        target: MaintenanceTarget,
        label: &'static str,
        warning: Option<&'static str>,
        tables: Vec<TableRef>,
        mode: MaintenanceMode,
        cx: &mut Context<Self>,
    ) {
        let sql = match maintenance_sql(profile.database_type, op, &target, &tables) {
            Ok(sql) => sql,
            Err(error) => {
                self.set_status(
                    StatusLevel::Error,
                    crate::localization::format(
                        "shell.maintenance.script_failed",
                        &[("operation", label), ("error", &error.to_string())],
                    ),
                );
                cx.notify();
                return;
            }
        };
        let script = maintenance_script(label, &target, &sql);
        match mode {
            MaintenanceMode::Script => {
                let status = crate::localization::format(
                    "shell.maintenance.scripted",
                    &[("operation", label)],
                );
                self.with_explorer_target_active(Some(profile), cx, move |app, cx| {
                    app.new_tab(cx, Some(script));
                    app.set_status(StatusLevel::Info, status);
                    cx.notify();
                });
            }
            MaintenanceMode::Run => {
                let store = crate::connection::ConnectionStore::global();
                let run_block = maintenance_run_block(
                    &profile,
                    store,
                    self.production_unlock.as_ref(),
                    sqlly_intel::query::safety::Instant::now(),
                );
                let request = MaintenanceRequest {
                    label: label.to_string(),
                    target,
                    sql,
                    warning,
                    script,
                    run_block,
                };
                let context = crate::tool_dialog::ToolDialogContext::new(
                    profile.clone(),
                    self.services_for_profile(&profile),
                );
                let dialog = cx.new(|cx| MaintenanceDialog::new(context, request, cx));
                cx.subscribe(&dialog, Self::handle_maintenance_dialog_event)
                    .detach();
                self.overlays.maintenance_dialog = Some(dialog);
                cx.notify();
            }
        }
    }

    pub(super) fn handle_maintenance_dialog_event(
        &mut self,
        _view: Entity<MaintenanceDialog>,
        event: &crate::tool_dialog::ToolDialogEvent,
        cx: &mut Context<Self>,
    ) {
        let close = self.apply_tool_dialog_event(event, cx);
        if close {
            self.overlays.maintenance_dialog = None;
            self.restore_editor_focus(cx);
            cx.notify();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::CellValue;

    fn ctx(kind: NodeKind) -> ExplorerActionContext {
        ExplorerActionContext {
            node_id: "n".into(),
            title: "t".into(),
            kind,
            server_id: None,
            database: Some("shop".into()),
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
    fn target_follows_the_node_kind() {
        assert_eq!(
            maintenance_target(&ctx(NodeKind::Database("shop".into()))),
            Some(MaintenanceTarget::Database {
                name: "shop".into()
            })
        );
        assert_eq!(
            maintenance_target(&ctx(NodeKind::Table {
                schema: "".into(),
                name: "orders".into()
            })),
            Some(MaintenanceTarget::Table {
                schema: None,
                name: "orders".into()
            })
        );
        assert_eq!(
            maintenance_target(&ctx(NodeKind::Index {
                schema: "dbo".into(),
                table: "Orders".into(),
                name: "IX_Orders".into()
            })),
            Some(MaintenanceTarget::Index {
                schema: Some("dbo".into()),
                table: "Orders".into(),
                name: "IX_Orders".into()
            })
        );
        assert_eq!(
            maintenance_target(&ctx(NodeKind::View {
                schema: "dbo".into(),
                name: "v".into()
            })),
            None
        );
    }

    #[test]
    fn catalog_rows_become_table_refs_in_order() {
        let rows = vec![
            vec![
                CellValue::String("dbo".into()),
                CellValue::String("Orders".into()),
                CellValue::Null,
            ],
            vec![
                CellValue::String("sales".into()),
                CellValue::String("Lines".into()),
            ],
            // Rows without a name are skipped, never rendered as ``.
            vec![CellValue::String("dbo".into()), CellValue::Null],
            vec![CellValue::Null, CellValue::String("loose".into())],
        ];
        assert_eq!(
            table_refs_from_rows(rows),
            vec![
                TableRef::new(Some("dbo"), "Orders"),
                TableRef::new(Some("sales"), "Lines"),
                TableRef::new(None, "loose"),
            ]
        );
    }

    #[test]
    fn select_only_lock_blocks_run_with_the_policy_reason() {
        let dir = tempfile::tempdir().unwrap();
        let store =
            crate::connection::ConnectionStore::from_path(dir.path().join("connections.json"));
        let secret = sqlly_core::SecretRef {
            provider: sqlly_core::SecretProvider::InMemory,
            key: "k".to_string(),
        };
        let mut profile =
            sqlly_core::ConnectionProfile::sql_login("prod", "localhost", "sa", secret);
        assert_eq!(
            maintenance_run_block(
                &profile,
                &store,
                None,
                sqlly_intel::query::safety::Instant::now()
            ),
            None
        );
        profile.block_all_but_select = true;
        assert_eq!(
            maintenance_run_block(
                &profile,
                &store,
                None,
                sqlly_intel::query::safety::Instant::now()
            ),
            Some(RunBlock::SelectOnly)
        );
    }
}
