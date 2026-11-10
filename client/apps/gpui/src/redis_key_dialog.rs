//! The Redis key browser's dialogs: Filter Keys… (a MATCH pattern for one
//! database node), Set TTL…, Remove expiry (PERSIST) and Delete Key… for
//! single keys, and
//! the bulk actions — Delete by Pattern…, Set TTL by Pattern… and Import
//! Command File…, plus the same delete / TTL apply for keys selected in a
//! results-grid key listing (`with_selected_keys`), and Filter Keys' hint
//! that turns a value query typed as a pattern into an FT.SEARCH tab.
//! The single-key dialogs confirm before they run the one
//! command they name on the node's own connection. The bulk dialogs never
//! write first: a dry run counts (or lists) what a pattern matches with the
//! same bounded `SCAN` the browser uses, and only then does the confirmed
//! apply run — batched `UNLINK` with progress and a stop button, per-key
//! `EXPIRE`/`PERSIST`, or a line-wise file run that collects every failure.
//!
//! These dialogs run through `tool_dialog::run_query`, which bypasses the
//! editor's run gate, so they carry the gate's checks themselves: a
//! read-only connection or a locked production connection disables every
//! write (the count and file check still work), a bulk delete must have its
//! key count typed back, and an import is classified line by line with the
//! editor's command allowlist — destructive or server-administration lines
//! are listed and need a typed confirmation, as does any import the
//! connection's safety settings would make the editor confirm.

use gpui::{
    Entity, EventEmitter, FocusHandle, Focusable, IntoElement, ParentElement, Render, Styled,
    Subscription, Window, div, prelude::*, px,
};
use gpui_component::Disableable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::select::{Select, SelectEvent, SelectState};
use sqlly_core::CellValue;
use sqlly_intel::query::safety::redis::{RedisCommand, quote_argument};

use crate::dialog::{Dialog, DialogFrame};
use crate::redis_bulk::{self, BatchPacer, ChunkResult, DryRun, ImportReview, LineApply};
use crate::theme::Theme;
use crate::tool_dialog::{ToolDialogContext, ToolQueryOutcome};

/// What the dialog is for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RedisKeyDialogMode {
    /// The MATCH pattern the database node at `node_id` scans with, and
    /// the patterns last typed on this profile (most recent first) for the
    /// dialog's recent-patterns dropdown.
    Filter {
        node_id: String,
        current: Option<String>,
        recent: Vec<String>,
    },
    /// `EXPIRE key <seconds>`.
    SetTtl { node_id: String, key: String },
    /// `PERSIST key`.
    Persist { node_id: String, key: String },
    /// `DEL key`, after typing the key name.
    Delete { node_id: String, key: String },
    /// Batched `UNLINK` over every key matching `pattern`, after a dry-run
    /// count of the same scan.
    BulkDelete { node_id: String, pattern: String },
    /// `EXPIRE` (or `PERSIST`) across every key matching `pattern`, after a
    /// dry-run count of the same scan.
    BatchTtl { node_id: String, pattern: String },
    /// A local command file run line-wise with per-line error capture.
    ImportCommands { node_id: String },
}

impl RedisKeyDialogMode {
    fn node_id(&self) -> &str {
        match self {
            Self::Filter { node_id, .. }
            | Self::SetTtl { node_id, .. }
            | Self::Persist { node_id, .. }
            | Self::Delete { node_id, .. }
            | Self::BulkDelete { node_id, .. }
            | Self::BatchTtl { node_id, .. }
            | Self::ImportCommands { node_id } => node_id,
        }
    }

    fn key(&self) -> Option<&str> {
        match self {
            Self::SetTtl { key, .. } | Self::Persist { key, .. } | Self::Delete { key, .. } => {
                Some(key)
            }
            _ => None,
        }
    }

    fn is_bulk(&self) -> bool {
        matches!(
            self,
            Self::BulkDelete { .. } | Self::BatchTtl { .. } | Self::ImportCommands { .. }
        )
    }
}

/// What the dialog asks of the app.
#[derive(Clone, Debug)]
pub enum RedisKeyDialogEvent {
    /// Store this MATCH pattern (`None` = every key) on the node and reload it.
    Filter {
        node_id: String,
        pattern: Option<String>,
    },
    /// A command ran; reload the node's parent listing and say so (an
    /// empty `node_id` — a results-grid selection — reloads nothing).
    Changed { node_id: String, message: String },
    /// Filter Keys was given a value query: open this FT.SEARCH script in a
    /// query tab on the dialog's connection.
    SearchValues {
        script: String,
        profile: Box<sqlly_core::ConnectionProfile>,
    },
    /// Dismiss the dialog.
    Close,
}

/// The bulk flow's phase. Nothing writes before `Ready`, and `Ready` only
/// ever comes from a finished dry run or file check on the current input.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum BulkPhase {
    /// Editing the pattern / file path. The dry run still has to happen.
    #[default]
    Edit,
    /// The dry run (or file check) is in flight.
    Checking,
    /// The dry run is done and its count is on screen; the apply may start.
    Ready,
    /// The apply is running, one chunk at a time.
    Applying,
    /// Finished or stopped: the summary is on screen.
    Done,
}

/// Everything the bulk modes track; unused by the single-key modes.
#[derive(Default)]
struct BulkState {
    phase: BulkPhase,
    /// The dry run's keys, byte-exact, and whether the scan covered the
    /// whole keyspace (a capped or stopped scan is partial, and says so).
    keys: Vec<Vec<u8>>,
    scan_complete: bool,
    /// The dry run's one-line verdict, honest about partial counts.
    scan_status: String,
    /// The dry run walked a Redis Cluster: keys are ordered by hash slot
    /// and every UNLINK batch stays inside one slot.
    cluster: bool,
    /// The paged dry run while it is walking; `None` outside Checking.
    dry_run: Option<DryRun>,
    /// BatchTtl: PERSIST instead of EXPIRE.
    ttl_persist: bool,
    /// The TTL/import chunk driver once the apply starts.
    apply: Option<LineApply>,
    /// Import: the parsed file, moved into the apply on Run.
    pending_commands: Vec<RedisCommand>,
    /// Import: the file's safety review, and whether running it needs the
    /// typed confirmation phrase.
    review: Option<ImportReview>,
    confirm_required: bool,
    /// Delete: keys consumed into UNLINK batches so far, batches issued,
    /// the server-reported totals, and the pacer sizing the next batch.
    delete_next: usize,
    delete_batches: usize,
    delete_applied: i64,
    delete_failures: Vec<String>,
    delete_pacer: BatchPacer,
    /// When the chunk in flight was issued, for the pacer.
    chunk_started: Option<std::time::Instant>,
    /// Stop after the current chunk (or dry-run page).
    cancel_requested: bool,
    /// The Done phase's summary text (also emitted to the status bar).
    summary: String,
}

impl BulkState {
    fn reset(&mut self) {
        *self = BulkState {
            ttl_persist: self.ttl_persist,
            ..Default::default()
        };
    }
}

/// Import files are read fully into memory for parsing; cap them honestly.
pub(crate) const IMPORT_MAX_BYTES: u64 = 8 * 1024 * 1024;
/// Failure rows the Done phase lists before a "+N more" note.
const FAILURE_DISPLAY_CAP: usize = 50;
/// Flagged import lines the review lists before a "+N more" note.
const FLAGGED_DISPLAY_CAP: usize = 20;

/// Why this dialog may not write on its connection (see
/// [`redis_bulk::write_block_reason`]); the key filter never writes.
fn write_block_for(
    context: &ToolDialogContext,
    mode: &RedisKeyDialogMode,
    production: sqlly_intel::query::safety::ProductionDecision,
) -> Option<String> {
    if matches!(mode, RedisKeyDialogMode::Filter { .. }) {
        return None;
    }
    let profile = &context.profile;
    redis_bulk::write_block_reason(
        profile.enforce_read_only || profile.block_all_but_select,
        production,
    )
}

/// A bulk delete runs only once the user has typed back the number of keys
/// the dry run matched — the magnitude is the thing to confirm.
fn bulk_delete_confirmed(count: usize, typed: &str) -> bool {
    count > 0 && typed.trim() == count.to_string()
}

