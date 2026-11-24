//! The notebook tab body: a virtualized, variable-height cell list with a top
//! bar, a left rail (outline, parameters, variables, lineage, search), a
//! right inspector for the active cell, and a bottom activity panel.
//!
//! Never calls `spawn_query_execution`: the host runs cells through
//! `execute_query_with_origin(ExecuteOrigin::NotebookCell)` when this view
//! emits [`NotebookEvent::RunCell`], and reports back through
//! [`NotebookView::dispatch_result`] and the cell's results view.
//!
//! Editors are created lazily: a cell gets a live [`SqlEditor`] (requested
//! from the host, which configures services and completion) only when it is
//! edited, and editors far from the viewport or beyond the live-editor
//! budget are released. Cells without an editor draw a static highlighted
//! preview.

use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;
#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;
#[cfg(target_arch = "wasm32")]
use web_time::Instant;

use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, ListAlignment, ListState, MouseButton, MouseMoveEvent,
    ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _, Subscription, Task,
    Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::input::InputState;
use sqlly_core::DisplayMessage;
use sqlly_lang::sqlmodel::DatabaseType;
use sqlly_services::notebook::plan::{Limits, StepAction, StepStatus, merge_plan};
use sqlly_services::notebook::run_record::new_run_id;
use sqlly_services::notebook::{
    CellEvent, CellFingerprint, ConnectionRef, NotebookDoc, RandomIds, RunPlan, Schedule,
};
use sqlly_services::notebook_security::TrustDecision;

use super::autosave::{self, FileStamp, NotebookPrefs};
use super::cells::{self, CellRenderCx, EditorMetrics};
use super::connection::{CellRunTarget, ConnectionChoice};
use super::doc_adapter::{KIND_MARKDOWN, KIND_SQL, RunScope, ShellDoc};
use super::layout::NotebookLayout;
use super::preview::{self, SqlPreview};
use super::reveal::Reveal;
use crate::editor::{SqlEditor, SqlEditorEvent};
use crate::results::{ResultsView, ResultsViewEvent};
use crate::theme::Theme;

/// Live editors kept at once; the least recently used beyond this are released.
pub const MAX_LIVE_EDITORS: usize = 8;
/// Editors this many cells away from the viewport are released.
pub const RELEASE_DISTANCE: usize = 40;
/// Activity entries kept in the bottom panel.
pub const MAX_ACTIVITY: usize = 200;
pub const DEFAULT_RESULTS_HEIGHT: f32 = 240.0;
/// Rows an attachment cell's preview grid shows.
pub const ATTACHMENT_PREVIEW_ROWS: u64 = 1000;
/// Cells laid out beyond each edge of the visible list, in pixels. Every one
/// is rendered again each frame, so this stays small: scrolling renders the
/// cells it brings into view anyway.
const OVERDRAW: f32 = 200.0;
pub const MIN_RESULTS_HEIGHT: f32 = 100.0;
pub const MAX_RESULTS_HEIGHT: f32 = 900.0;
const STALENESS_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(300);

pub use sqlly_services::notebook::CellStatus;

/// Translation key of a cell status badge. A result that is not
/// presentable as current reads as out of date whatever its status.
pub fn status_label_key(status: CellStatus, stale: bool) -> &'static str {
    match status {
        CellStatus::Fresh | CellStatus::Cached | CellStatus::Partial if stale => {
            "notebook.shell.state.stale"
        }
        CellStatus::Idle => "notebook.shell.state.idle",
        CellStatus::Validating => "notebook.shell.state.validating",
        CellStatus::Waiting => "notebook.shell.state.waiting",
        CellStatus::AwaitingApproval => "notebook.shell.state.awaiting_approval",
        CellStatus::Queued => "notebook.shell.state.queued",
        CellStatus::Running => "notebook.shell.state.running",
        CellStatus::Streaming => "notebook.shell.state.streaming",
        CellStatus::Fresh => "notebook.shell.state.fresh",
        CellStatus::Cached => "notebook.shell.state.cached",
        CellStatus::Stale => "notebook.shell.state.stale",
        CellStatus::Canceled => "notebook.shell.state.canceled",
        CellStatus::Blocked => "notebook.shell.state.blocked",
        CellStatus::Failed => "notebook.shell.state.failed",
        CellStatus::Partial => "notebook.shell.state.partial",
        CellStatus::Unavailable => "notebook.shell.state.unavailable",
    }
}

/// What Run (and Cmd+Enter) does with the active cell.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RunMode {
    #[default]
    CellOnly,
    WithUpstream,
}

impl RunMode {
    pub fn scope(self) -> RunScope {
        match self {
            Self::CellOnly => RunScope::Cell,
            Self::WithUpstream => RunScope::CellWithUpstream,
        }
    }

    pub fn label_key(self) -> &'static str {
        match self {
            Self::CellOnly => "notebook.shell.top_bar.run_mode_cell",
            Self::WithUpstream => "notebook.shell.top_bar.run_mode_upstream",
        }
    }
}

/// How the host's attempt to start a cell's query went.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DispatchOutcome {
    Started,
    AwaitingApproval,
    NotStarted(DisplayMessage),
}

/// What the host needs to run one cell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunRequest {
    pub sql: String,
    pub target: CellRunTarget,
    /// What query history keeps instead of `sql` when the run binds a
    /// sensitive parameter.
    pub history_sql: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaveState {
    Saved,
    Dirty,
    Autosaved,
    Recovered,
}

pub(super) struct CellRuntime {
    pub(super) editor: Option<Entity<SqlEditor>>,
    editor_sub: Option<Subscription>,
    pub(super) results: Option<Entity<ResultsView>>,
    results_sub: Option<Subscription>,
    pub(super) results_height: f32,
    /// Fingerprint and run id of the run in flight, kept until it finishes.
    pub(super) run_fingerprint: Option<CellFingerprint>,
    pub(super) run_id: Option<String>,
    pub(super) started: Option<Instant>,
    /// The audit event written when the run was handed to the host, finished
    /// when it completes, fails or is stopped.
    pub(super) audit: Option<sqlly_services::notebook_security::AuditEvent>,
    pub(super) elapsed_ms: Option<u64>,
    pub(super) finished_at: Option<String>,
    /// Why the cell did not run or failed, as a key and arguments (or the
    /// server's own text), translated only when drawn.
    pub(super) message: Option<DisplayMessage>,
    /// Stop canceled the run in flight: whatever it still reports (a query
    /// that ignored the cancel) is dropped, never kept as the cell's result.
    pub(super) stopped: bool,
    preview: Option<Rc<SqlPreview>>,
    last_used: u64,
}

impl CellRuntime {
    /// Drop the live editor (and the preview built for the old text); the
    /// results view stays.
    pub(super) fn drop_editor(&mut self) {
        self.editor = None;
        self.editor_sub = None;
        self.preview = None;
    }
}

impl CellRuntime {
    /// The cell's message in the current language.
    pub(crate) fn message_text(&self) -> Option<String> {
        self.message.as_ref().map(crate::localization::message)
    }
}

impl Default for CellRuntime {
    fn default() -> Self {
        Self {
            editor: None,
            editor_sub: None,
            results: None,
            results_sub: None,
            results_height: DEFAULT_RESULTS_HEIGHT,
            run_fingerprint: None,
            run_id: None,
            started: None,
            audit: None,
            elapsed_ms: None,
            finished_at: None,
            message: None,
            stopped: false,
            preview: None,
            last_used: 0,
        }
    }
}

/// One run in flight: the core plan and the core scheduler's progress
/// through it. Independent cells start together within the per-connection
/// limit; a cell waits only for the steps it depends on.
pub(super) struct RunProgress {
    pub(super) plan: RunPlan,
    status: Vec<StepStatus>,
    /// Started by the schedule and handed to the host; not started yet.
    dispatching: HashSet<String>,
    /// Started by the schedule but held back while another cell waits for a
    /// change review (the review is one prompt for the whole app).
    held: VecDeque<String>,
    /// Running cells asked for again: they run once more when they finish.
    rerun: HashSet<String>,
    ran: usize,
}

impl RunProgress {
    fn new(plan: RunPlan) -> Self {
        let status = vec![StepStatus::Pending; plan.steps.len()];
        Self {
            plan,
            status,
            dispatching: HashSet::new(),
            held: VecDeque::new(),
            rerun: HashSet::new(),
            ran: 0,
        }
    }

    fn step_of(&self, id: &str) -> Option<usize> {
        self.plan.steps.iter().position(|s| s.cell_id == id)
    }

    /// Fold `next` into this run: new steps are added, finished ones start
    /// over, running ones run again once they finish.
    fn merge(&mut self, next: RunPlan) {
        let merged = merge_plan(&mut self.plan, &mut self.status, next);
        for step in merged.rerun {
            self.rerun.insert(self.plan.steps[step].cell_id.clone());
        }
    }

    fn is_running(&self, id: &str) -> bool {
        self.step_of(id)
            .is_some_and(|i| self.status[i] == StepStatus::Running)
    }
}

#[derive(Clone, Debug)]
pub(super) struct ActivityEntry {
    pub(super) at: String,
    pub(super) cell: Option<String>,
    pub(super) message: String,
    pub(super) error: bool,
    pub(super) elapsed_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DragTarget {
    Left,
    Right,
    Activity,
    Results,
}

/// What the pointer last moved over in the cell list: that cell, or the
/// gap below the list item at that index. It draws its full controls and
/// stays set while the pointer is over a menu opened from it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Hovered {
    Cell(String),
    Gap(usize),
}

struct DragState {
    target: DragTarget,
    cell: Option<String>,
    start: f32,
    start_size: f32,
}

#[derive(Clone, Debug)]
pub enum NotebookEvent {
    /// Create a live editor (and a results view, when the cell has none) for each cell.
    NeedCellEntities {
        cell_ids: Vec<String>,
    },
    RunCell {
        cell_id: String,
    },
    Dirty,
    SaveRequested,
    SaveAsRequested,
    ExportMarkdownRequested,
    ExportJupyterRequested,
    Status {
        message: String,
        error: bool,
    },
    /// Cancel the query the notebook is running.
    StopRequested,
    /// Open this SQL in a new query tab (a visual cell's Open in Editor).
    OpenSqlInEditor {
        sql: String,
    },
    /// Rail and panel sizes changed; persist them with the workspace layout.
    LayoutChanged(NotebookLayout),
    TitleChanged(String),
    /// Open the notebook as one SQL script in a new query tab.
    ConvertToScriptRequested,
    /// The file changed on disk and the user chose to reload it.
    ReloadRequested,
    /// Run the AI cell's agent; `consented` once the user approved sharing
    /// its context with a cloud model.
    AiRunRequested {
        cell_id: String,
        consented: bool,
    },
    /// The notebook wrote itself to its file (autosave); the tab is clean.
    SavedToFile,
    /// Run an analysis cell's read-only queries on its connection and
    /// answer with `analysis_reply`.
    AnalysisQuery(super::analysis::AnalysisRequest),
    /// Run a parameter's discovery query read-only and answer with
    /// [`NotebookView::param_options_loaded`].
    ParamOptionsRequested(Box<super::params::ParamOptionsRequest>),
}

impl EventEmitter<NotebookEvent> for NotebookView {}

