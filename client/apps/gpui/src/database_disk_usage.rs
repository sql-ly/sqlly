//! Disk Usage pane — per-database sizes for the connected server, plus the
//! current database's file layout where the engine has one (SQL Server).
//!
//! Read-only observer: one refresh in flight, a generation token against
//! stale responses, and nothing here ever writes to the server.

use gpui::{
    Entity, EventEmitter, FocusHandle, IntoElement, ParentElement, Render, Styled, Window, div,
    prelude::*, px,
};
use gpui_component::Disableable as _;
use gpui_component::button::Button;
use sqlly_datatable::SqllyDataTable;
use sqlly_services::{DiskDialect, disk_dialect};

use crate::dialog::{Dialog, DialogFrame};
use crate::theme::Theme;
use crate::tool_dialog::{ToolDialogContext, ToolDialogEvent, ToolScope};

/// Which view the pane is showing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DiskTab {
    Databases,
    Files,
}

impl DiskTab {
    fn label(self) -> String {
        crate::localization::text(match self {
            DiskTab::Databases => "explorer.database_disk_usage.databases_tab",
            DiskTab::Files => "explorer.database_disk_usage.files_tab",
        })
    }
}

impl EventEmitter<ToolDialogEvent> for DiskUsagePane {}

pub struct DiskUsagePane {
    context: ToolDialogContext,
    /// Server scope lists every database; database scope shows only the
    /// right-clicked database's file layout.
    scope: ToolScope,
    tab: DiskTab,
    grid: Option<Entity<SqllyDataTable>>,
    row_count: usize,
    error: Option<String>,
    loading: bool,
    refresh_generation: u64,
    refresh_in_flight: bool,
    focus_handle: FocusHandle,
    frame: DialogFrame,
    /// True once popped out into its own OS window: the window boundary is
    /// the dialog boundary and close tears the window down.
    windowed: bool,
}

impl DiskUsagePane {
    pub fn new(context: ToolDialogContext, scope: ToolScope, cx: &mut gpui::Context<Self>) -> Self {
        // A database-scoped pane opens on that database's files; a server-scoped
        // pane opens on the all-databases overview.
        let initial_tab = match scope {
            ToolScope::Database
                if context
                    .services
                    .admin_tools
                    .disk_files_query_for(context.profile.database_type)
                    .is_some() =>
            {
                DiskTab::Files
            }
            _ => DiskTab::Databases,
        };
        let mut this = Self {
            context,
            scope,
            tab: initial_tab,
            grid: None,
            row_count: 0,
            error: None,
            loading: true,
            refresh_generation: 0,
            refresh_in_flight: false,
            focus_handle: cx.focus_handle(),
            // Fixed frame: the embedded data grid manages its own scrolling.
            frame: DialogFrame::new(Theme::scaled(860.0), Theme::scaled(500.0)),
            windowed: false,
        };
        this.refresh(cx);
        this
    }

    fn dialect(&self) -> Option<DiskDialect> {
        disk_dialect(self.context.profile.database_type)
    }

    /// Whether the Files tab exists for this engine at all.
    fn has_files_tab(&self) -> bool {
        self.context
            .services
            .admin_tools
            .disk_files_query_for(self.context.profile.database_type)
            .is_some()
    }

    fn switch_tab(&mut self, tab: DiskTab, cx: &mut gpui::Context<Self>) {
        if self.tab != tab {
            self.tab = tab;
            self.grid = None;
            self.row_count = 0;
            self.error = None;
            self.loading = true;
            self.refresh_generation += 1;
            self.refresh_in_flight = false;
            self.refresh(cx);
        }
    }

    fn refresh(&mut self, cx: &mut gpui::Context<Self>) {
        if self.refresh_in_flight {
            return;
        }
        let db = self.context.profile.database_type;
        let query = match self.tab {
            DiskTab::Databases => self
                .context
                .services
                .admin_tools
                .disk_databases_query_for(db),
            DiskTab::Files => self.context.services.admin_tools.disk_files_query_for(db),
        };
        let Some(query) = query else {
            self.loading = false;
            if self.dialect().is_none() {
                self.error = Some(crate::localization::format(
                    "explorer.database_disk_usage.engine_unavailable",
                    &[("engine_name", db.display_name())],
                ));
            } else {
                self.error = Some(crate::localization::text(
                    "explorer.database_disk_usage.file_layout_unavailable",
                ));
            }
            cx.notify();
            return;
        };
        self.refresh_in_flight = true;
        self.refresh_generation += 1;
        let generation = self.refresh_generation;
        crate::tool_dialog::run_query(
            &self.context,
            query.to_string(),
            cx,
            move |this, outcome, cx| {
                if this.refresh_generation != generation {
                    return;
                }
                this.refresh_in_flight = false;
                this.loading = false;
                if let Some(err) = outcome.error {
                    this.error = Some(err);
                    this.grid = None;
                    this.row_count = 0;
                } else {
                    this.error = None;
                    let set = outcome.sets.into_iter().next().unwrap_or_default();
                    this.row_count = set.rows.len();
                    this.grid = crate::tool_dialog::build_result_grid(&set, cx);
                }
                cx.notify();
            },
        );
        cx.notify();
    }

    fn close(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        if self.windowed {
            window.remove_window();
        } else {
            cx.emit(ToolDialogEvent::Close);
        }
    }

