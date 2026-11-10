//! Activity Monitor — a live admin pane over the connection's dynamic
//! management views: sessions, blocking chains, expensive queries, and wait
//! statistics, each in a real `SqllyDataTable` grid.
//!
//! Live-surface discipline (the contract every refreshing admin pane keeps):
//! one refresh in flight at a time, a generation token so a stale response
//! can never overwrite a newer one, rows bounded by the shared tool-dialog
//! cap, auto-refresh only while the pane is open on the current tab (the
//! timer dies with the entity), and errors — permission failures included —
//! rendered as honest text where the grid would be.

use gpui::{
    Entity, EventEmitter, FocusHandle, IntoElement, ParentElement, Render, Styled, Subscription,
    Task, Window, div, prelude::*, px,
};
use gpui_component::Disableable as _;
use gpui_component::Sizable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputEvent, InputState};
use sqlly_datatable::SqllyDataTable;
use sqlly_services::AdminToolsService;
use std::collections::{HashMap, HashSet};

use crate::dialog::{Dialog, DialogFrame};
use crate::theme::Theme;
use crate::tool_dialog::{ToolDialogContext, ToolDialogEvent, ToolQuerySet, ToolScope};

/// Seconds between automatic refreshes while the pane is open.
const AUTO_REFRESH_SECS: u64 = 5;

/// The refresh-interval choices the toolbar button cycles through.
const REFRESH_INTERVALS: [u64; 3] = [2, 5, 15];

/// Milliseconds between live-grid syncs while a Redis live tab is active.
const LIVE_TICK_MILLIS: u64 = 500;

/// Rows the live tables materialize (newest first); the rings behind them
/// hold more — the profiler ring defaults to 10,000 entries. The status line
/// and the banner both say so whenever the ring holds more than this.
const LIVE_DISPLAY_CAP: usize = 2_000;

/// Upper bound for the user-configurable ring caps.
const MAX_LIVE_CAP: usize = 100_000;

/// A deliberately conservative default: this is a triage hint, not a claim
/// that the query is pathological.
const LONG_RUNNING_SECS: i64 = 30;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum ActivityTriageFilter {
    #[default]
    All,
    Blockers,
    Blocked,
    LongRunning,
    IdleInTransaction,
    /// Decision-model act/watch/ignore ranking. Reachable only while the
    /// per-pane "Rank by attention" switch is on (which itself exists only
    /// when a decision provider is configured with row data allowed).
    Attention,
}

impl ActivityTriageFilter {
    fn label(self) -> String {
        crate::localization::text(match self {
            Self::All => "explorer.activity_monitor.filter_all",
            Self::Blockers => "explorer.activity_monitor.filter_blockers",
            Self::Blocked => "explorer.activity_monitor.filter_blocked",
            Self::LongRunning => "explorer.activity_monitor.filter_long_running",
            Self::IdleInTransaction => "explorer.activity_monitor.filter_idle_in_transaction",
            Self::Attention => "explorer.activity_monitor.filter_attention",
        })
    }

    fn next(self, attention_available: bool) -> Self {
        match self {
            Self::All => Self::Blockers,
            Self::Blockers => Self::Blocked,
            Self::Blocked => Self::LongRunning,
            Self::LongRunning => Self::IdleInTransaction,
            Self::IdleInTransaction if attention_available => Self::Attention,
            Self::IdleInTransaction | Self::Attention => Self::All,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct BlockingPosition {
    root: Option<i64>,
    depth: usize,
    cycle: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TerminationReview {
    session_id: i32,
    script: String,
}

/// A Pub/Sub row's message, kept for the Preview action. Shares the
/// capture's `Arc` instead of copying the payload on every grid refresh.
#[derive(Clone, Debug)]
enum LivePayload {
    #[cfg(not(target_arch = "wasm32"))]
    Message(std::sync::Arc<sqlly_services::redis_live::PubSubMessage>),
}

/// `HH:MM:SS.mmm` local wall clock for a Unix-epoch seconds value (the
/// MONITOR line's server timestamp).
fn clock_time(seconds: f64) -> String {
    let whole = seconds.trunc() as i64;
    let nanos = (seconds.fract() * 1e9).round() as u32;
    chrono::DateTime::from_timestamp(whole, nanos)
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format("%H:%M:%S%.3f")
                .to_string()
        })
        .unwrap_or_else(|| format!("{seconds:.3}"))
}

/// `HH:MM:SS.mmm` local wall clock for epoch milliseconds (a Pub/Sub
/// message's receive stamp).
fn clock_time_millis(millis: i64) -> String {
    chrono::DateTime::from_timestamp_millis(millis)
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format("%H:%M:%S%.3f")
                .to_string()
        })
        .unwrap_or_else(|| millis.to_string())
}

/// The Profiler's time box from its toolbar field, in seconds: blank or
/// unparsable text falls back to the default, and the value is clamped to
/// 1 s … one hour — the capture always has a hard stop.
#[cfg(not(target_arch = "wasm32"))]
fn parse_time_box_secs(text: &str) -> u64 {
    text.trim()
        .parse::<u64>()
        .ok()
        .filter(|n| *n >= 1)
        .unwrap_or(sqlly_services::redis_live::DEFAULT_PROFILER_TIME_BOX_SECS)
        .min(sqlly_services::redis_live::MAX_PROFILER_TIME_BOX_SECS)
}

/// The reviewed statement for a slow-log threshold typed into the Expensive
/// tab's field: whole microseconds, `-1` disables the log. `None` for
/// anything else (and for engines without a slow log), so invalid input
/// never reaches the server.
fn slowlog_set_statement(
    dialect: Option<sqlly_services::ActivityDialect>,
    text: &str,
) -> Option<String> {
    let micros = text.trim().replace('_', "").parse::<i64>().ok()?;
    sqlly_services::admin::activity::ActivityMonitor::slowlog_threshold_set_for(dialect?, micros)
}

/// Why a CONFIG SET / CONFIG REWRITE staged in the monitor (the slow-log
/// threshold, keyspace notifications) may not run: the production guard
/// has writes locked. `None` otherwise.
fn slowlog_write_block_reason(
    production: sqlly_intel::query::safety::ProductionDecision,
) -> Option<String> {
    (production == sqlly_intel::query::safety::ProductionDecision::ReadOnly)
        .then(|| crate::localization::text("explorer.activity_monitor.production_write_block"))
}

/// A CONFIG statement awaiting the user's confirmation (the slow-log
/// threshold change, the CONFIG REWRITE that persists it, or enabling
/// keyspace notifications for the Pub/Sub tab).
#[derive(Clone, Debug, PartialEq, Eq)]
struct SlowlogReview {
    statement: String,
    explanation: String,
}

/// The Pub/Sub tab's read of the keyspace-notification classes.
const KEYSPACE_NOTIFY_QUERY: &str = "CONFIG GET notify-keyspace-events";
/// The reviewed statement that turns key-event and keyspace notifications
/// on for every event class. Runtime only, like the slow-log threshold.
const KEYSPACE_NOTIFY_ENABLE: &str = "CONFIG SET notify-keyspace-events KEA";

/// The `notify-keyspace-events` value from a shaped `CONFIG GET` reply
/// (field/value rows); an empty string when the setting is off, `None`
/// when the rows do not carry it.
fn keyspace_notify_flags_from_rows(rows: &[Vec<sqlly_core::CellValue>]) -> Option<String> {
    rows.iter().find_map(|row| {
        let field = match row.first() {
            Some(sqlly_core::CellValue::String(s)) => s.as_str(),
            _ => return None,
        };
        if !field.eq_ignore_ascii_case("notify-keyspace-events") {
            return None;
        }
        match row.get(1) {
            Some(sqlly_core::CellValue::String(s)) => Some(s.trim().to_string()),
            Some(sqlly_core::CellValue::Null) | None => Some(String::new()),
            _ => None,
        }
    })
}

/// Whether the Key events preset will see anything: the server publishes
/// `__keyevent@<db>__:<event>` only with the `E` class plus at least one
/// event class (`A` for all, or any of `g$lshzxetdmn`).
fn keyspace_events_on(flags: &str) -> bool {
    flags.contains('E') && flags.chars().any(|c| !matches!(c, 'K' | 'E'))
}

/// The keyspace strip's lead text for the configured classes.
fn keyspace_notify_note(flags: Option<&str>) -> String {
    let key = match flags {
        None => "explorer.activity_monitor.keyspace_read_failed",
        Some("") => "explorer.activity_monitor.keyspace_off",
        Some(flags) if keyspace_events_on(flags) => "explorer.activity_monitor.keyspace_on",
        Some(_) => "explorer.activity_monitor.keyspace_key_events_off",
    };
    match flags {
        Some(flags) if !flags.is_empty() => crate::localization::format(key, &[("flags", flags)]),
        _ => crate::localization::text(key),
    }
}

/// The Pub/Sub grid's row for one message: the keyspace-notification
/// halves are their own columns so an `expired` on `user:42` reads at a
/// glance; ordinary messages leave them empty.
#[cfg(not(target_arch = "wasm32"))]
fn pubsub_row(message: &sqlly_services::redis_live::PubSubMessage) -> Vec<sqlly_core::CellValue> {
    let optional = |text: Option<&str>| {
        text.map(|text| sqlly_core::CellValue::String(live_cell_text(text)))
            .unwrap_or(sqlly_core::CellValue::Null)
    };
    vec![
        sqlly_core::CellValue::String(clock_time_millis(message.received_millis)),
        sqlly_core::CellValue::String(live_cell_text(&message.channel)),
        optional(message.pattern.as_deref()),
        optional(message.event.as_deref()),
        optional(message.key.as_deref()),
        sqlly_core::CellValue::String(pubsub_payload_cell(message)),
    ]
}

/// The logical database the Key events preset names in its channel
/// patterns: the profile's database, 0 when blank or unparsable (the
/// connection itself would already have refused a non-numeric one).
#[cfg(not(target_arch = "wasm32"))]
fn preset_database_index(profile: &sqlly_core::ConnectionProfile) -> i64 {
    sqlly_engines::redis::database_index(profile).unwrap_or(0)
}

/// Parse a ring-cap input: a positive integer, clamped to [`MAX_LIVE_CAP`],
/// falling back to the tool's default when the text is not one.
fn parse_live_cap(text: &str, default: usize) -> usize {
    text.trim()
        .parse::<usize>()
        .ok()
        .filter(|n| *n >= 1)
        .unwrap_or(default)
        .min(MAX_LIVE_CAP)
}

/// The live tables' trailing `node` column, present only on a cluster
/// connection (every master's stream is merged into one ring).
#[cfg(not(target_arch = "wasm32"))]
fn with_node_column(
    mut columns: Vec<sqlly_core::ResultColumn>,
    cluster: bool,
) -> Vec<sqlly_core::ResultColumn> {
    if cluster {
        columns.push(live_column("node", "string"));
    }
    columns
}

/// A `node` cell: the cluster master's `host:port`.
#[cfg(not(target_arch = "wasm32"))]
fn node_cell(node: Option<&str>) -> sqlly_core::CellValue {
    node.map(|n| sqlly_core::CellValue::String(n.to_string()))
        .unwrap_or(sqlly_core::CellValue::Null)
}

/// A `ResultColumn` for a live-tool table (no engine type names to mirror).
fn live_column(name: &str, data_type: &str) -> sqlly_core::ResultColumn {
    sqlly_core::ResultColumn {
        name: name.into(),
        data_type: data_type.into(),
        nullable: true,
    }
}

/// Lowercase hex of the first bytes of a binary payload, for the table's
/// single-line display cell.
fn hex_preview(bytes: &[u8]) -> String {
    const SHOWN: usize = 16;
    let mut out = String::with_capacity(SHOWN * 2 + 1);
    for byte in bytes.iter().take(SHOWN) {
        out.push_str(&format!("{byte:02x}"));
    }
    if bytes.len() > SHOWN {
        out.push('…');
    }
    out
}

/// The counters-and-state half of a live tab's status line. `shown` is how
/// many rows the table holds, so a ring bigger than [`LIVE_DISPLAY_CAP`]
/// (or a filter) never reads as if every buffered entry were on screen.
#[cfg(not(target_arch = "wasm32"))]
fn live_summary_parts<T>(
    capture: &sqlly_services::redis_live::LiveCapture<T>,
    noun: &str,
    shown: usize,
) -> String {
    let (received, dropped, buffered) = capture.counts();
    let noun = crate::localization::text(if noun == "commands" {
        "explorer.activity_monitor.live_noun_commands"
    } else {
        "explorer.activity_monitor.live_noun_messages"
    });
    let format_count = |key: &str, count: u64| {
        crate::localization::format(key, &[("count", &count.to_string()), ("noun", &noun)])
    };
    let mut parts = vec![format_count(
        "explorer.activity_monitor.live_buffered",
        buffered as u64,
    )];
    if shown < buffered {
        parts.push(live_shown_note(shown, buffered));
    }
    parts.extend([
        format_count("explorer.activity_monitor.live_received", received),
        format_count("explorer.activity_monitor.live_dropped", dropped),
    ]);
    if capture.is_paused() {
        parts.push(crate::localization::text(
            "explorer.activity_monitor.live_recording_paused",
        ));
    }
    match capture.status() {
        sqlly_services::redis_live::LiveStatus::Connecting => parts.push(
            crate::localization::text("explorer.activity_monitor.live_connecting"),
        ),
        sqlly_services::redis_live::LiveStatus::Live => parts.push(crate::localization::text(
            "explorer.activity_monitor.live_active",
        )),
        sqlly_services::redis_live::LiveStatus::Ended(reason) => {
            parts.push(crate::localization::message(reason))
        }
    }
    parts.join(" · ")
}

/// "showing …" for a live table that holds fewer rows than the ring: the
/// display cap, a filter, or both.
#[cfg(not(target_arch = "wasm32"))]
fn live_shown_note(shown: usize, buffered: usize) -> String {
    if shown >= LIVE_DISPLAY_CAP {
        crate::localization::format(
            "explorer.activity_monitor.live_shown_capped",
            &[("buffered", buffered), ("cap", LIVE_DISPLAY_CAP)],
        )
    } else {
        crate::localization::format(
            "explorer.activity_monitor.live_shown_filtered",
            &[("shown", shown), ("buffered", buffered)],
        )
    }
}

/// The first selected row (display index) and column, if any. Live grids
/// anchor on it across refreshes.
fn selection_anchor(selection: &sqlly_datatable::Selection) -> Option<(usize, Option<usize>)> {
    use sqlly_datatable::Selection;
    match selection {
        Selection::Cell(row, col) => Some((*row, Some(*col))),
        Selection::CellRange(r1, c1, r2, _) => Some(((*r1).min(*r2), Some(*c1))),
        Selection::Cells(cells) => cells.first().map(|(row, col)| (*row, Some(*col))),
        Selection::Row(row) => Some((*row, None)),
        Selection::RowRange(r1, r2) => Some(((*r1).min(*r2), None)),
        Selection::Rows(rows) => rows.first().map(|row| (*row, None)),
        Selection::None | Selection::Column(_) | Selection::Columns(_) => None,
    }
}

/// Where the entry shown at `old_row` sits in the new row order, by its
/// capture sequence number; `None` once it has been evicted or cleared.
#[cfg(not(target_arch = "wasm32"))]
fn reanchor_row(old_seqs: &[u64], new_seqs: &[u64], old_row: usize) -> Option<usize> {
    let seq = old_seqs.get(old_row)?;
    new_seqs.iter().position(|candidate| candidate == seq)
}

/// The selection after a live refresh: the same entry (by sequence number)
/// stays selected wherever it moved, collapsed to its first row; column
/// selections are positional and survive as-is.
#[cfg(not(target_arch = "wasm32"))]
fn remap_live_selection(
    selection: &sqlly_datatable::Selection,
    old_seqs: &[u64],
    new_seqs: &[u64],
) -> sqlly_datatable::Selection {
    use sqlly_datatable::Selection;
    if matches!(selection, Selection::Column(_) | Selection::Columns(_)) {
        return selection.clone();
    }
    let Some((row, col)) = selection_anchor(selection) else {
        return Selection::None;
    };
    match (reanchor_row(old_seqs, new_seqs, row), col) {
        (Some(row), Some(col)) => Selection::Cell(row, col),
        (Some(row), None) => Selection::Row(row),
        (None, _) => Selection::None,
    }
}

/// Longest text a live table cell shows. A Pub/Sub payload or a MONITOR
/// argument list can be megabytes; the capture keeps at most 64 KiB of each
/// (marked as truncated with the full size), the cell gets a prefix of
/// that, and Preview Payload opens everything that was kept.
#[cfg(not(target_arch = "wasm32"))]
const LIVE_CELL_CHARS: usize = 1_000;

/// A live cell's text: at most [`LIVE_CELL_CHARS`] characters (with `…`
/// when cut), line breaks collapsed for the grid's single-line cells.
#[cfg(not(target_arch = "wasm32"))]
fn live_cell_text(text: &str) -> String {
    let text = match text.char_indices().nth(LIVE_CELL_CHARS) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_string(),
    };
    if text.contains(['\n', '\r']) {
        text.replace("\r\n", " ").replace(['\n', '\r'], " ")
    } else {
        text
    }
}

/// The Pub/Sub grid's payload cell: a prefix of the kept text (or a hex
/// preview of kept bytes), with the full published size named whenever the
/// capture kept only part of it.
#[cfg(not(target_arch = "wasm32"))]
fn pubsub_payload_cell(message: &sqlly_services::redis_live::PubSubMessage) -> String {
    match &message.payload {
        sqlly_services::redis_live::PubSubPayload::Text(text) => {
            let cell = live_cell_text(text);
            if message.payload_truncated() {
                crate::localization::format(
                    "explorer.activity_monitor.payload_text_truncated",
                    &[("text", &cell), ("bytes", &message.payload_len.to_string())],
                )
            } else {
                cell
            }
        }
        sqlly_services::redis_live::PubSubPayload::Bytes(bytes) => {
            let key = if message.payload_truncated() {
                "explorer.activity_monitor.payload_bytes_truncated"
            } else {
                "explorer.activity_monitor.payload_bytes"
            };
            crate::localization::format(
                key,
                &[
                    ("hex", &hex_preview(bytes)),
                    ("bytes", &message.payload_len.to_string()),
                ],
            )
        }
    }
}

