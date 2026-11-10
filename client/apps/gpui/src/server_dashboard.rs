//! Server Dashboard — live health tiles over read-only catalog snapshots.
//!
//! Polls every 5 s (Activity Monitor's ticker + generation-token contract),
//! turns consecutive counter snapshots into per-second rates on the client,
//! and draws theme-token sparklines. Nothing here writes to the server.
//!
//! One pane per connection: dashboards are non-modal and several stay open
//! side by side, each polling its own server, so the app owns a list of them
//! rather than a single slot.
//!
//! A refresh runs every [`MetricGroup`](sqlly_services::MetricGroup) the
//! engine offers as its own query, in parallel, and folds the answers into one
//! snapshot. A group that errors — a DMV Azure does not expose, a catalog this
//! login cannot read — costs only its own tiles and is named in the header
//! instead of blanking the pane.

use std::collections::HashMap;
#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;
#[cfg(target_arch = "wasm32")]
use web_time::Instant;

use gpui::{
    AppContext as _, Entity, EventEmitter, FocusHandle, InteractiveElement as _, IntoElement,
    ParentElement, Render, SharedString, StatefulInteractiveElement as _, Styled, Task, Window,
    canvas, div, point, px,
};
use gpui_component::Disableable as _;
#[cfg(not(target_arch = "wasm32"))]
use gpui_component::Sizable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use sqlly_core::{CellValue, ResultColumn};
use sqlly_datatable::SqllyDataTable;
#[cfg(not(target_arch = "wasm32"))]
use sqlly_datatable::{ChartCanvas, ChartState, MAX_SOURCE_ROWS};
use sqlly_services::AdminToolsService;

use crate::dialog::{Dialog, DialogFrame};
#[cfg(not(target_arch = "wasm32"))]
use crate::saved_charts::{SavedChart, refresh_label, relative_age, tile_refresh_due};
use crate::theme::Theme;
#[cfg(not(target_arch = "wasm32"))]
use crate::tool_dialog::ToolQueryOutcome;
use crate::tool_dialog::{AdminToolLink, ToolDialogContext, ToolDialogEvent};

/// Seconds between automatic refreshes while the pane is open.
const AUTO_REFRESH_SECS: u64 = 5;

/// Floor for the database-sizes grid (unscaled points): a header plus
/// several rows stay visible however many tiles and widgets sit above it.
const SIZES_GRID_MIN_HEIGHT: f32 = 200.0;

/// Sparkline history length: five minutes at a 5 s interval.
pub const HISTORY_LEN: usize = 60;

/// One polled sample: counter values by key plus when it arrived.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub at: Instant,
    pub values: HashMap<String, f64>,
}

/// A tile's current reading and sparkline history (newest last, capped).
#[derive(Debug, Clone, PartialEq)]
pub struct Tile {
    pub key: &'static str,
    pub label: String,
    pub value: Option<f64>,
    pub unit: Unit,
    pub history: Vec<f64>,
    pub max: Option<f64>,
    pub detail: Option<String>,
    pub section: Section,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    Count,
    PerSecond,
    Percent,
    Seconds,
    Megabytes,
    Ratio,
}

/// Which band of the dashboard a tile belongs to. Reading top to bottom
/// answers "who is on the server", "how hard is it working", "is anything
/// wrong", "what is it consuming" — in that order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Sessions,
    Throughput,
    Health,
    Resources,
}

impl Section {
    pub fn title(self) -> String {
        crate::localization::text(match self {
            Self::Sessions => "explorer.server_dashboard.section_sessions",
            Self::Throughput => "explorer.server_dashboard.section_throughput",
            Self::Health => "explorer.server_dashboard.section_health",
            Self::Resources => "explorer.server_dashboard.section_resources",
        })
    }
}

/// Display order of the sections.
const SECTIONS: &[Section] = &[
    Section::Sessions,
    Section::Throughput,
    Section::Health,
    Section::Resources,
];

struct TileSpec {
    key: &'static str,
    label: &'static str,
    unit: Unit,
    source: &'static str,
    rate: bool,
    section: Section,
}

const fn tile(
    key: &'static str,
    label: &'static str,
    unit: Unit,
    rate: bool,
    section: Section,
) -> TileSpec {
    TileSpec {
        key,
        label,
        unit,
        source: key,
        rate,
        section,
    }
}

/// Every tile the dashboard can draw, in display order within its section. A
/// tile appears only when the engine reported its key, so this table is a
/// superset: SQL Server contributes page life expectancy, PostgreSQL the
/// idle-in-transaction count, MySQL slow queries, SQLite its file statistics.
const TILE_SPECS: &[TileSpec] = &[
    // Sessions — who is connected and what are they doing right now.
    tile(
        "connections_active",
        "explorer.server_dashboard.tile_connections",
        Unit::Ratio,
        false,
        Section::Sessions,
    ),
    tile(
        "sessions_running",
        "explorer.server_dashboard.tile_running",
        Unit::Count,
        false,
        Section::Sessions,
    ),
    tile(
        "sessions_blocked",
        "explorer.server_dashboard.tile_blocked",
        Unit::Count,
        false,
        Section::Sessions,
    ),
    tile(
        "sessions_idle_in_transaction",
        "explorer.server_dashboard.tile_idle_in_transaction",
        Unit::Count,
        false,
        Section::Sessions,
    ),
    tile(
        "longest_query_seconds",
        "explorer.server_dashboard.tile_longest_query",
        Unit::Seconds,
        false,
        Section::Sessions,
    ),
    tile(
        "oldest_transaction_seconds",
        "explorer.server_dashboard.tile_oldest_transaction",
        Unit::Seconds,
        false,
        Section::Sessions,
    ),
    // Throughput — cumulative counters the client turns into rates.
    tile(
        "transactions_total",
        "explorer.server_dashboard.tile_transactions_per_second",
        Unit::PerSecond,
        true,
        Section::Throughput,
    ),
    tile(
        "queries_total",
        "explorer.server_dashboard.tile_queries_per_second",
        Unit::PerSecond,
        true,
        Section::Throughput,
    ),
    tile(
        "rows_returned_total",
        "explorer.server_dashboard.tile_rows_read_per_second",
        Unit::PerSecond,
        true,
        Section::Throughput,
    ),
    tile(
        "slow_queries_total",
        "explorer.server_dashboard.tile_slow_queries_per_second",
        Unit::PerSecond,
        true,
        Section::Throughput,
    ),
    // Health — the counters you want at zero.
    tile(
        "deadlocks_total",
        "explorer.server_dashboard.tile_deadlocks_per_second",
        Unit::PerSecond,
        true,
        Section::Health,
    ),
    tile(
        "lock_waits_total",
        "explorer.server_dashboard.tile_lock_waits_per_second",
        Unit::PerSecond,
        true,
        Section::Health,
    ),
    tile(
        "temp_files_total",
        "explorer.server_dashboard.tile_temp_spills_per_second",
        Unit::PerSecond,
        true,
        Section::Health,
    ),
    tile(
        "replication_lag_seconds",
        "explorer.server_dashboard.tile_replication_lag",
        Unit::Seconds,
        false,
        Section::Health,
    ),
    // Resources — memory, disk, and how long this has been up.
    tile(
        "cache_hit_ratio",
        "explorer.server_dashboard.tile_cache_hit_percent",
        Unit::Percent,
        false,
        Section::Resources,
    ),
    tile(
        "page_life_expectancy",
        "explorer.server_dashboard.tile_page_life_expectancy",
        Unit::Seconds,
        false,
        Section::Resources,
    ),
    tile(
        "memory_used_mb",
        "explorer.server_dashboard.tile_memory_in_use",
        Unit::Megabytes,
        false,
        Section::Resources,
    ),
    tile(
        "memory_configured_mb",
        "explorer.server_dashboard.tile_shared_buffers",
        Unit::Megabytes,
        false,
        Section::Resources,
    ),
    tile(
        "storage_total_mb",
        "explorer.server_dashboard.tile_storage",
        Unit::Megabytes,
        false,
        Section::Resources,
    ),
    tile(
        "reclaimable_mb",
        "explorer.server_dashboard.tile_reclaimable",
        Unit::Megabytes,
        false,
        Section::Resources,
    ),
    tile(
        "objects_total",
        "explorer.server_dashboard.tile_tables_and_views",
        Unit::Count,
        false,
        Section::Resources,
    ),
    tile(
        "indexes_total",
        "explorer.server_dashboard.tile_indexes",
        Unit::Count,
        false,
        Section::Resources,
    ),
    tile(
        "uptime_seconds",
        "explorer.server_dashboard.tile_uptime",
        Unit::Seconds,
        false,
        Section::Resources,
    ),
];

