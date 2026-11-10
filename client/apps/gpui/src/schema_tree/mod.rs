use gpui::{
    Context, DismissEvent, Entity, EventEmitter, ScrollHandle, Subscription, Window, point, px,
};
use gpui_component::menu::PopupMenu;
use sqlly_core::{AzureSqlDiscovery, ConnectionProfile};
use sqlly_services::{ProcedureParameter, Services};
use std::rc::Rc;
#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;
use uuid::Uuid;
#[cfg(target_arch = "wasm32")]
use web_time::Instant;

use crate::entra_grouping::{GroupedProfiles, ServerGroupingMode, group};
use crate::table_actions::ColumnDescriptor;
use crate::theme::Theme;

mod load;
#[cfg(test)]
mod menu_layout_tests;
mod menus;
mod model;
mod pin_groups;
pub(crate) mod redis;
mod render;
mod selection;

pub use load::LoadState;
pub use menus::{
    ExplorerMenuAction, ExplorerTool, MaintenanceMode, MenuAvailability, maintenance_scope,
};
pub use model::{
    FavoriteObject, GroupIdentities, NodeKind, PinnedKind, TreeNode, pinned_for_server,
    toggled_favorites,
};

use load::{
    ForeignKeyCache, apply_expansion, build_hydrated_db_children, classify_load_error,
    collapse_all_nodes, collect_expanded_ids, collect_expansion, column_load_payload,
    copy_identity_for, escape_id_segment, extract_db_from_id, load_children_async,
    merge_hydrated_children, parameter_load_payload, prune_hidden_schemas, reset_subtree_loads,
};
#[cfg(test)]
use menus::action_to_event;
use menus::{
    add_menu_entries, expand_node_by_id, find_object_path, menu_entries_for_context,
    mutations_allowed,
};
#[cfg(test)]
use model::find_node_mut;
use model::{
    collect_server_nodes, expand_server_for_activate, find_node, find_node_any, find_node_any_mut,
    find_server_node, find_server_node_mut, load_favorites, make_azure_tree, make_server_node,
    parent_id_of, parse_column_detail, save_favorites, server_uuid_from_id, sort_pinned_first,
    sort_pinned_first_node,
};
#[cfg(test)]
use render::{FIRST_PAINT_ROW_CAP, FLATTEN_CACHE_BUILDS, FLATTEN_CACHE_HITS, NODE_MATCH_CALLS};
use render::{FlatNode, database_names_from_roots, flatten_tree, flatten_tree_filtered};
// Imports used only by the test module below, which the non-test lib build
// would otherwise flag as unused.
#[cfg(test)]
use gpui::AppContext;
#[cfg(test)]
use load::{
    ServerHealth, copy_names, extract_schema_from_id, is_system_database, is_system_schema,
    object_title, sanitize_display, server_health, unescape_id_segment,
};
#[cfg(test)]
use menus::MenuEntry;
#[cfg(test)]
use render::{
    align_info_for, failed_load_inline_word, object_tooltip, viewport_window, with_load_state,
};
#[cfg(test)]
use sqlly_core::{ConnectionEnvironment, ObjectKind};

pub(crate) const TREE_LEVEL_INDENT: f32 = 12.0;
pub(crate) const TREE_ROW_INSET: f32 = 8.0;

/// Which kind of data-manipulation script to generate from a table's rows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TableScriptKind {
    /// `INSERT INTO … VALUES …` script.
    Insert,
    /// `MERGE … USING (VALUES …) …` upsert script.
    Merge,
    /// Temp-table create + insert script built from the table's rows.
    TempTable,
}

/// Where a generated script should be sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScriptDestination {
    NewQuery,
    Clipboard,
}

/// Format for "Save Data As" table export.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TableSaveFormat {
    Csv,
    Excel,
    SqllyExport,
    NewTempTableImport,
    InsertInto,
    UpdateOrInsert,
}

/// A table or view the tree's multi-selection acts on. Carries the node's own
/// server/database identity so the app runs the action where the object lives,
/// not on whatever connection is active — the same retarget contract as
/// [`SchemaTreeEvent::OpenSqlInNewTab`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BulkObjectRef {
    pub server_id: Option<Uuid>,
    pub database: Option<String>,
    pub schema: String,
    pub name: String,
    pub kind: BulkObjectKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BulkObjectKind {
    Table,
    View,
}

/// The selection banner's actions. Every form lands in a reviewed tab or a
/// dialog — nothing executes blindly.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BulkTreeAction {
    /// One dependency-ordered CREATE script per source database.
    ScriptCreate,
    /// One guarded DROP script per source database.
    ScriptDrop,
    /// Hand the selected tables to the table-export dialog, pre-checked.
    ExportData,
    /// Script one engine maintenance statement per selected table.
    Maintenance {
        op: sqlly_services::maintenance::MaintenanceOp,
    },
}
/// Events emitted by the schema tree for the parent to handle.
#[derive(Clone)]
pub enum SchemaTreeEvent {
    /// User double-clicked a table/view to open a SELECT query. Carries the
    /// server and database the node lives on so the parent can retarget the
    /// active connection to *that* host/database before opening the query —
    /// otherwise the SELECT would run against whatever connection is active.
    SelectObject {
        schema: String,
        name: String,
        server_id: Option<Uuid>,
        database: Option<String>,
    },
    /// Open the visual query builder with this table already on the canvas.
    DesignQuery {
        schema: String,
        name: String,
        server_id: Option<Uuid>,
        database: Option<String>,
    },
    /// User selected a context-menu action that produces SQL. The parent opens
    /// it in a **new** query tab. Carries the node's server/database identity
    /// so the parent retargets the active connection first, exactly like
    /// [`SchemaTreeEvent::SelectObject`] — the action must run where the node
    /// lives, not on whatever connection is active.
    ///
    /// This used to be `InsertSql`, and the handler set the active editor's
    /// text — clearing the undo stack with it, so "Select Top 1000" silently
    /// destroyed unsaved work with no way back. The tree never wants the
    /// current editor: double-clicking a table already opens a new tab
    /// ([`SchemaTreeEvent::SelectObject`]), and SSMS scripts to a new window
    /// too. One contract for both paths, and nothing the user typed is ever at
    /// risk.
    OpenSqlInNewTab {
        sql: String,
        server_id: Option<Uuid>,
        database: Option<String>,
    },
    /// Export the database node as a SQL dump via the export dialog.
    ExportDatabaseSql {
        server_id: Option<Uuid>,
        database: Option<String>,
    },
    /// Run a .sql file against this database via the run-file dialog.
    RunSqlFile {
        server_id: Option<Uuid>,
        database: Option<String>,
    },
    /// Open the Object Browser for this database, optionally scoped to a schema.
    BrowseObjects {
        server_id: Option<Uuid>,
        database: Option<String>,
        schema: Option<String>,
    },
    /// Search this database for a literal value.
    SearchDatabaseValue {
        server_id: Option<Uuid>,
        database: Option<String>,
    },
    /// User asked to script a table's data as INSERT or MERGE. The parent
    /// runs `SELECT * FROM [schema].[table]`, collects the rows, generates
    /// the script via `table_actions`, and inserts it into the editor.
    /// Carries the node's identity for the same retarget contract as
    /// [`SchemaTreeEvent::OpenSqlInNewTab`]: the row read must hit the
    /// node's server/database.
    ScriptTableAs {
        schema: String,
        name: String,
        kind: TableScriptKind,
        server_id: Option<Uuid>,
        database: Option<String>,
    },
    /// A Redis database or key folder finished its SCAN. The parent folds
    /// the key names into command IntelliSense so key slots complete from
    /// the explorer's last scan.
    RedisKeysLoaded {
        profile_id: Uuid,
        database: String,
        keys: Vec<sqlly_intel::redis_complete::KnownKey>,
    },
    /// A table/view's columns finished loading. The parent uses this to enrich
    /// the live completion catalog so `schema.table.` column completion works.
    ColumnsLoaded {
        schema: String,
        name: String,
        columns: Vec<String>,
    },
    /// A stored procedure's parameters finished loading. The parent uses this to
    /// enrich the live completion procedures so EXEC declaration snippets carry
    /// real parameter slots.
    ProcedureParametersLoaded {
        schema: String,
        name: String,
        parameters: Vec<ProcedureParameter>,
    },
    /// A server node's children — its databases — finished loading. Emitted so
    /// the query-bar can re-read the database list for the newly activated
    /// server: the tree loads databases asynchronously and only notifies its own
    /// entity, so without this event the App view never re-renders and the
    /// database dropdown keeps the previous server's list.
    DatabasesLoaded {
        /// Profile id of the server whose databases just loaded.
        profile_id: Uuid,
    },
    /// Open the text in a new tab AND run it at once — the Redis key
    /// browser's Open Value: a read command the user chose by clicking the
    /// key, so there is nothing to review first. Same retarget contract as
    /// [`SchemaTreeEvent::OpenSqlInNewTab`].
    RunCommandInNewTab {
        sql: String,
        /// A tab name that says what the reader shows (`orders — first
        /// 1,000` for the capped list/zset/stream readers); `None` keeps
        /// the tab's automatic name.
        title: Option<String>,
        server_id: Option<Uuid>,
        database: Option<String>,
    },
    /// Open the Redis key view on this key (double-click or Open Value):
    /// its header, paged value and per-type edits, on the key's own
    /// server and logical database. The browser build, which has no key
    /// view, runs the key's reader in a tab instead.
    OpenRedisKey {
        key: String,
        kind: String,
        server_id: Option<Uuid>,
        database: Option<String>,
    },
    /// User activated a server node — Enter on the selected row. Connect to it
    /// (make it the active connection); the parent translates this into its own
    /// connect flow.
    ///
    /// Deliberately *not* emitted for a plain click: clicking a server or one of
    /// its databases browses, and browsing must leave the query tab's connection
    /// alone. The other explicit route is the row's Connect context-menu item,
    /// which the parent handles directly.
    ActivateServer(Uuid),
    /// Escape in the tree asks for the filter to be dropped. The filter text is
    /// owned by the field above the tree, so the request goes up rather than
    /// the tree clearing its own mirror and silently disagreeing with the box
    /// the user can still read.
    FilterCleared,
    /// User clicked a discovered Azure database — adopt it as a saved
    /// connection profile.
    ///
    /// Link only, never connect: the click is the same browsing gesture that
    /// leaves a saved server's connection alone (see [`Self::ActivateServer`]).
    /// The row's "Link & Connect" menu item is what also moves the query tab.
    LinkAzureDatabase(AzureSqlDiscovery),
    /// Expanding a node failed against the server the node belongs to. The
    /// parent inspects the message for conditions it can offer to fix — an
    /// Azure SQL firewall block (error 40615) above all, since browsing the
    /// tree is usually where a blocked IP is discovered, long before any query
    /// is run. The tree itself only paints the node's error state.
    LoadFailed {
        /// Profile id of the server whose load failed; the parent resolves it
        /// back to a profile rather than the tree shipping a copy of one
        /// (profiles carry hydrated credentials at this point).
        profile_id: Uuid,
        message: String,
        /// True when the failed load was the server node's own database
        /// enumeration — the only failure that proves the *server* could not
        /// be reached. Deeper loads (a schema folder, a table's columns) can
        /// fail for reasons that say nothing about reachability, and must
        /// not turn the server dot red.
        server_node: bool,
    },
    /// User selected a typed explorer action from the context menu that
    /// does not map to an existing event variant. The parent handles this
    /// with its own action dispatcher.
    ExplorerActionRequested {
        context: ExplorerActionContext,
        action: ExplorerMenuAction,
    },
    /// The multi-selection banner asked to act on every selected table/view.
    /// Emitted with the full object list gathered at click time; the tree
    /// clears the selection as it emits.
    BulkActionRequested {
        action: BulkTreeAction,
        objects: Vec<BulkObjectRef>,
    },
    /// Pin to Group ▸ New Group… or Rename Group…: ask for a name, then call
    /// [`SchemaTree::create_pin_group`] (with `pin` to put in the new group)
    /// or [`SchemaTree::rename_pin_group`] (when `rename` carries the
    /// group's id and current name).
    PromptPinGroupName {
        server_id: Uuid,
        rename: Option<(Uuid, String)>,
        pin: Option<FavoriteObject>,
    },
}

impl EventEmitter<SchemaTreeEvent> for SchemaTree {}

/// Value carried by a gpui drag started on a table/view/column row.
#[derive(Clone, Debug)]
pub struct DraggedIdentifier {
    pub server_id: Option<Uuid>,
    pub database: Option<String>,
    pub schema: Option<String>,
    /// Table/view name; for a column drag this is the parent object.
    pub object: String,
    /// Set for column/parameter rows.
    pub column: Option<String>,
}

impl DraggedIdentifier {
    pub fn object_display(&self) -> String {
        if let Some(column) = &self.column {
            column.clone()
        } else if let Some(schema) = &self.schema {
            format!("{schema}.{}", self.object)
        } else {
            self.object.clone()
        }
    }
}
/// Lightweight context captured at right-click time. Contains only stable,
/// cheap-to-clone data — no subtree clones, no database handles.
#[derive(Clone, Debug)]
pub struct ExplorerActionContext {
    pub node_id: String,
    pub title: String,
    pub kind: NodeKind,
    pub server_id: Option<Uuid>,
    pub database: Option<String>,
    pub schema: Option<String>,
    pub name: Option<String>,
    /// Parent table for index/statistic nodes (their Properties dialogs
    /// address them as `schema.table.name`). `None` for every other kind.
    pub table: Option<String>,
    /// Raw leaf identifier for the "Copy Name" action — the object's own name,
    /// never the (possibly schema-prefixed, sanitized) display [`Self::title`].
    /// `None` for kinds with no copyable name (e.g. the server root).
    pub copy_name: Option<String>,
    /// Bracket-quoted qualified identifier for "Copy Qualified Name"
    /// (`[schema].[table].[column]`, `]` doubled). `None` when the kind has no
    /// meaningful qualified form. Computed once here so `app.rs` stays a plain
    /// clipboard write and the format lives in one tested place.
    pub copy_qualified: Option<String>,
    /// Column descriptors extracted from the table/view's loaded column
    /// children, used for "CREATE TABLE Script". Empty when the node hasn't
    /// been expanded yet (the DDL degrades to a bare `CREATE TABLE … ()`).
    pub columns: Vec<ColumnDescriptor>,
    /// Connection profile owning the node (tokenized where the tree applied
    /// runtime Entra tokens), so menu actions that hit the engine — like the
    /// Properties editor — don't have to re-resolve it from the id. Boxed to
    /// keep `SchemaTreeEvent::ExplorerActionRequested` from dwarfing the
    /// enum's other variants.
    pub profile: Option<Box<ConnectionProfile>>,
    pub position: gpui::Point<gpui::Pixels>,
}

/// Per-server connection health, tracked independently for every server node.
/// Green must mean "we can talk to it", red "we tried and could not reach it",
/// and the default gray "no connection attempted (or explicitly disconnected)".
/// One server's state says nothing about another's — any number can be
/// `Connected` at the same time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ConnectionState {
    /// Never connected, or explicitly disconnected: the default state.
    #[default]
    Disconnected,
    /// A connect/probe verified we can talk to this server.
    Connected,
    /// The last connect/probe/query attempt failed to reach this server.
    Unreachable,
    /// Last session ended with this server reachable; nothing has verified it
    /// since launch. Rendered aged/dimmed — never as a live green.
    StaleConnected,
    /// Last session ended with this server unreachable; nothing has retried
    /// it since launch. Rendered aged/dimmed.
    StaleUnreachable,
}

impl ConnectionState {
    /// A persisted last-known state restored at launch, as opposed to health
    /// observed in this session.
    pub fn is_stale(self) -> bool {
        matches!(self, Self::StaleConnected | Self::StaleUnreachable)
    }
}

pub struct SchemaTree {
    services: Services,
    /// Resolves the services for each server the tree expands. The tree
    /// browses every saved connection, not just the active one, so a server
    /// node must load through ITS route (in-process, Via-Relay session, or
    /// the browser's relay proxy) rather than whichever services the active
    /// connection installed. `None` (tests, standalone trees) falls back to
    /// [`Self::services`].
    router: Option<crate::service_router::ServiceRouter>,
    log: crate::log_console::DiagnosticLog,
    /// All saved connection profiles, rendered as top-level server nodes.
    profiles: Vec<ConnectionProfile>,
    /// Per-connection runtime Microsoft Entra **SQL** access tokens, minted at
    /// connect time (keyed by `ConnectionProfile.id`). Held here — never
    /// persisted to the `ConnectionStore` — and re-applied to `profiles` on
    /// every [`SchemaTree::set_connections`] so lazy schema loads
    /// (`load_children` → `profile_for_node_id`) carry the token the engine's
    /// TDS layer needs for `EntraBrowser` auth. Without this, expanding a
    /// server/database node after connect would hit the engine with a
    /// tokenless profile and fail authentication.
    runtime_entra_tokens: std::collections::HashMap<Uuid, String>,
    runtime_cloud_iam_tokens: std::collections::HashMap<Uuid, String>,
    /// Shared SQL-token minter, injected by the app after construction. Lets a
    /// server/database expansion re-mint an expired Entra token before dialing,
    /// instead of reusing the (possibly hours-stale) token stamped at connect
    /// time — the reason expansion used to fail with "token expired" while a
    /// query window against the same server still worked. `None` in tests.
    entra_sql_token_provider: Option<crate::azure_sql_token::EntraSqlTokenProvider>,
    /// Per-(connection, database) cache of the database-wide foreign-key listing
    /// so expanding many tables' Foreign Keys folders reuses one query. Cleared
    /// on refresh / connection change.
    fk_cache: ForeignKeyCache,
    /// Discovered Azure SQL resources, rendered as an Azure hierarchy below
    /// the saved-connection server nodes.
    azure_discoveries: Vec<AzureSqlDiscovery>,
    /// How server nodes are bucketed into grouping headers.
    grouping: ServerGroupingMode,
    /// Colors and icons for the client/project/environment entities, so a group
    /// header can show which environment it is. Pushed in by the sidebar, which
    /// owns the single read of the connection store.
    entities: GroupIdentities,
    /// The server bound to the focused query window, for the query-bar
    /// database dropdown (`database_names`) and engine-type fallback. This is
    /// a window-scope pointer, not a statement about connectivity — per-server
    /// connection health lives in `connection_states`.
    active: Option<Uuid>,
    /// Observable per-server connection health. Absence means
    /// [`ConnectionState::Disconnected`] (the default gray state). Multiple
    /// servers can be `Connected` at once; the dot in the rail is derived from
    /// this map, never from which server is "active".
    connection_states: std::collections::HashMap<Uuid, ConnectionState>,
    /// Whether the persisted last-known health has been restored (first
    /// [`Self::set_connections`] of the session only).
    health_seeded: bool,
    roots: Vec<TreeNode>,
    /// Bumped on every `roots` mutation so the flatten cache can skip a
    /// per-frame walk when the tree and filter needle have not changed.
    tree_revision: u64,
    /// `(tree_revision, cache_key) → flattened rows`. Shared by render and
    /// keyboard navigation. `Rc` so a cache hit is a pointer bump, not a
    /// `Vec`/`NodeKind` clone of every visible row.
    #[allow(clippy::type_complexity)]
    flatten_cache: std::cell::RefCell<Option<(u64, String, Rc<Vec<FlatNode>>)>>,
    /// Server-explorer organization settings (Settings ▸ Data Model). Cached
    /// here; refreshed via [`SchemaTree::reload_data_model_prefs`] when the
    /// preferences dialog closes.
    data_model_prefs: crate::preferences::DataModelPreferences,
    /// Node ids that were expanded when a refresh dropped their subtree.
    /// As reloaded children arrive, ids found here are re-expanded and
    /// re-fetched, so a refresh restores the user's drill-down in place
    /// (with fresh data) instead of collapsing everything to the server.
    reexpand_after_refresh: std::collections::HashSet<String>,
    /// Redis key browser: the MATCH pattern each database node scans with
    /// (Filter Keys…), keyed by node id. Absent means every key.
    redis_filters: std::collections::HashMap<String, String>,
    /// Compiled explorer filter. Empty means unfiltered.
    filter: crate::explorer_search::ExplorerQuery,
    /// What the tree looked like when the current filter session began, so
    /// clearing the filter puts it back: the filter reveals matches under
    /// collapsed folders, and clicks on those revealed rows flip stored
    /// expansion state that the unfiltered view then honours — leaving every
    /// folder the search touched hanging open. `None` outside a session.
    pre_filter: Option<PreFilterState>,
    /// Compiled once per filter change, reused per node during flatten.
    compiled: Rc<crate::explorer_search::CompiledQuery>,
    /// Accumulated type-ahead keys, and when the last one landed. Cleared after
    /// a pause so `ac` then later `in` are two searches, not `acin`.
    type_ahead: String,
    type_ahead_at: Option<Instant>,
    /// The node id the user last clicked — the tree's selection.
    ///
    /// Clicking a leaf used to mark nothing at all: `last_click_id` below is a
    /// double-click timer, not a selection, so only the *active server* carried
    /// a persistent mark and the row a context menu was about to act on was
    /// unidentifiable. Set on left-click and, critically, at the top of the
    /// right-click handler — a menu must visibly belong to the row it will act
    /// on.
    selected: Option<String>,
    /// Ctrl/Shift-click multi-selection on table/view rows, driving the bulk
    /// banner. Distinct from `selected` (the single keyboard/context-menu
    /// mark): the banner acts on this set, the menu on the mark.
    multi: selection::MultiSelection,
    last_click_id: Option<String>,
    last_click_time: Option<Instant>,
    context_menu: Option<ExplorerActionContext>,
    /// The popup menu entity for the open context menu, rendered inside the
    /// anchored overlay. Built when the menu opens; `context_menu` above keeps
    /// carrying the node context and the anchor position.
    context_menu_view: Option<Entity<PopupMenu>>,
    /// Keeps the open menu's `DismissEvent` subscription alive while the menu
    /// is up; dropped when the menu closes.
    context_menu_dismiss: Option<Subscription>,
    scroll: ScrollHandle,
    /// Favorited objects (persisted), shown in a strip above the tree.
    favorites: Vec<FavoriteObject>,
    /// Collapse state of the Favorites strip.
    favorites_expanded: bool,
    /// Transient notice under the Favorites strip (e.g. "not loaded yet").
    favorites_notice: Option<String>,
    /// Named pin groups (all servers), persisted beside the pin list.
    pin_groups: Vec<pin_groups::PinGroup>,
    /// True while the vertical scrollbar is being dragged.
    ///
    /// The thumb used to be a painted rectangle with no handlers at all: you
    /// could see it, aim at it, press, drag — and nothing moved, which reads as
    /// a frozen app rather than a missing feature.
    scrollbar_dragging: bool,
    /// Keyboard focus for the tree. GPUI exposes no accessibility tree for
    /// in-window content, so VoiceOver cannot reach a custom tree in any GPUI
    /// app today — but keyboard-only navigation is entirely ours to provide,
    /// and a source list you can only operate with a mouse excludes people for
    /// no good reason. Arrow keys walk and expand; Enter opens.
    focus_handle: gpui::FocusHandle,
}

impl SchemaTree {
    pub fn new(
        services: Services,
        log: crate::log_console::DiagnosticLog,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        // Folder titles are stored on the nodes (and their lowercase filter
        // copies); re-title them when the language changes.
        cx.observe_global::<crate::localization::LocalizationChanged>(|this, cx| {
            this.relocalize(cx);
        })
        .detach();
        let initial_pin_groups = pin_groups::load_pin_groups();
        Self {
            focus_handle,
            services,
            router: None,
            log,
            profiles: Vec::new(),
            azure_discoveries: Vec::new(),
            runtime_entra_tokens: std::collections::HashMap::new(),
            runtime_cloud_iam_tokens: std::collections::HashMap::new(),
            entra_sql_token_provider: None,
            fk_cache: ForeignKeyCache::default(),
            grouping: ServerGroupingMode::None,
            entities: GroupIdentities::default(),
            active: None,
            connection_states: std::collections::HashMap::new(),
            health_seeded: false,
            roots: Vec::new(),
            tree_revision: 0,
            flatten_cache: std::cell::RefCell::new(None),
            data_model_prefs: crate::preferences::DataModelPreferences::load(),
            reexpand_after_refresh: std::collections::HashSet::new(),
            redis_filters: std::collections::HashMap::new(),
            filter: crate::explorer_search::ExplorerQuery::parse("", Default::default()),
            pre_filter: None,
            compiled: Rc::new(crate::explorer_search::CompiledQuery::compile(
                &crate::explorer_search::ExplorerQuery::parse("", Default::default()),
            )),
            type_ahead: String::new(),
            type_ahead_at: None,
            selected: None,
            multi: selection::MultiSelection::default(),
            last_click_id: None,
            last_click_time: None,
            context_menu: None,
            context_menu_view: None,
            context_menu_dismiss: None,
            scroll: ScrollHandle::new(),
            favorites: {
                let mut favorites = load_favorites();
                pin_groups::drop_unknown_groups(&mut favorites, &initial_pin_groups);
                favorites
            },
            favorites_expanded: true,
            favorites_notice: None,
            pin_groups: initial_pin_groups,
            scrollbar_dragging: false,
        }
    }

    /// Swap the engine services this tree drives metadata through. Used when a
    /// connection is opened so a Via-Relay connection's schema loads over the
    /// relay; a Direct connection restores the in-process services. Later
    /// metadata loads clone `self.services` at call time, so replacing the field
    /// is enough for every subsequent expansion/refresh.
    pub fn set_services(&mut self, services: Services, _cx: &mut Context<Self>) {
        self.services = services;
    }

    /// Route each server's metadata loads through `router`, per profile.
    pub fn set_router(&mut self, router: crate::service_router::ServiceRouter) {
        self.router = Some(router);
    }

    /// The services that load `profile`'s children: its own route when a
    /// router is installed, else the tree-wide services.
    fn services_for(&self, profile: &ConnectionProfile) -> Services {
        match &self.router {
            Some(router) => router.services_for(profile),
            None => self.services.clone(),
        }
    }

    /// The engine type of a server node, for rendering SQL with that engine's
    /// pattern equivalents. Falls back to the active connection, then to the
    /// default (SQL Server) when the node has no server id.
    pub fn database_type_for(&self, server_id: Option<Uuid>) -> sqlly_lang::sqlmodel::DatabaseType {
        server_id
            .or(self.active)
            .and_then(|id| self.profiles.iter().find(|p| p.id.0 == id))
            .map(|p| p.database_type)
            .unwrap_or_default()
    }

    /// Replace the set of saved connections shown as server roots. Preserves the
    /// expansion/loaded state of any server that survives the rebuild (keyed by
    /// its stable `srv:{uuid}` id), so refreshing the connection list does not
    /// collapse a server the user has drilled into.
    pub fn set_connections(&mut self, profiles: Vec<ConnectionProfile>, cx: &mut Context<Self>) {
        self.profiles = profiles;
        // A deleted connection must not leave a dangling health entry behind.
        let profiles = &self.profiles;
        self.connection_states
            .retain(|id, _| profiles.iter().any(|p| p.id.0 == *id));
        // First profile load of this session: restore each server's persisted
        // last-known health as an explicitly *stale* state. The dots stay
        // informative across a restart without a persisted green ever
        // claiming "we can talk to it right now". Later profile updates never
        // re-seed — an explicit disconnect must stay gray.
        if !self.health_seeded {
            self.health_seeded = true;
            for profile in &self.profiles {
                let state = match profile.status.last_test_status {
                    Some(sqlly_core::ConnectionTestStatus::Succeeded) => {
                        ConnectionState::StaleConnected
                    }
                    Some(sqlly_core::ConnectionTestStatus::Failed) => {
                        ConnectionState::StaleUnreachable
                    }
                    _ => continue,
                };
                self.connection_states.entry(profile.id.0).or_insert(state);
            }
        }
        // A connection edit may move the server out from under a cached listing.
        self.fk_cache.clear();
        self.reapply_runtime_entra_tokens();
        self.reapply_runtime_cloud_iam_tokens();
        self.rebuild_roots();
        cx.notify();
    }

    /// Attach a runtime Entra SQL access token for a connection so subsequent
    /// lazy schema loads authenticate. Called by the app view immediately
    /// after a token is minted at connect time. The token is stored in-memory
    /// only (never written to the `ConnectionStore`) and survives connection
    /// list refreshes via [`Self::reapply_runtime_entra_tokens`].
    pub fn set_runtime_entra_token(&mut self, id: Uuid, token: String, cx: &mut Context<Self>) {
        self.runtime_entra_tokens.insert(id, token);
        self.reapply_runtime_entra_tokens();
        cx.notify();
    }

    pub fn set_runtime_cloud_iam_token(&mut self, id: Uuid, token: String, cx: &mut Context<Self>) {
        self.runtime_cloud_iam_tokens.insert(id, token);
        self.reapply_runtime_cloud_iam_tokens();
        cx.notify();
    }

    /// Inject the shared SQL-token provider so a node expansion can re-mint an
    /// expired Entra token before dialing. Called once by the app after the
    /// sidebar is built.
    pub fn set_entra_sql_token_provider(
        &mut self,
        provider: crate::azure_sql_token::EntraSqlTokenProvider,
    ) {
        self.entra_sql_token_provider = Some(provider);
    }

    /// Stamp each held runtime token onto its matching in-memory profile. A
    /// `ConnectionStore` refresh rebuilds `self.profiles` from disk (which
    /// never contains secrets), so the token must be re-applied here or it
    /// would be silently lost on the next refresh.
    fn reapply_runtime_entra_tokens(&mut self) {
        for profile in &mut self.profiles {
            if let Some(token) = self.runtime_entra_tokens.get(&profile.id.0) {
                profile.runtime_entra_access_token = Some(token.clone());
            }
        }
    }

    fn reapply_runtime_cloud_iam_tokens(&mut self) {
        for profile in &mut self.profiles {
            if let Some(token) = self.runtime_cloud_iam_tokens.get(&profile.id.0) {
                profile.runtime_cloud_iam_token = Some(token.clone());
            }
        }
    }

    /// Replace the set of discovered Azure SQL resources. The Azure hierarchy
    /// is rebuilt as additional roots below the saved-connection server nodes.
    pub fn set_azure_discoveries(
        &mut self,
        discoveries: Vec<AzureSqlDiscovery>,
        cx: &mut Context<Self>,
    ) {
        self.azure_discoveries = discoveries;
        self.rebuild_roots();
        cx.notify();
    }

    /// Change the grouping dimension (headers) and rebuild the roots, again
    /// preserving per-server drill-down state.
    pub fn set_grouping(&mut self, grouping: ServerGroupingMode, cx: &mut Context<Self>) {
        if self.grouping == grouping {
            return;
        }
        self.grouping = grouping;
        self.rebuild_roots();
        cx.notify();
    }

    /// Resolve the copy payloads for a node against the loaded tree. Thin wrapper
    /// over [`copy_identity_for`] bound to `self.roots`.
    fn copy_identity(&self, node_id: &str, kind: &NodeKind) -> (Option<String>, Option<String>) {
        copy_identity_for(&self.roots, node_id, kind)
    }

    /// Select `node_id` and open its context menu at `position`.
    ///
    /// Shared by right-click and the row's `…` button so the two can't drift.
    /// Selecting first is the point: a menu offering DROP TABLE has to visibly
    /// belong to the row it will act on before it opens.
    fn open_context_menu(
        &mut self,
        node_id: String,
        kind: NodeKind,
        title: String,
        position: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selected = Some(node_id.clone());
        let server_id = match &kind {
            NodeKind::Server { id } => Some(*id),
            _ => server_uuid_from_id(&node_id),
        };
        let database = match &kind {
            NodeKind::Database(name) => Some(name.clone()),
            _ => {
                let db = extract_db_from_id(&node_id);
                if db.is_empty() { None } else { Some(db) }
            }
        };
        let (schema, name) = match &kind {
            NodeKind::Table { schema, name }
            | NodeKind::View { schema, name }
            | NodeKind::Procedure { schema, name }
            | NodeKind::Function { schema, name }
            | NodeKind::Type { schema, name }
            | NodeKind::Trigger { schema, name, .. }
            | NodeKind::Sequence { schema, name }
            | NodeKind::Synonym { schema, name, .. }
            | NodeKind::Event { schema, name }
            | NodeKind::MaterializedView { schema, name } => {
                (Some(schema.clone()), Some(name.clone()))
            }
            NodeKind::Index { schema, name, .. } | NodeKind::Statistic { schema, name, .. } => {
                (Some(schema.clone()), Some(name.clone()))
            }
            NodeKind::Constraint { schema, name, .. } => (Some(schema.clone()), Some(name.clone())),
            NodeKind::Extension { name, .. } => (None, Some(name.clone())),
            NodeKind::SchemaFolder { schema } => (Some(schema.clone()), None),
            _ => (None, None),
        };
        let table = match &kind {
            NodeKind::Index { table, .. } | NodeKind::Statistic { table, .. } => {
                Some(table.clone())
            }
            NodeKind::Trigger { table, .. } => table.clone(),
            NodeKind::Constraint { table, .. } => Some(table.clone()),
            _ => None,
        };
        let columns = match &kind {
            NodeKind::Table { .. } | NodeKind::View { .. } | NodeKind::MaterializedView { .. } => {
                self.extract_column_descriptors(&node_id)
            }
            _ => Vec::new(),
        };
        let profile = self.profile_for_node_id(&node_id).map(Box::new);
        let (copy_name, copy_qualified) = self.copy_identity(&node_id, &kind);
        let menu_ctx = ExplorerActionContext {
            node_id,
            title,
            kind,
            server_id,
            database,
            schema,
            name,
            table,
            copy_name,
            copy_qualified,
            columns,
            profile,
            position,
        };

        let availability = self.menu_availability(&menu_ctx);
        let entries = menu_entries_for_context(&menu_ctx, &availability);

        let tree = cx.entity();
        let view = PopupMenu::build(window, cx, {
            let menu_ctx = menu_ctx.clone();
            move |menu, window, cx| {
                // A 220px minimum width so the explorer's context menus line up
                // instead of jittering to each menu's longest label. It is a
                // floor, not a fixed width: a few entries ("Generate SQL
                // involving this procedure") are legitimately wider and must not
                // be clipped.
                let menu = menu.min_w(px(Theme::scaled(220.0)));
                add_menu_entries(menu, &entries, &tree, &menu_ctx, window, cx)
            }
        });
        // The menu dismisses itself (Escape, click-out, item confirm); mirror
        // that by dropping the tree's menu state so the overlay unmounts.
        let dismiss = cx.subscribe(&view, |this, _menu, _: &DismissEvent, cx| {
            this.close_context_menu(cx);
        });
        // Suppress tooltips for as long as the menu is up. A row's tooltip
        // timer starts on the mouse-move that precedes the right-click and
        // fires regardless of the click, so without this the tooltip lands on
        // top of the menu it opened.
        if self.context_menu.is_none() {
            crate::tooltip::menu_opened(cx);
        }
        self.context_menu = Some(menu_ctx);
        self.context_menu_view = Some(view);
        self.context_menu_dismiss = Some(dismiss);
        cx.notify();
    }

    /// Compute the feature-availability flags that gate a node's context menu,
    /// keyed to *that node's own server* rather than whichever connection is
    /// globally active.
    pub(crate) fn menu_availability(&self, menu_ctx: &ExplorerActionContext) -> MenuAvailability {
        // Read-only actions retarget to *this node's* own server before they
        // run (see `resolve_explorer_target`), so they're available whenever
        // that server has a resolvable profile — not only when it happens to be
        // the single globally-active connection. Gating on `self.active` left
        // every read action on a database disabled whenever another server (or
        // none) was active, even though opening a query would have connected to
        // the right server on demand.
        let has_connection = menu_ctx
            .server_id
            .is_some_and(|sid| self.profiles.iter().any(|p| p.id.0 == sid));
        // Ask *this node's* connection about its guardrails, not whichever
        // server happens to be active.
        let safety = menu_ctx
            .server_id
            .and_then(|sid| self.profiles.iter().find(|p| p.id.0 == sid))
            .map(crate::mutation_review::SafetyFeatures::from_profile);
        MenuAvailability {
            is_connected: menu_ctx
                .server_id
                .is_some_and(|sid| self.connection_state(sid) == ConnectionState::Connected),
            is_loading: find_node(&self.roots, &menu_ctx.node_id)
                .map(|n| matches!(n.load_state, LoadState::Loading))
                .unwrap_or(false),
            can_mutate: has_connection && safety.is_some_and(mutations_allowed),
            has_connection,
            db: self.database_type_for(menu_ctx.server_id),
            custom_actions: custom_actions_for(&menu_ctx.kind),
            connections_read_only: crate::connection::connections_are_proxied(),
            redis_scan_more: self
                .redis_listing_note(&menu_ctx.node_id)
                .is_some_and(|(_, listing)| listing.scan_incomplete()),
            pin_groups: menu_ctx
                .server_id
                .map(|sid| {
                    pin_groups::groups_for_server(&self.pin_groups, sid)
                        .into_iter()
                        .map(|g| (g.id, g.name.clone()))
                        .collect()
                })
                .unwrap_or_default(),
        }
    }