/// The notice Preview Payload shows for a truncated message; `None` when the
/// whole payload was kept.
#[cfg(not(target_arch = "wasm32"))]
fn pubsub_truncation_notice(message: &sqlly_services::redis_live::PubSubMessage) -> Option<String> {
    message.payload_truncated().then(|| {
        crate::localization::format(
            "explorer.activity_monitor.payload_truncation_notice",
            &[
                ("kept", &message.payload.len().to_string()),
                ("total", &message.payload_len.to_string()),
                (
                    "limit",
                    &(sqlly_services::redis_live::MAX_STORED_PAYLOAD_BYTES / 1024).to_string(),
                ),
            ],
        )
    })
}

/// Why the live tool on `tab` cannot run on `profile` (a relay-routed
/// connection); `None` for direct connections and for the DMV tabs.
#[cfg(not(target_arch = "wasm32"))]
fn live_refusal_for(profile: &sqlly_core::ConnectionProfile, tab: ActivityTab) -> Option<String> {
    let feature = match tab {
        ActivityTab::Profiler => {
            crate::localization::text("explorer.activity_monitor.feature_profiler")
        }
        ActivityTab::PubSub => {
            crate::localization::text("explorer.activity_monitor.feature_pubsub")
        }
        _ => return None,
    };
    matches!(
        profile.route,
        sqlly_core::connection::ConnectionRoute::ViaRelay { .. }
    )
    .then(|| {
        crate::localization::format(
            "explorer.activity_monitor.relay_refusal",
            &[("feature", &feature)],
        )
    })
}

/// Put `rows` into a live grid, keeping the user's place. The first call
/// builds the grid; later calls replace its rows IN PLACE through the
/// windowed-row API (the only whole-data replacement the grid offers), so
/// the entity — and with it the scroll position and selection — survives
/// every tick. The selection is re-anchored to the same entry by sequence
/// number, and while the user is scrolled away from the top the view
/// shifts by the rows inserted above it, so reading a busy stream is not a
/// moving target. Windowed mode turns off the grid's own sort and column
/// filters; the Profiler's filter fields do that job for live data.
#[cfg(not(target_arch = "wasm32"))]
fn apply_live_rows(
    grid: &mut Option<Entity<SqllyDataTable>>,
    old_seqs: &[u64],
    columns: Vec<sqlly_core::ResultColumn>,
    rows: Vec<Vec<sqlly_core::CellValue>>,
    new_seqs: &[u64],
    cx: &mut gpui::App,
) {
    let Some(existing) = grid.as_ref() else {
        let total = rows.len();
        let built = crate::tool_dialog::build_result_grid(&ToolQuerySet { columns, rows }, cx);
        if let Some(built) = &built {
            let state = built.read(cx).state.clone();
            state.update(cx, |state, _| {
                let rows = state.data.rows.clone();
                if let Err(error) = state.set_row_window(total, 0, rows) {
                    tracing::warn!(%error, "live grid: could not enter windowed mode");
                }
            });
        }
        *grid = built;
        return;
    };
    let grid_rows = match crate::grid_bridge::build_grid_data(None, &columns, &rows) {
        Ok(data) => data.rows,
        Err(error) => {
            tracing::warn!(%error, "live grid: rows did not convert");
            return;
        }
    };
    let state = existing.read(cx).state.clone();
    state.update(cx, |state, cx| {
        let selection = remap_live_selection(&state.selection, old_seqs, new_seqs);
        let offset = state.scroll_handle.offset();
        let old_y = f32::from(offset.y);
        let row_height = state.row_height;
        let old_top = if row_height > 0.0 {
            (old_y / row_height) as usize
        } else {
            0
        };
        if let Err(error) = state.set_row_window(grid_rows.len(), 0, grid_rows) {
            tracing::warn!(%error, "live grid: row replacement failed");
            return;
        }
        state.selection = selection;
        if old_y > 0.0
            && let Some(new_top) = reanchor_row(old_seqs, new_seqs, old_top)
        {
            let new_y = (old_y + (new_top as f32 - old_top as f32) * row_height).max(0.0);
            state.scroll_handle.set_offset(gpui::Point {
                x: offset.x,
                y: px(new_y),
            });
        }
        cx.notify();
    });
}

/// The source row behind a grid's selection. Selections are display rows;
/// live grids are windowed at offset 0, and `resident_row_for_display`
/// keeps this right even if that ever changes.
fn selected_source_row(grid: &Entity<SqllyDataTable>, cx: &gpui::App) -> Option<usize> {
    let state = grid.read(cx).state.read(cx);
    let (display_row, _) = selection_anchor(&state.selection)?;
    state.resident_row_for_display(display_row)
}

/// Which view the pane is showing. The first four are DMV-query tabs behind
/// every engine; `Profiler` and `Pub/Sub` are the Redis live tools, offered
/// only for Redis connections and driven by their own dedicated-connection
/// captures instead of `refresh()`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ActivityTab {
    Sessions,
    Blocking,
    ExpensiveQueries,
    Waits,
    Profiler,
    PubSub,
}

impl ActivityTab {
    pub fn all() -> [ActivityTab; 4] {
        [
            ActivityTab::Sessions,
            ActivityTab::Blocking,
            ActivityTab::ExpensiveQueries,
            ActivityTab::Waits,
        ]
    }

    /// The Redis live tools, in tab order.
    pub fn live() -> [ActivityTab; 2] {
        [ActivityTab::Profiler, ActivityTab::PubSub]
    }

    /// The tab strip for a connection: the DMV tabs for every engine, plus
    /// the live tools where their capability allows (every RESP-compatible
    /// engine — the same entry the command palette checks).
    pub fn tabs_for(db: sqlly_core::DatabaseType) -> Vec<ActivityTab> {
        let mut tabs: Vec<ActivityTab> = Self::all().into_iter().collect();
        tabs.extend(Self::live().into_iter().filter(|tab| tab.offered_for(db)));
        tabs
    }

    /// Whether this tab can be selected on `db`. DMV tabs always can; a live
    /// tool needs its capability, so the palette, the tab strip and a click
    /// can never disagree.
    pub fn offered_for(self, db: sqlly_core::DatabaseType) -> bool {
        let tool = match self {
            ActivityTab::Profiler => crate::capabilities::AdminTool::RedisProfiler,
            ActivityTab::PubSub => crate::capabilities::AdminTool::RedisPubSub,
            _ => return true,
        };
        crate::capabilities::admin_tool(db, tool).is_available()
    }

    /// A live-tool tab runs its own capture, not the DMV refresh loop.
    pub fn is_live(self) -> bool {
        matches!(self, ActivityTab::Profiler | ActivityTab::PubSub)
    }

    pub fn label(self) -> String {
        crate::localization::text(match self {
            ActivityTab::Sessions => "explorer.activity_monitor.tab_sessions",
            ActivityTab::Blocking => "explorer.activity_monitor.tab_blocking",
            ActivityTab::ExpensiveQueries => "explorer.activity_monitor.tab_expensive_queries",
            ActivityTab::Waits => "explorer.activity_monitor.tab_waits",
            ActivityTab::Profiler => "explorer.activity_monitor.tab_profiler",
            ActivityTab::PubSub => "explorer.activity_monitor.tab_pubsub",
        })
    }

    pub fn query_kind(self) -> &'static str {
        match self {
            ActivityTab::Sessions => "sessions",
            ActivityTab::Blocking => "blocking",
            ActivityTab::ExpensiveQueries => "expensive",
            ActivityTab::Waits => "waits",
            // Live tabs never reach the DMV query path; the arms exist so
            // the match stays exhaustive.
            ActivityTab::Profiler => "profiler",
            ActivityTab::PubSub => "pubsub",
        }
    }

    pub fn query(self, admin: &dyn AdminToolsService) -> String {
        admin.activity_query(self.query_kind())
    }

    /// The tab's query in the connected engine's own catalog language, or
    /// `None` when the engine has no live-activity concept (SQLite).
    pub fn query_for(
        self,
        admin: &dyn AdminToolsService,
        db: sqlly_core::DatabaseType,
    ) -> Option<String> {
        admin.activity_query_for(db, self.query_kind())
    }
}

impl EventEmitter<ToolDialogEvent> for ActivityMonitorPane {}

pub struct ActivityMonitorPane {
    context: ToolDialogContext,
    /// Database scope narrows the Sessions view to connections on the
    /// right-clicked database; server scope shows every session.
    scope: ToolScope,
    tab: ActivityTab,
    grid: Option<Entity<SqllyDataTable>>,
    /// Display columns + rows behind the filtered grid, kept so selected-row
    /// actions use the same row the user sees.
    columns: Vec<sqlly_core::ResultColumn>,
    rows: Vec<Vec<sqlly_core::CellValue>>,
    source_columns: Vec<sqlly_core::ResultColumn>,
    source_rows: Vec<Vec<sqlly_core::CellValue>>,
    row_count: usize,
    triage_filter: ActivityTriageFilter,
    /// Whether the decision model can rank rows at all — provider configured
    /// AND row data allowed. Sampled once at pane construction (the
    /// preferences pane is modal to this dialog anyway) so render never hits
    /// the keyring.
    attention_offered: bool,
    /// The per-pane "Rank by attention" switch. Off by default and
    /// deliberately not persisted: telemetry rows contain other users' SQL
    /// text, so every launch starts with nothing sent.
    attention_enabled: bool,
    /// One act/watch/ignore call per source row, from the last triage pass.
    attention_verdicts: Option<Vec<(crate::admin_triage::Attention, f32)>>,
    attention_in_flight: bool,
    /// When the last triage pass started — triage runs at most once per 5 s
    /// however often the pane refreshes.
    attention_last: Option<std::time::Instant>,
    /// A failed pass reports here (status line) and falls back to All.
    attention_error: Option<String>,
    pending_termination: Option<TerminationReview>,
    /// Redis slow-log threshold for the Expensive tab: `None` not read yet,
    /// `Some(None)` the read failed or was refused, `Some(Some(µs))` the
    /// configured `slowlog-log-slower-than`.
    slowlog_threshold: Option<Option<i64>>,
    /// Whether a threshold read is in flight (one at a time).
    slowlog_loading: bool,
    slowlog_threshold_input: Entity<InputState>,
    /// The CONFIG statement shown for review before it runs.
    slowlog_review: Option<SlowlogReview>,
    /// Outcome of the last reviewed CONFIG statement (or the input error).
    slowlog_message: Option<(bool, String)>,
    /// Why the threshold read failed (permission vs a disabled or renamed
    /// CONFIG), shown instead of a bare "unknown".
    slowlog_threshold_error: Option<String>,
    /// Why CONFIG SET / CONFIG REWRITE may not run here (a locked
    /// production connection). These run through `tool_dialog::run_query`,
    /// which skips the editor's run gate, so `AppView` hands the guard's
    /// verdict in via [`Self::set_slowlog_write_block`].
    slowlog_write_block: Option<String>,
    slowlog_production_check: Option<crate::tool_dialog::ProductionCheck>,
    /// `notify-keyspace-events` for the Pub/Sub tab's keyspace strip:
    /// `None` not read yet, `Some(None)` the read failed or was refused,
    /// `Some(Some(flags))` the configured classes (empty = off).
    keyspace_flags: Option<Option<String>>,
    /// Whether a keyspace-flags read is in flight (one at a time).
    keyspace_loading: bool,
    /// Why the keyspace-flags read failed, in the user's terms.
    keyspace_error: Option<String>,
    error: Option<String>,
    loading: bool,
    /// One refresh in flight at a time; a stale completion is discarded when
    /// its token no longer matches.
    refresh_generation: u64,
    refresh_in_flight: bool,
    last_refreshed: Option<std::time::Instant>,
    /// Auto-refresh paused by the user; manual Refresh still works.
    paused: bool,
    /// Seconds between automatic refreshes; the toolbar cycles the choices.
    interval_secs: u64,
    /// Auto-refresh ticker. Dropping the pane drops the task — a closed
    /// monitor can never keep polling in the background.
    _ticker: Task<()>,
    /// The previous Waits refresh's counters, so each refresh reports the
    /// change since the one before. Cleared on tab switches so a return to
    /// the Waits tab starts sampling fresh.
    wait_snapshot: Option<WaitSnapshot>,
    // --- Redis live tools (the Profiler and Pub/Sub tabs) ---
    /// MONITOR capture behind the Profiler tab, on its own connection.
    /// Stopped when the tab is left; dropped (and thereby closed) with the
    /// pane. Native only — the live lane needs real sockets.
    #[cfg(not(target_arch = "wasm32"))]
    profiler: Option<sqlly_services::redis_live::ProfilerCapture>,
    /// Subscription behind the Pub/Sub tab; same lifecycle as the profiler.
    #[cfg(not(target_arch = "wasm32"))]
    pubsub: Option<sqlly_services::redis_live::PubSubCapture>,
    profiler_grid: Option<Entity<SqllyDataTable>>,
    /// Capture sequence numbers of the profiler grid's rows, newest first:
    /// the identity that keeps a selection on the same command across
    /// refreshes.
    profiler_seqs: Vec<u64>,
    /// (change stamp, command filter, client filter, substring) the
    /// profiler grid was last synced for; syncs happen only when it moves.
    profiler_built_for: Option<((u64, usize), String, String, String)>,
    pubsub_grid: Option<Entity<SqllyDataTable>>,
    /// Capture sequence numbers of the Pub/Sub grid's rows, newest first.
    pubsub_seqs: Vec<u64>,
    /// Full payloads parallel to the Pub/Sub grid's rows, for Preview.
    pubsub_payloads: Vec<LivePayload>,
    /// The capture change stamp the Pub/Sub grid was last synced for.
    pubsub_built_for: Option<(u64, usize)>,
    filter_command_input: Entity<InputState>,
    filter_client_input: Entity<InputState>,
    filter_text_input: Entity<InputState>,
    profiler_cap_input: Entity<InputState>,
    /// Seconds before the Profiler stops itself (the hard time box).
    profiler_time_box_input: Entity<InputState>,
    channels_input: Entity<InputState>,
    pubsub_cap_input: Entity<InputState>,
    /// One-off live-tab messages (empty channel list, unpreviewable payload).
    live_notice: Option<String>,
    /// 500 ms sync ticker, alive only while a live tab is active.
    _live_ticker: Option<Task<()>>,
    _input_subscriptions: Vec<Subscription>,
    focus_handle: FocusHandle,
    frame: DialogFrame,
    /// True once popped out into its own OS window: the window boundary is
    /// the dialog boundary and close tears the window down.
    windowed: bool,
}

/// The selected row's SQL text, from the first column whose name mentions
/// query/sql text and holds a non-empty string. Pure so it's testable.
fn sql_text_in_row(
    columns: &[sqlly_core::ResultColumn],
    row: &[sqlly_core::CellValue],
) -> Option<String> {
    for (idx, col) in columns.iter().enumerate() {
        let name = col.name.to_ascii_lowercase();
        if !(name.contains("query") || name.contains("sql")) || name.contains("handle") {
            continue;
        }
        if let Some(sqlly_core::CellValue::String(s)) = row.get(idx)
            && !s.trim().is_empty()
        {
            return Some(s.clone());
        }
    }
    None
}

fn column_index(columns: &[sqlly_core::ResultColumn], aliases: &[&str]) -> Option<usize> {
    columns.iter().position(|column| {
        let name = column.name.to_ascii_lowercase();
        aliases.iter().any(|alias| name == *alias)
    })
}

fn cell_i64(value: Option<&sqlly_core::CellValue>) -> Option<i64> {
    match value? {
        sqlly_core::CellValue::I64(value) => Some(*value),
        sqlly_core::CellValue::F64(value) if value.is_finite() => Some(*value as i64),
        sqlly_core::CellValue::String(value) => value.trim().parse().ok(),
        _ => None,
    }
}

fn cell_text(value: Option<&sqlly_core::CellValue>) -> Option<&str> {
    match value? {
        sqlly_core::CellValue::String(value) => Some(value.as_str()),
        _ => None,
    }
}

fn session_column(tab: ActivityTab, columns: &[sqlly_core::ResultColumn]) -> Option<usize> {
    match tab {
        ActivityTab::Sessions => column_index(columns, &["session_id", "pid", "id"]),
        // A blocking row describes an edge. The blocker is the intervention
        // target; the confirmation copy names it explicitly before scripting.
        ActivityTab::Blocking => column_index(columns, &["blocking_session_id", "blocking_pid"]),
        ActivityTab::ExpensiveQueries | ActivityTab::Waits => None,
        ActivityTab::Profiler | ActivityTab::PubSub => None,
    }
}

fn blocked_column(tab: ActivityTab, columns: &[sqlly_core::ResultColumn]) -> Option<usize> {
    match tab {
        ActivityTab::Sessions => session_column(tab, columns),
        ActivityTab::Blocking => column_index(
            columns,
            &["blocked_session_id", "waiting_session_id", "waiting_pid"],
        ),
        ActivityTab::ExpensiveQueries | ActivityTab::Waits => None,
        ActivityTab::Profiler | ActivityTab::PubSub => None,
    }
}

fn blocker_column(columns: &[sqlly_core::ResultColumn]) -> Option<usize> {
    column_index(columns, &["blocking_session_id", "blocking_pid"])
}

