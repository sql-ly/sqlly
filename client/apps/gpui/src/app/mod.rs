// Browser build: swap tokio's channels/spawn for the reactor-free shim so
// the `tokio::` paths below compile unchanged.
use crate::tooltip::AppTooltip;
#[cfg(target_arch = "wasm32")]
use crate::wasm_tokio as tokio;
use gpui::{
    Anchor, Animation, AnimationExt as _, AnyElement, AnyView, App, AppContext, Bounds, Context,
    CursorStyle, ElementId, Entity, EventEmitter, FocusHandle, Focusable, IntoElement,
    KeyDownEvent, ModifiersChangedEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    PathPromptOptions, Pixels, Point, PromptButton, PromptLevel, Render, ScrollHandle,
    SharedString, Styled, Task, Window, WindowAppearance, WindowBounds, WindowOptions, actions,
    anchored, deferred, div, img, point, prelude::*, px, size,
};
use gpui_component::animation::cubic_bezier;
use std::time::Duration;
#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;
#[cfg(target_arch = "wasm32")]
use web_time::Instant;

/// Base (unscaled) width of the editor zoom slider track, in px. Layout and
/// drag hit-testing both multiply this by `Theme::scaled`.
pub(crate) const ZOOM_TRACK_WIDTH: f32 = 84.0;
/// Base (unscaled) diameter of the round zoom slider handle, in px.
#[allow(dead_code)]
pub(crate) const ZOOM_HANDLE_SIZE: f32 = 12.0;

/// Maps a GPUI window appearance to a simple dark/light bool for the theme.
/// Both vibrant variants count as their base tone.
fn appearance_is_dark(appearance: WindowAppearance) -> bool {
    matches!(
        appearance,
        WindowAppearance::Dark | WindowAppearance::VibrantDark
    )
}

#[cfg(not(target_arch = "wasm32"))]
pub fn install_ssh_host_key_decider() {
    ssh_host_key::install_ssh_host_key_decider();
    ssh_host_key::install_ssh_challenge_responder();
}

mod retarget;
use retarget::{RetargetDecision, retarget_decision};

mod overlays;
mod pending_dialogs;
use pending_dialogs::*;

mod completion_cache;
mod redis_intellisense;
pub(crate) use redis_intellisense::RedisCompletionContext;
mod tabs;
use completion_cache::{CachedDataModel, format_snapshot_age};

mod ddl_navigation;
#[cfg(not(target_arch = "wasm32"))]
mod query_history;
mod query_resume;
#[cfg(not(target_arch = "wasm32"))]
mod result_cache;
mod toolbar;
pub use toolbar::{QueryToggle, QueryToolbarState, StatusLevel, ToolbarIcon};

mod account;
mod ai;
mod azure;
#[cfg(target_arch = "wasm32")]
pub(crate) mod browser_proxy;
mod commands;
mod connection;
mod execute;
mod explain;
mod explorer;
mod explorer_power;
mod files;
mod fk_follow;
mod generate_model;
mod maintenance;
mod procedure_run;
#[cfg(not(target_arch = "wasm32"))]
mod ssh_host_key;
mod transactions;
use files::{load_recent_files, reveal_path_in_file_manager};
#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod deep_links;
mod general_feedback;
#[cfg(not(target_arch = "wasm32"))]
mod mcp_proposals;
mod saved_charts;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod tab_move;
mod tab_switcher;
mod tip_of_the_day;
mod unlock;
pub(crate) use unlock::open_upgrade_surface_from;
// pub(crate) so panes outside `app` can reach `direct_route_refusal`.
mod chrome_region;
pub(crate) mod tools;
mod validation;
mod window_layout;
mod workspace;
use workspace::restored_tab_connection_target;

/// The seed query a brand-new install opens with. The welcome canvas treats
/// an editor holding exactly this text (or nothing) as untouched: the user
/// has not started writing, so the canvas may fill the central area. Type
/// anything and the workbench takes over.
const DEFAULT_TAB_SQL: &str = "SELECT TOP 100 * FROM sys.tables";
const DATAMODEL_CACHE_LIMIT: usize = 16;
const VALUE_LOOKUP_CACHE_LIMIT: usize = 100;

use sqlly_core::{AzureSqlDiscovery, ConnectionProfile, ConnectionProvenance, EntraAccountBinding};
use sqlly_intel::intellisense::CompletionCatalog;
use sqlly_services::{
    IntelliSenseRuleConfig, ProcedureInfo, Services, ValueContext, distinct_word_lookup_sql,
    parse_rules, strip_jsonc, value_lookup_cache_key,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use crate::ai_panel::{AiPanel, AiPanelEvent, AiSection};
use crate::azure_firewall_flow as firewall_flow;
use crate::command_palette::{CommandPalette, CommandPaletteEvent, CommandRegistry};
use crate::connection::ConnectionStore;
use crate::connection_editor::{ConnectionEditor, ConnectionEditorEvent};
use crate::connection_management::{ConnectionManagement, ConnectionManagementEvent};
use crate::dialog::{Dialog, DialogFrame};
use crate::editor::SqlEditor;
use crate::entity_management::{EntityManagement, EntityManagementEvent};
use crate::pane_divider::{CollapseDirection, PaneDivider};
use crate::project_file_picker::{ProjectFilePicker, ProjectFilePickerEvent};
use crate::query_history::QueryHistory;
use crate::results::{ExportFormat, LayoutMode, OutputPanelTab, ResultsView};
use crate::schema_tree::{
    DraggedIdentifier, ExplorerActionContext, ExplorerMenuAction, ExplorerTool, NodeKind,
    SchemaTreeEvent, ScriptDestination,
};
use crate::sidebar::{Sidebar, SidebarEvent};
use crate::tab_history::TabHistory;
use crate::tab_switcher::{SwitchDirection, TabMru, TabSwitcherSession};
use crate::tabs::{
    ConnectionBreadcrumb, MidRegionDestination, MidRegionTab, QueryTab, all_tabs_item_label,
    all_tabs_matches, apply_tab_reorder, auto_name_from_sql, clamp_ai_width, clamp_sidebar_width,
    dirty_dot_is_neutral, inactive_tab_connection_cue, keep_split_out_panel,
    mid_region_destination, name_looks_auto, remap_index_after_move, tab_chrome_state,
};
use crate::theme::{Theme, ThemeKind};
use crate::tool_output::{ToolOutputEvent, ToolOutputView};
use crate::workspace_dialog::{WorkspaceDialog, WorkspaceDialogEvent, WorkspaceDialogMode};
use gpui_component::Disableable as _;
use gpui_component::Icon;
use gpui_component::Sizable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::{DropdownMenu, PopupMenuItem};
use gpui_component::select::{SelectEvent, SelectState};
use gpui_component::slider::{Slider, SliderEvent, SliderState};

use crate::icon_catalog::AppIcon;

actions!(
    app_view,
    [
        ToggleCommandPalette,
        QuickOpen,
        NewQueryTab,
        NewNotebook,
        OpenNotebook,
        SaveNotebook,
        RunNotebookCell,
        RunAllNotebookCells,
        CloseTab,
        ToggleSidebar,
        ShowSqlLibrary,
        FocusExplorer,
        ToggleAiPanel,
        ToggleAgentMode,
        ToggleLogConsole,
        OpenAccount,
        ExecuteActiveQuery,
        ExecuteActiveQueryKeepResults,
        FindInResults,
        ToggleEditorResultsFocus,
        GotoColumnInResults,
        StageInsertRowInResults,
        StageDeleteRowInResults,
        StageUndoInResults,
        StageRedoInResults,
        RefreshResults,
        RunPicker,
        RunOnTargets,
        CancelActiveQuery,
        ClearAllData,
        ValidateActiveQuery,
        FormatActiveQuery,
        MinifyActiveQuery,
        OpenPreferences,
        OpenObjectProperties,
        RefreshObjectExplorer,
        Disconnect,
        ShowConnectionManagement,
        ShowEntityManagement,
        NextEditorTab,
        PreviousEditorTab,
        PreviousTabInHistory,
        NextTabInHistory,
        CancelTabSwitcher,
        MinimizeWindow,
        ZoomWindow,
        ZoomEditorIn,
        ZoomEditorOut,
        ZoomEditorReset,
        ToggleZoomPopover,
        CloseZoomPopover,
        BringAllToFront,
        Quit,
        About,
        CommitTransaction,
        RollbackTransaction,
        ToggleTransactionMode,
        NewWindow,
        OpenSql,
        OpenSqlFile,
        RunSqlFile,
        SearchDatabaseValue,
        OpenProjectFile,
        OpenSqllyExport,
        ExportConnections,
        ExportCredentials,
        ImportConnections,
        ImportConnectionsFromTools,
        ExportForPgAdmin,
        ExportForAzureDataStudio,
        ExportForPgService,
        ExportForDbeaver,
        ExportForDataGrip,
        ExportForDbGate,
        OpenFolder,
        SaveSql,
        SaveToLibrary,
        SaveResultsCsv,
        SaveWorkspaceAs,
        OpenWorkspace,
        ManageWorkspaces,
        SearchSchema,
        ToggleResultsPane,
        ToggleResultsMaximized,
        DetachResults,
        ReattachResults,
        ToggleCellDetails,
        ToggleHistoryPanel,
        HistoryPrevVersion,
        HistoryNextVersion,
        SelectOutputResults,
        SelectOutputMessages,
        SelectOutputPlan,
        SelectOutputHistory,
        SelectOutputDdl,
        SelectOutputDiff,
        ReopenClosedTab,
        ThemeSystem,
        ThemeLight,
        ThemeDark,
        ThemeHighContrast,
        GenerateSql,
        AskSchema,
        SendSelectionToAi,
        Connect,
        TemplateExplorer,
        ImportExport,
        ImportFromClipboard,
        QueryStore,
        ShowQueryHistory,
        ExtendedEvents,
        SchemaCompare,
        SchemaHistory,
        SaveSchemaSnapshot,
        DataCompare,
        DataTransfer,
        BackupRestore,
        ExportDatabaseSql,
        AddScheduledDump,
        ActivityMonitor,
        ServerDashboard,
        DiskUsageTool,
        RedisMemoryAnalyzerTool,
        ObjectBrowserTool,
        IndexAnalyzerTool,
        TestDataGeneratorTool,
        ExportSchemaDocs,
        TenantScoping,
        ResultSnapshots,
        SavedCharts,
        ChartDashboardTool,
        ConnectionHealth,
        RelationshipGraph,
        QueryBuilder,
        SqlAgent,
        Security,
        Designers,
        AzureSqlConnection,
        AddFirewallRule,
        ShowEntraAccountDialog,
        ToggleIntelliSenseDebug,
        SplitVertical,
        SplitHorizontal,
        SplitSameBufferVertical,
        SplitSameBufferHorizontal,
        CloseSplit,
        ExecutionPlanViewer,
        EstimatedPlanViewer,
        ExplainProfile,
        OpenPlanFile,
        PastePlan,
        OpenKeybindings,
        OpenMcpServerSettings,
        OpenExternalEditor,
        OpenSourceLicenses,
        SqllyHelp,
        FormatDirectivesReference,
        SendFeedback,
        TipOfTheDay,
        CheckForUpdates,
        UnlockSqlly,
        AppStoreEdition,
        PrivacyPolicy,
        RunQuickActionSlot1,
        RunQuickActionSlot2,
        RunQuickActionSlot3,
        RunQuickActionSlot4,
        RunQuickActionSlot5,
        RunQuickActionSlot6,
        RunQuickActionSlot7,
        RunQuickActionSlot8,
        RunQuickActionSlot9,
        QuickSwitchConnection1,
        QuickSwitchConnection2,
        QuickSwitchConnection3,
        QuickSwitchConnection4,
        QuickSwitchConnection5,
        QuickSwitchConnection6,
        QuickSwitchConnection7,
        QuickSwitchConnection8,
        QuickSwitchConnection9,
    ]
);

actions!(sqlly_edit, [Undo, Redo, Cut, Copy, Paste, SelectAll]);

/// Find the current index of a tab by its stable id. Extracted from
/// [`AppView::tab_index_by_id`] so the retargeting invariant — never trust an
/// index captured before an async prompt; re-resolve by id — is unit-testable
/// without a live window. A `None` result means the tab is gone (closed while a
/// prompt was up), and the caller must do nothing rather than act on a stale
/// index that now points at some other tab.
fn tab_index_of<'a>(ids: impl Iterator<Item = &'a str>, id: &str) -> Option<usize> {
    ids.enumerate().find(|(_, tid)| *tid == id).map(|(i, _)| i)
}

/// The connection state the one native application menu is built from after
/// a language switch: the frontmost workspace window's, else any workspace
/// window's, else disconnected.
fn native_menu_connected_state(active: Option<bool>, fallback: Option<bool>) -> bool {
    active.or(fallback).unwrap_or(false)
}

#[cfg(test)]
mod native_menu_tests {
    use super::native_menu_connected_state;

    #[test]
    fn native_menu_follows_the_frontmost_window_not_the_last_one() {
        assert!(native_menu_connected_state(Some(true), Some(false)));
        assert!(!native_menu_connected_state(Some(false), Some(true)));
        assert!(native_menu_connected_state(None, Some(true)));
        assert!(!native_menu_connected_state(None, None));
    }
}

/// Loading state for the signed-in account's subscription, shown in the
/// account dialog.
#[derive(Debug, Clone)]
enum AccountSubscriptionState {
    /// Not requested yet (dialog closed).
    Idle,
    /// Fetch in flight.
    Loading,
    /// Fetch completed: `Some` is an active/known subscription; `None` means
    /// signed in but on the free tier (the store answered 404).
    Loaded(Option<crate::store::Subscription>),
    /// Fetch failed; carries a user-presentable message.
    Failed(String),
}

pub struct AppView {
    services: Services,
    /// Route-aware `Services` resolution: Direct → the in-process `services`,
    /// ViaRelay → the remote-backed services installed for that profile's
    /// established relay session, and ViaRelay with NO session → an erroring
    /// bundle whose engine calls tell the user to connect the relay session
    /// (never a silent direct attempt). Shared with the connection editor so
    /// its Test Connection / database probe resolve against live session
    /// state. See [`Self::services_for_profile`].
    service_router: crate::service_router::ServiceRouter,
    /// Route facts for each established Via-Relay session — the relay endpoint
    /// the session landed on and the bridge's display name. Feeds the status
    /// bar's route-indicator tooltip; installed and cleared in lockstep with
    /// the router's remote sessions.
    remote_route_facts: HashMap<sqlly_core::ConnectionId, crate::status_bar::RelayRouteFacts>,
    /// Monotonic install generation per Via-Relay connection. A session-loss
    /// watcher fires only when its generation is still current, so an
    /// explicit disconnect (or a replacement session) can never be mistaken
    /// for a lost connection.
    #[cfg(not(target_arch = "wasm32"))]
    remote_session_generations: HashMap<sqlly_core::ConnectionId, u64>,
    sidebar: Entity<Sidebar>,
    /// Frame-cached chrome rows (tab strip + command bar, status strip);
    /// see `chrome_region.rs`. Created on first render.
    tab_bar_region: Option<Entity<crate::app::chrome_region::ChromeRegion>>,
    status_bar_region: Option<Entity<crate::app::chrome_region::ChromeRegion>>,
    /// App render counter the chrome regions measure staleness against.
    chrome_frame: u64,
    /// A chrome refresh timer is running (one at a time).
    chrome_refresh_pending: bool,
    sidebar_visible: bool,
    pub(crate) tabs: Vec<QueryTab>,
    pub(crate) active_tab: usize,
    tab_counter: usize,
    /// Tab currently being dragged for reorder, if any.
    dragging_tab: Option<usize>,
    /// Keyboard focus for the query-tab strip (Left/Right/Home/End).
    tab_strip_focus: FocusHandle,
    /// Fallback focus for the workbench root. Overlay dialogs and drawers steal
    /// a focus handle while open; when they close we restore the editor, or this
    /// handle if there is no tab, so the root action chain stays valid.
    workbench_focus: FocusHandle,
    /// Which tab the strip's keyboard caret is on.
    tab_strip_focus_index: usize,
    /// Searchable all-tabs list filter.
    all_tabs_filter: Entity<InputState>,
    all_tabs_open: bool,
    /// Click-to-open editor zoom slider on the status-bar percent.
    zoom_popover_open: bool,
    /// In-progress ctrl+tab / ctrl+shift+tab editor-tab switcher session.
    /// `None` when no gesture is active. The live selection is never changed
    /// while a session is active — only `original_index` is remembered, so a
    /// cancel (Escape / blur / click-away) restores the original focus.
    tab_switcher_session: Option<TabSwitcherSession>,
    /// Strip indices in MRU order, snapshotted when a switcher gesture starts
    /// so the overlay and commit use the same list until Ctrl is released.
    tab_switcher_order: Vec<usize>,
    /// Activation-order stack of tab ids. Front = most recently used.
    tab_mru: TabMru,
    /// Browser-style back/forward stack of tab ids. Session-only.
    tab_history: TabHistory,
    /// When true, `select_tab` updates the MRU but does not push history
    /// (back/forward navigation must not re-record the hop).
    navigating_history: bool,
    /// Right-click tab menu: `(tab index, window position)` while open.
    tab_context_menu: Option<(usize, gpui::Point<Pixels>)>,
    /// Session-only stack of recently closed tabs for Reopen Closed Tab
    /// (newest last, bounded).
    closed_tabs: Vec<tabs::ClosedTabSnapshot>,
    /// Recently opened SQL files (newest first, bounded, persisted).
    recent_files: Vec<String>,
    #[cfg(not(target_arch = "wasm32"))]
    pending_tool_import: Option<files::PendingToolImport>,
    overlays: overlays::OverlayHost,
    /// Scroll handle for the tab-switcher overlay's scrollable row list.
    /// Used to scroll the highlighted row into view on each advance.
    tab_switcher_scroll: ScrollHandle,
    ai_panel: Entity<AiPanel>,
    pub(crate) active_profile: Option<ConnectionProfile>,
    /// Live completion catalog built from the connected database's metadata.
    /// Pushed to every editor tab via `set_completion_catalog`. Default (empty)
    /// until a connection is established.
    completion_catalog: Arc<CompletionCatalog>,
    /// Background data-model pulls still running. Non-zero means the live
    /// catalog may be empty or disk-seeded; the Query Builder shows a spinner.
    datamodel_refreshes_in_flight: usize,
    /// In-scope stored procedures for `EXEC` declaration snippets.
    completion_procedures: Arc<Vec<ProcedureInfo>>,
    /// The canonical datamodel snapshot — the single source of truth for
    /// IntelliSense. Built from `MetadataService` via `build_model_snapshot`,
    /// then converted to `completion_catalog` via `from_model_snapshot`. Future
    /// metadata sources (filesystem DDL, catalog sync, etc.) plug into this
    /// snapshot; IntelliSense always reads from it.
    ///
    /// Incremental schema-tree events (`ColumnsLoaded`, `ProcedureParametersLoaded`)
    /// update this snapshot first via `merge_columns_into_snapshot` /
    /// `merge_procedure_params_into_snapshot`, then update the derived catalog /
    /// procedures, keeping the snapshot and derived state in sync.
    completion_snapshot: Arc<sqlly_lang::sqlmodel::ModelSnapshot>,
    /// Bumped by every schema change (DDL run, explorer refresh). A pull
    /// stamps the epoch it started in; while `completion_snapshot` predates
    /// the current epoch the explorer is not re-seeded from it, so an altered
    /// table is never refilled with its old columns.
    schema_change_epoch: u64,
    completion_snapshot_epoch: u64,
    /// Data-model snapshots cached per (connection, database), so switching
    /// back to a server/database combo we have already pulled applies its
    /// IntelliSense instantly instead of re-hitting the engine. Populated on
    /// every completion fetch; schema refresh from the connection chip forces
    /// a re-fetch (bypassing this cache) and overwrites the entry with a fresh
    /// snapshot. Keyed by `datamodel_key` (profile id + database name).
    /// Command IntelliSense for Redis profiles: server vocabularies and
    /// scanned key names, pushed to editors like the completion catalog.
    pub(crate) redis_intellisense: redis_intellisense::RedisIntelliSenseState,
    datamodel_cache: HashMap<(uuid::Uuid, String), CachedDataModel>,
    /// Insertion/touch order for `datamodel_cache` (most-recent last). Caps
    /// the map at [`DATAMODEL_CACHE_LIMIT`] with true LRU eviction.
    datamodel_cache_order: Vec<(uuid::Uuid, String)>,
    /// User-configurable completion rules (loaded from JSONC preferences).
    completion_rules: IntelliSenseRuleConfig,
    /// Bounded in-memory cache for distinct column-value lookups. Keyed by
    /// `value_lookup_cache_key(connection_id, database, schema, table, column)`.
    /// Cleared on disconnect / catalog refresh. Capped at [`VALUE_LOOKUP_CACHE_LIMIT`].
    value_lookup_cache: HashMap<String, Vec<sqlly_services::ValueLookupEntry>>,
    /// Insertion/touch order for `value_lookup_cache` (most-recent last).
    value_lookup_cache_order: Vec<String>,
    status_message: String,
    /// The status message the user clicked to read in full, with its level.
    /// The strip ellipsizes; this is where the untruncated text lives.
    status_detail: Option<(StatusLevel, String)>,
    /// Bounded record of look-away-worthy events (long query completions,
    /// failures, updates) behind the status-bar bell.
    notifications: crate::notification_center::NotificationCenter,
    /// System-level notification preference (bell dropdown toggle).
    os_notify: crate::notification_center::OsNotifyPreferences,
    /// Mirrors `window.is_window_active()` for paths without a `Window`
    /// (query pumps decide whether to raise a system notification).
    window_is_active: bool,
    /// Whether the bell's dropdown is open.
    notifications_open: bool,
    /// Long-running file jobs (exports, dumps, transfers) for the task center.
    background_tasks: crate::background_tasks::BackgroundTasks,
    /// Whether the task-center dropdown is open.
    tasks_open: bool,
    #[cfg(not(target_arch = "wasm32"))]
    dump_task_id: Option<crate::background_tasks::TaskId>,
    #[cfg(not(target_arch = "wasm32"))]
    transfer_task_id: Option<crate::background_tasks::TaskId>,
    /// Severity of `status_message`. The status bar is the only channel the
    /// shell has for reporting what happened, and every message used to render
    /// in the same dim gray — so a connection failure was indistinguishable
    /// from "Schema metadata refreshed". Always write both through
    /// `set_status`, never assign `status_message` directly.
    status_level: StatusLevel,
    ai_visible: bool,
    ai_width: f32,
    query_history: QueryHistory,
    pub(crate) param_history: crate::param_history::ParamHistory,
    command_registry: CommandRegistry,
    /// User snippets and quick-action slots. Reloaded when Preferences saves.
    snippet_prefs: crate::snippets::SnippetPreferences,
    named_workspace_store: crate::workspace_session::NamedWorkspaceStore,
    named_workspace_index: crate::workspace_session::NamedWorkspaceIndex,
    /// Project root the picker last used, remembered across openings.
    project_file_picker_root: Option<PathBuf>,
    formatting: crate::preferences::ResultFormattingPreferences,
    /// Results → Performance: whether the per-query timing breakdown shows in
    /// the results status bar. Pushed into every `ResultsView` alongside
    /// `formatting`; off by default.
    show_detailed_query_timing: bool,
    editor_prefs: crate::preferences::EditorPreferences,
    appearance_prefs: crate::preferences::AppearancePreferences,
    /// DDL navigation (modifier-click → go to definition) preferences.
    /// Pushed to every editor at tab creation and on preferences changes.
    ddl_nav_prefs: crate::preferences::DdlNavigationPreferences,
    detached_results_prefs: crate::preferences::DetachedResultsPreferences,
    log: crate::log_console::DiagnosticLog,
    log_console: Entity<crate::log_console::LogConsoleView>,
    /// In-window menu bar for platforms without a native application menu
    /// (Windows / Linux); not rendered on macOS.
    window_menu_bar: Entity<crate::window_menu_bar::WindowMenuBar>,
    /// Connection state the installed menus were built for, so the Tools ▸
    /// Connect/Disconnect item is only reinstalled when the connection actually
    /// toggles rather than every frame. Starts `false` to match the
    /// disconnected menu installed at launch.
    menus_reflect_connected: bool,
    /// The engine the installed menus were last gated for; `None` while
    /// disconnected. Part of the same fingerprint as
    /// `menus_reflect_connected`, so switching between connected profiles of
    /// different engines re-grays the in-window Tools entries.
    menus_reflect_engine: Option<sqlly_lang::sqlmodel::DatabaseType>,
    menus_reflect_localization_revision: u64,
    /// IntelliSense / LSP debug panel. Re-runs the same `Services` calls the
    /// `sqlly-lsp` server makes and renders the LSP wire results. Toggled by the
    /// existing `ToggleIntelliSenseDebug` action; hidden by default.
    lsp_debug: Entity<crate::lsp_debug::LspDebugView>,
    /// Debounce timer for auto-refreshing the LSP debug panel while it is
    /// visible. Replacing this field drops the prior `Task`, cancelling its
    /// pending timer fire so no stale refresh lands. `None` when no debounce is
    /// pending.
    lsp_debug_debounce_task: Option<Task<()>>,
    pub(crate) pending: PendingDialogs,
    /// One-shot: open Settings at the masking pane on the next render (the
    /// grid-menu event that requests it has no `Window` in scope).
    pending_open_masking_settings: bool,
    /// Frames (position/size) for the unified-chrome dialogs AppView renders
    /// inline (pending confirms, account/nag/update, rename, run picker…).
    /// Keyed by dialog id; a frame outlives its dialog so position persists
    /// across reopens.
    pub(crate) dialog_frames: crate::dialog::DialogFrames,
    /// Monotonic token for AI mutation reviews (stale-response rejection).
    mutation_review_generation: u64,
    /// One-shot bypass of the connection's `auto_wrap_rollback` for the next
    /// run, armed by "Execute Without Rollback Wrap". Consumed at dispatch so a
    /// production safety wrap is never left disabled beyond the single run the
    /// user asked for. Also cleared when a pending mutation review is cancelled.
    execute_rollback_bypass_once: bool,
    /// One-shot agent write grant armed by Review and run from agent mode.
    agent_write_grant: Option<crate::agent_grant::WriteGrant>,
    /// MCP write proposal awaiting execute / confirm-dialog resolution.
    #[cfg(not(target_arch = "wasm32"))]
    mcp_pending_write: Option<(uuid::Uuid, String)>,
    /// Timed write unlock for the active production connection (in-memory only).
    production_unlock: Option<sqlly_intel::query::safety::WriteUnlock>,
    /// Refresh the explorer after a database DDL run completes.
    refresh_explorer_after_run: bool,
    /// One-second tick while [`production_unlock`] is active so the banner countdown refreshes.
    production_unlock_tick: Option<Task<()>>,
    /// Timed unlock of per-connection `block_all_but_select`, keyed by profile id.
    read_only_unlocks: HashMap<String, sqlly_intel::query::safety::WriteUnlock>,
    read_only_unlock_tick: Option<Task<()>>,
    /// One-second tick while any tab holds an open manual transaction.
    manual_tx_tick: Option<Task<()>>,
    transaction_prefs: crate::preferences::TransactionPreferences,
    /// App-wide 1s ticker for per-tab result auto-refresh. Dropped when every tab is off.
    auto_refresh_ticker: Option<Task<()>>,
    /// Debounced filesystem watcher for open `.sql` tabs (native only).
    #[cfg(not(target_arch = "wasm32"))]
    file_watch: Option<crate::file_watch::FileWatchHub>,
    /// 500ms drain loop for [`file_watch`].
    #[cfg(not(target_arch = "wasm32"))]
    file_watch_ticker: Option<Task<()>>,
    /// A firewall block recognised in an error that arrived off the main
    /// thread, waiting for the next frame to become a dialog.
    ///
    /// It cannot become one where it is detected: the dialog's text fields are
    /// `InputState` entities, and those need a `&mut Window` that an async
    /// update on the view does not carry. Materializing in `render` also keeps
    /// the prompt in the window whose query failed rather than in whichever
    /// window happens to be frontmost.
    // firewall prompt lives in `pending.firewall_prompt`
    /// Server FQDN + Entra account the open dialog will act on, captured when
    /// it was armed so a later profile switch cannot redirect the ARM call at
    /// a server the user never saw named in the prompt.
    firewall_prompt_target: Option<(String, sqlly_services::security::azure::EntraAccountId)>,
    /// The free-tier pay dialog, shown at most once per
    /// [`crate::license::NAG_DIALOG_INTERVAL_HOURS`]-hour window. Decided at
    /// construction (which also records the showing) so it never re-triggers
    /// mid-session.
    show_nag_dialog: bool,
    /// The signed-in SQLLY Store session, cached so the account control can
    /// reflect signed-in state without reading the keychain on every frame.
    /// Loaded in the background after construction, and refreshed the same
    /// way on window activation (a sign-in may have finished in the separate
    /// sign-in window) and when the control is clicked. `None` when signed
    /// out, the token has expired, or the first read hasn't answered yet.
    account_session: Option<crate::store::StoreSession>,
    /// A click on the account control is waiting for its session read; a
    /// second click until then is ignored.
    account_click_pending: bool,
    /// Whether the account / subscription details dialog is open.
    show_account_dialog: bool,
    /// Set when the user copies their device id from the account dialog, so the
    /// dialog can show an inline "copied" confirmation. Reset each time the
    /// dialog opens or closes.
    account_device_id_copied: bool,
    /// A newer desktop build the store advertised, detected at startup by
    /// comparing the changelog feed's latest version to this build. `None` until
    /// the check completes (best-effort; silent on failure) or when up to date.
    update_available: Option<crate::app_update::UpdateAvailable>,
    /// When the last update check finished (any outcome). Opening the
    /// changes dialog re-checks when this is more than 30 minutes old, so
    /// the dialog never shows stale availability.
    update_checked_at: Option<Instant>,
    /// Scroll position of the update dialog's changes list; tracked so the
    /// list can draw a visible scrollbar.
    update_dialog_scroll: gpui::ScrollHandle,
    /// Whether the "update available" changes dialog is open.
    show_update_dialog: bool,
    /// Latched by the manual menu check so the shared update-status observer
    /// surfaces that check's outcome (dialog / toast) in this window only.
    awaiting_manual_update_check: bool,
    /// Latched by the launch check in the first workspace window of the
    /// process, so an update it finds opens the dialog (unless the user turned
    /// off "Show at startup"). Consumed by that check's first outcome.
    startup_update_notice_pending: bool,
    /// When set, open the download page once [`in_flight_work`] reports idle.
    update_install_when_idle: bool,
    /// Poll loop watching for idle so a deferred download can proceed.
    update_idle_poll: Option<Task<()>>,
    /// The in-app download + install of the available update.
    update_install: crate::self_update::InstallState,
    /// The download/prepare task behind [`Self::update_install`].
    update_install_task: Option<Task<()>>,
    /// Progress and cancel flag shared with the running download, so the
    /// dialog's Cancel can stop it.
    #[cfg(not(target_arch = "wasm32"))]
    update_install_progress: Option<std::sync::Arc<crate::self_update::DownloadProgress>>,
    /// The progress of a cancelled job that may still be winding down
    /// (stopping its download, removing what it staged). The next install
    /// waits for it to finish first, since both use the same scratch and
    /// staging paths.
    #[cfg(not(target_arch = "wasm32"))]
    update_install_draining: Option<std::sync::Arc<crate::self_update::DownloadProgress>>,
    /// Restart into the update as soon as it is staged (the idle-install path).
    update_restart_when_ready: bool,
    /// State of the subscription fetch backing the account dialog.
    account_subscription: AccountSubscriptionState,
    entra_accounts_registry: crate::azure_registry::EntraAccountRegistryStore,
    entra_token_cache: crate::azure_keyring::KeyringTokenCacheStore,
    entra_http_transport: crate::azure_http::ReqwestHttpTransport,
    /// Shared minter of SQL-audience access tokens (keyed by Entra account id,
    /// 5-minute refresh skew, in-memory cache). Cloned into the connection
    /// dialogs and the schema tree so every surface refreshes the token the same
    /// way the query/connect path does. In-memory only — never persisted.
    entra_sql_token_provider: crate::azure_sql_token::EntraSqlTokenProvider,
    #[cfg(not(target_arch = "wasm32"))]
    cloud_iam_token_provider: crate::cloud_iam_token::CloudIamTokenProvider,
    /// Discovered Azure SQL resources across all registered Entra accounts.
    azure_discoveries: Vec<AzureSqlDiscovery>,
    /// In-flight Azure discovery task. Replacing this field cancels the prior
    /// task.
    azure_discovery_task: Option<Task<()>>,
    toolbar: QueryToolbarState,
    /// Query-bar Server dropdown (gpui-component `Select`): the state entity
    /// owns the open/highlight/selection interaction; the app only feeds it
    /// items + selection and reacts to `SelectEvent::Confirm`.
    qbar_server_select: Entity<SelectState<Vec<String>>>,
    /// Item list currently loaded into `qbar_server_select`, used to detect
    /// when the connection set changed and the state needs `set_items`.
    qbar_server_items: Vec<String>,
    /// Selection last pushed into `qbar_server_select` by the app.
    qbar_server_selected: Option<String>,
    /// Query-bar Database dropdown (gpui-component `Select`).
    qbar_database_select: Entity<SelectState<Vec<String>>>,
    /// Item list currently loaded into `qbar_database_select`.
    qbar_database_items: Vec<String>,
    /// Selection last pushed into `qbar_database_select` by the app.
    qbar_database_selected: Option<String>,
    /// Editor zoom slider state (50–200, percent). Fed by `set_editor_zoom`;
    /// user drags come back via `SliderEvent::Change` (set_value emits no
    /// event, so programmatic sync cannot echo).
    zoom_slider: Entity<SliderState>,
    sidebar_width: f32,
    ai_panel_width: f32,
    results_height: f32,
    results_pane: crate::results_pane_state::ResultsPaneState,
    cell_detail_visible: bool,
    cell_detail_width: f32,
    cell_detail_height: f32,
    is_resizing_cell_detail: bool,
    drag_start_cell_detail_size: f32,
    log_height: f32,
    is_resizing_sidebar: bool,
    is_resizing_ai: bool,
    is_resizing_results: bool,
    is_resizing_log: bool,
    /// Dragging the active tab's DDL side-panel divider.
    is_resizing_ddl_panel: bool,
    /// Dragging the active tab's history side-panel divider.
    is_resizing_history_panel: bool,
    drag_start_pos: Pixels,
    drag_start_sidebar_width: f32,
    drag_start_ai_width: f32,
    drag_start_results_height: f32,
    drag_start_log_height: f32,
    drag_start_ddl_panel_width: f32,
    drag_start_history_panel_width: f32,
    /// Editor split (two tab groups or same-buffer two views). `None` = the
    /// usual single pane. `active_tab` stays the focused pane's tab — see
    /// `editor_groups`.
    split: Option<crate::editor_groups::SplitState>,
    /// Second editor view of a same-buffer split, sharing the active tab's
    /// document (see `SqlEditor::new_shared_view`). Always re-bound to the
    /// active tab when the active tab changes; `None` unless the split is
    /// same-buffer.
    same_buffer_view: Option<Entity<SqlEditor>>,
    /// Dragging the divider between the two split panes.
    is_resizing_split: bool,
    drag_start_split_size: f32,
    /// Tab whose DDL / history side panel a divider drag is resizing. With a
    /// split, panels render in both panes, so the dragged divider's tab is
    /// captured at drag start instead of assuming the active tab.
    ddl_resize_tab: usize,
    history_resize_tab: usize,
    /// Query version history preferences (enabled, idle seal interval, result
    /// storage, caps, git local history). Cached copy reloaded when the
    /// preferences window closes.
    /// Scheduled database dump definitions (persisted to dump-schedules.json).
    #[cfg(not(target_arch = "wasm32"))]
    dump_schedules: crate::dump_schedule::DumpSchedules,
    /// Cancel flags for in-flight scheduled dump runs, keyed by schedule id.
    #[cfg(not(target_arch = "wasm32"))]
    dump_runs: HashMap<uuid::Uuid, Arc<std::sync::atomic::AtomicBool>>,
    /// The schedule id currently owning the serialized dump runner, if any.
    #[cfg(not(target_arch = "wasm32"))]
    dump_schedule_active_id: Option<uuid::Uuid>,
    query_history_prefs: crate::preferences::QueryHistoryPreferences,
    #[cfg(not(target_arch = "wasm32"))]
    result_cache_prefs: crate::preferences::ResultCachePreferences,
    /// Lazily-opened per-tab version stores, keyed by store key (file-path
    /// hash for file-backed tabs, tab id for untitled tabs).
    #[cfg(not(target_arch = "wasm32"))]
    history_stores: std::collections::HashMap<String, crate::query_version_history::VersionStore>,
    /// Per-window random id mixed into untitled-tab store keys, so two windows
    /// that both have an untitled "tab-1" never share a history store.
    #[cfg(not(target_arch = "wasm32"))]
    history_session_id: String,
    /// Editor text-zoom multiplier (1.0 == 100% == the configured editor font
    /// size). Applied to every tab's editor via `apply_editor_zoom`. See
    /// `crate::editor_zoom` for the pure math.
    editor_zoom: f32,
    /// Pinch leftover until it crosses a 0.1 zoom step.
    pinch_zoom_accumulator: f32,
    /// Sync the zoom slider on the next render (gesture path has no `Window`).
    zoom_slider_needs_sync: bool,
    /// FIM (fill-in-the-middle) inline completion preferences, loaded from the
    /// prefs file. When `enabled` is false no requests are made.
    fim_prefs: crate::fim_completion::FimPreferences,
    /// Monotonic token for the pending FIM request. Each new debounce cancels
    /// the prior request by incrementing this; the in-flight task compares its
    /// captured token against this field and discards stale results.
    fim_request_token: u64,
    /// Debounce timer task for the pending FIM request. Replacing this field
    /// drops the prior `Task`, cancelling its pending timer fire.
    fim_debounce_task: Option<Task<()>>,
    /// Coalescing token for the debounced workspace-session autosave. Each edit
    /// bumps it; a timer a few seconds later writes the session only if the
    /// token still matches (no further edits), so unsaved buffer text survives
    /// an unclean exit without saving on every keystroke.
    session_autosave_token: u64,
    /// The most recent failed query: its error text paired with the SQL that
    /// actually produced it, captured together at failure time so "Explain
    /// Last Error" can never assert the wrong (edited, or another tab's) SQL
    /// failed. Cleared by the next successful run.
    last_query_error: Option<crate::app::ai::LastQueryError>,
    /// Debounce timer task for auto-validation on editor change. Replacing
    /// this field drops the prior `Task`, cancelling its pending fire.
    validation_debounce_task: Option<Task<()>>,
    /// Monotonic token for the pending validation request. Each new debounce
    /// cancels the prior by incrementing this; the in-flight task compares its
    /// captured token and discards stale results.
    validation_token: u64,
    /// Whether per-query AI summaries are enabled. Both kinds are generated:
    /// the long summary feeds the summary bar under the editor, the short one
    /// (15 words max) the Ctrl+Tab switcher rows.
    ai_summary_enabled: bool,
    /// Ollama model name for AI summary generation, read from the same env /
    /// prefs as FIM. Defaults to the Qwen 2.5 Coder 1.5B model.
    ai_summary_model: String,
    /// Monotonic token for the pending AI summary request. Each new debounce
    /// cancels the prior request by incrementing this.
    ai_summary_request_token: u64,
    /// Debounce timer task for the pending AI summary request.
    ai_summary_debounce_task: Option<Task<()>>,
    /// Word budget for the *next* long-summary generation, recomputed from the
    /// query editor's width on every layout (so resizes update it). Changing
    /// the budget never regenerates on the fly — the new value is simply used
    /// the next time a summary run fires.
    ai_summary_long_word_budget: Option<usize>,
    /// Persisted per-query AI summary metadata (loaded on startup, flushed on
    /// tab close / app exit).
    ai_summary_metadata: crate::query_ai_summary::QueryAISummaryMetadata,
    /// Query resume preferences (scope), loaded from disk on startup and
    /// reloaded when the preferences window closes. Mirrors the Swift
    /// `queryResumePreferences` property on `QueryWorkspaceController`.
    query_resume_prefs: crate::preferences::QueryResumePreferences,
}

/// Where each `QueryToggle` lives after the Phase 9 command bar. Every variant
/// maps; nothing is dropped. Unimplemented toggles still have a home so wiring
/// one up is a one-line `is_implemented` change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub enum QueryToggleHome {
    /// Format icon button on the command bar.
    ActionBarFormat,
    /// Bound to the Run control (run-current-statement).
    ActionBarRun,
    /// Plan capture lives on the output Plan tab / overflow.
    OutputTabPlan,
    /// AI autocomplete belongs with the AI drawer.
    AiDrawer,
    /// Overflow `⋮` menu on the command bar.
    Overflow,
}

impl QueryToggle {
    /// Command-bar slot this toggle occupies. Every variant maps.
    #[allow(dead_code)]
    pub fn action_bar_home(self) -> QueryToggleHome {
        match self {
            QueryToggle::AutoFormat => QueryToggleHome::ActionBarFormat,
            QueryToggle::RunCurrentStatement => QueryToggleHome::ActionBarRun,
            QueryToggle::ShowEstimatedPlan | QueryToggle::ShowActualPlan => {
                QueryToggleHome::OutputTabPlan
            }
            QueryToggle::AiAutocomplete => QueryToggleHome::AiDrawer,
            QueryToggle::IncludeStatistics
            | QueryToggle::TrackQueryDifference
            | QueryToggle::QueryResume
            | QueryToggle::ProcedureMode => QueryToggleHome::Overflow,
        }
    }
}

/// Minimal tooltip view rendered by the gpui `.tooltip()` closure. Just a
/// label chip with a panel background and border so it reads against any
/// surface. Built fresh on each hover by [`tooltip_view`].
struct TooltipView {
    text: SharedString,
    /// When set, the tooltip is capped at this width and the text wraps onto
    /// multiple lines (at word boundaries — words are never split).
    max_width: Option<Pixels>,
    /// Pointer position at the tooltip's first paint. A tooltip only stays up
    /// while the pointer rests near where it opened.
    opened_at: Option<gpui::Point<Pixels>>,
    /// Latched once the pointer strays past the travel budget: the tooltip
    /// stays hidden for the rest of this hover, even if the pointer returns.
    dismissed: bool,
    /// A repaint ticker runs while the tooltip is visible so pointer travel is
    /// noticed even when the movement itself would not repaint the window.
    watching: bool,
}

/// How far (logical px, pre-scale) the pointer may travel from where a tooltip
/// opened before the tooltip hides.
const TOOLTIP_TRAVEL_BUDGET: f32 = 50.0;

impl Render for TooltipView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // gpui keeps a tooltip up while its anchor is hovered, which on large
        // anchors lets the pointer wander far from the chip. The app's rule is
        // tighter: moving more than the travel budget from the open position
        // dismisses it.
        let pos = window.mouse_position();
        let opened = *self.opened_at.get_or_insert(pos);
        if !self.dismissed {
            let dx = f32::from(pos.x) - f32::from(opened.x);
            let dy = f32::from(pos.y) - f32::from(opened.y);
            if (dx * dx + dy * dy).sqrt() > Theme::scaled(TOOLTIP_TRAVEL_BUDGET) {
                self.dismissed = true;
            }
        }
        if self.dismissed {
            return gpui::Empty.into_any_element();
        }
        if !self.watching {
            self.watching = true;
            cx.spawn(async move |view, cx| {
                loop {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(80))
                        .await;
                    // Stop when the tooltip view is dropped (hover ended) or
                    // it has dismissed itself.
                    match view.update(cx, |view, cx| {
                        cx.notify();
                        view.dismissed
                    }) {
                        Ok(false) => {}
                        _ => break,
                    }
                }
            })
            .detach();
        }
        let mut chip = div()
            .px(px(Theme::scaled(6.0)))
            .py(px(Theme::scaled(3.0)))
            .rounded(px(Theme::scaled(4.0)))
            // A tooltip floats over content, so it takes the popup surface +
            // popup border — the same "above" cue as every other floating
            // surface (menus, dropdowns, the command palette).
            .bg(Theme::popup_bg())
            .border_1()
            .border_color(Theme::popup_border())
            .text_size(px(Theme::font_size_ratio(0.85)))
            .text_color(Theme::text());
        if let Some(max_width) = self.max_width {
            chip = chip.max_w(max_width).whitespace_normal();
        }
        chip.child(self.text.clone()).into_any_element()
    }
}

/// Build a fresh tooltip [`AnyView`] for the given text. Handed to
/// `StatefulInteractiveElement::tooltip`.
pub(crate) fn tooltip_view(text: impl Into<SharedString>, cx: &mut App) -> AnyView {
    cx.new(|_cx| TooltipView {
        text: text.into(),
        max_width: None,
        opened_at: None,
        dismissed: false,
        watching: false,
    })
    .into()
}

/// Like [`tooltip_view`], but auto-wraps long text at a 500px maximum width
/// (word-boundary wrapping only). Used where the tooltip carries full-sentence
/// content, e.g. a clipped AI summary.
pub(crate) fn wrapping_tooltip_view(text: impl Into<SharedString>, cx: &mut App) -> AnyView {
    cx.new(|_cx| TooltipView {
        text: text.into(),
        max_width: Some(px(500.0)),
        opened_at: None,
        dismissed: false,
        watching: false,
    })
    .into()
}

pub enum AppViewEvent {
    #[allow(dead_code)]
    StatusChanged,
}

impl EventEmitter<AppViewEvent> for AppView {}

impl AppView {
    /// Settings → Editor → Query parameters: whether `:name` / `?` / `$1`
    /// placeholders in the SQL get strip rows and are filled on Run.
    pub(crate) fn detect_query_placeholders(&self) -> bool {
        self.editor_prefs.detect_query_placeholders
    }

    pub fn new(services: Services, window: &mut Window, cx: &mut Context<Self>) -> Self {
        // Follow the OS appearance from the first paint.
        crate::theme::set_system_dark(appearance_is_dark(window.appearance()));

        // Browser preview: seed the bundled sample connections (Northwind on
        // SQLite, Chinook on PGlite) into the store before the sidebar reads
        // it, so the explorer opens on ready-to-use databases on first
        // launch. Idempotent across reloads.
        // With a relay proxy configured the proxy's listing replaces the
        // store once the socket is up (see `start_browser_proxy` below), so
        // the samples are not seeded at all.
        #[cfg(target_arch = "wasm32")]
        if !browser_proxy::configured() {
            crate::connection::ensure_browser_sample_profile();
            crate::connection::ensure_browser_pglite_profile();
        }

        // Load the process-wide connection store once at startup (password
        // migration, fail-closed corrupt-file flag).
        let _ = ConnectionStore::global();

        let diag_prefs = crate::preferences::DiagnosticLoggingPreferences::load();
        let log = crate::log_console::DiagnosticLog::with_file(
            diag_prefs.level.into(),
            crate::log_console::diagnostic_log_file(),
        );
        let sidebar = cx.new(|cx| Sidebar::new(services.clone(), log.clone(), window, cx));
        let ai_panel = cx.new(|cx| AiPanel::new(services.clone(), cx));
        let formatting = crate::preferences::ResultFormattingPreferences::load();
        let show_detailed_query_timing =
            crate::preferences::PerformancePreferences::load().show_detailed_query_timing;
        let editor_prefs = crate::preferences::EditorPreferences::load();
        editor_prefs.apply_match_options();
        let snippet_prefs = crate::snippets::SnippetPreferences::load();
        let appearance_prefs = crate::preferences::AppearancePreferences::load();
        appearance_prefs.apply_theme();
        crate::theme::Theme::set_font_prefs(
            &appearance_prefs.interface_font,
            appearance_prefs.interface_font_size,
            &appearance_prefs.editor_font,
            appearance_prefs.editor_font_size,
        );
        crate::theme::Theme::set_results_font_prefs(
            &appearance_prefs.results_font,
            appearance_prefs.results_font_size,
        );
        let log_console =
            cx.new(|cx| crate::log_console::LogConsoleView::new(log.clone(), window, cx));
        let lsp_debug = cx.new(crate::lsp_debug::LspDebugView::new);
        let window_menu_bar = cx.new(|_| {
            crate::window_menu_bar::WindowMenuBar::new(crate::menu_bar::build_menus(false))
        });

        // Restore the persisted workspace layout (sidebar width, AI panel
        // visibility/width, tab list). Mirrors the Swift `WorkspaceSession`
        // restore on app launch. Falls back to defaults on any error; an
        // unreadable session file is moved aside first so the next save
        // cannot overwrite the unsaved scripts inside it.
        #[cfg(not(target_arch = "wasm32"))]
        let (session, preserved_session_path) = crate::workspace_session::load_session_preserving(
            &crate::workspace_session::session_file(),
        );
        #[cfg(target_arch = "wasm32")]
        let session =
            crate::workspace_session::load_session(&crate::workspace_session::session_file());
        let named_workspace_store = crate::workspace_session::NamedWorkspaceStore::at_default();
        let named_workspace_index = named_workspace_store.load_index();

        let initial_zoom = crate::editor_zoom::clamp_zoom(session.layout.editor_zoom);

        // Query-bar Server/Database dropdowns. Items are synced from the
        // sidebar's connection/database lists on every frame (change-detected),
        // selection confirms route through the same connect/switch paths the
        // old kit Select's on_change used.
        let qbar_server_select =
            cx.new(|cx| SelectState::new(Vec::<String>::new(), None, window, cx));
        cx.subscribe_in(
            &qbar_server_select,
            window,
            |this: &mut Self, _state, event: &SelectEvent<Vec<String>>, _window, cx| {
                let SelectEvent::Confirm(value) = event;
                let Some(name) = value.clone().filter(|v| !v.is_empty()) else {
                    return;
                };
                this.select_server_by_name(name, cx);
            },
        )
        .detach();
        let qbar_database_select =
            cx.new(|cx| SelectState::new(Vec::<String>::new(), None, window, cx));
        cx.subscribe_in(
            &qbar_database_select,
            window,
            |this: &mut Self, _state, event: &SelectEvent<Vec<String>>, _window, cx| {
                let SelectEvent::Confirm(value) = event;
                if let Some(db) = value.clone().filter(|v| !v.is_empty()) {
                    this.select_database_by_name(db, cx);
                }
                cx.notify();
            },
        )
        .detach();

        let all_tabs_filter = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "shell.tabs.search_placeholder",
            ))
        });
        cx.subscribe_in(
            &all_tabs_filter,
            window,
            |this: &mut Self, _state, event: &InputEvent, _window, cx| {
                if matches!(event, InputEvent::Change) {
                    this.all_tabs_open = true;
                    cx.notify();
                }
            },
        )
        .detach();
        let tab_strip_focus = cx.focus_handle();
        let workbench_focus = cx.focus_handle();

        // Editor zoom slider (percent). User drags land here; programmatic
        // zoom changes are pushed into the state by `set_editor_zoom`.
        let zoom_slider = cx.new(|_| {
            SliderState::new()
                .min(crate::editor_zoom::MIN_ZOOM * 100.0)
                .max(crate::editor_zoom::MAX_ZOOM * 100.0)
                .step(1.0)
                .default_value(crate::editor_zoom::zoom_to_percent(initial_zoom) as f32)
        });
        cx.subscribe_in(
            &zoom_slider,
            window,
            |this: &mut Self, _state, event: &SliderEvent, window, cx| {
                let SliderEvent::Change(value) = event else {
                    return;
                };
                let zoom = value.start() / 100.0;
                this.apply_editor_zoom_value(zoom, false, window, cx);
            },
        )
        .detach();

        // Built before the struct literal so the shared SQL-token provider can
        // clone the same transport / keyring / registry the individual fields
        // hold.
        let entra_accounts_registry =
            crate::azure_registry::EntraAccountRegistryStore::at_default().unwrap_or_else(|e| {
                tracing::warn!(
                    error = %e,
                    "could not load entra-accounts.json; starting with empty registry"
                );
                #[cfg(not(target_arch = "wasm32"))]
                {
                    crate::azure_registry::EntraAccountRegistryStore::from_path(
                        std::env::temp_dir().join("sqlly-entra-accounts-default.json"),
                    )
                    .expect("in-memory registry should always initialize")
                }
                #[cfg(target_arch = "wasm32")]
                {
                    crate::host_storage::remove(
                        crate::host_storage::StorageArea::Config,
                        "entra-accounts.json",
                    );
                    crate::azure_registry::EntraAccountRegistryStore::at_default()
                        .expect("registry loads after clearing the corrupt store")
                }
            });
        let entra_token_cache = crate::azure_keyring::KeyringTokenCacheStore::new(
            crate::distribution::keychain_service!("SQLLY.entra"),
        );
        #[cfg(not(target_arch = "wasm32"))]
        let entra_http_transport = crate::azure_http::ReqwestHttpTransport::build_default()
            .unwrap_or_else(|e| {
                tracing::warn!(
                    error = %e,
                    "could not build reqwest HTTP client; using transport-default fallback"
                );
                crate::azure_http::ReqwestHttpTransport::new({
                    let _ = rustls::crypto::ring::default_provider().install_default();
                    reqwest::Client::new()
                })
            });
        #[cfg(target_arch = "wasm32")]
        let entra_http_transport = crate::azure_http::ReqwestHttpTransport::build_default()
            .expect("browser transport construction is infallible");
        let entra_sql_token_provider = crate::azure_sql_token::EntraSqlTokenProvider::new(
            entra_http_transport.clone(),
            entra_token_cache.clone(),
            entra_accounts_registry.clone(),
        );
        #[cfg(not(target_arch = "wasm32"))]
        let cloud_iam_token_provider = crate::cloud_iam_token::CloudIamTokenProvider::new();

        let mut app = Self {
            service_router: crate::service_router::ServiceRouter::new(services.clone()),
            remote_route_facts: HashMap::new(),
            #[cfg(not(target_arch = "wasm32"))]
            remote_session_generations: HashMap::new(),
            services,
            sidebar,
            sidebar_visible: session.layout.object_explorer_visible,
            tabs: Vec::new(),
            active_tab: 0,
            tab_counter: 0,
            dragging_tab: None,
            tab_strip_focus,
            workbench_focus,
            tab_strip_focus_index: 0,
            all_tabs_filter,
            all_tabs_open: false,
            zoom_popover_open: false,
            tab_switcher_session: None,
            tab_switcher_order: Vec::new(),
            tab_mru: TabMru::default(),
            tab_history: TabHistory::new(TabHistory::DEFAULT_CAP),
            navigating_history: false,
            tab_context_menu: None,
            closed_tabs: Vec::new(),
            recent_files: load_recent_files(),
            #[cfg(not(target_arch = "wasm32"))]
            pending_tool_import: None,
            overlays: overlays::OverlayHost::default(),
            tab_switcher_scroll: ScrollHandle::new(),
            ai_panel,
            active_profile: None,
            completion_catalog: Arc::new(completion_cache::merge_user_snippets(
                CompletionCatalog::default(),
                &snippet_prefs,
            )),
            datamodel_refreshes_in_flight: 0,
            completion_procedures: Arc::new(Vec::new()),
            completion_snapshot: Arc::new(sqlly_lang::sqlmodel::ModelSnapshot::empty()),
            schema_change_epoch: 0,
            completion_snapshot_epoch: 0,
            redis_intellisense: redis_intellisense::RedisIntelliSenseState::default(),
            datamodel_cache: HashMap::new(),
            datamodel_cache_order: Vec::new(),
            completion_rules: AppView::load_rule_config(),
            value_lookup_cache: HashMap::new(),
            value_lookup_cache_order: Vec::new(),
            status_message: crate::localization::text("shell.status.ready"),
            status_detail: None,
            notifications: crate::notification_center::NotificationCenter::default(),
            os_notify: crate::notification_center::OsNotifyPreferences::load(),
            window_is_active: true,
            notifications_open: false,
            background_tasks: crate::background_tasks::BackgroundTasks::default(),
            tasks_open: false,
            #[cfg(not(target_arch = "wasm32"))]
            dump_task_id: None,
            #[cfg(not(target_arch = "wasm32"))]
            transfer_task_id: None,
            status_level: StatusLevel::Info,
            // The AI drawer never starts open, whatever the last session had
            // it at: a fresh launch should show the editor and the explorer,
            // not a panel the user has to close. Its width IS restored, so
            // reopening it lands where they left it. (Opening a saved
            // workspace is a deliberate act and does restore the drawer — see
            // `apply_workspace_session`.)
            ai_visible: false,
            ai_width: session.layout.ai_assistant_width,
            query_history: QueryHistory::new(),
            param_history: crate::param_history::ParamHistory::load(),
            command_registry: CommandRegistry::default_registry(),
            snippet_prefs,
            named_workspace_store,
            named_workspace_index,
            project_file_picker_root: None,
            formatting,
            show_detailed_query_timing,
            editor_prefs,
            appearance_prefs,
            ddl_nav_prefs: crate::preferences::DdlNavigationPreferences::load(),
            detached_results_prefs: crate::preferences::DetachedResultsPreferences::load(),
            log,
            log_console,
            window_menu_bar,
            tab_bar_region: None,
            status_bar_region: None,
            chrome_frame: 0,
            chrome_refresh_pending: false,
            menus_reflect_connected: false,
            menus_reflect_engine: None,
            menus_reflect_localization_revision: crate::localization::chrome_revision(),
            lsp_debug,
            lsp_debug_debounce_task: None,
            pending: PendingDialogs {
                explain_prefs: crate::preferences::ExplainPreferences::load(),
                ..Default::default()
            },
            pending_open_masking_settings: false,
            dialog_frames: crate::dialog::DialogFrames::new(),
            mutation_review_generation: 0,
            execute_rollback_bypass_once: false,
            agent_write_grant: None,
            #[cfg(not(target_arch = "wasm32"))]
            mcp_pending_write: None,
            production_unlock: None,
            refresh_explorer_after_run: false,
            production_unlock_tick: None,
            read_only_unlocks: HashMap::new(),
            read_only_unlock_tick: None,
            manual_tx_tick: None,
            transaction_prefs: crate::preferences::TransactionPreferences::load(),
            auto_refresh_ticker: None,
            #[cfg(not(target_arch = "wasm32"))]
            file_watch: None,
            #[cfg(not(target_arch = "wasm32"))]
            file_watch_ticker: None,
            firewall_prompt_target: None,
            show_nag_dialog: {
                // Decide once at launch. Record immediately so a relaunch
                // inside the interval stays quiet — "once per 8-hour window".
                let show = crate::license::should_show_nag_dialog_now();
                if show {
                    crate::license::record_nag_shown(chrono::Utc::now());
                }
                show
            },
            account_session: None,
            account_click_pending: false,
            show_account_dialog: false,
            account_device_id_copied: false,
            update_available: None,
            update_checked_at: None,
            update_dialog_scroll: gpui::ScrollHandle::new(),
            show_update_dialog: false,
            awaiting_manual_update_check: false,
            startup_update_notice_pending: false,
            update_install_when_idle: false,
            update_idle_poll: None,
            update_install: crate::self_update::InstallState::Idle,
            update_install_task: None,
            #[cfg(not(target_arch = "wasm32"))]
            update_install_progress: None,
            #[cfg(not(target_arch = "wasm32"))]
            update_install_draining: None,
            update_restart_when_ready: false,
            account_subscription: AccountSubscriptionState::Idle,
            entra_accounts_registry: entra_accounts_registry.clone(),
            entra_token_cache: entra_token_cache.clone(),
            entra_http_transport: entra_http_transport.clone(),
            entra_sql_token_provider,
            #[cfg(not(target_arch = "wasm32"))]
            cloud_iam_token_provider,
            azure_discoveries: Vec::new(),
            azure_discovery_task: None,
            toolbar: QueryToolbarState::default(),
            qbar_server_select,
            qbar_server_items: Vec::new(),
            qbar_server_selected: None,
            qbar_database_select,
            qbar_database_items: Vec::new(),
            qbar_database_selected: None,
            zoom_slider,
            sidebar_width: session.layout.sidebar_width,
            ai_panel_width: clamp_ai_width(session.layout.ai_assistant_width),
            results_height: session.layout.results_height,
            results_pane: session.layout.effective_results_pane(),
            cell_detail_visible: session.layout.cell_detail_visible,
            cell_detail_width: session.layout.cell_detail_width,
            cell_detail_height: session.layout.cell_detail_height,
            is_resizing_cell_detail: false,
            drag_start_cell_detail_size: 0.0,
            log_height: {
                let h = session.layout.diagnostics_height;
                if h <= 0.0 {
                    crate::workspace_session::DEFAULT_DIAGNOSTICS_HEIGHT
                } else {
                    h.clamp(
                        crate::workspace_session::MIN_DIAGNOSTICS_HEIGHT,
                        crate::workspace_session::MAX_DIAGNOSTICS_HEIGHT,
                    )
                }
            },
            is_resizing_sidebar: false,
            is_resizing_ai: false,
            is_resizing_results: false,
            is_resizing_log: false,
            is_resizing_ddl_panel: false,
            is_resizing_history_panel: false,
            drag_start_pos: px(0.0),
            drag_start_sidebar_width: 0.0,
            drag_start_ai_width: 0.0,
            drag_start_results_height: 0.0,
            drag_start_log_height: 0.0,
            drag_start_ddl_panel_width: 0.0,
            drag_start_history_panel_width: 0.0,
            split: None,
            same_buffer_view: None,
            is_resizing_split: false,
            drag_start_split_size: 0.0,
            ddl_resize_tab: 0,
            history_resize_tab: 0,
            query_history_prefs: crate::preferences::QueryHistoryPreferences::load(),
            #[cfg(not(target_arch = "wasm32"))]
            result_cache_prefs: crate::preferences::ResultCachePreferences::load(),
            #[cfg(not(target_arch = "wasm32"))]
            history_stores: std::collections::HashMap::new(),
            #[cfg(not(target_arch = "wasm32"))]
            history_session_id: uuid::Uuid::new_v4().to_string(),
            editor_zoom: initial_zoom,
            pinch_zoom_accumulator: 0.0,
            zoom_slider_needs_sync: false,
            // Load, don't default: `enabled` defaults to false, so a hardcoded
            // `::default()` here meant FIM could never be switched on no matter
            // what the user saved. Refreshed on prefs-close.
            fim_prefs: crate::fim_completion::FimPreferences::load(),
            fim_request_token: 0,
            fim_debounce_task: None,
            session_autosave_token: 0,
            last_query_error: None,
            validation_debounce_task: None,
            validation_token: 0,
            // Real persisted settings (Preferences ▸ AI ▸ On device); an empty
            // model override falls back to the FIM default model. Refreshed on
            // prefs-close like the other preference caches.
            ai_summary_enabled: crate::preferences::AiSummaryPreferences::load().enabled,
            ai_summary_model: {
                let prefs = crate::preferences::AiSummaryPreferences::load();
                match prefs.model_override() {
                    Some(model) => model.to_string(),
                    None => crate::fim_completion::FimAiModel::Qwen25Coder15BBase
                        .ollama_model_name()
                        .to_string(),
                }
            },
            ai_summary_request_token: 0,
            ai_summary_debounce_task: None,
            ai_summary_long_word_budget: None,
            ai_summary_metadata: crate::query_ai_summary::ai_summary_metadata_file()
                .and_then(|p| {
                    if p.exists() {
                        Some(crate::query_ai_summary::QueryAISummaryMetadataStore::load(
                            &p,
                        ))
                    } else {
                        None
                    }
                })
                .unwrap_or_default(),
            query_resume_prefs: crate::preferences::QueryResumePreferences::load(),
            #[cfg(not(target_arch = "wasm32"))]
            dump_schedules: crate::dump_schedule::DumpSchedules::load(),
            #[cfg(not(target_arch = "wasm32"))]
            dump_runs: HashMap::new(),
            #[cfg(not(target_arch = "wasm32"))]
            dump_schedule_active_id: None,
        };
        // The explorer browses every saved connection, so each server node
        // loads through its own route (relay session, browser relay proxy, or
        // in-process) instead of whatever the active connection installed.
        let router = app.service_router.clone();
        app.sidebar.update(cx, |s, cx| s.set_router(router, cx));
        // The stored session is a keychain / Credential Manager read: load it
        // off the UI thread so first paint never waits on it.
        app.refresh_account_session(cx);

        // Restore tabs from the session, or create a default tab if none.
        let restored_connection_store = ConnectionStore::global();
        let restore_policy = crate::preferences::TabPreferences::load().restore_policy;
        let (restore_tabs, selected) = crate::workspace_session::filter_tabs_for_restore(
            &session.tabs,
            session.selected_tab,
            restore_policy,
        );
        // A "Move to New Window" window opens with just the moved tab, not
        // a second copy of the saved session.
        #[cfg(not(target_arch = "wasm32"))]
        let moved_tab_seed = tab_move::take_pending_seed();
        #[cfg(target_arch = "wasm32")]
        let moved_tab_seed: Option<()> = None;
        if let Some(seed) = moved_tab_seed {
            #[cfg(not(target_arch = "wasm32"))]
            app.adopt_transferred_tab(seed, cx);
            #[cfg(target_arch = "wasm32")]
            let _ = seed;
        } else if restore_tabs.is_empty() {
            app.new_tab(cx, Some(DEFAULT_TAB_SQL.to_string()));
        } else {
            for tab_state in &restore_tabs {
                if let Some(path) = tab_state.notebook_path.as_ref() {
                    if std::path::Path::new(path).exists() {
                        app.open_notebook_from_path(path, cx);
                        if let Some(tab) = app.tabs.last_mut() {
                            tab.name = tab_state.title.clone();
                            tab.pinned = tab_state.pinned;
                            tab.user_named = tab_state.user_named;
                            tab.auto_named = !tab.user_named;
                            tab.ddl_panel_width = tab_state.ddl_panel_width;
                            #[cfg(not(target_arch = "wasm32"))]
                            {
                                tab.history_panel_width = tab_state.history_panel_width;
                            }
                            tab.connection_target =
                                restored_tab_connection_target(tab_state, |id| {
                                    restored_connection_store.get(id).is_some()
                                });
                            tab.restored_connection_inert = tab.connection_target.is_some();
                            #[cfg(not(target_arch = "wasm32"))]
                            {
                                tab.result_cache_key = tab_state.result_cache_key.clone();
                            }
                        }
                    } else {
                        tracing::debug!(path, "notebook file missing; skipping restore");
                    }
                    continue;
                }
                // For file-backed tabs, read content from disk. For unsaved
                // tabs, use the content saved in the session.
                let initial_text = if let Some(ref path) = tab_state.file_path {
                    std::fs::read_to_string(path)
                        .ok()
                        .or_else(|| Some(String::new()))
                } else {
                    tab_state.content.clone()
                };
                app.new_tab(cx, initial_text);
                if let Some(tab) = app.tabs.last_mut() {
                    tab.name = tab_state.title.clone();
                    tab.file_path = tab_state.file_path.clone();
                    tab.pinned = tab_state.pinned;
                    tab.user_named = tab_state.user_named;
                    tab.ddl_panel_width = tab_state.ddl_panel_width;
                    #[cfg(not(target_arch = "wasm32"))]
                    {
                        tab.history_panel_width = tab_state.history_panel_width;
                    }
                    tab.apply_restored_auto_naming(tab_state);
                    // Restore the tab's pinned connection (overriding the
                    // inherit-on-create stamp); the pin activates when the
                    // tab is focused after the session's connection restore.
                    tab.connection_target = restored_tab_connection_target(tab_state, |id| {
                        restored_connection_store.get(id).is_some()
                    });
                    tab.restored_connection_inert = tab.connection_target.is_some();
                    tab.query_params = crate::query_params_strip::ParamStripState::from_persisted(
                        &tab_state.query_params,
                    );
                    // Restore AI summary + resume state from the metadata
                    // store. The key is file_path when present, else the
                    // automatic tab number, else tab name; records saved
                    // under the display name by older builds are adopted.
                    let key = tab.metadata_key();
                    let legacy = tab.legacy_metadata_keys(Some(&tab_state.title));
                    if let Some(record) =
                        app.ai_summary_metadata.get_or_adopt(&key, &legacy).cloned()
                    {
                        Self::apply_metadata_record_to_tab(tab, &record);
                    }
                    // Recompute effective state from the loaded override +
                    // global scope.
                    let enabled = crate::preferences::QueryResumePolicy::effective_enabled(
                        app.query_resume_prefs.scope,
                        tab.is_file_backed(),
                        tab.resume_override_enabled,
                    );
                    tab.resume_effective_enabled = enabled;
                    if enabled && tab.pending_resume_connection.is_some() {
                        tab.has_pending_resume_confirmation = true;
                    } else if !enabled && tab.pending_resume_connection.is_some() {
                        // Resume OFF — clear pending connection per Swift line 5372.
                        tab.has_pending_resume_confirmation = false;
                    }
                    #[cfg(not(target_arch = "wasm32"))]
                    {
                        tab.result_cache_key = tab_state.result_cache_key.clone();
                    }
                }
            }
            // The `new_tab` counter increments per call, but restored tabs may
            // have higher numeric names; advance the counter to avoid collisions.
            let max_num = session
                .tabs
                .iter()
                .filter_map(|tab| {
                    tab.auto_number
                        .or_else(|| crate::tabs::auto_title_number(&tab.title))
                        .map(|number| number as usize)
                })
                .max()
                .unwrap_or(0);
            app.tab_counter = app.tab_counter.max(max_num);
            if !app.tabs.is_empty() {
                app.active_tab = selected.min(app.tabs.len() - 1);
            }
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            app.restore_cached_result_sets(cx);
            app.purge_orphaned_result_caches();
        }
        app.seed_tab_navigation();
        // Sync the toolbar toggle state to the restored active tab's
        // per-tab resume/diff/FIM state.
        app.sync_toolbar_to_active_tab();

        // Restore the editor split (validated by `sanitized()`, re-checked
        // against the live tab list all the same).
        if let Some(saved) = &session.layout.editor_split {
            let axis = match saved.axis {
                crate::workspace_session::SplitMode::Vertical => {
                    Some(crate::editor_groups::SplitAxis::Vertical)
                }
                crate::workspace_session::SplitMode::Horizontal => {
                    Some(crate::editor_groups::SplitAxis::Horizontal)
                }
                crate::workspace_session::SplitMode::None => None,
            };
            if let Some(axis) = axis {
                if saved.same_buffer {
                    // Same-buffer split: re-attach both views to the restored
                    // active tab's document.
                    let split = crate::editor_groups::SplitState::new_same_buffer(
                        axis,
                        saved.active_first,
                        saved.first_size,
                    );
                    if split.is_valid(app.active_tab, app.tabs.len())
                        && let Some(tab) = app.tabs.get(app.active_tab)
                        && tab.ddl_view.is_none()
                    {
                        let view = SqlEditor::new_shared_view(&tab.editor, cx);
                        cx.subscribe(&view, Self::handle_editor_event).detach();
                        app.same_buffer_view = Some(view);
                        app.split = Some(split);
                    }
                } else {
                    let split = crate::editor_groups::SplitState::new(
                        axis,
                        saved.other_tab,
                        saved.active_first,
                        saved.first_size,
                    );
                    if split.is_valid(app.active_tab, app.tabs.len()) {
                        app.split = Some(split);
                    }
                }
            }
        }
        if let Some(section) = session
            .layout
            .ai_assistant_section
            .as_deref()
            .and_then(AiSection::from_persisted_key)
        {
            app.ai_panel
                .update(cx, |panel, cx| panel.set_active_section(section, cx));
        }

        // Give the Server Explorer the shared SQL-token minter so node
        // expansion refreshes an expired Entra token instead of failing.
        let entra_provider = app.entra_sql_token_provider.clone();
        app.sidebar.update(cx, |s, cx| {
            s.set_entra_sql_token_provider(entra_provider, cx);
        });

        // Saved server/database ids remain attached to their tabs, but launch
        // is deliberately disconnected. Restoring a workspace must never make
        // a network request, prompt for credentials, or execute SQL.
        app.active_profile = None;

        // Restore the project-file picker's root so it reopens to the same
        // folder as last session.
        app.project_file_picker_root = session
            .layout
            .project_file_picker_root
            .as_deref()
            .map(PathBuf::from)
            .filter(|p| p.is_dir());
        app.sidebar.update(cx, |sidebar, cx| {
            sidebar.restore_panel(session.layout.sidebar_panel.as_deref(), cx);
        });

        cx.subscribe_in(&app.sidebar, window, Self::handle_sidebar_event)
            .detach();
        cx.subscribe(&app.ai_panel, Self::handle_ai_event).detach();
        cx.subscribe(&app.lsp_debug, Self::handle_lsp_debug_event)
            .detach();

        // NOTE: editor events are already subscribed inside `new_tab` (which
        // created every tab above). Subscribing again here double-delivered
        // every editor event for startup tabs — e.g. one modifier-click
        // opened two DDL tabs.

        // Re-theme the whole app when the OS switches between light and dark
        // while `System` is the active kind. Mirrors the manual theme handlers.
        cx.observe_window_appearance(window, |this, window, cx| {
            crate::theme::set_system_dark(appearance_is_dark(window.appearance()));
            this.refresh_all_grid_themes(cx);
            cx.notify();
        })
        .detach();

        // Cancel the tab switcher when the window loses focus (parity with
        // macOS `NSWindow.didResignKeyNotification` in
        // `QueryWorkspaceController.swift:2693`).
        cx.observe_window_activation(window, |this, window, cx| {
            this.window_is_active = window.is_window_active();
            if window.is_window_active() {
                // A sign-in (or sign-out) may have completed in the separate
                // sign-in window while this one was in the background — re-read
                // the session so the account control reflects it on return.
                this.refresh_account_session(cx);
            } else if this.tab_switcher_session.is_some() {
                this.cancel_tab_switcher(cx);
            }
        })
        .detach();

        // Load the cached Azure SQL discoveries so discovered servers appear in
        // the sidebar immediately on startup — no network, no waiting for the
        // Entra auth/token flow. Azure is only re-contacted when the user
        // explicitly refreshes an account (Explorer "Refresh" on an Azure node,
        // adding/removing an Entra account, or a new Entra sign-in), which
        // overwrites this cache via `trigger_azure_discovery`.
        app.load_cached_azure_discoveries(cx);

        // Keep the licensed-user marker fresh so paid users never see the nag
        // surfaces (unless they opted to keep the supporter message).
        app.refresh_entitlement(cx);

        // Tip of the Day: once per launch, shortly after the workbench is up.
        app.schedule_startup_tip(cx);
        app.show_app_store_edition_notice_once(cx);

        // Observe the shared update-status entity so the chip/dialog react to
        // whichever surface ran the check (menu action, About window, launch).
        let update_status = crate::update_status::handle(cx);
        cx.observe(&update_status, Self::on_update_status_changed)
            .detach();

        // Best-effort: surface a newer build in the status bar if the store has
        // one. Never blocks startup and stays silent when offline. The
        // ticker keeps checking periodically after that — including after an
        // update was already found, so an even newer build still surfaces —
        // instead of only ever checking once at launch.
        // Only the first window of the process announces at startup; windows
        // opened later just pick up the status-bar chip.
        {
            static STARTUP_UPDATE_NOTICE_CLAIMED: std::sync::atomic::AtomicBool =
                std::sync::atomic::AtomicBool::new(false);
            app.startup_update_notice_pending =
                !STARTUP_UPDATE_NOTICE_CLAIMED.swap(true, std::sync::atomic::Ordering::SeqCst);
        }
        app.report_previous_update_failure(cx);
        app.check_for_update(false, cx);
        {
            const UPDATE_RECHECK_SECS: u64 = 30 * 60;
            let weak = cx.weak_entity();
            let async_cx = cx.to_async();
            cx.spawn(async move |_this, _cx| {
                loop {
                    async_cx
                        .background_executor()
                        .timer(std::time::Duration::from_secs(UPDATE_RECHECK_SECS))
                        .await;
                    let mut cx = async_cx.clone();
                    if weak
                        .update(&mut cx, |this: &mut Self, cx| {
                            this.check_for_update(false, cx);
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .detach();
        }

        #[cfg(not(target_arch = "wasm32"))]
        {
            const MCP_PROPOSAL_POLL_SECS: u64 = 2;
            let weak = cx.weak_entity();
            let async_cx = cx.to_async();
            cx.spawn(async move |_this, _cx| {
                loop {
                    async_cx
                        .background_executor()
                        .timer(std::time::Duration::from_secs(MCP_PROPOSAL_POLL_SECS))
                        .await;
                    let mut cx = async_cx.clone();
                    if weak
                        .update(&mut cx, |this: &mut Self, cx| {
                            this.poll_mcp_proposals(cx);
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .detach();
        }

        #[cfg(not(target_arch = "wasm32"))]
        {
            const OPEN_REQUEST_POLL_SECS: u64 = 2;
            let weak = cx.weak_entity();
            let async_cx = cx.to_async();
            cx.spawn(async move |_this, _cx| {
                loop {
                    async_cx
                        .background_executor()
                        .timer(std::time::Duration::from_secs(OPEN_REQUEST_POLL_SECS))
                        .await;
                    let mut cx = async_cx.clone();
                    if weak
                        .update(&mut cx, |this: &mut Self, cx| {
                            this.poll_open_requests(cx);
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .detach();
            app.handle_startup_deep_links(cx);
        }

        // Upload feedback reports an earlier run left on disk, then keep
        // retrying anything still pending every five minutes (once per
        // process, however many windows open).
        #[cfg(not(target_arch = "wasm32"))]
        crate::feedback_uploader::sync::start();

        #[cfg(not(target_arch = "wasm32"))]
        {
            let weak = cx.weak_entity();
            let async_cx = cx.to_async();
            cx.spawn(async move |_this, _cx| {
                loop {
                    async_cx
                        .background_executor()
                        .timer(std::time::Duration::from_secs(60))
                        .await;
                    let mut cx = async_cx.clone();
                    if weak
                        .update(&mut cx, |app: &mut Self, cx| {
                            app.tick_dump_schedules(cx);
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .detach();
        }

        // iCloud sync (off unless the user turned it on): one worker per
        // process; each window refreshes when a pass changed something here.
        #[cfg(all(target_os = "macos", not(target_arch = "wasm32")))]
        {
            crate::icloud_sync::runtime::start();
            let weak = cx.weak_entity();
            let async_cx = cx.to_async();
            cx.spawn(async move |_this, _cx| {
                let mut seen = crate::icloud_sync::runtime::generation();
                loop {
                    async_cx
                        .background_executor()
                        .timer(std::time::Duration::from_secs(2))
                        .await;
                    let current = crate::icloud_sync::runtime::generation();
                    let mut cx = async_cx.clone();
                    if current == seen {
                        if weak.update(&mut cx, |_app: &mut Self, _cx| {}).is_err() {
                            break;
                        }
                        continue;
                    }
                    seen = current;
                    if weak
                        .update(&mut cx, |app: &mut Self, cx| app.icloud_sync_applied(cx))
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .detach();
        }

        #[cfg(target_arch = "wasm32")]
        {
            let mut app = app;
            app.start_browser_proxy(cx);
            return app;
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            // Unreadable local stores are kept untouched; say so once the
            // window (and its notification layer) is up.
            let mut warnings = Vec::new();
            if let Some(path) = preserved_session_path {
                warnings.push(crate::localization::format(
                    "shell.local_stores.session_preserved",
                    &[("path", &path.display().to_string())],
                ));
            }
            if let Some(error) = ConnectionStore::global().load_error() {
                warnings.push(crate::localization::format(
                    "shell.local_stores.connections_unreadable",
                    &[("error", error)],
                ));
            }
            if !warnings.is_empty() {
                cx.spawn_in(window, async move |this, cx| {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(500))
                        .await;
                    let _ = this.update_in(cx, |_, window, cx| {
                        use gpui_component::WindowExt as _;
                        for message in warnings {
                            window.push_notification(
                                gpui_component::notification::Notification::warning(message),
                                cx,
                            );
                        }
                    });
                })
                .detach();
            }
            app
        }
    }

    /// Register app-level actions as global fallbacks so menu items are always
    /// enabled, not just when a query editor has keyboard focus. The element-
    /// level `on_action` handlers on the root `stack` div only fire when that
    /// div (or a descendant) is in the focus chain. When a modal dialog or
    /// non-editor element has focus, those handlers are unreachable and the
    /// menu items appear disabled. Global handlers registered via
    /// `App::on_action` are always available regardless of focus.
    ///
    /// Handlers route to the ACTIVE `AppView` window at dispatch time (falling
    /// back to any live one), never to a captured window handle. Capturing a
    /// handle broke every menu item once the user opened a second window and
    /// closed the first: the action targeted the dead window and the `.ok()`
    /// silently discarded it — "the settings dialog will not open".
    /// The active `AppView` window, or any live one when a non-AppView window
    /// (e.g. About) is key or no window is active. Global action handlers must
    /// resolve their target through this at dispatch time instead of capturing
    /// a window handle, so they survive the original window being closed.
    /// Open a workbench window.
    ///
    /// The single place windows are created — startup, File > New Window, and
    /// the macOS Dock-icon reopen all land here. It is an associated function
    /// rather than a method on purpose: on macOS, closing the last window does
    /// not quit the app, so the one moment you most need to open a window is
    /// exactly when there is no `AppView` left to ask. Anything that requires
    /// `&mut self` to make a window is unreachable from that state.
    ///
    /// Also the only copy of the platform titlebar rules, which the old
    /// `handle_new_window` silently omitted — every secondary window on
    /// Windows/Linux got native chrome plus the app's own inline titlebar.
    pub(crate) fn open_window(
        services: Services,
        cx: &mut gpui::App,
    ) -> Option<gpui::WindowHandle<gpui_component::Root>> {
        let bounds = Bounds::centered(None, size(px(1200.), px(800.)), cx);
        let mut window_options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            app_id: Some(crate::about::APP_ID.into()),
            ..Default::default()
        };
        // Custom title bar on every platform: macOS keeps native traffic
        // lights (transparent titlebar + inset position) and the system menu;
        // Windows/Linux hide native chrome and draw min/max/close themselves.
        window_options.titlebar = Some(gpui::TitlebarOptions {
            title: Some("SQLLY".into()),
            appears_transparent: true,
            traffic_light_position: if cfg!(target_os = "macos") {
                Some(crate::window_menu_bar::macos_traffic_light_position())
            } else {
                None
            },
        });
        if cfg!(not(target_os = "macos")) {
            window_options.window_decorations = Some(gpui::WindowDecorations::Client);
        }
        if let Ok(window) = cx.open_window(window_options, move |window, cx| {
            // gpui-component requires the window's first view to be a `Root`,
            // which hosts its popup/dialog/notification layers.
            let app_view = cx.new(|cx| AppView::new(services, window, cx));
            cx.new(|cx| gpui_component::Root::new(app_view, window, cx))
        }) {
            // Reopening from the Dock with no window left leaves the app
            // unfocused otherwise: the window exists but stays behind whatever
            // the user was in.
            cx.activate(true);
            let _ = window.update(cx, |_view, window, _cx| {
                window.activate_window();
            });
            return Some(window);
        }
        None
    }

    /// The frontmost workbench window (falling back to any live one). Since
    /// the port to gpui-component every window's root view is a
    /// [`gpui_component::Root`]; a workbench window is one whose Root hosts an
    /// [`AppView`].
    pub(crate) fn active_app_window(
        cx: &gpui::App,
    ) -> Option<gpui::WindowHandle<gpui_component::Root>> {
        let hosts_app_view =
            |handle: &gpui::WindowHandle<gpui_component::Root>, cx: &gpui::App| -> bool {
                handle
                    .read(cx)
                    .ok()
                    .map(|root| root.view().clone().downcast::<AppView>().is_ok())
                    .unwrap_or(false)
            };
        cx.active_window()
            .and_then(|w| w.downcast::<gpui_component::Root>())
            .filter(|h| hosts_app_view(h, cx))
            .or_else(|| {
                cx.windows()
                    .into_iter()
                    .filter_map(|w| w.downcast::<gpui_component::Root>())
                    .find(|h| hosts_app_view(h, cx))
            })
    }

    /// Run `f` against the frontmost workbench's `AppView` with window access.
    /// The single dispatch path for global action fallbacks now that the
    /// window's root view is `gpui_component::Root` rather than `AppView`.
    ///
    /// Deliberately goes through `AnyWindowHandle` rather than
    /// `WindowHandle<Root>::update`: the typed update holds an exclusive
    /// lease on the `Root` entity for the whole callback, and any
    /// gpui-component `WindowExt` call inside `f` (`push_notification`,
    /// `open_dialog`, …) re-enters the `Root` entity (since gpui-component
    /// 0.7 it reads `Root` to reach the per-window `WindowState` plugin) — a
    /// double lease, which panics ("cannot read Root while it is already
    /// being updated").
    /// Here `Root` is only read briefly to reach the `AppView`, so `f` is
    /// free to open toasts and dialogs.
    pub(crate) fn update_active_app_view<R>(
        cx: &mut gpui::App,
        f: impl FnOnce(&mut AppView, &mut Window, &mut Context<AppView>) -> R,
    ) -> Option<R> {
        let handle = Self::active_app_window(cx)?;
        gpui::AnyWindowHandle::from(handle)
            .update(cx, |root, window, cx| {
                let root = root.downcast::<gpui_component::Root>().ok()?;
                let view = root.read(cx).view().clone().downcast::<AppView>().ok()?;
                Some(view.update(cx, |view, cx| f(view, window, cx)))
            })
            .ok()
            .flatten()
    }

    pub(crate) fn refresh_all_localized_windows(cx: &mut gpui::App) {
        let windows = cx.windows();
        crate::localization::retain_window_titles(&windows);
        let active_window = cx.active_window().map(|window| window.window_id());
        // The native application menu is one per app: rebuild it once, from
        // the frontmost workspace window's connection state, after the loop.
        let mut native_menu_connected: Option<bool> = None;
        let mut fallback_menu_connected: Option<bool> = None;
        for handle in windows {
            let is_active = active_window == Some(handle.window_id());
            let _ = handle.update(cx, |root, window, cx| {
                if let Some(title) = crate::localization::window_title(handle.window_id()) {
                    window.set_window_title(&title);
                }
                window.refresh();
                let Ok(root) = root.downcast::<gpui_component::Root>() else {
                    return;
                };
                let Ok(view) = root.read(cx).view().clone().downcast::<AppView>() else {
                    return;
                };
                view.update(cx, |view, cx| {
                    view.command_registry = CommandRegistry::default_registry();
                    view.all_tabs_filter.update(cx, |input, cx| {
                        input.set_placeholder(
                            crate::localization::text("shell.tabs.search_placeholder"),
                            window,
                            cx,
                        );
                    });
                    // Automatic `Query N` tab names follow the new language.
                    for tab in &mut view.tabs {
                        tab.relocalize_auto_name();
                    }
                    let connected = view.active_profile.is_some();
                    let engine = view
                        .active_profile
                        .as_ref()
                        .map(|profile| profile.database_type);
                    if is_active {
                        native_menu_connected = Some(connected);
                    } else if fallback_menu_connected.is_none() {
                        fallback_menu_connected = Some(connected);
                    }
                    view.menus_reflect_connected = connected;
                    view.menus_reflect_engine = engine;
                    view.menus_reflect_localization_revision =
                        crate::localization::chrome_revision();
                    // The in-window menu bar is per window: each gets its own.
                    view.window_menu_bar.update(cx, |bar, cx| {
                        bar.set_menus(crate::menu_bar::build_menus(connected));
                        bar.set_disabled_actions(Self::engine_gated_menu_actions(engine));
                        cx.notify();
                    });
                    view.sidebar.update(cx, |_sidebar, cx| cx.notify());
                    if let Some(preferences) = &view.overlays.preferences_window {
                        preferences.update(cx, |_preferences, cx| cx.notify());
                    }
                    cx.notify();
                });
            });
        }
        cx.set_menus(crate::menu_bar::build_menus(native_menu_connected_state(
            native_menu_connected,
            fallback_menu_connected,
        )));
    }

    /// App-level fallbacks for menu items and global shortcuts.
    ///
    /// `services` is needed because [`NewWindow`] must work with *no* window
    /// open: every other action here is routed to an existing `AppView`, but
    /// that routing is precisely what breaks when the last window closes.
    pub(crate) fn register_global_actions(cx: &mut gpui::App, services: Services) {
        // Sweep untitled-tab history stores left behind by a crashed or
        // force-quit session — untitled history must never persist.
        #[cfg(not(target_arch = "wasm32"))]
        crate::query_version_history::VersionStore::purge_untitled_stores();
        // Actions that operate on a window, dispatched to whichever `AppView`
        // is frontmost. These no-op when no window is open, which is correct:
        // there is nothing for "Refresh Object Explorer" to refresh.
        macro_rules! g {
            ($action:ident, $handler:ident) => {
                cx.on_action(move |action: &crate::app::$action, cx| {
                    // Menu items and shortcuts reach this global listener
                    // from inside gpui's `active_window.update(..)`, so the
                    // window is leased and `update_active_app_view` cannot
                    // read or update it (it silently no-ops). Defer until
                    // the dispatch has returned the window.
                    let action = action.clone();
                    cx.defer(move |cx| {
                        AppView::update_active_app_view(cx, |view, win, cx| {
                            view.$handler(&action, win, cx);
                        });
                    });
                });
            };
        }

        // New Window is the exception, and must never depend on a window
        // already existing: on macOS closing the last window leaves the app
        // running with only the menu bar, and this action is the way back.
        cx.on_action(move |_: &crate::app::NewWindow, cx| {
            let _ = AppView::open_window(services.clone(), cx);
        });

        g!(ToggleCommandPalette, handle_toggle_command_palette);
        g!(QuickOpen, handle_quick_open);
        g!(NewQueryTab, handle_new_query_tab_action);
        g!(NewNotebook, handle_new_notebook);
        g!(OpenNotebook, handle_open_notebook);
        g!(SaveNotebook, handle_save_notebook);
        g!(RunNotebookCell, handle_run_notebook_cell);
        g!(RunAllNotebookCells, handle_run_all_notebook_cells);
        g!(CloseTab, handle_close_tab_action);
        g!(ToggleSidebar, handle_toggle_sidebar);
        g!(ShowSqlLibrary, handle_show_sql_library);
        g!(FocusExplorer, handle_focus_explorer);
        g!(ToggleAiPanel, handle_toggle_ai_panel);
        g!(ToggleAgentMode, handle_toggle_agent_mode);
        g!(ToggleLogConsole, handle_toggle_log_console);
        g!(OpenAccount, handle_open_account);
        g!(ExecuteActiveQuery, handle_execute_active_query);
        g!(
            ExecuteActiveQueryKeepResults,
            handle_execute_active_query_keep_results
        );
        g!(FindInResults, handle_find_in_results);
        g!(ToggleEditorResultsFocus, handle_toggle_editor_results_focus);
        g!(GotoColumnInResults, handle_goto_column_in_results);
        g!(StageInsertRowInResults, handle_stage_insert_row_in_results);
        g!(StageDeleteRowInResults, handle_stage_delete_row_in_results);
        g!(StageUndoInResults, handle_stage_undo_in_results);
        g!(StageRedoInResults, handle_stage_redo_in_results);
        g!(RefreshResults, handle_refresh_results);
        g!(RunPicker, handle_run_picker);
        g!(RunOnTargets, handle_run_on_targets);
        g!(CancelActiveQuery, handle_cancel_active_query);
        g!(CommitTransaction, handle_commit_transaction);
        g!(RollbackTransaction, handle_rollback_transaction);
        g!(ToggleTransactionMode, handle_toggle_transaction_mode);
        g!(ClearAllData, handle_clear_all_data);
        g!(ValidateActiveQuery, handle_validate_active_query);
        g!(FormatActiveQuery, handle_format_active_query);
        g!(MinifyActiveQuery, handle_minify_active_query);
        g!(OpenPreferences, handle_open_preferences);
        g!(OpenObjectProperties, handle_open_object_properties);
        g!(RefreshObjectExplorer, handle_refresh_object_explorer);
        g!(Disconnect, handle_disconnect_action);
        g!(ShowConnectionManagement, handle_show_connection_management);
        g!(ShowEntityManagement, handle_show_entity_management);
        g!(OpenSql, handle_open_sql);
        g!(OpenSqlFile, handle_open_sql_file);
        g!(RunSqlFile, handle_run_sql_file);
        g!(SearchDatabaseValue, handle_search_database_value);
        g!(OpenProjectFile, handle_open_project_file);
        g!(OpenSqllyExport, handle_open_sqlly_export);
        g!(ExportConnections, handle_export_connections);
        g!(ExportCredentials, handle_export_credentials);
        g!(ImportConnections, handle_import_connections);
        g!(
            ImportConnectionsFromTools,
            handle_import_connections_from_tools
        );
        g!(ExportForPgAdmin, handle_export_for_pg_admin);
        g!(
            ExportForAzureDataStudio,
            handle_export_for_azure_data_studio
        );
        g!(ExportForPgService, handle_export_for_pg_service);
        g!(ExportForDbeaver, handle_export_for_dbeaver);
        g!(ExportForDataGrip, handle_export_for_data_grip);
        g!(ExportForDbGate, handle_export_for_db_gate);
        g!(OpenFolder, handle_open_folder);
        g!(SaveSql, handle_save_sql);
        g!(SaveToLibrary, handle_save_to_library);
        g!(SaveResultsCsv, handle_save_results);
        g!(SaveWorkspaceAs, handle_save_workspace_as);
        g!(OpenWorkspace, handle_open_workspace);
        g!(ManageWorkspaces, handle_manage_workspaces);
        g!(SearchSchema, handle_search_schema);
        g!(ToggleResultsPane, handle_toggle_results_pane);
        g!(ToggleResultsMaximized, handle_toggle_results_maximized);
        g!(DetachResults, handle_detach_results);
        g!(ReattachResults, handle_reattach_results);
        g!(ToggleCellDetails, handle_toggle_cell_details);
        g!(SelectOutputResults, handle_select_output_results);
        g!(SelectOutputMessages, handle_select_output_messages);
        g!(SelectOutputPlan, handle_select_output_plan);
        g!(SelectOutputHistory, handle_select_output_history);
        g!(SelectOutputDdl, handle_select_output_ddl);
        g!(SelectOutputDiff, handle_select_output_diff);
        g!(ReopenClosedTab, handle_reopen_closed_tab);
        #[cfg(not(target_arch = "wasm32"))]
        g!(ToggleHistoryPanel, handle_toggle_history_panel_action);
        #[cfg(not(target_arch = "wasm32"))]
        g!(HistoryPrevVersion, handle_history_prev_version_action);
        #[cfg(not(target_arch = "wasm32"))]
        g!(HistoryNextVersion, handle_history_next_version_action);
        g!(ThemeSystem, handle_theme_system);
        g!(ThemeLight, handle_theme_light);
        g!(ThemeDark, handle_theme_dark);
        g!(ThemeHighContrast, handle_theme_high_contrast);
        g!(GenerateSql, handle_generate_sql);
        g!(AskSchema, handle_ask_schema);
        g!(SendSelectionToAi, handle_send_selection_to_ai);
        g!(Connect, handle_connect);
        g!(TemplateExplorer, handle_template_explorer);
        g!(ImportExport, handle_import_export);
        g!(ImportFromClipboard, handle_import_from_clipboard);
        g!(QueryStore, handle_query_store);
        g!(DiskUsageTool, handle_disk_usage);
        g!(RedisMemoryAnalyzerTool, handle_redis_memory_analyzer);
        g!(ObjectBrowserTool, handle_object_browser);
        g!(IndexAnalyzerTool, handle_index_analyzer);
        g!(TestDataGeneratorTool, handle_test_data_generator);
        g!(ExportSchemaDocs, handle_export_schema_docs);
        g!(TenantScoping, handle_tenant_scoping);
        g!(ResultSnapshots, handle_result_snapshots);
        g!(SavedCharts, handle_saved_charts);
        g!(ChartDashboardTool, handle_chart_dashboard);
        g!(ConnectionHealth, handle_connection_health);
        g!(RelationshipGraph, handle_relationship_graph);
        g!(QueryBuilder, handle_query_builder);
        g!(ShowQueryHistory, handle_show_query_history);
        g!(ExtendedEvents, handle_extended_events);
        g!(SchemaCompare, handle_schema_compare);
        g!(SchemaHistory, handle_schema_history);
        g!(SaveSchemaSnapshot, handle_save_schema_snapshot);
        g!(DataCompare, handle_data_compare);
        #[cfg(not(target_arch = "wasm32"))]
        g!(DataTransfer, handle_data_transfer);
        g!(BackupRestore, handle_backup_restore);
        g!(ExportDatabaseSql, handle_export_database_sql);
        g!(AddScheduledDump, handle_add_scheduled_dump);
        g!(ActivityMonitor, handle_activity_monitor);
        g!(ServerDashboard, handle_server_dashboard);
        g!(SqlAgent, handle_sql_agent);
        g!(Security, handle_security);
        g!(Designers, handle_designers);
        g!(AzureSqlConnection, handle_azure_sql_connection);
        g!(ShowEntraAccountDialog, handle_show_entra_account_dialog);
        g!(AddFirewallRule, handle_add_firewall_rule);
        g!(ToggleIntelliSenseDebug, handle_toggle_intellisense_debug);
        g!(SplitVertical, handle_split_vertical);
        g!(SplitHorizontal, handle_split_horizontal);
        g!(SplitSameBufferVertical, handle_split_same_buffer_vertical);
        g!(
            SplitSameBufferHorizontal,
            handle_split_same_buffer_horizontal
        );
        g!(CloseSplit, handle_close_split);
        g!(ExecutionPlanViewer, handle_execution_plan_viewer);
        g!(EstimatedPlanViewer, handle_estimated_plan_viewer);
        g!(ExplainProfile, handle_explain_profile);
        g!(OpenPlanFile, handle_open_plan_file);
        g!(PastePlan, handle_paste_plan);
        g!(OpenKeybindings, handle_open_keybindings);
        g!(OpenMcpServerSettings, handle_open_mcp_server_settings);
        g!(OpenExternalEditor, handle_open_external_editor);
        g!(SqllyHelp, handle_sqlly_help);
        #[cfg(target_arch = "wasm32")]
        {
            g!(About, handle_about);
            g!(OpenSourceLicenses, handle_open_source_licenses);
        }
        g!(
            FormatDirectivesReference,
            handle_format_directives_reference
        );
        g!(SendFeedback, handle_send_feedback);
        g!(TipOfTheDay, handle_tip_of_the_day);
        g!(CheckForUpdates, handle_check_for_updates);
        g!(UnlockSqlly, handle_unlock_sqlly);
        g!(AppStoreEdition, handle_app_store_edition);
        g!(PrivacyPolicy, handle_privacy_policy);
        g!(MinimizeWindow, handle_minimize_window);
        g!(ZoomWindow, handle_zoom_window);
        g!(ZoomEditorIn, handle_zoom_editor_in);
        g!(ZoomEditorOut, handle_zoom_editor_out);
        g!(ZoomEditorReset, handle_zoom_editor_reset);
        g!(ToggleZoomPopover, handle_toggle_zoom_popover);
        g!(CloseZoomPopover, handle_close_zoom_popover);
        g!(BringAllToFront, handle_bring_all_to_front);
        g!(Undo, handle_undo);
        g!(Redo, handle_redo);

        // Nine fixed quick-action slots (no payload-carrying gpui actions).
        // Palette and shortcuts share execute_command so capability gating
        // stays in one place.
        macro_rules! qa {
            ($action:ident, $id:expr) => {
                cx.on_action(move |_: &crate::app::$action, cx| {
                    AppView::update_active_app_view(cx, |view, win, cx| {
                        view.execute_command($id, win, cx);
                    });
                });
            };
        }
        qa!(RunQuickActionSlot1, "quickAction.slot1");
        qa!(RunQuickActionSlot2, "quickAction.slot2");
        qa!(RunQuickActionSlot3, "quickAction.slot3");
        qa!(RunQuickActionSlot4, "quickAction.slot4");
        qa!(RunQuickActionSlot5, "quickAction.slot5");
        qa!(RunQuickActionSlot6, "quickAction.slot6");
        qa!(RunQuickActionSlot7, "quickAction.slot7");
        qa!(RunQuickActionSlot8, "quickAction.slot8");
        qa!(RunQuickActionSlot9, "quickAction.slot9");
        qa!(QuickSwitchConnection1, "connection.quickSwitch1");
        qa!(QuickSwitchConnection2, "connection.quickSwitch2");
        qa!(QuickSwitchConnection3, "connection.quickSwitch3");
        qa!(QuickSwitchConnection4, "connection.quickSwitch4");
        qa!(QuickSwitchConnection5, "connection.quickSwitch5");
        qa!(QuickSwitchConnection6, "connection.quickSwitch6");
        qa!(QuickSwitchConnection7, "connection.quickSwitch7");
        qa!(QuickSwitchConnection8, "connection.quickSwitch8");
        qa!(QuickSwitchConnection9, "connection.quickSwitch9");
    }

    /// Open the Result Snapshots browser (Tools menu / palette).
    fn handle_result_snapshots(
        &mut self,
        _action: &ResultSnapshots,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        #[cfg(target_arch = "wasm32")]
        {
            let _ = window;
            self.set_status(
                StatusLevel::Info,
                crate::browser_preview::unavailable(&crate::localization::text(
                    "shell.browser.result_snapshots_unavailable",
                )),
            );
            cx.notify();
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            if self.overlays.snapshot_browser.is_some() {
                return;
            }
            let store = crate::result_snapshots::SnapshotStore::default_store();
            let view =
                cx.new(|cx| crate::result_snapshots::SnapshotBrowser::new(store, window, cx));
            cx.subscribe(&view, Self::handle_snapshot_browser_event)
                .detach();
            self.overlays.snapshot_browser = Some(view);
            cx.notify();
        }
    }

    fn select_output_shortcut(&mut self, tab: OutputPanelTab, cx: &mut Context<Self>) {
        if tab == OutputPanelTab::Diff {
            let show_diff = self
                .tabs
                .get(self.active_tab)
                .is_some_and(|t| t.diff_mode_enabled || self.toolbar.track_query_difference);
            if !show_diff {
                return;
            }
        }
        if let Some(mid) = MidRegionTab::from_output_tab(tab) {
            self.select_mid_region_tab(mid, cx);
            return;
        }
        self.show_results();
        if let Some(t) = self.tabs.get(self.active_tab) {
            t.results.update(cx, |r, cx| {
                r.select_output_tab(tab, cx);
            });
        }
        if let Some(t) = self.tabs.get_mut(self.active_tab) {
            t.output_tab = tab;
        }
        cx.notify();
    }

    fn handle_tab_strip_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.tabs.is_empty() {
            return;
        }
        let key = event.keystroke.key.as_str();
        let count = self.tabs.len();
        let idx = self.tab_strip_focus_index.min(count - 1);
        match key {
            "left" => {
                self.tab_strip_focus_index = idx.saturating_sub(1);
                cx.notify();
            }
            "right" => {
                self.tab_strip_focus_index = (idx + 1).min(count - 1);
                cx.notify();
            }
            "home" => {
                self.tab_strip_focus_index = 0;
                cx.notify();
            }
            "end" => {
                self.tab_strip_focus_index = count - 1;
                cx.notify();
            }
            "enter" | "space" => {
                self.select_tab(idx, cx);
            }
            "backspace" | "delete" => {
                self.request_close_tab(idx, window, cx);
            }
            "f2" => {
                self.begin_tab_rename(idx, cx);
            }
            "escape" => {
                self.cancel_tab_rename(cx);
            }
            _ => {}
        }
    }

    /// Drop leftover popovers/menus, then hand keyboard focus back to the
    /// workbench.
    ///
    /// Overlay dialogs (Preferences, Connection Management, …) and drawers
    /// (Explorer, AI) grab a focus handle while open. When one closes we used
    /// to clear its tracking field *first*; if focus was left pointing at the
    /// now-dropped (or now-unrendered) entity, popovers never got a blur to
    /// unregister, and the root `stack`'s element-level `on_action` handlers
    /// fell out of the focus chain — so the *next* Settings / sidebar / menu
    /// invocation silently no-ops until the user clicks the editor. Restore
    /// focus while the overlay is still mounted, then drop it.
    fn restore_workbench_focus(&mut self, window: Option<&mut Window>, cx: &mut Context<Self>) {
        self.dismiss_floating_ui(cx);
        if let Some(tab) = self.tabs.get(self.active_tab) {
            let handle = tab.editor.read(cx).focus_handle(cx);
            tab.editor.update(cx, |e, cx| {
                e.request_focus(cx);
            });
            if let Some(window) = window {
                window.focus(&handle, cx);
            }
        } else if let Some(window) = window {
            window.focus(&self.workbench_focus, cx);
        }
    }

    /// Close menus that live on surfaces we are about to hide or that would
    /// otherwise keep a deferred popover registered after their parent drops.
    fn dismiss_floating_ui(&mut self, cx: &mut Context<Self>) {
        self.all_tabs_open = false;
        self.zoom_popover_open = false;
        self.notifications_open = false;
        self.tasks_open = false;
        self.sidebar
            .update(cx, |sidebar, cx| sidebar.dismiss_floating_ui(cx));
        self.ai_panel
            .update(cx, |panel, cx| panel.dismiss_floating_ui(cx));
    }

    /// Pin/unpin a tab. Pinned tabs move to the front of the strip (after
    /// any already-pinned tabs), unpinned ones drop back behind the pinned
    /// block; the active tab is tracked by identity across the reorder.
    fn toggle_tab_pin(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get_mut(index) else {
            return;
        };
        tab.pinned = !tab.pinned;
        let active_id = self.tabs.get(self.active_tab).map(|t| t.id.clone());
        let moved = self.tabs.remove(index);
        let boundary = self.tabs.iter().take_while(|t| t.pinned).count();
        self.tabs.insert(boundary, moved);
        if let Some(active_id) = active_id
            && let Some(new_active) = self.tab_index_by_id(&active_id)
        {
            self.active_tab = new_active;
        }
        self.sync_connection_to_active_tab(cx);
        self.save_workspace_session(cx);
        cx.notify();
    }

    fn handle_editor_event(
        &mut self,
        editor: Entity<SqlEditor>,
        event: &crate::editor::SqlEditorEvent,
        cx: &mut Context<Self>,
    ) {
        let tab_idx = match self.find_tab_by_editor(&editor) {
            Some(i) => i,
            None => {
                let found = self.tabs.iter().enumerate().find_map(|(i, t)| {
                    t.notebook
                        .as_ref()
                        .and_then(|nb| nb.read(cx).cell_id_for_editor(&editor).map(|_| i))
                });
                match found {
                    Some(i) => i,
                    None => return,
                }
            }
        };
        let notebook_cell_id = self
            .tabs
            .get(tab_idx)
            .and_then(|t| t.notebook.as_ref())
            .and_then(|nb| nb.read(cx).cell_id_for_editor(&editor));
        match event {
            crate::editor::SqlEditorEvent::Execute => {
                if let Some(cell_id) = notebook_cell_id {
                    self.run_notebook_cell(tab_idx, cell_id, cx);
                } else {
                    self.execute_query(tab_idx, cx);
                }
            }
            crate::editor::SqlEditorEvent::RunStatementAt { offset } => {
                let offset = *offset;
                editor.update(cx, |ed, cx| ed.place_cursor(offset, cx));
                self.pending.run_statement_offset = Some(offset);
                self.execute_query(tab_idx, cx);
            }
            crate::editor::SqlEditorEvent::Change => {
                if let Some(tab) = self.tabs.get_mut(tab_idx) {
                    tab.dirty = true;
                    if notebook_cell_id.is_none() {
                        tab.ai_summary.mark_content_changed();
                    }
                }
                if notebook_cell_id.is_none() {
                    self.maybe_auto_name_tab(tab_idx, cx);
                    let proc_script = editor.read(cx).shared_text();
                    let proc_dialect = self.active_proc_dialect();
                    if let Some(tab) = self.tabs.get_mut(tab_idx) {
                        tab.proc_mode
                            .refresh_from_script_for(&proc_script, proc_dialect);
                    }
                    #[cfg(not(target_arch = "wasm32"))]
                    self.schedule_history_seal(tab_idx, cx);
                }
                self.schedule_session_autosave(cx);
                if notebook_cell_id.is_none() {
                    self.schedule_fim_completion(tab_idx, cx);
                    self.schedule_ai_summary(tab_idx, cx);
                }
                if self.lsp_debug.read(cx).is_visible() && tab_idx == self.active_tab {
                    self.schedule_lsp_debug_refresh(cx);
                }
                if notebook_cell_id.is_none() && self.active_profile.is_some() {
                    self.schedule_validation(tab_idx, cx);
                }
                cx.notify();
            }
            crate::editor::SqlEditorEvent::FimAccepted { text: _ } => {
                // The editor already inserted the text; just mark the tab dirty.
                if let Some(tab) = self.tabs.get_mut(tab_idx) {
                    tab.dirty = true;
                }
                cx.notify();
            }
            crate::editor::SqlEditorEvent::FimTriggerRequested => {
                self.trigger_fim_completion_manually(tab_idx, cx);
            }
            crate::editor::SqlEditorEvent::Validate => {
                self.validate_sql(tab_idx, cx);
            }
            crate::editor::SqlEditorEvent::ValueLookup { word, sql } => {
                self.run_value_lookup(tab_idx, word.clone(), sql.clone(), cx);
            }
            crate::editor::SqlEditorEvent::ValueCompletionsRequested {
                context,
                request_token,
                partial,
                fk,
            } => {
                self.run_value_completions_fetch(
                    tab_idx,
                    context.clone(),
                    *request_token,
                    partial.clone(),
                    fk.as_deref().cloned(),
                    cx,
                );
            }
            crate::editor::SqlEditorEvent::DdlNavigationRequested { offset, sql } => {
                self.start_ddl_navigation(tab_idx, sql.clone(), *offset, cx);
            }
            crate::editor::SqlEditorEvent::FeedbackRequested {
                snapshot,
                include_all_default,
                kind_defaults,
                font,
            } => {
                if let Some(message) =
                    crate::intellisense_feedback::feedback_upload().disabled_message()
                {
                    self.notify_feature_disabled(message, cx);
                    return;
                }
                self.open_intellisense_feedback_dialog(
                    (**snapshot).clone(),
                    *include_all_default,
                    *kind_defaults,
                    font.clone(),
                    cx,
                );
            }
            crate::editor::SqlEditorEvent::VimModeChanged => {
                cx.notify();
            }
            crate::editor::SqlEditorEvent::Status { level, message } => {
                self.set_status(*level, message.clone());
                cx.notify();
            }
            crate::editor::SqlEditorEvent::ZoomBy { delta } => {
                let steps =
                    crate::editor_zoom::pinch_zoom_steps(&mut self.pinch_zoom_accumulator, *delta);
                if steps != 0 {
                    let z = self.editor_zoom + steps as f32 * crate::editor_zoom::ZOOM_STEP;
                    self.apply_editor_zoom_from_gesture(z, cx);
                }
            }
        }
    }

    /// The welcome-canvas model, built fresh on every frame the canvas shows.
    /// Rows come from the sidebar's connection list (sidebar order); shortcut
    /// hints resolve through the keybinding registry, persisted overrides
    /// included, so the canvas keeps telling the truth after a rebind.
    ///
    /// Deliberately not cached on the view: connections change hands on many
    /// paths (editor save, management save/delete, the explorer's context-menu
    /// delete, Azure discovery adoption, …) and a cached copy has to be
    /// invalidated on every one of them. The rebuild is a handful of string
    /// clones — the only real work, reading `keybindings.json`, is cached
    /// inside `shortcut_hint` — and the canvas only renders while disconnected.
    fn build_welcome_model(&self, cx: &App) -> crate::welcome::WelcomeModel {
        let hints = crate::welcome::WelcomeHints {
            run: crate::keybindings::shortcut_hint("query.execute"),
            palette: crate::keybindings::shortcut_hint("tools.commandPalette"),
            sidebar: crate::keybindings::shortcut_hint("view.toggleSidebar"),
        };
        crate::welcome::welcome_model(
            self.sidebar.read(cx).connections(),
            hints,
            crate::connection::connections_are_proxied(),
        )
    }

    fn select_mid_region_tab(&mut self, tab: MidRegionTab, cx: &mut Context<Self>) {
        let active = self.active_tab;
        match mid_region_destination(tab) {
            MidRegionDestination::EditorBand => {}
            MidRegionDestination::Output(output) => {
                self.show_results();
                if output == OutputPanelTab::History {
                    #[cfg(not(target_arch = "wasm32"))]
                    {
                        let already_split = self
                            .tabs
                            .get(active)
                            .is_some_and(|t| keep_split_out_panel(t.history_split));
                        if !already_split {
                            let _ = self.ensure_history_panel(active, cx);
                        }
                    }
                }
                if let Some(results) = self.tabs.get(active).map(|t| t.results.clone()) {
                    results.update(cx, |rv, cx| {
                        rv.select_output_tab(output, cx);
                    });
                }
                if let Some(t) = self.tabs.get_mut(active) {
                    t.output_tab = output;
                }
            }
            MidRegionDestination::AiDrawer => {
                self.ai_visible = true;
            }
        }
        if let Some(t) = self.tabs.get_mut(active) {
            t.mid_region_tab = MidRegionTab::Editor;
        }
        cx.notify();
    }

    /// When the Entra accounts dialog emits `Changed` (user added or removed
    /// an account), tell every connection picker to re-read the registry so
    /// the new account list reaches the EntraBrowser binding dropdown
    /// without the user reopening the dialog. `Dismissed` drops the dialog
    /// from the AppView's owned-entity list so the next invocation rebuilds
    /// it from the registry's up-to-date contents.
    /// Routes [`crate::notebook_view::NotebookEvent`]s from a notebook tab.
    fn handle_notebook_event(
        &mut self,
        notebook: Entity<crate::notebook_view::NotebookView>,
        event: &crate::notebook_view::NotebookEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            crate::notebook_view::NotebookEvent::NeedCellEntities { cell_ids } => {
                let mut pairs = Vec::new();
                for cell_id in cell_ids {
                    let source = notebook
                        .read(cx)
                        .model()
                        .cell(cell_id)
                        .map(|c| c.source.clone());
                    let editor = self.make_cell_editor(source.as_deref(), cx);
                    let results = self.make_cell_results(&editor, cx);
                    cx.subscribe(&editor, Self::handle_editor_event).detach();
                    cx.subscribe(&results, Self::handle_results_event).detach();
                    pairs.push((cell_id.clone(), editor, results));
                }
                notebook.update(cx, |nb, cx| nb.attach_cell_entities(pairs, cx));
            }
            crate::notebook_view::NotebookEvent::RunCell { cell_id } => {
                if let Some(tab_idx) = self
                    .tabs
                    .iter()
                    .position(|t| t.notebook.as_ref() == Some(&notebook))
                {
                    self.run_notebook_cell(tab_idx, cell_id.clone(), cx);
                }
            }
            crate::notebook_view::NotebookEvent::Dirty => {
                if let Some(tab) = self
                    .tabs
                    .iter_mut()
                    .find(|t| t.notebook.as_ref() == Some(&notebook))
                {
                    tab.dirty = true;
                }
                self.schedule_session_autosave(cx);
                cx.notify();
            }
            crate::notebook_view::NotebookEvent::SaveRequested => {
                if let Some(tab_idx) = self
                    .tabs
                    .iter()
                    .position(|t| t.notebook.as_ref() == Some(&notebook))
                {
                    self.save_notebook(tab_idx, false, cx);
                }
            }
            crate::notebook_view::NotebookEvent::SaveAsRequested => {
                if let Some(tab_idx) = self
                    .tabs
                    .iter()
                    .position(|t| t.notebook.as_ref() == Some(&notebook))
                {
                    self.save_notebook(tab_idx, true, cx);
                }
            }
            crate::notebook_view::NotebookEvent::ExportMarkdownRequested => {
                if let Some(tab_idx) = self
                    .tabs
                    .iter()
                    .position(|t| t.notebook.as_ref() == Some(&notebook))
                {
                    self.export_notebook_markdown(tab_idx, cx);
                }
            }
            crate::notebook_view::NotebookEvent::ExportJupyterRequested => {
                if let Some(tab_idx) = self
                    .tabs
                    .iter()
                    .position(|t| t.notebook.as_ref() == Some(&notebook))
                {
                    self.export_notebook_ipynb(tab_idx, cx);
                }
            }
            crate::notebook_view::NotebookEvent::Status { message, error } => {
                self.set_status(
                    if *error {
                        StatusLevel::Error
                    } else {
                        StatusLevel::Info
                    },
                    message.clone(),
                );
                cx.notify();
            }
        }
    }

    /// Routes [`crate::results::ResultsViewEvent`]s from a tab's results view.
    /// Today only row-edit requests are routed; query lifecycle events are
    /// consumed elsewhere.
    fn handle_results_event(
        &mut self,
        results: Entity<ResultsView>,
        event: &crate::results::ResultsViewEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            crate::results::ResultsViewEvent::EditRowRequested {
                result_set_index,
                columns,
                row,
                sql,
                intent,
            } => {
                self.open_row_editor_dialog(
                    results,
                    *result_set_index,
                    columns.clone(),
                    row.clone(),
                    sql.clone(),
                    *intent,
                    crate::row_editor::RowEditorMode::Commit,
                    cx,
                );
            }
            crate::results::ResultsViewEvent::StageRequested {
                result_set_index,
                columns,
                row,
                sql,
                intent,
            } => {
                if *intent == crate::row_editor::RowEditorIntent::BatchInsert {
                    #[cfg(not(target_arch = "wasm32"))]
                    self.open_batch_insert_dialog(
                        results,
                        *result_set_index,
                        columns.clone(),
                        row.clone(),
                        sql.clone(),
                        cx,
                    );
                    #[cfg(target_arch = "wasm32")]
                    {
                        let _ = (results, result_set_index, columns, row, sql);
                        self.set_status(
                            StatusLevel::Info,
                            crate::localization::text("shell.browser.batch_insert_unavailable"),
                        );
                        cx.notify();
                    }
                } else {
                    self.open_row_editor_dialog(
                        results,
                        *result_set_index,
                        columns.clone(),
                        row.clone(),
                        sql.clone(),
                        *intent,
                        crate::row_editor::RowEditorMode::Stage,
                        cx,
                    );
                }
            }
            crate::results::ResultsViewEvent::CellActionRequested {
                result_set_index,
                columns,
                row,
                sql,
                column_name,
                result_column,
                action,
            } => {
                self.stage_cell_action(
                    results,
                    *result_set_index,
                    columns.clone(),
                    row.clone(),
                    sql.clone(),
                    (column_name.clone(), *result_column),
                    action.clone(),
                    cx,
                );
            }
            crate::results::ResultsViewEvent::PasteRequested {
                result_set_index,
                columns,
                anchor_row,
                sql,
                text,
                anchor,
                overwrite,
                fill_range,
            } => {
                self.handle_paste_requested(
                    results,
                    *result_set_index,
                    columns.clone(),
                    anchor_row.clone(),
                    sql.clone(),
                    text.clone(),
                    *anchor,
                    *overwrite,
                    *fill_range,
                    cx,
                );
            }
            crate::results::ResultsViewEvent::SaveStagedRequested {
                batch,
                rollback_sql,
                preview_sql,
            } => {
                self.handle_save_staged_requested(
                    results,
                    batch.clone(),
                    rollback_sql.clone(),
                    preview_sql.clone(),
                    cx,
                );
            }
            crate::results::ResultsViewEvent::FollowForeignKeyRequested {
                result_set_index,
                columns,
                row,
                column_index,
                sql,
            } => {
                self.follow_foreign_key(
                    *result_set_index,
                    columns.clone(),
                    row.clone(),
                    *column_index,
                    sql.clone(),
                    cx,
                );
            }
            crate::results::ResultsViewEvent::CopyUpdateRequested {
                result_set_index,
                columns,
                rows,
                sample_row,
                sql,
            } => {
                self.copy_sql_update(
                    *result_set_index,
                    columns.clone(),
                    rows.clone(),
                    sample_row.clone(),
                    sql.clone(),
                    cx,
                );
            }
            crate::results::ResultsViewEvent::StageDeleteManyRequested {
                result_set_index,
                columns,
                rows,
                sample_row,
                sql,
            } => {
                self.stage_delete_many(
                    results,
                    *result_set_index,
                    columns.clone(),
                    rows.clone(),
                    sample_row.clone(),
                    sql.clone(),
                    cx,
                );
            }
            crate::results::ResultsViewEvent::HydrateCellRequested {
                result_set_index,
                columns,
                row,
                sql,
                column_index,
                column_name,
                truncated,
                purpose,
            } => {
                self.hydrate_cell(
                    results,
                    *result_set_index,
                    columns.clone(),
                    row.clone(),
                    sql.clone(),
                    *column_index,
                    column_name.clone(),
                    *truncated,
                    *purpose,
                    cx,
                );
            }
            crate::results::ResultsViewEvent::QueryCompleted => {
                #[cfg(not(target_arch = "wasm32"))]
                {
                    self.attach_run_results(&results, cx);
                    self.cache_run_results(&results, cx);
                }
                // `-- sqlly map: open=auto` / `-- sqlly view: map`.
                let mapped = results.clone();
                cx.defer(move |cx| crate::map_view::auto_open(&mapped, cx));
                if let Some(tab_idx) = self.tabs.iter().position(|tab| tab.results == results) {
                    if let Some(tab) = self.tabs.get_mut(tab_idx) {
                        tab.last_run_completed_at = Some(Instant::now());
                    }
                    self.refresh_result_source(tab_idx, crate::editor::RunOutcome::Succeeded, cx);
                }
            }
            crate::results::ResultsViewEvent::ExecutionPlanCaptureRequested(kind) => {
                self.explain_query_kind(*kind, cx);
            }
            crate::results::ResultsViewEvent::ExecutionPlanOpenRequested => {
                self.open_plan_file(cx);
            }
            crate::results::ResultsViewEvent::ExecutionPlanPasteRequested => {
                self.paste_plan(cx);
            }
            crate::results::ResultsViewEvent::ExecutionPlanOpenSqlInNewTab(sql) => {
                self.new_tab(cx, Some(sql.clone()));
            }
            crate::results::ResultsViewEvent::LayoutModeChanged(mode) => {
                self.toolbar.result_layout_stacked = matches!(mode, LayoutMode::Stacked);
                let layout_str = if self.toolbar.result_layout_stacked {
                    "stacked"
                } else {
                    "tabular"
                };
                if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                    tab.per_tab_result_layout = Some(layout_str.to_string());
                    if tab.resume_effective_enabled {
                        self.flush_tab_resume_metadata(self.active_tab);
                    }
                }
                cx.notify();
            }
            crate::results::ResultsViewEvent::OutputTabSelected(tab) => {
                let active = self.active_tab;
                if let Some(t) = self.tabs.get_mut(active) {
                    t.output_tab = *tab;
                    t.mid_region_tab = MidRegionTab::Editor;
                }
                if matches!(
                    tab,
                    OutputPanelTab::History | OutputPanelTab::Ddl | OutputPanelTab::Plan
                ) {
                    self.show_results();
                }
                if *tab == OutputPanelTab::History {
                    #[cfg(not(target_arch = "wasm32"))]
                    {
                        let already_split = self
                            .tabs
                            .get(active)
                            .is_some_and(|t| keep_split_out_panel(t.history_split));
                        if !already_split {
                            let _ = self.ensure_history_panel(active, cx);
                        }
                    }
                }
                cx.notify();
            }
            crate::results::ResultsViewEvent::AutoRefreshIntervalChanged(secs) => {
                if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.results == results) {
                    tab.auto_refresh_secs = *secs;
                }
                self.ensure_auto_refresh_ticker(cx);
            }
            crate::results::ResultsViewEvent::TotalRowCountRequested {
                result_set_index,
                sql,
            } => {
                if let Some(tab_idx) = self.tabs.iter().position(|tab| tab.results == results) {
                    self.spawn_total_count(tab_idx, *result_set_index, sql.clone(), cx);
                }
            }
            crate::results::ResultsViewEvent::TotalRowCountCancelRequested => {
                if let Some(tab_idx) = self.tabs.iter().position(|tab| tab.results == results) {
                    self.cancel_total_count(tab_idx, cx);
                }
            }
            crate::results::ResultsViewEvent::QueryFailed => {
                if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.results == results) {
                    tab.auto_refresh_secs = None;
                }
                results.update(cx, |view, cx| view.set_auto_refresh_secs(None, cx));
                if let Some(tab_idx) = self.tabs.iter().position(|tab| tab.results == results) {
                    self.refresh_result_source(tab_idx, crate::editor::RunOutcome::Failed, cx);
                }
            }
            crate::results::ResultsViewEvent::ResultSettingsRequested(change) => {
                self.apply_result_settings_change(&results, change, cx);
            }
            crate::results::ResultsViewEvent::LockRequested(snapshot) => {
                self.apply_result_lock(&results, snapshot, cx);
            }
            crate::results::ResultsViewEvent::ComputedColumnWritten { line, replacing } => {
                self.write_computed_column(&results, line, replacing.as_deref(), cx);
            }
            crate::results::ResultsViewEvent::GoToEditorOffset(offset) => {
                if let Some(tab) = self.tabs.iter().find(|t| t.results == results) {
                    let editor = tab.editor.clone();
                    editor.update(cx, |e, cx| e.go_to_offset(*offset, cx));
                    editor.update(cx, |e, cx| e.request_focus(cx));
                }
            }
            crate::results::ResultsViewEvent::ActiveResultChanged => {
                if let Some(tab_idx) = self.tabs.iter().position(|tab| tab.results == results) {
                    let outcome = self
                        .tabs
                        .get(tab_idx)
                        .and_then(|tab| tab.editor.read(cx).result_source().map(|s| s.outcome));
                    if let Some(outcome) = outcome {
                        self.refresh_result_source(tab_idx, outcome, cx);
                    }
                }
            }
            #[cfg(not(target_arch = "wasm32"))]
            crate::results::ResultsViewEvent::ExportToFileRequested { format } => {
                self.prompt_and_export(*format, results, cx);
            }
            crate::results::ResultsViewEvent::OpenMaskingSettings => {
                self.pending_open_masking_settings = true;
                cx.notify();
            }
            crate::results::ResultsViewEvent::MaskingOverrideRequested {
                column_name,
                include,
            } => {
                let Some(mut profile) = self.active_profile.clone() else {
                    return;
                };
                let rule = sqlly_core::MaskRule {
                    pattern: column_name.clone(),
                    scope: sqlly_core::RuleScope::ColumnName,
                };
                if *include {
                    profile
                        .masking
                        .exclude
                        .retain(|existing| existing.pattern != *column_name);
                    if !profile
                        .masking
                        .include
                        .iter()
                        .any(|existing| existing.pattern == *column_name)
                    {
                        profile.masking.include.push(rule);
                    }
                } else {
                    profile
                        .masking
                        .include
                        .retain(|existing| existing.pattern != *column_name);
                    if !profile
                        .masking
                        .exclude
                        .iter()
                        .any(|existing| existing.pattern == *column_name)
                    {
                        profile.masking.exclude.push(rule);
                    }
                }
                crate::connection::ConnectionStore::global().update(profile.id.0, profile.clone());
                self.active_profile = Some(profile.clone());
                results.update(cx, |view, cx| {
                    view.set_masking_overrides(profile.masking.clone());
                    view.rebuild_all_grids_from_source(cx);
                });
            }
            crate::results::ResultsViewEvent::AdvanceQuery { .. } => {}
            // The tab strip's unsaved dot counts staged grid edits.
            crate::results::ResultsViewEvent::StagedEditsChanged => cx.notify(),
            // Grid cell menu "Cell Details": open the panel (the grid
            // already selected the clicked cell, so it shows that cell).
            crate::results::ResultsViewEvent::CellDetailOpenRequested => {
                if !self.cell_detail_visible {
                    self.cell_detail_visible = true;
                    self.push_cell_detail_to_tabs(cx);
                    self.save_workspace_session(cx);
                    cx.notify();
                }
            }
            crate::results::ResultsViewEvent::CellDetailClosed => {
                self.cell_detail_visible = false;
                self.push_cell_detail_to_tabs(cx);
                self.save_workspace_session(cx);
                cx.notify();
            }
            crate::results::ResultsViewEvent::CellDetailSideToggled => {
                let mut rf = crate::preferences::ResultFormattingPreferences::load();
                rf.cell_detail_side = match rf.cell_detail_side {
                    crate::preferences::CellDetailSide::Right => {
                        crate::preferences::CellDetailSide::Bottom
                    }
                    crate::preferences::CellDetailSide::Bottom => {
                        crate::preferences::CellDetailSide::Right
                    }
                };
                crate::preferences::PreferencesStore::new().set_result_formatting(rf.clone());
                self.formatting = rf;
                self.apply_cell_detail_layout(cx);
                cx.notify();
            }
            crate::results::ResultsViewEvent::CellDetailDragStarted { pos } => {
                self.is_resizing_cell_detail = true;
                self.drag_start_pos = px(*pos);
                self.drag_start_cell_detail_size = self.cell_detail_size_for_side();
                cx.notify();
            }
            crate::results::ResultsViewEvent::MaximizeToggleRequested => {
                self.results_pane = self.results_pane.toggled_maximized();
                self.save_workspace_session(cx);
                cx.notify();
            }
            crate::results::ResultsViewEvent::DetachRequested => {
                self.detach_results(cx);
            }
            crate::results::ResultsViewEvent::SaveChartToLibraryRequested { spec, sql } => {
                self.open_save_chart_prompt(spec.clone(), sql.clone(), cx);
            }
        }
    }

    /// Write a result-name menu / Result Settings request into the query
    /// that produced the results: `-- sqlly` directives and/or the producing
    /// statement commented out, as ONE undoable edit of the tab's editor.
    fn apply_result_settings_change(
        &mut self,
        results: &Entity<ResultsView>,
        change: &crate::result_header_actions::ResultSettingsChange,
        cx: &mut Context<Self>,
    ) {
        let Some(tab_idx) = self.tabs.iter().position(|tab| &tab.results == results) else {
            return;
        };
        let tab = &self.tabs[tab_idx];
        if tab.snapshot_only {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.results.snapshot_read_only"),
            );
            cx.notify();
            return;
        }
        let editor = tab.editor.clone();
        let buffer = editor.read(cx).text().to_string();
        let dialect = editor.read(cx).database_type().family();
        let (run_sql, result_count) = {
            let view = results.read(cx);
            (
                view.query_sql().map(str::to_string),
                view.result_set_count(),
            )
        };
        let ctx = crate::result_header_actions::RunContext {
            span: tab.last_executed_span,
            run_sql: run_sql.as_deref(),
            result_count,
            dialect,
        };
        let applied =
            match crate::result_header_actions::apply_result_settings(&buffer, &ctx, change) {
                Ok(applied) => applied,
                Err(message) => {
                    self.set_status(StatusLevel::Error, message);
                    cx.notify();
                    return;
                }
            };
        if !applied.changed {
            self.set_status(
                StatusLevel::Info,
                crate::localization::format(
                    "shell.results.settings_already_applied",
                    &[("result_set", &(change.slot + 1).to_string())],
                ),
            );
            cx.notify();
            return;
        }
        // `replace_document` makes the edit ONE undoable step and emits no
        // Change event, so drive what a typed edit would: proc-mode rescan,
        // dirty flag, session autosave. The executed span is carried across
        // the edit so the result-source highlight and later menu actions
        // still find the run.
        let proc_dialect = sqlly_services::proc_dialect(editor.read(cx).database_type());
        if let Some(tab) = self.tabs.get_mut(tab_idx) {
            tab.proc_mode
                .refresh_from_script_for(&applied.text, proc_dialect);
            tab.last_executed_span = applied.span;
            tab.dirty = true;
        }
        editor.update(cx, |e, cx| e.replace_document(applied.text, cx));
        self.schedule_session_autosave(cx);
        if let Some(outcome) = editor.read(cx).result_source().map(|s| s.outcome) {
            self.refresh_result_source(tab_idx, outcome, cx);
        }
        // The query now says it; let the results view show it without a re-run.
        results.update(cx, |view, cx| view.confirm_result_settings(change, cx));
        let n = change.slot + 1;
        let message = if change.comment_out {
            crate::localization::format(
                "shell.results.statement_commented_out",
                &[("result_set", &n.to_string())],
            )
        } else {
            crate::localization::format(
                "shell.results.settings_updated",
                &[("result_set", &n.to_string())],
            )
        };
        self.set_status(StatusLevel::Success, message);
        cx.notify();
    }

    /// Write a result lock into the query that produced the results: one
    /// `/* sqlly lock` block reproducing every result as it looks now,
    /// replacing the previous block and the directives it now owns, as ONE
    /// undoable edit of the tab's editor.
    fn apply_result_lock(
        &mut self,
        results: &Entity<ResultsView>,
        snapshot: &crate::results::LockSnapshot,
        cx: &mut Context<Self>,
    ) {
        let Some(tab_idx) = self.tabs.iter().position(|tab| &tab.results == results) else {
            return;
        };
        let tab = &self.tabs[tab_idx];
        if tab.snapshot_only {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.results.lock_snapshot_read_only"),
            );
            cx.notify();
            return;
        }
        let editor = tab.editor.clone();
        let buffer = editor.read(cx).text().to_string();
        let region = tab.last_executed_span.unwrap_or((0, buffer.len()));
        let applied = crate::result_lock::apply_lock(&buffer, region, &snapshot.lines);
        for note in &snapshot.notes {
            results.update(cx, |view, cx| view.push_message(note.clone(), cx));
        }
        if !applied.changed {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.results.lock_already_matches"),
            );
            cx.notify();
            return;
        }
        let proc_dialect = sqlly_services::proc_dialect(editor.read(cx).database_type());
        if let Some(tab) = self.tabs.get_mut(tab_idx) {
            tab.proc_mode
                .refresh_from_script_for(&applied.text, proc_dialect);
            tab.last_executed_span = Some(applied.span);
            tab.dirty = true;
        }
        editor.update(cx, |e, cx| e.replace_document(applied.text, cx));
        self.schedule_session_autosave(cx);
        if let Some(outcome) = editor.read(cx).result_source().map(|s| s.outcome) {
            self.refresh_result_source(tab_idx, outcome, cx);
        }
        results.update(cx, |view, cx| view.note_locked(cx));
        let message = if snapshot.notes.is_empty() {
            crate::localization::text("shell.results.locked")
        } else {
            crate::localization::text("shell.results.locked_with_notes")
        };
        self.set_status(StatusLevel::Success, message);
        cx.notify();
    }

    /// Write a formula-bar computed column into the query: replace the
    /// `-- sqlly column:` line it edits (wherever it is, lock block
    /// included), or add the line at the top of the code that ran.
    fn write_computed_column(
        &mut self,
        results: &Entity<ResultsView>,
        line: &str,
        replacing: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        let Some(tab_idx) = self.tabs.iter().position(|tab| &tab.results == results) else {
            return;
        };
        if self.tabs[tab_idx].snapshot_only {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.results.computed_column_snapshot"),
            );
            cx.notify();
            return;
        }
        let editor = self.tabs[tab_idx].editor.clone();
        let buffer = editor.read(cx).text().to_string();
        let region = self.tabs[tab_idx]
            .last_executed_span
            .unwrap_or((0, buffer.len()));
        let (text, span) = crate::computed_columns::upsert_line(&buffer, region, line, replacing);
        if text == buffer {
            return;
        }
        let proc_dialect = sqlly_services::proc_dialect(editor.read(cx).database_type());
        if let Some(tab) = self.tabs.get_mut(tab_idx) {
            tab.proc_mode.refresh_from_script_for(&text, proc_dialect);
            tab.last_executed_span = Some(span);
            tab.dirty = true;
        }
        editor.update(cx, |e, cx| e.replace_document(text, cx));
        self.schedule_session_autosave(cx);
        self.set_status(
            StatusLevel::Success,
            crate::localization::text("shell.results.computed_column_written"),
        );
        cx.notify();
    }

    fn refresh_result_source(
        &mut self,
        tab_idx: usize,
        outcome: crate::editor::RunOutcome,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.get(tab_idx) else {
            return;
        };
        let Some(span) = tab.last_executed_span else {
            return;
        };
        let buffer = tab.editor.read(cx).text().to_string();
        let dialect = tab.editor.read(cx).database_type().family();
        let result_count = tab.results.read(cx).result_set_count();
        let active = tab.results.read(cx).active_result_index();
        let narrowed = crate::editor::statements::narrow_result_source(
            span,
            &buffer,
            result_count,
            active,
            dialect,
        );
        let editor = tab.editor.clone();
        editor.update(cx, |ed, cx| {
            ed.set_result_source(
                Some(crate::editor::ResultSource {
                    span: narrowed,
                    outcome,
                }),
                cx,
            );
        });
    }

    /// Opens the row-editor modal for a clicked result row. The dialog
    /// resolves editability asynchronously; the app only supplies the
    /// connection (with a fresh Entra token when needed) and `Services`.
    #[allow(clippy::too_many_arguments)]
    fn hydrate_cell(
        &mut self,
        results: Entity<ResultsView>,
        result_set_index: usize,
        columns: Vec<sqlly_core::ResultColumn>,
        row: Vec<sqlly_core::CellValue>,
        sql: String,
        column_index: usize,
        column_name: String,
        truncated: sqlly_services::TruncatedCell,
        purpose: crate::cell_hydration::HydratePurpose,
        cx: &mut Context<Self>,
    ) {
        let limits = sqlly_services::CellLimits::default();
        let preview_text = row
            .get(column_index)
            .map(crate::result_export::cell_display)
            .unwrap_or_default();
        if let Some(msg) =
            crate::cell_hydration::hydration_blocker(&truncated, &["id".into()], &limits)
        {
            if purpose == crate::cell_hydration::HydratePurpose::Viewer {
                crate::structured_data_viewer::open_structured_data_viewer(
                    &column_name,
                    &crate::cell_hydration::unavailable_banner(&truncated, &preview_text, &limits),
                    cx,
                );
            }
            if purpose == crate::cell_hydration::HydratePurpose::Dock {
                let banner =
                    crate::cell_hydration::unavailable_banner(&truncated, &preview_text, &limits);
                results.update(cx, |rv, cx| {
                    rv.apply_cell_hydration_unavailable(banner, cx);
                });
            }
            self.set_status(StatusLevel::Info, msg);
            cx.notify();
            return;
        }
        let Some(profile) = self.active_profile.clone() else {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.cells.connect_to_view"),
            );
            cx.notify();
            return;
        };
        if let Some(reason) = crate::capabilities::row_editing(profile.database_type).reason() {
            self.set_status(StatusLevel::Info, reason.to_string());
            cx.notify();
            return;
        }
        self.with_entra_sql_token(profile, cx, move |app, mut profile, cx| {
            let store = crate::connection::ConnectionStore::global();
            let now = Instant::now();
            crate::mutation_review::apply_inherited_execution_policy(
                &mut profile,
                store,
                app.production_unlock.as_ref(),
                now,
            );
            let request = sqlly_services::row_editing::RowEditOpenRequest {
                sql,
                result_set_index,
                columns,
                row,
            };
            let services = app.services_for_profile(&profile);
            let (tx, mut rx) = tokio::sync::mpsc::channel::<
                Result<sqlly_services::row_editing::RowEditOpenOutcome, sqlly_core::DisplayMessage>,
            >(1);
            cx.background_spawn(async move {
                let outcome = match crate::tokio_handle()
                    .spawn(
                        async move { services.row_editing.open_session(&profile, request).await },
                    )
                    .await
                {
                    Ok(outcome) => outcome.map_err(|e| e.message()),
                    Err(join_err) => Err(sqlly_core::DisplayMessage::raw(join_err.to_string())),
                };
                let _ = tx.send(outcome).await;
            })
            .detach();
            let weak_self = cx.weak_entity();
            let async_cx = cx.to_async();
            cx.spawn(async move |_this, _cx| {
                if let Some(outcome) = rx.recv().await {
                    let mut cx = async_cx.clone();
                    let _ = weak_self.update(&mut cx, |this, cx| match outcome {
                        Ok(sqlly_services::row_editing::RowEditOpenOutcome::Editable(session)) => {
                            let col_idx = session
                                .columns
                                .iter()
                                .position(|c| c.name == column_name)
                                .unwrap_or(column_index);
                            let value = session.original_row.get(col_idx).cloned();
                            let Some(value) = value else {
                                this.set_status(
                                    StatusLevel::Info,
                                    crate::localization::text("shell.cells.read_failed"),
                                );
                                cx.notify();
                                return;
                            };
                            if crate::cell_hydration::cell_byte_len(&value) > limits.hard_cap_bytes
                            {
                                let msg = crate::cell_hydration::hydration_blocker(
                                    &truncated,
                                    &session.key_columns,
                                    &limits,
                                )
                                .unwrap_or_else(|| {
                                    crate::localization::format(
                                        "shell.cells.viewer_limit_exceeded",
                                        &[(
                                            "limit",
                                            &crate::cell_hydration::human_size(
                                                limits.hard_cap_bytes,
                                            ),
                                        )],
                                    )
                                });
                                if purpose == crate::cell_hydration::HydratePurpose::Viewer {
                                    crate::structured_data_viewer::open_structured_data_viewer(
                                        &column_name,
                                        &crate::cell_hydration::unavailable_banner(
                                            &truncated,
                                            &preview_text,
                                            &limits,
                                        ),
                                        cx,
                                    );
                                }
                                if purpose == crate::cell_hydration::HydratePurpose::Dock {
                                    let banner = crate::cell_hydration::unavailable_banner(
                                        &truncated,
                                        &preview_text,
                                        &limits,
                                    );
                                    results.update(cx, |rv, cx| {
                                        rv.apply_cell_hydration_unavailable(banner, cx);
                                    });
                                }
                                this.set_status(StatusLevel::Info, msg);
                                cx.notify();
                                return;
                            }
                            match purpose {
                                crate::cell_hydration::HydratePurpose::Viewer => {
                                    crate::structured_data_viewer::open_structured_data_viewer(
                                        &column_name,
                                        &crate::result_export::cell_display(&value),
                                        cx,
                                    );
                                }
                                crate::cell_hydration::HydratePurpose::Hex => {
                                    let bytes = match &value {
                                        sqlly_core::CellValue::Bytes(b) => b.clone(),
                                        sqlly_core::CellValue::String(s) => s.as_bytes().to_vec(),
                                        other => {
                                            crate::result_export::cell_display(other).into_bytes()
                                        }
                                    };
                                    crate::structured_data_viewer::open_binary_viewer(
                                        &column_name,
                                        &bytes,
                                        cx,
                                    );
                                }
                                crate::cell_hydration::HydratePurpose::Copy => {
                                    let text = crate::result_export::cell_display(&value);
                                    let size = crate::cell_hydration::human_size(
                                        crate::cell_hydration::cell_byte_len(&value),
                                    );
                                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
                                    this.set_status(
                                        StatusLevel::Info,
                                        crate::localization::format(
                                            "shell.cells.copied_full_value",
                                            &[("size", &size)],
                                        ),
                                    );
                                }
                                crate::cell_hydration::HydratePurpose::Dock => {
                                    results.update(cx, |rv, cx| {
                                        rv.apply_hydrated_cell_value(value, cx);
                                    });
                                }
                            }
                            cx.notify();
                        }
                        Ok(sqlly_services::row_editing::RowEditOpenOutcome::NotEditable {
                            reason,
                            message,
                        }) => {
                            let reason =
                                crate::localization::stored_message_or(message.as_ref(), reason);
                            let mut msg =
                                crate::cell_hydration::hydration_blocker(&truncated, &[], &limits)
                                    .unwrap_or_default();
                            if !reason.is_empty() {
                                if msg.is_empty() {
                                    msg = reason;
                                } else {
                                    msg = format!("{msg} — {reason}");
                                }
                            }
                            if purpose == crate::cell_hydration::HydratePurpose::Viewer {
                                crate::structured_data_viewer::open_structured_data_viewer(
                                    &column_name,
                                    &crate::cell_hydration::unavailable_banner(
                                        &truncated,
                                        &preview_text,
                                        &limits,
                                    ),
                                    cx,
                                );
                            }
                            if purpose == crate::cell_hydration::HydratePurpose::Dock {
                                let banner = crate::cell_hydration::unavailable_banner(
                                    &truncated,
                                    &preview_text,
                                    &limits,
                                );
                                results.update(cx, |rv, cx| {
                                    rv.apply_cell_hydration_unavailable(banner, cx);
                                });
                            }
                            this.set_status(StatusLevel::Info, msg);
                            cx.notify();
                        }
                        Err(err) => {
                            if purpose == crate::cell_hydration::HydratePurpose::Viewer {
                                crate::structured_data_viewer::open_structured_data_viewer(
                                    &column_name,
                                    &crate::cell_hydration::unavailable_banner(
                                        &truncated,
                                        &preview_text,
                                        &limits,
                                    ),
                                    cx,
                                );
                            }
                            if purpose == crate::cell_hydration::HydratePurpose::Dock {
                                let banner = crate::cell_hydration::unavailable_banner(
                                    &truncated,
                                    &preview_text,
                                    &limits,
                                );
                                results.update(cx, |rv, cx| {
                                    rv.apply_cell_hydration_unavailable(banner, cx);
                                });
                            }
                            this.set_status(StatusLevel::Info, crate::localization::message(err));
                            cx.notify();
                        }
                    });
                }
            })
            .detach();
        });
    }

    /// The connection-level row-edit verdict for the active connection, as
    /// the row editor would see it when opened now: the active profile with
    /// the inherited execution policy applied (exactly like
    /// [`Self::open_row_editor_dialog`]), run through
    /// [`crate::row_edit_gate::effective_connection_block_reason`]. Handed to
    /// a results view before a run so its grid hides the pencil when editing
    /// would be refused.
    pub(crate) fn row_edit_connection_block(&self) -> Option<String> {
        let mut profile = self.active_profile.clone();
        if let Some(profile) = profile.as_mut() {
            crate::mutation_review::apply_inherited_execution_policy(
                profile,
                crate::connection::ConnectionStore::global(),
                self.production_unlock.as_ref(),
                Instant::now(),
            );
        }
        crate::row_edit_gate::effective_connection_block_reason(profile.as_ref())
    }

    #[allow(clippy::too_many_arguments)]
    fn open_row_editor_dialog(
        &mut self,
        results: Entity<ResultsView>,
        result_set_index: usize,
        columns: Vec<sqlly_core::ResultColumn>,
        row: Vec<sqlly_core::CellValue>,
        sql: String,
        intent: crate::row_editor::RowEditorIntent,
        mode: crate::row_editor::RowEditorMode,
        cx: &mut Context<Self>,
    ) {
        if self.overlays.row_editor.is_some() {
            return;
        }
        // Connection + engine gate, shared with the results grid's pencil
        // (`row_edit_gate`): fail here with the reason instead of letting the
        // dialog's async open fail with a protocol error.
        if let Some(reason) =
            crate::row_edit_gate::connection_block_reason(self.active_profile.as_ref())
        {
            self.set_status(StatusLevel::Info, reason);
            cx.notify();
            return;
        }
        let Some(profile) = self.active_profile.clone() else {
            return;
        };
        self.with_entra_sql_token(profile, cx, move |app, mut profile, cx| {
            let store = crate::connection::ConnectionStore::global();
            let now = Instant::now();
            crate::mutation_review::apply_inherited_execution_policy(
                &mut profile,
                store,
                app.production_unlock.as_ref(),
                now,
            );
            let request = sqlly_services::row_editing::RowEditOpenRequest {
                sql,
                result_set_index,
                columns,
                row,
            };
            // Route by the profile: a Via-Relay connection's edits travel
            // through the secure relay session to the bridge, which runs the
            // same row-editing service with the same guards as a direct edit.
            let services = app.services_for_profile(&profile);
            let app_view = cx.entity();
            // RowEditor::open_with needs a live `Window` (its per-field inputs
            // are gpui-component InputStates), but this callback can run from
            // an async token-fetch continuation with no window in scope. Defer
            // one tick and re-enter through the active window.
            cx.defer(move |app| {
                let Some(win) = app.active_window() else {
                    return;
                };
                let _ = win.update(app, |_root, window, app| {
                    app_view.update(app, |this, cx| {
                        if this.overlays.row_editor.is_some() {
                            return;
                        }
                        let dialog = cx.new(|cx| {
                            crate::row_editor::RowEditor::open_with(
                                services, profile, request, intent, mode, window, cx,
                            )
                        });
                        cx.subscribe(&dialog, Self::handle_row_editor_event)
                            .detach();
                        this.overlays.row_editor = Some((dialog, results));
                        cx.notify();
                    });
                });
            });
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn stage_cell_action(
        &mut self,
        results: Entity<ResultsView>,
        result_set_index: usize,
        columns: Vec<sqlly_core::ResultColumn>,
        row: Vec<sqlly_core::CellValue>,
        sql: String,
        (column_name, result_column): (String, usize),
        action: crate::cell_actions::CellAction,
        cx: &mut Context<Self>,
    ) {
        if let Some(reason) =
            crate::row_edit_gate::connection_block_reason(self.active_profile.as_ref())
        {
            self.set_status(StatusLevel::Info, reason);
            cx.notify();
            return;
        }
        let Some(profile) = self.active_profile.clone() else {
            return;
        };
        self.with_entra_sql_token(profile, cx, move |app, mut profile, cx| {
            let store = crate::connection::ConnectionStore::global();
            let now = Instant::now();
            crate::mutation_review::apply_inherited_execution_policy(
                &mut profile,
                store,
                app.production_unlock.as_ref(),
                now,
            );
            let request = sqlly_services::row_editing::RowEditOpenRequest {
                sql,
                result_set_index,
                columns: columns.clone(),
                row: row.clone(),
            };
            let services = app.services_for_profile(&profile);
            let db = profile.database_type;
            // Relay route whose bridge has not advertised the
            // server-evaluated generators (an older bridge rejects them).
            let generators_blocked =
                crate::cell_actions::server_expressions_blocked(&profile, &services);
            // The row and column index belong to this run's result; a re-run
            // while the session opens must not stage them against new rows.
            let run_generation = results.read(cx).run_generation();
            let (tx, mut rx) = tokio::sync::mpsc::channel::<
                Result<sqlly_services::row_editing::RowEditOpenOutcome, sqlly_core::DisplayMessage>,
            >(1);
            cx.background_spawn(async move {
                let outcome = match crate::tokio_handle()
                    .spawn(
                        async move { services.row_editing.open_session(&profile, request).await },
                    )
                    .await
                {
                    Ok(outcome) => outcome.map_err(|e| e.message()),
                    Err(join_err) => Err(sqlly_core::DisplayMessage::raw(join_err.to_string())),
                };
                let _ = tx.send(outcome).await;
            })
            .detach();
            let weak_self = cx.weak_entity();
            let async_cx = cx.to_async();
            cx.spawn(async move |_this, _cx| {
                if let Some(outcome) = rx.recv().await {
                    let mut cx = async_cx.clone();
                    let _ = weak_self.update(&mut cx, |this, cx| {
                        match outcome {
                            Ok(sqlly_services::row_editing::RowEditOpenOutcome::Editable(_))
                                if results.read(cx).run_generation() != run_generation =>
                            {
                                this.set_status(
                                    StatusLevel::Info,
                                    crate::localization::text(
                                        "shell.cells.results_changed_before_staging",
                                    ),
                                );
                            }
                            Ok(sqlly_services::row_editing::RowEditOpenOutcome::Editable(
                                session,
                            )) => {
                                this.apply_cell_action_session(
                                    results,
                                    *session,
                                    columns,
                                    row,
                                    (column_name, result_column),
                                    action,
                                    (db, generators_blocked),
                                    cx,
                                );
                            }
                            Ok(sqlly_services::row_editing::RowEditOpenOutcome::NotEditable {
                                reason,
                                message,
                            }) => {
                                let reason = crate::localization::stored_message_or(
                                    message.as_ref(),
                                    reason,
                                );
                                this.set_status(StatusLevel::Info, reason);
                            }
                            Err(err) => {
                                this.set_status(
                                    StatusLevel::Info,
                                    crate::localization::message(err),
                                );
                            }
                        }
                        cx.notify();
                    });
                }
            })
            .detach();
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_cell_action_session(
        &mut self,
        results: Entity<ResultsView>,
        session: sqlly_services::row_editing::RowEditSession,
        columns: Vec<sqlly_core::ResultColumn>,
        row: Vec<sqlly_core::CellValue>,
        (result_name, result_column): (String, usize),
        action: crate::cell_actions::CellAction,
        (db, generators_blocked): (sqlly_core::DatabaseType, bool),
        cx: &mut Context<Self>,
    ) {
        // Resolve the clicked result column to its table column through
        // provenance only: in `SELECT left(body, 50) AS body` the result
        // column is an expression, and a name match would stage writes (Set
        // NULL, a transform of the full original) into the real `body`.
        let Some(column) =
            sqlly_services::row_editing::table_column_for_result(&session, result_column).cloned()
        else {
            self.set_status(
                StatusLevel::Info,
                crate::localization::format(
                    "shell.cells.expression_not_editable",
                    &[("column", &result_name)],
                ),
            );
            return;
        };
        let column_name = column.name.clone();
        if let crate::cell_actions::CellAction::SqlFunction(expr) = &action
            && generators_blocked
            && !crate::cell_actions::expression_allowed_over_relay(*expr)
        {
            self.set_status(
                StatusLevel::Info,
                crate::cell_actions::RELAY_SQL_FUNCTIONS_REASON.to_string(),
            );
            return;
        }
        let actions = crate::cell_actions::actions_for(db, &column, column.is_editable())
            .for_route(generators_blocked);
        let allowed = match &action {
            crate::cell_actions::CellAction::SetNull => actions.set_null,
            crate::cell_actions::CellAction::SetDefault => actions.set_default,
            crate::cell_actions::CellAction::ServerTime => actions.server_time,
            crate::cell_actions::CellAction::SetBool(_) => actions.bool_choices,
            crate::cell_actions::CellAction::SetEnum(value) => {
                actions.enum_choices.iter().any(|choice| choice == value)
            }
            crate::cell_actions::CellAction::LoadFromFile(_)
            | crate::cell_actions::CellAction::SetBytes(_) => actions.binary,
            crate::cell_actions::CellAction::SaveToFile(_) => true,
            crate::cell_actions::CellAction::Transform(_) => actions.transforms,
            crate::cell_actions::CellAction::NewUuid => actions.new_uuid,
            crate::cell_actions::CellAction::LocalTimestamp => actions.server_time,
            crate::cell_actions::CellAction::SqlFunction(expr) => {
                actions.sql_functions.contains(expr)
            }
            // Validated and parsed for the column below.
            crate::cell_actions::CellAction::SetText(_) => true,
        };
        if !allowed {
            let key = match &action {
                crate::cell_actions::CellAction::SetDefault => "shell.cells.no_default",
                crate::cell_actions::CellAction::SetNull => "shell.cells.not_nullable",
                crate::cell_actions::CellAction::ServerTime
                | crate::cell_actions::CellAction::LocalTimestamp => "shell.cells.not_temporal",
                crate::cell_actions::CellAction::LoadFromFile(_)
                | crate::cell_actions::CellAction::SetBytes(_) => "shell.cells.not_binary",
                crate::cell_actions::CellAction::Transform(_) => "shell.cells.not_editable_text",
                crate::cell_actions::CellAction::NewUuid => "shell.cells.not_uuid",
                _ => "shell.cells.action_unsupported",
            };
            let message = if matches!(&action, crate::cell_actions::CellAction::SetDefault)
                && let Some(reason) = crate::capabilities::cell_default_in_update(db).reason()
            {
                reason.to_string()
            } else {
                crate::localization::format(key, &[("column", &column_name)])
            };
            self.set_status(StatusLevel::Info, message);
            return;
        }
        if let crate::cell_actions::CellAction::SaveToFile(path) = &action {
            #[cfg(not(target_arch = "wasm32"))]
            {
                let Some(index) = session
                    .columns
                    .iter()
                    .position(|column| column.name == column_name)
                else {
                    return;
                };
                let wrote = match session.original_row.get(index) {
                    Some(sqlly_core::CellValue::Bytes(bytes)) => {
                        std::fs::write(path, bytes).map_err(|err| err.to_string())
                    }
                    Some(sqlly_core::CellValue::String(text)) => {
                        std::fs::write(path, text.as_bytes()).map_err(|err| err.to_string())
                    }
                    Some(_) => Err(crate::localization::text("shell.cells.not_binary_or_text")),
                    None => Err(crate::localization::text("shell.cells.original_unreadable")),
                };
                if let Err(error) = wrote {
                    self.set_status(
                        StatusLevel::Info,
                        crate::localization::format(
                            "shell.cells.save_file_failed",
                            &[("error", &error)],
                        ),
                    );
                }
            }
            #[cfg(target_arch = "wasm32")]
            {
                let _ = path;
                self.set_status(
                    StatusLevel::Info,
                    crate::localization::text("shell.cells.save_file_unavailable"),
                );
            }
            return;
        }
        let edit = match action {
            crate::cell_actions::CellAction::SetNull => {
                crate::pending_changes::CellEdit::Value(sqlly_core::CellValue::Null)
            }
            crate::cell_actions::CellAction::SetDefault => {
                crate::pending_changes::CellEdit::Expression(
                    sqlly_services::row_editing::ColumnExpression::Default,
                )
            }
            crate::cell_actions::CellAction::ServerTime => {
                crate::pending_changes::CellEdit::Expression(
                    sqlly_services::row_editing::ColumnExpression::ServerTime,
                )
            }
            crate::cell_actions::CellAction::SetBool(Some(value)) => {
                crate::pending_changes::CellEdit::Value(sqlly_core::CellValue::Bool(value))
            }
            crate::cell_actions::CellAction::SetBool(None) => {
                crate::pending_changes::CellEdit::Value(sqlly_core::CellValue::Null)
            }
            crate::cell_actions::CellAction::SetEnum(value) => {
                crate::pending_changes::CellEdit::Value(sqlly_core::CellValue::String(value))
            }
            crate::cell_actions::CellAction::SetBytes(bytes) => {
                if let Some(message) = crate::cell_actions::blob_too_large(bytes.len() as u64) {
                    self.set_status(StatusLevel::Info, message);
                    return;
                }
                crate::pending_changes::CellEdit::Value(sqlly_core::CellValue::Bytes(bytes))
            }
            crate::cell_actions::CellAction::LoadFromFile(path) => {
                #[cfg(target_arch = "wasm32")]
                {
                    let _ = path;
                    self.set_status(
                        StatusLevel::Info,
                        crate::localization::text("shell.cells.load_file_unavailable"),
                    );
                    return;
                }
                #[cfg(not(target_arch = "wasm32"))]
                {
                    match std::fs::read(&path) {
                        Ok(bytes) => {
                            if let Some(message) =
                                crate::cell_actions::blob_too_large(bytes.len() as u64)
                            {
                                self.set_status(StatusLevel::Info, message);
                                return;
                            }
                            crate::pending_changes::CellEdit::Value(sqlly_core::CellValue::Bytes(
                                bytes,
                            ))
                        }
                        Err(error) => {
                            self.set_status(
                                StatusLevel::Info,
                                crate::localization::format(
                                    "shell.cells.load_file_failed",
                                    &[("error", &error.to_string())],
                                ),
                            );
                            return;
                        }
                    }
                }
            }
            crate::cell_actions::CellAction::SaveToFile(_) => return,
            crate::cell_actions::CellAction::Transform(transform) => {
                // Transform the staged value when the cell already has one,
                // so macros chain (trim, then Title Case) before saving.
                let staged =
                    crate::pending_changes::row_key_from_result_row(&session, &columns, &row)
                        .and_then(|key| results.read(cx).staged_cell_edit(&key, &column_name));
                let current = match staged {
                    Some(crate::pending_changes::CellEdit::Value(value)) => value,
                    Some(crate::pending_changes::CellEdit::Expression(_)) => {
                        self.set_status(
                            StatusLevel::Info,
                            crate::localization::format(
                                "shell.cells.staged_expression",
                                &[("column", &column_name)],
                            ),
                        );
                        return;
                    }
                    None => session
                        .columns
                        .iter()
                        .position(|c| c.name == column_name)
                        .and_then(|index| session.original_row.get(index).cloned())
                        .unwrap_or(sqlly_core::CellValue::Null),
                };
                let Some(text) = crate::cell_actions::transformed_cell_text(&current, transform)
                else {
                    self.set_status(
                        StatusLevel::Info,
                        crate::localization::format(
                            "shell.cells.nothing_to_transform",
                            &[("transform", transform.label()), ("column", column_name)],
                        ),
                    );
                    return;
                };
                crate::pending_changes::CellEdit::Value(sqlly_core::CellValue::String(text))
            }
            crate::cell_actions::CellAction::NewUuid => crate::pending_changes::CellEdit::Value(
                sqlly_core::CellValue::String(crate::cell_actions::new_uuid_text()),
            ),
            crate::cell_actions::CellAction::LocalTimestamp => {
                let kind = crate::row_editor_model::field_kind_for(db, &column.data_type);
                let Some(text) = crate::cell_actions::quick_pick_now(kind) else {
                    return;
                };
                crate::pending_changes::CellEdit::Value(sqlly_core::CellValue::String(text))
            }
            crate::cell_actions::CellAction::SqlFunction(expr) => {
                crate::pending_changes::CellEdit::Expression(expr)
            }
            crate::cell_actions::CellAction::SetText(text) => {
                match crate::cell_actions::text_cell_edit(db, &column, &text) {
                    Ok(edit) => edit,
                    Err(reason) => {
                        self.set_status(StatusLevel::Info, reason);
                        return;
                    }
                }
            }
        };
        let Some(key) = crate::pending_changes::row_key_from_result_row(&session, &columns, &row)
        else {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.cells.no_key_column"),
            );
            return;
        };
        let mut changes = std::collections::BTreeMap::new();
        changes.insert(column_name, edit);
        let op = crate::row_editor::StagedRowOp::Update {
            key,
            original_row: session.original_row.clone(),
            changes,
        };
        let slot = results
            .read(cx)
            .staging_slot
            .or(results.read(cx).pending_slot)
            .unwrap_or(0);
        results.update(cx, |rv, cx| {
            rv.staging_slot = None;
            rv.apply_staged_op(slot, session, op, cx);
        });
    }

    fn handle_row_editor_event(
        &mut self,
        view: Entity<crate::row_editor::RowEditor>,
        event: &crate::row_editor::RowEditorEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            crate::row_editor::RowEditorEvent::Dismissed { saved } => {
                let saved = *saved;
                self.restore_editor_focus(cx);
                let results = if self
                    .overlays
                    .row_editor
                    .as_ref()
                    .is_some_and(|(dialog, _)| dialog == &view)
                {
                    self.overlays.row_editor.take().map(|(_, results)| results)
                } else {
                    self.overlays
                        .popped_out_row_editors
                        .iter()
                        .position(|(dialog, _)| dialog == &view)
                        .map(|index| self.overlays.popped_out_row_editors.remove(index).1)
                };
                let Some(results) = results else {
                    return;
                };
                if saved {
                    self.refresh_results_after_row_edit(results, cx);
                }
                cx.notify();
            }
            crate::row_editor::RowEditorEvent::Staged { session, op } => {
                let session = session.clone();
                let op = op.clone();
                let results = if self
                    .overlays
                    .row_editor
                    .as_ref()
                    .is_some_and(|(dialog, _)| dialog == &view)
                {
                    self.overlays.row_editor.take().map(|(_, results)| results)
                } else {
                    self.overlays
                        .popped_out_row_editors
                        .iter()
                        .position(|(dialog, _)| dialog == &view)
                        .map(|index| self.overlays.popped_out_row_editors.remove(index).1)
                };
                let Some(results) = results else {
                    return;
                };
                self.restore_editor_focus(cx);
                let slot = results
                    .read(cx)
                    .staging_slot
                    .or(results.read(cx).pending_slot)
                    .unwrap_or(0);
                results.update(cx, |rv, cx| {
                    rv.staging_slot = None;
                    rv.apply_staged_op(slot, session, op, cx);
                });
                cx.notify();
            }
            crate::row_editor::RowEditorEvent::OpenQuery { sql } => {
                // Open the related-rows query in a fresh tab for inspection;
                // leave the editor dialog open so the user can keep editing.
                self.new_tab(cx, Some(sql.clone()));
                cx.notify();
            }
            crate::row_editor::RowEditorEvent::PopOut(payload) => {
                let Some((_dialog, results)) = self.overlays.row_editor.take() else {
                    return;
                };
                if let Some(dialog) = crate::row_editor::RowEditor::open_window(payload.clone(), cx)
                {
                    cx.subscribe(&dialog, Self::handle_row_editor_event)
                        .detach();
                    self.overlays.popped_out_row_editors.push((dialog, results));
                }
                cx.notify();
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_paste_requested(
        &mut self,
        results: Entity<ResultsView>,
        result_set_index: usize,
        columns: Vec<sqlly_core::ResultColumn>,
        anchor_row: Option<Vec<sqlly_core::CellValue>>,
        sql: String,
        text: String,
        anchor: (usize, usize),
        overwrite: bool,
        fill_range: Option<(usize, usize)>,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self.active_profile.clone() else {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.rows.connect_to_paste"),
            );
            cx.notify();
            return;
        };
        if let Some(reason) = crate::capabilities::row_editing(profile.database_type).reason() {
            self.set_status(StatusLevel::Info, reason.to_string());
            cx.notify();
            return;
        }
        let slot = results
            .read(cx)
            .staging_slot
            .or(results.read(cx).pending_slot)
            .unwrap_or(0);
        self.with_entra_sql_token(profile, cx, move |app, profile, cx| {
            let request = sqlly_services::row_editing::RowEditOpenRequest {
                sql,
                result_set_index,
                columns: columns.clone(),
                row: anchor_row.clone().unwrap_or_default(),
            };
            let services = app.services_for_profile(&profile);
            let db = profile.database_type;
            let has_anchor = anchor_row.is_some();
            let (tx, mut rx) = tokio::sync::mpsc::channel::<
                Result<sqlly_services::row_editing::RowEditOpenOutcome, sqlly_core::DisplayMessage>,
            >(1);
            cx.background_spawn(async move {
                let outcome = match crate::tokio_handle()
                    .spawn(async move {
                        if has_anchor {
                            services.row_editing.open_session(&profile, request).await
                        } else {
                            services
                                .row_editing
                                .open_insert_session(&profile, request)
                                .await
                        }
                    })
                    .await
                {
                    Ok(outcome) => outcome.map_err(|e| e.message()),
                    Err(join_err) => Err(sqlly_core::DisplayMessage::raw(join_err.to_string())),
                };
                let _ = tx.send(outcome).await;
            })
            .detach();
            let weak_self = cx.weak_entity();
            let async_cx = cx.to_async();
            cx.spawn(async move |_this, _cx| {
                if let Some(outcome) = rx.recv().await {
                    let mut cx = async_cx.clone();
                    let _ = weak_self.update(&mut cx, |this, cx| {
                        match outcome {
                            Ok(sqlly_services::row_editing::RowEditOpenOutcome::Editable(
                                session,
                            )) => {
                                let (_, cells) = crate::paste_rows::parse_clipboard(&text);
                                let cells = if overwrite {
                                    crate::paste_rows::fill_selection(cells, fill_range)
                                } else {
                                    cells
                                };
                                let mut mapped =
                                    crate::paste_rows::map_rows(&session, db, anchor.1, &cells);
                                let mut targets = Vec::new();
                                if overwrite {
                                    for i in 0..cells.len() {
                                        let display_row = anchor.0 + i;
                                        let Some(row) = results.read(cx).display_row_source(
                                            slot,
                                            display_row,
                                            cx,
                                        ) else {
                                            mapped.errors.push(crate::paste_rows::PasteCellError {
                                                row: i,
                                                column: "(key)".to_string(),
                                                text: String::new(),
                                                reason: "could not resolve row key".to_string(),
                                            });
                                            continue;
                                        };
                                        match crate::pending_changes::row_key_from_result_row(
                                            &session, &columns, &row,
                                        ) {
                                            Some(key) => {
                                                let changes: std::collections::BTreeMap<_, _> =
                                                    mapped
                                                        .rows
                                                        .get(i)
                                                        .cloned()
                                                        .unwrap_or_default()
                                                        .into_iter()
                                                        .collect();
                                                targets.push((key, row, changes));
                                            }
                                            None => {
                                                mapped.errors.push(
                                                    crate::paste_rows::PasteCellError {
                                                        row: i,
                                                        column: "(key)".to_string(),
                                                        text: String::new(),
                                                        reason: "could not resolve row key"
                                                            .to_string(),
                                                    },
                                                );
                                            }
                                        }
                                    }
                                    mapped.rows.clear();
                                }
                                results.update(cx, |rv, cx| {
                                    rv.staging_slot = None;
                                    rv.apply_paste(slot, *session, mapped, targets, cx);
                                });
                            }
                            Ok(sqlly_services::row_editing::RowEditOpenOutcome::NotEditable {
                                reason,
                                message,
                            }) => {
                                let reason = crate::localization::stored_message_or(
                                    message.as_ref(),
                                    reason,
                                );
                                this.set_status(StatusLevel::Info, reason);
                            }
                            Err(err) => {
                                this.set_status(
                                    StatusLevel::Info,
                                    crate::localization::message(err),
                                );
                            }
                        }
                        cx.notify();
                    });
                }
            })
            .detach();
        });
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[allow(clippy::too_many_arguments)]
    fn open_batch_insert_dialog(
        &mut self,
        results: Entity<ResultsView>,
        result_set_index: usize,
        columns: Vec<sqlly_core::ResultColumn>,
        row: Vec<sqlly_core::CellValue>,
        sql: String,
        cx: &mut Context<Self>,
    ) {
        if self.overlays.batch_insert_dialog.is_some() {
            return;
        }
        let Some(profile) = self.active_profile.clone() else {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.rows.connect_to_insert"),
            );
            cx.notify();
            return;
        };
        if let Some(reason) = crate::capabilities::row_editing(profile.database_type).reason() {
            self.set_status(StatusLevel::Info, reason.to_string());
            cx.notify();
            return;
        }
        let slot = results
            .read(cx)
            .staging_slot
            .or(results.read(cx).pending_slot)
            .unwrap_or(0);
        self.with_entra_sql_token(profile, cx, move |app, profile, cx| {
            let request = sqlly_services::row_editing::RowEditOpenRequest {
                sql,
                result_set_index,
                columns,
                row: row.clone(),
            };
            let services = app.services_for_profile(&profile);
            let db = profile.database_type;
            let (tx, mut rx) = tokio::sync::mpsc::channel::<
                Result<sqlly_services::row_editing::RowEditOpenOutcome, sqlly_core::DisplayMessage>,
            >(1);
            cx.background_spawn(async move {
                let outcome = match crate::tokio_handle()
                    .spawn(
                        async move { services.row_editing.open_session(&profile, request).await },
                    )
                    .await
                {
                    Ok(outcome) => outcome.map_err(|e| e.message()),
                    Err(join_err) => Err(sqlly_core::DisplayMessage::raw(join_err.to_string())),
                };
                let _ = tx.send(outcome).await;
            })
            .detach();
            let weak_self = cx.weak_entity();
            let async_cx = cx.to_async();
            let template = row;
            cx.spawn(async move |_this, _cx| {
                if let Some(outcome) = rx.recv().await {
                    let mut cx = async_cx.clone();
                    let _ = weak_self.update(&mut cx, |this, cx| {
                        match outcome {
                            Ok(sqlly_services::row_editing::RowEditOpenOutcome::Editable(
                                session,
                            )) => {
                                let app_view = cx.entity();
                                cx.defer(move |app| {
                                    let Some(win) = app.active_window() else {
                                        return;
                                    };
                                    let _ = win.update(app, |_root, window, app| {
                                        app_view.update(app, |this, cx| {
                                            if this.overlays.batch_insert_dialog.is_some() {
                                                return;
                                            }
                                            let model = crate::batch_insert_dialog::BatchInsertModel::from_template(
                                                *session,
                                                Some(template),
                                                db,
                                            );
                                            let dialog = cx.new(|cx| {
                                                crate::batch_insert_dialog::BatchInsertDialog::new(
                                                    model, window, cx,
                                                )
                                            });
                                            let results_cb = results.clone();
                                            cx.subscribe(&dialog, move |this, _view, event, cx| {
                                                this.handle_batch_insert_event(
                                                    results_cb.clone(),
                                                    slot,
                                                    event,
                                                    cx,
                                                );
                                            })
                                            .detach();
                                            this.overlays.batch_insert_dialog = Some(dialog);
                                            cx.notify();
                                        });
                                    });
                                });
                            }
                            Ok(
                                sqlly_services::row_editing::RowEditOpenOutcome::NotEditable {
                                    reason,
                                    message,
                                },
                            ) => {
                                let reason =
                                    crate::localization::stored_message_or(message.as_ref(), reason);
                                this.set_status(StatusLevel::Info, reason);
                                cx.notify();
                            }
                            Err(err) => {
                                this.set_status(StatusLevel::Info, crate::localization::message(err));
                                cx.notify();
                            }
                        }
                    });
                }
            })
            .detach();
        });
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn handle_batch_insert_event(
        &mut self,
        results: Entity<ResultsView>,
        slot: usize,
        event: &crate::batch_insert_dialog::BatchInsertEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            crate::batch_insert_dialog::BatchInsertEvent::Staged { session, rows } => {
                let session = session.as_ref().clone();
                let rows = rows.clone();
                self.overlays.batch_insert_dialog = None;
                self.restore_editor_focus(cx);
                results.update(cx, |rv, cx| {
                    rv.staging_slot = None;
                    rv.pending_slot.get_or_insert(slot);
                    rv.apply_batch_inserts(slot, session, rows, cx);
                });
            }
            crate::batch_insert_dialog::BatchInsertEvent::Dismissed => {
                self.overlays.batch_insert_dialog = None;
                self.restore_editor_focus(cx);
            }
        }
        cx.notify();
    }

    /// Resolve the primary key via the row-editing service and copy UPDATE
    /// statements for the selected rows. Never writes a partial script.
    fn copy_sql_update(
        &mut self,
        result_set_index: usize,
        columns: Vec<sqlly_core::ResultColumn>,
        rows: Vec<Vec<sqlly_core::CellValue>>,
        sample_row: Vec<sqlly_core::CellValue>,
        sql: String,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self.active_profile.clone() else {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.rows.connect_to_copy_update"),
            );
            cx.notify();
            return;
        };
        if let Some(reason) = crate::capabilities::row_editing(profile.database_type).reason() {
            self.set_status(StatusLevel::Info, reason.to_string());
            cx.notify();
            return;
        }
        self.with_entra_sql_token(profile, cx, move |app, profile, cx| {
            let request = sqlly_services::row_editing::RowEditOpenRequest {
                sql,
                result_set_index,
                columns: columns.clone(),
                row: sample_row,
            };
            let services = app.services_for_profile(&profile);
            let db = profile.database_type;
            let (tx, mut rx) = tokio::sync::mpsc::channel::<
                Result<sqlly_services::row_editing::RowEditOpenOutcome, sqlly_core::DisplayMessage>,
            >(1);
            cx.background_spawn(async move {
                let outcome = match crate::tokio_handle()
                    .spawn(
                        async move { services.row_editing.open_session(&profile, request).await },
                    )
                    .await
                {
                    Ok(outcome) => outcome.map_err(|e| e.message()),
                    Err(join_err) => Err(sqlly_core::DisplayMessage::raw(join_err.to_string())),
                };
                let _ = tx.send(outcome).await;
            })
            .detach();
            let weak_self = cx.weak_entity();
            let async_cx = cx.to_async();
            cx.spawn(async move |_this, _cx| {
                if let Some(outcome) = rx.recv().await {
                    let mut cx = async_cx.clone();
                    let _ = weak_self.update(&mut cx, |this, cx| {
                        match outcome {
                            Ok(sqlly_services::row_editing::RowEditOpenOutcome::Editable(
                                session,
                            )) => {
                                let table =
                                    format!("{}.{}", session.target.schema, session.target.table);
                                match crate::result_export::update_statements_for(
                                    db,
                                    &table,
                                    &columns,
                                    &rows,
                                    &session.key_columns,
                                ) {
                                    Ok(text) => {
                                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                            text,
                                        ));
                                        this.set_status(
                                            StatusLevel::Info,
                                            crate::localization::format(
                                                if rows.len() == 1 {
                                                    "shell.rows.copied_update_singular"
                                                } else {
                                                    "shell.rows.copied_update_plural"
                                                },
                                                &[("count", &rows.len().to_string())],
                                            ),
                                        );
                                    }
                                    Err(err) => {
                                        this.set_status(StatusLevel::Info, err.to_string());
                                    }
                                }
                            }
                            Ok(sqlly_services::row_editing::RowEditOpenOutcome::NotEditable {
                                reason,
                                message,
                            }) => {
                                let reason = crate::localization::stored_message_or(
                                    message.as_ref(),
                                    reason,
                                );
                                this.set_status(StatusLevel::Info, reason);
                            }
                            Err(err) => {
                                this.set_status(
                                    StatusLevel::Info,
                                    crate::localization::message(err),
                                );
                            }
                        }
                        cx.notify();
                    });
                }
            })
            .detach();
        });
    }

    /// Open one row-edit session, then stage every selected row as a delete
    /// without showing the row-editor modal.
    // Cohesive row-staging entry point; splitting the args would not clarify it.
    #[allow(clippy::too_many_arguments)]
    fn stage_delete_many(
        &mut self,
        results: Entity<ResultsView>,
        result_set_index: usize,
        columns: Vec<sqlly_core::ResultColumn>,
        rows: Vec<Vec<sqlly_core::CellValue>>,
        sample_row: Vec<sqlly_core::CellValue>,
        sql: String,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self.active_profile.clone() else {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.rows.connect_to_stage_deletes"),
            );
            cx.notify();
            return;
        };
        if let Some(reason) = crate::capabilities::row_editing(profile.database_type).reason() {
            self.set_status(StatusLevel::Info, reason.to_string());
            cx.notify();
            return;
        }
        self.with_entra_sql_token(profile, cx, move |app, profile, cx| {
            let request = sqlly_services::row_editing::RowEditOpenRequest {
                sql,
                result_set_index,
                columns: columns.clone(),
                row: sample_row,
            };
            let services = app.services_for_profile(&profile);
            let (tx, mut rx) = tokio::sync::mpsc::channel::<
                Result<sqlly_services::row_editing::RowEditOpenOutcome, sqlly_core::DisplayMessage>,
            >(1);
            cx.background_spawn(async move {
                let outcome = match crate::tokio_handle()
                    .spawn(
                        async move { services.row_editing.open_session(&profile, request).await },
                    )
                    .await
                {
                    Ok(outcome) => outcome.map_err(|e| e.message()),
                    Err(join_err) => Err(sqlly_core::DisplayMessage::raw(join_err.to_string())),
                };
                let _ = tx.send(outcome).await;
            })
            .detach();
            let weak_self = cx.weak_entity();
            let async_cx = cx.to_async();
            let results_for_apply = results.clone();
            let staged_count = rows.len();
            cx.spawn(async move |_this, _cx| {
                if let Some(outcome) = rx.recv().await {
                    let mut cx = async_cx.clone();
                    let _ = weak_self.update(&mut cx, |this, cx| {
                        match outcome {
                            Ok(sqlly_services::row_editing::RowEditOpenOutcome::Editable(
                                session,
                            )) => {
                                let slot = results_for_apply
                                    .read(cx)
                                    .staging_slot
                                    .or(results_for_apply.read(cx).pending_slot)
                                    .unwrap_or(0);
                                results_for_apply.update(cx, |rv, cx| {
                                    rv.staging_slot = None;
                                    rv.apply_staged_deletes(slot, *session, rows, cx);
                                });
                                this.set_status(
                                    StatusLevel::Info,
                                    crate::localization::format(
                                        if staged_count == 1 {
                                            "shell.rows.staged_delete_singular"
                                        } else {
                                            "shell.rows.staged_delete_plural"
                                        },
                                        &[("count", &staged_count.to_string())],
                                    ),
                                );
                            }
                            Ok(sqlly_services::row_editing::RowEditOpenOutcome::NotEditable {
                                reason,
                                message,
                            }) => {
                                let reason = crate::localization::stored_message_or(
                                    message.as_ref(),
                                    reason,
                                );
                                this.set_status(StatusLevel::Info, reason);
                            }
                            Err(err) => {
                                this.set_status(
                                    StatusLevel::Info,
                                    crate::localization::message(err),
                                );
                            }
                        }
                        cx.notify();
                    });
                }
            })
            .detach();
        });
    }

    /// Arms the staged-batch save through the same mutation-confirm gate as Run.
    fn handle_save_staged_requested(
        &mut self,
        results: Entity<ResultsView>,
        batch: sqlly_services::row_editing::StagedBatch,
        rollback_sql: String,
        preview_sql: String,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self.active_profile.clone() else {
            return;
        };
        // A relay bridge that cannot commit a batch would fail the save;
        // refuse it up front with the reason instead.
        if crate::cell_actions::staged_save_blocked(&profile, &self.services_for_profile(&profile))
        {
            self.set_status(
                StatusLevel::Error,
                crate::cell_actions::RELAY_STAGED_SAVE_REASON.to_string(),
            );
            cx.notify();
            return;
        }
        let store = crate::connection::ConnectionStore::global();
        let now = Instant::now();
        let mut profile = profile.clone();
        crate::mutation_review::apply_inherited_execution_policy(
            &mut profile,
            store,
            self.production_unlock.as_ref(),
            now,
        );
        let safety = crate::mutation_review::SafetyFeatures::effective_for_profile(&profile, store);
        let production = safety.production_guard();
        if production.forces_select_only(self.production_unlock.as_ref(), now) {
            self.set_status(
                StatusLevel::Error,
                crate::localization::text("shell.staged_save.production_read_only"),
            );
            cx.notify();
            return;
        }
        let table = batch
            .target
            .as_ref()
            .map(|t| t.quoted())
            .unwrap_or_else(|| crate::localization::text("shell.staged_save.table_fallback"));
        let n = batch.ops.len();
        let bottom_line = crate::localization::format(
            if n == 1 {
                "shell.staged_save.commit_singular"
            } else {
                "shell.staged_save.commit_plural"
            },
            &[("count", &n.to_string()), ("table", &table)],
        );
        let needs_confirm = safety.use_confirmation_dialog
            || safety.only_allow_select
            || safety.require_ai_review_for_data_changes;
        if needs_confirm {
            self.mutation_review_generation += 1;
            let generation = self.mutation_review_generation;
            self.pending.staged_save = Some(crate::app::pending_dialogs::PendingStagedSave {
                batch,
                rollback_sql,
                preview_sql: preview_sql.clone(),
                results,
                bottom_line,
                ai_review_required: safety.require_ai_review_for_data_changes,
                ai_explanation: None,
                ai_generation: generation,
            });
            if safety.require_ai_review_for_data_changes {
                self.spawn_ai_mutation_review(preview_sql, generation, cx);
            }
            cx.notify();
            return;
        }
        self.execute_staged_save(profile, batch, rollback_sql, preview_sql, results, cx);
    }

    fn execute_staged_save(
        &mut self,
        profile: sqlly_core::ConnectionProfile,
        batch: sqlly_services::row_editing::StagedBatch,
        rollback_sql: String,
        preview_sql: String,
        results: Entity<ResultsView>,
        cx: &mut Context<Self>,
    ) {
        // Mint/refresh the Entra SQL token before the commit's fresh connect —
        // the staged batch runs on the row-edit warm lane, a distinct
        // connection from the query lane, so it must carry a current token just
        // like the query path and the editor-open path do. Without this, a
        // stale token surfaces as Azure's "cannot open server" (40532) login
        // failure at commit time.
        self.with_entra_sql_token(profile, cx, move |this, profile, cx| {
            let services = this.services_for_profile(&profile);
            let server = profile.server.clone();
            let database = profile.database.clone();
            // Use the crate's gated `Instant` alias (web_time on wasm); the bare
            // `std::time::Instant::now()` panics in the browser build.
            let started = Instant::now();
            let (tx, mut rx) = tokio::sync::mpsc::channel::<
                Result<sqlly_services::row_editing::BatchOutcome, sqlly_core::StudioError>,
            >(1);
            let request = sqlly_services::row_editing::BatchRequest { batch };
            // The row-edit service connects and times out through tokio, so
            // it runs on the app runtime (a bare gpui background thread has no
            // reactor) — the same hop `row_editor::spawn_insert` makes.
            cx.background_spawn(async move {
                let outcome = match crate::tokio_handle()
                    .spawn(async move { services.row_editing.apply_batch(&profile, request).await })
                    .await
                {
                    Ok(outcome) => outcome,
                    Err(join_err) => Err(sqlly_core::StudioError::Other(join_err.to_string())),
                };
                let _ = tx.send(outcome).await;
            })
            .detach();
            let app = cx.entity();
            cx.spawn(async move |_, cx| {
                if let Some(outcome) = rx.recv().await {
                    app.update(cx, |this, cx| {
                        this.finish_staged_save(
                            outcome,
                            results,
                            &server,
                            database.as_deref(),
                            preview_sql,
                            rollback_sql,
                            started,
                            cx,
                        );
                    });
                }
            })
            .detach();
        });
    }

    // Cohesive save-completion handler; splitting the args would not clarify it.
    #[allow(clippy::too_many_arguments)]
    fn finish_staged_save(
        &mut self,
        outcome: Result<sqlly_services::row_editing::BatchOutcome, sqlly_core::StudioError>,
        results: Entity<ResultsView>,
        server: &str,
        database: Option<&str>,
        preview_sql: String,
        rollback_sql: String,
        // Gated alias (web_time on wasm); matches the `Instant::now()` caller.
        started: Instant,
        cx: &mut Context<Self>,
    ) {
        match outcome {
            Ok(sqlly_services::row_editing::BatchOutcome::Committed { .. }) => {
                let duration_ms = started.elapsed().as_millis() as u64;
                crate::query_history::QueryHistory::log_completed_with_rollback(
                    &preview_sql,
                    server,
                    database,
                    Some(duration_ms),
                    crate::query_history::HistoryOutcome::Succeeded,
                    Some(&rollback_sql),
                );
                results.update(cx, |rv, cx| rv.discard_staged(cx));
                self.refresh_results_after_row_edit(results, cx);
                self.set_status(
                    StatusLevel::Success,
                    crate::localization::text("shell.staged_save.saved_status"),
                );
            }
            Ok(sqlly_services::row_editing::BatchOutcome::Conflict {
                statement_index,
                affected,
            }) => {
                self.set_status(
                    StatusLevel::Error,
                    crate::localization::format(
                        if affected == 1 {
                            "shell.staged_save.conflict_singular"
                        } else {
                            "shell.staged_save.conflict_plural"
                        },
                        &[
                            ("statement_index", &statement_index.to_string()),
                            ("affected", &affected.to_string()),
                        ],
                    ),
                );
            }
            Err(error) => {
                self.set_status(
                    StatusLevel::Error,
                    crate::localization::message(error.message()),
                );
            }
        }
        cx.notify();
    }

    fn confirm_staged_save(&mut self, cx: &mut Context<Self>) {
        let Some(save) = self.pending.staged_save.clone() else {
            return;
        };
        if save.ai_review_required && save.ai_explanation.is_none() {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.staged_save.ai_wait"),
            );
            cx.notify();
            return;
        }
        let Some(profile) = self.active_profile.clone() else {
            return;
        };
        self.pending.staged_save = None;
        self.execute_staged_save(
            profile,
            save.batch,
            save.rollback_sql,
            save.preview_sql,
            save.results,
            cx,
        );
    }

    fn cancel_staged_save(&mut self, cx: &mut Context<Self>) {
        self.pending.staged_save = None;
        self.set_status(
            StatusLevel::Info,
            crate::localization::text("shell.staged_save.cancelled_status"),
        );
        cx.notify();
    }

    /// After a committed row edit, re-runs the originating query so the grid
    /// shows current data — but never re-executes a batch containing
    /// mutations (re-running those silently could apply changes twice).
    fn refresh_results_after_row_edit(
        &mut self,
        results: Entity<ResultsView>,
        cx: &mut Context<Self>,
    ) {
        let Some(tab_idx) = self.tabs.iter().position(|tab| tab.results == results) else {
            return;
        };
        let Some(sql) = results.read(cx).query_sql().map(|s| s.to_string()) else {
            return;
        };
        if crate::mutation_review::contains_non_select_statement(&sql) {
            self.set_status(
                StatusLevel::Success,
                crate::localization::text("shell.rows.saved_refresh_hint"),
            );
            return;
        }
        let Some(profile) = self.active_profile.clone() else {
            return;
        };
        self.with_entra_sql_token(profile, cx, move |this, profile, cx| {
            let run_region = crate::execution_error::RunRegion {
                start: 0,
                revision: this
                    .tabs
                    .get(tab_idx)
                    .map(|tab| tab.editor.read(cx).content_revision())
                    .unwrap_or(0),
            };
            this.spawn_query_execution(
                tab_idx,
                sql.clone(),
                profile,
                results,
                None,
                crate::query_history::HistoryProvenance::Manual,
                sql,
                run_region,
                cx,
            );
        });
    }

    /// Count user-visible work that should block a deferred update download.
    pub(super) fn in_flight_work(&self, cx: &App) -> crate::app_update::InFlightWork {
        let queries = self
            .tabs
            .iter()
            .filter(|tab| {
                tab.active_query_id.is_some()
                    || tab.active_multi_run.is_some()
                    || tab.results.read(cx).is_running()
            })
            .count();
        #[cfg(not(target_arch = "wasm32"))]
        let dumps = usize::from(
            self.overlays
                .database_dump_dialog
                .as_ref()
                .is_some_and(|dialog| dialog.read(cx).is_running()),
        );
        #[cfg(target_arch = "wasm32")]
        let dumps = 0;
        #[cfg(not(target_arch = "wasm32"))]
        let transfers = usize::from(
            self.overlays
                .data_transfer_pane
                .as_ref()
                .is_some_and(|pane| pane.read(cx).is_running()),
        );
        #[cfg(target_arch = "wasm32")]
        let transfers = 0;
        let ai_runs = usize::from(self.ai_panel.read(cx).has_pending());
        let exports = self.background_tasks.running_count();
        let manual_tx = self
            .tabs
            .iter()
            .filter(|tab| tab.manual_tx.is_some())
            .count();
        crate::app_update::InFlightWork {
            queries,
            dumps,
            transfers,
            ai_runs,
            exports,
            manual_tx,
        }
    }

    /// Set the status line and its severity together. The only supported way to
    /// write the status: assigning `status_message` directly would leave the
    /// previous level in place, so an old error would color a new success.
    fn set_status(&mut self, level: StatusLevel, message: impl Into<String>) {
        self.status_level = level;
        self.status_message = message.into();
    }

    fn new_query_tab(&mut self, cx: &mut Context<Self>) {
        self.new_tab(cx, None);
    }

    #[allow(dead_code)]
    fn close_current_tab(&mut self, cx: &mut Context<Self>) {
        self.close_tab(self.active_tab, cx);
    }

    /// The placeholder shown in the query area when every tab is closed: the
    /// SQLLY logo, centered, over a nudge on how to open a new query window.
    fn render_empty_query_area(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let logo = crate::icons::sqlly_icon_large().map(|icon| {
            img(icon)
                .w(px(Theme::scaled(72.0)))
                .h(px(Theme::scaled(72.0)))
                .into_any_element()
        });
        #[cfg(target_os = "macos")]
        let shortcut = "\u{2318}N"; // ⌘N
        #[cfg(not(target_os = "macos"))]
        let shortcut = "Ctrl+N";
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(Theme::scaled(14.0)))
            .children(logo)
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(1.3)))
                    .text_color(Theme::text())
                    .child(crate::localization::text("shell.empty.no_query")),
            )
            .child(
                div()
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::format(
                        "shell.empty.new_query_shortcut",
                        &[("shortcut", shortcut)],
                    )),
            )
            .child(
                div()
                    .id("empty-new-query")
                    .mt(px(Theme::scaled(4.0)))
                    .px(px(Theme::scaled(14.0)))
                    .py(px(Theme::scaled(7.0)))
                    .rounded(px(Theme::scaled(6.0)))
                    .border_1()
                    .border_color(Theme::border())
                    .text_color(Theme::text())
                    .cursor_pointer()
                    .hover(|s| s.bg(Theme::popup_bg()))
                    .child(crate::localization::text("shell.empty.new_query_button"))
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.new_query_tab(cx);
                        }),
                    ),
            )
    }

    fn render_tab_bar(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        self.sync_qbar_selects(window, cx);
        if self
            .overlays
            .tab_rename
            .as_ref()
            .is_some_and(|rename| rename.input.is_none())
        {
            let value = self
                .overlays
                .tab_rename
                .as_ref()
                .map(|rename| rename.value.clone())
                .unwrap_or_default();
            let input = cx.new(|cx| {
                InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                    window,
                    cx,
                    "tabs.rename.placeholder",
                ))
            });
            let subscription = cx.subscribe(
                &input,
                |this: &mut Self, state, event: &InputEvent, cx| match event {
                    InputEvent::Change => {
                        if let Some(rename) = &mut this.overlays.tab_rename {
                            rename.value = state.read(cx).value().to_string();
                        }
                    }
                    InputEvent::PressEnter { .. } => this.confirm_tab_rename(cx),
                    InputEvent::Focus => {
                        if let Some(rename) = &mut this.overlays.tab_rename {
                            rename.focused = true;
                        }
                    }
                    InputEvent::Blur => {
                        if this
                            .overlays
                            .tab_rename
                            .as_ref()
                            .is_some_and(|rename| rename.focused)
                        {
                            this.cancel_tab_rename(cx);
                        }
                    }
                },
            );
            input.update(cx, |state, cx| {
                state.set_value(&value, window, cx);
                state.focus(window, cx);
            });
            if let Some(rename) = &mut self.overlays.tab_rename {
                rename.input = Some(input);
                rename._subscription = Some(subscription);
            }
        }
        let renaming = self
            .overlays
            .tab_rename
            .as_ref()
            .and_then(|rename| Some((rename.tab_idx, rename.input.clone()?)));
        let active = self.active_tab;
        let this = cx.entity();

        let connected_now = self.active_profile.is_some();
        // Names of the connections tabs are pinned to, so an inactive tab's
        // cue names its own server rather than the focused tab's.
        let connection_names: std::collections::HashMap<uuid::Uuid, String> = self
            .sidebar
            .read(cx)
            .connections()
            .iter()
            .map(|p| (p.id.0, p.name.clone()))
            .collect();
        let tab_items: Vec<AnyElement> = self
            .tabs
            .iter()
            .enumerate()
            .map(|(i, tab)| {
                let labels = tab.tab_bar_labels(i, i == active, connected_now);
                let running = labels.running;
                let name = labels.title;
                let connection_name = tab
                    .pending_resume_connection
                    .as_ref()
                    .map(|c| c.server.as_str())
                    .or_else(|| {
                        tab.connection_target
                            .as_ref()
                            .and_then(|t| connection_names.get(&t.profile_id))
                            .map(String::as_str)
                    })
                    .or(self.active_profile.as_ref().map(|p| p.name.as_str()));
                let chrome = tab_chrome_state(i == active);
                // Staged grid edits are unsaved work too (the dot is not
                // only about the SQL text).
                let dirty = tab.dirty || tabs::tab_has_staged_grid_edits(tab, cx);
                #[cfg(not(target_arch = "wasm32"))]
                let disk_badge = match tab.disk_state {
                    crate::file_watch::DiskState::ModifiedOnDisk => Some("↻"),
                    crate::file_watch::DiskState::DeletedOnDisk => Some("⌫"),
                    crate::file_watch::DiskState::Clean => None,
                };
                #[cfg(target_arch = "wasm32")]
                let disk_badge: Option<&str> = None;
                let close_this = this.clone();
                let menu_this = this.clone();
                let middle_this = this.clone();
                let select_this = this.clone();
                let hover_this = this.clone();
                let close_tip = labels.close_label;
                let tooltip_text = labels.tooltip;
                let rename_input = renaming
                    .as_ref()
                    .filter(|(idx, _)| *idx == i)
                    .map(|(_, input)| input.clone());
                let is_renaming = rename_input.is_some();
                let is_notebook = tab.is_notebook();
                let cue = if i != active && !is_renaming {
                    inactive_tab_connection_cue(connection_name)
                } else {
                    None
                };
                div()
                    .id(("query-tab", i))
                    .px(px(Theme::scaled(8.0)))
                    .py(px(Theme::scaled(3.0)))
                    .rounded(px(Theme::scaled(3.0)))
                    .max_w(px(Theme::scaled(220.0)))
                    .min_w(px(Theme::scaled(72.0)))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(Theme::scaled(6.0)))
                    .cursor_pointer()
                    .overflow_hidden()
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .text_color(if chrome.dim {
                        Theme::text_dim()
                    } else {
                        Theme::text()
                    })
                    .bg(if chrome.filled {
                        Theme::panel_bg()
                    } else {
                        gpui::transparent_black()
                    })
                    .hover(|s| {
                        if chrome.filled {
                            s
                        } else {
                            s.text_color(Theme::text())
                        }
                    })
                    .when(!is_renaming, |d| {
                        d.app_tooltip(move |_window, cx| tooltip_view(tooltip_text.clone(), cx))
                    })
                    .on_mouse_down(gpui::MouseButton::Left, move |event, _, app| {
                        select_this.update(app, |this, cx| {
                            this.tab_strip_focus_index = i;
                            if event.click_count >= 2 {
                                this.dragging_tab = None;
                                this.begin_tab_rename(i, cx);
                                return;
                            }
                            this.dragging_tab = Some(i);
                            this.select_tab(i, cx);
                        });
                    })
                    .on_mouse_move(move |_, _, app| {
                        hover_this.update(app, |this, cx| {
                            if let Some(from) = this.dragging_tab
                                && from != i
                            {
                                this.reorder_query_tab(from, i, cx);
                            }
                        });
                    })
                    .on_mouse_down(gpui::MouseButton::Middle, move |_, window, app| {
                        middle_this.update(app, |this, cx| {
                            this.request_close_tab(i, window, cx);
                        });
                    })
                    .on_mouse_down(gpui::MouseButton::Right, move |event, _window, app| {
                        app.stop_propagation();
                        let position = event.position;
                        menu_this.update(app, |this, cx| {
                            this.tab_context_menu = Some((i, position));
                            cx.notify();
                        });
                    })
                    // Running marker: a static accent dot before the title
                    // (the design system's Busy cue — motion is banned, so no
                    // spinner). The tooltip already says "(running)".
                    .when(running && !is_renaming, |d| {
                        d.child(
                            div()
                                .flex_none()
                                .w(px(Theme::scaled(6.0)))
                                .h(px(Theme::scaled(6.0)))
                                .rounded_full()
                                .bg(Theme::accent()),
                        )
                    })
                    .child(if let Some(input) = rename_input {
                        let escape_this = this.clone();
                        div()
                            .id(("tab-rename", i))
                            .min_w_0()
                            .h_full()
                            .flex_1()
                            .overflow_hidden()
                            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| {
                                cx.stop_propagation();
                            })
                            .on_key_down(move |event: &KeyDownEvent, _, app| {
                                if event.keystroke.key.as_str() == "escape" {
                                    escape_this.update(app, |this, cx| {
                                        this.cancel_tab_rename(cx);
                                    });
                                }
                            })
                            .child(
                                Input::new(&input)
                                    .appearance(false)
                                    .xsmall()
                                    .h_full()
                                    .min_w_0()
                                    .flex_1(),
                            )
                            .into_any_element()
                    } else {
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(Theme::scaled(4.0)))
                            .min_w_0()
                            .overflow_hidden()
                            .when(is_notebook, |d| {
                                d.child(
                                    Icon::empty()
                                        .path(AppIcon::Terminal.path())
                                        .text_color(Theme::text_dim())
                                        .w(px(Theme::scaled(12.0)))
                                        .h(px(Theme::scaled(12.0))),
                                )
                            })
                            .child(
                                div()
                                    .text_size(px(Theme::font_size_ratio(0.85)))
                                    .text_color(if chrome.dim {
                                        Theme::text_dim()
                                    } else {
                                        Theme::text()
                                    })
                                    .whitespace_nowrap()
                                    .overflow_hidden()
                                    .child(name.clone()),
                            )
                            .into_any_element()
                    })
                    .children(cue.map(|server| {
                        div()
                            .text_size(px(Theme::font_size_ratio(0.85)))
                            .text_color(Theme::text_dim())
                            .whitespace_nowrap()
                            .child(server)
                    }))
                    // When both apply, running wins: one marker per tab.
                    .when(dirty && !running && !is_renaming, |d| {
                        d.child(
                            div()
                                .text_color(if dirty_dot_is_neutral() {
                                    Theme::text()
                                } else {
                                    Theme::accent()
                                })
                                .text_size(px(Theme::font_size_ratio(0.85)))
                                .child("•"),
                        )
                    })
                    .children((!is_renaming).then_some(disk_badge).flatten().map(|glyph| {
                        div()
                            .text_color(Theme::text_dim())
                            .text_size(px(Theme::font_size_ratio(0.85)))
                            .child(glyph)
                    }))
                    .when(!is_renaming, |d| {
                        d.child(
                            Button::new(("tab-close", i))
                                .icon(Icon::empty().path(AppIcon::Close.path()))
                                .ghost()
                                .xsmall()
                                .app_tooltip_text(close_tip)
                                .on_click(move |_, window, app| {
                                    // Closing must not also select the tab underneath.
                                    app.stop_propagation();
                                    close_this.update(app, |this, cx| {
                                        this.request_close_tab(i, window, cx);
                                    });
                                }),
                        )
                    })
                    .into_any_element()
            })
            .collect();

        let save_this = this.clone();
        let format_this = this.clone();
        let refresh_this = this.clone();
        let overflow_this = this.clone();
        let save_hint = crate::keybindings::shortcut_hint("file.save");
        let format_hint = crate::keybindings::shortcut_hint("query.format");
        let save_tip: SharedString = if save_hint.is_empty() {
            crate::localization::text("shell.editor.save_tooltip").into()
        } else {
            crate::localization::format(
                "shell.editor.save_shortcut_tooltip",
                &[("shortcut", &save_hint)],
            )
            .into()
        };
        let format_tip: SharedString = if format_hint.is_empty() {
            crate::localization::text("shell.editor.format_tooltip").into()
        } else {
            crate::localization::format(
                "shell.editor.format_shortcut_tooltip",
                &[("shortcut", &format_hint)],
            )
            .into()
        };
        let refresh_tip: SharedString = self
            .active_profile
            .as_ref()
            .and_then(|p| self.datamodel_cache.get(&Self::datamodel_key(p)))
            .map(|cached| {
                crate::localization::format(
                    "shell.editor.refresh_schema_age_tooltip",
                    &[("age", &format_snapshot_age(cached.fetched_at))],
                )
                .into()
            })
            .unwrap_or_else(|| {
                crate::localization::text("shell.editor.refresh_schema_tooltip").into()
            });

        let tab_strip = div()
            .flex()
            .flex_row()
            .h(px(Theme::chrome_bar_height()))
            .bg(Theme::tab_strip_bg())
            .border_b_1()
            .border_color(Theme::border())
            .items_center()
            .px(px(Theme::chrome_inline_px()))
            .gap(px(Theme::scaled(2.0)))
            .font(Theme::ui_font())
            .child(
                div()
                    .id("query-tabs")
                    .track_focus(&self.tab_strip_focus)
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(Theme::scaled(2.0)))
                    .min_w_0()
                    .flex_shrink_1()
                    .overflow_x_scroll()
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                        this.handle_tab_strip_key(event, window, cx);
                        cx.stop_propagation();
                    }))
                    .children(tab_items),
            )
            .child(
                Button::new("new-tab")
                    .icon(
                        Icon::empty()
                            .path(AppIcon::Add.path())
                            .size(px(Theme::toolbar_icon_size())),
                    )
                    .ghost()
                    .xsmall()
                    .app_tooltip_text(crate::localization::text("shell.tabs.new_query_tooltip"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.new_query_tab(cx);
                    })),
            )
            .child(div().flex_1())
            .child({
                let tabs_this = this.clone();
                let filter = self.all_tabs_filter.read(cx).value().to_string();
                let tab_summaries: Vec<(usize, String)> = self
                    .tabs
                    .iter()
                    .enumerate()
                    .filter_map(|(i, t)| {
                        let connection = t
                            .pending_resume_connection
                            .as_ref()
                            .map(|c| c.server.as_str())
                            .or(self.active_profile.as_ref().map(|p| p.name.as_str()));
                        let label = all_tabs_item_label(
                            &t.name,
                            t.dirty,
                            connection,
                            t.active_query_id.is_some(),
                        );
                        all_tabs_matches(&filter, &label).then_some((i, label))
                    })
                    .collect();
                let open = self.all_tabs_open;
                let popover = div()
                    .w(px(Theme::scaled(280.0)))
                    .max_h(px(Theme::scaled(320.0)))
                    .bg(Theme::popup_bg())
                    .border_1()
                    .border_color(Theme::popup_border())
                    .rounded(px(Theme::scaled(6.0)))
                    .p(px(Theme::scaled(6.0)))
                    .occlude()
                    .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                        if this.all_tabs_open {
                            this.all_tabs_open = false;
                            cx.notify();
                        }
                    }))
                    .child(Input::new(&self.all_tabs_filter).appearance(false))
                    .child(
                        div()
                            .mt(px(Theme::scaled(4.0)))
                            .max_h(px(Theme::scaled(260.0)))
                            .overflow_hidden()
                            .flex()
                            .flex_col()
                            .children(tab_summaries.into_iter().map(|(idx, label)| {
                                let pick = tabs_this.clone();
                                div()
                                    .id(("all-tab-row", idx))
                                    .px(px(Theme::scaled(8.0)))
                                    .py(px(Theme::scaled(4.0)))
                                    .rounded(px(Theme::scaled(3.0)))
                                    .cursor_pointer()
                                    .hover(|s| s.bg(Theme::row_hover()))
                                    .child(label)
                                    .on_mouse_down(gpui::MouseButton::Left, move |_, _, app| {
                                        pick.update(app, |this, cx| {
                                            this.all_tabs_open = false;
                                            this.select_tab(idx, cx);
                                        });
                                    })
                            })),
                    );
                div()
                    .relative()
                    .child(
                        Button::new("all-tabs")
                            .ghost()
                            .xsmall()
                            .icon(crate::icons::chevron_down())
                            .app_tooltip_text(crate::localization::text(
                                "shell.tabs.all_tabs_tooltip",
                            ))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.all_tabs_open = !this.all_tabs_open;
                                cx.notify();
                            })),
                    )
                    .when(open, |d| {
                        d.child(
                            deferred(
                                anchored()
                                    .anchor(Anchor::TopRight)
                                    .offset(point(px(0.0), px(Theme::scaled(4.0))))
                                    .snap_to_window_with_margin(px(Theme::scaled(8.0)))
                                    .child(popover),
                            )
                            .with_priority(1),
                        )
                    })
            });

        let command_bar = div()
            .flex()
            .flex_row()
            .h(px(Theme::chrome_bar_height()))
            .bg(Theme::bg())
            .border_b_1()
            .border_color(Theme::border())
            .items_center()
            .px(px(Theme::chrome_inline_px()))
            .gap(px(Theme::scaled(2.0)))
            .font(Theme::ui_font())
            .child(self.render_connection_chip())
            .child(Self::command_bar_icon(
                "qbar-save",
                AppIcon::Save,
                save_tip,
                move |_, window, app| {
                    save_this.update(app, |this, cx| {
                        this.handle_save_sql(&SaveSql, window, cx);
                    });
                },
            ))
            .child(Self::command_bar_icon(
                "qbar-format",
                AppIcon::FormatSql,
                format_tip,
                move |_, window, app| {
                    format_this.update(app, |this, cx| {
                        this.handle_format_active_query(&FormatActiveQuery, window, cx);
                    });
                },
            ))
            .child(Self::command_bar_icon(
                "qbar-refresh-schema",
                AppIcon::RefreshSchema,
                refresh_tip,
                move |_, window, app| {
                    refresh_this.update(app, |this, cx| {
                        this.handle_refresh_object_explorer(&RefreshObjectExplorer, window, cx);
                    });
                },
            ))
            .child({
                let toggles: Vec<(QueryToggle, bool, String)> = QueryToggle::all()
                    .into_iter()
                    .filter(|t| t.is_implemented())
                    .map(|t| (t, self.toolbar.get(t), t.tooltip().to_string()))
                    .collect();
                let plan_unavailable = self.execution_plan_unavailable_reason(
                    crate::execution_plan_view::PlanKind::Actual,
                );
                Button::new("qbar-overflow")
                    .icon(Icon::empty().path(AppIcon::MoreVertical.path()))
                    .ghost()
                    .compact()
                    .tab_stop(false)
                    .with_size(px(Theme::toolbar_icon_size() / 0.75))
                    .app_tooltip_text(crate::localization::text(
                        "shell.query_tools.overflow_tooltip",
                    ))
                    .dropdown_menu(move |menu, _, _| {
                        let plan = overflow_this.clone();
                        let edit = overflow_this.clone();
                        let mut menu = menu
                            .item(
                                PopupMenuItem::new(crate::localization::text(
                                    "shell.query_tools.edit_connection",
                                ))
                                .on_click(
                                    move |_ev, window, app| {
                                        edit.update(app, |this, cx| {
                                            if let Some(id) =
                                                this.active_profile.as_ref().map(|p| p.id.0)
                                            {
                                                this.open_connection_editor(id, window, cx);
                                            }
                                        });
                                    },
                                ),
                            )
                            .separator()
                            .item(explain::gated_plan_menu_item(
                                crate::localization::text("shell.query_tools.capture_actual_plan"),
                                plan_unavailable.clone(),
                                move |_ev, _window, app| {
                                    plan.update(app, |this, cx| {
                                        this.explain_active_query(cx);
                                    });
                                },
                            ));
                        for (t, on, label) in &toggles {
                            let toggle_this = overflow_this.clone();
                            let t = *t;
                            let on = *on;
                            menu =
                                menu.item(PopupMenuItem::new(label.clone()).checked(on).on_click(
                                    move |_ev, _window, app| {
                                        toggle_this.update(app, |this, cx| {
                                            this.apply_query_toggle(t, cx);
                                        });
                                    },
                                ));
                        }
                        menu
                    })
            })
            .child(div().flex_1())
            .child(self.render_run_button(cx))
            .child(self.render_transaction_controls(cx));

        div().flex().flex_col().child(tab_strip).child(command_bar)
    }

    fn apply_query_toggle(&mut self, t: QueryToggle, cx: &mut Context<Self>) {
        let is_resume = t == QueryToggle::QueryResume;
        let is_proc = t == QueryToggle::ProcedureMode;
        let proc_running = is_proc
            && self
                .tabs
                .get(self.active_tab)
                .is_some_and(|tab| tab.active_query_id.is_some());
        let engine_gate = {
            let db = self.active_db_type();
            match t {
                QueryToggle::ProcedureMode => crate::capabilities::procedure_mode(db).reason(),
                QueryToggle::IncludeStatistics => {
                    crate::capabilities::include_statistics(db).reason()
                }
                _ => None,
            }
        };
        let is_disabled = (is_resume
            && self.query_resume_prefs.scope == crate::preferences::QueryResumeScope::None)
            || proc_running
            || engine_gate.is_some();
        if is_disabled {
            if let Some(reason) = engine_gate {
                self.set_status(StatusLevel::Info, reason.to_string());
            }
            return;
        }
        if is_resume {
            self.toggle_resume_override(cx);
            return;
        }
        if is_proc {
            self.toggle_proc_mode(cx);
            return;
        }
        self.toolbar.toggle(t);
        if t == QueryToggle::TrackQueryDifference
            && let Some(tab) = self.tabs.get(self.active_tab)
        {
            let val = self.toolbar.track_query_difference;
            tab.results.update(cx, |r, cx| {
                r.set_track_difference(val, cx);
            });
            if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                tab.diff_mode_enabled = val;
                if tab.resume_effective_enabled {
                    self.flush_tab_resume_metadata(self.active_tab);
                }
            }
        }
        if t == QueryToggle::AiAutocomplete {
            let val = self.toolbar.ai_autocomplete;
            if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                tab.fim_autocomplete_enabled = val;
                if tab.resume_effective_enabled {
                    self.flush_tab_resume_metadata(self.active_tab);
                }
            }
            if !val {
                self.fim_debounce_task = None;
                self.fim_request_token += 1;
                if let Some(tab) = self.tabs.get(self.active_tab) {
                    tab.editor.update(cx, |e, cx| e.hide_fim_suggestion(cx));
                }
            }
        }
        cx.notify();
    }

    fn command_bar_icon(
        id: &'static str,
        icon: AppIcon,
        tooltip: SharedString,
        on_down: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    ) -> impl IntoElement {
        div()
            .id(id)
            .flex()
            .items_center()
            .justify_center()
            .p(px(Theme::scaled(4.0)))
            .cursor(gpui::CursorStyle::PointingHand)
            .text_color(Theme::text_dim())
            .hover(|s| s.text_color(Theme::text()))
            .app_tooltip(move |_window, cx| tooltip_view(tooltip.clone(), cx))
            .child(
                Icon::empty()
                    .path(icon.path())
                    .size(px(Theme::toolbar_icon_size())),
            )
            .on_mouse_down(MouseButton::Left, on_down)
    }

    fn render_production_banner(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let profile = self.active_profile.as_ref()?;
        let store = ConnectionStore::global();
        let now = Instant::now();
        let chip = crate::status_bar::resolve_production_chip(
            Some(profile),
            store,
            self.production_unlock.as_ref(),
            now,
        )?;
        let s = Theme::scaled;
        let unlocked = chip.unlocked_remaining.is_some();
        let banner_text = if let Some(remaining) = chip.unlocked_remaining.as_deref() {
            crate::localization::format(
                "shell.production.unlocked_banner",
                &[("remaining", remaining)],
            )
        } else {
            crate::localization::text("shell.production.read_only_banner")
        };
        let this = cx.entity();
        Some(
            div()
                .h(px(Theme::row_height()))
                .px(px(s(12.0)))
                .flex()
                .items_center()
                .gap(px(s(8.0)))
                .bg(Theme::bg())
                .border_b_1()
                .border_color(Theme::border())
                .text_color(Theme::warning())
                .text_size(px(Theme::font_size_ratio(0.87)))
                .child(
                    div()
                        .w(px(s(6.0)))
                        .h(px(s(6.0)))
                        .rounded(px(s(3.0)))
                        .bg(Theme::warning()),
                )
                .child(banner_text)
                .child(div().flex_1())
                .when(!unlocked, |row| {
                    row.child(
                        Button::new("prod-unlock-1")
                            .ghost()
                            .small()
                            .label(crate::localization::text("shell.production.unlock_one_minute"))
                            .on_click({
                                let this = this.clone();
                                move |_, _, cx| {
                                    this.update(cx, |view, cx| {
                                        view.unlock_production_writes(
                                            sqlly_intel::query::safety::UnlockWindow::OneMinute,
                                            cx,
                                        );
                                    });
                                }
                            }),
                    )
                    .child(
                        Button::new("prod-unlock-5")
                            .ghost()
                            .small()
                            .label(crate::localization::text("shell.production.unlock_five_minutes"))
                            .on_click({
                                let this = this.clone();
                                move |_, _, cx| {
                                    this.update(cx, |view, cx| {
                                        view.unlock_production_writes(
                                            sqlly_intel::query::safety::UnlockWindow::FiveMinutes,
                                            cx,
                                        );
                                    });
                                }
                            }),
                    )
                    .child(
                        Button::new("prod-unlock-15")
                            .ghost()
                            .small()
                            .label(crate::localization::text("shell.production.unlock_fifteen_minutes"))
                            .on_click({
                                let this = this.clone();
                                move |_, _, cx| {
                                    this.update(cx, |view, cx| {
                                        view.unlock_production_writes(
                                            sqlly_intel::query::safety::UnlockWindow::FifteenMinutes,
                                            cx,
                                        );
                                    });
                                }
                            }),
                    )
                })
                .when(unlocked, |row| {
                    row.child(
                        Button::new("prod-lock")
                            .ghost()
                            .small()
                            .label(crate::localization::text("shell.production.lock_now"))
                            .on_click({
                                let this = this.clone();
                                move |_, _, cx| {
                                    this.update(cx, |view, cx| view.lock_production_writes(cx));
                                }
                            }),
                    )
                })
                .into_any_element(),
        )
    }

    fn render_connection_chip(&self) -> impl IntoElement + use<> {
        let connected = self.active_profile.is_some();
        let chip = ConnectionBreadcrumb::from_names(
            self.active_profile.as_ref().map(|p| p.name.as_str()),
            self.active_profile
                .as_ref()
                .and_then(|p| p.database.as_deref()),
        );
        let tip: SharedString = chip.chip_label_with_state(connected).into();
        div()
            .id("connection-chip")
            .flex()
            .flex_row()
            .items_center()
            .justify_start()
            .app_tooltip(move |_window, cx| tooltip_view(tip.clone(), cx))
            .child(self.render_qbar_connection_pickers())
    }

    /// Run Statements picker: tick statements to execute as one reviewed batch.
    fn render_run_picker_dialog(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let overlay = self.overlays.run_picker.as_ref()?;
        let s = Theme::scaled;
        let summary = overlay.model.summary();
        let selected_count = overlay.model.rows.iter().filter(|r| r.checked).count();
        let run_label = if selected_count == 0 {
            crate::localization::text("shell.run_picker.run_without_selection")
        } else {
            let key = if selected_count == 1 {
                "shell.run_picker.run_singular"
            } else {
                "shell.run_picker.run_plural"
            };
            crate::localization::format(key, &[("count", &selected_count.to_string())])
        };
        let this = cx.entity();
        let mut rows = div().flex().flex_col().gap(px(s(4.0)));
        for (i, row) in overlay.model.rows.iter().enumerate() {
            let kind_color = if row.kind.is_write() {
                Theme::warning()
            } else {
                Theme::text_dim()
            };
            let preview = row.preview.clone();
            let kind_label = row.kind.label();
            rows = rows.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(8.0)))
                    .child({
                        let pick = this.clone();
                        Checkbox::new(("run-pick", i))
                            .checked(row.checked)
                            .label(preview)
                            .tab_stop(true)
                            .on_click(move |checked: &bool, _window, app| {
                                pick.update(app, |this, cx| {
                                    if let Some(overlay) = &mut this.overlays.run_picker {
                                        overlay.model.set_checked(i, *checked);
                                    }
                                    cx.notify();
                                });
                            })
                    })
                    .child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.75)))
                            .text_color(kind_color)
                            .child(kind_label),
                    ),
            );
        }
        Some(
            Dialog::new("run-picker-dialog", |this: &mut Self| {
                this.dialog_frames.get("run-picker-dialog", || {
                    DialogFrame::new_auto(Theme::scaled(560.0))
                })
            })
            .title(crate::localization::text("shell.run_picker.title"))
            .on_close(|this, _window, cx| {
                this.overlays.run_picker = None;
                cx.notify();
            })
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .max_h(px(s(320.0)))
                    .overflow_hidden()
                    .child(rows),
            )
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.82)))
                    .text_color(Theme::text_dim())
                    .child(summary),
            )
            .leading_button({
                let pick = this.clone();
                Button::new("run-pick-all")
                    .label(crate::localization::text("shell.run_picker.select_all"))
                    .on_click(move |_, _window, app| {
                        pick.update(app, |this, cx| {
                            if let Some(overlay) = &mut this.overlays.run_picker {
                                overlay.model.set_all(true);
                            }
                            cx.notify();
                        });
                    })
            })
            .leading_button({
                let pick = this.clone();
                Button::new("run-pick-none")
                    .label(crate::localization::text("shell.run_picker.select_none"))
                    .on_click(move |_, _window, app| {
                        pick.update(app, |this, cx| {
                            if let Some(overlay) = &mut this.overlays.run_picker {
                                overlay.model.set_all(false);
                            }
                            cx.notify();
                        });
                    })
            })
            .button({
                let pick = this.clone();
                Button::new("run-pick-confirm")
                    .label(run_label)
                    .primary()
                    .on_click(move |_, _window, app| {
                        pick.update(app, |this, cx| {
                            this.confirm_run_picker(cx);
                        });
                    })
            })
            .build(self, window, cx)
            .into_any_element(),
        )
    }

    fn confirm_run_picker(&mut self, cx: &mut Context<Self>) {
        let Some(overlay) = self.overlays.run_picker.take() else {
            return;
        };
        if overlay.model.selected_spans().is_empty() {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.run_picker.selection_required"),
            );
            self.overlays.run_picker = Some(overlay);
            cx.notify();
            return;
        };
        let tab_idx = overlay.tab_idx;
        let Some(tab) = self.tabs.get(tab_idx) else {
            return;
        };
        let sql = tab.editor.read(cx).text().to_string();
        let Some(batch) = overlay.model.batch_sql(&sql) else {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.run_picker.selection_required"),
            );
            self.overlays.run_picker = Some(overlay);
            cx.notify();
            return;
        };
        let start = overlay.model.first_selected_start().unwrap_or(0);
        self.pending.run_statement_selection = Some((batch, start));
        self.execute_query_with_origin(tab_idx, execute::ExecuteOrigin::UserRun, cx);
    }

    /// The reviewed rename dialog: name input, live dialect-SQL preview, and
    /// a script-to-tab confirm (nothing executes from the dialog).
    fn render_rename_dialog(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        use gpui_component::input::{Input, InputEvent, InputState};
        self.overlays.rename_dialog.as_ref()?;
        let s = Theme::scaled;
        if self
            .overlays
            .rename_dialog
            .as_ref()
            .is_some_and(|dialog| dialog.input.is_none())
        {
            let input = cx.new(|cx| {
                InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                    window,
                    cx,
                    "shell.rename.new_name_placeholder",
                ))
            });
            let subscription = cx.subscribe(
                &input,
                |this: &mut Self, state, event: &InputEvent, cx| match event {
                    InputEvent::Change => {
                        let value = state.read(cx).value().to_string();
                        if let Some(dialog) = &mut this.overlays.rename_dialog {
                            dialog.value = value;
                            dialog.error = None;
                        }
                        cx.notify();
                    }
                    InputEvent::PressEnter { .. } => this.confirm_rename(cx),
                    _ => {}
                },
            );
            input.update(cx, |state, cx| state.focus(window, cx));
            if let Some(dialog) = &mut self.overlays.rename_dialog {
                dialog.input = Some(input);
                dialog._subscription = Some(subscription);
            }
        }
        let dialog = self.overlays.rename_dialog.as_ref()?;
        let input = dialog.input.clone()?;
        let qt =
            crate::table_actions::QualifiedTable::new(&dialog.name).with_schema(&dialog.schema);
        let preview = match crate::table_actions::rename_sql_with_name(
            dialog.db,
            dialog.kind,
            &qt,
            &dialog.value,
        ) {
            Ok(sql) => sql,
            Err(_) => crate::table_actions::rename_sql_for(dialog.db, dialog.kind, &qt),
        };
        let error = dialog.error.clone().or_else(|| {
            if dialog.value.trim().is_empty() {
                None
            } else {
                crate::table_actions::rename_identifier_error(&dialog.value)
            }
        });
        let title = crate::localization::format("shell.rename.title", &[("name", &dialog.name)]);
        Some(
            Dialog::new("rename-dialog", |this: &mut Self| {
                this.dialog_frames.get("rename-dialog", || {
                    DialogFrame::new_auto(Theme::scaled(560.0))
                })
            })
            .title(title)
            .on_close(|this, _window, cx| {
                this.overlays.rename_dialog = None;
                cx.notify();
            })
            .child(Input::new(&input))
            .children_body(error.map(|error| {
                div()
                    .text_size(px(Theme::font_size_ratio(0.82)))
                    .text_color(Theme::error())
                    .child(error)
            }))
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.82)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text("shell.rename.preview_notice")),
            )
            .child(
                div()
                    .max_h(px(s(180.0)))
                    .overflow_hidden()
                    .p(px(s(8.0)))
                    .rounded_md()
                    .bg(Theme::sidebar_bg())
                    .font_family(Theme::editor_font_family())
                    .text_size(px(Theme::font_size_ratio(0.82)))
                    .text_color(Theme::text())
                    .whitespace_normal()
                    .child(preview),
            )
            .button(
                Button::new("rename-confirm")
                    .label(crate::localization::text("shell.rename.open_script_button"))
                    .xsmall()
                    .on_click(cx.listener(|this, _ev, _w, cx| this.confirm_rename(cx))),
            )
            .build(self, window, cx)
            .into_any_element(),
        )
    }

    /// The New / Rename Pin Group dialog: one name field, refused names
    /// explained inline.
    fn render_pin_group_dialog(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        use gpui_component::input::{Input, InputEvent, InputState};
        let needs_input = self.overlays.pin_group_dialog.as_ref()?.input.is_none();
        if needs_input {
            let initial = self
                .overlays
                .pin_group_dialog
                .as_ref()
                .map(|d| d.value.clone())
                .unwrap_or_default();
            let input = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(crate::localization::input_placeholder(
                        window,
                        cx,
                        "explorer.schema_tree.pin_group_name_placeholder",
                    ))
                    .default_value(initial)
            });
            let subscription = cx.subscribe(
                &input,
                |this: &mut Self, state, event: &InputEvent, cx| match event {
                    InputEvent::Change => {
                        let value = state.read(cx).value().to_string();
                        if let Some(dialog) = &mut this.overlays.pin_group_dialog {
                            dialog.value = value;
                            dialog.error = None;
                        }
                        cx.notify();
                    }
                    InputEvent::PressEnter { .. } => this.confirm_pin_group_dialog(cx),
                    _ => {}
                },
            );
            input.update(cx, |state, cx| state.focus(window, cx));
            if let Some(dialog) = &mut self.overlays.pin_group_dialog {
                dialog.input = Some(input);
                dialog._subscription = Some(subscription);
            }
        }
        let dialog = self.overlays.pin_group_dialog.as_ref()?;
        let input = dialog.input.clone()?;
        let (title, button) = match &dialog.rename {
            Some((_, name)) => (
                crate::localization::format(
                    "explorer.schema_tree.pin_group_rename_title",
                    &[("name", name)],
                ),
                crate::localization::text("explorer.schema_tree.pin_group_rename_button"),
            ),
            None => (
                crate::localization::text("explorer.schema_tree.pin_group_create_title"),
                crate::localization::text("explorer.schema_tree.pin_group_create_button"),
            ),
        };
        let error = dialog.error.clone();
        Some(
            Dialog::new("pin-group-dialog", |this: &mut Self| {
                this.dialog_frames.get("pin-group-dialog", || {
                    DialogFrame::new_auto(Theme::scaled(420.0))
                })
            })
            .title(title)
            .on_close(|this, _window, cx| {
                this.overlays.pin_group_dialog = None;
                cx.notify();
            })
            .child(Input::new(&input))
            .children_body(error.map(|error| {
                div()
                    .text_size(px(Theme::font_size_ratio(0.82)))
                    .text_color(Theme::error())
                    .child(error)
            }))
            .button(
                Button::new("pin-group-confirm")
                    .label(button)
                    .xsmall()
                    .on_click(cx.listener(|this, _ev, _w, cx| this.confirm_pin_group_dialog(cx))),
            )
            .build(self, window, cx)
            .into_any_element(),
        )
    }

    /// Create or rename the pin group; a refused name stays in the dialog
    /// with its reason.
    fn confirm_pin_group_dialog(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = &self.overlays.pin_group_dialog else {
            return;
        };
        let (server_id, rename, pin, value) = (
            dialog.server_id,
            dialog.rename.clone(),
            dialog.pin.clone(),
            dialog.value.clone(),
        );
        let result = self.sidebar.update(cx, |sidebar, cx| match rename {
            Some((group, _)) => sidebar.rename_pin_group(group, &value, cx),
            None => sidebar.create_pin_group(server_id, &value, pin, cx),
        });
        match result {
            Ok(()) => self.overlays.pin_group_dialog = None,
            Err(reason) => {
                if let Some(dialog) = &mut self.overlays.pin_group_dialog {
                    dialog.error = Some(reason);
                }
            }
        }
        cx.notify();
    }

    /// Validate the typed name, generate the dialect rename script with the
    /// real name, and open it in a new tab on the object's own connection.
    fn confirm_rename(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = &self.overlays.rename_dialog else {
            return;
        };
        let qt =
            crate::table_actions::QualifiedTable::new(&dialog.name).with_schema(&dialog.schema);
        match crate::table_actions::rename_sql_with_name(dialog.db, dialog.kind, &qt, &dialog.value)
        {
            Ok(sql) => {
                let server_id = dialog.server_id;
                let database = dialog.database.clone();
                self.overlays.rename_dialog = None;
                let target =
                    self.resolve_explorer_target(server_id.as_ref(), database.as_deref(), cx);
                self.with_explorer_target_active(target, cx, move |app, cx| {
                    app.new_tab(cx, Some(sql));
                    app.set_status(
                        StatusLevel::Info,
                        crate::localization::text("shell.rename.script_opened_status"),
                    );
                    cx.notify();
                });
            }
            Err(error) => {
                if let Some(dialog) = &mut self.overlays.rename_dialog {
                    dialog.error = Some(error);
                }
                cx.notify();
            }
        }
    }

    /// The right-click tab menu: pin, close verbs, path actions, and a
    /// Recent Files section. Rendered as an app-level overlay so it can
    /// outgrow the tab strip.
    fn render_tab_context_menu(
        &self,
        window_id: gpui::WindowId,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let (tab_idx, position) = *self.tab_context_menu.as_ref()?;
        let tab = self.tabs.get(tab_idx)?;
        let s = Theme::scaled;
        let pinned = tab.pinned;
        let file_path = tab.file_path.clone();
        let has_right = tab_idx + 1 < self.tabs.len();

        type MenuAction = std::rc::Rc<dyn Fn(&mut AppView, &mut Window, &mut Context<AppView>)>;
        let row = |id: &'static str,
                   label: String,
                   enabled: bool,
                   cx: &mut Context<Self>,
                   on_click: MenuAction|
         -> gpui::AnyElement {
            let mut el = div()
                .id(id)
                .px(px(s(12.0)))
                .py(px(s(4.0)))
                .text_size(px(Theme::font_size_ratio(0.92)))
                .text_color(if enabled {
                    Theme::text()
                } else {
                    Theme::text_dim()
                });
            if enabled {
                el = el
                    .cursor_pointer()
                    .hover(|style| style.bg(Theme::row_hover()))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _ev, window, cx| {
                            this.tab_context_menu = None;
                            on_click(this, window, cx);
                            cx.notify();
                        }),
                    );
            }
            el.child(label).into_any_element()
        };
        let separator = |sep_key: u32| {
            div()
                .id(("tab-menu-sep", sep_key))
                .h(px(1.0))
                .my(px(s(3.0)))
                .bg(Theme::border())
                .into_any_element()
        };

        let mut rows: Vec<gpui::AnyElement> = Vec::new();
        rows.push(row(
            "tab-menu-pin",
            if pinned {
                crate::localization::text("tabs.context.unpin")
            } else {
                crate::localization::text("tabs.context.pin")
            },
            true,
            cx,
            std::rc::Rc::new(move |this, _window, cx| this.toggle_tab_pin(tab_idx, cx)),
        ));
        rows.push(row(
            "tab-menu-rename",
            crate::localization::text("tabs.context.rename"),
            true,
            cx,
            std::rc::Rc::new(move |this, _window, cx| this.begin_tab_rename(tab_idx, cx)),
        ));
        // Move the tab (text, caret, connection, results snapshot) to
        // another workbench window. Native-only: the browser has one window.
        #[cfg(not(target_arch = "wasm32"))]
        {
            rows.push(separator(3));
            rows.push(row(
                "tab-menu-move-new-window",
                crate::localization::text("tabs.context.move_new_window"),
                true,
                cx,
                std::rc::Rc::new(move |this, _window, cx| {
                    this.move_tab_to_window(tab_idx, None, cx)
                }),
            ));
            for (n, (handle, label)) in self
                .move_tab_window_targets(window_id, cx)
                .into_iter()
                .enumerate()
            {
                let el = div()
                    .id(("tab-menu-move-window", n))
                    .px(px(s(12.0)))
                    .py(px(s(4.0)))
                    .text_size(px(Theme::font_size_ratio(0.92)))
                    .text_color(Theme::text())
                    .truncate()
                    .cursor_pointer()
                    .hover(|style| style.bg(Theme::row_hover()))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _ev, _window, cx| {
                            this.tab_context_menu = None;
                            this.move_tab_to_window(tab_idx, Some(handle), cx);
                            cx.notify();
                        }),
                    )
                    .child(label);
                rows.push(el.into_any_element());
            }
        }
        #[cfg(target_arch = "wasm32")]
        let _ = window_id;
        rows.push(separator(0));
        rows.push(row(
            "tab-menu-close",
            crate::localization::text("tabs.context.close"),
            !pinned,
            cx,
            std::rc::Rc::new(move |this, window, cx| this.request_close_tab(tab_idx, window, cx)),
        ));
        rows.push(row(
            "tab-menu-close-others",
            crate::localization::text("tabs.context.close_others"),
            self.tabs.len() > 1,
            cx,
            std::rc::Rc::new(move |this, window, cx| {
                this.close_tabs_where_prompting(move |i, _| i == tab_idx, window, cx)
            }),
        ));
        rows.push(row(
            "tab-menu-close-right",
            crate::localization::text("tabs.context.close_right"),
            has_right,
            cx,
            std::rc::Rc::new(move |this, window, cx| {
                this.close_tabs_where_prompting(move |i, _| i <= tab_idx, window, cx)
            }),
        ));
        rows.push(row(
            "tab-menu-close-all",
            crate::localization::text("tabs.context.close_all"),
            true,
            cx,
            std::rc::Rc::new(|this, window, cx| {
                this.close_tabs_where_prompting(|_, _| false, window, cx)
            }),
        ));
        if let Some(path) = file_path {
            rows.push(separator(1));
            let copy_path = path.clone();
            rows.push(row(
                "tab-menu-copy-path",
                crate::localization::text("tabs.context.copy_path"),
                true,
                cx,
                std::rc::Rc::new(move |this, _window, cx| {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(copy_path.clone()));
                    this.set_status(
                        StatusLevel::Info,
                        crate::localization::text("tabs.context.path_copied_status"),
                    );
                }),
            ));
            #[cfg(target_os = "macos")]
            let reveal_label = crate::localization::text("tabs.context.reveal_finder");
            #[cfg(not(target_os = "macos"))]
            let reveal_label = crate::localization::text("tabs.context.reveal_file_manager");
            rows.push(row(
                "tab-menu-reveal",
                reveal_label.to_string(),
                true,
                cx,
                std::rc::Rc::new(move |_this, _window, _cx| {
                    reveal_path_in_file_manager(&path);
                }),
            ));
        }
        if !self.recent_files.is_empty() {
            rows.push(separator(2));
            rows.push(
                div()
                    .px(px(s(12.0)))
                    .py(px(s(3.0)))
                    .text_size(px(Theme::font_size_ratio(0.78)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text("tabs.context.recent_files"))
                    .into_any_element(),
            );
            for (n, path) in self.recent_files.iter().take(8).enumerate() {
                let open_path = path.clone();
                let label = std::path::Path::new(path)
                    .file_name()
                    .map(|f| f.to_string_lossy().to_string())
                    .unwrap_or_else(|| path.clone());
                let mut el = div()
                    .id(("tab-menu-recent", n))
                    .px(px(s(12.0)))
                    .py(px(s(4.0)))
                    .text_size(px(Theme::font_size_ratio(0.92)))
                    .text_color(Theme::text())
                    .truncate()
                    .cursor_pointer()
                    .hover(|style| style.bg(Theme::row_hover()));
                el = el.on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _ev, _window, cx| {
                        this.tab_context_menu = None;
                        this.open_recent_file(open_path.clone(), cx);
                        cx.notify();
                    }),
                );
                rows.push(el.child(label).into_any_element());
            }
        }

        Some(
            div()
                .id("tab-context-menu-backdrop")
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .occlude()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _ev, _window, cx| {
                        this.tab_context_menu = None;
                        cx.notify();
                    }),
                )
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(|this, _ev, _window, cx| {
                        this.tab_context_menu = None;
                        cx.notify();
                    }),
                )
                .child(
                    div()
                        .absolute()
                        .left(position.x)
                        .top(position.y)
                        .min_w(px(s(220.0)))
                        .py(px(s(4.0)))
                        .rounded_md()
                        .border_1()
                        .border_color(Theme::popup_border())
                        .bg(Theme::popup_bg())
                        .font(Theme::ui_font())
                        .occlude()
                        .flex()
                        .flex_col()
                        .children(rows),
                )
                .into_any_element(),
        )
    }

    /// Keep the connection chip's server/database selects in sync with the
    /// live profile list and the active database.
    fn sync_qbar_selects(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let server_names: Vec<String> = self
            .sidebar
            .read(cx)
            .connections()
            .iter()
            .map(|p| p.name.clone())
            .collect();
        let server_selected = self
            .active_profile
            .as_ref()
            .map(|p| p.name.clone())
            .filter(|s| !s.is_empty());

        let mut db_names = self.sidebar.update(cx, |s, cx| s.database_names(cx));
        if let Some(active_db) = self
            .active_profile
            .as_ref()
            .and_then(|p| p.database.clone())
            .filter(|s| !s.is_empty())
            && !db_names.contains(&active_db)
        {
            db_names.insert(0, active_db);
        }
        let database_selected = self
            .active_profile
            .as_ref()
            .and_then(|p| p.database.clone())
            .filter(|s| !s.is_empty());

        if self.qbar_server_items != server_names {
            self.qbar_server_items = server_names.clone();
            let items = server_names;
            self.qbar_server_select
                .update(cx, |state, cx| state.set_items(items, window, cx));
            self.qbar_server_selected = None;
        }
        if self.qbar_server_selected != server_selected {
            self.qbar_server_selected = server_selected.clone();
            self.qbar_server_select
                .update(cx, |state, cx| match &server_selected {
                    Some(value) => state.set_selected_value(value, window, cx),
                    None => state.set_selected_index(None, window, cx),
                });
        }
        if self.qbar_database_items != db_names {
            self.qbar_database_items = db_names.clone();
            let items = db_names;
            self.qbar_database_select
                .update(cx, |state, cx| state.set_items(items, window, cx));
            self.qbar_database_selected = None;
        }
        if self.qbar_database_selected != database_selected {
            self.qbar_database_selected = database_selected.clone();
            self.qbar_database_select
                .update(cx, |state, cx| match &database_selected {
                    Some(value) => state.set_selected_value(value, window, cx),
                    None => state.set_selected_index(None, window, cx),
                });
        }
    }
}

impl AppView {
    /// Prepends the custom titlebar (brand + layout toggles; menus and window
    /// controls on Windows/Linux) for render paths that don't go through `main`.
    fn wrap_with_titlebar(&self, element: AnyElement) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .size_full()
            .child(self.menu_bar_element())
            .child(element)
            .into_any_element()
    }
}

impl AppView {
    /// One pane's full stack for `tab_idx`: the editor (with its per-tab DDL
    /// and history side panels), the procedure strip, the AI summary bar, and
    /// the results pane behind its divider. DDL viewer tabs render the viewer
    /// instead. Shared by the single-pane layout and both split panes;
    /// `pane_tag` keeps per-pane element ids distinct.
    /// The frame-cached region for `kind`, created on first use, marked as
    /// shown by the current frame (see `chrome_region.rs`).
    fn chrome_region(
        &mut self,
        kind: crate::app::chrome_region::ChromeRegionKind,
        cx: &mut Context<Self>,
    ) -> Entity<crate::app::chrome_region::ChromeRegion> {
        use crate::app::chrome_region::{ChromeRegion, ChromeRegionKind};
        let slot = match kind {
            ChromeRegionKind::TabBar => &mut self.tab_bar_region,
            ChromeRegionKind::StatusBar => &mut self.status_bar_region,
        };
        let region = match slot {
            Some(region) => region.clone(),
            None => {
                let app = cx.entity();
                let region = cx.new(|cx| ChromeRegion::new(kind, &app, cx));
                *slot = Some(region.clone());
                region
            }
        };
        let frame = self.chrome_frame;
        region.update(cx, |region, _| region.placed(frame));
        region
    }

    /// Tab strip + command bar, drawn from the frame cache. The slot is
    /// sized here (two chrome bars) because a cached view skips its own
    /// layout; `render_tab_bar` builds exactly those two rows.
    fn tab_bar_region_element(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let region = self.chrome_region(crate::app::chrome_region::ChromeRegionKind::TabBar, cx);
        div()
            .w_full()
            .flex_none()
            .h(px(2.0 * Theme::chrome_bar_height()))
            .child(region.cached(gpui::StyleRefinement::default().size_full()))
            .into_any_element()
    }

    /// The status strip, drawn from the frame cache (see
    /// [`Self::tab_bar_region_element`] for the sizing rule).
    fn status_bar_region_element(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let region = self.chrome_region(crate::app::chrome_region::ChromeRegionKind::StatusBar, cx);
        div()
            .w_full()
            .flex_none()
            .h(px(crate::status_bar::strip_height()))
            .child(region.cached(gpui::StyleRefinement::default().size_full()))
            .into_any_element()
    }

    /// The status bar (~26px, full width): left chrome + connection, dim
    /// center status, Search / zoom% / version on the right. Rendered by the
    /// frame-cached status region, not by `render` directly.
    pub(crate) fn render_status_bar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let log_visible = self.log_console.read(cx).is_visible();
        let log_badge = crate::log_console::hidden_badge_count(
            log_visible,
            self.log_console.read(cx).entry_count(),
        );
        let status = self.status_message.clone();
        let status_level = self.status_level;
        let mut status_state = crate::status_bar::resolve_with_identities(
            self.active_profile.as_ref(),
            self.sidebar.read(cx).entities(),
        );
        // Refine the route with the live session's facts (actual relay
        // endpoint + bridge name) when a relay session is open for the active
        // profile; the resolver alone only knows the profile's advisory hint.
        status_state.route = crate::status_bar::resolve_route(
            self.active_profile.as_ref(),
            self.active_profile
                .as_ref()
                .and_then(|p| self.remote_route_facts.get(&p.id)),
        );
        let now = Instant::now();
        let production_chip = crate::status_bar::resolve_production_chip(
            self.active_profile.as_ref(),
            ConnectionStore::global(),
            self.production_unlock.as_ref(),
            now,
        );
        status_state = status_state.with_production(production_chip);
        let transaction_chip = crate::status_bar::resolve_transaction_chip(self.active_tab());
        status_state = status_state.with_transaction(transaction_chip);
        let read_only_unlock = self
            .active_profile
            .as_ref()
            .and_then(|p| self.read_only_unlocks.get(&p.id.0.to_string()));
        let read_only_chip = crate::status_bar::resolve_read_only_chip(
            self.active_profile.as_ref(),
            read_only_unlock,
            now,
        );
        status_state = status_state.with_read_only(read_only_chip);

        // A selected profile whose server failed to connect must not read as a
        // healthy connection: consult the schema tree's load state (the source
        // of its "failed" badge) so the status circle goes red instead of green.
        let connection_healthy = self
            .active_profile
            .as_ref()
            .is_none_or(|p| !self.sidebar.read(cx).server_load_failed(p.id.0, cx));

        // Status bar (~26px, full width): left chrome + connection,
        // dim center status, Search / zoom% / version on the right.
        div()
            .flex()
            .flex_row()
            .h(px(crate::status_bar::strip_height()))
            .bg(Theme::bg())
            .border_t_1()
            .border_color(Theme::border())
            .px(px(Theme::scaled(12.0)))
            .items_center()
            .gap(px(Theme::scaled(12.0)))
            .text_size(px(crate::status_bar::bar_font_size()))
            .text_color(Theme::text_dim())
            .child(
                // `min_w_0` lets the cluster shrink below its content
                // width so a long status message truncates with an
                // ellipsis instead of stretching the bar onto extra
                // lines; the chrome and tagline keep their intrinsic
                // minimum, so only the message gives way.
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .min_w_0()
                    .gap(px(Theme::scaled(6.0)))
                    .child(crate::status_bar::render_left(
                        &status_state,
                        connection_healthy,
                        log_badge,
                        self.account_session.is_some(),
                    ))
                    .children(crate::status_bar::render_tagline())
                    .child(Self::render_status_message(status_level, status, cx)),
            )
            .child(crate::status_bar::render_badges(&status_state))
            .child(div().flex_1())
            .child(self.render_task_chip(cx))
            .child(self.render_notification_bell(cx))
            .child({
                let zoom_control = self.render_zoom_control(cx).into_any_element();
                crate::status_bar::render_right_cluster(
                    crate::editor_zoom::zoom_to_percent(self.editor_zoom),
                    self.zoom_popover_open,
                    zoom_control,
                    self.update_available.as_ref().map(|u| u.latest.as_str()),
                    self.tabs
                        .get(self.active_tab)
                        .and_then(|t| t.editor.read(cx).vim_mode_label()),
                )
            })
            .into_any_element()
    }

    /// A child view drawn from gpui's frame cache: `render`, layout and
    /// prepaint are skipped on frames where the view's own entity (or a
    /// descendant's) was not notified, which is every frame of a results-grid
    /// scroll. `live` opts out for the frames an in-element animation needs.
    fn frame_cached<V: Render>(view: Entity<V>, live: bool) -> AnyElement {
        if live {
            view.into_any_element()
        } else {
            view.cached(gpui::StyleRefinement::default().size_full())
                .into_any_element()
        }
    }

    /// The in-window menu bar from the frame cache; it notifies itself when
    /// its toggles or menus change.
    fn menu_bar_element(&self) -> AnyElement {
        self.window_menu_bar
            .clone()
            .cached(crate::window_menu_bar::WindowMenuBar::cached_root_style())
            .into_any_element()
    }

    fn render_pane_stack(
        &mut self,
        tab_idx: usize,
        pane_tag: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let s = Theme::scaled;
        let Some(tab) = self.tabs.get(tab_idx) else {
            return div().flex_1().into_any_element();
        };
        let editor = tab.editor.clone();
        let results = tab.results.clone();
        let ddl_view = tab.ddl_view.clone();
        let notebook = tab.notebook.clone();
        let ddl_split = keep_split_out_panel(tab.ddl_split);
        let ddl_panel = tab.ddl_panel.clone().filter(|_| ddl_split);
        let ddl_panel_width = tab.ddl_panel_width;
        #[cfg(not(target_arch = "wasm32"))]
        let history_split = keep_split_out_panel(tab.history_split);
        #[cfg(not(target_arch = "wasm32"))]
        let (history_panel, history_panel_width) = (
            tab.history_panel.clone().filter(|_| history_split),
            tab.history_panel_width,
        );
        let ai_summary_text = tab
            .ai_summary
            .summary(crate::query_ai_summary::QueryAISummaryKind::Long)
            .map(|s| s.to_string());
        let results_detached = tab.results_detached;
        let maximized = self.results_pane == crate::results_pane_state::ResultsPaneState::Maximized;

        if let Some(ddl_view) = ddl_view {
            // Read-only DDL tab: the viewer replaces the editor+results stack.
            return div()
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .min_w_0()
                .child(div().flex_1().min_h_0().child(ddl_view))
                .into_any_element();
        }

        if let Some(notebook) = notebook {
            return div()
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .min_w_0()
                .child(
                    div()
                        .id("notebook-scroll")
                        .flex_1()
                        .min_h_0()
                        .overflow_scroll()
                        .child(notebook),
                )
                .into_any_element();
        }

        #[cfg(not(target_arch = "wasm32"))]
        results.update(cx, |rv, _cx| {
            rv.bind_output_panels(history_panel.clone(), ddl_panel.clone());
            rv.set_pane_maximized(maximized);
        });
        #[cfg(target_arch = "wasm32")]
        results.update(cx, |rv, _cx| {
            rv.bind_output_panels(ddl_panel.clone());
            rv.set_pane_maximized(maximized);
        });

        // --- Results divider (horizontal PaneDivider between editor and results) ---
        let this = cx.entity();
        let results_handle = PaneDivider::horizontal(
            format!("results-divider-{pane_tag}"),
            CollapseDirection::Down,
        )
        .collapsed(!self.results_visible())
        .label(crate::localization::text("shell.panes.results"))
        .on_drag_start({
            let this = this.clone();
            move |pos: f32, _window, app| {
                this.update(app, |this, cx| {
                    this.is_resizing_results = true;
                    this.drag_start_pos = px(pos);
                    this.drag_start_results_height = this.results_height;
                    cx.notify();
                });
            }
        })
        .on_toggle({
            let this = this.clone();
            move |_toggle, _window, app| {
                this.update(app, |this, cx| {
                    this.results_pane = this.results_pane.toggled();
                    this.save_workspace_session(cx);
                    cx.notify();
                });
            }
        });

        // AI summary strip: rendered only when summaries are enabled. The zoom
        // control that used to share this band moved to the status bar, so when
        // summaries are off the whole strip — and its hairline — disappears
        // instead of persisting as a near-empty bordered band.
        let summary_display_width = self.ai_summary_display_width(tab_idx, cx);
        let ai_summary_bar = if maximized {
            None
        } else {
            self.ai_summary_enabled.then(|| {
                // Single-line summary label, truncated with an ellipsis. When the
                // text is estimated to clip, hovering shows the full text in a
                // wrapping tooltip.
                // Flatten any block-level markdown first: the row is one line
                // tall and overflow-clipped, so a heading/list/fenced answer
                // would otherwise paint only its first block. Word-budgeting
                // makes block answers rare, not impossible.
                let text = crate::query_ai_summary::flatten_markdown_to_inline(
                    &ai_summary_text
                        .unwrap_or_else(|| crate::localization::text("shell.ai_summary.empty")),
                );
                let clips = summary_display_width
                    .is_some_and(|w| crate::query_ai_summary::estimated_text_width(&text) > w);
                // The summary text is selectable so it can be copied. It rides a
                // gpui-component `TextView` (the app's read-only selectable-text
                // surface); the summary is word-budgeted to the available width, so
                // it stays about one line, and the fixed height + clipped overflow
                // keep the status bar from growing if it ever runs long. The full
                // text still shows in a hover tooltip when it clips.
                let full: SharedString = text.clone().into();
                let label = div()
                    .id(("ai-summary-text", tab_idx))
                    .flex_1()
                    .min_w(px(0.0))
                    .h(px(Theme::row_height()))
                    .overflow_hidden()
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text_dim())
                    .child(
                        gpui_component::text::TextView::markdown(
                            SharedString::from(format!("ai-summary-text-md-{pane_tag}")),
                            text,
                        )
                        .selectable(true),
                    );
                let label = if clips {
                    label.app_tooltip_wrapping(full.clone()).into_any_element()
                } else {
                    label.into_any_element()
                };
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(8.0)))
                    .px(px(s(12.0)))
                    .py(px(s(4.0)))
                    .bg(Theme::bg())
                    // No bottom border: the results pane divider that follows
                    // paints its own 1px hairline, and stacking both showed
                    // as a single 2px rule under the strip.
                    .child(label)
                    .child(
                        div()
                            .id(("ai-summary-update", tab_idx))
                            .px(px(s(8.0)))
                            .h(px(Theme::row_height()))
                            .flex()
                            .items_center()
                            .cursor(gpui::CursorStyle::PointingHand)
                            .rounded(px(s(3.0)))
                            .text_size(px(Theme::font_size()))
                            .text_color(Theme::text_dim())
                            .hover(|s| s.bg(Theme::border()))
                            .child(crate::localization::text("shell.ai_summary.update_button"))
                            .on_mouse_down(
                                gpui::MouseButton::Left,
                                cx.listener(|this, _ev, _window, cx| {
                                    this.force_ai_summary_refresh(cx);
                                }),
                            ),
                    )
            })
        };

        let results_body: AnyElement = if results_detached {
            let this = cx.entity();
            crate::detached_results::render_placeholder(
                move |_window, app| {
                    this.update(app, |app, cx| app.reattach_results(cx));
                },
                cx,
            )
        } else {
            results.clone().into_any_element()
        };
        let results_pane = if !self.results_visible() {
            None
        } else if maximized {
            Some(div().flex_1().min_h_0().child(results_body))
        } else {
            Some(div().h(px(self.results_height)).child(results_body))
        };

        // --- Per-tab DDL side panel (right of the editor), with a drag
        // divider. Lives on the query tab that initiated the navigation; the
        // divider captures `tab_idx` so a drag in either split pane resizes
        // the pane it belongs to, never whichever tab happens to be active.
        // Omitted while maximized: the editor entity is not in the tree.
        let ddl_panel_section = (!maximized).then_some(ddl_panel).flatten().map(|panel| {
            let this = cx.entity();
            let divider = PaneDivider::vertical(
                format!("ddl-panel-divider-{pane_tag}"),
                CollapseDirection::Right,
            )
            .label(crate::localization::text("shell.panes.ddl"))
            .on_drag_start({
                let this = this.clone();
                move |pos: f32, _window, app| {
                    this.update(app, |this, cx| {
                        this.is_resizing_ddl_panel = true;
                        this.ddl_resize_tab = tab_idx;
                        this.drag_start_pos = px(pos);
                        this.drag_start_ddl_panel_width = this
                            .tabs
                            .get(tab_idx)
                            .map(|t| t.ddl_panel_width)
                            .unwrap_or(380.0);
                        cx.notify();
                    });
                }
            })
            .on_toggle({
                let this = this.clone();
                move |_toggle, _window, app| {
                    this.update(app, |this, cx| {
                        if let Some(tab) = this.tabs.get_mut(tab_idx) {
                            tab.ddl_panel = None;
                        }
                        cx.notify();
                    });
                }
            });
            div()
                .flex()
                .flex_row()
                .h_full()
                .flex_shrink_0()
                .child(divider)
                .child(div().w(px(ddl_panel_width)).h_full().child(panel))
        });

        // --- Per-tab history side panel (right of the DDL panel), same
        // drag-divider pattern. Shows the tab's sealed version timeline.
        #[cfg(not(target_arch = "wasm32"))]
        let history_panel_section = (!maximized)
            .then_some(history_panel)
            .flatten()
            .map(|panel| {
                let this = cx.entity();
                let divider = PaneDivider::vertical(
                    format!("history-panel-divider-{pane_tag}"),
                    CollapseDirection::Right,
                )
                .label(crate::localization::text("shell.panes.history"))
                .on_drag_start({
                    let this = this.clone();
                    move |pos: f32, _window, app| {
                        this.update(app, |this, cx| {
                            this.is_resizing_history_panel = true;
                            this.history_resize_tab = tab_idx;
                            this.drag_start_pos = px(pos);
                            this.drag_start_history_panel_width = this
                                .tabs
                                .get(tab_idx)
                                .map(|t| t.history_panel_width)
                                .unwrap_or(300.0);
                            cx.notify();
                        });
                    }
                })
                .on_toggle({
                    let this = this.clone();
                    move |_toggle, _window, app| {
                        this.update(app, |this, cx| {
                            if let Some(tab) = this.tabs.get_mut(tab_idx) {
                                tab.history_panel = None;
                            }
                            cx.notify();
                        });
                    }
                });
                div()
                    .flex()
                    .flex_row()
                    .h_full()
                    .flex_shrink_0()
                    .child(divider)
                    .child(div().w(px(history_panel_width)).h_full().child(panel))
            });

        let editor_area = if maximized {
            let this = cx.entity();
            let preview = {
                let text = editor.read(cx).text();
                text.lines()
                    .map(str::trim)
                    .find(|line| !line.is_empty())
                    .map(|line| crate::er_diagram::truncate_label(line, 120))
                    .unwrap_or_default()
            };
            div()
                .h(px(Theme::row_height()))
                .flex_shrink_0()
                .flex()
                .flex_row()
                .items_center()
                .min_h_0()
                .px(px(s(8.0)))
                .gap(px(s(8.0)))
                .border_b_1()
                .border_color(Theme::border())
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .text_size(px(Theme::font_size()))
                        .text_color(Theme::text_dim())
                        .child(preview),
                )
                .child(
                    Button::new("restore-editor")
                        .label(crate::localization::text("shell.panes.restore_editor"))
                        .ghost()
                        .xsmall()
                        .on_click(move |_, _, app| {
                            this.update(app, |this, cx| {
                                this.results_pane =
                                    crate::results_pane_state::ResultsPaneState::Split;
                                this.save_workspace_session(cx);
                                cx.notify();
                            });
                        }),
                )
        } else {
            let editor_area = div()
                .flex_1()
                .flex()
                .flex_row()
                .min_h_0()
                .border_b_1()
                .border_color(Theme::border())
                .child(div().flex_1().min_w_0().min_h_0().child(Self::frame_cached(
                    editor.clone(),
                    editor.read(cx).needs_live_frames(),
                )))
                .children(ddl_panel_section);
            #[cfg(not(target_arch = "wasm32"))]
            let editor_area = editor_area.children(history_panel_section);
            editor_area
        };

        // Procedure-mode parameter strip: sits between the editor it
        // parameterizes and the AI summary bar, visible only while the mode
        // is on for this tab.
        let proc_strip = if maximized {
            None
        } else {
            self.render_proc_strip(tab_idx, window, cx)
        };
        // `-- sqlly param:` strip: same band, shown when the buffer declares
        // parameters and Procedure mode is off.
        let params_strip = if maximized {
            None
        } else {
            self.render_query_params_strip(tab_idx, window, cx)
        };

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .child(editor_area)
            .children(proc_strip)
            .children(params_strip)
            .children(ai_summary_bar)
            .child(results_handle)
            .children(results_pane)
            .into_any_element()
    }

    /// The same-buffer split's trailing pane: the second editor view alone.
    /// The leading pane keeps the full stack (results, panels, bars) — those
    /// are single entities that cannot be mounted twice in one window.
    fn render_same_buffer_view_pane(&self) -> AnyElement {
        let Some(view) = self.same_buffer_view.clone() else {
            return div().flex_1().into_any_element();
        };
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .border_b_1()
            .border_color(Theme::border())
            .child(div().flex_1().min_w_0().min_h_0().child(view))
            .into_any_element()
    }
}

impl Render for AppView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        self.begin_chrome_frame(cx);
        // Re-sync the gpui-component global theme whenever the app palette
        // changed (theme kind/family switch, OS appearance flip, font prefs).
        // Fingerprint-gated, so steady-state frames are a cheap compare.
        crate::theme::sync_component_theme(cx);

        if self.zoom_slider_needs_sync {
            self.zoom_slider_needs_sync = false;
            let pct = crate::editor_zoom::zoom_to_percent(self.editor_zoom) as f32;
            self.zoom_slider.update(cx, |slider, cx| {
                slider.set_value(pct, window, cx);
            });
        }

        // Grid menu "Masking Rules…": open Settings on the masking pane.
        // Deferred to render because the requesting event has no `Window`.
        if self.pending_open_masking_settings {
            self.pending_open_masking_settings = false;
            self.handle_open_preferences(&OpenPreferences, window, cx);
            if let Some(prefs) = &self.overlays.preferences_window {
                prefs.update(cx, |prefs, cx| {
                    prefs.select_pane(crate::preferences::PreferencesPane::ResultsMasking, cx);
                });
            }
        }

        #[cfg(not(target_arch = "wasm32"))]
        if let Some(preselect) = self.overlays.pending_data_transfer_preselect.take() {
            self.open_data_transfer_for(preselect, window, cx);
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(schema) = self.overlays.pending_import_from_clipboard.take() {
            self.open_import_from_clipboard(schema, window, cx);
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(database) = self.overlays.pending_database_dump.take() {
            self.open_database_dump_dialog(database, window, cx);
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Some((database, schema)) = self.overlays.pending_table_export.take() {
            self.open_table_export_dialog(database, schema, window, cx);
        }
        #[cfg(not(target_arch = "wasm32"))]
        if self.overlays.pending_database_search {
            self.overlays.pending_database_search = false;
            self.handle_search_database_value(&SearchDatabaseValue, window, cx);
        }
        #[cfg(not(target_arch = "wasm32"))]
        if self.overlays.pending_run_sql_file {
            self.overlays.pending_run_sql_file = false;
            self.handle_run_sql_file(&RunSqlFile, window, cx);
        }
        #[cfg(not(target_arch = "wasm32"))]
        if self.overlays.pending_tunnel_profiles {
            self.overlays.pending_tunnel_profiles = false;
            self.open_tunnel_profiles_dialog(None, window, cx);
        }
        #[cfg(not(target_arch = "wasm32"))]
        if self.overlays.pending_open_mcp_activity {
            self.overlays.pending_open_mcp_activity = false;
            self.open_mcp_activity(window, cx);
        }
        if let Some((schema, name)) = self.overlays.pending_query_builder.take() {
            self.open_query_builder_with_table(&schema, &name, window, cx);
        }
        if let Some((tool, scope)) = self.overlays.pending_explorer_tool.take() {
            self.dispatch_explorer_tool(tool, scope, window, cx);
        }

        // Keep Tools ▸ Connect/Disconnect in step with the live connection.
        self.sync_connection_menus(cx);
        self.sync_plan_capture_gate(cx);
        if self.production_unlock.is_some() && self.production_unlock_tick.is_none() {
            self.schedule_production_unlock_tick(cx);
        }
        if !self.read_only_unlocks.is_empty() && self.read_only_unlock_tick.is_none() {
            self.schedule_read_only_unlock_tick(cx);
        }
        if self.tabs.iter().any(|t| t.manual_tx.is_some()) && self.manual_tx_tick.is_none() {
            self.schedule_manual_tx_tick(cx);
        }

        let log_visible = self.log_console.read(cx).is_visible();
        let layout = crate::window_menu_bar::LayoutToggleState {
            sidebar: self.sidebar_visible,
            ai_drawer: self.ai_visible,
            log: self.results_visible(),
            ai_in_flight: self.ai_panel.read(cx).is_run_in_flight(),
        };
        // A notify made while the window is drawing neither reaches this
        // frame nor schedules another, and the bar is drawn from the frame
        // cache, so the notify waits until the draw is over.
        if self
            .window_menu_bar
            .update(cx, |bar, _| bar.set_layout_toggles(layout))
        {
            let bar = self.window_menu_bar.clone();
            cx.defer(move |cx| bar.update(cx, |_, cx| cx.notify()));
        }

        // A firewall block recognised since the last frame becomes its dialog
        // here — this is the first point after detection with a `&mut Window`
        // to build the dialog's text fields against.
        self.materialize_firewall_prompt(window, cx);
        // SSH host-key prompts come from the native tunnel stack; the module
        // (and this method) are not built for wasm.
        #[cfg(not(target_arch = "wasm32"))]
        self.materialize_host_key_prompt(window, cx);
        #[cfg(not(target_arch = "wasm32"))]
        self.materialize_ssh_challenge_prompt(window, cx);
        #[cfg(not(target_arch = "wasm32"))]
        self.ensure_file_watch_ticker(cx);
        #[cfg(not(target_arch = "wasm32"))]
        self.materialize_disk_change_prompt(window, cx);

        // Keep the long-summary word budget in sync with the editor width.
        // Render runs on every resize; this only stores the new ideal word
        // count for the next generation — it never regenerates on the fly.
        self.refresh_ai_summary_word_budget(cx);

        let sidebar = self.sidebar.clone();
        let ai_panel = self.ai_panel.clone();
        let modal = self.overlays.connection_editor.clone();
        let management = self.overlays.connection_management.clone();
        let entity_management = self.overlays.entity_management.clone();
        let ai_visible = self.ai_visible;
        let palette = self.overlays.command_palette.clone();
        let file_picker = self.overlays.project_file_picker.clone();
        let workspace_dialog = self.overlays.workspace_dialog.clone();
        let prefs = self.overlays.preferences_window.clone();
        let mutation_review = self.pending.mutation_review.clone();
        let staged_save = self.pending.staged_save.clone();
        let resume_confirmation = self.pending.resume_confirmation.clone();
        let connection_deletion = self.pending.connection_deletion.clone();
        let transaction_close = self.pending.transaction_close.clone();
        let entra_dialog = self.overlays.entra_accounts_dialog.clone();
        let security_dialog = self.overlays.security_dialog.clone();
        let backup_restore_dialog = self.overlays.backup_restore_dialog.clone();
        #[cfg(not(target_arch = "wasm32"))]
        let native_restore_dialog = self.overlays.native_restore_dialog.clone();
        #[cfg(not(target_arch = "wasm32"))]
        let run_sql_file_dialog = self.overlays.run_sql_file_dialog.clone();
        #[cfg(not(target_arch = "wasm32"))]
        let batch_insert_dialog = self.overlays.batch_insert_dialog.clone();
        #[cfg(not(target_arch = "wasm32"))]
        let tunnel_profiles_dialog = self.overlays.tunnel_profiles_dialog.clone();
        #[cfg(not(target_arch = "wasm32"))]
        let deep_link_sheet = self.overlays.deep_link_sheet.clone();
        #[cfg(not(target_arch = "wasm32"))]
        let database_search_dialog = self.overlays.database_search_dialog.clone();
        #[cfg(not(target_arch = "wasm32"))]
        let database_dump_dialog = self.overlays.database_dump_dialog.clone();
        #[cfg(not(target_arch = "wasm32"))]
        let table_export_dialog = self.overlays.table_export_dialog.clone();
        #[cfg(not(target_arch = "wasm32"))]
        let database_dialog = self.overlays.database_dialog.clone();
        #[cfg(not(target_arch = "wasm32"))]
        let redis_key_dialog = self.overlays.redis_key_dialog.clone();
        #[cfg(not(target_arch = "wasm32"))]
        let redis_key_action_dialog = self.overlays.redis_key_action_dialog.clone();
        let maintenance_dialog = self.overlays.maintenance_dialog.clone();
        let activity_monitor_pane = self.overlays.activity_monitor_pane.clone();
        let server_dashboard_panes: Vec<_> = self
            .overlays
            .server_dashboard_panes
            .iter()
            .map(|(_, pane)| pane.clone())
            .collect();
        let template_explorer_dialog = self.overlays.template_explorer_dialog.clone();
        let query_store_pane = self.overlays.query_store_pane.clone();
        let disk_usage_pane = self.overlays.disk_usage_pane.clone();
        let redis_memory_pane = self.overlays.redis_memory_pane.clone();
        #[cfg(not(target_arch = "wasm32"))]
        let redis_acl_pane = self.overlays.redis_acl_pane.clone();
        #[cfg(not(target_arch = "wasm32"))]
        let redis_key_view = self.overlays.redis_key_view.clone();
        #[cfg(not(target_arch = "wasm32"))]
        let redis_cluster_topology = self.overlays.redis_cluster_topology.clone();
        let object_browser_pane = self.overlays.object_browser_pane.clone();
        let index_analyzer_pane = self.overlays.index_analyzer_pane.clone();
        let test_data_generator_pane = self.overlays.test_data_generator_pane.clone();
        let tenant_scoping_dialog = self.overlays.tenant_scoping_dialog.clone();
        let query_history_browser = self.overlays.query_history_browser.clone();
        #[cfg(not(target_arch = "wasm32"))]
        let mcp_activity_dialog = self.overlays.mcp_activity_dialog.clone();
        let object_dependencies_pane = self.overlays.object_dependencies_pane.clone();
        let sql_agent_pane = self.overlays.sql_agent_pane.clone();
        let import_export_dialog = self.overlays.import_export_dialog.clone();
        let schema_compare_pane = self.overlays.schema_compare_pane.clone();
        #[cfg(not(target_arch = "wasm32"))]
        let schema_history_dialog = self.overlays.schema_history_dialog.clone();
        #[cfg(not(target_arch = "wasm32"))]
        let save_schema_snapshot_dialog = self.overlays.save_schema_snapshot_dialog.clone();
        #[cfg(not(target_arch = "wasm32"))]
        let schema_compare_deploy_dialog = self.overlays.schema_compare_deploy_dialog.clone();
        let data_compare_pane = self.overlays.data_compare_pane.clone();
        #[cfg(not(target_arch = "wasm32"))]
        let data_transfer_pane = self.overlays.data_transfer_pane.clone();
        let xevents_pane = self.overlays.xevents_pane.clone();
        let designer_dialog = self.overlays.designer_dialog.clone();
        let structure_editor = self.overlays.structure_editor.clone();
        let multi_target_dialog = self.overlays.multi_target_dialog.clone();
        let firewall_dialog = self.overlays.firewall_rule_dialog.clone();
        #[cfg(not(target_arch = "wasm32"))]
        let host_key_dialog = self.overlays.host_key_dialog.clone();
        #[cfg(not(target_arch = "wasm32"))]
        let ssh_challenge_dialog = self.overlays.ssh_challenge_dialog.clone();
        #[cfg(not(target_arch = "wasm32"))]
        let credential_export_dialog = self.overlays.credential_export_dialog.clone();
        let row_editor_dialog = self
            .overlays
            .row_editor
            .as_ref()
            .map(|(dialog, _)| dialog.clone());

        let tab_switcher = self.render_tab_switcher_overlay(window, cx);

        // A stale split (indices no longer coherent after some mutation, or a
        // same-buffer split whose second view went missing) must be dropped,
        // never drawn. Render-time cleanup only clears local state; the
        // orphaned view entity (if any) simply drops, taking its
        // subscriptions with it.
        if self.split.as_ref().is_some_and(|split| {
            !split.is_valid(self.active_tab, self.tabs.len())
                || (split.is_same_buffer() && self.same_buffer_view.is_none())
        }) {
            self.split = None;
            self.same_buffer_view = None;
        }

        #[cfg(not(target_arch = "wasm32"))]
        let (history_panel, _history_panel_width) = match self.active_tab() {
            Some(tab) => (tab.history_panel.clone(), tab.history_panel_width),
            None => (None, 300.0),
        };
        let (results, ddl_view, ddl_panel) = match self.active_tab() {
            Some(tab) => (
                Some(tab.results.clone()),
                tab.ddl_view.clone(),
                tab.ddl_panel.clone(),
            ),
            // Connected with every query tab closed: fall through to the
            // normal workbench layout so the server explorer, status bar, and
            // overlay stack keep their places — only the query area swaps to
            // the "no query open" nudge. The early return below is reserved
            // for the disconnected welcome canvas, which owns the whole
            // window.
            None if self.active_profile.is_some() => (None, None, None),
            None => {
                tracing::trace!(
                    modal = modal.is_some(),
                    management = management.is_some(),
                    "render: no active tab"
                );
                // No active query tab: still render overlay surfaces that can be
                // opened in this state (connection manager, command palette,
                // preferences). Otherwise these modals would be set but never
                // reach the stack below due to this early return.
                let mut empty = div().relative().size_full().bg(Theme::bg());
                // Behind the modals: the same welcome canvas, so closing
                // every tab while disconnected never leaves a dead window.
                let model = self.build_welcome_model(cx);
                empty = empty.child(crate::welcome::render_welcome(&model, cx));
                if let Some(modal) = modal {
                    empty = empty.child(modal);
                }
                if let Some(management) = management {
                    empty = empty.child(management);
                }
                if let Some(em) = entity_management {
                    empty = empty.child(em);
                }
                if let Some(ed) = entra_dialog.clone() {
                    empty = empty.child(ed);
                }
                if let Some(sd) = security_dialog.clone() {
                    empty = empty.child(sd);
                }
                // The firewall prompt must reach the screen with no query
                // tab open too: a blocked connection fails the schema tree
                // long before anything is typed into an editor.
                if let Some(fd) = firewall_dialog.clone() {
                    empty = empty.child(fd);
                }
                #[cfg(not(target_arch = "wasm32"))]
                if let Some(hkd) = host_key_dialog.clone() {
                    empty = empty.child(hkd);
                }
                #[cfg(not(target_arch = "wasm32"))]
                if let Some(scd) = ssh_challenge_dialog.clone() {
                    empty = empty.child(scd);
                }
                #[cfg(not(target_arch = "wasm32"))]
                if let Some(ced) = credential_export_dialog.clone() {
                    empty = empty.child(ced);
                }
                if let Some(palette) = palette {
                    empty = empty.child(palette);
                }
                if let Some(file_picker) = file_picker {
                    empty = empty.child(file_picker);
                }
                if let Some(workspace_dialog) = workspace_dialog {
                    empty = empty.child(workspace_dialog);
                }
                if let Some(prefs) = prefs {
                    empty = empty.child(prefs);
                }
                empty = empty.track_focus(&self.workbench_focus);
                // gpui-component's sheet/dialog/notification layers are drawn
                // by the window's `Root` (a plugin `gpui_component::init`
                // registers), not by this view — see the note at the end of
                // `render`.
                let element = empty.into_any_element();
                let element = self.wrap_with_titlebar(element);
                return element;
            }
        };

        #[cfg(not(target_arch = "wasm32"))]
        if let Some(results) = &results {
            results.update(cx, |rv, _cx| {
                rv.bind_output_panels(history_panel.clone(), ddl_panel.clone());
            });
        }
        #[cfg(target_arch = "wasm32")]
        if let Some(results) = &results {
            results.update(cx, |rv, _cx| {
                rv.bind_output_panels(ddl_panel.clone());
            });
        }

        // The welcome canvas owns the central column while the app is
        // disconnected AND the active tab is untouched — never run, seed
        // query unedited. Typing one character, or running anything, brings
        // the workbench back, so write-first users keep their flow.
        let show_welcome = self.active_profile.is_none()
            && ddl_view.is_none()
            && self.active_tab().is_some_and(|tab| {
                !tab.is_notebook() && tab.results.read(cx).is_pristine() && {
                    let text = tab.editor.read(cx).text().trim();
                    text.is_empty() || text == DEFAULT_TAB_SQL
                }
            });

        let sidebar_width = clamp_sidebar_width(self.sidebar_width);

        let tab_bar = self.tab_bar_region_element(cx);

        // --- Query tab area: options bar + the editor+results stack. The
        // stack itself is built per pane by `render_pane_stack`, shared by
        // the single-pane and split layouts.
        let query_tab_area = if self.active_tab().is_none() {
            // Every query tab is closed while connected: the centered logo and
            // new-query nudge take the editor+results stack's place, while the
            // explorer, tab bar, and status bar stay where they were.
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .child(self.render_empty_query_area(cx))
                .into_any_element()
        } else if let Some(ddl_view) = ddl_view.filter(|_| self.split.is_none()) {
            // Read-only DDL tab: the viewer replaces the editor+results stack.
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .child(div().flex_1().min_h_0().child(ddl_view))
                .into_any_element()
        } else if show_welcome {
            let model = self.build_welcome_model(cx);
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .child(crate::welcome::render_welcome(&model, cx))
                .into_any_element()
        } else if let Some(split) = self.split.clone() {
            let first_tab = split.first_tab(self.active_tab);
            let second_tab = split.second_tab(self.active_tab);
            let first_stack = self.render_pane_stack(first_tab, "first", window, cx);
            // Same-buffer split: the trailing pane is the second VIEW of the
            // active tab's document — a bare editor, since the results grid,
            // panels, and bars already render once in the leading pane (a
            // gpui entity cannot be mounted twice in one window).
            let second_stack = if split.is_same_buffer() {
                self.render_same_buffer_view_pane()
            } else {
                self.render_pane_stack(second_tab, "second", window, cx)
            };
            let this = cx.entity();
            let split_divider = match split.axis {
                crate::editor_groups::SplitAxis::Vertical => {
                    PaneDivider::vertical("editor-split-divider", CollapseDirection::Right)
                }
                crate::editor_groups::SplitAxis::Horizontal => {
                    PaneDivider::horizontal("editor-split-divider", CollapseDirection::Down)
                }
            }
            .label(crate::localization::text("shell.panes.split"))
            .on_drag_start({
                let this = this.clone();
                move |pos: f32, _window, app| {
                    this.update(app, |this, cx| {
                        this.is_resizing_split = true;
                        this.drag_start_pos = px(pos);
                        this.drag_start_split_size =
                            this.split.as_ref().map(|s| s.first_size).unwrap_or(0.0);
                        cx.notify();
                    });
                }
            })
            .on_toggle({
                let this = this.clone();
                move |_toggle, _window, app| {
                    this.update(app, |this, cx| this.close_split(cx));
                }
            });
            // Click-to-focus routing: clicking anywhere inside a pane makes
            // its tab the active one, so menus, shortcuts, and the query
            // toolbar target the pane that was clicked.
            let focus_pane = |first: bool| {
                let this = this.clone();
                move |_ev: &gpui::MouseDownEvent, _window: &mut Window, app: &mut gpui::App| {
                    this.update(app, |this, cx| this.focus_split_pane(first, cx));
                }
            };
            let first_pane = div()
                .flex()
                .flex_col()
                .min_w_0()
                .min_h_0()
                .flex_shrink_0()
                .on_mouse_down(MouseButton::Left, focus_pane(true))
                .child(first_stack);
            let second_pane = div()
                .flex()
                .flex_col()
                .min_w_0()
                .min_h_0()
                .flex_1()
                .on_mouse_down(MouseButton::Left, focus_pane(false))
                .child(second_stack);
            let split_row = match split.axis {
                crate::editor_groups::SplitAxis::Vertical => div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h_0()
                    .child(first_pane.w(px(split.first_size)).h_full())
                    .child(split_divider)
                    .child(second_pane),
                crate::editor_groups::SplitAxis::Horizontal => div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .child(first_pane.h(px(split.first_size)).w_full())
                    .child(split_divider)
                    .child(second_pane),
            };
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .child(split_row)
                // Keyed by tab index: the fade replays on a tab switch (and
                // when the welcome canvas hands over the workbench), never on
                // the steady-state frames in between.
                .with_animation(
                    ElementId::NamedInteger("query-tab-content".into(), self.active_tab as u64),
                    Animation::new(Duration::from_millis(140))
                        .with_easing(cubic_bezier(0.16, 1.0, 0.3, 1.0)),
                    |this, delta| this.opacity(0.55 + 0.45 * delta),
                )
                .into_any_element()
        } else {
            let stack = self.render_pane_stack(self.active_tab, "main", window, cx);
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .child(stack)
                .with_animation(
                    ElementId::NamedInteger("query-tab-content".into(), self.active_tab as u64),
                    Animation::new(Duration::from_millis(140))
                        .with_easing(cubic_bezier(0.16, 1.0, 0.3, 1.0)),
                    |this, delta| this.opacity(0.55 + 0.45 * delta),
                )
                .into_any_element()
        };

        // --- Right side: tab bar (55px) + query tab area ---
        let right_side = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .child(tab_bar)
            .children(self.render_production_banner(cx))
            .child(query_tab_area);

        // --- Sidebar + PaneDivider. Hidden from the title bar = gone, no strip. ---
        let this = cx.entity();
        let mut row = div().flex().flex_row().flex_1().min_h_0();
        if self.sidebar_visible {
            let sidebar_divider = PaneDivider::vertical("sidebar-divider", CollapseDirection::Left)
                .on_drag_start({
                    let this = this.clone();
                    move |pos: f32, _window, app| {
                        this.update(app, |this, cx| {
                            this.is_resizing_sidebar = true;
                            this.drag_start_pos = px(pos);
                            this.drag_start_sidebar_width = this.sidebar_width;
                            cx.notify();
                        });
                    }
                });
            // The explorer keeps its width when the row is short of room.
            // Without `flex_shrink_0` it absorbs all of the shrink (the
            // right side is `flex_1` + `min_w_0`), and on Windows the first
            // frame of a new window is laid out before its real size arrives,
            // so a fresh install opened with the explorer squeezed to nothing.
            row = row.child(
                div()
                    .flex()
                    .flex_row()
                    .flex_shrink_0()
                    .h_full()
                    .overflow_hidden()
                    .child(
                        div()
                            .w(px(sidebar_width))
                            .flex_shrink_0()
                            .h_full()
                            .overflow_hidden()
                            .child(Self::frame_cached(
                                sidebar.clone(),
                                sidebar.read(cx).needs_live_frames(cx),
                            )),
                    )
                    .child(sidebar_divider),
            );
        }
        row = row.child(right_side);
        if ai_visible {
            let this = cx.entity();
            let ai_width = clamp_ai_width(self.ai_panel_width);
            let ai_divider = PaneDivider::vertical("ai-drawer-divider", CollapseDirection::Right)
                .on_drag_start({
                    let this = this.clone();
                    move |pos: f32, _window, app| {
                        this.update(app, |this, cx| {
                            this.is_resizing_ai = true;
                            this.drag_start_pos = px(pos);
                            this.drag_start_ai_width = this.ai_panel_width;
                            cx.notify();
                        });
                    }
                });
            row = row.child(
                div()
                    .flex()
                    .flex_row()
                    .flex_shrink_0()
                    .h_full()
                    .overflow_hidden()
                    .child(ai_divider)
                    .child(
                        div()
                            .w(px(ai_width))
                            .flex_shrink_0()
                            .h_full()
                            .overflow_hidden()
                            .child(ai_panel.clone()),
                    ),
            );
        }

        let log_height = self.log_height.clamp(
            crate::workspace_session::MIN_DIAGNOSTICS_HEIGHT,
            crate::workspace_session::MAX_DIAGNOSTICS_HEIGHT,
        );
        let log_divider = PaneDivider::horizontal("log-divider", CollapseDirection::Down)
            .on_drag_start({
                let this = this.clone();
                move |pos: f32, _window, app| {
                    this.update(app, |this, cx| {
                        this.is_resizing_log = true;
                        this.drag_start_pos = px(pos);
                        this.drag_start_log_height = this.log_height;
                        cx.notify();
                    });
                }
            });

        let main = div()
            .flex()
            .flex_col()
            .size_full()
            .bg(Theme::bg())
            .text_color(Theme::text())
            .child(self.menu_bar_element())
            .child(row)
            .when(log_visible, |d| {
                d.child(log_divider).child(
                    div()
                        .h(px(log_height))
                        .min_h_0()
                        .overflow_hidden()
                        .child(self.log_console.clone()),
                )
            })
            .child(self.lsp_debug.clone())
            .child(self.status_bar_region_element(cx))
            .when(
                self.is_resizing_sidebar
                    || self.is_resizing_ai
                    || self.is_resizing_ddl_panel
                    || self.is_resizing_history_panel
                    || (self.is_resizing_cell_detail
                        && matches!(
                            self.formatting.cell_detail_side,
                            crate::preferences::CellDetailSide::Right
                        )),
                |d| d.cursor(CursorStyle::ResizeLeftRight),
            )
            .when(
                self.is_resizing_results
                    || self.is_resizing_log
                    || (self.is_resizing_cell_detail
                        && matches!(
                            self.formatting.cell_detail_side,
                            crate::preferences::CellDetailSide::Bottom
                        )),
                |d| d.cursor(CursorStyle::ResizeUpDown),
            )
            .when(
                !self.is_resizing_sidebar
                    && !self.is_resizing_ai
                    && !self.is_resizing_ddl_panel
                    && !self.is_resizing_history_panel
                    && !self.is_resizing_results
                    && !self.is_resizing_log
                    && !self.is_resizing_cell_detail,
                |d| d.cursor(CursorStyle::Arrow),
            )
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _window, cx| {
                if ev.pressed_button != Some(MouseButton::Left) {
                    this.end_pane_resize(cx);
                }
                if this.is_resizing_sidebar {
                    let delta = f32::from(ev.position.x) - f32::from(this.drag_start_pos);
                    this.sidebar_width = clamp_sidebar_width(this.drag_start_sidebar_width + delta);
                    cx.notify();
                }
                if this.is_resizing_ai {
                    let delta = f32::from(this.drag_start_pos) - f32::from(ev.position.x);
                    this.ai_panel_width = clamp_ai_width(this.drag_start_ai_width + delta);
                    this.ai_width = this.ai_panel_width;
                    cx.notify();
                }
                if this.is_resizing_results {
                    // Dragging up increases height, so invert the delta
                    let delta = f32::from(this.drag_start_pos) - f32::from(ev.position.y);
                    this.results_height = (this.drag_start_results_height + delta).clamp(
                        crate::workspace_session::MIN_RESULTS_HEIGHT,
                        crate::workspace_session::MAX_RESULTS_HEIGHT,
                    );
                    cx.notify();
                }
                if this.is_resizing_log {
                    let delta = f32::from(this.drag_start_pos) - f32::from(ev.position.y);
                    this.log_height = (this.drag_start_log_height + delta).clamp(
                        crate::workspace_session::MIN_DIAGNOSTICS_HEIGHT,
                        crate::workspace_session::MAX_DIAGNOSTICS_HEIGHT,
                    );
                    cx.notify();
                }
                if this.is_resizing_ddl_panel {
                    // The panel sits on the right, so dragging left widens it.
                    let delta = f32::from(this.drag_start_pos) - f32::from(ev.position.x);
                    let width = (this.drag_start_ddl_panel_width + delta).clamp(240.0, 900.0);
                    let target = this.ddl_resize_tab;
                    if let Some(tab) = this.tabs.get_mut(target) {
                        tab.ddl_panel_width = width;
                    }
                    cx.notify();
                }
                #[cfg(not(target_arch = "wasm32"))]
                if this.is_resizing_history_panel {
                    // Same right-side geometry as the DDL panel.
                    let delta = f32::from(this.drag_start_pos) - f32::from(ev.position.x);
                    let width = (this.drag_start_history_panel_width + delta).clamp(240.0, 600.0);
                    let target = this.history_resize_tab;
                    if let Some(tab) = this.tabs.get_mut(target) {
                        tab.history_panel_width = width;
                    }
                    cx.notify();
                }
                if this.is_resizing_cell_detail {
                    match this.formatting.cell_detail_side {
                        crate::preferences::CellDetailSide::Right => {
                            let delta = f32::from(this.drag_start_pos) - f32::from(ev.position.x);
                            this.cell_detail_width = (this.drag_start_cell_detail_size + delta)
                                .clamp(
                                    crate::workspace_session::MIN_CELL_DETAIL_WIDTH,
                                    crate::workspace_session::MAX_CELL_DETAIL_WIDTH,
                                );
                        }
                        crate::preferences::CellDetailSide::Bottom => {
                            let delta = f32::from(this.drag_start_pos) - f32::from(ev.position.y);
                            this.cell_detail_height = (this.drag_start_cell_detail_size + delta)
                                .clamp(
                                    crate::workspace_session::MIN_CELL_DETAIL_HEIGHT,
                                    crate::workspace_session::MAX_CELL_DETAIL_HEIGHT,
                                );
                        }
                    }
                    let size = this.cell_detail_size_for_side();
                    if let Some(results) = this.tabs.get(this.active_tab).map(|t| t.results.clone())
                    {
                        results.update(cx, |rv, cx| rv.set_cell_detail_size(size, cx));
                    }
                    cx.notify();
                }
                if this.is_resizing_split {
                    let start = f32::from(this.drag_start_pos);
                    let base = this.drag_start_split_size;
                    if let Some(split) = &mut this.split {
                        let delta = match split.axis {
                            crate::editor_groups::SplitAxis::Vertical => {
                                f32::from(ev.position.x) - start
                            }
                            crate::editor_groups::SplitAxis::Horizontal => {
                                f32::from(ev.position.y) - start
                            }
                        };
                        split.first_size = crate::editor_groups::clamp_pane_size(base + delta);
                        cx.notify();
                    }
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _ev: &MouseUpEvent, _window, cx| {
                    this.end_pane_resize(cx);
                    this.is_resizing_split = false;
                    this.dragging_tab = None;
                    this.save_workspace_session(cx);
                    cx.notify();
                }),
            );

        let mut stack = div()
            .size_full()
            // Always keep a live focus target on the workbench root so closing
            // a dialog or drawer cannot leave the window pointed at a dropped
            // handle (which silently disables menu shortcuts and overlay opens).
            .track_focus(&self.workbench_focus)
            // The app's typeface is declared here, on the root, not on `main`.
            //
            // `main` is only one of `stack`'s children: the other eleven are
            // overlays (Preferences, command palette, connection and entity
            // management, the row editor, file picker, object properties, tool
            // output, mutation review, …). With the font on `main`, every one of
            // them rendered in GPUI's default system face instead of the user's
            // monospace — DESIGN.md's One Instrument Rule broken by mount point,
            // in eleven places, for as long as the overlays have existed. This
            // was found in Preferences and is not a Preferences bug.
            //
            // Read at render time per the Live Resolution Rule; never cached.
            .font(gpui::font(Theme::font_family()))
            .text_size(px(Theme::font_size()))
            // Finder/Explorer drag-and-drop: dropped .sql files open as
            // file-backed tabs (an already-open path refocuses its tab).
            // Native only — the browser preview has no external file drops.
            .on_drop(
                cx.listener(|this, paths: &gpui::ExternalPaths, _window, cx| {
                    for path in paths.paths() {
                        crate::sandbox_access::remember(path);
                    }
                    this.open_dropped_sql_files(paths.paths().to_vec(), cx);
                }),
            )
            .on_drop(
                cx.listener(|this, dragged: &DraggedIdentifier, _window, cx| {
                    this.insert_dragged_identifier(dragged.clone(), cx);
                }),
            )
            .on_action(cx.listener(Self::handle_toggle_command_palette))
            .on_action(cx.listener(Self::handle_new_query_tab_action))
            .on_action(cx.listener(Self::handle_new_notebook))
            .on_action(cx.listener(Self::handle_open_notebook))
            .on_action(cx.listener(Self::handle_save_notebook))
            .on_action(cx.listener(Self::handle_run_notebook_cell))
            .on_action(cx.listener(Self::handle_run_all_notebook_cells))
            .on_action(cx.listener(Self::handle_close_tab_action))
            .on_action(cx.listener(Self::handle_toggle_sidebar))
            .on_action(cx.listener(Self::handle_show_sql_library))
            .on_action(cx.listener(Self::handle_focus_explorer))
            .on_action(cx.listener(Self::handle_toggle_ai_panel))
            .on_action(cx.listener(Self::handle_toggle_log_console))
            .on_action(cx.listener(Self::handle_execute_active_query))
            .on_action(cx.listener(Self::handle_cancel_active_query))
            .on_action(cx.listener(Self::handle_validate_active_query))
            .on_action(cx.listener(Self::handle_open_preferences))
            .on_action(cx.listener(Self::handle_open_account))
            .on_action(cx.listener(Self::handle_open_object_properties))
            .on_action(cx.listener(Self::handle_refresh_object_explorer))
            .on_action(cx.listener(Self::handle_disconnect_action))
            .on_action(cx.listener(Self::handle_show_connection_management))
            .on_action(cx.listener(Self::handle_show_entity_management))
            .on_action(cx.listener(Self::handle_next_editor_tab))
            .on_action(cx.listener(Self::handle_previous_editor_tab))
            .on_action(cx.listener(Self::handle_previous_tab_in_history))
            .on_action(cx.listener(Self::handle_next_tab_in_history))
            .on_action(cx.listener(Self::handle_cancel_tab_switcher))
            .on_action(cx.listener(Self::handle_new_window))
            .on_action(cx.listener(Self::handle_open_sql))
            .on_action(cx.listener(Self::handle_open_sql_file))
            .on_action(cx.listener(Self::handle_run_sql_file))
            .on_action(cx.listener(Self::handle_search_database_value))
            .on_action(cx.listener(Self::handle_open_project_file))
            .on_action(cx.listener(Self::handle_open_folder))
            .on_action(cx.listener(Self::handle_save_sql))
            .on_action(cx.listener(Self::handle_save_to_library))
            .on_action(cx.listener(Self::handle_save_results))
            .on_action(cx.listener(Self::handle_save_workspace_as))
            .on_action(cx.listener(Self::handle_open_workspace))
            .on_action(cx.listener(Self::handle_manage_workspaces))
            .on_action(cx.listener(Self::handle_search_schema))
            .on_action(cx.listener(Self::handle_toggle_results_pane))
            .on_action(cx.listener(Self::handle_toggle_results_maximized))
            .on_action(cx.listener(Self::handle_detach_results))
            .on_action(cx.listener(Self::handle_reattach_results))
            .on_action(cx.listener(Self::handle_toggle_cell_details))
            .on_action(cx.listener(Self::handle_select_output_results))
            .on_action(cx.listener(Self::handle_select_output_messages))
            .on_action(cx.listener(Self::handle_select_output_plan))
            .on_action(cx.listener(Self::handle_select_output_history))
            .on_action(cx.listener(Self::handle_select_output_ddl))
            .on_action(cx.listener(Self::handle_select_output_diff))
            .on_action(cx.listener(Self::handle_reopen_closed_tab))
            .on_action(cx.listener(Self::handle_theme_system))
            .on_action(cx.listener(Self::handle_theme_light))
            .on_action(cx.listener(Self::handle_theme_dark))
            .on_action(cx.listener(Self::handle_theme_high_contrast))
            .on_action(cx.listener(Self::handle_generate_sql))
            .on_action(cx.listener(Self::handle_ask_schema))
            .on_action(cx.listener(Self::handle_send_selection_to_ai))
            .on_action(cx.listener(Self::handle_connect))
            .on_action(cx.listener(Self::handle_template_explorer))
            .on_action(cx.listener(Self::handle_import_export))
            .on_action(cx.listener(Self::handle_query_store))
            .on_action(cx.listener(Self::handle_disk_usage))
            .on_action(cx.listener(Self::handle_redis_memory_analyzer))
            .on_action(cx.listener(Self::handle_object_browser))
            .on_action(cx.listener(Self::handle_index_analyzer))
            .on_action(cx.listener(Self::handle_test_data_generator))
            .on_action(cx.listener(Self::handle_export_schema_docs))
            .on_action(cx.listener(Self::handle_tenant_scoping))
            .on_action(cx.listener(Self::handle_extended_events))
            .on_action(cx.listener(Self::handle_schema_compare))
            .on_action(cx.listener(Self::handle_schema_history))
            .on_action(cx.listener(Self::handle_save_schema_snapshot))
            .on_action(cx.listener(Self::handle_data_compare))
            .on_action(cx.listener(Self::handle_backup_restore))
            .on_action(cx.listener(Self::handle_activity_monitor))
            .on_action(cx.listener(Self::handle_sql_agent))
            .on_action(cx.listener(Self::handle_security))
            .on_action(cx.listener(Self::handle_designers))
            .on_action(cx.listener(Self::handle_azure_sql_connection))
            .on_action(cx.listener(Self::handle_show_entra_account_dialog))
            .on_action(cx.listener(Self::handle_add_firewall_rule))
            .on_action(cx.listener(Self::handle_toggle_intellisense_debug))
            .on_action(cx.listener(Self::handle_split_vertical))
            .on_action(cx.listener(Self::handle_split_horizontal))
            .on_action(cx.listener(Self::handle_execution_plan_viewer))
            .on_action(cx.listener(Self::handle_open_keybindings))
            .on_action(cx.listener(Self::handle_open_external_editor))
            .on_action(cx.listener(Self::handle_sqlly_help))
            .on_action(cx.listener(Self::handle_minimize_window))
            .on_action(cx.listener(Self::handle_zoom_window))
            .on_action(cx.listener(Self::handle_toggle_zoom_popover))
            .on_action(cx.listener(Self::handle_close_zoom_popover))
            .on_action(cx.listener(Self::handle_check_for_updates))
            .on_action(cx.listener(Self::handle_bring_all_to_front))
            .on_action(cx.listener(Self::handle_quit))
            .on_modifiers_changed(
                cx.listener(|this, event: &ModifiersChangedEvent, window, cx| {
                    if !event.modifiers.control
                        && crate::tab_switcher::modifier_release_commits(
                            this.tab_switcher_session.is_some(),
                            window.last_input_was_keyboard(),
                        )
                    {
                        this.commit_tab_switcher(cx);
                    }
                }),
            )
            .child(main);
        // Data Transfer is a native-only tool (its pane depends on the
        // transfer engine that is not built for wasm); register its action
        // handler only where the handler exists.
        #[cfg(not(target_arch = "wasm32"))]
        {
            stack = stack.on_action(cx.listener(Self::handle_data_transfer));
        }
        tracing::trace!(
            modal = modal.is_some(),
            management = management.is_some(),
            "render: active tab"
        );
        if let Some(modal) = modal {
            stack = stack.child(modal);
        }
        if let Some(management) = management {
            stack = stack.child(management);
        }
        if let Some(em) = entity_management {
            stack = stack.child(em);
        }
        if let Some(ed) = entra_dialog {
            stack = stack.child(ed);
        }
        if let Some(sd) = security_dialog {
            stack = stack.child(sd);
        }
        if let Some(brd) = backup_restore_dialog {
            stack = stack.child(brd);
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(nrd) = native_restore_dialog {
            stack = stack.child(nrd);
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(rsf) = run_sql_file_dialog {
            stack = stack.child(rsf);
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(bid) = batch_insert_dialog {
            stack = stack.child(bid);
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(tpd) = tunnel_profiles_dialog {
            stack = stack.child(tpd);
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(sheet) = deep_link_sheet {
            stack = stack.child(sheet);
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(dbs) = database_search_dialog {
            stack = stack.child(dbs);
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(ddd) = database_dump_dialog {
            stack = stack.child(ddd);
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(ted) = table_export_dialog {
            stack = stack.child(ted);
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(dd) = database_dialog {
            stack = stack.child(dd);
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(rd) = redis_key_dialog {
            stack = stack.child(rd);
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(ra) = redis_key_action_dialog {
            stack = stack.child(ra);
        }
        if let Some(md) = maintenance_dialog {
            stack = stack.child(md);
        }
        // Every open dashboard renders, most recently raised last (on top).
        for pane in server_dashboard_panes {
            stack = stack.child(pane);
        }
        if let Some(amp) = activity_monitor_pane {
            stack = stack.child(amp);
        }
        if let Some(ted) = template_explorer_dialog {
            stack = stack.child(ted);
        }
        if let Some((prompt, _, _)) = &self.overlays.template_params_dialog {
            stack = stack.child(prompt.clone());
        }
        if let Some(qsp) = query_store_pane {
            stack = stack.child(qsp);
        }
        if let Some(dup) = disk_usage_pane {
            stack = stack.child(dup);
        }
        if let Some(rmp) = redis_memory_pane {
            stack = stack.child(rmp);
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(acl) = redis_acl_pane {
            stack = stack.child(acl);
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(rkv) = redis_key_view {
            stack = stack.child(rkv);
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(rct) = redis_cluster_topology {
            stack = stack.child(rct);
        }
        if let Some(obp) = object_browser_pane {
            stack = stack.child(obp);
        }
        if let Some(iap) = index_analyzer_pane {
            stack = stack.child(iap);
        }
        if let Some(tdg) = test_data_generator_pane {
            stack = stack.child(tdg);
        }
        if let Some(tsd) = tenant_scoping_dialog {
            stack = stack.child(tsd);
        }
        if let Some(qhb) = query_history_browser {
            stack = stack.child(qhb);
        }
        if let Some(odp) = object_dependencies_pane {
            stack = stack.child(odp);
        }
        if let Some(sap) = sql_agent_pane {
            stack = stack.child(sap);
        }
        if let Some(ied) = import_export_dialog {
            stack = stack.child(ied);
        }
        if let Some(scp) = schema_compare_pane {
            stack = stack.child(scp);
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(shd) = schema_history_dialog {
            stack = stack.child(shd);
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(ssd) = save_schema_snapshot_dialog {
            stack = stack.child(ssd);
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(scd) = schema_compare_deploy_dialog {
            stack = stack.child(scd);
        }
        if let Some(dcp) = data_compare_pane {
            stack = stack.child(dcp);
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(dtp) = data_transfer_pane {
            stack = stack.child(dtp);
        }
        if let Some(xep) = xevents_pane {
            stack = stack.child(xep);
        }
        if let Some(dsg) = designer_dialog {
            stack = stack.child(dsg);
        }
        if let Some(editor) = structure_editor {
            stack = stack.child(editor);
        }
        if let Some(mtd) = multi_target_dialog {
            stack = stack.child(mtd);
        }
        if let Some(fd) = firewall_dialog {
            stack = stack.child(fd);
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(hkd) = host_key_dialog {
            stack = stack.child(hkd);
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(scd) = ssh_challenge_dialog {
            stack = stack.child(scd);
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(ced) = credential_export_dialog {
            stack = stack.child(ced);
        }
        // Below the nag so a launch that shows both reveals the tip once the
        // pay dialog is dismissed.
        if let Some(tip) = &self.overlays.tip_of_the_day {
            stack = stack.child(tip.clone());
        }
        if self.show_nag_dialog {
            let nag = self.render_nag_dialog(window, cx);
            stack = stack.child(nag);
        }
        if let Some(edition) = &self.overlays.app_store_edition_dialog {
            stack = stack.child(edition.clone());
        }
        if let Some(unlock) = &self.overlays.unlock_dialog {
            stack = stack.child(unlock.clone());
        }
        if self.show_account_dialog {
            let account = self.render_account_dialog(window, cx);
            stack = stack.child(account);
        }
        if self.show_update_dialog && self.update_available.is_some() {
            let update = self.render_update_dialog(window, cx);
            stack = stack.child(update);
        }
        if let Some(row_editor) = row_editor_dialog {
            stack = stack.child(row_editor);
        }
        if let Some(palette) = palette {
            stack = stack.child(palette);
        }
        if let Some(file_picker) = file_picker {
            stack = stack.child(file_picker);
        }
        if let Some(workspace_dialog) = workspace_dialog {
            stack = stack.child(workspace_dialog);
        }
        if let Some(prefs) = prefs {
            stack = stack.child(prefs);
        }
        // Opened from Settings > MCP, so it stacks above the Settings dialog.
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(mcp_activity) = mcp_activity_dialog {
            stack = stack.child(mcp_activity);
        }
        if let Some(editor) = &self.overlays.object_editor {
            stack = stack.child(editor.clone());
        }
        if let Some(tool_out) = &self.overlays.tool_output_view {
            stack = stack.child(tool_out.clone());
        }
        if let Some((title_key, panel)) = self.overlays.information_panel.clone() {
            stack = stack.child(
                Dialog::new("help-information-panel", |this: &mut Self| {
                    this.dialog_frames.get("help-information-panel", || {
                        DialogFrame::new(Theme::scaled(640.0), Theme::scaled(620.0))
                    })
                })
                .title(crate::localization::text(title_key))
                .dismiss_on_scrim_click(true)
                .on_close(|this, _window, cx| {
                    this.overlays.information_panel = None;
                    this.restore_editor_focus(cx);
                    cx.notify();
                })
                .child(div().size_full().min_h_0().child(panel))
                .build(self, window, cx),
            );
        }
        if let Some(review) = mutation_review {
            let bottom_line = review.bottom_line.clone();
            let has_unbounded = review.has_unbounded;
            let review_sql = review.sql.clone();
            let production_review = review.production;
            let ai_pending = review.ai_review_required && review.ai_explanation.is_none();
            let ai_explanation = review.ai_explanation.clone();
            stack = stack.child(
                Dialog::new("mutation-review-confirm", |this: &mut Self| {
                    this.dialog_frames.get("mutation-review-confirm", || {
                        DialogFrame::new_auto(Theme::scaled(560.0))
                    })
                })
                .title(if production_review {
                    crate::localization::text("shell.mutation_review.production_title")
                } else {
                    crate::localization::text("shell.mutation_review.title")
                })
                // Execute stays gated on the AI review, which the stock
                // confirm pair cannot express — custom footer buttons keep
                // the exact gating; the ✕/scrim route through cancel.
                .use_default_close(false)
                .dismiss_on_scrim_click(true)
                .on_close(|this, _window, cx| this.cancel_mutation(cx))
                .child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.87)))
                        .text_color(Theme::text())
                        .child(bottom_line),
                )
                .child(
                    div()
                        .font_family(Theme::editor_font_family())
                        .text_size(px(Theme::font_size_ratio(0.82)))
                        .text_color(Theme::text())
                        .bg(Theme::panel_bg())
                        .border_1()
                        .border_color(Theme::border())
                        .rounded(px(s(4.0)))
                        .p(px(s(8.0)))
                        .max_h(px(s(200.0)))
                        .overflow_hidden()
                        .child(review_sql),
                )
                .children_body(has_unbounded.then(|| {
                    div()
                        .text_size(px(Theme::font_size_ratio(0.80)))
                        .text_color(Theme::error())
                        .child(crate::localization::text(
                            "shell.mutation_review.unbounded_warning",
                        ))
                }))
                // ADR-008: the generated review (or its in-progress state)
                // above the buttons.
                .children_body(ai_explanation.map(|explanation| {
                    div()
                        .max_h(px(s(220.0)))
                        .overflow_hidden()
                        .p(px(s(10.0)))
                        .rounded(px(s(6.0)))
                        .bg(Theme::bg())
                        .border_1()
                        .border_color(Theme::border())
                        .text_size(px(Theme::font_size_ratio(0.82)))
                        .text_color(Theme::text())
                        .child(explanation)
                }))
                .children_body(ai_pending.then(|| {
                    div()
                        .text_size(px(Theme::font_size_ratio(0.82)))
                        .text_color(Theme::text_dim())
                        .child(crate::localization::text(
                            "shell.mutation_review.ai_pending",
                        ))
                }))
                .button(
                    Button::new("mutation-cancel")
                        .label(crate::localization::text("shell.mutation_review.cancel"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.cancel_mutation(cx);
                        })),
                )
                .button(
                    Button::new("mutation-confirm")
                        .label(crate::localization::text("shell.mutation_review.execute"))
                        .warning()
                        .disabled(ai_pending)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if !ai_pending {
                                this.confirm_mutation(cx);
                            }
                        })),
                )
                .build(self, window, cx),
            );
        }
        if let Some(save) = staged_save {
            let bottom_line = save.bottom_line.clone();
            let preview_sql = save.preview_sql.clone();
            // "Copy as SQL" gets the guarded copy script (row-count checks
            // where the engine allows), the same text the results pane copies.
            let copy_sql = save.results.read(cx).staged_change_script();
            let ai_pending = save.ai_review_required && save.ai_explanation.is_none();
            let ai_explanation = save.ai_explanation.clone();
            stack = stack.child(
                Dialog::new("staged-save-confirm", |this: &mut Self| {
                    this.dialog_frames.get("staged-save-confirm", || {
                        DialogFrame::new_auto(Theme::scaled(560.0))
                    })
                })
                .title(crate::localization::text("shell.staged_save.title"))
                // Save stays gated on the AI review, which the stock confirm
                // pair cannot express — custom footer buttons keep the exact
                // gating; the ✕/scrim route through cancel.
                .use_default_close(false)
                .dismiss_on_scrim_click(true)
                .on_close(|this, _window, cx| this.cancel_staged_save(cx))
                .child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.87)))
                        .text_color(Theme::text())
                        .child(bottom_line),
                )
                .child(
                    div()
                        .font_family(Theme::editor_font_family())
                        .text_size(px(Theme::font_size_ratio(0.82)))
                        .text_color(Theme::text())
                        .bg(Theme::panel_bg())
                        .border_1()
                        .border_color(Theme::border())
                        .rounded(px(s(4.0)))
                        .p(px(s(8.0)))
                        .max_h(px(s(200.0)))
                        .overflow_hidden()
                        .child(preview_sql),
                )
                .children_body(ai_explanation.map(|explanation| {
                    div()
                        .max_h(px(s(220.0)))
                        .overflow_hidden()
                        .p(px(s(10.0)))
                        .rounded(px(s(6.0)))
                        .bg(Theme::bg())
                        .border_1()
                        .border_color(Theme::border())
                        .text_size(px(Theme::font_size_ratio(0.82)))
                        .text_color(Theme::text())
                        .child(explanation)
                }))
                .children_body(ai_pending.then(|| {
                    div()
                        .text_size(px(Theme::font_size_ratio(0.82)))
                        .text_color(Theme::text_dim())
                        .child(crate::localization::text("shell.staged_save.ai_pending"))
                }))
                .button(
                    Button::new("staged-save-copy-sql")
                        .label(crate::localization::text("shell.staged_save.copy_sql"))
                        .on_click(move |_, _, cx| {
                            cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                copy_sql.clone(),
                            ));
                        }),
                )
                .button(
                    Button::new("staged-save-cancel")
                        .label(crate::localization::text("shell.staged_save.cancel"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.cancel_staged_save(cx);
                        })),
                )
                .button(
                    Button::new("staged-save-confirm")
                        .label(crate::localization::text("shell.staged_save.save"))
                        .warning()
                        .disabled(ai_pending)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if !ai_pending {
                                this.confirm_staged_save(cx);
                            }
                        })),
                )
                .build(self, window, cx),
            );
        }
        if let Some(rc) = resume_confirmation {
            let server = rc.server.clone();
            let database = rc.database.clone();
            let db_display = database
                .as_deref()
                .map(str::to_string)
                .unwrap_or_else(|| crate::localization::text("shell.resume.default_database"));
            let rc_tab_idx = rc.tab_idx;
            // Resolve the remembered profile for the details list
            // (environment + credentials come from the saved connection). Use
            // the same soft-match resolver as the actual switch so the details
            // and the Switch action agree on which profile is targeted.
            let resume_profile = Self::resolve_resume_profile(&rc.server, &rc.database);
            let environment = resume_profile
                .as_ref()
                .map(|p| crate::status_bar::localized_environment_label(p.environment))
                .unwrap_or_else(|| crate::localization::text("shell.resume.unknown_environment"));
            let credentials = resume_profile
                .as_ref()
                .map(|p| match &p.authentication {
                    sqlly_core::AuthenticationType::SqlLogin { username, .. } => username.clone(),
                    sqlly_core::AuthenticationType::EntraDeviceCode
                    | sqlly_core::AuthenticationType::EntraBrowser => p
                        .entra_account
                        .as_ref()
                        .and_then(|a| a.username.clone())
                        .unwrap_or_else(|| "Microsoft Entra".to_string()),
                    sqlly_core::AuthenticationType::Integrated => {
                        crate::localization::text("shell.resume.integrated_credentials")
                    }
                    sqlly_core::AuthenticationType::AwsRdsIam { username, .. }
                    | sqlly_core::AuthenticationType::GcpCloudSqlIam { username } => {
                        username.clone()
                    }
                    sqlly_core::AuthenticationType::AuthToken { .. } => {
                        crate::localization::text("shell.resume.auth_token_credentials")
                    }
                })
                .unwrap_or_else(|| crate::localization::text("shell.resume.unknown_credentials"));
            let dont_ask_checked = self
                .tabs
                .get(rc_tab_idx)
                .map(|t| t.suppress_resume_connection_confirmation)
                .unwrap_or(false);
            let detail_row = |label: &str, value: String| {
                div()
                    .flex()
                    .flex_row()
                    .gap(px(s(6.0)))
                    .text_size(px(Theme::font_size_ratio(0.80)))
                    .child(
                        div()
                            .w(px(s(90.0)))
                            .text_color(Theme::text_dim())
                            .child(format!("{label}:")),
                    )
                    // min_w(0): a long server name wraps instead of
                    // running past the card's edge.
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .text_color(Theme::text())
                            .child(value),
                    )
            };
            stack = stack.child(
                Dialog::new("resume-connection-confirm", |this: &mut Self| {
                    this.dialog_frames.get("resume-connection-confirm", || {
                        DialogFrame::new_auto(Theme::scaled(400.0))
                    })
                })
                .title(crate::localization::text("shell.resume.title"))
                .confirm(
                    crate::localization::text("shell.resume.switch_button"),
                    crate::localization::text("shell.resume.cancel_button"),
                )
                .dismiss_on_scrim_click(true)
                .on_confirm(|this, _window, cx| this.confirm_resume_connection(cx))
                .on_reject(|this, _window, cx| this.cancel_resume_connection(cx))
                .on_close(|this, _window, cx| this.cancel_resume_connection(cx))
                .child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.87)))
                        .text_color(Theme::text())
                        .child(crate::localization::format(
                            "shell.resume.switch_prompt",
                            &[("server", &server), ("database", &db_display)],
                        )),
                )
                .child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.80)))
                        .text_color(Theme::text_dim())
                        .child(crate::localization::text(
                            "shell.resume.disconnected_explanation",
                        )),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(s(4.0)))
                        .child(detail_row(
                            &crate::localization::text("shell.resume.server_label"),
                            server.clone(),
                        ))
                        .child(detail_row(
                            &crate::localization::text("shell.resume.database_label"),
                            db_display.to_string(),
                        ))
                        .child(detail_row(
                            &crate::localization::text("shell.resume.environment_label"),
                            environment,
                        ))
                        .child(detail_row(
                            &crate::localization::text("shell.resume.credentials_label"),
                            credentials,
                        )),
                )
                .leading_button({
                    let this = cx.entity();
                    Checkbox::new("resume-dont-ask")
                        .checked(dont_ask_checked)
                        .label(crate::localization::text("shell.resume.dont_ask_again"))
                        .tab_stop(true)
                        .on_click(move |_checked: &bool, _window, app| {
                            this.update(app, |this, cx| {
                                if let Some(tab) = this.tabs.get_mut(rc_tab_idx) {
                                    tab.suppress_resume_connection_confirmation =
                                        !tab.suppress_resume_connection_confirmation;
                                    cx.notify();
                                }
                            });
                        })
                })
                .build(self, window, cx),
            );
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(notice) = self.pending.terminal_notice.clone() {
            let tool = notice.launch.tool.binary_name();
            stack = stack.child(
                Dialog::new("terminal-secret-notice", |this: &mut Self| {
                    this.dialog_frames.get("terminal-secret-notice", || {
                        DialogFrame::new_auto(Theme::scaled(440.0))
                    })
                })
                .title(crate::localization::text("shell.terminal_notice.title"))
                .confirm(
                    crate::localization::text("shell.terminal_notice.open_button"),
                    crate::localization::text("shell.terminal_notice.cancel_button"),
                )
                .dismiss_on_scrim_click(true)
                .on_confirm(|this, _window, cx| this.confirm_terminal_notice(cx))
                .on_reject(|this, _window, cx| this.cancel_terminal_notice(cx))
                .on_close(|this, _window, cx| this.cancel_terminal_notice(cx))
                .children_body(terminal_notice_body(&notice.name, tool))
                .leading_button({
                    let this = cx.entity();
                    Checkbox::new("terminal-notice-dont-show")
                        .checked(notice.dont_show_again)
                        .label(crate::localization::text(
                            "shell.terminal_notice.dont_show_again",
                        ))
                        .tab_stop(true)
                        .on_click(move |_checked: &bool, _window, app| {
                            this.update(app, |this, cx| {
                                if let Some(pending) = this.pending.terminal_notice.as_mut() {
                                    pending.dont_show_again = !pending.dont_show_again;
                                    cx.notify();
                                }
                            });
                        })
                })
                .build(self, window, cx),
            );
        }
        if let Some(pd) = connection_deletion {
            let detail_row = |label: &str, value: String| {
                div()
                    .flex()
                    .flex_row()
                    .gap(px(s(6.0)))
                    .text_size(px(Theme::font_size_ratio(0.80)))
                    .child(
                        div()
                            .w(px(s(90.0)))
                            .text_color(Theme::text_dim())
                            .child(format!("{label}:")),
                    )
                    // min_w(0): a long server name wraps instead of
                    // running past the card's edge.
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .text_color(Theme::text())
                            .child(value),
                    )
            };
            stack = stack.child(
                Dialog::new("connection-deletion-confirm", |this: &mut Self| {
                    this.dialog_frames.get("connection-deletion-confirm", || {
                        DialogFrame::new_auto(Theme::scaled(400.0))
                    })
                })
                .title(crate::localization::text("shell.delete_connection.title"))
                .confirm(
                    crate::localization::text("shell.delete_connection.delete_button"),
                    crate::localization::text("shell.delete_connection.cancel_button"),
                )
                .danger()
                .dismiss_on_scrim_click(true)
                .on_confirm(|this, _window, cx| this.confirm_delete_connection(cx))
                .on_reject(|this, _window, cx| this.cancel_delete_connection(cx))
                .on_close(|this, _window, cx| this.cancel_delete_connection(cx))
                .child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.87)))
                        .text_color(Theme::text())
                        .child(crate::localization::format(
                            "shell.delete_connection.prompt",
                            &[("name", &pd.name)],
                        )),
                )
                .child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.80)))
                        .text_color(Theme::text_dim())
                        .child(crate::localization::text("shell.delete_connection.warning")),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(s(4.0)))
                        .child(detail_row(
                            &crate::localization::text("shell.delete_connection.server_label"),
                            pd.server.clone(),
                        ))
                        .child(detail_row(
                            &crate::localization::text("shell.delete_connection.environment_label"),
                            pd.environment.clone(),
                        )),
                )
                .build(self, window, cx),
            );
        }
        if let Some(dialog) = self.render_notifications_dialog(window, cx) {
            stack = stack.child(dialog);
        }
        if let Some(dialog) = self.render_status_detail_dialog(window, cx) {
            stack = stack.child(dialog);
        }
        if let Some(pt) = transaction_close {
            stack = stack.child(
                Dialog::new("transaction-close-confirm", |this: &mut Self| {
                    this.dialog_frames.get("transaction-close-confirm", || {
                        DialogFrame::new_auto(Theme::scaled(400.0))
                    })
                })
                .title(crate::localization::text("shell.transaction_close.title"))
                // Three-way choice: the default Close button is the cancel
                // path; commit/rollback are the dialog's own actions.
                .dismiss_on_scrim_click(true)
                .on_close(|this, _window, cx| this.cancel_transaction_close(cx))
                .child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.87)))
                        .text_color(Theme::text())
                        .child(crate::localization::format(
                            "shell.transaction_close.uncommitted_prompt",
                            &[
                                ("tab_name", &pt.tab_name),
                                ("count", &pt.statements.to_string()),
                            ],
                        )),
                )
                .button(
                    Button::new("tx-close-rollback")
                        .label(crate::localization::text(
                            "shell.transaction_close.rollback_button",
                        ))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.confirm_transaction_close_rollback(cx);
                        })),
                )
                .button(
                    Button::new("tx-close-commit")
                        .label(crate::localization::text(
                            "shell.transaction_close.commit_button",
                        ))
                        .primary()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.confirm_transaction_close_commit(cx);
                        })),
                )
                .build(self, window, cx),
            );
        }
        if let Some(overlay) = tab_switcher {
            stack = stack.child(overlay);
        }
        if let Some(menu) = self.render_tab_context_menu(window.window_handle().window_id(), cx) {
            stack = stack.child(menu);
        }
        if let Some(browser) = &self.overlays.snapshot_browser {
            stack = stack.child(browser.clone());
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            if let Some(dashboard) = &self.overlays.chart_dashboard {
                stack = stack.child(dashboard.clone());
            }
            if let Some(panel) = &self.overlays.saved_charts_panel {
                stack = stack.child(panel.clone());
            }
            if let Some(prompt) = &self.overlays.save_chart_dialog {
                stack = stack.child(prompt.clone());
            }
        }
        if let Some(dialog) = &self.overlays.connection_health_dialog {
            stack = stack.child(dialog.clone());
        }
        if let Some(rename) = self.render_rename_dialog(window, cx) {
            stack = stack.child(rename);
        }
        if let Some(pin_group) = self.render_pin_group_dialog(window, cx) {
            stack = stack.child(pin_group);
        }
        if let Some(picker) = self.render_run_picker_dialog(window, cx) {
            stack = stack.child(picker);
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(tool_import) = self.render_tool_import_dialog(window, cx) {
            stack = stack.child(tool_import);
        }
        if let Some(er) = &self.overlays.er_diagram {
            stack = stack.child(er.clone());
        }
        if let Some(qb) = &self.overlays.query_builder {
            stack = stack.child(qb.clone());
        }
        if let Some(feedback) = &self.overlays.intellisense_feedback {
            stack = stack.child(feedback.clone());
        }
        // gpui-component overlay layers (dialogs/sheets opened via
        // `window.open_dialog` / `window.open_sheet`, toasts pushed via
        // `window.push_notification`) are NOT rendered here. Since
        // gpui-component 0.7 the window's `Root` mounts them itself, above
        // this whole view, through the `WindowState` plugin that
        // `gpui_component::init` registers — so every window built with
        // `gpui_component::Root::new` gets them, as long as `init` ran before
        // the window opened. Do not add a full-window `.occlude()` around app
        // overlays to compensate — an extra size-full occlude leftover is what
        // once made Settings/sidebars unopenable after close.
        // No extra wrapper here: `main` (inside the stack) already renders
        // the custom titlebar on non-macOS platforms.
        stack.into_any_element()
    }
}