pub struct NotebookView {
    pub(super) doc: ShellDoc,
    pub(super) list_state: ListState,
    pub(super) cells: HashMap<String, CellRuntime>,
    pub(super) active: Option<String>,
    pub(super) selected: Vec<String>,
    pub(super) editing_markdown: Option<String>,
    pub(super) hovered: Option<Hovered>,
    pending_focus: Option<String>,
    pub(super) pending_reveal: Option<super::reveal::PendingReveal>,
    focus_root_pending: bool,
    requested: HashSet<String>,
    lru_tick: u64,
    pub(super) run: Option<RunProgress>,
    pub(super) run_mode: RunMode,
    pub(super) activity: VecDeque<ActivityEntry>,
    pub(super) layout: NotebookLayout,
    drag: Option<DragState>,
    pub(super) tab_connection: Option<ConnectionChoice>,
    choices_provider: Rc<dyn Fn() -> Vec<ConnectionChoice>>,
    pub(super) choices: Vec<ConnectionChoice>,
    /// Trust as last assessed, granted or saved; `trust::live` applies the
    /// in-app edits since.
    pub(super) trust: TrustDecision,
    /// [`Self::live_trust`] as last computed: the document generation and
    /// the base decision it came from.
    pub(super) trust_live: std::cell::RefCell<Option<(u64, TrustDecision, TrustDecision)>>,
    /// The trust file could not be read or written.
    pub(super) trust_problem: Option<String>,
    /// Why the last run was refused, shown in the trust banner.
    pub(super) trust_notice: Option<String>,
    /// Whether each SQL cell's source changes data, by a hash of the source,
    /// so cell headers don't re-read the SQL every frame.
    pub(super) writes_seen: HashMap<String, (u64, bool)>,
    /// What the file scan flagged on open without blocking it, shown until
    /// dismissed.
    pub(super) scan_warnings: Vec<String>,
    pub(super) grant: Option<super::trust::GrantDraft>,
    pub(super) trust_flash: bool,
    pub(super) unresolved: Vec<ConnectionRef>,
    pub(super) elevation_prompt: Option<String>,
    pub(super) editing: super::editing::EditingState,
    dirty: bool,
    pub(super) save_state: SaveState,
    pub(super) file_path: Option<String>,
    file_stamp: Option<FileStamp>,
    pub(super) conflict: bool,
    /// The file opened is a copy of a notebook this machine knows at
    /// another path: it takes its own id before it is first saved.
    pub(super) copy_pending: bool,
    /// Unsaved changes to this file from the last session, offered until
    /// the user restores or discards them.
    pub(super) recovery_offer: Option<NotebookDoc>,
    pub(super) prefs: NotebookPrefs,
    autosave_task: Option<Task<()>>,
    staleness_task: Option<Task<()>>,
    conflict_poll: Option<Task<()>>,
    pub(super) title_input: Option<Entity<InputState>>,
    pub(super) search_input: Option<Entity<InputState>>,
    pub(super) search_query: String,
    pub(super) name_input: Option<(String, Entity<InputState>)>,
    pub(super) param_ui: super::params::ParamsState,
    pub(super) metrics: EditorMetrics,
    pub(super) ai: super::ai::NotebookAi,
    pub(super) local: super::local::LocalData,
    pub(super) local_name_input: Option<Entity<InputState>>,
    pub(super) local_inputs: HashMap<String, Vec<(&'static str, Entity<InputState>)>>,
    // Local data (and its encrypted cache) is desktop-only.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub(super) encrypted_cache: Option<Rc<dyn super::local::cache::CacheBackend>>,
    /// Cell outputs non-SQL cells read, and the cross-filter selections.
    pub(super) outputs: super::outputs::OutputRegistry,
    pub(super) visual: super::visual::VisualCells,
    pub(super) analysis: super::analysis::AnalysisState,
    pub(super) present: super::present::PresentState,
    pub(super) embedded: super::embedded::EmbeddedState,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl Focusable for NotebookView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// How a notebook tab was opened, which decides trust and recovery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotebookOpening {
    /// Created on this machine (New Notebook, Open as Notebook).
    New,
    /// Read from a file; trusted only when this machine's trust store says so.
    File,
    /// Reopened from this machine's recovery autosave.
    Recovered,
}

impl NotebookView {
    pub fn open(model: NotebookDoc, opening: NotebookOpening, cx: &mut Context<Self>) -> Self {
        Self::with_choices(
            model,
            opening,
            Rc::new(super::connection::local_choices),
            cx,
        )
    }

    /// `choices` lists the connection profiles cells can pick from.
    pub fn with_choices(
        model: NotebookDoc,
        opening: NotebookOpening,
        choices: Rc<dyn Fn() -> Vec<ConnectionChoice>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let choice_list = choices();
        let mut doc = ShellDoc::new(model);
        doc.set_connections(None, None, choice_list.clone());
        let assessed = match opening {
            NotebookOpening::New => super::trust::record_created(doc.model(), None),
            NotebookOpening::File | NotebookOpening::Recovered => {
                super::trust::assess(doc.model(), None)
            }
        };
        let list_state = ListState::new(doc.len() + 1, ListAlignment::Top, px(OVERDRAW));
        let scrolled = cx.weak_entity();
        list_state.set_scroll_handler(move |_, _, cx| {
            scrolled.update(cx, |this, _| this.cancel_reveal()).ok();
        });
        let unresolved = doc.unresolved_connections();
        let active = doc.cell(0).map(|c| c.id.to_string());
        let mut this = Self {
            doc,
            list_state,
            cells: HashMap::new(),
            active,
            selected: Vec::new(),
            editing_markdown: None,
            hovered: None,
            pending_focus: None,
            pending_reveal: None,
            focus_root_pending: false,
            requested: HashSet::new(),
            lru_tick: 0,
            run: None,
            run_mode: RunMode::default(),
            activity: VecDeque::new(),
            layout: NotebookLayout::default(),
            drag: None,
            tab_connection: None,
            choices_provider: choices,
            choices: choice_list,
            trust: assessed.decision,
            trust_live: Default::default(),
            trust_problem: None,
            trust_notice: None,
            writes_seen: HashMap::new(),
            scan_warnings: Vec::new(),
            grant: None,
            trust_flash: false,
            unresolved,
            elevation_prompt: None,
            editing: Default::default(),
            dirty: false,
            save_state: if opening == NotebookOpening::Recovered {
                SaveState::Recovered
            } else {
                SaveState::Saved
            },
            file_path: None,
            file_stamp: None,
            conflict: false,
            copy_pending: false,
            recovery_offer: None,
            prefs: NotebookPrefs::load(),
            autosave_task: None,
            staleness_task: None,
            conflict_poll: None,
            title_input: None,
            search_input: None,
            search_query: String::new(),
            name_input: None,
            param_ui: Default::default(),
            metrics: EditorMetrics::default(),
            ai: Default::default(),
            local: super::local::LocalData::new(),
            local_name_input: None,
            local_inputs: HashMap::new(),
            encrypted_cache: None,
            outputs: super::outputs::OutputRegistry::default(),
            visual: super::visual::VisualCells::default(),
            analysis: super::analysis::AnalysisState::default(),
            present: Default::default(),
            embedded: Default::default(),
            focus_handle: cx.focus_handle(),
            _subscriptions: Vec::new(),
        };
        this.note_trust_problem(assessed.problem);
        this.mark_browser_unavailable();
        this.editing_start();
        if opening == NotebookOpening::New {
            this.autosave_now(cx);
        }
        this
    }

    // ── Host API ──────────────────────────────────────────────────────────

    pub fn model(&self) -> &NotebookDoc {
        self.doc.model()
    }

    pub fn doc(&self) -> &ShellDoc {
        &self.doc
    }

    /// The document to write to disk.
    pub fn model_for_save(&self) -> NotebookDoc {
        self.doc.model().clone()
    }

    #[cfg(test)]
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    #[cfg(test)]
    pub fn is_trusted(&self) -> bool {
        self.live_trust().level != sqlly_services::notebook_security::TrustLevel::Untrusted
    }

    pub fn set_database_type(&mut self, db: DatabaseType) {
        self.doc
            .set_connections(Some(db), self.tab_connection.clone(), self.choices.clone());
    }

    pub fn set_layout(&mut self, layout: NotebookLayout) {
        self.layout = layout.sanitized();
    }

    #[cfg(test)]
    pub fn layout(&self) -> &NotebookLayout {
        &self.layout
    }

    pub fn set_editor_metrics(&mut self, metrics: EditorMetrics, cx: &mut Context<Self>) {
        if self.metrics != metrics {
            self.metrics = metrics;
            self.list_state.remeasure();
            cx.notify();
        }
    }

    /// The tab's (or app's) active connection, which cells without their
    /// own connection run on.
    pub fn set_tab_connection(&mut self, choice: Option<ConnectionChoice>, cx: &mut Context<Self>) {
        if self.tab_connection != choice {
            self.tab_connection = choice;
            self.doc
                .set_connections(None, self.tab_connection.clone(), self.choices.clone());
            self.doc.refresh_staleness();
            cx.notify();
        }
    }

    /// Where the connection pickers read the local profiles from.
    pub(crate) fn choices_source(&self) -> Rc<dyn Fn() -> Vec<ConnectionChoice>> {
        self.choices_provider.clone()
    }

    /// Re-read the connection profiles cells can pick from.
    fn refresh_choices(&mut self, cx: &mut Context<Self>) {
        self.choices = (self.choices_provider)();
        self.doc
            .set_connections(None, self.tab_connection.clone(), self.choices.clone());
        let still: Vec<String> = self
            .doc
            .unresolved_connections()
            .into_iter()
            .map(|r| r.id)
            .collect();
        self.unresolved.retain(|r| still.contains(&r.id));
        cx.notify();
    }

    /// The notebook was written to `path` (Save / Save As).
    pub fn saved_to(&mut self, path: String, cx: &mut Context<Self>) {
        let was_unsaved = self.file_path.is_none();
        self.dirty = false;
        self.save_state = SaveState::Saved;
        self.conflict = false;
        self.file_stamp = autosave::file_stamp(std::path::Path::new(&path));
        self.record_trusted_save(Some(&path));
        self.file_path = Some(path);
        if was_unsaved {
            autosave::remove_recovery(self.doc.id());
        }
        autosave::remove_file_recovery(self.doc.id());
        self.recovery_offer = None;
        self.start_conflict_poll(cx);
        cx.notify();
    }

    /// The notebook was read from `path`.
    pub fn opened_from(&mut self, path: String, cx: &mut Context<Self>) {
        self.reassess_trust_at(&path);
        self.file_stamp = autosave::file_stamp(std::path::Path::new(&path));
        self.file_path = Some(path);
        self.start_conflict_poll(cx);
    }

    #[cfg(test)]
    pub(super) fn preview_count(&self) -> usize {
        self.cells
            .values()
            .filter(|rt| rt.preview.is_some())
            .count()
    }

    #[cfg(test)]
    pub(super) fn doc_mut_for_test(&mut self) -> &mut ShellDoc {
        &mut self.doc
    }

    #[cfg(test)]
    pub(super) fn mark_dirty_for_test(&mut self, cx: &mut Context<Self>) {
        self.mark_dirty(cx);
    }

    pub fn cell_id_for_editor(&self, editor: &Entity<SqlEditor>) -> Option<String> {
        self.cells
            .iter()
            .find(|(_, rt)| rt.editor.as_ref() == Some(editor))
            .map(|(id, _)| id.clone())
    }

    /// Editor and results a run of `id` writes to. A cell whose editor was
    /// released borrows `fallback_editor` (the tab's hidden editor).
    pub fn cell_run_targets(
        &self,
        id: &str,
        fallback_editor: &Entity<SqlEditor>,
    ) -> Option<(Entity<SqlEditor>, Entity<ResultsView>)> {
        let rt = self.cells.get(id)?;
        let results = rt.results.clone()?;
        Some((
            rt.editor.clone().unwrap_or_else(|| fallback_editor.clone()),
            results,
        ))
    }