fn elapsed_seconds(
    columns: &[sqlly_core::ResultColumn],
    row: &[sqlly_core::CellValue],
) -> Option<i64> {
    if let Some(index) = column_index(columns, &["seconds_running", "elapsed_seconds"]) {
        return cell_i64(row.get(index));
    }
    let index = column_index(columns, &["total_elapsed_time", "elapsed_ms"])?;
    cell_i64(row.get(index)).map(|milliseconds| milliseconds / 1_000)
}

fn blocking_positions(
    tab: ActivityTab,
    columns: &[sqlly_core::ResultColumn],
    rows: &[Vec<sqlly_core::CellValue>],
) -> (HashMap<i64, BlockingPosition>, HashMap<i64, usize>) {
    let Some(blocked_idx) = blocked_column(tab, columns) else {
        return (HashMap::new(), HashMap::new());
    };
    let Some(blocker_idx) = blocker_column(columns) else {
        return (HashMap::new(), HashMap::new());
    };
    let mut parent = HashMap::new();
    let mut child_count = HashMap::new();
    for row in rows {
        let Some(blocked) = cell_i64(row.get(blocked_idx)).filter(|id| *id > 0) else {
            continue;
        };
        let Some(blocker) = cell_i64(row.get(blocker_idx)).filter(|id| *id > 0 && *id != blocked)
        else {
            continue;
        };
        parent.insert(blocked, blocker);
        *child_count.entry(blocker).or_insert(0) += 1;
    }

    let mut nodes: HashSet<i64> = parent.keys().copied().collect();
    nodes.extend(parent.values().copied());
    let positions = nodes
        .into_iter()
        .map(|start| {
            let mut current = start;
            let mut path = Vec::new();
            let mut seen = HashSet::new();
            while let Some(next) = parent.get(&current).copied() {
                if !seen.insert(current) {
                    let root = path.iter().copied().min().or(Some(current));
                    return (
                        start,
                        BlockingPosition {
                            root,
                            depth: path.len(),
                            cycle: true,
                        },
                    );
                }
                path.push(current);
                current = next;
            }
            (
                start,
                BlockingPosition {
                    root: Some(current),
                    depth: path.len(),
                    cycle: false,
                },
            )
        })
        .collect();
    (positions, child_count)
}

fn triaged_set(
    tab: ActivityTab,
    filter: ActivityTriageFilter,
    columns: &[sqlly_core::ResultColumn],
    rows: &[Vec<sqlly_core::CellValue>],
) -> ToolQuerySet {
    if matches!(
        tab,
        ActivityTab::ExpensiveQueries
            | ActivityTab::Waits
            | ActivityTab::Profiler
            | ActivityTab::PubSub
    ) {
        return ToolQuerySet {
            columns: columns.to_vec(),
            rows: rows.to_vec(),
        };
    }
    let (positions, child_count) = blocking_positions(tab, columns, rows);
    let node_idx = blocked_column(tab, columns);
    let action_idx = session_column(tab, columns);
    let status_idx = column_index(columns, &["status", "request_status", "state"]);
    let mut output_columns = columns.to_vec();
    output_columns.extend([
        sqlly_core::ResultColumn {
            name: "triage".into(),
            data_type: "text".into(),
            nullable: false,
        },
        sqlly_core::ResultColumn {
            name: "blocking_root_session_id".into(),
            data_type: "bigint".into(),
            nullable: true,
        },
        sqlly_core::ResultColumn {
            name: "blocking_depth".into(),
            data_type: "bigint".into(),
            nullable: false,
        },
        sqlly_core::ResultColumn {
            name: "running_seconds".into(),
            data_type: "bigint".into(),
            nullable: true,
        },
    ]);

    let output_rows = rows
        .iter()
        .filter_map(|row| {
            let node = node_idx.and_then(|index| cell_i64(row.get(index)));
            let position = node
                .and_then(|id| positions.get(&id).copied())
                .unwrap_or_default();
            let is_blocked = position.depth > 0;
            let action_node = action_idx.and_then(|index| cell_i64(row.get(index)));
            let is_blocker = action_node
                .and_then(|id| child_count.get(&id))
                .copied()
                .unwrap_or(0)
                > 0;
            let running_seconds = elapsed_seconds(columns, row);
            let is_long_running =
                running_seconds.is_some_and(|seconds| seconds >= LONG_RUNNING_SECS);
            let status = status_idx
                .and_then(|index| cell_text(row.get(index)))
                .unwrap_or_default()
                .to_ascii_lowercase();
            let is_idle_transaction = status.contains("idle in transaction");
            let include = match filter {
                // Attention keeps every row here; the decision-model
                // ordering and Ignore-hiding happen in `attention_ordered`
                // so row indices still line up with the source set.
                ActivityTriageFilter::All | ActivityTriageFilter::Attention => true,
                ActivityTriageFilter::Blockers => is_blocker,
                ActivityTriageFilter::Blocked => is_blocked,
                ActivityTriageFilter::LongRunning => is_long_running,
                ActivityTriageFilter::IdleInTransaction => is_idle_transaction,
            };
            if !include {
                return None;
            }

            let triage = crate::localization::text(
                match (is_blocker, is_blocked, is_long_running, position.cycle) {
                    (_, _, _, true) => "explorer.activity_monitor.triage_blocking_cycle",
                    (true, true, _, false) => {
                        "explorer.activity_monitor.triage_blocking_and_blocked"
                    }
                    (true, false, _, false) => "explorer.activity_monitor.triage_root_blocker",
                    (false, true, _, false) => "explorer.activity_monitor.triage_blocked",
                    (false, false, true, false) => "explorer.activity_monitor.triage_long_running",
                    _ if is_idle_transaction => {
                        "explorer.activity_monitor.triage_idle_in_transaction"
                    }
                    _ => "explorer.activity_monitor.triage_normal",
                },
            );
            let mut derived = row.clone();
            derived.push(sqlly_core::CellValue::String(triage));
            derived.push(
                position
                    .root
                    .map(sqlly_core::CellValue::I64)
                    .unwrap_or(sqlly_core::CellValue::Null),
            );
            derived.push(sqlly_core::CellValue::I64(position.depth as i64));
            derived.push(
                running_seconds
                    .map(sqlly_core::CellValue::I64)
                    .unwrap_or(sqlly_core::CellValue::Null),
            );
            Some(derived)
        })
        .collect();
    ToolQuerySet {
        columns: output_columns,
        rows: output_rows,
    }
}

/// Order a triaged set by the decision model's call: `Act` rows first,
/// `Watch` after (higher confidence first within each group), `Ignore`
/// hidden, with a readable `attention` column appended. A row the model was
/// never asked about (beyond the request cap) counts as `Watch` at zero
/// confidence, so ranking can never hide a row by omission. Pure so the
/// ordering is testable without a provider.
fn attention_ordered(
    set: ToolQuerySet,
    verdicts: &[(crate::admin_triage::Attention, f32)],
) -> ToolQuerySet {
    use crate::admin_triage::Attention;
    let mut columns = set.columns;
    columns.push(sqlly_core::ResultColumn {
        name: "attention".into(),
        data_type: "text".into(),
        nullable: false,
    });
    let mut ranked: Vec<(Attention, f32, Vec<sqlly_core::CellValue>)> = set
        .rows
        .into_iter()
        .enumerate()
        .filter_map(|(index, mut row)| {
            let (attention, confidence) = verdicts
                .get(index)
                .copied()
                .unwrap_or((Attention::Watch, 0.0));
            if attention == Attention::Ignore {
                return None;
            }
            row.push(sqlly_core::CellValue::String(attention.label()));
            Some((attention, confidence, row))
        })
        .collect();
    ranked.sort_by(|a, b| {
        a.0.rank()
            .cmp(&b.0.rank())
            .then(b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal))
    });
    ToolQuerySet {
        columns,
        rows: ranked.into_iter().map(|(_, _, row)| row).collect(),
    }
}

/// The numeric wait counters from one refresh, keyed by wait type, so the
/// next refresh can report what changed in between.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct WaitSnapshot {
    /// Names of the delta-eligible columns the values were captured from;
    /// a mismatch (query shape changed) invalidates the snapshot.
    signature: Vec<String>,
    values: HashMap<String, Vec<Option<i64>>>,
}

/// Cumulative wait statistics only say where time went since the server
/// started; triage needs where it is going right now. Keyed on the wait-type
/// column, this appends a `<column>_delta` column for every numeric column,
/// holding the increase since the previous refresh — Null on the first
/// sample, for wait types not seen before, and after a counter reset
/// (current below previous, e.g. a server restart or stats clear). Pure so
/// the sampling math is testable without a live server.
fn wait_delta_set(
    columns: &[sqlly_core::ResultColumn],
    rows: &[Vec<sqlly_core::CellValue>],
    previous: Option<&WaitSnapshot>,
) -> (ToolQuerySet, WaitSnapshot) {
    let passthrough = || ToolQuerySet {
        columns: columns.to_vec(),
        rows: rows.to_vec(),
    };
    let Some(key_idx) = column_index(columns, &["wait_type", "wait_event_type", "event_name"])
    else {
        return (passthrough(), WaitSnapshot::default());
    };
    // A column takes part in delta sampling when it is not the key and every
    // non-null cell reads as an integer (data-driven: dialects disagree on
    // declared type names).
    let numeric_indices: Vec<usize> = (0..columns.len())
        .filter(|&idx| idx != key_idx)
        .filter(|&idx| {
            let mut any = false;
            for row in rows {
                match row.get(idx) {
                    Some(sqlly_core::CellValue::Null) | None => {}
                    value => match cell_i64(value) {
                        Some(_) => any = true,
                        None => return false,
                    },
                }
            }
            any
        })
        .collect();
    if numeric_indices.is_empty() {
        return (passthrough(), WaitSnapshot::default());
    }
    let signature: Vec<String> = numeric_indices
        .iter()
        .map(|&idx| columns[idx].name.clone())
        .collect();
    let previous = previous.filter(|snapshot| snapshot.signature == signature);

    let mut output_columns = columns.to_vec();
    for &idx in &numeric_indices {
        output_columns.push(sqlly_core::ResultColumn {
            name: format!("{}_delta", columns[idx].name),
            data_type: "bigint".into(),
            nullable: true,
        });
    }
    let mut snapshot = WaitSnapshot {
        signature,
        values: HashMap::new(),
    };
    let output_rows = rows
        .iter()
        .map(|row| {
            let key = cell_text(row.get(key_idx)).unwrap_or_default().to_string();
            let current: Vec<Option<i64>> = numeric_indices
                .iter()
                .map(|&idx| cell_i64(row.get(idx)))
                .collect();
            let mut derived = row.clone();
            let before = previous.and_then(|snapshot| snapshot.values.get(&key));
            for (offset, value) in current.iter().enumerate() {
                let delta = match (before.and_then(|values| values.get(offset).copied()), value) {
                    (Some(Some(before)), Some(now)) if now >= &before => Some(now - before),
                    _ => None,
                };
                derived.push(
                    delta
                        .map(sqlly_core::CellValue::I64)
                        .unwrap_or(sqlly_core::CellValue::Null),
                );
            }
            snapshot.values.insert(key, current);
            derived
        })
        .collect();
    (
        ToolQuerySet {
            columns: output_columns,
            rows: output_rows,
        },
        snapshot,
    )
}

fn permission_guidance(db: sqlly_core::DatabaseType) -> String {
    use sqlly_core::ConnectionKind;
    crate::localization::text(match db.connection_kind() {
        ConnectionKind::Tds => "explorer.activity_monitor.permission_sql_server",
        ConnectionKind::PostgresWire => "explorer.activity_monitor.permission_postgres",
        ConnectionKind::MySqlWire => "explorer.activity_monitor.permission_mysql",
        ConnectionKind::ClickHouseHttp => "explorer.activity_monitor.permission_clickhouse",
        ConnectionKind::Resp => "explorer.activity_monitor.permission_redis",
        _ => "explorer.activity_monitor.permission_unsupported",
    })
}

