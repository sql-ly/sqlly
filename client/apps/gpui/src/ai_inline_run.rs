//! SELECT-only inline query preview for the AI drawer.
//!
//! Pure drain + eligibility live here so the panel can run a capped read
//! without touching the editor run path. The compact grid is a small view
//! that reuses `tool_dialog::build_result_grid`.

use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, ParentElement, Render,
    Styled, Window, div, px,
};
use gpui_component::Sizable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use sqlly_core::{CellValue, ConnectionProfile, ResultColumn};
use sqlly_datatable::SqllyDataTable;
use sqlly_intel::query::safety::classify_select_only;
use sqlly_services::{QueryEvent, Services};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(not(target_arch = "wasm32"))]
use std::time::Duration;
#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;
#[cfg(target_arch = "wasm32")]
use web_time::Instant;

use crate::theme::Theme;
use crate::tool_dialog::{ToolQuerySet, build_result_grid};

pub const INLINE_ROW_CAP: usize = 200;
pub const INLINE_ROW_BUFFER: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InlineRunStatus {
    Running,
    Done,
    Cancelled,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct InlineRunOutcome {
    pub columns: Vec<ResultColumn>,
    pub rows: Vec<Vec<CellValue>>,
    pub truncated: bool,
    pub elapsed_ms: u128,
    pub status: InlineRunStatus,
}

/// Human footer: "12 rows · 34 ms", "200+ rows (capped) · 1.2 s",
/// "Cancelled after 3 rows", "Error: <message>".
pub fn footer_text(o: &InlineRunOutcome) -> String {
    let row_count = o.rows.len().to_string();
    let elapsed = format_elapsed(o.elapsed_ms);
    match &o.status {
        InlineRunStatus::Failed(message) => {
            crate::localization::format("ai.inline_run.footer_error", &[("message", message)])
        }
        InlineRunStatus::Cancelled => crate::localization::format(
            "ai.inline_run.footer_cancelled",
            &[("row_count", &row_count)],
        ),
        InlineRunStatus::Running => crate::localization::text("ai.inline_run.running_status"),
        InlineRunStatus::Done if o.truncated => crate::localization::format(
            "ai.inline_run.footer_capped",
            &[
                ("row_cap", &INLINE_ROW_CAP.to_string()),
                ("elapsed", &elapsed),
            ],
        ),
        InlineRunStatus::Done => crate::localization::format(
            "ai.inline_run.footer_done",
            &[("row_count", &row_count), ("elapsed", &elapsed)],
        ),
    }
}

pub fn history_summary(o: &InlineRunOutcome) -> String {
    let row_count = o.rows.len().to_string();
    match &o.status {
        InlineRunStatus::Failed(message) => {
            crate::localization::format("ai.inline_run.history_failed", &[("message", message)])
        }
        InlineRunStatus::Cancelled => crate::localization::format(
            "ai.inline_run.history_cancelled",
            &[("row_count", &row_count)],
        ),
        _ => crate::localization::format(
            "ai.inline_run.history_done",
            &[
                ("row_count", &row_count),
                ("elapsed", &format_elapsed(o.elapsed_ms)),
            ],
        ),
    }
}

fn format_elapsed(ms: u128) -> String {
    if ms >= 1000 {
        format!("{:.1} s", ms as f64 / 1000.0)
    } else {
        format!("{ms} ms")
    }
}

/// Decide whether a SQL block may run inline: SELECT-only per
/// `classify_select_only`, non-empty, read as every engine would read it.
/// Prefer [`inline_eligibility_for`] when the connection is known.
pub fn inline_eligibility(sql: &str) -> Result<(), String> {
    inline_eligibility_for(None, sql)
}

/// [`inline_eligibility`] read as `db` reads the SQL (every execution unit,
/// every lexical reading the engine could apply).
pub fn inline_eligibility_for(
    db: Option<sqlly_core::DatabaseType>,
    sql: &str,
) -> Result<(), String> {
    let classification = match db {
        Some(db) => sqlly_intel::query::safety::classify_select_only_for(db, sql),
        None => classify_select_only(sql),
    };
    if classification.select_only {
        Ok(())
    } else {
        Err(classification
            .reason
            .unwrap_or_else(|| crate::localization::text("ai.inline_run.not_select")))
    }
}

pub enum DrainStep {
    Continue,
    CapReached,
    Terminal,
}

/// Pure fold over the event stream; returns Some(outcome) once terminal.
/// Only result set 0 is kept; `Rows` beyond the cap flip `truncated` and the
/// caller must cancel.
pub struct Drain {
    outcome: InlineRunOutcome,
    started: Instant,
}

impl Drain {
    pub fn new() -> Self {
        Self {
            outcome: InlineRunOutcome {
                columns: Vec::new(),
                rows: Vec::new(),
                truncated: false,
                elapsed_ms: 0,
                status: InlineRunStatus::Running,
            },
            started: Instant::now(),
        }
    }

    pub fn push(&mut self, event: QueryEvent) -> DrainStep {
        match event {
            QueryEvent::ResultSet { index: 0, columns } => {
                self.outcome.columns = columns;
                DrainStep::Continue
            }
            QueryEvent::Rows { index: 0, rows } => {
                for row in rows {
                    if self.outcome.rows.len() >= INLINE_ROW_CAP {
                        self.outcome.truncated = true;
                        return DrainStep::CapReached;
                    }
                    self.outcome.rows.push(row);
                }
                if self.outcome.rows.len() >= INLINE_ROW_CAP {
                    self.outcome.truncated = true;
                    DrainStep::CapReached
                } else {
                    DrainStep::Continue
                }
            }
            QueryEvent::Error { message } => {
                self.outcome.status = InlineRunStatus::Failed(message);
                self.stamp_elapsed();
                DrainStep::Terminal
            }
            QueryEvent::Completed { elapsed_ms } => {
                self.outcome.elapsed_ms = elapsed_ms;
                if !matches!(self.outcome.status, InlineRunStatus::Failed(_)) {
                    self.outcome.status = InlineRunStatus::Done;
                }
                DrainStep::Terminal
            }
            QueryEvent::Cancelled => {
                self.stamp_elapsed();
                self.outcome.status = if self.outcome.truncated {
                    InlineRunStatus::Done
                } else {
                    InlineRunStatus::Cancelled
                };
                DrainStep::Terminal
            }
            _ => DrainStep::Continue,
        }
    }

    pub fn finish(mut self) -> InlineRunOutcome {
        self.stamp_elapsed();
        if matches!(self.outcome.status, InlineRunStatus::Running) {
            self.outcome.status = if self.outcome.truncated {
                InlineRunStatus::Done
            } else {
                InlineRunStatus::Cancelled
            };
        }
        self.outcome
    }

    fn stamp_elapsed(&mut self) {
        if self.outcome.elapsed_ms == 0 {
            self.outcome.elapsed_ms = self.started.elapsed().as_millis();
        }
    }
}

impl Default for Drain {
    fn default() -> Self {
        Self::new()
    }
}

fn failed_outcome(message: String) -> InlineRunOutcome {
    InlineRunOutcome {
        columns: Vec::new(),
        rows: Vec::new(),
        truncated: false,
        elapsed_ms: 0,
        status: InlineRunStatus::Failed(message),
    }
}

/// Execute `sql` through the query API with a hard row cap. The cloned
/// profile is SELECT-only at the driver (`block_all_but_select`).
pub async fn execute_inline(
    services: &Services,
    profile: &ConnectionProfile,
    sql: &str,
    cancel: Arc<AtomicBool>,
) -> InlineRunOutcome {
    let mut profile = profile.clone();
    profile.block_all_but_select = true;
    let mut handle = match services
        .queries
        .execute(&profile, sql, INLINE_ROW_BUFFER)
        .await
    {
        Ok(handle) => handle,
        Err(error) => return failed_outcome(error.to_string()),
    };
    let task_id = handle.task_id.clone();

    let drain_loop = async {
        let mut drain = Drain::new();
        let mut asked_cancel = false;
        loop {
            if cancel.load(Ordering::Relaxed) && !asked_cancel {
                let _ = services.queries.cancel(&handle.task_id).await;
                asked_cancel = true;
            }
            let event = {
                #[cfg(not(target_arch = "wasm32"))]
                {
                    loop {
                        tokio::select! {
                            event = handle.events.recv() => break event,
                            _ = tokio::time::sleep(Duration::from_millis(200)) => {
                                if cancel.load(Ordering::Relaxed) {
                                    break handle.events.recv().await;
                                }
                            }
                        }
                    }
                }
                #[cfg(target_arch = "wasm32")]
                {
                    handle.events.recv().await
                }
            };
            match event {
                None => return drain.finish(),
                Some(event) => match drain.push(event) {
                    DrainStep::Continue => {}
                    DrainStep::CapReached => {
                        if !asked_cancel {
                            let _ = services.queries.cancel(&handle.task_id).await;
                            asked_cancel = true;
                        }
                    }
                    DrainStep::Terminal => return drain.finish(),
                },
            }
        }
    };

    #[cfg(not(target_arch = "wasm32"))]
    {
        match tokio::time::timeout(Duration::from_secs(30), drain_loop).await {
            Ok(outcome) => outcome,
            Err(_) => {
                let _ = services.queries.cancel(&task_id).await;
                failed_outcome(crate::localization::text("ai.inline_run.timeout"))
            }
        }
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = task_id;
        drain_loop.await
    }
}

pub enum InlineResultEvent {
    OpenInTab { sql: String },
    Cancel,
}

pub struct InlineResultView {
    sql: String,
    outcome: Option<InlineRunOutcome>,
    grid: Option<Entity<SqllyDataTable>>,
    cancel: Option<Arc<AtomicBool>>,
    production: bool,
    focus_handle: FocusHandle,
}

impl EventEmitter<InlineResultEvent> for InlineResultView {}

impl Focusable for InlineResultView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl InlineResultView {
    pub fn new(sql: String, cx: &mut Context<Self>) -> Self {
        Self {
            sql,
            outcome: None,
            grid: None,
            cancel: None,
            production: false,
            focus_handle: cx.focus_handle(),
        }
    }

    pub fn set_running(&mut self, cancel: Arc<AtomicBool>, cx: &mut Context<Self>) {
        self.cancel = Some(cancel);
        self.outcome = None;
        self.grid = None;
        cx.notify();
    }

    pub fn set_production(&mut self, production: bool) {
        self.production = production;
    }

    pub fn set_outcome(&mut self, outcome: InlineRunOutcome, cx: &mut Context<Self>) {
        self.grid = if !outcome.rows.is_empty() && !outcome.columns.is_empty() {
            build_result_grid(
                &ToolQuerySet {
                    columns: outcome.columns.clone(),
                    rows: outcome.rows.clone(),
                },
                cx,
            )
        } else {
            None
        };
        self.outcome = Some(outcome);
        self.cancel = None;
        cx.notify();
    }

    pub fn outcome(&self) -> Option<&InlineRunOutcome> {
        self.outcome.as_ref()
    }

    pub fn grid(&self) -> Option<&Entity<SqllyDataTable>> {
        self.grid.as_ref()
    }

    pub fn is_running(&self) -> bool {
        self.outcome.is_none() && self.cancel.is_some()
    }

    pub(crate) fn request_open_in_tab(&mut self, cx: &mut Context<Self>) {
        cx.emit(InlineResultEvent::OpenInTab {
            sql: self.sql.clone(),
        });
    }

    pub(crate) fn request_cancel(&mut self, cx: &mut Context<Self>) {
        if let Some(flag) = &self.cancel {
            flag.store(true, Ordering::Relaxed);
        }
        cx.emit(InlineResultEvent::Cancel);
        cx.notify();
    }
}

impl Render for InlineResultView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let mut body = div()
            .flex()
            .flex_col()
            .gap(px(s(4.0)))
            .border_1()
            .border_color(Theme::border())
            .rounded(px(s(4.0)))
            .p(px(s(6.0)));

        if self.is_running() {
            body = body.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(8.0)))
                    .child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.82)))
                            .text_color(Theme::text_dim())
                            .child(crate::localization::text("ai.inline_run.running_status")),
                    )
                    .child(
                        Button::new("inline-cancel")
                            .small()
                            .ghost()
                            .label(crate::localization::text("ai.inline_run.cancel_button"))
                            .on_click(cx.listener(|this, _, _, cx| this.request_cancel(cx))),
                    ),
            );
        } else if let Some(outcome) = self.outcome().cloned() {
            match &outcome.status {
                InlineRunStatus::Failed(message) => {
                    body = body.child(
                        div()
                            .text_size(px(Theme::font_size()))
                            .text_color(Theme::text_dim())
                            .child(crate::localization::format(
                                "ai.inline_run.result_error",
                                &[("message", message)],
                            )),
                    );
                }
                InlineRunStatus::Done | InlineRunStatus::Cancelled | InlineRunStatus::Running
                    if outcome.rows.is_empty() && self.grid().is_none() =>
                {
                    body = body.child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.82)))
                            .text_color(Theme::text_dim())
                            .child(crate::localization::text("ai.inline_run.no_rows")),
                    );
                }
                _ => {
                    if let Some(grid) = self.grid() {
                        body =
                            body.child(div().h(px(s(220.0))).overflow_hidden().child(grid.clone()));
                    }
                }
            }

            let mut footer = footer_text(&outcome);
            if self.production {
                footer = crate::localization::format(
                    "ai.inline_run.footer_production",
                    &[("footer", &footer)],
                );
            }
            body = body.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap(px(s(8.0)))
                    .child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.82)))
                            .text_color(Theme::text_dim())
                            .child(footer),
                    )
                    .child(
                        Button::new("inline-open-tab")
                            .small()
                            .label(crate::localization::text(
                                "ai.inline_run.open_results_button",
                            ))
                            .on_click(cx.listener(|this, _, _, cx| this.request_open_in_tab(cx))),
                    ),
            );
        }

        body
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::ResultColumn;

    fn col(name: &str) -> ResultColumn {
        ResultColumn {
            name: name.into(),
            data_type: "int".into(),
            nullable: false,
        }
    }

    fn row(n: i64) -> Vec<CellValue> {
        vec![CellValue::I64(n)]
    }

    fn result_set_0(names: &[&str]) -> QueryEvent {
        QueryEvent::ResultSet {
            index: 0,
            columns: names.iter().copied().map(col).collect(),
        }
    }

    fn rows_0(values: impl IntoIterator<Item = i64>) -> QueryEvent {
        QueryEvent::Rows {
            index: 0,
            rows: values.into_iter().map(row).collect(),
        }
    }

    #[test]
    fn drain_collects_first_result_set_only() {
        let mut drain = Drain::new();
        assert!(matches!(
            drain.push(result_set_0(&["id"])),
            DrainStep::Continue
        ));
        assert!(matches!(drain.push(rows_0([1, 2])), DrainStep::Continue));
        assert!(matches!(
            drain.push(QueryEvent::ResultSet {
                index: 1,
                columns: vec![col("other")],
            }),
            DrainStep::Continue
        ));
        assert!(matches!(
            drain.push(QueryEvent::Rows {
                index: 1,
                rows: vec![row(99)],
            }),
            DrainStep::Continue
        ));
        assert!(matches!(
            drain.push(QueryEvent::Completed { elapsed_ms: 12 }),
            DrainStep::Terminal
        ));
        let outcome = drain.finish();
        assert_eq!(outcome.columns.len(), 1);
        assert_eq!(outcome.rows.len(), 2);
        assert_eq!(outcome.status, InlineRunStatus::Done);
        assert!(!outcome.truncated);
    }

    #[test]
    fn drain_caps_at_200_and_reports_truncated() {
        let mut drain = Drain::new();
        drain.push(result_set_0(&["id"]));
        let step = drain.push(rows_0(0..250));
        assert!(matches!(step, DrainStep::CapReached));
        assert!(drain.outcome.truncated);
        assert_eq!(drain.outcome.rows.len(), INLINE_ROW_CAP);
    }

    #[test]
    fn drain_error_is_failed() {
        let mut drain = Drain::new();
        let step = drain.push(QueryEvent::Error {
            message: "boom".into(),
        });
        assert!(matches!(step, DrainStep::Terminal));
        let outcome = drain.finish();
        assert_eq!(outcome.status, InlineRunStatus::Failed("boom".into()));
    }

    #[test]
    fn drain_cancel_after_cap_is_done() {
        let mut drain = Drain::new();
        drain.push(result_set_0(&["id"]));
        drain.push(rows_0(0..200));
        let step = drain.push(QueryEvent::Cancelled);
        assert!(matches!(step, DrainStep::Terminal));
        let outcome = drain.finish();
        assert_eq!(outcome.status, InlineRunStatus::Done);
        assert!(outcome.truncated);
    }

    #[test]
    fn drain_cancel_before_cap_is_cancelled() {
        let mut drain = Drain::new();
        drain.push(result_set_0(&["id"]));
        drain.push(rows_0([1, 2, 3]));
        let step = drain.push(QueryEvent::Cancelled);
        assert!(matches!(step, DrainStep::Terminal));
        let outcome = drain.finish();
        assert_eq!(outcome.status, InlineRunStatus::Cancelled);
        assert_eq!(outcome.rows.len(), 3);
    }

    #[test]
    fn footer_text_variants() {
        let done = InlineRunOutcome {
            columns: vec![col("id")],
            rows: vec![row(1), row(2), row(3)],
            truncated: false,
            elapsed_ms: 34,
            status: InlineRunStatus::Done,
        };
        assert_eq!(footer_text(&done), "3 rows · 34 ms");

        let capped = InlineRunOutcome {
            columns: vec![col("id")],
            rows: (0..200).map(row).collect(),
            truncated: true,
            elapsed_ms: 1200,
            status: InlineRunStatus::Done,
        };
        assert_eq!(footer_text(&capped), "200+ rows (capped) · 1.2 s");

        let cancelled = InlineRunOutcome {
            columns: vec![col("id")],
            rows: vec![row(1), row(2), row(3)],
            truncated: false,
            elapsed_ms: 10,
            status: InlineRunStatus::Cancelled,
        };
        assert_eq!(footer_text(&cancelled), "Cancelled after 3 rows");

        let failed = InlineRunOutcome {
            columns: vec![],
            rows: vec![],
            truncated: false,
            elapsed_ms: 0,
            status: InlineRunStatus::Failed("nope".into()),
        };
        assert_eq!(footer_text(&failed), "Error: nope");
    }

    #[test]
    fn eligibility_rejects_update_and_multi_statement_with_write() {
        assert!(inline_eligibility("UPDATE t SET x = 1").is_err());
        assert!(inline_eligibility("SELECT 1; UPDATE t SET x = 1").is_err());
        assert!(inline_eligibility("").is_err());
    }

    #[test]
    fn eligibility_accepts_cte_select() {
        assert!(inline_eligibility("WITH x AS (SELECT 1 AS n) SELECT * FROM x").is_ok());
        assert!(inline_eligibility("SELECT 1").is_ok());
    }

    #[test]
    fn history_summary_shape() {
        let done = InlineRunOutcome {
            columns: vec![col("id")],
            rows: vec![row(1); 12],
            truncated: false,
            elapsed_ms: 34,
            status: InlineRunStatus::Done,
        };
        assert_eq!(history_summary(&done), "Ran inline: 12 rows in 34 ms");

        let failed = InlineRunOutcome {
            columns: vec![],
            rows: vec![],
            truncated: false,
            elapsed_ms: 0,
            status: InlineRunStatus::Failed("timeout".into()),
        };
        assert_eq!(history_summary(&failed), "Ran inline: failed — timeout");
    }

    fn ensure_components_initialized(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            if cx.try_global::<gpui_component::Theme>().is_none() {
                gpui_component::init(cx);
            }
        });
    }

    fn sample_outcome(status: InlineRunStatus, n: usize) -> InlineRunOutcome {
        InlineRunOutcome {
            columns: vec![col("id"), col("name")],
            rows: (0..n as i64)
                .map(|i| vec![CellValue::I64(i), CellValue::String(format!("r{i}"))])
                .collect(),
            truncated: false,
            elapsed_ms: 15,
            status,
        }
    }

    #[gpui::test]
    fn completed_inline_run_renders_a_grid_entity(cx: &mut gpui::TestAppContext) {
        ensure_components_initialized(cx);
        let (view, cx) =
            cx.add_window_view(|_window, cx| InlineResultView::new("SELECT 1".into(), cx));
        view.update(cx, |view, cx| {
            view.set_outcome(sample_outcome(InlineRunStatus::Done, 3), cx);
        });
        view.read_with(cx, |view, _cx| {
            assert!(view.grid().is_some());
            let footer = footer_text(view.outcome().expect("outcome"));
            assert!(
                footer.contains("3 rows"),
                "footer should mention 3 rows: {footer}"
            );
        });
    }

    #[gpui::test]
    fn failed_inline_run_has_no_grid(cx: &mut gpui::TestAppContext) {
        ensure_components_initialized(cx);
        let (view, cx) =
            cx.add_window_view(|_window, cx| InlineResultView::new("SELECT 1".into(), cx));
        view.update(cx, |view, cx| {
            view.set_outcome(
                InlineRunOutcome {
                    columns: vec![],
                    rows: vec![],
                    truncated: false,
                    elapsed_ms: 0,
                    status: InlineRunStatus::Failed("denied".into()),
                },
                cx,
            );
        });
        view.read_with(cx, |view, _cx| {
            assert!(view.grid().is_none());
        });
    }

    #[gpui::test]
    fn open_in_tab_event_is_emitted_from_inline_view(cx: &mut gpui::TestAppContext) {
        ensure_components_initialized(cx);
        let (view, cx) =
            cx.add_window_view(|_window, cx| InlineResultView::new("SELECT 42".into(), cx));
        let events: std::rc::Rc<std::cell::RefCell<Vec<String>>> =
            std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let seen = events.clone();
        cx.update(|_window, cx| {
            cx.subscribe(&view, move |_view, event: &InlineResultEvent, _cx| {
                if let InlineResultEvent::OpenInTab { sql } = event {
                    seen.borrow_mut().push(sql.clone());
                }
            })
            .detach();
        });
        view.update(cx, |view, cx| view.request_open_in_tab(cx));
        assert_eq!(*events.borrow(), vec!["SELECT 42".to_string()]);
    }
}
