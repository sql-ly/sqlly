//! Shared plumbing for the Tools-menu admin dialogs (Backup/Restore,
//! Activity Monitor, Template Explorer, Query Store, SQL Agent, Import/
//! Export, Schema Compare, Extended Events, Designers).
//!
//! Deliberately small: a captured routing context, one typed event enum, a
//! query runner that keeps result columns (the dialogs render live grids),
//! and a grid construction helper. Each dialog stays a concrete view — there
//! is no generic dialog framework here, and there must not be one. `AppView`
//! owns tab creation, execution, and status; dialogs emit events.
//!
//! Script-first contract: every mutating admin workflow scripts reviewed SQL
//! into an editor tab via [`ToolDialogEvent::ScriptToEditor`]. None of the
//! first-generation dialogs execute destructive SQL directly.

// Browser build: swap tokio's channels/spawn for the reactor-free shim so
// the `tokio::` paths below compile unchanged.
#[cfg(target_arch = "wasm32")]
use crate::wasm_tokio as tokio;
use gpui::{AppContext as _, Context, Entity};
use sqlly_core::{CellValue, ConnectionProfile, ResultColumn};
use sqlly_datatable::SqllyDataTable;
use sqlly_services::{QueryEvent, Services};

use crate::app::StatusLevel;

/// Whether a tool dialog was opened against a whole server or a single
/// database. A tool reached from a right-clicked *database* node scopes its
/// view to that database; the same tool reached from the server node, the
/// command palette, the dashboard footer, or a keyboard shortcut stays
/// server-wide. Defaults to `Server` so every non-database entry point keeps
/// its existing breadth.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ToolScope {
    #[default]
    Server,
    Database,
}

/// Routing captured when a tool dialog opens: the profile it was opened for
/// and that profile's route-aware services. A profile switch after open must
/// not redirect in-flight admin work, so dialogs never re-resolve either.
#[derive(Clone)]
pub struct ToolDialogContext {
    pub profile: ConnectionProfile,
    pub services: Services,
}

impl ToolDialogContext {
    pub fn new(profile: ConnectionProfile, services: Services) -> Self {
        Self { profile, services }
    }
}

/// The production guard's live verdict for a tool's connection. Tool
/// writes go through [`run_query`], which skips the editor's run gate, so
/// a tool that can write asks this right before it does (the unlock can
/// expire while the tool is open). Built by `AppView`; call it only from
/// event handlers, never while `AppView` itself is being updated.
pub type ProductionCheck =
    std::rc::Rc<dyn Fn(&gpui::App) -> sqlly_intel::query::safety::ProductionDecision>;

/// What a tool dialog asks of `AppView`. Dialogs stay passive: they never
/// open tabs, run SQL against the workbench, or write the status bar
/// themselves.
#[derive(Clone)]
pub enum ToolDialogEvent {
    /// Open the SQL in a new editor tab for review (the safe default for
    /// every mutating admin workflow).
    ScriptToEditor { sql: String },
    /// Open reviewed SQL pinned to a captured destination connection. Used by
    /// cross-connection tools so a later workbench switch cannot redirect the
    /// script to the wrong server or database.
    ScriptToEditorForProfile {
        sql: String,
        profile: Box<ConnectionProfile>,
    },
    /// Open the SQL in a new editor tab AND execute it immediately on the
    /// active connection. Emitted by the history browser's Run action for
    /// saved (pinned) queries the user already trusts — the deliberate
    /// exception to the review-first contract above, and only for SQL the
    /// user personally ran or pinned before. `AppView` must create the tab
    /// exactly like [`Self::ScriptToEditor`] and then trigger the same
    /// execute path the Run command uses on that new tab.
    ScriptToEditorAndRun { sql: String },
    /// Like [`Self::ScriptToEditorForProfile`], but execute immediately on the
    /// pinned maintenance database after tab creation (every execute gate still
    /// applies).
    ScriptToEditorForProfileAndRun {
        sql: String,
        profile: Box<ConnectionProfile>,
    },
    /// Surface a status-bar message.
    Status { level: StatusLevel, message: String },
    /// A database dump finished (export dialog or scheduled run).
    DumpCompleted {
        message: String,
        level: StatusLevel,
        primary_path: Option<std::path::PathBuf>,
        cancelled: bool,
    },
    /// A database dump export job started.
    DumpStarted { output_path: std::path::PathBuf },
    /// Open Run SQL File with a preselected path (plan 173).
    #[cfg(not(target_arch = "wasm32"))]
    RunSqlFile { path: std::path::PathBuf },
    /// A cross-connection data transfer started.
    TransferStarted,
    /// A cross-connection data transfer finished.
    TransferFinished { cancelled: bool, failed: bool },
    /// Persist a scheduled dump from the schedule-mode dialog.
    SaveDumpSchedule {
        schedule: Box<crate::dump_schedule::DumpSchedule>,
    },
    /// Open a sibling admin pane. Routed as an event rather than a dispatched
    /// action because a popped-out pane lives in its own OS window, where
    /// `Window::dispatch_action` never reaches `AppView` — the entity
    /// subscription does.
    OpenTool(AdminToolLink),
    /// Open the Save Schema Snapshot dialog for a live database (Schema
    /// Compare's per-side Snapshot button, Schema History's Take Snapshot).
    /// `profile.database` names the database to capture.
    SaveSchemaSnapshot { profile: Box<ConnectionProfile> },
    /// Dismiss the dialog.
    Close,
    /// Open the AI drawer's Ask lane seeded with a prepared prompt; never
    /// executes anything.
    AnalyzeWithAi { prompt: String },
}

