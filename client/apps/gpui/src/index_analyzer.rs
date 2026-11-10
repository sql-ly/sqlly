//! Index Analyzer pane — real usage counters for every index, least-read
//! first, with script-first DROP INDEX for the ones the user decides to
//! remove.
//!
//! Read-only observer with one mutation path: a reviewed DROP INDEX script
//! into an editor tab, refused outright for primary-key/unique (enforcing)
//! indexes because those need constraint DDL instead.

use gpui::{
    Entity, EventEmitter, FocusHandle, IntoElement, ParentElement, Render, Styled, Window, div, px,
};
use gpui_component::Disableable as _;
use gpui_component::button::Button;
use sqlly_core::CellValue;
use sqlly_datatable::SqllyDataTable;
use sqlly_services::AdminToolsService;

use crate::dialog::{Dialog, DialogFrame};
use crate::theme::Theme;
use crate::tool_dialog::{ToolDialogContext, ToolDialogEvent};

fn column_index(columns: &[sqlly_core::ResultColumn], name: &str) -> Option<usize> {
    columns
        .iter()
        .position(|column| column.name.eq_ignore_ascii_case(name))
}

fn cell_text(value: Option<&CellValue>) -> Option<&str> {
    match value? {
        CellValue::String(value) => Some(value.as_str()),
        _ => None,
    }
}

fn cell_truthy(value: Option<&CellValue>) -> bool {
    matches!(value, Some(CellValue::Bool(true)) | Some(CellValue::I64(1)))
        || matches!(value, Some(CellValue::String(v)) if v == "1" || v.eq_ignore_ascii_case("true"))
}

/// The error text for a failed usage query. Oracle's usage tracking lives in
/// `DBA_INDEX_USAGE`, which most logins cannot read; name the missing grant
/// instead of leaving the user with a bare ORA-00942.
fn usage_error_text(db: sqlly_core::DatabaseType, err: String) -> String {
    if db.connection_kind() == sqlly_core::ConnectionKind::OracleNet
        && (err.contains("ORA-00942") || err.contains("ORA-01031"))
    {
        crate::localization::format(
            "explorer.index_analyzer.oracle_permission",
            &[("error", &err)],
        )
    } else {
        err
    }
}

/// The reviewed DROP INDEX script for one grid row, or the reason there is
/// none. Pure so the guard logic is testable: enforcing indexes are refused,
/// and the identifying columns must all be present.
fn drop_script_for_row(
    admin: &dyn AdminToolsService,
    db: sqlly_core::DatabaseType,
    columns: &[sqlly_core::ResultColumn],
    row: &[CellValue],
) -> Result<String, String> {
    if cell_truthy(column_index(columns, "is_enforcing").and_then(|idx| row.get(idx))) {
        return Err(crate::localization::text(
            "explorer.index_analyzer.enforcing_index",
        ));
    }
    let field = |name: &str| -> Result<String, String> {
        column_index(columns, name)
            .and_then(|idx| cell_text(row.get(idx)))
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string)
            .ok_or_else(|| {
                crate::localization::format(
                    "explorer.index_analyzer.missing_field",
                    &[("field", name)],
                )
            })
    };
    let schema = field("schema_name")?;
    let table = field("table_name")?;
    let index = field("index_name")?;
    let sql = admin
        .index_drop_sql_for(db, &schema, &table, &index)
        .ok_or_else(|| crate::localization::text("explorer.index_analyzer.drop_unsupported"))?;
    Ok(format!(
        "-- Review before running: this permanently drops the index and cannot be undone without rebuilding it.\n-- Usage counters reset on server restart — low numbers may only mean a recent restart.\n{sql}"
    ))
}

