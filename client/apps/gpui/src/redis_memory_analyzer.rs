//! Redis Memory Analyzer pane — read-only reports over a bounded sample of
//! one logical database: memory by data type, memory by key prefix (one to
//! three namespace levels deep, split on a chosen set of delimiters), the
//! largest keys, and the TTL distribution. The drill-down companion to Disk Usage, which
//! sees the per-database overview this reports zoom into.
//!
//! Every report is derived from ONE bounded sample per refresh (the driver
//! memoizes the run's sample), so the three reports agree with each other
//! and the server is walked once. A one-row summary says how big the
//! sample was, whether it covered the whole keyspace (exact) or not
//! (estimates extrapolated), and why bytes could not be measured when
//! `MEMORY USAGE` is unavailable (unknown command, ACL `NOPERM`) — in which
//! case the by-type and TTL reports still show their key counts.
//!
//! Read-only observer: one refresh in flight, a generation token against
//! stale responses, a Stop that cancels the sample on the server (the
//! driver's cancel drops the connection between chunks, so the walk never
//! runs on unattended), and nothing here ever writes to the server.

use gpui::{
    Entity, EventEmitter, FocusHandle, IntoElement, ParentElement, Render, Styled, Window, div, px,
};
use gpui_component::button::Button;
use gpui_component::{Disableable as _, Selectable as _, Sizable as _};
use sqlly_core::CellValue;
use sqlly_datatable::SqllyDataTable;

use crate::dialog::{Dialog, DialogFrame};
use crate::schema_tree::redis::{
    MEMORY_ANALYSIS_SAMPLE, MEMORY_ANALYSIS_TOP, memory_analysis_script,
};
use crate::theme::Theme;
use crate::tool_dialog::{ToolDialogContext, ToolDialogEvent};

impl EventEmitter<ToolDialogEvent> for RedisMemoryPane {}

/// The summary plus the report commands, one result set each, all over
/// the same bounded sample of the logical database the pane was opened for
/// (same size ⇒ the driver reuses the run's sample): memory by type, top
/// keys, TTL buckets, then the by-prefix report at every depth the pane
/// offers, so switching depth never resamples. The editable variant users
/// can copy (with its honest-bounds header comment) lives in
/// `memory_analysis_script`.
pub fn report_script(delimiters: &[String]) -> String {
    let mut script = format!(
        "SQLLY.MEMSUMMARY {MEMORY_ANALYSIS_SAMPLE}\n\
         SQLLY.MEMBYTYPE {MEMORY_ANALYSIS_SAMPLE}\n\
         SQLLY.TOPKEYS {MEMORY_ANALYSIS_SAMPLE} {MEMORY_ANALYSIS_TOP}\n\
         SQLLY.TTLBUCKETS {MEMORY_ANALYSIS_SAMPLE}"
    );
    let delimiters = delimiter_argument(delimiters);
    for depth in 1..=PREFIX_DEPTHS {
        script.push_str(&format!(
            "\nSQLLY.MEMBYPREFIX {MEMORY_ANALYSIS_SAMPLE} {depth} {delimiters}"
        ));
    }
    script
}

/// The delimiter set as one `SQLLY.MEMBYPREFIX` argument: space-separated
/// (so a multi-character delimiter survives) and quoted when it needs it.
pub fn delimiter_argument(delimiters: &[String]) -> String {
    let joined = if delimiters.is_empty() {
        sqlly_core::redis_keys::DEFAULT_KEY_DELIMITER.to_string()
    } else {
        delimiters.join(" ")
    };
    sqlly_intel::query::safety::redis::quote_argument(joined.as_bytes())
}

/// Result-set index of the summary row; the reports follow it.
const SUMMARY_SET: usize = 0;
/// Prefix depths the by-prefix report offers (1..=this).
pub const PREFIX_DEPTHS: usize = 3;
/// Result-set index of the depth-1 by-prefix report; deeper ones follow.
const PREFIX_SET: usize = SUMMARY_SET + 4;

