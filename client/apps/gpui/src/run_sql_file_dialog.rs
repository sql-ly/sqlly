//! Run SQL File — stream a script to the captured connection with review and progress.

use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use gpui::{
    Entity, EventEmitter, FocusHandle, IntoElement, ParentElement, Render, Styled, Subscription,
    Window, div, prelude::*, px, relative,
};
use gpui_component::Disableable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::select::{Select, SelectEvent, SelectState};
use sqlly_intel::query::safety::classify_select_only;
use sqlly_lang::sqllex::StreamSplitter;
use sqlly_lang::sqlmodel::Dialect;
use sqlly_services::mysql_definer::{DefinerMode, DefinerRewriter};
use sqlly_services::{
    ErrorDecision, OnError, ScriptCancel, ScriptOutcome, ScriptProgress, ScriptRunOptions,
    ScriptUnitFailure, TransactionMode, open_script_file, run_script,
};
use tokio::sync::{mpsc, oneshot};

use crate::app::StatusLevel;
use crate::dialog::{Dialog, DialogFrame};
use crate::mutation_review::SafetyFeatures;
use crate::theme::Theme;
use crate::tool_dialog::{ToolDialogContext, ToolDialogEvent};

const BATCH_SIZE_MIN: usize = 1;
const BATCH_SIZE_MAX: usize = 500;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Page {
    Setup,
    Review,
    Progress,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum OnErrorChoice {
    #[default]
    Ask,
    Stop,
    Continue,
}

impl OnErrorChoice {
    /// Order of the on-error select's labels.
    const ALL: [Self; 3] = [Self::Ask, Self::Stop, Self::Continue];

    fn from_row(row: Option<usize>) -> Self {
        row.and_then(|row| Self::ALL.get(row).copied())
            .unwrap_or(Self::Ask)
    }

    fn label(self) -> String {
        let key = match self {
            Self::Ask => "tools.run_sql_file.on_error_ask",
            Self::Stop => "tools.run_sql_file.on_error_stop",
            Self::Continue => "tools.run_sql_file.on_error_continue",
        };
        crate::localization::text(key)
    }

    fn to_on_error(self) -> OnError {
        match self {
            Self::Ask => OnError::Ask,
            Self::Stop => OnError::Stop,
            Self::Continue => OnError::Continue,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum TransactionChoice {
    #[default]
    None,
    Single,
}

impl TransactionChoice {
    /// Order of the transaction select's labels.
    const ALL: [Self; 2] = [Self::None, Self::Single];

    fn from_row(row: Option<usize>) -> Self {
        row.and_then(|row| Self::ALL.get(row).copied())
            .unwrap_or(Self::None)
    }

    fn label(self) -> String {
        let key = match self {
            Self::None => "tools.run_sql_file.transaction_none",
            Self::Single => "tools.run_sql_file.transaction_single",
        };
        crate::localization::text(key)
    }

    fn to_mode(self) -> TransactionMode {
        match self {
            Self::None => TransactionMode::None,
            Self::Single => TransactionMode::Single,
        }
    }
}

/// What to do with MySQL / MariaDB `DEFINER=` clauses before running.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum DefinerChoice {
    #[default]
    Keep,
    Strip,
    CurrentUser,
}

impl DefinerChoice {
    /// Order of the DEFINER select's labels.
    const ALL: [Self; 3] = [Self::Keep, Self::Strip, Self::CurrentUser];

    fn from_row(row: Option<usize>) -> Self {
        row.and_then(|row| Self::ALL.get(row).copied())
            .unwrap_or(Self::Keep)
    }

    fn label(self) -> String {
        let key = match self {
            Self::Keep => "tools.run_sql_file.definer_keep",
            Self::Strip => "tools.run_sql_file.definer_strip",
            Self::CurrentUser => "tools.run_sql_file.definer_current_user",
        };
        crate::localization::text(key)
    }

    /// The rewrite to apply; DEFINER clauses only exist in MySQL-family
    /// scripts, so other engines always run the file as written.
    fn to_mode(self, dialect: Dialect) -> Option<DefinerMode> {
        if dialect != Dialect::MySql {
            return None;
        }
        match self {
            Self::Keep => None,
            Self::Strip => Some(DefinerMode::Strip),
            Self::CurrentUser => Some(DefinerMode::CurrentUser),
        }
    }

    /// Review-page line for a script that holds `count` DEFINER clauses.
    fn review_line(self, count: usize) -> Option<String> {
        if count == 0 {
            return None;
        }
        let key = match self {
            Self::Keep => "tools.run_sql_file.review_definer_keep",
            Self::Strip => "tools.run_sql_file.review_definer_strip",
            Self::CurrentUser => "tools.run_sql_file.review_definer_current_user",
        };
        Some(crate::localization::format(
            key,
            &[("count", &count.to_string())],
        ))
    }
}

/// Form state for validation and execution options.
#[derive(Clone, Debug, Default)]
pub struct RunSqlFileModel {
    pub path: String,
    pub on_error: OnErrorChoice,
    pub transaction: TransactionChoice,
    pub coalesce: bool,
    pub batch_size: usize,
    pub definer: DefinerChoice,
}

impl RunSqlFileModel {
    pub fn validate(&self) -> Result<(), String> {
        if self.path.trim().is_empty() {
            return Err(crate::localization::text(
                "tools.run_sql_file.file_required",
            ));
        }
        if self.batch_size < BATCH_SIZE_MIN || self.batch_size > BATCH_SIZE_MAX {
            return Err(crate::localization::format(
                "tools.run_sql_file.batch_size_invalid",
                &[
                    ("minimum", &BATCH_SIZE_MIN.to_string()),
                    ("maximum", &BATCH_SIZE_MAX.to_string()),
                ],
            ));
        }
        Ok(())
    }

    pub fn to_options(&self, dialect: Dialect, session_key: String) -> ScriptRunOptions {
        ScriptRunOptions {
            dialect,
            on_error: self.on_error.to_on_error(),
            transaction: self.transaction.to_mode(),
            coalesce_inserts: self.coalesce && dialect != Dialect::SqlServer,
            session_key,
            coalesce_max_rows: self.batch_size,
            coalesce_max_bytes: 1024 * 1024,
            definer: self.definer.to_mode(dialect),
        }
    }
}

#[derive(Clone, Debug, Default)]
struct ScanSummary {
    units: usize,
    first_lines: String,
    has_go: bool,
    mutating_units: usize,
    total_bytes: u64,
    /// DEFINER clauses in the script (MySQL family only).
    definer_clauses: usize,
}

impl EventEmitter<ToolDialogEvent> for RunSqlFileDialog {}

pub struct RunSqlFileDialog {
    context: ToolDialogContext,
    page: Page,
    model: RunSqlFileModel,
    path_input: Entity<InputState>,
    on_error_select: Entity<SelectState<Vec<String>>>,
    transaction_select: Entity<SelectState<Vec<String>>>,
    definer_select: Entity<SelectState<Vec<String>>>,
    batch_input: Entity<InputState>,
    scan: Option<ScanSummary>,
    scanning: bool,
    run_blocked_reason: Option<String>,
    running: bool,
    progress: Option<ScriptProgress>,
    failures: Vec<ScriptUnitFailure>,
    pending_failure: Option<(ScriptUnitFailure, oneshot::Sender<ErrorDecision>)>,
    last_summary: Option<String>,
    cancel: Option<ScriptCancel>,
    cancel_flag: Option<Arc<AtomicBool>>,
    focus_handle: FocusHandle,
    frame: DialogFrame,
    selects_dirty: bool,
    _subscriptions: Vec<Subscription>,
    _tasks: Vec<gpui::Task<()>>,
}

impl RunSqlFileDialog {
    pub fn new(
        context: ToolDialogContext,
        preselected_path: Option<PathBuf>,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        let on_error_select = cx.new(|cx| {
            SelectState::new(
                OnErrorChoice::ALL.map(OnErrorChoice::label).to_vec(),
                Some(gpui_component::IndexPath::new(0)),
                window,
                cx,
            )
        });
        let transaction_select = cx.new(|cx| {
            SelectState::new(
                TransactionChoice::ALL
                    .map(TransactionChoice::label)
                    .to_vec(),
                Some(gpui_component::IndexPath::new(0)),
                window,
                cx,
            )
        });
        let definer_select = cx.new(|cx| {
            SelectState::new(
                DefinerChoice::ALL.map(DefinerChoice::label).to_vec(),
                Some(gpui_component::IndexPath::new(0)),
                window,
                cx,
            )
        });
        let path_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("/path/to/script.sql"));
        let batch_input = cx.new(|cx| InputState::new(window, cx).placeholder("500"));

        let subscriptions = vec![
            cx.subscribe_in(
                &path_input,
                window,
                |this: &mut Self, state, event: &InputEvent, _window, cx| {
                    if let InputEvent::Change = event {
                        this.model.path = state.read(cx).value().to_string();
                        cx.notify();
                    }
                },
            ),
            cx.subscribe_in(
                &batch_input,
                window,
                |this: &mut Self, state, event: &InputEvent, _window, cx| {
                    if let InputEvent::Change = event {
                        let text = state.read(cx).value().to_string();
                        if let Ok(n) = text.trim().parse::<usize>() {
                            this.model.batch_size = n;
                        }
                        cx.notify();
                    }
                },
            ),
            cx.subscribe_in(
                &on_error_select,
                window,
                |this: &mut Self, select, event: &SelectEvent<Vec<String>>, _window, cx| {
                    let SelectEvent::Confirm(value) = event;
                    if value.is_some() {
                        this.model.on_error = OnErrorChoice::from_row(
                            select.read(cx).selected_index(cx).map(|ix| ix.row),
                        );
                        cx.notify();
                    }
                },
            ),
            cx.subscribe_in(
                &transaction_select,
                window,
                |this: &mut Self, select, event: &SelectEvent<Vec<String>>, _window, cx| {
                    let SelectEvent::Confirm(value) = event;
                    if value.is_some() {
                        this.model.transaction = TransactionChoice::from_row(
                            select.read(cx).selected_index(cx).map(|ix| ix.row),
                        );
                        cx.notify();
                    }
                },
            ),
            cx.subscribe_in(
                &definer_select,
                window,
                |this: &mut Self, select, event: &SelectEvent<Vec<String>>, _window, cx| {
                    let SelectEvent::Confirm(value) = event;
                    if value.is_some() {
                        this.model.definer = DefinerChoice::from_row(
                            select.read(cx).selected_index(cx).map(|ix| ix.row),
                        );
                        cx.notify();
                    }
                },
            ),
        ];

        let mut model = RunSqlFileModel {
            batch_size: 500,
            ..Default::default()
        };
        if let Some(path) = &preselected_path {
            model.path = path.display().to_string();
        }

        let this = Self {
            context,
            page: Page::Setup,
            model,
            path_input,
            on_error_select,
            transaction_select,
            definer_select,
            batch_input,
            scan: None,
            scanning: false,
            run_blocked_reason: None,
            running: false,
            progress: None,
            failures: Vec::new(),
            pending_failure: None,
            last_summary: None,
            cancel: None,
            cancel_flag: None,
            focus_handle: cx.focus_handle(),
            frame: DialogFrame::new_auto(Theme::scaled(520.0)),
            selects_dirty: true,
            _subscriptions: subscriptions,
            _tasks: Vec::new(),
        };
        if let Some(path) = preselected_path {
            this.path_input.update(cx, |state, cx| {
                state.set_value(path.display().to_string(), window, cx);
            });
        }
        this
    }

    pub fn take_last_summary(&mut self) -> Option<String> {
        self.last_summary.take()
    }

    fn close(&mut self, cx: &mut gpui::Context<Self>) {
        if let Some(cancel) = &self.cancel {
            cancel.cancel();
        }
        if let Some(flag) = &self.cancel_flag {
            flag.store(true, Ordering::Relaxed);
        }
        cx.emit(ToolDialogEvent::Close);
    }

    fn choose_file(&mut self, cx: &mut gpui::Context<Self>) {
        let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(crate::localization::text("tools.run_sql_file.dialog_title").into()),
        });
        let weak = cx.weak_entity();
        let async_cx = cx.to_async();
        cx.spawn(async move |_this, _cx| {
            if let Ok(Ok(Some(paths))) = rx.await
                && let Some(path) = paths.into_iter().next()
            {
                let mut cx = async_cx.clone();
                weak.update(&mut cx, |this, cx| {
                    this.model.path = path.display().to_string();
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    fn start_scan(&mut self, cx: &mut gpui::Context<Self>) {
        if let Err(reason) = self.model.validate() {
            cx.emit(ToolDialogEvent::Status {
                level: StatusLevel::Info,
                message: reason,
            });
            return;
        }
        self.scanning = true;
        self.scan = None;
        self.run_blocked_reason = None;
        let path = PathBuf::from(self.model.path.trim());
        let dialect = self.context.profile.database_type.family();
        let weak = cx.weak_entity();
        let async_cx = cx.to_async();

        cx.spawn(async move |_this, _cx| {
            let summary = tokio::task::spawn_blocking(move || scan_file(&path, dialect)).await;
            let mut cx = async_cx.clone();
            weak.update(&mut cx, |this, cx| {
                this.scanning = false;
                match summary {
                    Ok(Ok(scan)) => {
                        this.apply_scan_review(scan);
                        this.page = Page::Review;
                    }
                    Ok(Err(err)) => {
                        cx.emit(ToolDialogEvent::Status {
                            level: StatusLevel::Error,
                            message: err,
                        });
                    }
                    Err(_) => {
                        cx.emit(ToolDialogEvent::Status {
                            level: StatusLevel::Error,
                            message: crate::localization::text("tools.run_sql_file.scan_failed"),
                        });
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn apply_scan_review(&mut self, scan: ScanSummary) {
        let store = crate::connection::ConnectionStore::global();
        let safety = SafetyFeatures::effective_for_profile(&self.context.profile, store);
        if safety.require_ai_review_for_data_changes && scan.mutating_units > 0 {
            self.run_blocked_reason = Some(crate::localization::text(
                "tools.run_sql_file.ai_review_required",
            ));
        } else if self.model.transaction == TransactionChoice::Single
            && scan.has_go
            && self.context.profile.database_type.family() == Dialect::SqlServer
        {
            self.run_blocked_reason = Some(crate::localization::text(
                "tools.run_sql_file.transaction_go_blocked",
            ));
        }
        self.scan = Some(scan);
    }

    fn start_run(&mut self, cx: &mut gpui::Context<Self>) {
        if self.run_blocked_reason.is_some() {
            return;
        }
        if let Err(reason) = self.model.validate() {
            cx.emit(ToolDialogEvent::Status {
                level: StatusLevel::Info,
                message: reason,
            });
            return;
        }
        self.page = Page::Progress;
        self.running = true;
        self.failures.clear();
        self.progress = None;
        self.pending_failure = None;

        let cancel = ScriptCancel::new();
        let cancel_flag = Arc::new(AtomicBool::new(false));
        self.cancel = Some(cancel.clone());
        self.cancel_flag = Some(cancel_flag);

        let profile = self.context.profile.clone();
        let services = self.context.services.clone();
        let path = PathBuf::from(self.model.path.trim());
        let dialect = profile.database_type.family();
        let options = self
            .model
            .to_options(dialect, format!("run-file:{}", uuid::Uuid::new_v4()));
        let (progress_tx, mut progress_rx) = mpsc::channel::<ScriptProgress>(32);
        let (error_tx, mut error_rx) =
            mpsc::channel::<(ScriptUnitFailure, oneshot::Sender<ErrorDecision>)>(4);

        let weak = cx.weak_entity();
        let async_cx = cx.to_async();
        let weak_progress = weak.clone();
        let async_cx_progress = async_cx.clone();

        let progress_task = cx.spawn(async move |_this, _cx| {
            while let Some(progress) = progress_rx.recv().await {
                let mut cx = async_cx_progress.clone();
                weak_progress
                    .update(&mut cx, |this, cx| {
                        this.progress = Some(progress);
                        cx.notify();
                    })
                    .ok();
            }
        });

        let weak_run = weak.clone();
        let async_cx_run = async_cx.clone();
        let error_tx_run = error_tx.clone();

        cx.spawn(async move |_this, _cx| {
            let (reader, total_bytes) = match open_script_file(&path) {
                Ok(pair) => pair,
                Err(err) => {
                    let mut cx = async_cx_run.clone();
                    weak_run
                        .update(&mut cx, |this, cx| {
                            this.running = false;
                            cx.emit(ToolDialogEvent::Status {
                                level: StatusLevel::Error,
                                message: crate::localization::format(
                                    "tools.run_sql_file.open_failed",
                                    &[("error", &err.to_string())],
                                ),
                            });
                            cx.notify();
                        })
                        .ok();
                    return;
                }
            };

            let outcome = run_script(
                services.queries.as_ref(),
                &profile,
                reader,
                total_bytes,
                &options,
                progress_tx,
                cancel,
                move |failure| {
                    let error_tx = error_tx_run.clone();
                    Box::pin(async move {
                        let (tx, rx) = oneshot::channel();
                        if error_tx.send((failure.clone(), tx)).await.is_err() {
                            return ErrorDecision::Stop;
                        }
                        rx.await.unwrap_or(ErrorDecision::Stop)
                    })
                },
            )
            .await;

            let mut cx = async_cx_run.clone();
            weak_run
                .update(&mut cx, |this, cx| {
                    this.running = false;
                    this.cancel = None;
                    this.cancel_flag = None;
                    match outcome {
                        Ok(outcome) => this.finish_run(outcome, cx),
                        Err(err) => {
                            cx.emit(ToolDialogEvent::Status {
                                level: StatusLevel::Error,
                                message: crate::localization::format(
                                    "tools.run_sql_file.run_failed",
                                    &[("error", &err.to_string())],
                                ),
                            });
                        }
                    }
                    cx.notify();
                })
                .ok();
        })
        .detach();

        let weak_err = weak.clone();
        let async_cx_err = async_cx.clone();
        let error_task = cx.spawn(async move |_this, _cx| {
            while let Some((failure, reply)) = error_rx.recv().await {
                let mut cx = async_cx_err.clone();
                weak_err
                    .update(&mut cx, |this, cx| {
                        this.pending_failure = Some((failure, reply));
                        cx.notify();
                    })
                    .ok();
            }
        });

        self._tasks = vec![progress_task, error_task];
        cx.notify();
    }

    fn resolve_failure(&mut self, decision: ErrorDecision, cx: &mut gpui::Context<Self>) {
        if let Some((failure, reply)) = self.pending_failure.take() {
            self.failures.push(ScriptUnitFailure {
                decision,
                ..failure
            });
            let _ = reply.send(decision);
        }
        cx.notify();
    }

    fn finish_run(&mut self, outcome: ScriptOutcome, cx: &mut gpui::Context<Self>) {
        let summary = format_summary(&outcome);
        self.last_summary = Some(summary);
        let level = if outcome.cancelled || outcome.stopped_on_error || outcome.units_failed > 0 {
            StatusLevel::Error
        } else {
            StatusLevel::Success
        };
        let message = if outcome.cancelled {
            crate::localization::format(
                "tools.run_sql_file.run_cancelled",
                &[
                    ("done", &outcome.units_done.to_string()),
                    ("total", &outcome.units_total.to_string()),
                ],
            )
        } else if outcome.stopped_on_error {
            crate::localization::format(
                "tools.run_sql_file.run_stopped",
                &[
                    ("done", &outcome.units_done.to_string()),
                    ("total", &outcome.units_total.to_string()),
                ],
            )
        } else {
            crate::localization::format(
                "tools.run_sql_file.run_finished",
                &[
                    ("done", &outcome.units_done.to_string()),
                    ("total", &outcome.units_total.to_string()),
                    ("rows", &outcome.rows_affected.to_string()),
                    ("elapsed_ms", &outcome.elapsed_ms.to_string()),
                ],
            )
        };
        cx.emit(ToolDialogEvent::Status { level, message });
    }

    fn connection_line(&self) -> String {
        let p = &self.context.profile;
        format!("{} — {}", p.name, p.server)
    }

    fn checkbox(
        &self,
        id: &'static str,
        label: String,
        checked: bool,
        set: fn(&mut RunSqlFileModel, bool),
        cx: &mut gpui::Context<Self>,
    ) -> gpui::AnyElement {
        Checkbox::new(id)
            .label(label)
            .checked(checked)
            .on_click(cx.listener(move |this, checked, _window, cx| {
                set(&mut this.model, *checked);
                cx.notify();
            }))
            .into_any_element()
    }
}

fn scan_file(path: &std::path::Path, dialect: Dialect) -> Result<ScanSummary, String> {
    let (mut reader, total_bytes) = open_script_file(path).map_err(|e| {
        crate::localization::format(
            "tools.run_sql_file.scan_open_failed",
            &[("error", &e.to_string())],
        )
    })?;
    let mut splitter = StreamSplitter::new(dialect);
    let mut units = Vec::new();
    let mut raw = String::new();
    // Counting only: the review reports how many DEFINER clauses the chosen
    // option would touch; the run applies the rewrite itself.
    let mut definers =
        (dialect == Dialect::MySql).then(|| DefinerRewriter::new(DefinerMode::Strip));

    while let Some(chunk) = reader.next_chunk().map_err(|e| e.to_string())? {
        if raw.len() < 32_000 {
            raw.push_str(&chunk);
        }
        if let Some(counter) = definers.as_mut() {
            counter.push(chunk.as_bytes());
        }
        splitter.push(&chunk, &mut units);
    }
    splitter.finish(&mut units).map_err(|e| e.to_string())?;
    let definer_clauses = definers.as_mut().map_or(0, |counter| {
        counter.finish();
        counter.rewrites()
    });

    let mutating_units = units
        .iter()
        .filter(|u| !classify_select_only(&u.text).select_only)
        .count();
    let has_go = raw
        .lines()
        .any(|line| line.trim().eq_ignore_ascii_case("GO"));
    let first_lines = raw.lines().take(200).collect::<Vec<_>>().join("\n");

    Ok(ScanSummary {
        units: units.len(),
        first_lines,
        has_go,
        mutating_units,
        total_bytes,
        definer_clauses,
    })
}

fn format_summary(outcome: &ScriptOutcome) -> String {
    let mut text = crate::localization::format(
        "tools.run_sql_file.detailed_summary",
        &[
            ("total", &outcome.units_total.to_string()),
            ("done", &outcome.units_done.to_string()),
            ("failed", &outcome.units_failed.to_string()),
            ("skipped", &outcome.units_skipped.to_string()),
            ("rows", &outcome.rows_affected.to_string()),
            ("elapsed_ms", &outcome.elapsed_ms.to_string()),
        ],
    );
    if let Some(database) = &outcome.database_changed_to {
        text.push_str(&crate::localization::format(
            "tools.run_sql_file.database_changed",
            &[("database", database)],
        ));
    }
    if !outcome.failures.is_empty() {
        text.push_str(&crate::localization::text(
            "tools.run_sql_file.failures_heading",
        ));
        for failure in &outcome.failures {
            let decision = match failure.decision {
                ErrorDecision::Retry => {
                    crate::localization::text("tools.run_sql_file.decision_retry")
                }
                ErrorDecision::Skip => {
                    crate::localization::text("tools.run_sql_file.decision_skip")
                }
                ErrorDecision::SkipAll => {
                    crate::localization::text("tools.run_sql_file.decision_skip_all")
                }
                ErrorDecision::Stop => {
                    crate::localization::text("tools.run_sql_file.decision_stop")
                }
            };
            text.push_str(&crate::localization::format(
                "tools.run_sql_file.failure_detail",
                &[
                    ("ordinal", &failure.ordinal.to_string()),
                    ("line", &failure.start_line.to_string()),
                    ("message", &failure.message),
                    ("decision", &decision),
                ],
            ));
        }
    }
    if !outcome.messages.is_empty() {
        text.push_str(&crate::localization::text(
            "tools.run_sql_file.messages_heading",
        ));
        for message in outcome.messages.iter().take(50) {
            text.push_str(&format!("  {message}\n"));
        }
    }
    text.trim_end().to_string()
}

impl gpui::Focusable for RunSqlFileDialog {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for RunSqlFileDialog {
    fn render(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        if self.selects_dirty {
            self.path_input.update(cx, |state, cx| {
                state.set_value(self.model.path.clone(), window, cx);
            });
            self.batch_input.update(cx, |state, cx| {
                state.set_value(self.model.batch_size.to_string(), window, cx);
            });
            self.selects_dirty = false;
        }

        let dialect = self.context.profile.database_type.family();
        let coalesce_enabled = dialect != Dialect::SqlServer;

        let body = match self.page {
            Page::Setup => self
                .render_setup(coalesce_enabled, s, cx)
                .into_any_element(),
            Page::Review => self.render_review(s).into_any_element(),
            Page::Progress => self.render_progress(s).into_any_element(),
        };

        // Phase-dependent footer: setup Scan; review Back/Run; progress
        // Cancel plus the error-decision buttons while a failure is pending.
        // The chrome's default Close replaces the old Cancel buttons.
        let mut leading_buttons: Vec<gpui::AnyElement> = Vec::new();
        let mut buttons: Vec<gpui::AnyElement> = Vec::new();
        match self.page {
            Page::Setup => {
                let validation = self.model.validate().err();
                buttons.push(
                    Button::new("rsf-scan")
                        .primary()
                        .label(if self.scanning {
                            crate::localization::text("tools.run_sql_file.scanning_button")
                        } else {
                            crate::localization::text("tools.run_sql_file.scan_button")
                        })
                        .disabled(self.scanning || validation.is_some())
                        .on_click(cx.listener(|this, _, _window, cx| this.start_scan(cx)))
                        .into_any_element(),
                );
            }
            Page::Review => {
                leading_buttons.push(
                    Button::new("rsf-back")
                        .label(crate::localization::text("tools.run_sql_file.back_button"))
                        .on_click(cx.listener(|this, _, _window, cx| {
                            this.page = Page::Setup;
                            cx.notify();
                        }))
                        .into_any_element(),
                );
                buttons.push(
                    Button::new("rsf-run")
                        .primary()
                        .label(crate::localization::text("tools.run_sql_file.run_button"))
                        .disabled(self.run_blocked_reason.is_some())
                        .on_click(cx.listener(|this, _, _window, cx| this.start_run(cx)))
                        .into_any_element(),
                );
            }
            Page::Progress => {
                buttons.push(
                    Button::new("rsf-cancel-run")
                        .label(crate::localization::text(
                            "tools.run_sql_file.cancel_run_button",
                        ))
                        .disabled(!self.running)
                        .on_click(cx.listener(|this, _, _window, cx| {
                            if let Some(cancel) = &this.cancel {
                                cancel.cancel();
                            }
                            cx.notify();
                        }))
                        .into_any_element(),
                );
                if self.pending_failure.is_some() {
                    buttons.push(
                        Button::new("rsf-retry")
                            .label(crate::localization::text("tools.run_sql_file.retry_button"))
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.resolve_failure(ErrorDecision::Retry, cx)
                            }))
                            .into_any_element(),
                    );
                    buttons.push(
                        Button::new("rsf-skip")
                            .label(crate::localization::text("tools.run_sql_file.skip_button"))
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.resolve_failure(ErrorDecision::Skip, cx)
                            }))
                            .into_any_element(),
                    );
                    buttons.push(
                        Button::new("rsf-skip-all")
                            .label(crate::localization::text(
                                "tools.run_sql_file.skip_all_button",
                            ))
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.resolve_failure(ErrorDecision::SkipAll, cx);
                            }))
                            .into_any_element(),
                    );
                    buttons.push(
                        Button::new("rsf-stop")
                            .label(crate::localization::text("tools.run_sql_file.stop_button"))
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.resolve_failure(ErrorDecision::Stop, cx)
                            }))
                            .into_any_element(),
                    );
                }
            }
        }

        let focus_handle = self.focus_handle.clone();
        let mut dialog = Dialog::new("run-sql-file-dialog", |this: &mut Self| &mut this.frame)
            .title(crate::localization::text("tools.run_sql_file.dialog_title"))
            .easy_close(true)
            .key_context("RunSqlFileDialog")
            .track_focus(&focus_handle)
            .on_close(|this, _window, cx| this.close(cx))
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.80)))
                    .text_color(Theme::text_dim())
                    .child(self.connection_line()),
            )
            .child(body);
        for button in leading_buttons {
            dialog = dialog.leading_button(button);
        }
        for button in buttons {
            dialog = dialog.button(button);
        }
        dialog.build(self, window, cx)
    }
}