/// The sibling admin panes the Server Dashboard links to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdminToolLink {
    ActivityMonitor,
    QueryInsights,
    DiskUsage,
    RedisMemoryAnalyzer,
    RedisClusterTopology,
}

/// One result set from an admin query, with its column metadata (unlike the
/// security dialog's row-only collector — the tool dialogs feed grids).
#[derive(Clone, Debug, Default)]
pub struct ToolQuerySet {
    pub columns: Vec<ResultColumn>,
    pub rows: Vec<Vec<CellValue>>,
}

/// Everything an admin query produced. `error` carries the first error event
/// (permission failures land here as honest text, not empty grids).
#[derive(Clone, Debug, Default)]
pub struct ToolQueryOutcome {
    pub sets: Vec<ToolQuerySet>,
    pub error: Option<String>,
}

/// Hard per-set row bound for admin surfaces. Live monitors and top-query
/// views are summaries; anything larger belongs in a real query tab.
pub const TOOL_QUERY_ROW_CAP: usize = 500;

/// Run `sql` on the dialog's captured route and collect bounded result sets
/// with columns. Row batches beyond [`TOOL_QUERY_ROW_CAP`] per set are
/// dropped (the truncation is visible: `truncated` flips on the set's
/// outcome via row count == cap).
pub async fn execute_collect(
    services: &Services,
    profile: &ConnectionProfile,
    sql: &str,
) -> ToolQueryOutcome {
    execute_collect_with_cap(services, profile, sql, TOOL_QUERY_ROW_CAP).await
}

/// [`execute_collect`] with an explicit per-set row cap, for surfaces whose
/// consumer has its own bound (the chart dashboard feeds the chart engine's
/// source-row limit rather than a 500-row grid).
pub async fn execute_collect_with_cap(
    services: &Services,
    profile: &ConnectionProfile,
    sql: &str,
    row_cap: usize,
) -> ToolQueryOutcome {
    let mut outcome = ToolQueryOutcome::default();
    let mut handle = match services.queries.execute(profile, sql, 256).await {
        Ok(handle) => handle,
        Err(e) => {
            outcome.error = Some(e.to_string());
            return outcome;
        }
    };
    while let Some(event) = handle.events.recv().await {
        match event {
            QueryEvent::ResultSet { index, columns } => {
                if outcome.sets.len() <= index {
                    outcome.sets.resize_with(index + 1, ToolQuerySet::default);
                }
                outcome.sets[index].columns = columns;
            }
            QueryEvent::Rows { index, rows } => {
                if outcome.sets.len() <= index {
                    outcome.sets.resize_with(index + 1, ToolQuerySet::default);
                }
                let set = &mut outcome.sets[index];
                let room = row_cap.saturating_sub(set.rows.len());
                set.rows.extend(rows.into_iter().take(room));
            }
            QueryEvent::Error { message } => {
                outcome.error = Some(message);
            }
            QueryEvent::Completed { .. } | QueryEvent::Cancelled => break,
            _ => {}
        }
    }
    outcome
}

