// Browser build: swap tokio's channels/spawn for the reactor-free shim so
// the `tokio::` paths below compile unchanged.
#[cfg(target_arch = "wasm32")]
use crate::wasm_tokio as tokio;

use std::collections::HashSet;
use std::path::PathBuf;

use gpui::{
    AnyElement, Context, Entity, EventEmitter, IntoElement, KeyDownEvent, MouseDownEvent, Render,
    ScrollHandle, SharedString, Styled, Subscription, Window, actions, div, point, prelude::*, px,
    relative,
};

actions!(preferences, [ClosePreferences]);

/// The key context the Preferences dialog dispatches under. It exists so
/// `escape` can be bound *here* specifically: `main.rs` binds a context-less
/// `escape` to `CancelTabSwitcher`, and a matched binding consumes the
/// keystroke before any `on_key_down` listener runs — which is why Esc did
/// nothing in this window no matter how the handler was written. A binding
/// scoped to this context is deeper in the dispatch path, so it wins.
pub const PREFS_KEY_CONTEXT: &str = "Preferences";

use crate::external_editor::{
    ExternalEditorId, ExternalEditorPreferences, ExternalEditorPreferencesStore,
    ExternalEditorPreset,
};
use crate::field_input::FieldInput;
use crate::fim_completion::{
    DEFAULT_STOP_SEQUENCES, DEFAULT_SYSTEM_PROMPT, FimAiModel, FimPromptFormat, FimTriggerKind,
};
use crate::gesture_prefs::{
    GesturePreferences, QuerySwapGestureDirection, ResultContainerScrollModifier,
};
use crate::preferences::{
    self, AliasGeneration, AutomagicCommaPlacement, BoolFormatStyle, BooleanOperatorPlacement,
    CaseExpressionStyle, CellDetailSide, ClauseBodyPlacement, CommaStyle, CommentStyle, CopyFormat,
    CteStyle, DateTimeZoneMode, DdlNavigationModifier, DdlNavigationPresentation,
    DdlPopupMonitorPreference, DdlStyle, DecimalSeparator, DmlStyle, EpochUnit, FunctionArgStyle,
    FunctionCase, GridDensity, IdentifierCase, IdentifierQuoting, InListStyle, IndentStyle,
    IntelliSenseSource, JoinLayout, KeywordCase, KeywordGroupCasing, MajorClausePlacement,
    NegativeFormatStyle, NumberStyle, OnClausePlacement, OrderGroupLayout, PreferencesPane,
    PreferencesStore, PreferencesTreeNode, PreferencesUiState, Preset, ResultKind,
    ResultTextAlignment, SelectListStyle, SemicolonStyle, SqlDialect, SubqueryStyle,
    ThousandsSeparator, ValuesStyle, build_preferences_tree,
};
use crate::result_export_code::CodeLanguage;
use crate::results::ExportFormat;
use crate::theme::Theme;
use gpui_component::IndexPath;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{InputEvent, InputState};
use gpui_component::radio::Radio;
use gpui_component::select::{Select, SelectEvent, SelectState};
use gpui_component::slider::{Slider, SliderEvent, SliderState};
use gpui_component::{Disableable, Selectable};

/// A `select_dropdown` option's on-select handler.
type DropdownHandler =
    std::rc::Rc<dyn Fn(&mut PreferencesWindow, &mut Window, &mut Context<PreferencesWindow>)>;
/// −/+ stepper button handler.
type StepperHandler = std::rc::Rc<dyn Fn(&mut PreferencesWindow, &mut Context<PreferencesWindow>)>;
/// One entry in a `select_dropdown` menu: `(display label, is-selected, on-select)`.
type DropdownOption = (String, bool, DropdownHandler);
/// Routes a text input's current text into the matching store field. One of
/// these is registered per input in the constructor and runs from the input's
/// `InputEvent::Change` subscription.
type InputApply = Box<dyn Fn(&mut PreferencesWindow, String, &mut Context<PreferencesWindow>)>;

/// Backing state for one `select_dropdown` id. The gpui-component
/// `SelectState` owns the popup's open/close and highlight state internally;
/// this slot only bridges the dropdown's per-render option handlers to the
/// `SelectEvent::Confirm` subscription, and caches the last label set pushed
/// into the widget so items are only re-pushed when they actually change.
struct SelectSlot {
    state: Entity<SelectState<Vec<String>>>,
    handlers: std::rc::Rc<std::cell::RefCell<Vec<DropdownHandler>>>,
    labels: Vec<String>,
    _subscription: Subscription,
}

/// Lazily-created text field for a per-column override (JSON path, template).
type OverrideTextApply =
    std::rc::Rc<dyn Fn(&mut PreferencesWindow, String, &mut Context<PreferencesWindow>)>;

struct OverrideTextSlot {
    state: Entity<InputState>,
    apply: std::rc::Rc<std::cell::RefCell<OverrideTextApply>>,
    _subscription: Subscription,
}

/// Routes a color picker's committed color (`None` = cleared) into the store.
type ColorApply = std::rc::Rc<
    dyn Fn(&mut PreferencesWindow, Option<gpui::Hsla>, &mut Context<PreferencesWindow>),
>;

/// Lazily-created color picker (per-kind "Color cells by data type" colors).
struct ColorSlot {
    state: Entity<gpui_component::color_picker::ColorPickerState>,
    apply: std::rc::Rc<std::cell::RefCell<ColorApply>>,
    _subscription: Subscription,
}

/// What a numeric preference field's text means right now.
///
/// The distinction that matters is `Empty` vs `Invalid`. A cleared field is
/// someone mid-edit and must not be nagged; `abc` is a mistake and must not be
/// silently dropped, which is exactly what the old `if let Ok(..)` setters did —
/// they treated both cases as "do nothing, say nothing".
#[derive(Debug, PartialEq, Eq)]
enum FieldParse<T> {
    Empty,
    Invalid,
    Value(T),
}