impl EventEmitter<ToolDialogEvent> for ServerDashboardPane {}

/// One refresh in flight: metric groups answer independently and fold into
/// this before the tiles are rebuilt.
struct PendingRefresh {
    remaining: usize,
    values: HashMap<String, f64>,
    /// Names of the groups that errored, for the "partially unavailable" note.
    unavailable: Vec<String>,
    first_error: Option<String>,
    succeeded: usize,
}

/// A saved chart pinned to this dashboard. The tile machinery mirrors the
/// Chart Dashboard's: the chart's own query runs on this pane's captured
/// route, read-only SELECT only, on the chart's saved refresh interval.
/// Desktop-only, like the chart library itself.
#[cfg(not(target_arch = "wasm32"))]
struct WidgetTile {
    chart: SavedChart,
    select_only: bool,
    state: Option<Entity<ChartState>>,
    canvas: Option<Entity<ChartCanvas>>,
    error: Option<String>,
    loading: bool,
    /// Freshness token: a response from an older run is dropped.
    generation: u64,
    last_completed: Option<Instant>,
    last_completed_unix_ms: Option<i64>,
}

#[cfg(not(target_arch = "wasm32"))]
impl WidgetTile {
    fn new(chart: SavedChart, db: sqlly_core::DatabaseType) -> Self {
        let select_only = crate::saved_charts::widget_sql_is_select_only(&chart, db);
        Self {
            chart,
            select_only,
            state: None,
            canvas: None,
            error: None,
            loading: false,
            generation: 0,
            last_completed: None,
            last_completed_unix_ms: None,
        }
    }

    fn secs_since_completion(&self) -> Option<u64> {
        self.last_completed.map(|at| at.elapsed().as_secs())
    }
}

/// The profile a widget's query runs on: this dashboard's connection,
/// retargeted to the database the chart was saved against (the Chart
/// Dashboard's `resolve_profile` rule). A chart saved without a database runs
/// on the connection's own default.
#[cfg(not(target_arch = "wasm32"))]
fn widget_profile(
    base: &sqlly_core::ConnectionProfile,
    chart: &SavedChart,
) -> sqlly_core::ConnectionProfile {
    let mut profile = base.clone();
    if let Some(database) = chart.database.clone().filter(|db| !db.is_empty()) {
        profile.database = Some(database);
    }
    profile
}

/// Rebuild the widget tiles for a freshly read pin order. A tile whose chart
/// is still pinned keeps its rendered chart and run state; it is re-run only
/// when what it would run changed (SQL, database, chart settings). Returns
/// the rebuilt tiles and the indices that need a run: brand-new tiles and
/// edited ones — never tiles that merely moved.
#[cfg(not(target_arch = "wasm32"))]
fn rebuild_widget_tiles(
    mut old: Vec<WidgetTile>,
    ordered: Vec<SavedChart>,
    db: sqlly_core::DatabaseType,
) -> (Vec<WidgetTile>, Vec<usize>) {
    let mut to_run = Vec::new();
    let tiles = ordered
        .into_iter()
        .enumerate()
        .map(|(index, chart)| {
            match old
                .iter()
                .position(|tile| tile.chart.id == chart.id)
                .map(|at| old.remove(at))
            {
                Some(mut tile) => {
                    let changed = tile.chart.sql != chart.sql
                        || tile.chart.database != chart.database
                        || tile.chart.chart_directive != chart.chart_directive;
                    if changed {
                        tile.select_only =
                            crate::saved_charts::widget_sql_is_select_only(&chart, db);
                        to_run.push(index);
                    }
                    tile.chart = chart;
                    tile
                }
                None => {
                    to_run.push(index);
                    WidgetTile::new(chart, db)
                }
            }
        })
        .collect();
    (tiles, to_run)
}

pub struct ServerDashboardPane {
    context: ToolDialogContext,
    tiles: Vec<Tile>,
    prev: Option<Snapshot>,
    sizes_grid: Option<Entity<SqllyDataTable>>,
    error: Option<String>,
    /// Metric groups this connection cannot read (permission, edition, or a
    /// catalog the engine does not have).
    unavailable: Vec<String>,
    pending: Option<PendingRefresh>,
    loading: bool,
    refresh_generation: u64,
    refresh_in_flight: bool,
    last_refreshed: Option<Instant>,
    paused: bool,
    interval_secs: u64,
    _ticker: Task<()>,
    focus_handle: FocusHandle,
    frame: DialogFrame,
    /// True once popped out into its own OS window: the window boundary is
    /// the dialog boundary and close tears the window down.
    windowed: bool,
    /// Saved charts pinned to this connection's dashboard, in pin order.
    #[cfg(not(target_arch = "wasm32"))]
    widgets: Vec<WidgetTile>,
    /// The "Add widget" chooser is showing (the connection's pinnable charts).
    #[cfg(not(target_arch = "wasm32"))]
    add_widget_open: bool,
    /// Error from the pin store or chart library, named in the widgets strip
    /// so a corrupt file is not mistaken for "nothing pinned".
    #[cfg(not(target_arch = "wasm32"))]
    widget_error: Option<String>,
    #[cfg(not(target_arch = "wasm32"))]
    widgets_scroll: gpui::ScrollHandle,
}

fn permission_guidance(db: sqlly_core::DatabaseType) -> String {
    use sqlly_core::ConnectionKind;
    crate::localization::text(match db.connection_kind() {
        ConnectionKind::Tds => "explorer.server_dashboard.permission_sql_server",
        ConnectionKind::PostgresWire => "explorer.server_dashboard.permission_postgres",
        ConnectionKind::MySqlWire => "explorer.server_dashboard.permission_mysql",
        ConnectionKind::ClickHouseHttp => "explorer.server_dashboard.permission_clickhouse",
        ConnectionKind::Resp => "explorer.server_dashboard.permission_redis",
        _ => "explorer.server_dashboard.permission_unsupported",
    })
}

fn cell_f64(value: &CellValue) -> Option<f64> {
    match value {
        CellValue::I64(n) => Some(*n as f64),
        CellValue::F64(n) => n.is_finite().then_some(*n),
        CellValue::String(s) => s.trim().parse().ok(),
        CellValue::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        CellValue::Null | CellValue::Bytes(_) => None,
    }
}

fn column_index(columns: &[ResultColumn], name: &str) -> Option<usize> {
    columns
        .iter()
        .position(|c| c.name.eq_ignore_ascii_case(name))
}

/// Parse the two-column `metric,value` set into a Snapshot; non-numeric or NULL values are skipped.
pub fn snapshot_from_set(
    columns: &[ResultColumn],
    rows: &[Vec<CellValue>],
    at: Instant,
) -> Snapshot {
    let metric_idx = column_index(columns, "metric").unwrap_or(0);
    let value_idx = column_index(columns, "value").unwrap_or(1);
    let mut values = HashMap::new();
    for row in rows {
        let Some(CellValue::String(metric)) = row.get(metric_idx) else {
            continue;
        };
        let metric = metric.trim();
        if metric.is_empty() {
            continue;
        }
        let Some(value) = row.get(value_idx).and_then(cell_f64) else {
            continue;
        };
        values.insert(metric.to_string(), value);
    }
    Snapshot { at, values }
}

/// Per-second rate between two snapshots for a cumulative counter; `None` on the first sample, on counter reset (new < old) or zero elapsed.
pub fn rate_per_second(prev: &Snapshot, next: &Snapshot, key: &str) -> Option<f64> {
    let old = prev.values.get(key)?;
    let new = next.values.get(key)?;
    if *new < *old {
        return None;
    }
    let elapsed = next.at.saturating_duration_since(prev.at).as_secs_f64();
    if elapsed <= 0.0 {
        return None;
    }
    Some((*new - *old) / elapsed)
}