/// Spawn `execute_collect` off the UI thread and hand the outcome back to
/// `on_done` on the entity. The freshness token is the caller's problem —
/// pass a generation captured before the call and compare in `on_done`.
pub fn run_query<V: 'static>(
    context: &ToolDialogContext,
    sql: String,
    cx: &mut Context<V>,
    on_done: impl FnOnce(&mut V, ToolQueryOutcome, &mut Context<V>) + 'static,
) {
    run_query_with_cap(context, sql, TOOL_QUERY_ROW_CAP, cx, on_done)
}

/// [`run_query`] with an explicit per-set row cap (see
/// [`execute_collect_with_cap`]).
pub fn run_query_with_cap<V: 'static>(
    context: &ToolDialogContext,
    sql: String,
    row_cap: usize,
    cx: &mut Context<V>,
    on_done: impl FnOnce(&mut V, ToolQueryOutcome, &mut Context<V>) + 'static,
) {
    let services = context.services.clone();
    let profile = context.profile.clone();
    // Run `execute_collect` off the UI thread as a gpui background `Task`, then
    // let one foreground task await it and update the entity.
    //
    // On native we `block_on` the query so the background task runs it to
    // completion synchronously on its own gpui background thread and never
    // suspends onto — and resumes from — a tokio worker thread. That matters
    // for tests: if the background task resumed on a tokio thread, completing
    // it would wake the foreground task from that thread, and gpui's
    // TestScheduler pins local tasks to their spawning thread — the cross-thread
    // wake aborts the whole test binary (SIGABRT). Awaiting the gpui `Task`
    // (not a raw tokio channel) keeps the foreground wake on a gpui thread.
    let task = cx.background_spawn(collect_on_background(services, profile, sql, row_cap));
    cx.spawn(async move |this, cx| {
        if let Some(outcome) = task.await {
            this.update(cx, |this, cx| on_done(this, outcome, cx)).ok();
        }
    })
    .detach();
}

/// The error text a run stopped through [`QueryCancel::cancel`] reports.
pub const CANCELLED_MESSAGE: &str = "Stopped before the run finished";

/// A handle on a [`run_query_cancellable`] run: Stop asks the engine to
/// cancel the query (the driver releases its loop and drops the
/// connection) and the collector stops reading between events, so the
/// outcome arrives promptly with [`CANCELLED_MESSAGE`] as its error.
#[derive(Clone)]
pub struct QueryCancel {
    task_id: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    services: Services,
}

impl QueryCancel {
    pub fn cancel<V: 'static>(&self, cx: &mut Context<V>) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let task_id = self.task_id.lock().ok().and_then(|slot| slot.clone());
        let Some(task_id) = task_id else {
            return;
        };
        let services = self.services.clone();
        cx.background_spawn(async move {
            cancel_on_background(services, task_id).await;
        })
        .detach();
    }
}

/// [`run_query_with_cap`] whose run can be stopped from the UI: the
/// returned handle cancels it. The outcome still arrives through `on_done`
/// — with [`CANCELLED_MESSAGE`] as its error when it was stopped — so the
/// caller's freshness token works unchanged.
pub fn run_query_cancellable<V: 'static>(
    context: &ToolDialogContext,
    sql: String,
    row_cap: usize,
    cx: &mut Context<V>,
    on_done: impl FnOnce(&mut V, ToolQueryOutcome, &mut Context<V>) + 'static,
) -> QueryCancel {
    let handle = QueryCancel {
        task_id: Default::default(),
        stop: Default::default(),
        services: context.services.clone(),
    };
    let services = context.services.clone();
    let profile = context.profile.clone();
    let task_slot = handle.task_id.clone();
    let stop = handle.stop.clone();
    let task = cx.background_spawn(collect_cancellable_on_background(
        services, profile, sql, row_cap, task_slot, stop,
    ));
    cx.spawn(async move |this, cx| {
        if let Some(outcome) = task.await {
            this.update(cx, |this, cx| on_done(this, outcome, cx)).ok();
        }
    })
    .detach();
    handle
}