    pub fn cell_entities(&self, id: &str) -> Option<(Entity<SqlEditor>, Entity<ResultsView>)> {
        let rt = self.cells.get(id)?;
        Some((rt.editor.clone()?, rt.results.clone()?))
    }

    pub fn has_results(&self, id: &str) -> bool {
        self.cells.get(id).is_some_and(|rt| rt.results.is_some())
    }

    #[cfg(test)]
    pub fn live_editor_count(&self) -> usize {
        self.cells.values().filter(|rt| rt.editor.is_some()).count()
    }

    pub fn has_live_editor(&self, id: &str) -> bool {
        self.cells.get(id).is_some_and(|rt| rt.editor.is_some())
    }

    #[cfg(test)]
    pub fn active_cell(&self) -> Option<&str> {
        self.active.as_deref()
    }

    /// The cell keyboard commands act on: the focused editor's cell, else the
    /// active cell while the notebook itself has focus.
    pub fn focused_cell(&self, window: &Window, cx: &App) -> Option<String> {
        for (id, rt) in &self.cells {
            if let Some(editor) = &rt.editor
                && editor
                    .read(cx)
                    .focus_handle(cx)
                    .contains_focused(window, cx)
            {
                return Some(id.clone());
            }
        }
        if self.focus_handle.contains_focused(window, cx) {
            return self.active.clone();
        }
        self.editing_markdown.clone()
    }

    pub fn in_command_mode(&self, window: &Window) -> bool {
        self.focus_handle.is_focused(window)
    }

    /// Attach host-made editors (and results views) to cells.
    pub fn attach_cell_entities(
        &mut self,
        pairs: Vec<(String, Entity<SqlEditor>, Entity<ResultsView>)>,
        cx: &mut Context<Self>,
    ) {
        for (id, editor, results) in pairs {
            self.requested.remove(&id);
            let Some(cell) = self.doc.cell_by_id(&id) else {
                continue;
            };
            let source = cell.source.to_string();
            let language = if cell.kind == KIND_MARKDOWN {
                crate::editor::EditorLanguage::Markdown
            } else {
                crate::editor::EditorLanguage::Sql
            };
            editor.update(cx, |e, cx| {
                e.set_language(language, cx);
                e.set_text(&source, cx);
            });
            let edit_id = id.clone();
            let sub =
                cx.subscribe(
                    &editor,
                    move |this, editor, event: &SqlEditorEvent, cx| match event {
                        SqlEditorEvent::Change => {
                            let text = editor.read(cx).text().to_string();
                            this.source_edited(&edit_id, &text, cx);
                        }
                        SqlEditorEvent::NotebookCompletionAccepted { label } => {
                            this.slash_accepted(&edit_id, label, cx);
                        }
                        _ => {}
                    },
                );
            self.lru_tick += 1;
            let tick = self.lru_tick;
            let rt = self.cells.entry(id.clone()).or_default();
            rt.editor = Some(editor);
            rt.editor_sub = Some(sub);
            rt.last_used = tick;
            if rt.results.is_none() {
                self.attach_results(&id, results, cx);
            }
            self.remeasure_cell(&id);
        }
        self.apply_local_completion(cx);
        self.release_editors(cx);
        cx.notify();
    }

    /// Attach a results view to a cell that has none (a run of a cell whose
    /// editor was never opened).
    pub fn attach_results(
        &mut self,
        id: &str,
        results: Entity<ResultsView>,
        cx: &mut Context<Self>,
    ) {
        let run_id = id.to_string();
        let sub = cx.subscribe(
            &results,
            move |this, _results, event: &ResultsViewEvent, cx| match event {
                ResultsViewEvent::QueryCompleted => this.sql_cell_completed(&run_id, cx),
                ResultsViewEvent::QueryFailed => this.cell_finished(&run_id, false, cx),
                _ => {}
            },
        );
        let rt = self.cells.entry(id.to_string()).or_default();
        rt.results = Some(results.clone());
        rt.results_sub = Some(sub);
        self.restore_cached_output(id, &results, cx);
    }

    // ── Editors ───────────────────────────────────────────────────────────

    fn source_edited(&mut self, id: &str, text: &str, cx: &mut Context<Self>) {
        if self.doc.set_source(id, text) {
            if let Some(rt) = self.cells.get_mut(id) {
                rt.preview = None;
            }
            self.editing_typed(id, text);
            self.mark_typed(cx);
        }
    }

    fn request_editor(&mut self, id: &str, cx: &mut Context<Self>) {
        let uses_editor = self
            .doc
            .cell_by_id(id)
            .is_some_and(|c| cells::renderer_for(c.kind).uses_editor());
        if !uses_editor || self.has_live_editor(id) || !self.requested.insert(id.to_string()) {
            return;
        }
        cx.emit(NotebookEvent::NeedCellEntities {
            cell_ids: vec![id.to_string()],
        });
    }

    fn touch(&mut self, id: &str) {
        self.lru_tick += 1;
        let tick = self.lru_tick;
        if let Some(rt) = self.cells.get_mut(id) {
            rt.last_used = tick;
        }
    }

    /// Release editors beyond the live budget or far from the viewport. The
    /// active, focused-to-be, edited and running cells keep theirs.
    fn release_editors(&mut self, cx: &mut Context<Self>) {
        let top = self.list_state.logical_scroll_top().item_ix;
        let keep: HashSet<&str> = [
            self.active.as_deref(),
            self.pending_focus.as_deref(),
            self.editing_markdown.as_deref(),
        ]
        .into_iter()
        .flatten()
        .collect();
        let mut live: Vec<(String, u64, usize)> = self
            .cells
            .iter()
            .filter(|(id, rt)| {
                rt.editor.is_some()
                    && !keep.contains(id.as_str())
                    && !self.doc.status(id).is_active()
            })
            .map(|(id, rt)| {
                let index = self.doc.index_of(id).unwrap_or(usize::MAX);
                (id.clone(), rt.last_used, index)
            })
            .collect();
        live.sort_by_key(|(_, used, _)| std::cmp::Reverse(*used));
        let budget = MAX_LIVE_EDITORS.saturating_sub(keep.len());
        let mut released = Vec::new();
        for (n, (id, _, index)) in live.into_iter().enumerate() {
            let far = index == usize::MAX || index.abs_diff(top) > RELEASE_DISTANCE;
            if n >= budget || far {
                released.push(id);
            }
        }
        for id in released {
            if let Some(rt) = self.cells.get_mut(&id) {
                rt.editor = None;
                rt.editor_sub = None;
            }
            self.remeasure_cell(&id);
        }
        cx.notify();
    }

    fn ensure_preview(&mut self, id: &str) -> Option<Rc<SqlPreview>> {
        let source = self.doc.cell_by_id(id)?.source;
        let hash = preview::source_hash(source);
        let dialect = self.doc.database_type().family();
        let rt = self.cells.entry(id.to_string()).or_default();
        if rt.preview.as_ref().is_none_or(|p| p.source_hash != hash) {
            rt.preview = Some(Rc::new(preview::build(dialect, source)));
        }
        rt.preview.clone()
    }

    pub(super) fn remeasure_cell(&self, id: &str) {
        if let Some(index) = self.doc.index_of(id) {
            self.list_state.remeasure_items(index..index + 1);
        }
    }

    // ── Selection and modes ───────────────────────────────────────────────

    pub(crate) fn activate_cell(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.active.as_deref() != Some(id) {
            self.active = Some(id.to_string());
            self.name_input = None;
        }
        self.touch(id);
        cx.notify();
    }

    /// Enter edit mode on `id`: its editor is requested (or focused).
    pub(crate) fn edit_cell(&mut self, id: &str, cx: &mut Context<Self>) {
        self.activate_cell(id, cx);
        self.reveal(id, Reveal::Edit, cx);
        let Some(cell) = self.doc.cell_by_id(id) else {
            return;
        };
        if !cells::renderer_for(cell.kind).uses_editor() {
            return;
        }
        if cell.kind == KIND_MARKDOWN {
            self.editing_markdown = Some(id.to_string());
        }
        self.pending_focus = Some(id.to_string());
        self.request_editor(id, cx);
        self.release_editors(cx);
    }

    /// Move keyboard focus to the notebook itself on the next render.
    pub(super) fn request_focus_root(&mut self) {
        self.focus_root_pending = true;
    }

    pub(crate) fn finish_markdown_edit(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.editing_markdown.take() {
            self.remeasure_cell(&id);
        }
        self.focus_root_pending = true;
        cx.notify();
    }

    fn select_relative(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.doc.is_empty() {
            return;
        }
        let len = self.doc.len();
        let current = self
            .active
            .as_deref()
            .and_then(|id| self.doc.index_of(id))
            .unwrap_or(0);
        let next = (current as isize + delta).clamp(0, len as isize - 1) as usize;
        let next = self.skip_folded(current, next, delta);
        if let Some(cell) = self.doc.cell(next) {
            let id = cell.id.to_string();
            self.activate_cell(&id, cx);
            self.reveal(&id, Reveal::Select, cx);
        }
    }

    /// Select cell `id` and scroll it into view.
    pub(crate) fn reveal_cell(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.doc.index_of(id).is_some() {
            self.activate_cell(id, cx);
            self.reveal(id, Reveal::Select, cx);
            self.focus_root_pending = true;
        }
    }

    /// Whether `id` is waiting for its editor to arrive and take focus.
    pub(super) fn awaits_editor(&self, id: &str) -> bool {
        self.pending_focus.as_deref() == Some(id)
    }

    /// The pointer moved onto a cell or a gap; leaving one changes nothing,
    /// so a menu opened from it keeps its button.
    pub(super) fn hover(&mut self, target: Hovered, cx: &mut Context<Self>) {
        if self.hovered.as_ref() != Some(&target) {
            self.hovered = Some(target);
            cx.notify();
        }
    }

    /// Whether a cell draws its full header controls: the active cell
    /// (the one the keyboard is on) and the one under the pointer. Other
    /// cells show their kind, name and state without buttons.
    pub(super) fn shows_controls(&self, id: &str) -> bool {
        self.active.as_deref() == Some(id)
            || matches!(&self.hovered, Some(Hovered::Cell(h)) if h == id)
    }

    pub(crate) fn toggle_selected(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(at) = self.selected.iter().position(|s| s == id) {
            self.selected.remove(at);
        } else {
            self.selected.push(id.to_string());
        }
        cx.notify();
    }

    // ── Structure ─────────────────────────────────────────────────────────

    pub(super) fn mark_dirty(&mut self, cx: &mut Context<Self>) {
        self.editing_observe();
        self.dirty = true;
        self.save_state = SaveState::Dirty;
        cx.emit(NotebookEvent::Dirty);
        self.schedule_autosave(cx);
        self.schedule_staleness(cx);
        cx.notify();
    }

    /// [`Self::mark_dirty`] for text typed in a cell editor: the editor
    /// keeps its own undo, so no history step is compared, and the caches
    /// that walk every cell (folds, find, references) catch up when typing
    /// pauses.
    fn mark_typed(&mut self, cx: &mut Context<Self>) {
        self.dirty = true;
        self.save_state = SaveState::Dirty;
        cx.emit(NotebookEvent::Dirty);
        self.schedule_autosave(cx);
        self.schedule_staleness(cx);
        cx.notify();
    }

