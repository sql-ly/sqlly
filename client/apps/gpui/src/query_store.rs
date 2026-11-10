//! Query Insights pane — bounded top-query statistics from SQL Server Query
//! Store, PostgreSQL pg_stat_statements, MySQL Performance Schema, or the
//! ClickHouse query log (`system.query_log`, which also keeps failures), or
//! the Oracle shared pool (V$SQLAREA).
//!
//! Each source has a different lifecycle, so the pane detects its native
//! collector first, offers a reviewed setup script only where SQL can enable
//! it, and names engine-specific permission/configuration requirements.

use gpui::{
    Entity, EventEmitter, FocusHandle, IntoElement, ParentElement, Render, Styled, Window, div, px,
};
use gpui_component::Disableable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use sqlly_core::CellValue;
use sqlly_datatable::SqllyDataTable;
use sqlly_services::QueryStoreRank;

use crate::dialog::{Dialog, DialogFrame};
use crate::theme::Theme;
use crate::tool_dialog::{ToolDialogContext, ToolDialogEvent};

/// How many top queries the grid shows.
const TOP_QUERY_LIMIT: u32 = 50;

/// What detection found.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Availability {
    Checking,
    /// Query Store is on (READ_WRITE or READ_ONLY).
    Enabled,
    /// The options view exists but the store is off.
    Disabled,
    /// The check failed — not SQL Server, no such database, or no permission.
    Unavailable(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InsightEngine {
    SqlServer,
    Postgres,
    MySql,
    ClickHouse,
    Oracle,
}

impl InsightEngine {
    fn for_database(db: sqlly_core::DatabaseType) -> Option<Self> {
        use sqlly_core::ConnectionKind;
        match db.connection_kind() {
            ConnectionKind::Tds => Some(Self::SqlServer),
            ConnectionKind::PostgresWire => Some(Self::Postgres),
            ConnectionKind::MySqlWire => Some(Self::MySql),
            ConnectionKind::ClickHouseHttp => Some(Self::ClickHouse),
            ConnectionKind::OracleNet => Some(Self::Oracle),
            _ => None,
        }
    }

    fn source_label(self) -> &'static str {
        match self {
            Self::SqlServer => "SQL Server Query Store",
            Self::Postgres => "PostgreSQL pg_stat_statements",
            Self::MySql => "MySQL Performance Schema",
            Self::ClickHouse => "ClickHouse query log",
            Self::Oracle => "Oracle shared pool (V$SQLAREA)",
        }
    }

    fn availability_query(self, database: &str) -> String {
        match self {
            Self::SqlServer => format!(
                "USE [{}]; SELECT actual_state_desc FROM sys.database_query_store_options",
                database.replace(']', "]]"),
            ),
            Self::Postgres => "SELECT EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'pg_stat_statements') AS enabled".into(),
            Self::MySql => "SELECT @@performance_schema AS enabled".into(),
            // The log table only exists once the server has flushed a query
            // into it, and each user's log_queries setting decides whether
            // its queries are written at all.
            Self::ClickHouse => "SELECT ((SELECT count() FROM system.tables WHERE database = 'system' AND name = 'query_log') > 0) AND ((SELECT value FROM system.settings WHERE name = 'log_queries') = '1') AS enabled".into(),
            // The shared pool always has cursors; the check is whether the
            // login may read V$SQLAREA at all (ORA-00942 otherwise).
            Self::Oracle => "SELECT COUNT(*) AS enabled FROM v$sqlarea WHERE ROWNUM <= 1".into(),
        }
    }

    fn supports_time_window(self) -> bool {
        matches!(self, Self::SqlServer | Self::ClickHouse)
    }

    /// The ClickHouse query log also records failed queries with their
    /// exception; the other stores keep only successful executions.
    fn supports_failures(self) -> bool {
        self == Self::ClickHouse
    }

    fn setup_sql(self, database: &str) -> Option<String> {
        match self {
            Self::SqlServer => Some(QueryStorePane::enable_sql(database)),
            Self::Postgres => Some("-- Requires sufficient privileges. Some PostgreSQL installations also require pg_stat_statements in shared_preload_libraries and a server restart.\nCREATE EXTENSION IF NOT EXISTS pg_stat_statements;".into()),
            Self::MySql => None,
            // log_queries is a per-user setting from users.xml or a settings
            // profile; there is no single statement that turns it on durably.
            Self::ClickHouse => None,
            Self::Oracle => None,
        }
    }

    fn unavailable_guidance(self, database: &str) -> String {
        match self {
            Self::SqlServer => crate::localization::format(
                "query.store.guidance_sqlserver",
                &[("database", database)],
            ),
            Self::Postgres => crate::localization::text("query.store.guidance_postgres"),
            Self::MySql => crate::localization::text("query.store.guidance_mysql"),
            Self::ClickHouse => crate::localization::text("query.store.guidance_clickhouse"),
            Self::Oracle => crate::localization::text("query.store.guidance_oracle"),
        }
    }

    fn disabled_message(self, database: &str) -> String {
        match self {
            Self::SqlServer => crate::localization::format(
                "query.store.disabled_sqlserver",
                &[("database", database)],
            ),
            Self::Postgres => crate::localization::text("query.store.disabled_postgres"),
            Self::MySql => crate::localization::text("query.store.disabled_mysql"),
            Self::ClickHouse => crate::localization::text("query.store.disabled_clickhouse"),
            Self::Oracle => crate::localization::text("query.store.disabled_oracle"),
        }
    }
}