/// [`execute_collect_with_cap`] that publishes its task id for cancellation
/// and stops reading when the flag flips, cancelling the engine's run.
async fn execute_collect_cancellable(
    services: &Services,
    profile: &ConnectionProfile,
    sql: &str,
    row_cap: usize,
    task_slot: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> ToolQueryOutcome {
    let mut outcome = ToolQueryOutcome::default();
    let mut handle = match services.queries.execute(profile, sql, 256).await {
        Ok(handle) => handle,
        Err(e) => {
            outcome.error = Some(e.to_string());
            return outcome;
        }
    };
    if let Ok(mut slot) = task_slot.lock() {
        *slot = Some(handle.task_id.clone());
    }
    let stopped =
        |stop: &std::sync::atomic::AtomicBool| stop.load(std::sync::atomic::Ordering::Relaxed);
    if stopped(&stop) {
        let _ = services.queries.cancel(&handle.task_id).await;
        outcome.error = Some(CANCELLED_MESSAGE.to_string());
        return outcome;
    }
    while let Some(event) = handle.events.recv().await {
        if stopped(&stop) {
            let _ = services.queries.cancel(&handle.task_id).await;
            outcome.error = Some(CANCELLED_MESSAGE.to_string());
            break;
        }
        match event {
            QueryEvent::ResultSet { index, columns } => {
                if outcome.sets.len() <= index {
                    outcome.sets.resize_with(index + 1, ToolQuerySet::default);
                }
                outcome.sets[index].columns = columns;
            }
            QueryEvent::Rows { index, rows } => {
                if outcome.sets.len() <= index {
                    outcome.sets.resize_with(index + 1, ToolQuerySet::default);
                }
                let set = &mut outcome.sets[index];
                let room = row_cap.saturating_sub(set.rows.len());
                set.rows.extend(rows.into_iter().take(room));
            }
            QueryEvent::Error { message } => {
                outcome.error = Some(if stopped(&stop) {
                    CANCELLED_MESSAGE.to_string()
                } else {
                    message
                });
            }
            QueryEvent::Cancelled => {
                outcome.error = Some(CANCELLED_MESSAGE.to_string());
                break;
            }
            QueryEvent::Completed { .. } => break,
            _ => {}
        }
    }
    outcome
}

async fn collect_cancellable_on_background(
    services: Services,
    profile: ConnectionProfile,
    sql: String,
    row_cap: usize,
    task_slot: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Option<ToolQueryOutcome> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        Some(crate::tokio_handle().block_on(execute_collect_cancellable(
            &services, &profile, &sql, row_cap, task_slot, stop,
        )))
    }
    #[cfg(target_arch = "wasm32")]
    {
        crate::tokio_handle()
            .spawn(async move {
                execute_collect_cancellable(&services, &profile, &sql, row_cap, task_slot, stop)
                    .await
            })
            .await
            .ok()
    }
}

async fn cancel_on_background(services: Services, task_id: String) {
    #[cfg(not(target_arch = "wasm32"))]
    {
        crate::tokio_handle().block_on(async {
            let _ = services.queries.cancel(&task_id).await;
        });
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = crate::tokio_handle()
            .spawn(async move {
                let _ = services.queries.cancel(&task_id).await;
            })
            .await;
    }
}

/// The background half of [`run_query_with_cap`]: run the collection to
/// completion on the calling gpui background thread (native: `block_on`, so
/// the task never resumes from a tokio worker; wasm: the reactor-free shim).
async fn collect_on_background(
    services: Services,
    profile: ConnectionProfile,
    sql: String,
    row_cap: usize,
) -> Option<ToolQueryOutcome> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        Some(
            crate::tokio_handle()
                .block_on(execute_collect_with_cap(&services, &profile, &sql, row_cap)),
        )
    }
    #[cfg(target_arch = "wasm32")]
    {
        crate::tokio_handle()
            .spawn(
                async move { execute_collect_with_cap(&services, &profile, &sql, row_cap).await },
            )
            .await
            .ok()
    }
}

