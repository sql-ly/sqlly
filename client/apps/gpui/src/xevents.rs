//! Extended Events pane — session list with running state, per-session
//! event/target detail, and script-first start/stop.
//!
//! Scope guards: a server node lists *server*-scoped sessions
//! (`sys.server_event_sessions`); a right-clicked database lists that
//! database's *database*-scoped sessions (`sys.database_event_sessions`) —
//! also the only kind Azure SQL Database exposes, so a server-scope open there
//! falls back to database scope. Start/stop never execute from the pane — the
//! ALTER
//! EVENT SESSION statement scripts into a tab for review, matching the
//! script-first admin contract.

use gpui::{
    Entity, EventEmitter, FocusHandle, IntoElement, ParentElement, Render, Styled, Window, div, px,
};
use gpui_component::Disableable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use sqlly_core::CellValue;
use sqlly_datatable::SqllyDataTable;

use crate::app::StatusLevel;
use crate::dialog::{Dialog, DialogFrame};
use crate::theme::Theme;
use crate::tool_dialog::{ToolDialogContext, ToolDialogEvent, ToolScope};

/// Which catalog the sessions came from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum XeScope {
    Server,
    Database,
}

impl XeScope {
    pub fn label(self) -> String {
        crate::localization::text(match self {
            XeScope::Server => "explorer.xevents.scope_server",
            XeScope::Database => "explorer.xevents.scope_database",
        })
    }

    /// Sessions with their running state (LEFT JOIN against the live DMV).
    pub fn sessions_query(self) -> &'static str {
        match self {
            XeScope::Server => {
                "SELECT s.name, s.startup_state, \
                 CASE WHEN d.name IS NULL THEN 0 ELSE 1 END AS is_running \
                 FROM sys.server_event_sessions s \
                 LEFT JOIN sys.dm_xe_sessions d ON d.name = s.name \
                 ORDER BY s.name"
            }
            XeScope::Database => {
                "SELECT s.name, s.startup_state, \
                 CASE WHEN d.name IS NULL THEN 0 ELSE 1 END AS is_running \
                 FROM sys.database_event_sessions s \
                 LEFT JOIN sys.dm_xe_database_sessions d ON d.name = s.name \
                 ORDER BY s.name"
            }
        }
    }

    /// Events + targets for one session.
    pub fn detail_query(self, session: &str) -> String {
        let name = session.replace('\'', "''");
        match self {
            XeScope::Server => format!(
                "SELECT e.name AS event_name, t.name AS target_name \
                 FROM sys.server_event_sessions s \
                 LEFT JOIN sys.server_event_session_events e \
                 ON s.event_session_id = e.event_session_id \
                 LEFT JOIN sys.server_event_session_targets t \
                 ON s.event_session_id = t.event_session_id \
                 WHERE s.name = N'{name}' ORDER BY e.name"
            ),
            XeScope::Database => format!(
                "SELECT e.name AS event_name, t.name AS target_name \
                 FROM sys.database_event_sessions s \
                 LEFT JOIN sys.database_event_session_events e \
                 ON s.event_session_id = e.event_session_id \
                 LEFT JOIN sys.database_event_session_targets t \
                 ON s.event_session_id = t.event_session_id \
                 WHERE s.name = N'{name}' ORDER BY e.name"
            ),
        }
    }

    /// The reviewed start/stop statement for a session in this scope.
    pub fn state_change_sql(self, session: &str, start: bool) -> String {
        let scope = match self {
            XeScope::Server => "SERVER",
            XeScope::Database => "DATABASE",
        };
        let state = if start { "START" } else { "STOP" };
        format!(
            "ALTER EVENT SESSION [{}] ON {scope} STATE = {state};",
            session.replace(']', "]]")
        )
    }
}

impl EventEmitter<ToolDialogEvent> for ExtendedEventsPane {}

pub struct ExtendedEventsPane {
    context: ToolDialogContext,
    scope: XeScope,
    sessions_grid: Option<Entity<SqllyDataTable>>,
    session_names: Vec<String>,
    detail_grid: Option<Entity<SqllyDataTable>>,
    detail_session: Option<String>,
    error: Option<String>,
    loading: bool,
    generation: u64,
    focus_handle: FocusHandle,
    frame: DialogFrame,
}