/// The reviewed rebuild script for one grid row (`ALTER INDEX … REBUILD` on
/// SQL Server, `REINDEX INDEX` on PostgreSQL), or the reason there is none —
/// the same identifying columns as [`drop_script_for_row`], and engines
/// without an index-level rebuild (MySQL) are refused with a reason.
fn rebuild_script_for_row(
    db: sqlly_core::DatabaseType,
    columns: &[sqlly_core::ResultColumn],
    row: &[CellValue],
) -> Result<String, String> {
    use sqlly_services::maintenance::{MaintenanceOp, MaintenanceTarget, maintenance_sql};
    let field = |name: &str| -> Result<String, String> {
        column_index(columns, name)
            .and_then(|idx| cell_text(row.get(idx)))
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string)
            .ok_or_else(|| {
                crate::localization::format(
                    "explorer.index_analyzer.missing_field",
                    &[("field", name)],
                )
            })
    };
    let target = MaintenanceTarget::Index {
        schema: Some(field("schema_name")?),
        table: field("table_name")?,
        name: field("index_name")?,
    };
    let sql = maintenance_sql(db, MaintenanceOp::Reindex, &target, &[])
        .map_err(|_| crate::localization::text("explorer.index_analyzer.rebuild_unsupported"))?;
    Ok(format!(
        "-- Review before running: rebuilding takes locks on the table and can run for a long time on large indexes.\n{sql}"
    ))
}

impl EventEmitter<ToolDialogEvent> for IndexAnalyzerPane {}

pub struct IndexAnalyzerPane {
    context: ToolDialogContext,
    grid: Option<Entity<SqllyDataTable>>,
    columns: Vec<sqlly_core::ResultColumn>,
    rows: Vec<Vec<CellValue>>,
    row_count: usize,
    error: Option<String>,
    loading: bool,
    refresh_generation: u64,
    refresh_in_flight: bool,
    focus_handle: FocusHandle,
    frame: DialogFrame,
}

impl IndexAnalyzerPane {
    pub fn new(context: ToolDialogContext, cx: &mut gpui::Context<Self>) -> Self {
        let mut this = Self {
            context,
            grid: None,
            columns: Vec::new(),
            rows: Vec::new(),
            row_count: 0,
            error: None,
            loading: true,
            refresh_generation: 0,
            refresh_in_flight: false,
            focus_handle: cx.focus_handle(),
            // Fixed frame: the embedded data grid manages its own scrolling.
            frame: DialogFrame::new(Theme::scaled(960.0), Theme::scaled(540.0)),
        };
        this.refresh(cx);
        this
    }

    fn refresh(&mut self, cx: &mut gpui::Context<Self>) {
        if self.refresh_in_flight {
            return;
        }
        let db = self.context.profile.database_type;
        let Some(query) = self.context.services.admin_tools.index_usage_query_for(db) else {
            self.loading = false;
            self.error = Some(crate::localization::format(
                "explorer.index_analyzer.counters_unsupported",
                &[("engine", &db.display_name())],
            ));
            cx.notify();
            return;
        };
        self.refresh_in_flight = true;
        self.refresh_generation += 1;
        let generation = self.refresh_generation;
        crate::tool_dialog::run_query(&self.context, query, cx, move |this, outcome, cx| {
            if this.refresh_generation != generation {
                return;
            }
            this.refresh_in_flight = false;
            this.loading = false;
            if let Some(err) = outcome.error {
                this.error = Some(usage_error_text(db, err));
                this.grid = None;
                this.columns = Vec::new();
                this.rows = Vec::new();
                this.row_count = 0;
            } else {
                this.error = None;
                let set = outcome.sets.into_iter().next().unwrap_or_default();
                this.row_count = set.rows.len();
                this.grid = crate::tool_dialog::build_result_grid(&set, cx);
                this.columns = set.columns;
                this.rows = set.rows;
            }
            cx.notify();
        });
        cx.notify();
    }

    fn selected_row(&self, cx: &gpui::App) -> Option<&[CellValue]> {
        let grid = self.grid.as_ref()?;
        let row = crate::tool_dialog::selected_source_row(grid, cx)?;
        self.rows.get(row).map(Vec::as_slice)
    }