/// Build a themed `SqllyDataTable` for one result set. `None` when the set
/// has no columns (nothing to show).
/// Collapse newlines in a string cell to single spaces so the grid can shape it
/// as one line. Non-string cells and newline-free strings pass through untouched
/// (no allocation), so this is cheap for the common case.
fn single_line_cell(cell: &CellValue) -> CellValue {
    match cell {
        CellValue::String(s) if s.contains(['\n', '\r']) => {
            CellValue::String(s.replace("\r\n", " ").replace(['\n', '\r'], " "))
        }
        other => other.clone(),
    }
}

/// The grid config for a tool pane: the user's Results type defaults
/// (integers without decimals, the decimal/date/boolean formats, the NULL
/// marker, motion) so tool grids read like the results grid. The crate's
/// bare `GridConfig::default()` renders every integer column — counts,
/// ranks, session ids — with two decimals ("76.00"). Per-column-name
/// overrides and conditional-formatting rules are keyed to query result
/// columns, so they stay off here.
fn tool_grid_config(columns: &[ResultColumn]) -> sqlly_datatable::GridConfig {
    tool_grid_config_from(
        crate::preferences::ResultFormattingPreferences::load(),
        columns,
    )
}

fn tool_grid_config_from(
    mut prefs: crate::preferences::ResultFormattingPreferences,
    columns: &[ResultColumn],
) -> sqlly_datatable::GridConfig {
    prefs.column_overrides.clear();
    let mut config = crate::grid_bridge::build_grid_config(None, &prefs, columns);
    config.conditional_rules.clear();
    config
}

pub fn build_result_grid(set: &ToolQuerySet, cx: &mut gpui::App) -> Option<Entity<SqllyDataTable>> {
    if set.columns.is_empty() {
        return None;
    }
    // The grid paints every cell as a single shaped line, and GPUI's shaper
    // panics ("text argument should not contain newlines") on an embedded
    // newline. Tool-dialog result sets routinely carry multi-line SQL
    // (Query Insights' `query_sql_text`, Extended Events), so collapse newlines
    // to spaces for display. Callers keep the untouched `set.rows` for their
    // "open the reviewed SQL" actions, so the full text is never lost.
    let display_rows: Vec<Vec<CellValue>> = set
        .rows
        .iter()
        .map(|row| row.iter().map(single_line_cell).collect())
        .collect();
    let data = crate::grid_bridge::build_grid_data(None, &set.columns, &display_rows).ok()?;
    // Builder-supplied theme keeps the grid host-themed; a post-build
    // `state.theme` write is overwritten by the widget's own palette on the
    // next render.
    let theme = crate::grid_bridge::build_grid_theme();
    let view = SqllyDataTable::builder(data)
        .config(tool_grid_config(&set.columns))
        .context_menu_visibility(crate::grid_bridge::grid_menu_tooltip_policy)
        .theme(theme)
        .build(cx);
    Some(cx.new(|_cx| view))
}

/// First DISPLAY row of a grid selection (cell or whole-row selections), if
/// any. Display rows follow the grid's current sort/filter — never index a
/// pane's own row vector with this; use [`selected_source_row`].
pub fn selected_row(sel: &sqlly_datatable::Selection) -> Option<usize> {
    use sqlly_datatable::Selection;
    match sel {
        Selection::Cell(row, _) | Selection::Row(row) => Some(*row),
        Selection::CellRange(r1, _, r2, _) | Selection::RowRange(r1, r2) => Some((*r1).min(*r2)),
        Selection::Cells(cells) => cells.first().map(|(row, _)| *row),
        Selection::Rows(rows) => rows.first().copied(),
        _ => None,
    }
}

/// The SOURCE row (index into the rows the pane handed the grid) behind the
/// grid's selection, mapped through the grid's sort/filter. Shared by every
/// pane that acts on "the selected row" (Query Store, SQL Agent, Activity
/// Monitor, Index Analyzer, Extended Events, ...): after a header sort the
/// display row and the source row differ, and acting on the display index
/// would target a different job/session/index than the one highlighted.
pub fn selected_source_row(
    grid: &gpui::Entity<sqlly_datatable::SqllyDataTable>,
    cx: &gpui::App,
) -> Option<usize> {
    let state = grid.read(cx).state.read(cx);
    let display = selected_row(&state.selection)?;
    state.resident_row_for_display(display)
}