/// Parse a numeric preference field. Pure so the Empty/Invalid/Value contract is
/// testable without a window, a store, or the user's real preference files.
///
/// Thousands separators are stripped because people paste `50,000` out of a
/// query plan and mean fifty thousand.
fn parse_field<T: std::str::FromStr>(text: &str) -> FieldParse<T> {
    let cleaned = text.trim().replace([',', '_'], "");
    if cleaned.is_empty() {
        return FieldParse::Empty;
    }
    match cleaned.parse::<T>() {
        Ok(v) => FieldParse::Value(v),
        Err(_) => FieldParse::Invalid,
    }
}

/// Apply an edit to the SQL-formatting set and mark it `Custom`.
///
/// Pure, and separate from the store write, so the invariant this function
/// exists for — *a hand edit always invalidates the preset name* — can be
/// asserted directly. A preset is a description of the values; leaving it
/// reading "Default" after the values change makes the dropdown lie.
fn sql_edited(
    mut s: preferences::SqlFormattingPreferences,
    f: impl FnOnce(&mut preferences::SqlFormattingPreferences),
) -> preferences::SqlFormattingPreferences {
    f(&mut s);
    s.preset = Preset::Custom;
    s
}

/// Lets [`PreferencesWindow::sql_enum`] label any of the ported SQL-formatting
/// enums without each of the ~20 call sites repeating
/// `|v| v.display_title().to_string()`. The titles themselves are the ported
/// Swift strings and are asserted in `preferences.rs`.
trait SqlOption: PartialEq + Clone + 'static {
    fn title(&self) -> String;
}

macro_rules! impl_sql_option {
    ($($t:ty),+ $(,)?) => {
        $(impl SqlOption for $t {
            fn title(&self) -> String {
                self.display_title().to_string()
            }
        })+
    };
}

impl_sql_option!(
    preferences::KeywordCase,
    preferences::IdentifierCase,
    preferences::IdentifierQuoting,
    preferences::MajorClausePlacement,
    preferences::SelectListStyle,
    preferences::CommaStyle,
    preferences::BooleanOperatorPlacement,
    preferences::JoinLayout,
    preferences::OnClausePlacement,
    preferences::FunctionCase,
    preferences::FunctionArgStyle,
    preferences::SubqueryStyle,
    preferences::CteStyle,
    preferences::DmlStyle,
    preferences::ValuesStyle,
    preferences::DdlStyle,
    preferences::CommentStyle,
    preferences::SqlDialect,
    preferences::IndentStyle,
    preferences::CaseExpressionStyle,
    preferences::InListStyle,
    preferences::OrderGroupLayout,
    preferences::SemicolonStyle,
);

/// The Table-after-FROM and First-condition dropdowns read their option names
/// from the catalog (the hint text beneath them quotes the same name). The
/// persisted value is the enum's serde name; this text is display-only.
impl SqlOption for preferences::ClauseBodyPlacement {
    fn title(&self) -> String {
        crate::localization::text(match self {
            preferences::ClauseBodyPlacement::NewLineIndented => {
                "settings.sql_formatting.clause_body_placement.new_line_indented"
            }
            preferences::ClauseBodyPlacement::SameLine => {
                "settings.sql_formatting.clause_body_placement.same_line"
            }
        })
    }
}

pub enum PreferencesWindowEvent {
    Closed,
    /// The theme family or appearance mode changed; the app should re-theme
    /// live surfaces (grids, chrome) without closing the dialog.
    ThemeChanged,
    /// Preferences → Display's results font or size changed; re-apply grid
    /// metrics to open result grids.
    ResultsFontChanged,
    /// Language was applied; rebuild translated chrome and refresh every window.
    LanguageChanged,
    /// One or more IntelliSense data-model cache files were deleted from the
    /// IntelliSense Cache pane. The app should drop any in-memory snapshot whose
    /// backing file is now gone, so a later visit to that database refreshes
    /// from the server instead of serving the cleared snapshot.
    IntelliSenseCacheChanged,
    /// Snippets & Quick Actions were saved; the app should reload the JSON,
    /// rebuild the command registry, and remount user snippets onto the
    /// completion catalog without closing the dialog.
    SnippetsChanged,
    /// Open the MCP Activity audit-log viewer from the MCP settings pane.
    #[cfg(not(target_arch = "wasm32"))]
    OpenMcpActivity,
}

impl EventEmitter<PreferencesWindowEvent> for PreferencesWindow {}