impl RunSqlFileDialog {
    fn field(label: String, control: impl IntoElement, s: fn(f32) -> f32) -> impl IntoElement {
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(s(8.0)))
            .child(
                div()
                    .w(px(s(110.0)))
                    .flex_shrink_0()
                    .text_size(px(Theme::font_size_ratio(0.87)))
                    .text_color(Theme::text_dim())
                    .child(label),
            )
            .child(div().flex_1().child(control))
    }

    fn render_setup(
        &mut self,
        coalesce_enabled: bool,
        s: fn(f32) -> f32,
        cx: &mut gpui::Context<Self>,
    ) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap(px(s(10.0)))
            .child(Self::field(
                crate::localization::text("tools.run_sql_file.file_label"),
                div()
                    .flex()
                    .flex_row()
                    .gap(px(s(6.0)))
                    .child(div().flex_1().child(Input::new(&self.path_input)))
                    .child(
                        Button::new("rsf-choose")
                            .label(crate::localization::text(
                                "tools.run_sql_file.choose_button",
                            ))
                            .on_click(cx.listener(|this, _, _window, cx| this.choose_file(cx))),
                    ),
                s,
            ))
            .child(Self::field(
                crate::localization::text("tools.run_sql_file.on_error_label"),
                Select::new(&self.on_error_select).w_full(),
                s,
            ))
            .child(Self::field(
                crate::localization::text("tools.run_sql_file.transaction_label"),
                Select::new(&self.transaction_select).w_full(),
                s,
            ))
            .when(
                self.context.profile.database_type.family() == Dialect::MySql,
                |this| {
                    this.child(Self::field(
                        crate::localization::text("tools.run_sql_file.definer_label"),
                        Select::new(&self.definer_select).w_full(),
                        s,
                    ))
                },
            )
            .child(
                div()
                    .pl(px(s(118.0)))
                    .opacity(if coalesce_enabled { 1.0 } else { 0.5 })
                    .child(self.checkbox(
                        "rsf-coalesce",
                        crate::localization::text("tools.run_sql_file.coalesce_label"),
                        self.model.coalesce && coalesce_enabled,
                        |m, v| m.coalesce = v,
                        cx,
                    )),
            )
            .child(Self::field(
                crate::localization::text("tools.run_sql_file.batch_size_label"),
                Input::new(&self.batch_input),
                s,
            ))
    }

    fn render_review(&self, s: fn(f32) -> f32) -> impl IntoElement {
        let scan = self.scan.as_ref();
        let units = scan.map(|s| s.units).unwrap_or(0);
        let mutating = scan.map(|s| s.mutating_units).unwrap_or(0);
        let size = scan.map(|s| s.total_bytes).unwrap_or(0);
        let preview = scan.map(|s| s.first_lines.clone()).unwrap_or_default();
        let definer_line = self
            .model
            .definer
            .review_line(scan.map(|s| s.definer_clauses).unwrap_or(0));
        let blocked = self.run_blocked_reason.clone();
        let safety_line = blocked.clone().unwrap_or_else(|| {
            crate::localization::format(
                "tools.run_sql_file.review_safety",
                &[
                    ("units", &units.to_string()),
                    ("mutating", &mutating.to_string()),
                ],
            )
        });

        div()
            .flex()
            .flex_col()
            .gap(px(s(10.0)))
            .child(div().text_size(px(Theme::font_size_ratio(0.87))).child(
                crate::localization::format(
                    "tools.run_sql_file.review_counts",
                    &[("units", &units.to_string()), ("bytes", &size.to_string())],
                ),
            ))
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.87)))
                    .text_color(if blocked.is_some() {
                        Theme::error()
                    } else {
                        Theme::text()
                    })
                    .child(safety_line),
            )
            .children(definer_line.map(|line| {
                div()
                    .text_size(px(Theme::font_size_ratio(0.87)))
                    .text_color(Theme::text())
                    .child(line)
            }))
            .child(
                div()
                    .p(px(s(8.0)))
                    .bg(Theme::sidebar_bg())
                    .border_1()
                    .border_color(Theme::border())
                    .rounded(px(s(4.0)))
                    .max_h(px(s(200.0)))
                    .overflow_hidden()
                    .text_size(px(Theme::font_size_ratio(0.80)))
                    .text_color(Theme::kind_string())
                    .whitespace_normal()
                    .child(preview),
            )
    }

    fn render_progress(&self, s: fn(f32) -> f32) -> impl IntoElement {
        let progress = self.progress.clone();
        let pct = progress
            .as_ref()
            .map(|p| {
                if p.total_bytes == 0 {
                    0.0
                } else {
                    p.bytes_read as f32 / p.total_bytes as f32
                }
            })
            .unwrap_or(0.0);
        let done = progress.as_ref().map(|p| p.units_done).unwrap_or(0);
        let failed = progress.as_ref().map(|p| p.units_failed).unwrap_or(0);
        let skipped = progress.as_ref().map(|p| p.units_skipped).unwrap_or(0);
        let total = done + failed + skipped;
        let line = progress.as_ref().map(|p| p.current_line).unwrap_or(0);
        let rows = progress.as_ref().map(|p| p.rows_affected).unwrap_or(0);
        let last_msg = progress
            .as_ref()
            .and_then(|p| p.last_message.clone())
            .unwrap_or_default();

        div()
            .flex()
            .flex_col()
            .gap(px(s(10.0)))
            .child(
                div()
                    .h(px(s(4.0)))
                    .bg(Theme::border())
                    .rounded(px(s(2.0)))
                    .child(
                        div()
                            .h_full()
                            .w(relative(pct))
                            .bg(Theme::accent())
                            .rounded(px(s(2.0))),
                    ),
            )
            .child(crate::localization::format(
                "tools.run_sql_file.progress_status",
                &[
                    ("statement", &total.to_string()),
                    ("line", &line.to_string()),
                    ("rows", &rows.to_string()),
                ],
            ))
            .when(!last_msg.is_empty(), |el| el.child(last_msg))
            .children(self.failures.iter().map(|f| {
                div()
                    .text_size(px(Theme::font_size_ratio(0.80)))
                    .text_color(Theme::error())
                    .child(crate::localization::format(
                        "tools.run_sql_file.failure_line",
                        &[("line", &f.start_line.to_string()), ("message", &f.message)],
                    ))
            }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choices_dispatch_on_row() {
        assert_eq!(OnErrorChoice::from_row(Some(0)), OnErrorChoice::Ask);
        assert_eq!(OnErrorChoice::from_row(Some(1)), OnErrorChoice::Stop);
        assert_eq!(OnErrorChoice::from_row(Some(2)), OnErrorChoice::Continue);
        assert_eq!(OnErrorChoice::from_row(None), OnErrorChoice::Ask);
        assert_eq!(
            TransactionChoice::from_row(Some(0)),
            TransactionChoice::None
        );
        assert_eq!(
            TransactionChoice::from_row(Some(1)),
            TransactionChoice::Single
        );
        assert_eq!(TransactionChoice::from_row(None), TransactionChoice::None);
    }

    #[test]
    fn validate_rejects_empty_path_and_bad_batch_size() {
        let mut model = RunSqlFileModel::default();
        assert!(model.validate().is_err());
        model.path = "/tmp/x.sql".into();
        model.batch_size = 0;
        assert!(model.validate().is_err());
        model.batch_size = 501;
        assert!(model.validate().is_err());
        model.batch_size = 1;
        assert!(model.validate().is_ok());
        model.batch_size = 500;
        assert!(model.validate().is_ok());
    }

    #[test]
    fn to_options_maps_fields() {
        let model = RunSqlFileModel {
            path: "/tmp/a.sql".into(),
            on_error: OnErrorChoice::Stop,
            transaction: TransactionChoice::Single,
            coalesce: true,
            batch_size: 100,
            definer: DefinerChoice::Strip,
        };
        let opts = model.to_options(Dialect::Postgres, "run-file:test".into());
        assert_eq!(opts.on_error, OnError::Stop);
        assert_eq!(opts.transaction, TransactionMode::Single);
        assert!(opts.coalesce_inserts);
        assert_eq!(opts.coalesce_max_rows, 100);
        // DEFINER clauses exist only in MySQL-family scripts.
        assert_eq!(opts.definer, None);
        let mysql = model.to_options(Dialect::MySql, "run-file:test".into());
        assert_eq!(mysql.definer, Some(DefinerMode::Strip));
    }

    #[test]
    fn definer_choice_dispatches_on_row_and_maps_to_a_mode() {
        assert_eq!(DefinerChoice::from_row(Some(0)), DefinerChoice::Keep);
        assert_eq!(DefinerChoice::from_row(Some(1)), DefinerChoice::Strip);
        assert_eq!(DefinerChoice::from_row(Some(2)), DefinerChoice::CurrentUser);
        assert_eq!(DefinerChoice::from_row(None), DefinerChoice::Keep);
        assert_eq!(DefinerChoice::Keep.to_mode(Dialect::MySql), None);
        assert_eq!(
            DefinerChoice::CurrentUser.to_mode(Dialect::MySql),
            Some(DefinerMode::CurrentUser)
        );
        assert_eq!(DefinerChoice::CurrentUser.to_mode(Dialect::SqlServer), None);
        assert_eq!(DefinerChoice::Strip.review_line(0), None);
        assert!(DefinerChoice::Strip.review_line(3).is_some());
    }

    #[test]
    fn scan_counts_definer_clauses_in_mysql_scripts_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dump.sql");
        std::fs::write(
            &path,
            "CREATE DEFINER=`root`@`localhost` PROCEDURE p() SELECT 1;\n/*!50013 DEFINER=`root`@`%` SQL SECURITY DEFINER */\nSELECT 'DEFINER=`x`@`y`';\n",
        )
        .unwrap();
        assert_eq!(scan_file(&path, Dialect::MySql).unwrap().definer_clauses, 2);
        assert_eq!(
            scan_file(&path, Dialect::Postgres).unwrap().definer_clauses,
            0
        );
    }
}