/// Layout check for the tool panes' embedded grids, run after a real draw:
/// the grid's painted body must have room for at least `min_rows` data rows
/// below its header. A grid mounted in an auto-height wrapper resolves its
/// `size_full` root to zero height and paints nothing at all, even though
/// the pane holds every row.
#[cfg(test)]
pub(crate) fn assert_grid_shows_rows(
    grid: &Entity<SqllyDataTable>,
    min_rows: usize,
    what: impl AsRef<str>,
    cx: &gpui::App,
) {
    let what = what.as_ref();
    let state = grid.read(cx).state.read(cx);
    let height = f32::from(state.bounds.size.height);
    let body = height - state.header_height;
    let needed = state.row_height * min_rows as f32;
    assert!(
        body >= needed,
        "{what}: the grid painted {height}px tall ({body}px below the header), \
         which cannot show {min_rows} row(s) of {}px",
        state.row_height
    );
}

/// A tool context whose connection refuses at once (127.0.0.1:1): a pane's
/// own opening query fails fast, and the test installs the rows itself.
#[cfg(all(test, not(target_arch = "wasm32")))]
pub(crate) fn unreachable_tool_context(db: sqlly_core::DatabaseType) -> ToolDialogContext {
    use sqlly_core::{SecretProvider, SecretRef};
    crate::ensure_test_tokio_runtime();
    // The opening query's connect needs a TLS provider, as `main` installs.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let secret = SecretRef {
        provider: SecretProvider::InMemory,
        key: "k".to_string(),
    };
    let mut profile = ConnectionProfile::sql_login("tool-test", "127.0.0.1", "sa", secret);
    profile.database_type = db;
    profile.port = 1;
    ToolDialogContext::new(profile, Services::new())
}

/// A small three-column result for a tool grid.
#[cfg(test)]
pub(crate) fn sample_tool_set(rows: usize) -> ToolQuerySet {
    ToolQuerySet {
        columns: ["name", "count", "bytes"]
            .iter()
            .map(|name| ResultColumn {
                name: (*name).to_string(),
                data_type: if *name == "name" { "varchar" } else { "bigint" }.to_string(),
                nullable: true,
            })
            .collect(),
        rows: (0..rows)
            .map(|i| {
                vec![
                    CellValue::String(format!("row{i}")),
                    CellValue::I64(i as i64),
                    CellValue::I64(1024 * i as i64),
                ]
            })
            .collect(),
    }
}

/// Test host shaped like `AppView`'s root stack: the full-size workbench,
/// then an open overlay entity as the next sibling. An overlay that renders
/// a plain block instead of the dialog chrome's absolute overlay is laid
/// out below the workbench — outside the window — in this stack, exactly
/// as in the app.
#[cfg(test)]
pub(crate) struct OverlayStack {
    pub overlay: gpui::AnyView,
}

#[cfg(test)]
impl gpui::Render for OverlayStack {
    fn render(
        &mut self,
        _window: &mut gpui::Window,
        _cx: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        use gpui::{ParentElement as _, Styled as _};
        gpui::div()
            .size_full()
            .child(gpui::div().size_full())
            .child(self.overlay.clone())
    }
}