impl ExtendedEventsPane {
    pub fn new(
        context: ToolDialogContext,
        tool_scope: ToolScope,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        // A database node lists that database's own Extended Events sessions
        // (`sys.database_event_sessions`); the server node lists the instance's
        // server-scoped sessions.
        let scope = match tool_scope {
            ToolScope::Database => XeScope::Database,
            ToolScope::Server => XeScope::Server,
        };
        let mut this = Self {
            context,
            scope,
            sessions_grid: None,
            session_names: Vec::new(),
            detail_grid: None,
            detail_session: None,
            error: None,
            loading: true,
            generation: 0,
            focus_handle: cx.focus_handle(),
            // Fixed frame: the embedded data grids manage their own scrolling.
            frame: DialogFrame::new(Theme::scaled(860.0), Theme::scaled(540.0)),
        };
        this.load_sessions(true, cx);
        this
    }

    /// Load sessions in the current scope; `allow_fallback` retries in
    /// database scope when the server-scope catalog does not exist (Azure).
    fn load_sessions(&mut self, allow_fallback: bool, cx: &mut gpui::Context<Self>) {
        self.loading = true;
        self.generation += 1;
        let generation = self.generation;
        let scope = self.scope;
        crate::tool_dialog::run_query(
            &self.context,
            scope.sessions_query().to_string(),
            cx,
            move |this, outcome, cx| {
                if this.generation != generation {
                    return;
                }
                if let Some(err) = outcome.error {
                    if allow_fallback && scope == XeScope::Server {
                        this.scope = XeScope::Database;
                        this.load_sessions(false, cx);
                    } else {
                        this.error = Some(err);
                        this.loading = false;
                    }
                } else {
                    this.loading = false;
                    this.error = None;
                    let set = outcome.sets.into_iter().next().unwrap_or_default();
                    this.session_names = set
                        .rows
                        .iter()
                        .filter_map(|r| match r.first() {
                            Some(CellValue::String(s)) => Some(s.clone()),
                            _ => None,
                        })
                        .collect();
                    this.sessions_grid = crate::tool_dialog::build_result_grid(&set, cx);
                    this.detail_grid = None;
                    this.detail_session = None;
                }
                cx.notify();
            },
        );
        cx.notify();
    }

    /// Session name behind the currently selected sessions-grid row.
    fn selected_session(&self, cx: &gpui::App) -> Option<String> {
        let grid = self.sessions_grid.as_ref()?;
        let row = crate::tool_dialog::selected_source_row(grid, cx)?;
        self.session_names.get(row).cloned()
    }

    fn show_detail(&mut self, cx: &mut gpui::Context<Self>) {
        let Some(session) = self.selected_session(cx) else {
            cx.emit(ToolDialogEvent::Status {
                level: StatusLevel::Info,
                message: crate::localization::text("explorer.xevents.select_session"),
            });
            return;
        };
        self.generation += 1;
        let generation = self.generation;
        let scope = self.scope;
        let session_for_done = session.clone();
        crate::tool_dialog::run_query(
            &self.context,
            scope.detail_query(&session),
            cx,
            move |this, outcome, cx| {
                if this.generation != generation {
                    return;
                }
                if let Some(err) = outcome.error {
                    this.error = Some(err);
                } else {
                    let set = outcome.sets.into_iter().next().unwrap_or_default();
                    this.detail_grid = crate::tool_dialog::build_result_grid(&set, cx);
                    this.detail_session = Some(session_for_done.clone());
                }
                cx.notify();
            },
        );
        cx.notify();
    }

    fn script_state_change(&mut self, start: bool, cx: &mut gpui::Context<Self>) {
        let Some(session) = self.selected_session(cx) else {
            cx.emit(ToolDialogEvent::Status {
                level: StatusLevel::Info,
                message: crate::localization::text("explorer.xevents.select_session"),
            });
            return;
        };
        let sql = self.scope.state_change_sql(&session, start);
        cx.emit(ToolDialogEvent::ScriptToEditor { sql });
        cx.emit(ToolDialogEvent::Close);
    }

    fn close(&mut self, cx: &mut gpui::Context<Self>) {
        cx.emit(ToolDialogEvent::Close);
    }
}