    /// Re-check which results are out of date once edits pause, so typing
    /// in a large notebook does not re-fingerprint every cell per keystroke.
    fn schedule_staleness(&mut self, cx: &mut Context<Self>) {
        self.staleness_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(STALENESS_DEBOUNCE).await;
            this.update(cx, |this, cx| {
                this.staleness_task = None;
                if this.editing_settle() {
                    cx.notify();
                }
                if !this.doc.refresh_staleness().is_empty() {
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    /// Insert a `kind` cell at `index`; returns its id.
    pub(crate) fn insert_cell(
        &mut self,
        kind: &str,
        index: usize,
        cx: &mut Context<Self>,
    ) -> String {
        let index = index.min(self.doc.len());
        let id = self.doc.insert(kind, index);
        self.list_state.splice(index..index, 1);
        self.editing.toast = None;
        self.activate_cell(&id, cx);
        self.editing_label("notebook.editing.step.insert");
        self.mark_dirty(cx);
        id
    }

    fn insert_relative(&mut self, below: bool, cx: &mut Context<Self>) -> String {
        let index = match self.active.as_deref().and_then(|id| self.doc.index_of(id)) {
            Some(at) if below => at + 1,
            Some(at) => at,
            None => self.doc.len(),
        };
        let id = self.insert_cell(KIND_SQL, index, cx);
        self.reveal(&id, Reveal::Select, cx);
        id
    }

    pub(crate) fn delete_cell(&mut self, id: &str, cx: &mut Context<Self>) {
        self.delete_cells(vec![id.to_string()], cx);
    }

    pub(crate) fn undo_delete(&mut self, cx: &mut Context<Self>) {
        self.undo_structure(cx);
    }

    pub(crate) fn dismiss_undo(&mut self, cx: &mut Context<Self>) {
        self.editing.toast = None;
        cx.notify();
    }

    pub(crate) fn move_cell(&mut self, id: &str, delta: isize, cx: &mut Context<Self>) {
        let Some(from) = self.doc.index_of(id) else {
            return;
        };
        let to = from as isize + delta;
        if to < 0 || to as usize >= self.doc.len() {
            return;
        }
        let to = to as usize;
        if self.doc.move_cell(from, to) {
            let (lo, hi) = (from.min(to), from.max(to));
            self.list_state.splice(lo..hi + 1, hi - lo + 1);
            self.editing_label("notebook.editing.step.move");
            self.mark_dirty(cx);
        }
    }

    pub(crate) fn convert_cell(&mut self, id: &str, kind: &str, cx: &mut Context<Self>) {
        if self.doc.status(id).is_active() {
            return;
        }
        if self.doc.set_kind(id, kind) {
            if let Some(rt) = self.cells.get_mut(id) {
                rt.editor = None;
                rt.editor_sub = None;
                rt.preview = None;
            }
            if kind == KIND_SQL && self.editing_markdown.as_deref() == Some(id) {
                self.editing_markdown = None;
            }
            self.remeasure_cell(id);
            self.editing_label("notebook.editing.step.convert");
            self.mark_dirty(cx);
        }
    }

    pub(crate) fn toggle_collapsed(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.doc.toggle_collapsed(id) {
            self.remeasure_cell(id);
            self.editing_label("notebook.editing.step.collapse");
            self.mark_dirty(cx);
        }
    }

    /// Give every live editor its cell's current text after a change made
    /// to many cells at once (a parameter rename), so the next keystroke
    /// does not write the old text back.
    pub(super) fn sync_editors_to_doc(&mut self, cx: &mut Context<Self>) {
        let live: Vec<(String, Entity<SqlEditor>)> = self
            .cells
            .iter()
            .filter_map(|(id, rt)| rt.editor.clone().map(|e| (id.clone(), e)))
            .collect();
        for (id, editor) in live {
            let Some(source) = self.doc.cell_by_id(&id).map(|c| c.source.to_string()) else {
                continue;
            };
            if editor.read(cx).text() != source {
                editor.update(cx, |e, cx| e.replace_document(source, cx));
            }
        }
        for rt in self.cells.values_mut() {
            rt.preview = None;
        }
    }

    pub(crate) fn rename_cell(&mut self, id: &str, name: &str, cx: &mut Context<Self>) {
        match self.doc.set_name(id, name) {
            Ok(true) => {
                for rt in self.cells.values_mut() {
                    rt.preview = None;
                }
                let ids: Vec<(String, String)> = self
                    .cells
                    .iter()
                    .filter_map(|(cid, rt)| rt.editor.as_ref().map(|_| cid.clone()))
                    .filter_map(|cid| {
                        self.doc
                            .cell_by_id(&cid)
                            .map(|c| (cid.clone(), c.source.to_string()))
                    })
                    .collect();
                for (cid, source) in ids {
                    if cid == id {
                        continue;
                    }
                    if let Some(editor) = self.cells.get(&cid).and_then(|rt| rt.editor.clone())
                        && editor.read(cx).text() != source
                    {
                        editor.update(cx, |e, cx| e.set_text(&source, cx));
                    }
                }
                self.editing_label("notebook.editing.step.rename");
                self.mark_dirty(cx);
            }
            Ok(false) => {}
            Err(message) => self.status(message, true, cx),
        }
    }

    pub(crate) fn set_title(&mut self, title: &str, cx: &mut Context<Self>) {
        if self.doc.set_title(title) {
            cx.emit(NotebookEvent::TitleChanged(title.trim().to_string()));
            self.mark_dirty(cx);
        }
    }

    // ── Connections, safety, trust ────────────────────────────────────────

    pub(crate) fn set_cell_connection(
        &mut self,
        id: &str,
        choice: Option<ConnectionChoice>,
        cx: &mut Context<Self>,
    ) {
        if self.doc.set_cell_connection(id, choice) {
            self.refresh_choices(cx);
            self.mark_dirty(cx);
        }
    }

    pub(crate) fn set_notebook_connection(
        &mut self,
        choice: Option<ConnectionChoice>,
        cx: &mut Context<Self>,
    ) {
        let follow_tab = choice.is_none();
        if self.doc.set_notebook_connection(choice) {
            self.refresh_choices(cx);
            self.mark_dirty(cx);
        } else if follow_tab && !self.doc.can_follow_tab() {
            self.status(
                crate::localization::text("notebook.fu_core.connection.pinned_cells_keep_default"),
                false,
                cx,
            );
        }
    }

    /// The choice a cell runs on, and whether it is the cell's own pick.
    pub(crate) fn cell_connection(&self, id: &str) -> (Option<ConnectionChoice>, bool) {
        let own = self.doc.cell_by_id(id).and_then(|c| c.connection).is_some();
        let choice = match self.doc.effective_connection(id) {
            Some(r) => Some(self.choice_for_ref(r)),
            None => self.tab_connection.clone(),
        };
        (choice, own)
    }

    /// A connection slot as a choice: the local profile it maps to, or a
    /// stand-in carrying the slot's own label when it maps to nothing here.
    pub(crate) fn choice_for_ref(&self, r: &ConnectionRef) -> ConnectionChoice {
        let mapped = r
            .profile_id
            .as_deref()
            .and_then(|id| self.choices.iter().find(|c| c.profile_id == id));
        match mapped {
            Some(choice) => ConnectionChoice {
                database: r.database.clone().or(choice.database.clone()),
                ..choice.clone()
            },
            None => ConnectionChoice {
                profile_id: r.profile_id.clone().unwrap_or_default(),
                label: r.label.clone(),
                environment: sqlly_core::ConnectionEnvironment::Unset,
                database_type: self.doc.database_type(),
                database: r.database.clone(),
                ..ConnectionChoice::default()
            },
        }
    }

    pub(crate) fn is_resolved(&self, profile_id: &str) -> bool {
        self.choices.iter().any(|c| c.profile_id == profile_id)
    }

    /// Ask before letting `id` write: the prompt names the target connection.
    pub(crate) fn request_allow_writes(&mut self, id: &str, cx: &mut Context<Self>) {
        self.elevation_prompt = Some(id.to_string());
        cx.notify();
    }

    pub(crate) fn confirm_allow_writes(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.elevation_prompt.take()
            && self.doc.set_read_only(&id, false)
        {
            self.log(
                Some(&id),
                crate::localization::text("notebook.shell.activity.writes_allowed"),
                false,
                None,
            );
            self.mark_dirty(cx);
        }
        cx.notify();
    }

    pub(crate) fn cancel_allow_writes(&mut self, cx: &mut Context<Self>) {
        self.elevation_prompt = None;
        cx.notify();
    }

    pub(crate) fn make_read_only(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.doc.set_read_only(id, true) {
            self.mark_dirty(cx);
        }
    }

    pub(crate) fn remap_connection(
        &mut self,
        from: &str,
        to: ConnectionChoice,
        cx: &mut Context<Self>,
    ) {
        let changed = self.doc.remap_connection(from, &to);
        self.unresolved.retain(|r| r.id != from);
        if changed > 0 {
            self.mark_dirty(cx);
        }
        cx.notify();
    }

    pub(crate) fn dismiss_remap(&mut self, cx: &mut Context<Self>) {
        self.unresolved.clear();
        cx.notify();
    }

    // ── Running ───────────────────────────────────────────────────────────

    /// The cell's state from the core reducer.
    pub fn cell_state(&self, id: &str) -> CellStatus {
        self.doc.status(id)
    }

    /// A result exists but no longer matches the cell's inputs (the core's
    /// `is_presentable_as_current` says no).
    pub(crate) fn is_stale(&self, id: &str) -> bool {
        let state = self.doc.cell_state(id);
        state.output.is_some() && !state.is_presentable_as_current() && !state.status.is_active()
    }

    pub fn is_running(&self) -> bool {
        self.run.is_some()
    }

    /// Whether `id` is the cell this view is handing to the host right now.
    pub fn is_dispatching(&self, id: &str) -> bool {
        self.run
            .as_ref()
            .is_some_and(|run| run.dispatching.contains(id))
            && self.cell_state(id) == CellStatus::Queued
    }

    /// A run of `id` asked for from outside the queue (editor Cmd+Enter, the
    /// Run Notebook Cell command): planned with the current run mode.
    pub fn request_run_from_host(&mut self, id: &str, cx: &mut Context<Self>) {
        self.activate_cell(id, cx);
        self.run_scope(self.run_mode.scope(), Some(id.to_string()), cx);
    }

    pub fn run_active(&mut self, cx: &mut Context<Self>) {
        let anchor = self.active.clone();
        self.run_scope(self.run_mode.scope(), anchor, cx);
    }

    pub fn run_all(&mut self, cx: &mut Context<Self>) {
        self.run_scope(RunScope::All, None, cx);
    }

    pub fn run_scope(&mut self, scope: RunScope, anchor: Option<String>, cx: &mut Context<Self>) {
        if self.trust.level == sqlly_services::notebook_security::TrustLevel::Untrusted {
            self.refuse_untrusted_run(anchor.as_deref());
            self.status(
                crate::localization::text("notebook.shell.trust.run_blocked"),
                true,
                cx,
            );
            return;
        }
        if scope.needs_anchor() && anchor.is_none() {
            self.status(
                crate::localization::text("notebook.shell.run.no_active_cell"),
                false,
                cx,
            );
            return;
        }
        let selected = if scope == RunScope::Selected && self.selected.is_empty() {
            anchor.iter().cloned().collect()
        } else {
            self.selected.clone()
        };
        let plan = self.doc.plan(scope, anchor.as_deref(), &selected);
        for (id, reason) in &plan.blocked {
            if let Some(rt) = self.cells.get_mut(id) {
                rt.message = Some(reason.clone());
            }
            self.remeasure_cell(id);
            self.log(Some(id), crate::localization::message(reason), true, None);
        }
        if scope == RunScope::ValidateOnly {
            self.validate(&plan.validate, plan.blocked.len(), cx);
            return;
        }
        for id in &plan.reused {
            self.log(
                Some(id),
                crate::localization::text("notebook.shell.activity.reused"),
                false,
                None,
            );
        }
        if plan.execute.is_empty() {
            let message = match plan.blocked.first() {
                Some((_, reason)) => crate::localization::message(reason),
                None => crate::localization::text("notebook.shell.run.nothing_to_run"),
            };
            self.status(message, !plan.blocked.is_empty(), cx);
            return;
        }
        // A run whose only running cells wait on a decision that will not
        // come (a dismissed review, a block) settles them as not run, so
        // the new request goes ahead and their dependents are blocked
        // rather than left queued.
        let stuck: Vec<String> = self
            .run
            .as_ref()
            .map(|run| {
                run.plan
                    .steps
                    .iter()
                    .zip(&run.status)
                    .filter(|(_, st)| **st == StepStatus::Running)
                    .map(|(s, _)| s.cell_id.clone())
                    .collect::<Vec<_>>()
            })
            .filter(|running| {
                !running.is_empty()
                    && running.iter().all(|id| {
                        matches!(
                            self.doc.status(id),
                            CellStatus::AwaitingApproval | CellStatus::Blocked | CellStatus::Failed
                        )
                    })
            })
            .unwrap_or_default();
        self.settle_stuck(&stuck);
        let queue: Vec<String> = match &self.run {
            Some(run) => plan
                .execute
                .iter()
                .filter(|id| !run.is_running(id))
                .cloned()
                .collect(),
            None => plan.execute.clone(),
        };
        for id in &queue {
            self.doc.apply(id, CellEvent::Queued);
            self.remeasure_cell(id);
        }
        for id in &queue {
            if self.mark_unavailable_if_blocked(id)
                && let Some(reason) = self.cells.get(id).and_then(|rt| rt.message_text())
            {
                self.log(Some(id), reason, true, None);
            }
        }
        match &mut self.run {
            Some(run) => run.merge(plan.plan),
            None => {
                self.run = Some(RunProgress::new(plan.plan));
                self.begin_run_record(scope);
            }
        }
        self.advance(cx);
    }

    /// Cells leaving the document (cut, merged away, undone, replaced) no
    /// longer belong to the run: their steps end as not run, their
    /// dependents are skipped, and a run left with nothing else ends.
    pub(super) fn forget_run_steps(&mut self, ids: &[String], cx: &mut Context<Self>) {
        let Some(run) = self.run.as_mut() else {
            return;
        };
        if !ids.iter().any(|id| run.step_of(id).is_some()) {
            return;
        }
        for id in ids {
            run.dispatching.remove(id);
            run.rerun.remove(id);
            run.held.retain(|h| h != id);
            if let Some(step) = run.step_of(id)
                && matches!(run.status[step], StepStatus::Pending | StepStatus::Running)
            {
                run.status[step] = StepStatus::Failed;
            }
        }
        self.advance(cx);
    }

    /// End `ids`' steps as not run without ending the run.
    fn settle_stuck(&mut self, ids: &[String]) {
        let limits = self.limits();
        for id in ids {
            if self.doc.status(id) == CellStatus::AwaitingApproval {
                self.doc.apply(
                    id,
                    CellEvent::Canceled {
                        server_confirmed: true,
                    },
                );
                self.remeasure_cell(id);
            }
            let Some(run) = self.run.as_mut() else {
                return;
            };
            run.dispatching.remove(id);
            run.rerun.remove(id);
            if let Some(step) = run.step_of(id) {
                let mut schedule =
                    Schedule::resume(&run.plan, limits, std::mem::take(&mut run.status));
                schedule.finish(step, false);
                run.status = schedule.statuses().to_vec();
            }
        }
    }

    fn validate(&mut self, cells: &[String], blocked: usize, cx: &mut Context<Self>) {
        let mut problems = blocked;
        for id in cells {
            self.doc.apply(id, CellEvent::ValidationStarted);
            let error = self.run_request(id).err();
            if let Some(message) = &error {
                problems += 1;
                if let Some(rt) = self.cells.get_mut(id) {
                    rt.message = Some(DisplayMessage::raw(message.clone()));
                }
                self.log(Some(id), message.clone(), true, None);
            }
            self.doc.apply(
                id,
                CellEvent::Validated {
                    error: error.map(DisplayMessage::raw),
                },
            );
        }
        let message = if problems == 0 {
            crate::localization::format(
                if cells.len() == 1 {
                    "notebook.shell.run.validated_ok_one"
                } else {
                    "notebook.shell.run.validated_ok"
                },
                &[("count", cells.len())],
            )
        } else {
            crate::localization::format(
                if problems == 1 {
                    "notebook.shell.run.validated_problems_one"
                } else {
                    "notebook.shell.run.validated_problems"
                },
                &[("count", problems)],
            )
        };
        self.log(None, message.clone(), problems > 0, None);
        self.layout.activity_visible = true;
        self.status(message, problems > 0, cx);
    }

    pub(super) fn limits(&self) -> Limits {
        Limits {
            per_connection: self.local.prefs.per_connection,
            local: Limits::default().local,
        }
    }

    /// Start every step the schedule allows now and hand the cells to the
    /// host; end the run once nothing is pending or running.
    pub(super) fn advance(&mut self, cx: &mut Context<Self>) {
        let limits = self.limits();
        let mut start: Vec<String> = Vec::new();
        loop {
            let Some(run) = self.run.as_mut() else {
                return;
            };
            let mut schedule = Schedule::resume(&run.plan, limits, std::mem::take(&mut run.status));
            let unreachable: Vec<(String, String)> = schedule
                .skip_unreachable()
                .into_iter()
                .filter_map(|step| {
                    let s = &run.plan.steps[step];
                    let by = s.depends_on.iter().find(|d| {
                        matches!(
                            schedule.status(**d),
                            StepStatus::Failed | StepStatus::Skipped
                        )
                    })?;
                    Some((s.cell_id.clone(), run.plan.steps[*by].cell_id.clone()))
                })
                .collect();
            let ready = schedule.start_ready();
            let mut settled = false;
            for step in ready {
                let id = run.plan.steps[step].cell_id.clone();
                let executes = run.plan.steps[step].action == StepAction::Execute;
                if !executes {
                    schedule.finish(step, true);
                    settled = true;
                } else if self.doc.status(&id) != CellStatus::Queued {
                    // The core blocked it (a failed upstream) or it was
                    // canceled; its dependents are skipped with it.
                    schedule.finish(step, false);
                    settled = true;
                } else {
                    start.push(id);
                }
            }
            let finished = schedule.is_finished();
            run.status = schedule.statuses().to_vec();
            for (id, by) in unreachable {
                if self.doc.status(&id) == CellStatus::Queued {
                    self.doc.apply(&id, CellEvent::Blocked { by });
                    self.remeasure_cell(&id);
                }
            }
            let Some(run) = self.run.as_mut() else {
                return;
            };
            if finished && run.dispatching.is_empty() && run.held.is_empty() {
                let ran = run.ran;
                self.run = None;
                self.finish_run_record(cx);
                let message = crate::localization::format(
                    if ran == 1 {
                        "notebook.shell.run.all_completed_one"
                    } else {
                        "notebook.shell.run.all_completed"
                    },
                    &[("count", ran)],
                );
                self.log(None, message.clone(), false, None);
                self.status(message, false, cx);
                return;
            }
            if !settled {
                break;
            }
        }
        for id in start {
            self.start_cell(&id, cx);
        }
        self.release_held(cx);
        cx.notify();
    }

    /// The schedule started `id`: hold it while a review is open, prepare
    /// local data for cells that read it, then hand it to the host.
    fn start_cell(&mut self, id: &str, cx: &mut Context<Self>) {
        let fingerprint = self.doc.cell_fingerprint(id);
        let rt = self.cells.entry(id.to_string()).or_default();
        rt.run_fingerprint = fingerprint;
        rt.run_id = Some(new_run_id(&mut RandomIds));
        rt.started = None;
        rt.message = None;
        rt.stopped = false;
        self.local.truncated.remove(id);
        self.remeasure_cell(id);
        if self.awaiting_review() {
            if let Some(run) = self.run.as_mut() {
                run.held.push_back(id.to_string());
            }
            return;
        }
        self.launch_cell(id, cx);
    }

    /// Check a started cell's authorization and hand it to whoever runs it:
    /// the analysis lane, local data preparation, or the host.
    fn launch_cell(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Err(message) = self.check_run(id) {
            if let Some(run) = self.run.as_mut() {
                run.dispatching.insert(id.to_string());
            }
            self.dispatch_result(id, DispatchOutcome::NotStarted(message), cx);
            return;
        }
        if self.start_analysis_cell(id, cx) {
            cx.notify();
            return;
        }
        // Cells computed in the notebook from upstream outputs (charts,
        // tables, transforms) settle here, never reaching the host.
        if self.runs_locally(id) {
            self.run_local_cell(id, cx);
            return;
        }
        if self.needs_local_preparation(id) {
            self.prepare_local_run(id, cx);
        } else {
            self.dispatch(id, cx);
        }
    }

    /// Hand a started cell to the host.
    pub(super) fn dispatch(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(run) = self.run.as_mut() else {
            return;
        };
        run.dispatching.insert(id.to_string());
        cx.emit(NotebookEvent::RunCell {
            cell_id: id.to_string(),
        });
        cx.notify();
    }

    /// Whether a cell of this notebook waits for its change review, or is a
    /// write handed to the host that may open one: cells started meanwhile
    /// wait, since the review blocks every other run in the tab.
    fn awaiting_review(&self) -> bool {
        self.run.as_ref().is_some_and(|run| {
            run.plan.steps.iter().any(|s| {
                self.doc.status(&s.cell_id) == CellStatus::AwaitingApproval
                    || (s.requires_approval && run.dispatching.contains(&s.cell_id))
            })
        })
    }

    fn release_held(&mut self, cx: &mut Context<Self>) {
        while !self.awaiting_review() {
            let Some(id) = self.run.as_mut().and_then(|run| run.held.pop_front()) else {
                return;
            };
            if self.doc.status(&id) != CellStatus::Queued {
                self.finish_step(&id, false, cx);
                continue;
            }
            self.launch_cell(&id, cx);
        }
    }

    /// Record a started step's outcome with the scheduler and start what it
    /// unblocked.
    pub(super) fn finish_step(&mut self, id: &str, ok: bool, cx: &mut Context<Self>) {
        let limits = self.limits();
        let Some(run) = self.run.as_mut() else {
            return;
        };
        run.dispatching.remove(id);
        let mut again = false;
        if let Some(step) = run.step_of(id) {
            let mut schedule = Schedule::resume(&run.plan, limits, std::mem::take(&mut run.status));
            again = run.rerun.remove(id);
            if again {
                schedule.requeue(step);
            } else {
                schedule.finish(step, ok);
            }
            run.status = schedule.statuses().to_vec();
            if ok {
                run.ran += 1;
            }
        }
        if again {
            self.doc.apply(id, CellEvent::Queued);
            self.remeasure_cell(id);
        }
        self.advance(cx);
    }

    /// Fail a started cell before the host ran it (local preparation).
    pub(super) fn fail_started(
        &mut self,
        id: &str,
        message: impl Into<DisplayMessage>,
        cx: &mut Context<Self>,
    ) {
        let message = message.into();
        self.doc.apply(
            id,
            CellEvent::Failed {
                error: message.clone(),
            },
        );
        let text = crate::localization::message(&message);
        if let Some(rt) = self.cells.get_mut(id) {
            rt.message = Some(message);
            rt.started = None;
        }
        self.remeasure_cell(id);
        self.log(Some(id), text, true, None);
        self.finish_step(id, false, cx);
    }

    /// What the host runs for `id`: the SQL the core builds and the
    /// connection, read-only mode and timeout it runs under.
    pub fn run_request(&self, id: &str) -> Result<RunRequest, String> {
        let cell = self
            .doc
            .cell_by_id(id)
            .ok_or_else(|| crate::localization::text("notebook.shell.run.cell_missing"))?;
        if cell.kind == super::doc_adapter::KIND_ATTACHMENT {
            let relation = self
                .doc
                .attachment(id)
                .and_then(|a| a.relation)
                .ok_or_else(|| {
                    crate::localization::text("notebook.local.attachment.no_relation")
                })?;
            return Ok(RunRequest {
                sql: format!("SELECT * FROM \"{relation}\" LIMIT {ATTACHMENT_PREVIEW_ROWS}"),
                target: CellRunTarget {
                    profile_id: None,
                    read_only: true,
                    timeout_seconds: None,
                    local: true,
                },
                history_sql: None,
            });
        }
        let read_only = cell.read_only();
        let timeout_seconds = cell.policy.timeout_seconds;
        let local = self.doc.targets_local(id);
        if read_only && self.doc.writes(id) {
            return Err(crate::localization::text(
                "notebook.shell.safety.read_only_blocks_writes",
            ));
        }
        let profile_id = match self.doc.effective_connection(id) {
            _ if local => None,
            Some(slot) => {
                let unresolved = || {
                    crate::localization::format(
                        "notebook.shell.remap.unresolved_run",
                        &[("connection_name", slot.label.as_str())],
                    )
                };
                let profile = slot.profile_id.as_deref().ok_or_else(unresolved)?;
                if !self.is_resolved(profile) {
                    return Err(unresolved());
                }
                Some(uuid::Uuid::parse_str(profile).map_err(|_| unresolved())?)
            }
            None => None,
        };
        if let Some(message) = self.run_refusal(&self.run_decision(id)) {
            return Err(crate::localization::message(message));
        }
        let sql = self.doc.sql_for_run(id)?;
        Ok(RunRequest {
            history_sql: self.doc.history_sql_for_run(id),
            sql,
            target: CellRunTarget {
                profile_id,
                read_only: read_only || local,
                timeout_seconds,
                local,
            },
        })
    }

    pub(super) fn mark_started(&mut self, id: &str) {
        if matches!(
            self.doc.status(id),
            CellStatus::Running | CellStatus::Streaming
        ) {
            return;
        }
        let rt = self.cells.entry(id.to_string()).or_default();
        let run_id = rt
            .run_id
            .get_or_insert_with(|| new_run_id(&mut RandomIds))
            .clone();
        if rt.run_fingerprint.is_none() {
            rt.run_fingerprint = self.doc.cell_fingerprint(id);
        }
        rt.started.get_or_insert_with(Instant::now);
        self.doc.apply(id, CellEvent::Started { run_id });
        self.log(
            Some(id),
            crate::localization::text("notebook.shell.activity.started"),
            false,
            None,
        );
    }

    /// The host's attempt to start the cell it was handed.
    pub fn dispatch_result(&mut self, id: &str, outcome: DispatchOutcome, cx: &mut Context<Self>) {
        match outcome {
            DispatchOutcome::Started => {
                if let Some(run) = self.run.as_mut() {
                    run.dispatching.remove(id);
                }
                self.mark_started(id);
                self.release_held(cx);
            }
            DispatchOutcome::AwaitingApproval => {
                self.doc.apply(id, CellEvent::AwaitingApproval);
                self.log(
                    Some(id),
                    crate::localization::text("notebook.shell.activity.awaiting_approval"),
                    false,
                    None,
                );
            }
            DispatchOutcome::NotStarted(reason) => {
                let message = crate::localization::message(&reason);
                self.finish_audit(
                    id,
                    sqlly_services::notebook_security::audit::AuditOutcome::Failed,
                    None,
                    Some(&message),
                );
                self.doc.apply(
                    id,
                    CellEvent::Failed {
                        error: reason.clone(),
                    },
                );
                if let Some(rt) = self.cells.get_mut(id) {
                    rt.message = Some(reason);
                    rt.started = None;
                }
                self.remeasure_cell(id);
                self.log(Some(id), message, true, None);
                if self
                    .run
                    .as_ref()
                    .is_some_and(|r| r.dispatching.contains(id))
                {
                    if let Some(run) = self.run.as_mut() {
                        run.rerun.remove(id);
                    }
                    self.finish_step(id, false, cx);
                }
            }
        }
        cx.notify();
    }

    pub(super) fn cell_finished(&mut self, id: &str, ok: bool, cx: &mut Context<Self>) {
        if self.drop_stopped_report(id) {
            cx.notify();
            return;
        }
        // A confirmed change review re-runs outside the dispatch path.
        self.mark_started(id);
        let now = chrono::Local::now().format("%H:%M:%S").to_string();
        let published = self.outputs.get(id);
        let row_count = published.as_ref().map(|o| o.row_count).unwrap_or_else(|| {
            self.cells
                .get(id)
                .and_then(|rt| rt.results.clone())
                .and_then(|r| r.read(cx).result_set_at(0).map(|rs| rs.rows.len() as u64))
                .unwrap_or(0)
        });
        let completeness = published
            .as_ref()
            .map(|o| o.completeness)
            .unwrap_or(sqlly_services::notebook::Completeness::Complete);
        let mut elapsed_ms = None;
        let mut started_with = None;
        if let Some(rt) = self.cells.get_mut(id) {
            elapsed_ms = rt.started.take().map(|s| s.elapsed().as_millis() as u64);
            rt.elapsed_ms = elapsed_ms.or(rt.elapsed_ms);
            rt.finished_at = Some(now);
            started_with = rt.run_fingerprint.take().zip(rt.run_id.take());
            if ok {
                rt.message = None;
            }
        }
        self.record_cell_run(
            id,
            ok,
            started_with.as_ref().map(|(f, _)| f),
            elapsed_ms,
            cx,
        );
        self.finish_audit(
            id,
            if ok {
                sqlly_services::notebook_security::audit::AuditOutcome::Completed
            } else {
                sqlly_services::notebook_security::audit::AuditOutcome::Failed
            },
            Some((row_count, elapsed_ms)),
            None,
        );
        let truncated = self.local.truncated.get(id).map(|t| t.rows_kept);
        if ok {
            if let Some((fingerprint, run_id)) = started_with {
                let completeness = match truncated {
                    Some(cap) => sqlly_services::notebook::Completeness::Truncated { cap },
                    None => completeness,
                };
                self.doc
                    .complete_with(id, &run_id, fingerprint.clone(), row_count, completeness);
                self.after_cell_completed(id, &run_id, fingerprint, cx);
            }
        } else {
            self.doc.apply(
                id,
                CellEvent::Failed {
                    error: DisplayMessage::new(
                        "notebook.shell.activity.failed",
                        crate::localization::text("notebook.shell.activity.failed"),
                    ),
                },
            );
        }
        self.remeasure_cell(id);
        let key = if ok {
            "notebook.shell.activity.completed"
        } else {
            "notebook.shell.activity.failed"
        };
        self.log(Some(id), crate::localization::text(key), !ok, elapsed_ms);
        if self.run.as_ref().is_some_and(|r| r.is_running(id)) {
            self.finish_step(id, ok, cx);
        }
        cx.notify();
    }

    /// Whether `id`'s report belongs to a run Stop canceled; it is dropped
    /// (once) and the cell stays canceled.
    pub(super) fn drop_stopped_report(&mut self, id: &str) -> bool {
        match self.cells.get_mut(id) {
            Some(rt) if rt.stopped => {
                rt.stopped = false;
                rt.started = None;
                true
            }
            _ => false,
        }
    }

    /// Stop: drop the queue and cancel the cell that is running.
    pub fn stop(&mut self, cx: &mut Context<Self>) {
        let Some(run) = self.run.take() else {
            return;
        };
        let running = run.status.contains(&StepStatus::Running);
        for step in run.plan.steps.iter().zip(&run.status) {
            if *step.1 == StepStatus::Running {
                self.cancel_analysis(Some(step.0.cell_id.as_str()));
            }
        }
        self.finish_run_record(cx);
        for id in run.plan.steps.iter().map(|s| &s.cell_id) {
            let status = self.doc.status(id);
            let in_flight = matches!(status, CellStatus::Running | CellStatus::Streaming)
                || run.dispatching.contains(id);
            if status.is_active() {
                self.doc.apply(
                    id,
                    CellEvent::Canceled {
                        server_confirmed: false,
                    },
                );
            }
            if let Some(rt) = self.cells.get_mut(id) {
                rt.started = None;
                if in_flight {
                    rt.stopped = true;
                    rt.run_id = None;
                    rt.run_fingerprint = None;
                }
            }
            self.finish_audit(
                id,
                sqlly_services::notebook_security::audit::AuditOutcome::Canceled,
                None,
                None,
            );
            self.remeasure_cell(id);
        }
        self.cancel_local_jobs();
        if running {
            cx.emit(NotebookEvent::StopRequested);
        }
        let message = crate::localization::text("notebook.shell.activity.stopped");
        self.log(None, message.clone(), false, None);
        self.status(message, false, cx);
    }

    /// Cells of the current run not finished yet: running ones first, then
    /// those still waiting, in plan order.
    pub(super) fn queued(&self) -> Vec<String> {
        let Some(run) = &self.run else {
            return Vec::new();
        };
        let with = |want: StepStatus| {
            run.plan
                .steps
                .iter()
                .zip(&run.status)
                .filter(move |(_, st)| **st == want)
                .map(|(s, _)| s.cell_id.clone())
        };
        with(StepStatus::Running)
            .chain(with(StepStatus::Pending))
            .collect()
    }

    /// Cells of the current run the host is running right now.
    pub(super) fn running_cells(&self) -> Vec<String> {
        self.queued()
            .into_iter()
            .filter(|id| {
                matches!(
                    self.doc.status(id),
                    CellStatus::Running | CellStatus::Streaming
                )
            })
            .collect()
    }

    pub(super) fn log(
        &mut self,
        cell: Option<&str>,
        message: String,
        error: bool,
        elapsed_ms: Option<u64>,
    ) {
        let cell = cell.map(|id| {
            self.doc
                .cell_by_id(id)
                .map(|c| c.display_name().to_string())
                .unwrap_or_else(|| id.to_string())
        });
        self.activity.push_back(ActivityEntry {
            at: chrono::Local::now().format("%H:%M:%S").to_string(),
            cell,
            message,
            error,
            elapsed_ms,
        });
        while self.activity.len() > MAX_ACTIVITY {
            self.activity.pop_front();
        }
    }

    pub(super) fn status(&mut self, message: String, error: bool, cx: &mut Context<Self>) {
        cx.emit(NotebookEvent::Status { message, error });
        cx.notify();
    }

    // ── Autosave and conflicts ────────────────────────────────────────────

    pub(crate) fn set_autosave_files(&mut self, on: bool, cx: &mut Context<Self>) {
        self.prefs.autosave_files = on;
        self.prefs.save();
        if on && self.dirty {
            self.schedule_autosave(cx);
        }
        cx.notify();
    }

    fn schedule_autosave(&mut self, cx: &mut Context<Self>) {
        let wanted = match self.file_path {
            None => self.prefs.recovery_autosave,
            Some(_) => self.prefs.autosave_files && !self.conflict,
        };
        if !wanted {
            return;
        }
        self.autosave_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(autosave::AUTOSAVE_DEBOUNCE)
                .await;
            this.update(cx, |this, cx| this.autosave_now(cx)).ok();
        }));
    }

    /// Write the recovery copy (unsaved notebooks) or the file (when file
    /// autosave is on and the file has not changed under us).
    pub fn autosave_now(&mut self, cx: &mut Context<Self>) {
        self.autosave_task = None;
        match self.file_path.clone() {
            None => {
                if !self.prefs.recovery_autosave {
                    return;
                }
                let model = self.model_for_save();
                if autosave::write_recovery(&model).is_ok() {
                    self.record_trusted_save(None);
                    if self.dirty {
                        self.save_state = SaveState::Autosaved;
                    }
                    cx.notify();
                }
            }
            Some(path) => {
                // Older SQLLY builds cannot read the current format, so a
                // legacy `.sqlnb.json` file is only ever replaced by an
                // explicit Save (which offers a `.sqllynb`).
                if !self.prefs.autosave_files || !self.dirty || super::files::is_legacy_path(&path)
                {
                    return;
                }
                let file = std::path::Path::new(&path);
                if autosave::changed_on_disk(file, self.file_stamp) {
                    self.conflict = true;
                    cx.notify();
                    return;
                }
                self.take_own_identity(cx);
                let text = super::files::file_text(&path, &self.model_for_save());
                if super::files::write_text(&path, &text).is_ok() {
                    super::browser::write_through(&path, &text);
                    self.file_stamp = autosave::file_stamp(file);
                    self.record_trusted_save(Some(&path));
                    self.dirty = false;
                    self.save_state = SaveState::Autosaved;
                    cx.emit(NotebookEvent::SavedToFile);
                    cx.notify();
                }
            }
        }
    }

    /// The app is quitting: write what the debounce has not yet, so no
    /// edit is lost. An unsaved notebook gets its recovery copy; a saved one
    /// with changes is written to its file when file autosave may, else its
    /// changes go to a recovery copy beside the file, offered on restore.
    pub fn flush_for_quit(&mut self, cx: &mut Context<Self>) {
        self.autosave_task = None;
        if self.file_path.is_none() {
            self.autosave_now(cx);
            return;
        }
        if self.dirty {
            self.autosave_now(cx);
        }
        if self.dirty {
            let _ = autosave::write_file_recovery(&self.model_for_save());
        } else if self.recovery_offer.is_none() {
            autosave::remove_file_recovery(self.doc.id());
        }
    }

    /// Offer `doc`, this file's unsaved changes from the last session.
    pub fn offer_recovery(&mut self, doc: NotebookDoc, cx: &mut Context<Self>) {
        if doc.notebook_id == self.doc.id() && &doc != self.doc.model() {
            self.recovery_offer = Some(doc);
            cx.notify();
        } else {
            autosave::remove_file_recovery(self.doc.id());
        }
    }

    /// Put the offered changes in place, as one undoable step; the notebook
    /// is unsaved until the user saves it.
    pub(crate) fn restore_offered(&mut self, cx: &mut Context<Self>) {
        let Some(doc) = self.recovery_offer.take() else {
            return;
        };
        if self.is_running() {
            self.recovery_offer = Some(doc);
            self.status(
                crate::localization::text("notebook.fix_runtime.recovery.busy"),
                true,
                cx,
            );
            return;
        }
        self.change_doc(
            "notebook.fix_runtime.recovery.undo_step",
            move |model| *model = doc,
            cx,
        );
        self.save_state = SaveState::Recovered;
        cx.notify();
    }

    pub(crate) fn discard_offered(&mut self, cx: &mut Context<Self>) {
        self.recovery_offer = None;
        autosave::remove_file_recovery(self.doc.id());
        cx.notify();
    }

    /// Whether Save may write the file: not while it changed on disk and
    /// the user has not chosen Reload or Keep mine.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub fn may_overwrite_file(&mut self, cx: &mut Context<Self>) -> bool {
        self.check_disk(cx);
        !self.conflict
    }

