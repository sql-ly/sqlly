use gpui::{
    App, Bounds, Context, DismissEvent, Element, ElementId, Entity, EntityInputHandler,
    EventEmitter, FocusHandle, Focusable, Font, FontWeight, GlobalElementId, Hsla,
    InspectorElementId, IntoElement, KeyDownEvent, LayoutId, ModifiersChangedEvent, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, Point, Render, ScrollHandle, ShapedLine, SharedString,
    Styled, Subscription, TextRun, UTF16Selection, Window, actions, anchored, deferred, div, point,
    prelude::*, px,
};
use sqlly_intel::intellisense::{
    AcceptHistory, CompletionCatalog, CompletionItem, CompletionKind, CompletionSource, HoverInfo,
    HoverParameter, RoutineSignature, SignatureHelp,
};
use sqlly_services::{
    CompletionRequest, IntelliSenseRuleConfig, IntelliSenseTriggerKind, ProcedureInfo,
    RankingHints, Services, ValueContext, matches_partial,
};
#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;
use std::{fmt::Write as _, ops::Range};
use unicode_segmentation::*;
#[cfg(target_arch = "wasm32")]
use web_time::Instant;

use crate::find_replace::{self, FindQuery};
use crate::preferences::{AliasGeneration, DdlNavigationPreferences, EditorPreferences};
use crate::theme::Theme;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::{PopupMenu, PopupMenuItem};
use gpui_component::{Selectable as _, Sizable as _};

actions!(
    sql_editor,
    [
        Backspace,
        Delete,
        Dismiss,
        Left,
        Right,
        Up,
        Down,
        Home,
        End,
        Enter,
        ShiftEnter,
        SelectAll,
        SelectLeft,
        SelectRight,
        SelectUp,
        SelectDown,
        SelectHome,
        SelectEnd,
        Tab,
        ShiftTab,
        ExecuteQuery,
        Cancel,
        OpenContextMenu,
        TriggerCompletions,
        TriggerValueLookup,
        RefreshDates,
        TriggerAiCompletion,
        Fold,
        Unfold,
        FoldAll,
        UnfoldAll,
        CaptureIntelliSenseFeedback,
        InsertDistinctValuesAsInList,
        PasteAsInList,
        ConvertDelimitedList,
        Validate,
        FormatSql,
        FormatDocument,
        MinifySql,
        PageDown,
        PageUp,
        SelectPageDown,
        SelectPageUp,
        ToggleLineComment,
        AddLineComment,
        RemoveLineComment,
        MoveWordLeft,
        MoveWordRight,
        SelectWordLeft,
        SelectWordRight,
        DeleteWordBackward,
        DeleteWordForward,
        MoveHumpLeft,
        MoveHumpRight,
        SelectHumpLeft,
        SelectHumpRight,
        MoveToBeginningOfDocument,
        MoveToEndOfDocument,
        SelectToBeginningOfDocument,
        SelectToEndOfDocument,
        DeleteToBeginningOfLine,
        DeleteToEndOfLine,
        Copy,
        Cut,
        Paste,
        Undo,
        Redo,
        Find,
        Replace,
        FindNext,
        FindPrev,
        AddCursorForNextOccurrence,
        SelectAllOccurrences,
        AddCursorAbove,
        AddCursorBelow,
        ColumnSelectUp,
        ColumnSelectDown,
        GoToLine,
        MoveLineUp,
        MoveLineDown,
        DuplicateLine,
        JoinLines,
        GoToNextError,
        QualifyIdentifier,
    ]
);

/// The literal a value completion inserts, in the connection's own string
/// rules: a stored value ending in `\` must not escape the closing quote on
/// MySQL or ClickHouse (the value is data from the database, so it is
/// treated as hostile). SQL Server keeps a plain `'…'` — doubled quotes are
/// its only escape, and an `N'…'` literal against a `varchar` column can cost
/// an index seek — unless the value has characters only `N'…'` preserves.
fn value_completion_literal(
    db: sqlly_lang::sqlmodel::DatabaseType,
    value: &str,
    operator: sqlly_services::ValueOperator,
) -> String {
    use sqlly_lang::sqlmodel::Dialect;
    if db.family() == Dialect::SqlServer && value.is_ascii() {
        sqlly_services::intellisense::format_sql_literal(value, operator)
    } else {
        sqlly_services::intellisense::value_lookup::format_sql_literal_for(db, value, operator)
    }
}

/// Maximum number of completion items shown at once in the popup. Used by both
/// `clamp_completion_scroll` (to decide when to scroll) and the render path
/// (to decide how many rows to paint). Keep this single source of truth so the
/// clamp math and the renderer never drift apart.
const COMPLETION_MAX_VISIBLE: usize = 8;
/// Line-number digits the gutter always makes room for.
const GUTTER_MIN_DIGITS: usize = 4;
/// Line-number digits the gutter grows to fit (a 9,999,999-line script).
const GUTTER_MAX_DIGITS: usize = 7;

/// How long the pointer must rest on a word before its hover card appears —
/// long enough that reading or moving across code never flashes cards.
const HOVER_CARD_DWELL: std::time::Duration = std::time::Duration::from_secs(3);

pub fn register_keybindings(cx: &mut App) {
    crate::editor_keybindings::register_keybindings(cx);
}

mod bars;
mod date_helper;
mod dialogs;
mod element;
pub(crate) mod group_by_lint;
mod identity_lint;
mod info_cards;
mod input;
mod insert_hints;
#[cfg(test)]
pub(crate) mod perf_fixture;
#[cfg(test)]
mod perf_tests;
mod render;
mod shaping;
mod sql_text;
pub mod statements;
#[cfg(test)]
mod tests;
mod text;
mod types;
mod vim;

pub use element::EditorElement;
// Previously reachable as `crate::editor::{EditorPrepaintState, PaintLine}`;
// only `EditorElement`'s own machinery uses them today.
#[allow(unused_imports)]
pub use element::{EditorPrepaintState, PaintLine};
pub use types::{DiagnosticFix, DiagnosticSeverity, DiagnosticSource, ValidationDiagnostic};

use bars::*;
use dialogs::*;
use shaping::*;
use sql_text::*;
use text::*;
use types::*;
use vim::*;

/// One occurrence-highlight cache entry: `(selection start, selection end,
/// content revision, ranges)`.
type OccurrenceCacheEntry = (usize, usize, u64, std::rc::Rc<Vec<Range<usize>>>);

pub struct SqlEditor {
    focus_handle: FocusHandle,
    content: SharedString,
    cursor: usize,
    selection_anchor: Option<usize>,
    last_layout: Vec<ShapedLine>,
    /// First absolute line index stored in `last_layout`. Paint and hit-test
    /// paths translate absolute→relative via this offset.
    last_layout_first_line: usize,
    /// Per-line tab-expansion maps matching `last_layout` (the shaped lines
    /// are the tab-expanded *display* strings; these convert byte offsets
    /// between content and display). `Rc` so paint/mouse paths share one
    /// allocation per frame.
    last_tab_maps: std::rc::Rc<Vec<crate::tab_map::TabMap>>,
    last_bounds: Option<Bounds<Pixels>>,
    /// The scroll viewport size and max offset `render` sized the scrollbar
    /// thumbs from. They come from the previous layout, so prepaint compares
    /// them with this frame's and asks for one more frame when they moved.
    scrollbar_geometry: (gpui::Size<Pixels>, gpui::Point<Pixels>),
    scroll_handle: ScrollHandle,
    /// Monotonically incremented on every content mutation so future
    /// optimizations (shaped-line cache, etc.) can detect staleness. Values
    /// are drawn from `revision_counter`, which two same-buffer views share,
    /// so a given revision number always names one specific buffer state and
    /// the shared buffer-derived caches below can never serve a stale entry.
    content_revision: u64,
    /// Source of `content_revision` values. Editor-private normally; shared
    /// (one counter for both views) in a same-buffer split.
    revision_counter: std::rc::Rc<std::cell::Cell<u64>>,
    /// Bumped on every *local* shared-document mutation (content, undo/redo
    /// stacks, diagnostics). The peer view of a same-buffer split adopts this
    /// editor's document state whenever the epoch moves; adoption itself
    /// never bumps it, which is what keeps the sync loop-free.
    doc_epoch: u64,
    /// Same-buffer split wiring: the other view showing this document, plus
    /// the subscriptions that keep the two in sync. `None` outside a split.
    shared_peer: Option<SharedDocumentPeer>,
    /// Lazily (re)built newline index, keyed by `content_revision`. All
    /// line/column lookups go through `line_index()`; nothing may scan the
    /// document per call — that was an O(n)-per-keystroke/frame hot spot.
    /// `Rc` so a same-buffer split's two views share one index per edit
    /// instead of each rebuilding it (buffer-derived caches stay per-document).
    line_index_cache: SharedLineIndexCache,
    /// Per-line block-comment depth, keyed by `(content_revision, dialect)`.
    /// The renderer shapes one line at a time, so this is what lets a
    /// `/* … */` spanning several lines stay a comment past its first line.
    comment_state_cache: SharedCommentStateCache,
    /// Widest line in display columns (tabs expanded, insert-hint labels
    /// included), keyed by `(content_revision, tab_width,
    /// insert_hints_version)`; feeds `request_layout` without a
    /// full-document scan per frame. Shared across same-buffer views.
    max_display_cols_cache: std::rc::Rc<std::cell::RefCell<Option<MaxDisplayColsKey>>>,
    /// Per-line-text shaping cache: classifying + shaping a line is the
    /// dominant prepaint cost, and unchanged lines re-shape identically every
    /// frame while scrolling or typing. Keyed by the line's text; cleared
    /// whenever the probe (font, size, tab width, theme colors) changes.
    /// Shared across same-buffer views, so the same line never shapes twice.
    shape_cache: std::rc::Rc<std::cell::RefCell<ShapeCache>>,
    /// Code-folding state (discovered regions + collapsed set + display map),
    /// reconciled lazily against `content_revision` on access. Interior
    /// mutability so read paths (paint, hit tests) can reconcile too.
    folds: std::cell::RefCell<(Option<u64>, crate::folding::EditorFolds)>,
    /// Dim `column:` labels for INSERT … VALUES, keyed to `content_revision`.
    insert_hints: std::rc::Rc<Vec<insert_hints::InsertHint>>,
    insert_hints_revision: Option<u64>,
    /// Bumped on every `insert_hints` assignment. Hints can change without a
    /// content edit (catalog push, preference toggle), so width caches keyed
    /// only by `content_revision` would go stale without this.
    insert_hints_version: u64,
    /// Generation counter for debounced insert-hint recomputes.
    insert_hints_generation: u64,
    /// Generation counter for debounced auto-completion tasks.
    completion_generation: u64,
    /// Occurrence-highlight cache: the ranges painted for the current
    /// selection, keyed by `(selection start, selection end,
    /// content_revision)` so a paint pass never rescans the buffer for a
    /// state it already computed. Interior mutability because the paint
    /// path only holds `&self`. `None` when nothing is highlightable.
    occurrence_cache: std::cell::RefCell<Option<OccurrenceCacheEntry>>,
    #[allow(dead_code)]
    is_focused: bool,
    is_selecting: bool,
    /// Option/Alt drag origin for a rectangular selection, expressed in
    /// visible display-line and tab-expanded column coordinates.
    rectangular_anchor: Option<(usize, usize)>,
    /// Current rectangle corner for Shift+Alt+Up/Down, so repeated presses walk one display line at a time.
    rectangular_target: Option<(usize, usize)>,
    /// Leftover Cmd/Ctrl-wheel delta in line units so trackpads do not fire a zoom step per pixel.
    zoom_wheel_accumulator: f32,
    /// Sticky column for vertical caret movement, in *chars* (never bytes —
    /// byte columns can land inside a multibyte UTF-8 sequence on another
    /// line). Set by up/down/page moves, cleared by any horizontal move,
    /// click, or edit, so moving through a short line remembers the column.
    goal_column: Option<usize>,
    /// Secondary carets for multi-cursor editing (the primary stays in
    /// `cursor`/`selection_anchor`). Always sorted, deduped, never on the
    /// primary, capped at [`MAX_EXTRA_CURSORS`]. Cleared by any action that
    /// is not on the multi-cursor allowlist (typing, backspace/delete,
    /// paste, the add-cursor commands, undo/redo).
    extra_cursors: Vec<ExtraCursor>,
    /// Caret offset of the most recently added extra cursor — the reference
    /// point `AddCursorAbove`/`AddCursorBelow` extend from. `None` (or an
    /// offset no longer backed by an extra) falls back to the primary.
    last_added_extra: Option<usize>,
    /// Tab-stop walk after expanding a snippet. `None` when no session is live.
    snippet_session: Option<SnippetSession>,
    /// `Some` while Vim mode is on; view state, not shared with a split peer.
    vim: Option<VimState>,
    /// Byte range of in-progress IME composition text (CJK preedit, dead-key
    /// accents). While `Some`, the span renders underlined, auto-completion
    /// is suppressed, and the whole composition session forms one undo entry.
    marked_range: Option<Range<usize>>,
    font: Font,
    font_size: Pixels,
    line_height: Pixels,
    line_number_width: Pixels,
    services: Option<Services>,
    completion_catalog: std::sync::Arc<CompletionCatalog>,
    /// [`Self::semantic_names`] cache: the catalog it was built from (held so
    /// its address cannot be reused) and the names.
    #[allow(clippy::type_complexity)]
    semantic_names: std::cell::RefCell<
        Option<(
            std::sync::Arc<CompletionCatalog>,
            std::sync::Arc<crate::sql_syntax::SemanticNames>,
        )>,
    >,
    schema_snapshot: std::sync::Arc<sqlly_lang::sqlmodel::ModelSnapshot>,
    completion_procedures: std::sync::Arc<Vec<ProcedureInfo>>,
    /// Command IntelliSense for Redis tabs: the vocabulary and scanned key
    /// names the app pushes (bundled vocabulary, no keys, until it does).
    redis_completion: std::sync::Arc<crate::app::RedisCompletionContext>,
    completion_rules: std::sync::Arc<IntelliSenseRuleConfig>,
    accept_history: std::sync::Arc<std::sync::Mutex<AcceptHistory>>,
    completions: std::rc::Rc<Vec<CompletionItem>>,
    completion_index: usize,
    completion_scroll_offset: usize,
    completion_visible: bool,
    /// The popup was opened by the manual trigger (Ctrl+Space). Keeps it open —
    /// showing "No suggestions" — across subsequent automatic passes that come
    /// back empty, so the user can keep typing into a live popup until they
    /// dismiss or accept it.
    completion_manual_open: bool,
    /// When the popup last went hidden → shown; drives its ~120ms fade-in.
    /// `None` reads as fully opaque, so state set directly in tests (and any
    /// path that never transitions) paints exactly as before. Updating the
    /// list while the popup is already up (typing filters, value lookups
    /// merging in) keeps the original timestamp — the fade plays on arrival,
    /// not on every keystroke.
    completion_shown_at: Option<Instant>,
    completion_replace_start: usize,
    /// Revision and caret produced by accepting a completion, whose own edit
    /// must not immediately reopen the popup.
    accepted_completion_at: Option<(u64, usize)>,
    /// True once the user has explicitly highlighted a row with Up/Down (or a
    /// click). The popup opens with NO row selected: until this latches, Tab
    /// still accepts the top-ranked item but Enter keeps its default meaning
    /// (insert a newline) — an unsolicited popup must never steal a line
    /// break the user typed. Reset whenever the list is rebuilt (typing
    /// re-filters, a new automatic pass lands), so a stale highlight can't
    /// make Enter accept an item the user chose from a previous list.
    completion_user_navigated: bool,
    /// Latched value-lookup context. `Some` while the popup is showing
    /// distinct column values (triggered by `TriggerValueLookup`). Cleared
    /// when the cursor leaves the value position or the popup is closed.
    active_value_lookup: Option<ValueContext>,
    /// Monotonic token guarding against stale async value-lookup results.
    /// Incremented on every new value-lookup request; `apply_value_lookup_results`
    /// ignores results whose token doesn't match.
    value_lookup_token: u64,
    /// Base completion items captured at value-lookup trigger time. Merged
    /// below the value items so the popup still shows ordinary completions.
    value_lookup_base_items: Vec<CompletionItem>,
    /// Raw values from the last fetch, kept unfiltered so typing after the
    /// popup opens re-filters locally without another DB query. A foreign-key
    /// id column's entries carry the referenced row's readable label.
    value_lookup_values: Vec<sqlly_services::ValueLookupEntry>,
    /// The foreign-key pivot the latched value lookup follows, when its
    /// column is an FK id column the model knows (labels the popup's detail).
    value_lookup_fk: Option<sqlly_intel::modelengine::intellisense::FkValuePivot>,
    /// True while an async value-lookup fetch is in flight.
    value_lookup_pending: bool,
    /// The partial the stored values were fetched for (`""` for the
    /// unfiltered trigger fetch) and whether that fetch hit the row cap. A
    /// capped fetch is only a sample: once the typed text stops matching it
    /// the popup re-queries with the text as a server-side filter rather
    /// than showing nothing (feedback ifb_2fc6).
    value_lookup_fetched_partial: String,
    value_lookup_truncated: bool,
    diagnostics: std::rc::Rc<Vec<ValidationDiagnostic>>,
    /// Line → diagnostic indices, rebuilt when diagnostics are set.
    diag_by_line: std::rc::Rc<std::collections::HashMap<usize, Vec<usize>>>,
    /// Inclusive buffer-line span per diagnostic, rebuilt when diagnostics are set.
    diag_spans: std::rc::Rc<Vec<(usize, usize)>>,
    /// Cached `"{n}"` labels so paint does not `format!` line numbers per frame.
    line_number_labels: std::cell::RefCell<Vec<SharedString>>,
    hovered_diagnostic: Option<usize>,
    /// Byte range `(start, end)` of the `-- sqlly …` formatting directive the
    /// mouse is hovering, if any. Drives the info popup that explains the
    /// directive and links to the full options reference.
    hovered_sqlly_directive: Option<(usize, usize)>,
    /// Resolved hover card for the identifier under the pointer, with its
    /// source span. Set by the dwell timer, cleared on move-out/edit/scroll.
    hover_info: Option<(HoverInfo, (usize, usize))>,
    /// Header line of the collapsed fold whose `⋯` chip the pointer is over.
    fold_preview: Option<usize>,
    /// Pending dwell task; replaced on every mouse move so only a 350 ms
    /// pause fires a resolve.
    hover_dwell_task: Option<gpui::Task<()>>,
    /// Popup rect stamped at paint time (same role as `directive_hint_bounds`).
    hover_card_bounds: std::cell::Cell<Option<Bounds<Pixels>>>,
    /// Painted `⋯` chip rects for the current frame, keyed by fold header line.
    fold_chip_bounds: std::cell::RefCell<Vec<(usize, Bounds<Pixels>)>>,
    /// The natural-date helper opened by Ctrl+Shift+Space in a date
    /// comparison (see `date_helper.rs`).
    date_helper: Option<date_helper::DateHelper>,
    /// `--sqlly date:` marked predicates, keyed by content revision.
    #[allow(clippy::type_complexity)]
    date_marker_cache: std::cell::RefCell<(
        Option<u64>,
        std::rc::Rc<Vec<sqlly_intel::natural_date::MarkedPredicate>>,
    )>,
    /// Window bounds of the painted ↻ date chips, re-stamped every paint.
    date_chip_bounds: std::cell::RefCell<Vec<(Bounds<Pixels>, date_helper::DateChip)>>,
    /// Signature help for the call the caret is inside, if any.
    signature_help: Option<SignatureHelp>,
    /// `open_paren` the user pressed Esc on, so help stays hidden for that call.
    signature_dismissed_at_paren: Option<usize>,
    /// Live procedure signatures, rebuilt when `completion_procedures` changes.
    routine_signatures: std::sync::Arc<Vec<RoutineSignature>>,
    editor_prefs: EditorPreferences,
    /// DDL navigation gesture config (modifier-click → go to definition).
    /// Pushed by the app at tab creation and on preferences changes.
    ddl_nav_prefs: DdlNavigationPreferences,
    /// Active connection engine for literal rendering (paste-as-IN-list).
    database_type: sqlly_lang::sqlmodel::DatabaseType,
    /// FIM (fill-in-the-middle) ghost-text suggestion. `Some(text)` when an
    /// inline AI completion is visible at the cursor; the ghost text is
    /// rendered in a dim color and accepted via Tab. Mirrors the Swift
    /// `Editor.showFIMSuggestion(text)` / `hideFIMSuggestion()` flow.
    fim_suggestion: Option<String>,
    fim_visible: bool,
    needs_focus: bool,
    undo_stack: std::collections::VecDeque<EditorSnapshot>,
    redo_stack: Vec<EditorSnapshot>,
    /// Tests: force "format as you go" on/off for this editor instead of
    /// reading the (process-wide, cached) formatter settings.
    #[cfg(test)]
    pub(super) format_as_you_go_override: Option<bool>,
    /// Open coalescing group for single-char edits: consecutive typing (or
    /// backspaces/deletes) at contiguous offsets share one undo snapshot, so
    /// Ctrl+Z rewinds a run of typing, not one character. `None` = next edit
    /// starts a fresh group.
    undo_group: Option<UndoGroupState>,
    /// Window-space mouse position from the last mouse-move, so pressing or
    /// releasing the DDL-navigation modifier can re-evaluate the link-hover
    /// affordance without waiting for the mouse to move again.
    last_mouse_position: Option<Point<Pixels>>,
    /// Byte span of the navigable symbol under the mouse while the DDL
    /// navigation modifier is held. Rendered like a hyperlink (accent color +
    /// underline) with a pointing-hand cursor, signalling that a click will
    /// go to the definition.
    link_hover_range: Option<(usize, usize)>,
    context_menu: Option<EditorContextMenu>,
    format_dialog: Option<ColumnFormatDialog>,
    formatter_picker: Option<Entity<crate::formatter_picker_dialog::FormatterPickerDialog>>,
    formatter_picker_sub: Option<Subscription>,
    pending_format_sample: Option<sqlly_core::CellValue>,
    convert_dialog: Option<ConvertListDialog>,
    /// Frames for the editor's unified-chrome dialogs (`crate::dialog`),
    /// keyed by dialog id since their state structs are `Option`al.
    dialog_frames: crate::dialog::DialogFrames,
    /// Find/replace bar state, created on first open and reused after.
    find_bar: Option<FindBarState>,
    /// Go to Line bar state, created on first open and reused after.
    goto_line_bar: Option<GoToLineBarState>,
    /// Per-query Automagic comma overrides. `None` follows preferences;
    /// `Some` is changed from the editor context menu and never persisted.
    automagic_comma_overrides: [Option<bool>; 3],
    /// Per-query override for WHERE/ON connector repair; `None` follows the
    /// preference.
    automagic_connector_override: Option<bool>,
    /// Window bounds of the "to improve this, press F2" chip, stamped at
    /// paint time so `handle_mouse_down` can hit-test clicks on it. `None`
    /// whenever the completion popup (and with it the chip) is off screen.
    feedback_hint_bounds: std::cell::Cell<Option<Bounds<Pixels>>>,
    /// Window bounds of the `-- sqlly …` directive info popup, stamped at paint
    /// time so `handle_mouse_down` can hit-test a click on it (which opens the
    /// formatting-directives reference) and `handle_mouse_move` can keep the
    /// popup alive while the pointer is over it. `None` when no popup is shown.
    directive_hint_bounds: std::cell::Cell<Option<Bounds<Pixels>>>,
    /// Window bounds of the diagnostic hover popup and of its "Fix this" link,
    /// stamped at paint time (same role as `directive_hint_bounds`): the popup
    /// stays up while the pointer is over it, and a click on the link applies
    /// the diagnostic's fix. `None` when no popup / no link is shown.
    diagnostic_popup_bounds: std::cell::Cell<Option<Bounds<Pixels>>>,
    diagnostic_fix_bounds: std::cell::Cell<Option<Bounds<Pixels>>>,
    /// Window bounds of the diagnostic popup's "formatting directives
    /// reference" link, painted when the diagnostic is on a `-- sqlly …`
    /// comment. A click opens the reference window.
    diagnostic_directive_link_bounds: std::cell::Cell<Option<Bounds<Pixels>>>,
    /// Window bounds of the completion popup's item area, stamped at paint time
    /// so `handle_mouse_down` can turn a click into a row selection and the
    /// scroll-wheel handler can tell when the pointer is over the popup. `None`
    /// whenever the popup is not painted.
    completion_popup_bounds: std::cell::Cell<Option<Bounds<Pixels>>>,
    /// Painted height of one completion row, so a click's y offset maps to a row.
    completion_row_height: std::cell::Cell<f32>,
    /// Trigger kind + manual flag of the completion pass that produced the
    /// visible popup, captured into the "improve IntelliSense" snapshot.
    last_completion_trigger: Option<(IntelliSenseTriggerKind, bool)>,
    /// Display line whose play glyph is under the pointer.
    hovered_play_line: Option<usize>,
    /// Span + outcome of the last run, for the result-source tint and glyph.
    result_source: Option<ResultSource>,
    /// `statement_spans` cache keyed by `content_revision`.
    statement_cache: std::cell::RefCell<(Option<u64>, Vec<statements::StatementSpan>)>,
}

/// Outcome of the last run, for the gutter glyph tint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcome {
    Succeeded,
    Failed,
}

/// Lines that produced the active result set, plus whether that run succeeded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultSource {
    pub span: (usize, usize),
    pub outcome: RunOutcome,
}

pub enum SqlEditorEvent {
    Change,
    Execute,
    /// Gutter play glyph clicked; `offset` is the statement's first non-blank
    /// byte. The app layer moves the caret there and runs with the
    /// current-statement scope through the normal gate.
    RunStatementAt {
        offset: usize,
    },
    Validate,
    /// Request a distinct-values lookup for the column under the cursor. Carries
    /// the current word (best-effort column hint) and the full SQL text so the
    /// app layer can resolve a table reference and run `SELECT DISTINCT`.
    ValueLookup {
        word: String,
        sql: String,
    },
    /// Request async distinct-value fetch for a resolved value context. The
    /// app layer queries the DB and calls `apply_value_lookup_results` with
    /// the matching `request_token`.
    ValueCompletionsRequested {
        context: ValueContext,
        request_token: u64,
        /// The text typed so far at the value position. Empty for the
        /// trigger fetch; non-empty when a capped sample no longer covers
        /// what the user typed and the fetch must filter server-side.
        partial: String,
        /// Set when the column is a foreign-key id column: fetch the
        /// referenced table's (key, readable label) pairs instead of the raw
        /// ids, so the user searches by name and the id is inserted.
        fk: Option<Box<sqlly_intel::modelengine::intellisense::FkValuePivot>>,
    },
    /// A FIM (fill-in-the-middle) suggestion was accepted. Carries the inserted
    /// text so the app layer can mark the editor dirty / re-validate.
    FimAccepted {
        #[allow(dead_code)]
        text: String,
    },
    /// The user pressed the manual FIM trigger shortcut. The app layer owns
    /// the request pipeline (prefs, debounce token, Ollama call), so the
    /// editor only reports the keypress.
    FimTriggerRequested,
    /// Modifier-click on a symbol: navigate to its DDL. Carries the click's
    /// byte offset and the full SQL text (`SharedString` clones are cheap —
    /// no buffer copy on the click hot path).
    DdlNavigationRequested {
        offset: usize,
        sql: SharedString,
    },
    /// F2 (or the hint chip) captured an "Improve IntelliSense" context. The
    /// app layer hosts the dialog above the workbench — rendered inside the
    /// editor it would clip to the pane. Carries the frozen snapshot, the
    /// preference-derived initial include-all state, and the editor font for
    /// the query preview. Boxed: the snapshot is large and the enum's other
    /// variants are small.
    FeedbackRequested {
        snapshot: Box<crate::intellisense_feedback::FeedbackSnapshot>,
        include_all_default: bool,
        /// Preference-derived default kind visibility for the disclosure list.
        kind_defaults: crate::feedback_tree::KindVisibility,
        font: Font,
    },
    /// Vim Normal / Insert / Visual changed; the status-bar badge should repaint.
    VimModeChanged,
    /// Non-fatal editor action outcome for the app status bar.
    Status {
        level: crate::app::StatusLevel,
        message: String,
    },
    /// Cmd/Ctrl-wheel or pinch over the editor. Positive `delta` zooms in.
    ZoomBy {
        delta: f32,
    },
}

impl EventEmitter<SqlEditorEvent> for SqlEditor {}