fn availability_enabled(engine: InsightEngine, value: Option<&CellValue>) -> bool {
    match engine {
        InsightEngine::SqlServer => match value {
            Some(CellValue::String(state)) => !state.eq_ignore_ascii_case("off"),
            _ => false,
        },
        InsightEngine::Postgres
        | InsightEngine::MySql
        | InsightEngine::ClickHouse
        | InsightEngine::Oracle => match value {
            Some(CellValue::Bool(value)) => *value,
            Some(CellValue::I64(value)) => *value != 0,
            Some(CellValue::F64(value)) => *value != 0.0,
            Some(CellValue::String(value)) => matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "on" | "true" | "yes"
            ),
            _ => false,
        },
    }
}

fn rank_label(engine: Option<InsightEngine>, rank: QueryStoreRank) -> String {
    let key = match (engine, rank) {
        (_, QueryStoreRank::AvgDuration) => "query.store.rank_avg_duration",
        (_, QueryStoreRank::ExecutionCount) => "query.store.rank_executions",
        (Some(InsightEngine::Postgres), QueryStoreRank::AvgLogicalReads) => {
            "query.store.rank_avg_buffer_accesses"
        }
        (Some(InsightEngine::MySql), QueryStoreRank::AvgLogicalReads) => {
            "query.store.rank_avg_rows_examined"
        }
        (Some(InsightEngine::ClickHouse), QueryStoreRank::AvgLogicalReads) => {
            "query.store.rank_avg_rows_read"
        }
        (Some(InsightEngine::Oracle), QueryStoreRank::AvgLogicalReads) => {
            "query.store.rank_avg_buffer_gets"
        }
        (_, QueryStoreRank::AvgLogicalReads) => "query.store.rank_avg_logical_reads",
    };
    crate::localization::text(key)
}

impl EventEmitter<ToolDialogEvent> for QueryStorePane {}

/// Time-window choices for the top-queries view (`None` = all captured).
const TIME_WINDOWS: [(Option<u32>, &str); 4] = [
    (None, "query.store.window_all_time"),
    (Some(1), "query.store.window_last_hour"),
    (Some(24), "query.store.window_last_day"),
    (Some(24 * 7), "query.store.window_last_week"),
];

fn time_window_label(index: usize) -> String {
    crate::localization::text(TIME_WINDOWS[index].1)
}