    fn script_drop(&mut self, cx: &mut gpui::Context<Self>) {
        let admin = &*self.context.services.admin_tools;
        let db = self.context.profile.database_type;
        let Some(row) = self.selected_row(cx).map(<[CellValue]>::to_vec) else {
            cx.emit(ToolDialogEvent::Status {
                level: crate::app::StatusLevel::Info,
                message: crate::localization::text("explorer.index_analyzer.select_index"),
            });
            return;
        };
        match drop_script_for_row(admin, db, &self.columns, &row) {
            Ok(sql) => {
                cx.emit(ToolDialogEvent::ScriptToEditor { sql });
                cx.emit(ToolDialogEvent::Status {
                    level: crate::app::StatusLevel::Info,
                    message: crate::localization::text("explorer.index_analyzer.drop_scripted"),
                });
                cx.emit(ToolDialogEvent::Close);
            }
            Err(message) => cx.emit(ToolDialogEvent::Status {
                level: crate::app::StatusLevel::Warning,
                message,
            }),
        }
    }

    fn script_rebuild(&mut self, cx: &mut gpui::Context<Self>) {
        let db = self.context.profile.database_type;
        let Some(row) = self.selected_row(cx).map(<[CellValue]>::to_vec) else {
            cx.emit(ToolDialogEvent::Status {
                level: crate::app::StatusLevel::Info,
                message: crate::localization::text("explorer.index_analyzer.select_index"),
            });
            return;
        };
        match rebuild_script_for_row(db, &self.columns, &row) {
            Ok(sql) => {
                cx.emit(ToolDialogEvent::ScriptToEditor { sql });
                cx.emit(ToolDialogEvent::Status {
                    level: crate::app::StatusLevel::Info,
                    message: crate::localization::text("explorer.index_analyzer.rebuild_scripted"),
                });
                cx.emit(ToolDialogEvent::Close);
            }
            Err(message) => cx.emit(ToolDialogEvent::Status {
                level: crate::app::StatusLevel::Warning,
                message,
            }),
        }
    }

    fn close(&mut self, cx: &mut gpui::Context<Self>) {
        cx.emit(ToolDialogEvent::Close);
    }
}