fn push_history(history: &mut Vec<f64>, value: f64) {
    history.push(value);
    if history.len() > HISTORY_LEN {
        let drop = history.len() - HISTORY_LEN;
        history.drain(..drop);
    }
}

/// Rebuild the tile list from the latest snapshot and the previous one, appending to each tile's history (truncate to HISTORY_LEN).
pub fn update_tiles(tiles: &mut Vec<Tile>, prev: Option<&Snapshot>, next: &Snapshot) {
    let mut next_tiles = Vec::new();
    for spec in TILE_SPECS {
        if !next.values.contains_key(spec.source) {
            continue;
        }
        let value = if spec.rate {
            prev.and_then(|p| rate_per_second(p, next, spec.source))
        } else {
            next.values.get(spec.source).copied()
        };
        let mut history = tiles
            .iter()
            .find(|t| t.key == spec.key)
            .map(|t| t.history.clone())
            .unwrap_or_default();
        if let Some(v) = value {
            push_history(&mut history, v);
        }
        let max = if spec.unit == Unit::Ratio {
            next.values.get("connections_max").copied()
        } else {
            None
        };
        let detail = if spec.source == "deadlocks_total" {
            next.values.get(spec.source).map(|v| {
                crate::localization::format(
                    "explorer.server_dashboard.deadlocks_total_detail",
                    &[("count", &format_compact(*v))],
                )
            })
        } else {
            None
        };
        next_tiles.push(Tile {
            key: spec.key,
            label: crate::localization::text(spec.label),
            value,
            unit: spec.unit,
            history,
            max,
            detail,
            section: spec.section,
        });
    }
    *tiles = next_tiles;
}

fn format_compact(value: f64) -> String {
    let abs = value.abs();
    if abs >= 1_000_000.0 {
        format!("{:.1}M", value / 1_000_000.0)
    } else if abs >= 1_000.0 {
        format!("{:.1}k", value / 1_000.0)
    } else if value.fract() == 0.0 {
        format!("{:.0}", value)
    } else {
        format!("{:.1}", value)
    }
}

fn format_duration(seconds: f64) -> String {
    let secs = seconds.max(0.0) as u64;
    let days = secs / 86_400;
    let hours = (secs % 86_400) / 3_600;
    let mins = (secs % 3_600) / 60;
    let rem = secs % 60;
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {mins}m")
    } else if mins > 0 {
        format!("{mins}m {rem}s")
    } else {
        format!("{rem}s")
    }
}

/// Human label: "1.2k/s", "97.4 %", "3d 4h", "12 / 100", "512 MB".
pub fn format_value(value: f64, unit: Unit, max: Option<f64>) -> String {
    match unit {
        Unit::Count => format_compact(value),
        Unit::PerSecond => format!("{}/s", format_compact(value)),
        Unit::Percent => format!("{:.1} %", value),
        Unit::Seconds => format_duration(value),
        Unit::Megabytes => format!("{} MB", format_compact(value)),
        Unit::Ratio => {
            let left = format_compact(value);
            match max {
                Some(m) if m <= 0.0 => crate::localization::format(
                    "explorer.server_dashboard.ratio_unlimited",
                    &[("value", &left)],
                ),
                Some(m) => format!("{left} / {}", format_compact(m)),
                None => left,
            }
        }
    }
}

fn sparkline(history: Vec<f64>) -> impl IntoElement {
    canvas(
        |bounds, _, _| bounds,
        move |bounds, _, window, _| {
            if history.len() < 2 {
                return;
            }
            let mut min = f64::INFINITY;
            let mut max = f64::NEG_INFINITY;
            for &v in &history {
                min = min.min(v);
                max = max.max(v);
            }
            let range = (max - min).max(1e-9);
            let w = f32::from(bounds.size.width);
            let h = f32::from(bounds.size.height);
            let origin_x = f32::from(bounds.origin.x);
            let origin_y = f32::from(bounds.origin.y);
            let last = (history.len() - 1) as f32;
            let mut path = gpui::PathBuilder::stroke(px(1.5));
            for (i, &v) in history.iter().enumerate() {
                let t = i as f32 / last;
                let x = origin_x + w * t;
                let y = origin_y + h * (1.0 - ((v - min) / range) as f32);
                let p = point(px(x), px(y));
                if i == 0 {
                    path.move_to(p);
                } else {
                    path.line_to(p);
                }
            }
            if let Ok(path) = path.build() {
                window.paint_path(path, Theme::accent());
            }
        },
    )
}

impl ServerDashboardPane {
    /// One dashboard per connection: `cascade` is how many are already open,
    /// so the new card lands beside them instead of exactly on top.
    pub fn new(context: ToolDialogContext, cascade: usize, cx: &mut gpui::Context<Self>) -> Self {
        let ticker = {
            let weak = cx.weak_entity();
            let async_cx = cx.to_async();
            cx.spawn(async move |_this, _cx| {
                let mut wait = AUTO_REFRESH_SECS;
                loop {
                    async_cx
                        .background_executor()
                        .timer(std::time::Duration::from_secs(wait.max(1)))
                        .await;
                    let mut cx = async_cx.clone();
                    match weak.update(&mut cx, |this: &mut Self, cx| {
                        if !this.paused {
                            this.refresh(cx);
                            #[cfg(not(target_arch = "wasm32"))]
                            this.refresh_due_widgets(cx);
                        } else {
                            cx.notify();
                        }
                        this.interval_secs
                    }) {
                        Ok(interval) => wait = interval,
                        Err(_) => break,
                    }
                }
            })
        };
        let mut this = Self {
            context,
            tiles: Vec::new(),
            prev: None,
            sizes_grid: None,
            error: None,
            unavailable: Vec::new(),
            pending: None,
            loading: true,
            refresh_generation: 0,
            refresh_in_flight: false,
            last_refreshed: None,
            paused: false,
            interval_secs: AUTO_REFRESH_SECS,
            _ticker: ticker,
            focus_handle: cx.focus_handle(),
            // Fixed frame: the sizes grid manages its own scrolling and must
            // not sit inside the chrome's scroll container. Dashboards are
            // non-modal and several can be open at once, so each one opens a
            // step down and to the right of the last.
            frame: DialogFrame::new(Theme::scaled(960.0), Theme::scaled(640.0)).with_placement(
                crate::dialog::DialogPlacement::WindowPoint(gpui::point(
                    px(Theme::scaled(96.0) + Theme::scaled(28.0) * cascade as f32),
                    px(Theme::scaled(72.0) + Theme::scaled(28.0) * cascade as f32),
                )),
            ),
            windowed: false,
            #[cfg(not(target_arch = "wasm32"))]
            widgets: Vec::new(),
            #[cfg(not(target_arch = "wasm32"))]
            add_widget_open: false,
            #[cfg(not(target_arch = "wasm32"))]
            widget_error: None,
            #[cfg(not(target_arch = "wasm32"))]
            widgets_scroll: gpui::ScrollHandle::new(),
        };
        this.refresh(cx);
        #[cfg(not(target_arch = "wasm32"))]
        this.load_widgets(None, cx);
        this
    }