/// Read, decode and parse an import file (background thread). Refuses
/// oversized files, non-UTF-8 lines and unparsable lines with the reason.
fn read_import_file(path: &str) -> Result<(String, Vec<RedisCommand>), String> {
    let read_error = |error: &dyn std::fmt::Display| {
        crate::localization::format(
            "explorer.redis_key_dialog.file_read_error",
            &[("path", path), ("error", &error.to_string())],
        )
    };
    let script_error = |error: &dyn std::fmt::Display| {
        crate::localization::format(
            "explorer.redis_key_dialog.invalid_command_file",
            &[("error", &error.to_string())],
        )
    };
    let meta = std::fs::metadata(path).map_err(|error| read_error(&error))?;
    if meta.len() > IMPORT_MAX_BYTES {
        return Err(crate::localization::format(
            "explorer.redis_key_dialog.file_too_large",
            &[
                ("path", path),
                ("size", &(meta.len() / 1_048_576).to_string()),
                ("limit", &(IMPORT_MAX_BYTES / 1_048_576).to_string()),
            ],
        ));
    }
    let bytes = std::fs::read(path).map_err(|error| read_error(&error))?;
    let text = redis_bulk::decode_import(&bytes).map_err(|error| script_error(&error))?;
    let commands = sqlly_intel::query::safety::redis::parse_script(text)
        .map_err(|error| script_error(&error))?;
    Ok((text.to_string(), commands))
}

pub struct RedisKeyDialog {
    context: ToolDialogContext,
    mode: RedisKeyDialogMode,
    input: Entity<InputState>,
    /// BatchTtl's seconds / Import's file path (unused by the other modes).
    second_input: Entity<InputState>,
    /// Filter: the profile's recent patterns, when it has any.
    recent_select: Option<Entity<SelectState<Vec<String>>>>,
    typed: String,
    typed_second: String,
    running: bool,
    error: Option<String>,
    bulk: BulkState,
    /// Why this connection may not be written at all (read-only policy,
    /// locked production), captured when the dialog opened.
    write_block: Option<String>,
    /// The production guard's decision (refreshed right before every
    /// write); an unlocked production connection always confirms imports.
    production: sqlly_intel::query::safety::ProductionDecision,
    production_check: crate::tool_dialog::ProductionCheck,
    /// Bumps per dry run / file check (and on every pattern or path edit)
    /// so a stale count or file read is dropped instead of applied.
    check_generation: u64,
    /// The bulk keys came from a results-grid selection rather than a
    /// pattern: no pattern to edit or count, the keys are listed instead.
    selected_keys: bool,
    /// Filter: the FT._LIST lookup behind "Search values with FT.SEARCH…"
    /// is in flight, and what it found when no search could be opened.
    value_search_running: bool,
    value_search_note: Option<String>,
    focus_handle: FocusHandle,
    frame: DialogFrame,
    _subscriptions: Vec<Subscription>,
}

/// Selected keys the confirmation lists by name before a "+N more" note.
const SELECTED_PREVIEW_CAP: usize = 8;

/// The first selected keys as the confirmation lists them (binary-safe,
/// quoted the way `redis-cli` would print them) and how many more there are.
fn selected_keys_preview(keys: &[Vec<u8>]) -> (Vec<String>, usize) {
    let shown = keys
        .iter()
        .take(SELECTED_PREVIEW_CAP)
        .map(|key| quote_argument(key))
        .collect();
    (shown, keys.len().saturating_sub(SELECTED_PREVIEW_CAP))
}

impl EventEmitter<RedisKeyDialogEvent> for RedisKeyDialog {}

impl RedisKeyDialog {
    /// `production` is the production guard's decision for the dialog's
    /// connection at open time and `production_check` re-reads it before
    /// each write (`AppView` owns the unlock state, which can expire).
    pub fn new(
        context: ToolDialogContext,
        mode: RedisKeyDialogMode,
        production: sqlly_intel::query::safety::ProductionDecision,
        production_check: crate::tool_dialog::ProductionCheck,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        let (placeholder, initial) = match &mode {
            RedisKeyDialogMode::Filter { current, .. } => (
                crate::localization::text("explorer.redis_key_dialog.filter_placeholder"),
                current.clone().unwrap_or_default(),
            ),
            RedisKeyDialogMode::SetTtl { .. } => (
                crate::localization::text("explorer.redis_key_dialog.ttl_placeholder"),
                String::new(),
            ),
            RedisKeyDialogMode::Persist { .. } => (String::new(), String::new()),
            RedisKeyDialogMode::Delete { key, .. } => (
                crate::localization::format(
                    "explorer.redis_key_dialog.delete_placeholder",
                    &[("key", key)],
                ),
                String::new(),
            ),
            RedisKeyDialogMode::BulkDelete { pattern, .. }
            | RedisKeyDialogMode::BatchTtl { pattern, .. } => (
                crate::localization::text("explorer.redis_key_dialog.bulk_pattern_placeholder"),
                pattern.clone(),
            ),
            RedisKeyDialogMode::ImportCommands { .. } => (
                crate::localization::text("explorer.redis_key_dialog.import_path_placeholder"),
                String::new(),
            ),
        };
        let second_placeholder = match &mode {
            RedisKeyDialogMode::BatchTtl { .. } => {
                crate::localization::text("explorer.redis_key_dialog.bulk_ttl_placeholder")
            }
            RedisKeyDialogMode::BulkDelete { .. } => crate::localization::text(
                "explorer.redis_key_dialog.bulk_delete_confirmation_placeholder",
            ),
            RedisKeyDialogMode::ImportCommands { .. } => crate::localization::format(
                "explorer.redis_key_dialog.import_confirmation_placeholder",
                &[("phrase", redis_bulk::IMPORT_CONFIRM_PHRASE)],
            ),
            _ => String::new(),
        };
        let input = cx.new(|cx| {
            let state = InputState::new(window, cx).placeholder(placeholder);
            if initial.is_empty() {
                state
            } else {
                state.default_value(initial.clone())
            }
        });
        let second_input = cx.new(|cx| InputState::new(window, cx).placeholder(second_placeholder));
        let recent_select = match &mode {
            RedisKeyDialogMode::Filter { recent, .. } if !recent.is_empty() => {
                Some(cx.new(|cx| SelectState::new(recent.clone(), None, window, cx)))
            }
            _ => None,
        };
        let mut subscriptions = vec![cx.subscribe_in(
            &input,
            window,
            |this: &mut Self, state, event: &InputEvent, window, cx| match event {
                InputEvent::Change => {
                    let value = state.read(cx).value().to_string();
                    if value == this.typed {
                        return;
                    }
                    this.typed = value;
                    this.error = None;
                    this.value_search_note = None;
                    if this.mode.is_bulk() {
                        // A changed pattern or path invalidates the dry run:
                        // nothing may apply against a stale count, and a
                        // confirmation typed for the old count must not
                        // carry over to the new one.
                        this.bulk.reset();
                        this.bulk.phase = BulkPhase::Edit;
                        this.check_generation += 1;
                        if matches!(
                            this.mode,
                            RedisKeyDialogMode::BulkDelete { .. }
                                | RedisKeyDialogMode::ImportCommands { .. }
                        ) && !this.typed_second.is_empty()
                        {
                            this.second_input
                                .update(cx, |s, cx| s.set_value("", window, cx));
                        }
                    }
                    cx.notify();
                }
                InputEvent::PressEnter { .. } => this.confirm(cx),
                _ => {}
            },
        )];
        subscriptions.push(cx.subscribe_in(
            &second_input,
            window,
            |this: &mut Self, state, event: &InputEvent, _window, cx| match event {
                InputEvent::Change => {
                    this.typed_second = state.read(cx).value().to_string();
                    this.error = None;
                    cx.notify();
                }
                InputEvent::PressEnter { .. } => this.confirm(cx),
                _ => {}
            },
        ));
        if let Some(select) = &recent_select {
            // Picking a recent pattern fills the field; Apply still confirms.
            subscriptions.push(cx.subscribe_in(
                select,
                window,
                |this: &mut Self, _, event: &SelectEvent<Vec<String>>, window, cx| {
                    let SelectEvent::Confirm(Some(pattern)) = event else {
                        return;
                    };
                    let pattern = pattern.clone();
                    this.input.update(cx, |state, cx| {
                        state.set_value(pattern.clone(), window, cx);
                    });
                    this.typed = pattern;
                    this.error = None;
                    cx.notify();
                },
            ));
        }
        let width = if mode.is_bulk() {
            Theme::scaled(560.0)
        } else {
            Theme::scaled(480.0)
        };
        let write_block = write_block_for(&context, &mode, production);
        Self {
            context,
            mode,
            input,
            second_input,
            recent_select,
            typed: initial,
            typed_second: String::new(),
            running: false,
            error: None,
            bulk: BulkState::default(),
            write_block,
            production,
            production_check,
            check_generation: 0,
            selected_keys: false,
            value_search_running: false,
            value_search_note: None,
            focus_handle: cx.focus_handle(),
            frame: DialogFrame::new_auto(width),
            _subscriptions: subscriptions,
        }
    }