pub struct QueryStorePane {
    context: ToolDialogContext,
    engine: Option<InsightEngine>,
    database: String,
    availability: Availability,
    grid: Option<Entity<SqllyDataTable>>,
    /// Raw rows behind the grid, kept to resolve the selected row's SQL text.
    rows: Vec<Vec<CellValue>>,
    /// Column index of `query_sql_text` in the top-queries result.
    sql_text_col: Option<usize>,
    /// Current ranking metric; the toolbar cycles the choices.
    rank: QueryStoreRank,
    /// Index into [`TIME_WINDOWS`]; the toolbar cycles the choices.
    window_idx: usize,
    /// Show the recent-failures list instead of the top queries (engines
    /// whose log keeps failures, i.e. ClickHouse).
    failures: bool,
    loading: bool,
    refresh_generation: u64,
    focus_handle: FocusHandle,
    frame: DialogFrame,
    /// True once popped out into its own OS window: the window boundary is
    /// the dialog boundary and close tears the window down.
    windowed: bool,
}

impl QueryStorePane {
    pub fn new(context: ToolDialogContext, cx: &mut gpui::Context<Self>) -> Self {
        let engine = InsightEngine::for_database(context.profile.database_type);
        let database = context.profile.database.clone().unwrap_or_else(|| {
            if engine == Some(InsightEngine::SqlServer) {
                "master".to_string()
            } else {
                crate::localization::text("query.store.current_database")
            }
        });
        let mut this = Self {
            context,
            engine,
            database,
            availability: Availability::Checking,
            grid: None,
            rows: Vec::new(),
            sql_text_col: None,
            rank: QueryStoreRank::AvgDuration,
            window_idx: 0,
            failures: false,
            loading: true,
            refresh_generation: 0,
            focus_handle: cx.focus_handle(),
            // Fixed frame: the embedded data grid manages its own scrolling.
            frame: DialogFrame::new(Theme::scaled(860.0), Theme::scaled(520.0)),
            windowed: false,
        };
        this.check_availability(cx);
        this
    }

    /// The `ALTER DATABASE` statement offered when the store is off.
    pub fn enable_sql(database: &str) -> String {
        format!(
            "ALTER DATABASE [{}] SET QUERY_STORE = ON (OPERATION_MODE = READ_WRITE);",
            database.replace(']', "]]")
        )
    }

    fn check_availability(&mut self, cx: &mut gpui::Context<Self>) {
        self.availability = Availability::Checking;
        self.refresh_generation += 1;
        let generation = self.refresh_generation;
        let Some(engine) = self.engine else {
            self.availability = Availability::Unavailable(crate::localization::text(
                "query.store.unsupported_collector",
            ));
            self.loading = false;
            cx.notify();
            return;
        };
        let sql = engine.availability_query(&self.database);
        crate::tool_dialog::run_query(&self.context, sql, cx, move |this, outcome, cx| {
            if this.refresh_generation != generation {
                return;
            }
            if let Some(err) = outcome.error {
                this.availability = Availability::Unavailable(err);
                this.loading = false;
            } else {
                let state = outcome
                    .sets
                    .first()
                    .and_then(|s| s.rows.first())
                    .and_then(|r| r.first());
                if !availability_enabled(engine, state) {
                    this.availability = Availability::Disabled;
                    this.loading = false;
                } else {
                    this.availability = Availability::Enabled;
                    this.load_top_queries(cx);
                }
            }
            cx.notify();
        });
        cx.notify();
    }