    /// Start one refresh: every metric group runs as its own query, in
    /// parallel, and the snapshot is assembled from whichever ones answer.
    fn refresh(&mut self, cx: &mut gpui::Context<Self>) {
        if self.refresh_in_flight {
            return;
        }
        let admin = self.context.services.admin_tools.clone();
        let database_type = self.context.profile.database_type;
        let Some(groups) = AdminToolsService::dashboard_metric_groups_for(&*admin, database_type)
        else {
            self.loading = false;
            self.refresh_in_flight = false;
            self.error = Some(crate::localization::format(
                "explorer.server_dashboard.unsupported_engine",
                &[("engine", &database_type.display_name())],
            ));
            cx.notify();
            return;
        };
        if groups.is_empty() {
            self.loading = false;
            self.refresh_in_flight = false;
            cx.notify();
            return;
        }
        self.refresh_in_flight = true;
        self.refresh_generation += 1;
        let generation = self.refresh_generation;
        self.pending = Some(PendingRefresh {
            remaining: groups.len(),
            values: HashMap::new(),
            unavailable: Vec::new(),
            first_error: None,
            succeeded: 0,
        });
        for (name, sql) in groups {
            crate::tool_dialog::run_query(&self.context, sql, cx, move |this, outcome, cx| {
                if this.refresh_generation != generation {
                    return;
                }
                let Some(pending) = this.pending.as_mut() else {
                    return;
                };
                match outcome.error {
                    Some(err) => {
                        pending.unavailable.push(name);
                        if pending.first_error.is_none() {
                            pending.first_error = Some(err);
                        }
                    }
                    None => {
                        pending.succeeded += 1;
                        let set = outcome.sets.into_iter().next().unwrap_or_default();
                        let snapshot = snapshot_from_set(&set.columns, &set.rows, Instant::now());
                        pending.values.extend(snapshot.values);
                    }
                }
                pending.remaining -= 1;
                if pending.remaining == 0 {
                    this.finish_metrics(generation, cx);
                }
                cx.notify();
            });
        }
        cx.notify();
    }

    /// All groups have answered: fold them into one snapshot, rebuild the
    /// tiles, and go fetch the per-database sizes.
    fn finish_metrics(&mut self, generation: u64, cx: &mut gpui::Context<Self>) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        self.last_refreshed = Some(Instant::now());
        self.unavailable = pending.unavailable;
        if pending.succeeded == 0 {
            // Nothing answered: this is a real failure, not a partial view.
            self.refresh_in_flight = false;
            self.loading = false;
            self.error = Some(pending.first_error.unwrap_or_else(|| {
                crate::localization::text("explorer.server_dashboard.no_metrics")
            }));
            cx.notify();
            return;
        }
        self.error = None;
        let next = Snapshot {
            at: Instant::now(),
            values: pending.values,
        };
        update_tiles(&mut self.tiles, self.prev.as_ref(), &next);
        self.prev = Some(next);

        let admin = self.context.services.admin_tools.clone();
        let Some(sizes_sql) = AdminToolsService::dashboard_sizes_query_for(
            &*admin,
            self.context.profile.database_type,
        ) else {
            self.refresh_in_flight = false;
            self.loading = false;
            cx.notify();
            return;
        };
        crate::tool_dialog::run_query(&self.context, sizes_sql, cx, move |this, outcome, cx| {
            if this.refresh_generation != generation {
                return;
            }
            this.refresh_in_flight = false;
            this.loading = false;
            match outcome.error {
                // The sizes grid is one panel among many — losing it must not
                // blank the tiles that did load.
                Some(_) => {
                    this.sizes_grid = None;
                    this.unavailable.push(crate::localization::text(
                        "explorer.server_dashboard.database_sizes_unavailable",
                    ));
                }
                None => {
                    let set = outcome.sets.into_iter().next().unwrap_or_default();
                    this.sizes_grid = crate::tool_dialog::build_result_grid(&set, cx);
                }
            }
            cx.notify();
        });
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
    /// same dashboard — tiles, sparkline history, and its 5 s ticker — carries
    /// on in the window.
    fn pop_out(&mut self, cx: &mut gpui::Context<Self>) {
        let entity = cx.entity();
        let connection = self.context.profile.name.clone();
        let title = move || {
            crate::localization::format(
                "explorer.server_dashboard.title",
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
                    Theme::scaled(960.0),
                    Theme::scaled(700.0),
                ),
                None,
                cx,
                move |_, cx| {
                    build_entity.update(cx, |pane, _| pane.windowed = true);
                    build_entity
                },
            );
            if opened.is_some() {
                // The window owns the dashboard now; drop the inline card.
                entity.update(cx, |_, cx| cx.emit(ToolDialogEvent::Close));
            }
        });
    }

    fn age_label(&self) -> String {
        match self.last_refreshed {
            Some(at) => crate::localization::format(
                "explorer.server_dashboard.refreshed_ago",
                &[("seconds", &at.elapsed().as_secs().to_string())],
            ),
            None if self.loading => crate::localization::text("explorer.server_dashboard.loading"),
            None => crate::localization::text("explorer.server_dashboard.not_refreshed"),
        }
    }
}

/// Custom widgets: saved charts pinned to this connection's dashboard. Pins
/// persist as JSON keyed by the connection's stable id; the tiles run through
/// this pane's captured route with the Chart Dashboard's read-only SELECT
/// rule and each chart's own refresh interval.
#[cfg(not(target_arch = "wasm32"))]
impl ServerDashboardPane {
    fn connection_key(&self) -> String {
        self.context.profile.id.0.to_string()
    }

    /// Re-read the pin list and rebuild the widget tiles, preserving any tile
    /// whose chart is still pinned (its rendered chart stays, and it is not
    /// re-run unless its query changed — reordering never refetches). Pins
    /// naming deleted charts are pruned from disk here so a deleted chart does
    /// not linger as a broken tile forever. `action_error` is the failure of
    /// the pin/unpin/move that triggered the reload; it is shown ahead of any
    /// store load error rather than being overwritten by it.
    fn load_widgets(&mut self, action_error: Option<String>, cx: &mut gpui::Context<Self>) {
        let pins = crate::saved_charts::WidgetPinStore::default_store();
        let charts = crate::saved_charts::SavedChartStore::default_store();
        let chart_error = charts.load_error();
        let connection = self.connection_key();
        let library = charts.list();
        let pin_ids = if chart_error.is_some() {
            // The library could not be read, so every chart would look
            // deleted: never prune against it (that would wipe the pins).
            pins.pins_for(&connection)
        } else {
            let live: Vec<String> = library.iter().map(|c| c.id.clone()).collect();
            pins.prune(&connection, &live)
        };
        self.widget_error = action_error.or_else(|| pins.load_error()).or(chart_error);
        let ordered = crate::saved_charts::resolve_pinned_charts(&library, &pin_ids);
        let (tiles, to_run) = rebuild_widget_tiles(
            std::mem::take(&mut self.widgets),
            ordered,
            self.context.profile.database_type,
        );
        self.widgets = tiles;
        for index in to_run {
            self.refresh_widget(index, cx);
        }
        cx.notify();
    }

    fn pin_widget(&mut self, chart_id: String, cx: &mut gpui::Context<Self>) {
        let pins = crate::saved_charts::WidgetPinStore::default_store();
        let error = pins.pin(&self.connection_key(), &chart_id).err();
        self.add_widget_open = false;
        self.load_widgets(error, cx);
    }

    fn unpin_widget(&mut self, chart_id: &str, cx: &mut gpui::Context<Self>) {
        let pins = crate::saved_charts::WidgetPinStore::default_store();
        let error = pins.unpin(&self.connection_key(), chart_id).err();
        self.load_widgets(error, cx);
    }

    fn move_widget(&mut self, chart_id: &str, earlier: bool, cx: &mut gpui::Context<Self>) {
        let pins = crate::saved_charts::WidgetPinStore::default_store();
        let error = pins
            .move_pin(&self.connection_key(), chart_id, earlier)
            .err();
        self.load_widgets(error, cx);
    }

    /// Cycle a widget's refresh interval (manual → 30 s → 1 min → 5 min →
    /// 15 min → manual). The interval is the saved chart's own, so the Chart
    /// Dashboard shows the same cadence.
    fn cycle_widget_refresh(&mut self, index: usize, cx: &mut gpui::Context<Self>) {
        let Some(tile) = self.widgets.get_mut(index) else {
            return;
        };
        let next = crate::saved_charts::next_refresh_choice(tile.chart.refresh_secs);
        match crate::saved_charts::SavedChartStore::default_store()
            .set_refresh(&tile.chart.id, next)
        {
            Ok(()) => tile.chart.refresh_secs = next,
            Err(error) => self.widget_error = Some(error),
        }
        cx.notify();
    }