    /// Pop out into a standalone OS window; the inline pane closes and this
    /// same pane — grid, tab, and in-flight refresh included — carries on in
    /// the window.
    fn pop_out(&mut self, cx: &mut gpui::Context<Self>) {
        let entity = cx.entity();
        let connection = self.context.profile.name.clone();
        let title = move || {
            crate::localization::format(
                "explorer.database_disk_usage.title",
                &[("connection", &connection)],
            )
        };
        // Opening the window renders this pane into it immediately, which
        // must not happen inside its own update — defer past it.
        cx.defer(move |cx| {
            let build_entity = entity.clone();
            let opened = crate::dialog::open_detached(
                crate::dialog::DetachedOptions::localized_with(
                    title,
                    Theme::scaled(860.0),
                    Theme::scaled(560.0),
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

impl gpui::Focusable for DiskUsagePane {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for DiskUsagePane {
    fn render(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let active = self.tab;
        let tabs: Vec<DiskTab> = match (self.scope, self.has_files_tab()) {
            // Database scope: only that database's files (the all-databases
            // overview belongs to the server-scoped tool). Fall back to the
            // overview for engines that expose no per-file layout.
            (ToolScope::Database, true) => vec![DiskTab::Files],
            (ToolScope::Database, false) => vec![DiskTab::Databases],
            (ToolScope::Server, true) => vec![DiskTab::Databases, DiskTab::Files],
            (ToolScope::Server, false) => vec![DiskTab::Databases],
        };

        let tab_row = div()
            .flex()
            .flex_row()
            .gap(px(s(4.0)))
            .children(tabs.into_iter().map(|tab| {
                let is_active = tab == active;
                div()
                    .px(px(s(10.0)))
                    .py(px(s(4.0)))
                    .rounded(px(s(4.0)))
                    .cursor_pointer()
                    .bg(if is_active {
                        Theme::selection()
                    } else {
                        gpui::transparent_black()
                    })
                    .text_color(if is_active {
                        Theme::text()
                    } else {
                        Theme::text_dim()
                    })
                    .child(tab.label())
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(move |this, _, _, cx| this.switch_tab(tab, cx)),
                    )
            }));

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
                .child(crate::localization::text(
                    "explorer.database_disk_usage.loading",
                ))
                .into_any_element()
        } else {
            div()
                .p(px(s(12.0)))
                .text_color(Theme::text_dim())
                .child(crate::localization::text(
                    "explorer.database_disk_usage.empty",
                ))
                .into_any_element()
        };

        let focus_handle = self.focus_handle.clone();
        let mut dialog = Dialog::new("disk-usage-pane", |this: &mut Self| &mut this.frame)
            .title(crate::localization::format(
                "explorer.database_disk_usage.title",
                &[("connection", &self.context.profile.name)],
            ))
            .easy_close(true)
            .key_context("DiskUsagePane")
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
                    .child(tab_row)
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
                                "explorer.database_disk_usage.footer",
                                &[("count", &self.row_count.to_string())],
                            )),
                    ),
            )
            .leading_button(
                Button::new("disk-usage-refresh")
                    .label(crate::localization::text(
                        "explorer.database_disk_usage.refresh_button",
                    ))
                    .disabled(self.loading)
                    .on_click(cx.listener(|this, _, _window, cx| this.refresh(cx))),
            );
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
    fn dialects_cover_native_engines_only() {
        assert_eq!(
            disk_dialect(sqlly_core::DatabaseType::SqlServer),
            Some(DiskDialect::SqlServer)
        );
        assert_eq!(
            disk_dialect(sqlly_core::DatabaseType::Postgres),
            Some(DiskDialect::Postgres)
        );
        assert_eq!(
            disk_dialect(sqlly_core::DatabaseType::MariaDb),
            Some(DiskDialect::MySql)
        );
        assert_eq!(
            disk_dialect(sqlly_core::DatabaseType::Sqlite),
            Some(DiskDialect::Sqlite)
        );
        assert_eq!(
            disk_dialect(sqlly_core::DatabaseType::DuckDb),
            Some(DiskDialect::DuckDb)
        );
        assert_eq!(
            disk_dialect(sqlly_core::DatabaseType::ClickHouse),
            Some(DiskDialect::ClickHouse)
        );
        assert_eq!(
            disk_dialect(sqlly_core::DatabaseType::Oracle),
            Some(DiskDialect::Oracle)
        );
        assert_eq!(disk_dialect(sqlly_core::DatabaseType::Db2), None);
    }

    #[test]
    fn files_tab_only_exists_where_the_engine_has_files() {
        let admin = DefaultAdminToolsService;
        assert!(
            admin
                .disk_files_query_for(sqlly_core::DatabaseType::SqlServer)
                .is_some()
        );
        assert!(
            admin
                .disk_files_query_for(sqlly_core::DatabaseType::Postgres)
                .is_none()
        );
    }

    /// The pane's grid, drawn in the real pane, gets room for its rows (a
    /// grid in an auto-height wrapper paints zero pixels tall).
    #[gpui::test]
    fn disk_usage_grid_gets_room_to_paint_its_rows(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            if cx.try_global::<gpui_component::Theme>().is_none() {
                gpui_component::init(cx);
            }
        });
        let context =
            crate::tool_dialog::unreachable_tool_context(sqlly_core::DatabaseType::SqlServer);
        let (view, cx) = cx.add_window_view(|_window, cx| {
            DiskUsagePane::new(context, crate::tool_dialog::ToolScope::Server, cx)
        });
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
            crate::tool_dialog::assert_grid_shows_rows(grid, 3, "Disk Usage", cx);
        });
    }
}