    /// Multi-select click routing, ahead of the normal click behavior. Returns
    /// `true` when the click was consumed by selection management (ctrl/shift
    /// on a table or view) and the row's usual behavior (expand, open) must
    /// not run. Plain clicks fall through after collapsing the set.
    pub(crate) fn handle_multi_click(
        &mut self,
        node_id: &str,
        kind: &NodeKind,
        modifiers: &gpui::Modifiers,
        cx: &mut Context<Self>,
    ) -> bool {
        let selectable = matches!(kind, NodeKind::Table { .. } | NodeKind::View { .. });
        if selectable && crate::keybindings::primary_modifier(modifiers) {
            // Ctrl/Cmd toggles. The toggled row also takes the single mark, so
            // the keyboard and any following right-click know where they are.
            self.selected = Some(node_id.to_string());
            self.multi.toggle(node_id);
            cx.notify();
            return true;
        }
        if selectable && modifiers.shift {
            self.selected = Some(node_id.to_string());
            let visible_selectable: Vec<String> = self
                .visible_rows()
                .iter()
                .filter(|row| matches!(row.kind, NodeKind::Table { .. } | NodeKind::View { .. }))
                .map(|row| row.id.to_string())
                .collect();
            self.multi.range_to(node_id, &visible_selectable);
            cx.notify();
            return true;
        }
        // A plain click collapses the set (and re-anchors a later shift range).
        if self.multi.plain_click(node_id) {
            cx.notify();
        }
        false
    }

    /// The multi-selection as bulk-action objects, in click order.
    fn bulk_objects(&self) -> Vec<BulkObjectRef> {
        self.multi
            .members()
            .iter()
            .filter_map(|id| {
                let node = find_node_any(&self.roots, id)?;
                let (schema, name, kind) = match &node.kind {
                    NodeKind::Table { schema, name } => (schema, name, BulkObjectKind::Table),
                    NodeKind::View { schema, name } => (schema, name, BulkObjectKind::View),
                    _ => return None,
                };
                Some(BulkObjectRef {
                    server_id: server_uuid_from_id(id),
                    database: Some(extract_db_from_id(id)).filter(|d| !d.is_empty()),
                    schema: schema.clone(),
                    name: name.clone(),
                    kind,
                })
            })
            .collect()
    }

    /// Emit the banner's action with the gathered selection, then clear both
    /// the set and any banner menu. The selection is consumed by the action:
    /// re-running it means selecting again, and a stale set must never drive a
    /// second script.
    pub(crate) fn emit_bulk_action(&mut self, action: BulkTreeAction, cx: &mut Context<Self>) {
        let objects = self.bulk_objects();
        self.multi.clear();
        self.close_context_menu(cx);
        // Every member stale (nothing left to act on): the set is still
        // cleared above, so the banner can never get stuck.
        if !objects.is_empty() {
            cx.emit(SchemaTreeEvent::BulkActionRequested { action, objects });
        }
        cx.notify();
    }

    /// The banner's Maintenance ▸ popup: the table-scope maintenance ops of
    /// the first selected object's engine, each scripting to a reviewed tab.
    /// Reuses the context menu's overlay machinery with a banner position.
    pub(crate) fn open_maintenance_menu(
        &mut self,
        position: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(server_id) = self
            .multi
            .members()
            .first()
            .and_then(|id| server_uuid_from_id(id))
        else {
            return;
        };
        let db = self.database_type_for(Some(server_id));
        let Some(dialect) = sqlly_services::tool_dialects::maintenance_dialect(db) else {
            return;
        };
        let ops = sqlly_services::maintenance::ops_for(
            dialect,
            sqlly_services::maintenance::MaintenanceScope::Table,
        );
        if ops.is_empty() {
            return;
        }
        let tree = cx.entity();
        let view = gpui_component::menu::PopupMenu::build(window, cx, move |menu, _window, _cx| {
            let mut menu = menu.min_w(px(Theme::scaled(200.0)));
            for op in ops {
                let tree = tree.clone();
                let label = op
                    .describe(
                        dialect,
                        sqlly_services::maintenance::MaintenanceScope::Table,
                    )
                    .map(|text| text.script_label.to_string())
                    .unwrap_or_else(|| {
                        crate::localization::text("explorer.schema_tree.maintenance_fallback")
                    });
                menu = menu.item(gpui_component::menu::PopupMenuItem::new(label).on_click(
                    move |_ev: &gpui::ClickEvent, _window, app| {
                        tree.update(app, |this, cx| {
                            this.emit_bulk_action(BulkTreeAction::Maintenance { op }, cx);
                        });
                    },
                ));
            }
            menu
        });
        let dismiss = cx.subscribe(&view, |this, _menu, _: &DismissEvent, cx| {
            this.close_context_menu(cx);
        });
        if self.context_menu.is_none() {
            crate::tooltip::menu_opened(cx);
        }
        self.context_menu = Some(ExplorerActionContext {
            node_id: String::new(),
            title: String::new(),
            kind: NodeKind::TablesFolder,
            server_id: None,
            database: None,
            schema: None,
            name: None,
            table: None,
            copy_name: None,
            copy_qualified: None,
            columns: Vec::new(),
            profile: None,
            position,
        });
        self.context_menu_view = Some(view);
        self.context_menu_dismiss = Some(dismiss);
        cx.notify();
    }

    /// Drop all state belonging to an open context menu, if any.
    pub(crate) fn close_context_menu(&mut self, cx: &mut Context<Self>) {
        if self.context_menu.is_some() || self.context_menu_view.is_some() {
            let was_open = self.context_menu.is_some();
            self.context_menu = None;
            self.context_menu_view = None;
            self.context_menu_dismiss = None;
            if was_open {
                crate::tooltip::menu_closed(cx);
            }
            cx.notify();
        }
    }

    /// Build explorer action context for the keyboard-selected node.
    pub(crate) fn explorer_action_context_for_selection(&self) -> Option<ExplorerActionContext> {
        let id = self.selected.clone()?;
        let node = find_node_any(&self.roots, &id)?;
        if matches!(node.kind, NodeKind::Group { .. }) {
            return None;
        }
        let kind = node.kind.clone();
        let title = node.title.to_string();
        let server_id = match &kind {
            NodeKind::Server { id } => Some(*id),
            _ => server_uuid_from_id(&id),
        };
        let database = match &kind {
            NodeKind::Database(name) => Some(name.clone()),
            _ => {
                let db = extract_db_from_id(&id);
                if db.is_empty() { None } else { Some(db) }
            }
        };
        let (schema, name) = match &kind {
            NodeKind::Table { schema, name }
            | NodeKind::View { schema, name }
            | NodeKind::Procedure { schema, name }
            | NodeKind::Function { schema, name }
            | NodeKind::Type { schema, name }
            | NodeKind::Trigger { schema, name, .. }
            | NodeKind::Sequence { schema, name }
            | NodeKind::Synonym { schema, name, .. }
            | NodeKind::Event { schema, name }
            | NodeKind::MaterializedView { schema, name } => {
                (Some(schema.clone()), Some(name.clone()))
            }
            NodeKind::Index { schema, name, .. } | NodeKind::Statistic { schema, name, .. } => {
                (Some(schema.clone()), Some(name.clone()))
            }
            NodeKind::Constraint { schema, name, .. } => (Some(schema.clone()), Some(name.clone())),
            NodeKind::Extension { name, .. } => (None, Some(name.clone())),
            NodeKind::SchemaFolder { schema } => (Some(schema.clone()), None),
            _ => (None, None),
        };
        let table = match &kind {
            NodeKind::Index { table, .. } | NodeKind::Statistic { table, .. } => {
                Some(table.clone())
            }
            NodeKind::Trigger { table, .. } => table.clone(),
            NodeKind::Constraint { table, .. } => Some(table.clone()),
            _ => None,
        };
        let columns = match &kind {
            NodeKind::Table { .. } | NodeKind::View { .. } => self.extract_column_descriptors(&id),
            _ => Vec::new(),
        };
        let profile = self.profile_for_node_id(&id).map(Box::new);
        let (copy_name, copy_qualified) = self.copy_identity(&id, &kind);
        Some(ExplorerActionContext {
            node_id: id,
            title,
            kind,
            server_id,
            database,
            schema,
            name,
            table,
            copy_name,
            copy_qualified,
            columns,
            profile,
            position: point(px(0.0), px(0.0)),
        })
    }

    /// Scroll so that the pointer's position along the track maps to the same
    /// position in the content. Shared by the initial click and each drag move,
    /// so a press followed by a drag is continuous.
    fn scroll_to_pointer(&self, y: gpui::Pixels) {
        let bounds = self.scroll.bounds();
        if bounds.size.height <= px(0.0) {
            return;
        }
        let ratio = ((y - bounds.origin.y) / bounds.size.height).clamp(0.0, 1.0);
        let offset = self.scroll.offset();
        self.scroll
            .set_offset(point(offset.x, -(self.scroll.max_offset().y * ratio)));
    }

    /// The rows currently on screen, in order — the list the keyboard walks.
    /// Built from the same flatten the renderer uses, so what Down moves to is
    /// always what the user can see.
    fn visible_rows(&self) -> Rc<Vec<FlatNode>> {
        self.cached_flatten()
    }

    /// Re-title fixed-wording folders and notes in the current language.
    fn relocalize(&mut self, cx: &mut Context<Self>) {
        if model::relocalize_titles(&mut self.roots) {
            self.bump_tree_revision();
        }
        cx.notify();
    }

    fn bump_tree_revision(&mut self) {
        self.tree_revision = self.tree_revision.wrapping_add(1);
        // Every structural change (reload, refresh, connection list rebuild)
        // passes through here: drop selected rows that no longer exist.
        self.prune_multi_selection();
    }

    /// Drop multi-selection members whose table/view node is gone (a refresh
    /// cleared its folder, the connection was removed). Members under a
    /// *collapsed* parent stay selected: their node still exists, the banner
    /// counts them, and the bulk action acts on them.
    fn prune_multi_selection(&mut self) {
        if self.multi.is_empty() {
            return;
        }
        let roots = &self.roots;
        self.multi.retain(|id| {
            find_node_any(roots, id)
                .is_some_and(|n| matches!(n.kind, NodeKind::Table { .. } | NodeKind::View { .. }))
        });
    }

    fn cached_flatten(&self) -> Rc<Vec<FlatNode>> {
        let key = self.filter.cache_key();
        if let Some((rev, cached_key, rows)) = self.flatten_cache.borrow().as_ref()
            && *rev == self.tree_revision
            && cached_key.as_str() == key
        {
            #[cfg(test)]
            FLATTEN_CACHE_HITS.with(|c| c.set(c.get() + 1));
            return Rc::clone(rows);
        }
        #[cfg(test)]
        FLATTEN_CACHE_BUILDS.with(|c| c.set(c.get() + 1));
        let mut visible = Vec::new();
        if self.filter.is_empty() {
            flatten_tree(&self.roots, &mut visible);
        } else {
            flatten_tree_filtered(&self.roots, &mut visible, None, &self.compiled);
        }
        let rows = Rc::new(visible);
        self.flatten_cache
            .borrow_mut()
            .replace((self.tree_revision, key, Rc::clone(&rows)));
        rows
    }

    /// Select the row at `index` in the visible list and bring it into view.
    fn select_index(&mut self, rows: &[FlatNode], index: usize, cx: &mut Context<Self>) {
        let Some(row) = rows.get(index) else {
            return;
        };
        self.selected = Some(row.id.to_string());
        self.scroll_row_into_view(index);
        cx.notify();
    }

    /// The pin identity of a menu's object: a pinnable explorer node, or the
    /// pinned row of a group folder. `None` for anything else.
    fn favorite_for_ctx(&self, ctx: &ExplorerActionContext) -> Option<FavoriteObject> {
        let server_id = ctx.server_id?;
        if let NodeKind::Pinned { index } = &ctx.kind {
            return pinned_for_server(&self.favorites, server_id)
                .get(*index)
                .map(|f| (*f).clone());
        }
        let kind = model::pinned_kind_from_node(&ctx.kind)?;
        let (name, database) = match &ctx.kind {
            NodeKind::Database(db) => (db.clone(), None),
            _ => (
                ctx.name.clone().filter(|name| !name.is_empty())?,
                ctx.database.clone(),
            ),
        };
        Some(FavoriteObject {
            server_id,
            database,
            schema: ctx.schema.clone().filter(|schema| !schema.is_empty()),
            name,
            kind,
            order: 0,
            group: None,
        })
    }

    /// Pin to Group / Move to Group: put the menu's object in `group`
    /// (`None` = ungrouped), pinning it first if needed.
    fn pin_to_group(
        &mut self,
        ctx: &ExplorerActionContext,
        group: Option<Uuid>,
        cx: &mut Context<Self>,
    ) {
        let Some(favorite) = self.favorite_for_ctx(ctx) else {
            return;
        };
        self.favorites =
            pin_groups::assign_pin_group(std::mem::take(&mut self.favorites), &favorite, group);
        self.persist_pinned_and_rebuild(cx);
    }

    /// Create a named pin group on `server_id`, optionally putting `pin` in
    /// it. Returns the localized reason when the name is refused.
    pub fn create_pin_group(
        &mut self,
        server_id: Uuid,
        name: &str,
        pin: Option<FavoriteObject>,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let group = pin_groups::create_pin_group(&self.pin_groups, server_id, name, Uuid::new_v4())
            .map_err(pin_groups::PinGroupNameError::message)?;
        let id = group.id;
        self.pin_groups.push(group);
        pin_groups::save_pin_groups(&self.pin_groups);
        if let Some(pin) = pin {
            self.favorites =
                pin_groups::assign_pin_group(std::mem::take(&mut self.favorites), &pin, Some(id));
        }
        self.persist_pinned_and_rebuild(cx);
        Ok(())
    }

    /// Rename a pin group. Returns the localized reason when the name is
    /// refused.
    pub fn rename_pin_group(
        &mut self,
        group: Uuid,
        name: &str,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        pin_groups::rename_pin_group(&mut self.pin_groups, group, name)
            .map_err(pin_groups::PinGroupNameError::message)?;
        pin_groups::save_pin_groups(&self.pin_groups);
        self.rebuild_roots();
        cx.notify();
        Ok(())
    }

    /// Delete a pin group; its objects stay pinned, ungrouped.
    fn delete_pin_group(&mut self, group: Uuid, cx: &mut Context<Self>) {
        pin_groups::delete_pin_group(&mut self.pin_groups, &mut self.favorites, group);
        pin_groups::save_pin_groups(&self.pin_groups);
        self.persist_pinned_and_rebuild(cx);
    }

    fn pin_group_name(&self, group: Uuid) -> Option<String> {
        self.pin_groups
            .iter()
            .find(|g| g.id == group)
            .map(|g| g.name.clone())
    }

    /// Add or remove the context object from the pinned list (persisted).
    fn toggle_favorite(&mut self, ctx: &ExplorerActionContext, cx: &mut Context<Self>) {
        if matches!(ctx.kind, NodeKind::Pinned { .. }) {
            return;
        }
        let Some(favorite) = self.favorite_for_ctx(ctx) else {
            return;
        };
        self.favorites = toggled_favorites(std::mem::take(&mut self.favorites), favorite);
        save_favorites(&self.favorites);
        self.favorites_notice = None;
        self.rebuild_roots();
        cx.notify();
    }

    /// Click on a pinned object: reveal it in the loaded tree, or explain why not.
    fn open_favorite(&mut self, server_id: Uuid, index: usize, cx: &mut Context<Self>) {
        let pins: Vec<FavoriteObject> = pinned_for_server(&self.favorites, server_id)
            .into_iter()
            .cloned()
            .collect();
        let Some(favorite) = pins.get(index).cloned() else {
            return;
        };
        let revealed = if favorite.kind == PinnedKind::Database {
            self.reveal_object(Some(favorite.server_id), None, &favorite.name, cx)
        } else {
            self.reveal_object(
                Some(favorite.server_id),
                favorite.schema.as_deref(),
                &favorite.name,
                cx,
            )
        };
        self.favorites_notice = if revealed {
            None
        } else {
            Some(crate::localization::format(
                "explorer.schema_tree.favorite_not_loaded",
                &[("name", &favorite.name)],
            ))
        };
        sort_pinned_first(&mut self.roots, &self.favorites);
        pin_groups::apply_pin_group_folders(&mut self.roots, &self.favorites, &self.pin_groups);
        self.bump_tree_revision();
        cx.notify();
    }

    fn persist_pinned_and_rebuild(&mut self, cx: &mut Context<Self>) {
        save_favorites(&self.favorites);
        self.rebuild_roots();
        cx.notify();
    }

    fn unpin_at(&mut self, server_id: Uuid, index: usize, cx: &mut Context<Self>) {
        let pins: Vec<FavoriteObject> = pinned_for_server(&self.favorites, server_id)
            .into_iter()
            .cloned()
            .collect();
        let Some(favorite) = pins.get(index).cloned() else {
            return;
        };
        self.favorites = toggled_favorites(std::mem::take(&mut self.favorites), favorite);
        self.persist_pinned_and_rebuild(cx);
    }

    fn move_pinned_at(
        &mut self,
        server_id: Uuid,
        index: usize,
        delta: isize,
        cx: &mut Context<Self>,
    ) {
        // Pinned rows live in group folders, so up/down moves within the
        // pin's own group.
        self.favorites = pin_groups::move_pinned_in_group(
            std::mem::take(&mut self.favorites),
            server_id,
            index,
            delta,
        );
        self.persist_pinned_and_rebuild(cx);
    }

    /// Flatten every loaded connection/database/object node into quick-open
    /// candidates. Walks `self.roots` only — nothing is loaded here, so an
    /// unexpanded server contributes just its Server row (quick-open never
    /// connects on the user's behalf; see LinkAzureDatabase comment in
    /// app/explorer.rs).
    pub fn quick_open_candidates(&self) -> Vec<crate::quick_open::QuickOpenCandidate> {
        let mut out = Vec::new();
        collect_quick_open_candidates(&self.roots, &WalkCtx::default(), &mut out);
        out
    }

    pub fn reveal_object(
        &mut self,
        server_id: Option<Uuid>,
        schema: Option<&str>,
        name: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(path) = find_object_path(&self.roots, server_id, schema, name) else {
            return false;
        };
        self.reveal_path(path, cx)
    }

    /// Reveal a node by exact id: expand every ancestor, select, scroll.
    /// `false` when the id is not in the loaded tree.
    pub fn reveal_node_id(&mut self, target_id: &str, cx: &mut Context<Self>) -> bool {
        let Some(path) = find_node_id_path(&self.roots, target_id) else {
            return false;
        };
        self.reveal_path(path, cx)
    }

    fn reveal_path(&mut self, path: Vec<String>, cx: &mut Context<Self>) -> bool {
        let target_id = path.last().cloned().unwrap_or_default();
        // Expand every ancestor on the path so the flatten pass emits the row.
        for ancestor_id in &path[..path.len().saturating_sub(1)] {
            expand_node_by_id(&mut self.roots, ancestor_id);
        }
        self.bump_tree_revision();
        let rows = self.visible_rows();
        let Some(index) = rows.iter().position(|r| r.id.as_ref() == target_id) else {
            return false;
        };
        self.selected = Some(target_id);
        self.scroll_row_into_view(index);
        cx.notify();
        true
    }

    /// Scroll the minimum distance needed to put row `index` on screen.
    ///
    /// Keyboard navigation without this is a trick: the selection walks off the
    /// bottom of the viewport and the user is left arrowing at a list that
    /// appears not to respond.
    fn scroll_row_into_view(&self, index: usize) {
        let row_h = Theme::row_height();
        let viewport_h = f32::from(self.scroll.bounds().size.height);
        if viewport_h <= 0.0 || row_h <= 0.0 {
            return;
        }
        // `offset.y` is negative as the content scrolls up.
        let offset = self.scroll.offset();
        let scrolled = -f32::from(offset.y);
        let top = index as f32 * row_h;
        let bottom = top + row_h;
        let target = if top < scrolled {
            top
        } else if bottom > scrolled + viewport_h {
            bottom - viewport_h
        } else {
            return;
        };
        let max = f32::from(self.scroll.max_offset().y);
        self.scroll
            .set_offset(point(offset.x, px(-target.clamp(0.0, max.max(0.0)))));
    }

