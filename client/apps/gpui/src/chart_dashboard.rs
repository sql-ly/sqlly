//! Chart Dashboard — every saved chart as a live tile in a responsive grid.
//!
//! Each tile owns one chart state + canvas from the datatable's chart engine
//! (the same renderer the results Chart tab uses, minus its tab bar and
//! configuration sidebar) fed by the tile's own query run. Runs go through
//! the route-aware services for the chart's saved connection, are capped at
//! the chart engine's source-row limit, and are restricted to read-only
//! SELECT text — a saved chart can never modify data from here. Tiles whose
//! connection profile no longer exists say so instead of failing silently.

#![cfg(not(target_arch = "wasm32"))]

use std::sync::Arc;
use std::time::Instant;

use gpui::{
    Entity, EventEmitter, FocusHandle, IntoElement, ParentElement, Render, Styled, Window, div,
    prelude::*, px,
};
use gpui_component::Disableable as _;
use gpui_component::Sizable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use sqlly_core::ConnectionProfile;
use sqlly_datatable::{ChartCanvas, ChartState, MAX_SOURCE_ROWS};

use crate::dialog::{Dialog, DialogFrame};
use crate::saved_charts::{
    SavedChart, SavedChartStore, dashboard_columns, refresh_label, relative_age, tile_refresh_due,
    tile_rows,
};
use crate::service_router::ServiceRouter;
use crate::theme::Theme;
use crate::tool_dialog::{ToolDialogContext, ToolDialogEvent, ToolQueryOutcome};

/// Height of one dashboard tile (logical pixels before UI scaling).
const TILE_HEIGHT: f32 = 280.0;

/// Message shown on a tile whose saved connection profile was deleted.
pub fn missing_connection_message() -> String {
    crate::localization::text("results.chart_dashboard.missing_connection")
}

/// Message shown on a tile whose SQL is not a read-only SELECT.
pub fn not_select_message() -> String {
    crate::localization::text("results.chart_dashboard.not_select")
}

struct Tile {
    chart: SavedChart,
    /// The saved connection profile, retargeted to the saved database; `None`
    /// when the profile was deleted since the chart was saved.
    profile: Option<ConnectionProfile>,
    select_only: bool,
    state: Option<Entity<ChartState>>,
    canvas: Option<Entity<ChartCanvas>>,
    error: Option<String>,
    loading: bool,
    /// Freshness token: a response from an older run is dropped.
    generation: u64,
    last_completed: Option<Instant>,
    last_completed_unix_ms: Option<i64>,
    row_count: usize,
}

impl Tile {
    fn secs_since_completion(&self) -> Option<u64> {
        self.last_completed.map(|at| at.elapsed().as_secs())
    }
}

/// Resolve a saved chart's profile: the stored connection id looked up in the
/// live connection store, with the saved database applied.
pub fn resolve_profile(chart: &SavedChart) -> Option<ConnectionProfile> {
    let id = uuid::Uuid::parse_str(chart.connection_id.as_deref()?).ok()?;
    let mut profile = crate::connection::ConnectionStore::global().get(id)?;
    if let Some(database) = chart.database.clone().filter(|db| !db.is_empty()) {
        profile.database = Some(database);
    }
    Some(profile)
}

pub struct ChartDashboard {
    store: SavedChartStore,
    router: ServiceRouter,
    tiles: Vec<Tile>,
    focus_handle: FocusHandle,
    frame: DialogFrame,
    windowed: bool,
    _ticker: gpui::Task<()>,
}

impl EventEmitter<ToolDialogEvent> for ChartDashboard {}

impl ChartDashboard {
    pub fn new(
        store: SavedChartStore,
        router: ServiceRouter,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        let ticker = {
            let weak = cx.weak_entity();
            let async_cx = cx.to_async();
            cx.spawn(async move |_this, _cx| {
                loop {
                    async_cx
                        .background_executor()
                        .timer(std::time::Duration::from_secs(1))
                        .await;
                    let mut cx = async_cx.clone();
                    if weak
                        .update(&mut cx, |this: &mut Self, cx| this.tick(cx))
                        .is_err()
                    {
                        break;
                    }
                }
            })
        };
        let mut this = Self {
            store,
            router,
            tiles: Vec::new(),
            focus_handle: cx.focus_handle(),
            frame: DialogFrame::new(Theme::scaled(1180.0), Theme::scaled(720.0)),
            windowed: false,
            _ticker: ticker,
        };
        this.reload(cx);
        this
    }