    /// Act on keys selected in a results grid instead of a pattern: the
    /// keys are known, so the dialog opens ready to confirm (a delete still
    /// needs the count typed back) with the keys listed. `cluster` keeps
    /// every UNLINK batch inside one hash slot; `persist` presets Remove
    /// expiry for a Set TTL dialog.
    pub fn with_selected_keys(mut self, keys: Vec<Vec<u8>>, cluster: bool, persist: bool) -> Self {
        self.selected_keys = true;
        self.bulk.keys = keys;
        self.bulk.cluster = cluster;
        if cluster {
            redis_bulk::order_by_slot(&mut self.bulk.keys);
        }
        self.bulk.scan_complete = true;
        self.bulk.ttl_persist = persist;
        self.bulk.scan_status = crate::localization::format(
            "explorer.redis_key_dialog.selected_keys_status",
            &[("count", &self.bulk.keys.len().to_string())],
        );
        self.bulk.phase = BulkPhase::Ready;
        self
    }

    /// "Search values with FT.SEARCH…": list the database's search indexes,
    /// then open a search over the first one in a query tab — or, when the
    /// server has no search module or no index, say what is needed.
    fn search_values(&mut self, cx: &mut gpui::Context<Self>) {
        if self.value_search_running {
            return;
        }
        self.value_search_running = true;
        self.value_search_note = None;
        cx.notify();
        let pattern = self.typed.clone();
        crate::tool_dialog::run_query(
            &self.context,
            crate::schema_tree::redis::INDEXES_COMMAND.to_string(),
            cx,
            move |this, outcome: ToolQueryOutcome, cx| {
                this.value_search_running = false;
                if let Some(error) = outcome.error {
                    this.value_search_note = Some(crate::localization::format(
                        "explorer.redis_key_dialog.value_search_no_module",
                        &[("error", &error)],
                    ));
                    cx.notify();
                    return;
                }
                let indexes: Vec<String> = outcome
                    .sets
                    .first()
                    .map(|set| crate::schema_tree::redis::parse_index_rows(&set.rows))
                    .unwrap_or_default()
                    .into_iter()
                    .map(|index| index.name)
                    .collect();
                match crate::redis_key_filters::value_search_script(&pattern, &indexes) {
                    Some(script) => {
                        cx.emit(RedisKeyDialogEvent::SearchValues {
                            script,
                            profile: Box::new(this.context.profile.clone()),
                        });
                        cx.emit(RedisKeyDialogEvent::Close);
                    }
                    None => {
                        this.value_search_note = Some(crate::localization::text(
                            "explorer.redis_key_dialog.value_search_no_index",
                        ));
                        cx.notify();
                    }
                }
            },
        );
    }

    /// The command the confirm button runs, or `None` for the filter and
    /// bulk modes (which build their own scripts).
    pub fn command(&self) -> Option<String> {
        let key = self.mode.key()?;
        let key = quote_argument(key.as_bytes());
        Some(match &self.mode {
            RedisKeyDialogMode::SetTtl { .. } => {
                format!("EXPIRE {key} {}", self.typed.trim())
            }
            RedisKeyDialogMode::Persist { .. } => format!("PERSIST {key}"),
            RedisKeyDialogMode::Delete { .. } => format!("DEL {key}"),
            _ => return None,
        })
    }

    fn confirm_allowed(&self) -> bool {
        if self.running {
            return false;
        }
        if self.mode.is_bulk() && self.bulk.phase == BulkPhase::Done {
            // "Done" closes the dialog.
            return true;
        }
        // Writes are refused outright on a read-only or locked production
        // connection; the bulk dry runs and the file check still work.
        let writes_allowed = self.write_block.is_none();
        match &self.mode {
            RedisKeyDialogMode::Filter { .. } => true,
            RedisKeyDialogMode::Persist { .. } => writes_allowed,
            RedisKeyDialogMode::SetTtl { .. } => {
                writes_allowed && self.typed.trim().parse::<i64>().is_ok_and(|n| n > 0)
            }
            RedisKeyDialogMode::Delete { key, .. } => writes_allowed && self.typed.trim() == key,
            RedisKeyDialogMode::BulkDelete { .. } => match self.bulk.phase {
                BulkPhase::Edit => !self.typed.trim().is_empty(),
                BulkPhase::Ready => {
                    writes_allowed
                        && bulk_delete_confirmed(self.bulk.keys.len(), &self.typed_second)
                }
                _ => false,
            },
            RedisKeyDialogMode::BatchTtl { .. } => match self.bulk.phase {
                BulkPhase::Edit => !self.typed.trim().is_empty(),
                BulkPhase::Ready => {
                    writes_allowed
                        && !self.bulk.keys.is_empty()
                        && (self.bulk.ttl_persist
                            || self.typed_second.trim().parse::<i64>().is_ok_and(|n| n > 0))
                }
                _ => false,
            },
            RedisKeyDialogMode::ImportCommands { .. } => match self.bulk.phase {
                BulkPhase::Edit => !self.typed.trim().is_empty(),
                BulkPhase::Ready => {
                    writes_allowed
                        && !self.bulk.pending_commands.is_empty()
                        && (!self.bulk.confirm_required
                            || self.typed_second.trim() == redis_bulk::IMPORT_CONFIRM_PHRASE)
                }
                _ => false,
            },
        }
    }

    fn confirm(&mut self, cx: &mut gpui::Context<Self>) {
        // The production unlock may have expired since the dialog opened:
        // re-read the guard before anything that could write.
        self.production = (self.production_check)(cx);
        self.write_block = write_block_for(&self.context, &self.mode, self.production);
        if !self.confirm_allowed() {
            cx.notify();
            return;
        }
        if self.mode.is_bulk() && self.bulk.phase == BulkPhase::Done {
            self.close(cx);
            return;
        }
        match &self.mode {
            RedisKeyDialogMode::Filter { .. } => {
                let node_id = self.mode.node_id().to_string();
                let pattern = Some(self.typed.trim().to_string()).filter(|p| !p.is_empty());
                cx.emit(RedisKeyDialogEvent::Filter { node_id, pattern });
                cx.emit(RedisKeyDialogEvent::Close);
            }
            RedisKeyDialogMode::SetTtl { .. }
            | RedisKeyDialogMode::Persist { .. }
            | RedisKeyDialogMode::Delete { .. } => self.run_single(cx),
            RedisKeyDialogMode::BulkDelete { .. } | RedisKeyDialogMode::BatchTtl { .. } => {
                match self.bulk.phase {
                    BulkPhase::Edit => self.start_dry_run(cx),
                    BulkPhase::Ready => self.start_apply(cx),
                    _ => {}
                }
            }
            RedisKeyDialogMode::ImportCommands { .. } => match self.bulk.phase {
                BulkPhase::Edit => self.check_file(cx),
                BulkPhase::Ready => self.start_apply(cx),
                _ => {}
            },
        }
    }

    /// The single-key modes: one confirmed command, one reply, done.
    fn run_single(&mut self, cx: &mut gpui::Context<Self>) {
        let node_id = self.mode.node_id().to_string();
        let Some(command) = self.command() else {
            return;
        };
        self.running = true;
        self.error = None;
        cx.notify();
        let describe = command.clone();
        crate::tool_dialog::run_query(
            &self.context,
            command,
            cx,
            move |this, outcome: ToolQueryOutcome, cx| {
                this.running = false;
                if let Some(error) = outcome.error {
                    this.error = Some(error);
                    cx.notify();
                    return;
                }
                let reply = outcome
                    .sets
                    .first()
                    .and_then(|set| set.rows.first())
                    .and_then(|row| row.first())
                    .map(|cell| match cell {
                        sqlly_core::CellValue::I64(n) => n.to_string(),
                        sqlly_core::CellValue::String(s) => s.clone(),
                        other => format!("{other:?}"),
                    })
                    .unwrap_or_default();
                cx.emit(RedisKeyDialogEvent::Changed {
                    node_id,
                    message: format!("{describe} → {reply}"),
                });
                cx.emit(RedisKeyDialogEvent::Close);
            },
        );
    }