/// Delimiter characters the by-prefix report offers besides the
/// connection's own.
const COMMON_DELIMITERS: [&str; 7] = [":", "|", "/", ".", "#", "-", "_"];

/// The connection's own delimiters (its key-browser setting), the default
/// selection of the by-prefix report.
pub fn default_delimiters(profile: &sqlly_core::ConnectionProfile) -> Vec<String> {
    sqlly_core::redis_keys::parse_delimiters(&sqlly_core::redis_keys::effective_delimiter(profile))
}

/// The delimiter chips on offer: the connection's own first, then the
/// common ones it does not already name.
pub fn delimiter_choices(defaults: &[String]) -> Vec<String> {
    let mut out = defaults.to_vec();
    for common in COMMON_DELIMITERS {
        if !out.iter().any(|d| d == common) {
            out.push(common.to_string());
        }
    }
    out
}

/// Toggle one delimiter in the selection, never leaving it empty. Returns
/// whether the selection changed.
pub fn toggle_delimiter(selected: &mut Vec<String>, delimiter: &str) -> bool {
    if let Some(at) = selected.iter().position(|d| d == delimiter) {
        if selected.len() == 1 {
            return false;
        }
        selected.remove(at);
    } else {
        selected.push(delimiter.to_string());
    }
    true
}

/// What `SQLLY.MEMSUMMARY` said about the sample.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SampleSummary {
    pub database: Option<i64>,
    pub sampled_keys: i64,
    pub total_keys: i64,
    pub complete: bool,
    pub vanished_keys: i64,
    pub memory_measured: bool,
    pub memory_note: Option<String>,
}

impl SampleSummary {
    /// Read the summary set's one row by column name (so a reordered or
    /// widened summary never misreads a field).
    pub fn from_set(set: &crate::tool_dialog::ToolQuerySet) -> Option<Self> {
        let row = set.rows.first()?;
        let cell = |name: &str| {
            set.columns
                .iter()
                .position(|c| c.name == name)
                .and_then(|i| row.get(i))
        };
        let int = |name: &str| match cell(name) {
            Some(CellValue::I64(n)) => Some(*n),
            _ => None,
        };
        let flag = |name: &str| matches!(cell(name), Some(CellValue::Bool(true)));
        Some(SampleSummary {
            database: int("database"),
            sampled_keys: int("sampled_keys").unwrap_or(0),
            total_keys: int("total_keys").unwrap_or(0),
            complete: flag("complete"),
            vanished_keys: int("vanished_keys").unwrap_or(0),
            memory_measured: flag("memory_measured"),
            memory_note: match cell("memory_note") {
                Some(CellValue::String(text)) => Some(text.clone()),
                _ => None,
            },
        })
    }

    /// The footer line: sample size, exact vs extrapolated, db.
    pub fn describe(&self) -> String {
        let sampled = self.sampled_keys.to_string();
        let total = self.total_keys.to_string();
        let database = self.database.map(|db| db.to_string()).unwrap_or_default();
        let (key, arguments): (&str, Vec<(&str, &str)>) =
            match (self.database.is_some(), self.complete) {
                (true, true) => (
                    "explorer.redis_memory.sample_exact_database",
                    vec![
                        ("database", database.as_str()),
                        ("sampled", sampled.as_str()),
                        ("total", total.as_str()),
                    ],
                ),
                (false, true) => (
                    "explorer.redis_memory.sample_exact",
                    vec![("sampled", sampled.as_str()), ("total", total.as_str())],
                ),
                (true, false) => (
                    "explorer.redis_memory.sample_partial_database",
                    vec![
                        ("database", database.as_str()),
                        ("sampled", sampled.as_str()),
                        ("total", total.as_str()),
                    ],
                ),
                (false, false) => (
                    "explorer.redis_memory.sample_partial",
                    vec![("sampled", sampled.as_str()), ("total", total.as_str())],
                ),
            };
        let mut text = crate::localization::format(key, &arguments);
        if self.vanished_keys > 0 {
            text.push_str(&crate::localization::format(
                "explorer.redis_memory.sample_vanished",
                &[("count", &self.vanished_keys.to_string())],
            ));
        }
        text
    }
}

