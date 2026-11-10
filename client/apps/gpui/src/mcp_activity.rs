//! MCP Activity — browse and export the local MCP server audit log.

#[cfg(not(target_arch = "wasm32"))]
use std::path::Path;

use gpui::{
    Context, Entity, EventEmitter, FocusHandle, IntoElement, ParentElement, Render, Styled, Task,
    Window, div, prelude::*, px,
};
use gpui_component::Sizable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::select::{Select, SelectEvent, SelectState};
use sqlly_core::{CellValue, ResultColumn};
use sqlly_datatable::SqllyDataTable;
use sqlly_services::mcp::audit::{self, AuditApproval, AuditOutcome, AuditRecord};
use uuid::Uuid;

use crate::dialog::{Dialog, DialogFrame};
use crate::theme::Theme;
use crate::tool_dialog::{ToolDialogEvent, ToolQuerySet, build_result_grid};

const READ_LIMIT: usize = 5_000;

/// Outcome filter values; the outcome select lists "all" first, then these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutcomeFilter {
    Ok,
    Refused,
    Error,
}

const OUTCOME_CHOICES: [OutcomeFilter; 3] = [
    OutcomeFilter::Ok,
    OutcomeFilter::Refused,
    OutcomeFilter::Error,
];

impl OutcomeFilter {
    fn of(outcome: &AuditOutcome) -> Self {
        match outcome {
            AuditOutcome::Ok => OutcomeFilter::Ok,
            AuditOutcome::Refused(_) => OutcomeFilter::Refused,
            AuditOutcome::Error(_) => OutcomeFilter::Error,
        }
    }
}

/// Row 0 of the outcome select is "all outcomes"; the rest follow
/// [`OUTCOME_CHOICES`].
fn outcome_filter_from_row(row: Option<usize>) -> Option<OutcomeFilter> {
    row.and_then(|row| row.checked_sub(1))
        .and_then(|ix| OUTCOME_CHOICES.get(ix).copied())
}

/// Row 0 of a filter select is its translated "all" entry; later rows map
/// into `options`, so a value spelled like the "all" label still filters.
fn filter_option_from_row<T: Clone>(options: &[T], row: Option<usize>) -> Option<T> {
    row.and_then(|row| row.checked_sub(1))
        .and_then(|ix| options.get(ix).cloned())
}

pub(crate) fn filter_records<'a>(
    records: &'a [AuditRecord],
    tool: Option<&str>,
    outcome: Option<OutcomeFilter>,
    connection: Option<Uuid>,
) -> Vec<&'a AuditRecord> {
    records
        .iter()
        .filter(|record| {
            tool.is_none_or(|needle| record.tool == needle)
                && outcome.is_none_or(|needle| OutcomeFilter::of(&record.outcome) == needle)
                && connection.is_none_or(|id| record.connection_id == Some(id))
        })
        .collect()
}

fn outcome_label(record: &AuditRecord) -> String {
    let key = match record.outcome {
        AuditOutcome::Ok => "mcp.activity.outcome_ok",
        AuditOutcome::Refused(_) => "mcp.activity.outcome_refused",
        AuditOutcome::Error(_) => "mcp.activity.outcome_error",
    };
    crate::localization::text(key)
}

fn outcome_reason(record: &AuditRecord) -> String {
    match &record.outcome {
        AuditOutcome::Ok => String::new(),
        AuditOutcome::Refused(reason) | AuditOutcome::Error(reason) => reason.clone(),
    }
}

fn approval_label(approval: Option<AuditApproval>) -> String {
    let key = match approval {
        None => return String::new(),
        Some(AuditApproval::Pending) => "mcp.activity.approval_pending",
        Some(AuditApproval::Approved) => "mcp.activity.approval_approved",
        Some(AuditApproval::Rejected) => "mcp.activity.approval_rejected",
        Some(AuditApproval::Expired) => "mcp.activity.approval_expired",
        Some(AuditApproval::Executed) => "mcp.activity.approval_executed",
    };
    crate::localization::text(key)
}