/// macOS System Settings-style Preferences dialog: 250px-wide searchable sidebar
/// of folders + leaves, detail pane on the right, themed to match the rest of
/// the app. Mirrors the structure of the Swift `PreferencesController` so each
/// pane has the same headings, controls, and labels as the macOS counterpart.
pub struct PreferencesWindow {
    store: PreferencesStore,
    /// The full preference tree (n-level folders → leaf panes).
    tree: Vec<PreferencesTreeNode>,
    filtered_tree: Vec<PreferencesTreeNode>,
    expanded: HashSet<String>,
    selected_pane: PreferencesPane,
    search_query: String,
    /// Per-pane lowercased rendered text; what the sidebar search matches
    /// against. Built by `rebuild_search_index` (see `search.rs`).
    search_index: std::collections::HashMap<PreferencesPane, String>,
    /// True when the index must be rebuilt (cleared by the warm-up build that
    /// runs on the frame after the dialog's first paint).
    index_stale: bool,
    /// The first paint has happened, so the next render may run the index
    /// build without delaying the dialog's appearance.
    index_warmup_frame_done: bool,
    /// True when `filtered_tree` must be recomputed from the current query.
    /// The recompute happens in `render`, where the window is available.
    search_dirty: bool,
    /// When `Some`, pane render bodies append their text here instead of (in
    /// addition to) rendering it — the index-pass collector.
    text_collector: std::cell::RefCell<Option<String>>,
    /// The text-field values as of this render (or index pass), so the
    /// `&self` render helpers can index and highlight what a field holds.
    /// Refreshed only while a search is active or the index is building.
    input_values: std::cell::RefCell<std::collections::HashMap<&'static str, String>>,
    /// Scroll-to-first-match is armed: the next highlighted match rendered in
    /// the visible pane scrolls itself into view and disarms this.
    pane_reveal_pending: std::cell::Cell<bool>,
    /// Per-frame guard so only the *first* match in a render carries the
    /// reveal marker. Reset at the top of `render`.
    pane_match_claimed: std::cell::Cell<bool>,
    /// The sidebar should scroll the selected row into view on the next
    /// render (set when a search is cleared or the dialog reopens).
    sidebar_reveal_pending: std::cell::Cell<bool>,
    /// Lazily-created `SelectState`s, one per `select_dropdown` id. The
    /// gpui-component widget manages its own open/close and keyboard-highlight
    /// state; this map only carries each dropdown's option handlers between
    /// renders. `RefCell` because slots are created and refreshed from
    /// `&self` render helpers.
    select_slots: std::cell::RefCell<std::collections::HashMap<&'static str, SelectSlot>>,
    /// Scheduled dumps pane: the store as last loaded, so rendering never
    /// reads `dump-schedules.json` (see `dump_schedules.rs`).
    scheduled_dumps_cache: std::cell::RefCell<dump_schedules::ScheduledDumpsCache>,
    /// Per-override free-text inputs (JSON path, missing text, template),
    /// created on first render of that override row.
    override_text_slots:
        std::cell::RefCell<std::collections::HashMap<&'static str, OverrideTextSlot>>,
    /// Color pickers, created on first render of their row.
    color_slots: std::cell::RefCell<std::collections::HashMap<&'static str, ColorSlot>>,
    /// Transient selection for the "Add column override" form (the kind the
    /// new override will be created with). Not persisted until Add is clicked.
    override_kind: ResultKind,
    /// Transient scope for the "Add pattern" form on Sensitive columns.
    masking_new_scope: sqlly_core::RuleScope,
    /// Free-text input widget states, keyed by element id. Created once in the
    /// constructor; each carries an `InputEvent::Change` subscription that
    /// routes the text into the matching store field.
    inputs: std::collections::HashMap<&'static str, FieldInput>,
    /// Per-input counts of pending programmatic `set_value` echoes.
    /// gpui-component emits `InputEvent::Change` for programmatic sets too, so
    /// [`Self::set_input_value`] pre-increments this and the Change
    /// subscription consumes exactly that many events — a reset or preset
    /// re-seed must not re-run the field's apply logic (which could, e.g.,
    /// stamp the SQL preset `Custom` or re-create a just-removed override).
    suppressed_input_changes: std::collections::HashMap<&'static str, usize>,
    /// DDL popup position sliders (shown as percent 0–100, stored 0.0–1.0).
    ddl_popup_x_slider: Entity<SliderState>,
    ddl_popup_y_slider: Entity<SliderState>,
    /// Input/slider event subscriptions; dropped with the view.
    _subscriptions: Vec<Subscription>,
    /// Cloud AI provider state: API key inputs + fetched model lists + status.
    cloud_ai: CloudAIState,
    /// Decision model pane: config + key entry + Test status.
    decision_ai: DecisionAiState,
    coding_agent_cli: CodingAgentCliState,
    /// On device pane: result of the latest local-runtime probe.
    ai_runtime_probe: AiRuntimeProbe,
    /// Monotonic token for runtime probes. A completion carrying a stale token
    /// is dropped, so a slow first Check can never overwrite a newer one.
    ai_runtime_probe_generation: u64,
    /// Single-request AI workspace. Closing Settings drops its prompts,
    /// response, metrics, and any in-progress request.
    ai_playground: Entity<crate::ai_playground::AiPlayground>,
    #[cfg(not(target_arch = "wasm32"))]
    mcp_settings: Entity<crate::mcp_settings::McpSettingsView>,
    /// Keybindings editor model + search filter. Loaded lazily on first
    /// render of the Keyboard Shortcuts pane.
    keybindings_model: Option<crate::keybindings::KeybindingsEditorModel>,
    keybindings_search: String,
    /// When Some, the user has clicked a binding cell and we're capturing
    /// the next keystroke. The string is the command id for app-level
    /// bindings, or "editor:ActionName" for editor-level bindings. The
    /// usize is the slot (0-based) within the command's binding list.
    keybinding_capture: Option<(String, usize)>,
    /// Editor keybinding overrides loaded from `editor-keybindings.json`.
    /// Lazily populated alongside `keybindings_model`.
    editor_keybindings_overrides: Option<crate::editor_keybindings::EditorKeybindingMap>,
    /// Saved shortcut edits cannot safely replace GPUI's entire live keymap
    /// without erasing private component bindings, so Preferences states the
    /// activation boundary explicitly instead of pretending the change is live.
    keybindings_restart_required: bool,
    /// A prospective app/editor binding conflict. Conflicting changes are not
    /// persisted; this explains which command already owns the shortcut.
    keybinding_conflict: Option<String>,
    /// Cached list of system font names for the font-picker dropdowns.
    system_fonts: Vec<String>,
    /// Cached monitor list for the DDL Navigation popup controls. Refreshed
    /// when the pane is selected, not every render.
    monitors: Vec<crate::ddl_navigation::DdlMonitorInfo>,
    /// Scroll positions of the sidebar and detail pane, tracked so they can be
    /// persisted on close and restored on reopen.
    sidebar_scroll: ScrollHandle,
    pane_scroll: ScrollHandle,
    /// Per-input validation messages, keyed by the input's id. A numeric field
    /// that cannot parse records its complaint here instead of silently
    /// discarding the keystroke; the pane renders it under the input.
    field_errors: std::collections::HashMap<&'static str, String>,
    /// Set after the dialog claims focus on its first paint, so it does not
    /// steal focus back from a field the user has since clicked into.
    did_initial_focus: bool,
    /// Whatever was focused before the dialog claimed focus on first paint,
    /// restored on close. Without this the window stays pointed at the dead
    /// dialog handle after the entity drops, and focus-routed actions (⌘,)
    /// silently stop firing — the "Preferences won't reopen" bug.
    previous_focus: Option<gpui::FocusHandle>,
    /// True once "Reset All Overrides" has been clicked but not confirmed.
    /// Cleared when the pane changes, so an armed reset can never survive a
    /// trip elsewhere and fire on a later, unrelated click.
    reset_all_armed: bool,
    /// Focus for the dialog as a whole. GPUI routes key events along the focus
    /// path, so without this the window can receive no keys at all — which is
    /// why Esc did nothing and the keybinding-capture overlay could never see a
    /// keystroke.
    focus_handle: gpui::FocusHandle,
    /// Geometry (position, size, in-flight drag) for the unified dialog
    /// chrome that frames the Settings card.
    frame: crate::dialog::DialogFrame,
    /// Editable SQL input for the formatting pane's live preview. A real
    /// `SqlEditor` so the test area gets highlighting and normal editing.
    sql_preview_input: gpui::Entity<crate::editor::SqlEditor>,
    /// Index into [`SQL_PREVIEW_SAMPLES`] the input was last seeded from.
    sql_preview_sample: usize,
    /// Listing for the IntelliSense Cache pane. `None` until the pane is first
    /// rendered, and reset to `None` after a clear so the next render re-scans
    /// the cache directory. Held off the render hot path so the folder is read
    /// once per visit, not once per frame.
    intellisense_cache_entries: Option<Vec<sqlly_services::datamodel_cache::CachedModelEntry>>,
    /// True once "Clear all" (IntelliSense Cache pane) has been clicked but not
    /// confirmed. Its own field, not shared with `reset_all_armed`, so arming
    /// one destructive action never arms the other. Disarmed on the second
    /// click, on a single-row clear, and on a pane change.
    cache_clear_all_armed: bool,
    /// True once "Clear cached results" has been clicked but not confirmed.
    #[cfg(not(target_arch = "wasm32"))]
    result_cache_clear_armed: bool,
    /// Working copy of snippets-preferences.json. Written on Save.
    snippet_prefs: crate::snippets::SnippetPreferences,
    selected_snippet: Option<usize>,
    selected_quick: Option<usize>,
    snippet_save_error: Option<String>,
    /// Outcome of the last VS Code snippet import/export: (is_error, message).
    snippet_transfer_status: Option<(bool, String)>,
    pending_language: crate::localization::Language,
    pending_translation_file: Option<String>,
    language_status: Option<String>,
    language_error: Option<String>,
}