    /// The 5 s metric tick drives widget scheduling too: a widget starts a
    /// run when its own saved interval has elapsed since its last completion
    /// (or it has never completed), never while a run is in flight.
    fn refresh_due_widgets(&mut self, cx: &mut gpui::Context<Self>) {
        let due: Vec<usize> = self
            .widgets
            .iter()
            .enumerate()
            .filter(|(_, tile)| {
                tile_refresh_due(
                    tile.chart.refresh_secs,
                    tile.loading,
                    tile.select_only,
                    tile.secs_since_completion(),
                )
            })
            .map(|(index, _)| index)
            .collect();
        for index in due {
            self.refresh_widget(index, cx);
        }
    }

    fn refresh_widget(&mut self, index: usize, cx: &mut gpui::Context<Self>) {
        let Some(tile) = self.widgets.get_mut(index) else {
            return;
        };
        if tile.loading {
            return;
        }
        // The chart dashboard's contract, verbatim: only read-only SELECT
        // runs from a dashboard tile, however the SQL got here.
        if !tile.select_only {
            tile.error = Some(crate::chart_dashboard::not_select_message());
            cx.notify();
            return;
        }
        tile.loading = true;
        tile.generation += 1;
        let generation = tile.generation;
        let chart_id = tile.chart.id.clone();
        let sql = tile.chart.sql.clone();
        // Same route and services as the dashboard, but on the database the
        // chart was saved against.
        let context = ToolDialogContext::new(
            widget_profile(&self.context.profile, &tile.chart),
            self.context.services.clone(),
        );
        crate::tool_dialog::run_query_with_cap(
            &context,
            sql,
            MAX_SOURCE_ROWS,
            cx,
            move |this, outcome, cx| this.apply_widget_outcome(&chart_id, generation, outcome, cx),
        );
        cx.notify();
    }

    fn apply_widget_outcome(
        &mut self,
        chart_id: &str,
        generation: u64,
        outcome: ToolQueryOutcome,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some(index) = self
            .widgets
            .iter()
            .position(|tile| tile.chart.id == chart_id)
        else {
            return;
        };
        let now_ms = chrono::Utc::now().timestamp_millis();
        let tile = &mut self.widgets[index];
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
                "explorer.server_dashboard.widget_no_result_set",
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
        tile.error = None;
        let rows = std::sync::Arc::new(rows);
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
        let _ =
            crate::saved_charts::SavedChartStore::default_store().touch_last_run(chart_id, now_ms);
        cx.notify();
    }

    /// One widget tile: chart canvas (or its honest error), the saved refresh
    /// interval, and the remove/reorder verbs the pin list persists.
    fn render_widget(&self, index: usize, cx: &mut gpui::Context<Self>) -> gpui::AnyElement {
        let s = Theme::scaled;
        let tile = &self.widgets[index];
        let scope = self.context.profile.id.0;
        let chart_id = tile.chart.id.clone();
        let now_ms = chrono::Utc::now().timestamp_millis();
        let stamp = match (tile.loading, tile.last_completed_unix_ms) {
            (true, _) => crate::localization::text("explorer.server_dashboard.widget_refreshing"),
            (false, Some(ms)) => crate::localization::format(
                "explorer.server_dashboard.widget_refreshed",
                &[("age", &relative_age(ms, now_ms))],
            ),
            (false, None) => {
                crate::localization::text("explorer.server_dashboard.widget_not_refreshed")
            }
        };
        let interval = refresh_label(tile.chart.refresh_secs);
        let can_refresh = tile.select_only && !tile.loading;
        let first = index == 0;
        let last = index + 1 == self.widgets.len();

        let body: gpui::AnyElement = if let Some(error) = &tile.error {
            div()
                .size_full()
                .p(px(s(10.0)))
                .text_size(px(Theme::font_size_ratio(0.80)))
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
                .text_size(px(Theme::font_size_ratio(0.80)))
                .text_color(Theme::text_dim())
                .child(crate::localization::text(if tile.loading {
                    "explorer.server_dashboard.widget_loading"
                } else {
                    "explorer.server_dashboard.widget_no_data"
                }))
                .into_any_element()
        };

        div()
            .w(px(s(320.0)))
            .h(px(s(220.0)))
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
                    .gap(px(s(4.0)))
                    .px(px(s(8.0)))
                    .py(px(s(4.0)))
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
                                    .text_size(px(Theme::font_size_ratio(0.84)))
                                    .text_color(Theme::text())
                                    .child(tile.chart.name.clone()),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(Theme::font_size_ratio(0.70)))
                                    .text_color(Theme::text_dim())
                                    .child(stamp),
                            ),
                    )
                    .child(
                        Button::new(SharedString::from(format!(
                            "dash-widget-interval-{scope}-{chart_id}"
                        )))
                        .ghost()
                        .xsmall()
                        .label(interval)
                        .tooltip(crate::localization::text(
                            "explorer.server_dashboard.widget_interval_tooltip",
                        ))
                        .on_click(cx.listener(
                            move |this, _ev, _w, cx| {
                                this.cycle_widget_refresh(index, cx);
                            },
                        )),
                    )
                    .child(
                        Button::new(SharedString::from(format!(
                            "dash-widget-refresh-{scope}-{chart_id}"
                        )))
                        .ghost()
                        .xsmall()
                        .label(crate::localization::text(
                            "explorer.server_dashboard.widget_refresh_button",
                        ))
                        .disabled(!can_refresh)
                        .on_click(cx.listener(
                            move |this, _ev, _w, cx| {
                                this.refresh_widget(index, cx);
                            },
                        )),
                    )
                    .child(
                        Button::new(SharedString::from(format!(
                            "dash-widget-earlier-{scope}-{chart_id}"
                        )))
                        .ghost()
                        .xsmall()
                        .label("←")
                        .disabled(first)
                        .on_click(cx.listener({
                            let chart_id = chart_id.clone();
                            move |this, _ev, _w, cx| this.move_widget(&chart_id, true, cx)
                        })),
                    )
                    .child(
                        Button::new(SharedString::from(format!(
                            "dash-widget-later-{scope}-{chart_id}"
                        )))
                        .ghost()
                        .xsmall()
                        .label("→")
                        .disabled(last)
                        .on_click(cx.listener({
                            let chart_id = chart_id.clone();
                            move |this, _ev, _w, cx| this.move_widget(&chart_id, false, cx)
                        })),
                    )
                    .child(
                        Button::new(SharedString::from(format!(
                            "dash-widget-remove-{scope}-{chart_id}"
                        )))
                        .ghost()
                        .xsmall()
                        .label("✕")
                        .on_click(cx.listener(
                            move |this, _ev, _w, cx| {
                                this.unpin_widget(&chart_id, cx);
                            },
                        )),
                    ),
            )
            .child(div().flex_1().min_h(px(0.0)).child(body))
            .into_any_element()
    }

    /// The widgets band: pinned chart tiles plus, while the chooser is open,
    /// the connection's pinnable saved charts. Renders between the built-in
    /// metric bands and the sizes grid.
    fn render_widgets(&self, cx: &mut gpui::Context<Self>) -> Option<gpui::AnyElement> {
        if self.widgets.is_empty() && !self.add_widget_open && self.widget_error.is_none() {
            return None;
        }
        let s = Theme::scaled;
        let mut band = div().flex().flex_col().gap(px(s(6.0))).child(
            div()
                .text_color(Theme::text_dim())
                .text_size(px(Theme::font_size_ratio(0.74)))
                .child(crate::localization::text(
                    "explorer.server_dashboard.widgets_heading",
                )),
        );
        if let Some(error) = &self.widget_error {
            band = band.child(
                div()
                    .text_color(Theme::warning())
                    .text_size(px(Theme::font_size_ratio(0.80)))
                    .whitespace_normal()
                    .child(crate::localization::format(
                        "explorer.server_dashboard.widget_list_unavailable",
                        &[("error", error)],
                    )),
            );
        }
        if self.add_widget_open {
            let connection = self.connection_key();
            let pinned: Vec<String> = self
                .widgets
                .iter()
                .map(|tile| tile.chart.id.clone())
                .collect();
            let candidates = crate::saved_charts::pinnable_charts(
                &crate::saved_charts::SavedChartStore::default_store().list(),
                &connection,
                &pinned,
            );
            let mut chooser = div()
                .id("dash-widget-chooser")
                .max_h(px(s(140.0)))
                .overflow_y_scroll()
                .border_1()
                .border_color(Theme::border())
                .rounded(px(s(4.0)))
                .flex()
                .flex_col();
            if candidates.is_empty() {
                chooser = chooser.child(
                    div()
                        .p(px(s(10.0)))
                        .text_size(px(Theme::font_size_ratio(0.80)))
                        .text_color(Theme::text_dim())
                        .whitespace_normal()
                        .child(crate::localization::text(
                            "explorer.server_dashboard.widget_chooser_empty",
                        )),
                );
            }
            for chart in candidates {
                let select_only = crate::saved_charts::widget_sql_is_select_only(
                    &chart,
                    self.context.profile.database_type,
                );
                let chart_id = chart.id.clone();
                let mut row = div()
                    .id(SharedString::from(format!("dash-widget-pick-{}", chart.id)))
                    .px(px(s(10.0)))
                    .py(px(s(5.0)))
                    .text_size(px(Theme::font_size_ratio(0.84)))
                    .text_color(if select_only {
                        Theme::text()
                    } else {
                        Theme::text_dim()
                    })
                    .child(crate::localization::format(
                        if select_only {
                            "explorer.server_dashboard.widget_candidate"
                        } else {
                            "explorer.server_dashboard.widget_candidate_not_select"
                        },
                        &[("name", &chart.name), ("location", &chart.location_label())],
                    ));
                if select_only {
                    row = row
                        .cursor_pointer()
                        .hover(|d| d.bg(Theme::row_hover()))
                        .on_click(cx.listener(move |this, _ev, _w, cx| {
                            this.pin_widget(chart_id.clone(), cx);
                        }));
                }
                chooser = chooser.child(row);
            }
            band = band.child(chooser);
        }
        if !self.widgets.is_empty() {
            band = band.child(
                div()
                    .id("dash-widget-tiles")
                    .max_h(px(s(240.0)))
                    .overflow_y_scroll()
                    .track_scroll(&self.widgets_scroll)
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .gap(px(s(8.0)))
                            .children(
                                (0..self.widgets.len()).map(|index| self.render_widget(index, cx)),
                            ),
                    ),
            );
        }
        Some(band.into_any_element())
    }
}