    /// The dry run every bulk write starts with: the same bounded `SCAN`
    /// the browser lists with, never a write, driven one page at a time so
    /// Stop is honoured between pages. Its keys — byte-exact — are exactly
    /// what the apply will touch; a stopped or capped walk is labelled
    /// partial and the apply still touches only what it listed.
    fn start_dry_run(&mut self, cx: &mut gpui::Context<Self>) {
        let pattern = self.typed.trim().to_string();
        self.bulk.phase = BulkPhase::Checking;
        self.bulk.cancel_requested = false;
        self.bulk.dry_run = Some(DryRun::new(&pattern, redis_bulk::BULK_KEY_LIMIT));
        self.error = None;
        self.check_generation += 1;
        cx.notify();
        self.run_dry_run_page(cx);
    }

    /// Issue the dry run's next page, or finish the count when the walk is
    /// complete, capped, or stopped.
    fn run_dry_run_page(&mut self, cx: &mut gpui::Context<Self>) {
        let generation = self.check_generation;
        if self.bulk.cancel_requested
            && let Some(run) = &mut self.bulk.dry_run
        {
            run.stop();
        }
        let Some(command) = self.bulk.dry_run.as_ref().and_then(DryRun::next_command) else {
            self.finish_dry_run(cx);
            return;
        };
        crate::tool_dialog::run_query_with_cap(
            &self.context,
            command,
            crate::schema_tree::redis::listing_row_cap(redis_bulk::DRY_RUN_PAGE),
            cx,
            move |this, outcome, cx| {
                // The pattern was edited while the count ran: this page
                // counts a pattern that is no longer on screen — drop it.
                if this.check_generation != generation || this.bulk.phase != BulkPhase::Checking {
                    return;
                }
                if let Some(error) = outcome.error {
                    this.bulk.phase = BulkPhase::Edit;
                    this.bulk.dry_run = None;
                    this.error = Some(error);
                    cx.notify();
                    return;
                }
                let rows = outcome
                    .sets
                    .first()
                    .map(|set| set.rows.clone())
                    .unwrap_or_default();
                if let Some(run) = &mut this.bulk.dry_run {
                    run.absorb(&rows);
                }
                cx.notify();
                this.run_dry_run_page(cx);
            },
        );
    }

    fn finish_dry_run(&mut self, cx: &mut gpui::Context<Self>) {
        let Some(run) = self.bulk.dry_run.take() else {
            return;
        };
        self.bulk.scan_status = run.status();
        self.bulk.scan_complete = !run.is_partial();
        self.bulk.cluster = run.cluster;
        self.bulk.keys = run.keys;
        if self.bulk.cluster {
            redis_bulk::order_by_slot(&mut self.bulk.keys);
        }
        self.bulk.phase = BulkPhase::Ready;
        cx.notify();
    }