    fn load_top_queries(&mut self, cx: &mut gpui::Context<Self>) {
        self.loading = true;
        self.refresh_generation += 1;
        let generation = self.refresh_generation;
        let hours = self
            .engine
            .filter(|engine| engine.supports_time_window())
            .and_then(|_| TIME_WINDOWS[self.window_idx].0);
        let admin = &*self.context.services.admin_tools;
        let db = self.context.profile.database_type;
        let sql = if self.failures {
            admin.query_insights_failures(db, TOP_QUERY_LIMIT, hours)
        } else {
            admin.query_insights_top(db, &self.database, TOP_QUERY_LIMIT, self.rank, hours)
        };
        let Some(sql) = sql else {
            self.loading = false;
            self.availability = Availability::Unavailable(crate::localization::text(
                "query.store.unsupported_collector",
            ));
            cx.notify();
            return;
        };
        crate::tool_dialog::run_query(&self.context, sql, cx, move |this, outcome, cx| {
            if this.refresh_generation != generation {
                return;
            }
            this.loading = false;
            if let Some(err) = outcome.error {
                this.availability = Availability::Unavailable(err);
                this.grid = None;
                this.rows.clear();
            } else {
                let set = outcome.sets.into_iter().next().unwrap_or_default();
                this.sql_text_col = set
                    .columns
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case("query_sql_text"));
                this.rows = set.rows.clone();
                this.grid = crate::tool_dialog::build_result_grid(&set, cx);
            }
            cx.notify();
        });
        cx.notify();
    }

    /// SQL text of the currently selected grid row, if any.
    fn selected_query_text(&self, cx: &gpui::App) -> Option<String> {
        let col = self.sql_text_col?;
        let grid = self.grid.as_ref()?;
        let row = crate::tool_dialog::selected_source_row(grid, cx)?;
        match self.rows.get(row)?.get(col)? {
            CellValue::String(s) => Some(s.clone()),
            _ => None,
        }
    }

    fn open_selected(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        match self.selected_query_text(cx) {
            Some(sql) => {
                cx.emit(ToolDialogEvent::ScriptToEditor { sql });
                self.close(window, cx);
            }
            None => cx.emit(ToolDialogEvent::Status {
                level: crate::app::StatusLevel::Info,
                message: crate::localization::text("query.store.selection_required"),
            }),
        }
    }

    fn script_enable(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        if let Some(sql) = self
            .engine
            .and_then(|engine| engine.setup_sql(&self.database))
        {
            cx.emit(ToolDialogEvent::ScriptToEditor { sql });
            self.close(window, cx);
        } else {
            cx.emit(ToolDialogEvent::Status {
                level: crate::app::StatusLevel::Info,
                message: crate::localization::text("query.store.manual_setup_required"),
            });
        }
    }

    fn close(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        if self.windowed {
            window.remove_window();
        } else {
            cx.emit(ToolDialogEvent::Close);
        }
    }

    /// Pop out into a standalone OS window; the inline pane closes and this
    /// same pane — grid, ranking, and time window included — carries on in
    /// the window.
    fn pop_out(&mut self, cx: &mut gpui::Context<Self>) {
        let entity = cx.entity();
        // Opening the window renders this pane into it immediately, which
        // must not happen inside its own update — defer past it.
        cx.defer(move |cx| {
            let build_entity = entity.clone();
            let opened = crate::dialog::open_detached(
                crate::dialog::DetachedOptions::localized(
                    "query.store.dialog_title",
                    Theme::scaled(860.0),
                    Theme::scaled(580.0),
                ),
                None,
                cx,
                move |_, cx| {
                    build_entity.update(cx, |pane, _| pane.windowed = true);
                    build_entity
                },
            );
            if opened.is_some() {
                // The window owns the pane now; drop the inline card.
                entity.update(cx, |_, cx| cx.emit(ToolDialogEvent::Close));
            }
        });
    }
}