    /// Re-read the library and refresh every tile. Tiles for charts that are
    /// still present keep their rendered chart until the new run lands.
    pub fn reload(&mut self, cx: &mut gpui::Context<Self>) {
        let charts = self.store.list();
        let mut old: Vec<Tile> = std::mem::take(&mut self.tiles);
        self.tiles = charts
            .into_iter()
            .map(|chart| {
                let existing = old
                    .iter()
                    .position(|tile| tile.chart.id == chart.id)
                    .map(|index| old.remove(index));
                let profile = resolve_profile(&chart);
                let select_only = match profile.as_ref() {
                    Some(p) => sqlly_intel::query::safety::classify_select_only_for(
                        p.database_type,
                        &chart.sql,
                    ),
                    None => sqlly_intel::query::safety::classify_select_only(&chart.sql),
                }
                .select_only;
                match existing {
                    Some(mut tile) => {
                        tile.chart = chart;
                        tile.profile = profile;
                        tile.select_only = select_only;
                        tile
                    }
                    None => Tile {
                        chart,
                        profile,
                        select_only,
                        state: None,
                        canvas: None,
                        error: None,
                        loading: false,
                        generation: 0,
                        last_completed: None,
                        last_completed_unix_ms: None,
                        row_count: 0,
                    },
                }
            })
            .collect();
        self.refresh_all(cx);
    }

    pub fn refresh_all(&mut self, cx: &mut gpui::Context<Self>) {
        for index in 0..self.tiles.len() {
            self.refresh_tile(index, cx);
        }
        cx.notify();
    }

    /// The once-a-second tick: start a run on every tile whose interval has
    /// elapsed, and repaint so "last refreshed" stamps stay live.
    fn tick(&mut self, cx: &mut gpui::Context<Self>) {
        let due: Vec<usize> = self
            .tiles
            .iter()
            .enumerate()
            .filter(|(_, tile)| {
                tile.profile.is_some()
                    && tile_refresh_due(
                        tile.chart.refresh_secs,
                        tile.loading,
                        tile.select_only,
                        tile.secs_since_completion(),
                    )
            })
            .map(|(index, _)| index)
            .collect();
        for index in due {
            self.refresh_tile(index, cx);
        }
        cx.notify();
    }

    fn refresh_tile(&mut self, index: usize, cx: &mut gpui::Context<Self>) {
        let Some(tile) = self.tiles.get_mut(index) else {
            return;
        };
        if tile.loading {
            return;
        }
        let Some(profile) = tile.profile.clone() else {
            tile.error = Some(missing_connection_message());
            return;
        };
        if !tile.select_only {
            tile.error = Some(not_select_message());
            return;
        }
        tile.loading = true;
        tile.generation += 1;
        let generation = tile.generation;
        let chart_id = tile.chart.id.clone();
        let sql = tile.chart.sql.clone();
        let context = ToolDialogContext::new(profile.clone(), self.router.services_for(&profile));
        crate::tool_dialog::run_query_with_cap(
            &context,
            sql,
            MAX_SOURCE_ROWS,
            cx,
            move |this, outcome, cx| this.apply_outcome(&chart_id, generation, outcome, cx),
        );
    }