impl gpui::Focusable for ServerDashboardPane {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ServerDashboardPane {
    fn render(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;

        let render_tile = |tile: &Tile| {
            let displayed = tile
                .value
                .map(|v| format_value(v, tile.unit, tile.max))
                .unwrap_or_else(|| "—".into());
            div()
                .w(px(s(168.0)))
                .p(px(s(10.0)))
                .bg(Theme::panel_bg())
                .border_1()
                .border_color(Theme::border())
                .rounded(px(s(6.0)))
                .flex()
                .flex_col()
                .gap(px(s(4.0)))
                .child(
                    div()
                        .text_color(Theme::text_dim())
                        .text_size(px(Theme::font_size_ratio(0.80)))
                        .child(tile.label.clone()),
                )
                .child(
                    div()
                        .text_color(Theme::text())
                        .text_size(px(Theme::font_size_ratio(1.15)))
                        .child(displayed),
                )
                .children(tile.detail.clone().map(|detail| {
                    div()
                        .text_color(Theme::text_dim())
                        .text_size(px(Theme::font_size_ratio(0.74)))
                        .child(detail)
                }))
                .child(
                    div()
                        .w(px(s(HISTORY_LEN as f32)))
                        .h(px(s(24.0)))
                        .child(sparkline(tile.history.clone())),
                )
        };

        // One band per section, in a fixed order; a section whose engine
        // reported none of its metrics is left out entirely rather than
        // shown as an empty heading.
        let tiles = div()
            .flex()
            .flex_col()
            .gap(px(s(10.0)))
            .children(SECTIONS.iter().filter_map(|section| {
                let of_section: Vec<&Tile> = self
                    .tiles
                    .iter()
                    .filter(|tile| tile.section == *section)
                    .collect();
                if of_section.is_empty() {
                    return None;
                }
                Some(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(s(6.0)))
                        .child(
                            div()
                                .text_color(Theme::text_dim())
                                .text_size(px(Theme::font_size_ratio(0.74)))
                                .child(section.title().to_uppercase()),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .flex_wrap()
                                .gap(px(s(8.0)))
                                .children(of_section.into_iter().map(render_tile)),
                        ),
                )
            }));

        let content: gpui::AnyElement = if let Some(err) = &self.error {
            div()
                .p(px(s(12.0)))
                .text_color(Theme::error())
                .text_size(px(Theme::font_size_ratio(0.87)))
                .whitespace_normal()
                .child(crate::localization::format(
                    "explorer.server_dashboard.load_failed",
                    &[
                        ("error", err),
                        (
                            "permission",
                            &permission_guidance(self.context.profile.database_type),
                        ),
                    ],
                ))
                .into_any_element()
        } else if self.loading && self.tiles.is_empty() {
            div()
                .p(px(s(12.0)))
                .text_color(Theme::text_dim())
                .child(crate::localization::text(
                    "explorer.server_dashboard.loading",
                ))
                .into_any_element()
        } else {
            // The tiles and widgets keep their natural height and the
            // column scrolls; the sizes grid fills what is left but never
            // below a few rows. With `min_h(0)` a full tile board squeezed
            // it to nothing (a dashboard with no database sizes visible).
            let mut column = div()
                .id(SharedString::from(format!(
                    "dash-content-{}",
                    self.context.profile.id.0
                )))
                .flex()
                .flex_col()
                .gap(px(s(10.0)))
                .size_full()
                .overflow_y_scroll()
                .child(tiles);
            #[cfg(not(target_arch = "wasm32"))]
            if let Some(band) = self.render_widgets(cx) {
                column = column.child(band);
            }
            column
                .child(
                    div()
                        .flex_1()
                        .flex_shrink_0()
                        .min_h(px(s(SIZES_GRID_MIN_HEIGHT)))
                        .border_1()
                        .border_color(Theme::border())
                        .rounded(px(s(4.0)))
                        .child(if let Some(grid) = &self.sizes_grid {
                            div().size_full().child(grid.clone()).into_any_element()
                        } else {
                            div()
                                .p(px(s(12.0)))
                                .text_color(Theme::text_dim())
                                .child(crate::localization::text(
                                    "explorer.server_dashboard.no_database_sizes",
                                ))
                                .into_any_element()
                        }),
                )
                .into_any_element()
        };