    fn start_conflict_poll(&mut self, cx: &mut Context<Self>) {
        if self.conflict_poll.is_some() {
            return;
        }
        self.conflict_poll = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(autosave::CONFLICT_POLL)
                    .await;
                if this.update(cx, |this, cx| this.check_disk(cx)).is_err() {
                    break;
                }
            }
        }));
    }

    /// Raise the conflict indicator when the file changed on disk.
    pub fn check_disk(&mut self, cx: &mut Context<Self>) {
        let Some(path) = &self.file_path else {
            return;
        };
        if !self.conflict && autosave::changed_on_disk(std::path::Path::new(path), self.file_stamp)
        {
            self.conflict = true;
            self.log(
                None,
                crate::localization::text("notebook.shell.conflict.detected"),
                true,
                None,
            );
            cx.notify();
        }
    }

    #[cfg(test)]
    pub fn has_conflict(&self) -> bool {
        self.conflict
    }

    #[cfg(test)]
    pub fn recovery_offered(&self) -> bool {
        self.recovery_offer.is_some()
    }

    pub(crate) fn keep_mine(&mut self, cx: &mut Context<Self>) {
        self.conflict = false;
        if let Some(path) = &self.file_path {
            self.file_stamp = autosave::file_stamp(std::path::Path::new(path));
        }
        self.dirty = true;
        self.save_state = SaveState::Dirty;
        cx.emit(NotebookEvent::Dirty);
        cx.notify();
    }

    // ── Layout ────────────────────────────────────────────────────────────

    pub(crate) fn update_layout(
        &mut self,
        change: impl FnOnce(&mut NotebookLayout),
        cx: &mut Context<Self>,
    ) {
        change(&mut self.layout);
        self.layout = self.layout.clone().sanitized();
        cx.emit(NotebookEvent::LayoutChanged(self.layout.clone()));
        cx.notify();
    }

    pub(crate) fn begin_drag(&mut self, target: DragTarget, position: f32) {
        let start_size = match target {
            DragTarget::Left => self.layout.left_width,
            DragTarget::Right => self.layout.right_width,
            DragTarget::Activity => self.layout.activity_height,
            DragTarget::Results => DEFAULT_RESULTS_HEIGHT,
        };
        self.drag = Some(DragState {
            target,
            cell: None,
            start: position,
            start_size,
        });
    }

    pub(crate) fn begin_results_drag(&mut self, id: &str, y: f32, cx: &mut Context<Self>) {
        let start_size = self
            .cells
            .get(id)
            .map(|rt| rt.results_height)
            .unwrap_or(DEFAULT_RESULTS_HEIGHT);
        self.drag = Some(DragState {
            target: DragTarget::Results,
            cell: Some(id.to_string()),
            start: y,
            start_size,
        });
        cx.notify();
    }

    fn drag_to(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        let Some(drag) = &self.drag else {
            return;
        };
        if event.pressed_button != Some(MouseButton::Left) {
            self.end_drag(cx);
            return;
        }
        let x = f32::from(event.position.x);
        let y = f32::from(event.position.y);
        match drag.target {
            DragTarget::Left => self.layout.left_width = drag.start_size + (x - drag.start),
            DragTarget::Right => self.layout.right_width = drag.start_size - (x - drag.start),
            DragTarget::Activity => {
                self.layout.activity_height = drag.start_size - (y - drag.start)
            }
            DragTarget::Results => {
                let height = (drag.start_size + (y - drag.start))
                    .clamp(MIN_RESULTS_HEIGHT, MAX_RESULTS_HEIGHT);
                if let Some(id) = drag.cell.clone() {
                    if let Some(rt) = self.cells.get_mut(&id) {
                        rt.results_height = height;
                    }
                    self.remeasure_cell(&id);
                }
            }
        }
        self.layout = self.layout.clone().sanitized();
        cx.notify();
    }

    fn end_drag(&mut self, cx: &mut Context<Self>) {
        if let Some(drag) = self.drag.take() {
            if drag.target != DragTarget::Results {
                cx.emit(NotebookEvent::LayoutChanged(self.layout.clone()));
            } else if let Some(id) = drag.cell {
                self.embedded_resize_ended(&id, cx);
            }
        }
        cx.notify();
    }

    /// The cell whose output is being resized right now.
    pub(super) fn resizing_output_of(&self) -> Option<&str> {
        self.drag
            .as_ref()
            .filter(|d| d.target == DragTarget::Results)
            .and_then(|d| d.cell.as_deref())
    }

    // ── Keyboard actions ──────────────────────────────────────────────────

    fn on_enter_command_mode(
        &mut self,
        _: &super::EnterCommandMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(id) = self.focused_cell(window, cx)
            && let Some(editor) = self.cells.get(&id).and_then(|rt| rt.editor.clone())
            && editor.update(cx, |e, cx| e.dismiss_for_command_mode(cx))
        {
            return;
        }
        if let Some(id) = self.editing_markdown.take() {
            self.remeasure_cell(&id);
        }
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    fn on_select_next(
        &mut self,
        _: &super::SelectNextCell,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.insert_menu_step(1, cx) {
            return;
        }
        self.select_relative(1, cx);
    }

    fn on_select_previous(
        &mut self,
        _: &super::SelectPreviousCell,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.insert_menu_step(-1, cx) {
            return;
        }
        self.select_relative(-1, cx);
    }

    fn on_insert_above(
        &mut self,
        _: &super::InsertCellAbove,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.insert_relative(false, cx);
    }

    fn on_insert_below(
        &mut self,
        _: &super::InsertCellBelow,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.insert_relative(true, cx);
    }

    fn on_delete(&mut self, _: &super::DeleteCell, _: &mut Window, cx: &mut Context<Self>) {
        self.delete_selection(cx);
    }

    fn on_to_markdown(
        &mut self,
        _: &super::ConvertToMarkdown,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.convert_selection(KIND_MARKDOWN, cx);
    }

    fn on_to_sql(&mut self, _: &super::ConvertToSql, _: &mut Window, cx: &mut Context<Self>) {
        self.convert_selection(KIND_SQL, cx);
    }

    fn on_edit(&mut self, _: &super::EditCell, _: &mut Window, cx: &mut Context<Self>) {
        if self.insert_menu_confirm(cx) {
            return;
        }
        if let Some(id) = self.active.clone() {
            self.edit_cell(&id, cx);
        }
    }

    fn on_run_and_advance(
        &mut self,
        _: &super::RunCellAndAdvance,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let editing = !self.in_command_mode(window);
        let Some(id) = self
            .focused_cell(window, cx)
            .or_else(|| self.active.clone())
        else {
            return;
        };
        self.activate_cell(&id, cx);
        self.run_active(cx);
        let index = self.doc.index_of(&id).unwrap_or(0);
        let next = if index + 1 < self.doc.len() {
            self.doc.cell(index + 1).map(|c| c.id.to_string())
        } else {
            Some(self.insert_cell(KIND_SQL, index + 1, cx))
        };
        if let Some(next) = next {
            self.activate_cell(&next, cx);
            self.reveal(&next, Reveal::Select, cx);
            if editing {
                self.edit_cell(&next, cx);
            } else {
                self.focus_root_pending = true;
            }
        }
    }

    fn on_run_and_insert(
        &mut self,
        _: &super::RunCellAndInsertBelow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = self
            .focused_cell(window, cx)
            .or_else(|| self.active.clone())
        else {
            return;
        };
        self.activate_cell(&id, cx);
        self.run_active(cx);
        let index = self
            .doc
            .index_of(&id)
            .map(|i| i + 1)
            .unwrap_or(self.doc.len());
        let new = self.insert_cell(KIND_SQL, index, cx);
        self.edit_cell(&new, cx);
    }

    /// Palette and menu entry points for the keyboard commands.
    pub fn command(&mut self, command: NotebookCommand, cx: &mut Context<Self>) {
        let active = self.active.clone();
        match command {
            NotebookCommand::SelectNext => self.select_relative(1, cx),
            NotebookCommand::SelectPrevious => self.select_relative(-1, cx),
            NotebookCommand::InsertAbove => {
                self.insert_relative(false, cx);
            }
            NotebookCommand::InsertBelow => {
                self.insert_relative(true, cx);
            }
            NotebookCommand::Delete => self.delete_selection(cx),
            NotebookCommand::ToMarkdown => self.convert_selection(KIND_MARKDOWN, cx),
            NotebookCommand::ToSql => self.convert_selection(KIND_SQL, cx),
            NotebookCommand::Edit => {
                if let Some(id) = active {
                    self.edit_cell(&id, cx);
                }
            }
            NotebookCommand::CommandMode => {
                self.editing_markdown = None;
                self.focus_root_pending = true;
                cx.notify();
            }
            NotebookCommand::RunAndAdvance => {
                self.run_active(cx);
                self.select_relative(1, cx);
            }
            NotebookCommand::RunAndInsertBelow => {
                self.run_active(cx);
                let id = self.insert_relative(true, cx);
                self.edit_cell(&id, cx);
            }
            NotebookCommand::Stop => self.stop(cx),
            NotebookCommand::Trust => self.trust_notebook(cx),
            NotebookCommand::AllowWrites => {
                if let Some(id) = active {
                    self.request_allow_writes(&id, cx);
                }
            }
            NotebookCommand::MaterializeLocally => {
                if let Some(id) = active {
                    self.request_materialize(&id, cx);
                }
            }
            NotebookCommand::ToggleLocalTarget => {
                if let Some(id) = active {
                    let local = self.doc.targets_local(&id);
                    self.set_cell_local(&id, !local, cx);
                }
            }
            NotebookCommand::ShowLocalData => self.update_layout(
                |l| {
                    l.left_visible = true;
                    l.left_page = super::layout::LeftRailPage::LocalData;
                },
                cx,
            ),
            NotebookCommand::InsertAttachment => {
                let index = match active.as_deref().and_then(|id| self.doc.index_of(id)) {
                    Some(at) => at + 1,
                    None => self.doc.len(),
                };
                let id = self.insert_cell(super::doc_adapter::KIND_ATTACHMENT, index, cx);
                self.reveal(&id, Reveal::Select, cx);
            }
            NotebookCommand::OpenDashboard => self.open_dashboard_editor(None, cx),
            NotebookCommand::ViewDashboard => self.open_dashboard_viewer(None, cx),
            NotebookCommand::ToggleZen => self.toggle_zen(cx),
            NotebookCommand::ToggleDense => self.toggle_dense(cx),
            NotebookCommand::Editing(command) => self.editing_command(command, cx),
            NotebookCommand::Embedded(command) => self.embedded_command(command, cx),
        }
    }
}