pub(crate) fn records_to_rows(
    records: &[&AuditRecord],
) -> (Vec<ResultColumn>, Vec<Vec<CellValue>>) {
    let columns = vec![
        column(&crate::localization::text("mcp.activity.column_time")),
        column(&crate::localization::text("mcp.activity.column_tool")),
        column(&crate::localization::text("mcp.activity.column_connection")),
        column(&crate::localization::text("mcp.activity.column_database")),
        column(&crate::localization::text("mcp.activity.column_outcome")),
        column(&crate::localization::text("mcp.activity.column_reason")),
        column(&crate::localization::text("mcp.activity.column_rows")),
        column(&crate::localization::text("mcp.activity.column_duration")),
        column(&crate::localization::text("mcp.activity.column_approval")),
        column(&crate::localization::text("mcp.activity.column_sql")),
    ];
    let rows = records
        .iter()
        .map(|record| {
            vec![
                CellValue::String(record.ts.format("%Y-%m-%d %H:%M:%S").to_string()),
                CellValue::String(record.tool.clone()),
                CellValue::String(record.connection_name.clone().unwrap_or_default()),
                CellValue::String(record.database.clone().unwrap_or_default()),
                CellValue::String(outcome_label(record)),
                CellValue::String(outcome_reason(record)),
                CellValue::String(record.rows.map(|n| n.to_string()).unwrap_or_default()),
                CellValue::String(
                    record
                        .duration_ms
                        .map(|n| n.to_string())
                        .unwrap_or_default(),
                ),
                CellValue::String(approval_label(record.approval)),
                CellValue::String(record.sql_redacted.clone().unwrap_or_default()),
            ]
        })
        .collect();
    (columns, rows)
}

fn column(name: &str) -> ResultColumn {
    ResultColumn {
        name: name.to_string(),
        data_type: "text".into(),
        nullable: true,
    }
}

impl EventEmitter<ToolDialogEvent> for McpActivityView {}

pub struct McpActivityView {
    records: Vec<AuditRecord>,
    grid: Option<Entity<SqllyDataTable>>,
    columns: Vec<ResultColumn>,
    rows: Vec<Vec<CellValue>>,
    filter_tool: Option<String>,
    filter_outcome: Option<OutcomeFilter>,
    filter_connection: Option<Uuid>,
    /// Tool names behind the tool select's rows after its "all" entry.
    tool_options: Vec<String>,
    /// Connection ids behind the connection select's rows after its "all" entry.
    connection_options: Vec<Option<Uuid>>,
    tool_select: Entity<SelectState<Vec<String>>>,
    outcome_select: Entity<SelectState<Vec<String>>>,
    connection_select: Entity<SelectState<Vec<String>>>,
    error: Option<String>,
    focus_handle: FocusHandle,
    frame: DialogFrame,
    _load: Task<()>,
}