impl gpui::Focusable for QueryStorePane {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for QueryStorePane {
    fn render(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let availability = self.availability.clone();
        let source_label = self
            .engine
            .map(|engine| engine.source_label().to_string())
            .unwrap_or_else(|| crate::localization::text("query.store.unsupported_engine"));
        let supports_time_window = self.engine.is_some_and(InsightEngine::supports_time_window);
        let supports_failures = self.engine.is_some_and(InsightEngine::supports_failures);
        let showing_failures = self.failures && supports_failures;

        let content: gpui::AnyElement = match &availability {
            Availability::Checking => div()
                .p(px(s(12.0)))
                .text_color(Theme::text_dim())
                .child(crate::localization::format(
                    "query.store.checking",
                    &[("source", &source_label)],
                ))
                .into_any_element(),
            Availability::Unavailable(err) => div()
                .p(px(s(12.0)))
                .text_color(Theme::error())
                .text_size(px(Theme::font_size_ratio(0.87)))
                .whitespace_normal()
                .child(crate::localization::format(
                    "query.store.unavailable",
                    &[
                        ("source", &source_label),
                        ("error", err),
                        (
                            "guidance",
                            &self
                                .engine
                                .map(|engine| engine.unavailable_guidance(&self.database))
                                .unwrap_or_else(|| {
                                    crate::localization::text("query.store.unsupported_guidance")
                                }),
                        ),
                    ],
                ))
                .into_any_element(),
            Availability::Disabled => div()
                .p(px(s(12.0)))
                .flex()
                .flex_col()
                .gap(px(s(8.0)))
                .child(
                    div()
                        .text_color(Theme::text())
                        .text_size(px(Theme::font_size_ratio(0.87)))
                        .whitespace_normal()
                        .child(
                            self.engine
                                .map(|engine| engine.disabled_message(&self.database))
                                .unwrap_or_else(|| {
                                    crate::localization::text("query.store.no_supported_collector")
                                }),
                        ),
                )
                .children(
                    self.engine
                        .filter(|engine| engine.setup_sql(&self.database).is_some())
                        .map(|_| {
                            Button::new("qs-enable")
                                .label(crate::localization::text(
                                    "query.store.script_enable_button",
                                ))
                                .on_click(
                                    cx.listener(|this, _, window, cx| {
                                        this.script_enable(window, cx)
                                    }),
                                )
                        }),
                )
                .into_any_element(),
            Availability::Enabled => {
                if self.loading {
                    div()
                        .p(px(s(12.0)))
                        .text_color(Theme::text_dim())
                        .child(crate::localization::text("query.store.loading"))
                        .into_any_element()
                } else if let Some(grid) = &self.grid {
                    div().size_full().child(grid.clone()).into_any_element()
                } else {
                    div()
                        .p(px(s(12.0)))
                        .text_color(Theme::text_dim())
                        .child(crate::localization::format(
                            "query.store.empty",
                            &[("source", &source_label)],
                        ))
                        .into_any_element()
                }
            }
        };

        let enabled = availability == Availability::Enabled;
        let focus_handle = self.focus_handle.clone();
        let mut dialog = Dialog::new("query-store-pane", |this: &mut Self| &mut this.frame)
            // "Query Insights" is what the menu, command palette, and the
            // pane's own subtitle call this tool; the title bar matches.
            .title(crate::localization::text("query.store.dialog_title"))
            .easy_close(true)
            .key_context("QueryStorePane")
            .track_focus(&focus_handle)
            .windowed(self.windowed)
            .on_close(|this, window, cx| this.close(window, cx))
            // bare_body: the embedded data grid must not sit in a scroll
            // container.
            .bare_body()
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .flex()
                    .flex_col()
                    .gap(px(s(8.0)))
                    .p(px(s(14.0)))
                    .child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.87)))
                            .text_color(Theme::text_dim())
                            .child(crate::localization::format(
                                "query.store.subtitle",
                                &[("source", &source_label), ("database", &self.database)],
                            )),
                    )
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
                            .child(if showing_failures {
                                crate::localization::format(
                                    "query.store.failures_footer",
                                    &[
                                        ("limit", &TOP_QUERY_LIMIT.to_string()),
                                        ("source", &source_label),
                                        (
                                            "window",
                                            &time_window_label(self.window_idx).to_lowercase(),
                                        ),
                                    ],
                                )
                            } else {
                                crate::localization::format(
                                    "query.store.top_queries_footer",
                                    &[
                                        ("limit", &TOP_QUERY_LIMIT.to_string()),
                                        ("source", &source_label),
                                        (
                                            "rank",
                                            &rank_label(self.engine, self.rank).to_lowercase(),
                                        ),
                                        (
                                            "window",
                                            &if supports_time_window {
                                                time_window_label(self.window_idx).to_lowercase()
                                            } else {
                                                crate::localization::text(
                                                    "query.store.cumulative_statistics",
                                                )
                                            },
                                        ),
                                    ],
                                )
                            }),
                    ),
            )
            .leading_button(
                Button::new("qs-rank")
                    .label(crate::localization::format(
                        "query.store.rank_button",
                        &[("rank", &rank_label(self.engine, self.rank))],
                    ))
                    .disabled(!enabled || showing_failures)
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.rank = match this.rank {
                            QueryStoreRank::AvgDuration => QueryStoreRank::ExecutionCount,
                            QueryStoreRank::ExecutionCount => QueryStoreRank::AvgLogicalReads,
                            QueryStoreRank::AvgLogicalReads => QueryStoreRank::AvgDuration,
                        };
                        this.load_top_queries(cx);
                    })),
            )
            .leading_button(
                Button::new("qs-window")
                    .label(if supports_time_window {
                        time_window_label(self.window_idx)
                    } else {
                        crate::localization::text("query.store.cumulative_since_reset")
                    })
                    .disabled(!enabled || !supports_time_window)
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.window_idx = (this.window_idx + 1) % TIME_WINDOWS.len();
                        this.load_top_queries(cx);
                    })),
            )
            .leading_button(
                Button::new("qs-refresh")
                    .label(crate::localization::text("query.store.refresh_button"))
                    .disabled(self.loading)
                    .on_click(cx.listener(|this, _, _window, cx| this.check_availability(cx))),
            )
            .button(
                Button::new("qs-open")
                    .primary()
                    .label(crate::localization::text(
                        "query.store.open_selected_button",
                    ))
                    .disabled(!enabled)
                    .on_click(cx.listener(|this, _, window, cx| this.open_selected(window, cx))),
            );
        if supports_failures {
            dialog = dialog.leading_button(
                Button::new("qs-failures")
                    .label(if showing_failures {
                        crate::localization::text("query.store.showing_failures_button")
                    } else {
                        crate::localization::text("query.store.showing_top_button")
                    })
                    .disabled(!enabled)
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.failures = !this.failures;
                        this.load_top_queries(cx);
                    })),
            );
        }
        if !self.windowed {
            dialog = dialog.detachable(|this, _window, cx| this.pop_out(cx));
        }
        dialog.build(self, window, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_services::{AdminToolsService, DefaultAdminToolsService};

    #[test]
    fn enable_sql_targets_the_database() {
        assert_eq!(
            QueryStorePane::enable_sql("Sales"),
            "ALTER DATABASE [Sales] SET QUERY_STORE = ON (OPERATION_MODE = READ_WRITE);"
        );
    }

    #[test]
    fn enable_sql_escapes_brackets() {
        assert!(QueryStorePane::enable_sql("we]ird").contains("[we]]ird]"));
    }

    #[test]
    fn selected_row_resolves_common_selections() {
        use sqlly_datatable::Selection;
        let selected_row = crate::tool_dialog::selected_row;
        assert_eq!(selected_row(&Selection::Cell(3, 1)), Some(3));
        assert_eq!(selected_row(&Selection::CellRange(5, 0, 2, 1)), Some(2));
        assert_eq!(selected_row(&Selection::None), None);
    }

    #[test]
    fn top_queries_sql_mentions_query_store_views() {
        let admin = DefaultAdminToolsService;
        let sql = admin.query_store_top("Sales", TOP_QUERY_LIMIT);
        assert!(sql.contains("sys.query_store_query"));
        assert!(sql.contains("TOP (50)"));
    }

    #[test]
    fn ranked_query_orders_by_metric_and_bounds_window() {
        let admin = DefaultAdminToolsService;
        let sql = admin
            .query_insights_top(
                sqlly_core::DatabaseType::SqlServer,
                "Sales",
                50,
                QueryStoreRank::ExecutionCount,
                Some(24),
            )
            .unwrap();
        assert!(sql.contains("ORDER BY rs.count_executions DESC"), "{sql}");
        assert!(
            sql.contains("DATEADD(HOUR, -24, SYSUTCDATETIME())"),
            "{sql}"
        );
        let unbounded = admin
            .query_insights_top(
                sqlly_core::DatabaseType::SqlServer,
                "Sales",
                50,
                QueryStoreRank::AvgLogicalReads,
                None,
            )
            .unwrap();
        assert!(
            unbounded.contains("ORDER BY rs.avg_logical_io_reads DESC"),
            "{unbounded}"
        );
        assert!(!unbounded.contains("DATEADD"), "{unbounded}");
    }

    /// Oracle rides the same pane: the shared pool is the source, ranking
    /// by buffer gets is labelled honestly, and the time window (a Query
    /// Store feature) stays off.
    #[test]
    fn oracle_insights_come_from_the_shared_pool() {
        let engine = InsightEngine::for_database(sqlly_core::DatabaseType::Oracle).unwrap();
        assert_eq!(engine, InsightEngine::Oracle);
        assert!(engine.source_label().contains("V$SQLAREA"));
        assert!(engine.availability_query("ignored").contains("v$sqlarea"));
        assert!(!engine.supports_time_window());
        assert!(engine.setup_sql("ignored").is_none());
        assert!(
            engine
                .unavailable_guidance("ignored")
                .contains("V_$SQLAREA")
        );
        assert!(availability_enabled(engine, Some(&CellValue::I64(1))));
        assert!(!availability_enabled(engine, Some(&CellValue::I64(0))));
        assert_eq!(
            rank_label(Some(engine), QueryStoreRank::AvgLogicalReads),
            "Avg Buffer Gets"
        );
        let sql = DefaultAdminToolsService
            .query_insights_top(
                sqlly_core::DatabaseType::Oracle,
                "ignored",
                TOP_QUERY_LIMIT,
                QueryStoreRank::AvgDuration,
                None,
            )
            .unwrap();
        assert!(sql.contains("query_sql_text"), "{sql}");
    }

    #[test]
    fn time_window_choices_are_distinct_labels() {
        let labels: Vec<_> = TIME_WINDOWS.iter().map(|(_, l)| *l).collect();
        let mut unique = labels.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), labels.len());
    }

    #[test]
    fn insight_engines_use_native_collectors_and_truthful_time_controls() {
        let sql_server = InsightEngine::for_database(sqlly_core::DatabaseType::SqlServer).unwrap();
        let postgres = InsightEngine::for_database(sqlly_core::DatabaseType::Postgres).unwrap();
        let mysql = InsightEngine::for_database(sqlly_core::DatabaseType::MySql).unwrap();
        assert!(
            sql_server
                .availability_query("Sales")
                .contains("query_store_options")
        );
        assert!(
            postgres
                .availability_query("ignored")
                .contains("pg_extension")
        );
        assert!(
            mysql
                .availability_query("ignored")
                .contains("@@performance_schema")
        );
        assert!(sql_server.supports_time_window());
        assert!(!postgres.supports_time_window());
        assert!(!mysql.supports_time_window());
        assert!(InsightEngine::for_database(sqlly_core::DatabaseType::Sqlite).is_none());
        let clickhouse = InsightEngine::for_database(sqlly_core::DatabaseType::ClickHouse).unwrap();
        assert!(
            clickhouse
                .availability_query("ignored")
                .contains("log_queries")
        );
        assert!(
            clickhouse
                .availability_query("ignored")
                .contains("query_log")
        );
        assert!(clickhouse.supports_time_window());
        assert!(clickhouse.supports_failures());
        assert!(!sql_server.supports_failures());
        assert!(clickhouse.setup_sql("ignored").is_none());
        assert!(
            clickhouse
                .disabled_message("ignored")
                .contains("log_queries")
        );
        assert!(
            clickhouse
                .unavailable_guidance("ignored")
                .contains("SYSTEM FLUSH LOGS")
        );
        assert!(availability_enabled(clickhouse, Some(&CellValue::I64(1))));
        assert!(!availability_enabled(clickhouse, Some(&CellValue::I64(0))));
        assert_eq!(
            rank_label(Some(clickhouse), QueryStoreRank::AvgLogicalReads),
            "Avg Rows Read"
        );
    }

    #[test]
    fn clickhouse_insights_read_the_query_log_with_a_window_and_failures() {
        let admin = DefaultAdminToolsService;
        let db = sqlly_core::DatabaseType::ClickHouse;
        let sql = admin
            .query_insights_top(db, "ignored", 50, QueryStoreRank::AvgLogicalReads, Some(24))
            .unwrap();
        assert!(sql.contains("FROM system.query_log"), "{sql}");
        assert!(sql.contains("normalized_query_hash"), "{sql}");
        assert!(sql.contains("normalizeQuery(query)"), "{sql}");
        assert!(sql.contains("type = 'QueryFinish'"), "{sql}");
        assert!(sql.contains("INTERVAL 24 HOUR"), "{sql}");
        assert!(
            sql.contains("ORDER BY avg_read_rows DESC LIMIT 50"),
            "{sql}"
        );
        let all_time = admin
            .query_insights_top(db, "ignored", 10, QueryStoreRank::AvgDuration, None)
            .unwrap();
        assert!(!all_time.contains("INTERVAL"), "{all_time}");
        assert!(all_time.contains("ORDER BY avg_duration_ms DESC LIMIT 10"));
        let failures = admin.query_insights_failures(db, 20, Some(1)).unwrap();
        assert!(failures.contains("'ExceptionBeforeStart', 'ExceptionWhileProcessing'"));
        assert!(failures.contains("query AS query_sql_text"), "{failures}");
        assert!(failures.contains("INTERVAL 1 HOUR"), "{failures}");
        assert!(failures.contains("ORDER BY event_time DESC LIMIT 20"));
        assert!(
            admin
                .query_insights_failures(sqlly_core::DatabaseType::Postgres, 20, None)
                .is_none()
        );
    }

    #[test]
    fn collector_setup_is_only_offered_when_sql_can_help() {
        assert!(
            InsightEngine::SqlServer
                .setup_sql("Sales")
                .unwrap()
                .contains("QUERY_STORE = ON")
        );
        assert!(
            InsightEngine::Postgres
                .setup_sql("ignored")
                .unwrap()
                .contains("CREATE EXTENSION")
        );
        assert!(InsightEngine::MySql.setup_sql("ignored").is_none());
    }

    #[test]
    fn availability_values_decode_each_driver_shape() {
        assert!(availability_enabled(
            InsightEngine::SqlServer,
            Some(&CellValue::String("READ_WRITE".into()))
        ));
        assert!(!availability_enabled(
            InsightEngine::SqlServer,
            Some(&CellValue::String("OFF".into()))
        ));
        assert!(availability_enabled(
            InsightEngine::Postgres,
            Some(&CellValue::Bool(true))
        ));
        assert!(availability_enabled(
            InsightEngine::MySql,
            Some(&CellValue::I64(1))
        ));
        assert!(!availability_enabled(
            InsightEngine::MySql,
            Some(&CellValue::I64(0))
        ));
    }

    #[test]
    fn work_rank_label_matches_the_engine_metric() {
        assert_eq!(
            rank_label(
                Some(InsightEngine::SqlServer),
                QueryStoreRank::AvgLogicalReads
            ),
            "Avg Logical Reads"
        );
        assert_eq!(
            rank_label(
                Some(InsightEngine::Postgres),
                QueryStoreRank::AvgLogicalReads
            ),
            "Avg Buffer Accesses"
        );
        assert_eq!(
            rank_label(Some(InsightEngine::MySql), QueryStoreRank::AvgLogicalReads),
            "Avg Rows Examined"
        );
    }

    /// The pane's grid, drawn in the real pane, gets room for its rows (a
    /// grid in an auto-height wrapper paints zero pixels tall).
    #[gpui::test]
    fn query_insights_grid_gets_room_to_paint_its_rows(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            if cx.try_global::<gpui_component::Theme>().is_none() {
                gpui_component::init(cx);
            }
        });
        let context =
            crate::tool_dialog::unreachable_tool_context(sqlly_core::DatabaseType::SqlServer);
        let (view, cx) = cx.add_window_view(|_window, cx| QueryStorePane::new(context, cx));
        cx.simulate_resize(gpui::size(gpui::px(1440.0), gpui::px(900.0)));
        cx.run_until_parked();
        view.update(cx, |pane, cx| {
            pane.availability = Availability::Enabled;
            pane.loading = false;
            pane.grid =
                crate::tool_dialog::build_result_grid(&crate::tool_dialog::sample_tool_set(20), cx);
            cx.notify();
        });
        cx.run_until_parked();
        view.read_with(cx, |pane, cx| {
            let grid = pane.grid.as_ref().expect("grid");
            crate::tool_dialog::assert_grid_shows_rows(grid, 3, "Query Insights", cx);
        });
    }
}