/// Keyboard commands the palette can also run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotebookCommand {
    SelectNext,
    SelectPrevious,
    InsertAbove,
    InsertBelow,
    Delete,
    ToMarkdown,
    ToSql,
    Edit,
    CommandMode,
    RunAndAdvance,
    RunAndInsertBelow,
    Stop,
    Trust,
    AllowWrites,
    MaterializeLocally,
    ToggleLocalTarget,
    ShowLocalData,
    InsertAttachment,
    OpenDashboard,
    ViewDashboard,
    ToggleZen,
    ToggleDense,
    /// Multi-select, clipboard, undo, moves, find, bookmarks, folds, pins.
    Editing(super::editing::EditingCommand),
    /// Embedded outputs: per cell and for the whole notebook.
    Embedded(super::embedded::EmbeddedCommand),
}

impl Render for NotebookView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_inputs(window, cx);
        self.ensure_local_inputs(window, cx);
        // A focused editor makes its cell active; a pending focus lands once
        // the editor exists.
        let focused = self
            .cells
            .iter()
            .find(|(_, rt)| {
                rt.editor
                    .as_ref()
                    .is_some_and(|e| e.read(cx).focus_handle(cx).contains_focused(window, cx))
            })
            .map(|(id, _)| id.clone());
        if let Some(id) = focused
            && self.active.as_deref() != Some(id.as_str())
        {
            self.active = Some(id);
            self.name_input = None;
        }
        if let Some(id) = self.pending_focus.clone()
            && let Some(editor) = self.cells.get(&id).and_then(|rt| rt.editor.clone())
        {
            self.pending_focus = None;
            window.focus(&editor.read(cx).focus_handle(cx), cx);
        }
        if std::mem::take(&mut self.focus_root_pending) {
            window.focus(&self.focus_handle, cx);
        }
        self.settle_reveal(window, cx);
        self.sync_output_only(cx);
        if self.present.takes_over() {
            return self.render_takeover(window, cx);
        }
        let command_mode = self.focus_handle.is_focused(window);
        let zen = self.present.zen;
        self.refresh_reference_contexts(cx);

        let canvas = div()
            .id("nb-canvas")
            .flex_1()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .bg(Theme::bg())
            .children(self.render_find_bar(window, cx))
            .children(self.render_pinned_strip(cx))
            .children(self.render_bulk_bar(cx))
            .child(
                gpui::list(
                    self.list_state.clone(),
                    cx.processor(|this, ix: usize, window, cx| {
                        this.render_list_item(ix, window, cx)
                    }),
                )
                .flex_1()
                .size_full(),
            )
            .when_some(self.undo_toast(cx), |d, toast| d.child(toast))
            .when_some(self.render_history_panel(cx), |d, panel| d.child(panel));
        let canvas = self.editing_canvas(canvas, cx);

        let body = div()
            .flex()
            .flex_row()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .when(!zen, |d| d.child(self.render_left_rail(window, cx)))
            .child(canvas)
            .when(!zen, |d| d.child(self.render_inspector(window, cx)));

        let root = div()
            .id("notebook")
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .min_h_0()
            .min_w_0()
            .key_context({
                let mut context = gpui::KeyContext::default();
                context.add(super::KEY_CONTEXT);
                if command_mode {
                    context.add(super::COMMAND_KEY_CONTEXT);
                }
                context
            })
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_enter_command_mode))
            .on_action(cx.listener(Self::on_select_next))
            .on_action(cx.listener(Self::on_select_previous))
            .on_action(cx.listener(Self::on_insert_above))
            .on_action(cx.listener(Self::on_insert_below))
            .on_action(cx.listener(Self::on_delete))
            .on_action(cx.listener(Self::on_to_markdown))
            .on_action(cx.listener(Self::on_to_sql))
            .on_action(cx.listener(Self::on_edit))
            .on_action(cx.listener(Self::on_run_and_advance))
            .on_action(cx.listener(Self::on_run_and_insert))
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                if this.drag.is_some() {
                    this.drag_to(event, cx);
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if this.drag.is_some() {
                        this.end_drag(cx);
                    }
                }),
            )
            .child(self.render_top_bar(window, cx))
            .children(self.render_banners(cx))
            .children(self.render_param_apply_bar(cx))
            .child(body)
            .when(!zen, |d| d.child(self.render_activity(window, cx)))
            .children(self.render_elevation_prompt(cx))
            .children(self.render_local_prompt(cx))
            .children(self.render_grant_dialog(cx))
            .children(self.render_param_editor());
        let root = super::embedded::keys::register_actions(root, cx);
        super::editing::keys::register_actions(root, cx)
    }
}