/// Built-in sample queries for the SQL formatting preview.
const SQL_PREVIEW_SAMPLES: &[(&str, &str)] = &[
    (
        "SELECT with JOINs",
        "select u.id, u.name, count(*) as order_count from users u left join orders o on \
         o.user_id = u.id and o.deleted_at is null where u.active = 1 and u.created_at > \
         '2026-01-01' group by u.id, u.name having count(*) > 10 order by order_count desc \
         limit 50",
    ),
    (
        "CTE + subquery",
        "with totals as (select user_id, sum(amount) as total from order_items group by user_id) \
         select t.user_id, t.total, (select max(created_at) from orders o where o.user_id = \
         t.user_id) as last_order from totals t where t.total > 100 order by t.total desc",
    ),
    (
        "INSERT + UPDATE",
        "insert into audit_log (event, actor, created_at) values ('login', 'matt', now()), \
         ('logout', 'sam', now());\nupdate users set name = 'Bob', updated_at = now() where id = 7;",
    ),
    (
        "CREATE TABLE",
        "create table users (id bigint not null primary key, email varchar(255) not null, \
         display_name text, created_at timestamp default now(), constraint uq_email unique \
         (email));",
    ),
    (
        "CASE + IN list",
        "select case when status = 'active' then 1 when status = 'trial' then 2 else 0 end as \
         code, coalesce(nickname, name, 'unknown') as label from accounts where plan in ('pro', \
         'team', 'enterprise') and region = 'eu' -- keep an eye on this filter",
    ),
];

// --- IntelliSense Cache pane helpers -----------------------------------------

/// Human-readable server/host for a cache row. Prefers the stored `server`
/// field; falls back to the host segment of the frozen `identity`
/// (`server|port|db|auth|…`) for legacy files written before `server` existed;
/// and finally to a generic label so a row is never blank.
fn cache_server_label(entry: &sqlly_services::datamodel_cache::CachedModelEntry) -> String {
    let server = entry.server.trim();
    if !server.is_empty() {
        return server.to_string();
    }
    let host = entry.identity.split('|').next().unwrap_or("").trim();
    if !host.is_empty() {
        return host.to_string();
    }
    crate::localization::text("settings.mod.static.unknown_server").to_string()
}