    fn apply_outcome(
        &mut self,
        chart_id: &str,
        generation: u64,
        outcome: ToolQueryOutcome,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some(index) = self.tiles.iter().position(|tile| tile.chart.id == chart_id) else {
            return;
        };
        let now_ms = chrono::Utc::now().timestamp_millis();
        let tile = &mut self.tiles[index];
        if tile.generation != generation {
            return;
        }
        tile.loading = false;
        tile.last_completed = Some(Instant::now());
        tile.last_completed_unix_ms = Some(now_ms);
        if let Some(error) = outcome.error {
            tile.error = Some(error);
            cx.notify();
            return;
        }
        let spec = tile.chart.spec();
        let set_index = spec.as_ref().map(|spec| spec.result_set_index).unwrap_or(0);
        let mut sets = outcome.sets;
        let chosen = if sets
            .get(set_index)
            .is_some_and(|set| !set.columns.is_empty())
        {
            set_index
        } else {
            0
        };
        let set = if sets.get(chosen).is_some_and(|set| !set.columns.is_empty()) {
            sets.swap_remove(chosen)
        } else {
            tile.error = Some(crate::localization::text(
                "results.chart_dashboard.no_result_set",
            ));
            cx.notify();
            return;
        };
        let column_count = set.columns.len();
        let columns: Vec<sqlly_datatable::Column> = set
            .columns
            .iter()
            .map(|column| crate::grid_bridge::convert_column(None, column))
            .collect();
        let rows: Vec<Vec<sqlly_datatable::CellValue>> = set
            .rows
            .into_iter()
            .map(|row| {
                let mut cells: Vec<sqlly_datatable::CellValue> = row
                    .into_iter()
                    .take(column_count)
                    .map(crate::grid_bridge::convert_cell_owned)
                    .collect();
                cells.resize(column_count, sqlly_datatable::CellValue::None);
                cells
            })
            .collect();
        let config = spec
            .and_then(|spec| spec.chart_config(&set.columns))
            .unwrap_or_default();
        tile.row_count = rows.len();
        tile.error = None;
        let rows = Arc::new(rows);
        match &tile.state {
            Some(state) => state.update(cx, |state, cx| {
                state.set_source(columns, rows);
                state.set_config(config);
                cx.notify();
            }),
            None => {
                let theme = crate::grid_bridge::build_grid_theme();
                let focus = cx.focus_handle();
                let font_size = Theme::font_size();
                let state = cx.new(|_| {
                    ChartState::new(columns, rows, config, theme, false, font_size, focus)
                });
                let canvas = cx.new(|_| ChartCanvas::new(state.clone()));
                tile.state = Some(state);
                tile.canvas = Some(canvas);
            }
        }
        let _ = self.store.touch_last_run(chart_id, now_ms);
        cx.notify();
    }

    fn close(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        if self.windowed {
            window.remove_window();
        } else {
            cx.emit(ToolDialogEvent::Close);
        }
    }

    /// Pop out into a standalone OS window; the inline card closes and this
    /// same dashboard — tiles, timers, and in-flight runs included — carries
    /// on in the window.
    fn pop_out(&mut self, cx: &mut gpui::Context<Self>) {
        let entity = cx.entity();
        cx.defer(move |cx| {
            let build_entity = entity.clone();
            let opened = crate::dialog::open_detached(
                crate::dialog::DetachedOptions::localized(
                    "results.chart_dashboard.window_title",
                    Theme::scaled(1180.0),
                    Theme::scaled(760.0),
                ),
                None,
                cx,
                move |_, cx| {
                    build_entity.update(cx, |pane, _| pane.windowed = true);
                    build_entity
                },
            );
            if opened.is_some() {
                entity.update(cx, |_, cx| cx.emit(ToolDialogEvent::Close));
            }
        });
    }

    fn render_tile(&self, index: usize, cx: &mut gpui::Context<Self>) -> gpui::AnyElement {
        let s = Theme::scaled;
        let tile = &self.tiles[index];
        let now_ms = chrono::Utc::now().timestamp_millis();
        let stamp = match (tile.loading, tile.last_completed_unix_ms) {
            (true, _) => crate::localization::text("results.chart_dashboard.tile_refreshing"),
            (false, Some(ms)) => crate::localization::format(
                "results.chart_dashboard.tile_refreshed",
                &[("relative_age", &relative_age(ms, now_ms))],
            ),
            (false, None) => {
                crate::localization::text("results.chart_dashboard.tile_not_refreshed")
            }
        };
        let interval = refresh_label(tile.chart.refresh_secs);
        let can_refresh = tile.profile.is_some() && tile.select_only && !tile.loading;

        let body: gpui::AnyElement = if let Some(error) = &tile.error {
            div()
                .size_full()
                .p(px(s(12.0)))
                .text_size(px(Theme::font_size_ratio(0.84)))
                .text_color(Theme::error())
                .whitespace_normal()
                .child(error.clone())
                .into_any_element()
        } else if let Some(canvas) = &tile.canvas {
            div().size_full().child(canvas.clone()).into_any_element()
        } else {
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(Theme::text_dim())
                .child(crate::localization::text(if tile.loading {
                    "results.chart_dashboard.tile_loading"
                } else {
                    "results.chart_dashboard.tile_no_data"
                }))
                .into_any_element()
        };

        div()
            .id(("chart-tile", index))
            .flex_1()
            .min_w(px(0.0))
            .h(px(s(TILE_HEIGHT)))
            .flex()
            .flex_col()
            .border_1()
            .border_color(Theme::border())
            .rounded(px(s(6.0)))
            .bg(Theme::panel_bg())
            .overflow_hidden()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(8.0)))
                    .px(px(s(10.0)))
                    .py(px(s(6.0)))
                    .border_b_1()
                    .border_color(Theme::border())
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .truncate()
                                    .text_color(Theme::text())
                                    .child(tile.chart.name.clone()),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(Theme::font_size_ratio(0.76)))
                                    .text_color(Theme::text_dim())
                                    .child(format!(
                                        "{} · {stamp} · {interval}",
                                        tile.chart.location_label()
                                    )),
                            ),
                    )
                    .child(
                        Button::new(("chart-tile-refresh", index))
                            .ghost()
                            .xsmall()
                            .label(crate::localization::text(
                                "results.chart_dashboard.tile_refresh_button",
                            ))
                            .disabled(!can_refresh)
                            .on_click(cx.listener(move |this, _ev, _w, cx| {
                                this.refresh_tile(index, cx);
                                cx.notify();
                            })),
                    ),
            )
            .child(div().flex_1().min_h(px(0.0)).child(body))
            .into_any_element()
    }
}