    /// Import: read and parse the file before anything runs. A file that is
    /// not redis-cli-syntax (an unbalanced quote, a bad JSON line) or not
    /// UTF-8 is refused whole with the offending line number — running a
    /// mangled suffix would be worse than refusing. The read happens off the
    /// UI thread; a result for a path edited since is dropped.
    fn check_file(&mut self, cx: &mut gpui::Context<Self>) {
        let path = self.typed.trim().to_string();
        self.error = None;
        self.bulk.phase = BulkPhase::Checking;
        self.check_generation += 1;
        let generation = self.check_generation;
        cx.notify();
        let task = cx.background_spawn(async move { read_import_file(&path) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                if this.check_generation != generation || this.bulk.phase != BulkPhase::Checking {
                    return;
                }
                match result {
                    Ok((text, commands)) => {
                        let review = ImportReview::of(&commands);
                        this.bulk.confirm_required =
                            !review.flagged.is_empty() || this.editor_gate_would_confirm(&text);
                        this.bulk.review = Some(review);
                        this.bulk.pending_commands = commands;
                        this.bulk.phase = BulkPhase::Ready;
                    }
                    Err(message) => {
                        this.bulk.phase = BulkPhase::Edit;
                        this.error = Some(message);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Whether the editor's run gate would make the user confirm this text
    /// on this connection (the same Redis analysis and safety settings, an
    /// unlocked production connection included).
    fn editor_gate_would_confirm(&self, text: &str) -> bool {
        let Some(analysis) = crate::mutation_review::analyze_redis(text) else {
            return false;
        };
        crate::mutation_review::SafetyFeatures::effective_for_profile(
            &self.context.profile,
            crate::connection::ConnectionStore::global(),
        )
        .requires_confirmation(&analysis, self.production)
    }

    /// Begin the confirmed apply: build the batch plan from the dry run's
    /// keys (delete/TTL) or the parsed file (import), then drive it one
    /// chunk per query.
    fn start_apply(&mut self, cx: &mut gpui::Context<Self>) {
        self.bulk.cancel_requested = false;
        self.bulk.chunk_started = None;
        match &self.mode {
            RedisKeyDialogMode::BulkDelete { .. } => {
                self.bulk.delete_next = 0;
                self.bulk.delete_batches = 0;
                self.bulk.delete_applied = 0;
                self.bulk.delete_failures.clear();
                self.bulk.delete_pacer = BatchPacer::new(redis_bulk::UNLINK_BATCH);
            }
            RedisKeyDialogMode::BatchTtl { .. } => {
                let seconds = self.typed_second.trim().parse::<i64>().unwrap_or(0);
                let persist = self.bulk.ttl_persist;
                let commands = redis_bulk::ttl_commands(&self.bulk.keys, seconds, persist);
                self.bulk.apply = Some(LineApply::new(commands));
            }
            RedisKeyDialogMode::ImportCommands { .. } => {
                let commands = std::mem::take(&mut self.bulk.pending_commands);
                self.bulk.apply = Some(LineApply::new(commands));
            }
            _ => return,
        }
        self.bulk.phase = BulkPhase::Applying;
        cx.notify();
        self.run_next_chunk(cx);
    }

    /// Issue the next chunk, or finish when the plan ran out or the user
    /// asked to stop. Each chunk is one query; the stop flag is honoured
    /// between chunks, so a batch already sent always completes — and the
    /// pacer keeps every batch to about a second, so that wait stays short.
    fn run_next_chunk(&mut self, cx: &mut gpui::Context<Self>) {
        if let Some(started) = self.bulk.chunk_started.take() {
            let elapsed = started.elapsed();
            match &self.mode {
                RedisKeyDialogMode::BulkDelete { .. } => self.bulk.delete_pacer.record(elapsed),
                _ => {
                    if let Some(apply) = &mut self.bulk.apply {
                        apply.record_chunk(elapsed);
                    }
                }
            }
        }
        if self.bulk.cancel_requested {
            self.finish_apply(false, cx);
            return;
        }
        self.bulk.chunk_started = Some(std::time::Instant::now());
        match &self.mode {
            RedisKeyDialogMode::BulkDelete { .. } => {
                let start = self.bulk.delete_next;
                let end = redis_bulk::batch_end(
                    &self.bulk.keys,
                    start,
                    self.bulk.delete_pacer.size(),
                    self.bulk.cluster,
                );
                if start >= end {
                    self.finish_apply(true, cx);
                    return;
                }
                let line = redis_bulk::unlink_line(&self.bulk.keys[start..end]);
                self.bulk.delete_next = end;
                self.bulk.delete_batches += 1;
                let batch_no = self.bulk.delete_batches;
                crate::tool_dialog::run_query(&self.context, line, cx, move |this, outcome, cx| {
                    if let Some(error) = outcome.error {
                        this.bulk
                            .delete_failures
                            .push(format!("batch {batch_no}: {error}"));
                    } else {
                        let unlinked = outcome
                            .sets
                            .first()
                            .and_then(|set| set.rows.first())
                            .and_then(|row| row.first())
                            .and_then(|cell| match cell {
                                CellValue::I64(n) => Some(*n),
                                _ => None,
                            })
                            .unwrap_or(0);
                        this.bulk.delete_applied += unlinked;
                    }
                    cx.notify();
                    this.run_next_chunk(cx);
                });
            }
            RedisKeyDialogMode::BatchTtl { .. } | RedisKeyDialogMode::ImportCommands { .. } => {
                let chunk = self
                    .bulk
                    .apply
                    .as_ref()
                    .and_then(|apply| apply.next_chunk());
                let Some(chunk) = chunk else {
                    self.finish_apply(true, cx);
                    return;
                };
                crate::tool_dialog::run_query(&self.context, chunk.script.clone(), cx, {
                    let chunk = chunk.clone();
                    move |this, outcome, cx| {
                        let result = match outcome.error {
                            None => ChunkResult::Ok,
                            Some(error) if redis_bulk::parse_line_error(&error).is_some() => {
                                ChunkResult::Failed(error)
                            }
                            Some(error) => ChunkResult::Fatal(error),
                        };
                        if let Some(apply) = &mut this.bulk.apply {
                            apply.apply(&chunk, result);
                        }
                        cx.notify();
                        this.run_next_chunk(cx);
                    }
                });
            }
            _ => {}
        }
    }

    /// The apply is over (ran to the end, or stopped): summarize, refresh
    /// the node, and leave the summary on screen.
    fn finish_apply(&mut self, completed: bool, cx: &mut gpui::Context<Self>) {
        self.bulk.phase = BulkPhase::Done;
        let node_id = self.mode.node_id().to_string();
        let message = crate::localization::format(
            "explorer.redis_key_dialog.apply_summary_in_database",
            &[
                ("summary", &self.apply_summary(completed)),
                ("database", &self.database_label()),
            ],
        );
        self.bulk.summary = message.clone();
        cx.emit(RedisKeyDialogEvent::Changed { node_id, message });
        cx.notify();
    }

    /// The apply's one-line summary so far.
    fn apply_summary(&self, completed: bool) -> String {
        match &self.mode {
            RedisKeyDialogMode::BulkDelete { .. } => {
                let failed = self.bulk.delete_failures.len();
                let key = match (completed, failed > 0) {
                    (true, false) => "explorer.redis_key_dialog.delete_complete",
                    (true, true) => "explorer.redis_key_dialog.delete_complete_failed",
                    (false, false) => "explorer.redis_key_dialog.delete_stopped",
                    (false, true) => "explorer.redis_key_dialog.delete_stopped_failed",
                };
                let mut arguments = vec![("applied", self.bulk.delete_applied.to_string())];
                if !completed {
                    arguments.push(("total", self.bulk.keys.len().to_string()));
                }
                if failed > 0 {
                    arguments.push(("failed", failed.to_string()));
                }
                let arguments = arguments
                    .iter()
                    .map(|(name, value)| (*name, value.as_str()))
                    .collect::<Vec<_>>();
                crate::localization::format(key, &arguments)
            }
            RedisKeyDialogMode::BatchTtl { .. } => self
                .bulk
                .apply
                .as_ref()
                .map(|apply| apply.summary("keys"))
                .unwrap_or_default(),
            RedisKeyDialogMode::ImportCommands { .. } => self
                .bulk
                .apply
                .as_ref()
                .map(|apply| apply.summary("commands"))
                .unwrap_or_default(),
            _ => String::new(),
        }
    }

    /// The dialog's logical database as the profile names it.
    fn database_label(&self) -> String {
        crate::schema_tree::redis::database_label_text(
            self.context.profile.database.as_deref().unwrap_or("0"),
            &self.context.profile,
        )
    }

    /// A paged dry run is walking (Delete / Set TTL by pattern in Checking).
    fn scanning(&self) -> bool {
        self.bulk.phase == BulkPhase::Checking && self.bulk.dry_run.is_some()
    }

    fn close(&mut self, cx: &mut gpui::Context<Self>) {
        if self.bulk.phase == BulkPhase::Applying {
            // Closing drops this dialog, so the in-flight chunk's callback
            // will never run: stop here and post the partial summary to the
            // status bar now. The chunk already sent may still land.
            self.bulk.cancel_requested = true;
            let node_id = self.mode.node_id().to_string();
            let message = crate::localization::format(
                "explorer.redis_key_dialog.closed_mid_run",
                &[("summary", &self.apply_summary(false))],
            );
            cx.emit(RedisKeyDialogEvent::Changed { node_id, message });
        }
        cx.emit(RedisKeyDialogEvent::Close);
    }
}

impl Focusable for RedisKeyDialog {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl RedisKeyDialog {
    fn render_single_body(&self, cx: &mut gpui::Context<Self>) -> gpui::Div {
        let s = Theme::scaled;
        let (prompt, danger) = match &self.mode {
            RedisKeyDialogMode::Filter { .. } => (
                crate::localization::text("explorer.redis_key_dialog.filter_explanation"),
                false,
            ),
            RedisKeyDialogMode::SetTtl { .. } => (
                crate::localization::text("explorer.redis_key_dialog.ttl_explanation"),
                false,
            ),
            RedisKeyDialogMode::Persist { .. } => (
                crate::localization::text("explorer.redis_key_dialog.persist_explanation"),
                false,
            ),
            RedisKeyDialogMode::Delete { .. } => (
                crate::localization::text("explorer.redis_key_dialog.delete_explanation"),
                true,
            ),
            _ => unreachable!("bulk modes render their own body"),
        };
        let mut body = div().flex().flex_col().gap(px(s(10.0))).child(
            div()
                .text_size(px(Theme::font_size_ratio(0.87)))
                .text_color(if danger {
                    Theme::warning()
                } else {
                    Theme::text_dim()
                })
                .child(prompt),
        );
        if !matches!(self.mode, RedisKeyDialogMode::Persist { .. }) {
            body = body.child(Input::new(&self.input).w_full());
        }
        if matches!(self.mode, RedisKeyDialogMode::Filter { .. })
            && let Some(kind) = crate::redis_key_filters::value_query_kind(&self.typed)
        {
            body = body.child(self.render_value_query_hint(kind, cx));
        }
        if let Some(select) = &self.recent_select {
            body = body.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(8.0)))
                    .child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.87)))
                            .text_color(Theme::text_dim())
                            .child(crate::localization::text(
                                "explorer.redis_key_dialog.recent_label",
                            )),
                    )
                    .child(
                        Select::new(select)
                            .placeholder(crate::localization::text(
                                "explorer.redis_key_dialog.recent_placeholder",
                            ))
                            .w_full(),
                    ),
            );
        }
        if let Some(command) = self.command() {
            body = body.child(
                div()
                    .p(px(s(8.0)))
                    .rounded(px(s(4.0)))
                    .bg(Theme::sidebar_bg())
                    .font_family(Theme::editor_font_family())
                    .border_1()
                    .border_color(Theme::border())
                    .text_size(px(Theme::font_size_ratio(0.87)))
                    .text_color(Theme::text())
                    .child(command),
            );
        }
        body
    }

    /// Filter Keys was given something that reads as a value query: say
    /// that the filter matches key names, and offer the value search.
    fn render_value_query_hint(
        &self,
        kind: crate::redis_key_filters::ValueQueryKind,
        cx: &mut gpui::Context<Self>,
    ) -> gpui::Div {
        let s = Theme::scaled;
        let message = crate::localization::text(match kind {
            crate::redis_key_filters::ValueQueryKind::Json => {
                "explorer.redis_key_dialog.value_query_hint_json"
            }
            crate::redis_key_filters::ValueQueryKind::Search => {
                "explorer.redis_key_dialog.value_query_hint_search"
            }
        });
        let mut hint = div()
            .flex()
            .flex_col()
            .gap(px(s(6.0)))
            .p(px(s(8.0)))
            .rounded(px(s(4.0)))
            .border_1()
            .border_color(Theme::border())
            .bg(Theme::sidebar_bg())
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.84)))
                    .text_color(Theme::warning())
                    .child(message),
            )
            .child(
                div().flex().flex_row().child(
                    Button::new("redis-filter-search-values")
                        .label(crate::localization::text(if self.value_search_running {
                            "explorer.redis_key_dialog.value_search_running"
                        } else {
                            "explorer.redis_key_dialog.value_search_button"
                        }))
                        .disabled(self.value_search_running)
                        .on_click(cx.listener(|this, _, _window, cx| this.search_values(cx))),
                ),
            );
        if let Some(note) = &self.value_search_note {
            hint = hint.child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.80)))
                    .text_color(Theme::text_dim())
                    .child(note.clone()),
            );
        }
        hint
    }

    /// The selected keys a grid action will touch, listed before it runs.
    fn render_selected_keys(&self) -> gpui::Div {
        let s = Theme::scaled;
        let (shown, more) = selected_keys_preview(&self.bulk.keys);
        let mut list = div()
            .flex()
            .flex_col()
            .gap(px(s(2.0)))
            .p(px(s(8.0)))
            .rounded(px(s(4.0)))
            .bg(Theme::sidebar_bg())
            .border_1()
            .border_color(Theme::border());
        for key in shown {
            list = list.child(
                div()
                    .font_family(Theme::editor_font_family())
                    .text_size(px(Theme::font_size_ratio(0.80)))
                    .text_color(Theme::text())
                    .child(key),
            );
        }
        if more > 0 {
            list = list.child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.80)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::format(
                        "explorer.redis_key_dialog.selected_keys_more",
                        &[("count", &more.to_string())],
                    )),
            );
        }
        list
    }

    fn render_bulk_body(&self, cx: &mut gpui::Context<Self>) -> gpui::Div {
        let s = Theme::scaled;
        let prompt = crate::localization::text(match &self.mode {
            RedisKeyDialogMode::BulkDelete { .. } if self.selected_keys => {
                "explorer.redis_key_dialog.selected_delete_explanation"
            }
            RedisKeyDialogMode::BatchTtl { .. } if self.selected_keys => {
                "explorer.redis_key_dialog.selected_ttl_explanation"
            }
            RedisKeyDialogMode::BulkDelete { .. } => {
                "explorer.redis_key_dialog.bulk_delete_explanation"
            }
            RedisKeyDialogMode::BatchTtl { .. } => "explorer.redis_key_dialog.bulk_ttl_explanation",
            RedisKeyDialogMode::ImportCommands { .. } => {
                "explorer.redis_key_dialog.import_explanation"
            }
            _ => unreachable!("single-key modes render their own body"),
        });
        let mut body = div().flex().flex_col().gap(px(s(10.0))).child(
            div()
                .text_size(px(Theme::font_size_ratio(0.87)))
                .text_color(Theme::text_dim())
                .child(prompt),
        );

        let editing = matches!(self.bulk.phase, BulkPhase::Edit | BulkPhase::Checking);
        if self.selected_keys {
            body = body.child(self.render_selected_keys());
        } else if !matches!(self.mode, RedisKeyDialogMode::ImportCommands { .. }) {
            body = body.child(Input::new(&self.input).w_full().disabled(!editing));
        } else {
            body = body.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(8.0)))
                    .child(Input::new(&self.input).w_full().disabled(!editing))
                    .child(
                        crate::theme::accent_pill_button(
                            "redis-import-browse",
                            crate::localization::text("explorer.redis_key_dialog.browse_button"),
                        )
                        .on_click(cx.listener(|this, _, window, cx| {
                            let Some(path) = rfd::FileDialog::new()
                                .add_filter(
                                    crate::localization::text(
                                        "explorer.redis_key_dialog.file_type",
                                    ),
                                    &["redis", "txt"],
                                )
                                .pick_file()
                            else {
                                return;
                            };
                            let text = path.display().to_string();
                            this.input.update(cx, |state, cx| {
                                state.set_value(text, window, cx);
                            });
                            this.error = None;
                            cx.notify();
                        })),
                    ),
            );
        }
        // The TTL and PERSIST controls stay visible through the count: the
        // apply button reads them in the Ready phase.
        let ttl_controls = matches!(
            self.bulk.phase,
            BulkPhase::Edit | BulkPhase::Checking | BulkPhase::Ready
        );
        if matches!(self.mode, RedisKeyDialogMode::BatchTtl { .. }) && ttl_controls {
            body = body
                .child(
                    Input::new(&self.second_input)
                        .w_full()
                        .disabled(self.bulk.ttl_persist),
                )
                .child(
                    Checkbox::new("redis-bulk-persist")
                        .label(crate::localization::text(
                            "explorer.redis_key_dialog.bulk_persist_checkbox",
                        ))
                        .checked(self.bulk.ttl_persist)
                        .on_click(cx.listener(|this, checked: &bool, _window, cx| {
                            this.bulk.ttl_persist = *checked;
                            cx.notify();
                        })),
                );
        }

        // Phase status: what the dry run or file check found, or where the
        // apply stands.
        let status: Option<String> = match self.bulk.phase {
            BulkPhase::Checking => Some(
                if matches!(self.mode, RedisKeyDialogMode::ImportCommands { .. }) {
                    crate::localization::text("explorer.redis_key_dialog.checking_file")
                } else {
                    let seen = self
                        .bulk
                        .dry_run
                        .as_ref()
                        .map(|run| run.keys.len())
                        .unwrap_or(0);
                    let key = if self.bulk.cancel_requested {
                        "explorer.redis_key_dialog.stopping_scan"
                    } else {
                        "explorer.redis_key_dialog.scanning"
                    };
                    crate::localization::format(key, &[("count", &seen.to_string())])
                },
            ),
            BulkPhase::Ready => match &self.mode {
                RedisKeyDialogMode::BulkDelete { .. } | RedisKeyDialogMode::BatchTtl { .. } => {
                    Some(self.bulk.scan_status.clone())
                }
                RedisKeyDialogMode::ImportCommands { .. } => {
                    let review = self.bulk.review.as_ref();
                    Some(crate::localization::format(
                        "explorer.redis_key_dialog.file_check_summary",
                        &[
                            ("commands", &self.bulk.pending_commands.len().to_string()),
                            ("writes", &review.map(|r| r.writes).unwrap_or(0).to_string()),
                            (
                                "flagged",
                                &review.map(|r| r.flagged.len()).unwrap_or(0).to_string(),
                            ),
                        ],
                    ))
                }
                _ => None,
            },
            BulkPhase::Applying => match &self.mode {
                RedisKeyDialogMode::BulkDelete { .. } => Some(crate::localization::format(
                    "explorer.redis_key_dialog.unlinking_progress",
                    &[
                        ("processed", &self.bulk.delete_next.to_string()),
                        ("total", &self.bulk.keys.len().to_string()),
                        ("batches", &self.bulk.delete_batches.to_string()),
                        ("applied", &self.bulk.delete_applied.to_string()),
                    ],
                )),
                RedisKeyDialogMode::BatchTtl { .. } => {
                    let apply = self.bulk.apply.as_ref();
                    Some(crate::localization::format(
                        "explorer.redis_key_dialog.ttl_progress",
                        &[
                            (
                                "processed",
                                &apply.map(|a| a.processed()).unwrap_or(0).to_string(),
                            ),
                            ("total", &apply.map(|a| a.total()).unwrap_or(0).to_string()),
                        ],
                    ))
                }
                RedisKeyDialogMode::ImportCommands { .. } => {
                    let apply = self.bulk.apply.as_ref();
                    Some(crate::localization::format(
                        "explorer.redis_key_dialog.import_progress",
                        &[
                            (
                                "processed",
                                &apply.map(|a| a.processed()).unwrap_or(0).to_string(),
                            ),
                            ("total", &apply.map(|a| a.total()).unwrap_or(0).to_string()),
                        ],
                    ))
                }
                _ => None,
            },
            BulkPhase::Done => {
                if self.bulk.summary.is_empty() {
                    None
                } else {
                    Some(self.bulk.summary.clone())
                }
            }
            _ => None,
        };
        if let Some(text) = status {
            body = body.child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.87)))
                    .text_color(Theme::text())
                    .child(text),
            );
        }
        if self.bulk.phase == BulkPhase::Ready && self.write_block.is_none() {
            body = body.child(self.render_ready_confirmation());
        }
        if self.bulk.phase == BulkPhase::Applying {
            let fraction = match &self.mode {
                RedisKeyDialogMode::BulkDelete { .. } => {
                    if self.bulk.keys.is_empty() {
                        1.0
                    } else {
                        self.bulk.delete_next as f32 / self.bulk.keys.len() as f32
                    }
                }
                _ => {
                    let apply = self.bulk.apply.as_ref();
                    match apply {
                        Some(apply) if apply.total() > 0 => {
                            apply.processed() as f32 / apply.total() as f32
                        }
                        _ => 0.0,
                    }
                }
            };
            body = body.child(
                div()
                    .h(px(s(6.0)))
                    .w_full()
                    .rounded(px(s(3.0)))
                    .bg(Theme::sidebar_bg())
                    .child(
                        div()
                            .h_full()
                            .w(gpui::relative(fraction.clamp(0.0, 1.0)))
                            .rounded(px(s(3.0)))
                            .bg(Theme::accent()),
                    ),
            );
        }
        if self.bulk.phase == BulkPhase::Done {
            body = body.child(self.render_bulk_result());
        }
        body
    }

    /// The Ready phase's typed confirmation: the matched key count for a
    /// bulk delete; for an import, the flagged lines and (when the review
    /// or the connection's safety settings call for it) the phrase.
    fn render_ready_confirmation(&self) -> gpui::Div {
        let s = Theme::scaled;
        let note = |text: String, warn: bool| {
            div()
                .text_size(px(Theme::font_size_ratio(0.87)))
                .text_color(if warn {
                    Theme::warning()
                } else {
                    Theme::text_dim()
                })
                .child(text)
        };
        let mut out = div().flex().flex_col().gap(px(s(6.0)));
        match &self.mode {
            RedisKeyDialogMode::BulkDelete { .. } if !self.bulk.keys.is_empty() => {
                let count = self.bulk.keys.len().to_string();
                out = out
                    .child(note(
                        crate::localization::format(
                            "explorer.redis_key_dialog.delete_confirmation",
                            &[("count", &count)],
                        ),
                        true,
                    ))
                    .child(Input::new(&self.second_input).w_full());
            }
            RedisKeyDialogMode::ImportCommands { .. } => {
                if let Some(review) = &self.bulk.review
                    && !review.flagged.is_empty()
                {
                    let mut list = div()
                        .id("redis-import-flagged")
                        .flex()
                        .flex_col()
                        .gap(px(s(2.0)))
                        .p(px(s(8.0)))
                        .rounded(px(s(4.0)))
                        .bg(Theme::sidebar_bg())
                        .border_1()
                        .border_color(Theme::border())
                        .max_h(px(s(140.0)))
                        .overflow_y_scroll()
                        .child(note(
                            crate::localization::format(
                                "explorer.redis_key_dialog.flagged_commands",
                                &[("count", &review.flagged.len().to_string())],
                            ),
                            true,
                        ));
                    for flagged in review.flagged.iter().take(FLAGGED_DISPLAY_CAP) {
                        list = list.child(
                            div()
                                .font_family(Theme::editor_font_family())
                                .text_size(px(Theme::font_size_ratio(0.80)))
                                .text_color(Theme::text())
                                .child(crate::localization::format(
                                    "explorer.redis_key_dialog.flagged_line",
                                    &[
                                        ("line", &flagged.line.to_string()),
                                        ("command", &flagged.command),
                                    ],
                                )),
                        );
                    }
                    if review.flagged.len() > FLAGGED_DISPLAY_CAP {
                        list = list.child(note(
                            crate::localization::format(
                                "explorer.redis_key_dialog.flagged_more_not_shown",
                                &[(
                                    "count",
                                    &(review.flagged.len() - FLAGGED_DISPLAY_CAP).to_string(),
                                )],
                            ),
                            false,
                        ));
                    }
                    out = out.child(list);
                }
                if self.bulk.confirm_required {
                    out = out
                        .child(note(
                            crate::localization::format(
                                "explorer.redis_key_dialog.import_confirmation",
                                &[("phrase", redis_bulk::IMPORT_CONFIRM_PHRASE)],
                            ),
                            true,
                        ))
                        .child(Input::new(&self.second_input).w_full());
                }
            }
            _ => {}
        }
        out
    }

    /// The Done phase: the summary plus the collected failures, capped with
    /// an honest "+N more" note.
    fn render_bulk_result(&self) -> gpui::Div {
        let s = Theme::scaled;
        let failures: Vec<String> = match &self.mode {
            RedisKeyDialogMode::BulkDelete { .. } => self.bulk.delete_failures.clone(),
            _ => self
                .bulk
                .apply
                .as_ref()
                .map(|apply| {
                    apply
                        .outcome
                        .failed
                        .iter()
                        .map(|f| {
                            if f.line > 0 {
                                crate::localization::format(
                                    "explorer.redis_key_dialog.failure_line",
                                    &[
                                        ("line", &f.line.to_string()),
                                        ("command", &f.command),
                                        ("error", &f.error),
                                    ],
                                )
                            } else if f.command.is_empty() {
                                f.error.clone()
                            } else {
                                crate::localization::format(
                                    "explorer.redis_key_dialog.failure_command",
                                    &[("command", &f.command), ("error", &f.error)],
                                )
                            }
                        })
                        .collect()
                })
                .unwrap_or_default(),
        };
        let mut result = div().flex().flex_col().gap(px(s(4.0)));
        if !failures.is_empty() {
            let shown = failures.len().min(FAILURE_DISPLAY_CAP);
            let mut list = div()
                .id("redis-bulk-failures")
                .flex()
                .flex_col()
                .gap(px(s(2.0)))
                .p(px(s(8.0)))
                .rounded(px(s(4.0)))
                .bg(Theme::sidebar_bg())
                .border_1()
                .border_color(Theme::border())
                .max_h(px(s(160.0)))
                .overflow_y_scroll()
                .child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.87)))
                        .text_color(Theme::warning())
                        .child(crate::localization::format(
                            "explorer.redis_key_dialog.failures_heading",
                            &[("count", &failures.len().to_string())],
                        )),
                );
            for failure in failures.into_iter().take(shown) {
                list = list.child(
                    div()
                        .font_family(Theme::editor_font_family())
                        .text_size(px(Theme::font_size_ratio(0.80)))
                        .text_color(Theme::text())
                        .child(failure),
                );
            }
            if failures_len_note(shown, self.bulk_failure_count()) {
                list = list.child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.80)))
                        .text_color(Theme::text_dim())
                        .child(crate::localization::format(
                            "explorer.redis_key_dialog.failures_more_not_shown",
                            &[("count", &(self.bulk_failure_count() - shown).to_string())],
                        )),
                );
            }
            result = result.child(list);
        } else {
            result = result.child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.87)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text(
                        "explorer.redis_key_dialog.no_failures",
                    )),
            );
        }
        result
    }

    fn bulk_failure_count(&self) -> usize {
        match &self.mode {
            RedisKeyDialogMode::BulkDelete { .. } => self.bulk.delete_failures.len(),
            _ => self
                .bulk
                .apply
                .as_ref()
                .map(|a| a.outcome.failed.len())
                .unwrap_or(0),
        }
    }
}