/// A complete, correctly-pluralized age message for one time unit.
fn cache_pluralize(count: usize, unit: &str) -> String {
    let key = match (unit, count == 1) {
        ("minute", true) => "settings.intellisense_cache.age.minute_single",
        ("minute", false) => "settings.intellisense_cache.age.minute_plural",
        ("hour", true) => "settings.intellisense_cache.age.hour_single",
        ("hour", false) => "settings.intellisense_cache.age.hour_plural",
        ("day", true) => "settings.intellisense_cache.age.day_single",
        ("day", false) => "settings.intellisense_cache.age.day_plural",
        ("month", true) => "settings.intellisense_cache.age.month_single",
        ("month", false) => "settings.intellisense_cache.age.month_plural",
        ("year", true) => "settings.intellisense_cache.age.year_single",
        _ => "settings.intellisense_cache.age.year_plural",
    };
    crate::localization::format(key, &[("count", &count.to_string())])
}

fn cache_count_label(count: usize, kind: &str) -> String {
    let key = match (kind, count == 1) {
        ("cache", true) => "settings.intellisense_cache.count.cache_single",
        ("cache", false) => "settings.intellisense_cache.count.cache_plural",
        ("model", true) => "settings.intellisense_cache.count.model_single",
        ("model", false) => "settings.intellisense_cache.count.model_plural",
        ("database", true) => "settings.intellisense_cache.count.database_single",
        _ => "settings.intellisense_cache.count.database_plural",
    };
    crate::localization::format(key, &[("count", &count.to_string())])
}

/// Coarse "how long ago" label for a cache's fetch time. `fetched` is unix
/// seconds (UTC); `now` is the current unix seconds. Future and missing times
/// are treated as recent. No "cached" prefix — the section heading supplies it.
fn cache_humanize_age(now: i64, fetched: i64) -> String {
    let secs = now.saturating_sub(fetched);
    if secs < 0 || fetched == 0 {
        return crate::localization::text("settings.mod.static.recently_2");
    }
    if secs < 60 {
        return crate::localization::text("settings.mod.static.just_now");
    }
    let mins = secs / 60;
    if mins < 60 {
        return cache_pluralize(mins as usize, "minute");
    }
    let hours = mins / 60;
    if hours < 24 {
        return cache_pluralize(hours as usize, "hour");
    }
    let days = hours / 24;
    if days < 30 {
        return cache_pluralize(days as usize, "day");
    }
    let months = days / 30;
    if months < 12 {
        return cache_pluralize(months as usize, "month");
    }
    let years = days / 365;
    cache_pluralize(years as usize, "year")
}

/// Platform-appropriate label for the "open the cache folder" button.
fn reveal_button_label() -> String {
    #[cfg(target_os = "macos")]
    {
        crate::localization::text("settings.mod.static.reveal_in_finder")
    }
    #[cfg(target_os = "windows")]
    {
        crate::localization::text("settings.mod.static.show_in_explorer")
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        crate::localization::text("settings.mod.static.open_cache_folder")
    }
}

/// State for the AI > Cloud preferences pane, keyed by provider.
///
/// The old shape held one `String` per provider seeded from the config and
/// written back on every keystroke — which, with a no-op save underneath,
/// meant a live API key sat in a plaintext field and then vanished on close.
/// Two rules keep this honest now:
///
/// 1. **A saved key is never read back into the UI.** `key_input` holds only
///    what the user is typing this session. If the Keychain has a key, the pane
///    says so; it does not show it. The only way not to display a secret is
///    not to put it in the field at all.
/// 2. **`key_saved` is cached.** Asking the Keychain is real I/O and must never
///    happen in a render path; it is refreshed on open and after each save.
#[derive(Default)]
struct CloudAIState {
    profiles: crate::cloud_ai::AiProfilesConfig,
    selected: Option<String>,
    key_input: std::collections::HashMap<String, String>,
    key_saved: std::collections::HashMap<String, bool>,
    status: Option<String>,
    fetching: bool,
    testing: bool,
    fetch_generation: u64,
    test_generation: u64,
    latency: std::collections::HashMap<String, std::time::Duration>,
    adding_kind: bool,
}

/// Decision model pane state. Same key discipline as [`CloudAIState`]: a
/// saved key is never read back into the UI, and `key_saved` is cached so the
/// keychain is never asked in a render path.
#[derive(Default)]
struct DecisionAiState {
    config: crate::decision_ai::DecisionAiConfig,
    /// What the user is typing this session; never hydrated from the keychain.
    key_input: String,
    key_saved: bool,
    status: Option<String>,
    testing: bool,
    /// Monotonic token so a stale Test completion can never overwrite a newer one.
    test_generation: u64,
}

#[derive(Default)]
struct CodingAgentCliState {
    config: crate::local_cli_agent_config::LocalCliAgentConfig,
    status: Option<String>,
    health_detail: Option<String>,
    health_ok: Option<bool>,
    checking: bool,
    check_generation: u64,
}

/// On device pane: state of the local AI runtime probe behind the Check
/// button. One value, not booleans, so "checking" can never coexist with a
/// stale result on screen.
#[derive(Default)]
enum AiRuntimeProbe {
    /// No probe has run since the dialog opened.
    #[default]
    Idle,
    /// A probe is in flight (disables the Check button).
    Checking,
    /// The runtime answered; carries the installed model names it reported.
    Reachable(Vec<String>),
    /// The runtime could not be reached; carries the error text.
    Unreachable(String),
}

// Pane implementations, split by concern. Each holds further inherent
// `impl PreferencesWindow` blocks (plus, in `layout`, the `Render` impl).
mod advanced;
mod ai;
mod appearance;
mod data_model;
mod ddl;
mod dump_schedules;
mod editor;
mod gestures;
mod history_and_caches;
mod icloud_sync;
mod init;
mod keybindings;
mod layout;
mod protobuf_schemas;
mod results;
mod search;
mod snippets;
mod sql_formatting;
mod state;
mod widgets;