/// Titles for the report slots, in result-set order.
pub const REPORT_TITLES: [&str; 3] = [
    "explorer.redis_memory.report_by_type",
    "explorer.redis_memory.report_top_keys",
    "explorer.redis_memory.report_ttl_distribution",
];

fn report_title(slot: usize) -> String {
    crate::localization::text(REPORT_TITLES[slot])
}

fn pane_title(connection: &str) -> String {
    crate::localization::format("explorer.redis_memory.title", &[("connection", connection)])
}

pub struct RedisMemoryPane {
    context: ToolDialogContext,
    grids: [Option<Entity<SqllyDataTable>>; 3],
    row_counts: [usize; 3],
    /// The by-prefix report at depths 1..=3 (all from the same sample).
    prefix_grids: [Option<Entity<SqllyDataTable>>; PREFIX_DEPTHS],
    prefix_rows: [usize; PREFIX_DEPTHS],
    /// Which by-prefix depth is on screen (1-based).
    prefix_depth: usize,
    /// The delimiters the by-prefix report splits names on; changing them
    /// takes a new sample.
    delimiters: Vec<String>,
    summary: Option<SampleSummary>,
    error: Option<String>,
    loading: bool,
    refresh_generation: u64,
    refresh_in_flight: bool,
    /// The sample in flight, so Stop can cancel it.
    cancel: Option<crate::tool_dialog::QueryCancel>,
    /// The last sample was stopped: the reports (if any) are the previous
    /// sample's, and the footer says so.
    stopped: bool,
    focus_handle: FocusHandle,
    frame: DialogFrame,
    /// True once popped out into its own OS window: the window boundary is
    /// the dialog boundary and close tears the window down.
    windowed: bool,
}

impl RedisMemoryPane {
    pub fn new(context: ToolDialogContext, cx: &mut gpui::Context<Self>) -> Self {
        let delimiters = default_delimiters(&context.profile);
        let mut this = Self {
            context,
            grids: [None, None, None],
            row_counts: [0; 3],
            prefix_grids: [None, None, None],
            prefix_rows: [0; PREFIX_DEPTHS],
            prefix_depth: 1,
            delimiters,
            summary: None,
            error: None,
            loading: true,
            refresh_generation: 0,
            refresh_in_flight: false,
            cancel: None,
            stopped: false,
            focus_handle: cx.focus_handle(),
            // Fixed frame: the embedded data grids manage their own scrolling.
            frame: DialogFrame::new(Theme::scaled(860.0), Theme::scaled(760.0)),
            windowed: false,
        };
        this.refresh(cx);
        this
    }

    /// Point an already-open pane at another connection or database: drop
    /// the old reports and resample. A refresh still in flight for the old
    /// target is discarded by the generation bump.
    pub fn retarget(&mut self, context: ToolDialogContext, cx: &mut gpui::Context<Self>) {
        self.delimiters = default_delimiters(&context.profile);
        self.context = context;
        self.grids = [None, None, None];
        self.row_counts = [0; 3];
        self.prefix_grids = [None, None, None];
        self.prefix_rows = [0; PREFIX_DEPTHS];
        self.summary = None;
        self.error = None;
        self.loading = true;
        self.refresh_in_flight = false;
        self.cancel = None;
        self.stopped = false;
        self.refresh_generation += 1;
        self.refresh(cx);
    }

    /// The profile this pane reports on.
    pub fn profile(&self) -> &sqlly_core::ConnectionProfile {
        &self.context.profile
    }