fn failures_len_note(shown: usize, total: usize) -> bool {
    shown < total
}

impl Render for RedisKeyDialog {
    fn render(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let titled = |key, name: &String| crate::localization::format(key, &[("name", name)]);
        let (title, confirm_label, danger) = match &self.mode {
            RedisKeyDialogMode::Filter { .. } => (
                crate::localization::text("explorer.redis_key_dialog.filter_title"),
                crate::localization::text("explorer.redis_key_dialog.filter_apply"),
                false,
            ),
            RedisKeyDialogMode::SetTtl { key, .. } => (
                titled("explorer.redis_key_dialog.ttl_title", key),
                crate::localization::text("explorer.redis_key_dialog.ttl_apply"),
                false,
            ),
            RedisKeyDialogMode::Persist { key, .. } => (
                titled("explorer.redis_key_dialog.persist_title", key),
                crate::localization::text("explorer.redis_key_dialog.persist_apply"),
                false,
            ),
            RedisKeyDialogMode::Delete { key, .. } => (
                titled("explorer.redis_key_dialog.delete_title", key),
                crate::localization::text("explorer.redis_key_dialog.delete_apply"),
                true,
            ),
            RedisKeyDialogMode::BulkDelete { .. } => (
                crate::localization::text(if self.selected_keys {
                    "explorer.redis_key_dialog.selected_delete_title"
                } else {
                    "explorer.redis_key_dialog.bulk_delete_title"
                }),
                crate::localization::text(match self.bulk.phase {
                    BulkPhase::Edit | BulkPhase::Checking => {
                        "explorer.redis_key_dialog.bulk_delete_count"
                    }
                    BulkPhase::Ready => "explorer.redis_key_dialog.bulk_delete_apply",
                    BulkPhase::Applying => "explorer.redis_key_dialog.bulk_delete_running",
                    BulkPhase::Done => "explorer.redis_key_dialog.bulk_delete_done",
                }),
                self.bulk.phase == BulkPhase::Ready,
            ),
            RedisKeyDialogMode::BatchTtl { .. } => (
                crate::localization::text(if self.selected_keys {
                    "explorer.redis_key_dialog.selected_ttl_title"
                } else {
                    "explorer.redis_key_dialog.bulk_ttl_title"
                }),
                crate::localization::text(match self.bulk.phase {
                    BulkPhase::Edit | BulkPhase::Checking => {
                        "explorer.redis_key_dialog.bulk_ttl_count"
                    }
                    BulkPhase::Ready => {
                        if self.bulk.ttl_persist {
                            "explorer.redis_key_dialog.bulk_ttl_persist_apply"
                        } else {
                            "explorer.redis_key_dialog.bulk_ttl_apply"
                        }
                    }
                    BulkPhase::Applying => "explorer.redis_key_dialog.bulk_ttl_running",
                    BulkPhase::Done => "explorer.redis_key_dialog.bulk_ttl_done",
                }),
                false,
            ),
            RedisKeyDialogMode::ImportCommands { .. } => (
                crate::localization::text("explorer.redis_key_dialog.import_title"),
                crate::localization::text(match self.bulk.phase {
                    BulkPhase::Edit | BulkPhase::Checking => {
                        "explorer.redis_key_dialog.import_check"
                    }
                    BulkPhase::Ready => "explorer.redis_key_dialog.import_apply",
                    BulkPhase::Applying => "explorer.redis_key_dialog.import_running",
                    BulkPhase::Done => "explorer.redis_key_dialog.import_done",
                }),
                false,
            ),
        };
        // Every key dialog names the database it acts on (with its name).
        let title = crate::localization::format(
            "explorer.redis_key_dialog.title_with_database",
            &[("title", &title), ("database", &self.database_label())],
        );
        let mut body = if self.mode.is_bulk() {
            self.render_bulk_body(cx)
        } else {
            self.render_single_body(cx)
        };
        if let Some(reason) = &self.write_block {
            body = body.child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.87)))
                    .text_color(Theme::warning())
                    .child(reason.clone()),
            );
        }
        if let Some(error) = &self.error {
            body = body.child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.80)))
                    .text_color(Theme::error())
                    .child(error.clone()),
            );
        }
        let focus_handle = self.focus_handle.clone();
        let confirm_allowed = self.confirm_allowed();
        let applying = self.bulk.phase == BulkPhase::Applying;
        // The dry run's pages can be stopped too; the import's file check is
        // one local read and keeps a plain Cancel.
        let stoppable = applying || self.scanning();
        Dialog::new("redis-key-dialog", |this: &mut Self| &mut this.frame)
            .title(title)
            .easy_close(!applying)
            .easy_close_when_dirty(!applying)
            .key_context("RedisKeyDialog")
            .track_focus(&focus_handle)
            .on_close(|this, _window, cx| this.close(cx))
            .child(body)
            .button(
                Button::new("redis-key-dialog-cancel")
                    .label(crate::localization::text(if stoppable {
                        "explorer.redis_key_dialog.stop_button"
                    } else {
                        "explorer.redis_key_dialog.cancel_button"
                    }))
                    .disabled(stoppable && self.bulk.cancel_requested)
                    .on_click(cx.listener(|this, _, _window, cx| {
                        if this.bulk.phase == BulkPhase::Applying || this.scanning() {
                            this.bulk.cancel_requested = true;
                            cx.notify();
                        } else {
                            this.close(cx);
                        }
                    })),
            )
            .button({
                let button = Button::new("redis-key-dialog-confirm")
                    .label(confirm_label)
                    .disabled(!confirm_allowed || applying);
                let button = if danger {
                    button.danger()
                } else {
                    button.primary()
                };
                button.on_click(cx.listener(|this, _, _window, cx| this.confirm(cx)))
            })
            .build(self, window, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_keys_preview_lists_the_first_keys_binary_safe() {
        let keys: Vec<Vec<u8>> = (0..10)
            .map(|i| format!("user:{i}").into_bytes())
            .chain([vec![0xff, b' ', b'x']])
            .collect();
        let (shown, more) = selected_keys_preview(&keys);
        assert_eq!(shown.len(), SELECTED_PREVIEW_CAP);
        assert_eq!(shown[0], "user:0");
        assert_eq!(more, 3);
        let (shown, more) = selected_keys_preview(&[vec![0xff, b' ', b'x']]);
        assert_eq!(shown, vec!["\"\\xff x\""]);
        assert_eq!(more, 0);
    }

    #[test]
    fn bulk_delete_needs_the_exact_count_typed_back() {
        assert!(bulk_delete_confirmed(1234, "1234"));
        assert!(bulk_delete_confirmed(1234, " 1234 "));
        assert!(!bulk_delete_confirmed(1234, ""));
        assert!(!bulk_delete_confirmed(1234, "123"));
        assert!(!bulk_delete_confirmed(1234, "1,234"));
        assert!(!bulk_delete_confirmed(0, "0"), "nothing to delete");
    }

    /// Stop during the dry run leaves the keys seen so far, no further
    /// page, and a count labelled partial — the apply would touch exactly
    /// those keys and the dialog says so.
    #[test]
    fn stopping_a_dry_run_mid_scan_leaves_a_partial_count_labelled_partial() {
        let page = |names: &[&str], cursor: &str| -> Vec<Vec<CellValue>> {
            names
                .iter()
                .map(|name| {
                    vec![
                        CellValue::String((*name).to_string()),
                        CellValue::String("string".into()),
                        CellValue::Null,
                        CellValue::Bool(false),
                        CellValue::String(cursor.to_string()),
                    ]
                })
                .collect()
        };
        let mut run = DryRun::new("session:*", redis_bulk::BULK_KEY_LIMIT);
        run.absorb(&page(&["session:1", "session:2", "session:3"], "512"));
        assert!(run.next_command().is_some(), "the walk would continue");
        // The user presses Stop between pages.
        run.stop();
        assert_eq!(run.next_command(), None, "no further page is issued");
        assert!(run.is_partial() && run.stopped && !run.complete);
        assert_eq!(run.keys.len(), 3, "the partial count is what was seen");
        let status = run.status();
        assert!(status.starts_with("3 keys match"), "{status}");
        assert!(status.contains("partial"), "{status}");
        assert!(status.contains("stopped"), "{status}");
        // A completed walk is never partial, and Stop after it is a no-op.
        let mut done = DryRun::new("session:*", 10);
        done.absorb(&[vec![
            CellValue::String("session:1".into()),
            CellValue::String("string".into()),
            CellValue::Null,
            CellValue::Bool(true),
            CellValue::String("0".into()),
        ]]);
        done.stop();
        assert!(!done.stopped && !done.is_partial());
        assert_eq!(done.status(), "1 keys match");
    }

    #[test]
    fn import_file_refuses_non_utf8_lines_and_keeps_escaped_binary() {
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("bad.redis");
        std::fs::write(&bad, b"SET a 1\nSET b \xff\n").unwrap();
        let error = read_import_file(bad.to_str().unwrap()).unwrap_err();
        assert!(error.contains("line 2"), "{error}");

        let good = dir.path().join("good.redis");
        std::fs::write(&good, "SET a 1\r\nSET b \"\\xff\"\r\n").unwrap();
        let (_, commands) = read_import_file(good.to_str().unwrap()).unwrap();
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[1].args[2], vec![0xff]);

        let unparsable = dir.path().join("quote.redis");
        std::fs::write(&unparsable, "SET a \"open\n").unwrap();
        assert!(read_import_file(unparsable.to_str().unwrap()).is_err());
    }
}