// ============================================================================
// File-system helpers (small, kept local to this module so the PreferencesStore
// doesn't grow new responsibilities just for prefs UI plumbing).
// ============================================================================

fn preferences_dir() -> PathBuf {
    #[cfg(not(target_arch = "wasm32"))]
    let mut p = dirs::data_dir().unwrap_or_else(std::env::temp_dir);
    // Browser build: nominal path only — the stores read/write localStorage.
    #[cfg(target_arch = "wasm32")]
    let mut p = PathBuf::new();
    p.push("SQLLY");
    #[cfg(not(target_arch = "wasm32"))]
    if !p.exists() {
        let _ = std::fs::create_dir_all(&p);
    }
    p
}

fn external_editor_prefs_path() -> PathBuf {
    let mut p = preferences_dir();
    p.push("external-editor.json");
    p
}

fn gesture_prefs_path() -> PathBuf {
    let mut p = preferences_dir();
    p.push("gesture-preferences.json");
    p
}

fn load_gesture_prefs() -> GesturePreferences {
    crate::gesture_prefs::load_preferences(&gesture_prefs_path())
}

// ============================================================================
// Tiny utilities
// ============================================================================

fn alignment_label(a: &preferences::ResultTextAlignment) -> String {
    use preferences::ResultTextAlignment::*;
    match a {
        Left => crate::localization::text("settings.mod.option.align_left"),
        Center => crate::localization::text("settings.mod.option.align_center"),
        Right => crate::localization::text("settings.mod.option.align_right"),
    }
}

fn intellisense_label(s: &IntelliSenseSource) -> String {
    match s {
        IntelliSenseSource::BuiltIn => crate::localization::text("settings.mod.option.built"),
        IntelliSenseSource::RustEngine => {
            crate::localization::text("settings.mod.option.rust_engine")
        }
    }
}

fn timezone_mode_label(m: &DateTimeZoneMode) -> String {
    match m {
        DateTimeZoneMode::AlwaysUtc => crate::localization::text("settings.mod.option.always_utc"),
        DateTimeZoneMode::NativeAsStored => {
            crate::localization::text("settings.mod.option.native_as_stored")
        }
        DateTimeZoneMode::AlwaysLocal => {
            crate::localization::text("settings.mod.option.always_local")
        }
        DateTimeZoneMode::AlwaysInTimeZone => {
            crate::localization::text("settings.mod.option.specific_time_zone")
        }
    }
}

/// The single-bool scroll-modifier presets offered in the gestures pane, paired
/// with display labels. Mirrors `ResultContainerScrollModifier::display_title`.
fn scroll_modifier_presets() -> Vec<(String, ResultContainerScrollModifier)> {
    let base = ResultContainerScrollModifier {
        shift: false,
        command: false,
        function: false,
        control: false,
        option: false,
    };
    vec![
        (
            crate::localization::text("settings.gestures.scroll_modifier.none"),
            base,
        ),
        (
            "Shift".to_string(),
            ResultContainerScrollModifier {
                shift: true,
                ..base
            },
        ),
        (
            "Command".to_string(),
            ResultContainerScrollModifier {
                command: true,
                ..base
            },
        ),
        (
            "Control".to_string(),
            ResultContainerScrollModifier {
                control: true,
                ..base
            },
        ),
        (
            "Option".to_string(),
            ResultContainerScrollModifier {
                option: true,
                ..base
            },
        ),
        (
            "Function".to_string(),
            ResultContainerScrollModifier {
                function: true,
                ..base
            },
        ),
    ]
}

fn query_swap_label(d: &QuerySwapGestureDirection) -> String {
    match d {
        QuerySwapGestureDirection::Disabled => {
            crate::localization::text("settings.mod.option.disabled")
        }
        QuerySwapGestureDirection::Vertical => {
            crate::localization::text("settings.mod.option.vertical")
        }
        QuerySwapGestureDirection::Horizontal => {
            crate::localization::text("settings.mod.option.horizontal")
        }
    }
}

fn result_kind_label(k: &ResultKind) -> String {
    match k {
        ResultKind::Date => crate::localization::text("settings.mod.option.date"),
        ResultKind::Bool => crate::localization::text("settings.mod.option.boolean"),
        ResultKind::Uuid => "UUID".to_string(),
        ResultKind::Integer => crate::localization::text("settings.mod.option.integer"),
        ResultKind::Decimal => crate::localization::text("settings.mod.option.decimal"),
        ResultKind::Text => crate::localization::text("settings.mod.option.text_inherit"),
        ResultKind::Mask => crate::localization::text("settings.mod.option.mask"),
        ResultKind::Epoch => crate::localization::text("settings.mod.option.epoch_datetime"),
        ResultKind::JsonPath => crate::localization::text("settings.mod.option.json_path"),
        ResultKind::Template => crate::localization::text("settings.mod.option.template"),
        ResultKind::Number => crate::localization::text("settings.mod.option.number_style"),
    }
}

// ============================================================================
// Convenience: re-export FIM model defaults so callers don't need to import
// the fim_completion module just to look at the values.
// ============================================================================