impl gpui::Focusable for ExtendedEventsPane {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ExtendedEventsPane {
    fn render(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;

        let sessions: gpui::AnyElement = if let Some(err) = &self.error {
            div()
                .p(px(s(12.0)))
                .text_color(Theme::error())
                .text_size(px(Theme::font_size_ratio(0.87)))
                .whitespace_normal()
                .child(crate::localization::format(
                    "explorer.xevents.load_failed",
                    &[("error", err)],
                ))
                .into_any_element()
        } else if self.loading {
            div()
                .p(px(s(12.0)))
                .text_color(Theme::text_dim())
                .child(crate::localization::text("explorer.xevents.loading"))
                .into_any_element()
        } else if let Some(grid) = &self.sessions_grid {
            div().size_full().child(grid.clone()).into_any_element()
        } else {
            div()
                .p(px(s(12.0)))
                .text_color(Theme::text_dim())
                .child(crate::localization::text("explorer.xevents.no_sessions"))
                .into_any_element()
        };

        let detail: gpui::AnyElement = if let Some(grid) = &self.detail_grid {
            div()
                .flex()
                .flex_col()
                .size_full()
                .gap(px(s(4.0)))
                .child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.80)))
                        .text_color(Theme::text_dim())
                        .child(crate::localization::format(
                            "explorer.xevents.detail_title",
                            &[("session", &self.detail_session.clone().unwrap_or_default())],
                        )),
                )
                .child(div().flex_1().min_h(px(0.0)).child(grid.clone()))
                .into_any_element()
        } else {
            div()
                .p(px(s(12.0)))
                .text_color(Theme::text_dim())
                .text_size(px(Theme::font_size_ratio(0.87)))
                .child(crate::localization::text("explorer.xevents.detail_prompt"))
                .into_any_element()
        };

        let focus_handle = self.focus_handle.clone();
        Dialog::new("xevents-pane", |this: &mut Self| &mut this.frame)
            .title(crate::localization::format(
                "explorer.xevents.title",
                &[
                    ("connection", &self.context.profile.name),
                    ("scope", &self.scope.label()),
                ],
            ))
            .easy_close(true)
            .key_context("ExtendedEventsPane")
            .track_focus(&focus_handle)
            .on_close(|this, _window, cx| this.close(cx))
            // bare_body: the embedded data grids must not sit in a scroll
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
                            .flex()
                            .flex_row()
                            .flex_1()
                            .min_h(px(0.0))
                            .gap(px(s(8.0)))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .border_1()
                                    .border_color(Theme::border())
                                    .rounded(px(s(4.0)))
                                    .child(sessions),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .border_1()
                                    .border_color(Theme::border())
                                    .rounded(px(s(4.0)))
                                    .child(detail),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.74)))
                            .text_color(Theme::text_dim())
                            .child(crate::localization::text(
                                "explorer.xevents.review_explanation",
                            )),
                    ),
            )
            .leading_button(
                Button::new("xe-refresh")
                    .label(crate::localization::text("explorer.xevents.refresh_button"))
                    .disabled(self.loading)
                    .on_click(cx.listener(|this, _, _window, cx| this.load_sessions(true, cx))),
            )
            .button(
                Button::new("xe-start")
                    .label(crate::localization::text("explorer.xevents.script_start"))
                    .disabled(self.loading)
                    .on_click(
                        cx.listener(|this, _, _window, cx| this.script_state_change(true, cx)),
                    ),
            )
            .button(
                Button::new("xe-stop")
                    .label(crate::localization::text("explorer.xevents.script_stop"))
                    .disabled(self.loading)
                    .on_click(
                        cx.listener(|this, _, _window, cx| this.script_state_change(false, cx)),
                    ),
            )
            .button(
                Button::new("xe-detail")
                    .primary()
                    .label(crate::localization::text(
                        "explorer.xevents.show_detail_button",
                    ))
                    .disabled(self.loading)
                    .on_click(cx.listener(|this, _, _window, cx| this.show_detail(cx))),
            )
            .build(self, window, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_and_database_scopes_query_their_catalogs() {
        assert!(
            XeScope::Server
                .sessions_query()
                .contains("sys.server_event_sessions")
        );
        assert!(
            XeScope::Database
                .sessions_query()
                .contains("sys.database_event_sessions")
        );
    }

    #[test]
    fn detail_query_escapes_session_name() {
        let sql = XeScope::Server.detail_query("o'brien");
        assert!(sql.contains("N'o''brien'"));
    }

    #[test]
    fn state_change_sql_matches_scope() {
        assert_eq!(
            XeScope::Server.state_change_sql("system_health", true),
            "ALTER EVENT SESSION [system_health] ON SERVER STATE = START;"
        );
        assert_eq!(
            XeScope::Database.state_change_sql("s]x", false),
            "ALTER EVENT SESSION [s]]x] ON DATABASE STATE = STOP;"
        );
    }
}