impl ActivityMonitorPane {
    pub fn new(
        context: ToolDialogContext,
        scope: ToolScope,
        initial_tab: ActivityTab,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
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
                    // Refresh (unless paused) and pick up the current
                    // interval for the next sleep in the same update.
                    match weak.update(&mut cx, |this: &mut Self, cx| {
                        if !this.paused && !this.tab.is_live() {
                            this.refresh(cx);
                        } else {
                            // Keep the refresh-age readout live while paused.
                            cx.notify();
                        }
                        this.interval_secs
                    }) {
                        Ok(interval) => wait = interval,
                        // Pane is gone; stop ticking.
                        Err(_) => break,
                    }
                }
            })
        };
        let filter_command_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "explorer.activity_monitor.filter_command_placeholder",
            ))
        });
        let filter_client_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "explorer.activity_monitor.filter_client_placeholder",
            ))
        });
        let filter_text_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "explorer.activity_monitor.filter_text_placeholder",
            ))
        });
        let profiler_cap_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("10000")
                .default_value("10000")
        });
        // Matches redis_live::DEFAULT_PROFILER_TIME_BOX_SECS (native-only
        // crate module, so the literal keeps the wasm build compiling).
        let profiler_time_box_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("300")
                .default_value("300")
        });
        let slowlog_threshold_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "explorer.activity_monitor.slowlog_threshold_placeholder",
            ))
        });
        let channels_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "explorer.activity_monitor.channels_placeholder",
            ))
        });
        let pubsub_cap_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("1000")
                .default_value("1000")
        });
        // Filter edits invalidate the profiler grid's stamp so the next
        // 500 ms tick rebuilds it; they never touch the capture itself.
        let input_subscriptions = [
            &filter_command_input,
            &filter_client_input,
            &filter_text_input,
        ]
        .into_iter()
        .map(|input| {
            cx.subscribe_in(
                input,
                window,
                |this: &mut Self, _, event: &InputEvent, _, cx| {
                    if let InputEvent::Change = event {
                        this.profiler_built_for = None;
                        cx.notify();
                    }
                },
            )
        })
        .collect();
        // Live tabs only make sense on the engines they capture from.
        let initial_tab = if initial_tab.offered_for(context.profile.database_type) {
            initial_tab
        } else {
            ActivityTab::Sessions
        };
        let mut this = Self {
            context,
            scope,
            tab: initial_tab,
            grid: None,
            columns: Vec::new(),
            rows: Vec::new(),
            source_columns: Vec::new(),
            source_rows: Vec::new(),
            row_count: 0,
            triage_filter: ActivityTriageFilter::All,
            attention_offered: crate::decision_ai::load_decision_ai_config().allow_row_data
                && crate::decision_ai::decision_provider().is_some(),
            attention_enabled: false,
            attention_verdicts: None,
            attention_in_flight: false,
            attention_last: None,
            attention_error: None,
            pending_termination: None,
            slowlog_threshold: None,
            slowlog_loading: false,
            slowlog_threshold_input,
            slowlog_review: None,
            slowlog_message: None,
            slowlog_threshold_error: None,
            slowlog_write_block: None,
            slowlog_production_check: None,
            keyspace_flags: None,
            keyspace_loading: false,
            keyspace_error: None,
            error: None,
            loading: !initial_tab.is_live(),
            refresh_generation: 0,
            refresh_in_flight: false,
            last_refreshed: None,
            paused: false,
            interval_secs: AUTO_REFRESH_SECS,
            _ticker: ticker,
            wait_snapshot: None,
            #[cfg(not(target_arch = "wasm32"))]
            profiler: None,
            #[cfg(not(target_arch = "wasm32"))]
            pubsub: None,
            profiler_grid: None,
            profiler_seqs: Vec::new(),
            profiler_built_for: None,
            pubsub_grid: None,
            pubsub_seqs: Vec::new(),
            pubsub_payloads: Vec::new(),
            pubsub_built_for: None,
            filter_command_input,
            filter_client_input,
            filter_text_input,
            profiler_cap_input,
            profiler_time_box_input,
            channels_input,
            pubsub_cap_input,
            live_notice: None,
            _live_ticker: None,
            _input_subscriptions: input_subscriptions,
            focus_handle: cx.focus_handle(),
            frame: DialogFrame::new(Theme::scaled(900.0), Theme::scaled(560.0)),
            windowed: false,
        };
        if initial_tab.is_live() {
            this.start_live_ticker(cx);
            if initial_tab == ActivityTab::PubSub {
                this.load_keyspace_flags(cx);
            }
        } else {
            this.refresh(cx);
        }
        this
    }

    /// The selected row's SQL text, when the current tab carries one.
    fn selected_sql(&self, cx: &gpui::App) -> Option<String> {
        let grid = self.grid.as_ref()?;
        let row = crate::tool_dialog::selected_source_row(grid, cx)?;
        sql_text_in_row(&self.columns, self.rows.get(row)?)
    }

    fn selected_session_id(&self, cx: &gpui::App) -> Option<i32> {
        let grid = self.grid.as_ref()?;
        let row = crate::tool_dialog::selected_source_row(grid, cx)?;
        let column = session_column(self.tab, &self.columns)?;
        let value = cell_i64(self.rows.get(row)?.get(column)).filter(|id| *id > 0)?;
        i32::try_from(value).ok()
    }

    fn selected_diagnostic_script(&self, cx: &gpui::App) -> Option<String> {
        self.context
            .services
            .admin_tools
            .activity_diagnostic_sql_for(
                self.context.profile.database_type,
                self.selected_session_id(cx)?,
            )
    }

    fn selected_termination_review(&self, cx: &gpui::App) -> Option<TerminationReview> {
        let session_id = self.selected_session_id(cx)?;
        let command = self
            .context
            .services
            .admin_tools
            .activity_kill_sql_for(self.context.profile.database_type, session_id)?;
        Some(TerminationReview {
            session_id,
            script: format!(
                "-- Review before executing: this terminates session {session_id} and may roll back its open transaction.\n{command};"
            ),
        })
    }

    fn rebuild_grid(&mut self, cx: &mut gpui::Context<Self>) {
        let mut set = triaged_set(
            self.tab,
            self.triage_filter,
            &self.source_columns,
            &self.source_rows,
        );
        if self.triage_filter == ActivityTriageFilter::Attention
            && let Some(verdicts) = &self.attention_verdicts
        {
            set = attention_ordered(set, verdicts);
        }
        self.grid = crate::tool_dialog::build_result_grid(&set, cx);
        self.columns = set.columns;
        self.rows = set.rows;
        self.pending_termination = None;
    }

    fn cycle_triage_filter(&mut self, cx: &mut gpui::Context<Self>) {
        self.triage_filter = self
            .triage_filter
            .next(self.attention_offered && self.attention_enabled);
        self.maybe_spawn_attention(cx);
        self.rebuild_grid(cx);
        cx.notify();
    }

    fn toggle_attention(&mut self, cx: &mut gpui::Context<Self>) {
        self.attention_enabled = !self.attention_enabled;
        if self.attention_enabled {
            self.triage_filter = ActivityTriageFilter::Attention;
            self.maybe_spawn_attention(cx);
        } else {
            if self.triage_filter == ActivityTriageFilter::Attention {
                self.triage_filter = ActivityTriageFilter::All;
            }
            self.attention_verdicts = None;
            self.attention_error = None;
        }
        self.rebuild_grid(cx);
        cx.notify();
    }

    /// Run one triage pass over the current source rows, at most once per
    /// 5 s, one in flight at a time. A missing provider or a failed pass
    /// falls back to All with a status-line note — the pane never blocks on
    /// the model.
    fn maybe_spawn_attention(&mut self, cx: &mut gpui::Context<Self>) {
        if self.triage_filter != ActivityTriageFilter::Attention
            || !self.attention_enabled
            || self.attention_in_flight
            || self.source_rows.is_empty()
        {
            return;
        }
        if self
            .attention_last
            .is_some_and(|at| at.elapsed().as_secs() < 5)
        {
            return;
        }
        let Some(provider) = crate::decision_ai::decision_provider() else {
            self.attention_error = Some(crate::localization::text(
                "explorer.activity_monitor.attention_unavailable",
            ));
            self.triage_filter = ActivityTriageFilter::All;
            return;
        };
        self.attention_in_flight = true;
        self.attention_last = Some(std::time::Instant::now());
        let context = format!(
            "Live database server telemetry: the {} view of an activity monitor",
            self.tab.label()
        );
        let columns = self.source_columns.clone();
        let rows = self.source_rows.clone();
        let generation = self.refresh_generation;
        cx.spawn(async move |this, cx| {
            let verdicts = crate::admin_triage::triage_rows(
                provider.as_ref(),
                &context,
                &columns,
                &rows,
                crate::admin_triage::TRIAGE_MAX_ROWS,
            )
            .await;
            let _ = this.update(cx, |this, cx| {
                this.attention_in_flight = false;
                if this.refresh_generation != generation {
                    return;
                }
                match verdicts {
                    Some(verdicts) => {
                        this.attention_error = None;
                        this.attention_verdicts = Some(verdicts);
                    }
                    None => {
                        this.attention_error = Some(crate::localization::text(
                            "explorer.activity_monitor.attention_failed",
                        ));
                        this.attention_verdicts = None;
                        if this.triage_filter == ActivityTriageFilter::Attention {
                            this.triage_filter = ActivityTriageFilter::All;
                        }
                    }
                }
                this.rebuild_grid(cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn switch_tab(&mut self, tab: ActivityTab, cx: &mut gpui::Context<Self>) {
        if self.tab == tab {
            return;
        }
        // The live tools exist only for RESP engines; nothing else can
        // select them.
        if !tab.offered_for(self.context.profile.database_type) {
            return;
        }
        // Leaving a live tab stops its capture — a MONITOR or subscriber
        // connection is never held open in the background. The buffered
        // entries stay for reading until the next Start.
        #[cfg(not(target_arch = "wasm32"))]
        {
            match self.tab {
                ActivityTab::Profiler => self.stop_profiler_capture(),
                ActivityTab::PubSub => self.stop_pubsub_capture(),
                _ => {}
            }
        }
        self.tab = tab;
        self.grid = None;
        self.row_count = 0;
        self.triage_filter = ActivityTriageFilter::All;
        // The new tab's rows have a different shape; stale verdicts must
        // not rank them.
        self.attention_verdicts = None;
        self.attention_error = None;
        self.attention_last = None;
        self.pending_termination = None;
        self.slowlog_review = None;
        self.slowlog_message = None;
        self.error = None;
        self.wait_snapshot = None;
        self.live_notice = None;
        // Invalidate any in-flight refresh for the previous tab.
        self.refresh_generation += 1;
        self.refresh_in_flight = false;
        if tab.is_live() {
            self.loading = false;
            self.start_live_ticker(cx);
            if tab == ActivityTab::PubSub && self.keyspace_flags.is_none() {
                self.load_keyspace_flags(cx);
            }
        } else {
            self._live_ticker = None;
            self.loading = true;
            self.refresh(cx);
        }
    }

    /// Open the pane straight onto a tab (the command-palette Redis live
    /// tools land here). Same rules as clicking the tab.
    pub fn show_tab(&mut self, tab: ActivityTab, cx: &mut gpui::Context<Self>) {
        self.switch_tab(tab, cx);
    }

    /// The 500 ms ticker that syncs live grids from their captures. Dropped
    /// when the tab or the pane goes away — it can never outlive either.
    fn start_live_ticker(&mut self, cx: &mut gpui::Context<Self>) {
        if self._live_ticker.is_some() {
            return;
        }
        let weak = cx.weak_entity();
        let async_cx = cx.to_async();
        self._live_ticker = Some(cx.spawn(async move |_this, _cx| {
            loop {
                async_cx
                    .background_executor()
                    .timer(std::time::Duration::from_millis(LIVE_TICK_MILLIS))
                    .await;
                let mut cx = async_cx.clone();
                match weak.update(&mut cx, |this: &mut Self, cx| {
                    this.sync_live_grids(cx);
                    cx.notify();
                }) {
                    Ok(()) => {}
                    // Pane is gone; stop ticking.
                    Err(_) => break,
                }
            }
        }));
    }

    fn input_value(state: &Entity<InputState>, cx: &gpui::App) -> String {
        state.read(cx).value().to_string()
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn profiler_filter(&self, cx: &gpui::App) -> sqlly_services::redis_live::ProfilerFilter {
        sqlly_services::redis_live::ProfilerFilter {
            command: Self::input_value(&self.filter_command_input, cx),
            client: Self::input_value(&self.filter_client_input, cx),
            substring: Self::input_value(&self.filter_text_input, cx),
        }
    }

    /// Why the live tool on `tab` cannot run on this connection. The live
    /// lane dials the server itself with locally resolved credentials, so a
    /// relay-routed connection — reached only through its bridge — is
    /// refused rather than silently dialed direct.
    #[cfg(not(target_arch = "wasm32"))]
    fn live_refusal(&self, tab: ActivityTab) -> Option<String> {
        live_refusal_for(&self.context.profile, tab)
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn start_profiler_capture(&mut self, cx: &mut gpui::Context<Self>) {
        self.stop_profiler_capture();
        if let Some(refusal) = self.live_refusal(ActivityTab::Profiler) {
            self.live_notice = Some(refusal);
            cx.notify();
            return;
        }
        let cap = parse_live_cap(
            &Self::input_value(&self.profiler_cap_input, cx),
            sqlly_services::redis_live::DEFAULT_PROFILER_CAP,
        );
        let time_box = std::time::Duration::from_secs(parse_time_box_secs(&Self::input_value(
            &self.profiler_time_box_input,
            cx,
        )));
        #[cfg(test)]
        crate::ensure_test_tokio_runtime();
        self.profiler = Some(sqlly_services::redis_live::start_profiler(
            &crate::tokio_handle(),
            self.context.profile.clone(),
            cap,
            Some(time_box),
        ));
        self.profiler_seqs = Vec::new();
        self.profiler_grid = None;
        self.profiler_built_for = None;
        self.live_notice = None;
        cx.notify();
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn stop_profiler_capture(&mut self) {
        if let Some(capture) = &self.profiler {
            capture.stop();
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn start_pubsub_capture(&mut self, cx: &mut gpui::Context<Self>) {
        self.stop_pubsub_capture();
        if let Some(refusal) = self.live_refusal(ActivityTab::PubSub) {
            self.live_notice = Some(refusal);
            cx.notify();
            return;
        }
        let targets = sqlly_services::redis_live::parse_subscribe_targets(&Self::input_value(
            &self.channels_input,
            cx,
        ));
        if targets.is_empty() {
            self.pubsub = None;
            self.live_notice = Some(crate::localization::text(
                "explorer.activity_monitor.channels_required",
            ));
            cx.notify();
            return;
        }
        let cap = parse_live_cap(
            &Self::input_value(&self.pubsub_cap_input, cx),
            sqlly_services::redis_live::DEFAULT_PUBSUB_CAP,
        );
        #[cfg(test)]
        crate::ensure_test_tokio_runtime();
        match sqlly_services::redis_live::start_pubsub(
            &crate::tokio_handle(),
            self.context.profile.clone(),
            targets,
            cap,
        ) {
            Ok(capture) => {
                self.pubsub = Some(capture);
                self.pubsub_seqs = Vec::new();
                self.pubsub_payloads = Vec::new();
                self.pubsub_grid = None;
                self.pubsub_built_for = None;
                self.live_notice = None;
            }
            Err(error) => {
                self.pubsub = None;
                self.live_notice = Some(crate::localization::message(error.message()));
            }
        }
        cx.notify();
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn stop_pubsub_capture(&mut self) {
        if let Some(capture) = &self.pubsub {
            capture.stop();
        }
    }

    /// Rebuild the active live grid when its stamp moved (new entries,
    /// overflow drops, filter edits). Runs on the live ticker; cheap when
    /// nothing changed.
    #[cfg(not(target_arch = "wasm32"))]
    fn sync_live_grids(&mut self, cx: &mut gpui::Context<Self>) {
        match self.tab {
            ActivityTab::Profiler => self.sync_profiler_grid(cx),
            ActivityTab::PubSub => self.sync_pubsub_grid(cx),
            _ => {}
        }
    }

    #[cfg(target_arch = "wasm32")]
    fn sync_live_grids(&mut self, _cx: &mut gpui::Context<Self>) {}

    #[cfg(not(target_arch = "wasm32"))]
    fn sync_profiler_grid(&mut self, cx: &mut gpui::Context<Self>) {
        let Some(capture) = &self.profiler else {
            return;
        };
        let filter = self.profiler_filter(cx);
        let stamp = (
            capture.change_stamp(),
            filter.command.clone(),
            filter.client.clone(),
            filter.substring.clone(),
        );
        if self.profiler_built_for.as_ref() == Some(&stamp) {
            return;
        }
        let snapshot = capture.snapshot_filtered(LIVE_DISPLAY_CAP, &filter);
        // A cluster capture merges every master's stream: name the node.
        let cluster = sqlly_engines::redis::cluster::is_known_cluster(&self.context.profile);
        let mut seqs = Vec::with_capacity(snapshot.entries.len());
        let rows: Vec<Vec<sqlly_core::CellValue>> = snapshot
            .entries
            .iter()
            .rev()
            .map(|(seq, entry)| {
                seqs.push(*seq);
                let mut row = vec![
                    sqlly_core::CellValue::String(clock_time(entry.server_seconds)),
                    entry
                        .database
                        .map(sqlly_core::CellValue::I64)
                        .unwrap_or(sqlly_core::CellValue::Null),
                    sqlly_core::CellValue::String(live_cell_text(&entry.client)),
                    sqlly_core::CellValue::String(entry.command.clone()),
                    sqlly_core::CellValue::String(live_cell_text(&entry.text)),
                ];
                if cluster {
                    row.push(node_cell(entry.node.as_deref()));
                }
                row
            })
            .collect();
        self.profiler_built_for = Some(stamp);
        // Nothing to show yet: keep the "waiting" message instead of an
        // empty grid.
        if rows.is_empty() && self.profiler_grid.is_none() {
            return;
        }
        apply_live_rows(
            &mut self.profiler_grid,
            &self.profiler_seqs,
            with_node_column(
                vec![
                    live_column("time", "string"),
                    live_column("db", "integer"),
                    live_column("client", "string"),
                    live_column("command", "string"),
                    live_column("arguments", "string"),
                ],
                cluster,
            ),
            rows,
            &seqs,
            cx,
        );
        self.profiler_seqs = seqs;
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn sync_pubsub_grid(&mut self, cx: &mut gpui::Context<Self>) {
        let Some(capture) = &self.pubsub else {
            return;
        };
        let stamp = capture.change_stamp();
        if self.pubsub_built_for == Some(stamp) {
            return;
        }
        let snapshot = capture.snapshot_tail(LIVE_DISPLAY_CAP);
        let cluster = sqlly_engines::redis::cluster::is_known_cluster(&self.context.profile);
        let mut rows = Vec::with_capacity(snapshot.entries.len());
        let mut payloads = Vec::with_capacity(snapshot.entries.len());
        let mut seqs = Vec::with_capacity(snapshot.entries.len());
        for (seq, message) in snapshot.entries.iter().rev() {
            let mut row = pubsub_row(message);
            if cluster {
                row.push(node_cell(message.node.as_deref()));
            }
            rows.push(row);
            payloads.push(LivePayload::Message(message.clone()));
            seqs.push(*seq);
        }
        self.pubsub_built_for = Some(stamp);
        if rows.is_empty() && self.pubsub_grid.is_none() {
            return;
        }
        apply_live_rows(
            &mut self.pubsub_grid,
            &self.pubsub_seqs,
            with_node_column(
                vec![
                    live_column("time", "string"),
                    live_column("channel", "string"),
                    live_column("pattern", "string"),
                    live_column("event", "string"),
                    live_column("key", "string"),
                    live_column("payload", "string"),
                ],
                cluster,
            ),
            rows,
            &seqs,
            cx,
        );
        self.pubsub_payloads = payloads;
        self.pubsub_seqs = seqs;
    }

    /// The selected Pub/Sub row's full payload, for the Preview action —
    /// looked up through the grid's display-to-source mapping, never by the
    /// raw display index.
    fn selected_pubsub_payload(&self, cx: &gpui::App) -> Option<LivePayload> {
        let grid = self.pubsub_grid.as_ref()?;
        let row = selected_source_row(grid, cx)?;
        self.pubsub_payloads.get(row).cloned()
    }

    /// Open the selected payload in the structured-data viewer: JSON and
    /// XML pretty-printed, binary as a hex dump. Plain text says so instead
    /// of opening an empty-looking viewer.
    fn preview_pubsub_payload(&mut self, cx: &mut gpui::Context<Self>) {
        let Some(payload) = self.selected_pubsub_payload(cx) else {
            return;
        };
        match payload {
            #[cfg(not(target_arch = "wasm32"))]
            LivePayload::Message(message) => {
                // Say plainly when only a prefix was kept, and how big the
                // original was.
                self.live_notice = pubsub_truncation_notice(&message);
                match &message.payload {
                    sqlly_services::redis_live::PubSubPayload::Text(text) => {
                        let preview = crate::structured_data::classify_cell(
                            &sqlly_core::CellValue::String(text.clone()),
                        );
                        // Plain text opens too: the viewer's format picker
                        // can re-read it (JSON pretty, hex, MessagePack,
                        // Base64, gzip/zstd/LZ4/deflate, Protobuf, PHP,
                        // Java, pickle), and Auto sniffs those on its own.
                        let preview = match preview {
                            crate::structured_data::ContentPreview::Hex => {
                                crate::structured_data::ContentPreview::Text
                            }
                            other => other,
                        };
                        crate::structured_data_viewer::open_cell_preview(
                            "payload", preview, text, cx,
                        );
                    }
                    sqlly_services::redis_live::PubSubPayload::Bytes(bytes) => {
                        crate::structured_data_viewer::open_binary_viewer("payload", bytes, cx);
                    }
                }
                cx.notify();
            }
        }
    }

    /// Status line for a live tab: counters first, then the capture state.
    #[cfg(not(target_arch = "wasm32"))]
    fn live_summary(&self) -> String {
        match self.tab {
            ActivityTab::Profiler => match &self.profiler {
                Some(capture) => live_summary_parts(capture, "commands", self.profiler_seqs.len()),
                None => crate::localization::text("explorer.activity_monitor.profiler_idle"),
            },
            ActivityTab::PubSub => match &self.pubsub {
                Some(capture) => live_summary_parts(capture, "messages", self.pubsub_seqs.len()),
                None => crate::localization::text("explorer.activity_monitor.pubsub_idle"),
            },
            _ => String::new(),
        }
    }

    /// The Redis live tabs. The browser preview has no Redis sockets, so it
    /// gets the honest note instead of the tools.
    #[cfg(target_arch = "wasm32")]
    fn render_live_content(&mut self, _cx: &mut gpui::Context<Self>) -> gpui::AnyElement {
        let s = Theme::scaled;
        div()
            .p(px(s(12.0)))
            .text_color(Theme::text_dim())
            .whitespace_normal()
            .child(crate::localization::text(
                "explorer.activity_monitor.live_browser_unavailable",
            ))
            .into_any_element()
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn render_live_content(&mut self, cx: &mut gpui::Context<Self>) -> gpui::AnyElement {
        match self.tab {
            ActivityTab::Profiler => self.render_profiler_tab(cx),
            ActivityTab::PubSub => self.render_pubsub_tab(cx),
            _ => div().into_any_element(),
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn live_capture_running<T>(
        capture: Option<&sqlly_services::redis_live::LiveCapture<T>>,
    ) -> bool {
        capture.is_some_and(|c| {
            matches!(
                c.status(),
                sqlly_services::redis_live::LiveStatus::Connecting
                    | sqlly_services::redis_live::LiveStatus::Live
            )
        })
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn live_banner(&self, text: String) -> gpui::AnyElement {
        let s = Theme::scaled;
        div()
            .mx(px(s(12.0)))
            .mt(px(s(6.0)))
            .p(px(s(8.0)))
            .border_1()
            .border_color(Theme::border())
            .rounded(px(s(4.0)))
            .text_size(px(Theme::font_size_ratio(0.8)))
            .text_color(Theme::text_dim())
            .whitespace_normal()
            .child(text)
            .into_any_element()
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn live_warning_row(text: String) -> gpui::AnyElement {
        let s = Theme::scaled;
        div()
            .mx(px(s(12.0)))
            .mt(px(s(6.0)))
            .text_size(px(Theme::font_size_ratio(0.8)))
            .text_color(Theme::warning())
            .whitespace_normal()
            .child(text)
            .into_any_element()
    }

    /// The tab's warnings: a relay refusal (which also disables Start), or
    /// a one-off notice.
    #[cfg(not(target_arch = "wasm32"))]
    fn live_notice_row(&self, refusal: Option<&String>) -> Option<gpui::AnyElement> {
        refusal
            .or(self.live_notice.as_ref())
            .map(|text| Self::live_warning_row(text.clone()))
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn render_profiler_tab(&mut self, cx: &mut gpui::Context<Self>) -> gpui::AnyElement {
        let s = Theme::scaled;
        let running = Self::live_capture_running(self.profiler.as_ref());
        let capturing = self.profiler.is_some();
        let paused = self.profiler.as_ref().is_some_and(|c| c.is_paused());
        let (dropped, cap) = self
            .profiler
            .as_ref()
            .map(|c| (c.counts().1, c.cap()))
            .unwrap_or((
                0,
                parse_live_cap(
                    &Self::input_value(&self.profiler_cap_input, cx),
                    sqlly_services::redis_live::DEFAULT_PROFILER_CAP,
                ),
            ));
        let time_box_secs =
            parse_time_box_secs(&Self::input_value(&self.profiler_time_box_input, cx));
        let refusal = self.live_refusal(ActivityTab::Profiler);
        let refused = refusal.is_some();

        let toolbar = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(s(6.0)))
            .px(px(s(12.0)))
            .pt(px(s(10.0)))
            .child(if running {
                Button::new("am-prof-stop")
                    .small()
                    .label(crate::localization::text(
                        "explorer.activity_monitor.profiler_stop_button",
                    ))
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.stop_profiler_capture();
                        cx.notify();
                    }))
            } else {
                Button::new("am-prof-start")
                    .small()
                    .primary()
                    .label(crate::localization::text(
                        "explorer.activity_monitor.profiler_start_button",
                    ))
                    .disabled(refused)
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.start_profiler_capture(cx);
                    }))
            })
            .child(
                Button::new("am-prof-pause")
                    .small()
                    .label(crate::localization::text(if paused {
                        "explorer.activity_monitor.profiler_resume_button"
                    } else {
                        "explorer.activity_monitor.profiler_pause_button"
                    }))
                    .disabled(!running)
                    .on_click(cx.listener(|this, _, _window, cx| {
                        if let Some(capture) = &this.profiler {
                            capture.set_paused(!capture.is_paused());
                        }
                        cx.notify();
                    })),
            )
            .child(
                Button::new("am-prof-clear")
                    .small()
                    .label(crate::localization::text(
                        "explorer.activity_monitor.profiler_clear_button",
                    ))
                    .disabled(!capturing)
                    .on_click(cx.listener(|this, _, _window, cx| {
                        if let Some(capture) = &this.profiler {
                            capture.clear();
                        }
                        this.profiler_built_for = None;
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .w(px(s(96.0)))
                    .child(Input::new(&self.profiler_cap_input)),
            )
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text(
                        "explorer.activity_monitor.profiler_stop_after_label",
                    )),
            )
            .child(
                div()
                    .w(px(s(64.0)))
                    .child(Input::new(&self.profiler_time_box_input)),
            )
            .child(
                Button::new("am-prof-apply-cap")
                    .small()
                    .label(crate::localization::text(
                        "explorer.activity_monitor.profiler_apply_cap_button",
                    ))
                    .disabled(!capturing)
                    .on_click(cx.listener(|this, _, _window, cx| {
                        let cap = parse_live_cap(
                            &Self::input_value(&this.profiler_cap_input, cx),
                            sqlly_services::redis_live::DEFAULT_PROFILER_CAP,
                        );
                        if let Some(capture) = &this.profiler {
                            capture.set_cap(cap);
                        }
                        cx.notify();
                    })),
            )
            .child(div().flex_1())
            .child(
                div()
                    .w(px(s(110.0)))
                    .child(Input::new(&self.filter_command_input)),
            )
            .child(
                div()
                    .w(px(s(130.0)))
                    .child(Input::new(&self.filter_client_input)),
            )
            .child(
                div()
                    .w(px(s(150.0)))
                    .child(Input::new(&self.filter_text_input)),
            );

        let body: gpui::AnyElement = if let Some(grid) = &self.profiler_grid {
            div().size_full().child(grid.clone()).into_any_element()
        } else {
            let connecting = self.profiler.as_ref().is_some_and(|c| {
                matches!(
                    c.status(),
                    sqlly_services::redis_live::LiveStatus::Connecting
                )
            });
            let buffered = self.profiler.as_ref().map_or(0, |c| c.counts().2);
            let message = crate::localization::text(if !capturing {
                "explorer.activity_monitor.profiler_empty_idle"
            } else if connecting {
                "explorer.activity_monitor.profiler_empty_connecting"
            } else if buffered > 0 {
                "explorer.activity_monitor.profiler_empty_filtered"
            } else {
                "explorer.activity_monitor.profiler_empty_waiting"
            });
            div()
                .p(px(s(12.0)))
                .text_color(Theme::text_dim())
                .child(message)
                .into_any_element()
        };

        div()
            .flex()
            .flex_col()
            .size_full()
            .child(toolbar)
            .child(self.live_banner(crate::localization::format(
                "explorer.activity_monitor.profiler_banner",
                &[
                    ("cap", &cap.to_string()),
                    ("dropped", &dropped.to_string()),
                    ("display_cap", &LIVE_DISPLAY_CAP.to_string()),
                    ("seconds", &time_box_secs.to_string()),
                ],
            )))
            .children(self.live_notice_row(refusal.as_ref()))
            .child(div().flex_1().min_h(px(0.0)).mt(px(s(8.0))).child(body))
            .into_any_element()
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn render_pubsub_tab(&mut self, cx: &mut gpui::Context<Self>) -> gpui::AnyElement {
        let s = Theme::scaled;
        let running = Self::live_capture_running(self.pubsub.as_ref());
        let subscribed = self.pubsub.is_some();
        let paused = self.pubsub.as_ref().is_some_and(|c| c.is_paused());
        let (dropped, cap) = self
            .pubsub
            .as_ref()
            .map(|c| (c.counts().1, c.cap()))
            .unwrap_or((
                0,
                parse_live_cap(
                    &Self::input_value(&self.pubsub_cap_input, cx),
                    sqlly_services::redis_live::DEFAULT_PUBSUB_CAP,
                ),
            ));
        let has_selection = self.selected_pubsub_payload(cx).is_some();
        let refusal = self.live_refusal(ActivityTab::PubSub);
        let refused = refusal.is_some();

        let toolbar = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(s(6.0)))
            .px(px(s(12.0)))
            .pt(px(s(10.0)))
            .child(
                div()
                    .flex_1()
                    .child(Input::new(&self.channels_input).disabled(running)),
            )
            .child(if running {
                Button::new("am-sub-stop")
                    .small()
                    .label(crate::localization::text(
                        "explorer.activity_monitor.pubsub_stop_button",
                    ))
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.stop_pubsub_capture();
                        cx.notify();
                    }))
            } else {
                Button::new("am-sub-start")
                    .small()
                    .primary()
                    .label(crate::localization::text(
                        "explorer.activity_monitor.pubsub_subscribe_button",
                    ))
                    .disabled(refused)
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.start_pubsub_capture(cx);
                    }))
            })
            .child(
                Button::new("am-sub-key-events")
                    .small()
                    .label(crate::localization::text(
                        "explorer.activity_monitor.pubsub_key_events_button",
                    ))
                    .tooltip(crate::localization::text(
                        "explorer.activity_monitor.pubsub_key_events_tooltip",
                    ))
                    .disabled(refused || running)
                    .on_click(cx.listener(|this, _, window, cx| {
                        let db = preset_database_index(&this.context.profile);
                        let text = sqlly_services::redis_live::key_events_preset(
                            db,
                            &Self::input_value(&this.channels_input, cx),
                        );
                        this.channels_input
                            .update(cx, |state, cx| state.set_value(text, window, cx));
                        this.start_pubsub_capture(cx);
                    })),
            )
            .child(
                Button::new("am-sub-pause")
                    .small()
                    .label(crate::localization::text(if paused {
                        "explorer.activity_monitor.pubsub_resume_button"
                    } else {
                        "explorer.activity_monitor.pubsub_pause_button"
                    }))
                    .disabled(!running)
                    .on_click(cx.listener(|this, _, _window, cx| {
                        if let Some(capture) = &this.pubsub {
                            capture.set_paused(!capture.is_paused());
                        }
                        cx.notify();
                    })),
            )
            .child(
                Button::new("am-sub-clear")
                    .small()
                    .label(crate::localization::text(
                        "explorer.activity_monitor.pubsub_clear_button",
                    ))
                    .disabled(!subscribed)
                    .on_click(cx.listener(|this, _, _window, cx| {
                        if let Some(capture) = &this.pubsub {
                            capture.clear();
                        }
                        this.pubsub_built_for = None;
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .w(px(s(90.0)))
                    .child(Input::new(&self.pubsub_cap_input)),
            )
            .child(
                Button::new("am-sub-apply-cap")
                    .small()
                    .label(crate::localization::text(
                        "explorer.activity_monitor.pubsub_apply_cap_button",
                    ))
                    .disabled(!subscribed)
                    .on_click(cx.listener(|this, _, _window, cx| {
                        let cap = parse_live_cap(
                            &Self::input_value(&this.pubsub_cap_input, cx),
                            sqlly_services::redis_live::DEFAULT_PUBSUB_CAP,
                        );
                        if let Some(capture) = &this.pubsub {
                            capture.set_cap(cap);
                        }
                        cx.notify();
                    })),
            )
            .child(
                Button::new("am-sub-preview")
                    .small()
                    .label(crate::localization::text(
                        "explorer.activity_monitor.pubsub_preview_button",
                    ))
                    .disabled(!has_selection)
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.preview_pubsub_payload(cx);
                    })),
            );

        let body: gpui::AnyElement = if let Some(grid) = &self.pubsub_grid {
            div().size_full().child(grid.clone()).into_any_element()
        } else {
            let connecting = self.pubsub.as_ref().is_some_and(|c| {
                matches!(
                    c.status(),
                    sqlly_services::redis_live::LiveStatus::Connecting
                )
            });
            let message = crate::localization::text(if !subscribed {
                "explorer.activity_monitor.pubsub_empty_idle"
            } else if connecting {
                "explorer.activity_monitor.pubsub_empty_connecting"
            } else {
                "explorer.activity_monitor.pubsub_empty_waiting"
            });
            div()
                .p(px(s(12.0)))
                .text_color(Theme::text_dim())
                .child(message)
                .into_any_element()
        };

        let keyspace_strip = self.render_keyspace_strip(cx);
        div()
            .flex()
            .flex_col()
            .size_full()
            .child(toolbar)
            .child(self.live_banner(crate::localization::format(
                "explorer.activity_monitor.pubsub_banner",
                &[
                    ("cap", &cap.to_string()),
                    ("dropped", &dropped.to_string()),
                    ("display_cap", &LIVE_DISPLAY_CAP.to_string()),
                ],
            )))
            .child(keyspace_strip)
            .children(self.live_notice_row(refusal.as_ref()))
            .child(div().flex_1().min_h(px(0.0)).mt(px(s(8.0))).child(body))
            .into_any_element()
    }

    /// The Pub/Sub tab's keyspace row: what `notify-keyspace-events` is set
    /// to, the reviewed CONFIG SET that turns key events on when they are
    /// off, and the same review strip as the slow-log threshold.
    #[cfg(not(target_arch = "wasm32"))]
    fn render_keyspace_strip(&mut self, cx: &mut gpui::Context<Self>) -> gpui::AnyElement {
        let s = Theme::scaled;
        let note = match &self.keyspace_flags {
            None => crate::localization::text("explorer.activity_monitor.keyspace_reading"),
            Some(flags) => {
                let mut note = keyspace_notify_note(flags.as_deref());
                if let Some(error) = &self.keyspace_error {
                    note.push_str(&format!(" ({error})"));
                }
                note
            }
        };
        let already_on = self
            .keyspace_flags
            .as_ref()
            .and_then(|flags| flags.as_deref())
            .is_some_and(keyspace_events_on);
        let writes_blocked = self.slowlog_write_block.is_some();
        let row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(s(6.0)))
            .child(
                div()
                    .flex_1()
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .text_color(Theme::text_dim())
                    .whitespace_normal()
                    .child(note),
            )
            .child(
                Button::new("am-keyspace-enable")
                    .small()
                    .label(crate::localization::text(
                        "explorer.activity_monitor.keyspace_enable_button",
                    ))
                    .disabled(writes_blocked || already_on || self.keyspace_loading)
                    .on_click(cx.listener(|this, _, _window, cx| this.review_keyspace_enable(cx))),
            );
        div()
            .mx(px(s(12.0)))
            .mt(px(s(6.0)))
            .flex()
            .flex_col()
            .gap(px(s(6.0)))
            .child(row)
            .children(self.render_config_review(cx))
            .into_any_element()
    }

    /// Read `notify-keyspace-events` for the Pub/Sub tab's keyspace row.
    #[cfg(not(target_arch = "wasm32"))]
    fn load_keyspace_flags(&mut self, cx: &mut gpui::Context<Self>) {
        if self.activity_dialect() != Some(sqlly_services::ActivityDialect::Redis)
            || self.keyspace_loading
        {
            return;
        }
        self.keyspace_loading = true;
        crate::tool_dialog::run_query(
            &self.context,
            KEYSPACE_NOTIFY_QUERY.to_string(),
            cx,
            |this, outcome, cx| {
                this.keyspace_loading = false;
                // The slow-log read's wording covers CONFIG GET generally
                // (ACL permission vs a disabled or renamed CONFIG).
                this.keyspace_error = outcome
                    .error
                    .as_deref()
                    .map(|error| {
                        crate::localization::message(
                            sqlly_services::admin::activity::ActivityMonitor::slowlog_threshold_error_message(error),
                        )
                    });
                let flags = if outcome.error.is_some() {
                    None
                } else {
                    outcome
                        .sets
                        .first()
                        .and_then(|set| keyspace_notify_flags_from_rows(&set.rows))
                };
                this.keyspace_flags = Some(flags);
                cx.notify();
            },
        );
    }

    #[cfg(target_arch = "wasm32")]
    fn load_keyspace_flags(&mut self, _cx: &mut gpui::Context<Self>) {}

    /// Stage `CONFIG SET notify-keyspace-events KEA` for review (nothing
    /// runs yet).
    #[cfg(not(target_arch = "wasm32"))]
    fn review_keyspace_enable(&mut self, cx: &mut gpui::Context<Self>) {
        if self.slowlog_write_block.is_some() {
            return;
        }
        self.slowlog_message = None;
        self.slowlog_review = Some(SlowlogReview {
            statement: KEYSPACE_NOTIFY_ENABLE.to_string(),
            explanation: crate::localization::text(
                "explorer.activity_monitor.keyspace_enable_explanation",
            ),
        });
        cx.notify();
    }

    fn activity_dialect(&self) -> Option<sqlly_services::ActivityDialect> {
        sqlly_services::activity_dialect(self.context.profile.database_type)
    }

    /// Read `slowlog-log-slower-than` for the Expensive tab's threshold row.
    /// Engines without a slow log never query.
    fn load_slowlog_threshold(&mut self, cx: &mut gpui::Context<Self>) {
        let Some(query) = self.activity_dialect().and_then(
            sqlly_services::admin::activity::ActivityMonitor::slowlog_threshold_query_for,
        ) else {
            return;
        };
        if self.slowlog_loading {
            return;
        }
        self.slowlog_loading = true;
        crate::tool_dialog::run_query(&self.context, query.to_string(), cx, |this, outcome, cx| {
            this.slowlog_loading = false;
            // Keep the server's reason: "unknown" alone cannot tell a
            // missing ACL permission from a renamed CONFIG command.
            this.slowlog_threshold_error = outcome
                .error
                .as_deref()
                .map(|error| {
                        crate::localization::message(
                            sqlly_services::admin::activity::ActivityMonitor::slowlog_threshold_error_message(error),
                        )
                    });
            let threshold = if outcome.error.is_some() {
                None
            } else {
                outcome.sets.first().and_then(|set| {
                    sqlly_services::admin::activity::ActivityMonitor::slowlog_threshold_from_rows(
                        &set.rows,
                    )
                })
            };
            this.slowlog_threshold = Some(threshold);
            cx.notify();
        });
    }

    /// The connection the slow-log strip's CONFIG statements run on.
    pub fn slowlog_profile(&self) -> &sqlly_core::ConnectionProfile {
        &self.context.profile
    }

    /// Hand the slow-log strip the production guard: the decision now (to
    /// disable CONFIG SET / CONFIG REWRITE while writes are locked) and the
    /// live check re-run right before a reviewed statement executes. Called
    /// by `AppView` when the monitor opens or is re-invoked.
    pub fn set_production_check(
        &mut self,
        production: sqlly_intel::query::safety::ProductionDecision,
        check: crate::tool_dialog::ProductionCheck,
        cx: &mut gpui::Context<Self>,
    ) {
        self.slowlog_production_check = Some(check);
        self.apply_production_decision(production);
        cx.notify();
    }

    fn apply_production_decision(
        &mut self,
        production: sqlly_intel::query::safety::ProductionDecision,
    ) {
        let reason = slowlog_write_block_reason(production);
        if reason.is_some() {
            // A statement staged while unlocked must not run once locked.
            self.slowlog_review = None;
        }
        self.slowlog_write_block = reason;
    }

    /// Stage the typed threshold for review (nothing runs yet).
    fn review_slowlog_threshold(&mut self, cx: &mut gpui::Context<Self>) {
        if self.slowlog_write_block.is_some() {
            return;
        }
        let text = Self::input_value(&self.slowlog_threshold_input, cx);
        match slowlog_set_statement(self.activity_dialect(), &text) {
            Some(statement) => {
                self.slowlog_message = None;
                self.slowlog_review = Some(SlowlogReview {
                    statement,
                    explanation: crate::localization::text(
                        "explorer.activity_monitor.slowlog_change_explanation",
                    ),
                });
            }
            None => {
                self.slowlog_review = None;
                self.slowlog_message = Some((
                    false,
                    crate::localization::text("explorer.activity_monitor.slowlog_invalid"),
                ));
            }
        }
        cx.notify();
    }

    /// Stage CONFIG REWRITE for review.
    fn review_slowlog_rewrite(&mut self, cx: &mut gpui::Context<Self>) {
        if self.slowlog_write_block.is_some() {
            return;
        }
        let Some(statement) = self
            .activity_dialect()
            .and_then(sqlly_services::admin::activity::ActivityMonitor::slowlog_rewrite_for)
        else {
            return;
        };
        self.slowlog_message = None;
        self.slowlog_review = Some(SlowlogReview {
            statement: statement.to_string(),
            explanation: crate::localization::text(
                "explorer.activity_monitor.slowlog_rewrite_explanation",
            ),
        });
        cx.notify();
    }

    /// Run the reviewed statement on the pane's connection (guardrails
    /// apply: a read-only connection refuses it with its reason), then
    /// re-read the threshold.
    fn apply_slowlog_review(&mut self, cx: &mut gpui::Context<Self>) {
        // Re-check the guard now: an unlock can expire while the pane is open.
        if let Some(check) = self.slowlog_production_check.clone() {
            self.apply_production_decision(check(cx));
        }
        if let Some(reason) = self.slowlog_write_block.clone() {
            self.slowlog_review = None;
            self.slowlog_message = Some((false, reason));
            cx.notify();
            return;
        }
        let Some(review) = self.slowlog_review.take() else {
            return;
        };
        let statement = review.statement.clone();
        crate::tool_dialog::run_query(
            &self.context,
            statement.clone(),
            cx,
            move |this, outcome, cx| {
                this.slowlog_message = Some(match outcome.error {
                    Some(error) => (
                        false,
                        crate::localization::format(
                            "explorer.activity_monitor.config_failed",
                            &[("statement", &statement), ("error", &error)],
                        ),
                    ),
                    None => (
                        true,
                        crate::localization::format(
                            "explorer.activity_monitor.config_succeeded",
                            &[("statement", &statement)],
                        ),
                    ),
                });
                // Re-read whichever setting the tab shows, so the strip
                // reflects the server rather than the statement's intent.
                if this.tab == ActivityTab::PubSub {
                    this.keyspace_flags = None;
                    this.load_keyspace_flags(cx);
                } else {
                    this.slowlog_threshold = None;
                    this.load_slowlog_threshold(cx);
                }
                cx.notify();
            },
        );
        cx.notify();
    }

    /// The reviewed-CONFIG rows shared by the slow-log and keyspace strips:
    /// the write-block reason, the staged statement with Run / Cancel, and
    /// the outcome of the last run.
    fn render_config_review(&self, cx: &mut gpui::Context<Self>) -> Vec<gpui::AnyElement> {
        let s = Theme::scaled;
        let writes_blocked = self.slowlog_write_block.is_some();
        let mut rows = Vec::new();
        if let Some(reason) = self.slowlog_write_block.clone() {
            rows.push(
                div()
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .text_color(Theme::warning())
                    .whitespace_normal()
                    .child(reason)
                    .into_any_element(),
            );
        }
        if let Some(review) = self.slowlog_review.clone() {
            rows.push(
                div()
                    .p(px(s(10.0)))
                    .border_1()
                    .border_color(Theme::warning())
                    .rounded(px(s(4.0)))
                    .bg(Theme::warning().opacity(0.08))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(8.0)))
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap(px(s(4.0)))
                            .child(
                                div()
                                    .font_family(Theme::editor_font_family())
                                    .text_color(Theme::text())
                                    .child(review.statement.clone()),
                            )
                            .child(
                                div()
                                    .text_size(px(Theme::font_size_ratio(0.85)))
                                    .text_color(Theme::text_dim())
                                    .whitespace_normal()
                                    .child(review.explanation.clone()),
                            ),
                    )
                    .child(
                        Button::new("am-slowlog-run")
                            .small()
                            .primary()
                            .label(crate::localization::text(
                                "explorer.activity_monitor.config_run_button",
                            ))
                            .disabled(writes_blocked)
                            .on_click(
                                cx.listener(|this, _, _window, cx| this.apply_slowlog_review(cx)),
                            ),
                    )
                    .child(
                        Button::new("am-slowlog-cancel")
                            .small()
                            .label(crate::localization::text(
                                "explorer.activity_monitor.config_cancel_button",
                            ))
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.slowlog_review = None;
                                cx.notify();
                            })),
                    )
                    .into_any_element(),
            );
        }
        if let Some((ok, message)) = self.slowlog_message.clone() {
            rows.push(
                div()
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .text_color(if ok { Theme::success() } else { Theme::error() })
                    .whitespace_normal()
                    .child(message)
                    .into_any_element(),
            );
        }
        rows
    }

    /// The Expensive tab's slow-log row (Redis only): the configured
    /// threshold, a field to change it, and the review strip.
    fn render_slowlog_strip(&mut self, cx: &mut gpui::Context<Self>) -> Option<gpui::AnyElement> {
        if self.tab != ActivityTab::ExpensiveQueries {
            return None;
        }
        self.activity_dialect().and_then(
            sqlly_services::admin::activity::ActivityMonitor::slowlog_threshold_query_for,
        )?;
        let s = Theme::scaled;
        let note = match self.slowlog_threshold {
            None => crate::localization::text("explorer.activity_monitor.slowlog_reading"),
            Some(threshold) => {
                let lead = crate::localization::message(
                    sqlly_services::admin::activity::ActivityMonitor::slowlog_threshold_lead_message(
                        threshold,
                    ),
                );
                let detail = crate::localization::message(
                    sqlly_services::admin::activity::ActivityMonitor::slowlog_threshold_note_message(
                        threshold,
                    ),
                );
                let (key, arguments): (&str, Vec<(&str, &str)>) =
                    if let Some(error) = &self.slowlog_threshold_error {
                        (
                            "explorer.activity_monitor.slowlog_summary_error",
                            vec![("lead", &lead), ("detail", &detail), ("error", error)],
                        )
                    } else {
                        (
                            "explorer.activity_monitor.slowlog_summary",
                            vec![("lead", &lead), ("detail", &detail)],
                        )
                    };
                crate::localization::format(key, &arguments)
            }
        };
        let writes_blocked = self.slowlog_write_block.is_some();
        let row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(s(6.0)))
            .child(
                div()
                    .flex_1()
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .text_color(Theme::text_dim())
                    .whitespace_normal()
                    .child(note),
            )
            .child(
                div()
                    .w(px(s(190.0)))
                    .child(Input::new(&self.slowlog_threshold_input)),
            )
            .child(
                Button::new("am-slowlog-set")
                    .small()
                    .label(crate::localization::text(
                        "explorer.activity_monitor.slowlog_change_button",
                    ))
                    .disabled(writes_blocked)
                    .on_click(
                        cx.listener(|this, _, _window, cx| this.review_slowlog_threshold(cx)),
                    ),
            )
            .child(
                Button::new("am-slowlog-rewrite")
                    .small()
                    .label(crate::localization::text(
                        "explorer.activity_monitor.slowlog_rewrite_button",
                    ))
                    .disabled(writes_blocked)
                    .on_click(cx.listener(|this, _, _window, cx| this.review_slowlog_rewrite(cx))),
            );
        let strip = div()
            .mx(px(s(14.0)))
            .mt(px(s(8.0)))
            .flex()
            .flex_col()
            .gap(px(s(6.0)))
            .child(row)
            .children(self.render_config_review(cx));
        Some(strip.into_any_element())
    }

    fn refresh(&mut self, cx: &mut gpui::Context<Self>) {
        if self.refresh_in_flight {
            return;
        }
        self.refresh_in_flight = true;
        self.refresh_generation += 1;
        let generation = self.refresh_generation;
        let tab = self.tab;
        let admin = self.context.services.admin_tools.clone();
        let Some(mut query) = tab.query_for(&*admin, self.context.profile.database_type) else {
            self.loading = false;
            self.refresh_in_flight = false;
            self.error = Some(crate::localization::format(
                "explorer.activity_monitor.unsupported_engine",
                &[("engine", &self.context.profile.database_type.display_name())],
            ));
            cx.notify();
            return;
        };
        // Database scope narrows the SQL Server Sessions view to connections on
        // the pane's own database. `DB_ID()` (no argument) resolves to that
        // database because the query already runs in its context, so the filter
        // needs no interpolation. Other tabs/engines stay server-wide.
        if self.scope == ToolScope::Database
            && tab == ActivityTab::Sessions
            && self.context.profile.database_type == sqlly_core::DatabaseType::SqlServer
        {
            query = query.replace(
                "WHERE s.session_id > 50",
                "WHERE s.session_id > 50 AND s.database_id = DB_ID()",
            );
        }
        if tab == ActivityTab::ExpensiveQueries && self.slowlog_threshold.is_none() {
            self.load_slowlog_threshold(cx);
        }
        crate::tool_dialog::run_query(&self.context, query, cx, move |this, outcome, cx| {
            if this.refresh_generation != generation {
                return;
            }
            this.refresh_in_flight = false;
            this.loading = false;
            this.last_refreshed = Some(std::time::Instant::now());
            if let Some(err) = outcome.error {
                this.error = Some(err);
                this.grid = None;
                this.columns = Vec::new();
                this.rows = Vec::new();
                this.source_columns = Vec::new();
                this.source_rows = Vec::new();
                this.row_count = 0;
            } else {
                this.error = None;
                let mut set = outcome.sets.into_iter().next().unwrap_or_default();
                if tab == ActivityTab::Waits {
                    // Bake the change-since-last-refresh columns into the
                    // source set on data arrival only — grid rebuilds from
                    // filter toggles must not re-diff a snapshot against
                    // itself.
                    let (delta_set, snapshot) =
                        wait_delta_set(&set.columns, &set.rows, this.wait_snapshot.as_ref());
                    set = delta_set;
                    this.wait_snapshot = Some(snapshot);
                }
                this.row_count = set.rows.len();
                this.source_columns = set.columns;
                this.source_rows = set.rows;
                this.rebuild_grid(cx);
                // Fresh rows, fresh ranking — but never more than one triage
                // pass per 5 s however short the refresh interval is.
                this.maybe_spawn_attention(cx);
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
    /// same monitor — tab, filters, and its auto-refresh ticker — carries on
    /// in the window.
    fn pop_out(&mut self, cx: &mut gpui::Context<Self>) {
        let entity = cx.entity();
        let connection = self.context.profile.name.clone();
        let title = move || {
            crate::localization::format(
                "explorer.activity_monitor.title",
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
                    Theme::scaled(900.0),
                    Theme::scaled(620.0),
                ),
                None,
                cx,
                move |_, cx| {
                    build_entity.update(cx, |pane, _| pane.windowed = true);
                    build_entity
                },
            );
            if opened.is_some() {
                // The window owns the monitor now; drop the inline card.
                entity.update(cx, |_, cx| cx.emit(ToolDialogEvent::Close));
            }
        });
    }

    /// Status line under the grid: row count + truncation + refresh age.
    /// Live tabs report their capture instead.
    fn summary_text(&self) -> String {
        #[cfg(not(target_arch = "wasm32"))]
        if self.tab.is_live() {
            return self.live_summary();
        }
        #[cfg(target_arch = "wasm32")]
        if self.tab.is_live() {
            return String::new();
        }
        let format_count = |key: &str, count: usize| {
            crate::localization::format(key, &[("count", &count.to_string())])
        };
        let mut parts = Vec::new();
        if self.row_count >= crate::tool_dialog::TOOL_QUERY_ROW_CAP {
            parts.push(format_count(
                "explorer.activity_monitor.summary_first_rows",
                crate::tool_dialog::TOOL_QUERY_ROW_CAP,
            ));
        } else {
            parts.push(format_count(
                "explorer.activity_monitor.summary_rows",
                self.row_count,
            ));
        }
        if self.rows.len() != self.row_count {
            parts.push(crate::localization::format(
                "explorer.activity_monitor.summary_filter_matches",
                &[
                    ("count", &self.rows.len().to_string()),
                    ("filter", &self.triage_filter.label()),
                ],
            ));
        }
        if self.paused {
            parts.push(crate::localization::text(
                "explorer.activity_monitor.summary_auto_paused",
            ));
        } else {
            parts.push(format_count(
                "explorer.activity_monitor.summary_auto_interval",
                self.interval_secs as usize,
            ));
        }
        if let Some(at) = self.last_refreshed {
            parts.push(format_count(
                "explorer.activity_monitor.summary_refreshed_ago",
                at.elapsed().as_secs() as usize,
            ));
        }
        if let Some(err) = &self.attention_error {
            parts.push(err.clone());
        } else if self.attention_in_flight {
            parts.push(crate::localization::text(
                "explorer.activity_monitor.summary_ranking",
            ));
        } else if self.triage_filter == ActivityTriageFilter::Attention
            && self.attention_verdicts.is_some()
        {
            parts.push(crate::localization::text(
                "explorer.activity_monitor.summary_ranked",
            ));
        }
        parts.join(" · ")
    }
}

impl gpui::Focusable for ActivityMonitorPane {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ActivityMonitorPane {
    fn render(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let active = self.tab;

        let tabs = div().flex().flex_row().gap(px(s(4.0))).children(
            ActivityTab::tabs_for(self.context.profile.database_type)
                .into_iter()
                .map(|tab| {
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
                }),
        );

        let content: gpui::AnyElement = if self.tab.is_live() {
            self.render_live_content(cx)
        } else if let Some(err) = &self.error {
            div()
                .p(px(s(12.0)))
                .text_color(Theme::error())
                .text_size(px(Theme::font_size_ratio(0.87)))
                .whitespace_normal()
                .child(crate::localization::format(
                    "explorer.activity_monitor.load_failed",
                    &[
                        ("tab", &self.tab.label()),
                        ("error", err),
                        (
                            "permission",
                            &permission_guidance(self.context.profile.database_type),
                        ),
                    ],
                ))
                .into_any_element()
        } else if let Some(grid) = &self.grid {
            div().size_full().child(grid.clone()).into_any_element()
        } else if self.loading {
            div()
                .p(px(s(12.0)))
                .text_color(Theme::text_dim())
                .child(crate::localization::text(
                    "explorer.activity_monitor.loading",
                ))
                .into_any_element()
        } else {
            div()
                .p(px(s(12.0)))
                .text_color(Theme::text_dim())
                .child(crate::localization::text(
                    "explorer.activity_monitor.no_rows",
                ))
                .into_any_element()
        };

        // Selected-row actions for the footer (the DMV tabs all carry a
        // query column).
        let sql = self.selected_sql(cx);
        let has_sql = sql.is_some();
        let sql_for_copy = sql.clone();
        let diagnostic = self.selected_diagnostic_script(cx);
        let has_diagnostic = diagnostic.is_some();
        let termination = self.selected_termination_review(cx);
        let has_termination = termination.is_some();
        let slowlog_strip = self.render_slowlog_strip(cx);

        let focus_handle = self.focus_handle.clone();
        let dialog = Dialog::new("activity-monitor-pane", |this: &mut Self| &mut this.frame)
            .windowed(self.windowed)
            .title(crate::localization::format(
                "explorer.activity_monitor.title",
                &[("connection", &self.context.profile.name)],
            ))
            .easy_close(true)
            .key_context("ActivityMonitorPane")
            .track_focus(&focus_handle)
            .on_close(|this, window, cx| this.close(window, cx))
            .bare_body()
            .child(div().px(px(s(14.0))).pt(px(s(10.0))).child(tabs))
            .children_body(self.pending_termination.clone().map(|review| {
                let script_for_copy = review.script.clone();
                let script_for_editor = review.script.clone();
                div()
                    .mx(px(s(14.0)))
                    .mt(px(s(8.0)))
                    .p(px(s(10.0)))
                    .border_1()
                    .border_color(Theme::warning())
                    .rounded(px(s(4.0)))
                    .bg(Theme::warning().opacity(0.08))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(8.0)))
                    .child(
                        div()
                            .flex_1()
                            .text_color(Theme::text())
                            .whitespace_normal()
                            .child(crate::localization::format(
                                "explorer.activity_monitor.termination_review",
                                &[("session", &review.session_id.to_string())],
                            )),
                    )
                    .child(
                        Button::new("am-copy-termination")
                            .small()
                            .label(crate::localization::text(
                                "explorer.activity_monitor.termination_copy_button",
                            ))
                            .on_click(cx.listener(move |_this, _, _window, cx| {
                                cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                    script_for_copy.clone(),
                                ));
                            })),
                    )
                    .child(
                        Button::new("am-confirm-termination")
                            .small()
                            .label(crate::localization::text(
                                "explorer.activity_monitor.termination_open_button",
                            ))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.emit(ToolDialogEvent::ScriptToEditor {
                                    sql: script_for_editor.clone(),
                                });
                                this.close(window, cx);
                            })),
                    )
                    .child(
                        Button::new("am-cancel-termination")
                            .small()
                            .label(crate::localization::text(
                                "explorer.activity_monitor.termination_cancel_button",
                            ))
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.pending_termination = None;
                                cx.notify();
                            })),
                    )
            }))
            .children_body(slowlog_strip)
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .mx(px(s(14.0)))
                    .mt(px(s(8.0)))
                    .border_1()
                    .border_color(Theme::border())
                    .rounded(px(s(4.0)))
                    .child(content),
            )
            .child(
                div()
                    .px(px(s(14.0)))
                    .py(px(s(8.0)))
                    .text_size(px(Theme::font_size_ratio(0.74)))
                    .text_color(Theme::text_dim())
                    .child(self.summary_text()),
            );
        // The DMV toolbar and row actions apply to the query-backed tabs;
        // the live tools carry their own controls inside the content area.
        let mut dialog = if !self.tab.is_live() {
            let dialog = dialog.leading_button(
                Button::new("am-triage-filter")
                    .label(self.triage_filter.label())
                    .disabled(matches!(
                        self.tab,
                        ActivityTab::ExpensiveQueries | ActivityTab::Waits
                    ))
                    .on_click(cx.listener(|this, _, _window, cx| this.cycle_triage_filter(cx))),
            );
            // Off by default, never persisted: turning this on sends the
            // visible rows — SQL text of other sessions included — to the
            // configured decision provider on each ranking pass. Absent
            // entirely while no provider is configured or row data is not
            // allowed.
            let dialog = if self.attention_offered {
                dialog.leading_button(
                    Button::new("am-attention")
                        .label(crate::localization::text(if self.attention_enabled {
                            "explorer.activity_monitor.attention_on_button"
                        } else {
                            "explorer.activity_monitor.attention_enable_button"
                        }))
                        .on_click(cx.listener(|this, _, _window, cx| this.toggle_attention(cx))),
                )
            } else {
                dialog
            };
            dialog
                .leading_button(
                    Button::new("am-interval")
                        .label(crate::localization::format(
                            "explorer.activity_monitor.interval_button",
                            &[("seconds", &self.interval_secs.to_string())],
                        ))
                        .on_click(cx.listener(|this, _, _window, cx| {
                            let idx = REFRESH_INTERVALS
                                .iter()
                                .position(|&i| i == this.interval_secs)
                                .unwrap_or(0);
                            this.interval_secs =
                                REFRESH_INTERVALS[(idx + 1) % REFRESH_INTERVALS.len()];
                            cx.notify();
                        })),
                )
                .leading_button(
                    Button::new("am-pause")
                        .label(crate::localization::text(if self.paused {
                            "explorer.activity_monitor.auto_resume_button"
                        } else {
                            "explorer.activity_monitor.auto_pause_button"
                        }))
                        .on_click(cx.listener(|this, _, _window, cx| {
                            this.paused = !this.paused;
                            cx.notify();
                        })),
                )
                .leading_button(
                    Button::new("am-refresh")
                        .label(crate::localization::text(
                            "explorer.activity_monitor.refresh_button",
                        ))
                        .disabled(self.refresh_in_flight)
                        .on_click(cx.listener(|this, _, _window, cx| this.refresh(cx))),
                )
                // Action cluster, secondary first and the primary action last —
                // the same shape Query Insights uses for "Open Selected Query".
                .button(
                    Button::new("am-copy-sql")
                        .label(crate::localization::text(
                            "explorer.activity_monitor.copy_sql_button",
                        ))
                        .disabled(!has_sql)
                        .on_click(cx.listener(move |_this, _, _window, cx| {
                            if let Some(sql) = sql_for_copy.clone() {
                                cx.write_to_clipboard(gpui::ClipboardItem::new_string(sql));
                            }
                        })),
                )
                .button(
                    Button::new("am-copy-diagnostic")
                        .label(crate::localization::text(
                            "explorer.activity_monitor.copy_diagnostic_button",
                        ))
                        .disabled(!has_diagnostic)
                        .on_click(cx.listener(move |_this, _, _window, cx| {
                            if let Some(sql) = diagnostic.clone() {
                                cx.write_to_clipboard(gpui::ClipboardItem::new_string(sql));
                            }
                        })),
                )
                .button(
                    Button::new("am-script-termination")
                        .label(crate::localization::text(
                            "explorer.activity_monitor.script_termination_button",
                        ))
                        .disabled(!has_termination)
                        .on_click(cx.listener(move |this, _, _window, cx| {
                            this.pending_termination = termination.clone();
                            cx.notify();
                        })),
                )
                .button(
                    Button::new("am-open-sql")
                        .primary()
                        .label(crate::localization::text(
                            "explorer.activity_monitor.open_query_button",
                        ))
                        .disabled(!has_sql)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            if let Some(sql) = sql.clone() {
                                cx.emit(ToolDialogEvent::ScriptToEditor { sql });
                                this.close(window, cx);
                            }
                        })),
                )
        } else {
            dialog
        };
        if !self.windowed {
            dialog = dialog.detachable(|this, _window, cx| this.pop_out(cx));
        }
        dialog.build(self, window, cx)
    }
}