    fn refresh(&mut self, cx: &mut gpui::Context<Self>) {
        if self.refresh_in_flight {
            return;
        }
        self.refresh_in_flight = true;
        self.loading = true;
        self.stopped = false;
        self.refresh_generation += 1;
        let generation = self.refresh_generation;
        let cancel = crate::tool_dialog::run_query_cancellable(
            &self.context,
            report_script(&self.delimiters),
            crate::tool_dialog::TOOL_QUERY_ROW_CAP,
            cx,
            move |this, outcome, cx| {
                if this.refresh_generation != generation {
                    return;
                }
                this.refresh_in_flight = false;
                this.loading = false;
                this.cancel = None;
                // A stopped sample leaves the previous reports in place —
                // nothing new was measured, so nothing old is replaced.
                if outcome.error.as_deref() == Some(crate::tool_dialog::CANCELLED_MESSAGE) {
                    this.stopped = true;
                    cx.notify();
                    return;
                }
                // Keep every report that arrived before an error: a later
                // report failing must not blank the earlier ones.
                this.error = outcome.error;
                this.summary = outcome
                    .sets
                    .get(SUMMARY_SET)
                    .and_then(SampleSummary::from_set);
                for slot in 0..3 {
                    match outcome.sets.get(SUMMARY_SET + 1 + slot) {
                        Some(set) => {
                            this.row_counts[slot] = set.rows.len();
                            this.grids[slot] = crate::tool_dialog::build_result_grid(set, cx);
                        }
                        None => {
                            this.row_counts[slot] = 0;
                            this.grids[slot] = None;
                        }
                    }
                }
                for depth in 0..PREFIX_DEPTHS {
                    match outcome.sets.get(PREFIX_SET + depth) {
                        Some(set) => {
                            this.prefix_rows[depth] = set.rows.len();
                            this.prefix_grids[depth] =
                                crate::tool_dialog::build_result_grid(set, cx);
                        }
                        None => {
                            this.prefix_rows[depth] = 0;
                            this.prefix_grids[depth] = None;
                        }
                    }
                }
                cx.notify();
            },
        );
        self.cancel = Some(cancel);
        cx.notify();
    }

    /// Show the by-prefix report at another depth (already sampled).
    fn set_prefix_depth(&mut self, depth: usize, cx: &mut gpui::Context<Self>) {
        self.prefix_depth = depth.clamp(1, PREFIX_DEPTHS);
        cx.notify();
    }

    /// Add or remove a delimiter and take a new sample split on the new set.
    fn toggle_prefix_delimiter(&mut self, delimiter: &str, cx: &mut gpui::Context<Self>) {
        if toggle_delimiter(&mut self.delimiters, delimiter) {
            if self.refresh_in_flight {
                // The sample in flight splits on the old set: drop it and
                // resample with the new one.
                if let Some(cancel) = self.cancel.take() {
                    cancel.cancel(cx);
                }
                self.refresh_in_flight = false;
            }
            self.refresh(cx);
        }
        cx.notify();
    }

    /// Stop the sample in flight. The driver's cancel is honoured between
    /// the walk's pages and usage chunks, so the server stops promptly and
    /// the pane keeps whatever the previous sample showed.
    fn stop(&mut self, cx: &mut gpui::Context<Self>) {
        if let Some(cancel) = self.cancel.take() {
            cancel.cancel(cx);
        }
        cx.notify();
    }