        // Bare body: the sizes grid manages its own scrolling.
        let body = div()
            .flex_1()
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .gap(px(s(8.0)))
            .px(px(s(14.0)))
            .py(px(s(10.0)))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(s(10.0)))
                    .child(
                        div()
                            .text_color(Theme::text_dim())
                            .text_size(px(Theme::font_size_ratio(0.80)))
                            .child(self.age_label()),
                    )
                    // Naming what this login cannot read beats a silently
                    // shorter dashboard: a missing tile is otherwise
                    // indistinguishable from a metric the engine lacks.
                    .children(
                        (!self.unavailable.is_empty() && self.error.is_none()).then(|| {
                            div()
                                .text_color(Theme::warning())
                                .text_size(px(Theme::font_size_ratio(0.80)))
                                .child(crate::localization::format(
                                    "explorer.server_dashboard.unavailable_groups",
                                    &[("groups", &self.unavailable.join(", "))],
                                ))
                        }),
                    ),
            )
            .child(div().flex_1().min_h(px(0.0)).child(content));

        // Every id is scoped to the connection: several dashboards render at
        // once, and gpui element ids must be unique among them.
        let scope = self.context.profile.id.0;
        let focus_handle = self.focus_handle.clone();
        let mut dialog = Dialog::new(
            format!("server-dashboard-pane-{scope}"),
            |this: &mut Self| &mut this.frame,
        )
        .title(crate::localization::format(
            "explorer.server_dashboard.title",
            &[("connection", &self.context.profile.name)],
        ))
        // Non-modal: a dashboard per connection, all watchable side by side
        // while the rest of the app stays usable behind them.
        .is_modal(false)
        .easy_close(true)
        .key_context("ServerDashboardPane")
        .track_focus(&focus_handle)
        .bare_body()
        .windowed(self.windowed)
        .on_close(|this, window, cx| this.close(window, cx))
        .child(body)
        .leading_button(
            Button::new(SharedString::from(format!("dash-pause-{scope}")))
                .label(crate::localization::text(if self.paused {
                    "explorer.server_dashboard.resume_button"
                } else {
                    "explorer.server_dashboard.pause_button"
                }))
                .on_click(cx.listener(|this, _, _window, cx| {
                    this.paused = !this.paused;
                    cx.notify();
                })),
        )
        .leading_button(
            Button::new(SharedString::from(format!("dash-refresh-{scope}")))
                .label(crate::localization::text(
                    "explorer.server_dashboard.refresh_button",
                ))
                .disabled(self.refresh_in_flight)
                .on_click(cx.listener(|this, _, _window, cx| this.refresh(cx))),
        );
        #[cfg(not(target_arch = "wasm32"))]
        {
            dialog = dialog.leading_button(
                Button::new(SharedString::from(format!("dash-add-widget-{scope}")))
                    .label(crate::localization::text(if self.add_widget_open {
                        "explorer.server_dashboard.close_chooser_button"
                    } else {
                        "explorer.server_dashboard.add_widget_button"
                    }))
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.add_widget_open = !this.add_widget_open;
                        cx.notify();
                    })),
            );
        }
        // The cross-tool links are navigation, not actions on this
        // dashboard: they belong in the footer's leading utility cluster,
        // de-emphasized, rather than mixed into the Close/action cluster on
        // the right where the primary action of a dialog normally sits. The
        // Redis-only Memory Analyzer joins them beside Disk Usage.
        let mut links = vec![
            (
                "activity",
                crate::capabilities::AdminTool::ActivityMonitor.label(),
                AdminToolLink::ActivityMonitor,
            ),
            (
                "insights",
                crate::capabilities::AdminTool::QueryInsights.label(),
                AdminToolLink::QueryInsights,
            ),
            (
                "disk",
                crate::capabilities::AdminTool::DiskUsage.label(),
                AdminToolLink::DiskUsage,
            ),
        ];
        if self.context.profile.database_type.connection_kind()
            == sqlly_lang::sqlmodel::ConnectionKind::Resp
        {
            links.push((
                "memory",
                crate::capabilities::AdminTool::RedisMemoryAnalyzer.label(),
                AdminToolLink::RedisMemoryAnalyzer,
            ));
            // A cluster (set so, or detected on this session's connect) gets
            // its topology beside the memory report.
            #[cfg(not(target_arch = "wasm32"))]
            if sqlly_engines::redis::cluster::is_known_cluster(&self.context.profile) {
                links.push((
                    "topology",
                    crate::capabilities::AdminTool::RedisClusterTopology.label(),
                    AdminToolLink::RedisClusterTopology,
                ));
            }
        }
        for (id, label, tool) in links {
            dialog = dialog.leading_button(
                Button::new(SharedString::from(format!("dash-{id}-{scope}")))
                    .ghost()
                    .label(label)
                    .on_click(cx.listener(move |_this, _, _window, cx| {
                        cx.emit(ToolDialogEvent::OpenTool(tool));
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
    use std::time::Duration;

    fn column(name: &str) -> ResultColumn {
        ResultColumn {
            name: name.into(),
            data_type: "varchar".into(),
            nullable: true,
        }
    }

    fn snap_at(at: Instant, pairs: &[(&str, f64)]) -> Snapshot {
        Snapshot {
            at,
            values: pairs.iter().map(|(k, v)| ((*k).to_string(), *v)).collect(),
        }
    }

    #[test]
    fn snapshot_skips_non_numeric_rows() {
        let columns = vec![column("metric"), column("value")];
        let rows = vec![
            vec![
                CellValue::String("connections_active".into()),
                CellValue::I64(12),
            ],
            vec![
                CellValue::String("cache_hit_ratio".into()),
                CellValue::String("97.4".into()),
            ],
            vec![CellValue::String("queries_total".into()), CellValue::Null],
            vec![
                CellValue::String("deadlocks_total".into()),
                CellValue::String("nope".into()),
            ],
            vec![
                CellValue::String("temp_files_total".into()),
                CellValue::Bytes(vec![1]),
            ],
        ];
        let snap = snapshot_from_set(&columns, &rows, Instant::now());
        assert_eq!(snap.values.get("connections_active"), Some(&12.0));
        assert_eq!(snap.values.get("cache_hit_ratio"), Some(&97.4));
        assert!(!snap.values.contains_key("queries_total"));
        assert!(!snap.values.contains_key("deadlocks_total"));
        assert!(!snap.values.contains_key("temp_files_total"));
    }

    #[test]
    fn rate_is_none_on_first_sample_and_on_reset() {
        let t0 = Instant::now();
        let first = snap_at(t0, &[("transactions_total", 1000.0)]);
        let mut tiles = Vec::new();
        update_tiles(&mut tiles, None, &first);
        let tx = tiles
            .iter()
            .find(|t| t.key == "transactions_total")
            .unwrap();
        assert_eq!(tx.value, None);

        let reset = snap_at(
            t0 + Duration::from_secs(5),
            &[("transactions_total", 100.0)],
        );
        assert_eq!(rate_per_second(&first, &reset, "transactions_total"), None);
    }

    #[test]
    fn rate_uses_elapsed_seconds() {
        let t0 = Instant::now();
        let prev = snap_at(t0, &[("transactions_total", 1000.0)]);
        let next = snap_at(
            t0 + Duration::from_secs(5),
            &[("transactions_total", 1500.0)],
        );
        assert_eq!(
            rate_per_second(&prev, &next, "transactions_total"),
            Some(100.0)
        );
    }

    #[test]
    fn history_is_capped_at_sixty() {
        let mut tiles = Vec::new();
        let t0 = Instant::now();
        for i in 0..70u64 {
            let snap = snap_at(
                t0 + Duration::from_secs(i),
                &[("cache_hit_ratio", i as f64)],
            );
            update_tiles(&mut tiles, None, &snap);
        }
        let tile = tiles.iter().find(|t| t.key == "cache_hit_ratio").unwrap();
        assert_eq!(tile.history.len(), HISTORY_LEN);
        assert_eq!(tile.history[0], 10.0);
        assert_eq!(*tile.history.last().unwrap(), 69.0);
    }

    #[test]
    fn tiles_hide_keys_absent_from_snapshot() {
        let mut tiles = Vec::new();
        let snap = snap_at(
            Instant::now(),
            &[("connections_active", 4.0), ("uptime_seconds", 90.0)],
        );
        update_tiles(&mut tiles, None, &snap);
        let keys: Vec<_> = tiles.iter().map(|t| t.key).collect();
        assert!(keys.contains(&"connections_active"));
        assert!(keys.contains(&"uptime_seconds"));
        assert!(!keys.contains(&"queries_total"));
        assert!(!keys.contains(&"replication_lag_seconds"));
    }

    /// Tiles are grouped for reading: sessions first, then throughput, health,
    /// and what the server is consuming.
    #[test]
    fn tiles_carry_their_section() {
        let mut tiles = Vec::new();
        let snap = snap_at(
            Instant::now(),
            &[
                ("connections_active", 4.0),
                ("sessions_blocked", 1.0),
                ("queries_total", 10.0),
                ("deadlocks_total", 0.0),
                ("storage_total_mb", 2048.0),
            ],
        );
        update_tiles(&mut tiles, None, &snap);
        let section_of = |key: &str| tiles.iter().find(|t| t.key == key).unwrap().section;
        assert_eq!(section_of("connections_active"), Section::Sessions);
        assert_eq!(section_of("sessions_blocked"), Section::Sessions);
        assert_eq!(section_of("queries_total"), Section::Throughput);
        assert_eq!(section_of("deadlocks_total"), Section::Health);
        assert_eq!(section_of("storage_total_mb"), Section::Resources);
    }

    /// Each engine's metric keys must have a tile to land in, or its dashboard
    /// silently drops metrics it went to the trouble of querying.
    #[test]
    fn every_engine_metric_key_has_a_tile() {
        use sqlly_services::DashboardDialect;
        for dialect in [
            DashboardDialect::SqlServer,
            DashboardDialect::Postgres,
            DashboardDialect::MySql,
            DashboardDialect::MariaDb,
            DashboardDialect::Sqlite,
            DashboardDialect::DuckDb,
            DashboardDialect::ClickHouse,
            DashboardDialect::Oracle,
        ] {
            for group in sqlly_services::Dashboard::metric_groups_for(dialect) {
                for fragment in group.sql.split("SELECT '").skip(1) {
                    let key = fragment.split('\'').next().unwrap_or_default();
                    // `connections_max` is a tile's denominator, not a tile.
                    if key == "connections_max" {
                        continue;
                    }
                    assert!(
                        TILE_SPECS.iter().any(|spec| spec.source == key),
                        "{dialect:?} reports {key} with no tile to show it in"
                    );
                }
            }
        }
    }

    /// A file database has no sessions or counters, so its dashboard is the
    /// Resources band alone — and it must not come up empty.
    #[test]
    fn sqlite_reports_file_statistics() {
        let mut tiles = Vec::new();
        let snap = snap_at(
            Instant::now(),
            &[
                ("storage_total_mb", 12.5),
                ("reclaimable_mb", 0.5),
                ("objects_total", 42.0),
                ("indexes_total", 17.0),
            ],
        );
        update_tiles(&mut tiles, None, &snap);
        assert_eq!(tiles.len(), 4);
        assert!(tiles.iter().all(|t| t.section == Section::Resources));
        let storage = tiles.iter().find(|t| t.key == "storage_total_mb").unwrap();
        assert_eq!(
            format_value(storage.value.unwrap(), storage.unit, None),
            "12.5 MB"
        );
    }

    #[test]
    fn format_value_units() {
        assert_eq!(format_value(12.0, Unit::Ratio, Some(100.0)), "12 / 100");
        assert_eq!(format_value(7.0, Unit::Count, None), "7");
        assert_eq!(format_value(97.4, Unit::Percent, None), "97.4 %");
        assert_eq!(
            format_value(3.0 * 86_400.0 + 4.0 * 3_600.0, Unit::Seconds, None),
            "3d 4h"
        );
        assert_eq!(format_value(1200.0, Unit::PerSecond, None), "1.2k/s");
        assert_eq!(format_value(512.0, Unit::Megabytes, None), "512 MB");
        assert_eq!(format_value(12.0, Unit::Ratio, Some(0.0)), "12 / unlimited");
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn widget_chart(sql: &str) -> crate::saved_charts::SavedChart {
        crate::saved_charts::SavedChart {
            id: "c1".into(),
            name: "widget".into(),
            created_unix_ms: 0,
            updated_unix_ms: 0,
            connection_id: Some("conn".into()),
            connection_name: "conn".into(),
            database: None,
            sql: sql.into(),
            chart_directive: String::new(),
            refresh_secs: Some(60),
            last_run_unix_ms: None,
        }
    }

    /// The tile captures the read-only verdict when it is built: a non-SELECT
    /// chart pinned to a dashboard is refused at refresh time, before any SQL
    /// leaves the app — the chart dashboard's rule, unchanged.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn widget_tile_captures_select_only_at_construction() {
        assert!(
            WidgetTile::new(widget_chart("SELECT 1"), sqlly_core::DatabaseType::Postgres)
                .select_only
        );
        assert!(
            WidgetTile::new(
                widget_chart("WITH x AS (SELECT 1) SELECT * FROM x"),
                sqlly_core::DatabaseType::Postgres
            )
            .select_only
        );
        assert!(
            !WidgetTile::new(
                widget_chart("DELETE FROM t WHERE id = 1"),
                sqlly_core::DatabaseType::Postgres
            )
            .select_only
        );
        assert!(
            !WidgetTile::new(
                widget_chart("SELECT 1; DROP TABLE t"),
                sqlly_core::DatabaseType::Postgres
            )
            .select_only
        );
    }

    /// A widget runs on the database its chart was saved against, not the
    /// dashboard connection's default database.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn widget_profile_retargets_to_the_charts_database() {
        let mut base = sqlly_core::ConnectionProfile::sql_login(
            "dash",
            "localhost",
            "sa",
            sqlly_core::SecretRef {
                provider: sqlly_core::SecretProvider::InMemory,
                key: "k".into(),
            },
        );
        base.database = Some("master".into());
        let mut chart = widget_chart("SELECT 1");
        chart.database = Some("Sales".into());
        let profile = widget_profile(&base, &chart);
        assert_eq!(profile.database.as_deref(), Some("Sales"));
        assert_eq!(profile.id, base.id, "same connection, same route");
        chart.database = Some(String::new());
        assert_eq!(
            widget_profile(&base, &chart).database.as_deref(),
            Some("master")
        );
        chart.database = None;
        assert_eq!(
            widget_profile(&base, &chart).database.as_deref(),
            Some("master")
        );
    }

    /// Reordering pins keeps every tile and schedules no runs; a new pin and
    /// an edited chart are the only tiles that run.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn rebuilding_tiles_reruns_only_new_or_edited_charts() {
        let chart = |id: &str, sql: &str| {
            let mut chart = widget_chart(sql);
            chart.id = id.into();
            chart
        };
        let mut a = WidgetTile::new(chart("a", "SELECT 1"), sqlly_core::DatabaseType::Postgres);
        a.generation = 7;
        let b = WidgetTile::new(chart("b", "SELECT 2"), sqlly_core::DatabaseType::Postgres);
        let (tiles, to_run) = rebuild_widget_tiles(
            vec![a, b],
            vec![chart("b", "SELECT 2"), chart("a", "SELECT 1")],
            sqlly_core::DatabaseType::Postgres,
        );
        let ids: Vec<&str> = tiles.iter().map(|t| t.chart.id.as_str()).collect();
        assert_eq!(ids, vec!["b", "a"]);
        assert!(to_run.is_empty(), "a reorder never refetches");
        assert_eq!(tiles[1].generation, 7, "tile state survives the move");

        let (tiles, to_run) = rebuild_widget_tiles(
            tiles,
            vec![
                chart("b", "SELECT 2"),
                chart("a", "DELETE FROM t"),
                chart("c", "SELECT 3"),
            ],
            sqlly_core::DatabaseType::Postgres,
        );
        assert_eq!(to_run, vec![1, 2], "edited a and new c run; b does not");
        assert!(!tiles[1].select_only, "an edit re-checks the SELECT rule");
    }

    /// The pane's grid, drawn in the real pane, gets room for its rows (a
    /// grid in an auto-height wrapper paints zero pixels tall).
    #[gpui::test]
    fn database_sizes_grid_keeps_rows_below_a_full_tile_board(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            if cx.try_global::<gpui_component::Theme>().is_none() {
                gpui_component::init(cx);
            }
        });
        let context =
            crate::tool_dialog::unreachable_tool_context(sqlly_core::DatabaseType::SqlServer);
        let (view, cx) = cx.add_window_view(|_window, cx| ServerDashboardPane::new(context, 0, cx));
        cx.simulate_resize(gpui::size(gpui::px(1440.0), gpui::px(900.0)));
        cx.run_until_parked();
        view.update(cx, |pane, cx| {
            pane.error = None;
            pane.loading = false;
            // Every tile the engine can report, as on a live server.
            let snapshot = Snapshot {
                at: Instant::now(),
                values: TILE_SPECS
                    .iter()
                    .map(|spec| (spec.source.to_string(), 1.0))
                    .collect(),
            };
            update_tiles(&mut pane.tiles, None, &snapshot);
            assert!(pane.tiles.len() > 10, "a full tile board");
            pane.sizes_grid =
                crate::tool_dialog::build_result_grid(&crate::tool_dialog::sample_tool_set(20), cx);
            cx.notify();
        });
        cx.run_until_parked();
        view.read_with(cx, |pane, cx| {
            let grid = pane.sizes_grid.as_ref().expect("grid");
            crate::tool_dialog::assert_grid_shows_rows(
                grid,
                3,
                "Server Dashboard database sizes",
                cx,
            );
        });
    }
}