impl gpui::Focusable for ChartDashboard {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ChartDashboard {
    fn render(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        // Body width drives the column count: the OS window when popped out,
        // the dialog card otherwise (minus its padding).
        let body_width = if self.windowed {
            f32::from(window.viewport_size().width)
        } else {
            self.frame.width()
        } - s(28.0);
        let columns = dashboard_columns(body_width / Theme::scaled(1.0));
        let any_loading = self.tiles.iter().any(|tile| tile.loading);
        let tile_count = self.tiles.len();

        let mut grid = div()
            .id("chart-dashboard-grid")
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(s(10.0)));
        if self.tiles.is_empty() {
            grid = grid.child(
                div()
                    .p(px(s(12.0)))
                    .text_color(Theme::text_dim())
                    .whitespace_normal()
                    .child(crate::localization::text(
                        "results.chart_dashboard.empty_state",
                    )),
            );
        }
        for row in tile_rows(tile_count, columns) {
            let mut row_el = div().flex().flex_row().gap(px(s(10.0))).flex_shrink_0();
            let filled = row.len();
            for index in row {
                row_el = row_el.child(self.render_tile(index, cx));
            }
            // Pad a short last row so its tiles keep the same width as the
            // full rows above.
            for pad in filled..columns {
                row_el = row_el.child(div().id(("chart-tile-pad", pad)).flex_1().min_w(px(0.0)));
            }
            grid = grid.child(row_el);
        }

        let focus_handle = self.focus_handle.clone();
        let mut dialog = Dialog::new("chart-dashboard", |this: &mut Self| &mut this.frame)
            .title(crate::localization::text("results.chart_dashboard.title"))
            .easy_close(true)
            .key_context("ChartDashboard")
            .track_focus(&focus_handle)
            .windowed(self.windowed)
            .on_close(|this, window, cx| this.close(window, cx))
            // bare_body: the tile grid scrolls itself and each tile hosts a
            // canvas that must not sit in the chrome's scroll container.
            .bare_body()
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .flex()
                    .flex_col()
                    .gap(px(s(8.0)))
                    .p(px(s(14.0)))
                    .child(grid)
                    .child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.74)))
                            .text_color(Theme::text_dim())
                            .child(crate::localization::format(
                                "results.chart_dashboard.read_only_summary",
                                &[("chart_count", &tile_count.to_string())],
                            )),
                    ),
            )
            .leading_button(
                Button::new("chart-dashboard-refresh-all")
                    .label(crate::localization::text(
                        "results.chart_dashboard.refresh_all_button",
                    ))
                    .disabled(any_loading || self.tiles.is_empty())
                    .on_click(cx.listener(|this, _, _window, cx| this.refresh_all(cx))),
            )
            .leading_button(
                Button::new("chart-dashboard-reload")
                    .label(crate::localization::text(
                        "results.chart_dashboard.reload_library_button",
                    ))
                    .ghost()
                    .on_click(cx.listener(|this, _, _window, cx| this.reload(cx))),
            );
        if !self.windowed {
            dialog = dialog.detachable(|this, _window, cx| this.pop_out(cx));
        }
        dialog.build(self, window, cx)
    }
}