impl McpActivityView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let tool_select = cx.new(|cx| {
            SelectState::new(
                vec![crate::localization::text("mcp.activity.filter_all_tools")],
                Some(gpui_component::IndexPath::new(0)),
                window,
                cx,
            )
        });
        let outcome_select = cx.new(|cx| {
            SelectState::new(
                crate::localization::select_labels(window, cx, || {
                    vec![
                        crate::localization::text("mcp.activity.filter_all_outcomes"),
                        crate::localization::text("mcp.activity.outcome_ok"),
                        crate::localization::text("mcp.activity.outcome_refused"),
                        crate::localization::text("mcp.activity.outcome_error"),
                    ]
                }),
                Some(gpui_component::IndexPath::new(0)),
                window,
                cx,
            )
        });
        let connection_select = cx.new(|cx| {
            SelectState::new(
                vec![crate::localization::text(
                    "mcp.activity.filter_all_connections",
                )],
                Some(gpui_component::IndexPath::new(0)),
                window,
                cx,
            )
        });
        let mut view = Self {
            records: Vec::new(),
            grid: None,
            columns: Vec::new(),
            rows: Vec::new(),
            filter_tool: None,
            filter_outcome: None,
            filter_connection: None,
            tool_options: Vec::new(),
            connection_options: Vec::new(),
            tool_select,
            outcome_select,
            connection_select,
            error: None,
            focus_handle: cx.focus_handle(),
            frame: DialogFrame::new(Theme::scaled(960.0), Theme::scaled(620.0)),
            _load: Task::ready(()),
        };
        view.subscribe_selects(window, cx);
        cx.observe_global_in::<crate::localization::LocalizationChanged>(
            window,
            |view, window, cx| {
                let tool = view.tool_select.read(cx).selected_index(cx);
                let connection = view.connection_select.read(cx).selected_index(cx);
                view.populate_selects(window, cx);
                view.tool_select
                    .update(cx, |select, cx| select.set_selected_index(tool, window, cx));
                view.connection_select.update(cx, |select, cx| {
                    select.set_selected_index(connection, window, cx)
                });
                view.rebuild_grid(cx);
            },
        )
        .detach();
        view.refresh(window, cx);
        view
    }

    #[cfg(test)]
    pub fn set_records_for_test(&mut self, records: Vec<AuditRecord>, cx: &mut Context<Self>) {
        self.records = records;
        self.rebuild_grid(cx);
    }

    fn subscribe_selects(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.subscribe_in(
            &self.tool_select,
            window,
            |this, select, event, _window, cx| {
                let SelectEvent::Confirm(value) = event;
                let row = value
                    .as_ref()
                    .and_then(|_| select.read(cx).selected_index(cx))
                    .map(|ix| ix.row);
                this.filter_tool = filter_option_from_row(&this.tool_options, row);
                this.rebuild_grid(cx);
            },
        )
        .detach();
        cx.subscribe_in(
            &self.outcome_select,
            window,
            |this, select, event, _window, cx| {
                let SelectEvent::Confirm(value) = event;
                let row = value
                    .as_ref()
                    .and_then(|_| select.read(cx).selected_index(cx))
                    .map(|ix| ix.row);
                this.filter_outcome = outcome_filter_from_row(row);
                this.rebuild_grid(cx);
            },
        )
        .detach();
        cx.subscribe_in(
            &self.connection_select,
            window,
            |this, select, event, _window, cx| {
                let SelectEvent::Confirm(value) = event;
                let row = value
                    .as_ref()
                    .and_then(|_| select.read(cx).selected_index(cx))
                    .map(|ix| ix.row);
                if row == Some(0) {
                    this.filter_connection = None;
                } else if let Some(id) = filter_option_from_row(&this.connection_options, row) {
                    this.filter_connection = id;
                }
                this.rebuild_grid(cx);
            },
        )
        .detach();
    }

    fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            match audit::read_recent(READ_LIMIT) {
                Ok(records) => {
                    self.records = records;
                    self.error = None;
                    self.populate_selects(window, cx);
                    self.rebuild_grid(cx);
                }
                Err(message) => {
                    self.error = Some(message);
                    cx.notify();
                }
            }
        }
        #[cfg(target_arch = "wasm32")]
        {
            self.error = Some(crate::browser_preview::unavailable(
                &crate::localization::text("mcp.activity.browser_unavailable"),
            ));
            cx.notify();
        }
    }

    fn populate_selects(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut tools = vec![crate::localization::text("mcp.activity.filter_all_tools")];
        let mut seen_tools = std::collections::BTreeSet::new();
        self.tool_options.clear();
        for record in &self.records {
            if seen_tools.insert(record.tool.clone()) {
                tools.push(record.tool.clone());
                self.tool_options.push(record.tool.clone());
            }
        }
        self.tool_select.update(cx, |select, cx| {
            select.set_items(tools, window, cx);
            select.set_selected_index(Some(gpui_component::IndexPath::new(0)), window, cx);
        });

        let mut connections = vec![crate::localization::text(
            "mcp.activity.filter_all_connections",
        )];
        let mut seen = std::collections::BTreeSet::new();
        self.connection_options.clear();
        for record in &self.records {
            if let Some(name) = &record.connection_name
                && seen.insert(name.clone())
            {
                connections.push(name.clone());
                self.connection_options.push(record.connection_id);
            }
        }
        self.connection_select.update(cx, |select, cx| {
            select.set_items(connections, window, cx);
            select.set_selected_index(Some(gpui_component::IndexPath::new(0)), window, cx);
        });
    }

    fn rebuild_grid(&mut self, cx: &mut Context<Self>) {
        let filtered = filter_records(
            &self.records,
            self.filter_tool.as_deref(),
            self.filter_outcome,
            self.filter_connection,
        );
        let (columns, rows) = records_to_rows(&filtered);
        self.columns = columns.clone();
        self.rows = rows;
        self.grid = build_result_grid(
            &ToolQuerySet {
                columns,
                rows: self.rows.clone(),
            },
            cx,
        );
        cx.notify();
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn export_csv(&mut self, cx: &mut Context<Self>) {
        let columns = self.columns.clone();
        let rows = self.rows.clone();
        let opts = crate::results::adhoc_export_options(
            crate::results::ExportFormat::Csv,
            &crate::preferences::ExportPreferences::load(),
        );
        let rx = cx.prompt_for_new_path(Path::new("."), Some("mcp-activity.csv"));
        cx.spawn(async move |view, cx| {
            let Some(path) = rx.await.ok().and_then(|result| result.ok()).flatten() else {
                return;
            };
            let body = crate::result_export::csv_with(&columns, &rows, &opts);
            let result = std::fs::write(&path, body);
            let _ = view.update(cx, |view, cx| {
                view.error = result.err().map(|error| error.to_string());
                cx.emit(ToolDialogEvent::Status {
                    level: crate::app::StatusLevel::Info,
                    message: crate::localization::format(
                        "mcp.activity.exported_csv",
                        &[("path", &path.display().to_string())],
                    ),
                });
                cx.notify();
            });
        })
        .detach();
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn export_json(&mut self, cx: &mut Context<Self>) {
        let columns = self.columns.clone();
        let rows = self.rows.clone();
        let rx = cx.prompt_for_new_path(Path::new("."), Some("mcp-activity.json"));
        cx.spawn(async move |view, cx| {
            let Some(path) = rx.await.ok().and_then(|result| result.ok()).flatten() else {
                return;
            };
            let body = crate::result_export::json(&columns, &rows);
            let result = std::fs::write(&path, body);
            let _ = view.update(cx, |view, cx| {
                view.error = result.err().map(|error| error.to_string());
                cx.emit(ToolDialogEvent::Status {
                    level: crate::app::StatusLevel::Info,
                    message: crate::localization::format(
                        "mcp.activity.exported_json",
                        &[("path", &path.display().to_string())],
                    ),
                });
                cx.notify();
            });
        })
        .detach();
    }
}

impl Render for McpActivityView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let filtered_count = filter_records(
            &self.records,
            self.filter_tool.as_deref(),
            self.filter_outcome,
            self.filter_connection,
        )
        .len();
        let footer = crate::localization::format(
            "mcp.activity.footer",
            &[
                ("filtered_count", &filtered_count.to_string()),
                ("total_count", &self.records.len().to_string()),
                ("path", &audit::audit_dir().display().to_string()),
            ],
        );

        // `AppView` stacks this entity after the full-size workbench: it
        // needs the dialog chrome's overlay, or it lays out below the window.
        let content = div()
            .id("mcp-activity")
            .size_full()
            .flex()
            .flex_col()
            .gap(px(s(8.0)))
            .p(px(s(14.0)))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(s(6.0)))
                    .child(Select::new(&self.tool_select))
                    .child(Select::new(&self.outcome_select))
                    .child(Select::new(&self.connection_select))
                    .child(
                        Button::new("mcp-activity-refresh")
                            .small()
                            .label(crate::localization::text("mcp.activity.refresh_button"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.refresh(window, cx);
                            })),
                    )
                    .child(
                        Button::new("mcp-activity-export-csv")
                            .small()
                            .ghost()
                            .label(crate::localization::text("mcp.activity.export_csv_button"))
                            .on_click(cx.listener(|this, _, _window, cx| {
                                #[cfg(not(target_arch = "wasm32"))]
                                this.export_csv(cx);
                            })),
                    )
                    .child(
                        Button::new("mcp-activity-export-json")
                            .small()
                            .ghost()
                            .label(crate::localization::text("mcp.activity.export_json_button"))
                            .on_click(cx.listener(|this, _, _window, cx| {
                                #[cfg(not(target_arch = "wasm32"))]
                                this.export_json(cx);
                            })),
                    ),
            )
            .children(self.error.as_ref().map(|error| {
                div()
                    .text_size(px(Theme::font_size_ratio(0.9)))
                    .text_color(Theme::error())
                    .child(error.clone())
            }))
            .child(
                div()
                    .flex_1()
                    .min_h(px(s(160.0)))
                    .border_1()
                    .border_color(Theme::border())
                    .rounded(px(s(4.0)))
                    .children(self.grid.clone().map(|grid| div().size_full().child(grid))),
            );
        let focus_handle = self.focus_handle.clone();
        Dialog::new("mcp-activity-dialog", |this: &mut Self| &mut this.frame)
            .title(crate::localization::text("mcp.activity.dialog_title"))
            .easy_close(true)
            .key_context("McpActivity")
            .track_focus(&focus_handle)
            .bare_body()
            .on_close(|_this, _window, cx| cx.emit(ToolDialogEvent::Close))
            .child(content)
            .leading_button(
                div()
                    .text_size(px(Theme::font_size_ratio(0.74)))
                    .text_color(Theme::text_dim())
                    .child(footer),
            )
            .build(self, window, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn sample_record(tool: &str, connection: Option<Uuid>, name: &str) -> AuditRecord {
        AuditRecord {
            ts: Utc::now(),
            tool: tool.to_string(),
            connection_id: connection,
            connection_name: Some(name.to_string()),
            database: Some("app".to_string()),
            sql_hash: None,
            sql_redacted: Some("SELECT '<redacted>'".to_string()),
            duration_ms: Some(12),
            rows: Some(1),
            outcome: AuditOutcome::Ok,
            proposal_id: None,
            approval: None,
        }
    }

    #[test]
    fn apply_filters_matches_tool_outcome_connection() {
        let id = Uuid::new_v4();
        let records = vec![
            sample_record("run_readonly_query", Some(id), "dev"),
            sample_record("propose_write", Some(Uuid::new_v4()), "other"),
        ];
        let filtered = filter_records(
            &records,
            Some("run_readonly_query"),
            Some(OutcomeFilter::Ok),
            Some(id),
        );
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].tool, "run_readonly_query");
        assert!(filter_records(&records, None, Some(OutcomeFilter::Refused), None).is_empty());
    }

    #[test]
    fn filter_rows_skip_the_all_entry() {
        assert_eq!(outcome_filter_from_row(Some(0)), None);
        assert_eq!(outcome_filter_from_row(Some(1)), Some(OutcomeFilter::Ok));
        assert_eq!(
            outcome_filter_from_row(Some(2)),
            Some(OutcomeFilter::Refused)
        );
        assert_eq!(outcome_filter_from_row(Some(3)), Some(OutcomeFilter::Error));
        assert_eq!(outcome_filter_from_row(None), None);
        // A tool literally named like the "all" label is still a real filter.
        let tools = vec![crate::localization::text("mcp.activity.filter_all_tools")];
        assert_eq!(filter_option_from_row(&tools, Some(0)), None);
        assert_eq!(
            filter_option_from_row(&tools, Some(1)),
            Some(tools[0].clone())
        );
        assert_eq!(filter_option_from_row(&tools, Some(5)), None);
    }

    #[test]
    fn records_to_rows_shape() {
        let id = Uuid::new_v4();
        let records = [sample_record("proposal", Some(id), "dev")];
        let refs: Vec<_> = records.iter().collect();
        let (columns, rows) = records_to_rows(&refs);
        assert_eq!(columns.len(), 10);
        assert_eq!(rows.len(), 1);
        match &rows[0][9] {
            CellValue::String(sql) => assert!(sql.contains("redacted")),
            _ => panic!("expected redacted sql column"),
        }
    }

    #[gpui::test]
    fn viewer_renders_grid_from_records(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            if cx.try_global::<gpui_component::Theme>().is_none() {
                gpui_component::init(cx);
            }
        });
        let records = vec![
            sample_record("run_readonly_query", Some(Uuid::new_v4()), "dev"),
            sample_record("propose_write", Some(Uuid::new_v4()), "dev"),
            sample_record("proposal", Some(Uuid::new_v4()), "dev"),
        ];
        let (view, cx) = cx.add_window_view(|window, cx| {
            let mut activity = McpActivityView::new(window, cx);
            activity.set_records_for_test(records, cx);
            activity
        });
        view.read_with(cx, |activity, _| assert!(activity.grid.is_some()));
    }

    /// MCP Activity rendered a plain block that `AppView` stacks after the
    /// full-size workbench, so it opened below the window. It is a dialog
    /// card inside the window now, and its grid has room for its rows.
    #[gpui::test]
    fn viewer_paints_as_a_dialog_with_its_rows(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            if cx.try_global::<gpui_component::Theme>().is_none() {
                gpui_component::init(cx);
            }
        });
        let records = vec![
            sample_record("run_readonly_query", Some(Uuid::new_v4()), "dev"),
            sample_record("propose_write", Some(Uuid::new_v4()), "dev"),
        ];
        let (host, cx) = cx.add_window_view(|window, cx| {
            let activity = cx.new(|cx| {
                let mut activity = McpActivityView::new(window, cx);
                activity.set_records_for_test(records, cx);
                activity
            });
            crate::tool_dialog::OverlayStack {
                overlay: activity.into(),
            }
        });
        cx.simulate_resize(gpui::size(px(1440.0), px(900.0)));
        cx.run_until_parked();
        crate::tool_dialog::assert_card_inside_window(cx, "dialog-card-mcp-activity-dialog", 900.0);
        let activity = host.read_with(cx, |host, _| host.overlay.clone());
        let activity = activity
            .downcast::<McpActivityView>()
            .expect("activity view");
        activity.read_with(cx, |activity, cx| {
            let grid = activity.grid.as_ref().expect("grid");
            crate::tool_dialog::assert_grid_shows_rows(grid, 2, "MCP activity", cx);
        });
    }
}