/// Compile-time association used by tests: every tab has a non-empty DMV
/// query behind it.
#[allow(dead_code)]
fn tab_queries() -> Vec<(String, String)> {
    let admin = sqlly_services::DefaultAdminToolsService;
    ActivityTab::all()
        .into_iter()
        .map(|t| (t.label(), t.query(&admin)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn column(name: &str) -> sqlly_core::ResultColumn {
        sqlly_core::ResultColumn {
            name: name.into(),
            data_type: "bigint".into(),
            nullable: true,
        }
    }

    fn sessions_fixture() -> (
        Vec<sqlly_core::ResultColumn>,
        Vec<Vec<sqlly_core::CellValue>>,
    ) {
        use sqlly_core::CellValue::{I64, Null, String as Text};
        (
            vec![
                column("session_id"),
                column("blocking_session_id"),
                column("total_elapsed_time"),
                column("status"),
            ],
            vec![
                vec![I64(10), Null, I64(5_000), Text("running".into())],
                vec![I64(11), I64(10), I64(60_000), Text("suspended".into())],
                vec![
                    I64(12),
                    I64(11),
                    I64(120_000),
                    Text("idle in transaction".into()),
                ],
            ],
        )
    }

    fn waits_fixture() -> (
        Vec<sqlly_core::ResultColumn>,
        Vec<Vec<sqlly_core::CellValue>>,
    ) {
        use sqlly_core::CellValue::{I64, String as Text};
        (
            vec![
                column("wait_type"),
                column("waiting_tasks_count"),
                column("wait_time_ms"),
            ],
            vec![
                vec![Text("PAGEIOLATCH_SH".into()), I64(100), I64(5_000)],
                vec![Text("CXPACKET".into()), I64(40), I64(2_000)],
            ],
        )
    }

    #[test]
    fn wait_delta_first_sample_is_null_and_recorded() {
        let (cols, rows) = waits_fixture();
        let (set, snapshot) = wait_delta_set(&cols, &rows, None);
        assert_eq!(set.columns.len(), 5);
        assert_eq!(set.columns[3].name, "waiting_tasks_count_delta");
        assert_eq!(set.columns[4].name, "wait_time_ms_delta");
        for row in &set.rows {
            assert_eq!(row[3], sqlly_core::CellValue::Null);
            assert_eq!(row[4], sqlly_core::CellValue::Null);
        }
        assert_eq!(
            snapshot.values["PAGEIOLATCH_SH"],
            vec![Some(100), Some(5_000)]
        );
    }

    #[test]
    fn wait_delta_reports_increase_since_previous_refresh() {
        use sqlly_core::CellValue::{I64, String as Text};
        let (cols, rows) = waits_fixture();
        let (_, snapshot) = wait_delta_set(&cols, &rows, None);
        let next = vec![
            vec![Text("PAGEIOLATCH_SH".into()), I64(130), I64(6_500)],
            // New wait type: no previous sample, delta stays Null.
            vec![Text("LCK_M_X".into()), I64(3), I64(900)],
        ];
        let (set, _) = wait_delta_set(&cols, &next, Some(&snapshot));
        assert_eq!(set.rows[0][3], I64(30));
        assert_eq!(set.rows[0][4], I64(1_500));
        assert_eq!(set.rows[1][3], sqlly_core::CellValue::Null);
        assert_eq!(set.rows[1][4], sqlly_core::CellValue::Null);
    }

    #[test]
    fn wait_delta_counter_reset_yields_null_not_negative() {
        use sqlly_core::CellValue::{I64, String as Text};
        let (cols, rows) = waits_fixture();
        let (_, snapshot) = wait_delta_set(&cols, &rows, None);
        let after_restart = vec![vec![Text("PAGEIOLATCH_SH".into()), I64(4), I64(90)]];
        let (set, _) = wait_delta_set(&cols, &after_restart, Some(&snapshot));
        assert_eq!(set.rows[0][3], sqlly_core::CellValue::Null);
        assert_eq!(set.rows[0][4], sqlly_core::CellValue::Null);
    }

    #[test]
    fn wait_delta_signature_change_invalidates_previous_snapshot() {
        use sqlly_core::CellValue::{I64, String as Text};
        let (cols, rows) = waits_fixture();
        let (_, snapshot) = wait_delta_set(&cols, &rows, None);
        let reshaped = vec![column("wait_type"), column("wait_time_ms")];
        let next = vec![vec![Text("PAGEIOLATCH_SH".into()), I64(9_000)]];
        let (set, _) = wait_delta_set(&reshaped, &next, Some(&snapshot));
        assert_eq!(set.rows[0][2], sqlly_core::CellValue::Null);
    }

    #[test]
    fn wait_delta_without_wait_type_column_passes_through() {
        use sqlly_core::CellValue::I64;
        let cols = vec![column("value")];
        let rows = vec![vec![I64(1)]];
        let (set, snapshot) = wait_delta_set(&cols, &rows, None);
        assert_eq!(set.columns.len(), 1);
        assert!(snapshot.values.is_empty());
    }

    #[test]
    fn wait_delta_skips_non_numeric_columns() {
        use sqlly_core::CellValue::{I64, String as Text};
        let cols = vec![column("wait_type"), column("note"), column("wait_time_ms")];
        let rows = vec![vec![Text("IO".into()), Text("descriptive".into()), I64(10)]];
        let (set, _) = wait_delta_set(&cols, &rows, None);
        let names: Vec<&str> = set.columns.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"wait_time_ms_delta"));
        assert!(!names.contains(&"note_delta"));
    }

    #[test]
    fn every_tab_has_a_query() {
        for (label, query) in tab_queries() {
            assert!(!query.trim().is_empty(), "{label} query is empty");
            assert!(
                query.to_ascii_uppercase().contains("SELECT"),
                "{label} query is not a SELECT"
            );
        }
    }

    #[test]
    fn sql_text_in_row_finds_query_columns_and_skips_handles() {
        use sqlly_core::{CellValue, ResultColumn};
        let cols = vec![
            ResultColumn {
                name: "sql_handle".into(),
                data_type: "varbinary".into(),
                nullable: true,
            },
            ResultColumn {
                name: "query_text".into(),
                data_type: "nvarchar".into(),
                nullable: true,
            },
        ];
        let row = vec![
            CellValue::String("0xDEAD".into()),
            CellValue::String("SELECT 1".into()),
        ];
        assert_eq!(sql_text_in_row(&cols, &row), Some("SELECT 1".into()));
        // Empty query text yields nothing rather than a blank tab.
        let row = vec![CellValue::Null, CellValue::String("  ".into())];
        assert_eq!(sql_text_in_row(&cols, &row), None);
    }

    #[test]
    fn refresh_interval_choices_cycle() {
        assert!(REFRESH_INTERVALS.contains(&AUTO_REFRESH_SECS));
        let idx = REFRESH_INTERVALS
            .iter()
            .position(|&i| i == AUTO_REFRESH_SECS)
            .unwrap();
        assert_ne!(REFRESH_INTERVALS[(idx + 1) % REFRESH_INTERVALS.len()], 0);
    }

    #[test]
    fn live_tabs_are_redis_only_in_the_strip() {
        let redis = ActivityTab::tabs_for(sqlly_core::DatabaseType::Redis);
        assert_eq!(redis.len(), 6);
        assert_eq!(redis[4], ActivityTab::Profiler);
        assert_eq!(redis[5], ActivityTab::PubSub);
        assert!(redis.iter().all(|tab| !tab.label().is_empty()));
        let postgres = ActivityTab::tabs_for(sqlly_core::DatabaseType::Postgres);
        assert_eq!(postgres.len(), 4);
        assert!(!postgres.iter().any(|tab| tab.is_live()));
        assert!(ActivityTab::Profiler.is_live());
        assert!(ActivityTab::PubSub.is_live());
        assert!(!ActivityTab::Sessions.is_live());
    }

    #[test]
    fn slowlog_threshold_input_builds_a_reviewed_config_set() {
        use sqlly_services::ActivityDialect;
        let redis = Some(ActivityDialect::Redis);
        assert_eq!(
            slowlog_set_statement(redis, " 10000 "),
            Some("CONFIG SET slowlog-log-slower-than 10000".to_string())
        );
        assert_eq!(
            slowlog_set_statement(redis, "10_000"),
            Some("CONFIG SET slowlog-log-slower-than 10000".to_string())
        );
        assert_eq!(
            slowlog_set_statement(redis, "-1"),
            Some("CONFIG SET slowlog-log-slower-than -1".to_string())
        );
        assert_eq!(slowlog_set_statement(redis, "-2"), None);
        assert_eq!(slowlog_set_statement(redis, "10ms"), None);
        assert_eq!(slowlog_set_statement(redis, ""), None);
        assert_eq!(
            slowlog_set_statement(Some(ActivityDialect::Postgres), "10"),
            None
        );
        assert_eq!(slowlog_set_statement(None, "10"), None);
    }

    #[test]
    fn slowlog_config_writes_are_blocked_only_while_production_is_locked() {
        use sqlly_intel::query::safety::ProductionDecision;
        assert!(
            slowlog_write_block_reason(ProductionDecision::ReadOnly)
                .unwrap()
                .contains("Production is read-only")
        );
        assert!(slowlog_write_block_reason(ProductionDecision::WritesUnlocked).is_none());
        assert!(slowlog_write_block_reason(ProductionDecision::NotProduction).is_none());
    }

    #[test]
    fn keyspace_events_preset_expands_for_the_profile_database() {
        let mut profile = sqlly_core::ConnectionProfile::sql_login(
            "r",
            "cache.example.com",
            "u",
            sqlly_core::SecretRef {
                provider: sqlly_core::SecretProvider::InMemory,
                key: "p".into(),
            },
        );
        profile.database_type = sqlly_core::DatabaseType::Redis;
        profile.database = Some("3".into());
        let db = preset_database_index(&profile);
        assert_eq!(db, 3);
        let preset = sqlly_services::redis_live::key_events_preset(db, "");
        assert_eq!(
            sqlly_services::redis_live::parse_subscribe_targets(&preset),
            vec![sqlly_services::redis_live::SubscribeTarget::Pattern(
                "__keyevent@3__:*".into()
            )]
        );
        assert_eq!(
            sqlly_services::redis_live::parse_subscribe_targets(
                &sqlly_services::redis_live::key_events_preset(db, "user:*")
            ),
            vec![
                sqlly_services::redis_live::SubscribeTarget::Pattern("__keyevent@3__:*".into()),
                sqlly_services::redis_live::SubscribeTarget::Pattern(
                    "__keyspace@3__:user:*".into()
                ),
            ]
        );
        profile.database = None;
        assert_eq!(preset_database_index(&profile), 0);
    }

    #[test]
    fn keyspace_notification_rows_split_event_and_key() {
        let expired = sqlly_engines::redis::live::shape_pubsub_message(
            b"__keyevent@3__:expired",
            Some(b"__keyevent@3__:*"),
            b"user:42",
            0,
        );
        let row = pubsub_row(&expired);
        assert_eq!(row.len(), 6);
        assert_eq!(
            row[1],
            sqlly_core::CellValue::String("__keyevent@3__:expired".into())
        );
        assert_eq!(row[3], sqlly_core::CellValue::String("expired".into()));
        assert_eq!(row[4], sqlly_core::CellValue::String("user:42".into()));
        assert_eq!(row[5], sqlly_core::CellValue::String("user:42".into()));
        let plain = sqlly_engines::redis::live::shape_pubsub_message(b"news", None, b"hi", 0);
        let row = pubsub_row(&plain);
        assert_eq!(row[3], sqlly_core::CellValue::Null);
        assert_eq!(row[4], sqlly_core::CellValue::Null);
    }

    #[test]
    fn keyspace_notify_flags_parse_and_decide_whether_key_events_publish() {
        use sqlly_core::CellValue;
        let rows = vec![vec![
            CellValue::String("notify-keyspace-events".into()),
            CellValue::String("KEA".into()),
        ]];
        assert_eq!(
            keyspace_notify_flags_from_rows(&rows).as_deref(),
            Some("KEA")
        );
        let off = vec![vec![
            CellValue::String("notify-keyspace-events".into()),
            CellValue::String("".into()),
        ]];
        assert_eq!(keyspace_notify_flags_from_rows(&off).as_deref(), Some(""));
        let other = vec![vec![
            CellValue::String("maxmemory".into()),
            CellValue::String("0".into()),
        ]];
        assert_eq!(keyspace_notify_flags_from_rows(&other), None);
        assert_eq!(keyspace_notify_flags_from_rows(&[]), None);

        assert!(keyspace_events_on("KEA"));
        assert!(keyspace_events_on("Ex"));
        assert!(keyspace_events_on("AKE"));
        assert!(!keyspace_events_on(""));
        assert!(
            !keyspace_events_on("K$"),
            "keyspace only: no __keyevent channels"
        );
        assert!(!keyspace_events_on("E"), "no event class selected");

        assert!(keyspace_notify_note(Some("")).contains("off"));
        assert!(keyspace_notify_note(Some("KEA")).contains("on"));
        assert!(keyspace_notify_note(Some("K$")).contains("stay silent"));
        assert!(keyspace_notify_note(None).contains("Could not read"));
        assert_eq!(
            KEYSPACE_NOTIFY_ENABLE,
            "CONFIG SET notify-keyspace-events KEA"
        );
        assert_eq!(KEYSPACE_NOTIFY_QUERY, "CONFIG GET notify-keyspace-events");
    }

    #[test]
    fn time_box_parsing_clamps_and_defaults() {
        let default = sqlly_services::redis_live::DEFAULT_PROFILER_TIME_BOX_SECS;
        assert_eq!(
            default, 300,
            "the toolbar field's literal default must match"
        );
        assert_eq!(parse_time_box_secs("60"), 60);
        assert_eq!(parse_time_box_secs(" 5 "), 5);
        assert_eq!(parse_time_box_secs(""), default);
        assert_eq!(parse_time_box_secs("0"), default);
        assert_eq!(parse_time_box_secs("soon"), default);
        assert_eq!(
            parse_time_box_secs("999999"),
            sqlly_services::redis_live::MAX_PROFILER_TIME_BOX_SECS
        );
    }

    #[test]
    fn live_cap_parsing_clamps_and_defaults() {
        assert_eq!(parse_live_cap("5000", 10_000), 5000);
        assert_eq!(parse_live_cap(" 250 ", 10_000), 250);
        assert_eq!(parse_live_cap("", 10_000), 10_000);
        assert_eq!(parse_live_cap("abc", 10_000), 10_000);
        assert_eq!(parse_live_cap("0", 10_000), 10_000);
        assert_eq!(parse_live_cap("-3", 10_000), 10_000);
        assert_eq!(parse_live_cap("999999999", 10_000), MAX_LIVE_CAP);
    }

    #[test]
    fn clock_time_formats_epoch_values() {
        // Both helpers must produce HH:MM:SS.mmm-shaped local text and
        // never panic on odd input.
        let text = clock_time(1_751_524_000.123_5);
        assert_eq!(text.len(), 12, "{text}");
        assert_eq!(&text[2..3], ":");
        assert_eq!(&text[8..9], ".");
        assert_eq!(clock_time_millis(1_751_524_000_500).len(), 12);
        assert!(!clock_time(f64::NAN).is_empty());
    }

    #[test]
    fn hex_preview_bounds_binary_display() {
        assert_eq!(hex_preview(&[0xde, 0xad]), "dead");
        let long: Vec<u8> = (0..32).collect();
        let preview = hex_preview(&long);
        assert!(preview.ends_with('…'));
        assert!(preview.starts_with("00010203"));
    }

    #[test]
    fn tab_labels_are_distinct() {
        let labels: Vec<_> = ActivityTab::all().iter().map(|t| t.label()).collect();
        let mut unique = labels.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), labels.len());
    }

    #[test]
    fn blocking_chains_resolve_roots_depth_and_blocker_counts() {
        let (columns, rows) = sessions_fixture();
        let (positions, children) = blocking_positions(ActivityTab::Sessions, &columns, &rows);
        assert_eq!(positions[&10].root, Some(10));
        assert_eq!(positions[&10].depth, 0);
        assert_eq!(positions[&11].root, Some(10));
        assert_eq!(positions[&11].depth, 1);
        assert_eq!(positions[&12].root, Some(10));
        assert_eq!(positions[&12].depth, 2);
        assert_eq!(children[&10], 1);
        assert_eq!(children[&11], 1);
    }

    #[test]
    fn blocking_chains_are_cycle_safe() {
        use sqlly_core::CellValue::I64;
        let columns = vec![column("session_id"), column("blocking_session_id")];
        let rows = vec![vec![I64(20), I64(21)], vec![I64(21), I64(20)]];
        let (positions, _) = blocking_positions(ActivityTab::Sessions, &columns, &rows);
        assert!(positions[&20].cycle);
        assert!(positions[&21].cycle);
        assert_eq!(positions[&20].root, Some(20));
        assert_eq!(positions[&21].root, Some(20));
    }

    #[test]
    fn triage_filters_blockers_blocked_long_running_and_idle_transactions() {
        let (columns, rows) = sessions_fixture();
        let blockers = triaged_set(
            ActivityTab::Sessions,
            ActivityTriageFilter::Blockers,
            &columns,
            &rows,
        );
        assert_eq!(blockers.rows.len(), 2);
        let blocked = triaged_set(
            ActivityTab::Sessions,
            ActivityTriageFilter::Blocked,
            &columns,
            &rows,
        );
        assert_eq!(blocked.rows.len(), 2);
        let long_running = triaged_set(
            ActivityTab::Sessions,
            ActivityTriageFilter::LongRunning,
            &columns,
            &rows,
        );
        assert_eq!(long_running.rows.len(), 2);
        let idle = triaged_set(
            ActivityTab::Sessions,
            ActivityTriageFilter::IdleInTransaction,
            &columns,
            &rows,
        );
        assert_eq!(idle.rows.len(), 1);
        assert_eq!(long_running.columns.last().unwrap().name, "running_seconds");
    }

    #[test]
    fn blocking_rows_target_the_blocker_for_reviewed_intervention() {
        use sqlly_core::CellValue::String;
        let columns = vec![column("blocking_pid"), column("waiting_pid")];
        let row = [String("44".into()), String("45".into())];
        let target = session_column(ActivityTab::Blocking, &columns)
            .and_then(|index| cell_i64(row.get(index)));
        assert_eq!(target, Some(44));
    }

    #[test]
    fn permission_feedback_is_engine_specific() {
        assert!(
            permission_guidance(sqlly_core::DatabaseType::SqlServer).contains("VIEW SERVER STATE")
        );
        assert!(permission_guidance(sqlly_core::DatabaseType::Postgres).contains("pg_monitor"));
        assert!(permission_guidance(sqlly_core::DatabaseType::MySql).contains("PROCESS"));
        assert!(permission_guidance(sqlly_core::DatabaseType::Sqlite).contains("does not expose"));
    }

    #[gpui::test]
    fn triaged_rows_build_a_real_sortable_filterable_grid(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            if cx.try_global::<gpui_component::Theme>().is_none() {
                gpui_component::init(cx);
            }
            let (columns, rows) = sessions_fixture();
            let set = triaged_set(
                ActivityTab::Sessions,
                ActivityTriageFilter::All,
                &columns,
                &rows,
            );
            assert!(crate::tool_dialog::build_result_grid(&set, cx).is_some());
        });
    }

    #[test]
    fn attention_filter_hidden_without_provider() {
        // The cycle can only reach Attention while the pane offers it and
        // the per-pane switch is on: a full cycle without that never yields
        // the filter.
        let mut filter = ActivityTriageFilter::All;
        for _ in 0..10 {
            filter = filter.next(false);
            assert_ne!(filter, ActivityTriageFilter::Attention);
        }
        // With the switch on it slots in after Idle in transaction.
        assert_eq!(
            ActivityTriageFilter::IdleInTransaction.next(true),
            ActivityTriageFilter::Attention
        );
        assert_eq!(
            ActivityTriageFilter::Attention.next(true),
            ActivityTriageFilter::All
        );
    }

    #[test]
    fn existing_filters_unchanged() {
        // Attention shares `triaged_set` with the rule filters: selecting it
        // keeps every row and every derived column exactly as All does (the
        // model ordering happens afterwards), and ordering with no verdicts
        // hides nothing — so with the decision model absent the pane is
        // byte-for-byte what it was before this feature existed.
        let (columns, rows) = sessions_fixture();
        let all = triaged_set(
            ActivityTab::Sessions,
            ActivityTriageFilter::All,
            &columns,
            &rows,
        );
        let attention = triaged_set(
            ActivityTab::Sessions,
            ActivityTriageFilter::Attention,
            &columns,
            &rows,
        );
        assert_eq!(all.columns, attention.columns);
        assert_eq!(all.rows, attention.rows);
        let ordered = attention_ordered(attention, &[]);
        assert_eq!(ordered.rows.len(), all.rows.len());
        assert_eq!(ordered.columns.last().unwrap().name, "attention");
    }

    #[test]
    fn attention_orders_act_then_watch_and_hides_ignore() {
        use crate::admin_triage::Attention;
        let set = ToolQuerySet {
            columns: vec![column("session_id")],
            rows: (0..4)
                .map(|i| vec![sqlly_core::CellValue::I64(i)])
                .collect(),
        };
        let verdicts = vec![
            (Attention::Ignore, 0.9),
            (Attention::Watch, 0.8),
            (Attention::Act, 0.7),
            (Attention::Watch, 0.9),
        ];
        let ordered = attention_ordered(set, &verdicts);
        let ids: Vec<i64> = ordered
            .rows
            .iter()
            .map(|row| match &row[0] {
                sqlly_core::CellValue::I64(id) => *id,
                other => panic!("unexpected cell {other:?}"),
            })
            .collect();
        assert_eq!(ids, vec![2, 3, 1], "Act first, then Watch by confidence");
        let labels: Vec<&str> = ordered
            .rows
            .iter()
            .map(|row| match row.last() {
                Some(sqlly_core::CellValue::String(label)) => label.as_str(),
                other => panic!("unexpected attention cell {other:?}"),
            })
            .collect();
        assert_eq!(labels, vec!["Act", "Watch", "Watch"]);
    }

    #[test]
    fn live_tabs_follow_the_capability_on_every_resp_engine() {
        use sqlly_core::DatabaseType;
        for db in [
            DatabaseType::Redis,
            DatabaseType::Valkey,
            DatabaseType::KeyDb,
            DatabaseType::DragonflyDb,
            DatabaseType::AmazonElastiCache,
            DatabaseType::AmazonMemoryDb,
        ] {
            let tabs = ActivityTab::tabs_for(db);
            assert!(tabs.contains(&ActivityTab::Profiler), "{db:?}");
            assert!(tabs.contains(&ActivityTab::PubSub), "{db:?}");
            // The palette gate and the tab gate are the same answer.
            for tab in ActivityTab::live() {
                assert!(tab.offered_for(db), "{db:?} {tab:?}");
            }
        }
        assert!(!ActivityTab::Profiler.offered_for(DatabaseType::Postgres));
        assert!(ActivityTab::Sessions.offered_for(DatabaseType::Postgres));
    }

    #[test]
    fn live_tools_refuse_relay_routed_connections() {
        let mut profile = sqlly_core::ConnectionProfile::sql_login(
            "r",
            "cache.internal",
            "",
            sqlly_core::SecretRef {
                provider: sqlly_core::SecretProvider::InMemory,
                key: String::new(),
            },
        );
        profile.database_type = sqlly_core::DatabaseType::Redis;
        assert_eq!(live_refusal_for(&profile, ActivityTab::Profiler), None);
        profile.route = sqlly_core::connection::ConnectionRoute::ViaRelay {
            relay_device_id: "dev".into(),
            pair_id: "pair".into(),
            relay_hint: None,
        };
        let refusal = live_refusal_for(&profile, ActivityTab::Profiler).expect("refused");
        assert!(
            refusal.contains("Redis Profiler") && refusal.contains("relay"),
            "{refusal}"
        );
        assert!(live_refusal_for(&profile, ActivityTab::PubSub).is_some());
        // The DMV tabs run through the routed services and stay available.
        assert_eq!(live_refusal_for(&profile, ActivityTab::Sessions), None);
    }

    #[test]
    fn live_selection_follows_its_entry_as_new_rows_arrive() {
        use sqlly_datatable::Selection;
        // Newest first: rows 12, 11, 10 become 14, 13, 12, 11 (10 evicted).
        let old = [12, 11, 10];
        let new = [14, 13, 12, 11];
        assert_eq!(
            remap_live_selection(&Selection::Cell(1, 3), &old, &new),
            Selection::Cell(3, 3)
        );
        assert_eq!(
            remap_live_selection(&Selection::Row(0), &old, &new),
            Selection::Row(2)
        );
        assert_eq!(
            remap_live_selection(&Selection::CellRange(0, 1, 1, 2), &old, &new),
            Selection::Cell(2, 1)
        );
        // The selected entry was evicted: nothing stays selected rather than
        // silently pointing at a different message.
        assert_eq!(
            remap_live_selection(&Selection::Cell(2, 0), &old, &new),
            Selection::None
        );
        assert_eq!(
            remap_live_selection(&Selection::Column(2), &old, &new),
            Selection::Column(2)
        );
        assert_eq!(reanchor_row(&old, &new, 9), None);
    }

    #[test]
    fn live_status_discloses_the_display_cap() {
        assert_eq!(
            live_shown_note(LIVE_DISPLAY_CAP, 10_000),
            format!("showing the newest {LIVE_DISPLAY_CAP} of 10000")
        );
        assert_eq!(live_shown_note(12, 500), "showing 12 of 500");
        assert_eq!(live_cell_text("a\r\nb\nc"), "a b c");
        let long = "é".repeat(LIVE_CELL_CHARS + 5);
        let cell = live_cell_text(&long);
        assert_eq!(
            cell.chars().count(),
            LIVE_CELL_CHARS + 1,
            "prefix plus the ellipsis"
        );
        assert!(cell.ends_with('…'));
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn truncated_pubsub_payloads_say_so_in_the_cell_and_the_preview() {
        use sqlly_services::redis_live::MAX_STORED_PAYLOAD_BYTES;
        let big = vec![b'{'; MAX_STORED_PAYLOAD_BYTES + 500];
        let message = sqlly_engines::redis::live::shape_pubsub_message(b"c", None, &big, 0);
        let cell = pubsub_payload_cell(&message);
        assert!(
            cell.ends_with(&format!("(truncated, {} bytes)", big.len())),
            "{cell}"
        );
        let notice = pubsub_truncation_notice(&message).expect("truncated");
        assert!(
            notice.contains(&format!("of {} bytes", big.len())),
            "{notice}"
        );

        let mut binary = vec![0xffu8; MAX_STORED_PAYLOAD_BYTES + 1];
        binary[0] = 0;
        let message = sqlly_engines::redis::live::shape_pubsub_message(b"c", None, &binary, 0);
        let cell = pubsub_payload_cell(&message);
        assert!(
            cell.ends_with(&format!("({} bytes, truncated)", binary.len())),
            "{cell}"
        );

        let small = sqlly_engines::redis::live::shape_pubsub_message(b"c", None, b"hi", 0);
        assert_eq!(pubsub_payload_cell(&small), "hi");
        assert!(pubsub_truncation_notice(&small).is_none());
    }

    /// Regression: every tick used to build a brand-new grid, resetting
    /// scroll and selection. The live grid must be the same entity after a
    /// refresh, with the selection still on the same message.
    #[gpui::test]
    fn live_rows_update_in_place_and_keep_the_selection(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            if cx.try_global::<gpui_component::Theme>().is_none() {
                gpui_component::init(cx);
            }
            let columns = vec![
                live_column("channel", "string"),
                live_column("payload", "string"),
            ];
            let row = |text: &str| {
                vec![
                    sqlly_core::CellValue::String("news".into()),
                    sqlly_core::CellValue::String(text.into()),
                ]
            };
            let mut grid = None;
            apply_live_rows(
                &mut grid,
                &[],
                columns.clone(),
                vec![row("b"), row("a")],
                &[1, 0],
                cx,
            );
            let first = grid.clone().expect("grid built");
            let state = first.read(cx).state.clone();
            state.update(cx, |state, _| {
                state.selection = sqlly_datatable::Selection::Cell(0, 1)
            });

            apply_live_rows(
                &mut grid,
                &[1, 0],
                columns,
                vec![row("c"), row("b"), row("a")],
                &[2, 1, 0],
                cx,
            );
            let second = grid.expect("grid kept");
            assert_eq!(first.entity_id(), second.entity_id(), "same grid entity");
            let state = second.read(cx).state.read(cx);
            assert_eq!(state.data.rows.len(), 3);
            assert_eq!(
                state.selection,
                sqlly_datatable::Selection::Cell(1, 1),
                "still on message b"
            );
            assert_eq!(selected_source_row(&second, cx), Some(1));
        });
    }

    /// The pane's grid, drawn in the real pane, gets room for its rows (a
    /// grid in an auto-height wrapper paints zero pixels tall).
    #[gpui::test]
    fn activity_monitor_grid_gets_room_to_paint_its_rows(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            if cx.try_global::<gpui_component::Theme>().is_none() {
                gpui_component::init(cx);
            }
        });
        let context =
            crate::tool_dialog::unreachable_tool_context(sqlly_core::DatabaseType::SqlServer);
        let (view, cx) = cx.add_window_view(|window, cx| {
            ActivityMonitorPane::new(
                context,
                crate::tool_dialog::ToolScope::Server,
                ActivityTab::Sessions,
                window,
                cx,
            )
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
            crate::tool_dialog::assert_grid_shows_rows(grid, 3, "Activity Monitor", cx);
        });
    }
}