impl SqlEditor {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        let editor_prefs = EditorPreferences::default();
        Self {
            focus_handle,
            content: SharedString::default(),
            cursor: 0,
            selection_anchor: None,
            last_layout: Vec::new(),
            last_layout_first_line: 0,
            last_tab_maps: std::rc::Rc::new(Vec::new()),
            last_bounds: None,
            scrollbar_geometry: Default::default(),
            scroll_handle: ScrollHandle::new(),
            content_revision: 0,
            revision_counter: std::rc::Rc::new(std::cell::Cell::new(0)),
            doc_epoch: 0,
            shared_peer: None,
            line_index_cache: std::rc::Rc::new(std::cell::RefCell::new(None)),
            comment_state_cache: std::rc::Rc::new(std::cell::RefCell::new(None)),
            max_display_cols_cache: std::rc::Rc::new(std::cell::RefCell::new(None)),
            shape_cache: std::rc::Rc::new(std::cell::RefCell::new(ShapeCache::default())),
            folds: std::cell::RefCell::new((None, Default::default())),
            insert_hints: std::rc::Rc::new(Vec::new()),
            insert_hints_revision: None,
            insert_hints_version: 0,
            insert_hints_generation: 0,
            completion_generation: 0,
            occurrence_cache: std::cell::RefCell::new(None),
            is_focused: false,
            is_selecting: false,
            rectangular_anchor: None,
            rectangular_target: None,
            zoom_wheel_accumulator: 0.0,
            goal_column: None,
            extra_cursors: Vec::new(),
            last_added_extra: None,
            snippet_session: None,
            // View-local Vim state; None when the preference is off.
            vim: if editor_prefs.vim_mode {
                Some(VimState::new(VimMode::Normal))
            } else {
                None
            },
            marked_range: None,
            font: gpui::font(Theme::editor_font_family()),
            font_size: px(Theme::editor_font_size()),
            line_height: px(Theme::editor_font_size() * 1.5),
            line_number_width: Self::gutter_width(editor_prefs.show_line_numbers),
            services: None,
            completion_catalog: std::sync::Arc::new(CompletionCatalog::default()),
            semantic_names: std::cell::RefCell::new(None),
            schema_snapshot: std::sync::Arc::new(sqlly_lang::sqlmodel::ModelSnapshot::default()),
            completion_procedures: std::sync::Arc::new(Vec::new()),
            redis_completion: std::sync::Arc::new(crate::app::RedisCompletionContext::default()),
            completion_rules: std::sync::Arc::new(IntelliSenseRuleConfig::default()),
            accept_history: std::sync::Arc::default(),
            completions: std::rc::Rc::new(Vec::new()),
            completion_index: 0,
            completion_scroll_offset: 0,
            completion_visible: false,
            completion_manual_open: false,
            completion_shown_at: None,
            completion_replace_start: 0,
            accepted_completion_at: None,
            completion_user_navigated: false,
            active_value_lookup: None,
            value_lookup_token: 0,
            value_lookup_base_items: Vec::new(),
            value_lookup_values: Vec::new(),
            value_lookup_fk: None,
            value_lookup_pending: false,
            value_lookup_fetched_partial: String::new(),
            value_lookup_truncated: false,
            diagnostics: std::rc::Rc::new(Vec::new()),
            diag_by_line: std::rc::Rc::new(std::collections::HashMap::new()),
            diag_spans: std::rc::Rc::new(Vec::new()),
            line_number_labels: std::cell::RefCell::new(Vec::new()),
            hovered_diagnostic: None,
            hovered_sqlly_directive: None,
            hover_info: None,
            fold_preview: None,
            hover_dwell_task: None,
            hover_card_bounds: std::cell::Cell::new(None),
            fold_chip_bounds: std::cell::RefCell::new(Vec::new()),
            date_helper: None,
            date_marker_cache: std::cell::RefCell::new((None, std::rc::Rc::new(Vec::new()))),
            date_chip_bounds: std::cell::RefCell::new(Vec::new()),
            signature_help: None,
            signature_dismissed_at_paren: None,
            routine_signatures: std::sync::Arc::new(Vec::new()),
            editor_prefs,
            ddl_nav_prefs: DdlNavigationPreferences::default(),
            database_type: sqlly_lang::sqlmodel::DatabaseType::SqlServer,
            fim_suggestion: None,
            fim_visible: false,
            needs_focus: true,
            undo_stack: std::collections::VecDeque::new(),
            redo_stack: Vec::new(),
            #[cfg(test)]
            format_as_you_go_override: None,
            undo_group: None,
            last_mouse_position: None,
            link_hover_range: None,
            context_menu: None,
            format_dialog: None,
            formatter_picker: None,
            formatter_picker_sub: None,
            pending_format_sample: None,
            convert_dialog: None,
            dialog_frames: crate::dialog::DialogFrames::new(),
            goto_line_bar: None,
            find_bar: None,
            automagic_comma_overrides: [None, None, None],
            automagic_connector_override: None,
            feedback_hint_bounds: std::cell::Cell::new(None),
            directive_hint_bounds: std::cell::Cell::new(None),
            diagnostic_popup_bounds: std::cell::Cell::new(None),
            diagnostic_fix_bounds: std::cell::Cell::new(None),
            diagnostic_directive_link_bounds: std::cell::Cell::new(None),
            completion_popup_bounds: std::cell::Cell::new(None),
            completion_row_height: std::cell::Cell::new(0.0),
            last_completion_trigger: None,
            hovered_play_line: None,
            result_source: None,
            statement_cache: std::cell::RefCell::new((None, Vec::new())),
        }
    }

    /// Gutter width for the line-number column. Zero when line numbers are
    /// hidden so the text content reclaims the space. Includes the play-glyph
    /// column when line numbers are on.
    fn gutter_width(show_line_numbers: bool) -> Pixels {
        if show_line_numbers {
            px(40.0 + statements::PLAY_COLUMN_WIDTH)
        } else {
            px(0.0)
        }
    }

    /// Gutter width that fits the widest line number of a `line_count`-line
    /// buffer, given the advance of one digit in the editor font. Always
    /// reserves room for [`GUTTER_MIN_DIGITS`] digits so short scripts keep a
    /// steady gutter, and grows a digit at a time up to [`GUTTER_MAX_DIGITS`].
    fn fitted_gutter_width(
        show_line_numbers: bool,
        line_count: usize,
        digit_width: Pixels,
    ) -> Pixels {
        if !show_line_numbers {
            return px(0.0);
        }
        let digits =
            (line_count.max(1).ilog10() as usize + 1).clamp(GUTTER_MIN_DIGITS, GUTTER_MAX_DIGITS);
        // Play column, 8px before the number, and 14px after it for the
        // diagnostic dot / fold chevron slot.
        px(statements::PLAY_COLUMN_WIDTH + 8.0 + 14.0) + digit_width * digits as f32
    }

    /// Bump `content_revision` and invalidate the cached `LineIndex`. Call
    /// at every point where `self.content` is assigned a new value. Draws the
    /// new revision from the (possibly shared) counter so two same-buffer
    /// views can never mint the same revision for different buffer states,
    /// and bumps `doc_epoch` so the peer view adopts this document state.
    fn invalidate_text_derived_caches(&mut self) {
        let next = self.revision_counter.get().wrapping_add(1);
        self.revision_counter.set(next);
        self.content_revision = next;
        self.doc_epoch = self.doc_epoch.wrapping_add(1);
        if self
            .diagnostics
            .iter()
            .any(|d| d.source == DiagnosticSource::Execution)
        {
            self.diagnostics = std::rc::Rc::new(
                self.diagnostics
                    .iter()
                    .filter(|d| d.source != DiagnosticSource::Execution)
                    .cloned()
                    .collect(),
            );
            self.rebuild_diagnostic_index();
            self.hovered_diagnostic = None;
        }
        self.result_source = None;
        self.clear_hover_ui();
    }

    // ── Same-buffer split: one document, two views ─────────────────────────

    /// Create a second `SqlEditor` entity presenting `source`'s document: the
    /// text buffer, undo/redo stream, diagnostics, and completion inputs are
    /// shared (kept in sync through the peer link), while cursor, selection,
    /// extra cursors, scroll, find bar, and folds stay per-view. The new view
    /// opens on the source's caret but never steals focus.
    pub fn new_shared_view(source: &Entity<SqlEditor>, cx: &mut gpui::App) -> Entity<SqlEditor> {
        let source_handle = source.clone();
        let view = cx.new(|cx| {
            let mut view = SqlEditor::new(cx);
            view.needs_focus = false;
            let adopted_epoch = {
                let src = source_handle.read(cx);
                // Document state (mirrored from here on via the peer link).
                view.content = src.content.clone();
                view.content_revision = src.content_revision;
                view.revision_counter = src.revision_counter.clone();
                view.line_index_cache = src.line_index_cache.clone();
                view.comment_state_cache = src.comment_state_cache.clone();
                view.max_display_cols_cache = src.max_display_cols_cache.clone();
                view.shape_cache = src.shape_cache.clone();
                view.undo_stack = src.undo_stack.clone();
                view.redo_stack = src.redo_stack.clone();
                view.diagnostics = src.diagnostics.clone();
                view.diag_by_line = src.diag_by_line.clone();
                view.diag_spans = src.diag_spans.clone();
                // Per-view state seeded from the source so the pane opens on
                // the same spot; fully independent afterwards.
                view.cursor = src.cursor;
                view.selection_anchor = src.selection_anchor;
                // Settings the app pushes per tab; the primary forwards any
                // later changes through `forward_to_shared_view`.
                view.services = src.services.clone();
                view.completion_catalog = src.completion_catalog.clone();
                view.schema_snapshot = src.schema_snapshot.clone();
                view.completion_procedures = src.completion_procedures.clone();
                view.routine_signatures = src.routine_signatures.clone();
                view.completion_rules = src.completion_rules.clone();
                view.accept_history = src.accept_history.clone();
                view.editor_prefs = src.editor_prefs.clone();
                view.line_number_width = Self::gutter_width(src.editor_prefs.show_line_numbers);
                view.vim = if src.editor_prefs.vim_mode {
                    Some(VimState::new(VimMode::Normal))
                } else {
                    None
                };
                view.ddl_nav_prefs = src.ddl_nav_prefs.clone();
                view.font = src.font.clone();
                view.font_size = src.font_size;
                view.line_height = src.line_height;
                view.result_source = src.result_source.clone();
                view.insert_hints = src.insert_hints.clone();
                view.insert_hints_revision = src.insert_hints_revision;
                view.insert_hints_version = view.insert_hints_version.wrapping_add(1);
                view.insert_hints_generation = src.insert_hints_generation;
                src.doc_epoch
            };
            view.shared_peer = Some(SharedDocumentPeer {
                peer: source_handle.downgrade(),
                primary: false,
                adopted_epoch,
                _subscriptions: [
                    cx.observe(&source_handle, Self::sync_from_peer),
                    cx.subscribe(&source_handle, Self::on_peer_event),
                ],
            });
            view
        });
        let view_epoch = view.read(cx).doc_epoch;
        source.update(cx, |src, cx| {
            src.shared_peer = Some(SharedDocumentPeer {
                peer: view.downgrade(),
                primary: true,
                adopted_epoch: view_epoch,
                _subscriptions: [
                    cx.observe(&view, Self::sync_from_peer),
                    cx.subscribe(&view, Self::on_peer_event),
                ],
            });
        });
        view
    }

    /// Break the same-buffer link. The document stays exactly as it is in
    /// this editor; only the mirroring (and its subscriptions) stops.
    pub fn detach_shared_peer(&mut self) {
        self.shared_peer = None;
    }

    /// The live peer view of a same-buffer link, if any.
    pub fn shared_peer_entity(&self) -> Option<Entity<SqlEditor>> {
        self.shared_peer
            .as_ref()
            .and_then(|link| link.peer.upgrade())
    }

    /// This editor's caret (cursor + selection anchor), for carrying a
    /// closing split view's position into the surviving editor.
    pub fn caret_state(&self) -> (usize, Option<usize>) {
        (self.cursor, self.selection_anchor)
    }

    /// Adopt a caret position from another view of the same document,
    /// clamped to valid char boundaries.
    pub fn adopt_caret_state(
        &mut self,
        cursor: usize,
        anchor: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        self.cursor = utf8_safe_edit_range(&self.content, cursor..cursor).start;
        self.selection_anchor = anchor
            .map(|a| utf8_safe_edit_range(&self.content, a..a).start)
            .filter(|&a| a != self.cursor);
        self.goal_column = None;
        self.scroll_cursor_into_view();
        cx.notify();
    }

    /// Peer Change events also drive adoption: every user edit emits Change,
    /// which covers any mutation path that repaints without notifying.
    fn on_peer_event(
        &mut self,
        peer: Entity<SqlEditor>,
        event: &SqlEditorEvent,
        cx: &mut Context<Self>,
    ) {
        if matches!(event, SqlEditorEvent::Change) {
            self.sync_from_peer(peer, cx);
        }
    }

    /// Adopt the peer view's document state when its `doc_epoch` moved.
    /// Adoption never bumps this editor's own `doc_epoch`, which is what
    /// makes the two-way mirror converge instead of ping-ponging.
    fn sync_from_peer(&mut self, peer: Entity<SqlEditor>, cx: &mut Context<Self>) {
        let Some(link) = &self.shared_peer else {
            return;
        };
        if link.peer.entity_id() != peer.entity_id() {
            return;
        }
        let peer_epoch = peer.read(cx).doc_epoch;
        if link.adopted_epoch == peer_epoch {
            return;
        }
        let (content, revision, undo_stack, redo_stack, diagnostics, diag_by_line, diag_spans) = {
            let p = peer.read(cx);
            (
                p.content.clone(),
                p.content_revision,
                p.undo_stack.clone(),
                p.redo_stack.clone(),
                p.diagnostics.clone(),
                p.diag_by_line.clone(),
                p.diag_spans.clone(),
            )
        };
        if let Some(link) = &mut self.shared_peer {
            link.adopted_epoch = peer_epoch;
        }
        // Revisions come from the shared counter, so equal revisions mean
        // identical text (a diagnostics-only adoption).
        let content_changed = self.content_revision != revision;
        self.content = content;
        self.content_revision = revision;
        self.undo_stack = undo_stack;
        self.redo_stack = redo_stack;
        self.diagnostics = diagnostics;
        self.diag_by_line = diag_by_line;
        self.diag_spans = diag_spans;
        self.hovered_diagnostic = None;
        self.clear_hover_ui();
        if content_changed {
            // The peer edited the document out from under this view: break
            // undo coalescing, snap every per-view caret onto a valid char
            // boundary of the new text, and drop popups/overlays that were
            // keyed to the old text.
            self.undo_group = None;
            self.marked_range = None;
            self.goal_column = None;
            self.cursor = utf8_safe_edit_range(&self.content, self.cursor..self.cursor).start;
            self.selection_anchor = self
                .selection_anchor
                .map(|a| utf8_safe_edit_range(&self.content, a..a).start)
                .filter(|&a| a != self.cursor);
            for ec in &mut self.extra_cursors {
                ec.cursor = utf8_safe_edit_range(&self.content, ec.cursor..ec.cursor).start;
                ec.anchor = ec
                    .anchor
                    .map(|a| utf8_safe_edit_range(&self.content, a..a).start)
                    .filter(|&a| a != ec.cursor);
            }
            self.normalize_extra_cursors();
            self.close_completions(cx);
            self.hide_fim_suggestion(cx);
            if self.find_bar.as_ref().is_some_and(|bar| bar.visible) {
                self.schedule_find_rescan(cx);
            }
            self.schedule_insert_hints(cx);
            // Deliberately NO Change re-emit here: the app subscribes to both
            // views and maps either one to the tab, so user edits already
            // reach the per-tab pipeline from the entity they were typed
            // into. Re-emitting would also mis-flag programmatic loads
            // (`set_text` emits no Change, and its adoption must not either,
            // or opening a file with a split up would dirty the tab).
        }
        cx.notify();
    }

    /// Run `f` on the linked second view — from the primary (canonical)
    /// editor only, so app-pushed settings fan out to both views without any
    /// chance of a forwarding loop.
    fn forward_to_shared_view(
        &self,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut SqlEditor, &mut Context<SqlEditor>),
    ) {
        let Some(link) = &self.shared_peer else {
            return;
        };
        if !link.primary {
            return;
        }
        let Some(peer) = link.peer.upgrade() else {
            return;
        };
        peer.update(cx, f);
    }

    /// Applies editor preferences (line-number visibility, indent style/width).
    /// Recomputes the gutter width and repaints.
    pub fn set_editor_prefs(&mut self, prefs: EditorPreferences, cx: &mut Context<Self>) {
        if prefs.vim_mode && self.vim.is_none() {
            self.vim = Some(VimState::new(VimMode::Normal));
            self.selection_anchor = None;
            self.clear_extra_cursors();
            cx.emit(SqlEditorEvent::VimModeChanged);
        } else if !prefs.vim_mode && self.vim.is_some() {
            self.vim = None;
            cx.emit(SqlEditorEvent::VimModeChanged);
        }
        self.line_number_width = Self::gutter_width(prefs.show_line_numbers);
        self.editor_prefs = prefs.clone();
        if !self.editor_prefs.show_insert_value_hints {
            self.insert_hints = std::rc::Rc::new(Vec::new());
            self.insert_hints_revision = Some(self.content_revision);
            self.insert_hints_version = self.insert_hints_version.wrapping_add(1);
        }
        self.schedule_insert_hints(cx);
        self.forward_to_shared_view(cx, move |view, cx| view.set_editor_prefs(prefs, cx));
        cx.notify();
    }

    /// Status-bar label (`NORMAL` / `INSERT` / `VISUAL`), or `None` when Vim is off.
    pub fn vim_mode_label(&self) -> Option<&'static str> {
        self.vim.as_ref().map(|v| v.mode.label())
    }

    /// Flip this view's Vim preference and apply it. Persistence is the
    /// command / preferences pane's job; those paths push an absolute
    /// `EditorPreferences` via `set_editor_prefs` so every tab and split
    /// peer lands on the same value (a per-view flip would desync splits).
    #[allow(dead_code)]
    pub fn toggle_vim_mode(&mut self, cx: &mut Context<Self>) {
        let mut prefs = self.editor_prefs.clone();
        prefs.vim_mode = !prefs.vim_mode;
        self.set_editor_prefs(prefs, cx);
    }

    fn feed_vim(
        &mut self,
        key: VimKey,
        mut window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.vim.is_none() {
            return false;
        }
        let cmds = self.vim.as_mut().unwrap().feed(key);
        for cmd in cmds {
            self.apply_vim_command(cmd, window.as_deref_mut(), cx);
        }
        cx.notify();
        true
    }

    fn vim_selecting(&self) -> bool {
        self.vim.as_ref().is_some_and(|v| v.mode == VimMode::Visual)
    }

    fn vim_move_to(&mut self, target: usize, select: bool, cx: &mut Context<Self>) {
        let target = target.min(self.content.len());
        if select {
            if self.selection_anchor.is_none() {
                self.selection_anchor = Some(self.cursor);
            }
            self.cursor = target;
        } else {
            self.selection_anchor = None;
            self.cursor = target;
        }
        self.scroll_cursor_into_view();
        cx.notify();
    }

    fn clamp_normal_caret(&mut self) {
        if !self.vim.as_ref().is_some_and(|v| v.mode == VimMode::Normal) {
            return;
        }
        let (line, _) = self.line_col_for_offset(self.cursor);
        let start = self.line_start_offset(line);
        if self.cursor <= start {
            return;
        }
        let at_line_end =
            self.cursor == self.content.len() || self.content[self.cursor..].starts_with('\n');
        if at_line_end {
            self.cursor = self.previous_grapheme_boundary(self.cursor);
        }
    }

    fn vim_write_clipboard(&mut self, text: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
    }

    fn apply_vim_command(
        &mut self,
        cmd: VimCommand,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) {
        let select = self.vim_selecting();
        match cmd {
            VimCommand::Move { motion, count } => {
                self.apply_vim_motion(motion, count, select, cx);
                self.clamp_normal_caret();
            }
            VimCommand::Operate {
                op,
                mut motion,
                count,
            } => {
                if op == Operator::Change && motion == Motion::WordForward {
                    motion = Motion::WordEnd;
                }
                if matches!(motion, Motion::InnerWord | Motion::AWord) {
                    let range =
                        text_object_word_range(&self.content, self.cursor, motion == Motion::AWord);
                    self.vim_apply_operator(op, range, cx);
                } else {
                    let start = self.cursor;
                    self.selection_anchor = Some(start);
                    self.apply_vim_motion(motion, count, true, cx);
                    let range = self.selection().unwrap_or(start..start);
                    self.vim_apply_operator(op, range, cx);
                }
                self.clamp_normal_caret();
            }
            VimCommand::OperateLines { op, count } => {
                self.vim_operate_lines(op, count, cx);
                self.clamp_normal_caret();
            }
            VimCommand::OperateSelection { op } => {
                let range = self.selection().unwrap_or_else(|| {
                    let (line, _) = self.line_col_for_offset(self.cursor);
                    let end = self.line_end_offset(line);
                    if self.cursor < end {
                        self.cursor..self.next_grapheme_boundary(self.cursor)
                    } else {
                        self.cursor..self.cursor
                    }
                });
                self.vim_apply_operator(op, range, cx);
                self.clamp_normal_caret();
            }
            VimCommand::DeleteCharForward { count } => {
                let (line, _) = self.line_col_for_offset(self.cursor);
                let end = self.line_end_offset(line);
                let start = self.cursor;
                let mut cur = start;
                for _ in 0..count {
                    if cur >= end {
                        break;
                    }
                    cur = self.next_grapheme_boundary(cur);
                }
                if cur > start {
                    self.vim_apply_operator(Operator::Delete, start..cur, cx);
                }
                self.clamp_normal_caret();
            }
            VimCommand::InsertHere => {}
            VimCommand::InsertAfter => {
                let (line, _) = self.line_col_for_offset(self.cursor);
                let end = self.line_end_offset(line);
                if self.cursor < end {
                    self.move_right(false, cx);
                }
            }
            VimCommand::InsertLineStart => {
                let (line, _) = self.line_col_for_offset(self.cursor);
                self.vim_move_to(self.line_start_offset(line), false, cx);
            }
            VimCommand::InsertLineEnd => self.move_end(false, cx),
            VimCommand::OpenLineBelow => {
                self.move_end(false, cx);
                self.insert_newline(cx);
            }
            VimCommand::OpenLineAbove => {
                let (line, _) = self.line_col_for_offset(self.cursor);
                self.selection_anchor = None;
                self.cursor = self.line_start_offset(line);
                self.replace_selection("\n", cx);
                self.move_up(false, cx);
            }
            VimCommand::PasteAfter { count } => self.vim_paste(true, count, cx),
            VimCommand::PasteBefore { count } => self.vim_paste(false, count, cx),
            VimCommand::Undo => self.undo(cx),
            VimCommand::Redo => self.redo(cx),
            VimCommand::OpenFind => {
                if let Some(window) = window {
                    self.open_find_bar(false, window, cx);
                }
            }
            VimCommand::SetMode(mode) => {
                if let Some(v) = self.vim.as_mut() {
                    v.mode = mode;
                }
                match mode {
                    VimMode::Normal => {
                        self.selection_anchor = None;
                        self.close_completions(cx);
                        self.hide_fim_suggestion(cx);
                        self.clear_extra_cursors();
                        if self.snippet_session.is_some() {
                            self.end_snippet_session();
                        }
                    }
                    VimMode::Visual => {
                        self.selection_anchor = Some(self.cursor);
                    }
                    VimMode::Insert => {}
                }
                cx.emit(SqlEditorEvent::VimModeChanged);
            }
        }
    }

    fn apply_vim_motion(
        &mut self,
        motion: Motion,
        count: usize,
        select: bool,
        cx: &mut Context<Self>,
    ) {
        for _ in 0..count.max(1) {
            match motion {
                Motion::Left => {
                    let (line, _) = self.line_col_for_offset(self.cursor);
                    let start = self.line_start_offset(line);
                    if self.cursor > start {
                        self.vim_move_to(self.previous_grapheme_boundary(self.cursor), select, cx);
                    }
                }
                Motion::Right => {
                    let (line, _) = self.line_col_for_offset(self.cursor);
                    let end = self.line_end_offset(line);
                    if self.cursor < end {
                        let next = self.next_grapheme_boundary(self.cursor);
                        if next <= end {
                            self.vim_move_to(next, select, cx);
                        }
                    }
                }
                Motion::Up => self.move_up(select, cx),
                Motion::Down => self.move_down(select, cx),
                Motion::WordForward => self.move_word_right(select, cx),
                Motion::WordBackward => self.move_word_left(select, cx),
                Motion::WordEnd => {
                    let on = word_end_offset(&self.content, self.cursor);
                    let target = if select {
                        self.next_grapheme_boundary(on)
                    } else {
                        on
                    };
                    self.vim_move_to(target, select, cx);
                }
                Motion::LineStart => {
                    let (line, _) = self.line_col_for_offset(self.cursor);
                    self.vim_move_to(self.line_start_offset(line), select, cx);
                }
                Motion::LineEnd => {
                    let (line, _) = self.line_col_for_offset(self.cursor);
                    self.vim_move_to(self.line_end_offset(line), select, cx);
                }
                Motion::FirstLine => {
                    let anchor = if select {
                        Some(self.selection_anchor.unwrap_or(self.cursor))
                    } else {
                        None
                    };
                    self.go_to_line(1, cx);
                    if let Some(a) = anchor {
                        self.selection_anchor = Some(a);
                    }
                }
                Motion::LastLine => {
                    let anchor = if select {
                        Some(self.selection_anchor.unwrap_or(self.cursor))
                    } else {
                        None
                    };
                    let last = self.line_count().max(1);
                    self.go_to_line(last, cx);
                    if let Some(a) = anchor {
                        self.selection_anchor = Some(a);
                    }
                }
                Motion::InnerWord | Motion::AWord => {}
            }
        }
    }

    fn vim_apply_operator(&mut self, op: Operator, range: Range<usize>, cx: &mut Context<Self>) {
        let range = utf8_safe_edit_range(&self.content, range);
        if range.start == range.end && op == Operator::Yank {
            return;
        }
        let text = self.content[range.clone()].to_string();
        match op {
            Operator::Yank => {
                self.vim_write_clipboard(text, cx);
                self.cursor = range.start;
                self.selection_anchor = None;
            }
            Operator::Delete | Operator::Change => {
                self.vim_write_clipboard(text, cx);
                self.selection_anchor = Some(range.start);
                self.cursor = range.end;
                self.replace_selection("", cx);
            }
        }
    }

    fn vim_operate_lines(&mut self, op: Operator, count: usize, cx: &mut Context<Self>) {
        let (line, _) = self.line_col_for_offset(self.cursor);
        let mut start = self.line_start_offset(line);
        let end_line = line.saturating_add(count);
        let end = if end_line >= self.line_count() {
            self.content.len()
        } else {
            self.line_start_offset(end_line)
        };
        if end == self.content.len()
            && start > 0
            && !self.content[start..end].contains('\n')
            && self.content.as_bytes()[start - 1] == b'\n'
        {
            start -= 1;
        }
        let range = utf8_safe_edit_range(&self.content, start..end);
        let mut text = self.content[range.clone()].to_string();
        if !text.ends_with('\n') {
            text.push('\n');
        }
        match op {
            Operator::Yank => {
                self.vim_write_clipboard(text, cx);
                self.cursor = range.start;
                self.selection_anchor = None;
            }
            Operator::Delete => {
                self.vim_write_clipboard(text, cx);
                self.selection_anchor = Some(range.start);
                self.cursor = range.end;
                self.replace_selection("", cx);
            }
            Operator::Change => {
                self.vim_write_clipboard(text, cx);
                let replacement = if self.content[range.clone()].ends_with('\n') {
                    "\n"
                } else {
                    ""
                };
                self.selection_anchor = Some(range.start);
                self.cursor = range.end;
                self.replace_selection(replacement, cx);
                self.cursor = range.start;
            }
        }
    }

    fn vim_paste(&mut self, after: bool, count: usize, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        let text = if text.contains('\r') {
            text.replace("\r\n", "\n").replace('\r', "\n")
        } else {
            text
        };
        if text.is_empty() {
            return;
        }
        let text = text.repeat(count.max(1));
        let linewise = text.ends_with('\n');
        self.selection_anchor = None;
        if linewise {
            let body = text.trim_end_matches('\n');
            let (line, _) = self.line_col_for_offset(self.cursor);
            if after {
                if line + 1 >= self.line_count() {
                    let at = self.content.len();
                    self.cursor = at;
                    self.replace_selection(&format!("\n{body}"), cx);
                    self.cursor = at + 1;
                } else {
                    let at = self.line_start_offset(line + 1);
                    self.cursor = at;
                    self.replace_selection(&format!("{body}\n"), cx);
                    self.cursor = at;
                }
            } else {
                let at = self.line_start_offset(line);
                self.cursor = at;
                self.replace_selection(&format!("{body}\n"), cx);
                self.cursor = at;
            }
        } else if after {
            let (line, _) = self.line_col_for_offset(self.cursor);
            let end = self.line_end_offset(line);
            if self.cursor < end {
                self.move_right(false, cx);
            }
            let at = self.cursor;
            self.replace_selection(&text, cx);
            if !text.is_empty() {
                self.cursor = self.previous_grapheme_boundary(at + text.len());
            }
        } else {
            let at = self.cursor;
            self.replace_selection(&text, cx);
            if !text.is_empty() {
                self.cursor = self.previous_grapheme_boundary(at + text.len());
            }
        }
        self.clamp_normal_caret();
    }

    /// Applies DDL navigation gesture preferences. Called on every open
    /// editor when preferences change so rebinding takes effect immediately.
    pub fn set_ddl_nav_prefs(&mut self, prefs: DdlNavigationPreferences, cx: &mut Context<Self>) {
        self.ddl_nav_prefs = prefs.clone();
        self.forward_to_shared_view(cx, move |view, cx| view.set_ddl_nav_prefs(prefs, cx));
    }

    pub fn set_database_type(
        &mut self,
        db: sqlly_lang::sqlmodel::DatabaseType,
        cx: &mut Context<Self>,
    ) {
        if self.database_type != db {
            self.database_type = db;
            self.statement_cache.borrow_mut().0 = None;
            cx.notify();
        }
    }

    pub fn database_type(&self) -> sqlly_lang::sqlmodel::DatabaseType {
        self.database_type
    }

    /// Number of runnable statements in the buffer (dialect-aware).
    pub fn statement_count(&self) -> usize {
        self.with_statement_spans(|spans| spans.len())
    }

    fn database_type_for_literals(&self) -> sqlly_lang::sqlmodel::DatabaseType {
        self.database_type
    }

    pub fn set_font(&mut self, family: &str, size: f32, cx: &mut Context<Self>) {
        self.font = gpui::font(family.to_string());
        self.font_size = px(Theme::scaled(size));
        self.line_height = px(Theme::scaled(size * 1.5));
        let family = family.to_string();
        self.forward_to_shared_view(cx, move |view, cx| view.set_font(&family, size, cx));
        cx.notify();
    }

    pub fn set_diagnostics(
        &mut self,
        diagnostics: Vec<ValidationDiagnostic>,
        cx: &mut Context<Self>,
    ) {
        // Lint entries are editor-owned and recomputed on their own cadence;
        // a validation push must not wipe them (and never produces them).
        let mut next: Vec<ValidationDiagnostic> = self
            .diagnostics
            .iter()
            .filter(|d| d.source == DiagnosticSource::Lint)
            .cloned()
            .collect();
        next.extend(
            diagnostics
                .into_iter()
                .filter(|d| d.source != DiagnosticSource::Lint),
        );
        self.diagnostics = std::rc::Rc::new(next);
        self.rebuild_diagnostic_index();
        self.hovered_diagnostic = None;
        self.clear_hover_ui();
        // Diagnostics are document state: move the epoch so a same-buffer
        // peer view adopts them too.
        self.doc_epoch = self.doc_epoch.wrapping_add(1);
        cx.notify();
    }

    /// Underline a failed run's error span. Replaces any previous execution
    /// diagnostic, keeps validation diagnostics, and is dropped by the next
    /// buffer edit (see `invalidate_text_derived_caches`).
    pub fn set_execution_error(
        &mut self,
        diagnostic: ValidationDiagnostic,
        cx: &mut Context<Self>,
    ) {
        // Lint entries are filtered out here because `set_diagnostics`
        // re-adds the live ones — passing them through would duplicate them.
        let mut next: Vec<ValidationDiagnostic> = self
            .diagnostics
            .iter()
            .filter(|d| {
                d.source != DiagnosticSource::Execution && d.source != DiagnosticSource::Lint
            })
            .cloned()
            .collect();
        next.push(diagnostic);
        self.set_diagnostics(next, cx);
    }

    /// Replace the editor-owned lint diagnostics, leaving validation and
    /// execution entries untouched. No-op (no repaint) when the lint set is
    /// empty on both sides.
    fn apply_lint_diagnostics(&mut self, lints: Vec<ValidationDiagnostic>, cx: &mut Context<Self>) {
        let had = self
            .diagnostics
            .iter()
            .any(|d| d.source == DiagnosticSource::Lint);
        if lints.is_empty() && !had {
            return;
        }
        let mut next: Vec<ValidationDiagnostic> = self
            .diagnostics
            .iter()
            .filter(|d| d.source != DiagnosticSource::Lint)
            .cloned()
            .collect();
        next.extend(lints);
        self.diagnostics = std::rc::Rc::new(next);
        self.rebuild_diagnostic_index();
        self.doc_epoch = self.doc_epoch.wrapping_add(1);
        cx.notify();
    }

    pub fn content_revision(&self) -> u64 {
        self.content_revision
    }

    pub fn selection_range(&self) -> Option<Range<usize>> {
        self.selection()
    }

    pub fn validation_diagnostics(&self) -> &[ValidationDiagnostic] {
        self.diagnostics.as_ref()
    }

    pub fn clear_diagnostics(&mut self, cx: &mut Context<Self>) {
        self.diagnostics = std::rc::Rc::new(Vec::new());
        self.diag_by_line = std::rc::Rc::new(std::collections::HashMap::new());
        self.diag_spans = std::rc::Rc::new(Vec::new());
        self.hovered_diagnostic = None;
        self.clear_hover_ui();
        self.doc_epoch = self.doc_epoch.wrapping_add(1);
        cx.notify();
    }

    fn rebuild_diagnostic_index(&mut self) {
        let idx = self.line_index();
        let mut by_line: std::collections::HashMap<usize, Vec<usize>> =
            std::collections::HashMap::new();
        let mut spans = Vec::with_capacity(self.diagnostics.len());
        let content_len = self.content.len();
        for (i, d) in self.diagnostics.iter().enumerate() {
            by_line.entry(d.line).or_default().push(i);
            let start = d.start_byte.min(content_len);
            let end = d.end_byte.min(content_len);
            let start_line = idx.line_col_for_offset(start).0;
            let end_line = if end == 0 {
                0
            } else {
                idx.line_col_for_offset(end.saturating_sub(1)).0
            };
            spans.push((start_line, end_line.max(start_line)));
        }
        self.diag_by_line = std::rc::Rc::new(by_line);
        self.diag_spans = std::rc::Rc::new(spans);
    }

    pub fn content(&self) -> &SharedString {
        &self.content
    }

    pub fn set_services(&mut self, services: Services, cx: &mut Context<Self>) {
        self.services = Some(services.clone());
        self.forward_to_shared_view(cx, move |view, cx| view.set_services(services, cx));
    }

    pub fn set_completion_catalog(
        &mut self,
        catalog: std::sync::Arc<CompletionCatalog>,
        cx: &mut Context<Self>,
    ) {
        self.completion_catalog = catalog.clone();
        self.schedule_insert_hints(cx);
        self.forward_to_shared_view(cx, move |view, cx| {
            view.completion_catalog = catalog;
            view.schedule_insert_hints(cx);
            cx.notify();
        });
        cx.notify();
    }

    pub fn set_schema_snapshot(
        &mut self,
        snapshot: std::sync::Arc<sqlly_lang::sqlmodel::ModelSnapshot>,
        cx: &mut Context<Self>,
    ) {
        self.schema_snapshot = snapshot.clone();
        // Identity metadata just (dis)appeared: refresh the IDENTITY_INSERT
        // lint without waiting for an edit.
        self.schedule_insert_hints(cx);
        self.forward_to_shared_view(cx, move |view, cx| view.set_schema_snapshot(snapshot, cx));
        cx.notify();
    }

    /// Install the Redis command vocabulary and scanned key names for this
    /// tab's connection (command engines only; SQL tabs ignore it).
    pub fn set_redis_completion(
        &mut self,
        context: std::sync::Arc<crate::app::RedisCompletionContext>,
        cx: &mut Context<Self>,
    ) {
        self.redis_completion = context.clone();
        self.forward_to_shared_view(cx, move |view, cx| {
            view.redis_completion = context;
            cx.notify();
        });
        cx.notify();
    }

    pub fn open_result_column_format_dialog(
        &mut self,
        column: String,
        data_type: &str,
        sample: Option<sqlly_core::CellValue>,
        cx: &mut Context<Self>,
    ) {
        let kind = result_column_type_class(data_type);
        self.pending_format_sample = sample;
        self.format_dialog = Some(ColumnFormatDialog {
            choices: column_format_choices(&column, kind),
            column,
            selected: 0,
        });
        cx.notify();
    }

    pub fn open_formatter_picker(
        &mut self,
        column: String,
        sample: Option<sqlly_core::CellValue>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let dialog = cx.new(|cx| {
            crate::formatter_picker_dialog::FormatterPickerDialog::new(column, sample, window, cx)
        });
        self.formatter_picker_sub =
            Some(
                cx.subscribe(&dialog, |editor, _entity, event, cx| match event {
                    crate::formatter_picker_dialog::FormatterPickerEvent::Apply {
                        column,
                        directive,
                    } => {
                        let updated =
                            update_or_insert_format_comment(&editor.content, column, directive);
                        editor.formatter_picker = None;
                        editor.formatter_picker_sub = None;
                        editor.replace_text_with_history(updated, cx);
                    }
                    crate::formatter_picker_dialog::FormatterPickerEvent::Cancel => {
                        editor.formatter_picker = None;
                        editor.formatter_picker_sub = None;
                        cx.notify();
                    }
                }),
            );
        dialog.read(cx).focus_handle(cx).focus(window, cx);
        self.formatter_picker = Some(dialog);
        cx.notify();
    }

    /// Set the in-scope stored procedures used to generate `EXEC` declaration
    /// snippets (the "additional intellisense" path). Loaded from the connected
    /// database's programmable-object metadata.
    pub fn set_completion_procedures(
        &mut self,
        procedures: std::sync::Arc<Vec<ProcedureInfo>>,
        cx: &mut Context<Self>,
    ) {
        self.completion_procedures = procedures.clone();
        self.routine_signatures = Self::signatures_from_procedures(&procedures);
        self.forward_to_shared_view(cx, move |view, cx| {
            view.completion_procedures = procedures.clone();
            view.routine_signatures = Self::signatures_from_procedures(&procedures);
            cx.notify();
        });
        cx.notify();
    }

    fn signatures_from_procedures(
        procedures: &[ProcedureInfo],
    ) -> std::sync::Arc<Vec<RoutineSignature>> {
        std::sync::Arc::new(
            procedures
                .iter()
                .map(|p| RoutineSignature {
                    schema: p.schema.clone(),
                    name: p.name.clone(),
                    parameters: p
                        .parameters
                        .iter()
                        .map(|par| HoverParameter {
                            name: par.name.clone(),
                            data_type: par.data_type.clone(),
                            is_output: par.is_output,
                        })
                        .collect(),
                })
                .collect(),
        )
    }

    fn clear_hover_ui(&mut self) {
        self.hover_dwell_task = None;
        self.hover_info = None;
        self.hover_card_bounds.set(None);
        self.fold_preview = None;
    }

    fn schedule_hover_resolve(&mut self, cx: &mut Context<Self>) {
        if !self.editor_prefs.show_hover_info {
            self.hover_dwell_task = None;
            return;
        }
        self.hover_dwell_task = None;
        let weak_self = cx.weak_entity();
        let async_cx = cx.to_async();
        self.hover_dwell_task = Some(cx.spawn(async move |_this, _cx| {
            async_cx.background_executor().timer(HOVER_CARD_DWELL).await;
            let mut cx = async_cx.clone();
            weak_self
                .update(&mut cx, |this, cx| {
                    this.resolve_hover_at_pointer(cx);
                })
                .ok();
        }));
    }

    fn resolve_hover_at_pointer(&mut self, cx: &mut Context<Self>) {
        if !self.editor_prefs.show_hover_info
            || self.hovered_diagnostic.is_some()
            || self.hovered_sqlly_directive.is_some()
            || self.completion_visible
        {
            return;
        }
        let Some(position) = self.last_mouse_position else {
            return;
        };
        let Some(offset) = self.byte_offset_for_position(position) else {
            return;
        };
        if self.database_type.is_command_engine() {
            // Redis tabs hold commands: the card is the command's docs and
            // safety badges, not a schema object.
            let next = sqlly_intel::redis_complete::hover_at(
                &self.content,
                offset,
                &self.redis_completion.vocabulary,
            );
            if next != self.hover_info {
                self.hover_info = next;
                cx.notify();
            }
            return;
        }
        let database = self
            .completion_catalog
            .databases
            .first()
            .map(|s| s.as_str());
        let next = sqlly_intel::intellisense::hover_at(
            &self.content,
            offset,
            &self.completion_catalog,
            &self.schema_snapshot,
            &self.routine_signatures,
            "dbo",
            database,
        )
        .or_else(|| {
            sqlly_intel::intellisense::builtin_hover_at(
                self.database_type.family(),
                &self.content,
                offset,
            )
        });
        if next != self.hover_info {
            self.hover_info = next;
            cx.notify();
        }
    }

    fn refresh_signature_help(&mut self, cx: &mut Context<Self>) {
        if !self.editor_prefs.show_signature_help {
            if self.signature_help.is_some() {
                self.signature_help = None;
                cx.notify();
            }
            return;
        }
        if self.database_type.is_command_engine() {
            // A command line's documented syntax with the caret's argument
            // active. Esc hides it for that line (`open_paren` is the line
            // start); another line, or no command, lifts the dismissal.
            let next = sqlly_intel::redis_complete::signature_help_at(
                &self.content,
                self.cursor,
                &self.redis_completion.vocabulary,
            );
            let next = match next {
                Some(help) if Some(help.open_paren) == self.signature_dismissed_at_paren => None,
                Some(help) => {
                    self.signature_dismissed_at_paren = None;
                    Some(help)
                }
                None => {
                    self.signature_dismissed_at_paren = None;
                    None
                }
            };
            if next != self.signature_help {
                self.signature_help = next;
                cx.notify();
            }
            return;
        }
        // Scan only the caret's statement (the cached spans), not the whole
        // buffer: a call never spans statements, and tokenizing a 0.5 MB
        // script on every keystroke was a measurable share of typing time.
        let (base, end) = self
            .with_statement_spans(|spans| {
                statements::statement_index_at(spans, self.cursor)
                    .map(|i| (spans[i].start, spans[i].end))
                    .filter(|&(start, end)| start <= self.cursor && self.cursor <= end)
            })
            .unwrap_or((0, self.content.len()));
        let next = sqlly_intel::intellisense::signature_help_for(
            self.database_type.family(),
            &self.content[base..end],
            self.cursor - base,
            &self.routine_signatures,
        )
        .map(|mut help| {
            help.open_paren += base;
            help
        });
        let next = match next {
            Some(help) if Some(help.open_paren) == self.signature_dismissed_at_paren => None,
            other => other,
        };
        if next.is_none() {
            if let Some(ctx) = sqlly_intel::intellisense::call_context_at(
                &self.content[base..end],
                self.cursor - base,
            ) {
                if Some(ctx.open_paren + base) != self.signature_dismissed_at_paren {
                    self.signature_dismissed_at_paren = None;
                }
            } else {
                self.signature_dismissed_at_paren = None;
            }
        }
        if next != self.signature_help {
            self.signature_help = next;
            cx.notify();
        }
    }

    /// Set the user-configurable completion rule config. Loaded from the user's
    /// JSONC preferences file; defaults to empty (no filtering) when unset.
    pub fn set_completion_rules(&mut self, rules: IntelliSenseRuleConfig, cx: &mut Context<Self>) {
        self.completion_rules = std::sync::Arc::new(rules);
        let rules = self.completion_rules.clone();
        self.forward_to_shared_view(cx, move |view, cx| {
            view.completion_rules = rules;
            cx.notify();
        });
        cx.notify();
    }

    fn trigger_completions_manual(&mut self, cx: &mut Context<Self>) {
        self.completion_generation = self.completion_generation.wrapping_add(1);
        // Latch "manually opened" so the popup stays up (as "No suggestions")
        // through empty automatic passes until dismissed or accepted.
        self.completion_manual_open = true;
        self.trigger_completions_with(true, cx);
    }

    /// Open the completion popup right after a comma-space column expansion so
    /// the user immediately sees the in-scope columns (alias-qualified variants
    /// included) for the next column. The caret has just landed at the start of
    /// a fresh column line, where automatic popups are normally suppressed, so
    /// this runs a manual-kind pass to bypass the auto-trigger gate.
    ///
    /// Unlike `trigger_completions_manual` it does not latch
    /// `completion_manual_open`: if nothing is in scope (no aliased tables,
    /// disconnected, …) the popup simply stays closed instead of lingering as
    /// "No suggestions".
    fn open_column_completions(&mut self, cx: &mut Context<Self>) {
        // Supersede the automatic pass that `after_text_change` just scheduled;
        // otherwise it fires ~120 ms later, hits the line-start suppression in
        // `should_auto_trigger`, and hides the popup we are opening here.
        self.completion_generation = self.completion_generation.wrapping_add(1);
        self.completion_manual_open = false;
        self.trigger_completions_with_kind(IntelliSenseTriggerKind::ManualShortcut, true, cx);
    }

    /// Set popup visibility, stamping `completion_shown_at` on the hidden →
    /// shown transition so the paint path can fade the popup in. Every
    /// non-test visibility change goes through here.
    fn set_completion_visible(&mut self, visible: bool) {
        if visible && !self.completion_visible {
            self.completion_shown_at = Some(Instant::now());
        }
        self.completion_visible = visible;
    }

    fn trigger_completions_with(&mut self, manual: bool, cx: &mut Context<Self>) {
        let trigger = if manual {
            IntelliSenseTriggerKind::ManualShortcut
        } else {
            IntelliSenseTriggerKind::AutomaticTyping
        };
        self.trigger_completions_with_kind(trigger, manual, cx);
    }

    fn trigger_completions_with_kind(
        &mut self,
        trigger: IntelliSenseTriggerKind,
        manual: bool,
        cx: &mut Context<Self>,
    ) {
        if self.database_type.is_command_engine() {
            self.trigger_redis_completions(trigger, manual, cx);
            return;
        }
        if let Some((items, replace_start)) =
            pivot_directive_completions(&self.content, self.cursor)
        {
            self.completions = std::rc::Rc::new(items);
            self.completion_index = 0;
            self.completion_scroll_offset = 0;
            self.completion_replace_start = replace_start;
            self.set_completion_visible(true);
            self.completion_user_navigated = false;
            self.last_completion_trigger = None;
            cx.notify();
            return;
        }
        if let Some(services) = &self.services {
            self.last_completion_trigger = Some((trigger, manual));
            // The IntelliSense pass parses the whole document — run it on the
            // background executor so typing pauses never stall the UI thread.
            // All inputs are Arc/SharedString clones (no buffer copies).
            let intellisense = services.intellisense.clone();
            let sql = self.content.clone();
            let cursor = self.cursor;
            let catalog = self.completion_catalog.clone();
            let procedures = self.completion_procedures.clone();
            let rules = self.completion_rules.clone();
            let accept_history = self.accept_history.clone();
            let schema_snapshot = self.schema_snapshot.clone();
            let database_type = self.database_type;
            let generation = self.completion_generation;
            let weak_self = cx.weak_entity();
            let async_cx = cx.to_async();
            cx.spawn(async move |_this, _cx| {
                let response = async_cx
                    .background_executor()
                    .spawn(async move {
                        let history = accept_history.lock().ok().map(|guard| guard.clone());
                        let req = CompletionRequest {
                            sql: &sql,
                            cursor,
                            trigger,
                            manual,
                            catalog: &catalog,
                            procedures: procedures.as_slice(),
                            rules: &rules,
                            parameters: &[],
                            model: Some(&schema_snapshot),
                            database_type: Some(database_type),
                        };
                        intellisense.complete_enriched_response_with(
                            &req,
                            RankingHints {
                                accept_history: history.as_ref(),
                            },
                        )
                    })
                    .await;
                let mut cx = async_cx.clone();
                weak_self
                    .update(&mut cx, |editor, cx| {
                        // Drop stale results: the user typed (generation
                        // bumped) or moved the caret since this pass started.
                        if editor.completion_generation != generation || editor.cursor != cursor {
                            return;
                        }
                        editor.completions = std::rc::Rc::new(response.items);
                        editor.completion_index = 0;
                        editor.completion_scroll_offset = 0;
                        editor.completion_replace_start = response.replace_start;
                        // A manual trigger opens the popup even with no
                        // suggestions (showing "No suggestions"), and it stays
                        // open across empty automatic passes while
                        // `completion_manual_open` is latched. Plain automatic
                        // typing still only shows when there's something to show.
                        editor.set_completion_visible(
                            editor.completion_manual_open
                                || (response.should_render && !editor.completions.is_empty()),
                        );
                        editor.completion_user_navigated = false;
                        cx.notify();
                    })
                    .ok();
            })
            .detach();
        }
    }

    /// Command IntelliSense for a Redis tab: the vocabulary and key index
    /// the app pushed, looked at for the caret's line only (a few
    /// microseconds), so it runs inline rather than on the background
    /// executor. An automatic pass on an empty line stays quiet — every
    /// Enter would otherwise list the whole vocabulary — while a manual
    /// trigger always answers, so Ctrl+Space on a blank line browses it.
    fn trigger_redis_completions(
        &mut self,
        trigger: IntelliSenseTriggerKind,
        manual: bool,
        cx: &mut Context<Self>,
    ) {
        let line = sqlly_intel::redis_complete::line_context(&self.content, self.cursor);
        if !manual && line.before.is_empty() && line.current.text.is_empty() {
            self.close_completions(cx);
            return;
        }
        self.last_completion_trigger = Some((trigger, manual));
        let history = self.accept_history.lock().ok().map(|guard| guard.clone());
        let context = self.redis_completion.clone();
        let response = sqlly_intel::redis_complete::complete(
            &self.content,
            self.cursor,
            &context.vocabulary,
            sqlly_intel::redis_complete::CompletionOptions {
                database: &context.database,
                keys: &context.keys,
                history: history.as_ref(),
            },
        );
        self.completions = std::rc::Rc::new(response.items);
        self.completion_index = 0;
        self.completion_scroll_offset = 0;
        self.completion_replace_start = response.replace_start;
        self.set_completion_visible(self.completion_manual_open || !self.completions.is_empty());
        self.completion_user_navigated = false;
        cx.notify();
    }

    pub fn close_completions(&mut self, cx: &mut Context<Self>) {
        let changed = self.completion_visible
            || self.active_value_lookup.is_some()
            || self.value_lookup_pending;
        self.set_completion_visible(false);
        self.completion_manual_open = false;
        self.completions = std::rc::Rc::new(Vec::new());
        self.completion_index = 0;
        self.completion_scroll_offset = 0;
        self.completion_replace_start = 0;
        self.active_value_lookup = None;
        self.value_lookup_pending = false;
        self.value_lookup_base_items.clear();
        self.value_lookup_values.clear();
        self.value_lookup_fk = None;
        self.value_lookup_fetched_partial.clear();
        self.value_lookup_truncated = false;
        self.last_completion_trigger = None;
        if changed {
            cx.notify();
        }
    }

    fn accept_completion(&mut self, cx: &mut Context<Self>) {
        if self.completion_visible && self.completion_index < self.completions.len() {
            let item = self.completions[self.completion_index].clone();
            let sql_before_accept = self.content.clone();
            let cursor_before_accept = self.cursor;
            let multi_cursor = !self.extra_cursors.is_empty();
            let insert_text = accepted_completion_insert_text(
                &item,
                &self.editor_prefs,
                &self.content,
                self.cursor,
            );
            // Prefer per-item replace_span, then response-level
            // completion_replace_start, then fall back to word_start_before_cursor.
            let primary_replace_start = item
                .replace_span
                .map(|(s, _)| s)
                .unwrap_or(self.completion_replace_start)
                .min(
                    self.word_start_before_cursor()
                        .max(self.completion_replace_start),
                );
            let primary_replace_end = item.replace_span.map(|(_, e)| e).unwrap_or(self.cursor);
            if item.kind == CompletionKind::Snippet && crate::snippets::has_tab_stops(&insert_text)
            {
                let range =
                    utf8_safe_edit_range(&self.content, primary_replace_start..primary_replace_end);
                self.close_completions(cx);
                self.expand_snippet(range, &insert_text, cx);
                self.accepted_completion_at = Some((self.content_revision, self.cursor));
                return;
            }
            let insert_text = if multi_cursor {
                insert_text
            } else {
                self.formatted_completion_insert(
                    primary_replace_start,
                    primary_replace_end,
                    insert_text,
                )
            };
            let (cursors, primary_index) = self.all_cursor_points();
            let edits = cursors
                .iter()
                .enumerate()
                .map(|(index, cursor)| {
                    let range = if index == primary_index {
                        primary_replace_start..primary_replace_end
                    } else {
                        cursor.selection().unwrap_or_else(|| {
                            word_start_before(&self.content, cursor.cursor)..cursor.cursor
                        })
                    };
                    (
                        utf8_safe_edit_range(&self.content, range),
                        insert_text.clone(),
                        index == primary_index,
                    )
                })
                .collect();
            self.apply_edits_at_all_cursors(edits, cx);
            self.accepted_completion_at = Some((self.content_revision, self.cursor));
            if !matches!(
                item.kind,
                CompletionKind::Keyword | CompletionKind::Value | CompletionKind::Snippet
            ) && let Ok(mut history) = self.accept_history.lock()
            {
                history.record(&item.label);
            }
            self.close_completions(cx);
            if !multi_cursor {
                self.suggest_table_alias_after_completion(
                    &item,
                    &sql_before_accept,
                    cursor_before_accept,
                    cx,
                );
            }
        }
    }

    /// The completion text laid out by the SQL formatting settings for the
    /// statement it lands in (see [`format_insert_in_context`]); the raw text
    /// when formatting does not apply.
    fn formatted_completion_insert(
        &self,
        replace_start: usize,
        replace_end: usize,
        insert: String,
    ) -> String {
        if self.database_type.is_command_engine() {
            return insert;
        }
        let statement_start = self
            .with_statement_spans(|spans| {
                let idx = statements::statement_index_at(spans, replace_start)?;
                Some(statements::span_content_start(&self.content, spans[idx]))
            })
            .filter(|start| *start <= replace_start)
            .unwrap_or_else(|| {
                // A fresh statement past the last span starts at the first
                // non-blank character of the trailing text.
                let tail = &self.content[..replace_start];
                tail.len() - tail.trim_start().len()
            });
        let statement_end = self
            .with_statement_spans(|spans| {
                let idx = statements::statement_index_at(spans, replace_start)?;
                Some(spans[idx].end)
            })
            .unwrap_or(self.content.len());
        let rest = utf8_safe_edit_range(
            &self.content,
            replace_end.min(statement_end)..statement_end.max(replace_end),
        );
        let prefs = crate::preferences::load_sql_formatting_preferences();
        let indent = crate::sql_formatter::EditorIndent {
            style: self.editor_prefs.indent_style,
            width: self.editor_prefs.spaces_per_tab(),
        };
        format_insert_in_context(
            &self.content,
            statement_start,
            replace_start,
            &insert,
            self.content[rest].trim_end(),
            &prefs,
            indent,
        )
        .unwrap_or(insert)
    }

    /// After a table/view completion was accepted without an automatically
    /// inserted alias, offer the capitals-rule alias as a one-tap follow-up
    /// suggestion anchored at the post-insert cursor.
    fn suggest_table_alias_after_completion(
        &mut self,
        item: &CompletionItem,
        sql: &str,
        cursor: usize,
        cx: &mut Context<Self>,
    ) {
        let Some((alias, table)) = suggested_table_alias(item, &self.editor_prefs, sql, cursor)
        else {
            return;
        };
        self.completions = std::rc::Rc::new(vec![CompletionItem {
            label: alias.clone(),
            insert_text: if self.editor_prefs.alias_as_keyword {
                format!(" AS {alias}")
            } else {
                format!(" {alias}")
            },
            kind: CompletionKind::Snippet,
            source: CompletionSource::Snippet,
            score: 0,
            detail: format!("table alias for {table}"),
            replace_span: Some((self.cursor, self.cursor)),
        }]);
        self.completion_index = 0;
        self.completion_scroll_offset = 0;
        self.completion_replace_start = self.cursor;
        self.completion_user_navigated = false;
        self.completion_manual_open = false;
        // Retire any automatic pass scheduled by the text change so the
        // injected suggestion survives until the next edit or dismissal.
        self.completion_generation += 1;
        self.set_completion_visible(true);
        cx.notify();
    }

    /// Advance the dotted path being typed by one segment: with the completion
    /// popup open and the visible candidates agreeing on the next segment (or
    /// the user having navigated to one), replace the partial segment with the
    /// canonical segment plus `.` (`Acc|` → `Accounting.|`) and immediately
    /// re-scope the popup below the committed dot. Returns `true` when a
    /// segment was committed; `false` means the caller should fall through to
    /// its normal behavior (insert the literal `.` / move the caret).
    fn try_advance_completion_to_dot(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.completion_visible
            || self.completions.is_empty()
            || self.active_value_lookup.is_some()
            || !self.extra_cursors.is_empty()
            || self.selection_anchor.is_some()
        {
            return false;
        }
        let cursor = self.cursor.min(self.content.len());
        // The partial segment is the identifier fragment left of the caret
        // (word_start_before stops at dots); the dotted qualifiers before it
        // are the already-committed path.
        let partial_start = word_start_before(&self.content, cursor);
        let mut path_start = partial_start;
        for (index, character) in self.content[..partial_start].char_indices().rev() {
            if character.is_ascii_alphanumeric()
                || character == '_'
                || character == '.'
                || character == '@'
            {
                path_start = index;
            } else {
                break;
            }
        }
        let path_text = &self.content[path_start..cursor];
        // Bail on parameter member access (`@x.`) and on quoted/bracketed
        // partial segments — the scan stops at the bracket or quote, so those
        // leave a leading dot in the scanned text or sit right before it.
        if path_text.starts_with('.')
            || path_text.starts_with('@')
            || self.content[..path_start]
                .chars()
                .next_back()
                .is_some_and(|ch| matches!(ch, '[' | '"' | '`'))
        {
            return false;
        }
        let mut parts = path_text.split('.');
        let partial = parts.next_back().unwrap_or_default();
        let committed: Vec<&str> = parts.filter(|segment| !segment.is_empty()).collect();
        // Only advance a segment the user has actually started typing. With an
        // empty partial (caret sitting right after a committed dot, `Accounting.|`,
        // popup still open) `.` should insert a literal dot and Right should move
        // the caret, rather than drilling into a guessed child segment.
        if partial.is_empty() {
            return false;
        }
        let Some(segment) = decide_dot_advance(
            &self.completions,
            self.completion_index,
            self.completion_user_navigated,
            &committed,
            partial,
        ) else {
            return false;
        };
        let range = utf8_safe_edit_range(&self.content, partial_start..cursor);
        // A segment such as `Order Details` is committed quoted, the same
        // way accepting the whole name would insert it.
        let segment = self.database_type.quote_identifier_if_needed(&segment);
        self.apply_edits_at_all_cursors(vec![(range, format!("{segment}."), true)], cx);
        // Supersede the automatic pass scheduled by the edit and immediately
        // re-scope the popup below the committed dot.
        self.completion_generation = self.completion_generation.wrapping_add(1);
        self.trigger_completions_with_kind(IntelliSenseTriggerKind::Dot, false, cx);
        true
    }

    /// Show a FIM (fill-in-the-middle) ghost-text suggestion at the cursor.
    /// The suggestion is rendered in a dim color and accepted via Tab.
    /// Mirrors the Swift `Editor.showFIMSuggestion(text)`.
    pub fn show_fim_suggestion(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        let text = text.into();
        if text.is_empty() {
            self.hide_fim_suggestion(cx);
            return;
        }
        self.fim_suggestion = Some(text);
        self.fim_visible = true;
        cx.notify();
    }

    /// Hide any visible FIM ghost-text suggestion. Mirrors the Swift
    /// `Editor.hideFIMSuggestion()`.
    pub fn hide_fim_suggestion(&mut self, cx: &mut Context<Self>) {
        if self.fim_visible {
            self.fim_visible = false;
            self.fim_suggestion = None;
            cx.notify();
        }
    }

    /// Accept the current FIM suggestion: insert the ghost text at the cursor
    /// and clear the suggestion. Returns `true` if a suggestion was accepted.
    /// Mirrors the Swift `Editor.acceptFIMSuggestion()`.
    pub fn accept_fim_suggestion(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.fim_visible {
            return false;
        }
        if let Some(text) = self.fim_suggestion.take() {
            self.fim_visible = false;
            self.insert_at_all_cursors(&text, cx);
            cx.emit(SqlEditorEvent::FimAccepted { text });
            return true;
        }
        false
    }

    /// Current FIM suggestion text, if any. `None` when no suggestion is
    /// visible.
    #[allow(dead_code)]
    pub fn fim_suggestion(&self) -> Option<&str> {
        if self.fim_visible {
            self.fim_suggestion.as_deref()
        } else {
            None
        }
    }

    fn word_start_before_cursor(&self) -> usize {
        word_start_before(&self.content, self.cursor)
    }

    fn move_completion_up(&mut self, cx: &mut Context<Self>) {
        if !self.completion_visible || self.completions.is_empty() {
            return;
        }
        if !self.completion_user_navigated {
            // No row is highlighted yet: Up enters the list from the bottom
            // (mirror of Down entering from the top), keeping the popup open.
            self.completion_user_navigated = true;
            self.completion_index = self.completions.len() - 1;
        } else if self.completion_index > 0 {
            self.completion_index -= 1;
        } else {
            return;
        }
        self.clamp_completion_scroll();
        cx.notify();
    }

    fn move_completion_down(&mut self, cx: &mut Context<Self>) {
        if !self.completion_visible || self.completions.is_empty() {
            return;
        }
        if !self.completion_user_navigated {
            // The popup opens with no row highlighted, so the first Down
            // selects the first item rather than skipping past it to the
            // second.
            self.completion_user_navigated = true;
            self.completion_index = 0;
        } else if self.completion_index < self.completions.len() - 1 {
            self.completion_index += 1;
        } else {
            return;
        }
        self.clamp_completion_scroll();
        cx.notify();
    }

    fn clamp_completion_scroll(&mut self) {
        self.completion_scroll_offset = clamp_scroll_offset(
            self.completion_index,
            self.completion_scroll_offset,
            COMPLETION_MAX_VISIBLE,
        );
    }

    fn after_text_change(&mut self, cx: &mut Context<Self>) {
        self.goal_column = None;
        self.hide_fim_suggestion(cx);
        self.scroll_cursor_into_view();
        self.result_source = None;
        self.clear_hover_ui();
        cx.emit(SqlEditorEvent::Change);
        // The workbench draws the editor from gpui's frame cache, so the
        // edit only reaches the screen once this entity is notified. Every
        // path below must repaint now, not when a debounced task lands.
        cx.notify();

        // Keep find-bar highlights in step with buffer edits (debounced, so
        // a typing burst rescans once).
        if self.find_bar.as_ref().is_some_and(|bar| bar.visible) {
            self.schedule_find_rescan(cx);
        }
        self.schedule_insert_hints(cx);

        // Typing a natural-language date: the card previews it; no
        // completion popup while the phrase is being typed.
        self.sync_date_helper();
        if self.date_helper.is_some() {
            cx.notify();
            return;
        }

        if self.active_value_lookup.is_some() {
            let still_in_value = self
                .active_value_lookup
                .as_ref()
                .map(|ctx| self.cursor >= ctx.replace_start)
                .unwrap_or(false);
            if !still_in_value {
                self.active_value_lookup = None;
                self.value_lookup_pending = false;
                self.value_lookup_base_items.clear();
                self.value_lookup_values.clear();
                self.value_lookup_fk = None;
                self.value_lookup_fetched_partial.clear();
                self.value_lookup_truncated = false;
            }
        }

        if self.active_value_lookup.is_some() {
            // Re-filter the fetched values against what the user has typed
            // since the lookup was triggered; only a capped sample that the
            // typed text has outgrown costs another DB query.
            self.rebuild_value_lookup_completions(cx);
            cx.notify();
            return;
        }

        // No auto-completion while an IME composition is in progress because
        // the preedit text is not committed input yet. With multiple cursors,
        // the popup is computed from the primary and acceptance fans the same
        // completion out using each caret's local replacement range.
        if self.services.is_some() && self.marked_range.is_none() {
            self.schedule_automatic_completion(cx);
        } else {
            cx.notify();
        }
    }

    /// Schedule a debounced automatic completion (120 ms after the last
    /// keystroke). Rapid edits supersede pending tasks via a generation
    /// counter — only the most recent schedule fires.
    fn schedule_automatic_completion(&mut self, cx: &mut Context<Self>) {
        self.completion_generation = self.completion_generation.wrapping_add(1);
        let generation = self.completion_generation;
        let weak_self = cx.weak_entity();
        let async_cx = cx.to_async();
        cx.spawn(async move |_this, _cx| {
            async_cx
                .background_executor()
                .timer(std::time::Duration::from_millis(120))
                .await;
            let mut cx = async_cx.clone();
            weak_self
                .update(&mut cx, |editor, cx| {
                    if editor.completion_generation != generation {
                        return;
                    }
                    if editor.accepted_completion_at
                        != Some((editor.content_revision, editor.cursor))
                    {
                        editor.trigger_completions_with(false, cx);
                    }
                    editor.refresh_signature_help(cx);
                })
                .ok();
        })
        .detach();
    }

    /// Schedule a debounced INSERT analysis pass (80 ms after the last
    /// keystroke, catalog/snapshot push, or preference toggle): the
    /// `column:` value inlays (behind their preference) and the
    /// IDENTITY_INSERT lint (always on for engines that need it). Rapid
    /// edits supersede pending tasks via a generation counter.
    fn schedule_insert_hints(&mut self, cx: &mut Context<Self>) {
        self.insert_hints_generation = self.insert_hints_generation.wrapping_add(1);
        let generation = self.insert_hints_generation;
        let weak_self = cx.weak_entity();
        let async_cx = cx.to_async();
        cx.spawn(async move |_this, _cx| {
            async_cx
                .background_executor()
                .timer(std::time::Duration::from_millis(80))
                .await;
            let mut cx = async_cx.clone();
            let work = weak_self
                .update(&mut cx, |editor, cx| {
                    if editor.insert_hints_generation != generation {
                        return None;
                    }
                    if !insert_hints::may_contain_insert(&editor.content) {
                        let had = !editor.insert_hints.is_empty();
                        editor.insert_hints = std::rc::Rc::new(Vec::new());
                        editor.insert_hints_revision = Some(editor.content_revision);
                        editor.insert_hints_version = editor.insert_hints_version.wrapping_add(1);
                        editor.apply_lint_diagnostics(Vec::new(), cx);
                        if had {
                            cx.notify();
                        }
                        return None;
                    }
                    Some((
                        editor.content.clone(),
                        editor.completion_catalog.clone(),
                        editor.content_revision,
                        editor.database_type.family(),
                        editor.schema_snapshot.clone(),
                        editor.editor_prefs.show_insert_value_hints,
                        editor.database_type.is_command_engine(),
                    ))
                })
                .ok()
                .flatten();
            let Some((
                content,
                catalog,
                revision,
                dialect,
                snapshot,
                hints_enabled,
                command_engine,
            )) = work
            else {
                return;
            };
            let (hints, lints) = async_cx
                .background_executor()
                .spawn(async move {
                    // Redis scripts are not SQL: no INSERT hints, no lints.
                    if command_engine {
                        return (Vec::new(), Vec::new());
                    }
                    let hints = if hints_enabled {
                        insert_hints::insert_hints_for_buffer(&content, dialect, &|table| {
                            insert_hints::catalog_columns_for(&catalog, table)
                        })
                    } else {
                        Vec::new()
                    };
                    let mut lints = Self::identity_lint_diagnostics(&content, dialect, &snapshot);
                    lints.extend(group_by_lint::group_by_lint_diagnostics(&content, dialect));
                    (hints, lints)
                })
                .await;
            let mut cx = async_cx.clone();
            weak_self
                .update(&mut cx, |editor, cx| {
                    if editor.insert_hints_generation != generation {
                        return;
                    }
                    if editor.content_revision != revision {
                        return;
                    }
                    editor.insert_hints = std::rc::Rc::new(hints);
                    editor.insert_hints_revision = Some(revision);
                    editor.insert_hints_version = editor.insert_hints_version.wrapping_add(1);
                    editor.apply_lint_diagnostics(lints, cx);
                    cx.notify();
                })
                .ok();
        })
        .detach();
    }

    /// Apply a diagnostic's "Fix this" link. The fix is recomputed from the
    /// current buffer (never from text captured when the diagnostic was
    /// painted) and applied as one undoable edit.
    fn apply_diagnostic_fix(&mut self, diag: &ValidationDiagnostic, cx: &mut Context<Self>) {
        let fixed = match &diag.fix {
            Some(DiagnosticFix::AddToGroupBy { .. }) => {
                group_by_lint::group_by_fix_at(&self.content, diag.start_byte)
            }
            None => None,
        };
        self.hovered_diagnostic = None;
        self.diagnostic_popup_bounds.set(None);
        self.diagnostic_fix_bounds.set(None);
        if let Some((sql, cursor)) = fixed {
            self.cursor = cursor;
            self.replace_text_with_history(sql, cx);
        }
        cx.notify();
    }

    /// Map IDENTITY_INSERT lint findings to warning diagnostics. Pure so the
    /// debounced task can run it off the main thread.
    fn identity_lint_diagnostics(
        content: &str,
        dialect: sqlly_lang::sqlmodel::Dialect,
        snapshot: &sqlly_lang::sqlmodel::ModelSnapshot,
    ) -> Vec<ValidationDiagnostic> {
        identity_lint::identity_insert_warnings(content, dialect, snapshot)
            .into_iter()
            .map(|finding| ValidationDiagnostic {
                severity: DiagnosticSeverity::Warning,
                source: DiagnosticSource::Lint,
                message: finding.message,
                line: content[..finding.start.min(content.len())]
                    .matches('\n')
                    .count(),
                identifier: Some(finding.table),
                start_byte: finding.start,
                end_byte: finding.end,
                explainer: None,
                fix: None,
            })
            .collect()
    }

    /// Synchronous lint recompute for tests.
    #[cfg(test)]
    fn recompute_identity_lint_now(&mut self, cx: &mut Context<Self>) {
        let mut lints = Self::identity_lint_diagnostics(
            &self.content,
            self.database_type.family(),
            &self.schema_snapshot.clone(),
        );
        lints.extend(group_by_lint::group_by_lint_diagnostics(
            &self.content,
            self.database_type.family(),
        ));
        self.apply_lint_diagnostics(lints, cx);
    }

    /// Hints for the current buffer revision only. Stale offsets from a
    /// previous edit must never be painted.
    fn insert_hints_snapshot(&self) -> std::rc::Rc<Vec<insert_hints::InsertHint>> {
        if self.insert_hints_revision == Some(self.content_revision) {
            self.insert_hints.clone()
        } else {
            std::rc::Rc::new(Vec::new())
        }
    }

    fn recompute_insert_hints_now(&mut self) {
        if !self.editor_prefs.show_insert_value_hints
            || !insert_hints::may_contain_insert(&self.content)
        {
            self.insert_hints = std::rc::Rc::new(Vec::new());
            self.insert_hints_revision = Some(self.content_revision);
            self.insert_hints_version = self.insert_hints_version.wrapping_add(1);
            return;
        }
        let catalog = self.completion_catalog.clone();
        let hints = insert_hints::insert_hints_for_buffer(
            &self.content,
            self.database_type.family(),
            &|table| insert_hints::catalog_columns_for(&catalog, table),
        );
        self.insert_hints = std::rc::Rc::new(hints);
        self.insert_hints_revision = Some(self.content_revision);
        self.insert_hints_version = self.insert_hints_version.wrapping_add(1);
    }

    /// Recompute hints synchronously when the buffer is small and the cached
    /// set is stale (i.e. mid-keystroke, before the 80 ms debounce lands).
    /// Without this every keystroke blanks all labels and then pops them back,
    /// so the text and caret visibly shuffle left and right while typing.
    /// Large buffers keep the debounced background path and accept the blink.
    fn ensure_insert_hints_current(&mut self) {
        const SYNC_RECOMPUTE_LIMIT: usize = 64 * 1024;
        if self.insert_hints_revision == Some(self.content_revision)
            || self.content.len() > SYNC_RECOMPUTE_LIMIT
        {
            return;
        }
        self.recompute_insert_hints_now();
    }

    /// Snapshot for a non-coalescable edit (paste, indent, completion accept,
    /// formatting, …). Always pushes and closes any open typing group.
    fn push_history(&mut self) {
        self.undo_group = None;
        self.push_snapshot();
    }

    fn push_snapshot(&mut self) {
        self.undo_stack.push_back(EditorSnapshot {
            payload: SnapshotPayload::Full(self.content.clone()),
            cursor: self.cursor,
            selection_anchor: self.selection_anchor,
            extra_cursors: self.extra_cursors.clone(),
        });
        self.trim_undo_stack();
        self.redo_stack.clear();
    }

    /// Snapshot a single splice: `pre_range` is the span in the current
    /// (pre-edit) buffer that `new_len` bytes will occupy after the edit.
    fn push_delta(&mut self, pre_range: Range<usize>, new_len: usize) {
        self.snippet_session_after_edit(pre_range.clone(), new_len);
        self.undo_group = None;
        let start = pre_range.start.min(self.content.len());
        let end = pre_range.end.min(self.content.len()).max(start);
        let replaced_text = SharedString::from(self.content[start..end].to_string());
        self.undo_stack.push_back(EditorSnapshot {
            payload: SnapshotPayload::Delta {
                range: start..start + new_len,
                replaced_text,
            },
            cursor: self.cursor,
            selection_anchor: self.selection_anchor,
            extra_cursors: self.extra_cursors.clone(),
        });
        self.trim_undo_stack();
        self.redo_stack.clear();
    }

    fn trim_undo_stack(&mut self) {
        while self.undo_stack.len() > 1
            && (self.undo_stack.len() > MAX_UNDO_STACK
                || self
                    .undo_stack
                    .iter()
                    .map(|s| s.payload.retained_bytes())
                    .sum::<usize>()
                    > UNDO_BYTE_BUDGET)
        {
            self.undo_stack.pop_front();
        }
    }

    fn apply_snapshot(&mut self, snapshot: EditorSnapshot) {
        match snapshot.payload {
            SnapshotPayload::Full(content) => {
                self.content = content;
            }
            SnapshotPayload::Delta {
                range,
                replaced_text,
            } => {
                let start = range.start.min(self.content.len());
                let end = range.end.min(self.content.len()).max(start);
                let mut new_content =
                    String::with_capacity(self.content.len() - (end - start) + replaced_text.len());
                new_content.push_str(&self.content[..start]);
                new_content.push_str(&replaced_text);
                new_content.push_str(&self.content[end..]);
                self.content = new_content.into();
            }
        }
        self.invalidate_text_derived_caches();
        self.cursor = snapshot.cursor;
        self.selection_anchor = snapshot.selection_anchor;
        self.extra_cursors = snapshot.extra_cursors;
        self.last_added_extra = None;
        self.normalize_extra_cursors();
        if self.snippet_session.is_some() {
            self.snippet_session = None;
        }
    }

    fn capture_inverse(&self, snapshot: &EditorSnapshot) -> EditorSnapshot {
        match &snapshot.payload {
            SnapshotPayload::Full(_) => EditorSnapshot {
                payload: SnapshotPayload::Full(self.content.clone()),
                cursor: self.cursor,
                selection_anchor: self.selection_anchor,
                extra_cursors: self.extra_cursors.clone(),
            },
            SnapshotPayload::Delta {
                range,
                replaced_text,
            } => {
                let start = range.start.min(self.content.len());
                let end = range.end.min(self.content.len()).max(start);
                let new_text = SharedString::from(self.content[start..end].to_string());
                EditorSnapshot {
                    payload: SnapshotPayload::Delta {
                        range: start..start + replaced_text.len(),
                        replaced_text: new_text,
                    },
                    cursor: self.cursor,
                    selection_anchor: self.selection_anchor,
                    extra_cursors: self.extra_cursors.clone(),
                }
            }
        }
    }

    /// Snapshot for a single-char edit that may extend an open group. The
    /// edit continues the group when its kind matches, it lands at the open
    /// group's expected offset, and (for inserts) its whitespace class is
    /// unchanged — otherwise a fresh snapshot is pushed. `next` is the offset
    /// where the *following* edit of the same kind would land.
    fn push_history_grouped(
        &mut self,
        kind: UndoGroupKind,
        at: usize,
        next: usize,
        whitespace: bool,
    ) {
        let (pre_range, new_len) = if next >= at {
            (at..at, next - at)
        } else {
            (next..at, 0)
        };
        self.snippet_session_after_edit(pre_range, new_len);
        let continues = self.undo_group.is_some_and(|g| {
            g.kind == kind
                && g.at == at
                && (kind != UndoGroupKind::Insert || g.whitespace == whitespace)
        });
        if !continues {
            self.push_snapshot();
        }
        self.undo_group = Some(UndoGroupState {
            kind,
            at: next,
            whitespace,
        });
    }

    pub fn undo(&mut self, cx: &mut Context<Self>) {
        self.undo_group = None;
        let Some(snapshot) = self.undo_stack.pop_back() else {
            return;
        };
        let redo = self.capture_inverse(&snapshot);
        self.apply_snapshot(snapshot);
        self.redo_stack.push(redo);
        self.after_text_change(cx);
    }

    pub fn redo(&mut self, cx: &mut Context<Self>) {
        self.undo_group = None;
        let Some(snapshot) = self.redo_stack.pop() else {
            return;
        };
        let undo = self.capture_inverse(&snapshot);
        self.apply_snapshot(snapshot);
        self.undo_stack.push_back(undo);
        self.trim_undo_stack();
        self.after_text_change(cx);
    }

    pub fn set_text(&mut self, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        // Normalize CRLF/CR to LF: the whole editor (line index, column math,
        // rendering) assumes LF-only. Windows clipboards and files are the
        // common source of stray \r.
        let text: SharedString = text.into();
        self.content = if text.contains('\r') {
            text.replace("\r\n", "\n").replace('\r', "\n").into()
        } else {
            text
        };
        self.invalidate_text_derived_caches();
        // Clamp to length AND snap to a char boundary — shrinking content can
        // otherwise leave the caret mid-UTF-8-sequence, which panics later.
        let clamped = self.content.len().min(self.cursor);
        self.cursor = utf8_safe_edit_range(&self.content, clamped..clamped).start;
        self.marked_range = None;
        self.clear_extra_cursors();
        self.undo_group = None;
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.snippet_session = None;
        cx.notify();
    }

    /// Replace the whole document as ONE undoable edit, preserving prior undo
    /// history. Unlike [`set_text`] (which wipes the undo/redo stacks and is for
    /// loading a fresh buffer), this pushes a single snapshot of the current
    /// text first, so restoring a saved query version — or any programmatic full
    /// replace — can be reverted with a single Ctrl+Z back to exactly what was
    /// there before. Like `set_text`, it normalizes CRLF/CR to LF, clamps the
    /// caret to a char boundary, and drops selection/extra cursors, and it emits
    /// no `Change` event (callers drive follow-up work themselves).
    pub fn replace_document(&mut self, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        let text: SharedString = text.into();
        let normalized: SharedString = if text.contains('\r') {
            text.replace("\r\n", "\n").replace('\r', "\n").into()
        } else {
            text
        };
        // Identical content: no edit, so leave undo history untouched rather
        // than pushing a redundant snapshot.
        if normalized == self.content {
            return;
        }
        // One snapshot of the pre-edit buffer, with the typing group closed, so
        // the entire replace collapses to a single undo step; `push_snapshot`
        // already clears the redo stack. The undo stack is NOT cleared.
        self.push_history();
        self.content = normalized;
        self.invalidate_text_derived_caches();
        let clamped = self.content.len().min(self.cursor);
        self.cursor = utf8_safe_edit_range(&self.content, clamped..clamped).start;
        self.selection_anchor = None;
        self.marked_range = None;
        self.clear_extra_cursors();
        self.undo_group = None;
        self.snippet_session = None;
        cx.notify();
    }

    /// Insert `text` at the cursor, replacing any active selection. Public so
    /// the app layer can insert generated content (e.g. value-lookup results).
    /// Primary-only: app-driven inserts drop any extra (multi) cursors.
    pub fn insert(&mut self, text: &str, cx: &mut Context<Self>) {
        self.clear_extra_cursors();
        self.replace_selection(text, cx);
    }

    pub fn text(&self) -> &str {
        &self.content
    }

    /// Refcount clone of the buffer. Prefer this over `text().to_string()` on
    /// interactive hot paths that only need `&str` (procedure-mode head scan).
    pub fn shared_text(&self) -> SharedString {
        self.content.clone()
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Width of the editor's scroll viewport in pixels; 0.0 before the first
    /// layout. Used by the app layer to size width-dependent UI (e.g. the AI
    /// summary word budget).
    pub fn viewport_width(&self) -> f32 {
        f32::from(self.scroll_handle.bounds().size.width)
    }

    /// Build a compact, scalable image of the editor's current viewport for
    /// surfaces such as the Ctrl+Tab switcher. Only visible lines are encoded,
    /// keeping generation bounded for large query buffers.
    pub fn preview_svg(&self) -> Vec<u8> {
        let viewport = self.scroll_handle.bounds().size;
        let width = f32::from(viewport.width).max(1.0);
        let height = f32::from(viewport.height).max(1.0);
        let width = if width > 1.0 { width } else { 1200.0 };
        let height = if height > 1.0 { height } else { 800.0 };
        let line_height = f32::from(self.line_height).max(1.0);
        let font_size = f32::from(self.font_size).max(1.0);
        let scroll = self.scroll_handle.offset();
        let scroll_x = f32::from(scroll.x);
        let scroll_y = f32::from(scroll.y);
        let first_line = ((-scroll_y).max(0.0) / line_height).floor() as usize;
        let visible_count = (height / line_height).ceil() as usize + 1;
        let line_index = LineIndex::new(&self.content);
        let last_line = (first_line + visible_count).min(self.display_line_count());
        let gutter_width = if self.editor_prefs.show_line_numbers {
            f32::from(self.line_number_width)
        } else {
            0.0
        };

        let mut svg = String::with_capacity(4096);
        let _ = write!(
            svg,
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="{width:.1}" height="{height:.1}" viewBox="0 0 {width:.1} {height:.1}" preserveAspectRatio="xMidYMid meet">"#
        );
        let _ = write!(
            svg,
            r#"<rect width="100%" height="100%" fill="{}"/>"#,
            svg_color(Theme::bg())
        );
        if self.editor_prefs.show_line_numbers {
            let _ = write!(
                svg,
                r#"<rect width="{gutter_width:.1}" height="100%" fill="{}"/>"#,
                svg_color(Theme::sidebar_bg())
            );
        }

        svg.push_str(r#"<g font-family="#);
        write_xml_escaped(&mut svg, &Theme::editor_font_family());
        let _ = write!(svg, r#"" font-size="{font_size:.1}" xml:space="preserve">"#);

        let tab_width = self.editor_prefs.spaces_per_tab() as usize;
        for display_line in first_line..last_line {
            // The scroll axis is display space; map each slot back to the
            // buffer line painted there so the preview matches the editor
            // when folds are collapsed.
            let line_number = self.buffer_line_of_display(display_line);
            let line_start = line_index.line_start(line_number);
            let line_end = line_index.line_end(&self.content, line_number);
            let line_text = &self.content[line_start..line_end];
            let baseline = display_line as f32 * line_height + scroll_y + font_size;

            if self.editor_prefs.show_line_numbers {
                let _ = write!(
                    svg,
                    r#"<text x="8" y="{baseline:.1}" fill="{}">{}</text>"#,
                    svg_color(Theme::text_dim()),
                    line_number + 1
                );
            }

            let map = crate::tab_map::TabMap::new(line_text, tab_width);
            let runs = map.expand_runs(&Self::syntax_runs(
                line_text,
                &self.font,
                self.database_type.family(),
                self.comment_state_at(line_number),
                &self.editor_prefs.syntax_bold,
                &self.semantic_names(),
            ));
            let display = map.display();
            let x = gutter_width + 8.0 + scroll_x;
            let _ = write!(svg, r#"<text x="{x:.1}" y="{baseline:.1}">"#);
            let mut offset = 0;
            for run in runs {
                let end = (offset + run.len).min(display.len());
                if let Some(text) = display.get(offset..end) {
                    let _ = write!(svg, r#"<tspan fill="{}">"#, svg_color(run.color));
                    write_xml_escaped(&mut svg, text);
                    svg.push_str("</tspan>");
                }
                offset = end;
            }
            svg.push_str("</text>");
        }
        svg.push_str("</g></svg>");
        svg.into_bytes()
    }

    /// Take keyboard focus on the next render. Notifies, because the
    /// workbench draws the editor from gpui's frame cache: without it the
    /// request would sit until some unrelated notify rendered the editor.
    pub fn request_focus(&mut self, cx: &mut Context<Self>) {
        self.needs_focus = true;
        cx.notify();
    }

    /// Cancel the focus grab a newly created editor schedules for its first
    /// render. A supporting editor (the Settings SQL-formatting preview) must
    /// not pull focus away from whatever the user is typing in when its pane
    /// first appears.
    pub fn abandon_focus_request(&mut self) {
        self.needs_focus = false;
    }

    fn selection(&self) -> Option<Range<usize>> {
        self.selection_anchor.map(|anchor| {
            if anchor < self.cursor {
                anchor..self.cursor
            } else {
                self.cursor..anchor
            }
        })
    }

    /// Ranges of every *other* occurrence of the selected text, for the
    /// VS Code-style occurrence highlight painted under the selection.
    /// `None` when the feature is off, multi-cursor is live, the buffer is
    /// huge, or the selection is empty, multi-line, all-whitespace, a single
    /// byte, or longer than a short phrase.
    /// Results are cached on `(selection, content_revision)` so the paint
    /// pass rescans the buffer only when one of those changes.
    pub(super) fn occurrence_highlights(&self) -> Option<std::rc::Rc<Vec<Range<usize>>>> {
        const OCCURRENCE_LIMIT: usize = 5000;
        // A selection longer than this is a block, not a name worth
        // spotting elsewhere; a buffer larger than this would make every
        // selection change rescan megabytes on the paint path.
        const MAX_NEEDLE_BYTES: usize = 512;
        const MAX_SCAN_BYTES: usize = 8 * 1024 * 1024;
        if !self.editor_prefs.highlight_occurrences
            || !self.extra_cursors.is_empty()
            || self.content.len() > MAX_SCAN_BYTES
        {
            *self.occurrence_cache.borrow_mut() = None;
            return None;
        }
        let Some(sel) = self.selection() else {
            *self.occurrence_cache.borrow_mut() = None;
            return None;
        };
        let Some(needle) = self.content.get(sel.clone()) else {
            *self.occurrence_cache.borrow_mut() = None;
            return None;
        };
        if needle.len() < 2
            || needle.len() > MAX_NEEDLE_BYTES
            || needle.contains(['\n', '\r'])
            || needle.trim().is_empty()
        {
            *self.occurrence_cache.borrow_mut() = None;
            return None;
        }
        if let Some((start, end, revision, ranges)) = &*self.occurrence_cache.borrow()
            && *start == sel.start
            && *end == sel.end
            && *revision == self.content_revision
        {
            return Some(ranges.clone());
        }
        let mut ranges: Vec<Range<usize>> = Vec::new();
        let mut from = 0usize;
        while let Some(i) = self.content[from..].find(needle) {
            let start = from + i;
            let end = start + needle.len();
            // Skip the selection itself and any copy overlapping it (`aa`
            // selected inside `aaaa`), which would paint under the selection.
            if end <= sel.start || start >= sel.end {
                ranges.push(start..end);
                if ranges.len() >= OCCURRENCE_LIMIT {
                    break;
                }
            }
            from = end;
        }
        let ranges = std::rc::Rc::new(ranges);
        *self.occurrence_cache.borrow_mut() =
            Some((sel.start, sel.end, self.content_revision, ranges.clone()));
        Some(ranges)
    }

    pub fn selected_text(&self) -> Option<String> {
        self.selection().map(|r| self.content[r].to_string())
    }

    fn replace_selection(&mut self, text: &str, cx: &mut Context<Self>) {
        let range = self.selection().unwrap_or(self.cursor..self.cursor);
        let range = utf8_safe_edit_range(&self.content, range);
        let mut new_content = String::with_capacity(self.content.len() + text.len());
        new_content.push_str(&self.content[..range.start]);
        new_content.push_str(text);
        new_content.push_str(&self.content[range.end..]);
        // Plain typing (one non-newline char, nothing replaced) coalesces
        // into word-granular undo groups; anything else snapshots hard.
        let mut chars = text.chars();
        match (range.start == range.end, chars.next(), chars.next()) {
            (true, Some(c), None) if c != '\n' => self.push_history_grouped(
                UndoGroupKind::Insert,
                range.start,
                range.start + text.len(),
                c.is_whitespace(),
            ),
            _ => self.push_delta(range.clone(), text.len()),
        }
        self.content = new_content.into();
        self.invalidate_text_derived_caches();
        self.cursor = range.start + text.len();
        self.selection_anchor = None;
        self.after_text_change(cx);
    }

    fn delete_backward(&mut self, cx: &mut Context<Self>) {
        if !self.extra_cursors.is_empty() {
            self.delete_at_all_cursors(false, cx);
            return;
        }
        if let Some(_sel) = self.selection() {
            self.replace_selection("", cx);
        } else if self.cursor > 0 {
            let prev = self.previous_grapheme_boundary(self.cursor);
            let mut new_content = String::with_capacity(self.content.len());
            new_content.push_str(&self.content[..prev]);
            new_content.push_str(&self.content[self.cursor..]);
            self.push_history_grouped(UndoGroupKind::DeleteBackward, self.cursor, prev, false);
            self.content = new_content.into();
            self.invalidate_text_derived_caches();
            self.cursor = prev;
            self.selection_anchor = None;
            self.after_text_change(cx);
        }
    }

    // ── Multi-cursor editing ───────────────────────────────────────────────
    //
    // The primary caret lives in `cursor`/`selection_anchor` exactly as
    // before; `extra_cursors` holds the secondary carets. Only typing,
    // backspace/delete, and paste fan out to every caret — everything else
    // (completion accept, FIM, find/replace, formatting, comments, plain
    // movement, mouse clicks) operates on the primary and clears the extras.

    /// Drop every extra cursor (the primary survives untouched). Returns
    /// whether any were present so callers can skip a notify when nothing
    /// changed.
    fn clear_extra_cursors(&mut self) -> bool {
        self.last_added_extra = None;
        self.rectangular_anchor = None;
        self.rectangular_target = None;
        if self.extra_cursors.is_empty() {
            return false;
        }
        self.extra_cursors.clear();
        true
    }

    /// Enforce the extra-cursor invariants: ascending caret order, no
    /// duplicate carets, never on top of the primary caret, capped at
    /// [`MAX_EXTRA_CURSORS`]. Call after anything that adds or moves them.
    fn normalize_extra_cursors(&mut self) {
        let primary = self.cursor;
        self.extra_cursors.retain(|c| c.cursor != primary);
        self.extra_cursors.sort_by_key(|c| c.cursor);
        self.extra_cursors.dedup_by_key(|c| c.cursor);
        self.extra_cursors.truncate(MAX_EXTRA_CURSORS);
        if self.extra_cursors.is_empty() {
            self.last_added_extra = None;
        }
    }

    /// Add one extra caret, keeping the invariants. Adds beyond the cap are
    /// dropped silently.
    fn add_extra_cursor(&mut self, ec: ExtraCursor) {
        if self.extra_cursors.len() >= MAX_EXTRA_CURSORS {
            return;
        }
        self.extra_cursors.push(ec);
        self.normalize_extra_cursors();
    }

    /// Expand `body` (LSP snippet subset) over `replace`. On parse failure the
    /// markers are stripped and the text is inserted plainly. Multi-cursor
    /// extras are dropped first — snippet expansion is primary-only.
    pub(super) fn expand_snippet(
        &mut self,
        replace: Range<usize>,
        body: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        let replace = utf8_safe_edit_range(&self.content, replace);
        self.snippet_session = None;
        self.clear_extra_cursors();
        match crate::snippets::parse_snippet(body) {
            Err(_) => {
                self.selection_anchor = Some(replace.start);
                self.cursor = replace.end;
                self.replace_selection(&crate::snippets::strip_tab_stops(body), cx);
                false
            }
            Ok(parsed) => {
                let origin = replace.start;
                self.selection_anchor = Some(replace.start);
                self.cursor = replace.end;
                self.replace_selection(&parsed.text, cx);
                if parsed.stops.is_empty() {
                    self.cursor = origin + parsed.text.len();
                    self.selection_anchor = None;
                    return false;
                }
                let mut stops = parsed.stops;
                for stop in &mut stops {
                    for range in &mut stop.ranges {
                        range.start += origin;
                        range.end += origin;
                    }
                }
                self.snippet_session = Some(SnippetSession {
                    stops,
                    index: 0,
                    span: origin..origin + parsed.text.len(),
                });
                self.activate_snippet_stop(cx);
                true
            }
        }
    }

    fn activate_snippet_stop(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.snippet_session.as_ref() else {
            return;
        };
        let Some(stop) = session.stops.get(session.index).cloned() else {
            self.end_snippet_session();
            cx.notify();
            return;
        };
        self.clear_extra_cursors();
        let mut ranges = stop.ranges;
        if ranges.is_empty() {
            self.end_snippet_session();
            cx.notify();
            return;
        }
        let first = ranges.remove(0);
        if first.start == first.end {
            self.cursor = first.start.min(self.content.len());
            self.selection_anchor = None;
        } else {
            let start = first.start.min(self.content.len());
            let end = first.end.min(self.content.len()).max(start);
            self.selection_anchor = Some(start);
            self.cursor = end;
        }
        for range in ranges {
            self.add_extra_cursor(ExtraCursor {
                cursor: range.end.min(self.content.len()),
                anchor: if range.start == range.end {
                    None
                } else {
                    Some(range.start.min(self.content.len()))
                },
            });
        }
        if stop.number == 0 {
            self.end_snippet_session();
        }
        cx.notify();
    }

    pub(super) fn next_snippet_stop(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(session) = self.snippet_session.as_ref() else {
            return false;
        };
        if session.index + 1 >= session.stops.len() {
            self.end_snippet_session();
            cx.notify();
            return true;
        }
        if let Some(session) = self.snippet_session.as_mut() {
            session.index += 1;
        }
        self.activate_snippet_stop(cx);
        true
    }

    pub(super) fn previous_snippet_stop(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(session) = self.snippet_session.as_ref() else {
            return false;
        };
        if session.index == 0 {
            return true;
        }
        if let Some(session) = self.snippet_session.as_mut() {
            session.index -= 1;
        }
        self.activate_snippet_stop(cx);
        true
    }

    pub(super) fn end_snippet_session(&mut self) {
        self.snippet_session = None;
        self.clear_extra_cursors();
    }

    fn snippet_session_after_edit(&mut self, pre_range: Range<usize>, new_len: usize) {
        let Some(session) = self.snippet_session.as_ref() else {
            return;
        };
        if pre_range.start < session.span.start || pre_range.end > session.span.end {
            self.end_snippet_session();
            return;
        }
        let delta = new_len as isize - pre_range.len() as isize;
        let index = session.index;
        let Some(session) = self.snippet_session.as_mut() else {
            return;
        };
        let mut grown: Vec<(usize, usize)> = Vec::new();
        if let Some(stop) = session.stops.get_mut(index) {
            for (ri, range) in stop.ranges.iter_mut().enumerate() {
                if range.start <= pre_range.start && pre_range.end <= range.end {
                    range.end = (range.end as isize + delta).max(range.start as isize) as usize;
                    grown.push((index, ri));
                }
            }
        }
        for (si, stop) in session.stops.iter_mut().enumerate() {
            for (ri, range) in stop.ranges.iter_mut().enumerate() {
                if grown.contains(&(si, ri)) {
                    continue;
                }
                if range.start >= pre_range.end {
                    range.start = (range.start as isize + delta).max(0) as usize;
                    range.end = (range.end as isize + delta).max(range.start as isize) as usize;
                }
            }
        }
        session.span.end =
            (session.span.end as isize + delta).max(session.span.start as isize) as usize;
    }

    pub(super) fn try_expand_snippet_prefix(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.extra_cursors.is_empty() || self.selection_anchor.is_some() {
            return false;
        }
        let Some((start, prefix)) = self.snippet_prefix_before_cursor() else {
            return false;
        };
        let Some(body) = self.lookup_snippet_body(&prefix) else {
            return false;
        };
        self.expand_snippet(start..self.cursor, &body, cx);
        true
    }

    fn snippet_prefix_before_cursor(&self) -> Option<(usize, String)> {
        let bytes = self.content.as_bytes();
        let mut start = self.cursor.min(bytes.len());
        while start > 0 {
            let c = bytes[start - 1];
            if c.is_ascii_alphanumeric() || c == b'_' || c == b'-' {
                start -= 1;
            } else {
                break;
            }
        }
        if start == self.cursor {
            return None;
        }
        Some((start, self.content[start..self.cursor].to_string()))
    }

    fn lookup_snippet_body(&self, prefix: &str) -> Option<String> {
        self.completion_catalog
            .snippets
            .iter()
            .rev()
            .find(|(p, _)| p.eq_ignore_ascii_case(prefix))
            .map(|(_, body)| body.clone())
    }

    /// Every caret (primary + extras) in ascending caret order, plus the
    /// primary's slot in that order.
    fn all_cursor_points(&self) -> (Vec<ExtraCursor>, usize) {
        let mut all = self.extra_cursors.clone();
        let idx = all.partition_point(|c| c.cursor < self.cursor);
        all.insert(
            idx,
            ExtraCursor {
                cursor: self.cursor,
                anchor: self.selection_anchor,
            },
        );
        (all, idx)
    }

    /// Apply one replacement per caret as a single undo step. `edits` is
    /// `(range, replacement, is_primary)` per caret; every range indexes the
    /// pre-edit buffer. The buffer is rebuilt in one ascending forward splice
    /// — equivalent to applying the edits back-to-front, since no offset in
    /// the batch is shifted by an earlier edit. Afterwards each caret is a
    /// plain caret at the end of its replacement; carets that collapse onto
    /// the same offset (e.g. backspace at adjacent carets) merge.
    fn apply_edits_at_all_cursors(
        &mut self,
        mut edits: Vec<(Range<usize>, String, bool)>,
        cx: &mut Context<Self>,
    ) {
        if edits.is_empty() {
            return;
        }
        edits.sort_by_key(|(r, _, _)| (r.start, r.end));
        // Defensive: invariants keep selections disjoint, but never let an
        // overlapping pair double-splice.
        for i in 1..edits.len() {
            let prev_end = edits[i - 1].0.end;
            let r = &mut edits[i].0;
            if r.start < prev_end {
                r.start = prev_end.min(r.end);
            }
        }
        if edits
            .iter()
            .all(|(r, t, _)| r.start == r.end && t.is_empty())
        {
            return; // e.g. backspace with every caret at offset 0
        }
        if self.snippet_session.is_some() {
            for (range, text, _) in edits.iter().rev() {
                self.snippet_session_after_edit(range.clone(), text.len());
                if self.snippet_session.is_none() {
                    break;
                }
            }
        }
        self.push_history();
        let added: usize = edits.iter().map(|(_, t, _)| t.len()).sum();
        let mut new_content = String::with_capacity(self.content.len() + added);
        let mut carets: Vec<(usize, bool)> = Vec::with_capacity(edits.len());
        let mut last = 0usize;
        for (range, text, is_primary) in &edits {
            new_content.push_str(&self.content[last..range.start]);
            new_content.push_str(text);
            carets.push((new_content.len(), *is_primary));
            last = range.end;
        }
        new_content.push_str(&self.content[last..]);
        self.content = new_content.into();
        self.invalidate_text_derived_caches();
        self.cursor = carets
            .iter()
            .find(|(_, p)| *p)
            .map(|(c, _)| *c)
            .unwrap_or(0);
        self.selection_anchor = None;
        self.extra_cursors = carets
            .iter()
            .filter(|(_, p)| !*p)
            .map(|(c, _)| ExtraCursor::caret(*c))
            .collect();
        self.last_added_extra = None;
        self.normalize_extra_cursors();
        self.after_text_change(cx);
    }

    /// Insert `text` at every caret, replacing per-caret selections — the
    /// multi-cursor arm of typing and uniform paste.
    fn insert_at_all_cursors(&mut self, text: &str, cx: &mut Context<Self>) {
        let (all, primary_idx) = self.all_cursor_points();
        let edits = all
            .iter()
            .enumerate()
            .map(|(i, c)| {
                (
                    utf8_safe_edit_range(&self.content, c.edit_range()),
                    text.to_string(),
                    i == primary_idx,
                )
            })
            .collect();
        self.apply_edits_at_all_cursors(edits, cx);
    }

    /// Backspace/forward-delete at every caret: a caret with a selection
    /// deletes it, a plain caret deletes one grapheme.
    fn delete_at_all_cursors(&mut self, forward: bool, cx: &mut Context<Self>) {
        let (all, primary_idx) = self.all_cursor_points();
        let edits = all
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let range = if let Some(sel) = c.selection() {
                    sel
                } else if forward {
                    c.cursor..self.next_grapheme_boundary(c.cursor)
                } else {
                    self.previous_grapheme_boundary(c.cursor)..c.cursor
                };
                (
                    utf8_safe_edit_range(&self.content, range),
                    String::new(),
                    i == primary_idx,
                )
            })
            .collect();
        self.apply_edits_at_all_cursors(edits, cx);
    }

    /// Ensure the primary caret has a selection to match against: when it has
    /// none, select the word-ish token under (or just left of) the caret,
    /// mirroring double-click selection. Returns false when there is nothing
    /// selectable (whitespace-only surroundings, empty buffer).
    fn select_word_under_cursor(&mut self, cx: &mut Context<Self>) -> bool {
        let (s, e) = word_selection_range(&self.content, self.cursor);
        if s == e {
            return false;
        }
        self.selection_anchor = Some(s);
        self.cursor = e;
        cx.notify();
        true
    }

    /// Cmd+D: with a selection, add a caret + matching selection on the next
    /// occurrence of the selected text (wrapping past the end); with no
    /// selection, first select the word under the caret (the next press then
    /// starts adding occurrences) — VS Code semantics.
    fn add_cursor_for_next_occurrence(&mut self, cx: &mut Context<Self>) {
        self.close_completions(cx);
        self.hide_fim_suggestion(cx);
        let Some(primary) = self.selection() else {
            self.select_word_under_cursor(cx);
            return;
        };
        let needle = self.content[primary.clone()].to_string();
        if needle.is_empty() {
            return;
        }
        // Occurrences already claimed by a caret's selection.
        let claimed: Vec<usize> = std::iter::once(primary.start)
            .chain(
                self.extra_cursors
                    .iter()
                    .filter_map(|c| c.selection().map(|s| s.start)),
            )
            .collect();
        // Scan forward from the furthest caret, wrapping once.
        let scan_from = std::iter::once(primary.end)
            .chain(self.extra_cursors.iter().map(|c| c.cursor))
            .max()
            .unwrap_or(primary.end);
        let Some(start) =
            find_next_unclaimed_occurrence(&self.content, &needle, scan_from, &claimed)
        else {
            return;
        };
        if self.extra_cursors.len() >= MAX_EXTRA_CURSORS {
            return;
        }
        let caret = start + needle.len();
        self.add_extra_cursor(ExtraCursor {
            cursor: caret,
            anchor: Some(start),
        });
        self.last_added_extra = Some(caret);
        cx.notify();
    }

    /// Cmd+Shift+L: carets + selections on every occurrence of the selected
    /// text (or the word under the caret). The primary keeps its occurrence;
    /// the rest become extras, capped at [`MAX_EXTRA_CURSORS`].
    fn select_all_occurrences(&mut self, cx: &mut Context<Self>) {
        self.close_completions(cx);
        self.hide_fim_suggestion(cx);
        if self.selection().is_none() && !self.select_word_under_cursor(cx) {
            return;
        }
        let Some(primary) = self.selection() else {
            return;
        };
        let needle = self.content[primary.clone()].to_string();
        if needle.is_empty() {
            return;
        }
        self.extra_cursors.clear();
        self.last_added_extra = None;
        let content = self.content.clone();
        for (start, _) in content.match_indices(needle.as_str()) {
            if start == primary.start {
                continue;
            }
            if self.extra_cursors.len() >= MAX_EXTRA_CURSORS {
                break;
            }
            self.extra_cursors.push(ExtraCursor {
                cursor: start + needle.len(),
                anchor: Some(start),
            });
        }
        self.normalize_extra_cursors();
        cx.notify();
    }

    /// Cmd+Alt+Up/Down: add a plain caret one display line above/below the
    /// most recently added caret (falling back to the primary), at the shared
    /// goal column — the same sticky column vertical movement uses.
    fn add_cursor_vertical(&mut self, above: bool, cx: &mut Context<Self>) {
        self.close_completions(cx);
        self.hide_fim_suggestion(cx);
        let reference = self
            .last_added_extra
            .filter(|off| self.extra_cursors.iter().any(|c| c.cursor == *off))
            .unwrap_or(self.cursor);
        let col = self
            .goal_column
            .unwrap_or_else(|| self.char_col_for_offset(reference));
        let (line, _) = self.line_col_for_offset(reference);
        let display = self.display_line_of(line);
        let target_display = if above {
            let Some(d) = display.checked_sub(1) else {
                return;
            };
            d
        } else {
            let d = display + 1;
            if d >= self.display_line_count() {
                return;
            }
            d
        };
        let target_line = self.buffer_line_of_display(target_display);
        let offset = self.offset_for_char_col(target_line, col);
        if self.extra_cursors.len() >= MAX_EXTRA_CURSORS {
            return;
        }
        self.goal_column = Some(col);
        self.add_extra_cursor(ExtraCursor::caret(offset));
        self.last_added_extra = Some(offset);
        cx.notify();
    }

    fn delete_forward(&mut self, cx: &mut Context<Self>) {
        if !self.extra_cursors.is_empty() {
            self.delete_at_all_cursors(true, cx);
            return;
        }
        if let Some(_sel) = self.selection() {
            self.replace_selection("", cx);
        } else if self.cursor < self.content.len() {
            let next = self.next_grapheme_boundary(self.cursor);
            let mut new_content = String::with_capacity(self.content.len());
            new_content.push_str(&self.content[..self.cursor]);
            new_content.push_str(&self.content[next..]);
            self.push_history_grouped(
                UndoGroupKind::DeleteForward,
                self.cursor,
                self.cursor,
                false,
            );
            self.content = new_content.into();
            self.invalidate_text_derived_caches();
            self.selection_anchor = None;
            self.after_text_change(cx);
        }
    }

    fn move_left(&mut self, select: bool, cx: &mut Context<Self>) {
        self.close_completions(cx);
        self.hide_fim_suggestion(cx);
        self.clear_extra_cursors();
        self.goal_column = None;
        if !select {
            if let Some(sel) = self.selection() {
                self.cursor = sel.start;
                self.selection_anchor = None;
            } else if self.cursor > 0 {
                self.cursor = self.previous_grapheme_boundary(self.cursor);
            }
        } else if self.cursor > 0 {
            if self.selection_anchor.is_none() {
                self.selection_anchor = Some(self.cursor);
            }
            self.cursor = self.previous_grapheme_boundary(self.cursor);
        }
        self.scroll_cursor_into_view();
        self.refresh_signature_help(cx);
        cx.notify();
    }

    fn move_right(&mut self, select: bool, cx: &mut Context<Self>) {
        self.close_completions(cx);
        self.hide_fim_suggestion(cx);
        self.clear_extra_cursors();
        self.goal_column = None;
        if !select {
            if let Some(sel) = self.selection() {
                self.cursor = sel.end;
                self.selection_anchor = None;
            } else if self.cursor < self.content.len() {
                self.cursor = self.next_grapheme_boundary(self.cursor);
            }
        } else if self.cursor < self.content.len() {
            if self.selection_anchor.is_none() {
                self.selection_anchor = Some(self.cursor);
            }
            self.cursor = self.next_grapheme_boundary(self.cursor);
        }
        self.scroll_cursor_into_view();
        self.refresh_signature_help(cx);
        cx.notify();
    }

    /// The cached newline index for the current content, rebuilding at most
    /// once per edit (`content_revision`-keyed).
    fn line_index(&self) -> std::rc::Rc<LineIndex> {
        let mut cache = self.line_index_cache.borrow_mut();
        if let Some((rev, idx)) = cache.as_ref()
            && *rev == self.content_revision
        {
            return idx.clone();
        }
        let idx = std::rc::Rc::new(LineIndex::new(&self.content));
        *cache = Some((self.content_revision, idx.clone()));
        idx
    }

    /// Per-line block-comment depths for the current content and dialect,
    /// rebuilt at most once per edit (or engine switch).
    pub(super) fn comment_states(&self) -> std::rc::Rc<Vec<crate::sql_syntax::CommentState>> {
        let dialect = self.database_type.family();
        let mut cache = self.comment_state_cache.borrow_mut();
        if let Some((rev, cached_dialect, states)) = cache.as_ref()
            && *rev == self.content_revision
            && *cached_dialect == dialect
        {
            return states.clone();
        }
        let states = std::rc::Rc::new(crate::sql_syntax::comment_states(dialect, &self.content));
        *cache = Some((self.content_revision, dialect, states.clone()));
        states
    }

    /// The state line `line` starts in — `default` for any line past the end.
    pub(super) fn comment_state_at(&self, line: usize) -> crate::sql_syntax::CommentState {
        self.comment_states().get(line).copied().unwrap_or_default()
    }

    fn line_number_label(&self, n: usize) -> SharedString {
        if n == 0 {
            return SharedString::from("0");
        }
        let mut labels = self.line_number_labels.borrow_mut();
        while labels.len() < n {
            let next = labels.len() + 1;
            labels.push(SharedString::from(next.to_string()));
        }
        labels[n - 1].clone()
    }

    // ── Code folding ───────────────────────────────────────────────────────
    //
    // Folding is display-only: the buffer, cursor, selection, and undo all
    // stay byte-accurate on the full text. See `folding.rs` for the scanner
    // and the display map. All access goes through `with_folds`, which
    // reconciles regions against the current content revision lazily.

    fn with_folds<R>(&self, f: impl FnOnce(&mut crate::folding::EditorFolds) -> R) -> R {
        let mut cache = self.folds.borrow_mut();
        if cache.0 != Some(self.content_revision) {
            cache.1.reconcile(&self.content);
            cache.0 = Some(self.content_revision);
        }
        f(&mut cache.1)
    }

    fn with_statement_spans<R>(&self, f: impl FnOnce(&[statements::StatementSpan]) -> R) -> R {
        let mut cache = self.statement_cache.borrow_mut();
        if cache.0 != Some(self.content_revision) {
            cache.1 = if self.database_type.is_command_engine() {
                // One command per line.
                crate::mutation_review::command_line_spans(&self.content)
                    .into_iter()
                    .map(|(start, end)| statements::StatementSpan { start, end })
                    .collect()
            } else {
                statements::statement_spans_for(&self.content, self.database_type.family())
            };
            cache.0 = Some(self.content_revision);
        }
        f(&cache.1)
    }

    /// Byte span of the statement under the caret. `None` while a selection
    /// exists (the frame is hidden) or when the caret is not inside a
    /// runnable statement.
    pub fn statement_span_at_cursor(&self) -> Option<(usize, usize)> {
        if self.has_selection() {
            return None;
        }
        self.with_statement_spans(|spans| {
            let idx = statements::statement_index_at(spans, self.cursor)?;
            let span = spans[idx];
            Some((
                statements::span_content_start(&self.content, span),
                span.end,
            ))
        })
    }

    pub fn set_result_source(&mut self, src: Option<ResultSource>, cx: &mut Context<Self>) {
        self.result_source = src;
        cx.notify();
    }

    pub fn result_source(&self) -> Option<&ResultSource> {
        self.result_source.as_ref()
    }

    /// Buffer lines that head a runnable statement (play-glyph rows).
    pub fn statement_first_lines(&self) -> Vec<usize> {
        self.with_statement_spans(|spans| {
            let idx = self.line_index();
            spans
                .iter()
                .map(|span| {
                    let off = statements::span_content_start(&self.content, *span);
                    idx.line_col_for_offset(off).0
                })
                .collect()
        })
    }

    fn display_line_heads_statement(&self, display: usize) -> bool {
        let buffer_line = self.buffer_line_of_display(display);
        self.statement_first_lines().contains(&buffer_line)
    }

    /// Play-column hit that maps to a statement's first non-blank byte.
    pub(crate) fn gutter_play_target(&self, position: Point<Pixels>) -> Option<usize> {
        if !self.editor_prefs.show_line_numbers {
            return None;
        }
        let bounds = self.last_bounds?;
        let x = f32::from(position.x - bounds.origin.x);
        let y = f32::from(position.y - bounds.origin.y);
        let display = statements::play_glyph_hit(
            x,
            y,
            f32::from(self.line_height),
            self.display_line_count(),
        )?;
        let buffer_line = self.buffer_line_of_display(display);
        self.with_statement_spans(|spans| {
            let idx = self.line_index();
            spans.iter().find_map(|span| {
                let off = statements::span_content_start(&self.content, *span);
                let first = idx.line_col_for_offset(off).0;
                (first == buffer_line).then_some(off)
            })
        })
    }

    /// Number of lines the editor currently displays (total minus collapsed).
    fn display_line_count(&self) -> usize {
        let total = self.line_count();
        self.with_folds(|f| f.map().display_line_count(total))
    }

    /// Display index a buffer line paints at. Hidden lines resolve to their
    /// fold header's display index.
    fn display_line_of(&self, line: usize) -> usize {
        self.with_folds(|f| f.map().buffer_to_display(line))
    }

    /// Buffer line shown at a display index, clamped to the buffer.
    fn buffer_line_of_display(&self, display_line: usize) -> usize {
        let line = self.with_folds(|f| f.map().display_to_buffer(display_line));
        line.min(self.line_count().saturating_sub(1))
    }

    /// Snapshot of the current display map for a paint pass.
    fn fold_map_snapshot(&self) -> crate::folding::FoldMap {
        self.with_folds(|f| f.map().clone())
    }

    /// Expand any collapsed fold hiding the cursor's line. Called from
    /// prepaint, so no matter how the cursor got there (find navigation,
    /// undo, programmatic moves), the next frame shows it.
    fn reveal_cursor_folds(&mut self) -> bool {
        let (line, _) = self.line_col_for_offset(self.cursor);
        self.with_folds(|f| f.reveal_line(line))
    }

    /// Toggle the fold headed at `line` (the gutter chevron click).
    pub fn toggle_fold_at_line(&mut self, line: usize, cx: &mut Context<Self>) {
        if self.with_folds(|f| f.toggle_at(line)) {
            self.keep_cursor_out_of_folds();
            cx.notify();
        }
    }

    /// After collapsing, a cursor inside the hidden range would edit invisible
    /// text; park it at the end of the fold header instead (VS Code behavior).
    fn keep_cursor_out_of_folds(&mut self) {
        let (line, _) = self.line_col_for_offset(self.cursor);
        let hidden = self.with_folds(|f| f.map().is_hidden(line));
        if hidden {
            let header = self.buffer_line_of_display(self.display_line_of(line));
            self.cursor = self.line_end_offset(header);
            self.selection_anchor = None;
        }
    }

    fn fold_at_cursor(&mut self, cx: &mut Context<Self>) {
        let (line, _) = self.line_col_for_offset(self.cursor);
        if self.with_folds(|f| f.collapse_at(line)).is_some() {
            self.keep_cursor_out_of_folds();
            self.scroll_cursor_into_view();
            cx.notify();
        }
    }

    fn unfold_at_cursor(&mut self, cx: &mut Context<Self>) {
        let (line, _) = self.line_col_for_offset(self.cursor);
        if self.with_folds(|f| f.expand_at(line)) {
            cx.notify();
        }
    }

    fn fold_all_regions(&mut self, cx: &mut Context<Self>) {
        self.with_folds(|f| f.collapse_all());
        self.keep_cursor_out_of_folds();
        self.scroll_cursor_into_view();
        cx.notify();
    }

    fn unfold_all_regions(&mut self, cx: &mut Context<Self>) {
        self.with_folds(|f| f.expand_all());
        cx.notify();
    }

    /// Widest line in display columns (tabs expanded to `tab_width`, INSERT
    /// value labels included — they widen the display line, and a width that
    /// ignored them would clip the right end of hinted lines out of the
    /// horizontal scroll range). Cached per `(content_revision, tab_width,
    /// insert_hints_version)` so the full-document scan runs once per
    /// edit/hint change rather than once per frame.
    fn max_display_cols(&self, tab_width: usize) -> usize {
        let hints = self.insert_hints_snapshot();
        let mut cache = self.max_display_cols_cache.borrow_mut();
        if let Some((rev, width, hints_version, cols)) = cache.as_ref()
            && *rev == self.content_revision
            && *width == tab_width
            && *hints_version == self.insert_hints_version
        {
            return *cols;
        }
        let mut cols = 0usize;
        let mut hint_i = 0usize;
        let mut line_start = 0usize;
        for line in self.content.split('\n') {
            let line_end = line_start + line.len();
            let tabs = line.matches('\t').count();
            // Hints are sorted by offset; each label paints as "label + one
            // space" inside this line's display form.
            let mut inlay_chars = 0usize;
            while hint_i < hints.len() && hints[hint_i].offset < line_end {
                inlay_chars += hints[hint_i].label.chars().count() + 1;
                hint_i += 1;
            }
            cols =
                cols.max(line.chars().count() + tabs * tab_width.saturating_sub(1) + inlay_chars);
            line_start = line_end + 1;
        }
        *cache = Some((
            self.content_revision,
            tab_width,
            self.insert_hints_version,
            cols,
        ));
        cols
    }

    fn line_col_for_offset(&self, offset: usize) -> (usize, usize) {
        self.line_index().line_col_for_offset(offset)
    }

    fn line_start_offset(&self, line: usize) -> usize {
        self.line_index().line_start(line)
    }

    fn line_end_offset(&self, line: usize) -> usize {
        self.line_index().line_end(&self.content, line)
    }

    /// Char-based column of the cursor within its line — the unit for
    /// vertical caret movement. Byte columns are wrong for multibyte text:
    /// `min()`-ing a byte column onto another line can land inside a UTF-8
    /// sequence, and any later slice at that offset panics.
    fn char_col_for_offset(&self, offset: usize) -> usize {
        let (line, _) = self.line_col_for_offset(offset);
        let start = self.line_start_offset(line);
        self.content[start..offset].chars().count()
    }

    /// Byte offset of char-column `col` on `line`, clamped to the line end.
    /// Always a char boundary by construction.
    fn offset_for_char_col(&self, line: usize, col: usize) -> usize {
        let start = self.line_start_offset(line);
        let end = self.line_end_offset(line);
        self.content[start..end]
            .char_indices()
            .nth(col)
            .map(|(i, _)| start + i)
            .unwrap_or(end)
    }

    fn display_col_for_offset(&self, offset: usize) -> usize {
        let (line, _) = self.line_col_for_offset(offset);
        let start = self.line_start_offset(line);
        let width = self.editor_prefs.spaces_per_tab() as usize;
        self.content[start..offset]
            .chars()
            .fold(0usize, |column, character| {
                if character == '\t' {
                    column + (width - column % width)
                } else {
                    column + 1
                }
            })
    }

    fn offset_for_display_col(&self, line: usize, target: usize) -> usize {
        let start = self.line_start_offset(line);
        let end = self.line_end_offset(line);
        let width = self.editor_prefs.spaces_per_tab() as usize;
        let mut column = 0usize;
        for (relative, character) in self.content[start..end].char_indices() {
            if column >= target {
                return start + relative;
            }
            column += if character == '\t' {
                width - column % width
            } else {
                1
            };
        }
        end
    }

    /// Rebuild the active selections from an Option/Alt+Shift drag. The
    /// primary selection follows the pointer line and every other visible line
    /// becomes an extra cursor selection, capped by the normal multi-cursor
    /// limit. Folded lines stay folded because the rectangle walks display
    /// lines and maps only those back into the buffer.
    fn update_rectangular_selection(&mut self, target_offset: usize, cx: &mut Context<Self>) {
        let Some((anchor_display, anchor_column)) = self.rectangular_anchor else {
            return;
        };
        let (target_line, _) = self.line_col_for_offset(target_offset);
        let target_display = self.display_line_of(target_line);
        let target_column = self.display_col_for_offset(target_offset);
        let first_display = anchor_display.min(target_display);
        let last_display = anchor_display.max(target_display);
        let (left, right) = if anchor_column <= target_column {
            (anchor_column, target_column)
        } else {
            (target_column, anchor_column)
        };
        let forward = target_column >= anchor_column;
        let selection_for = |this: &Self, line: usize| {
            let left_offset = this.offset_for_display_col(line, left);
            let right_offset = this.offset_for_display_col(line, right);
            if forward {
                ExtraCursor {
                    cursor: right_offset,
                    anchor: Some(left_offset),
                }
            } else {
                ExtraCursor {
                    cursor: left_offset,
                    anchor: Some(right_offset),
                }
            }
        };

        let primary = selection_for(self, target_line);
        self.cursor = primary.cursor;
        self.selection_anchor = primary.anchor;
        self.extra_cursors.clear();
        for display in first_display..=last_display {
            if display == target_display || self.extra_cursors.len() >= MAX_EXTRA_CURSORS {
                continue;
            }
            let line = self.buffer_line_of_display(display);
            self.extra_cursors.push(selection_for(self, line));
        }
        self.last_added_extra = None;
        self.normalize_extra_cursors();
        self.rectangular_target = Some((target_display, target_column));
        cx.notify();
    }

    /// Shared body of the four vertical moves: land the caret on `target_line`
    /// at the sticky goal column (falling back to the current char column).
    fn move_vertical_to_line(&mut self, target_line: usize, select: bool, cx: &mut Context<Self>) {
        // Any caret jump invalidates the completion popup's replace range —
        // accepting after the jump would splice at a stale offset.
        self.close_completions(cx);
        self.hide_fim_suggestion(cx);
        self.clear_extra_cursors();
        let col = self
            .goal_column
            .unwrap_or_else(|| self.char_col_for_offset(self.cursor));
        let new_cursor = self.offset_for_char_col(target_line, col);
        if select {
            if self.selection_anchor.is_none() {
                self.selection_anchor = Some(self.cursor);
            }
        } else {
            self.selection_anchor = None;
        }
        self.cursor = new_cursor;
        self.goal_column = Some(col);
        self.scroll_cursor_into_view();
        self.refresh_signature_help(cx);
        cx.notify();
    }

    fn move_up(&mut self, select: bool, cx: &mut Context<Self>) {
        // Fold-aware: step through *display* lines so collapsed regions are
        // skipped instead of entered.
        let (line, _) = self.line_col_for_offset(self.cursor);
        let display = self.display_line_of(line);
        if display > 0 {
            let target = self.buffer_line_of_display(display - 1);
            self.move_vertical_to_line(target, select, cx);
        }
    }

    fn move_down(&mut self, select: bool, cx: &mut Context<Self>) {
        let (line, _) = self.line_col_for_offset(self.cursor);
        let display = self.display_line_of(line);
        if display + 1 < self.display_line_count() {
            let target = self.buffer_line_of_display(display + 1);
            self.move_vertical_to_line(target, select, cx);
        }
    }

    fn line_count(&self) -> usize {
        self.line_index().line_count()
    }

    fn move_home(&mut self, select: bool, cx: &mut Context<Self>) {
        self.close_completions(cx);
        self.hide_fim_suggestion(cx);
        self.clear_extra_cursors();
        self.goal_column = None;
        let (line, _) = self.line_col_for_offset(self.cursor);
        let start = self.line_start_offset(line);
        let end = self.line_end_offset(line);
        // Smart Home: first press lands on the first non-whitespace character
        // of the line; pressing again (or when already there) goes to column
        // 0. Matches VS Code / Visual Studio.
        let indent_end = start
            + self.content[start..end]
                .char_indices()
                .find(|(_, c)| !c.is_whitespace())
                .map(|(i, _)| i)
                .unwrap_or(0);
        let new_cursor = if self.cursor == indent_end {
            start
        } else {
            indent_end
        };
        if select {
            if self.selection_anchor.is_none() {
                self.selection_anchor = Some(self.cursor);
            }
        } else {
            self.selection_anchor = None;
        }
        self.cursor = new_cursor;
        self.scroll_cursor_into_view();
        self.refresh_signature_help(cx);
        cx.notify();
    }

    fn move_end(&mut self, select: bool, cx: &mut Context<Self>) {
        self.close_completions(cx);
        self.hide_fim_suggestion(cx);
        self.clear_extra_cursors();
        self.goal_column = None;
        let (line, _) = self.line_col_for_offset(self.cursor);
        let new_cursor = self.line_end_offset(line);
        if select {
            if self.selection_anchor.is_none() {
                self.selection_anchor = Some(self.cursor);
            }
        } else {
            self.selection_anchor = None;
        }
        self.cursor = new_cursor;
        self.scroll_cursor_into_view();
        self.refresh_signature_help(cx);
        cx.notify();
    }

    fn select_all(&mut self, cx: &mut Context<Self>) {
        self.clear_extra_cursors();
        self.selection_anchor = Some(0);
        self.cursor = self.content.len();
        cx.notify();
    }

    /// Visible lines in the editor viewport, used for page-up/page-down caret
    /// movement. The viewport is the scroll *container*'s bounds — the
    /// editor element itself is the full content (line_count × line_height).
    /// Falls back to 20 before the first layout.
    fn page_lines(&self) -> usize {
        let viewport_h = self.scroll_handle.bounds().size.height;
        if viewport_h > px(0.0) {
            ((viewport_h / self.line_height).max(1.0) as usize).max(1)
        } else {
            20
        }
    }

    fn scroll_cursor_into_view(&self) {
        let viewport = self.scroll_handle.bounds().size;
        if viewport.height <= px(0.0) {
            return;
        }
        let (cursor_line, cursor_col) = self.line_col_for_offset(self.cursor);
        // Vertical position is in *display* space so collapsed folds above
        // the cursor don't overshoot the scroll target.
        let cursor_y = self.line_height * self.display_line_of(cursor_line) as f32;
        let cursor_bottom = cursor_y + self.line_height;
        // gpui scroll offsets are ≤ 0 (content shifts up as you scroll
        // down), so the viewport top in content coordinates is -offset.y.
        let offset = self.scroll_handle.offset();
        let top = -offset.y;
        let mut new_offset = offset;
        if cursor_y < top {
            new_offset.y = -cursor_y;
        } else if cursor_bottom > top + viewport.height {
            new_offset.y = -(cursor_bottom - viewport.height);
        }

        // Horizontal follow: keep the caret inside the viewport with a small
        // margin, so End / long-line typing never leaves it offscreen. Uses
        // the last paint's shaped line; a line not yet laid out (big vertical
        // jump) self-corrects on the next call once it has been painted.
        if viewport.width > px(0.0)
            && let Some((layout, tab_map)) = self.layout_for_line(cursor_line)
        {
            let caret_x =
                self.line_number_width + layout.x_for_index(tab_map.to_display(cursor_col));
            let margin = px(32.0);
            let left = -offset.x;
            let right = left + viewport.width;
            if caret_x < left + margin {
                new_offset.x = -(caret_x - margin).max(px(0.0));
            } else if caret_x > right - margin {
                new_offset.x = -(caret_x + margin - viewport.width);
            }
        }

        if new_offset != offset {
            self.scroll_handle.set_offset(new_offset);
        }
    }

    fn move_page_down(&mut self, select: bool, cx: &mut Context<Self>) {
        let (line, _) = self.line_col_for_offset(self.cursor);
        let display = self.display_line_of(line);
        let last = self.display_line_count().saturating_sub(1);
        let target = self.buffer_line_of_display((display + self.page_lines()).min(last));
        self.move_vertical_to_line(target, select, cx);
    }

    fn move_page_up(&mut self, select: bool, cx: &mut Context<Self>) {
        let (line, _) = self.line_col_for_offset(self.cursor);
        let display = self.display_line_of(line);
        let target = self.buffer_line_of_display(display.saturating_sub(self.page_lines()));
        self.move_vertical_to_line(target, select, cx);
    }

    // ── Word / camel-hump / document navigation ───────────────────────────
    //
    // Word navigation treats `_` as a separator (snake_case-aware), so
    // `first_name` splits into `first` / `_` / `name`. Hump navigation moves
    // by camelCase subwords (`fooBar` -> `foo` | `Bar`) and also splits at
    // digit boundaries and `_`. Both skip whitespace then traverse one chunk.

    fn move_word_left(&mut self, select: bool, cx: &mut Context<Self>) {
        self.close_completions(cx);
        self.hide_fim_suggestion(cx);
        self.clear_extra_cursors();
        self.goal_column = None;
        let target = word_boundary_left(&self.content, self.cursor);
        let edge = self.selection().map(|r| r.start).unwrap_or(self.cursor);
        let (c, a) = move_caret(self.cursor, self.selection_anchor, select, target, edge);
        self.cursor = c;
        self.selection_anchor = a;
        self.scroll_cursor_into_view();
        cx.notify();
    }

    fn move_word_right(&mut self, select: bool, cx: &mut Context<Self>) {
        self.close_completions(cx);
        self.hide_fim_suggestion(cx);
        self.clear_extra_cursors();
        self.goal_column = None;
        let target = word_boundary_right(&self.content, self.cursor);
        let edge = self.selection().map(|r| r.end).unwrap_or(self.cursor);
        let (c, a) = move_caret(self.cursor, self.selection_anchor, select, target, edge);
        self.cursor = c;
        self.selection_anchor = a;
        self.scroll_cursor_into_view();
        cx.notify();
    }

    fn move_hump_left(&mut self, select: bool, cx: &mut Context<Self>) {
        self.close_completions(cx);
        self.hide_fim_suggestion(cx);
        self.clear_extra_cursors();
        self.goal_column = None;
        let target = hump_boundary_left(&self.content, self.cursor);
        let edge = self.selection().map(|r| r.start).unwrap_or(self.cursor);
        let (c, a) = move_caret(self.cursor, self.selection_anchor, select, target, edge);
        self.cursor = c;
        self.selection_anchor = a;
        self.scroll_cursor_into_view();
        cx.notify();
    }

    fn move_hump_right(&mut self, select: bool, cx: &mut Context<Self>) {
        self.close_completions(cx);
        self.hide_fim_suggestion(cx);
        self.clear_extra_cursors();
        self.goal_column = None;
        let target = hump_boundary_right(&self.content, self.cursor);
        let edge = self.selection().map(|r| r.end).unwrap_or(self.cursor);
        let (c, a) = move_caret(self.cursor, self.selection_anchor, select, target, edge);
        self.cursor = c;
        self.selection_anchor = a;
        self.scroll_cursor_into_view();
        cx.notify();
    }

    fn move_doc_start(&mut self, select: bool, cx: &mut Context<Self>) {
        self.close_completions(cx);
        self.hide_fim_suggestion(cx);
        self.clear_extra_cursors();
        self.goal_column = None;
        let edge = self.selection().map(|r| r.start).unwrap_or(self.cursor);
        let (c, a) = move_caret(self.cursor, self.selection_anchor, select, 0, edge);
        self.cursor = c;
        self.selection_anchor = a;
        self.scroll_cursor_into_view();
        cx.notify();
    }

    fn move_doc_end(&mut self, select: bool, cx: &mut Context<Self>) {
        self.close_completions(cx);
        self.hide_fim_suggestion(cx);
        self.clear_extra_cursors();
        self.goal_column = None;
        let target = self.content.len();
        let edge = self.selection().map(|r| r.end).unwrap_or(self.cursor);
        let (c, a) = move_caret(self.cursor, self.selection_anchor, select, target, edge);
        self.cursor = c;
        self.selection_anchor = a;
        self.scroll_cursor_into_view();
        cx.notify();
    }

    fn delete_word_backward(&mut self, cx: &mut Context<Self>) {
        self.clear_extra_cursors();
        if self.selection().is_some() {
            self.replace_selection("", cx);
            return;
        }
        let target = word_boundary_left(&self.content, self.cursor);
        if target < self.cursor {
            self.delete_range(target, self.cursor);
            self.cursor = target;
            self.selection_anchor = None;
            self.after_text_change(cx);
        }
    }

    fn delete_word_forward(&mut self, cx: &mut Context<Self>) {
        self.clear_extra_cursors();
        if self.selection().is_some() {
            self.replace_selection("", cx);
            return;
        }
        let target = word_boundary_right(&self.content, self.cursor);
        if target > self.cursor {
            self.delete_range(self.cursor, target);
            self.selection_anchor = None;
            self.after_text_change(cx);
        }
    }

    fn delete_to_line_start(&mut self, cx: &mut Context<Self>) {
        self.clear_extra_cursors();
        if self.selection().is_some() {
            self.replace_selection("", cx);
            return;
        }
        let (line, _) = self.line_col_for_offset(self.cursor);
        let target = self.line_start_offset(line);
        if target < self.cursor {
            self.delete_range(target, self.cursor);
            self.cursor = target;
            self.selection_anchor = None;
            self.after_text_change(cx);
        }
    }

    fn delete_to_line_end(&mut self, cx: &mut Context<Self>) {
        self.clear_extra_cursors();
        if self.selection().is_some() {
            self.replace_selection("", cx);
            return;
        }
        let (line, _) = self.line_col_for_offset(self.cursor);
        let target = self.line_end_offset(line);
        if target > self.cursor {
            self.delete_range(self.cursor, target);
            self.selection_anchor = None;
            self.after_text_change(cx);
        }
    }

    /// Remove `content[start..end]` without touching the cursor or anchor. The
    /// caller is responsible for repositioning the cursor and emitting the
    /// change via `after_text_change`.
    fn delete_range(&mut self, start: usize, end: usize) {
        let mut new_content = String::with_capacity(self.content.len());
        new_content.push_str(&self.content[..start]);
        new_content.push_str(&self.content[end..]);
        self.push_delta(start..end, 0);
        self.content = new_content.into();
        self.invalidate_text_derived_caches();
    }

    /// The first..=last line index covered by the current selection, or just the
    /// cursor's line when there is no selection. A selection ending exactly at a
    /// line start excludes that trailing line (standard editor behavior).
    fn affected_line_range(&self) -> (usize, usize) {
        let (start, end) = match self.selection() {
            Some(r) => (r.start, r.end),
            None => (self.cursor, self.cursor),
        };
        let (start_line, _) = self.line_col_for_offset(start);
        let (end_line, end_col) = self.line_col_for_offset(end);
        let end_line = if end_col == 0 && end_line > start_line {
            end_line - 1
        } else {
            end_line
        };
        (start_line, end_line.max(start_line))
    }

    /// Transform a single line's comment prefix. `mode` toggles, adds, or
    /// removes a SQL `-- ` line comment, preserving leading whitespace.
    fn transform_comment_line(line: &str, mode: CommentMode) -> String {
        let ws_end = line
            .char_indices()
            .take_while(|(_, c)| *c == ' ' || *c == '\t')
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        let ws = &line[..ws_end];
        let rest = &line[ws_end..];
        match mode {
            CommentMode::Add => format!("{ws}-- {rest}"),
            CommentMode::Remove => {
                if let Some(stripped) = rest.strip_prefix("-- ") {
                    format!("{ws}{stripped}")
                } else if let Some(stripped) = rest.strip_prefix("--") {
                    format!("{ws}{stripped}")
                } else {
                    line.to_string()
                }
            }
            CommentMode::Toggle => {
                if rest.starts_with("--") {
                    Self::transform_comment_line(line, CommentMode::Remove)
                } else {
                    Self::transform_comment_line(line, CommentMode::Add)
                }
            }
        }
    }

    fn edit_line_comments(&mut self, mode: CommentMode, cx: &mut Context<Self>) {
        self.clear_extra_cursors();
        let (start_line, end_line) = self.affected_line_range();
        let lines: Vec<&str> = self.content.split('\n').collect();
        if lines.is_empty() {
            return;
        }
        let mut new_lines: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
        for i in start_line..=end_line.min(lines.len() - 1) {
            new_lines[i] = Self::transform_comment_line(lines[i], mode);
        }
        let new_content = new_lines.join("\n");

        // Recompute offsets into the new content so the affected block stays
        // selected (lets the user toggle the same block again immediately).
        let mut first_start = 0usize;
        for line in new_lines.iter().take(start_line) {
            first_start += line.len() + 1;
        }
        let mut last_end = first_start;
        for line in new_lines.iter().take(end_line).skip(start_line) {
            last_end += line.len() + 1;
        }
        last_end += new_lines[end_line.min(new_lines.len() - 1)].len();

        self.push_history();
        self.content = new_content.into();
        self.invalidate_text_derived_caches();
        self.selection_anchor = Some(first_start);
        self.cursor = last_end;
        self.after_text_change(cx);
    }

    /// Byte offset of `(line, col)` in `lines`, clamping the column to the
    /// line's length and the line to the document.
    fn offset_at_line_col(lines: &[String], line: usize, col: usize) -> usize {
        let line = line.min(lines.len().saturating_sub(1));
        let mut offset = 0usize;
        for l in lines.iter().take(line) {
            offset += l.len() + 1;
        }
        offset + col.min(lines.get(line).map_or(0, |l| l.len()))
    }

    /// Move the current line (or every line the selection touches) up or
    /// down by one line, keeping the caret/selection on the moved block.
    fn move_lines(&mut self, up: bool, cx: &mut Context<Self>) {
        self.clear_extra_cursors();
        let (start_line, end_line) = self.affected_line_range();
        let mut lines: Vec<String> = self.content.split('\n').map(str::to_string).collect();
        if lines.is_empty() {
            return;
        }
        let end_line = end_line.min(lines.len() - 1);
        if up {
            if start_line == 0 {
                return;
            }
            let displaced = lines.remove(start_line - 1);
            lines.insert(end_line, displaced);
        } else {
            if end_line + 1 >= lines.len() {
                return;
            }
            let displaced = lines.remove(end_line + 1);
            lines.insert(start_line, displaced);
        }
        let shift: isize = if up { -1 } else { 1 };
        let (cursor_line, cursor_col) = self.line_col_for_offset(self.cursor);
        let anchor_line_col = self
            .selection_anchor
            .map(|anchor| self.line_col_for_offset(anchor));
        self.push_history();
        self.content = lines.join("\n").into();
        self.invalidate_text_derived_caches();
        self.cursor =
            Self::offset_at_line_col(&lines, cursor_line.saturating_add_signed(shift), cursor_col);
        self.selection_anchor = anchor_line_col.map(|(line, col)| {
            Self::offset_at_line_col(&lines, line.saturating_add_signed(shift), col)
        });
        self.scroll_cursor_into_view();
        self.after_text_change(cx);
    }

    /// Duplicate the current line (or the block of lines the selection
    /// touches) below itself, moving the caret/selection onto the copy.
    fn duplicate_line_or_selection(&mut self, cx: &mut Context<Self>) {
        self.clear_extra_cursors();
        let (start_line, end_line) = self.affected_line_range();
        let mut lines: Vec<String> = self.content.split('\n').map(str::to_string).collect();
        if lines.is_empty() {
            return;
        }
        let end_line = end_line.min(lines.len() - 1);
        let block: Vec<String> = lines[start_line..=end_line].to_vec();
        let block_len = block.len();
        for (i, line) in block.into_iter().enumerate() {
            lines.insert(end_line + 1 + i, line);
        }
        let (cursor_line, cursor_col) = self.line_col_for_offset(self.cursor);
        let anchor_line_col = self
            .selection_anchor
            .map(|anchor| self.line_col_for_offset(anchor));
        self.push_history();
        self.content = lines.join("\n").into();
        self.invalidate_text_derived_caches();
        self.cursor = Self::offset_at_line_col(&lines, cursor_line + block_len, cursor_col);
        self.selection_anchor = anchor_line_col
            .map(|(line, col)| Self::offset_at_line_col(&lines, line + block_len, col));
        self.scroll_cursor_into_view();
        self.after_text_change(cx);
    }

    /// Join the next line onto the current one (or collapse every line the
    /// selection touches into one), separating with a single space and
    /// trimming the joined line's leading whitespace.
    fn join_lines(&mut self, cx: &mut Context<Self>) {
        self.clear_extra_cursors();
        let (start_line, mut end_line) = self.affected_line_range();
        let lines: Vec<String> = self.content.split('\n').map(str::to_string).collect();
        if end_line <= start_line {
            end_line = start_line + 1;
        }
        if start_line >= lines.len().saturating_sub(1) {
            return;
        }
        let end_line = end_line.min(lines.len() - 1);
        let mut joined = lines[start_line].trim_end().to_string();
        let mut seam = joined.len();
        for line in lines.iter().take(end_line + 1).skip(start_line + 1) {
            let trimmed = line.trim_start();
            if !joined.is_empty() && !trimmed.is_empty() {
                joined.push(' ');
            }
            seam = joined.len();
            joined.push_str(trimmed);
        }
        let mut new_lines: Vec<String> = Vec::with_capacity(lines.len() - (end_line - start_line));
        new_lines.extend(lines.iter().take(start_line).cloned());
        new_lines.push(joined);
        new_lines.extend(lines.iter().skip(end_line + 1).cloned());
        self.push_history();
        self.content = new_lines.join("\n").into();
        self.invalidate_text_derived_caches();
        self.selection_anchor = None;
        self.cursor = Self::offset_at_line_col(&new_lines, start_line, seam);
        self.scroll_cursor_into_view();
        self.after_text_change(cx);
    }

    /// Jump the caret to the start of a 1-based line number, clamped to the
    /// document, clearing any selection.
    pub fn go_to_line(&mut self, line_number: usize, cx: &mut Context<Self>) {
        self.clear_extra_cursors();
        let lines: Vec<String> = self.content.split('\n').map(str::to_string).collect();
        let line = line_number
            .saturating_sub(1)
            .min(lines.len().saturating_sub(1));
        self.selection_anchor = None;
        self.cursor = Self::offset_at_line_col(&lines, line, 0);
        self.scroll_cursor_into_view();
        cx.notify();
    }

    pub fn go_to_offset(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.clear_extra_cursors();
        let clamped = offset.min(self.content.len());
        self.cursor = utf8_safe_edit_range(&self.content, clamped..clamped).start;
        self.selection_anchor = None;
        self.scroll_cursor_into_view();
        cx.notify();
    }

    /// Place the caret at `offset`, clearing any selection and extra cursors.
    pub fn place_cursor(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.go_to_offset(offset, cx);
    }

    fn go_to_next_error(&mut self, cx: &mut Context<Self>) {
        let errors: Vec<usize> = self
            .diagnostics
            .iter()
            .filter(|d| d.severity == DiagnosticSeverity::Error)
            .map(|d| d.start_byte)
            .collect();
        if errors.is_empty() {
            return;
        }
        let next = errors
            .iter()
            .copied()
            .find(|&start| start > self.cursor)
            .or_else(|| errors.first().copied());
        let Some(start) = next else {
            return;
        };
        self.clear_extra_cursors();
        self.cursor = start.min(self.content.len());
        self.cursor = utf8_safe_edit_range(&self.content, self.cursor..self.cursor).start;
        self.selection_anchor = None;
        self.scroll_cursor_into_view();
        cx.notify();
    }

    fn insert_newline(&mut self, cx: &mut Context<Self>) {
        self.clear_extra_cursors();
        let range = self.selection().unwrap_or(self.cursor..self.cursor);
        let insert = self.newline_insert_text(range.start);
        self.replace_selection(&insert, cx);
    }

    /// The text Enter inserts at `at`: a line break plus the indentation the
    /// new line should start with (the current line's, one unit deeper after
    /// a block-opening keyword, one unit shallower when a select-list item
    /// ended without a comma).
    fn newline_insert_text(&self, at: usize) -> String {
        let (line_idx, col) = self.line_col_for_offset(at);
        let lines: Vec<&str> = self.content.split('\n').collect();
        let current_line = lines.get(line_idx).copied().unwrap_or("");
        let before_cursor = &current_line[..col.min(current_line.len())];

        let indent: String = before_cursor
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();

        let after_indent = &before_cursor[indent.len()..];
        let extra = if ends_with_indent_keyword(after_indent) {
            self.editor_prefs.indent_unit()
        } else {
            String::new()
        };

        let final_indent = if extra.is_empty()
            && in_select_column_context(&self.content, at)
            && !after_indent.trim_end().ends_with(',')
        {
            remove_one_indent_unit(&indent, &self.editor_prefs.indent_unit())
        } else {
            indent
        };

        format!("\n{}{}", final_indent, extra)
    }

    fn insert_tab(&mut self, cx: &mut Context<Self>) {
        self.clear_extra_cursors();
        let unit = self.editor_prefs.indent_unit();
        let Some(_sel) = self.selection() else {
            self.replace_selection(&unit, cx);
            return;
        };
        let (start_line, end_line) = self.affected_line_range();
        let total_lines = self.line_count();
        if start_line == end_line {
            self.replace_selection(&unit, cx);
            return;
        }
        self.indent_lines(start_line, end_line.min(total_lines - 1), &unit, cx);
    }

    fn indent_lines(
        &mut self,
        start_line: usize,
        end_line: usize,
        unit: &str,
        cx: &mut Context<Self>,
    ) {
        let lines: Vec<&str> = self.content.split('\n').collect();
        let mut new_lines: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
        let count = end_line - start_line + 1;
        for i in start_line..=end_line.min(new_lines.len() - 1) {
            new_lines[i] = format!("{}{}", unit, lines[i]);
        }
        let new_content = new_lines.join("\n");
        let first_start = self.line_start_offset(start_line);
        let last_end = self.line_end_offset(end_line) + unit.len() * count;
        self.push_history();
        self.content = new_content.into();
        self.invalidate_text_derived_caches();
        self.selection_anchor = Some(first_start);
        self.cursor = last_end;
        self.after_text_change(cx);
        self.retire_automatic_completion(cx);
    }

    /// Whether the selection covers more than one line.
    fn selection_spans_lines(&self) -> bool {
        self.selection()
            .is_some_and(|range| self.content[range].contains('\n'))
    }

    /// Cancel the automatic completion pass an edit scheduled and close any
    /// popup: block indent / outdent is not typing, so no suggestions should
    /// appear at the end of the indented block.
    fn retire_automatic_completion(&mut self, cx: &mut Context<Self>) {
        self.completion_generation = self.completion_generation.wrapping_add(1);
        self.close_completions(cx);
    }

    fn unindent(&mut self, cx: &mut Context<Self>) {
        self.clear_extra_cursors();
        let unit = self.editor_prefs.indent_unit();
        let unit_len = unit.len();
        let (start_line, end_line) = self.affected_line_range();
        let lines: Vec<&str> = self.content.split('\n').collect();
        if lines.is_empty() {
            return;
        }
        let mut new_lines: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
        let mut total_removed = 0usize;
        for i in start_line..=end_line.min(lines.len() - 1) {
            let line = lines[i];
            if line.starts_with(unit.as_str()) {
                new_lines[i] = line[unit_len..].to_string();
                total_removed += unit_len;
            } else if line.starts_with('\t') && unit != "\t" {
                new_lines[i] = line[1..].to_string();
                total_removed += 1;
            } else {
                let ws_len = line
                    .chars()
                    .take_while(|c| *c == ' ' || *c == '\t')
                    .map(|c| c.len_utf8())
                    .sum::<usize>();
                if ws_len > 0 {
                    let remove = ws_len.min(unit_len);
                    new_lines[i] = line[remove..].to_string();
                    total_removed += remove;
                }
            }
        }
        if total_removed == 0 {
            return;
        }
        let new_content = new_lines.join("\n");
        let first_start = self.line_start_offset(start_line);
        let mut last_end = self.line_end_offset(end_line);
        let removed_before_last: usize = (start_line..=end_line.min(lines.len() - 1))
            .map(|i| lines[i].len() - new_lines[i].len())
            .sum();
        last_end -= removed_before_last;
        self.push_history();
        self.content = new_content.into();
        self.invalidate_text_derived_caches();
        self.selection_anchor = Some(first_start);
        self.cursor = last_end;
        self.after_text_change(cx);
        self.retire_automatic_completion(cx);
    }

    fn previous_grapheme_boundary(&self, offset: usize) -> usize {
        self.content[..offset]
            .grapheme_indices(true)
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0)
    }

    fn next_grapheme_boundary(&self, offset: usize) -> usize {
        self.content[offset..]
            .grapheme_indices(true)
            .nth(1)
            .map(|(i, _)| offset + i)
            .unwrap_or(self.content.len())
    }

    /// Styled runs for one line. `dialect` and `entry` carry the cross-line
    /// context the classifier needs: which comment spellings are live, and
    /// whether this line opens inside a block comment someone started above.
    /// `bold` picks which word kinds draw bold; `names` resolves plain words
    /// against the data model for the opt-in object kinds.
    fn syntax_runs(
        line: &str,
        base_font: &Font,
        dialect: sqlly_lang::sqlmodel::Dialect,
        entry: crate::sql_syntax::CommentState,
        bold: &crate::preferences::SyntaxBoldPreferences,
        names: &crate::sql_syntax::SemanticNames,
    ) -> Vec<TextRun> {
        use crate::sql_syntax::{SemanticKind, SyntaxTokenKind, classify_line, semantic_spans};

        let plain_run = |len: usize| TextRun {
            len,
            font: base_font.clone(),
            color: Theme::text(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        if line.is_empty() {
            return vec![plain_run(0)];
        }

        let (tokens, _exit) = classify_line(dialect, line, entry);
        // (start, end, color, bold) for every styled span, lexical and
        // data-model alike; the two never overlap.
        let mut spans: Vec<(usize, usize, Hsla, bool)> = tokens
            .iter()
            .map(|token| {
                let weight = match token.kind {
                    SyntaxTokenKind::Keyword => bold.keywords,
                    SyntaxTokenKind::Function => bold.functions,
                    SyntaxTokenKind::Type => bold.system_types,
                    SyntaxTokenKind::Number
                    | SyntaxTokenKind::String
                    | SyntaxTokenKind::Constant => bold.literals,
                    SyntaxTokenKind::Variable | SyntaxTokenKind::Comment => false,
                };
                (
                    token.start,
                    token.end,
                    Theme::syntax_color(token.kind),
                    weight,
                )
            })
            .collect();
        if bold.uses_data_model() {
            for (start, end, kind) in semantic_spans(line, &tokens, names) {
                let on = match kind {
                    SemanticKind::Schema => bold.schemas,
                    SemanticKind::Table => bold.tables,
                    SemanticKind::Procedure => bold.procedures,
                    SemanticKind::UserFunction => bold.user_functions,
                    SemanticKind::UserType => bold.user_types,
                };
                if on {
                    spans.push((start, end, Theme::text(), true));
                }
            }
            spans.sort_by_key(|span| (span.0, span.1));
        }
        if spans.is_empty() {
            return vec![plain_run(line.len())];
        }

        let mut runs = Vec::new();
        let mut prev_end = 0;

        for (start, end, color, weight) in spans {
            // Clamp to the line and to already-emitted runs: run lengths must
            // sum to exactly the line length or the shaper aborts the app, so
            // an out-of-range or overlapping token is clipped, never trusted.
            let start = start.max(prev_end);
            let end = end.min(line.len());
            if end <= start {
                continue;
            }
            if start > prev_end {
                runs.push(plain_run(start - prev_end));
            }

            let font = if weight {
                let mut f = base_font.clone();
                f.weight = FontWeight::BOLD;
                f
            } else {
                base_font.clone()
            };

            runs.push(TextRun {
                len: end - start,
                font,
                color,
                background_color: None,
                underline: None,
                strikethrough: None,
            });

            prev_end = end;
        }

        if prev_end < line.len() {
            runs.push(plain_run(line.len() - prev_end));
        }

        if runs.is_empty() {
            runs.push(plain_run(line.len()));
        }

        runs
    }

    /// The data-model names syntax highlighting resolves words against,
    /// rebuilt only when the IntelliSense catalog is replaced. Empty while
    /// no data-model bolding is switched on, so nothing is built for it.
    pub(super) fn semantic_names(&self) -> std::sync::Arc<crate::sql_syntax::SemanticNames> {
        if !self.editor_prefs.syntax_bold.uses_data_model() {
            // One shared empty set: its address is part of the shape-cache
            // probe, so a fresh one per frame would flush the cache.
            static EMPTY: std::sync::OnceLock<std::sync::Arc<crate::sql_syntax::SemanticNames>> =
                std::sync::OnceLock::new();
            return EMPTY.get_or_init(std::sync::Arc::default).clone();
        }
        let mut cache = self.semantic_names.borrow_mut();
        if let Some((catalog, names)) = cache.as_ref()
            && std::sync::Arc::ptr_eq(catalog, &self.completion_catalog)
        {
            return names.clone();
        }
        let names = std::sync::Arc::new(crate::sql_syntax::SemanticNames::from_catalog(
            &self.completion_catalog,
        ));
        *cache = Some((self.completion_catalog.clone(), names.clone()));
        names
    }

    // UTF-8 byte offset <-> UTF-16 code unit offset. Delegates to the cached
    // LineIndex so each conversion walks one line, not the whole document.
    fn offset_to_utf16(&self, offset: usize) -> usize {
        self.line_index().offset_to_utf16(&self.content, offset)
    }

    fn offset_from_utf16(&self, utf16_offset: usize) -> usize {
        self.line_index()
            .offset_from_utf16(&self.content, utf16_offset)
    }

    // Action handlers
    /// While a modal dialog is up, editing keys must not reach the buffer
    /// underneath it.
    fn modal_blocks_editing(&self) -> bool {
        self.format_dialog.is_some()
            || self.formatter_picker.is_some()
            || self.convert_dialog.is_some()
    }

    fn handle_backspace(&mut self, _: &Backspace, _: &mut Window, cx: &mut Context<Self>) {
        if self.modal_blocks_editing() {
            return;
        }
        if self.vim.as_ref().is_some_and(|v| v.intercepts_text()) {
            self.feed_vim(VimKey::Backspace, None, cx);
            return;
        }
        self.delete_backward(cx);
    }
    fn handle_delete(&mut self, _: &Delete, _: &mut Window, cx: &mut Context<Self>) {
        if self.modal_blocks_editing() {
            return;
        }
        if self.vim.as_ref().is_some_and(|v| v.intercepts_text()) {
            return;
        }
        self.delete_forward(cx);
    }
    fn handle_left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        self.hide_fim_suggestion(cx);
        self.move_left(false, cx);
    }
    fn handle_right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        self.hide_fim_suggestion(cx);
        // With the completion popup open, Right advances the dotted path by
        // one segment (`Acc|` → `Accounting.|`) instead of moving the caret.
        if self.try_advance_completion_to_dot(cx) {
            return;
        }
        self.move_right(false, cx);
    }
    fn handle_up(&mut self, _: &Up, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(dialog) = &mut self.format_dialog {
            dialog.selected = dialog.selected.saturating_sub(1);
            cx.notify();
            return;
        }
        self.hide_fim_suggestion(cx);
        if self.completion_visible {
            self.move_completion_up(cx);
        } else {
            self.move_up(false, cx);
        }
    }
    fn handle_down(&mut self, _: &Down, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(dialog) = &mut self.format_dialog {
            dialog.selected = (dialog.selected + 1).min(dialog.choices.len().saturating_sub(1));
            cx.notify();
            return;
        }
        self.hide_fim_suggestion(cx);
        if self.completion_visible {
            self.move_completion_down(cx);
        } else {
            self.move_down(false, cx);
        }
    }
    fn handle_home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.hide_fim_suggestion(cx);
        self.move_home(false, cx);
    }
    fn handle_end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        self.hide_fim_suggestion(cx);
        self.move_end(false, cx);
    }
    fn handle_select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.hide_fim_suggestion(cx);
        self.move_left(true, cx);
    }
    fn handle_select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.hide_fim_suggestion(cx);
        self.move_right(true, cx);
    }
    fn handle_select_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        self.hide_fim_suggestion(cx);
        self.move_up(true, cx);
    }
    fn handle_select_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        self.hide_fim_suggestion(cx);
        self.move_down(true, cx);
    }
    fn handle_select_home(&mut self, _: &SelectHome, _: &mut Window, cx: &mut Context<Self>) {
        self.hide_fim_suggestion(cx);
        self.move_home(true, cx);
    }
    fn handle_select_end(&mut self, _: &SelectEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.hide_fim_suggestion(cx);
        self.move_end(true, cx);
    }
    fn handle_select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.hide_fim_suggestion(cx);
        self.select_all(cx);
    }
    fn handle_copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = self.selected_text() {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        } else if let Some(line) = self.current_line_with_newline() {
            // No selection: copy the whole current line (VS Code / SSMS).
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(line));
        }
    }
    fn handle_cut(&mut self, _: &Cut, _: &mut Window, cx: &mut Context<Self>) {
        self.clear_extra_cursors();
        if let Some(text) = self.selected_text() {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
            self.replace_selection("", cx);
        } else if let Some(line) = self.current_line_with_newline() {
            // No selection: cut the whole current line (VS Code / SSMS).
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(line));
            let (line_idx, _) = self.line_col_for_offset(self.cursor);
            let start = self.line_start_offset(line_idx);
            let end = if line_idx + 1 < self.line_count() {
                self.line_start_offset(line_idx + 1)
            } else {
                self.content.len()
            };
            self.selection_anchor = Some(start);
            self.cursor = end;
            self.replace_selection("", cx);
        }
    }

    /// The current line's text including its trailing newline (when present) —
    /// the unit Copy/Cut act on when nothing is selected.
    fn current_line_with_newline(&self) -> Option<String> {
        if self.content.is_empty() {
            return None;
        }
        let (line_idx, _) = self.line_col_for_offset(self.cursor);
        let start = self.line_start_offset(line_idx);
        let end = if line_idx + 1 < self.line_count() {
            self.line_start_offset(line_idx + 1)
        } else {
            self.content.len()
        };
        (start < end).then(|| self.content[start..end].to_string())
    }
    fn handle_paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(item) = cx.read_from_clipboard()
            && let Some(text) = item.text()
        {
            // Windows clipboards carry CRLF; the editor is LF-only.
            let text = if text.contains('\r') {
                text.replace("\r\n", "\n").replace('\r', "\n")
            } else {
                text
            };
            if !self.extra_cursors.is_empty() {
                // Multi-cursor paste (format-on-paste is skipped — the text
                // is spliced into per-caret fragments): a clipboard with
                // exactly one line per caret distributes one line to each,
                // top to bottom; anything else pastes whole at every caret.
                let (all, primary_idx) = self.all_cursor_points();
                let mut lines: Vec<&str> = text.split('\n').collect();
                if lines.len() == all.len() + 1 && lines.last().is_some_and(|l| l.is_empty()) {
                    lines.pop(); // ignore a trailing newline for the count
                }
                let per_cursor_line = lines.len() == all.len();
                let edits = all
                    .iter()
                    .enumerate()
                    .map(|(i, c)| {
                        let insert = if per_cursor_line {
                            lines[i].to_string()
                        } else {
                            text.clone()
                        };
                        (
                            utf8_safe_edit_range(&self.content, c.edit_range()),
                            insert,
                            i == primary_idx,
                        )
                    })
                    .collect();
                self.apply_edits_at_all_cursors(edits, cx);
                return;
            }
            let text = self.maybe_format_pasted(text);
            self.replace_selection(&text, cx);
        }
    }

    /// Apply `format_on_paste`: only when the pasted text reads as one or
    /// more complete statements — formatting an expression fragment out of
    /// context would mangle the line it lands on.
    fn maybe_format_pasted(&self, text: String) -> String {
        if !looks_like_statement(&text) {
            return text;
        }
        let prefs = crate::preferences::load_sql_formatting_preferences();
        if !prefs.format_on_paste {
            return text;
        }
        let editor_indent = crate::sql_formatter::EditorIndent {
            style: self.editor_prefs.indent_style,
            width: self.editor_prefs.spaces_per_tab(),
        };
        crate::sql_formatter::format_sql(&text, &prefs, Some(editor_indent))
    }
    fn handle_enter(&mut self, _: &Enter, window: &mut Window, cx: &mut Context<Self>) {
        if self.convert_dialog.is_some() {
            self.apply_convert_list_replace(cx);
            return;
        }
        if let Some(dialog) = &self.format_dialog {
            let selected = dialog.selected;
            self.apply_format_choice(selected, window, cx);
            return;
        }
        if self.vim.as_ref().is_some_and(|v| v.intercepts_text()) {
            self.feed_vim(VimKey::Enter, None, cx);
            return;
        }
        if self.date_helper.is_some() && self.accept_date_helper(cx) {
            return;
        }
        self.accept_suggestion_or_insert_newline(cx);
    }
    fn handle_undo(&mut self, _: &Undo, _: &mut Window, cx: &mut Context<Self>) {
        self.undo(cx);
    }
    fn handle_redo(&mut self, _: &Redo, _: &mut Window, cx: &mut Context<Self>) {
        self.redo(cx);
    }
    fn accept_suggestion_or_insert_newline(&mut self, cx: &mut Context<Self>) {
        if self.accept_fim_suggestion(cx) {
            return;
        }
        // Enter only accepts a completion the user explicitly highlighted
        // with Up/Down. An un-navigated popup is a passive hint: Enter keeps
        // its default meaning (newline) so a line break is never hijacked
        // into accepting a suggestion — Tab is the "take the top item" key.
        if self.completion_visible && self.completion_user_navigated {
            self.accept_completion(cx);
            return;
        }
        if self.completion_visible {
            // Dismiss the now-stale popup instead of leaving it floating
            // over the fresh line; the next automatic pass re-evaluates.
            self.close_completions(cx);
        }
        if !self.format_as_you_go(cx) {
            self.insert_newline(cx);
        }
    }
    fn handle_shift_enter(&mut self, _: &ShiftEnter, _: &mut Window, cx: &mut Context<Self>) {
        if self.vim.as_ref().is_some_and(|v| v.intercepts_text()) {
            return;
        }
        self.shift_enter(cx);
    }

    fn shift_enter(&mut self, cx: &mut Context<Self>) {
        self.clear_extra_cursors();
        if self.completion_visible {
            self.close_completions(cx);
        }
        let range = self.selection().unwrap_or(self.cursor..self.cursor);
        let (line_idx, col) = self.line_col_for_offset(range.start);
        let lines: Vec<&str> = self.content.split('\n').collect();
        let current_line = lines.get(line_idx).copied().unwrap_or("");
        let before_cursor = &current_line[..col.min(current_line.len())];
        let indent: String = before_cursor
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        let after_indent = before_cursor[indent.len()..].trim_end();
        if in_select_column_context(&self.content, range.start) && !after_indent.ends_with(',') {
            // If cursor is at end of line (next char is \n or end of content),
            // just insert the comma — the existing newline serves as the break.
            let at_line_end = range.start >= self.content.len()
                || self.content.as_bytes().get(range.start) == Some(&b'\n');
            if at_line_end {
                self.replace_selection(",", cx);
            } else {
                let insert = format!(",\n{}", indent);
                self.replace_selection(&insert, cx);
            }
        } else {
            self.insert_newline(cx);
        }
    }
    fn handle_tab(&mut self, _: &Tab, _: &mut Window, cx: &mut Context<Self>) {
        if self.modal_blocks_editing() {
            return;
        }
        if self.vim.as_ref().is_some_and(|v| v.intercepts_text()) {
            return;
        }
        if self.date_helper.is_some() && self.accept_date_helper(cx) {
            return;
        }
        // FIM ghost text takes priority over the IntelliSense popup and a
        // plain tab indent — the Swift app accepts FIM on Tab/Return.
        if self.accept_fim_suggestion(cx) {
            return;
        }
        // Completion accept must win over snippet-stop walking and prefix
        // expand so Tab never steals IntelliSense. Plan 158 (vim) should route
        // insert-mode Tab through this same handler so snippet sessions keep
        // working; modal/normal-mode Tab may take over only when the popup is
        // closed and no snippet session is active.
        // Tab over a multi-line selection always indents the block: a popup
        // is never what it means, and accepting one would replace the
        // selection.
        if self.selection_spans_lines() {
            self.close_completions(cx);
            self.insert_tab(cx);
            return;
        }
        if self.completion_visible {
            self.accept_completion(cx);
            return;
        }
        if self.next_snippet_stop(cx) {
            return;
        }
        if !self.try_expand_snippet_prefix(cx) {
            self.insert_tab(cx);
        }
    }
    fn handle_shift_tab(&mut self, _: &ShiftTab, _: &mut Window, cx: &mut Context<Self>) {
        if self.vim.as_ref().is_some_and(|v| v.intercepts_text()) {
            return;
        }
        if !self.previous_snippet_stop(cx) {
            self.unindent(cx);
        }
    }
    fn handle_execute(&mut self, _: &ExecuteQuery, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(SqlEditorEvent::Execute);
    }
    fn handle_cancel(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        self.dismiss_transient_ui(cx);
    }

    fn handle_dismiss(&mut self, _: &Dismiss, _: &mut Window, cx: &mut Context<Self>) {
        self.dismiss_transient_ui(cx);
    }

    /// Esc ladder: one press dismisses the most transient thing on screen —
    /// hover card, signature help (with its completion popup), feedback
    /// dialog, format dialog, context menu, completion popup, ghost text,
    /// then the selection. Matches the convention of every mainstream
    /// editor.
    pub(crate) fn dismiss_transient_ui(&mut self, cx: &mut Context<Self>) {
        if self.close_date_helper(cx) {
            return;
        }
        if self.hover_info.is_some() {
            self.clear_hover_ui();
            cx.notify();
            return;
        }
        if let Some(help) = self.signature_help.take() {
            self.signature_dismissed_at_paren = Some(help.open_paren);
            // The argument-value popup stacked under the signature card is
            // part of the same call help, so one Esc closes both.
            if self.completion_visible {
                self.close_completions(cx);
            }
            cx.notify();
            return;
        }
        if let Some(v) = &self.vim
            && v.mode != VimMode::Normal
        {
            self.feed_vim(VimKey::Escape, None, cx);
            return;
        }
        if let Some(v) = &self.vim
            && v.has_pending()
        {
            self.feed_vim(VimKey::Escape, None, cx);
        }
        if self.snippet_session.is_some() {
            self.end_snippet_session();
            cx.notify();
            return;
        }
        if self.goto_line_bar.as_ref().is_some_and(|bar| bar.visible) {
            self.close_goto_line_bar(cx);
        } else if self.formatter_picker.is_some() {
            self.formatter_picker = None;
            self.formatter_picker_sub = None;
            cx.notify();
        } else if self.format_dialog.is_some() {
            self.format_dialog = None;
            cx.notify();
        } else if self.convert_dialog.is_some() {
            self.convert_dialog = None;
            cx.notify();
        } else if self.context_menu.is_some() {
            self.close_context_menu(cx);
            cx.notify();
        } else if self.completion_visible {
            self.close_completions(cx);
        } else if self.fim_visible {
            self.hide_fim_suggestion(cx);
            cx.notify();
        } else if self.clear_extra_cursors() {
            // Multi-cursor mode dismisses before the find bar and selection:
            // Esc collapses back to the single primary caret first.
            cx.notify();
        } else if self.find_bar.as_ref().is_some_and(|bar| bar.visible) {
            self.close_find_bar(cx);
        } else {
            self.selection_anchor = None;
            cx.notify();
        }
    }

    // ── Find / Replace bar ─────────────────────────────────────────────────

    /// Open (or refocus) the find bar, seeding the query from the current
    /// selection when it is a non-empty single-line one — the convention of
    /// every mainstream editor. `show_replace` also reveals the replace row.
    pub fn open_find_bar(
        &mut self,
        show_replace: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.clear_extra_cursors();
        if self.find_bar.is_none() {
            let find = cx.new(|cx| {
                InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                    window,
                    cx,
                    "editor.render.find_placeholder",
                ))
            });
            let replace = cx.new(|cx| {
                InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                    window,
                    cx,
                    "editor.render.replace_placeholder",
                ))
            });
            let on_find = cx.subscribe(&find, |this, state, event: &InputEvent, cx| match event {
                InputEvent::Change => {
                    let query = state.read(cx).value().to_string();
                    if let Some(bar) = &mut this.find_bar {
                        bar.query = query;
                    }
                    this.schedule_find_rescan(cx);
                }
                InputEvent::PressEnter { shift, .. } => {
                    if *shift {
                        this.find_prev(cx);
                    } else {
                        this.find_next(cx);
                    }
                }
                _ => {}
            });
            let on_replace =
                cx.subscribe(
                    &replace,
                    |this, state, event: &InputEvent, cx| match event {
                        InputEvent::Change => {
                            let replacement = state.read(cx).value().to_string();
                            if let Some(bar) = &mut this.find_bar {
                                bar.replacement = replacement;
                            }
                        }
                        InputEvent::PressEnter { .. } => this.replace_current(cx),
                        _ => {}
                    },
                );
            self.find_bar = Some(FindBarState {
                find,
                replace,
                visible: false,
                show_replace: false,
                case_sensitive: false,
                whole_word: false,
                regex: false,
                query: String::new(),
                replacement: String::new(),
                compiled_query: None,
                compile_error: None,
                matches: std::rc::Rc::new(Vec::new()),
                truncated: false,
                current: None,
                generation: 0,
                _subscriptions: [on_find, on_replace],
            });
        }
        let seed = self
            .selected_text()
            .filter(|text| !text.is_empty() && !text.contains('\n'));
        let bar = self.find_bar.as_mut().expect("created above");
        bar.visible = true;
        bar.show_replace = show_replace;
        let find = bar.find.clone();
        if let Some(seed) = seed {
            // `set_value` suppresses `InputEvent::Change` (gpui-component
            // 0.5), so the mirror field is written explicitly here.
            bar.query = seed.clone();
            find.update(cx, |state, cx| state.set_value(seed, window, cx));
        }
        find.focus_handle(cx).focus(window, cx);
        self.rescan_find_matches(cx);
    }

    /// Hide the bar and hand keyboard focus back to the editor. State is
    /// kept, so reopening restores the last query and options.
    pub fn close_find_bar(&mut self, cx: &mut Context<Self>) {
        if let Some(bar) = &mut self.find_bar {
            bar.visible = false;
            bar.current = None;
        }
        self.request_focus(cx);
        cx.notify();
    }

    /// Debounced rescan (120 ms, same cadence as automatic completion) so a
    /// keystroke burst in the find field or the buffer scans once, not per
    /// character.
    fn schedule_find_rescan(&mut self, cx: &mut Context<Self>) {
        let Some(bar) = &mut self.find_bar else {
            return;
        };
        if !bar.visible {
            return;
        }
        bar.generation = bar.generation.wrapping_add(1);
        let generation = bar.generation;
        let weak_self = cx.weak_entity();
        let async_cx = cx.to_async();
        cx.spawn(async move |_this, _cx| {
            async_cx
                .background_executor()
                .timer(std::time::Duration::from_millis(120))
                .await;
            let mut cx = async_cx.clone();
            weak_self
                .update(&mut cx, |editor, cx| {
                    if editor
                        .find_bar
                        .as_ref()
                        .is_none_or(|bar| bar.generation != generation)
                    {
                        return;
                    }
                    editor.rescan_find_matches(cx);
                })
                .ok();
        })
        .detach();
    }

    /// Recompute the match set for the current query against the current
    /// buffer. Never moves the caret — navigation does that, scans don't.
    fn rescan_find_matches(&mut self, cx: &mut Context<Self>) {
        let Some(bar) = &self.find_bar else {
            return;
        };
        let query = FindQuery::new(bar.query.clone())
            .case_sensitive(bar.case_sensitive)
            .whole_word(bar.whole_word)
            .regex(bar.regex);
        let compiled = if query.is_empty() {
            if let Some(bar) = &mut self.find_bar {
                bar.compiled_query = None;
                bar.compile_error = None;
            }
            None
        } else if bar
            .compiled_query
            .as_ref()
            .is_some_and(|compiled| compiled.source() == &query)
        {
            bar.compiled_query.clone()
        } else {
            match query.compile() {
                Ok(compiled) => {
                    if let Some(bar) = &mut self.find_bar {
                        bar.compiled_query = Some(compiled.clone());
                        bar.compile_error = None;
                    }
                    Some(compiled)
                }
                Err(error) => {
                    if let Some(bar) = &mut self.find_bar {
                        bar.compiled_query = None;
                        bar.compile_error = Some(error.to_string());
                    }
                    None
                }
            }
        };
        let (matches, truncated) = match compiled {
            Some(compiled) => compiled.find_matches(&self.content),
            None => (Vec::new(), false),
        };
        let current = find_replace::initial_index_at_or_after(&matches, self.cursor);
        if let Some(bar) = &mut self.find_bar {
            bar.matches = std::rc::Rc::new(matches);
            bar.truncated = truncated;
            bar.current = current;
        }
        cx.notify();
    }

    pub fn find_next(&mut self, cx: &mut Context<Self>) {
        self.step_find(true, cx);
    }

    pub fn find_prev(&mut self, cx: &mut Context<Self>) {
        self.step_find(false, cx);
    }

    /// Move `current` one match forward/backward (with wrap-around) and
    /// select it. The first forward step lands ON the match at/after the
    /// caret instead of skipping past it.
    fn step_find(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(bar) = &self.find_bar else {
            return;
        };
        let len = bar.matches.len();
        if len == 0 {
            cx.notify();
            return;
        }
        let at_current = bar
            .current
            .and_then(|i| bar.matches.get(i))
            .is_some_and(|m| self.selection() == Some(m.clone()));
        let next = if forward && !at_current {
            find_replace::initial_index_at_or_after(&bar.matches, self.cursor)
        } else if forward {
            find_replace::next_index(len, bar.current)
        } else {
            find_replace::prev_index(len, bar.current)
        };
        if let Some(bar) = &mut self.find_bar {
            bar.current = next;
        }
        self.select_current_match(cx);
    }

    /// Select the current match in the buffer and scroll it into view.
    fn select_current_match(&mut self, cx: &mut Context<Self>) {
        self.clear_extra_cursors();
        let Some(range) = self
            .find_bar
            .as_ref()
            .and_then(|bar| bar.current.and_then(|i| bar.matches.get(i).cloned()))
        else {
            cx.notify();
            return;
        };
        self.selection_anchor = Some(range.start);
        self.cursor = range.end;
        self.goal_column = None;
        self.scroll_cursor_into_view();
        cx.notify();
    }

    /// Replace the current match, then advance to the next one. The first
    /// invocation only selects the target (so nothing is replaced sight
    /// unseen); replacing routes through the normal edit path, so it
    /// snapshots undo like any other edit.
    pub fn replace_current(&mut self, cx: &mut Context<Self>) {
        let Some(bar) = &self.find_bar else {
            return;
        };
        let Some(range) = bar.current.and_then(|i| bar.matches.get(i).cloned()) else {
            cx.notify();
            return;
        };
        let replacement = bar
            .compiled_query
            .as_ref()
            .map(|compiled| compiled.replacement_for(&self.content, &range, &bar.replacement))
            .unwrap_or_else(|| bar.replacement.clone());
        if self.selection() != Some(range) {
            self.select_current_match(cx);
            return;
        }
        self.replace_selection(&replacement, cx);
        // The edit path scheduled an automatic completion pass; a find-bar
        // replace must never pop the suggestion list.
        self.completion_generation = self.completion_generation.wrapping_add(1);
        self.rescan_find_matches(cx);
        self.step_find(true, cx);
    }

    /// Replace every match in one undoable edit: exactly one snapshot is
    /// pushed, so a single undo restores the pre-replace-all buffer.
    pub fn replace_all_matches(&mut self, cx: &mut Context<Self>) {
        self.clear_extra_cursors();
        let Some(bar) = &self.find_bar else {
            return;
        };
        if bar.matches.is_empty() {
            cx.notify();
            return;
        }
        let matches = bar.matches.clone();
        let replacement = bar.replacement.clone();
        let Some(compiled) = bar.compiled_query.clone() else {
            cx.notify();
            return;
        };
        self.push_history();
        let new_content = compiled.replace_all(&self.content, &matches, &replacement);
        self.content = new_content.into();
        self.invalidate_text_derived_caches();
        // Clamp to length AND snap to a char boundary, exactly like
        // `set_text` — shrinking content can strand the caret mid-sequence.
        let clamped = self.content.len().min(self.cursor);
        self.cursor = utf8_safe_edit_range(&self.content, clamped..clamped).start;
        self.selection_anchor = None;
        self.after_text_change(cx);
        self.completion_generation = self.completion_generation.wrapping_add(1);
        self.rescan_find_matches(cx);
    }

    fn toggle_find_case_sensitive(&mut self, cx: &mut Context<Self>) {
        if let Some(bar) = &mut self.find_bar {
            bar.case_sensitive = !bar.case_sensitive;
        }
        self.rescan_find_matches(cx);
    }

    fn toggle_find_whole_word(&mut self, cx: &mut Context<Self>) {
        if let Some(bar) = &mut self.find_bar {
            bar.whole_word = !bar.whole_word;
        }
        self.rescan_find_matches(cx);
    }

    fn toggle_find_regex(&mut self, cx: &mut Context<Self>) {
        if let Some(bar) = &mut self.find_bar {
            bar.regex = !bar.regex;
        }
        self.rescan_find_matches(cx);
    }

    fn handle_find(&mut self, _: &Find, window: &mut Window, cx: &mut Context<Self>) {
        self.open_find_bar(false, window, cx);
    }

    fn handle_replace(&mut self, _: &Replace, window: &mut Window, cx: &mut Context<Self>) {
        self.open_find_bar(true, window, cx);
    }

    fn handle_find_next(&mut self, _: &FindNext, _: &mut Window, cx: &mut Context<Self>) {
        if self.find_bar.as_ref().is_some_and(|bar| bar.visible) {
            self.find_next(cx);
        }
    }

    fn handle_find_prev(&mut self, _: &FindPrev, _: &mut Window, cx: &mut Context<Self>) {
        if self.find_bar.as_ref().is_some_and(|bar| bar.visible) {
            self.find_prev(cx);
        }
    }

    // Multi-cursor action handlers.
    fn handle_add_cursor_for_next_occurrence(
        &mut self,
        _: &AddCursorForNextOccurrence,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.modal_blocks_editing() {
            return;
        }
        self.add_cursor_for_next_occurrence(cx);
    }

    fn handle_select_all_occurrences(
        &mut self,
        _: &SelectAllOccurrences,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.modal_blocks_editing() {
            return;
        }
        self.select_all_occurrences(cx);
    }

    fn handle_add_cursor_above(
        &mut self,
        _: &AddCursorAbove,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.modal_blocks_editing() {
            return;
        }
        self.add_cursor_vertical(true, cx);
    }

    fn handle_add_cursor_below(
        &mut self,
        _: &AddCursorBelow,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.modal_blocks_editing() {
            return;
        }
        self.add_cursor_vertical(false, cx);
    }

    fn handle_column_select_up(
        &mut self,
        _: &ColumnSelectUp,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.modal_blocks_editing() {
            return;
        }
        self.handle_column_select(false, cx);
    }

    fn handle_column_select_down(
        &mut self,
        _: &ColumnSelectDown,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.modal_blocks_editing() {
            return;
        }
        self.handle_column_select(true, cx);
    }

    /// Grow or shrink a rectangular column selection by one display line.
    fn handle_column_select(&mut self, down: bool, cx: &mut Context<Self>) {
        let (line, _) = self.line_col_for_offset(self.cursor);
        let display = self.display_line_of(line);
        let column = self.display_col_for_offset(self.cursor);
        if self.rectangular_anchor.is_none() {
            self.clear_extra_cursors();
            self.rectangular_anchor = Some((display, column));
            self.rectangular_target = Some((display, column));
        }
        let (target_display, target_column) = self.rectangular_target.unwrap_or((display, column));
        let last = self.display_line_count().saturating_sub(1);
        let next_display = if down {
            target_display.saturating_add(1).min(last)
        } else {
            target_display.saturating_sub(1)
        };
        self.rectangular_target = Some((next_display, target_column));
        let buffer_line = self.buffer_line_of_display(next_display);
        let offset = self.offset_for_display_col(buffer_line, target_column);
        self.update_rectangular_selection(offset, cx);
    }

    fn handle_pinch(
        &mut self,
        event: &gpui::PinchEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.delta != 0.0 {
            cx.emit(SqlEditorEvent::ZoomBy { delta: event.delta });
        }
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(v) = &self.vim
            && !self.modal_blocks_editing()
        {
            let ks = &event.keystroke;
            let unmodified = !ks.modifiers.platform && !ks.modifiers.control && !ks.modifiers.shift;
            // Insert/Visual Esc: consume here so tests (and hosts that
            // dispatch Escape as a raw key before the Dismiss action) leave
            // the mode. Normal-mode Esc falls through to the ladder.
            if unmodified && ks.key == "escape" && v.mode != VimMode::Normal {
                self.feed_vim(VimKey::Escape, Some(window), cx);
                cx.stop_propagation();
                return;
            }
            if v.intercepts_text() {
                if ks.modifiers.control && !ks.modifiers.platform && ks.key == "r" {
                    self.feed_vim(VimKey::CtrlR, Some(window), cx);
                    cx.stop_propagation();
                    return;
                }
                if unmodified && ks.key == "enter" {
                    self.feed_vim(VimKey::Enter, Some(window), cx);
                    cx.stop_propagation();
                    return;
                }
                if unmodified && ks.key == "backspace" {
                    self.feed_vim(VimKey::Backspace, Some(window), cx);
                    cx.stop_propagation();
                    return;
                }
                // Unmodified printable keys are consumed by the modal layer.
                // `dispatch_keystroke` (tests) skips `replace_text_in_range` after
                // stop_propagation, so feed here; the input-handler path is a
                // guard so a native insertText cannot insert or double-feed.
                if !ks.modifiers.platform && !ks.modifiers.control && ks.key_char.is_some() {
                    if self.marked_range.is_none()
                        && let Some(ch) = ks.key_char.as_deref().and_then(|s| {
                            let mut chars = s.chars();
                            match (chars.next(), chars.next()) {
                                (Some(c), None) => Some(c),
                                _ => None,
                            }
                        })
                    {
                        self.feed_vim(VimKey::Char(ch), Some(window), cx);
                    }
                    cx.stop_propagation();
                    return;
                }
            }
        }
        if self.formatter_picker.is_some() && event.keystroke.key == "escape" {
            self.formatter_picker = None;
            self.formatter_picker_sub = None;
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if self.format_dialog.is_some() && event.keystroke.key == "escape" {
            self.format_dialog = None;
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if self.convert_dialog.is_some() && event.keystroke.key == "escape" {
            self.convert_dialog = None;
            cx.stop_propagation();
            cx.notify();
        }
    }
    /// Open the editor context menu at the caret (Shift+F10 / the Menu key —
    /// the keyboard route to what right-click offers).
    fn handle_open_context_menu(
        &mut self,
        _: &OpenContextMenu,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(position) = self.cursor_menu_position() else {
            return;
        };
        let actions = self.context_actions_at(self.cursor);
        self.open_context_menu(position, actions, true, window, cx);
    }

    /// The context-menu actions for `offset`: the schema-driven ones, plus
    /// the IDENTITY_INSERT fix when the offset sits on a column this
    /// editor's lint has flagged. The fix leads — it is the answer to a
    /// warning already underlined there.
    fn context_actions_at(&self, offset: usize) -> Vec<EditorContextAction> {
        let mut actions = Vec::new();
        if let Some(fix) = identity_lint::identity_insert_fix_at(
            &self.content,
            self.database_type.family(),
            &self.schema_snapshot,
            offset,
        ) {
            actions.push(EditorContextAction::EnableIdentityInsert {
                sql: fix.sql,
                cursor: fix.cursor,
                table: fix.table,
            });
        }
        actions.extend(editor_context_actions(
            &self.content,
            offset,
            self.selection_range(),
            &self.schema_snapshot,
        ));
        actions
    }

    fn cursor_menu_position(&self) -> Option<Point<Pixels>> {
        let bounds = self.last_bounds?;
        let (line, col) = self.line_col_for_offset(self.cursor);
        let x = self
            .layout_for_line(line)
            .map(|(layout, tab_map)| layout.x_for_index(tab_map.to_display(col)))
            .unwrap_or(px(0.0));
        Some(Point::new(
            bounds.origin.x + self.line_number_width + x,
            bounds.origin.y + self.line_height * (self.display_line_of(line) + 1) as f32,
        ))
    }

    /// Build the context-menu popup for `actions` and store it as the open
    /// menu at `position` (window coordinates). An empty action set closes any
    /// open menu instead.
    fn open_context_menu(
        &mut self,
        position: Point<Pixels>,
        mut actions: Vec<EditorContextAction>,
        with_automagic: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The hover card is the editor's own tooltip. Opening a menu over it
        // left both on screen, one covering the other — drop it, and drop the
        // dwell timer that would otherwise pop a new one over the menu.
        self.clear_hover_ui();
        if with_automagic {
            actions.extend([
                EditorContextAction::ToggleAutomagicComma {
                    clause: ListClause::Select,
                    enabled: self.automagic_comma_enabled(ListClause::Select),
                },
                EditorContextAction::ToggleAutomagicComma {
                    clause: ListClause::GroupBy,
                    enabled: self.automagic_comma_enabled(ListClause::GroupBy),
                },
                EditorContextAction::ToggleAutomagicComma {
                    clause: ListClause::OrderBy,
                    enabled: self.automagic_comma_enabled(ListClause::OrderBy),
                },
                EditorContextAction::ToggleAutomagicConnectors {
                    enabled: self.automagic_connectors_enabled(),
                },
                EditorContextAction::ResetAutomagicCommas,
            ]);
        }
        let entity = cx.entity();
        // The right-click / keyboard context menu also carries the standard
        // editing commands every text editor has (the quick-fix menu that
        // shares this builder does not).
        let standard = if with_automagic {
            self.standard_context_items()
        } else {
            Vec::new()
        };
        // Key bindings on the standard items resolve in the editor's own key
        // context, so the labels match what the keyboard does here.
        let editor_focus = self.focus_handle.clone();
        let menu = PopupMenu::build(window, cx, |mut menu, window, cx| {
            menu = menu.action_context(editor_focus.clone());
            // The contextual fixes lead (they answer what is under the
            // caret); the per-query Automagic toggles go in their own
            // submenu at the end.
            let mut automagic = Vec::new();
            for (index, action) in actions.iter().enumerate() {
                if is_automagic_action(action) {
                    automagic.push(index);
                    continue;
                }
                let entity = entity.clone();
                menu = menu.item(
                    PopupMenuItem::new(editor_context_action_label(action)).on_click(
                        move |_event, _window, app| {
                            entity.update(app, |editor, cx| editor.apply_context_action(index, cx));
                        },
                    ),
                );
            }
            let leading = actions.len() > automagic.len();
            menu = add_standard_items(menu, &standard, leading, &entity, &editor_focus, window, cx);
            if !automagic.is_empty() {
                let labels: Vec<(usize, String)> = automagic
                    .iter()
                    .map(|&index| (index, editor_context_action_label(&actions[index])))
                    .collect();
                let entity = entity.clone();
                menu = menu.separator().submenu(
                    crate::localization::text("editor.context_menu.automagic_submenu"),
                    window,
                    cx,
                    move |mut sub, _window, _cx| {
                        for (index, label) in &labels {
                            let entity = entity.clone();
                            let index = *index;
                            sub = sub.item(PopupMenuItem::new(label.clone()).on_click(
                                move |_event, _window, app| {
                                    entity.update(app, |editor, cx| {
                                        editor.apply_context_action(index, cx)
                                    });
                                },
                            ));
                        }
                        sub
                    },
                );
            }
            menu
        });
        // The menu dismisses itself (Escape, click-out, item confirm); mirror
        // that by dropping the editor's menu state so the overlay unmounts.
        let dismiss = cx.subscribe(&menu, |editor, _menu, _: &DismissEvent, cx| {
            if editor.close_context_menu(cx) {
                cx.notify();
            }
        });
        // No tooltips while the menu is up — see `crate::tooltip`.
        if self.context_menu.is_none() {
            crate::tooltip::menu_opened(cx);
        }
        self.context_menu = Some(EditorContextMenu {
            position,
            actions,
            menu,
            _dismiss: dismiss,
        });
        cx.notify();
    }

    /// The standard commands of the editor's context menu, VS Code style:
    /// definition and run, clipboard, undo, find, SQL tools, and folding.
    /// Each runs the same handler as its menu-bar / keyboard command, at the
    /// caret the right-click placed.
    fn standard_context_items(&self) -> Vec<StandardContextItem> {
        use StandardContextItem::{Command, Separator, Submenu};
        let has_selection = self.has_selection();
        let has_text = !self.content.is_empty();
        let sql_engine = !self.database_type.is_command_engine();
        let cmd = |label: &'static str, enabled: bool, run: ContextCommand| Command {
            label,
            enabled,
            action: None,
            run,
        };
        // A command that mirrors an editor action (its shortcut is shown).
        let act = |label: &'static str,
                   enabled: bool,
                   action: Box<dyn gpui::Action>,
                   run: ContextCommand| Command {
            label,
            enabled,
            action: Some(action),
            run,
        };
        // Cut/Copy follow the keyboard shortcut: with nothing selected they
        // take the whole current line, so the menu says so instead of
        // greying out.
        let (cut_label, copy_label) = if has_selection {
            ("Cut", "Copy")
        } else {
            ("Cut Line", "Copy Line")
        };
        vec![
            cmd("Go to Definition", sql_engine, |e, _w, cx| {
                cx.emit(SqlEditorEvent::DdlNavigationRequested {
                    offset: e.cursor,
                    sql: e.content.clone(),
                });
            }),
            cmd("Run Statement", true, |e, _w, cx| {
                cx.emit(SqlEditorEvent::RunStatementAt { offset: e.cursor });
            }),
            act(
                "Change All Occurrences",
                true,
                Box::new(SelectAllOccurrences),
                |e, w, cx| e.handle_select_all_occurrences(&SelectAllOccurrences, w, cx),
            ),
            Separator,
            act(cut_label, has_text, Box::new(Cut), |e, w, cx| {
                e.handle_cut(&Cut, w, cx)
            }),
            act(copy_label, has_text, Box::new(Copy), |e, w, cx| {
                e.handle_copy(&Copy, w, cx)
            }),
            act("Paste", true, Box::new(Paste), |e, w, cx| {
                e.handle_paste(&Paste, w, cx)
            }),
            Separator,
            Submenu {
                label: "Edit",
                items: vec![
                    act("Undo", true, Box::new(Undo), |e, w, cx| {
                        e.handle_undo(&Undo, w, cx)
                    }),
                    act("Redo", true, Box::new(Redo), |e, w, cx| {
                        e.handle_redo(&Redo, w, cx)
                    }),
                    Separator,
                    act("Select All", true, Box::new(SelectAll), |e, w, cx| {
                        e.handle_select_all(&SelectAll, w, cx)
                    }),
                    act(
                        "Toggle Line Comment",
                        true,
                        Box::new(ToggleLineComment),
                        |e, w, cx| e.handle_toggle_line_comment(&ToggleLineComment, w, cx),
                    ),
                    act(
                        "Trigger Suggest",
                        true,
                        Box::new(TriggerCompletions),
                        |e, w, cx| e.handle_trigger_completions(&TriggerCompletions, w, cx),
                    ),
                ],
            },
            Submenu {
                label: "Find",
                items: vec![
                    act("Find", true, Box::new(Find), |e, w, cx| {
                        e.handle_find(&Find, w, cx)
                    }),
                    act("Replace", true, Box::new(Replace), |e, w, cx| {
                        e.handle_replace(&Replace, w, cx)
                    }),
                    act("Go to Line…", true, Box::new(GoToLine), |e, w, cx| {
                        e.handle_go_to_line(&GoToLine, w, cx)
                    }),
                ],
            },
            Submenu {
                label: "Format",
                items: vec![
                    act(
                        "Format Document",
                        sql_engine,
                        Box::new(FormatDocument),
                        |e, _w, cx| e.format_sql_document(cx),
                    ),
                    act(
                        "Format Selection",
                        sql_engine && has_selection,
                        Box::new(FormatSql),
                        |e, _w, cx| e.format_sql_buffer(cx),
                    ),
                ],
            },
            Submenu {
                label: "Folding",
                items: vec![
                    act("Fold All", true, Box::new(FoldAll), |e, w, cx| {
                        e.handle_fold_all(&FoldAll, w, cx)
                    }),
                    act("Unfold All", true, Box::new(UnfoldAll), |e, w, cx| {
                        e.handle_unfold_all(&UnfoldAll, w, cx)
                    }),
                ],
            },
        ]
    }

    fn handle_trigger_completions(
        &mut self,
        _: &TriggerCompletions,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.services.is_some() {
            self.trigger_completions_manual(cx);
        }
    }

    fn handle_trigger_ai_completion(
        &mut self,
        _: &TriggerAiCompletion,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.request_ai_completion(cx);
    }

    fn handle_fold(&mut self, _: &Fold, _: &mut Window, cx: &mut Context<Self>) {
        self.fold_at_cursor(cx);
    }

    fn handle_unfold(&mut self, _: &Unfold, _: &mut Window, cx: &mut Context<Self>) {
        self.unfold_at_cursor(cx);
    }

    fn handle_fold_all(&mut self, _: &FoldAll, _: &mut Window, cx: &mut Context<Self>) {
        self.fold_all_regions(cx);
    }

    fn handle_unfold_all(&mut self, _: &UnfoldAll, _: &mut Window, cx: &mut Context<Self>) {
        self.unfold_all_regions(cx);
    }

    /// Ask the app layer for a FIM ghost-text suggestion now (the manual
    /// trigger path — the shortcut works even in automatic mode).
    pub fn request_ai_completion(&mut self, cx: &mut Context<Self>) {
        cx.emit(SqlEditorEvent::FimTriggerRequested);
    }

    /// Whether the IntelliSense completion popup is currently showing. The FIM
    /// pipeline skips requests while it is up — ghost text under the popup
    /// would fight it for the same keystrokes.
    /// True while the completion popup's fade-in is still animating. The
    /// popup asks for animation frames from inside the element, which a
    /// frame-cached view would never run; the workbench renders the editor
    /// live for those few frames.
    pub fn needs_live_frames(&self) -> bool {
        self.completion_popup_visible()
            && self
                .completion_shown_at
                .is_some_and(|shown| shown.elapsed().as_secs_f32() < 0.15)
    }

    pub fn completion_popup_visible(&self) -> bool {
        self.completion_visible
    }

    /// Whether a non-empty selection exists. FIM requests are cursor-anchored,
    /// so they are skipped while text is selected.
    pub fn has_selection(&self) -> bool {
        self.selection_anchor.is_some_and(|a| a != self.cursor)
    }

    /// F2 over a live completion popup: freeze the full completion context
    /// (SQL around the caret, suggestions, rule config, catalog summary) and
    /// open the "improve IntelliSense" dialog so the user can tell us what
    /// should have been offered. Does nothing when no popup is up.
    fn handle_capture_intellisense_feedback(
        &mut self,
        _: &CaptureIntelliSenseFeedback,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_intellisense_feedback(window, cx);
    }

    /// Captures the "improve IntelliSense" context for the live completion
    /// popup — reached by F2 and by clicking the hint chip under the popup —
    /// and asks the app layer to host the dialog above the workbench (a
    /// modal rendered inside the editor would clip to the pane).
    ///
    /// Works even when there are zero suggestions: an empty popup is exactly
    /// when feedback matters most (IntelliSense offered nothing), and `capture`
    /// tolerates an empty suggestion list.
    fn open_intellisense_feedback(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if !self.completion_visible {
            return;
        }
        let (trigger, manual) = self.last_completion_trigger.unzip();
        let mut snapshot = crate::intellisense_feedback::capture(
            &self.content,
            self.cursor,
            &self.completions,
            self.completion_replace_start,
            self.completion_index,
            &self.completion_rules,
            &self.completion_catalog,
            &self.completion_procedures,
            trigger,
            manual.unwrap_or(false),
        );
        snapshot.settings = Some(crate::settings_snapshot::capture());
        cx.emit(SqlEditorEvent::FeedbackRequested {
            snapshot: Box::new(snapshot),
            include_all_default: self.editor_prefs.feedback_include_full_data_model,
            kind_defaults: self.editor_prefs.feedback_kind_visibility(),
            font: self.font.clone(),
        });
    }

    fn handle_validate(&mut self, _: &Validate, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(SqlEditorEvent::Validate);
    }

    fn handle_format_document(
        &mut self,
        _: &FormatDocument,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.format_sql_document(cx);
    }

    fn handle_format_sql(&mut self, _: &FormatSql, _: &mut Window, cx: &mut Context<Self>) {
        self.format_sql_buffer(cx);
    }

    fn handle_minify_sql(&mut self, _: &MinifySql, _: &mut Window, cx: &mut Context<Self>) {
        self.minify_sql_buffer(cx);
    }

    /// Minify the selection — the statement under the caret when nothing is
    /// selected, the whole buffer otherwise — as one undoable edit.
    pub fn minify_sql_buffer(&mut self, cx: &mut Context<Self>) {
        if self.database_type.is_command_engine() {
            return;
        }
        let range = self
            .selection()
            .or_else(|| {
                self.statement_span_at_cursor()
                    .map(|(start, end)| start..end)
            })
            .unwrap_or(0..self.content.len());
        self.clear_extra_cursors();
        let range = utf8_safe_edit_range(&self.content, range);
        let target = &self.content[range.clone()];
        let minified = crate::sql_formatter::minify(target, self.database_type.family());
        if minified == target {
            return;
        }
        let cursor_was_at_start = self.cursor <= range.start;
        self.selection_anchor = Some(range.start);
        self.cursor = range.end;
        self.replace_selection(&minified, cx);
        if cursor_was_at_start {
            self.cursor = range.start;
            cx.notify();
        }
    }

    /// "Format as you go": on Enter, reformat the statement under the caret,
    /// put the caret back after the same character it followed, and break
    /// the line there — all as one undo step, so Ctrl+Z restores the text
    /// and caret from before the keystroke (not a reformatted, selected
    /// statement). Returns whether it handled the Enter; `false` leaves the
    /// plain newline to the caller. Skipped for selections, multi-cursor
    /// edits, IME composition, command engines, and text that does not open
    /// with a statement keyword or that the formatter would not only
    /// re-flow.
    fn format_as_you_go(&mut self, cx: &mut Context<Self>) -> bool {
        if self.database_type.is_command_engine()
            || self.has_selection()
            || !self.extra_cursors.is_empty()
            || self.marked_range.is_some()
        {
            return false;
        }
        // Cached after the first read — this runs on every Enter.
        let prefs = crate::preferences::load_sql_formatting_preferences();
        #[cfg(test)]
        let enabled = self
            .format_as_you_go_override
            .unwrap_or(prefs.format_as_you_go);
        #[cfg(not(test))]
        let enabled = prefs.format_as_you_go;
        if !enabled {
            return false;
        }
        let Some(span) = self.statement_span_at_cursor() else {
            return false;
        };
        let editor_indent = crate::sql_formatter::EditorIndent {
            style: self.editor_prefs.indent_style,
            width: self.editor_prefs.spaces_per_tab(),
        };
        let Some((range, formatted, caret)) =
            format_as_you_go_edit(&self.content, span, self.cursor, &prefs, editor_indent)
        else {
            return false;
        };
        // One snapshot of the pre-Enter state (real caret, no selection),
        // then both splices without further history.
        self.push_history();
        self.splice_without_history(range, &formatted);
        self.cursor = caret.min(self.content.len());
        self.selection_anchor = None;
        let newline = self.newline_insert_text(self.cursor);
        let at = self.cursor;
        self.splice_without_history(at..at, &newline);
        self.cursor = at + newline.len();
        self.after_text_change(cx);
        true
    }

    /// Replace `range` with `text` without recording an undo entry — for a
    /// caller that has already pushed one snapshot covering several splices.
    /// Leaves the caret where the caller set it (clamped to the new length).
    fn splice_without_history(&mut self, range: Range<usize>, text: &str) {
        let range = utf8_safe_edit_range(&self.content, range);
        let mut new_content = String::with_capacity(self.content.len() + text.len());
        new_content.push_str(&self.content[..range.start]);
        new_content.push_str(text);
        new_content.push_str(&self.content[range.end..]);
        self.content = new_content.into();
        self.invalidate_text_derived_caches();
        self.snippet_session = None;
        self.cursor = self.cursor.min(self.content.len());
    }

    /// Format the selection (whole buffer when nothing is selected) with the
    /// current on-disk SQL formatting preferences, as one undoable edit.
    pub fn format_sql_buffer(&mut self, cx: &mut Context<Self>) {
        let prefs = crate::preferences::load_sql_formatting_preferences();
        let range = self.selection().unwrap_or(0..self.content.len());
        self.format_sql_range(range, &prefs, cx);
    }

    /// Format Document: the whole buffer with the current SQL formatting
    /// settings, whatever is selected, as one undoable edit.
    pub fn format_sql_document(&mut self, cx: &mut Context<Self>) {
        let prefs = crate::preferences::load_sql_formatting_preferences();
        self.format_sql_document_with(&prefs, cx);
    }

    /// Format the entire buffer regardless of selection (format-on-save).
    pub fn format_sql_document_with(
        &mut self,
        prefs: &crate::preferences::SqlFormattingPreferences,
        cx: &mut Context<Self>,
    ) {
        self.format_sql_range(0..self.content.len(), prefs, cx);
    }

    fn format_sql_range(
        &mut self,
        range: Range<usize>,
        prefs: &crate::preferences::SqlFormattingPreferences,
        cx: &mut Context<Self>,
    ) {
        // The SQL formatter would mangle Redis command lines.
        if self.database_type.is_command_engine() {
            return;
        }
        self.clear_extra_cursors();
        let editor_indent = crate::sql_formatter::EditorIndent {
            style: self.editor_prefs.indent_style,
            width: self.editor_prefs.spaces_per_tab(),
        };
        let range = utf8_safe_edit_range(&self.content, range);
        let target = &self.content[range.clone()];
        let formatted = crate::sql_formatter::format_sql(target, prefs, Some(editor_indent));
        if formatted == target {
            return;
        }
        let cursor_was_at_start = self.cursor <= range.start;
        self.selection_anchor = Some(range.start);
        self.cursor = range.end;
        self.replace_selection(&formatted, cx);
        if cursor_was_at_start {
            self.cursor = range.start;
            cx.notify();
        }
    }

    fn handle_trigger_value_lookup(
        &mut self,
        _: &TriggerValueLookup,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // In a date comparison the shortcut reads a natural-language date
        // instead of looking up column values.
        if self.open_date_helper(cx) {
            return;
        }
        let Some(services) = &self.services else {
            return;
        };

        let req = CompletionRequest {
            sql: &self.content,
            cursor: self.cursor,
            trigger: IntelliSenseTriggerKind::ManualValueLookup,
            manual: true,
            catalog: &self.completion_catalog,
            procedures: &self.completion_procedures,
            rules: &self.completion_rules,
            parameters: &[],
            model: Some(&self.schema_snapshot),
            database_type: Some(self.database_type),
        };
        let response = services.intellisense.complete_enriched_response(&req);

        let Some(context) = response.value_context else {
            self.close_completions(cx);
            return;
        };

        self.active_value_lookup = Some(context.clone());
        self.value_lookup_token = self.value_lookup_token.wrapping_add(1);
        self.value_lookup_base_items = response.items.clone();
        self.value_lookup_pending = true;
        self.value_lookup_fk = self.fk_pivot_for_value_context(&context);
        self.value_lookup_fetched_partial.clear();
        self.value_lookup_truncated = false;

        let token = self.value_lookup_token;
        cx.emit(SqlEditorEvent::ValueCompletionsRequested {
            context,
            request_token: token,
            partial: String::new(),
            fk: self.value_lookup_fk.clone().map(Box::new),
        });

        self.completions = std::rc::Rc::new(response.items);
        self.completion_index = 0;
        self.completion_scroll_offset = 0;
        self.completion_replace_start = response.replace_start;
        self.set_completion_visible(!self.completions.is_empty());
        self.completion_user_navigated = false;
        cx.notify();
    }

    fn handle_insert_distinct_values_as_in_list(
        &mut self,
        _: &InsertDistinctValuesAsInList,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let word = self.word_at_cursor();
        let sql = self.content.to_string();
        cx.emit(SqlEditorEvent::ValueLookup { word, sql });
    }

    fn handle_paste_as_in_list(
        &mut self,
        _: &PasteAsInList,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(item) = cx.read_from_clipboard() else {
            return;
        };
        let Some(text) = item.text() else {
            return;
        };
        let text = if text.contains('\r') {
            text.replace("\r\n", "\n").replace('\r', "\n")
        } else {
            text
        };
        match crate::list_tools::paste_as_in_list(self.database_type_for_literals(), &text) {
            Ok(list) => {
                if !self.extra_cursors.is_empty() {
                    let (all, primary_idx) = self.all_cursor_points();
                    let edits = all
                        .iter()
                        .enumerate()
                        .map(|(i, c)| {
                            (
                                utf8_safe_edit_range(&self.content, c.edit_range()),
                                list.clone(),
                                i == primary_idx,
                            )
                        })
                        .collect();
                    self.apply_edits_at_all_cursors(edits, cx);
                } else {
                    self.replace_selection(&list, cx);
                }
            }
            Err(message) => {
                cx.emit(SqlEditorEvent::Status {
                    level: crate::app::StatusLevel::Error,
                    message,
                });
            }
        }
    }

    fn handle_convert_delimited_list(
        &mut self,
        _: &ConvertDelimitedList,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(source) = self.selected_text().filter(|text| !text.trim().is_empty()) else {
            cx.emit(SqlEditorEvent::Status {
                level: crate::app::StatusLevel::Info,
                message: "Select the text to convert first.".to_string(),
            });
            return;
        };
        self.convert_dialog = Some(ConvertListDialog::new(source, window, cx));
        cx.notify();
    }

    fn apply_convert_list_replace(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.convert_dialog.as_ref() else {
            return;
        };
        match dialog.preview(cx) {
            Ok(out) => {
                self.convert_dialog = None;
                self.replace_selection(&out, cx);
            }
            Err(message) => {
                cx.emit(SqlEditorEvent::Status {
                    level: crate::app::StatusLevel::Error,
                    message,
                });
            }
        }
    }

    /// The foreign-key pivot for an `=` value slot on an FK id column (per
    /// the loaded data model), or `None` for plain columns, `LIKE` slots
    /// (a pattern over ids is not a name search), and unknown tables.
    fn fk_pivot_for_value_context(
        &self,
        context: &ValueContext,
    ) -> Option<sqlly_intel::modelengine::intellisense::FkValuePivot> {
        if context.operator != sqlly_services::ValueOperator::Equals {
            return None;
        }
        sqlly_intel::modelengine::intellisense::fk_value_pivot_for(
            &self.schema_snapshot,
            context.schema.as_deref(),
            context.table.as_deref()?,
            context.column.as_deref()?,
            self.database_type.family(),
        )
    }

    /// Apply async distinct-value lookup results (plain values, no labels).
    pub fn apply_value_lookup_results(
        &mut self,
        request_token: u64,
        values: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        let entries = values
            .into_iter()
            .map(sqlly_services::ValueLookupEntry::plain)
            .collect();
        self.apply_value_lookup_entries(request_token, entries, cx);
    }

    /// Apply async value-lookup results that are known to be the complete
    /// set for the column (no row cap hit, no server-side filter).
    pub fn apply_value_lookup_entries(
        &mut self,
        request_token: u64,
        values: Vec<sqlly_services::ValueLookupEntry>,
        cx: &mut Context<Self>,
    ) {
        self.apply_value_lookup_fetch(request_token, values, String::new(), false, cx);
    }

    /// Apply async value-lookup results. Stale tokens are ignored. Stores
    /// the raw entries and rebuilds the popup filtered against the text typed
    /// so far — so results arriving mid-typing are still narrowed. `partial`
    /// is the server-side filter the fetch ran with (empty for the trigger
    /// fetch) and `truncated` whether it hit the row cap; together they
    /// decide whether later keystrokes can filter locally or must re-query.
    pub fn apply_value_lookup_fetch(
        &mut self,
        request_token: u64,
        values: Vec<sqlly_services::ValueLookupEntry>,
        partial: String,
        truncated: bool,
        cx: &mut Context<Self>,
    ) {
        if request_token != self.value_lookup_token {
            return;
        }
        if self.active_value_lookup.is_none() {
            return;
        }

        self.value_lookup_values = values;
        self.value_lookup_fetched_partial = partial;
        self.value_lookup_truncated = truncated;
        self.value_lookup_pending = false;
        self.rebuild_value_lookup_completions(cx);
        cx.notify();
    }

    /// The text typed at the latched value position so far, without an
    /// opening quote.
    fn value_lookup_partial(&self, context: &ValueContext) -> (usize, usize, String) {
        let start = context.replace_start.min(self.content.len());
        let end = self.cursor.min(self.content.len());
        let raw = if start <= end {
            &self.content[start..end]
        } else {
            ""
        };
        (
            start,
            end,
            raw.strip_prefix('\'').unwrap_or(raw).to_string(),
        )
    }

    /// Whether the stored values can answer `partial` without another
    /// fetch. The stored set was fetched as "every value containing
    /// `fetched`" — complete when the row cap was not hit. Any text that
    /// still contains `fetched` is a narrower filter over that same set, so
    /// a complete set answers it locally; a capped sample only answers the
    /// exact text it was fetched for, and text the user backspaced away
    /// from `fetched` needs a fresh set either way.
    fn value_lookup_cache_covers(&self, partial: &str) -> bool {
        let fetched = self.value_lookup_fetched_partial.to_ascii_lowercase();
        let partial = partial.to_ascii_lowercase();
        if partial == fetched {
            return true;
        }
        !self.value_lookup_truncated && partial.contains(&fetched)
    }

    /// Ask the app layer for the column's values filtered by `partial`.
    /// One fetch at a time: a result that lands while the text has moved on
    /// rebuilds the popup, which re-checks coverage and requests again.
    fn request_value_lookup_refetch(
        &mut self,
        context: ValueContext,
        partial: String,
        cx: &mut Context<Self>,
    ) {
        if self.value_lookup_pending {
            return;
        }
        self.value_lookup_token = self.value_lookup_token.wrapping_add(1);
        self.value_lookup_pending = true;
        cx.emit(SqlEditorEvent::ValueCompletionsRequested {
            context,
            request_token: self.value_lookup_token,
            partial,
            fk: self.value_lookup_fk.clone().map(Box::new),
        });
    }

    /// Rebuild the value-lookup popup from the stored raw values, filtered by
    /// the live partial value (text between the latched replace start and the
    /// cursor, ignoring an opening quote). Value items carry an exact
    /// `replace_span` so accepting one replaces everything typed so far; base
    /// completion items are filtered by the same partial and merged below.
    fn rebuild_value_lookup_completions(&mut self, cx: &mut Context<Self>) {
        let Some(context) = self.active_value_lookup.clone() else {
            return;
        };

        let (start, end, partial) = self.value_lookup_partial(&context);
        if !self.value_lookup_cache_covers(&partial) {
            self.request_value_lookup_refetch(context.clone(), partial.clone(), cx);
        }
        let partial = partial.as_str();

        let replace_span = Some((start, end));
        let detail = format!(
            "distinct {}.{}",
            context.table.as_deref().unwrap_or("?"),
            context.column.as_deref().unwrap_or("?"),
        );

        // A foreign-key entry shows and matches its readable label (or its
        // key, so typing an id still works) and inserts the key with the
        // label as a trailing comment: `= 1 /* Beverages */`.
        let fk = self.value_lookup_fk.as_ref();
        let mut items: Vec<CompletionItem> = self
            .value_lookup_values
            .iter()
            .filter(|entry| {
                matches_partial(&entry.value, partial)
                    || entry
                        .label
                        .as_deref()
                        .is_some_and(|label| matches_partial(label, partial))
            })
            .take(100)
            .enumerate()
            .map(|(i, entry)| {
                let literal =
                    value_completion_literal(self.database_type, &entry.value, context.operator);
                let (label, insert_text, detail) = match &entry.label {
                    Some(label) => (
                        label.clone(),
                        sqlly_intel::modelengine::intellisense::fk_insert_text(&literal, label),
                        match fk {
                            Some(fk) => format!(
                                "{} = {} \u{2192} {}.{}",
                                fk.key_column,
                                entry.value,
                                fk.target.name.spelling(),
                                fk.display_column
                            ),
                            None => format!("key {}", entry.value),
                        },
                    ),
                    None => (entry.value.clone(), literal, detail.clone()),
                };
                CompletionItem {
                    label,
                    insert_text,
                    kind: CompletionKind::Value,
                    source: CompletionSource::Inferred,
                    score: 1000 - i as i32,
                    detail,
                    replace_span,
                }
            })
            .collect();

        items.extend(
            self.value_lookup_base_items
                .iter()
                .filter(|item| matches_partial(&item.label, partial))
                .cloned(),
        );

        self.completions = std::rc::Rc::new(items);
        self.completion_index = 0;
        self.completion_scroll_offset = 0;
        // The list just changed under any highlight the user had — drop back
        // to the unselected state so Enter can't accept a row they picked
        // from the previous filtering.
        self.completion_user_navigated = false;
        self.set_completion_visible(!self.completions.is_empty());
    }

    /// The whitespace-delimited token under the cursor — a best-effort column
    /// hint for value-lookup autocomplete.
    pub(crate) fn word_at_cursor(&self) -> String {
        let bytes = self.content.as_bytes();
        let mut start = self.cursor.min(bytes.len());
        while start > 0
            && !bytes[start - 1].is_ascii_whitespace()
            && bytes[start - 1] != b','
            && bytes[start - 1] != b'('
        {
            start -= 1;
        }
        let mut end = self.cursor.min(bytes.len());
        while end < bytes.len()
            && !bytes[end].is_ascii_whitespace()
            && bytes[end] != b','
            && bytes[end] != b')'
        {
            end += 1;
        }
        self.content[start..end].to_string()
    }

    fn handle_page_down(&mut self, _: &PageDown, _: &mut Window, cx: &mut Context<Self>) {
        self.move_page_down(false, cx);
    }
    fn handle_page_up(&mut self, _: &PageUp, _: &mut Window, cx: &mut Context<Self>) {
        self.move_page_up(false, cx);
    }
    fn handle_select_page_down(
        &mut self,
        _: &SelectPageDown,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_page_down(true, cx);
    }
    fn handle_select_page_up(&mut self, _: &SelectPageUp, _: &mut Window, cx: &mut Context<Self>) {
        self.move_page_up(true, cx);
    }
    fn handle_toggle_line_comment(
        &mut self,
        _: &ToggleLineComment,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.edit_line_comments(CommentMode::Toggle, cx);
    }
    fn handle_add_line_comment(
        &mut self,
        _: &AddLineComment,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.edit_line_comments(CommentMode::Add, cx);
    }
    fn handle_remove_line_comment(
        &mut self,
        _: &RemoveLineComment,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.edit_line_comments(CommentMode::Remove, cx);
    }
    fn handle_move_line_up(&mut self, _: &MoveLineUp, _: &mut Window, cx: &mut Context<Self>) {
        self.move_lines(true, cx);
    }
    fn handle_move_line_down(&mut self, _: &MoveLineDown, _: &mut Window, cx: &mut Context<Self>) {
        self.move_lines(false, cx);
    }
    fn handle_duplicate_line(&mut self, _: &DuplicateLine, _: &mut Window, cx: &mut Context<Self>) {
        self.duplicate_line_or_selection(cx);
    }
    fn handle_join_lines(&mut self, _: &JoinLines, _: &mut Window, cx: &mut Context<Self>) {
        self.join_lines(cx);
    }
    fn handle_go_to_line(&mut self, _: &GoToLine, window: &mut Window, cx: &mut Context<Self>) {
        self.open_goto_line_bar(window, cx);
    }

    fn handle_go_to_next_error(
        &mut self,
        _: &GoToNextError,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.go_to_next_error(cx);
    }

    fn handle_qualify_identifier(
        &mut self,
        _: &QualifyIdentifier,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let actions: Vec<EditorContextAction> = editor_context_actions(
            &self.content,
            self.cursor,
            self.selection_range(),
            &self.schema_snapshot,
        )
        .into_iter()
        .filter(|action| {
            matches!(
                action,
                EditorContextAction::QualifyIdentifier { .. }
                    | EditorContextAction::UnqualifyIdentifier { .. }
                    | EditorContextAction::QualifyAllInSelection { .. }
            )
        })
        .collect();
        match actions.len() {
            0 => {}
            1 => {
                let action = actions.into_iter().next().expect("len is 1");
                self.perform_context_action(action, cx);
            }
            _ => {
                let Some(position) = self.cursor_menu_position() else {
                    return;
                };
                self.open_context_menu(position, actions, false, window, cx);
            }
        }
    }

    /// Open (or refocus) the Go to Line bar.
    pub fn open_goto_line_bar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.goto_line_bar.is_none() {
            let input = cx.new(|cx| {
                InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                    window,
                    cx,
                    "editor.render.line_number_placeholder",
                ))
            });
            let subscription = cx.subscribe(&input, |this, state, event: &InputEvent, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    let raw = state.read(cx).value().to_string();
                    if let Ok(line) = raw.trim().parse::<usize>() {
                        this.go_to_line(line, cx);
                    }
                    this.close_goto_line_bar(cx);
                }
            });
            self.goto_line_bar = Some(GoToLineBarState {
                input,
                visible: false,
                _subscription: subscription,
            });
        }
        let bar = self.goto_line_bar.as_mut().expect("created above");
        bar.visible = true;
        let input = bar.input.clone();
        input.update(cx, |state, cx| state.set_value("", window, cx));
        input.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    pub fn close_goto_line_bar(&mut self, cx: &mut Context<Self>) {
        if let Some(bar) = &mut self.goto_line_bar {
            bar.visible = false;
        }
        self.request_focus(cx);
        cx.notify();
    }

    // Word / hump / document navigation handlers.
    fn handle_move_word_left(&mut self, _: &MoveWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        if self.editor_prefs.camel_hump_word_movement {
            self.move_hump_left(false, cx);
        } else {
            self.move_word_left(false, cx);
        }
    }
    fn handle_select_word_left(
        &mut self,
        _: &SelectWordLeft,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.editor_prefs.camel_hump_word_movement {
            self.move_hump_left(true, cx);
        } else {
            self.move_word_left(true, cx);
        }
    }
    fn handle_move_word_right(
        &mut self,
        _: &MoveWordRight,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.editor_prefs.camel_hump_word_movement {
            self.move_hump_right(false, cx);
        } else {
            self.move_word_right(false, cx);
        }
    }
    fn handle_select_word_right(
        &mut self,
        _: &SelectWordRight,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.editor_prefs.camel_hump_word_movement {
            self.move_hump_right(true, cx);
        } else {
            self.move_word_right(true, cx);
        }
    }
    fn handle_delete_word_backward(
        &mut self,
        _: &DeleteWordBackward,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.delete_word_backward(cx);
    }
    fn handle_delete_word_forward(
        &mut self,
        _: &DeleteWordForward,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.delete_word_forward(cx);
    }
    fn handle_move_hump_left(&mut self, _: &MoveHumpLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.move_hump_left(false, cx);
    }
    fn handle_select_hump_left(
        &mut self,
        _: &SelectHumpLeft,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_hump_left(true, cx);
    }
    fn handle_move_hump_right(
        &mut self,
        _: &MoveHumpRight,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_hump_right(false, cx);
    }
    fn handle_select_hump_right(
        &mut self,
        _: &SelectHumpRight,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_hump_right(true, cx);
    }
    fn handle_move_to_beginning_of_document(
        &mut self,
        _: &MoveToBeginningOfDocument,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_doc_start(false, cx);
    }
    fn handle_select_to_beginning_of_document(
        &mut self,
        _: &SelectToBeginningOfDocument,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_doc_start(true, cx);
    }
    fn handle_move_to_end_of_document(
        &mut self,
        _: &MoveToEndOfDocument,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_doc_end(false, cx);
    }
    fn handle_select_to_end_of_document(
        &mut self,
        _: &SelectToEndOfDocument,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_doc_end(true, cx);
    }
    fn handle_delete_to_beginning_of_line(
        &mut self,
        _: &DeleteToBeginningOfLine,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.delete_to_line_start(cx);
    }
    fn handle_delete_to_end_of_line(
        &mut self,
        _: &DeleteToEndOfLine,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.delete_to_line_end(cx);
    }

    /// Translate an absolute (buffer) line index to a relative index into
    /// `last_layout` / `last_tab_maps` and return both. `None` when the
    /// line is outside the visible range cached at the last prepaint.
    /// `last_layout_first_line` is a *display* index, so the buffer line is
    /// mapped through the fold display map first.
    fn layout_for_line(
        &self,
        absolute_line: usize,
    ) -> Option<(&ShapedLine, &crate::tab_map::TabMap)> {
        let display = self.display_line_of(absolute_line);
        let rel = display.checked_sub(self.last_layout_first_line)?;
        Some((self.last_layout.get(rel)?, self.last_tab_maps.get(rel)?))
    }

    /// Convert a window-space mouse position to a byte offset in `content`
    /// using the last paint's line layouts and tab maps. `None` before the
    /// first paint. Shared by caret placement, drag-selection, and the DDL
    /// navigation gesture so the hit-test math exists exactly once.
    ///
    /// `last_bounds` is the editor *content* element's bounds, whose origin
    /// gpui already shifts by the scroll offset (children of an
    /// `overflow_scroll` container are laid out at `origin + offset`, offset
    /// ≤ 0). Subtracting it therefore yields content-space coordinates
    /// directly — adding the scroll offset again would double-count it and
    /// land clicks a scroll-height too high.
    fn byte_offset_for_position(&self, position: Point<Pixels>) -> Option<usize> {
        let bounds = self.last_bounds?;
        let x = position.x - bounds.origin.x - self.line_number_width;
        let y = position.y - bounds.origin.y;

        // The y axis is display space (folds collapse lines out of layout);
        // map back to the buffer line that is actually painted there.
        let display =
            ((y / self.line_height) as usize).min(self.display_line_count().saturating_sub(1));
        let line = self.buffer_line_of_display(display);

        let line_start = self.line_start_offset(line);
        let line_end = self.line_end_offset(line);
        let line_text = &self.content[line_start..line_end];

        if let Some((layout, tab_map)) = self.layout_for_line(line) {
            let col = layout.closest_index_for_x(x);
            let col = tab_map.to_content(col);
            Some(line_start + col.min(line_text.len()))
        } else {
            Some(line_end)
        }
    }

    /// `true` when a mouse-down should trigger DDL navigation instead of
    /// caret placement: the feature is enabled, it's a plain single click,
    /// and exactly the configured modifier is held.
    fn is_ddl_navigation_click(&self, click_count: usize, modifiers: &gpui::Modifiers) -> bool {
        self.ddl_nav_prefs.enabled
            && click_count == 1
            && self.ddl_nav_prefs.activation_modifier.matches_flags(
                modifiers.platform,
                modifiers.control,
                modifiers.alt,
                modifiers.shift,
            )
    }

    /// Re-evaluate the modifier-hover link affordance: while the DDL
    /// navigation modifier is held and the symbol under the mouse resolves
    /// to a database object, its span is styled like a hyperlink. Kept cheap
    /// on the mouse-move hot path — resolution runs only while the modifier
    /// is held, and is skipped entirely while the pointer stays inside the
    /// currently highlighted span.
    fn update_link_hover(&mut self, modifiers: &gpui::Modifiers, cx: &mut Context<Self>) {
        let modifier_held = self.ddl_nav_prefs.enabled
            && self.ddl_nav_prefs.activation_modifier.matches_flags(
                modifiers.platform,
                modifiers.control,
                modifiers.alt,
                modifiers.shift,
            );
        let new_range = if modifier_held {
            self.last_mouse_position
                .and_then(|position| self.byte_offset_for_position(position))
                .and_then(|offset| {
                    if let Some((start, end)) = self.link_hover_range
                        && offset >= start
                        && offset < end
                    {
                        return Some((start, end));
                    }
                    // The active database is the one the app pushed into the
                    // catalog when the completion state was built.
                    let database = self
                        .completion_catalog
                        .databases
                        .first()
                        .map(|s| s.as_str());
                    sqlly_intel::intellisense::navigable_span_at(
                        &self.content,
                        offset,
                        &self.completion_catalog,
                        "dbo",
                        database,
                    )
                })
        } else {
            None
        };
        if new_range != self.link_hover_range {
            self.link_hover_range = new_range;
            cx.notify();
        }
    }

    fn handle_modifiers_changed(
        &mut self,
        event: &ModifiersChangedEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.update_link_hover(&event.modifiers, cx);
    }

    fn replace_text_with_history(&mut self, text: String, cx: &mut Context<Self>) {
        if text == self.content.as_ref() {
            return;
        }
        self.clear_extra_cursors();
        self.push_history();
        self.content = text.into();
        self.invalidate_text_derived_caches();
        let clamped = self.cursor.min(self.content.len());
        self.cursor = utf8_safe_edit_range(&self.content, clamped..clamped).start;
        self.selection_anchor = None;
        self.after_text_change(cx);
    }

    fn handle_context_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.focus_self(window);
        self.close_completions(cx);
        let Some(offset) = self.byte_offset_for_position(event.position) else {
            return;
        };
        // Right-clicking inside the selection keeps it (so Cut / Copy /
        // Format Selection act on it), as in every other editor; anywhere
        // else moves the caret there.
        let inside_selection = self
            .selection()
            .is_some_and(|range| range.start <= offset && offset <= range.end);
        if !inside_selection {
            self.cursor = offset;
            self.selection_anchor = None;
        }
        let actions = self.context_actions_at(offset);
        self.open_context_menu(event.position, actions, true, window, cx);
    }

    fn apply_context_action(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(action) = self
            .context_menu
            .as_ref()
            .and_then(|menu| menu.actions.get(index))
            .cloned()
        else {
            return;
        };
        self.close_context_menu(cx);
        self.perform_context_action(action, cx);
    }

    /// Drop the editor's context menu, releasing the app-wide tooltip
    /// suppression it took. Returns whether a menu was actually open.
    fn close_context_menu(&mut self, cx: &mut Context<Self>) -> bool {
        if self.context_menu.take().is_some() {
            crate::tooltip::menu_closed(cx);
            true
        } else {
            false
        }
    }

    fn perform_context_action(&mut self, action: EditorContextAction, cx: &mut Context<Self>) {
        match action {
            EditorContextAction::ExpandWildcard { sql, cursor } => {
                self.cursor = cursor;
                self.replace_text_with_history(sql, cx);
            }
            EditorContextAction::QualifyIdentifier { sql, cursor, .. }
            | EditorContextAction::UnqualifyIdentifier { sql, cursor, .. }
            | EditorContextAction::EnableIdentityInsert { sql, cursor, .. } => {
                self.cursor = cursor;
                self.replace_text_with_history(sql, cx);
            }
            EditorContextAction::QualifyAllInSelection { sql, .. } => {
                self.replace_text_with_history(sql, cx);
            }
            EditorContextAction::AddJoin { sql } | EditorContextAction::ShowDisplayName { sql } => {
                self.replace_text_with_history(sql, cx);
            }
            EditorContextAction::FormatColumn { column, kind } => {
                self.pending_format_sample = None;
                self.format_dialog = Some(ColumnFormatDialog {
                    choices: column_format_choices(&column, kind),
                    column,
                    selected: 0,
                });
                cx.notify();
            }
            EditorContextAction::ToggleAutomagicComma { clause, enabled: _ } => {
                let index = clause.index();
                let next = !self.automagic_comma_enabled(clause);
                self.automagic_comma_overrides[index] = Some(next);
                cx.notify();
            }
            EditorContextAction::ToggleAutomagicConnectors { enabled: _ } => {
                self.automagic_connector_override = Some(!self.automagic_connectors_enabled());
                cx.notify();
            }
            EditorContextAction::ResetAutomagicCommas => {
                self.automagic_comma_overrides = [None, None, None];
                self.automagic_connector_override = None;
                cx.notify();
            }
        }
    }

    fn apply_format_choice(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = self.format_dialog.take() else {
            return;
        };
        let Some(choice) = dialog.choices.get(index) else {
            cx.notify();
            return;
        };
        if choice.directive == "__custom__" {
            let sample = self.pending_format_sample.take();
            self.open_formatter_picker(dialog.column, sample, window, cx);
            return;
        }
        let updated =
            update_or_insert_format_comment(&self.content, &dialog.column, &choice.directive);
        self.replace_text_with_history(updated, cx);
    }

    /// Mouse-wheel over the completion popup scrolls its list instead of the SQL
    /// text underneath. Elsewhere it's ignored so normal text scrolling (handled
    /// by the tracked scroll handle) is unaffected. Cmd/Ctrl-wheel zooms.
    fn handle_scroll_wheel(
        &mut self,
        event: &gpui::ScrollEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.hover_info.is_some() {
            self.clear_hover_ui();
            cx.notify();
        }
        let zoom_mod = if cfg!(target_os = "macos") {
            event.modifiers.platform
        } else {
            event.modifiers.control || event.modifiers.platform
        };
        if zoom_mod {
            let delta_lines = match event.delta {
                gpui::ScrollDelta::Lines(p) => p.y,
                gpui::ScrollDelta::Pixels(p) => f32::from(p.y) / 40.0,
            };
            let steps =
                crate::editor_zoom::wheel_zoom_steps(&mut self.zoom_wheel_accumulator, delta_lines);
            if steps != 0 {
                cx.emit(SqlEditorEvent::ZoomBy {
                    delta: steps as f32 * crate::editor_zoom::ZOOM_STEP,
                });
            }
            cx.stop_propagation();
            return;
        }
        if !self.completion_visible {
            return;
        }
        let Some(rect) = self.completion_popup_bounds.get() else {
            return;
        };
        if !rect.contains(&event.position) {
            return;
        }
        // Consume the event so the text buffer doesn't also scroll.
        cx.stop_propagation();

        let row_height = self.completion_row_height.get();
        let max_offset = self
            .completions
            .len()
            .saturating_sub(COMPLETION_MAX_VISIBLE);
        if row_height <= 0.0 || max_offset == 0 {
            return;
        }
        // Positive `y` = wheel up = toward the top of the list.
        let delta_lines = match event.delta {
            gpui::ScrollDelta::Lines(p) => p.y,
            gpui::ScrollDelta::Pixels(p) => f32::from(p.y) / row_height,
        };
        let steps = delta_lines.abs().ceil() as usize;
        let new_offset = if delta_lines > 0.0 {
            self.completion_scroll_offset.saturating_sub(steps)
        } else {
            (self.completion_scroll_offset + steps).min(max_offset)
        };
        if new_offset != self.completion_scroll_offset {
            self.completion_scroll_offset = new_offset;
            cx.notify();
        }
    }

    // Mouse handlers
    fn handle_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Click on a ↻ date chip: re-evaluate that literal (or every one in
        // the statement) without moving the caret.
        if let Some(chip) = self.date_chip_at(event.position) {
            match chip {
                date_helper::DateChip::One(marker) => self.refresh_date_marker_at(marker, cx),
                date_helper::DateChip::Statement(start) => self.refresh_statement_dates(start, cx),
            }
            return;
        }
        if self.date_helper.take().is_some() {
            cx.notify();
        }

        // Click on the "to improve this, press F2" chip: open the feedback
        // dialog for the live popup instead of dismissing it.
        if self.completion_visible
            && let Some(hint) = self.feedback_hint_bounds.get()
            && hint.contains(&event.position)
        {
            self.open_intellisense_feedback(window, cx);
            return;
        }

        // Click on a diagnostic popup's "Fix this" link: apply the fix. A click
        // elsewhere on the popup is swallowed so it doesn't move the caret.
        if let Some(diag_idx) = self.hovered_diagnostic
            && let Some(popup) = self.diagnostic_popup_bounds.get()
            && popup.contains(&event.position)
        {
            if let Some(link) = self.diagnostic_fix_bounds.get()
                && link.contains(&event.position)
                && let Some(diag) = self.diagnostics.get(diag_idx).cloned()
            {
                self.apply_diagnostic_fix(&diag, cx);
            } else if self
                .diagnostic_directive_link_bounds
                .get()
                .is_some_and(|link| link.contains(&event.position))
            {
                crate::format_directives::open_format_directives_window(cx);
            }
            return;
        }

        // Click on the `-- sqlly …` directive info popup: open the formatting
        // directives reference instead of moving the caret.
        if self.hovered_sqlly_directive.is_some()
            && let Some(bounds) = self.directive_hint_bounds.get()
            && bounds.contains(&event.position)
        {
            crate::format_directives::open_format_directives_window(cx);
            return;
        }

        // Click inside the completion popup: pick the row under the pointer and
        // accept it. A click on an empty ("No suggestions") popup is consumed so
        // it doesn't dismiss; a click OUTSIDE falls through and dismisses below.
        if self.completion_visible
            && let Some(rect) = self.completion_popup_bounds.get()
            && rect.contains(&event.position)
        {
            let row_height = self.completion_row_height.get();
            if row_height > 0.0 {
                let offset_y = f32::from(event.position.y - rect.origin.y);
                let row = (offset_y / row_height) as usize + self.completion_scroll_offset;
                if row < self.completions.len() {
                    cx.focus_self(window);
                    self.completion_index = row;
                    self.accept_completion(cx);
                    return;
                }
            }
            return;
        }

        cx.focus_self(window);
        self.close_completions(cx);

        if let Some(offset) = self.gutter_play_target(event.position) {
            cx.emit(SqlEditorEvent::RunStatementAt { offset });
            return;
        }

        // Click on a fold chevron in the gutter: toggle the fold instead of
        // moving the caret. Only the gutter strip left of the text area
        // counts, and only on lines that actually head a fold region.
        if self.editor_prefs.show_line_numbers
            && let Some(bounds) = self.last_bounds
        {
            let x = event.position.x - bounds.origin.x;
            let y = event.position.y - bounds.origin.y;
            if x >= px(0.0) && x < self.line_number_width && y >= px(0.0) {
                let display = ((y / self.line_height) as usize)
                    .min(self.display_line_count().saturating_sub(1));
                let line = self.buffer_line_of_display(display);
                if self.with_folds(|f| f.is_fold_header(line)) {
                    self.toggle_fold_at_line(line, cx);
                    return;
                }
            }
        }

        // DDL navigation gesture: resolve the symbol under the pointer
        // instead of moving the caret or starting a selection. Emitting the
        // event is the entire hot path — resolution + fetch happen app-side.
        if self.is_ddl_navigation_click(event.click_count, &event.modifiers)
            && let Some(offset) = self.byte_offset_for_position(event.position)
        {
            self.is_selecting = false;
            cx.emit(SqlEditorEvent::DdlNavigationRequested {
                offset,
                sql: self.content.clone(),
            });
            return;
        }

        if let Some(offset) = self.byte_offset_for_position(event.position) {
            self.goal_column = None;
            if event.click_count == 1 && event.modifiers.alt {
                self.clear_extra_cursors();
                let (line, _) = self.line_col_for_offset(offset);
                let display = self.display_line_of(line);
                let column = self.display_col_for_offset(offset);
                self.rectangular_anchor = Some((display, column));
                self.rectangular_target = Some((display, column));
                self.cursor = offset;
                self.selection_anchor = Some(offset);
                self.is_selecting = true;
                self.update_rectangular_selection(offset, cx);
                return;
            }
            self.rectangular_anchor = None;
            self.rectangular_target = None;
            // A plain click collapses multi-cursor mode to the click point.
            self.clear_extra_cursors();

            // Shift+click extends the selection from the existing caret (or
            // selection anchor) to the click point — the universal text-field
            // convention on every platform.
            if event.modifiers.shift && event.click_count == 1 {
                if self.selection_anchor.is_none() {
                    self.selection_anchor = Some(self.cursor);
                }
                self.cursor = offset;
                self.is_selecting = true;
                cx.notify();
                return;
            }

            self.cursor = offset;

            // Click-count selection: single = place caret + start drag-select,
            // double = select the token (word/punct/whitespace run) under the
            // click, triple = select the whole line including its newline.
            let click = event.click_count;
            if click >= 3 {
                let (clicked_line, _) = self.line_col_for_offset(self.cursor);
                let start = self.line_start_offset(clicked_line);
                let end = if clicked_line + 1 < self.line_count() {
                    self.line_start_offset(clicked_line + 1)
                } else {
                    self.content.len()
                };
                self.selection_anchor = Some(start);
                self.cursor = end;
                self.is_selecting = false;
            } else if click == 2 {
                let (s, e) = token_range_at(&self.content, self.cursor);
                self.selection_anchor = Some(s);
                self.cursor = e;
                self.is_selecting = false;
            } else {
                self.selection_anchor = None;
                self.is_selecting = true;
            }
            self.refresh_signature_help(cx);
            cx.notify();
        }
    }

    fn handle_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.last_mouse_position = Some(event.position);
        let over_chip = self
            .fold_chip_bounds
            .borrow()
            .iter()
            .find(|(_, bounds)| bounds.contains(&event.position))
            .map(|(line, _)| *line);
        if over_chip != self.fold_preview {
            self.fold_preview = over_chip;
            cx.notify();
        }
        if self.is_selecting && self.hovered_play_line.is_some() {
            self.hovered_play_line = None;
        }
        if self.is_selecting
            && self.rectangular_anchor.is_some()
            && let Some(offset) = self.byte_offset_for_position(event.position)
        {
            self.update_rectangular_selection(offset, cx);
        } else if self.is_selecting
            && let Some(offset) = self.byte_offset_for_position(event.position)
        {
            self.cursor = offset;
            if self.selection_anchor.is_none() {
                self.selection_anchor = Some(self.cursor);
            }
            cx.notify();
        } else if let Some(bounds) = self.last_bounds {
            // bounds.origin is already scroll-shifted, so these are content
            // coordinates (see `byte_offset_for_position`).
            let x = event.position.x - bounds.origin.x - self.line_number_width;
            let y = event.position.y - bounds.origin.y;

            if y >= px(0.0) && x >= px(0.0) {
                let new_hovered =
                    self.byte_offset_for_position(event.position)
                        .and_then(|byte_offset| {
                            self.diagnostics.iter().position(|d| {
                                byte_offset >= d.start_byte && byte_offset < d.end_byte
                            })
                        });
                // Keep the popup up while the pointer is on it, so its
                // "Fix this" link stays reachable.
                let over_popup = self
                    .diagnostic_popup_bounds
                    .get()
                    .is_some_and(|b| b.contains(&event.position));
                let new_hovered = if new_hovered.is_none() && over_popup {
                    self.hovered_diagnostic
                } else {
                    new_hovered
                };
                if new_hovered != self.hovered_diagnostic {
                    self.hovered_diagnostic = new_hovered;
                    cx.notify();
                }
            } else if self.hovered_diagnostic.is_some() {
                self.hovered_diagnostic = None;
                cx.notify();
            }

            let play_hover = if self.editor_prefs.show_line_numbers {
                let x = f32::from(event.position.x - bounds.origin.x);
                let y = f32::from(event.position.y - bounds.origin.y);
                statements::play_glyph_hit(
                    x,
                    y,
                    f32::from(self.line_height),
                    self.display_line_count(),
                )
                .filter(|&display| self.display_line_heads_statement(display))
            } else {
                None
            };
            if play_hover != self.hovered_play_line {
                self.hovered_play_line = play_hover;
                cx.notify();
            }

            // sqlly formatting-directive hover: show the info popup only while
            // the pointer is over a directive's `-- sqlly` lead-in (not the
            // whole line, which made it pop up constantly), and keep it up while the
            // pointer moves onto the popup itself so its "see the options" link
            // stays reachable.
            let over_directive = self
                .byte_offset_for_position(event.position)
                .and_then(|off| {
                    crate::sqlly_directive::directive_keyword_span_at(&self.content, off)
                });
            let over_popup = self
                .directive_hint_bounds
                .get()
                .is_some_and(|b| b.contains(&event.position));
            let new_directive = if self.hovered_diagnostic.is_some() {
                // A diagnostic on the directive wins: its popup says what is
                // wrong (and links to the reference itself), and the info
                // popup would otherwise paint straight over it.
                None
            } else if over_directive.is_some() {
                over_directive
            } else if over_popup {
                self.hovered_sqlly_directive
            } else {
                None
            };
            if new_directive != self.hovered_sqlly_directive {
                self.hovered_sqlly_directive = new_directive;
                cx.notify();
            }

            if !self.editor_prefs.show_hover_info {
                if self.hover_info.is_some() {
                    self.clear_hover_ui();
                    cx.notify();
                }
            } else {
                let offset = self.byte_offset_for_position(event.position);
                let over_card = self
                    .hover_card_bounds
                    .get()
                    .is_some_and(|b| b.contains(&event.position));
                let inside_span = self.hover_info.as_ref().is_some_and(|(_, (start, end))| {
                    offset.is_some_and(|off| off >= *start && off <= *end)
                });
                if self.hover_info.is_some() && (inside_span || over_card) {
                    // Keep the card while the pointer stays on the word or the card.
                } else {
                    if self.hover_info.is_some() {
                        self.hover_info = None;
                        self.hover_card_bounds.set(None);
                        cx.notify();
                    }
                    self.schedule_hover_resolve(cx);
                }
            }

            self.update_link_hover(&event.modifiers, cx);
        }
    }

    /// The pointer left the editor: drop every pointer-driven popup (hover
    /// card, diagnostic and directive popups, fold preview, gutter play
    /// glyph) so none lingers over the editor while the mouse is elsewhere.
    fn handle_pointer_left(&mut self, cx: &mut Context<Self>) {
        self.last_mouse_position = None;
        self.clear_hover_ui();
        self.hovered_diagnostic = None;
        self.diagnostic_popup_bounds.set(None);
        self.diagnostic_fix_bounds.set(None);
        self.diagnostic_directive_link_bounds.set(None);
        self.hovered_sqlly_directive = None;
        self.directive_hint_bounds.set(None);
        self.hovered_play_line = None;
        self.link_hover_range = None;
        cx.notify();
    }

    fn handle_mouse_up(&mut self, _: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        self.is_selecting = false;
        self.rectangular_anchor = None;
        self.rectangular_target = None;
        // X11/Wayland convention: completing a mouse selection places it in
        // the primary selection for middle-click paste. The primary-selection
        // APIs only exist in gpui on Linux, so this must be a compile-time
        // gate, not a runtime `cfg!`.
        #[cfg(target_os = "linux")]
        if let Some(text) = self.selected_text() {
            cx.write_to_primary(gpui::ClipboardItem::new_string(text));
        }
        cx.notify();
    }

    /// Linux middle-click: place the caret and paste the primary selection.
    fn handle_middle_click(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !cfg!(target_os = "linux") {
            return;
        }
        cx.focus_self(window);
        if let Some(offset) = self.byte_offset_for_position(event.position) {
            self.clear_extra_cursors();
            self.cursor = offset;
            self.selection_anchor = None;
            // Compile-time gate: `read_from_primary` only exists in gpui on
            // Linux (the runtime check above never lets other platforms here).
            #[cfg(target_os = "linux")]
            if let Some(text) = cx.read_from_primary().and_then(|item| item.text()) {
                self.replace_selection(&text, cx);
                return;
            }
            cx.notify();
        }
    }

    /// Feature 5: When the user types a space and the previous character is a
    /// comma, and we're at the end of a line in a SELECT column context with
    /// no unclosed parentheses, expand the comma+space into `",\n{indent}"`.
    /// Returns `true` if the expansion was applied (caller should skip normal
    /// insertion), `false` otherwise.
    fn try_automagic_comma_repair(
        &mut self,
        text: &str,
        range: Option<&Range<usize>>,
        cx: &mut Context<Self>,
    ) -> bool {
        if range.is_some()
            || self.selection().is_some()
            || !self.extra_cursors.is_empty()
            || text.chars().count() != 1
            || !text
                .chars()
                .next()
                .is_some_and(|character| character.is_ascii_alphabetic() || character == '_')
        {
            return false;
        }
        let cursor = self.cursor;
        let (line, _) = self.line_col_for_offset(cursor);
        if line == 0 {
            return false;
        }
        let line_start = self.line_start_offset(line);
        if !self.content[line_start..cursor]
            .chars()
            .all(char::is_whitespace)
        {
            return false;
        }
        let previous_start = self.line_start_offset(line - 1);
        let previous_end = self.line_end_offset(line - 1);
        let previous = &self.content[previous_start..previous_end];
        let trimmed = previous.trim_end();
        let trimmed_len = trimmed.len();
        if trimmed.is_empty()
            || trimmed.ends_with(',')
            || trimmed.starts_with("--")
            || trimmed.ends_with(['+', '-', '*', '/', '.', '(', '='])
            || unclosed_parens_before_cursor(&self.content, cursor) != 0
            || !is_sql_code_at(
                &self.content,
                previous_start + trimmed.len().saturating_sub(1),
            )
        {
            return false;
        }
        let Some(clause) = list_clause_at(&self.content, cursor) else {
            return false;
        };
        let enabled = self.automagic_comma_enabled(clause);
        if !enabled || clause_header_only(trimmed, clause) {
            return false;
        }

        self.push_history();
        let (new_content, new_cursor) = match self.editor_prefs.automagic_comma_placement {
            crate::preferences::AutomagicCommaPlacement::Trailing => {
                let comma_at = previous_start + trimmed_len;
                let mut output = String::with_capacity(self.content.len() + text.len() + 1);
                output.push_str(&self.content[..comma_at]);
                output.push(',');
                output.push_str(&self.content[comma_at..cursor]);
                output.push_str(text);
                output.push_str(&self.content[cursor..]);
                (output, cursor + 1 + text.len())
            }
            crate::preferences::AutomagicCommaPlacement::Leading => {
                let mut output = String::with_capacity(self.content.len() + text.len() + 2);
                output.push_str(&self.content[..cursor]);
                output.push_str(", ");
                output.push_str(text);
                output.push_str(&self.content[cursor..]);
                (output, cursor + 2 + text.len())
            }
        };
        self.content = new_content.into();
        self.invalidate_text_derived_caches();
        self.cursor = new_cursor;
        self.selection_anchor = None;
        self.after_text_change(cx);
        true
    }

    fn automagic_comma_enabled(&self, clause: ListClause) -> bool {
        self.automagic_comma_overrides[clause.index()].unwrap_or(match clause {
            ListClause::Select => self.editor_prefs.automagic_select_commas,
            ListClause::GroupBy => self.editor_prefs.automagic_group_by_commas,
            ListClause::OrderBy => self.editor_prefs.automagic_order_by_commas,
        })
    }

    fn automagic_connectors_enabled(&self) -> bool {
        self.automagic_connector_override
            .unwrap_or(self.editor_prefs.automagic_where_connectors)
    }

    /// The connector sibling of comma repair: starting a new predicate on a
    /// fresh indented line inside WHERE or a JOIN's ON inserts the missing
    /// `AND ` before the typed character. Same trigger discipline as
    /// [`Self::try_automagic_comma_repair`] — single caret, no selection, a
    /// fresh line, balanced parens — plus predicate-specific guards: the
    /// previous line must not already end in a connector or dangling
    /// operator, and a pending `BETWEEN … AND` never triggers.
    fn try_automagic_connector_repair(
        &mut self,
        text: &str,
        range: Option<&Range<usize>>,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.automagic_connectors_enabled() {
            return false;
        }
        if range.is_some()
            || self.selection().is_some()
            || !self.extra_cursors.is_empty()
            || text.chars().count() != 1
            || !text
                .chars()
                .next()
                .is_some_and(|character| character.is_ascii_alphabetic() || character == '_')
        {
            return false;
        }
        let cursor = self.cursor;
        let (line, _) = self.line_col_for_offset(cursor);
        if line == 0 {
            return false;
        }
        let line_start = self.line_start_offset(line);
        if !self.content[line_start..cursor]
            .chars()
            .all(char::is_whitespace)
        {
            return false;
        }
        let previous_start = self.line_start_offset(line - 1);
        let previous_end = self.line_end_offset(line - 1);
        let previous = &self.content[previous_start..previous_end];
        let trimmed = previous.trim_end();
        if trimmed.is_empty()
            || trimmed.starts_with("--")
            || trimmed.ends_with(['+', '-', '*', '/', '.', '(', '=', '<', '>', ','])
            || unclosed_parens_before_cursor(&self.content, cursor) != 0
            || !is_sql_code_at(
                &self.content,
                previous_start + trimmed.len().saturating_sub(1),
            )
        {
            return false;
        }
        if predicate_line_is_incomplete(trimmed) {
            return false;
        }
        if predicate_clause_at(&self.content, cursor).is_none() {
            return false;
        }

        self.push_history();
        let mut output = String::with_capacity(self.content.len() + text.len() + 4);
        output.push_str(&self.content[..cursor]);
        output.push_str("AND ");
        output.push_str(text);
        output.push_str(&self.content[cursor..]);
        let new_cursor = cursor + 4 + text.len();
        self.content = output.into();
        self.invalidate_text_derived_caches();
        self.cursor = new_cursor;
        self.selection_anchor = None;
        self.after_text_change(cx);
        true
    }

    fn try_comma_space_expansion(
        &mut self,
        text: &str,
        range: Option<&Range<usize>>,
        cx: &mut Context<Self>,
    ) -> bool {
        if text != " " {
            return false;
        }
        let cursor_pos = range
            .map(|r| self.offset_from_utf16(r.start))
            .unwrap_or(self.cursor);
        let prev_is_comma = cursor_pos > 0
            && range.is_none()
            && self.content.as_bytes().get(cursor_pos - 1) == Some(&b',');
        if !prev_is_comma {
            return false;
        }
        let (line_idx, col) = self.line_col_for_offset(cursor_pos);
        let lines: Vec<&str> = self.content.split('\n').collect();
        let current_line = lines.get(line_idx).copied().unwrap_or("");
        let after_cursor = &current_line[col..];
        if !after_cursor.trim().is_empty() {
            return false;
        }
        if !in_select_column_context(&self.content, cursor_pos) {
            return false;
        }
        if unclosed_parens_before_cursor(&self.content, cursor_pos) > 0 {
            return false;
        }
        // The next character is already a newline. If the following line is a
        // blank (whitespace-only) column slot, hop the caret onto it and open
        // the popup rather than inserting a second blank line. Otherwise fall
        // through and insert a fresh indented line — this is the common case of
        // adding a column after the last one when the next line starts the
        // FROM (or another) clause.
        if self.content.as_bytes().get(cursor_pos) == Some(&b'\n') {
            let next_line_start = cursor_pos + 1;
            let next_line_end = self.content[next_line_start..]
                .find('\n')
                .map_or(self.content.len(), |idx| next_line_start + idx);
            let next_line = &self.content[next_line_start..next_line_end];
            if !next_line.is_empty() && next_line.trim().is_empty() {
                // Pure caret move onto the existing slot — no text change, so
                // skip `after_text_change` and just reposition + open the popup.
                self.cursor = next_line_end;
                self.selection_anchor = None;
                self.scroll_cursor_into_view();
                self.open_column_completions(cx);
                return true;
            }
        }
        let indent: String = current_line
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        let expansion = format!(",\n{}", indent);
        let mut new_content = String::with_capacity(self.content.len() + expansion.len());
        new_content.push_str(&self.content[..cursor_pos - 1]);
        new_content.push_str(&expansion);
        new_content.push_str(&self.content[cursor_pos..]);
        self.push_delta(cursor_pos - 1..cursor_pos, expansion.len());
        self.content = new_content.into();
        self.invalidate_text_derived_caches();
        self.cursor = cursor_pos - 1 + expansion.len();
        self.selection_anchor = None;
        self.after_text_change(cx);
        self.open_column_completions(cx);
        true
    }
}