impl gpui::Focusable for IndexAnalyzerPane {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for IndexAnalyzerPane {
    fn render(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;

        let content: gpui::AnyElement = if let Some(err) = &self.error {
            div()
                .p(px(s(12.0)))
                .text_color(Theme::error())
                .text_size(px(Theme::font_size_ratio(0.87)))
                .whitespace_normal()
                .child(err.clone())
                .into_any_element()
        } else if let Some(grid) = &self.grid {
            div().size_full().child(grid.clone()).into_any_element()
        } else if self.loading {
            div()
                .p(px(s(12.0)))
                .text_color(Theme::text_dim())
                .child(crate::localization::text("explorer.index_analyzer.loading"))
                .into_any_element()
        } else {
            div()
                .p(px(s(12.0)))
                .text_color(Theme::text_dim())
                .child(crate::localization::text("explorer.index_analyzer.no_rows"))
                .into_any_element()
        };

        let has_selection = self.selected_row(cx).is_some();

        let focus_handle = self.focus_handle.clone();
        Dialog::new("index-analyzer-pane", |this: &mut Self| &mut this.frame)
            .title(crate::localization::format(
                "explorer.index_analyzer.title",
                &[("connection", &self.context.profile.name)],
            ))
            .easy_close(true)
            .key_context("IndexAnalyzerPane")
            .track_focus(&focus_handle)
            .on_close(|this, _window, cx| this.close(cx))
            // bare_body: the embedded data grid must not sit in a scroll
            // container.
            .bare_body()
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .font(Theme::ui_font())
                    .flex()
                    .flex_col()
                    .gap(px(s(8.0)))
                    .p(px(s(14.0)))
                    .child(
                        div()
                            .flex_1()
                            .min_h(px(0.0))
                            .border_1()
                            .border_color(Theme::border())
                            .rounded(px(s(4.0)))
                            .child(content),
                    )
                    .child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.74)))
                            .text_color(Theme::text_dim())
                            .child(crate::localization::format(
                                "explorer.index_analyzer.summary",
                                &[("count", &self.row_count.to_string())],
                            )),
                    ),
            )
            .leading_button(
                Button::new("index-analyzer-refresh")
                    .label(crate::localization::text(
                        "explorer.index_analyzer.refresh_button",
                    ))
                    .disabled(self.loading)
                    .on_click(cx.listener(|this, _, _window, cx| this.refresh(cx))),
            )
            .button(
                Button::new("index-analyzer-rebuild")
                    .label(crate::localization::text(
                        "explorer.index_analyzer.rebuild_button",
                    ))
                    .disabled(!has_selection)
                    .on_click(cx.listener(|this, _, _window, cx| this.script_rebuild(cx))),
            )
            .button(
                Button::new("index-analyzer-drop")
                    .label(crate::localization::text(
                        "explorer.index_analyzer.drop_button",
                    ))
                    .disabled(!has_selection)
                    .on_click(cx.listener(|this, _, _window, cx| this.script_drop(cx))),
            )
            .build(self, window, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_services::{DefaultAdminToolsService, IndexDialect, index_dialect};

    fn columns() -> Vec<sqlly_core::ResultColumn> {
        ["schema_name", "table_name", "index_name", "is_enforcing"]
            .into_iter()
            .map(|name| sqlly_core::ResultColumn {
                name: name.into(),
                data_type: "text".into(),
                nullable: true,
            })
            .collect()
    }

    fn row(schema: &str, table: &str, index: &str, enforcing: bool) -> Vec<CellValue> {
        vec![
            CellValue::String(schema.into()),
            CellValue::String(table.into()),
            CellValue::String(index.into()),
            CellValue::Bool(enforcing),
        ]
    }

    #[test]
    fn dialects_cover_engines_with_usage_counters() {
        assert_eq!(
            index_dialect(sqlly_core::DatabaseType::SqlServer),
            Some(IndexDialect::SqlServer)
        );
        assert_eq!(
            index_dialect(sqlly_core::DatabaseType::Postgres),
            Some(IndexDialect::Postgres)
        );
        assert_eq!(
            index_dialect(sqlly_core::DatabaseType::MySql),
            Some(IndexDialect::MySql)
        );
        // Oracle 12.2+ tracks usage in DBA_INDEX_USAGE.
        assert_eq!(
            index_dialect(sqlly_core::DatabaseType::Oracle),
            Some(IndexDialect::Oracle)
        );
        // SQLite keeps no usage counters — no dialect, honest unavailable text.
        assert_eq!(index_dialect(sqlly_core::DatabaseType::Sqlite), None);
    }

    #[test]
    fn oracle_drop_and_rebuild_scripts_and_permission_hint() {
        let admin = DefaultAdminToolsService;
        let sql = drop_script_for_row(
            &admin,
            sqlly_core::DatabaseType::Oracle,
            &columns(),
            &row("HR", "EMP", "EMP_NAME_IX", false),
        )
        .unwrap();
        assert!(sql.contains("DROP INDEX \"HR\".\"EMP_NAME_IX\";"), "{sql}");
        let sql = rebuild_script_for_row(
            sqlly_core::DatabaseType::Oracle,
            &columns(),
            &row("HR", "EMP", "EMP_NAME_IX", false),
        )
        .unwrap();
        assert!(
            sql.contains("ALTER INDEX \"HR\".\"EMP_NAME_IX\" REBUILD;"),
            "{sql}"
        );
        let hint = usage_error_text(
            sqlly_core::DatabaseType::Oracle,
            "ORA-00942: table or view does not exist".into(),
        );
        assert!(hint.contains("DBA_INDEX_USAGE"), "{hint}");
        assert_eq!(
            usage_error_text(sqlly_core::DatabaseType::Postgres, "boom".into()),
            "boom"
        );
        assert_eq!(
            usage_error_text(sqlly_core::DatabaseType::Oracle, "ORA-00904".into()),
            "ORA-00904"
        );
    }

    #[test]
    fn drop_script_targets_the_selected_index() {
        let admin = DefaultAdminToolsService;
        let sql = drop_script_for_row(
            &admin,
            sqlly_core::DatabaseType::SqlServer,
            &columns(),
            &row("dbo", "Orders", "IX_Old", false),
        )
        .unwrap();
        assert!(sql.starts_with("-- Review before running"), "{sql}");
        assert!(
            sql.contains("DROP INDEX [IX_Old] ON [dbo].[Orders];"),
            "{sql}"
        );
    }

    #[test]
    fn enforcing_indexes_are_refused() {
        let admin = DefaultAdminToolsService;
        let err = drop_script_for_row(
            &admin,
            sqlly_core::DatabaseType::Postgres,
            &columns(),
            &row("public", "orders", "orders_pkey", true),
        )
        .unwrap_err();
        assert!(err.contains("constraint"), "{err}");
    }

    #[test]
    fn rebuild_script_targets_the_selected_index_per_engine() {
        let sql = rebuild_script_for_row(
            sqlly_core::DatabaseType::SqlServer,
            &columns(),
            &row("dbo", "Orders", "IX_Old", false),
        )
        .unwrap();
        assert!(sql.starts_with("-- Review before running"), "{sql}");
        assert!(
            sql.contains("ALTER INDEX [IX_Old] ON [dbo].[Orders] REBUILD;"),
            "{sql}"
        );
        let sql = rebuild_script_for_row(
            sqlly_core::DatabaseType::Postgres,
            &columns(),
            &row("public", "orders", "ix_old", false),
        )
        .unwrap();
        assert!(
            sql.contains("REINDEX INDEX \"public\".\"ix_old\";"),
            "{sql}"
        );
        // MySQL has no index-level rebuild: an honest refusal, not a guess.
        let err = rebuild_script_for_row(
            sqlly_core::DatabaseType::MySql,
            &columns(),
            &row("shop", "orders", "ix_old", false),
        )
        .unwrap_err();
        assert!(err.contains("not available"), "{err}");
        let err = rebuild_script_for_row(
            sqlly_core::DatabaseType::SqlServer,
            &columns(),
            &row("dbo", "Orders", "", false),
        )
        .unwrap_err();
        assert!(err.contains("index_name"), "{err}");
    }

    #[test]
    fn missing_identifiers_are_refused() {
        let admin = DefaultAdminToolsService;
        let err = drop_script_for_row(
            &admin,
            sqlly_core::DatabaseType::MySql,
            &columns(),
            &row("shop", "", "ix", false),
        )
        .unwrap_err();
        assert!(err.contains("table_name"), "{err}");
    }

    /// The pane's grid, drawn in the real pane, gets room for its rows (a
    /// grid in an auto-height wrapper paints zero pixels tall).
    #[gpui::test]
    fn index_analyzer_grid_gets_room_to_paint_its_rows(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            if cx.try_global::<gpui_component::Theme>().is_none() {
                gpui_component::init(cx);
            }
        });
        let context =
            crate::tool_dialog::unreachable_tool_context(sqlly_core::DatabaseType::SqlServer);
        let (view, cx) = cx.add_window_view(|_window, cx| IndexAnalyzerPane::new(context, cx));
        cx.simulate_resize(gpui::size(gpui::px(1440.0), gpui::px(900.0)));
        cx.run_until_parked();
        view.update(cx, |pane, cx| {
            pane.error = None;
            pane.loading = false;
            pane.grid =
                crate::tool_dialog::build_result_grid(&crate::tool_dialog::sample_tool_set(20), cx);
            cx.notify();
        });
        cx.run_until_parked();
        view.read_with(cx, |pane, cx| {
            let grid = pane.grid.as_ref().expect("grid");
            crate::tool_dialog::assert_grid_shows_rows(grid, 3, "Index Analyzer", cx);
        });
    }
}