    /// Move the selection `delta` rows, clamped at the ends. With nothing
    /// selected, the first row takes the mark.
    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let rows = self.visible_rows();
        if rows.is_empty() {
            return;
        }
        let current = self
            .selected
            .as_ref()
            .and_then(|id| rows.iter().position(|r| r.id.as_ref() == id));
        let next = match current {
            Some(i) => (i as isize + delta).clamp(0, rows.len() as isize - 1) as usize,
            None => 0,
        };
        self.select_index(&rows, next, cx);
    }

    /// How many rows fit on screen — the step for Page Up / Page Down. At least
    /// one, so a viewport shorter than a row still moves.
    fn page_rows(&self) -> isize {
        let row_h = Theme::row_height();
        let viewport_h = f32::from(self.scroll.bounds().size.height);
        if row_h <= 0.0 || viewport_h <= 0.0 {
            return 1;
        }
        ((viewport_h / row_h).floor() as isize - 1).max(1)
    }

    /// Jump to the first or last visible row.
    fn move_to_end(&mut self, last: bool, cx: &mut Context<Self>) {
        let rows = self.visible_rows();
        if rows.is_empty() {
            return;
        }
        let index = if last { rows.len() - 1 } else { 0 };
        self.select_index(&rows, index, cx);
    }

    /// Type-ahead: printable keys accumulate into a short-lived buffer and jump
    /// to the next row whose name starts with it — the macOS source-list
    /// convention, and what a SSMS user reaches for to get to `Orders` without
    /// scrolling. Searching *after* the current row makes repeated presses cycle
    /// through matches rather than sticking on the first.
    fn type_ahead(&mut self, ch: &str, cx: &mut Context<Self>) {
        let now = Instant::now();
        let stale = self
            .type_ahead_at
            .map(|t| now.duration_since(t).as_millis() > 800)
            .unwrap_or(true);
        if stale {
            self.type_ahead.clear();
        }
        self.type_ahead.push_str(&ch.to_lowercase());
        self.type_ahead_at = Some(now);

        let rows = self.visible_rows();
        if rows.is_empty() {
            return;
        }
        let start = self
            .selected
            .as_ref()
            .and_then(|id| rows.iter().position(|r| r.id.as_ref() == id))
            .map(|i| if stale { i + 1 } else { i })
            .unwrap_or(0);
        let needle = self.type_ahead.clone();
        let hit = (0..rows.len())
            .map(|off| (start + off) % rows.len())
            .find(|&i| rows[i].title_lower.as_ref().starts_with(&needle));
        if let Some(i) = hit {
            self.select_index(&rows, i, cx);
        }
    }

    /// Open the selected row's context menu from the keyboard, anchored just
    /// below its row. Without this, every action behind the menu — including
    /// Edit and Delete Connection, which no longer have inline links — is
    /// mouse-only.
    fn open_context_menu_for_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.selected.clone() else {
            return;
        };
        let rows = self.visible_rows();
        let Some(index) = rows.iter().position(|r| r.id.as_ref() == id) else {
            return;
        };
        let row = &rows[index];
        if matches!(row.kind, NodeKind::Group { .. }) {
            return;
        }
        // Anchor to the row's own position in the window, so the menu appears
        // where the selection is rather than under the pointer.
        let bounds = self.scroll.bounds();
        let scrolled = -f32::from(self.scroll.offset().y);
        let y = f32::from(bounds.origin.y) + (index as f32 * Theme::row_height()) - scrolled
            + Theme::row_height();
        let x = f32::from(bounds.origin.x) + Theme::scaled(TREE_ROW_INSET);
        let (kind, title) = (row.kind.clone(), row.title.to_string());
        self.open_context_menu(id, kind, title, point(px(x), px(y)), window, cx);
    }

    /// Right arrow: expand a collapsed node, or step into an expanded one.
    fn expand_or_descend(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.selected.clone() else {
            return;
        };
        let Some(node) = find_node_any(&self.roots, &id) else {
            return;
        };
        if node.kind.is_expandable() && !node.expanded {
            self.toggle_expand(&id, cx);
        } else if let Some(first) = node.children.first() {
            self.selected = Some(first.id.clone());
            cx.notify();
        }
    }

    /// Left arrow: collapse an expanded node, or step out to its parent.
    fn collapse_or_ascend(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.selected.clone() else {
            return;
        };
        let expanded = find_node_any(&self.roots, &id).is_some_and(|n| n.expanded);
        if expanded {
            self.toggle_expand(&id, cx);
        } else if let Some(parent) = parent_id_of(&self.roots, &id) {
            self.selected = Some(parent);
            cx.notify();
        }
    }

    /// Enter: do what double-clicking the row would do — open a table/view,
    /// connect a server, or toggle anything else.
    fn activate_selection(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.selected.clone() else {
            return;
        };
        let Some(node) = find_node_any(&self.roots, &id) else {
            return;
        };
        match node.kind.clone() {
            NodeKind::Pinned { index } => {
                if let Some(server_id) = server_uuid_from_id(&id) {
                    self.open_favorite(server_id, index, cx);
                }
            }
            NodeKind::Table { schema, name } | NodeKind::View { schema, name } => {
                cx.emit(SchemaTreeEvent::SelectObject {
                    schema,
                    name,
                    server_id: server_uuid_from_id(&id),
                    database: Some(extract_db_from_id(&id)).filter(|d| !d.is_empty()),
                });
            }
            NodeKind::Server { id: sid } => {
                self.toggle_expand(&id, cx);
                if find_node_any(&self.roots, &id).is_some_and(|n| n.expanded) {
                    self.active = Some(sid);
                    cx.emit(SchemaTreeEvent::ActivateServer(sid));
                }
            }
            kind if kind.is_expandable() => self.toggle_expand(&id, cx),
            _ => {}
        }
    }

    /// Copy the selected row's name to the clipboard, routed through the same
    /// `CopyName` action the context menu emits so the payload and the status
    /// message come from one place. Returns whether there was anything to copy.
    pub(crate) fn copy_selected_name(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(context) = self.explorer_action_context_for_selection() else {
            return false;
        };
        cx.emit(SchemaTreeEvent::ExplorerActionRequested {
            context,
            action: ExplorerMenuAction::CopyName,
        });
        true
    }

    fn on_key_down(
        &mut self,
        event: &gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let m = &event.keystroke.modifiers;
        let key = event.keystroke.key.as_str();

        // Shift+F10 and the Menu key are the standard keyboard routes to a
        // context menu; they are the only ones this tree has.
        if (key == "f10" && m.shift) || key == "menu" {
            self.open_context_menu_for_selection(window, cx);
            cx.stop_propagation();
            return;
        }

        // Cmd+C / Ctrl+C on the focused tree copies the selected node's name,
        // the same payload the row's "Copy Name" menu item writes. Nothing else
        // in the tree owns a selection to copy, so the chord is unambiguous.
        if key == "c"
            && crate::keybindings::primary_modifier(m)
            && !m.alt
            && !m.shift
            && !m.function
        {
            if self.copy_selected_name(cx) {
                cx.stop_propagation();
            }
            return;
        }

        // Cmd/Ctrl+Space toggles the cursor row in or out of the bulk
        // multi-selection (the Windows/Linux list convention; macOS reserves
        // Cmd+Space for Spotlight, so plain Space does the same below).
        if key == "space"
            && crate::keybindings::primary_modifier(m)
            && !m.alt
            && !m.shift
            && !m.function
        {
            self.toggle_cursor_in_multi(cx);
            cx.stop_propagation();
            return;
        }

        // Let modified chords through to the app's own bindings.
        if m.control || m.alt || m.platform || m.function {
            return;
        }

        // Shift+movement extends the bulk multi-selection from its anchor to
        // wherever the cursor lands, the keyboard twin of shift-click.
        if m.shift && matches!(key, "up" | "down" | "home" | "end" | "pageup" | "pagedown") {
            let from = self.selected.clone();
            match key {
                "up" => self.move_selection(-1, cx),
                "down" => self.move_selection(1, cx),
                "home" => self.move_to_end(false, cx),
                "end" => self.move_to_end(true, cx),
                "pageup" => {
                    let page = self.page_rows();
                    self.move_selection(-page, cx);
                }
                _ => {
                    let page = self.page_rows();
                    self.move_selection(page, cx);
                }
            }
            self.extend_multi_from(from, cx);
            cx.stop_propagation();
            return;
        }

        // Any plain key that moves the cursor re-anchors a later Shift range
        // on the row it lands on (the set itself is kept for Space toggling).
        let cursor_before = self.selected.clone();

        match key {
            "up" => self.move_selection(-1, cx),
            "down" => self.move_selection(1, cx),
            "right" => self.expand_or_descend(cx),
            "left" => self.collapse_or_ascend(cx),
            "home" => self.move_to_end(false, cx),
            "end" => self.move_to_end(true, cx),
            "pageup" => {
                let page = self.page_rows();
                self.move_selection(-page, cx);
            }
            "pagedown" => {
                let page = self.page_rows();
                self.move_selection(page, cx);
            }
            "enter" => self.activate_selection(cx),
            "escape" => {
                // Escape backs out of whatever is narrowing the view: the menu
                // first, then the filter, then the multi-selection, then the
                // selection. The filter text lives in the sidebar's field, so
                // clearing it here has to go back through the sidebar or the
                // two would disagree.
                if self.context_menu.is_some() {
                    self.close_context_menu(cx);
                } else if !self.filter.is_empty() {
                    cx.emit(SchemaTreeEvent::FilterCleared);
                } else if !self.multi.is_empty() {
                    self.multi.clear();
                    cx.notify();
                } else if self.selected.take().is_some() {
                    cx.notify();
                }
            }
            // Space toggles the cursor row — unless a type-ahead is mid-word,
            // where it is part of a name like `Order Details`.
            "space" if !self.type_ahead_is_live() => self.toggle_cursor_in_multi(cx),
            "space" => self.type_ahead(" ", cx),
            _ => {
                // Printable single characters drive type-ahead; everything else
                // is not ours.
                let is_printable =
                    key.chars().count() == 1 && !key.chars().next().unwrap().is_control();
                if !is_printable {
                    return;
                }
                self.type_ahead(key, cx);
            }
        }
        if self.selected != cursor_before
            && let Some(cursor) = self.selected.as_deref()
        {
            self.multi.set_anchor(cursor);
        }
        cx.stop_propagation();
    }

    /// Whether a type-ahead word is still being typed (the same 800 ms window
    /// `type_ahead` uses to decide whether to append or start over).
    fn type_ahead_is_live(&self) -> bool {
        !self.type_ahead.is_empty()
            && self
                .type_ahead_at
                .is_some_and(|t| t.elapsed().as_millis() <= 800)
    }

    /// Space / Cmd/Ctrl+Space: toggle the cursor row in or out of the bulk
    /// multi-selection. Only tables and views can be members; on any other
    /// row this does nothing.
    fn toggle_cursor_in_multi(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.selected.clone() else {
            return;
        };
        let selectable = find_node_any(&self.roots, &id)
            .is_some_and(|n| matches!(n.kind, NodeKind::Table { .. } | NodeKind::View { .. }));
        if selectable {
            self.multi.toggle(&id);
            cx.notify();
        }
    }

    /// After a Shift+movement: range the multi-selection from its anchor (or
    /// from `from`, the row the cursor left, when there is none) to the
    /// cursor, over the visible rows.
    fn extend_multi_from(&mut self, from: Option<String>, cx: &mut Context<Self>) {
        let Some(cursor) = self.selected.clone() else {
            return;
        };
        let from = from.unwrap_or_else(|| cursor.clone());
        let rows: Vec<(String, bool)> = self
            .visible_rows()
            .iter()
            .map(|row| {
                (
                    row.id.to_string(),
                    matches!(row.kind, NodeKind::Table { .. } | NodeKind::View { .. }),
                )
            })
            .collect();
        self.multi.extend_keyboard(&from, &cursor, &rows);
        cx.notify();
    }

    /// Take keyboard focus, anchoring the selection on the first row when there
    /// isn't one yet so the arrow keys have somewhere to start.
    pub fn focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus_handle, cx);
        if self.selected.is_none() {
            let rows = self.visible_rows();
            if let Some(first) = rows.first() {
                self.selected = Some(first.id.to_string());
            }
        }
        cx.notify();
    }

    /// Replace the entity identities used to paint group headers. Called by the
    /// sidebar alongside `set_connections`, from its one read of the store.
    pub fn set_entities(&mut self, entities: GroupIdentities, cx: &mut Context<Self>) {
        self.entities = entities;
        self.rebuild_roots();
        cx.notify();
    }

    /// Narrow the tree to nodes matching `query`, and to their ancestors.
    /// Empty (or whitespace, no scopes) restores the full tree.
    pub fn set_filter(
        &mut self,
        query: crate::explorer_search::ExplorerQuery,
        cx: &mut Context<Self>,
    ) {
        if self.filter == query {
            return;
        }
        let was_empty = self.filter.is_empty();
        let now_empty = query.is_empty();
        if was_empty && !now_empty {
            let mut expanded = std::collections::HashSet::new();
            collect_expanded_ids(&self.roots, &mut expanded);
            self.pre_filter = Some(PreFilterState {
                expanded,
                selected: self.selected.clone(),
            });
        }
        self.filter = query;
        self.compiled = Rc::new(crate::explorer_search::CompiledQuery::compile(&self.filter));
        if now_empty && let Some(saved) = self.pre_filter.take() {
            self.restore_pre_filter(saved, cx);
        }
        cx.notify();
    }

    /// Put expansion back the way it was before the search, with one
    /// exception: a row the user picked while searching stays revealed —
    /// its ancestors open, its own expansion as the user left it — and is
    /// scrolled into view, because that row is what the search was for.
    fn restore_pre_filter(&mut self, saved: PreFilterState, cx: &mut Context<Self>) {
        let picked = self
            .selected
            .clone()
            .filter(|id| saved.selected.as_deref() != Some(id.as_str()));
        let picked_expanded = picked
            .as_deref()
            .and_then(|id| find_node_any(&self.roots, id))
            .map(|n| n.expanded);
        apply_expanded_ids(&mut self.roots, &saved.expanded);
        self.favorites_expanded = self
            .roots
            .iter()
            .find(|n| matches!(n.kind, NodeKind::PinnedFolder { .. }))
            .is_some_and(|n| n.expanded);
        self.bump_tree_revision();
        if let Some(id) = picked {
            if let (Some(expanded), Some(node)) =
                (picked_expanded, find_node_any_mut(&mut self.roots, &id))
            {
                node.expanded = expanded;
            }
            self.reveal_node_id(&id, cx);
        }
    }

    /// Keep existing callers that only have the raw filter text.
    #[allow(dead_code)]
    pub fn set_filter_text(&mut self, raw: String, cx: &mut Context<Self>) {
        let query = crate::explorer_search::ExplorerQuery::parse(&raw, self.filter.scopes.clone());
        self.set_filter(query, cx);
    }

    pub fn set_filter_scopes(
        &mut self,
        scopes: std::collections::BTreeSet<crate::explorer_search::ObjectScope>,
        cx: &mut Context<Self>,
    ) {
        let mut query = self.filter.clone();
        query.scopes = scopes;
        self.set_filter(query, cx);
    }

    /// Point the tree at the focused query window's server (database dropdown
    /// source). `None` clears the pointer but keeps every server listed; it
    /// says nothing about connectivity — see [`SchemaTree::set_connection_state`].
    pub fn set_active(&mut self, id: Option<Uuid>, cx: &mut Context<Self>) {
        self.active = id;
        cx.notify();
    }

    /// Record a server's observed connection health. `Disconnected` returns the
    /// node to the default gray state (the map entry is dropped).
    pub fn set_connection_state(
        &mut self,
        id: Uuid,
        state: ConnectionState,
        cx: &mut Context<Self>,
    ) {
        let changed = match state {
            ConnectionState::Disconnected => self.connection_states.remove(&id).is_some(),
            _ => self.connection_states.insert(id, state) != Some(state),
        };
        if changed {
            cx.notify();
        }
    }

    /// This server's observed connection health (default: `Disconnected`).
    /// Whether any server node is mid-load, i.e. painting the pulsing
    /// "connecting" health dot. The workbench renders the sidebar live
    /// instead of from its frame cache while this holds, so the pulse keeps
    /// animating; a cached view would freeze it mid-cycle.
    pub fn any_server_loading(&self) -> bool {
        self.roots.iter().any(|n| {
            matches!(n.kind, NodeKind::Server { .. } | NodeKind::AzureSqlServer)
                && matches!(n.load_state, LoadState::Loading)
        })
    }

    pub fn connection_state(&self, id: Uuid) -> ConnectionState {
        self.connection_states.get(&id).copied().unwrap_or_default()
    }

    /// Snapshot of every server's observed connection health, for other
    /// surfaces (the Connection Health dashboard) that must show the same
    /// truth as the tree dots.
    pub fn connection_states_snapshot(&self) -> std::collections::HashMap<Uuid, ConnectionState> {
        self.connection_states.clone()
    }

    /// Explicit per-node disconnect: return the dot to the default gray and
    /// drop any in-flight database enumeration for this server, so a stale
    /// late success cannot flip the freshly disconnected server back to
    /// green (`DatabasesLoaded` is generation-guarded and never fires for an
    /// invalidated load).
    pub fn disconnect_server(&mut self, id: Uuid, cx: &mut Context<Self>) {
        self.connection_states.remove(&id);
        // A bulk action must never run against a server the user just
        // disconnected: its rows leave the multi-selection.
        self.multi
            .retain(|member| server_uuid_from_id(member) != Some(id));
        let server_id = format!("srv:{id}");
        if let Some(node) = find_server_node_mut(&mut self.roots, &server_id) {
            node.invalidate_pending_load();
            if matches!(node.load_state, LoadState::Loading) {
                node.load_state = LoadState::Unloaded;
            }
        }
        self.bump_tree_revision();
        cx.notify();
    }

    /// Clear a stale red dot when a new connect attempt begins: red means
    /// "the last attempt failed to reach it", which stops being true the
    /// moment a fresh attempt is in flight. Connected and default states are
    /// left alone.
    pub fn clear_unreachable(&mut self, id: Uuid, cx: &mut Context<Self>) {
        if matches!(
            self.connection_states.get(&id),
            Some(ConnectionState::Unreachable | ConnectionState::StaleUnreachable)
        ) {
            self.connection_states.remove(&id);
            cx.notify();
        }
    }

    /// Make `id` the active server and ensure its subtree is expanded/loaded.
    /// Called by the parent's connect flow so connecting from anywhere (sidebar
    /// click, command palette, connection manager) drills the tree open. Also
    /// expands the enclosing group header when grouped so the server is revealed
    /// even if the user had collapsed that group.
    pub fn activate(&mut self, id: Uuid, cx: &mut Context<Self>) {
        self.active = Some(id);
        let server_id = format!("srv:{id}");

        // Reveal + expand the server (and its parent group), collecting the node
        // id that still needs its databases loaded.
        let mut to_load: Option<String> = None;
        for root in &mut self.roots {
            match &root.kind {
                NodeKind::Server { .. } if root.id == server_id => {
                    expand_server_for_activate(root, &mut to_load);
                }
                NodeKind::Group { .. } => {
                    for child in &mut root.children {
                        if child.id == server_id {
                            root.expanded = true;
                            expand_server_for_activate(child, &mut to_load);
                        }
                    }
                }
                _ => {}
            }
        }

        if let Some(node_id) = to_load {
            self.load_children(&node_id, cx);
        }
        self.bump_tree_revision();
        cx.notify();
    }

    /// Rebuild `self.roots` from the current profiles + grouping, carrying over
    /// each surviving server's drilled-down subtree.
    ///
    /// This is also where a group header picks up the color and icon its entity
    /// was given in the entity editor — see [`GroupIdentities`].
    fn rebuild_roots(&mut self) {
        // Snapshot existing server subtrees by id so drill-down state survives.
        let mut preserved: std::collections::HashMap<String, TreeNode> =
            std::collections::HashMap::new();
        // The Azure hierarchy is rebuilt wholesale from the discovery list
        // rather than carried over, and `make_azure_tree` opens every level, so
        // without this a collapsed subscription sprang back open whenever an
        // unrelated connection was added. Server subtrees have always been
        // preserved; this gives Azure the same courtesy.
        let mut azure_expansion: std::collections::HashMap<String, bool> =
            std::collections::HashMap::new();
        collect_expansion(&self.roots, &mut azure_expansion);
        collect_server_nodes(&mut self.roots, &mut preserved);

        let profiles = self.profiles.clone();
        let grouped = group(&profiles, self.grouping);
        let mut roots = Vec::new();
        match grouped {
            GroupedProfiles::Flat(indices) => {
                for i in indices {
                    roots.push(make_server_node(
                        &profiles[i],
                        0,
                        &mut preserved,
                        &self.entities,
                    ));
                }
            }
            GroupedProfiles::Grouped(groups) => {
                for g in groups {
                    let (color_hex, image_path) = self.entities.for_label(self.grouping, &g.label);
                    let mut group_node = TreeNode::new(
                        format!("group:{}", g.id),
                        g.label,
                        NodeKind::Group {
                            color_hex,
                            image_path,
                        },
                        0,
                    );
                    group_node.expanded = true;
                    group_node.load_state = LoadState::Loaded;
                    group_node.children = g
                        .profile_indices
                        .iter()
                        .map(|&i| make_server_node(&profiles[i], 1, &mut preserved, &self.entities))
                        .collect();
                    roots.push(group_node);
                }
            }
        }

        // Append the Azure discovery tree (account → subscription → server →
        // database) below the saved-connection server nodes.
        if !self.azure_discoveries.is_empty() {
            let mut azure_roots = make_azure_tree(&self.azure_discoveries, 0);
            apply_expansion(&mut azure_roots, &azure_expansion);
            roots.extend(azure_roots);
        }

        self.roots = roots;
        sort_pinned_first(&mut self.roots, &self.favorites);
        pin_groups::apply_pin_group_folders(&mut self.roots, &self.favorites, &self.pin_groups);
        self.bump_tree_revision();
    }

    /// Names of the databases discovered for the *active* server. Empty when no
    /// server is active or its databases have not loaded yet. Used by the
    /// query-bar database dropdown.
    pub fn database_names(&self) -> Vec<String> {
        match self.active_server() {
            Some(server) => database_names_from_roots(&server.children),
            None => Vec::new(),
        }
    }

    /// Whether the tree has any content to show — either saved connections or
    /// discovered Azure resources. Used by the sidebar to decide whether to
    /// render the tree or the empty-state placeholder.
    pub fn has_content(&self) -> bool {
        !self.profiles.is_empty() || !self.azure_discoveries.is_empty()
    }

    /// True when the saved-connection server node for `profile_id` is in a
    /// failed load state — the signal behind the tree's "failed" badge. The
    /// status bar consults this so a server that could not connect stops
    /// reading as a healthy (green) connection.
    pub fn server_load_failed(&self, profile_id: Uuid) -> bool {
        fn walk(nodes: &[TreeNode], profile_id: Uuid) -> bool {
            nodes.iter().any(|node| {
                (matches!(node.kind, NodeKind::Server { id } if id == profile_id)
                    && matches!(node.load_state, LoadState::Failed(_)))
                    || walk(&node.children, profile_id)
            })
        }
        walk(&self.roots, profile_id)
    }

    /// The active server node, regardless of grouping/expansion.
    fn active_server(&self) -> Option<&TreeNode> {
        let id = self.active?;
        find_server_node(&self.roots, &format!("srv:{id}"))
    }

    /// Resolve the connection profile that owns `node_id`. Every schema node id
    /// begins with `srv:{uuid}`, so the owning server (and thus the profile
    /// used to run metadata queries) is recoverable from any descendant id.
    fn profile_for_node_id(&self, node_id: &str) -> Option<ConnectionProfile> {
        let uuid = server_uuid_from_id(node_id)?;
        self.profiles.iter().find(|p| p.id.0 == uuid).cloned()
    }

    fn toggle_expand(&mut self, node_id: &str, cx: &mut Context<Self>) {
        // `find_node_any_mut`, not `find_node_mut`: a filter can put a row on
        // screen whose parent is still collapsed, and pressing Right on it must
        // expand it rather than silently do nothing.
        if let Some(node) = find_node_any_mut(&mut self.roots, node_id) {
            node.expanded = !node.expanded;
            if matches!(node.kind, NodeKind::PinnedFolder { .. }) {
                self.favorites_expanded = node.expanded;
            }
            if node.expanded && matches!(node.load_state, LoadState::Unloaded) {
                self.load_children(node_id, cx);
            }
        }
        self.bump_tree_revision();
        cx.notify();
    }

    fn load_children(&mut self, node_id: &str, cx: &mut Context<Self>) {
        // Resolve which server (connection profile) this node belongs to from
        // its id (`srv:{uuid}:...`). Group headers and unknown ids have none.
        let Some(profile) = self.profile_for_node_id(node_id) else {
            return;
        };

        // `find_node_any_mut`: this node may have been revealed by the filter
        // with its parent still collapsed, and the visible-tree finder cannot
        // reach it — it would leave the row marked Loading forever.
        let node_info = if let Some(node) = find_node_any_mut(&mut self.roots, node_id) {
            node.load_state = LoadState::Loading;
            // This load owns the node from here; any earlier one in flight is
            // now stale and will drop its result.
            node.invalidate_pending_load();
            Some((node.kind.clone(), node.depth, node.load_generation))
        } else {
            None
        };
        let Some(node_info) = node_info else {
            return;
        };
        self.bump_tree_revision();
        let generation = node_info.2;

        cx.notify();

        let services = self.services_for(&profile);
        let log = self.log.clone();
        let entra_provider = self.entra_sql_token_provider.clone();
        let fk_cache = self.fk_cache.clone();
        // The key filter in force: this session's, or the one remembered
        // for this profile's database from an earlier session, restored
        // into the session map so the Filter Keys… dialog shows it too.
        let redis_filter = self.redis_filters.get(node_id).cloned().or_else(|| {
            let NodeKind::Database(db) = &node_info.0 else {
                return None;
            };
            if !profile.database_type.is_command_engine() {
                return None;
            }
            let remembered = crate::redis_key_filters::remembered_filter(profile.id.0, db)?;
            self.redis_filters
                .insert(node_id.to_string(), remembered.clone());
            Some(remembered)
        });
        let async_cx = cx.to_async();
        let weak_self = cx.weak_entity();
        let node_id = node_id.to_string();
        let node_id_fg = node_id.clone();
        // Which server this load speaks to, for the failure event: the tree
        // browses every saved connection, so a failure here need not belong to
        // the app's active profile.
        let profile_id = profile.id.0;
        let parent_depth = node_info.1;
        let kind_label = format!("{:?}", node_info.0);
        let prefs = self.data_model_prefs;

        cx.spawn(async move |_this, _cx| {
            log.log_verbose(
                "schema_tree",
                &format!("Loading children for {kind_label} ({node_id})"),
            );

            // Turn the saved profile into one that can actually authenticate.
            //
            // A profile on disk is secret-free by design: a SQL account stores
            // only its *account id* in `secret_ref.key`, with the password in
            // the system keyring. Every other path to the engine hydrates that
            // first — Test, the Databases tab, and the app's connect flow all
            // call `hydrate_profile` — but this one never did, so the tree
            // handed TDS an account id where a password belonged and got back
            // "missing secret for key <uuid>". The tree already learned this
            // lesson for Entra (`runtime_entra_tokens`); saved SQL accounts
            // arrived later and never got the same treatment.
            //
            // On the *background* executor, and this is not a detail:
            // `SecKeychainFindGenericPassword` is a blocking call that can sit
            // for a long time — a locked keychain, an ACL prompt, iCloud
            // sync — and doing it inline froze the whole app mid-expand, with
            // the main thread parked in `decodeDataBlob` and every click
            // ignored. `app.rs` gets away with it because connecting is a
            // one-off; the tree expands constantly.
            //
            // Resolved per load rather than cached on `self.profiles`, so a
            // plaintext password is never held for a server the user hasn't
            // asked to expand.
            let hydrated = async_cx
                .background_executor()
                .spawn(async move { crate::sql_credentials::hydrate_profile(profile) })
                .await;
            let profile = match hydrated {
                Ok(profile) => profile,
                Err(error) => {
                    // Surface the real reason ("no password in the system
                    // keyring") rather than letting it reach the user as a TDS
                    // auth failure about a key they have never heard of.
                    log.log_error(
                        "schema_tree",
                        &format!(
                            "Cannot resolve credentials for {kind_label} ({node_id}): {error}"
                        ),
                    );
                    let mut cx = async_cx.clone();
                    weak_self
                        .update(&mut cx, |this, cx| {
                            if let Some(node) = find_node_any_mut(&mut this.roots, &node_id_fg)
                                && node.load_generation == generation
                            {
                                node.load_state =
                                    classify_load_error(crate::localization::message(error));
                            }
                            this.bump_tree_revision();
                            cx.notify();
                        })
                        .ok();
                    return;
                }
            };

            // Re-mint the Entra SQL token before dialing, so an expansion that
            // happens after the connect-time token has lapsed (~1h) refreshes
            // just like a query window does, instead of failing with "token
            // expired". No-op for non-Entra profiles.
            let profile = match &entra_provider {
                Some(provider) => {
                    match crate::azure_sql_token::hydrate_entra_token(provider, profile).await {
                        Ok(profile) => profile,
                        Err(message) => {
                            log.log_error(
                                "schema_tree",
                                &format!(
                                    "Cannot refresh Azure sign-in for {kind_label} ({node_id}): {message}"
                                ),
                            );
                            let mut cx = async_cx.clone();
                            weak_self
                                .update(&mut cx, |this, cx| {
                                    if let Some(node) =
                                        find_node_any_mut(&mut this.roots, &node_id_fg)
                                        && node.load_generation == generation
                                    {
                                        node.load_state = classify_load_error(message);
                                    }
                                    this.bump_tree_revision();
                                    cx.notify();
                                })
                                .ok();
                            return;
                        }
                    }
                }
                None => profile,
            };

            let result = load_children_async(
                &services,
                &profile,
                &node_info.0,
                &node_id,
                parent_depth,
                prefs,
                &fk_cache,
                redis_filter.as_deref(),
            )
            .await;
            match &result {
                Ok(children) => log.log_verbose(
                    "schema_tree",
                    &format!(
                        "Loaded {} children for {kind_label} ({node_id})",
                        children.len()
                    ),
                ),
                Err(e) => log.log_error(
                    "schema_tree",
                    &format!("Failed to load children for {kind_label} ({node_id}): {e}"),
                ),
            }
            let mut cx = async_cx.clone();
            weak_self
                .update(&mut cx, |this, cx| {
                    let favorites = this.favorites.clone();
                    let groups = this.pin_groups.clone();
                    if let Some(node) = find_node_any_mut(&mut this.roots, &node_id_fg) {
                        // Something invalidated this node while the request was
                        // out — a refresh, or a preferences change that reshapes
                        // the level. Whatever came back describes a tree that no
                        // longer exists, so drop it rather than paint it over
                        // the load that superseded us.
                        if node.load_generation != generation {
                            log.log_verbose(
                                "schema_tree",
                                &format!("Discarding superseded load for {kind_label} ({node_id})"),
                            );
                            return;
                        }
                        match result {
                            Ok(children) => {
                                // When a table/view's columns load, surface them
                                // to the parent so the completion catalog can be
                                // enriched (incremental column completion).
                                if let Some((schema, name, cols)) =
                                    column_load_payload(&node_info.0, &children)
                                {
                                    cx.emit(SchemaTreeEvent::ColumnsLoaded {
                                        schema,
                                        name,
                                        columns: cols,
                                    });
                                }
                                if let Some((schema, name, parameters)) =
                                    parameter_load_payload(&node_info.0, &children)
                                {
                                    cx.emit(SchemaTreeEvent::ProcedureParametersLoaded {
                                        schema,
                                        name,
                                        parameters,
                                    });
                                }
                                // A Redis scan's key names feed command
                                // IntelliSense for that logical database.
                                if let Some(keys) =
                                    redis::redis_key_load_payload(&node_info.0, &children)
                                {
                                    cx.emit(SchemaTreeEvent::RedisKeysLoaded {
                                        profile_id,
                                        database: load::extract_db_from_id(&node_id_fg),
                                        keys,
                                    });
                                }
                                // A server node's children are its databases;
                                // tell the parent so the query-bar refreshes its
                                // database dropdown for this server.
                                if matches!(node_info.0, NodeKind::Server { .. }) {
                                    cx.emit(SchemaTreeEvent::DatabasesLoaded { profile_id });
                                }
                                node.children = children;
                                node.load_state = LoadState::Loaded;
                                if matches!(node_info.0, NodeKind::Server { .. }) {
                                    // Float pinned databases to the top of this
                                    // server's freshly-loaded database list.
                                    sort_pinned_first_node(node, &favorites, None, None);
                                    // Its loaded children replaced the pin
                                    // group folders; put them back on top.
                                    pin_groups::apply_pin_group_folders_to_server(
                                        node, &favorites, &groups,
                                    );
                                }
                            }
                            Err(e) => {
                                let message = e.to_string();
                                node.load_state = classify_load_error(e);
                                cx.emit(SchemaTreeEvent::LoadFailed {
                                    profile_id,
                                    message,
                                    server_node: matches!(node_info.0, NodeKind::Server { .. }),
                                });
                            }
                        }
                    }
                    this.bump_tree_revision();
                    // Restore the pre-refresh drill-down: any freshly loaded
                    // child that was expanded before the refresh re-expands
                    // and re-fetches, cascading level by level.
                    let mut to_reload: Vec<String> = Vec::new();
                    if !this.reexpand_after_refresh.is_empty()
                        && let Some(node) = find_node_any_mut(&mut this.roots, &node_id_fg)
                    {
                        for child in &mut node.children {
                            if this.reexpand_after_refresh.remove(&child.id)
                                && child.kind.is_expandable()
                            {
                                child.expanded = true;
                                to_reload.push(child.id.clone());
                            }
                        }
                    }
                    for id in to_reload {
                        this.load_children(&id, cx);
                    }
                    cx.notify();
                })
                .ok();
        })
        .detach();
    }

    /// Re-read the Data Model preferences (called when the settings dialog
    /// closes) and collapse loaded levels whose shape they control so the
    /// next expand rebuilds with the new organization.
    pub fn reload_data_model_prefs(&mut self, cx: &mut Context<Self>) {
        let new_prefs = crate::preferences::DataModelPreferences::load();
        if new_prefs != self.data_model_prefs {
            self.data_model_prefs = new_prefs;
            // Server/database subtrees were built under the old rules —
            // force a reload on next expand.
            for root in &mut self.roots {
                reset_subtree_loads(root);
            }
            self.bump_tree_revision();
        }
        cx.notify();
    }

    /// Extract `ColumnDescriptor`s from a table/view node's loaded column
    /// children for "CREATE TABLE Script". Parses each column child's
    /// `detail` field (format: `"int, nullable"`, `"nvarchar(50), not null"`,
    /// `"int, identity, nullable"`). Returns empty when the node hasn't been
    /// expanded yet — the DDL degrades to a bare `CREATE TABLE … ()`.
    fn extract_column_descriptors(&self, node_id: &str) -> Vec<ColumnDescriptor> {
        let Some(node) = find_node(&self.roots, node_id) else {
            return Vec::new();
        };
        node.children
            .iter()
            .filter(|c| matches!(c.kind, NodeKind::Column))
            .map(|c| {
                let (data_type, nullable) = parse_column_detail(c.detail.as_deref());
                ColumnDescriptor {
                    name: c.title.clone(),
                    data_type,
                    nullable,
                }
            })
            .collect()
    }

    /// Refresh a single node: drop its cached children, reset load state,
    /// and reload if it was expanded. Mirrors the Swift `refreshClicked`
    /// behavior (collapse + drop cached subtree + reload).
    /// The Redis key browser's MATCH pattern for a database node (`None` for
    /// every key), for the Filter Keys… dialog to show what is in force.
    pub fn redis_key_filter(&self, node_id: &str) -> Option<String> {
        self.redis_filters.get(node_id).cloned()
    }

    /// Set (or clear) the MATCH pattern a Redis database node scans with and
    /// reload it so the change shows at once.
    pub fn set_redis_key_filter(
        &mut self,
        node_id: &str,
        pattern: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let pattern = pattern
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty() && p != "*");
        match &pattern {
            Some(pattern) => {
                self.redis_filters
                    .insert(node_id.to_string(), pattern.clone());
            }
            None => {
                self.redis_filters.remove(node_id);
            }
        }
        // Remember it for this profile's database across sessions (and the
        // pattern among the profile's recent ones).
        if let Some(profile) = self.profile_for_node_id(node_id)
            && let Some(NodeKind::Database(db)) =
                find_node_any(&self.roots, node_id).map(|node| &node.kind)
        {
            crate::redis_key_filters::remember_filter(profile.id.0, db, pattern.as_deref());
        }
        // Force the reload even when the node is collapsed, so expanding it
        // next shows the filtered listing rather than a stale one.
        if let Some(node) = find_node_any_mut(&mut self.roots, node_id) {
            node.expanded = true;
        }
        self.refresh_node(node_id, cx);
    }

    /// The Redis key browser's retained listing under `node_id` — the
    /// listing's parent (database or folder) or its note row itself — with
    /// the parent's id. `None` when the listing has nothing left over.
    fn redis_listing_note(
        &self,
        node_id: &str,
    ) -> Option<(String, std::sync::Arc<redis::RedisListing>)> {
        let parent_id = node_id.strip_suffix(":note").unwrap_or(node_id).to_string();
        let parent = find_node_any(&self.roots, &parent_id)?;
        let listing = parent.children.iter().find_map(|child| match &child.kind {
            NodeKind::RedisNote { listing } => Some(listing.clone()),
            _ => None,
        })?;
        Some((parent_id, listing))
    }

    /// Clicking a listing's note row: show the rows already scanned when
    /// some are waiting, otherwise continue the walk the note says stopped.
    pub(crate) fn redis_note_activated(&mut self, node_id: &str, cx: &mut Context<Self>) {
        let Some((_, listing)) = self.redis_listing_note(node_id) else {
            return;
        };
        if listing.hidden_rows() > 0 {
            self.redis_extend_listing(node_id, false, cx);
        } else if listing.scan_incomplete() {
            self.redis_extend_listing(node_id, true, cx);
        }
    }

    /// Extend a Redis listing in place: Load more (`scan_more == false`)
    /// materializes the next window of rows already scanned, with no
    /// server round trip; Scan more continues the same `SCAN` from the
    /// cursor it stopped at and folds the new keys into the listing. Either
    /// way the parent's children are rebuilt from the retained listing, so
    /// folder counts stay right and the note row keeps telling the truth.
    pub(crate) fn redis_extend_listing(
        &mut self,
        node_id: &str,
        scan_more: bool,
        cx: &mut Context<Self>,
    ) {
        let Some((parent_id, listing)) = self.redis_listing_note(node_id) else {
            return;
        };
        let Some(profile) = self.profile_for_node_id(&parent_id) else {
            return;
        };
        let delimiter = sqlly_core::redis_keys::effective_delimiter(&profile);
        if !scan_more {
            let mut widened = (*listing).clone();
            widened.load_more();
            if let Some(node) = find_node_any_mut(&mut self.roots, &parent_id) {
                let fresh =
                    redis::nodes_for_window(&parent_id, node.depth + 1, &delimiter, widened);
                redis::replace_listing_children(&mut node.children, fresh);
            }
            self.bump_tree_revision();
            cx.notify();
            return;
        }
        let Some(cursor) = listing.cursor.clone() else {
            return;
        };
        // The note row doubles as the progress indicator; the rows already
        // shown stay put. The parent's load generation guards the result:
        // a refresh in the meantime drops it, as it would a stale load.
        let Some(generation) = find_node_any_mut(&mut self.roots, &parent_id).map(|node| {
            for child in &mut node.children {
                if matches!(child.kind, NodeKind::RedisNote { .. }) {
                    child.set_localized_title(|| {
                        crate::localization::text("explorer.schema_tree.scanning_more")
                    });
                }
            }
            node.load_generation
        }) else {
            return;
        };
        self.bump_tree_revision();
        cx.notify();

        let services = self.services_for(&profile);
        let log = self.log.clone();
        let database = extract_db_from_id(&parent_id);
        let pattern = listing.pattern.clone();
        let async_cx = cx.to_async();
        let weak_self = cx.weak_entity();
        cx.spawn(async move |_this, _cx| {
            // Same credential hydration as a child load: the saved profile
            // holds only an account id, the keyring the password (off the
            // UI thread — see `load_children`).
            let hydrated = async_cx
                .background_executor()
                .spawn(async move { crate::sql_credentials::hydrate_profile(profile) })
                .await;
            let result = match hydrated {
                Ok(profile) => redis::scan(&services, &profile, &database, &pattern, &cursor).await,
                Err(error) => Err(crate::localization::message(error)),
            };
            let mut cx = async_cx.clone();
            weak_self
                .update(&mut cx, |this, cx| {
                    if let Some(node) = find_node_any_mut(&mut this.roots, &parent_id)
                        && node.load_generation == generation
                    {
                        let note = node.children.iter().find_map(|child| match &child.kind {
                            NodeKind::RedisNote { listing } => Some(listing.clone()),
                            _ => None,
                        });
                        match (result, note) {
                            (Ok((keys, next)), Some(listing)) => {
                                log.log_verbose(
                                    "schema_tree",
                                    &format!("Scanned {} more keys for {parent_id}", keys.len()),
                                );
                                let mut extended = (*listing).clone();
                                extended.extend(keys, next);
                                let fresh = redis::nodes_for_window(
                                    &parent_id,
                                    node.depth + 1,
                                    &delimiter,
                                    extended,
                                );
                                redis::replace_listing_children(&mut node.children, fresh);
                            }
                            (Err(error), Some(listing)) => {
                                log.log_error(
                                    "schema_tree",
                                    &format!("Scan more failed for {parent_id}: {error}"),
                                );
                                for child in &mut node.children {
                                    if matches!(child.kind, NodeKind::RedisNote { .. }) {
                                        let (listing, error) = (listing.clone(), error.clone());
                                        child.set_localized_title(move || {
                                            crate::localization::format(
                                                "explorer.schema_tree.scan_more_failed",
                                                &[
                                                    ("summary", &redis::note_text(&listing)),
                                                    ("error", &error),
                                                ],
                                            )
                                        });
                                    }
                                }
                            }
                            (_, None) => {}
                        }
                    }
                    this.bump_tree_revision();
                    cx.notify();
                })
                .ok();
        })
        .detach();
    }

    pub fn refresh_node(&mut self, node_id: &str, cx: &mut Context<Self>) {
        // A refresh must re-read the schema, not serve a stale FK listing.
        self.fk_cache.clear();
        // Reachable from the "failed" marker on a filter-revealed row, so this
        // must see through collapsed ancestors too.
        let was_expanded = find_node_any_mut(&mut self.roots, node_id).map(|n| {
            let mut expanded_ids = std::collections::HashSet::new();
            collect_expanded_ids(&n.children, &mut expanded_ids);
            n.children.clear();
            n.load_state = LoadState::Unloaded;
            // Drop whatever is already in flight. `load_children` below bumps
            // this again for the replacement load; doing it here too covers the
            // collapsed case, where no replacement is started at all.
            n.invalidate_pending_load();
            (n.expanded, expanded_ids)
        });
        self.bump_tree_revision();

        if let Some((expanded, ids)) = was_expanded {
            self.reexpand_after_refresh.extend(ids);
            if expanded {
                self.load_children(node_id, cx);
                return;
            }
        }
        cx.notify();
    }

    /// Refresh one server's entire subtree: drop all cached children, reset
    /// load state, and reload if the server was expanded. This is the "full
    /// schema refresh" — it clears everything under that server and re-fetches
    /// from the database. Descends through group headers regardless of their
    /// expansion state so a collapsed group does not block the refresh.
    pub fn refresh_server(&mut self, id: Uuid, cx: &mut Context<Self>) {
        self.fk_cache.clear();
        let server_id = format!("srv:{id}");
        let was_expanded = find_server_node_mut(&mut self.roots, &server_id).map(|n| {
            let mut expanded_ids = std::collections::HashSet::new();
            collect_expanded_ids(&n.children, &mut expanded_ids);
            n.children.clear();
            n.load_state = LoadState::Unloaded;
            n.invalidate_pending_load();
            (n.expanded, expanded_ids)
        });
        self.bump_tree_revision();

        if let Some((expanded, ids)) = was_expanded {
            self.reexpand_after_refresh.extend(ids);
            if expanded {
                self.load_children(&server_id, cx);
                return;
            }
        }
        cx.notify();
    }

    /// Collapse every expanded node without dropping cached children, so the
    /// next expand is instant.
    pub fn collapse_all(&mut self, cx: &mut Context<Self>) {
        collapse_all_nodes(&mut self.roots);
        cx.notify();
    }

    /// Drop the schemas this connection now hides from `database`'s already
    /// loaded subtree in place — no reload, so hiding works offline and the
    /// rest of the tree keeps its expansion. Showing them again needs a
    /// reload instead ([`Self::refresh_node`] on the database).
    pub fn apply_hidden_schemas(
        &mut self,
        server_id: Uuid,
        database: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(hidden) = self
            .profiles
            .iter()
            .find(|p| p.id.0 == server_id)
            .and_then(|p| p.hidden_schemas_in(database))
            .cloned()
        else {
            return;
        };
        let db_id = format!("srv:{server_id}:db:{}", escape_id_segment(database));
        if let Some(db_node) = find_node_any_mut(&mut self.roots, &db_id) {
            prune_hidden_schemas(&mut db_node.children, &hidden);
        }
        self.bump_tree_revision();
        cx.notify();
    }

    /// Seed a database's subtree from an IntelliSense data-model snapshot, so
    /// every schema/table/view/column exists in the tree (and is findable by
    /// the filter) without the user expanding anything — the lazy loader only
    /// ever materialized what was expanded, which made the filter blind to the
    /// rest of the database.
    ///
    /// Only `Unloaded` levels are filled: anything the lazy loader (or the
    /// user) already fetched from the server is authoritative and left
    /// untouched. Levels the snapshot cannot describe (indexes, statistics,
    /// foreign-key folders, user-defined types) stay lazy.
    ///
    /// When the server's database list hasn't loaded yet, the active
    /// database's node is fabricated so hydration lands immediately; the real
    /// listing replaces it and the app re-hydrates on
    /// [`SchemaTreeEvent::DatabasesLoaded`].
    pub fn hydrate_from_snapshot(
        &mut self,
        profile_id: Uuid,
        database: &str,
        snapshot: &sqlly_lang::sqlmodel::ModelSnapshot,
        schemas: &[String],
        cx: &mut Context<Self>,
    ) {
        if database.is_empty() || (snapshot.is_empty() && schemas.is_empty()) {
            return;
        }
        let server_id = format!("srv:{profile_id}");
        let db_id = format!("{server_id}:db:{}", escape_id_segment(database));
        let prefs = self.data_model_prefs;

        if find_node_any_mut(&mut self.roots, &db_id).is_none() {
            let Some(server) = find_server_node_mut(&mut self.roots, &server_id) else {
                return;
            };
            let depth = server.depth + 1;
            server.children.push(TreeNode::new(
                db_id.clone(),
                database.to_string(),
                NodeKind::Database(database.to_string()),
                depth,
            ));
        }
        let profile = self.profile_for_node_id(&db_id);
        let db_type = profile
            .as_ref()
            .map(|p| p.database_type)
            .unwrap_or(sqlly_lang::sqlmodel::DatabaseType::SqlServer);
        let Some(db_node) = find_node_any_mut(&mut self.roots, &db_id) else {
            return;
        };
        let mut hydrated = build_hydrated_db_children(
            &db_node.id,
            db_node.depth,
            snapshot,
            schemas,
            prefs,
            db_type,
        );
        // Schemas this connection hides from the explorer stay hidden: the
        // snapshot knows them (IntelliSense still uses them), the tree must
        // not re-add them.
        if let Some(hidden) = profile.as_ref().and_then(|p| p.hidden_schemas_in(database)) {
            prune_hidden_schemas(&mut hydrated, hidden);
        }
        merge_hydrated_children(db_node, hydrated);
        self.bump_tree_revision();
        cx.notify();
    }
}

#[derive(Clone, Default)]
struct WalkCtx {
    server_id: Option<Uuid>,
    server_name: String,
    database: Option<String>,
}

fn collect_quick_open_candidates(
    nodes: &[TreeNode],
    ctx: &WalkCtx,
    out: &mut Vec<crate::quick_open::QuickOpenCandidate>,
) {
    use crate::quick_open::{QuickOpenCandidate, QuickOpenKind};
    use model::server_uuid_from_id;

    for node in nodes {
        match &node.kind {
            NodeKind::AzureAccount
            | NodeKind::AzureSubscription
            | NodeKind::AzureSqlServer
            | NodeKind::AzureDatabase(_) => continue,
            NodeKind::Server { id } => {
                out.push(QuickOpenCandidate {
                    node_id: node.id.clone(),
                    kind: QuickOpenKind::Server,
                    server_id: Some(*id),
                    server_name: node.title.clone(),
                    database: None,
                    schema: None,
                    name: node.title.clone(),
                    title: node.title.clone(),
                    title_lower: node.title_lower.clone(),
                });
                let child_ctx = WalkCtx {
                    server_id: Some(*id),
                    server_name: node.title.clone(),
                    database: None,
                };
                collect_quick_open_candidates(&node.children, &child_ctx, out);
            }
            NodeKind::Database(name) => {
                out.push(QuickOpenCandidate {
                    node_id: node.id.clone(),
                    kind: QuickOpenKind::Database,
                    server_id: ctx.server_id.or_else(|| server_uuid_from_id(&node.id)),
                    server_name: ctx.server_name.clone(),
                    database: Some(name.clone()),
                    schema: None,
                    name: name.clone(),
                    title: node.title.clone(),
                    title_lower: node.title_lower.clone(),
                });
                let child_ctx = WalkCtx {
                    server_id: ctx.server_id,
                    server_name: ctx.server_name.clone(),
                    database: Some(name.clone()),
                };
                collect_quick_open_candidates(&node.children, &child_ctx, out);
            }
            NodeKind::Table { schema, name }
            | NodeKind::View { schema, name }
            | NodeKind::Procedure { schema, name }
            | NodeKind::Function { schema, name } => {
                let kind = match &node.kind {
                    NodeKind::Table { .. } => QuickOpenKind::Table,
                    NodeKind::View { .. } => QuickOpenKind::View,
                    NodeKind::Procedure { .. } => QuickOpenKind::Procedure,
                    _ => QuickOpenKind::Function,
                };
                let title = if schema.is_empty() {
                    name.clone()
                } else {
                    format!("{schema}.{name}")
                };
                let database = ctx.database.clone().or_else(|| {
                    let db = extract_db_from_id(&node.id);
                    (!db.is_empty()).then_some(db)
                });
                out.push(QuickOpenCandidate {
                    node_id: node.id.clone(),
                    kind,
                    server_id: ctx.server_id.or_else(|| server_uuid_from_id(&node.id)),
                    server_name: ctx.server_name.clone(),
                    database,
                    schema: (!schema.is_empty()).then(|| schema.clone()),
                    name: name.clone(),
                    title_lower: title.to_lowercase(),
                    title,
                });
                collect_quick_open_candidates(&node.children, ctx, out);
            }
            _ => collect_quick_open_candidates(&node.children, ctx, out),
        }
    }
}

fn find_node_id_path(roots: &[TreeNode], target_id: &str) -> Option<Vec<String>> {
    fn walk(nodes: &[TreeNode], target_id: &str, path: &mut Vec<String>) -> bool {
        for node in nodes {
            path.push(node.id.clone());
            if node.id == target_id || walk(&node.children, target_id, path) {
                return true;
            }
            path.pop();
        }
        false
    }
    let mut path = Vec::new();
    walk(roots, target_id, &mut path).then_some(path)
}

pub fn script_object_kind_public(
    kind: &NodeKind,
) -> Option<crate::table_actions::ScriptObjectKind> {
    script_object_kind(kind)
}

/// The services-side object kind a node maps to for explorer-action templates
/// and the Deploy Cart. Materialized views are not included: their menu has
/// no Custom Actions or Deploy Cart entries, and their DDL and DROP differ
/// from a view's. Everything else is not actionable and maps to `None`.
pub(crate) fn explorer_object_kind(kind: &NodeKind) -> Option<sqlly_services::ExplorerObjectKind> {
    use sqlly_services::ExplorerObjectKind as K;
    Some(match kind {
        NodeKind::Table { .. } => K::Table,
        NodeKind::View { .. } => K::View,
        NodeKind::Procedure { .. } => K::Procedure,
        NodeKind::Function { .. } => K::Function,
        _ => return None,
    })
}

/// The user templates flagged as explorer actions for a node's kind, as menu
/// entries. Served from the in-memory catalog cache (kept current by
/// Template Explorer saves and the on-disk watcher), so opening the menu
/// never reads the file on the UI thread. An unreadable catalog lists no
/// actions here; running one reports the error.
fn custom_actions_for(kind: &NodeKind) -> Vec<menus::CustomActionEntry> {
    let Some(kind) = explorer_object_kind(kind) else {
        return Vec::new();
    };
    let templates = crate::template_explorer::try_load_user_templates().unwrap_or_default();
    sqlly_services::explorer_actions_for(&templates, kind)
        .into_iter()
        .map(|t| menus::CustomActionEntry {
            name: t.name.clone(),
            mode: t
                .explorer_action
                .as_ref()
                .map(|spec| spec.mode)
                .unwrap_or_default(),
        })
        .collect()
}

fn script_object_kind(kind: &NodeKind) -> Option<crate::table_actions::ScriptObjectKind> {
    use crate::table_actions::ScriptObjectKind as K;
    Some(match kind {
        NodeKind::Table { .. } => K::Table,
        NodeKind::View { .. } => K::View,
        NodeKind::Procedure { .. } => K::Procedure,
        NodeKind::Function { .. } => K::Function,
        NodeKind::Trigger { .. } => K::Trigger,
        NodeKind::Sequence { .. } => K::Sequence,
        NodeKind::Synonym { .. } => K::Synonym,
        NodeKind::Extension { .. } => K::Extension,
        NodeKind::Event { .. } => K::Event,
        NodeKind::MaterializedView { .. } => K::MaterializedView,
        NodeKind::Constraint { .. } => K::Constraint,
        _ => return None,
    })
}

/// Expansion and selection captured when a filter session starts; see
/// [`SchemaTree::set_filter`].
struct PreFilterState {
    expanded: std::collections::HashSet<String>,
    selected: Option<String>,
}