#[allow(dead_code)]
const FIM_DEFAULT_SYSTEM_PROMPT_REEXPORT: &str = DEFAULT_SYSTEM_PROMPT;
#[allow(dead_code)]
const FIM_DEFAULT_STOP_SEQUENCES_REEXPORT: &[&str] = DEFAULT_STOP_SEQUENCES;

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clause_body_placement_titles_follow_the_language() {
        use preferences::ClauseBodyPlacement::{NewLineIndented, SameLine};
        let english = crate::localization::test_language(crate::localization::Language::English);
        assert_eq!(NewLineIndented.title(), "New line, indented");
        assert_eq!(SameLine.title(), "Same line as the keyword");
        drop(english);
        let _spanish = crate::localization::test_language(crate::localization::Language::Spanish);
        assert_eq!(NewLineIndented.title(), "Nueva línea con indentación");
        assert_ne!(SameLine.title(), "Same line as the keyword");
    }

    // ---- IntelliSense Cache pane helpers ----

    #[test]
    fn cache_pluralize_counts() {
        let _guard = crate::localization::test_language(crate::localization::Language::English);
        assert_eq!(cache_pluralize(1, "day"), "1 day ago");
        assert_eq!(cache_pluralize(3, "day"), "3 days ago");
    }

    #[test]
    fn cache_humanize_age_buckets_without_the_cached_prefix() {
        // Section header already says "Cached", so rows must not repeat it.
        assert_eq!(
            cache_humanize_age(1_000, 995),
            crate::localization::text("settings.mod.static.just_now")
        );
        assert_eq!(cache_humanize_age(1_000, 940), "1 minute ago");
        assert_eq!(cache_humanize_age(1_000, 700), "5 minutes ago");
        assert_eq!(cache_humanize_age(10_000, 10_000 - 3 * 3600), "3 hours ago");
        assert_eq!(
            cache_humanize_age(1_000_000, 1_000_000 - 2 * 86_400),
            "2 days ago"
        );
    }

    #[test]
    fn cache_humanize_age_guards_missing_and_future_stamps() {
        assert_eq!(
            cache_humanize_age(1_000, 0),
            crate::localization::text("settings.mod.static.recently_2")
        ); // unstamped
        assert_eq!(
            cache_humanize_age(1_000, 2_000),
            crate::localization::text("settings.mod.static.recently_2")
        ); // future / skew
    }

    #[test]
    fn cache_server_label_falls_back_from_server_to_identity_to_generic() {
        use sqlly_services::datamodel_cache::CachedModelEntry;
        let entry = |server: &str, identity: &str| CachedModelEntry {
            path: std::path::PathBuf::new(),
            server: server.to_string(),
            identity: identity.to_string(),
            database: "db".to_string(),
            fetched_at_unix: 0,
            size_bytes: 0,
        };
        assert_eq!(
            cache_server_label(&entry("prod-sql", "ignored|1433")),
            "prod-sql"
        );
        assert_eq!(
            cache_server_label(&entry("", "prod-sql|1433|db")),
            "prod-sql"
        );
        assert_eq!(
            cache_server_label(&entry("", "")),
            crate::localization::text("settings.mod.static.unknown_server")
        );
    }

    // ---- numeric field validation ----
    //
    // The bug these pin: every numeric setter used to be `if let Ok(v) =
    // text.parse() { .. }`, so `abc` and `` were both "do nothing, say
    // nothing". The field accepted the keystroke, the store kept the old value,
    // and nothing on screen admitted it.

    #[test]
    fn empty_field_is_mid_edit_not_an_error() {
        assert_eq!(parse_field::<usize>(""), FieldParse::Empty);
        assert_eq!(parse_field::<usize>("   "), FieldParse::Empty);
    }

    #[test]
    fn unparseable_field_is_invalid_not_ignored() {
        assert_eq!(parse_field::<usize>("abc"), FieldParse::Invalid);
        assert_eq!(parse_field::<usize>("12x"), FieldParse::Invalid);
        assert_eq!(parse_field::<i32>("1.5"), FieldParse::Invalid);
    }

    #[test]
    fn field_accepts_thousands_separators() {
        // People paste "50,000" out of a query plan.
        assert_eq!(parse_field::<usize>("50,000"), FieldParse::Value(50_000));
        assert_eq!(parse_field::<usize>("50_000"), FieldParse::Value(50_000));
        assert_eq!(parse_field::<usize>(" 42 "), FieldParse::Value(42));
    }

    #[test]
    fn field_parses_floats_for_fim_params() {
        assert_eq!(parse_field::<f64>("0.25"), FieldParse::Value(0.25));
        assert_eq!(parse_field::<f64>("bogus"), FieldParse::Invalid);
    }

    #[test]
    fn negative_values_parse_for_signed_fields() {
        assert_eq!(parse_field::<i32>("-1"), FieldParse::Value(-1));
        // usize can't hold it: that's Invalid, not a silent zero.
        assert_eq!(parse_field::<usize>("-1"), FieldParse::Invalid);
    }

    // ---- SQL formatting preset invariant ----

    #[test]
    fn editing_any_field_switches_the_preset_to_custom() {
        // The preset names the values. Edit a value and the name is a lie
        // unless it becomes Custom.
        let s = preferences::SqlFormattingPreferences::for_preset(Preset::Compact);
        assert_eq!(s.preset, Preset::Compact);
        let edited = sql_edited(s, |s| s.break_after_select = !s.break_after_select);
        assert_eq!(edited.preset, Preset::Custom);
    }

    #[test]
    fn editing_applies_the_change_it_was_given() {
        let s = preferences::SqlFormattingPreferences::default();
        let before = s.align_data_types;
        let edited = sql_edited(s, |s| s.align_data_types = !s.align_data_types);
        assert_eq!(edited.align_data_types, !before);
    }

    #[test]
    fn editing_a_custom_set_leaves_it_custom() {
        let s = preferences::SqlFormattingPreferences::for_preset(Preset::Custom);
        let edited = sql_edited(s, |s| s.wrap_long_lines = !s.wrap_long_lines);
        assert_eq!(edited.preset, Preset::Custom);
    }

    #[test]
    fn a_preset_is_not_custom_until_it_is_edited() {
        // `apply_sql_preset` deliberately does not route through `sql_edited`;
        // if it ever does, the dropdown would stamp Custom over the preset the
        // user just chose and this test fails.
        for preset in Preset::all() {
            let s = preferences::SqlFormattingPreferences::for_preset(*preset);
            if *preset != Preset::Custom {
                assert_ne!(
                    sql_edited(s.clone(), |_| {}).preset,
                    *preset,
                    "{preset:?} survived an edit"
                );
            }
        }
    }

    #[test]
    fn filtered_node_drops_non_matches() {
        let tree = PreferencesTreeNode::folder_with_id(
            "Results",
            "Results",
            vec![PreferencesTreeNode::leaf(
                "Date",
                PreferencesPane::ResultsDate,
            )],
        );
        let filtered = tree.filtered_with("xyz", &|_| false);
        assert!(filtered.children.is_empty());
    }

    #[test]
    fn filtered_node_keeps_matches() {
        let tree = PreferencesTreeNode::folder_with_id(
            "Results",
            "Results",
            vec![PreferencesTreeNode::leaf(
                "Date",
                PreferencesPane::ResultsDate,
            )],
        );
        let filtered = tree.filtered_with("date", &|_| false);
        assert_eq!(filtered.children.len(), 1);
        assert_eq!(filtered.children[0].title, "Date");
    }

    #[test]
    fn filtered_node_empty_query_is_identity() {
        let tree = build_preferences_tree();
        let copy = tree[0].filtered_with("", &|_| false);
        assert_eq!(copy.title, tree[0].title);
        assert_eq!(copy.children.len(), tree[0].children.len());
    }

    #[test]
    fn alignment_label_is_stable() {
        assert_eq!(
            alignment_label(&preferences::ResultTextAlignment::Left),
            "Align left"
        );
        assert_eq!(
            alignment_label(&preferences::ResultTextAlignment::Center),
            "Align center"
        );
        assert_eq!(
            alignment_label(&preferences::ResultTextAlignment::Right),
            "Align right"
        );
    }

    #[test]
    fn gesture_query_swap_labels() {
        assert_eq!(
            match QuerySwapGestureDirection::Disabled {
                QuerySwapGestureDirection::Disabled => "Disabled",
                _ => "",
            },
            "Disabled"
        );
    }

    #[test]
    fn fim_models_have_names() {
        assert!(!FimAiModel::Qwen25Coder15BBase.display_name().is_empty());
        assert!(!FimAiModel::Qwen25Coder3BBase.display_name().is_empty());
    }

    #[test]
    fn fim_formats_have_names() {
        for f in FimPromptFormat::all() {
            assert!(!f.display_name().is_empty());
        }
    }

    #[test]
    fn fim_trigger_kinds_have_names() {
        assert!(!FimTriggerKind::Automatic.display_name().is_empty());
        assert!(!FimTriggerKind::Manual.display_name().is_empty());
    }

    #[test]
    fn preferences_dir_creates_sqlly_dir() {
        // We can't reliably test the path contents in CI without touching the
        // user's filesystem; just confirm the function returns a path ending in
        // SQLLY.
        let p = preferences_dir();
        assert_eq!(p.file_name().and_then(|s| s.to_str()), Some("SQLLY"));
    }

    #[test]
    fn intellisense_labels_are_distinct_and_nonempty() {
        let a = intellisense_label(&IntelliSenseSource::BuiltIn);
        let b = intellisense_label(&IntelliSenseSource::RustEngine);
        assert!(!a.is_empty() && !b.is_empty());
        assert_ne!(a, b);
    }

    #[test]
    fn timezone_mode_labels_cover_all_variants() {
        for m in [
            DateTimeZoneMode::AlwaysUtc,
            DateTimeZoneMode::NativeAsStored,
            DateTimeZoneMode::AlwaysLocal,
            DateTimeZoneMode::AlwaysInTimeZone,
        ] {
            assert!(!timezone_mode_label(&m).is_empty());
        }
    }

    #[test]
    fn query_swap_labels_cover_all_variants() {
        assert_eq!(
            query_swap_label(&QuerySwapGestureDirection::Disabled),
            "Disabled"
        );
        assert_eq!(
            query_swap_label(&QuerySwapGestureDirection::Vertical),
            "Vertical"
        );
        assert_eq!(
            query_swap_label(&QuerySwapGestureDirection::Horizontal),
            "Horizontal"
        );
    }

    #[test]
    fn result_kind_labels_are_nonempty() {
        for k in [
            ResultKind::Date,
            ResultKind::Bool,
            ResultKind::Uuid,
            ResultKind::Integer,
            ResultKind::Decimal,
            ResultKind::Text,
            ResultKind::Mask,
            ResultKind::Epoch,
            ResultKind::JsonPath,
            ResultKind::Template,
            ResultKind::Number,
        ] {
            assert!(!result_kind_label(&k).is_empty());
        }
    }

    #[test]
    fn scroll_modifier_presets_are_single_bool_or_none() {
        let presets = scroll_modifier_presets();
        // "None" preset has no modifier bits set.
        let none = presets[0].1;
        assert!(
            !none.shift && !none.command && !none.function && !none.control && !none.option,
            "first preset must be the empty (None) modifier"
        );
        // Every other preset sets exactly one modifier bit.
        for (label, m) in presets.iter().skip(1) {
            let set = [m.shift, m.command, m.function, m.control, m.option]
                .iter()
                .filter(|b| **b)
                .count();
            assert_eq!(set, 1, "preset {label} should set exactly one modifier");
        }
    }
}