/// Assert a dialog card (`dialog-card-<id>`) painted fully inside a window
/// of `window_h` points.
#[cfg(test)]
pub(crate) fn assert_card_inside_window(
    cx: &mut gpui::VisualTestContext,
    card_selector: &'static str,
    window_h: f32,
) -> gpui::Bounds<gpui::Pixels> {
    let card = cx
        .debug_bounds(card_selector)
        .unwrap_or_else(|| panic!("{card_selector} is painted"));
    let top = f32::from(card.origin.y);
    let bottom = top + f32::from(card.size.height);
    assert!(
        top >= 0.0 && bottom <= window_h,
        "{card_selector} lies inside the {window_h}px window: {card:?}"
    );
    card
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every tool pane indexes its own row vector with the selection. After a
    /// header sort the highlighted row is a different source row, so the
    /// lookup must go through the grid's display→source mapping — otherwise
    /// "Kill session" / "Run job" / "Script index" act on another row.
    #[gpui::test]
    fn selected_source_row_follows_a_sorted_grid(cx: &mut gpui::TestAppContext) {
        let set = ToolQuerySet {
            columns: vec![sqlly_core::ResultColumn {
                name: "job".into(),
                data_type: "nvarchar".into(),
                nullable: false,
            }],
            rows: ["alpha", "bravo", "charlie"]
                .iter()
                .map(|name| vec![CellValue::String((*name).into())])
                .collect(),
        };
        let grid = cx.update(|cx| {
            if cx.try_global::<gpui_component::Theme>().is_none() {
                gpui_component::init(cx);
            }
            build_result_grid(&set, cx).expect("grid")
        });
        let select_first_display_row = |cx: &mut gpui::TestAppContext, sel| {
            cx.update(|cx| {
                let state = grid.read(cx).state.clone();
                state.update(cx, |state, _| state.selection = sel);
            })
        };
        select_first_display_row(cx, sqlly_datatable::Selection::Cell(0, 0));
        assert_eq!(cx.update(|cx| selected_source_row(&grid, cx)), Some(0));
        cx.update(|cx| {
            let state = grid.read(cx).state.clone();
            state.update(cx, |state, _| {
                // Descending: "charlie" (source row 2) is displayed first.
                state.toggle_sort(0);
                state.toggle_sort(0);
            });
        });
        select_first_display_row(cx, sqlly_datatable::Selection::Cell(0, 0));
        assert_eq!(cx.update(|cx| selected_source_row(&grid, cx)), Some(2));
        // Whole-row selections (row-header clicks) resolve too.
        select_first_display_row(cx, sqlly_datatable::Selection::Row(0));
        assert_eq!(cx.update(|cx| selected_source_row(&grid, cx)), Some(2));
    }

    /// Tool grids used the crate's bare default config, which formats
    /// integer columns with two decimals: Redis Memory Analyzer ranks read
    /// "1.00" and key counts "76.00". They follow the Results type defaults
    /// now, without the per-column-name overrides meant for query results.
    #[test]
    fn tool_grids_format_integer_columns_without_decimals() {
        let columns = vec![sqlly_core::ResultColumn {
            name: "sampled_keys".into(),
            data_type: "integer".into(),
            nullable: false,
        }];
        let mut prefs = crate::preferences::ResultFormattingPreferences::default();
        // A per-name override that would show the column as a decimal.
        let two_places = crate::preferences::ColumnFormat::default_for_kind(
            crate::preferences::ResultKind::Decimal,
        );
        prefs
            .column_overrides
            .insert("sampled_keys".into(), two_places);
        let config = tool_grid_config_from(prefs, &columns);
        let format = config.resolve(0, sqlly_datatable::ColumnKind::Integer);
        let (text, _) =
            sqlly_datatable::format::format_cell(&sqlly_datatable::CellValue::Integer(76), &format);
        assert_eq!(text, "76");
        assert!(config.conditional_rules.is_empty());
    }

    #[test]
    fn single_line_cell_collapses_newlines() {
        // Multi-line SQL (the Query Insights crash case) becomes one line.
        let multiline = CellValue::String("SELECT *\r\nFROM t\nWHERE x = 1".to_string());
        let CellValue::String(flat) = single_line_cell(&multiline) else {
            panic!("expected string");
        };
        assert!(!flat.contains('\n') && !flat.contains('\r'), "{flat}");
        assert_eq!(flat, "SELECT * FROM t WHERE x = 1");
        // Newline-free strings and non-strings are unchanged.
        assert_eq!(
            single_line_cell(&CellValue::String("plain".into())),
            CellValue::String("plain".into())
        );
        assert_eq!(single_line_cell(&CellValue::I64(7)), CellValue::I64(7));
    }

    #[test]
    fn row_cap_bounds_each_set() {
        // Pure check of the cap arithmetic execute_collect applies.
        let mut set = ToolQuerySet::default();
        let room = TOOL_QUERY_ROW_CAP.saturating_sub(set.rows.len());
        let incoming: Vec<Vec<CellValue>> =
            (0..TOOL_QUERY_ROW_CAP + 100).map(|_| Vec::new()).collect();
        set.rows.extend(incoming.into_iter().take(room));
        assert_eq!(set.rows.len(), TOOL_QUERY_ROW_CAP);
    }
}