/// Set every node's expansion from `expanded`; nodes absent from the set
/// (including ones loaded during the search) collapse.
fn apply_expanded_ids(nodes: &mut [TreeNode], expanded: &std::collections::HashSet<String>) {
    for node in nodes {
        node.expanded = expanded.contains(&node.id);
        apply_expanded_ids(&mut node.children, expanded);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_lang::sqlmodel::DatabaseType;

    #[test]
    fn system_object_classification_is_engine_aware() {
        // SQL Server: the four shipped databases and its catalog schemas.
        for db in ["master", "tempdb", "model", "MSDB"] {
            assert!(is_system_database(db, DatabaseType::SqlServer), "{db}");
        }
        assert!(!is_system_database("Sales", DatabaseType::SqlServer));
        for sch in ["sys", "INFORMATION_SCHEMA", "guest", "db_owner"] {
            assert!(is_system_schema(sch, DatabaseType::SqlServer), "{sch}");
        }
        assert!(!is_system_schema("dbo", DatabaseType::SqlServer));

        // PostgreSQL: the template + maintenance databases, and the pg_* /
        // information_schema / crdb_internal catalog schemas.
        for db in ["template0", "template1", "postgres"] {
            assert!(is_system_database(db, DatabaseType::Postgres), "{db}");
        }
        assert!(!is_system_database("app_db", DatabaseType::Postgres));
        for sch in ["pg_catalog", "pg_toast", "information_schema"] {
            assert!(is_system_schema(sch, DatabaseType::Postgres), "{sch}");
        }
        assert!(is_system_schema("crdb_internal", DatabaseType::CockroachDb));
        assert!(!is_system_schema("public", DatabaseType::Postgres));

        // The Postgres template/maintenance names are not treated as system on
        // SQL Server, where a real user database could legitimately use them.
        assert!(!is_system_database("postgres", DatabaseType::SqlServer));
        assert!(!is_system_schema("pg_catalog", DatabaseType::SqlServer));
    }

    /// A grouping header with no entity identity attached — what the tree builds
    /// for a bucket whose client/project/environment has no color or icon set,
    /// and the shape most of these tests care about.
    fn test_group_kind() -> NodeKind {
        NodeKind::Group {
            color_hex: None,
            image_path: None,
        }
    }

    /// A server → Tables folder → three tables, with the folder collapsed —
    /// the shape the filter has to see through.
    fn filter_fixture() -> Vec<TreeNode> {
        let mut server =
            TreeNode::new("srv:s", "prod-sql", NodeKind::Server { id: Uuid::nil() }, 0);
        server.expanded = true;
        let mut tables = TreeNode::new("srv:s:tables", "Tables", NodeKind::TablesFolder, 1);
        // Deliberately collapsed: a filter the user must expand folders to use
        // is not a filter.
        tables.expanded = false;
        tables.children = ["Account", "AccountBalance", "Invoice"]
            .iter()
            .map(|n| {
                TreeNode::new(
                    format!("srv:s:tables:t:dbo:{n}"),
                    format!("dbo.{n}"),
                    NodeKind::Table {
                        schema: "dbo".to_string(),
                        name: n.to_string(),
                    },
                    2,
                )
            })
            .collect();
        server.children = vec![tables];
        vec![server]
    }

    fn compile_q(needle: &str) -> crate::explorer_search::CompiledQuery {
        crate::explorer_search::CompiledQuery::compile(
            &crate::explorer_search::ExplorerQuery::parse(needle, Default::default()),
        )
    }

    fn filtered_titles(needle: &str) -> Vec<String> {
        let roots = filter_fixture();
        let mut out = Vec::new();
        flatten_tree_filtered(&roots, &mut out, None, &compile_q(needle));
        out.into_iter().map(|f| f.title.to_string()).collect()
    }

    /// Perf smoke: filtering a large tree must stay linear-ish. The bound is
    /// deliberately generous (debug build, shared CI hardware) — it exists to
    /// catch an accidental O(n^2), not to benchmark.
    #[test]
    fn filter_large_tree_smoke() {
        let mut server =
            TreeNode::new("srv:s", "prod-sql", NodeKind::Server { id: Uuid::nil() }, 0);
        server.expanded = true;
        let mut tables = TreeNode::new("srv:s:tables", "Tables", NodeKind::TablesFolder, 1);
        tables.expanded = true;
        tables.children = (0..300)
            .map(|t| {
                let mut table = TreeNode::new(
                    format!("t:{t}"),
                    format!("dbo.Table{t}"),
                    NodeKind::Table {
                        schema: "dbo".to_string(),
                        name: format!("Table{t}"),
                    },
                    2,
                );
                table.children = (0..30)
                    .map(|c| {
                        TreeNode::new(
                            format!("t:{t}:c:{c}"),
                            format!("col_{c}"),
                            NodeKind::Column,
                            3,
                        )
                    })
                    .collect();
                table
            })
            .collect();
        server.children = vec![tables];
        let roots = vec![server];

        let started = std::time::Instant::now();
        for i in 0..50 {
            let mut out = Vec::new();
            flatten_tree_filtered(
                &roots,
                &mut out,
                None,
                &compile_q(&format!("table{}", i % 300)),
            );
            assert!(!out.is_empty());
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(4),
            "50 filter passes over a 9k-node tree took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn filter_reveals_matches_through_collapsed_folders() {
        // The two matching tables surface with their ancestors, even though
        // Tables is collapsed. Invoice does not.
        assert_eq!(
            filtered_titles("account"),
            vec![
                "prod-sql".to_string(),
                "Tables".to_string(),
                "dbo.Account".to_string(),
                "dbo.AccountBalance".to_string(),
            ]
        );
    }

    #[test]
    fn filter_is_case_insensitive_and_matches_substrings() {
        assert_eq!(filtered_titles("invoice").len(), 3); // server + folder + hit
        assert_eq!(
            filtered_titles("balance").last().unwrap(),
            "dbo.AccountBalance"
        );
    }

    #[test]
    fn filter_ranks_exact_then_prefix_then_substring_within_a_folder() {
        let mut server =
            TreeNode::new("srv:s", "prod-sql", NodeKind::Server { id: Uuid::nil() }, 0);
        server.expanded = true;
        let mut tables = TreeNode::new("srv:s:tables", "Tables", NodeKind::TablesFolder, 1);
        tables.expanded = true;
        tables.children = ["Account", "AccountHistory", "SubAccount", "Person"]
            .iter()
            .map(|n| {
                TreeNode::new(
                    format!("srv:s:tables:t:dbo:{n}"),
                    (*n).to_string(),
                    NodeKind::Table {
                        schema: "dbo".to_string(),
                        name: (*n).to_string(),
                    },
                    2,
                )
            })
            .collect();
        server.children = vec![tables];
        let mut out = Vec::new();
        flatten_tree_filtered(&[server], &mut out, None, &compile_q("account"));
        let tables: Vec<String> = out
            .into_iter()
            .filter(|n| matches!(n.kind, NodeKind::Table { .. }))
            .map(|n| n.title.to_string())
            .collect();
        assert_eq!(tables, vec!["Account", "AccountHistory", "SubAccount"]);
    }

    #[test]
    fn filter_abbreviation_finds_camel_case_table() {
        let mut server =
            TreeNode::new("srv:s", "prod-sql", NodeKind::Server { id: Uuid::nil() }, 0);
        server.expanded = true;
        let mut tables = TreeNode::new("srv:s:tables", "Tables", NodeKind::TablesFolder, 1);
        tables.children = vec![TreeNode::new(
            "srv:s:tables:t:dbo:SalesOrderHeader",
            "SalesOrderHeader",
            NodeKind::Table {
                schema: "dbo".into(),
                name: "SalesOrderHeader".into(),
            },
            2,
        )];
        server.children = vec![tables];
        let mut out = Vec::new();
        flatten_tree_filtered(&[server], &mut out, None, &compile_q("SOH"));
        assert!(out.iter().any(|n| n.title.as_ref() == "SalesOrderHeader"));
    }

    #[test]
    fn filter_regex_mode_matches_pattern() {
        let mut server =
            TreeNode::new("srv:s", "prod-sql", NodeKind::Server { id: Uuid::nil() }, 0);
        server.expanded = true;
        let mut tables = TreeNode::new("srv:s:tables", "Tables", NodeKind::TablesFolder, 1);
        tables.children = ["Account", "AccountHistory", "Person"]
            .iter()
            .map(|n| {
                TreeNode::new(
                    format!("t:{n}"),
                    (*n).to_string(),
                    NodeKind::Table {
                        schema: "dbo".into(),
                        name: (*n).to_string(),
                    },
                    2,
                )
            })
            .collect();
        server.children = vec![tables];
        let mut out = Vec::new();
        flatten_tree_filtered(&[server], &mut out, None, &compile_q("/^acc.*ry$/"));
        let tables: Vec<String> = out
            .into_iter()
            .filter(|n| matches!(n.kind, NodeKind::Table { .. }))
            .map(|n| n.title.to_string())
            .collect();
        assert_eq!(tables, vec!["AccountHistory"]);
    }

    #[test]
    fn filter_scope_tables_hides_matching_views() {
        let mut server =
            TreeNode::new("srv:s", "prod-sql", NodeKind::Server { id: Uuid::nil() }, 0);
        server.expanded = true;
        let mut tables = TreeNode::new("srv:s:tables", "Tables", NodeKind::TablesFolder, 1);
        tables.children.push(TreeNode::new(
            "t:Account",
            "Account",
            NodeKind::Table {
                schema: "dbo".into(),
                name: "Account".into(),
            },
            2,
        ));
        let mut views = TreeNode::new("srv:s:views", "Views", NodeKind::ViewsFolder, 1);
        views.children.push(TreeNode::new(
            "v:AccountView",
            "AccountView",
            NodeKind::View {
                schema: "dbo".into(),
                name: "AccountView".into(),
            },
            2,
        ));
        server.children = vec![tables, views];
        let mut scopes = std::collections::BTreeSet::new();
        scopes.insert(crate::explorer_search::ObjectScope::Tables);
        let q = crate::explorer_search::CompiledQuery::compile(
            &crate::explorer_search::ExplorerQuery::parse("acc", scopes),
        );
        let mut out = Vec::new();
        flatten_tree_filtered(&[server], &mut out, None, &q);
        assert!(
            out.iter()
                .any(|n| matches!(n.kind, NodeKind::Table { .. }) && n.title.as_ref() == "Account")
        );
        assert!(!out.iter().any(|n| matches!(n.kind, NodeKind::View { .. })));
        assert!(out.iter().any(|n| n.title.as_ref() == "Tables"));
    }

    fn table_kind() -> NodeKind {
        NodeKind::Table {
            schema: "dbo".into(),
            name: "Account".into(),
        }
    }

    #[test]
    fn tree_level_indent_is_twelve_px() {
        assert_eq!(TREE_LEVEL_INDENT, 12.0);
    }

    #[test]
    fn tree_row_inset_is_tighter_than_the_old_gutter() {
        assert_eq!(TREE_ROW_INSET, 8.0);
    }

    #[test]
    fn collapse_all_collapses_without_dropping_children() {
        let mut roots = filter_fixture();
        assert!(roots[0].expanded);
        assert_eq!(roots[0].children[0].children.len(), 3);
        collapse_all_nodes(&mut roots);
        assert!(!roots[0].expanded);
        assert!(!roots[0].children[0].expanded);
        assert_eq!(roots[0].children[0].children.len(), 3);
        assert_eq!(roots[0].children[0].children[0].title, "dbo.Account");
    }

    /// Connection state is per server: any number can be `Connected` at once,
    /// disconnecting one leaves the others untouched, and a deleted profile's
    /// entry is pruned.
    #[gpui::test]
    fn connection_state_is_tracked_per_server(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let services = Services::new();
            let log =
                crate::log_console::DiagnosticLog::new(crate::log_console::DiagnosticLogLevel::Off);
            let tree = cx.new(|cx| SchemaTree::new(services, log, cx));
            tree.update(cx, |tree, cx| {
                let a = profile("alpha");
                let b = profile("beta");
                let (a_id, b_id) = (a.id.0, b.id.0);
                tree.set_connections(vec![a.clone(), b], cx);

                tree.set_connection_state(a_id, ConnectionState::Connected, cx);
                tree.set_connection_state(b_id, ConnectionState::Connected, cx);
                assert_eq!(tree.connection_state(a_id), ConnectionState::Connected);
                assert_eq!(tree.connection_state(b_id), ConnectionState::Connected);

                tree.set_connection_state(a_id, ConnectionState::Disconnected, cx);
                assert_eq!(tree.connection_state(a_id), ConnectionState::Disconnected);
                assert_eq!(
                    tree.connection_state(b_id),
                    ConnectionState::Connected,
                    "disconnecting one server must not touch another"
                );

                tree.set_connection_state(a_id, ConnectionState::Unreachable, cx);
                assert_eq!(tree.connection_state(a_id), ConnectionState::Unreachable);

                // A new connect attempt clears a stale red dot without
                // claiming green — and leaves other states alone.
                tree.clear_unreachable(a_id, cx);
                assert_eq!(tree.connection_state(a_id), ConnectionState::Disconnected);
                tree.clear_unreachable(b_id, cx);
                assert_eq!(
                    tree.connection_state(b_id),
                    ConnectionState::Connected,
                    "clear_unreachable must not demote a connected server"
                );

                // Explicit disconnect returns the node to the default state
                // and invalidates any in-flight enumeration so a late load
                // completion cannot re-green it.
                let b_node_id = format!("srv:{b_id}");
                let generation_before =
                    find_server_node(&tree.roots, &b_node_id).map(|n| n.load_generation);
                tree.disconnect_server(b_id, cx);
                assert_eq!(tree.connection_state(b_id), ConnectionState::Disconnected);
                let generation_after =
                    find_server_node(&tree.roots, &b_node_id).map(|n| n.load_generation);
                assert_ne!(
                    generation_before, generation_after,
                    "disconnect must invalidate the server's pending load"
                );

                // Dropping a profile prunes its health entry.
                tree.set_connections(vec![a], cx);
                assert_eq!(tree.connection_state(b_id), ConnectionState::Disconnected);
            });
        });
    }

    /// The first profile load restores persisted last-known health as stale
    /// states; live observations and explicit disconnects then own the entry,
    /// and later profile refreshes never re-seed.
    #[gpui::test]
    fn persisted_health_seeds_stale_states_once(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let services = Services::new();
            let log =
                crate::log_console::DiagnosticLog::new(crate::log_console::DiagnosticLogLevel::Off);
            let tree = cx.new(|cx| SchemaTree::new(services, log, cx));
            tree.update(cx, |tree, cx| {
                let now = chrono::Utc::now();
                let mut green = profile("green");
                green.status.record_connected(now);
                let mut red = profile("red");
                red.status.record_failure(now, "timeout");
                let untested = profile("untested");
                let (green_id, red_id, untested_id) = (green.id.0, red.id.0, untested.id.0);

                tree.set_connections(vec![green.clone(), red.clone(), untested.clone()], cx);
                assert_eq!(
                    tree.connection_state(green_id),
                    ConnectionState::StaleConnected,
                    "a persisted green restores as stale, never live"
                );
                assert_eq!(
                    tree.connection_state(red_id),
                    ConnectionState::StaleUnreachable
                );
                assert_eq!(
                    tree.connection_state(untested_id),
                    ConnectionState::Disconnected
                );

                // A fresh attempt clears the stale red without claiming green.
                tree.clear_unreachable(red_id, cx);
                assert_eq!(tree.connection_state(red_id), ConnectionState::Disconnected);

                // Live observations replace stale states.
                tree.set_connection_state(green_id, ConnectionState::Connected, cx);
                assert_eq!(tree.connection_state(green_id), ConnectionState::Connected);

                // An explicit disconnect stays gray across profile refreshes —
                // the seeding happens once per session, not on every update.
                tree.disconnect_server(green_id, cx);
                tree.set_connections(vec![green, red, untested], cx);
                assert_eq!(
                    tree.connection_state(green_id),
                    ConnectionState::Disconnected,
                    "profile refresh must not resurrect stale health"
                );
                assert_eq!(tree.connection_state(red_id), ConnectionState::Disconnected);
            });
        });
    }

    /// Stale states render as their own dimmed dots, and are distinct from
    /// live health in the mapping.
    #[test]
    fn stale_states_map_to_stale_health_dots() {
        use ConnectionState as C;
        let server = NodeKind::Server { id: Uuid::nil() };
        assert_eq!(
            server_health(&server, &LoadState::Loaded, C::StaleConnected),
            Some(ServerHealth::StaleConnected)
        );
        assert_eq!(
            server_health(&server, &LoadState::Loaded, C::StaleUnreachable),
            Some(ServerHealth::StaleFailed)
        );
        // A live load in flight outranks last-known state.
        assert_eq!(
            server_health(&server, &LoadState::Loading, C::StaleConnected),
            Some(ServerHealth::Connecting)
        );
    }

    #[test]
    fn server_health_dot_is_green_amber_or_red() {
        use ConnectionState as C;
        let server = NodeKind::Server { id: Uuid::nil() };
        assert_eq!(
            server_health(&server, &LoadState::Loading, C::Disconnected),
            Some(ServerHealth::Connecting)
        );
        assert_eq!(
            server_health(
                &server,
                &LoadState::Failed("timeout".into()),
                C::Disconnected
            ),
            Some(ServerHealth::Failed)
        );
        assert_eq!(
            server_health(&server, &LoadState::Loaded, C::Connected),
            Some(ServerHealth::Connected)
        );
        // Red must mean "we tried and could not reach it", regardless of the
        // node's load state.
        assert_eq!(
            server_health(&server, &LoadState::Loaded, C::Unreachable),
            Some(ServerHealth::Failed)
        );
        assert_eq!(
            server_health(&server, &LoadState::Unloaded, C::Disconnected),
            None
        );
        assert!(server_health(&table_kind(), &LoadState::Loaded, C::Connected).is_none());
    }

    #[test]
    fn filter_keeps_ancestors_of_a_match_and_drops_everything_else() {
        // A needle matching only the folder shows the folder, not its children.
        assert_eq!(filtered_titles("tables"), vec!["prod-sql", "Tables"]);
        // No match anywhere yields nothing, which the render turns into a
        // "nothing matches" state rather than a bare empty rail.
        assert!(filtered_titles("zzz").is_empty());
    }

    #[test]
    fn find_object_path_walks_collapsed_ancestors() {
        // Collapsed server + folder: the path still resolves — reveal is
        // what expands it.
        let mut server = TreeNode::new("s", "prod", NodeKind::Server { id: Uuid::nil() }, 0);
        let mut tables = TreeNode::new("s:t", "Tables", NodeKind::TablesFolder, 1);
        tables.children.push(TreeNode::new(
            "s:t:orders",
            "dbo.Orders",
            NodeKind::Table {
                schema: "dbo".into(),
                name: "Orders".into(),
            },
            2,
        ));
        server.children.push(tables);
        let roots = vec![server];

        let path = find_object_path(&roots, None, Some("dbo"), "orders").expect("found");
        assert_eq!(path, vec!["s", "s:t", "s:t:orders"]);
        // Schema filter is honored.
        assert!(find_object_path(&roots, None, Some("sales"), "Orders").is_none());
        // Server scoping: a different server id finds nothing.
        assert!(
            find_object_path(&roots, Some(Uuid::from_u128(7)), Some("dbo"), "Orders").is_none()
        );
        // The right server id scopes into the subtree.
        assert!(find_object_path(&roots, Some(Uuid::nil()), None, "Orders").is_some());
    }

    #[gpui::test]
    fn quick_open_candidates_flattens_loaded_objects_only(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let services = Services::new();
            let log =
                crate::log_console::DiagnosticLog::new(crate::log_console::DiagnosticLogLevel::Off);
            let tree = cx.new(|cx| SchemaTree::new(services, log, cx));
            tree.update(cx, |tree, _cx| {
                tree.roots = quick_open_fixture();
                let cands = tree.quick_open_candidates();
                assert!(cands.iter().all(|c| !matches!(
                    c.kind,
                    crate::quick_open::QuickOpenKind::Table
                ) || c.schema.as_deref() == Some("dbo")),);
                let kinds: Vec<_> = cands.iter().map(|c| c.kind).collect();
                assert_eq!(
                    kinds,
                    vec![
                        crate::quick_open::QuickOpenKind::Server,
                        crate::quick_open::QuickOpenKind::Database,
                        crate::quick_open::QuickOpenKind::Table,
                        crate::quick_open::QuickOpenKind::Server,
                    ]
                );
                let table = cands
                    .iter()
                    .find(|c| c.kind == crate::quick_open::QuickOpenKind::Table)
                    .expect("table");
                assert_eq!(table.server_id, Some(Uuid::nil()));
                assert_eq!(table.database.as_deref(), Some("sales"));
                assert_eq!(table.schema.as_deref(), Some("dbo"));
                assert_eq!(table.name, "Orders");
                assert_eq!(table.title, "dbo.Orders");
                assert!(!cands.iter().any(|c| c.title == "Tables"));
                assert_eq!(cands[3].kind, crate::quick_open::QuickOpenKind::Server);
                assert_eq!(cands[3].title, "idle");
                assert!(cands[3].database.is_none());
            });
        });
    }

    /// Cmd+C / Ctrl+C on the focused tree copies the selected row's name — the
    /// same payload its Copy Name menu item writes — and does nothing (so the
    /// chord falls through) when nothing is selected.
    #[gpui::test]
    fn copy_selected_name_uses_the_selected_rows_name(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let services = Services::new();
            let log =
                crate::log_console::DiagnosticLog::new(crate::log_console::DiagnosticLogLevel::Off);
            let tree = cx.new(|cx| SchemaTree::new(services, log, cx));
            tree.update(cx, |tree, cx| {
                tree.roots = quick_open_fixture();

                assert!(
                    !tree.copy_selected_name(cx),
                    "nothing selected: nothing to copy"
                );

                // A server root — the kind that had no copyable name at all.
                tree.selected = Some("s".into());
                let ctx = tree
                    .explorer_action_context_for_selection()
                    .expect("server context");
                assert_eq!(ctx.copy_name.as_deref(), Some("prod"));
                assert!(tree.copy_selected_name(cx));

                // A table leaf still copies its raw name, not the display title.
                tree.selected = Some("s:t:orders".into());
                let ctx = tree
                    .explorer_action_context_for_selection()
                    .expect("table context");
                assert_eq!(ctx.copy_name.as_deref(), Some("Orders"));
                assert_eq!(ctx.copy_qualified.as_deref(), Some("[dbo].[Orders]"));
                assert!(tree.copy_selected_name(cx));
            });
        });
    }

    /// Clearing the filter restores the expansion the tree had before the
    /// search: the filter reveals rows under collapsed folders, and clicking
    /// those rows flips stored expansion that the unfiltered view honours.
    #[gpui::test]
    fn clearing_filter_restores_pre_search_expansion(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let services = Services::new();
            let log =
                crate::log_console::DiagnosticLog::new(crate::log_console::DiagnosticLogLevel::Off);
            let tree = cx.new(|cx| SchemaTree::new(services, log, cx));
            tree.update(cx, |tree, cx| {
                tree.roots = filter_fixture();
                tree.set_filter_text("account".into(), cx);
                // The filter shows the Tables folder open; a click on it (to
                // "collapse" what looks open) flips the stored state to true.
                tree.toggle_expand("srv:s:tables", cx);
                assert!(tree.roots[0].children[0].expanded);
                tree.set_filter_text("".into(), cx);
                assert!(tree.roots[0].expanded, "server was open before the search");
                assert!(
                    !tree.roots[0].children[0].expanded,
                    "Tables was collapsed before the search and must be again"
                );
                let titles: Vec<String> = tree
                    .visible_rows()
                    .iter()
                    .map(|r| r.title.to_string())
                    .collect();
                assert_eq!(titles, vec!["prod-sql", "Tables"]);
            });
        });
    }

    /// The one exception: a row picked during the search stays revealed and
    /// is scrolled into view once the filter clears.
    #[gpui::test]
    fn clearing_filter_keeps_picked_row_revealed(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let services = Services::new();
            let log =
                crate::log_console::DiagnosticLog::new(crate::log_console::DiagnosticLogLevel::Off);
            let tree = cx.new(|cx| SchemaTree::new(services, log, cx));
            tree.update(cx, |tree, cx| {
                tree.roots = filter_fixture();
                tree.set_filter_text("balance".into(), cx);
                tree.selected = Some("srv:s:tables:t:dbo:AccountBalance".into());
                tree.set_filter_text("".into(), cx);
                assert!(
                    tree.roots[0].children[0].expanded,
                    "the picked table's folder stays open"
                );
                assert_eq!(
                    tree.selected.as_deref(),
                    Some("srv:s:tables:t:dbo:AccountBalance")
                );
                assert!(
                    tree.visible_rows()
                        .iter()
                        .any(|r| r.id.as_ref() == "srv:s:tables:t:dbo:AccountBalance"),
                    "the picked row is on screen after the filter clears"
                );
            });
        });
    }

    /// A selection that predates the search is not a pick: clearing restores
    /// the pre-search expansion even though something is selected.
    #[gpui::test]
    fn clearing_filter_ignores_pre_existing_selection(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let services = Services::new();
            let log =
                crate::log_console::DiagnosticLog::new(crate::log_console::DiagnosticLogLevel::Off);
            let tree = cx.new(|cx| SchemaTree::new(services, log, cx));
            tree.update(cx, |tree, cx| {
                tree.roots = filter_fixture();
                tree.selected = Some("srv:s:tables".into());
                tree.set_filter_text("account".into(), cx);
                tree.toggle_expand("srv:s:tables", cx);
                tree.set_filter_text("".into(), cx);
                assert!(!tree.roots[0].children[0].expanded);
            });
        });
    }

    #[gpui::test]
    fn reveal_node_id_expands_ancestors_and_selects(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let services = Services::new();
            let log =
                crate::log_console::DiagnosticLog::new(crate::log_console::DiagnosticLogLevel::Off);
            let tree = cx.new(|cx| SchemaTree::new(services, log, cx));
            tree.update(cx, |tree, cx| {
                tree.roots = quick_open_fixture();
                assert!(tree.reveal_node_id("s:t:orders", cx));
                assert!(tree.roots[0].expanded);
                assert!(tree.roots[0].children[0].expanded);
                assert!(tree.roots[0].children[0].children[0].expanded);
                assert_eq!(tree.selected.as_deref(), Some("s:t:orders"));
                assert!(!tree.reveal_node_id("missing", cx));
            });
        });
    }

    /// A database's read actions must stay enabled as long as the database's
    /// *own* server has a saved profile — opening a query retargets to that
    /// server on demand. They must not require it to be the single globally
    /// active connection: gating on `self.active` disabled every item between
    /// Copy Name and Refresh whenever another server (or none) was active.
    #[gpui::test]
    fn database_menu_availability_keys_on_the_nodes_own_server(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let services = Services::new();
            let log =
                crate::log_console::DiagnosticLog::new(crate::log_console::DiagnosticLogLevel::Off);
            let tree = cx.new(|cx| SchemaTree::new(services, log, cx));
            tree.update(cx, |tree, cx| {
                let profile = profile("prod");
                let server_id = profile.id.0;
                tree.set_connections(vec![profile], cx);
                // No server is the active connection.
                tree.active = None;

                let mut ctx = ctx_for_kind(NodeKind::Database("sales".into()));
                ctx.server_id = Some(server_id);
                let avail = tree.menu_availability(&ctx);
                assert!(
                    avail.has_connection,
                    "a database whose server has a profile can run read actions even with no active connection"
                );
                assert!(
                    !avail.is_connected,
                    "is_connected tracks this server's own connection state, which is untried here"
                );

                // A different active server must not change the answer.
                tree.active = Some(Uuid::new_v4());
                assert!(tree.menu_availability(&ctx).has_connection);

                // Once this server is observed connected, its nodes report
                // is_connected regardless of the active pointer.
                tree.set_connection_state(server_id, ConnectionState::Connected, cx);
                assert!(tree.menu_availability(&ctx).is_connected);
                tree.set_connection_state(server_id, ConnectionState::Disconnected, cx);
                assert!(!tree.menu_availability(&ctx).is_connected);

                // A database whose server has no saved profile stays disabled.
                let mut orphan = ctx.clone();
                orphan.server_id = Some(Uuid::new_v4());
                assert!(!tree.menu_availability(&orphan).has_connection);
            });
        });
    }

    fn quick_open_fixture() -> Vec<TreeNode> {
        let mut server = TreeNode::new("s", "prod", NodeKind::Server { id: Uuid::nil() }, 0);
        let mut db = TreeNode::new("s:db:sales", "sales", NodeKind::Database("sales".into()), 1);
        let mut tables = TreeNode::new("s:t", "Tables", NodeKind::TablesFolder, 2);
        tables.children.push(TreeNode::new(
            "s:t:orders",
            "dbo.Orders",
            NodeKind::Table {
                schema: "dbo".into(),
                name: "Orders".into(),
            },
            3,
        ));
        db.children.push(tables);
        server.children.push(db);
        let idle = TreeNode::new(
            "idle",
            "idle",
            NodeKind::Server {
                id: Uuid::from_u128(9),
            },
            0,
        );
        vec![server, idle]
    }

    #[test]
    fn expand_node_by_id_expands_nested_nodes() {
        let mut server = TreeNode::new("s", "prod", NodeKind::Server { id: Uuid::nil() }, 0);
        server
            .children
            .push(TreeNode::new("s:t", "Tables", NodeKind::TablesFolder, 1));
        let mut roots = vec![server];
        assert!(expand_node_by_id(&mut roots, "s:t"));
        assert!(roots[0].children[0].expanded);
        assert!(!expand_node_by_id(&mut roots, "missing"));
    }

    fn table_with_columns_fixture() -> Vec<TreeNode> {
        let mut server =
            TreeNode::new("srv:s", "prod-sql", NodeKind::Server { id: Uuid::nil() }, 0);
        server.expanded = true;
        let mut tables = TreeNode::new("srv:s:tables", "Tables", NodeKind::TablesFolder, 1);
        tables.expanded = true;
        let mut orders = TreeNode::new(
            "srv:s:tables:t:dbo:Orders",
            "dbo.Orders",
            NodeKind::Table {
                schema: "dbo".into(),
                name: "Orders".into(),
            },
            2,
        );
        orders.children = ["Id", "Total", "PlacedOn"]
            .iter()
            .map(|c| {
                TreeNode::new(
                    format!("srv:s:tables:t:dbo:Orders:c:{c}"),
                    c.to_string(),
                    NodeKind::Column,
                    3,
                )
            })
            .collect();
        tables.children = vec![orders];
        server.children = vec![tables];
        vec![server]
    }

    #[test]
    fn filter_matched_table_reveals_all_its_columns() {
        // Inclusion-plus: "orders" matches the table, so EVERY column shows even
        // though none of the column names contain "orders".
        let roots = table_with_columns_fixture();
        let mut out = Vec::new();
        flatten_tree_filtered(&roots, &mut out, None, &compile_q("orders"));
        let titles: Vec<String> = out.into_iter().map(|f| f.title.to_string()).collect();
        assert_eq!(
            titles,
            vec![
                "prod-sql",
                "Tables",
                "dbo.Orders",
                "Id",
                "Total",
                "PlacedOn"
            ]
        );
    }

    #[test]
    fn filter_matched_column_reveals_its_table_with_only_the_match() {
        // The table name does not contain "total", but a column does: show the
        // table and only the matching column.
        let roots = table_with_columns_fixture();
        let mut out = Vec::new();
        flatten_tree_filtered(&roots, &mut out, None, &compile_q("total"));
        let titles: Vec<String> = out.into_iter().map(|f| f.title.to_string()).collect();
        assert_eq!(titles, vec!["prod-sql", "Tables", "dbo.Orders", "Total"]);
    }

    #[test]
    fn filter_match_work_is_bounded_by_node_count() {
        // A wide, deep synthetic tree: one folder, W tables, each with C columns.
        // The filter must compare each node's title at most once — the old
        // implementation re-scanned every node's whole subtree per ancestor.
        const W: usize = 40;
        const C: usize = 25;
        let mut folder = TreeNode::new("srv:x:db:D:tables", "Tables", NodeKind::TablesFolder, 0);
        for t in 0..W {
            let mut table = TreeNode::new(
                format!("srv:x:db:D:t:dbo:T{t}"),
                format!("T{t}"),
                NodeKind::Table {
                    schema: "dbo".into(),
                    name: format!("T{t}"),
                },
                1,
            );
            table.children = (0..C)
                .map(|c| {
                    TreeNode::new(
                        format!("srv:x:db:D:t:dbo:T{t}:c:Col{c}"),
                        format!("Col{c}"),
                        NodeKind::Column,
                        2,
                    )
                })
                .collect();
            folder.children.push(table);
        }
        let roots = vec![folder];
        let total_nodes = 1 + W + W * C;

        // A needle that matches nothing exercises the whole tree.
        NODE_MATCH_CALLS.with(|c| c.set(0));
        let mut out = Vec::new();
        let matched = flatten_tree_filtered(&roots, &mut out, None, &compile_q("zzz-no-match"));
        assert!(!matched);
        assert!(out.is_empty());
        let calls = NODE_MATCH_CALLS.with(|c| c.get());
        assert_eq!(
            calls, total_nodes,
            "expected exactly one comparison per node ({total_nodes}), got {calls}"
        );

        // A single deep match still costs one comparison per node — never more.
        NODE_MATCH_CALLS.with(|c| c.set(0));
        let mut out = Vec::new();
        assert!(flatten_tree_filtered(
            &roots,
            &mut out,
            None,
            &compile_q("col7")
        ));
        let calls = NODE_MATCH_CALLS.with(|c| c.get());
        assert_eq!(calls, total_nodes, "match must not re-scan: {calls}");
    }

    #[gpui::test]
    fn flatten_cache_hits_when_revision_and_needle_unchanged(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let services = Services::new();
            let log =
                crate::log_console::DiagnosticLog::new(crate::log_console::DiagnosticLogLevel::Off);
            let tree = cx.new(|cx| SchemaTree::new(services, log, cx));
            tree.update(cx, |tree, _cx| {
                tree.roots = filter_fixture();
                FLATTEN_CACHE_BUILDS.with(|c| c.set(0));
                FLATTEN_CACHE_HITS.with(|c| c.set(0));
                let first = tree.cached_flatten();
                let second = tree.cached_flatten();
                assert_eq!(
                    FLATTEN_CACHE_BUILDS.with(|c| c.get()),
                    1,
                    "unchanged revision+needle must flatten once"
                );
                assert_eq!(
                    FLATTEN_CACHE_HITS.with(|c| c.get()),
                    1,
                    "the second call must hit the cache"
                );
                let ids =
                    |rows: &[FlatNode]| rows.iter().map(|r| r.id.to_string()).collect::<Vec<_>>();
                assert_eq!(ids(&first), ids(&second));
                // Cache hit must share the flatten Rc, not clone Vec contents
                // (`NodeKind` owns Strings; a Vec clone is not an Arc bump).
                assert!(
                    Rc::ptr_eq(&first, &second),
                    "cache hit must return the same Rc, not a cloned Vec"
                );
                tree.bump_tree_revision();
                let _ = tree.cached_flatten();
                assert_eq!(
                    FLATTEN_CACHE_BUILDS.with(|c| c.get()),
                    2,
                    "a revision bump must rebuild"
                );
            });
        });
    }

    #[test]
    fn flatten_zero_viewport_does_not_materialize_every_row() {
        let (start, end) = viewport_window(5_000, 0.0, 0.0, Theme::row_height());
        assert_eq!(start, 0);
        assert!(
            end < 5_000,
            "first paint must not build every row, got win_end={end}"
        );
        assert_eq!(end, FIRST_PAINT_ROW_CAP);
        let (small_start, small_end) = viewport_window(10, 0.0, 0.0, Theme::row_height());
        assert_eq!((small_start, small_end), (0, 10));
    }

    #[test]
    fn flatten_positive_viewport_windows_like_024() {
        let row_h = 20.0;
        let viewport_h = 200.0;
        let (start, end) = viewport_window(5_000, viewport_h, 0.0, row_h);
        assert_eq!(start, 0);
        assert_eq!(end, 16, "10 visible rows + 6 overscan");
        let (mid_start, mid_end) = viewport_window(5_000, viewport_h, -400.0, row_h);
        assert_eq!(mid_start, 18);
        assert_eq!(mid_end, 34);
    }

    #[test]
    fn remembered_expansion_survives_a_rebuild() {
        let mut roots = filter_fixture();
        // The user collapses the server and expands Tables.
        roots[0].expanded = false;
        roots[0].children[0].expanded = true;

        let mut remembered = std::collections::HashMap::new();
        collect_expansion(&roots, &mut remembered);

        // A rebuild hands back a tree with the builder's defaults.
        let mut rebuilt = filter_fixture();
        assert!(rebuilt[0].expanded, "fixture default is expanded");
        apply_expansion(&mut rebuilt, &remembered);

        assert!(!rebuilt[0].expanded, "collapse should survive the rebuild");
        assert!(rebuilt[0].children[0].expanded, "expand should survive too");

        // A node the snapshot never saw keeps the builder's default, so newly
        // discovered resources still open.
        let mut fresh = filter_fixture();
        fresh[0].id = "srv:brand-new".to_string();
        apply_expansion(&mut fresh, &remembered);
        assert!(fresh[0].expanded);
    }

    #[test]
    fn a_superseded_load_is_discarded_rather_than_painted_over_the_fresh_one() {
        // Two loads in flight for one node is not hypothetical: refreshing a
        // node while its first load is still running spawns a second, and the
        // slow server that made you reach for refresh is exactly the one whose
        // first answer lands last.
        let mut node = TreeNode::new("srv:s:tables", "Tables", NodeKind::TablesFolder, 1);

        // Load A starts and captures its generation.
        node.invalidate_pending_load();
        let generation_a = node.load_generation;

        // A refresh invalidates it, and load B starts.
        node.invalidate_pending_load();
        node.invalidate_pending_load();
        let generation_b = node.load_generation;

        assert_ne!(generation_a, generation_b);
        // B is current and applies; A is stale and must not.
        assert_eq!(node.load_generation, generation_b, "B applies");
        assert_ne!(node.load_generation, generation_a, "A is dropped");
    }

    #[test]
    fn generation_survives_the_subtree_being_preserved_across_a_rebuild() {
        // `rebuild_roots` carries server subtrees over wholesale, so the guard
        // has to travel with the node or a load spanning a rebuild would look
        // current again.
        let mut node = TreeNode::new("srv:s", "s", NodeKind::Server { id: Uuid::nil() }, 0);
        node.invalidate_pending_load();
        let generation = node.load_generation;
        let preserved = node.clone();
        assert_eq!(preserved.load_generation, generation);
    }

    #[test]
    fn control_characters_in_identifiers_stay_visible_and_on_one_line() {
        // `CREATE TABLE [My<newline>Table]` is legal SQL Server. Rendered raw
        // into a fixed-height row the second line is clipped, so the table reads
        // as "My" and its real name is nowhere on screen.
        assert_eq!(object_title("dbo", "My\nTable", false), "dbo.My·Table");
        assert_eq!(object_title("", "tab\there", true), "tab·here");
        assert!(!object_title("dbo", "My\nTable", false).contains('\n'));

        // The common path is untouched — no allocation-shaped surprises for the
        // 99.9% of names that are ordinary.
        assert_eq!(object_title("dbo", "Users", false), "dbo.Users");
        assert_eq!(
            sanitize_display("Ünïcode_Ω_日本語_📊"),
            "Ünïcode_Ω_日本語_📊"
        );
    }

    #[test]
    fn group_identities_resolve_per_mode_without_touching_disk() {
        // Built from an in-hand store, never by reading one.
        let mut ids = GroupIdentities::default();
        ids.environments.insert(
            "production".to_string(),
            (Some("#B51F1F".to_string()), None),
        );
        ids.clients
            .insert("acme".to_string(), (Some("#2E8B57".to_string()), None));

        // Case-insensitive, matching how the bucketing merges labels.
        assert_eq!(
            ids.for_label(ServerGroupingMode::ByEnvironment, "Production")
                .0,
            Some("#B51F1F".to_string())
        );
        // The mode picks which entity family answers: a client called
        // "Production" must not colour an environment bucket, and vice versa.
        assert_eq!(
            ids.for_label(ServerGroupingMode::ByClient, "Production").0,
            None
        );
        assert_eq!(
            ids.for_label(ServerGroupingMode::ByClient, "ACME").0,
            Some("#2E8B57".to_string())
        );
        // Hostname buckets and ungrouped trees have no entity behind them.
        assert_eq!(
            ids.for_label(ServerGroupingMode::ByServer, "Production").0,
            None
        );
        assert_eq!(
            ids.for_label(ServerGroupingMode::None, "Production").0,
            None
        );
        // An unknown label is simply uncolored.
        assert_eq!(
            ids.for_label(ServerGroupingMode::ByEnvironment, "Staging")
                .0,
            None
        );
    }

    #[test]
    fn row_tooltip_builds_the_qualified_name_without_a_join() {
        // Same output as the old Vec+join, now one buffer. Guards the rewrite.
        let id = format!("srv:{}:db:Prod:tables:t:Accounting:Account", Uuid::nil());
        let kind = NodeKind::Table {
            schema: "Accounting".to_string(),
            name: "Account".to_string(),
        };
        assert_eq!(
            object_tooltip(&kind, "x", None, &id),
            "Prod.Accounting.Account"
        );
        // No database in the id: no leading dot.
        let bare = format!("srv:{}:t:dbo:Users", Uuid::nil());
        assert_eq!(
            object_tooltip(
                &NodeKind::Table {
                    schema: "dbo".to_string(),
                    name: "Users".to_string()
                },
                "x",
                None,
                &bare
            ),
            "dbo.Users"
        );
        // Empty schema: no double dot.
        assert_eq!(
            object_tooltip(
                &NodeKind::Table {
                    schema: String::new(),
                    name: "Users".to_string()
                },
                "x",
                None,
                &bare
            ),
            "Users"
        );
    }

    #[test]
    fn parent_id_of_walks_out_of_collapsed_subtrees() {
        let roots = filter_fixture();
        assert_eq!(
            parent_id_of(&roots, "srv:s:tables:t:dbo:Account").as_deref(),
            Some("srv:s:tables")
        );
        assert_eq!(
            parent_id_of(&roots, "srv:s:tables").as_deref(),
            Some("srv:s")
        );
        // A root has no parent — left-arrow stops rather than wrapping.
        assert_eq!(parent_id_of(&roots, "srv:s"), None);
        assert_eq!(parent_id_of(&roots, "nope"), None);
    }

    #[test]
    fn find_node_any_reaches_nodes_the_filter_reveals() {
        let mut roots = filter_fixture();
        // `Tables` is collapsed, so the visible-tree finders cannot see through
        // it — but the filter puts these rows on screen, and expanding one or
        // retrying its load has to work on what the user can actually see.
        let hidden = "srv:s:tables:t:dbo:Account";
        assert!(find_node(&roots, hidden).is_none());
        assert!(find_node_mut(&mut roots, hidden).is_none());
        assert!(find_node_any(&roots, hidden).is_some());
        assert!(find_node_any_mut(&mut roots, hidden).is_some());
    }

    #[test]
    fn id_segments_survive_colons_in_identifiers() {
        // `CREATE DATABASE [Sales:2024]` is legal, and node ids are
        // colon-delimited: unescaped, this resolved to `Sales` and every query
        // beneath the node ran against the wrong database.
        let db = "Sales:2024";
        let id = format!("srv:{}:db:{}", uuid::Uuid::nil(), escape_id_segment(db));
        assert_eq!(extract_db_from_id(&id), db);
        assert_eq!(extract_db_from_id(&format!("{id}:tables")), db);

        // The schema scope has the same exposure.
        let schema = "odd:schema";
        let sch_id = format!("{id}:sch:{}", escape_id_segment(schema));
        assert_eq!(extract_schema_from_id(&sch_id), Some(schema.to_string()));
        assert_eq!(extract_db_from_id(&sch_id), db);

        // The escape is reversible, including for a literal `%`.
        for raw in ["plain", "a:b", "100%", "%3A", "%25:x", ":", "%"] {
            assert_eq!(unescape_id_segment(&escape_id_segment(raw)), raw, "{raw}");
        }
    }

    #[test]
    fn permission_errors_are_not_failures() {
        // The messages a locked-down box actually returns (Msg 229/262/916/4060).
        for denied in [
            "The SELECT permission was denied on the object 'Orders', database 'Prod'",
            "CREATE DATABASE permission denied in database 'master'.",
            "The server principal \"app\" is not able to access the database \
             \"msdb\" under the current security context.",
            "Cannot open database \"Reporting\" requested by the login.",
        ] {
            assert!(
                matches!(
                    classify_load_error(denied.to_string()),
                    LoadState::PermissionDenied(_)
                ),
                "should be PermissionDenied: {denied}"
            );
        }

        // A real fault stays a failure.
        for failed in [
            "Connection reset by peer",
            "timed out after 30s",
            "TDS protocol error: unexpected token",
        ] {
            assert!(
                matches!(
                    classify_load_error(failed.to_string()),
                    LoadState::Failed(_)
                ),
                "should be Failed: {failed}"
            );
        }
    }

    #[test]
    fn row_tooltip_carries_the_qualified_name() {
        let id = format!(
            "srv:{}:db:Prod:tables:t:Accounting:Account",
            uuid::Uuid::nil()
        );
        let kind = NodeKind::Table {
            schema: "Accounting".to_string(),
            name: "Account".to_string(),
        };
        // Truncated, `Accounting.Account` and `Accounting.AccountBalance` are
        // the same pixels — the tooltip is what tells them apart.
        assert_eq!(
            object_tooltip(&kind, "Accounting.Account", None, &id),
            "Prod.Accounting.Account"
        );

        // Column rows fold their type in.
        let col = object_tooltip(&NodeKind::Column, "AccountId", Some("int, not null"), &id);
        assert_eq!(col, "AccountId — int, not null");

        // Glyphless kinds spell their kind out — with no gutter icon, the
        // tooltip is what says whether the row is a view or a procedure.
        let view = NodeKind::View {
            schema: "Accounting".to_string(),
            name: "AccountSummary".to_string(),
        };
        assert_eq!(
            object_tooltip(&view, "x", None, &id),
            "Prod.Accounting.AccountSummary — view"
        );
        let proc = NodeKind::Procedure {
            schema: "dbo".to_string(),
            name: "CloseBooks".to_string(),
        };
        assert_eq!(
            object_tooltip(&proc, "x", None, &id),
            "Prod.dbo.CloseBooks — stored procedure"
        );
        // Glyphed kinds stay bare — the icon already answers "what is it".
        assert_eq!(
            object_tooltip(&kind, "x", None, &id),
            "Prod.Accounting.Account"
        );

        // The row shows a status dot, not the word "failed"; the whole
        // message lives here. The reason must survive onto *any* base
        // tooltip — including a server row's, which is where loads actually
        // fail.
        let failed = with_load_state(
            object_tooltip(&kind, "Accounting.Account", None, &id),
            &LoadState::Failed("Connection reset by peer".to_string()),
        );
        assert!(failed.contains("Connection reset by peer"), "{failed}");
        assert!(
            failed_load_inline_word().is_none(),
            "failed loads must not paint the word on the row"
        );
        let denied = with_load_state(
            "SQLHOST-1\n10.0.0.12:1433".to_string(),
            &LoadState::PermissionDenied("The SELECT permission was denied".to_string()),
        );
        assert!(denied.contains("Permission denied."), "{denied}");
        assert!(denied.contains("SQLHOST-1"), "base must survive: {denied}");
    }

    #[test]
    fn failed_load_uses_status_dot_not_word() {
        assert!(failed_load_inline_word().is_none());
    }

    #[test]
    fn object_title_prefixes_schema_only_in_flat_mode() {
        assert_eq!(object_title("dbo", "Users", false), "dbo.Users");
        assert_eq!(object_title("dbo", "Users", true), "Users");
        assert_eq!(object_title("", "Users", false), "Users");
    }

    #[test]
    fn extract_schema_from_id_finds_schema_scope() {
        assert_eq!(
            extract_schema_from_id("srv:x:db:appdb:sch:Accounting:tables"),
            Some("Accounting".to_string())
        );
        assert_eq!(extract_schema_from_id("srv:x:db:appdb:tables"), None);
        // The db extractor must be unaffected by the schema segment.
        assert_eq!(
            extract_db_from_id("srv:x:db:appdb:sch:Accounting:tables"),
            "appdb"
        );
        // System-database folder ids keep the db segment reachable.
        assert_eq!(extract_db_from_id("srv:x:sysdbs:db:master"), "master");
    }

    #[test]
    fn align_info_covers_widest_sibling_name_and_type() {
        let mut table = TreeNode::new(
            "t",
            "WorkflowProcess",
            NodeKind::Table {
                schema: "dbo".into(),
                name: "WorkflowProcess".into(),
            },
            0,
        );
        let mut c1 = TreeNode::new("t:c:Id", "Id", NodeKind::Column, 1);
        c1.detail = Some("bigint(8), identity, not null".into());
        let mut c2 = TreeNode::new("t:c:Description", "Description", NodeKind::Column, 1);
        c2.detail = Some("nvarchar(4000), nullable".into());
        table.children = vec![c1, c2];

        let info = align_info_for(&table).expect("columns produce alignment");
        assert_eq!(info.name_chars, "Description".len());
        assert_eq!(info.type_chars, "nvarchar(4000)".len());
        assert!(matches!(
            info.surface,
            crate::preferences::AlignSurface::TableColumns
        ));
        // A folder parent yields no alignment.
        let folder = TreeNode::new("f", "Tables", NodeKind::TablesFolder, 0);
        assert!(align_info_for(&folder).is_none());
    }

    #[test]
    fn reset_subtree_loads_collapses_shape_bearing_levels() {
        let mut server = TreeNode::new(
            "srv:00000000-0000-0000-0000-000000000000",
            "local",
            NodeKind::Server {
                id: Uuid::parse_str("00000000-0000-0000-0000-000000000000").unwrap(),
            },
            0,
        );
        server.expanded = true;
        server.load_state = LoadState::Loaded;
        server.children = sample_tree();

        reset_subtree_loads(&mut server);
        assert!(!server.expanded);
        assert!(matches!(server.load_state, LoadState::Unloaded));
        assert!(server.children.is_empty());
    }

    /// Build a small tree: one database with the three folder children, with the
    /// database expanded so the folders are visible.
    fn sample_tree() -> Vec<TreeNode> {
        let mut db = TreeNode::new(
            "db:master",
            "master",
            NodeKind::Database("master".into()),
            0,
        );
        db.expanded = true;
        db.load_state = LoadState::Loaded;
        db.children = vec![
            TreeNode::new("db:master:tables", "Tables", NodeKind::TablesFolder, 1),
            TreeNode::new("db:master:views", "Views", NodeKind::ViewsFolder, 1),
            TreeNode::new(
                "db:master:prog",
                "Programmability",
                NodeKind::ProgrammabilityFolder,
                1,
            ),
        ];
        vec![db]
    }

    /// Regression: `flatten_tree` must surface each node's REAL id (e.g.
    /// `db:master:tables`), not a synthetic `depth:title`. Render uses this id to
    /// drive `toggle_expand`; when it was synthetic, `find_node_mut` never
    /// matched and nothing below the database level could be expanded.
    #[test]
    fn flatten_tree_emits_real_node_ids() {
        let tree = sample_tree();
        let mut flat = Vec::new();
        flatten_tree(&tree, &mut flat);

        let ids: Vec<&str> = flat.iter().map(|n| n.id.as_ref()).collect();
        assert_eq!(
            ids,
            vec![
                "db:master",
                "db:master:tables",
                "db:master:views",
                "db:master:prog",
            ]
        );
    }

    #[test]
    fn flatten_clones_row_group_built_with_the_node() {
        let tree = sample_tree();
        assert_eq!(tree[0].row_group.as_ref(), "row:db:master");
        let mut flat = Vec::new();
        flatten_tree(&tree, &mut flat);
        assert_eq!(flat[0].row_group.as_ref(), "row:db:master");
        assert_eq!(flat[1].row_group.as_ref(), "row:db:master:tables");
    }

    /// The ids emitted by `flatten_tree` must resolve back to the actual nodes
    /// via `find_node_mut` — this is the lookup the click handler performs, and
    /// it is exactly what broke when the id was synthetic.
    #[test]
    fn flattened_ids_resolve_via_find_node_mut() {
        let mut tree = sample_tree();
        let mut flat = Vec::new();
        flatten_tree(&tree, &mut flat);

        for node in &flat {
            assert!(
                find_node_mut(&mut tree, node.id.as_ref()).is_some(),
                "id {:?} from flatten_tree did not resolve via find_node_mut",
                node.id
            );
        }
    }

    /// A synthetic `depth:title` id (the old behavior) must NOT resolve — this
    /// pins down the precise cause of the regression so it can't silently
    /// return.
    #[test]
    fn synthetic_depth_title_id_does_not_resolve() {
        let mut tree = sample_tree();
        // The database row would have produced "0:master" under the old code.
        assert!(find_node_mut(&mut tree, "0:master").is_none());
        // ...while the real id resolves.
        assert!(find_node_mut(&mut tree, "db:master").is_some());
    }

    /// `database_names_from_roots` extracts the database root names and skips
    /// non-database roots (there are none in `sample_tree`, but the empty-input
    /// and multi-database cases are covered too). This backs the query-bar
    /// database dropdown.
    #[test]
    fn database_names_extracts_only_database_roots() {
        // Empty tree -> no databases.
        assert!(database_names_from_roots(&[]).is_empty());

        // sample_tree has one database root ("master") whose children are
        // folders, not databases — the folders must not appear in the names.
        let tree = sample_tree();
        let names = database_names_from_roots(&tree);
        assert_eq!(names, vec!["master".to_string()]);

        // Two database roots both surface, in order.
        let two = vec![
            TreeNode::new("db:a", "a", NodeKind::Database("a".into()), 0),
            TreeNode::new("db:b", "b", NodeKind::Database("b".into()), 0),
        ];
        assert_eq!(
            database_names_from_roots(&two),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    /// Regression (error-node pollution): when database enumeration fails,
    /// `set_profile` inserts a single synthetic error root. It must use
    /// `NodeKind::Error`, NOT `NodeKind::Database("error")`, so it can never be
    /// surfaced as a selectable database in the query-bar dropdown.
    #[test]
    fn error_root_is_not_reported_as_a_database() {
        // This mirrors exactly what set_profile builds on the Err branch.
        let roots = vec![TreeNode::new(
            "error",
            "Error: connection refused",
            NodeKind::Error,
            0,
        )];
        assert!(
            database_names_from_roots(&roots).is_empty(),
            "synthetic error node must not appear as a database name"
        );

        // A mix of a real database and an error node yields only the real db.
        let mixed = vec![
            TreeNode::new("db:real", "real", NodeKind::Database("real".into()), 0),
            TreeNode::new("error", "Error: x", NodeKind::Error, 0),
        ];
        assert_eq!(database_names_from_roots(&mixed), vec!["real".to_string()]);
    }

    /// The load-on-expand gate: `toggle_expand` triggers a child load only when
    /// a node becomes `expanded` AND is still `Unloaded`. This pins the exact
    /// predicate (`node.expanded && matches!(load_state, LoadState::Unloaded)`)
    /// used at the call site, and confirms the real id resolves through
    /// `find_node_mut` at every level so the load can actually be kicked off.
    #[test]
    fn expand_lifecycle_gates_child_load_by_id() {
        let mut tree = sample_tree();

        // A freshly discovered folder child (collapsed + never loaded).
        let folder_id = "db:master:tables";
        {
            let node = find_node_mut(&mut tree, folder_id).expect("folder resolves by real id");
            assert!(!node.expanded);
            assert!(matches!(node.load_state, LoadState::Unloaded));
            // Not expanded yet -> must NOT load.
            assert!(!(node.expanded && matches!(node.load_state, LoadState::Unloaded)));
        }

        // Simulate the expand click: flip expanded (what toggle_expand does).
        {
            let node = find_node_mut(&mut tree, folder_id).unwrap();
            node.expanded = true;
            // Now expanded + still unloaded -> load should fire.
            assert!(node.expanded && matches!(node.load_state, LoadState::Unloaded));
            node.load_state = LoadState::Loaded; // load completed
        }

        // Expanding an already-loaded node must NOT re-load.
        {
            let node = find_node_mut(&mut tree, folder_id).unwrap();
            assert!(!(node.expanded && matches!(node.load_state, LoadState::Unloaded)));
        }

        // Collapsing the database (already Loaded) must NOT trigger a load.
        {
            let db = find_node_mut(&mut tree, "db:master").unwrap();
            db.expanded = false;
            assert!(!(db.expanded && matches!(db.load_state, LoadState::Unloaded)));
        }
    }

    fn profile(name: &str) -> ConnectionProfile {
        use sqlly_core::{SecretProvider, SecretRef};
        ConnectionProfile::sql_login(
            name,
            "host",
            "sa",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: "k".into(),
            },
        )
    }

    /// A connection's notes show in its explorer tooltip, below the
    /// connection facts; a connection without notes gets no Notes line.
    #[test]
    fn server_tooltip_shows_the_connection_notes() {
        let mut p = profile("Billing");
        let plain = render::server_tooltip(&p, ConnectionState::Disconnected);
        assert!(!plain.contains("Notes:"), "{plain}");

        p.notes = Some("Owned by billing.\nNo writes during month-end.".into());
        let tooltip = render::server_tooltip(&p, ConnectionState::Disconnected);
        assert!(
            tooltip.ends_with("Notes: Owned by billing.\nNo writes during month-end."),
            "{tooltip}"
        );
    }

    /// The server uuid must be recoverable from any descendant node id, since
    /// `load_children` resolves the owning connection profile from the id.
    #[test]
    fn server_uuid_parses_from_any_descendant_id() {
        let id = Uuid::new_v4();
        let base = format!("srv:{id}");
        assert_eq!(server_uuid_from_id(&base), Some(id));
        assert_eq!(server_uuid_from_id(&format!("{base}:db:master")), Some(id));
        assert_eq!(
            server_uuid_from_id(&format!("{base}:db:master:tables")),
            Some(id)
        );
        assert_eq!(
            server_uuid_from_id(&format!("{base}:db:master:t:dbo:Orders")),
            Some(id)
        );
        // Non-server ids yield nothing.
        assert_eq!(server_uuid_from_id("group:byClient:acme"), None);
        assert_eq!(server_uuid_from_id("srv:not-a-uuid"), None);
    }

    /// `extract_db_from_id` must find the database segment in the new
    /// server-rooted id scheme (`srv:{uuid}:db:{name}[:...]`), regardless of
    /// depth, and return "" when there is no `:db:` segment.
    #[test]
    fn extract_db_handles_server_rooted_ids() {
        let id = Uuid::new_v4();
        let base = format!("srv:{id}:db:Sales");
        assert_eq!(extract_db_from_id(&base), "Sales");
        assert_eq!(extract_db_from_id(&format!("{base}:tables")), "Sales");
        assert_eq!(extract_db_from_id(&format!("{base}:t:dbo:Orders")), "Sales");
        assert_eq!(
            extract_db_from_id(&format!("{base}:prog:p:dbo:MyProc")),
            "Sales"
        );
        // A bare server id (no database yet) has no db segment.
        assert_eq!(extract_db_from_id(&format!("srv:{id}")), "");
    }

    /// A fresh server node is collapsed + unloaded, and its id/kind carry the
    /// profile's uuid so the id round-trips back to the profile.
    #[test]
    fn make_server_node_builds_fresh_collapsed_node() {
        let p = profile("Prod");
        let mut preserved = std::collections::HashMap::new();
        let node = make_server_node(&p, 0, &mut preserved, &GroupIdentities::default());
        assert_eq!(node.id, format!("srv:{}", p.id.0));
        assert_eq!(node.title, "Prod");
        assert!(!node.expanded);
        assert!(matches!(node.load_state, LoadState::Unloaded));
        assert!(matches!(node.kind, NodeKind::Server { id } if id == p.id.0));
        assert_eq!(server_uuid_from_id(&node.id), Some(p.id.0));
    }

    /// A pinned object floats to the top of the folder it organically lives in
    /// (its Tables folder), not to the top of the whole server, and the other
    /// objects keep their existing order.
    #[test]
    fn pinned_object_floats_to_top_of_its_folder() {
        let p = profile("Prod");
        let server_id = p.id.0;
        let sid = format!("srv:{server_id}");
        let table = |name: &str| {
            TreeNode::new(
                format!("{sid}:t:{name}"),
                name,
                NodeKind::Table {
                    schema: "dbo".into(),
                    name: name.into(),
                },
                4,
            )
        };
        let mut tables = TreeNode::new(
            format!("{sid}:db:db:tables"),
            "Tables",
            NodeKind::TablesFolder,
            3,
        );
        tables.children = vec![table("Alpha"), table("Orders"), table("Zeta")];
        let mut db = TreeNode::new(
            format!("{sid}:db:db"),
            "db",
            NodeKind::Database("db".into()),
            2,
        );
        db.children = vec![tables];
        let mut server = TreeNode::new(sid.clone(), "Prod", NodeKind::Server { id: server_id }, 0);
        server.children = vec![db];
        let mut roots = vec![server];

        let favorites = vec![FavoriteObject {
            server_id,
            database: Some("db".into()),
            schema: Some("dbo".into()),
            name: "Orders".into(),
            kind: PinnedKind::Table,
            order: 0,
            group: None,
        }];
        model::sort_pinned_first(&mut roots, &favorites);

        // No server-top "Pinned" folder is injected…
        assert!(
            !roots[0]
                .children
                .iter()
                .any(|c| matches!(c.kind, NodeKind::PinnedFolder { .. })),
            "pins no longer form a server-top group"
        );
        // …and the pinned table leads its own Tables folder, with the rest
        // keeping their order.
        let names: Vec<&str> = roots[0].children[0].children[0]
            .children
            .iter()
            .map(|c| c.title.as_str())
            .collect();
        assert_eq!(names, vec!["Orders", "Alpha", "Zeta"]);
    }

    /// Regrouping preserves a server's drilled-down subtree (via
    /// `collect_server_nodes` -> `make_server_node`) and reassigns depths so the
    /// subtree renders correctly at its new position (flat depth 0 vs grouped
    /// depth 1).
    #[test]
    fn make_server_node_preserves_and_redepths_subtree() {
        let p = profile("Prod");
        let sid = format!("srv:{}", p.id.0);

        // A server previously expanded at flat depth 0 with a loaded database
        // child (itself with a folder child), mimicking prior drill-down.
        let mut server = TreeNode::new(sid.clone(), "Prod", NodeKind::Server { id: p.id.0 }, 0);
        server.expanded = true;
        server.load_state = LoadState::Loaded;
        let mut db = TreeNode::new(
            format!("{sid}:db:master"),
            "master",
            NodeKind::Database("master".into()),
            1,
        );
        db.expanded = true;
        db.load_state = LoadState::Loaded;
        db.children = vec![TreeNode::new(
            format!("{sid}:db:master:tables"),
            "Tables",
            NodeKind::TablesFolder,
            2,
        )];
        server.children = vec![db];

        // Simulate a rebuild: drain into the preserved map, then rebuild at
        // depth 1 (as if now under a group header).
        let mut roots = vec![server];
        let mut preserved = std::collections::HashMap::new();
        collect_server_nodes(&mut roots, &mut preserved);
        assert!(roots.is_empty(), "server drained out of roots");
        assert!(preserved.contains_key(&sid));

        let rebuilt = make_server_node(&p, 1, &mut preserved, &GroupIdentities::default());
        // Expansion + loaded databases survived.
        assert!(rebuilt.expanded);
        assert_eq!(rebuilt.depth, 1);
        assert_eq!(rebuilt.children.len(), 1);
        // Depths were reassigned relative to the new server depth.
        assert_eq!(rebuilt.children[0].depth, 2); // database
        assert_eq!(rebuilt.children[0].children[0].depth, 3); // folder
        // Preserved entry was consumed.
        assert!(!preserved.contains_key(&sid));
    }

    /// A server node picks up its environment's identity color (matched by
    /// name, case-insensitively) so the rail can paint the env dot.
    #[test]
    fn make_server_node_carries_environment_color() {
        let mut p = profile("Prod");
        p.environment = ConnectionEnvironment::Production;
        let entities = GroupIdentities::default().with_entity(
            ServerGroupingMode::ByEnvironment,
            "production",
            Some("#C0392B"),
            None,
        );
        let mut preserved = std::collections::HashMap::new();
        let node = make_server_node(&p, 0, &mut preserved, &entities);
        assert_eq!(node.env_color_hex.as_deref(), Some("#C0392B"));
    }

    /// Local/Unset environments read as unassigned everywhere else in the app;
    /// they must not paint a dot here either.
    #[test]
    fn make_server_node_omits_color_for_unassigned_environments() {
        let entities = GroupIdentities::default().with_entity(
            ServerGroupingMode::ByEnvironment,
            "Local",
            Some("#0000FF"),
            None,
        );
        let mut preserved = std::collections::HashMap::new();
        let p = profile("Plain");
        let node = make_server_node(&p, 0, &mut preserved, &entities);
        assert_eq!(node.env_color_hex, None);
    }

    /// A preserved subtree keeps its drill-down but takes the *current*
    /// environment color — reassigning an environment between rebuilds must
    /// not leave the stale one on screen.
    #[test]
    fn make_server_node_refreshes_color_on_preserved_subtree() {
        let mut p = profile("Prod");
        p.environment = ConnectionEnvironment::Production;
        let sid = format!("srv:{}", p.id.0);
        let mut server = TreeNode::new(sid.clone(), "Prod", NodeKind::Server { id: p.id.0 }, 0);
        server.env_color_hex = Some("#111111".to_string());
        let mut roots = vec![server];
        let mut preserved = std::collections::HashMap::new();
        collect_server_nodes(&mut roots, &mut preserved);

        let entities = GroupIdentities::default().with_entity(
            ServerGroupingMode::ByEnvironment,
            "Production",
            Some("#C0392B"),
            None,
        );
        let rebuilt = make_server_node(&p, 0, &mut preserved, &entities);
        assert_eq!(rebuilt.env_color_hex.as_deref(), Some("#C0392B"));
    }

    /// `collect_server_nodes` descends through group headers so grouped server
    /// subtrees are preserved across a regroup, and `find_server_node` locates a
    /// server under a (collapsed) group header without relying on expansion.
    #[test]
    fn server_nodes_reachable_under_group_headers() {
        let id = Uuid::new_v4();
        let sid = format!("srv:{id}");
        let mut group = TreeNode::new("group:g", "Group", test_group_kind(), 0);
        group.expanded = false; // collapsed on purpose
        group.load_state = LoadState::Loaded;
        group.children = vec![TreeNode::new(
            sid.clone(),
            "Srv",
            NodeKind::Server { id },
            1,
        )];
        let roots = vec![group];

        // find_server_node ignores expansion (needed for database_names).
        assert!(find_server_node(&roots, &sid).is_some());

        // collect_server_nodes drains the nested server out of the group.
        let mut roots_mut = roots;
        let mut preserved = std::collections::HashMap::new();
        collect_server_nodes(&mut roots_mut, &mut preserved);
        assert!(preserved.contains_key(&sid));
    }

    /// `find_server_node_mut` is the mutable counterpart of
    /// `find_server_node` — it must descend through collapsed group headers
    /// so `refresh_server` can reach the server node regardless of
    /// group expansion state.
    #[test]
    fn find_server_node_mut_reaches_through_collapsed_group() {
        let id = Uuid::new_v4();
        let sid = format!("srv:{id}");
        let mut group = TreeNode::new("group:g", "Group", test_group_kind(), 0);
        group.expanded = false;
        group.load_state = LoadState::Loaded;
        let mut server = TreeNode::new(sid.clone(), "Srv", NodeKind::Server { id }, 1);
        server.expanded = true;
        server.load_state = LoadState::Loaded;
        server.children = vec![TreeNode::new(
            format!("{sid}:db:master"),
            "master",
            NodeKind::Database("master".into()),
            2,
        )];
        group.children = vec![server];
        let mut roots = vec![group];

        let found = find_server_node_mut(&mut roots, &sid);
        assert!(
            found.is_some(),
            "find_server_node_mut should find server through collapsed group"
        );
        let server_node = found.unwrap();
        assert_eq!(server_node.children.len(), 1);
        assert!(server_node.expanded);
    }

    /// `find_server_node_mut` returns `None` for a non-existent id.
    #[test]
    fn find_server_node_mut_returns_none_for_missing() {
        let id = Uuid::new_v4();
        let mut group = TreeNode::new("group:g", "Group", test_group_kind(), 0);
        group.children = vec![TreeNode::new(
            format!("srv:{id}"),
            "Srv",
            NodeKind::Server { id },
            1,
        )];
        let mut roots = vec![group];

        assert!(find_server_node_mut(&mut roots, "srv:nonexistent").is_none());
    }

    // -----------------------------------------------------------------
    // Menu builder tests
    // -----------------------------------------------------------------

    fn dummy_position() -> gpui::Point<gpui::Pixels> {
        gpui::Point::new(px(0.0), px(0.0))
    }

    fn ctx_for_kind(kind: NodeKind) -> ExplorerActionContext {
        let (schema, name) = match &kind {
            NodeKind::Table { schema, name }
            | NodeKind::View { schema, name }
            | NodeKind::Procedure { schema, name }
            | NodeKind::Function { schema, name }
            | NodeKind::Type { schema, name } => (Some(schema.clone()), Some(name.clone())),
            NodeKind::Index { schema, name, .. } | NodeKind::Statistic { schema, name, .. } => {
                (Some(schema.clone()), Some(name.clone()))
            }
            NodeKind::Constraint { schema, name, .. }
            | NodeKind::Trigger { schema, name, .. }
            | NodeKind::Sequence { schema, name }
            | NodeKind::Synonym { schema, name, .. }
            | NodeKind::Event { schema, name }
            | NodeKind::MaterializedView { schema, name } => {
                (Some(schema.clone()), Some(name.clone()))
            }
            NodeKind::Extension { name, .. } => (None, Some(name.clone())),
            NodeKind::SchemaFolder { schema } => (Some(schema.clone()), None),
            _ => (None, None),
        };
        let table = match &kind {
            NodeKind::Index { table, .. } | NodeKind::Statistic { table, .. } => {
                Some(table.clone())
            }
            NodeKind::Constraint { table, .. } => Some(table.clone()),
            NodeKind::Trigger { table, .. } => table.clone(),
            _ => None,
        };
        let server_id = match &kind {
            NodeKind::Server { id } => Some(*id),
            _ => None,
        };
        ExplorerActionContext {
            node_id: "test".into(),
            title: "test".into(),
            kind,
            server_id,
            database: None,
            schema,
            name,
            table,
            copy_name: None,
            copy_qualified: None,
            columns: Vec::new(),
            profile: None,
            position: dummy_position(),
        }
    }

    fn full_availability() -> MenuAvailability {
        MenuAvailability {
            is_connected: false,
            is_loading: false,
            can_mutate: true,
            has_connection: true,
            db: sqlly_lang::sqlmodel::DatabaseType::SqlServer,
            custom_actions: Vec::new(),
            connections_read_only: false,
            redis_scan_more: false,
            pin_groups: Vec::new(),
        }
    }

    /// A relay proxy's listing is read-only: the server menu keeps Edit (the
    /// editor opens read-only) but disables Duplicate, Delete, and Add Server.
    #[test]
    fn proxied_connections_disable_mutating_server_menu_items() {
        fn enabled_for(entries: &[MenuEntry], wanted: &str) -> bool {
            fn find(entries: &[MenuEntry], wanted: &str) -> Option<bool> {
                entries.iter().find_map(|entry| match entry {
                    MenuEntry::Action { label, enabled, .. } if *label == wanted => Some(*enabled),
                    MenuEntry::ActionOwned { label, enabled, .. } if label == wanted => {
                        Some(*enabled)
                    }
                    MenuEntry::SubmenuOwned { entries, .. } => find(entries, wanted),
                    _ => None,
                })
            }
            find(entries, wanted).unwrap_or_else(|| panic!("no menu item {wanted}"))
        }

        let ctx = ctx_for_kind(NodeKind::Server {
            id: uuid::Uuid::new_v4(),
        });
        let read_only = MenuAvailability {
            connections_read_only: true,
            ..full_availability()
        };
        let entries = menu_entries_for_context(&ctx, &read_only);
        assert!(enabled_for(&entries, "Edit Connection…"));
        assert!(!enabled_for(&entries, "Duplicate Connection…"));
        assert!(!enabled_for(&entries, "Delete Connection…"));
        assert!(!enabled_for(&entries, "Add Server…"));

        let entries = menu_entries_for_context(&ctx, &full_availability());
        assert!(enabled_for(&entries, "Duplicate Connection…"));
        assert!(enabled_for(&entries, "Delete Connection…"));
        assert!(enabled_for(&entries, "Add Server…"));
    }

    /// Every action label in the menu, through any depth of submenus (the
    /// large menus group their long tail into Script As / Export / Tools …).
    fn action_labels(entries: &[MenuEntry]) -> Vec<String> {
        let mut out = Vec::new();
        for e in entries {
            match e {
                MenuEntry::Action { label, .. } => out.push((*label).to_string()),
                MenuEntry::ActionOwned { label, .. } => out.push(label.clone()),
                MenuEntry::SubmenuOwned { entries, .. } => out.extend(action_labels(entries)),
                _ => {}
            }
        }
        out
    }

    /// Every submenu label, through any depth.
    fn submenu_labels(entries: &[MenuEntry]) -> Vec<String> {
        let mut out = Vec::new();
        for e in entries {
            if let MenuEntry::SubmenuOwned { label, entries, .. } = e {
                out.push(label.clone());
                out.extend(submenu_labels(entries));
            }
        }
        out
    }

    /// Whether the first submenu called `label` (searched depth-first) is
    /// enabled.
    fn submenu_enabled(entries: &[MenuEntry], label: &str) -> Option<bool> {
        entries.iter().find_map(|e| match e {
            MenuEntry::SubmenuOwned {
                label: l, enabled, ..
            } if l == label => Some(*enabled),
            MenuEntry::SubmenuOwned { entries, .. } => submenu_enabled(entries, label),
            _ => None,
        })
    }

    /// The first submenu called `label`, searched depth-first.
    fn find_submenu<'a>(entries: &'a [MenuEntry], label: &str) -> Option<&'a Vec<MenuEntry>> {
        entries.iter().find_map(|e| match e {
            MenuEntry::SubmenuOwned {
                label: l, entries, ..
            } if l == label => Some(entries),
            MenuEntry::SubmenuOwned { entries, .. } => find_submenu(entries, label),
            _ => None,
        })
    }

    #[test]
    fn menu_table_includes_all_expected_items() {
        let ctx = ctx_for_kind(NodeKind::Table {
            schema: "dbo".into(),
            name: "Orders".into(),
        });
        let items = menu_entries_for_context(&ctx, &full_availability());
        let actions = action_labels(&items);
        let submenus = submenu_labels(&items);

        assert!(
            actions.iter().any(|label| label == "Select Top 1000"),
            "table menu missing Select Top 1000"
        );
        assert!(
            actions.iter().any(|label| label == "Open in Query Builder"),
            "table menu missing Open in Query Builder"
        );
        assert!(
            actions.iter().any(|label| label == "Script as INSERT"),
            "table menu missing Script as INSERT"
        );
        assert!(
            actions.iter().any(|label| label == "Script as MERGE"),
            "table menu missing Script as MERGE"
        );
        assert!(
            actions.iter().any(|label| label == "DELETE all rows"),
            "table menu missing DELETE all rows"
        );
        assert!(
            actions.iter().any(|label| label == "TRUNCATE"),
            "table menu missing TRUNCATE"
        );
        assert!(
            actions.iter().any(|label| label == "DROP TABLE"),
            "table menu missing DROP TABLE"
        );
        assert!(
            actions.iter().any(|label| label == "Refresh"),
            "table menu missing Refresh"
        );
        assert!(
            actions
                .iter()
                .any(|label| label == "Focus in relationships diagram"),
            "table menu missing Focus in relationships diagram"
        );
        assert!(
            actions.iter().any(|label| label == "Properties…"),
            "table menu missing Properties"
        );
        // "Manage Connections..." lives on the Explorer overflow menu, not on
        // a single table, database, or view.
        assert!(
            !actions.iter().any(|label| label == "Manage Connections…"),
            "table menu should not carry the global Manage Connections command"
        );
        assert!(
            submenus.iter().any(|label| label == "CREATE To"),
            "table menu missing CREATE To submenu"
        );
        assert!(
            submenus.iter().any(|label| label == "Save Data As"),
            "table menu missing Save Data As submenu"
        );
    }

    #[test]
    fn menu_view_excludes_destructive_actions() {
        let ctx = ctx_for_kind(NodeKind::View {
            schema: "dbo".into(),
            name: "OrderView".into(),
        });
        let items = menu_entries_for_context(&ctx, &full_availability());
        let actions = action_labels(&items);

        assert!(
            actions.iter().any(|label| label == "Select Top 1000"),
            "view menu missing Select Top 1000"
        );
        assert!(
            actions.iter().any(|label| label == "Open in Query Builder"),
            "view menu missing Open in Query Builder"
        );
        assert!(
            actions.iter().any(|label| label == "Refresh"),
            "view menu missing Refresh"
        );
        assert!(
            actions.iter().any(|label| label == "Properties…"),
            "view menu missing Properties"
        );
        assert!(
            !actions.iter().any(|label| label == "DELETE all rows"),
            "view menu should NOT have DELETE all rows"
        );
        assert!(
            !actions.iter().any(|label| label == "TRUNCATE"),
            "view menu should NOT have TRUNCATE"
        );
        assert!(
            !actions.iter().any(|label| label == "DROP TABLE"),
            "view menu should NOT have DROP TABLE"
        );
        assert!(
            !actions.iter().any(|label| label == "Script as INSERT"),
            "view menu should NOT have Script as INSERT"
        );
        assert!(
            !actions.iter().any(|label| label == "Script as MERGE"),
            "view menu should NOT have Script as MERGE"
        );
    }

    #[test]
    fn menu_database_has_new_query_and_refresh() {
        let ctx = ctx_for_kind(NodeKind::Database("master".into()));
        let items = menu_entries_for_context(&ctx, &full_availability());
        let actions = action_labels(&items);

        assert!(
            actions.iter().any(|label| label == "New Query"),
            "database menu missing New Query"
        );
        assert_eq!(
            actions.first(),
            Some(&"New Query".to_string()),
            "a database's menu leads with New Query"
        );
        assert_eq!(
            actions.get(1),
            Some(&"Browse Objects…".to_string()),
            "Browse Objects… comes right after New Query"
        );
        assert!(
            actions.iter().any(|label| label == "Run SQL File…"),
            "database menu missing Run SQL File"
        );
        assert!(
            actions
                .iter()
                .any(|label| label == "Search Database for Value…"),
            "database menu missing Search Database for Value"
        );
        assert!(
            actions.iter().any(|label| label == "Refresh"),
            "database menu missing Refresh"
        );
        assert!(
            actions.iter().any(|label| label == "Properties…"),
            "database menu missing Properties"
        );
        assert!(
            !actions.iter().any(|label| label == "Manage Connections…"),
            "database menu should not carry the global Manage Connections command"
        );
        assert!(
            actions.iter().any(|label| label == "Drop Database…"),
            "database menu missing Drop Database"
        );
    }

    #[test]
    fn menu_database_disables_drop_when_cannot_mutate() {
        let ctx = ctx_for_kind(NodeKind::Database("master".into()));
        let mut availability = full_availability();
        availability.can_mutate = false;
        let items = menu_entries_for_context(&ctx, &availability);
        let drop = items.iter().find_map(|entry| match entry {
            MenuEntry::Action {
                label,
                action,
                enabled,
            } => {
                if *action == ExplorerMenuAction::DropDatabase {
                    Some((*label, *enabled))
                } else {
                    None
                }
            }
            MenuEntry::ActionOwned {
                label,
                action,
                enabled,
            } => {
                if *action == ExplorerMenuAction::DropDatabase {
                    Some((label.as_str(), *enabled))
                } else {
                    None
                }
            }
            _ => None,
        });
        assert_eq!(drop, Some(("Drop Database…", false)));
    }

    #[test]
    fn redis_bulk_writes_and_import_are_disabled_when_cannot_mutate() {
        let ctx = ctx_for_kind(NodeKind::Database("0".into()));
        let mut availability = full_availability();
        availability.db = sqlly_lang::sqlmodel::DatabaseType::Redis;
        availability.can_mutate = false;
        let items = menu_entries_for_context(&ctx, &availability);
        fn find(entries: &[MenuEntry], wanted: &ExplorerMenuAction) -> Option<bool> {
            entries.iter().find_map(|entry| match entry {
                MenuEntry::Action {
                    action, enabled, ..
                }
                | MenuEntry::ActionOwned {
                    action, enabled, ..
                } if action == wanted => Some(*enabled),
                MenuEntry::SubmenuOwned { entries, .. } => find(entries, wanted),
                _ => None,
            })
        }
        let enabled = |wanted: ExplorerMenuAction| find(&items, &wanted);
        assert_eq!(enabled(ExplorerMenuAction::RedisBulkDelete), Some(false));
        assert_eq!(enabled(ExplorerMenuAction::RedisBulkTtl), Some(false));
        assert_eq!(
            enabled(ExplorerMenuAction::RedisImportCommands),
            Some(false)
        );
        // Reading is not writing: the analyzer stays available.
        assert_eq!(enabled(ExplorerMenuAction::RedisMemoryAnalyzer), Some(true));
    }

    #[test]
    fn redis_index_menu_opens_queries_and_reviews_the_drop() {
        let ctx = ctx_for_kind(NodeKind::RedisIndex {
            name: "idx:users".into(),
        });
        let mut availability = full_availability();
        availability.db = sqlly_lang::sqlmodel::DatabaseType::Redis;
        let items = menu_entries_for_context(&ctx, &availability);
        let labels: Vec<&str> = items
            .iter()
            .filter_map(|entry| match entry {
                MenuEntry::Action { label, .. } => Some(*label),
                MenuEntry::ActionOwned { label, .. } => Some(label.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            labels,
            vec![
                "Copy Name",
                "Open (FT.INFO)",
                "Query…",
                "Explain…",
                "Profile…",
                "Drop Index…"
            ]
        );
        // The drop writes: greyed out wherever writes are.
        availability.can_mutate = false;
        let items = menu_entries_for_context(&ctx, &availability);
        let enabled = |wanted: ExplorerMenuAction| {
            items.iter().find_map(|entry| match entry {
                MenuEntry::Action {
                    action, enabled, ..
                }
                | MenuEntry::ActionOwned {
                    action, enabled, ..
                } if *action == wanted => Some(*enabled),
                _ => None,
            })
        };
        assert_eq!(enabled(ExplorerMenuAction::RedisIndexDrop), Some(false));
        assert_eq!(enabled(ExplorerMenuAction::RedisIndexQuery), Some(true));

        // Open runs at once; the rest open for editing and review.
        let db = sqlly_lang::sqlmodel::DatabaseType::Redis;
        match super::menus::action_to_event_for(db, &ExplorerMenuAction::RedisIndexInfo, &ctx) {
            SchemaTreeEvent::RunCommandInNewTab { sql, .. } => {
                assert_eq!(sql, "FT.INFO idx:users")
            }
            _ => panic!("Open should run FT.INFO in a new tab"),
        }
        for (action, needle) in [
            (
                ExplorerMenuAction::RedisIndexQuery,
                "FT.SEARCH idx:users \"*\" LIMIT 0 100",
            ),
            (
                ExplorerMenuAction::RedisIndexExplain,
                "FT.EXPLAIN idx:users",
            ),
            (
                ExplorerMenuAction::RedisIndexProfile,
                "FT.PROFILE idx:users SEARCH QUERY",
            ),
            (ExplorerMenuAction::RedisIndexDrop, "FT.DROPINDEX idx:users"),
        ] {
            match super::menus::action_to_event_for(db, &action, &ctx) {
                SchemaTreeEvent::OpenSqlInNewTab { sql, .. } => {
                    assert!(sql.contains(needle), "{action:?}: {sql}")
                }
                _ => panic!("{action:?} should open a script in a new tab"),
            }
        }

        // The folder only refreshes.
        let folder = ctx_for_kind(NodeKind::RedisIndexesFolder);
        let mut folder_availability = full_availability();
        folder_availability.db = db;
        let items = menu_entries_for_context(&folder, &folder_availability);
        let labels: Vec<&str> = items
            .iter()
            .filter_map(|entry| match entry {
                MenuEntry::Action { label, .. } => Some(*label),
                MenuEntry::ActionOwned { label, .. } => Some(label.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(labels, vec!["Copy Name", "Refresh"]);
    }

    #[test]
    fn menu_server_has_new_database() {
        let ctx = ctx_for_kind(NodeKind::Server {
            id: uuid::Uuid::new_v4(),
        });
        let items = menu_entries_for_context(&ctx, &full_availability());
        let actions = action_labels(&items);
        assert!(
            actions.iter().any(|label| label == "New Database…"),
            "server menu missing New Database"
        );
    }

    #[test]
    fn menu_object_custom_actions_list_matching_templates() {
        let mut availability = full_availability();
        availability.custom_actions = vec![
            menus::CustomActionEntry {
                name: "Row count".into(),
                mode: sqlly_services::ExplorerActionMode::Script,
            },
            menus::CustomActionEntry {
                name: "Rebuild".into(),
                mode: sqlly_services::ExplorerActionMode::Run,
            },
        ];
        let ctx = ctx_for_kind(NodeKind::Table {
            schema: "dbo".into(),
            name: "orders".into(),
        });
        let entries = menu_entries_for_context(&ctx, &availability);
        let custom = find_submenu(&entries, "Custom Actions").expect("custom actions submenu");
        let items: Vec<(String, ExplorerMenuAction)> = custom
            .iter()
            .filter_map(|e| match e {
                MenuEntry::ActionOwned { label, action, .. } => {
                    Some((label.clone(), action.clone()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].0, "Row count");
        assert!(
            matches!(&items[0].1, ExplorerMenuAction::RunTemplate { name } if name == "Row count")
        );
        assert_eq!(
            items[1].0, "Rebuild (run)",
            "run-mode actions say they execute"
        );
    }

    #[test]
    fn menu_object_without_templates_has_no_custom_actions_submenu() {
        let ctx = ctx_for_kind(NodeKind::Procedure {
            schema: "dbo".into(),
            name: "p".into(),
        });
        let entries = menu_entries_for_context(&ctx, &full_availability());
        assert!(find_submenu(&entries, "Custom Actions").is_none());
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn menu_deploy_cart_submenu_offers_data_for_tables_only() {
        let data_enabled = |kind: NodeKind| -> Option<bool> {
            let entries = menu_entries_for_context(&ctx_for_kind(kind), &full_availability());
            find_submenu(&entries, "Deploy Cart")?
                .iter()
                .find_map(|e| match e {
                    MenuEntry::Action {
                        action: ExplorerMenuAction::AddToDeployCart { include_data: true },
                        enabled,
                        ..
                    }
                    | MenuEntry::ActionOwned {
                        action: ExplorerMenuAction::AddToDeployCart { include_data: true },
                        enabled,
                        ..
                    } => Some(*enabled),
                    _ => None,
                })
        };
        assert_eq!(
            data_enabled(NodeKind::Table {
                schema: "dbo".into(),
                name: "t".into()
            }),
            Some(true)
        );
        assert_eq!(
            data_enabled(NodeKind::View {
                schema: "dbo".into(),
                name: "v".into()
            }),
            Some(false)
        );
        assert_eq!(
            data_enabled(NodeKind::Function {
                schema: "dbo".into(),
                name: "f".into()
            }),
            Some(false)
        );
        // Non-object nodes never offer the cart.
        assert_eq!(data_enabled(NodeKind::Column), None);
    }

    /// Two servers, each with an expanded Tables folder holding one table,
    /// ids in the real `srv:<uuid>:…` shape so server ownership resolves.
    fn multi_select_fixture(a: Uuid, b: Uuid) -> Vec<TreeNode> {
        [(a, "orders"), (b, "invoices")]
            .into_iter()
            .map(|(id, table)| {
                let mut server =
                    TreeNode::new(format!("srv:{id}"), "srv", NodeKind::Server { id }, 0);
                server.expanded = true;
                let mut folder = TreeNode::new(
                    format!("srv:{id}:db:shop:tables"),
                    "Tables",
                    NodeKind::TablesFolder,
                    1,
                );
                folder.expanded = true;
                folder.children = vec![TreeNode::new(
                    format!("srv:{id}:db:shop:tables:t:dbo:{table}"),
                    format!("dbo.{table}"),
                    NodeKind::Table {
                        schema: "dbo".to_string(),
                        name: table.to_string(),
                    },
                    2,
                )];
                server.children = vec![folder];
                server
            })
            .collect()
    }

    #[gpui::test]
    fn multi_selection_is_pruned_on_refresh_disconnect_and_stale_actions(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            let services = Services::new();
            let log =
                crate::log_console::DiagnosticLog::new(crate::log_console::DiagnosticLogLevel::Off);
            let tree = cx.new(|cx| SchemaTree::new(services, log, cx));
            let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
            let orders = format!("srv:{a}:db:shop:tables:t:dbo:orders");
            let invoices = format!("srv:{b}:db:shop:tables:t:dbo:invoices");
            tree.update(cx, |tree, cx| {
                tree.roots = multi_select_fixture(a, b);
                tree.multi.toggle(&orders);
                tree.multi.toggle(&invoices);

                // Collapsing a parent keeps the member (its node still exists).
                if let Some(folder) =
                    find_node_any_mut(&mut tree.roots, &format!("srv:{a}:db:shop:tables"))
                {
                    folder.expanded = false;
                }
                tree.bump_tree_revision();
                assert_eq!(tree.multi.len(), 2);

                // A refresh that clears the folder drops its member.
                if let Some(folder) =
                    find_node_any_mut(&mut tree.roots, &format!("srv:{a}:db:shop:tables"))
                {
                    folder.children.clear();
                }
                tree.bump_tree_revision();
                assert_eq!(tree.multi.members(), std::slice::from_ref(&invoices));

                // Disconnecting a server drops its members.
                tree.disconnect_server(b, cx);
                assert!(tree.multi.is_empty());

                // A set whose every member went stale behind the tree's back
                // is still cleared by the action, so the banner never sticks.
                tree.multi.toggle("srv:gone:db:x:tables:t:dbo:t");
                tree.emit_bulk_action(BulkTreeAction::ScriptCreate, cx);
                assert!(tree.multi.is_empty());
            });
        });
    }

    /// The keyboard drives the bulk multi-selection end to end: Shift+Down
    /// ranges over tables (skipping folders and servers), Esc clears, Space
    /// and Cmd/Ctrl+Space toggle only tables/views, and plain arrows walk
    /// without dropping the set.
    #[gpui::test]
    fn keyboard_builds_and_clears_the_bulk_multi_selection(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            if cx.try_global::<gpui_component::Theme>().is_none() {
                gpui_component::init(cx);
            }
        });
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let folder_a = format!("srv:{a}:db:shop:tables");
        let orders = format!("srv:{a}:db:shop:tables:t:dbo:orders");
        let folder_b = format!("srv:{b}:db:shop:tables");
        let invoices = format!("srv:{b}:db:shop:tables:t:dbo:invoices");
        let (tree, cx) = cx.add_window_view(|_window, cx| {
            let services = Services::new();
            let log =
                crate::log_console::DiagnosticLog::new(crate::log_console::DiagnosticLogLevel::Off);
            let mut tree = SchemaTree::new(services, log, cx);
            tree.roots = multi_select_fixture(a, b);
            tree.bump_tree_revision();
            tree
        });
        let press = |tree: &Entity<SchemaTree>, cx: &mut gpui::VisualTestContext, keys: &str| {
            let event = gpui::KeyDownEvent {
                keystroke: crate::keybindings::parse_keystroke(keys).expect("keystroke"),
                is_held: false,
                prefer_character_input: false,
            };
            tree.update_in(cx, |tree, window, cx| tree.on_key_down(&event, window, cx));
        };

        tree.update(cx, |tree, _| tree.selected = Some(folder_a.clone()));
        // Folder → orders: the range starts on the folder, holds only tables.
        press(&tree, cx, "shift-down");
        tree.update(cx, |tree, _| {
            assert_eq!(tree.selected.as_deref(), Some(orders.as_str()));
            assert_eq!(tree.multi.members(), std::slice::from_ref(&orders));
        });
        // On through server b and its folder to invoices.
        for _ in 0..3 {
            press(&tree, cx, "shift-down");
        }
        tree.update(cx, |tree, _| {
            assert_eq!(tree.selected.as_deref(), Some(invoices.as_str()));
            assert_eq!(tree.multi.members(), &[orders.clone(), invoices.clone()]);
        });
        // Esc clears the set (the bulk banner renders only while non-empty).
        press(&tree, cx, "escape");
        tree.update(cx, |tree, _| assert!(tree.multi.is_empty()));

        // Space toggles the cursor row in; plain arrows keep the set.
        press(&tree, cx, "space");
        press(&tree, cx, "up");
        tree.update(cx, |tree, _| {
            assert_eq!(tree.selected.as_deref(), Some(folder_b.as_str()));
            assert_eq!(tree.multi.members(), std::slice::from_ref(&invoices));
        });
        // A folder is never a member.
        press(&tree, cx, "space");
        tree.update(cx, |tree, _| assert_eq!(tree.multi.len(), 1));
        press(&tree, cx, "up");
        press(&tree, cx, "up");
        press(&tree, cx, "secondary-space");
        tree.update(cx, |tree, _| {
            assert_eq!(tree.multi.members(), &[invoices.clone(), orders.clone()]);
        });
        // Cmd/Ctrl+Space toggles it back out.
        press(&tree, cx, "secondary-space");
        tree.update(cx, |tree, _| {
            assert_eq!(tree.multi.members(), std::slice::from_ref(&invoices));
        });
    }

    /// The server's Tools submenu carries the instance-wide tools, and each
    /// entry names the tool it opens — the app reads the clicked node's
    /// connection from the same context the menu was built for.
    #[test]
    fn menu_server_tools_submenu_carries_the_server_tools() {
        let ctx = ctx_for_kind(NodeKind::Server {
            id: uuid::Uuid::new_v4(),
        });
        let items = menu_entries_for_context(&ctx, &full_availability());
        assert!(submenu_labels(&items).iter().any(|label| label == "Tools"));
        let tools = find_submenu(&items, "Tools").expect("server Tools submenu");
        assert_eq!(
            action_labels(tools),
            vec![
                "Server Dashboard…",
                "SQL Agent…",
                "Security…",
                "Connection Health…",
                "Disk Usage…",
            ]
        );
    }

    #[test]
    fn menu_database_tools_submenu_carries_the_database_tools() {
        let ctx = ctx_for_kind(NodeKind::Database("Sales".into()));
        let items = menu_entries_for_context(&ctx, &full_availability());
        // The tool windows are split across the Data, Schema and Tools
        // groups; all of them are still on the database's menu.
        for group in ["Data", "Schema", "Tools"] {
            assert!(find_submenu(&items, group).is_some(), "missing {group}");
        }
        let labels = action_labels(&items);
        for expected in [
            "Security…",
            "Backup / Restore…",
            "Activity Monitor…",
            "Designers…",
            "Import / Export…",
            "Schema Compare…",
            "Data Compare…",
            "Query Insights…",
            "Disk Usage…",
            "Index Analyzer…",
            "Test Data Generator…",
            "Relationship Graph…",
            "Query Builder…",
            "Document Schema (Markdown)…",
            "Tenant Scoping…",
            "Query History…",
            "Extended Events…",
        ] {
            assert!(
                labels.iter().any(|label| label == expected),
                "missing {expected}: {labels:?}"
            );
        }
    }

    /// Every tool runs SQL, so without a connection the whole submenu is
    /// disabled rather than opening a dialog with nothing to talk to.
    #[test]
    fn menu_tools_submenu_is_disabled_without_a_connection() {
        let ctx = ctx_for_kind(NodeKind::Database("Sales".into()));
        let mut availability = full_availability();
        availability.has_connection = false;
        let items = menu_entries_for_context(&ctx, &availability);
        // The database's Tools group still opens (it also holds the schema
        // snapshot items), but nothing in it can be picked.
        let tools = find_submenu(&items, "Tools").expect("Tools group");
        fn all_disabled(entries: &[MenuEntry]) -> bool {
            entries.iter().all(|entry| match entry {
                MenuEntry::Action { enabled, .. } | MenuEntry::ActionOwned { enabled, .. } => {
                    !*enabled
                }
                MenuEntry::SubmenuOwned {
                    enabled, entries, ..
                } => !*enabled || all_disabled(entries),
                MenuEntry::Separator => true,
            })
        }
        assert!(all_disabled(tools), "{tools:?}");

        // The server's Tools submenu is tools only, and collapses to one
        // disabled item.
        let server = ctx_for_kind(NodeKind::Server { id: Uuid::nil() });
        let items = menu_entries_for_context(&server, &availability);
        let enabled = items.iter().find_map(|entry| match entry {
            MenuEntry::SubmenuOwned { label, enabled, .. } if label == "Tools" => Some(*enabled),
            _ => None,
        });
        assert_eq!(enabled, Some(false));
    }

    #[test]
    fn menu_schema_folder_has_browse_objects() {
        let ctx = ctx_for_kind(NodeKind::SchemaFolder {
            schema: "dbo".into(),
        });
        let items = menu_entries_for_context(&ctx, &full_availability());
        let actions = action_labels(&items);
        assert_eq!(actions.first(), Some(&"Copy Name".to_string()));
        assert_eq!(actions.get(1), Some(&"Browse Objects…".to_string()));
        assert!(actions.iter().any(|label| label == "Refresh"));
    }

    // -----------------------------------------------------------------
    // Copy-identity (Copy Name / Copy Qualified Name payload) tests
    // -----------------------------------------------------------------

    #[test]
    fn menu_table_and_view_offer_generate_model_submenu() {
        use crate::model_codegen::ModelLanguage;
        use menus::GENERATE_MODEL_LABEL;
        for kind in [
            NodeKind::Table {
                schema: "dbo".into(),
                name: "Orders".into(),
            },
            NodeKind::View {
                schema: "dbo".into(),
                name: "vOrders".into(),
            },
        ] {
            let ctx = ctx_for_kind(kind);
            let items = menu_entries_for_context(&ctx, &full_availability());
            let submenu = find_submenu(&items, GENERATE_MODEL_LABEL)
                .unwrap_or_else(|| panic!("{:?} menu missing {GENERATE_MODEL_LABEL}", ctx.kind));
            let labels: Vec<&str> = submenu
                .iter()
                .filter_map(|e| match e {
                    MenuEntry::Action { label, .. } => Some(*label),
                    MenuEntry::ActionOwned { label, .. } => Some(label.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(labels.len(), ModelLanguage::all().len(), "{labels:?}");
            for lang in ModelLanguage::all() {
                let label = lang.label();
                assert!(labels.contains(&label.as_str()), "missing {label}");
            }
            // Every language opens a new tab…
            assert!(submenu.iter().all(|e| match e {
                MenuEntry::Action { action, .. } | MenuEntry::ActionOwned { action, .. } =>
                    matches!(
                        action,
                        ExplorerMenuAction::GenerateModel {
                            destination: ScriptDestination::NewQuery,
                            ..
                        }
                    ),
                _ => true,
            }));
            // …and the nested Copy to Clipboard submenu repeats them for the
            // clipboard.
            let copy = find_submenu(submenu, "Copy to Clipboard")
                .expect("Generate Model has a Copy to Clipboard submenu");
            assert_eq!(action_labels(copy), labels);
            assert!(copy.iter().all(|e| matches!(
                e,
                MenuEntry::Action {
                    action: ExplorerMenuAction::GenerateModel {
                        destination: ScriptDestination::Clipboard,
                        ..
                    },
                    ..
                } | MenuEntry::ActionOwned {
                    action: ExplorerMenuAction::GenerateModel {
                        destination: ScriptDestination::Clipboard,
                        ..
                    },
                    ..
                }
            )));
        }
    }

    #[test]
    fn menu_generate_model_needs_a_connection() {
        use menus::{GENERATE_ALL_MODELS_LABEL, GENERATE_MODEL_LABEL};
        let offline = MenuAvailability {
            has_connection: false,
            ..full_availability()
        };
        let table = ctx_for_kind(NodeKind::Table {
            schema: "dbo".into(),
            name: "Orders".into(),
        });
        let items = menu_entries_for_context(&table, &offline);
        let disabled = submenu_enabled(&items, GENERATE_MODEL_LABEL) == Some(false);
        assert!(
            disabled,
            "Generate Model must be disabled without a connection"
        );

        let db = ctx_for_kind(NodeKind::Database("Shop".into()));
        let items = menu_entries_for_context(&db, &offline);
        let disabled = submenu_enabled(&items, GENERATE_ALL_MODELS_LABEL) == Some(false);
        assert!(
            disabled,
            "Generate Models for All Tables must be disabled without a connection"
        );
    }

    #[test]
    fn menu_schema_and_database_offer_all_tables_models() {
        use crate::model_codegen::ModelLanguage;
        use menus::GENERATE_ALL_MODELS_LABEL;
        for kind in [
            NodeKind::SchemaFolder {
                schema: "sales".into(),
            },
            NodeKind::Database("Shop".into()),
        ] {
            let ctx = ctx_for_kind(kind);
            let items = menu_entries_for_context(&ctx, &full_availability());
            let submenu = find_submenu(&items, GENERATE_ALL_MODELS_LABEL).unwrap_or_else(|| {
                panic!("{:?} menu missing {GENERATE_ALL_MODELS_LABEL}", ctx.kind)
            });
            assert_eq!(action_labels(submenu).len(), ModelLanguage::all().len());
            assert!(submenu.iter().all(|e| matches!(
                e,
                MenuEntry::Action {
                    action: ExplorerMenuAction::GenerateModelsForAll { .. },
                    enabled: true,
                    ..
                } | MenuEntry::ActionOwned {
                    action: ExplorerMenuAction::GenerateModelsForAll { .. },
                    enabled: true,
                    ..
                }
            )));
        }
    }

    #[test]
    fn generate_model_actions_route_to_the_app_handler() {
        use crate::model_codegen::ModelLanguage;
        let ctx = ctx_for_kind(NodeKind::Table {
            schema: "dbo".into(),
            name: "Orders".into(),
        });
        let action = ExplorerMenuAction::GenerateModel {
            language: ModelLanguage::Prisma,
            destination: ScriptDestination::Clipboard,
        };
        match action_to_event(&action, &ctx) {
            SchemaTreeEvent::ExplorerActionRequested {
                action: routed,
                context,
            } => {
                assert_eq!(routed, action);
                assert_eq!(context.name.as_deref(), Some("Orders"));
            }
            _ => panic!("GenerateModel must route through ExplorerActionRequested"),
        }
    }

    #[test]
    fn copy_names_quotes_and_chains_present_segments() {
        // Schema + name (table-like).
        assert_eq!(
            copy_names(Some("dbo"), None, "Users"),
            ("Users".into(), Some("[dbo].[Users]".into()))
        );
        // Schema + parent + leaf (column-like).
        assert_eq!(
            copy_names(Some("dbo"), Some("Users"), "Id"),
            ("Id".into(), Some("[dbo].[Users].[Id]".into()))
        );
        // Names with spaces, dots, and reserved words each stay one bracketed
        // segment; a literal `]` is doubled so the identifier can't break out.
        assert_eq!(
            copy_names(Some("my schema"), Some("Order.Lines"), "we]rd"),
            (
                "we]rd".into(),
                Some("[my schema].[Order.Lines].[we]]rd]".into())
            )
        );
        assert_eq!(
            copy_names(Some("dbo"), None, "Order"),
            ("Order".into(), Some("[dbo].[Order]".into()))
        );
        // Parent but no schema still qualifies with what's present.
        assert_eq!(
            copy_names(None, Some("Users"), "Id"),
            ("Id".into(), Some("[Users].[Id]".into()))
        );
        // Nothing to qualify with -> name only.
        assert_eq!(copy_names(None, None, "Id"), ("Id".into(), None));
    }

    /// Build `dbo.Orders` with a column and a foreign key, and `dbo.GetOrders`
    /// with a parameter, so the leaf kinds have real parents to qualify against.
    fn copy_identity_fixture() -> Vec<TreeNode> {
        let mut orders = TreeNode::new(
            "srv:x:db:Sales:t:dbo:Orders",
            "Orders",
            NodeKind::Table {
                schema: "dbo".into(),
                name: "Orders".into(),
            },
            0,
        );
        let col = TreeNode::new(
            "srv:x:db:Sales:t:dbo:Orders:c:Order Id",
            "Order Id",
            NodeKind::Column,
            1,
        );
        let mut fk_folder = TreeNode::new(
            "srv:x:db:Sales:t:dbo:Orders:foreignkeys",
            "Foreign Keys",
            NodeKind::ForeignKeysFolder {
                schema: "dbo".into(),
                table: "Orders".into(),
            },
            1,
        );
        let fk = TreeNode::new(
            "srv:x:db:Sales:t:dbo:Orders:foreignkeys:out:FK_Orders_Customers",
            "FK_Orders_Customers",
            NodeKind::ForeignKey { outgoing: true },
            2,
        );
        fk_folder.children = vec![fk];
        orders.children = vec![col, fk_folder];

        let mut proc = TreeNode::new(
            "srv:x:db:Sales:p:dbo:GetOrders",
            "GetOrders",
            NodeKind::Procedure {
                schema: "dbo".into(),
                name: "GetOrders".into(),
            },
            0,
        );
        let param = TreeNode::new(
            "srv:x:db:Sales:p:dbo:GetOrders:param:@since",
            "@since",
            NodeKind::Parameter {
                data_type: "datetime".into(),
                is_output: false,
            },
            1,
        );
        proc.children = vec![param];
        vec![orders, proc]
    }

    #[test]
    fn copy_identity_qualifies_each_kind() {
        let roots = copy_identity_fixture();

        // Table: [schema].[name].
        assert_eq!(
            copy_identity_for(
                &roots,
                "srv:x:db:Sales:t:dbo:Orders",
                &NodeKind::Table {
                    schema: "dbo".into(),
                    name: "Orders".into(),
                },
            ),
            (Some("Orders".into()), Some("[dbo].[Orders]".into()))
        );

        // Column: [schema].[table].[column], via its parent table.
        assert_eq!(
            copy_identity_for(
                &roots,
                "srv:x:db:Sales:t:dbo:Orders:c:Order Id",
                &NodeKind::Column,
            ),
            (
                Some("Order Id".into()),
                Some("[dbo].[Orders].[Order Id]".into())
            )
        );

        // Parameter: [schema].[routine].[parameter], via its parent procedure.
        assert_eq!(
            copy_identity_for(
                &roots,
                "srv:x:db:Sales:p:dbo:GetOrders:param:@since",
                &NodeKind::Parameter {
                    data_type: "datetime".into(),
                    is_output: false,
                },
            ),
            (
                Some("@since".into()),
                Some("[dbo].[GetOrders].[@since]".into())
            )
        );

        // Foreign key: [schema].[table].[constraint], via its Foreign Keys folder.
        assert_eq!(
            copy_identity_for(
                &roots,
                "srv:x:db:Sales:t:dbo:Orders:foreignkeys:out:FK_Orders_Customers",
                &NodeKind::ForeignKey { outgoing: true },
            ),
            (
                Some("FK_Orders_Customers".into()),
                Some("[dbo].[Orders].[FK_Orders_Customers]".into())
            )
        );
    }

    #[test]
    fn copy_identity_handles_index_and_folders_and_omits_servers() {
        let roots = copy_identity_fixture();
        // Index carries schema+table+name in its own kind (no tree walk needed).
        assert_eq!(
            copy_identity_for(
                &roots,
                "unrelated",
                &NodeKind::Index {
                    schema: "dbo".into(),
                    table: "Orders".into(),
                    name: "IX_Orders_Customer".into(),
                },
            ),
            (
                Some("IX_Orders_Customer".into()),
                Some("[dbo].[Orders].[IX_Orders_Customer]".into())
            )
        );
        // A schema folder copies its canonical name but has nothing to qualify.
        assert_eq!(
            copy_identity_for(
                &roots,
                "unrelated",
                &NodeKind::SchemaFolder {
                    schema: "Sales".into(),
                },
            ),
            (Some("Sales".into()), None)
        );
        // A server root has no qualified form, but its name still copies —
        // taken from the node's own title.
        let servers = vec![TreeNode::new(
            "srv:x",
            "prod-sql",
            NodeKind::Server { id: Uuid::nil() },
            0,
        )];
        assert_eq!(
            copy_identity_for(&servers, "srv:x", &NodeKind::Server { id: Uuid::nil() }),
            (Some("prod-sql".into()), None)
        );
        // Only a node that isn't in the tree at all has nothing to copy.
        assert_eq!(
            copy_identity_for(&roots, "unrelated", &NodeKind::Server { id: Uuid::nil() }),
            (None, None)
        );
    }

    #[test]
    fn copy_identity_falls_back_to_the_title_for_grouping_folders() {
        // Grouping folders carry no name in their kind; the row's title is the
        // name the user sees, so it is the name Copy Name hands back.
        let roots = vec![TreeNode::new(
            "srv:x:t",
            "Tables",
            NodeKind::TablesFolder,
            0,
        )];
        assert_eq!(
            copy_identity_for(&roots, "srv:x:t", &NodeKind::TablesFolder),
            (Some("Tables".into()), None)
        );
    }

    #[test]
    fn copy_identity_column_without_loaded_parent_degrades_to_name_only() {
        // The column exists but its parent is not in the tree: qualified is None,
        // the raw name still copies.
        let orphan = vec![TreeNode::new("lonely:c:Id", "Id", NodeKind::Column, 0)];
        assert_eq!(
            copy_identity_for(&orphan, "lonely:c:Id", &NodeKind::Column),
            (Some("Id".into()), None)
        );
    }

    /// Every node kind the tree can right-click offers Copy Name — including
    /// the servers, folders and Azure rows that used to have no way to hand
    /// back the name they display.
    #[test]
    fn every_node_kind_offers_copy_name() {
        let kinds = vec![
            NodeKind::Server { id: Uuid::nil() },
            NodeKind::Database("master".into()),
            NodeKind::SchemaFolder {
                schema: "dbo".into(),
            },
            NodeKind::TablesFolder,
            NodeKind::ViewsFolder,
            NodeKind::ProgrammabilityFolder,
            NodeKind::ExtensionsFolder,
            NodeKind::Table {
                schema: "dbo".into(),
                name: "Orders".into(),
            },
            NodeKind::Column,
            NodeKind::Error,
            NodeKind::AzureAccount,
            NodeKind::AzureSubscription,
            NodeKind::AzureSqlServer,
            NodeKind::PinnedFolder {
                server_id: Uuid::nil(),
                group: Uuid::nil(),
            },
            NodeKind::Pinned { index: 0 },
            test_group_kind(),
        ];
        for kind in kinds {
            let ctx = ctx_for_kind(kind.clone());
            let actions = action_labels(&menu_entries_for_context(&ctx, &full_availability()));
            // The large menus keep Copy Name in their footer (or Copy ▸);
            // the small ones still lead with it.
            assert!(
                actions.iter().any(|label| label == "Copy Name"),
                "{kind:?} should offer Copy Name, got {actions:?}"
            );
        }
    }

    #[test]
    fn menu_server_has_connect_and_manage() {
        let id = Uuid::new_v4();
        let ctx = ctx_for_kind(NodeKind::Server { id });
        let items = menu_entries_for_context(&ctx, &full_availability());
        let actions = action_labels(&items);

        assert!(
            actions.iter().any(|label| label == "Connect"),
            "server menu missing Connect"
        );
        assert!(
            actions.iter().any(|label| label == "Edit Connection…"),
            "server menu missing Edit Connection"
        );
        assert!(
            actions.iter().any(|label| label == "Duplicate Connection…"),
            "server menu missing Duplicate Connection"
        );
        let edit = actions
            .iter()
            .position(|a| *a == "Edit Connection…")
            .expect("edit present");
        let duplicate = actions
            .iter()
            .position(|a| *a == "Duplicate Connection…")
            .expect("duplicate present");
        let delete = actions
            .iter()
            .position(|a| *a == "Delete Connection…")
            .expect("delete present");
        assert!(
            edit < duplicate && duplicate < delete,
            "Duplicate sits between Edit and Delete, got {actions:?}"
        );
        assert!(
            actions.iter().any(|label| label == "Delete Connection…"),
            "server menu missing Delete Connection"
        );
        assert!(
            actions.iter().any(|label| label == "Properties…"),
            "server menu missing Properties"
        );
        assert!(
            actions.iter().any(|label| label == "Add Server…"),
            "server menu missing Add Server"
        );
        assert!(
            !actions.iter().any(|label| label == "Manage Connections…"),
            "Manage Connections belongs on the Explorer overflow menu, not a server row"
        );
        assert!(
            actions.iter().any(|label| label == "Refresh"),
            "server menu missing Refresh"
        );
    }

    #[test]
    fn menu_folder_has_refresh_and_properties() {
        let ctx = ctx_for_kind(NodeKind::TablesFolder);
        let items = menu_entries_for_context(&ctx, &full_availability());
        let actions = action_labels(&items);

        assert!(
            actions.iter().any(|label| label == "Refresh"),
            "folder menu missing Refresh"
        );
        assert!(
            actions.iter().any(|label| label == "Properties…"),
            "folder menu missing Properties"
        );
    }

    #[test]
    fn menu_column_has_copy_actions() {
        let ctx = ctx_for_kind(NodeKind::Column);
        let items = menu_entries_for_context(&ctx, &full_availability());
        let actions = action_labels(&items);

        assert!(
            actions.iter().any(|label| label == "Copy Name"),
            "column menu missing Copy Name"
        );
        assert!(
            actions.iter().any(|label| label == "Copy Qualified Name"),
            "column menu missing Copy Qualified Name"
        );
        assert!(
            actions.iter().any(|label| label == "Properties…"),
            "column menu missing Properties"
        );
    }

    #[test]
    fn menu_parameter_has_copy_name() {
        let ctx = ctx_for_kind(NodeKind::Parameter {
            data_type: "int".into(),
            is_output: false,
        });
        let items = menu_entries_for_context(&ctx, &full_availability());
        let actions = action_labels(&items);

        assert!(
            actions.iter().any(|label| label == "Copy Name"),
            "parameter menu missing Copy Name"
        );
        assert!(
            actions.iter().any(|label| label == "Properties…"),
            "parameter menu missing Properties"
        );
    }

    #[test]
    fn menu_error_has_refresh() {
        let ctx = ctx_for_kind(NodeKind::Error);
        let items = menu_entries_for_context(&ctx, &full_availability());
        let actions = action_labels(&items);

        assert_eq!(
            actions,
            vec!["Copy Name", "Refresh"],
            "error menu should be Copy Name + Refresh"
        );
    }

    #[test]
    fn menu_group_offers_copy_name_only() {
        let ctx = ctx_for_kind(test_group_kind());
        let items = menu_entries_for_context(&ctx, &full_availability());
        assert_eq!(
            action_labels(&items),
            vec!["Copy Name"],
            "a group header has nothing to act on but its name"
        );
    }

    #[test]
    fn menu_table_destructive_gated_by_can_mutate() {
        let ctx = ctx_for_kind(NodeKind::Table {
            schema: "dbo".into(),
            name: "Orders".into(),
        });
        let avail = MenuAvailability {
            can_mutate: false,
            ..full_availability()
        };
        let items = menu_entries_for_context(&ctx, &avail);

        for entry in &items {
            if let MenuEntry::Action {
                label,
                enabled,
                action,
            } = entry
                && matches!(
                    action,
                    ExplorerMenuAction::DeleteAllRows
                        | ExplorerMenuAction::TruncateTable
                        | ExplorerMenuAction::DropTable
                )
            {
                assert!(
                    !*enabled,
                    "{label} should be disabled when can_mutate is false"
                );
            }
        }
    }

    #[test]
    fn menu_server_connect_gated_by_is_connected() {
        let id = Uuid::new_v4();
        let ctx = ctx_for_kind(NodeKind::Server { id });
        let avail = MenuAvailability {
            is_connected: true,
            ..full_availability()
        };
        let items = menu_entries_for_context(&ctx, &avail);

        for entry in &items {
            if let MenuEntry::Action {
                label,
                enabled,
                action,
            } = entry
                && matches!(action, ExplorerMenuAction::Connect)
            {
                assert!(
                    !*enabled,
                    "{label} should be disabled when is_connected is true"
                );
            }
        }
    }

    #[test]
    fn menu_save_data_as_has_six_formats() {
        let ctx = ctx_for_kind(NodeKind::Table {
            schema: "dbo".into(),
            name: "Orders".into(),
        });
        let items = menu_entries_for_context(&ctx, &full_availability());
        let save_submenu =
            find_submenu(&items, "Save Data As").expect("Save Data As submenu should exist");
        let formats = action_labels(save_submenu);
        assert_eq!(formats.len(), 6, "Save Data As should have 6 formats");
        assert!(formats.iter().any(|label| label == "CSV"));
        assert!(formats.iter().any(|label| label == "Excel"));
        assert!(formats.iter().any(|label| label == "SQLLY Export"));
        assert!(formats.iter().any(|label| label == "New Temp Table Import"));
        assert!(formats.iter().any(|label| label == "Insert Into"));
        assert!(formats.iter().any(|label| label == "Update or Insert"));
    }

    #[test]
    fn menu_create_to_has_two_destinations() {
        let ctx = ctx_for_kind(NodeKind::Table {
            schema: "dbo".into(),
            name: "Orders".into(),
        });
        let items = menu_entries_for_context(&ctx, &full_availability());
        let create_submenu =
            find_submenu(&items, "CREATE To").expect("CREATE To submenu should exist");
        let destinations = action_labels(create_submenu);
        assert_eq!(
            destinations.len(),
            2,
            "CREATE To should have 2 destinations"
        );
        assert!(destinations.iter().any(|label| label == "New Query Window"));
        assert!(destinations.iter().any(|label| label == "Clipboard"));
    }

    #[test]
    fn menu_procedure_has_run_and_copy() {
        let ctx = ctx_for_kind(NodeKind::Procedure {
            schema: "dbo".into(),
            name: "MyProc".into(),
        });
        let items = menu_entries_for_context(&ctx, &full_availability());
        let actions = action_labels(&items);

        assert!(
            actions.iter().any(|label| label == "Run…"),
            "procedure menu missing Run"
        );
        assert!(
            actions.iter().any(|label| label == "Copy EXEC Script"),
            "procedure menu missing Copy EXEC Script"
        );
        assert!(
            actions.iter().any(|label| label == "Properties…"),
            "procedure menu missing Properties"
        );
    }

    #[test]
    fn menu_function_has_execute_and_copy() {
        let ctx = ctx_for_kind(NodeKind::Function {
            schema: "dbo".into(),
            name: "MyFunc".into(),
        });
        let items = menu_entries_for_context(&ctx, &full_availability());
        let actions = action_labels(&items);

        assert!(
            actions.iter().any(|label| label == "Execute…"),
            "function menu missing Execute"
        );
        assert!(
            actions.iter().any(|label| label == "Copy Invocation"),
            "function menu missing Copy Invocation"
        );
        assert!(
            actions.iter().any(|label| label == "Properties…"),
            "function menu missing Properties"
        );
    }

    // -----------------------------------------------------------------
    // action_to_event mapping tests
    // -----------------------------------------------------------------

    #[test]
    fn action_to_event_select_top_rows_emits_insert_sql() {
        let ctx = ctx_for_kind(NodeKind::Table {
            schema: "dbo".into(),
            name: "Orders".into(),
        });
        let event = action_to_event(&ExplorerMenuAction::SelectTopRows { limit: 1000 }, &ctx);
        assert!(matches!(event, SchemaTreeEvent::OpenSqlInNewTab { .. }));
    }

    #[test]
    fn action_to_event_script_insert_emits_script_table_as() {
        let ctx = ctx_for_kind(NodeKind::Table {
            schema: "dbo".into(),
            name: "Orders".into(),
        });
        let event = action_to_event(&ExplorerMenuAction::ScriptInsert, &ctx);
        match event {
            SchemaTreeEvent::ScriptTableAs {
                schema, name, kind, ..
            } => {
                assert_eq!(schema, "dbo");
                assert_eq!(name, "Orders");
                assert_eq!(kind, TableScriptKind::Insert);
            }
            _ => panic!("expected ScriptTableAs, got something else"),
        }
    }

    #[test]
    fn action_to_event_script_merge_emits_script_table_as() {
        let ctx = ctx_for_kind(NodeKind::Table {
            schema: "dbo".into(),
            name: "Orders".into(),
        });
        let event = action_to_event(&ExplorerMenuAction::ScriptMerge, &ctx);
        match event {
            SchemaTreeEvent::ScriptTableAs { kind, .. } => {
                assert_eq!(kind, TableScriptKind::Merge);
            }
            _ => panic!("expected ScriptTableAs, got something else"),
        }
    }

    #[test]
    fn action_to_event_delete_all_rows_emits_insert_sql() {
        let ctx = ctx_for_kind(NodeKind::Table {
            schema: "dbo".into(),
            name: "Orders".into(),
        });
        let event = action_to_event(&ExplorerMenuAction::DeleteAllRows, &ctx);
        assert!(matches!(event, SchemaTreeEvent::OpenSqlInNewTab { .. }));
    }

    #[test]
    fn action_to_event_new_query_emits_explorer_action_requested() {
        let ctx = ctx_for_kind(NodeKind::Database("master".into()));
        let event = action_to_event(&ExplorerMenuAction::NewQuery, &ctx);
        match event {
            SchemaTreeEvent::ExplorerActionRequested { action, .. } => {
                assert_eq!(action, ExplorerMenuAction::NewQuery);
            }
            _ => panic!("expected ExplorerActionRequested, got something else"),
        }
    }

    #[test]
    fn action_to_event_refresh_emits_explorer_action_requested() {
        let ctx = ctx_for_kind(NodeKind::TablesFolder);
        let event = action_to_event(&ExplorerMenuAction::Refresh, &ctx);
        match event {
            SchemaTreeEvent::ExplorerActionRequested { action, .. } => {
                assert_eq!(action, ExplorerMenuAction::Refresh);
            }
            _ => panic!("expected ExplorerActionRequested, got something else"),
        }
    }

    #[test]
    fn action_to_event_properties_emits_explorer_action_requested() {
        let ctx = ctx_for_kind(NodeKind::Server { id: Uuid::new_v4() });
        let event = action_to_event(&ExplorerMenuAction::Properties, &ctx);
        match event {
            SchemaTreeEvent::ExplorerActionRequested { action, .. } => {
                assert_eq!(action, ExplorerMenuAction::Properties);
            }
            _ => panic!("expected ExplorerActionRequested, got something else"),
        }
    }

    // -----------------------------------------------------------------
    // Tier 2/3 menu and action tests
    // -----------------------------------------------------------------

    #[test]
    fn menu_table_has_view_data_and_count_rows() {
        let ctx = ctx_for_kind(NodeKind::Table {
            schema: "dbo".into(),
            name: "Orders".into(),
        });
        let items = menu_entries_for_context(&ctx, &full_availability());
        let actions = action_labels(&items);
        assert!(
            actions.iter().any(|label| label == "View Data"),
            "table menu missing View Data"
        );
        assert!(
            actions.iter().any(|label| label == "Count Rows"),
            "table menu missing Count Rows"
        );
    }

    #[test]
    fn menu_table_has_ai_submenu() {
        let ctx = ctx_for_kind(NodeKind::Table {
            schema: "dbo".into(),
            name: "Orders".into(),
        });
        let items = menu_entries_for_context(&ctx, &full_availability());
        let submenus = submenu_labels(&items);
        assert!(
            submenus.iter().any(|label| label == "AI"),
            "table menu missing AI submenu"
        );

        let ai_submenu = find_submenu(&items, "AI").expect("AI submenu should exist");
        let ai_actions = action_labels(ai_submenu);
        assert!(
            ai_actions
                .iter()
                .any(|label| label == "Ask AI about this table")
        );
        assert!(
            ai_actions
                .iter()
                .any(|label| label == "Generate SQL involving this table")
        );
        assert!(ai_actions.iter().any(|label| label == "Find Join Path to…"));
        assert!(ai_actions.iter().any(|label| label == "Pin to AI Context"));
    }

    #[test]
    fn menu_view_has_ai_submenu() {
        let ctx = ctx_for_kind(NodeKind::View {
            schema: "dbo".into(),
            name: "OrderView".into(),
        });
        let items = menu_entries_for_context(&ctx, &full_availability());
        let submenus = submenu_labels(&items);
        assert!(
            submenus.iter().any(|label| label == "AI"),
            "view menu missing AI submenu"
        );
    }

    #[test]
    fn menu_procedure_has_ai_submenu() {
        let ctx = ctx_for_kind(NodeKind::Procedure {
            schema: "dbo".into(),
            name: "MyProc".into(),
        });
        let items = menu_entries_for_context(&ctx, &full_availability());
        let submenus = submenu_labels(&items);
        assert!(
            submenus.iter().any(|label| label == "AI"),
            "procedure menu missing AI submenu"
        );
    }

    #[test]
    fn menu_function_has_ai_submenu() {
        let ctx = ctx_for_kind(NodeKind::Function {
            schema: "dbo".into(),
            name: "MyFunc".into(),
        });
        let items = menu_entries_for_context(&ctx, &full_availability());
        let submenus = submenu_labels(&items);
        assert!(
            submenus.iter().any(|label| label == "AI"),
            "function menu missing AI submenu"
        );
    }

    #[test]
    fn menu_view_has_view_data() {
        let ctx = ctx_for_kind(NodeKind::View {
            schema: "dbo".into(),
            name: "OrderView".into(),
        });
        let items = menu_entries_for_context(&ctx, &full_availability());
        let actions = action_labels(&items);
        assert!(
            actions.iter().any(|label| label == "View Data"),
            "view menu missing View Data"
        );
    }

    #[test]
    fn action_to_event_view_data_emits_insert_sql() {
        let ctx = ctx_for_kind(NodeKind::Table {
            schema: "dbo".into(),
            name: "Orders".into(),
        });
        let event = action_to_event(&ExplorerMenuAction::ViewData, &ctx);
        assert!(matches!(event, SchemaTreeEvent::OpenSqlInNewTab { .. }));
    }

    #[test]
    fn action_to_event_count_rows_emits_insert_sql() {
        let ctx = ctx_for_kind(NodeKind::Table {
            schema: "dbo".into(),
            name: "Orders".into(),
        });
        let event = action_to_event(&ExplorerMenuAction::CountRows, &ctx);
        assert!(matches!(event, SchemaTreeEvent::OpenSqlInNewTab { .. }));
    }

    #[test]
    fn action_to_event_ask_schema_emits_explorer_action_requested() {
        let ctx = ctx_for_kind(NodeKind::Table {
            schema: "dbo".into(),
            name: "Orders".into(),
        });
        let event = action_to_event(&ExplorerMenuAction::AskSchema, &ctx);
        match event {
            SchemaTreeEvent::ExplorerActionRequested { action, .. } => {
                assert_eq!(action, ExplorerMenuAction::AskSchema);
            }
            _ => panic!("expected ExplorerActionRequested, got something else"),
        }
    }

    #[test]
    fn action_to_event_generate_sql_emits_explorer_action_requested() {
        let ctx = ctx_for_kind(NodeKind::Table {
            schema: "dbo".into(),
            name: "Orders".into(),
        });
        let event = action_to_event(&ExplorerMenuAction::GenerateSql, &ctx);
        match event {
            SchemaTreeEvent::ExplorerActionRequested { action, .. } => {
                assert_eq!(action, ExplorerMenuAction::GenerateSql);
            }
            _ => panic!("expected ExplorerActionRequested, got something else"),
        }
    }

    #[test]
    fn action_to_event_find_join_path_emits_explorer_action_requested() {
        let ctx = ctx_for_kind(NodeKind::Table {
            schema: "dbo".into(),
            name: "Orders".into(),
        });
        let event = action_to_event(&ExplorerMenuAction::FindJoinPath, &ctx);
        match event {
            SchemaTreeEvent::ExplorerActionRequested { action, .. } => {
                assert_eq!(action, ExplorerMenuAction::FindJoinPath);
            }
            _ => panic!("expected ExplorerActionRequested, got something else"),
        }
    }

    #[test]
    fn action_to_event_pin_to_context_emits_explorer_action_requested() {
        let ctx = ctx_for_kind(NodeKind::Table {
            schema: "dbo".into(),
            name: "Orders".into(),
        });
        let event = action_to_event(&ExplorerMenuAction::PinToContext, &ctx);
        match event {
            SchemaTreeEvent::ExplorerActionRequested { action, .. } => {
                assert_eq!(action, ExplorerMenuAction::PinToContext);
            }
            _ => panic!("expected ExplorerActionRequested, got something else"),
        }
    }

    #[test]
    fn action_to_event_filter_data_emits_explorer_action_requested() {
        let ctx = ctx_for_kind(NodeKind::Table {
            schema: "dbo".into(),
            name: "Orders".into(),
        });
        let event = action_to_event(&ExplorerMenuAction::FilterData, &ctx);
        match event {
            SchemaTreeEvent::ExplorerActionRequested { action, .. } => {
                assert_eq!(action, ExplorerMenuAction::FilterData);
            }
            _ => panic!("expected ExplorerActionRequested, got something else"),
        }
    }

    #[test]
    fn menu_table_view_data_gated_by_has_connection() {
        let ctx = ctx_for_kind(NodeKind::Table {
            schema: "dbo".into(),
            name: "Orders".into(),
        });
        let avail = MenuAvailability {
            has_connection: false,
            ..full_availability()
        };
        let items = menu_entries_for_context(&ctx, &avail);
        for entry in &items {
            if let MenuEntry::Action {
                label,
                enabled,
                action,
            } = entry
            {
                if matches!(action, ExplorerMenuAction::ViewData) {
                    assert!(
                        !*enabled,
                        "{label} should be disabled when has_connection is false"
                    );
                }
                if matches!(action, ExplorerMenuAction::CountRows) {
                    assert!(
                        !*enabled,
                        "{label} should be disabled when has_connection is false"
                    );
                }
            }
        }
    }

    fn sample_discovery(account: &str, sub: &str, server: &str, db: &str) -> AzureSqlDiscovery {
        AzureSqlDiscovery {
            account_id: account.to_string(),
            username: format!("user@{}", account),
            tenant_id: format!("tenant-{}", account),
            subscription_id: sub.to_string(),
            subscription_name: Some(format!("Sub {}", sub)),
            resource_group: format!("rg-{}", server),
            server_id: format!("/sub/{}/servers/{}", sub, server),
            server_name: server.to_string(),
            server_fqdn: format!("{}.database.windows.net", server),
            location: "eastus".to_string(),
            database_id: format!("/sub/{}/servers/{}/dbs/{}", sub, server, db),
            database_name: db.to_string(),
            service_tier: Some("GeneralPurpose".to_string()),
            status: Some("Online".to_string()),
            linked_profile_id: None,
        }
    }

    #[test]
    fn make_azure_tree_builds_account_sub_server_db_hierarchy() {
        let discoveries = vec![
            sample_discovery("acct1", "sub1", "srv1", "db1"),
            sample_discovery("acct1", "sub1", "srv1", "db2"),
            sample_discovery("acct1", "sub2", "srv2", "db3"),
            sample_discovery("acct2", "sub3", "srv3", "db4"),
        ];
        let roots = make_azure_tree(&discoveries, 0);
        // 2 account roots
        assert_eq!(roots.len(), 2);

        // acct1 has 2 subscriptions
        assert_eq!(roots[0].children.len(), 2);
        // sub1 has 1 server (srv1)
        assert_eq!(roots[0].children[0].children.len(), 1);
        // srv1 has 2 databases
        assert_eq!(roots[0].children[0].children[0].children.len(), 2);
        // sub2 has 1 server (srv2) with 1 database
        assert_eq!(roots[0].children[1].children.len(), 1);
        assert_eq!(roots[0].children[1].children[0].children.len(), 1);

        // acct2 has 1 subscription with 1 server with 1 database
        assert_eq!(roots[1].children.len(), 1);
        assert_eq!(roots[1].children[0].children.len(), 1);
        assert_eq!(roots[1].children[0].children[0].children.len(), 1);
    }

    #[test]
    fn make_azure_tree_node_kinds_are_correct() {
        let discoveries = vec![sample_discovery("a", "s", "srv", "db")];
        let roots = make_azure_tree(&discoveries, 0);
        assert!(matches!(roots[0].kind, NodeKind::AzureAccount));
        assert!(matches!(
            roots[0].children[0].kind,
            NodeKind::AzureSubscription
        ));
        assert!(matches!(
            roots[0].children[0].children[0].kind,
            NodeKind::AzureSqlServer
        ));
        assert!(matches!(
            roots[0].children[0].children[0].children[0].kind,
            NodeKind::AzureDatabase(_)
        ));
    }

    #[test]
    fn make_azure_tree_depths_are_assigned_correctly() {
        let discoveries = vec![sample_discovery("a", "s", "srv", "db")];
        let roots = make_azure_tree(&discoveries, 0);
        assert_eq!(roots[0].depth, 0);
        assert_eq!(roots[0].children[0].depth, 1);
        assert_eq!(roots[0].children[0].children[0].depth, 2);
        assert_eq!(roots[0].children[0].children[0].children[0].depth, 3);
    }

    #[test]
    fn make_azure_tree_empty_input_returns_empty() {
        let roots = make_azure_tree(&[], 0);
        assert!(roots.is_empty());
    }

    #[test]
    fn make_azure_tree_load_state_is_loaded() {
        let discoveries = vec![sample_discovery("a", "s", "srv", "db")];
        let roots = make_azure_tree(&discoveries, 0);
        assert_eq!(roots[0].load_state, LoadState::Loaded);
        assert_eq!(roots[0].children[0].load_state, LoadState::Loaded);
    }

    // The old `menu_entries_to_kit_items` converter (and its danger-flag
    // helper) is gone: entries now feed `add_menu_entries`, which builds a
    // live `gpui_component` PopupMenu and therefore needs a real Window, so
    // its structure is covered by the windowed render tests below instead.

    #[test]
    fn azure_account_has_entra_id_icon() {
        assert!(
            crate::icons::entra_id_icon().is_some(),
            "Entra ID icon PNG should be embedded and loadable"
        );
        assert!(matches!(NodeKind::AzureAccount, NodeKind::AzureAccount));
        assert_eq!(NodeKind::AzureAccount.icon(), "AZ");
    }

    #[test]
    fn child_object_kinds_have_markers_and_leaf_shapes() {
        let index = NodeKind::Index {
            schema: "dbo".into(),
            table: "Orders".into(),
            name: "IX_Orders_Date".into(),
        };
        let statistic = NodeKind::Statistic {
            schema: "dbo".into(),
            table: "Orders".into(),
            name: "_WA_1".into(),
        };
        let type_node = NodeKind::Type {
            schema: "dbo".into(),
            name: "Email".into(),
        };
        assert_eq!(index.icon(), "IX");
        assert_eq!(statistic.icon(), "ST");
        assert_eq!(type_node.icon(), "TY");
        assert!(!index.is_expandable());
        assert!(!statistic.is_expandable());
        assert!(!type_node.is_expandable());
    }

    #[test]
    fn object_explorer_entities_carry_lucide_glyphs() {
        // The object-explorer icon scheme: server/database/table/view/
        // procedure/function/column/index glyphs for entity kinds, and no
        // gutter icon for a schema. Kind prefixes stay gone; kinds use icons.
        // Folders keep no gutter glyph (glyph_icon -> None).
        assert_eq!(
            NodeKind::Server { id: Uuid::nil() }.glyph_icon(),
            Some(crate::icon_catalog::AppIcon::Server.path())
        );
        assert_eq!(
            NodeKind::Database("db".into()).glyph_icon(),
            Some(crate::icon_catalog::AppIcon::Database.path())
        );
        assert_eq!(
            NodeKind::Table {
                schema: "dbo".into(),
                name: "Orders".into(),
            }
            .glyph_icon(),
            Some(crate::icon_catalog::AppIcon::Table.path())
        );
        assert_eq!(
            NodeKind::Column.glyph_icon(),
            Some(crate::icon_catalog::AppIcon::Column.path())
        );
        assert_eq!(
            NodeKind::Index {
                schema: "dbo".into(),
                table: "Orders".into(),
                name: "IX_Orders_Date".into(),
            }
            .glyph_icon(),
            Some(crate::icon_catalog::AppIcon::Index.path())
        );
        // A schema draws no gutter icon.
        assert_eq!(
            NodeKind::SchemaFolder {
                schema: "dbo".into()
            }
            .icon(),
            ""
        );
        assert_eq!(
            NodeKind::SchemaFolder {
                schema: "dbo".into()
            }
            .glyph_icon(),
            None
        );
        // A folder keeps no gutter glyph.
        assert_eq!(NodeKind::TablesFolder.glyph_icon(), None);
        assert_eq!(
            NodeKind::View {
                schema: "dbo".into(),
                name: "ActiveOrders".into(),
            }
            .glyph_icon(),
            Some(crate::icon_catalog::AppIcon::View.path())
        );
        assert_eq!(
            NodeKind::Procedure {
                schema: "dbo".into(),
                name: "usp_Close".into(),
            }
            .glyph_icon(),
            Some(crate::icon_catalog::AppIcon::Procedure.path())
        );
        assert_eq!(
            NodeKind::Function {
                schema: "dbo".into(),
                name: "fn_Total".into(),
            }
            .glyph_icon(),
            Some(crate::icon_catalog::AppIcon::Function.path())
        );
        assert_eq!(NodeKind::ViewsFolder.glyph_icon(), None);
        assert_eq!(NodeKind::ProgrammabilityFolder.glyph_icon(), None);
    }

    #[test]
    fn new_folders_are_expandable_with_object_markers() {
        let indexes = NodeKind::IndexesFolder {
            schema: "dbo".into(),
            table: "Orders".into(),
        };
        let statistics = NodeKind::StatisticsFolder {
            schema: "dbo".into(),
            table: "Orders".into(),
        };
        assert!(indexes.is_expandable());
        assert!(statistics.is_expandable());
        assert!(NodeKind::TypesFolder.is_expandable());
        assert_eq!(indexes.icon(), "IX");
        assert_eq!(statistics.icon(), "ST");
        assert_eq!(NodeKind::TypesFolder.icon(), "TY");
    }

    #[test]
    fn index_statistic_type_menus_offer_properties() {
        for kind in [
            NodeKind::Index {
                schema: "dbo".into(),
                table: "Orders".into(),
                name: "IX_Orders_Date".into(),
            },
            NodeKind::Statistic {
                schema: "dbo".into(),
                table: "Orders".into(),
                name: "_WA_1".into(),
            },
            NodeKind::Type {
                schema: "dbo".into(),
                name: "Email".into(),
            },
        ] {
            let ctx = ctx_for_kind(kind);
            let labels = action_labels(&menu_entries_for_context(&ctx, &full_availability()));
            assert!(
                labels.iter().any(|label| label == "Properties…"),
                "expected Properties... in {labels:?}"
            );
        }
    }

    #[test]
    fn index_context_carries_parent_table() {
        let ctx = ctx_for_kind(NodeKind::Index {
            schema: "dbo".into(),
            table: "Orders".into(),
            name: "IX_Orders_Date".into(),
        });
        assert_eq!(ctx.table.as_deref(), Some("Orders"));
        assert_eq!(ctx.schema.as_deref(), Some("dbo"));
        assert_eq!(ctx.name.as_deref(), Some("IX_Orders_Date"));
    }

    #[test]
    fn new_folder_menus_offer_refresh() {
        for kind in [
            NodeKind::IndexesFolder {
                schema: "dbo".into(),
                table: "Orders".into(),
            },
            NodeKind::StatisticsFolder {
                schema: "dbo".into(),
                table: "Orders".into(),
            },
            NodeKind::TypesFolder,
        ] {
            let ctx = ctx_for_kind(kind);
            let labels = action_labels(&menu_entries_for_context(&ctx, &full_availability()));
            assert!(
                labels.iter().any(|label| label == "Refresh"),
                "expected Refresh in {labels:?}"
            );
        }
    }

    /// Minimal metadata fake: every listing is empty except the one a given
    /// loader test feeds.
    struct FakeMetadata {
        schemas: Vec<String>,
        tables: Vec<sqlly_services::metadata::MetadataNode>,
        columns: Vec<sqlly_services::metadata::MetadataNode>,
        indexes: Vec<sqlly_services::metadata::MetadataNode>,
        statistics: Vec<sqlly_services::metadata::MetadataNode>,
        types: Vec<sqlly_services::metadata::MetadataNode>,
        constraints: Vec<sqlly_services::metadata::MetadataNode>,
        foreign_keys: Vec<sqlly_services::metadata::ForeignKeyRow>,
        /// How many times `list_foreign_keys` actually hit the service, so a
        /// test can prove the `ForeignKeyCache` collapses N table expansions into
        /// one database-wide fetch.
        fk_calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl FakeMetadata {
        fn services(self) -> Services {
            let real = Services::new();
            Services::from_parts(
                real.connections.clone(),
                real.intellisense.clone(),
                real.queries.clone(),
                real.catalog.clone(),
                std::sync::Arc::new(self),
                real.admin_tools.clone(),
                real.row_editing.clone(),
            )
        }
    }

    #[async_trait::async_trait]
    impl sqlly_services::metadata::MetadataService for FakeMetadata {
        async fn list_schemas(
            &self,
            _: &ConnectionProfile,
            _: &str,
        ) -> sqlly_core::StudioResult<Vec<String>> {
            Ok(self.schemas.clone())
        }

        async fn list_tables(
            &self,
            _: &ConnectionProfile,
            _: &str,
            schema: Option<&str>,
        ) -> sqlly_core::StudioResult<Vec<sqlly_services::metadata::MetadataNode>> {
            Ok(self
                .tables
                .iter()
                .filter(|t| schema.is_none_or(|s| t.schema.as_deref() == Some(s)))
                .cloned()
                .collect())
        }

        async fn list_views(
            &self,
            _: &ConnectionProfile,
            _: &str,
            _: Option<&str>,
        ) -> sqlly_core::StudioResult<Vec<sqlly_services::metadata::MetadataNode>> {
            Ok(Vec::new())
        }

        async fn list_programmable_objects(
            &self,
            _: &ConnectionProfile,
            _: &str,
            _: Option<&str>,
        ) -> sqlly_core::StudioResult<Vec<sqlly_services::metadata::MetadataNode>> {
            Ok(Vec::new())
        }

        async fn list_columns(
            &self,
            _: &ConnectionProfile,
            _: &str,
            _: &str,
            _: &str,
        ) -> sqlly_core::StudioResult<Vec<sqlly_services::metadata::MetadataNode>> {
            Ok(self.columns.clone())
        }

        async fn list_procedure_parameters(
            &self,
            _: &ConnectionProfile,
            _: &str,
            _: &str,
            _: &str,
        ) -> sqlly_core::StudioResult<Vec<ProcedureParameter>> {
            Ok(Vec::new())
        }

        async fn list_indexes(
            &self,
            _: &ConnectionProfile,
            _: &str,
            _: &str,
            _: &str,
        ) -> sqlly_core::StudioResult<Vec<sqlly_services::metadata::MetadataNode>> {
            Ok(self.indexes.clone())
        }

        async fn list_triggers(
            &self,
            _: &ConnectionProfile,
            _: &str,
            _: &str,
            _: &str,
        ) -> sqlly_core::StudioResult<Vec<sqlly_services::metadata::MetadataNode>> {
            Ok(Vec::new())
        }

        async fn list_constraints(
            &self,
            _: &ConnectionProfile,
            _: &str,
            _: &str,
            _: &str,
        ) -> sqlly_core::StudioResult<Vec<sqlly_services::metadata::MetadataNode>> {
            Ok(self.constraints.clone())
        }

        async fn list_statistics(
            &self,
            _: &ConnectionProfile,
            _: &str,
            _: &str,
            _: &str,
        ) -> sqlly_core::StudioResult<Vec<sqlly_services::metadata::MetadataNode>> {
            Ok(self.statistics.clone())
        }

        async fn list_types(
            &self,
            _: &ConnectionProfile,
            _: &str,
            _: Option<&str>,
        ) -> sqlly_core::StudioResult<Vec<sqlly_services::metadata::MetadataNode>> {
            Ok(self.types.clone())
        }

        async fn list_foreign_keys(
            &self,
            _: &ConnectionProfile,
            _: &str,
        ) -> sqlly_core::StudioResult<Vec<sqlly_services::metadata::ForeignKeyRow>> {
            self.fk_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(self.foreign_keys.clone())
        }
    }

    fn fake_metadata() -> FakeMetadata {
        FakeMetadata {
            schemas: Vec::new(),
            tables: Vec::new(),
            columns: Vec::new(),
            indexes: Vec::new(),
            statistics: Vec::new(),
            types: Vec::new(),
            constraints: Vec::new(),
            foreign_keys: Vec::new(),
            fk_calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    /// Load a node's children reusing a caller-provided [`ForeignKeyCache`] and a
    /// fixed profile, so consecutive loads share cache state the way the live
    /// tree does (each real expansion clones the same cache).
    fn load_with_cache(
        services: &Services,
        cache: &ForeignKeyCache,
        profile: &ConnectionProfile,
        kind: &NodeKind,
        node_id: &str,
    ) -> Result<Vec<TreeNode>, String> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        runtime.block_on(load_children_async(
            services,
            profile,
            kind,
            node_id,
            0,
            crate::preferences::DataModelPreferences::default(),
            cache,
            None,
        ))
    }

    fn test_profile() -> ConnectionProfile {
        use sqlly_core::{SecretProvider, SecretRef};
        ConnectionProfile::sql_login(
            "test",
            "host",
            "sa",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: "k".into(),
            },
        )
    }

    fn load_with(
        services: &Services,
        kind: &NodeKind,
        node_id: &str,
    ) -> Result<Vec<TreeNode>, String> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        runtime.block_on(load_children_async(
            services,
            &test_profile(),
            kind,
            node_id,
            0,
            crate::preferences::DataModelPreferences::default(),
            &ForeignKeyCache::default(),
            None,
        ))
    }

    fn hidden_staging_profile() -> ConnectionProfile {
        let mut profile = test_profile();
        profile.hide_schema("Sales", "staging");
        profile
    }

    fn schema_table(schema: &str, name: &str) -> sqlly_services::metadata::MetadataNode {
        let mut node = sqlly_services::metadata::MetadataNode::new(name, ObjectKind::Table);
        node.schema = Some(schema.to_string());
        node
    }

    fn load_with_prefs(
        services: &Services,
        profile: &ConnectionProfile,
        kind: &NodeKind,
        node_id: &str,
        prefs: crate::preferences::DataModelPreferences,
    ) -> Vec<TreeNode> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        runtime
            .block_on(load_children_async(
                services,
                profile,
                kind,
                node_id,
                0,
                prefs,
                &ForeignKeyCache::default(),
                None,
            ))
            .expect("load")
    }

    fn schema_folder_names(nodes: &[TreeNode]) -> Vec<String> {
        let mut out = Vec::new();
        for node in nodes {
            if let NodeKind::SchemaFolder { schema } = &node.kind {
                out.push(schema.clone());
            }
            out.extend(schema_folder_names(&node.children));
        }
        out
    }

    /// A schema hidden on this connection's database never gets a folder,
    /// while the same schema name in another database still does.
    #[test]
    fn hidden_schema_folders_are_left_out_of_the_database_level() {
        let mut fake = fake_metadata();
        fake.schemas = vec!["dbo".into(), "staging".into(), "sales".into()];
        let services = fake.services();
        let prefs = crate::preferences::DataModelPreferences {
            schema_display: crate::preferences::SchemaDisplayMode::SchemaFolders,
            ..Default::default()
        };
        let profile = hidden_staging_profile();
        let sales = load_with_prefs(
            &services,
            &profile,
            &NodeKind::Database("Sales".into()),
            "srv:x:db:Sales",
            prefs,
        );
        assert_eq!(schema_folder_names(&sales), vec!["dbo", "sales"]);
        let hr = load_with_prefs(
            &services,
            &profile,
            &NodeKind::Database("Hr".into()),
            "srv:x:db:Hr",
            prefs,
        );
        assert_eq!(schema_folder_names(&hr), vec!["dbo", "sales", "staging"]);
    }

    /// In the flat `schema.name` mode there are no schema folders; the
    /// hidden schema's objects are left out of the object folders instead.
    #[test]
    fn hidden_schema_objects_are_left_out_of_flat_folders() {
        let mut fake = fake_metadata();
        fake.tables = vec![
            schema_table("dbo", "Orders"),
            schema_table("staging", "OrdersLoad"),
        ];
        let services = fake.services();
        let prefs = crate::preferences::DataModelPreferences {
            schema_display: crate::preferences::SchemaDisplayMode::FlatPrefixed,
            ..Default::default()
        };
        let tables = load_with_prefs(
            &services,
            &hidden_staging_profile(),
            &NodeKind::TablesFolder,
            "srv:x:db:Sales:tables",
            prefs,
        );
        let titles: Vec<_> = tables.iter().map(|t| t.title.to_string()).collect();
        assert_eq!(titles, vec!["dbo.Orders"]);
        // Nothing hidden: both tables are listed.
        let tables = load_with_prefs(
            &services,
            &test_profile(),
            &NodeKind::TablesFolder,
            "srv:x:db:Sales:tables",
            prefs,
        );
        assert_eq!(tables.len(), 2);
    }

    /// The pruning pass drops hidden schema folders wherever the placement
    /// rules put them (inline or inside the System/Empty schema folders) and
    /// drops a grouping folder it leaves empty.
    #[test]
    fn prune_hidden_schemas_reaches_grouping_folders() {
        let schema = |id: &str, name: &str, depth: usize| {
            TreeNode::new(
                format!("srv:x:db:d:sch:{id}"),
                name.to_string(),
                NodeKind::SchemaFolder {
                    schema: name.to_string(),
                },
                depth,
            )
        };
        let mut sys = TreeNode::new(
            "srv:x:db:d:sysschemas",
            "System Schemas",
            NodeKind::SystemSchemasFolder,
            2,
        );
        sys.load_state = LoadState::Loaded;
        sys.children = vec![schema("sys", "sys", 3)];
        let mut empty = TreeNode::new(
            "srv:x:db:d:emptyschemas",
            "Empty Schemas",
            NodeKind::EmptySchemasFolder,
            2,
        );
        empty.load_state = LoadState::Loaded;
        empty.children = vec![schema("e1", "e1", 3), schema("e2", "e2", 3)];
        let mut nodes = vec![schema("dbo", "dbo", 2), empty, sys];
        let hidden: std::collections::BTreeSet<String> =
            ["sys", "e1", "dbo"].iter().map(|s| s.to_string()).collect();
        prune_hidden_schemas(&mut nodes, &hidden);
        assert_eq!(nodes.len(), 1, "System folder emptied and dropped");
        assert!(matches!(nodes[0].kind, NodeKind::EmptySchemasFolder));
        assert_eq!(schema_folder_names(&nodes), vec!["e2"]);
    }

    /// IntelliSense snapshot hydration builds the same tree the loader
    /// does, so a hidden schema must not come back through it.
    #[test]
    fn hydration_does_not_restore_hidden_schemas() {
        let snapshot = super::render::render_tests::hydration_snapshot();
        let mut profile = test_profile();
        profile.hide_schema("mydb", "dbo");
        for mode in [
            crate::preferences::SchemaDisplayMode::SchemaFolders,
            crate::preferences::SchemaDisplayMode::FlatPrefixed,
        ] {
            let prefs = crate::preferences::DataModelPreferences {
                schema_display: mode,
                ..Default::default()
            };
            let mut hydrated = build_hydrated_db_children(
                "srv:x:db:mydb",
                1,
                &snapshot,
                &["dbo".to_string(), "audit".to_string()],
                prefs,
                sqlly_lang::sqlmodel::DatabaseType::SqlServer,
            );
            prune_hidden_schemas(&mut hydrated, profile.hidden_schemas_in("mydb").unwrap());
            fn any_dbo(nodes: &[TreeNode]) -> bool {
                nodes.iter().any(|n| {
                    matches!(&n.kind,
                        NodeKind::SchemaFolder { schema }
                        | NodeKind::Table { schema, .. }
                        | NodeKind::View { schema, .. }
                        | NodeKind::Procedure { schema, .. }
                        if schema == "dbo")
                        || any_dbo(&n.children)
                })
            }
            assert!(!any_dbo(&hydrated), "{mode:?}: dbo must stay hidden");
        }
    }

    /// A snapshot holding one table per `(schema, name)`, for comparing the
    /// snapshot-hydrated order with the live loader's.
    fn tables_snapshot(tables: &[(&str, &str)]) -> sqlly_lang::sqlmodel::ModelSnapshot {
        use sqlly_lang::sqlmodel as m;
        let d = m::Dialect::SqlServer;
        let name = |s: &str| m::SqlName::new(s, d, false);
        let fragments = tables
            .iter()
            .map(|(schema, table)| m::ObjectFragment {
                reference: m::ObjectRef::new(None, Some(name(schema)), name(table)),
                kind: m::ObjectKind::Table,
                dialect: d,
                body: m::ObjectBody::Table(m::TableBody::default()),
                provenance: m::Provenance::Database {
                    profile: "p".into(),
                },
                fidelity: m::Fidelity::Exact,
                references: vec![],
            })
            .collect();
        let (snap, _) = m::ModelSnapshot::empty().apply(
            1,
            fragments,
            &std::collections::BTreeSet::new(),
            m::IncompleteRegions::default(),
        );
        snap
    }

    /// Case-sensitive server order (MySQL with lower_case_table_names=0,
    /// Postgres C collation, SQLite BINARY): every upper-case name sorts
    /// before every lower-case one.
    const CASE_SENSITIVE_TABLES: [(&str, &str); 5] = [
        ("Sales", "Orders"),
        ("dbo", "BookingTimesheet"),
        ("dbo", "Customer"),
        ("dbo", "booking"),
        ("dbo", "customer_notes"),
    ];

    /// A Tables folder loaded live from the server lists objects in the same
    /// case-insensitive order as one filled from the cached snapshot, so
    /// `booking` sits next to `BookingTimesheet` either way.
    #[test]
    fn live_tables_folder_matches_snapshot_case_insensitive_order() {
        let mut fake = fake_metadata();
        fake.tables = CASE_SENSITIVE_TABLES
            .iter()
            .map(|(s, n)| schema_table(s, n))
            .collect();
        let services = fake.services();
        let prefs = crate::preferences::DataModelPreferences {
            schema_display: crate::preferences::SchemaDisplayMode::FlatPrefixed,
            ..Default::default()
        };
        let live = load_with_prefs(
            &services,
            &test_profile(),
            &NodeKind::TablesFolder,
            "srv:x:db:mydb:tables",
            prefs,
        );
        let live: Vec<(String, String)> = live
            .iter()
            .map(|n| (n.id.clone(), n.title.to_string()))
            .collect();

        let hydrated = build_hydrated_db_children(
            "srv:x:db:mydb",
            1,
            &tables_snapshot(&CASE_SENSITIVE_TABLES),
            &["dbo".to_string(), "Sales".to_string()],
            prefs,
            sqlly_lang::sqlmodel::DatabaseType::SqlServer,
        );
        let tables = hydrated
            .iter()
            .find(|n| matches!(n.kind, NodeKind::TablesFolder))
            .expect("tables folder");
        let snapshot: Vec<(String, String)> = tables
            .children
            .iter()
            .map(|n| (n.id.clone(), n.title.to_string()))
            .collect();

        let titles: Vec<&str> = live.iter().map(|(_, t)| t.as_str()).collect();
        assert_eq!(
            titles,
            vec![
                "dbo.booking",
                "dbo.BookingTimesheet",
                "dbo.Customer",
                "dbo.customer_notes",
                "Sales.Orders",
            ]
        );
        assert_eq!(live, snapshot, "live and snapshot order must agree");
    }

    /// Schema folders loaded live are ordered case-insensitively, exactly like
    /// the snapshot-hydrated schema level.
    #[test]
    fn live_schema_folders_match_snapshot_case_insensitive_order() {
        let mut fake = fake_metadata();
        fake.schemas = vec!["Sales".into(), "audit".into(), "Zeta".into(), "dbo".into()];
        let services = fake.services();
        let prefs = crate::preferences::DataModelPreferences {
            schema_display: crate::preferences::SchemaDisplayMode::SchemaFolders,
            ..Default::default()
        };
        let live = load_with_prefs(
            &services,
            &test_profile(),
            &NodeKind::Database("mydb".into()),
            "srv:x:db:mydb",
            prefs,
        );
        let hydrated = build_hydrated_db_children(
            "srv:x:db:mydb",
            1,
            &tables_snapshot(&[("dbo", "T"), ("Sales", "T"), ("audit", "T"), ("Zeta", "T")]),
            &fake_schema_list(),
            prefs,
            sqlly_lang::sqlmodel::DatabaseType::SqlServer,
        );
        assert_eq!(
            schema_folder_names(&live),
            vec!["audit", "dbo", "Sales", "Zeta"]
        );
        assert_eq!(schema_folder_names(&live), schema_folder_names(&hydrated));
    }

    fn fake_schema_list() -> Vec<String> {
        vec!["Sales".into(), "audit".into(), "Zeta".into(), "dbo".into()]
    }

    /// Names differing only by case (legal on a case-sensitive server) keep a
    /// stable, deterministic order: the exact spelling breaks the tie.
    #[test]
    fn case_only_name_differences_break_ties_on_exact_spelling() {
        let mut fake = fake_metadata();
        fake.tables = vec![
            schema_table("dbo", "orders"),
            schema_table("dbo", "Orders"),
            schema_table("dbo", "ORDERS"),
        ];
        let services = fake.services();
        let prefs = crate::preferences::DataModelPreferences {
            schema_display: crate::preferences::SchemaDisplayMode::FlatPrefixed,
            ..Default::default()
        };
        let live = load_with_prefs(
            &services,
            &test_profile(),
            &NodeKind::TablesFolder,
            "srv:x:db:mydb:tables",
            prefs,
        );
        let titles: Vec<String> = live.iter().map(|n| n.title.to_string()).collect();
        assert_eq!(titles, vec!["dbo.ORDERS", "dbo.Orders", "dbo.orders"]);
    }

    /// Indexes keep the server's order: some engines list them by creation
    /// sequence (SQLite's `pragma_index_list`), which the tree preserves.
    #[test]
    fn index_folder_keeps_server_order() {
        let mut fake = fake_metadata();
        fake.indexes = vec![
            sqlly_services::metadata::MetadataNode::new("zz_pk", ObjectKind::Index),
            sqlly_services::metadata::MetadataNode::new("aa_idx", ObjectKind::Index),
        ];
        let services = fake.services();
        let kind = NodeKind::IndexesFolder {
            schema: "dbo".into(),
            table: "Orders".into(),
        };
        let children = load_with(&services, &kind, "srv:x:db:Sales:t:dbo:Orders:indexes").unwrap();
        let titles: Vec<String> = children.iter().map(|n| n.title.to_string()).collect();
        assert_eq!(titles, vec!["zz_pk", "aa_idx"]);
    }

    #[test]
    fn table_children_append_indexes_statistics_and_fk_folders() {
        let mut fake = fake_metadata();
        fake.columns = vec![
            sqlly_services::metadata::MetadataNode::new("Id", ObjectKind::Column),
            sqlly_services::metadata::MetadataNode::new("Total", ObjectKind::Column),
        ];
        let services = fake.services();
        let kind = NodeKind::Table {
            schema: "dbo".into(),
            name: "Orders".into(),
        };
        let children = load_with(&services, &kind, "srv:x:db:Sales:t:dbo:Orders").unwrap();
        assert_eq!(children.len(), 7);
        assert!(matches!(children[0].kind, NodeKind::Column));
        assert!(matches!(children[1].kind, NodeKind::Column));
        match &children[2].kind {
            NodeKind::IndexesFolder { schema, table } => {
                assert_eq!(schema, "dbo");
                assert_eq!(table, "Orders");
            }
            other => panic!("expected IndexesFolder, got {other:?}"),
        }
        assert!(matches!(
            children[3].kind,
            NodeKind::StatisticsFolder { .. }
        ));
        match &children[4].kind {
            NodeKind::ForeignKeysFolder { schema, table } => {
                assert_eq!(schema, "dbo");
                assert_eq!(table, "Orders");
            }
            other => panic!("expected ForeignKeysFolder, got {other:?}"),
        }
        assert_eq!(children[4].title, "Foreign Keys");
        assert!(matches!(children[5].kind, NodeKind::TriggersFolder { .. }));
        assert!(matches!(
            children[6].kind,
            NodeKind::ConstraintsFolder { .. }
        ));
        assert!(!children[2].expanded && children[2].children.is_empty());
    }

    #[test]
    fn programmability_children_lead_with_types_folder() {
        let services = fake_metadata().services();
        let children = load_with(
            &services,
            &NodeKind::ProgrammabilityFolder,
            "srv:x:db:Sales:prog",
        )
        .unwrap();
        assert!(!children.is_empty());
        assert!(matches!(children[0].kind, NodeKind::TypesFolder));
        assert_eq!(children[0].title, "Types");
    }

    #[test]
    fn foreign_keys_folder_shows_outgoing_and_incoming() {
        use sqlly_services::metadata::ForeignKeyRow;
        let mut fake = fake_metadata();
        fake.foreign_keys = vec![
            // Orders.CustomerId -> Customers.CustomerId (Orders references out).
            ForeignKeyRow {
                constraint_name: "FK_Orders_Customers".into(),
                from_schema: "dbo".into(),
                from_table: "Orders".into(),
                from_column: "CustomerId".into(),
                to_schema: "dbo".into(),
                to_table: "Customers".into(),
                to_column: "CustomerId".into(),
            },
            // OrderLines.OrderId -> Orders.OrderId (Orders is referenced by).
            ForeignKeyRow {
                constraint_name: "FK_OrderLines_Orders".into(),
                from_schema: "dbo".into(),
                from_table: "OrderLines".into(),
                from_column: "OrderId".into(),
                to_schema: "dbo".into(),
                to_table: "Orders".into(),
                to_column: "OrderId".into(),
            },
            // Unrelated key: must be filtered out.
            ForeignKeyRow {
                constraint_name: "FK_Invoices_Vendors".into(),
                from_schema: "dbo".into(),
                from_table: "Invoices".into(),
                from_column: "VendorId".into(),
                to_schema: "dbo".into(),
                to_table: "Vendors".into(),
                to_column: "VendorId".into(),
            },
        ];
        let services = fake.services();
        let kind = NodeKind::ForeignKeysFolder {
            schema: "dbo".into(),
            table: "Orders".into(),
        };
        let children =
            load_with(&services, &kind, "srv:x:db:Sales:t:dbo:Orders:foreignkeys").unwrap();
        assert_eq!(children.len(), 2, "only Orders' own relationships");
        // Outgoing first (row order): Orders -> Customers.
        match children[0].kind {
            NodeKind::ForeignKey { outgoing } => assert!(outgoing),
            ref other => panic!("expected outgoing ForeignKey, got {other:?}"),
        }
        assert_eq!(children[0].title, "dbo.Customers");
        assert_eq!(
            children[0].detail.as_deref(),
            Some("CustomerId → CustomerId")
        );
        // Incoming: OrderLines -> Orders.
        match children[1].kind {
            NodeKind::ForeignKey { outgoing } => assert!(!outgoing),
            ref other => panic!("expected incoming ForeignKey, got {other:?}"),
        }
        assert_eq!(children[1].title, "dbo.OrderLines");
        assert_eq!(children[1].detail.as_deref(), Some("OrderId ← OrderId"));
    }

    #[test]
    fn foreign_keys_folder_groups_composite_key_into_one_node() {
        use sqlly_services::metadata::ForeignKeyRow;
        let mut fake = fake_metadata();
        // A two-column composite FK arrives as two column-pair rows sharing one
        // constraint name; they must render as ONE relationship node.
        fake.foreign_keys = vec![
            ForeignKeyRow {
                constraint_name: "FK_OrderLines_Orders".into(),
                from_schema: "dbo".into(),
                from_table: "Orders".into(),
                from_column: "RegionId".into(),
                to_schema: "dbo".into(),
                to_table: "Regions".into(),
                to_column: "RegionId".into(),
            },
            ForeignKeyRow {
                constraint_name: "FK_OrderLines_Orders".into(),
                from_schema: "dbo".into(),
                from_table: "Orders".into(),
                from_column: "CountryId".into(),
                to_schema: "dbo".into(),
                to_table: "Regions".into(),
                to_column: "CountryId".into(),
            },
        ];
        let services = fake.services();
        let kind = NodeKind::ForeignKeysFolder {
            schema: "dbo".into(),
            table: "Orders".into(),
        };
        let children =
            load_with(&services, &kind, "srv:x:db:Sales:t:dbo:Orders:foreignkeys").unwrap();
        assert_eq!(children.len(), 1, "composite key is one relationship");
        assert_eq!(
            children[0].detail.as_deref(),
            Some("RegionId, CountryId → RegionId, CountryId")
        );
        // The node id is derived from the constraint's schema + name, not a row
        // index.
        assert!(children[0].id.ends_with(":fk:out:dbo.FK_OrderLines_Orders"));
    }

    /// One outgoing FK from `dbo.Orders` to `dbo.Customers`, plus an optional
    /// unrelated key, all sharing one DB-wide listing.
    fn orders_fk_rows() -> Vec<sqlly_services::metadata::ForeignKeyRow> {
        use sqlly_services::metadata::ForeignKeyRow;
        vec![
            ForeignKeyRow {
                constraint_name: "FK_Orders_Customers".into(),
                from_schema: "dbo".into(),
                from_table: "Orders".into(),
                from_column: "CustomerId".into(),
                to_schema: "dbo".into(),
                to_table: "Customers".into(),
                to_column: "CustomerId".into(),
            },
            ForeignKeyRow {
                constraint_name: "FK_Invoices_Orders".into(),
                from_schema: "dbo".into(),
                from_table: "Invoices".into(),
                from_column: "OrderId".into(),
                to_schema: "dbo".into(),
                to_table: "Orders".into(),
                to_column: "OrderId".into(),
            },
        ]
    }

    #[test]
    fn fk_cache_serves_multiple_table_expansions_from_one_fetch() {
        let mut fake = fake_metadata();
        fake.foreign_keys = orders_fk_rows();
        let calls = fake.fk_calls.clone();
        let services = fake.services();
        let cache = ForeignKeyCache::default();
        let profile = test_profile();

        // Expand Orders' FK folder, then Customers' — same connection + database.
        let orders = load_with_cache(
            &services,
            &cache,
            &profile,
            &NodeKind::ForeignKeysFolder {
                schema: "dbo".into(),
                table: "Orders".into(),
            },
            "srv:x:db:Sales:t:dbo:Orders:foreignkeys",
        )
        .unwrap();
        let customers = load_with_cache(
            &services,
            &cache,
            &profile,
            &NodeKind::ForeignKeysFolder {
                schema: "dbo".into(),
                table: "Customers".into(),
            },
            "srv:x:db:Sales:t:dbo:Customers:foreignkeys",
        )
        .unwrap();

        assert_eq!(orders.len(), 2, "outgoing + the incoming Invoices key");
        assert_eq!(customers.len(), 1, "Customers is referenced by Orders");
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the DB-wide FK listing must be fetched once and reused"
        );
    }

    #[test]
    fn fk_cache_refetches_after_clear() {
        let mut fake = fake_metadata();
        fake.foreign_keys = orders_fk_rows();
        let calls = fake.fk_calls.clone();
        let services = fake.services();
        let cache = ForeignKeyCache::default();
        let profile = test_profile();
        let folder = NodeKind::ForeignKeysFolder {
            schema: "dbo".into(),
            table: "Orders".into(),
        };
        let id = "srv:x:db:Sales:t:dbo:Orders:foreignkeys";

        load_with_cache(&services, &cache, &profile, &folder, id).unwrap();
        // A refresh / connection change clears the cache; the next load refetches.
        cache.clear();
        load_with_cache(&services, &cache, &profile, &folder, id).unwrap();
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "clearing the cache must force a fresh fetch"
        );
    }

    #[test]
    fn fk_node_id_is_stable_across_row_reorder_and_unrelated_additions() {
        use sqlly_services::metadata::ForeignKeyRow;
        let folder = NodeKind::ForeignKeysFolder {
            schema: "dbo".into(),
            table: "Orders".into(),
        };
        let id = "srv:x:db:Sales:t:dbo:Orders:foreignkeys";

        let target = ForeignKeyRow {
            constraint_name: "FK_Orders_Customers".into(),
            from_schema: "dbo".into(),
            from_table: "Orders".into(),
            from_column: "CustomerId".into(),
            to_schema: "dbo".into(),
            to_table: "Customers".into(),
            to_column: "CustomerId".into(),
        };
        let unrelated = ForeignKeyRow {
            constraint_name: "FK_Orders_Regions".into(),
            from_schema: "dbo".into(),
            from_table: "Orders".into(),
            from_column: "RegionId".into(),
            to_schema: "dbo".into(),
            to_table: "Regions".into(),
            to_column: "RegionId".into(),
        };

        // Baseline: just the target key.
        let mut fake = fake_metadata();
        fake.foreign_keys = vec![target.clone()];
        let base = load_with(&fake.services(), &folder, id).unwrap();
        let target_id = base
            .iter()
            .find(|n| n.title == "dbo.Customers")
            .map(|n| n.id.clone())
            .expect("target FK node present");

        // Reorder the rows and add an unrelated key: the target's id must not move.
        let mut fake2 = fake_metadata();
        fake2.foreign_keys = vec![unrelated, target];
        let after = load_with(&fake2.services(), &folder, id).unwrap();
        let target_id_after = after
            .iter()
            .find(|n| n.title == "dbo.Customers")
            .map(|n| n.id.clone())
            .expect("target FK node still present");
        assert_eq!(target_id, target_id_after, "constraint id must be stable");
    }

    #[test]
    fn fk_self_referencing_key_shows_once_as_outgoing() {
        use sqlly_services::metadata::ForeignKeyRow;
        let mut fake = fake_metadata();
        // Employees.ManagerId -> Employees.EmployeeId (self reference).
        fake.foreign_keys = vec![ForeignKeyRow {
            constraint_name: "FK_Employees_Manager".into(),
            from_schema: "dbo".into(),
            from_table: "Employees".into(),
            from_column: "ManagerId".into(),
            to_schema: "dbo".into(),
            to_table: "Employees".into(),
            to_column: "EmployeeId".into(),
        }];
        let children = load_with(
            &fake.services(),
            &NodeKind::ForeignKeysFolder {
                schema: "dbo".into(),
                table: "Employees".into(),
            },
            "srv:x:db:Sales:t:dbo:Employees:foreignkeys",
        )
        .unwrap();
        assert_eq!(children.len(), 1, "a self-referencing key renders once");
        match children[0].kind {
            NodeKind::ForeignKey { outgoing } => assert!(outgoing, "shown as outgoing"),
            ref other => panic!("expected outgoing ForeignKey, got {other:?}"),
        }
    }

    #[test]
    fn fk_same_constraint_name_in_different_schemas_are_distinct_nodes() {
        use sqlly_services::metadata::ForeignKeyRow;
        let mut fake = fake_metadata();
        // Two DIFFERENT constraints both named "FK_Ref" reference dbo.Orders —
        // one owned by dbo, one by sales. They must be two nodes with distinct
        // ids, not merged into one.
        fake.foreign_keys = vec![
            ForeignKeyRow {
                constraint_name: "FK_Ref".into(),
                from_schema: "dbo".into(),
                from_table: "Invoices".into(),
                from_column: "OrderId".into(),
                to_schema: "dbo".into(),
                to_table: "Orders".into(),
                to_column: "OrderId".into(),
            },
            ForeignKeyRow {
                constraint_name: "FK_Ref".into(),
                from_schema: "sales".into(),
                from_table: "Shipments".into(),
                from_column: "OrderId".into(),
                to_schema: "dbo".into(),
                to_table: "Orders".into(),
                to_column: "OrderId".into(),
            },
        ];
        let children = load_with(
            &fake.services(),
            &NodeKind::ForeignKeysFolder {
                schema: "dbo".into(),
                table: "Orders".into(),
            },
            "srv:x:db:Sales:t:dbo:Orders:foreignkeys",
        )
        .unwrap();
        assert_eq!(
            children.len(),
            2,
            "same name, different schemas => two nodes"
        );
        let ids: std::collections::HashSet<&str> = children.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids.len(), 2, "node ids must be distinct: {ids:?}");
    }

    #[test]
    fn indexes_folder_loads_index_leaves_with_detail() {
        let mut fake = fake_metadata();
        fake.indexes = vec![
            sqlly_services::metadata::MetadataNode::new("PK_Orders", ObjectKind::Index)
                .with_detail("CLUSTERED, primary key"),
            sqlly_services::metadata::MetadataNode::new("IX_Orders_Date", ObjectKind::Index)
                .with_detail("NONCLUSTERED"),
        ];
        let services = fake.services();
        let kind = NodeKind::IndexesFolder {
            schema: "dbo".into(),
            table: "Orders".into(),
        };
        let children = load_with(&services, &kind, "srv:x:db:Sales:t:dbo:Orders:indexes").unwrap();
        assert_eq!(children.len(), 2);
        match &children[0].kind {
            NodeKind::Index {
                schema,
                table,
                name,
            } => {
                assert_eq!(
                    (schema.as_str(), table.as_str(), name.as_str()),
                    ("dbo", "Orders", "PK_Orders")
                );
            }
            other => panic!("expected Index, got {other:?}"),
        }
        assert_eq!(
            children[0].detail.as_deref(),
            Some("CLUSTERED, primary key")
        );
        assert!(!children[0].kind.is_expandable());
    }

    #[test]
    fn statistics_folder_loads_statistic_leaves() {
        let mut fake = fake_metadata();
        fake.statistics = vec![
            sqlly_services::metadata::MetadataNode::new("_WA_Sys_1", ObjectKind::Statistic)
                .with_detail("auto"),
        ];
        let services = fake.services();
        let kind = NodeKind::StatisticsFolder {
            schema: "dbo".into(),
            table: "Orders".into(),
        };
        let children =
            load_with(&services, &kind, "srv:x:db:Sales:t:dbo:Orders:statistics").unwrap();
        assert_eq!(children.len(), 1);
        match &children[0].kind {
            NodeKind::Statistic { name, table, .. } => {
                assert_eq!(name, "_WA_Sys_1");
                assert_eq!(table, "Orders");
            }
            other => panic!("expected Statistic, got {other:?}"),
        }
        assert_eq!(children[0].detail.as_deref(), Some("auto"));
    }

    #[test]
    fn types_folder_loads_type_leaves_with_base_type_detail() {
        let mut fake = fake_metadata();
        fake.types = vec![
            sqlly_services::metadata::MetadataNode::new("Email", ObjectKind::Type)
                .with_schema("dbo")
                .with_detail("nvarchar"),
        ];
        let services = fake.services();
        let children = load_with(
            &services,
            &NodeKind::TypesFolder,
            "srv:x:db:Sales:prog:types",
        )
        .unwrap();
        assert_eq!(children.len(), 1);
        match &children[0].kind {
            NodeKind::Type { schema, name } => {
                assert_eq!(schema, "dbo");
                assert_eq!(name, "Email");
            }
            other => panic!("expected Type, got {other:?}"),
        }
        assert_eq!(children[0].detail.as_deref(), Some("nvarchar"));
    }

    #[test]
    fn database_extra_folders_follow_engine_features() {
        use sqlly_lang::sqlmodel::DatabaseType;

        let labels = |db: DatabaseType| {
            load::database_extra_folders("srv:1:db:db", 1, db)
                .into_iter()
                .map(|n| n.title)
                .collect::<Vec<_>>()
        };
        assert!(labels(DatabaseType::SqlServer).contains(&"Sequences".to_string()));
        assert!(labels(DatabaseType::SqlServer).contains(&"Synonyms".to_string()));
        assert!(labels(DatabaseType::SqlServer).contains(&"Triggers".to_string()));
        let pg = labels(DatabaseType::Postgres);
        assert!(pg.contains(&"Materialized Views".to_string()));
        assert!(pg.contains(&"Sequences".to_string()));
        assert!(pg.contains(&"Extensions".to_string()));
        assert!(pg.contains(&"Triggers".to_string()));
        let mysql = labels(DatabaseType::MySql);
        assert!(mysql.iter().any(|label| label == "Events"));
        assert!(mysql.iter().any(|label| label == "Triggers"));
        assert_eq!(labels(DatabaseType::Sqlite), vec!["Triggers".to_string()]);
        let crdb = labels(DatabaseType::CockroachDb);
        assert!(!crdb.contains(&"Materialized Views".to_string()));
        assert!(crdb.contains(&"Sequences".to_string()));
        assert!(crdb.contains(&"Triggers".to_string()));
    }

    #[test]
    fn new_leaf_kinds_have_glyphs_and_are_not_expandable() {
        use model::ConstraintKind;

        let trigger = NodeKind::Trigger {
            schema: "dbo".into(),
            table: Some("t".into()),
            name: "tr".into(),
        };
        assert!(trigger.glyph_icon().is_some());
        assert!(!trigger.is_expandable());
        let constraint = NodeKind::Constraint {
            schema: "dbo".into(),
            table: "t".into(),
            name: "ck".into(),
            kind: ConstraintKind::Check,
        };
        assert!(constraint.glyph_icon().is_some());
        assert!(!constraint.is_expandable());
    }

    #[test]
    fn constraints_folder_excludes_primary_and_foreign_keys() {
        let mut fake = fake_metadata();
        fake.constraints = vec![
            sqlly_services::metadata::MetadataNode::new("PK_Orders", ObjectKind::Constraint)
                .with_detail("PRIMARY_KEY_CONSTRAINT"),
            sqlly_services::metadata::MetadataNode::new("FK_Orders", ObjectKind::Constraint)
                .with_detail("FOREIGN_KEY_CONSTRAINT"),
            sqlly_services::metadata::MetadataNode::new("CK_Orders", ObjectKind::Constraint)
                .with_detail("CHECK_CONSTRAINT"),
            sqlly_services::metadata::MetadataNode::new("UQ_Orders", ObjectKind::Constraint)
                .with_detail("UNIQUE_CONSTRAINT"),
        ];
        let services = fake.services();
        let children = load_with(
            &services,
            &NodeKind::ConstraintsFolder {
                schema: "dbo".into(),
                table: "Orders".into(),
            },
            "srv:x:db:Sales:tables:t:dbo:Orders:constraints",
        )
        .unwrap();
        assert_eq!(children.len(), 2);
        assert!(
            children
                .iter()
                .all(|c| matches!(c.kind, NodeKind::Constraint { .. }))
        );
    }

    #[test]
    fn menu_trigger_offers_create_drop_and_properties() {
        let ctx = ctx_for_kind(NodeKind::Trigger {
            schema: "dbo".into(),
            table: Some("Orders".into()),
            name: "tr_ins".into(),
        });
        let actions = action_labels(&menu_entries_for_context(&ctx, &full_availability()));
        assert!(actions.iter().any(|label| label == "Script as CREATE"));
        assert!(actions.iter().any(|label| label == "Script as DROP"));
        assert!(actions.iter().any(|label| label == "Properties…"));
    }

    #[test]
    fn menu_extensions_folder_offers_install() {
        let ctx = ctx_for_kind(NodeKind::ExtensionsFolder);
        let actions = action_labels(&menu_entries_for_context(&ctx, &full_availability()));
        assert!(actions.iter().any(|label| label == "Install Extension…"));
    }

    #[test]
    fn menu_materialized_view_offers_refresh() {
        let ctx = ctx_for_kind(NodeKind::MaterializedView {
            schema: "public".into(),
            name: "mv".into(),
        });
        let actions = action_labels(&menu_entries_for_context(&ctx, &full_availability()));
        assert!(actions.iter().any(|label| label == "Refresh…"));
    }

    #[test]
    fn script_drop_for_constraint_emits_alter_table() {
        use menus::action_to_event_for;

        let ctx = ctx_for_kind(NodeKind::Constraint {
            schema: "public".into(),
            table: "orders".into(),
            name: "ck_price".into(),
            kind: model::ConstraintKind::Check,
        });
        let event = action_to_event_for(
            sqlly_lang::sqlmodel::DatabaseType::Postgres,
            &ExplorerMenuAction::ScriptDrop,
            &ctx,
        );
        match event {
            SchemaTreeEvent::OpenSqlInNewTab { sql, .. } => {
                assert!(sql.contains("ALTER TABLE"));
                assert!(sql.contains("DROP CONSTRAINT"));
            }
            _ => panic!("expected OpenSqlInNewTab"),
        }
    }

    #[test]
    fn refresh_matview_sql_is_previewed_not_run() {
        use menus::action_to_event_for;

        let ctx = ctx_for_kind(NodeKind::MaterializedView {
            schema: "public".into(),
            name: "mv".into(),
        });
        let event = action_to_event_for(
            sqlly_lang::sqlmodel::DatabaseType::Postgres,
            &ExplorerMenuAction::RefreshMaterializedView,
            &ctx,
        );
        assert!(matches!(event, SchemaTreeEvent::OpenSqlInNewTab { .. }));
    }

    // -----------------------------------------------------------------
    // Maintenance submenu
    // -----------------------------------------------------------------

    fn availability_for(db: sqlly_lang::sqlmodel::DatabaseType) -> MenuAvailability {
        MenuAvailability {
            db,
            ..full_availability()
        }
    }

    fn maintenance_entries(kind: NodeKind, avail: &MenuAvailability) -> Vec<MenuEntry> {
        let ctx = ctx_for_kind(kind);
        let items = menu_entries_for_context(&ctx, avail);
        find_submenu(&items, "Maintenance")
            .expect("Maintenance submenu")
            .clone()
    }

    /// The database submenu lists the node's engine vocabulary — DBCC on SQL
    /// Server, VACUUM on PostgreSQL — each op as a Script item, then a Run
    /// item after the separator.
    #[test]
    fn maintenance_database_submenu_is_engine_specific() {
        use sqlly_lang::sqlmodel::DatabaseType;
        let labels = |db| {
            action_labels(&maintenance_entries(
                NodeKind::Database("Sales".into()),
                &availability_for(db),
            ))
        };
        assert_eq!(
            labels(DatabaseType::SqlServer),
            vec![
                "Script UPDATE STATISTICS (all tables)",
                "Script DBCC CHECKDB",
                "Script Rebuild All Indexes",
                "Run UPDATE STATISTICS (all tables)…",
                "Run DBCC CHECKDB…",
                "Run Rebuild All Indexes…",
            ]
        );
        assert_eq!(
            labels(DatabaseType::Postgres),
            vec![
                "Script VACUUM",
                "Script VACUUM ANALYZE",
                "Script ANALYZE",
                "Script REINDEX DATABASE",
                "Run VACUUM…",
                "Run VACUUM ANALYZE…",
                "Run ANALYZE…",
                "Run REINDEX DATABASE…",
            ]
        );
        assert_eq!(
            labels(DatabaseType::MariaDb),
            vec![
                "Script ANALYZE TABLE (all tables)",
                "Script CHECK TABLE (all tables)",
                "Script OPTIMIZE TABLE (all tables)",
                "Run ANALYZE TABLE (all tables)…",
                "Run CHECK TABLE (all tables)…",
                "Run OPTIMIZE TABLE (all tables)…",
            ]
        );
        assert_eq!(
            labels(DatabaseType::Sqlite),
            vec![
                "Script VACUUM",
                "Script ANALYZE",
                "Script PRAGMA integrity_check",
                "Script REINDEX",
                "Run VACUUM…",
                "Run ANALYZE…",
                "Run PRAGMA integrity_check…",
                "Run REINDEX…",
            ]
        );
    }

    #[test]
    fn maintenance_table_and_index_submenus_follow_the_engine() {
        use sqlly_lang::sqlmodel::DatabaseType;
        let table = NodeKind::Table {
            schema: "public".into(),
            name: "orders".into(),
        };
        let pg = action_labels(&maintenance_entries(
            table.clone(),
            &availability_for(DatabaseType::Postgres),
        ));
        assert_eq!(
            pg,
            vec![
                "Script VACUUM",
                "Script VACUUM (ANALYZE)",
                "Script ANALYZE",
                "Script REINDEX TABLE",
                "Script VACUUM (FULL)",
                "Run VACUUM…",
                "Run VACUUM (ANALYZE)…",
                "Run ANALYZE…",
                "Run REINDEX TABLE…",
                "Run VACUUM (FULL)…",
            ]
        );
        let mysql = action_labels(&maintenance_entries(
            table,
            &availability_for(DatabaseType::MySql),
        ));
        assert!(
            mysql.iter().any(|label| label == "Script REPAIR TABLE"),
            "{mysql:?}"
        );
        assert!(
            mysql.iter().any(|label| label == "Run OPTIMIZE TABLE…"),
            "{mysql:?}"
        );

        let index = NodeKind::Index {
            schema: "dbo".into(),
            table: "Orders".into(),
            name: "IX_Orders".into(),
        };
        let ss = action_labels(&maintenance_entries(
            index.clone(),
            &availability_for(DatabaseType::SqlServer),
        ));
        assert_eq!(
            ss,
            vec![
                "Script ALTER INDEX … REBUILD",
                "Script ALTER INDEX … REORGANIZE",
                "Run ALTER INDEX … REBUILD…",
                "Run ALTER INDEX … REORGANIZE…",
            ]
        );
        // The index node still leads with Copy Name / Copy Qualified Name and
        // keeps Properties.
        let ctx = ctx_for_kind(index.clone());
        let items = menu_entries_for_context(&ctx, &availability_for(DatabaseType::SqlServer));
        let actions = action_labels(&items);
        assert_eq!(actions.first(), Some(&"Copy Name".to_string()));
        assert!(actions.iter().any(|label| label == "Properties…"));
        // MySQL has no index-level maintenance: the submenu is a single
        // disabled row rather than a broken item.
        let ctx = ctx_for_kind(index);
        let items = menu_entries_for_context(&ctx, &availability_for(DatabaseType::MySql));
        let disabled = items.iter().any(|e| {
            matches!(
                e,
                MenuEntry::SubmenuOwned {
                    label,
                    enabled: false,
                    ..
                } if label == "Maintenance"
            )
        });
        assert!(disabled, "{items:?}");
    }

    /// An engine with no maintenance vocabulary shows the capability reason
    /// inside the submenu instead of statements that would fail.
    #[test]
    fn maintenance_submenu_shows_a_reason_on_unsupported_engines() {
        let entries = maintenance_entries(
            NodeKind::Database("Sales".into()),
            &availability_for(sqlly_lang::sqlmodel::DatabaseType::CockroachDb),
        );
        assert_eq!(entries.len(), 1);
        match &entries[0] {
            MenuEntry::ActionOwned { label, enabled, .. } => {
                assert!(!enabled);
                assert!(label.contains("SQL Server, PostgreSQL"), "{label}");
            }
            other => panic!("expected the reason row, got {other:?}"),
        }
    }

    /// Run items are writes: a SELECT-only connection keeps the Script
    /// items and greys out every Run item; no connection greys out both.
    #[test]
    fn maintenance_run_items_respect_the_safety_gates() {
        use sqlly_lang::sqlmodel::DatabaseType;
        let table = NodeKind::Table {
            schema: "dbo".into(),
            name: "Orders".into(),
        };
        let select_only = MenuAvailability {
            can_mutate: false,
            ..availability_for(DatabaseType::SqlServer)
        };
        for entry in maintenance_entries(table.clone(), &select_only) {
            if let MenuEntry::Action {
                label,
                enabled,
                action,
            } = entry
            {
                let is_run = matches!(
                    action,
                    ExplorerMenuAction::Maintenance {
                        mode: MaintenanceMode::Run,
                        ..
                    }
                );
                assert_eq!(enabled, !is_run, "{label}");
            }
        }
        let offline = MenuAvailability {
            has_connection: false,
            can_mutate: false,
            ..availability_for(DatabaseType::SqlServer)
        };
        for entry in maintenance_entries(table, &offline) {
            if let MenuEntry::Action { label, enabled, .. } = entry {
                assert!(!enabled, "{label}");
            }
        }
    }

    /// Maintenance actions route to the app (which resolves the node's
    /// connection, expands per-table ops, and opens the Run confirmation),
    /// never to a bare SQL tab from the tree.
    #[test]
    fn maintenance_actions_route_to_the_app_handler() {
        use menus::action_to_event_for;
        use sqlly_services::maintenance::MaintenanceOp;

        let ctx = ctx_for_kind(NodeKind::Table {
            schema: "public".into(),
            name: "orders".into(),
        });
        for mode in [MaintenanceMode::Script, MaintenanceMode::Run] {
            let action = ExplorerMenuAction::Maintenance {
                op: MaintenanceOp::Vacuum,
                mode,
            };
            let event =
                action_to_event_for(sqlly_lang::sqlmodel::DatabaseType::Postgres, &action, &ctx);
            match event {
                SchemaTreeEvent::ExplorerActionRequested { action: got, .. } => {
                    assert_eq!(got, action);
                }
                _ => panic!("expected ExplorerActionRequested, got something else"),
            }
        }
        assert_eq!(
            maintenance_scope(&NodeKind::Database("x".into())),
            Some(sqlly_services::maintenance::MaintenanceScope::Database)
        );
        assert_eq!(
            maintenance_scope(&NodeKind::View {
                schema: "s".into(),
                name: "v".into()
            }),
            None
        );
    }
}