fn svg_color(color: Hsla) -> String {
    let rgba: gpui::Rgba = color.into();
    format!(
        "#{:02x}{:02x}{:02x}",
        (rgba.r * 255.0).round() as u8,
        (rgba.g * 255.0).round() as u8,
        (rgba.b * 255.0).round() as u8
    )
}

fn write_xml_escaped(output: &mut String, text: &str) {
    for character in text.chars() {
        match character {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '"' => output.push_str("&quot;"),
            '\'' => output.push_str("&apos;"),
            '\t' | ' '..='\u{10ffff}' => output.push(character),
            _ => {}
        }
    }
}

/// Add the editor's standard commands to a context menu, nesting each
/// `Submenu` group. A separator is placed only between rendered items, never
/// leading or trailing; `leading` says whether items already sit above.
fn add_standard_items(
    mut menu: PopupMenu,
    items: &[StandardContextItem],
    leading: bool,
    entity: &Entity<SqlEditor>,
    focus: &FocusHandle,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    let mut need_separator = leading;
    let mut any = false;
    for item in items {
        match item {
            StandardContextItem::Separator => need_separator = any || leading,
            StandardContextItem::Command {
                label,
                enabled,
                action,
                run,
            } => {
                if need_separator {
                    menu = menu.separator();
                    need_separator = false;
                }
                any = true;
                let entity = entity.clone();
                let run = *run;
                // The action is what the menu renders the shortcut from; the
                // click still runs the handler directly so the menu closes and
                // the command lands on this editor whatever has focus.
                let mut menu_item = PopupMenuItem::new(*label).disabled(!enabled).on_click(
                    move |_event, window, app| {
                        entity.update(app, |editor, cx| {
                            editor.close_context_menu(cx);
                            run(editor, window, cx);
                        });
                    },
                );
                if let Some(action) = action {
                    menu_item = menu_item.action(action.boxed_clone());
                }
                menu = menu.item(menu_item);
            }
            StandardContextItem::Submenu { label, items } => {
                if need_separator {
                    menu = menu.separator();
                    need_separator = false;
                }
                any = true;
                let items = items.clone();
                let entity = entity.clone();
                let focus = focus.clone();
                menu = menu.submenu(*label, window, cx, move |sub, window, cx| {
                    // Submenus resolve key bindings in the editor's context too,
                    // so their shortcuts match the keyboard.
                    let sub = sub.action_context(focus.clone());
                    add_standard_items(sub, &items, false, &entity, &focus, window, cx)
                });
            }
        }
    }
    menu
}