    /// Hand the user the commented, editable analyzer script so a custom
    /// sample size or a longer top list stays one paste away.
    fn copy_editable_script(&mut self, cx: &mut gpui::Context<Self>) {
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(memory_analysis_script()));
        cx.emit(ToolDialogEvent::Status {
            level: crate::app::StatusLevel::Success,
            message: crate::localization::text("explorer.redis_memory.script_copied"),
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
    /// same pane — grids and in-flight refresh included — carries on in the
    /// window.
    fn pop_out(&mut self, cx: &mut gpui::Context<Self>) {
        let entity = cx.entity();
        let connection = self.context.profile.name.clone();
        let title = move || pane_title(&connection);
        // Opening the window renders this pane into it immediately, which
        // must not happen inside its own update — defer past it.
        cx.defer(move |cx| {
            let build_entity = entity.clone();
            let opened = crate::dialog::open_detached(
                crate::dialog::DetachedOptions::localized_with(
                    title,
                    Theme::scaled(860.0),
                    Theme::scaled(680.0),
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

impl gpui::Focusable for RedisMemoryPane {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for RedisMemoryPane {
    fn render(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let focus_handle = self.focus_handle.clone();
        let mut dialog = Dialog::new("redis-memory-pane", |this: &mut Self| &mut this.frame)
            .title(pane_title(&self.context.profile.name))
            .easy_close(true)
            .key_context("RedisMemoryPane")
            .track_focus(&focus_handle)
            .windowed(self.windowed)
            .on_close(|this, window, cx| this.close(window, cx))
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
                    .children(self.error.clone().map(|err| {
                        div()
                            .text_size(px(Theme::font_size_ratio(0.92)))
                            .text_color(Theme::error())
                            .child(err)
                    }))
                    .children([
                        self.report_section(0),
                        self.prefix_section(cx),
                        self.report_section(1),
                        self.report_section(2),
                    ])
                    .children(self.summary.as_ref().and_then(|summary| {
                        summary.memory_note.clone().map(|note| {
                            div()
                                .text_size(px(Theme::font_size_ratio(0.8)))
                                .text_color(Theme::warning())
                                .child(crate::localization::format(
                                    "explorer.redis_memory.memory_unavailable",
                                    &[("error", &note)],
                                ))
                        })
                    }))
                    .child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.74)))
                            .text_color(Theme::text_dim())
                            .child(match (&self.summary, self.stopped) {
                                (Some(summary), true) => crate::localization::format(
                                    "explorer.redis_memory.stopped_previous",
                                    &[("summary", &summary.describe())],
                                ),
                                (None, true) => crate::localization::text(
                                    "explorer.redis_memory.stopped_before_sample",
                                ),
                                (Some(summary), false) => crate::localization::format(
                                    "explorer.redis_memory.sample_summary",
                                    &[
                                        ("cap", &MEMORY_ANALYSIS_SAMPLE.to_string()),
                                        ("summary", &summary.describe()),
                                    ],
                                ),
                                (None, false) => crate::localization::format(
                                    "explorer.redis_memory.sample_explanation",
                                    &[("cap", &MEMORY_ANALYSIS_SAMPLE.to_string())],
                                ),
                            }),
                    ),
            )
            .leading_button(
                Button::new("redis-memory-refresh")
                    .label(crate::localization::text(
                        "explorer.redis_memory.refresh_button",
                    ))
                    .disabled(self.loading)
                    .on_click(cx.listener(|this, _, _window, cx| this.refresh(cx))),
            )
            .leading_button(
                Button::new("redis-memory-stop")
                    .label(crate::localization::text(
                        "explorer.redis_memory.stop_button",
                    ))
                    .disabled(!self.loading || self.cancel.is_none())
                    .on_click(cx.listener(|this, _, _window, cx| this.stop(cx))),
            )
            .leading_button(
                Button::new("redis-memory-copy-script")
                    .label(crate::localization::text(
                        "explorer.redis_memory.copy_script_button",
                    ))
                    .on_click(cx.listener(|this, _, _window, cx| this.copy_editable_script(cx))),
            );
        if !self.windowed {
            dialog = dialog.detachable(|this, _window, cx| this.pop_out(cx));
        }
        dialog.build(self, window, cx)
    }
}

impl RedisMemoryPane {
    /// The by-prefix report: depth and delimiter pickers beside the title,
    /// then the grid for the chosen depth.
    fn prefix_section(&self, cx: &mut gpui::Context<Self>) -> gpui::AnyElement {
        let s = Theme::scaled;
        let depth = self.prefix_depth;
        let title = crate::localization::format(
            "explorer.redis_memory.report_row_count",
            &[
                (
                    "title",
                    &crate::localization::text("explorer.redis_memory.report_by_prefix"),
                ),
                ("count", &self.prefix_rows[depth - 1].to_string()),
            ],
        );
        let label = |text: String| {
            div()
                .text_size(px(Theme::font_size_ratio(0.8)))
                .text_color(Theme::text_dim())
                .child(text)
        };
        let mut controls = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap(px(s(4.0)))
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.86)))
                    .text_color(Theme::text())
                    .mr(px(s(8.0)))
                    .child(title),
            )
            .child(label(crate::localization::text(
                "explorer.redis_memory.prefix_depth_label",
            )));
        for level in 1..=PREFIX_DEPTHS {
            controls = controls.child(
                Button::new(("redis-memory-prefix-depth", level))
                    .label(level.to_string())
                    .xsmall()
                    .selected(level == depth)
                    .on_click(
                        cx.listener(move |this, _, _window, cx| this.set_prefix_depth(level, cx)),
                    ),
            );
        }
        controls = controls
            .child(div().w(px(s(8.0))))
            .child(label(crate::localization::text(
                "explorer.redis_memory.prefix_delimiters_label",
            )));
        for (i, delimiter) in delimiter_choices(&default_delimiters(&self.context.profile))
            .into_iter()
            .enumerate()
        {
            let on = self.delimiters.contains(&delimiter);
            let toggled = delimiter.clone();
            controls = controls.child(
                Button::new(("redis-memory-prefix-delimiter", i))
                    .label(delimiter.clone())
                    .xsmall()
                    .selected(on)
                    .tooltip(crate::localization::text(
                        "explorer.redis_memory.prefix_delimiter_tooltip",
                    ))
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        this.toggle_prefix_delimiter(&toggled, cx)
                    })),
            );
        }
        let body = match &self.prefix_grids[depth - 1] {
            Some(grid) => div().size_full().child(grid.clone().into_any_element()),
            None => div()
                .text_size(px(Theme::font_size_ratio(0.92)))
                .text_color(Theme::text_dim())
                .child(self.missing_report_text()),
        };
        div()
            .flex()
            .flex_col()
            .flex_basis(px(0.0))
            .flex_1()
            .min_h(px(s(110.0)))
            .gap(px(s(4.0)))
            .child(controls)
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .border_1()
                    .border_color(Theme::border())
                    .rounded(px(s(4.0)))
                    .child(body),
            )
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.74)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text(
                        "explorer.redis_memory.prefix_explanation",
                    )),
            )
            .into_any_element()
    }

    /// Why a report (or the by-prefix table) has no grid yet: the one place
    /// these placeholder lines are chosen.
    fn missing_report_text(&self) -> String {
        crate::localization::text(if self.error.is_some() {
            "explorer.redis_memory.report_unavailable"
        } else if self.loading {
            "explorer.redis_memory.sampling"
        } else if self.stopped {
            "explorer.redis_memory.report_stopped"
        } else {
            "explorer.redis_memory.report_empty"
        })
    }

    /// One labelled report slot: its title and either its grid or an honest
    /// empty/placeholder line.
    fn report_section(&self, slot: usize) -> gpui::AnyElement {
        let s = Theme::scaled;
        let title = report_title(slot);
        let body = match &self.grids[slot] {
            // `size_full`, not `flex_1`: the bordered box below is not a
            // flex container, so a flex-sized wrapper collapses to its
            // content height and the grid's `size_full` root resolves to
            // zero — every report painted empty while the header read
            // "5 rows".
            Some(grid) => div().size_full().child(grid.clone().into_any_element()),
            None => div()
                .text_size(px(Theme::font_size_ratio(0.92)))
                .text_color(Theme::text_dim())
                .child(self.missing_report_text()),
        };
        div()
            .flex()
            .flex_col()
            .flex_basis(px(0.0))
            .flex_1()
            .min_h(px(s(90.0)))
            .gap(px(s(4.0)))
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.86)))
                    .text_color(Theme::text())
                    .child(crate::localization::format(
                        "explorer.redis_memory.report_row_count",
                        &[
                            ("title", &title),
                            ("count", &self.row_counts[slot].to_string()),
                        ],
                    )),
            )
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .border_1()
                    .border_color(Theme::border())
                    .rounded(px(s(4.0)))
                    .child(body),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_script_runs_the_summary_and_every_report_over_one_sample_size() {
        let script = report_script(&[":".to_string()]);
        let lines: Vec<&str> = script.lines().collect();
        assert_eq!(lines.len(), 4 + PREFIX_DEPTHS, "{script}");
        assert!(
            lines[SUMMARY_SET].starts_with("SQLLY.MEMSUMMARY"),
            "{script}"
        );
        assert!(lines[1].contains("SQLLY.MEMBYTYPE"), "{script}");
        assert!(lines[2].contains("SQLLY.TOPKEYS"), "{script}");
        assert!(
            lines[2].ends_with(format!("{MEMORY_ANALYSIS_SAMPLE} {MEMORY_ANALYSIS_TOP}").as_str()),
            "the top-keys report must carry both bounds: {script}"
        );
        assert!(lines[3].contains("SQLLY.TTLBUCKETS"), "{script}");
        for depth in 1..=PREFIX_DEPTHS {
            assert_eq!(
                lines[PREFIX_SET + depth - 1],
                format!("SQLLY.MEMBYPREFIX {MEMORY_ANALYSIS_SAMPLE} {depth} :"),
                "{script}"
            );
        }
        // Every command names the same sample size, so the driver derives
        // all of them from one sample.
        for line in &lines {
            assert_eq!(
                line.split_whitespace().nth(1),
                Some(MEMORY_ANALYSIS_SAMPLE.to_string().as_str()),
                "{line}"
            );
        }
        assert!(
            !script
                .lines()
                .any(|line| line.trim_start().to_uppercase().starts_with("KEYS")),
            "the analyzer samples with SCAN, never the KEYS command: {script}"
        );
    }

    fn summary_set(memory_note: Option<&str>, complete: bool) -> crate::tool_dialog::ToolQuerySet {
        let column = |name: &str| sqlly_core::ResultColumn {
            name: name.to_string(),
            data_type: String::new(),
            nullable: true,
        };
        crate::tool_dialog::ToolQuerySet {
            // Deliberately not the driver's order: fields are read by name.
            columns: vec![
                column("memory_note"),
                column("complete"),
                column("sampled_keys"),
                column("total_keys"),
                column("vanished_keys"),
                column("memory_measured"),
                column("database"),
            ],
            rows: vec![vec![
                memory_note
                    .map(|n| CellValue::String(n.to_string()))
                    .unwrap_or(CellValue::Null),
                CellValue::Bool(complete),
                CellValue::I64(1000),
                CellValue::I64(52_000),
                CellValue::I64(3),
                CellValue::Bool(memory_note.is_none()),
                CellValue::I64(2),
            ]],
        }
    }

    #[test]
    fn summary_reads_fields_by_name_and_discloses_extrapolation() {
        let summary = SampleSummary::from_set(&summary_set(None, false)).unwrap();
        assert_eq!(summary.database, Some(2));
        assert_eq!(summary.sampled_keys, 1000);
        assert_eq!(summary.total_keys, 52_000);
        assert!(summary.memory_measured);
        let text = summary.describe();
        assert!(text.contains("sampled 1000 of 52000 keys"), "{text}");
        assert!(text.contains("extrapolated"), "{text}");
        assert!(text.contains("3 keys vanished"), "{text}");
        let exact = SampleSummary::from_set(&summary_set(None, true)).unwrap();
        assert!(exact.describe().contains("exact"));
        let unmeasured =
            SampleSummary::from_set(&summary_set(Some("MEMORY USAGE is not permitted"), true))
                .unwrap();
        assert!(!unmeasured.memory_measured);
        assert_eq!(
            unmeasured.memory_note.as_deref(),
            Some("MEMORY USAGE is not permitted")
        );
        assert!(SampleSummary::from_set(&Default::default()).is_none());
    }

    /// The three report grids are drawn inside the real pane (inline card
    /// and popped-out layout) and must each get room for their rows. They
    /// used to sit in an auto-height wrapper, so every grid painted zero
    /// pixels tall while the headers read "5 rows".
    #[gpui::test]
    fn report_grids_get_room_to_paint_their_rows(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            if cx.try_global::<gpui_component::Theme>().is_none() {
                gpui_component::init(cx);
            }
        });
        for windowed in [false, true] {
            let context =
                crate::tool_dialog::unreachable_tool_context(sqlly_core::DatabaseType::Redis);
            let (view, cx) = cx.add_window_view(|_window, cx| RedisMemoryPane::new(context, cx));
            cx.simulate_resize(gpui::size(px(1200.0), px(900.0)));
            cx.run_until_parked();
            view.update(cx, |pane, cx| {
                pane.windowed = windowed;
                pane.error = None;
                pane.loading = false;
                for slot in 0..3 {
                    let set = crate::tool_dialog::sample_tool_set(5);
                    pane.row_counts[slot] = set.rows.len();
                    pane.grids[slot] = crate::tool_dialog::build_result_grid(&set, cx);
                }
                cx.notify();
            });
            cx.run_until_parked();
            view.read_with(cx, |pane, cx| {
                for (slot, grid) in pane.grids.iter().enumerate() {
                    let grid = grid.as_ref().expect("report grid");
                    crate::tool_dialog::assert_grid_shows_rows(
                        grid,
                        1,
                        format!("{} (windowed: {windowed})", REPORT_TITLES[slot]),
                        cx,
                    );
                }
            });
        }
    }

    #[test]
    fn delimiter_sets_travel_as_one_argument_the_driver_splits_back() {
        let one = vec![":".to_string()];
        assert_eq!(delimiter_argument(&one), ":");
        let two = vec![":".to_string(), "|".to_string()];
        let argument = delimiter_argument(&two);
        assert_eq!(argument, "\": |\"");
        let script = report_script(&two);
        let commands = sqlly_intel::query::safety::redis::parse_script(&script).unwrap();
        let prefix = &commands[PREFIX_SET];
        assert_eq!(prefix.arg_text(2), "1");
        assert_eq!(
            sqlly_core::redis_keys::parse_delimiters(&prefix.arg_text(3)),
            two
        );
        let long = vec!["::".to_string(), "|".to_string()];
        let commands =
            sqlly_intel::query::safety::redis::parse_script(&report_script(&long)).unwrap();
        assert_eq!(
            sqlly_core::redis_keys::parse_delimiters(&commands[PREFIX_SET].arg_text(3)),
            long
        );
        assert_eq!(delimiter_argument(&[]), ":");
    }

    #[test]
    fn delimiter_choices_start_with_the_connections_own_and_never_empty() {
        let choices = delimiter_choices(&["::".to_string()]);
        assert_eq!(choices[0], "::");
        assert!(choices.iter().any(|d| d == ":"));
        assert!(choices.iter().any(|d| d == "|"));
        let defaults = delimiter_choices(&[":".to_string()]);
        assert_eq!(defaults.iter().filter(|d| *d == ":").count(), 1);
        let mut selected = vec![":".to_string()];
        assert!(!toggle_delimiter(&mut selected, ":"), "last one stays");
        assert!(toggle_delimiter(&mut selected, "|"));
        assert_eq!(selected, vec![":", "|"]);
        assert!(toggle_delimiter(&mut selected, ":"));
        assert_eq!(selected, vec!["|"]);
    }

    #[test]
    fn report_titles_cover_three_slots_in_order() {
        assert_eq!(REPORT_TITLES.len(), 3);
        assert_eq!(
            crate::localization::text(REPORT_TITLES[0]),
            "Memory by type"
        );
        assert_eq!(crate::localization::text(REPORT_TITLES[1]), "Top keys");
        assert_eq!(
            crate::localization::text(REPORT_TITLES[2]),
            "TTL distribution"
        );
    }
}