impl NotebookView {
    fn render_list_item(
        &mut self,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if ix >= self.doc.len() {
            return self.render_add_row(cx);
        }
        #[cfg(test)]
        super::editing::count_item_render();
        if self.is_folded_away(ix) {
            return div().id(("nb-cell", ix)).into_any_element();
        }
        let cell = self.render_cell(ix, window, cx);
        self.editing_wrap_cell(ix, cell, cx)
    }

    fn render_cell(
        &mut self,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        #[cfg(test)]
        super::perf_counters::bump(super::perf_counters::Pass::CellBuilt);
        if let Some(id) = self.doc.cell(ix).map(|c| c.id.to_string())
            && self.is_embedded(&id)
        {
            return self.render_embedded_cell(ix, &id, window, cx);
        }
        let Some(cell) = self.doc.cell(ix) else {
            return div().into_any_element();
        };
        let id = cell.id.to_string();
        let kind = cell.kind.to_string();
        let source = cell.source.to_string();
        let collapsed = cell.collapsed;
        let renderer = cells::renderer_for(&kind);
        let has_editor = self.has_live_editor(&id);
        let preview = (kind == KIND_SQL && !has_editor)
            .then(|| self.ensure_preview(&id))
            .flatten();
        let stale = self.is_stale(&id);
        let editing = self.editing_markdown.as_deref() == Some(id.as_str());
        let ai = (kind == super::ai::KIND_AI).then(|| self.ai_snapshot(&id, window, cx));
        let visual = self.prepare_visual(&id, &kind, window, cx);
        let interpolated = (kind == KIND_MARKDOWN)
            .then(|| self.interpolate_markdown(&id))
            .flatten();
        let markdown_images = (kind == KIND_MARKDOWN)
            .then(|| self.markdown_images(&source, cx))
            .flatten();
        let rt = self.cells.get(&id);
        let render_cx = CellRenderCx {
            id: id.clone(),
            index: ix,
            kind: kind.clone(),
            source,
            active: self.active.as_deref() == Some(id.as_str()),
            collapsed,
            state: self.doc.status(&id),
            stale,
            editor: rt.and_then(|rt| rt.editor.clone()),
            results: rt.and_then(|rt| rt.results.clone()),
            results_height: rt
                .map(|rt| rt.results_height)
                .unwrap_or(DEFAULT_RESULTS_HEIGHT),
            preview,
            editing,
            metrics: self.metrics.clone(),
            elapsed_ms: rt.and_then(|rt| rt.elapsed_ms),
            message: rt.and_then(|rt| rt.message_text()),
            ai,
            attachment: (kind == super::doc_adapter::KIND_ATTACHMENT)
                .then(|| self.attachment_cx(&id).map(Rc::new))
                .flatten(),
            output_only: false,
            visual,
            interpolated,
            markdown_images,
            analysis: None,
            spec: self
                .doc
                .model()
                .cell(&id)
                .map(|c| c.spec.clone())
                .unwrap_or_default(),
        };
        let render_cx = CellRenderCx {
            analysis: self.analysis_view(&id, &kind, window, cx),
            ..render_cx
        };
        self.apply_grid_chrome(&render_cx, false, cx);
        let header = self.render_cell_header(&render_cx, renderer, cx);
        let body = match renderer.render_with_view(self, &render_cx, window, cx) {
            Some(body) => body,
            None => renderer.render_body(&render_cx, window, cx),
        };
        let footer = self.render_local_footer(&render_cx, cx);
        let drill = self.render_drill_rows(&render_cx.id, cx);
        let active = render_cx.active;
        let selected = self.selected.contains(&id);
        let s = Theme::scaled;
        let (pad_x, pad_y) = self.present.cell_padding();
        let hover_id = id.clone();
        div()
            .id(("nb-cell", ix))
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                if *hovered {
                    this.hover(Hovered::Cell(hover_id.clone()), cx);
                }
            }))
            .w_full()
            .px(px(s(pad_x)))
            .py(px(s(pad_y)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .w_full()
                    .rounded(px(s(4.0)))
                    .border_1()
                    .border_color(if active {
                        Theme::accent()
                    } else if selected {
                        Theme::border_hover()
                    } else {
                        Theme::border()
                    })
                    .bg(Theme::panel_bg())
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                            this.click_cell(&id, event.modifiers, cx);
                        }),
                    )
                    .child(header)
                    .child(body)
                    .children(drill)
                    .children(footer),
            )
            .into_any_element()
    }
}
