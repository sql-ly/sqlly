use crate::tooltip::AppTooltip;
use gpui::{
    AnyElement, App, Context, DismissEvent, Entity, EventEmitter, Focusable, Global, IntoElement,
    MouseDownEvent, Render, Styled, Subscription, WeakEntity, Window, actions, anchored, deferred,
    div, point, prelude::*, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::{PopupMenu, PopupMenuItem};
use gpui_component::{Icon, Selectable as _, Sizable};
use sqlly_core::{AzureSqlDiscovery, ConnectionProfile};
use sqlly_services::Services;
use uuid::Uuid;

use crate::connection::ConnectionStore;
use crate::entra_grouping::ServerGroupingMode;
use crate::explorer_search::{ExplorerQuery, ObjectScope, QueryMode};
use crate::icon_catalog::AppIcon;
use crate::log_console::DiagnosticLog;
use crate::schema_tree::{GroupIdentities, SchemaTree, SchemaTreeEvent};
use crate::theme::Theme;
#[cfg(not(target_arch = "wasm32"))]
use std::path::PathBuf;

actions!(sidebar, [FocusExplorerFilter]);

struct ActiveSidebarHandle(WeakEntity<Sidebar>);

impl Global for ActiveSidebarHandle {}

/// Focus the explorer filter in the frontmost sidebar, opening it if hidden.
/// Used by the global ⌘⇧F / Ctrl+Shift+F binding from `lib.rs`.
pub(crate) fn focus_active_filter(window: &mut Window, cx: &mut App) {
    let Some(sidebar) = cx
        .try_global::<ActiveSidebarHandle>()
        .and_then(|h| h.0.upgrade())
    else {
        return;
    };
    sidebar.update(cx, |this, cx| this.focus_filter(window, cx));
}

pub enum SidebarEvent {
    ConnectRequested(Uuid),
    #[allow(dead_code)]
    DisconnectRequested,
    RefreshSchema,
    AddConnectionRequested,
    ManageConnectionsRequested,
    /// User clicked a discovered Azure database — link it as a saved
    /// connection and connect.
    LinkAzureDatabase(AzureSqlDiscovery),
    /// Forwarded from the schema tree.
    SchemaTree(SchemaTreeEvent),
    /// The Explorer / Library header switcher changed the visible body.
    PanelChanged(SidebarPanel),
    /// Forwarded from the SQL Library panel.
    #[cfg(not(target_arch = "wasm32"))]
    Library(crate::sql_library::SqlLibraryEvent),
}

/// Which body the sidebar is showing. Persisted as `"explorer"` / `"library"`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SidebarPanel {
    #[default]
    Explorer,
    Library,
}

impl EventEmitter<SidebarEvent> for Sidebar {}

pub struct Sidebar {
    connections: Vec<ConnectionProfile>,
    /// The persisted explorer grouping dimension (none / by client / project /
    /// environment / server). Read from the connection store on load and
    /// re-persisted when the user picks a different mode in the toolbar. The
    /// grouping is applied inside the unified server tree.
    grouping: ServerGroupingMode,
    /// Color/icon per client, project and environment, as read from the
    /// connection store. The tree paints group headers with these; kept here so
    /// the status bar can paint the active connection's chips from the same
    /// single read rather than re-parsing `connections.json` itself.
    entities: GroupIdentities,
    /// Free-text filter applied to the tree below. Held here because the field
    /// lives here; the matching is the tree's ([`SchemaTree::set_filter`]).
    filter: String,
    filter_scopes: std::collections::BTreeSet<ObjectScope>,
    /// The sidebar's rendered width, pushed down by the app each frame. The
    /// header uses it to decide whether the grouping label still fits.
    /// Whether the inline filter input is visible in the header. Off by
    /// default — the search icon (and ⌘⇧F) toggles it so the rail does not
    /// keep a persistent second row.
    filter_open: bool,
    /// The search field above the tree. A gpui-component input state; the
    /// rendered element uses the compact `.small()` size so it matches the
    /// query-bar's `Sm` server/database selects in height.
    filter_input: Entity<InputState>,
    /// Overflow menu from the header's more-vertical button (grouping +
    /// collapse-all + refresh).
    overflow_menu: Option<Entity<PopupMenu>>,
    overflow_menu_dismiss: Option<Subscription>,
    overflow_position: Option<gpui::Point<gpui::Pixels>>,
    /// The unified server explorer: every saved connection is a top-level
    /// server node that expands to databases -> tables/views/programmability ->
    /// columns/parameters, all in one tree.
    schema_tree: Entity<SchemaTree>,
    /// Explorer vs Library body. Library content is native-only.
    panel: SidebarPanel,
    #[cfg(not(target_arch = "wasm32"))]
    library: Option<Entity<crate::sql_library::SqlLibraryPanel>>,
    /// Keeps the filter-input change subscription alive for the view's life.
    _subscriptions: Vec<Subscription>,
}

impl Sidebar {
    /// Give the keyboard to the schema tree, and select its first row if
    /// nothing is selected yet — arriving with no selection would leave the
    /// arrow keys with no anchor and the rail looking inert.
    pub fn focus_tree(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.schema_tree.update(cx, |tree, cx| {
            tree.focus(window, cx);
        });
    }

    /// Re-read the Data Model preferences on the inner schema tree (called
    /// when the settings dialog closes).
    pub fn reload_data_model_prefs(&self, cx: &mut Context<Self>) {
        self.schema_tree
            .update(cx, |tree, cx| tree.reload_data_model_prefs(cx));
    }

    /// Reveal a schema object in the tree (expand, select, scroll). Serves
    /// the `auto_navigate_into_view` preference; false when the object is
    /// not among the loaded nodes.
    pub fn reveal_object(
        &self,
        server_id: Option<uuid::Uuid>,
        schema: Option<&str>,
        name: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        self.schema_tree.update(cx, |tree, cx| {
            tree.reveal_object(server_id, schema, name, cx)
        })
    }

    /// Create a named pin group (and put `pin` in it); the error is the
    /// localized reason a name was refused.
    pub fn create_pin_group(
        &self,
        server_id: uuid::Uuid,
        name: &str,
        pin: Option<crate::schema_tree::FavoriteObject>,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        self.schema_tree.update(cx, |tree, cx| {
            tree.create_pin_group(server_id, name, pin, cx)
        })
    }

    pub fn rename_pin_group(
        &self,
        group: uuid::Uuid,
        name: &str,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        self.schema_tree
            .update(cx, |tree, cx| tree.rename_pin_group(group, name, cx))
    }

    pub fn quick_open_candidates(&self, cx: &App) -> Vec<crate::quick_open::QuickOpenCandidate> {
        self.schema_tree.read(cx).quick_open_candidates()
    }

    /// True while the explorer paints a continuous animation (a connecting
    /// server's pulsing dot). The workbench then renders the sidebar live
    /// rather than from its frame cache.
    pub fn needs_live_frames(&self, cx: &gpui::App) -> bool {
        self.schema_tree.read(cx).any_server_loading()
    }

    pub(crate) fn explorer_action_context_for_selection(
        &self,
        cx: &App,
    ) -> Option<crate::schema_tree::ExplorerActionContext> {
        self.schema_tree
            .read(cx)
            .explorer_action_context_for_selection()
    }

    pub fn reveal_node_id(&self, id: &str, cx: &mut Context<Self>) -> bool {
        self.schema_tree
            .update(cx, |tree, cx| tree.reveal_node_id(id, cx))
    }

    /// Swap the engine services the schema tree drives metadata through, so the
    /// tree loads databases/objects over the same path the active connection
    /// uses (the relay for a Via-Relay connection, in-process for a Direct one).
    pub fn set_services(&self, services: Services, cx: &mut Context<Self>) {
        self.schema_tree
            .update(cx, |tree, cx| tree.set_services(services, cx));
    }

    /// Let the schema tree resolve each server's services by its own route.
    pub fn set_router(&self, router: crate::service_router::ServiceRouter, cx: &mut Context<Self>) {
        self.schema_tree
            .update(cx, |tree, _| tree.set_router(router));
    }

    pub fn new(
        services: Services,
        log: DiagnosticLog,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.set_global(ActiveSidebarHandle(cx.weak_entity()));
        let (connections, grouping, entities) = Self::load_store();
        let schema_tree = cx.new(|cx| SchemaTree::new(services.clone(), log, cx));

        // Seed the tree with the saved connections + grouping + identities.
        schema_tree.update(cx, |tree, cx| {
            tree.set_grouping(grouping, cx);
            tree.set_entities(entities.clone(), cx);
            tree.set_connections(connections.clone(), cx);
        });

        // Translate schema-tree events into sidebar events. Server-level actions
        // map onto the existing connection events so `app.rs` needs no changes;
        // everything else is forwarded verbatim.
        cx.subscribe_in(
            &schema_tree,
            window,
            |this, _tree, event: &SchemaTreeEvent, window, cx| match event {
                SchemaTreeEvent::ActivateServer(id) => {
                    cx.emit(SidebarEvent::ConnectRequested(*id));
                }
                SchemaTreeEvent::LinkAzureDatabase(discovery) => {
                    cx.emit(SidebarEvent::LinkAzureDatabase(discovery.clone()));
                }
                // Escape in the tree clears the field up here, so the box and
                // the tree can't disagree about what is being filtered. Clearing
                // the box emits a deferred `InputEvent::Change`, which re-runs
                // `set_filter`; the explicit call keeps the tree in step within
                // this update.
                SchemaTreeEvent::FilterCleared => {
                    this.filter_input
                        .update(cx, |state, cx| state.set_value("", window, cx));
                    this.filter_scopes.clear();
                    this.filter.clear();
                    this.schema_tree.update(cx, |tree, cx| {
                        tree.set_filter(ExplorerQuery::parse("", Default::default()), cx);
                    });
                    this.filter_open = false;
                }
                other => cx.emit(SidebarEvent::SchemaTree(other.clone())),
            },
        )
        .detach();

        let filter_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "sidebar.explorer.filter_placeholder",
            ))
        });
        // Push the field's text down to the tree as it changes. The change
        // event fires for typing and for programmatic `set_value` alike, and is
        // delivered after the emitting update unwinds, so the programmatic
        // clear on `FilterCleared` stays re-entrancy-safe.
        let _subscriptions = vec![cx.subscribe_in(
            &filter_input,
            window,
            |this, input, event: &InputEvent, _window, cx| {
                if let InputEvent::Change = event {
                    let text = input.read(cx).value().to_string();
                    this.set_filter(text, cx);
                }
            },
        )];

        #[cfg(not(target_arch = "wasm32"))]
        let library = {
            let root = crate::preferences::SqlLibraryPreferences::load().effective_root();
            let panel = cx.new(|cx| crate::sql_library::SqlLibraryPanel::new(root, window, cx));
            cx.subscribe_in(
                &panel,
                window,
                |_this, _lib, event: &crate::sql_library::SqlLibraryEvent, _window, cx| {
                    cx.emit(SidebarEvent::Library(event.clone()));
                },
            )
            .detach();
            Some(panel)
        };

        Self {
            connections,
            grouping,
            entities,
            filter: String::new(),
            filter_scopes: std::collections::BTreeSet::new(),
            filter_open: false,
            filter_input,
            overflow_menu: None,
            overflow_menu_dismiss: None,
            overflow_position: None,
            schema_tree,
            panel: SidebarPanel::Explorer,
            #[cfg(not(target_arch = "wasm32"))]
            library,
            _subscriptions,
        }
    }

    /// One read of the process-wide connection store, feeding everything
    /// derived from it: the profiles, the grouping mode, and the entity
    /// identities the tree paints group headers with.
    fn load_store() -> (Vec<ConnectionProfile>, ServerGroupingMode, GroupIdentities) {
        let store = ConnectionStore::global();
        (
            store.list(),
            store.server_grouping(),
            GroupIdentities::from_store(store),
        )
    }

    pub fn refresh_connections(&mut self, cx: &mut Context<Self>) {
        let (connections, grouping, entities) = Self::load_store();
        self.connections = connections.clone();
        self.grouping = grouping;
        self.entities = entities.clone();
        self.schema_tree.update(cx, |tree, cx| {
            tree.set_grouping(grouping, cx);
            tree.set_entities(entities, cx);
            tree.set_connections(connections, cx);
        });
        cx.notify();
    }

    /// The color/icon assigned to each client, project and environment. The
    /// status bar reads this to paint the active connection's chips.
    pub fn entities(&self) -> &GroupIdentities {
        &self.entities
    }

    /// All saved connection profiles, for the query-bar server dropdown.
    pub fn connections(&self) -> &[ConnectionProfile] {
        &self.connections
    }

    pub fn panel_persist_key(&self) -> &'static str {
        match self.panel {
            SidebarPanel::Explorer => "explorer",
            SidebarPanel::Library => "library",
        }
    }

    pub fn restore_panel(&mut self, key: Option<&str>, cx: &mut Context<Self>) {
        self.panel = match key {
            Some("library") => SidebarPanel::Library,
            _ => SidebarPanel::Explorer,
        };
        #[cfg(not(target_arch = "wasm32"))]
        if self.panel == SidebarPanel::Library {
            self.rescan_library(cx);
        }
        cx.notify();
    }

    pub fn show_library(&mut self, cx: &mut Context<Self>) {
        self.panel = SidebarPanel::Library;
        #[cfg(not(target_arch = "wasm32"))]
        self.rescan_library(cx);
        cx.emit(SidebarEvent::PanelChanged(self.panel));
        cx.notify();
    }

    pub fn set_panel(&mut self, panel: SidebarPanel, cx: &mut Context<Self>) {
        if self.panel == panel {
            return;
        }
        self.panel = panel;
        #[cfg(not(target_arch = "wasm32"))]
        if panel == SidebarPanel::Library {
            self.rescan_library(cx);
        }
        cx.emit(SidebarEvent::PanelChanged(self.panel));
        cx.notify();
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn set_library_root(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        if let Some(library) = &self.library {
            library.update(cx, |panel, cx| panel.set_root(root, cx));
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn rescan_library(&mut self, cx: &mut Context<Self>) {
        if let Some(library) = &self.library {
            library.update(cx, |panel, cx| panel.rescan(cx));
        }
    }

    /// Whether the active server for `profile_id` failed to connect/enumerate,
    /// per the schema tree. The status bar uses it so a profile that could not
    /// connect stops reading as a healthy connection.
    pub fn server_load_failed(&self, profile_id: Uuid, cx: &App) -> bool {
        self.schema_tree.read(cx).server_load_failed(profile_id)
    }

    /// Whether the sidebar has any content to render — saved connections or
    /// discovered Azure resources. Used by the render method to decide between
    /// the schema tree and the empty-state placeholder.
    pub fn has_content(&self, cx: &mut Context<Self>) -> bool {
        self.schema_tree.read(cx).has_content()
    }

    /// Names of the databases discovered for the active connection, for the
    /// query-bar database dropdown. Empty until the active server's databases
    /// finish loading.
    pub fn database_names(&self, cx: &mut Context<Self>) -> Vec<String> {
        self.schema_tree.read(cx).database_names()
    }

    /// Point the tree at the focused query window's server (the query-bar
    /// database dropdown source). Window-scope only — it says nothing about
    /// which servers are connected; see [`Sidebar::set_connection_state`].
    pub fn set_bound_server(&mut self, id: Option<Uuid>, cx: &mut Context<Self>) {
        self.schema_tree
            .update(cx, |tree, cx| tree.set_active(id, cx));
        cx.notify();
    }

    /// Attach a runtime Entra SQL access token for a connection so lazy schema
    /// loads in the tree authenticate. Must be called before `load_databases`
    /// (which triggers the first engine call for the server node).
    pub fn set_runtime_entra_token(&mut self, id: Uuid, token: String, cx: &mut Context<Self>) {
        self.schema_tree
            .update(cx, |tree, cx| tree.set_runtime_entra_token(id, token, cx));
    }

    pub fn set_runtime_cloud_iam_token(&mut self, id: Uuid, token: String, cx: &mut Context<Self>) {
        self.schema_tree.update(cx, |tree, cx| {
            tree.set_runtime_cloud_iam_token(id, token, cx)
        });
    }

    /// Inject the shared SQL-token provider so server/database expansion can
    /// re-mint an expired Entra token before dialing.
    pub fn set_entra_sql_token_provider(
        &mut self,
        provider: crate::azure_sql_token::EntraSqlTokenProvider,
        cx: &mut Context<Self>,
    ) {
        self.schema_tree
            .update(cx, |tree, _cx| tree.set_entra_sql_token_provider(provider));
    }

    /// Push discovered Azure SQL resources into the schema tree so they appear
    /// as an account → subscription → server → database hierarchy.
    pub fn set_azure_discoveries(
        &mut self,
        discoveries: Vec<AzureSqlDiscovery>,
        cx: &mut Context<Self>,
    ) {
        self.schema_tree
            .update(cx, |tree, cx| tree.set_azure_discoveries(discoveries, cx));
    }

    /// Make `profile` the active server and drill its subtree open (connect +
    /// list databases). The tree loads deeper levels lazily on expand.
    pub fn load_databases(&mut self, profile: ConnectionProfile, cx: &mut Context<Self>) {
        self.schema_tree
            .update(cx, |tree, cx| tree.activate(profile.id.0, cx));
    }

    /// Seed a database's explorer subtree from the IntelliSense data-model
    /// snapshot (see [`SchemaTree::hydrate_from_snapshot`]).
    pub fn hydrate_schema_tree(
        &mut self,
        profile_id: Uuid,
        database: &str,
        snapshot: &sqlly_lang::sqlmodel::ModelSnapshot,
        schemas: &[String],
        cx: &mut Context<Self>,
    ) {
        self.schema_tree.update(cx, |tree, cx| {
            tree.hydrate_from_snapshot(profile_id, database, snapshot, schemas, cx)
        });
    }

    /// Refresh a single node in the schema tree (drop cached children +
    /// reload if expanded).
    pub fn refresh_schema_node(&mut self, node_id: &str, cx: &mut Context<Self>) {
        self.schema_tree
            .update(cx, |tree, cx| tree.refresh_node(node_id, cx));
    }

    /// The Redis key browser's MATCH pattern on a database node, if any.
    pub fn redis_key_filter(&self, node_id: &str, cx: &gpui::App) -> Option<String> {
        self.schema_tree.read(cx).redis_key_filter(node_id)
    }

    /// Set or clear the Redis key browser's MATCH pattern on a database node
    /// and reload it.
    pub fn set_redis_key_filter(
        &mut self,
        node_id: &str,
        pattern: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.schema_tree.update(cx, |tree, cx| {
            tree.set_redis_key_filter(node_id, pattern, cx)
        });
    }

    /// Remove the schemas a connection now hides from a database's loaded
    /// explorer subtree, in place.
    pub fn apply_hidden_schemas(
        &mut self,
        server_id: Uuid,
        database: &str,
        cx: &mut Context<Self>,
    ) {
        self.schema_tree.update(cx, |tree, cx| {
            tree.apply_hidden_schemas(server_id, database, cx)
        });
    }

    /// Refresh one server's entire schema subtree — drop all cached children
    /// and re-fetch from the database. This is the full schema metadata
    /// refresh (as opposed to `refresh_connections` which only reloads
    /// connection profiles from disk). The caller names the server; there is
    /// no global "the" server to refresh.
    pub fn refresh_server_schema(&mut self, id: Uuid, cx: &mut Context<Self>) {
        self.schema_tree
            .update(cx, |tree, cx| tree.refresh_server(id, cx));
    }

    /// Explicit per-node disconnect: default-gray dot plus cancellation of
    /// any in-flight database enumeration, so a stale late success cannot
    /// re-green a server the user just disconnected.
    pub fn disconnect_server_node(&mut self, id: Uuid, cx: &mut Context<Self>) {
        self.schema_tree
            .update(cx, |tree, cx| tree.disconnect_server(id, cx));
    }

    /// Clear a stale red dot when a new connect attempt for `id` begins.
    pub fn clear_unreachable(&mut self, id: Uuid, cx: &mut Context<Self>) {
        self.schema_tree
            .update(cx, |tree, cx| tree.clear_unreachable(id, cx));
    }

    /// Record a server's observed connection health on its tree node (green /
    /// red / default-gray dot). Per server — other nodes are untouched.
    pub fn set_connection_state(
        &mut self,
        id: Uuid,
        state: crate::schema_tree::ConnectionState,
        cx: &mut Context<Self>,
    ) {
        self.schema_tree
            .update(cx, |tree, cx| tree.set_connection_state(id, state, cx));
    }

    /// Snapshot of every server's observed connection health — the same map
    /// the tree dots render from.
    pub fn connection_states(
        &self,
        cx: &gpui::App,
    ) -> std::collections::HashMap<Uuid, crate::schema_tree::ConnectionState> {
        self.schema_tree.read(cx).connection_states_snapshot()
    }

    /// Persists the chosen grouping mode to the connection store, applies it to
    /// the tree, and re-renders.
    fn set_grouping(&mut self, mode: ServerGroupingMode, cx: &mut Context<Self>) {
        if self.grouping == mode {
            return;
        }
        self.grouping = mode;
        ConnectionStore::global().set_server_grouping(mode);
        self.schema_tree
            .update(cx, |tree, cx| tree.set_grouping(mode, cx));
        cx.notify();
    }

    /// Label in the overflow grouping menu.
    fn short_group_label(mode: ServerGroupingMode) -> String {
        let key = match mode {
            ServerGroupingMode::None => "sidebar.grouping.all",
            ServerGroupingMode::ByClient => "sidebar.grouping.by_client",
            ServerGroupingMode::ByProject => "sidebar.grouping.by_project",
            ServerGroupingMode::ByEnvironment => "sidebar.grouping.by_environment",
            ServerGroupingMode::ByServer => "sidebar.grouping.by_server",
        };
        crate::localization::text(key)
    }

    fn overflow_extra_labels() -> [String; 3] {
        [
            crate::localization::text("sidebar.overflow.manage_connections"),
            crate::localization::text("sidebar.overflow.collapse_all"),
            crate::localization::text("sidebar.overflow.refresh"),
        ]
    }

    /// Push the filter text down to the tree, which does the matching.
    fn set_filter(&mut self, text: String, cx: &mut Context<Self>) {
        if self.filter == text {
            return;
        }
        self.filter = text;
        let query = ExplorerQuery::parse(&self.filter, self.filter_scopes.clone());
        self.schema_tree
            .update(cx, |tree, cx| tree.set_filter(query, cx));
        cx.notify();
    }

    fn toggle_scope(&mut self, scope: ObjectScope, cx: &mut Context<Self>) {
        if !self.filter_scopes.remove(&scope) {
            self.filter_scopes.insert(scope);
        }
        self.schema_tree.update(cx, |tree, cx| {
            tree.set_filter_scopes(self.filter_scopes.clone(), cx);
        });
        cx.notify();
    }

    #[cfg(test)]
    pub(crate) fn filter_is_open(&self) -> bool {
        self.filter_open
    }

    /// Open the inline filter and give it the keyboard.
    pub fn focus_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.panel != SidebarPanel::Explorer {
            return;
        }
        self.filter_open = true;
        window.focus(&self.filter_input.read(cx).focus_handle(cx), cx);
        cx.notify();
    }

    pub(crate) fn toggle_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        #[cfg(not(target_arch = "wasm32"))]
        if self.panel == SidebarPanel::Library {
            if let Some(library) = self.library.clone() {
                library.update(cx, |panel, cx| panel.toggle_filter(window, cx));
                cx.notify();
            }
            return;
        }
        if self.filter_open {
            self.filter_open = false;
            cx.notify();
        } else {
            self.focus_filter(window, cx);
        }
    }

    fn request_add_connection(&mut self, cx: &mut Context<Self>) {
        tracing::debug!("sidebar: add connection requested");
        cx.emit(SidebarEvent::AddConnectionRequested);
    }

    fn request_manage_connections(&mut self, cx: &mut Context<Self>) {
        tracing::debug!("sidebar: manage connections requested");
        cx.emit(SidebarEvent::ManageConnectionsRequested);
    }

    fn close_overflow_menu(&mut self, cx: &mut Context<Self>) {
        if self.overflow_menu.is_some() {
            self.overflow_menu = None;
            self.overflow_menu_dismiss = None;
            self.overflow_position = None;
            crate::tooltip::menu_closed(cx);
            cx.notify();
        }
    }

    /// Drop menus that would otherwise stay "open" after the Explorer is hidden
    /// (the entity lives on; only the render tree goes away). Call this *before*
    /// unmounting the drawer so click-away/blur can still run.
    pub(crate) fn dismiss_floating_ui(&mut self, cx: &mut Context<Self>) {
        self.close_overflow_menu(cx);
        self.schema_tree
            .update(cx, |tree, cx| tree.close_context_menu(cx));
    }

    #[cfg(test)]
    pub(crate) fn overflow_menu_is_open(&self) -> bool {
        self.overflow_menu.is_some()
    }

    #[cfg(test)]
    pub(crate) fn open_overflow_menu_at(
        &mut self,
        position: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_overflow_menu(position, window, cx);
    }

    fn open_overflow_menu(
        &mut self,
        position: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_overflow_menu(cx);
        let current = self.grouping;
        let sidebar = cx.entity();
        let view = PopupMenu::build(window, cx, {
            let sidebar = sidebar.clone();
            move |menu, _window, _cx| {
                let mut menu = menu.label(crate::localization::text("sidebar.overflow.group_by"));
                for mode in ServerGroupingMode::all() {
                    let mode = *mode;
                    let label = Self::short_group_label(mode);
                    let item_sidebar = sidebar.clone();
                    menu = menu.item(PopupMenuItem::new(label).checked(mode == current).on_click(
                        move |_ev, _window, app| {
                            item_sidebar.update(app, |this, cx| this.set_grouping(mode, cx));
                        },
                    ));
                }
                let collapse_sidebar = sidebar.clone();
                let refresh_sidebar = sidebar.clone();
                let manage_sidebar = sidebar.clone();
                let [manage_label, collapse_label, refresh_label] = Self::overflow_extra_labels();
                menu.separator()
                    .item(
                        PopupMenuItem::new(manage_label).on_click(move |_ev, _window, app| {
                            manage_sidebar.update(app, |this, cx| {
                                this.request_manage_connections(cx);
                            });
                        }),
                    )
                    .separator()
                    .item(
                        PopupMenuItem::new(collapse_label).on_click(move |_ev, _window, app| {
                            collapse_sidebar.update(app, |this, cx| {
                                this.schema_tree
                                    .update(cx, |tree, cx| tree.collapse_all(cx));
                            });
                        }),
                    )
                    .item(
                        PopupMenuItem::new(refresh_label).on_click(move |_ev, _window, app| {
                            refresh_sidebar.update(app, |_this, cx| {
                                cx.emit(SidebarEvent::RefreshSchema);
                            });
                        }),
                    )
            }
        });
        let dismiss = cx.subscribe(&view, |this, _menu, _: &DismissEvent, cx| {
            this.close_overflow_menu(cx);
        });
        // No tooltips while the menu is up — see `crate::tooltip`.
        crate::tooltip::menu_opened(cx);
        self.overflow_menu = Some(view);
        self.overflow_menu_dismiss = Some(dismiss);
        self.overflow_position = Some(position);
        cx.notify();
    }

    fn header_icon(
        id: &'static str,
        icon: AppIcon,
        tooltip: &'static str,
        active: bool,
        cx: &mut Context<Self>,
        on_down: impl Fn(&mut Self, &MouseDownEvent, &mut Window, &mut Context<Self>) + 'static,
    ) -> AnyElement {
        let ink = if active {
            Theme::accent()
        } else {
            Theme::text_dim()
        };
        div()
            .id(id)
            .flex()
            .items_center()
            .justify_center()
            .p(px(Theme::scaled(4.0)))
            .cursor(gpui::CursorStyle::PointingHand)
            .text_color(ink)
            .hover(|s| s.text_color(Theme::text()))
            .app_tooltip(move |_window, cx| {
                crate::app::tooltip_view(crate::localization::text(tooltip), cx)
            })
            .child(
                Icon::empty()
                    .path(icon.path())
                    .size(px(Theme::toolbar_icon_size())),
            )
            .on_mouse_down(gpui::MouseButton::Left, cx.listener(on_down))
            .into_any_element()
    }

    /// A compact toolbar button for the second header row.
    ///
    /// Mirrors the AI panel's session bar so both side panels read the same.
    fn action_button(id: &'static str, icon: AppIcon, label: &'static str) -> Button {
        Button::new(id)
            .xsmall()
            .ghost()
            .compact()
            .icon(
                Icon::empty()
                    .path(icon.path())
                    .size(px(Theme::inline_icon_size())),
            )
            .label(crate::localization::text(label))
    }

    fn render_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let s = Theme::scaled;
        let has_connections = self.has_content(cx);

        // Two rows, matching the AI panel: tabs plus an options menu on top, a
        // contextual action bar underneath. The actions drive whichever panel
        // the tabs have selected, so Library needs no toolbar of its own.
        #[cfg(not(target_arch = "wasm32"))]
        let library_active = self.panel == SidebarPanel::Library && self.library.is_some();
        #[cfg(target_arch = "wasm32")]
        let library_active = false;

        #[allow(unused_mut)]
        let mut filter_open = self.filter_open && !library_active;
        #[allow(unused_mut)]
        let mut filter_field: Option<AnyElement> = None;
        #[allow(unused_mut)]
        let mut actions = div()
            .flex()
            .flex_row()
            .items_center()
            .h(px(Theme::chrome_bar_height()))
            .px(px(Theme::chrome_inline_px()))
            .gap(px(s(4.0)));
        #[allow(unused_mut)]
        let mut overflow: AnyElement = Self::header_icon(
            "sidebar-overflow",
            AppIcon::MoreVertical,
            "sidebar.header.explorer_options_tooltip",
            self.overflow_menu.is_some(),
            cx,
            |this, ev: &MouseDownEvent, window, cx| {
                this.open_overflow_menu(ev.position, window, cx);
            },
        );

        #[cfg(not(target_arch = "wasm32"))]
        if library_active {
            let library = self.library.clone().expect("library_active implies Some");
            let lib = library.read(cx);
            let lib_filter_open = lib.filter_open();
            let lib_overflow_open = lib.overflow_open();
            filter_open = lib_filter_open;
            if lib_filter_open {
                filter_field = Some(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(Input::new(lib.filter_input()).cleanable(true).small())
                        .into_any_element(),
                );
            }
            let overflow_library = library.clone();
            overflow = Self::header_icon(
                "sidebar-overflow",
                AppIcon::MoreVertical,
                "sidebar.header.library_options_tooltip",
                lib_overflow_open,
                cx,
                move |_this, ev: &MouseDownEvent, window, cx| {
                    let position = ev.position;
                    overflow_library.update(cx, |panel, cx| {
                        panel.open_overflow(position, window, cx);
                    });
                },
            );
            if !lib_filter_open {
                let save_library = library.clone();
                let folder_library = library.clone();
                actions = actions
                    .child(
                        Self::action_button(
                            "sidebar-filter",
                            AppIcon::Search,
                            "sidebar.library.filter_button",
                        )
                        .selected(lib_filter_open)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.toggle_filter(window, cx);
                        })),
                    )
                    .child(
                        Self::action_button(
                            "sidebar-save-query",
                            AppIcon::Save,
                            "sidebar.library.save_query_button",
                        )
                        .on_click(move |_, _window, app| {
                            save_library.update(app, |panel, cx| {
                                panel.request_save_active_tab(cx);
                            });
                        }),
                    )
                    .child(
                        Self::action_button(
                            "sidebar-new-folder",
                            AppIcon::Add,
                            "sidebar.library.new_folder_button",
                        )
                        .on_click(move |_, window, app| {
                            folder_library.update(app, |panel, cx| {
                                panel.start_new_folder(window, cx);
                            });
                        }),
                    );
            }
        }

        if !library_active {
            if filter_open {
                filter_field = Some(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(Input::new(&self.filter_input).cleanable(true).small())
                        .into_any_element(),
                );
            } else {
                if has_connections {
                    actions = actions.child(
                        Self::action_button(
                            "sidebar-filter",
                            AppIcon::Search,
                            "sidebar.explorer.filter_button",
                        )
                        .selected(filter_open)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.toggle_filter(window, cx);
                        })),
                    );
                }
                if cfg!(not(target_arch = "wasm32")) {
                    actions = actions.child(
                        Self::action_button(
                            "sidebar-add-connection",
                            AppIcon::Add,
                            "sidebar.explorer.add_server_button",
                        )
                        .on_click(cx.listener(|this, _, _window, cx| {
                            this.request_add_connection(cx);
                        })),
                    );
                }
            }
        }

        let query = ExplorerQuery::parse(&self.filter, self.filter_scopes.clone());
        let compiled = crate::explorer_search::CompiledQuery::compile(&query);
        let invalid_hint = match &query.mode {
            QueryMode::Invalid(_) => Some(crate::localization::text(
                "sidebar.explorer.invalid_regex_hint",
            )),
            QueryMode::Regex(..) => compiled
                .error_message()
                .map(|m| m.lines().next().unwrap_or(m).to_string()),
            _ => None,
        };
        // The regex hint and the scope chips are Explorer filter features.
        let invalid_hint = if library_active { None } else { invalid_hint };
        let is_regex = !library_active
            && matches!(query.mode, QueryMode::Regex(..))
            && compiled.error_message().is_none();

        let close_filter = filter_open.then(|| {
            Self::header_icon(
                "sidebar-filter-close",
                AppIcon::Close,
                "sidebar.explorer.close_filter_tooltip",
                false,
                cx,
                |this, _ev: &MouseDownEvent, window, cx| this.toggle_filter(window, cx),
            )
        });
        let actions = actions
            .children(filter_field)
            .children(close_filter)
            .when(is_regex, |d| {
                d.child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.85)))
                        .text_color(Theme::text_dim())
                        .child(crate::localization::text("sidebar.explorer.regex_badge")),
                )
            })
            .into_any_element();

        let tab_row = div()
            .flex()
            .flex_row()
            .h(px(Theme::chrome_bar_height()))
            .items_center()
            .gap(px(s(2.0)))
            .pl(px(Theme::chrome_inline_px()))
            .pr(px(s(4.0)))
            .border_b_1()
            .border_color(Theme::border())
            .when(cfg!(not(target_arch = "wasm32")), |d| {
                #[cfg(not(target_arch = "wasm32"))]
                {
                    let this = cx.entity();
                    d.child(crate::chrome_tabs::chrome_tabs(
                        "sidebar-panel-tabs",
                        [
                            (
                                SidebarPanel::Explorer,
                                crate::localization::text("sidebar.tabs.explorer"),
                            ),
                            (
                                SidebarPanel::Library,
                                crate::localization::text("sidebar.tabs.library"),
                            ),
                        ],
                        self.panel,
                        move |panel, _window, app| {
                            this.update(app, |sidebar, cx| sidebar.set_panel(panel, cx));
                        },
                    ))
                }
                #[cfg(target_arch = "wasm32")]
                {
                    let _ = cx;
                    d
                }
            })
            .child(div().flex_1())
            .child(overflow);

        let chips = (filter_open && !library_active).then(|| {
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap(px(s(2.0)))
                .px(px(s(8.0)))
                .pb(px(s(4.0)))
                .children(
                    ObjectScope::ALL
                        .iter()
                        .copied()
                        .enumerate()
                        .map(|(i, scope)| {
                            let active = self.filter_scopes.contains(&scope);
                            Button::new(("explorer-scope", i))
                                .small()
                                .ghost()
                                .selected(active)
                                .label(scope.label())
                                .on_click(cx.listener(move |this, _, _window, cx| {
                                    this.toggle_scope(scope, cx);
                                }))
                        }),
                )
        });

        div()
            .flex()
            .flex_col()
            .border_b_1()
            .border_color(Theme::border())
            .child(tab_row)
            .child(actions)
            .children(chips)
            .when_some(invalid_hint, |d, hint| {
                d.child(
                    div()
                        .px(px(s(8.0)))
                        .pb(px(s(4.0)))
                        .text_size(px(Theme::font_size_ratio(0.85)))
                        .text_color(Theme::text_dim())
                        .child(hint),
                )
            })
            .into_any_element()
    }

    fn render_empty_state(&self, cx: &mut Context<Self>) -> AnyElement {
        let s = Theme::scaled;
        div()
            .flex()
            .flex_col()
            .flex_1()
            .items_center()
            .justify_center()
            .gap(px(s(12.0)))
            .p(px(s(24.0)))
            .child(
                Icon::empty()
                    .path(AppIcon::Server.path())
                    .size(px(s(48.0)))
                    .text_color(Theme::text_dim().opacity(0.45)),
            )
            .child(if cfg!(target_arch = "wasm32") {
                div()
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text("sidebar.empty.browser_demo"))
                    .into_any_element()
            } else {
                div()
                    .id("sidebar-empty-add-connection")
                    .cursor(gpui::CursorStyle::PointingHand)
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::accent())
                    .hover(|style| style.text_color(Theme::text()))
                    .child(crate::localization::text("sidebar.empty.add_server"))
                    .on_click(cx.listener(|this, _ev, _window, cx| {
                        this.request_add_connection(cx);
                    }))
                    .into_any_element()
            })
            .into_any_element()
    }
}

impl Render for Sidebar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let has_connections = self.has_content(cx);

        let body = match self.panel {
            SidebarPanel::Library => {
                #[cfg(not(target_arch = "wasm32"))]
                {
                    if let Some(library) = self.library.clone() {
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .w_full()
                            .min_h_0()
                            .child(library)
                            .into_any_element()
                    } else {
                        self.render_empty_state(cx)
                    }
                }
                #[cfg(target_arch = "wasm32")]
                {
                    self.render_empty_state(cx)
                }
            }
            SidebarPanel::Explorer => {
                if has_connections {
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .w_full()
                        .min_h_0()
                        .child(self.schema_tree.clone())
                        .into_any_element()
                } else {
                    self.render_empty_state(cx)
                }
            }
        };

        let chrome = div()
            .flex()
            .flex_col()
            .size_full()
            .bg(Theme::sidebar_bg())
            .on_action(cx.listener(|this, _: &FocusExplorerFilter, window, cx| {
                this.focus_filter(window, cx);
            }))
            .child(self.render_header(cx))
            .child(body);

        if let (Some(menu), Some(position)) = (self.overflow_menu.clone(), self.overflow_position) {
            let dismiss_entity = cx.entity();
            let overlay = deferred(anchored().position(point(position.x, position.y)).child(
                div().occlude().child(menu).on_mouse_down_out(
                    move |_ev: &MouseDownEvent, _window, app| {
                        dismiss_entity.update(app, |this, cx| {
                            this.close_overflow_menu(cx);
                        });
                    },
                ),
            ))
            .with_priority(1);
            chrome.child(overlay)
        } else {
            chrome
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn ensure_components_initialized(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            if cx.try_global::<gpui_component::Theme>().is_none() {
                gpui_component::init(cx);
            }
        });
    }

    struct SidebarHarness {
        sidebar: Entity<Sidebar>,
        add_connection_events: Rc<RefCell<usize>>,
    }

    impl SidebarHarness {
        fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
            let services = Services::new();
            let log = DiagnosticLog::new(crate::log_console::DiagnosticLogLevel::Off);
            let sidebar = cx.new(|cx| Sidebar::new(services, log, window, cx));
            let add_connection_events = Rc::new(RefCell::new(0usize));
            let adds = add_connection_events.clone();
            cx.subscribe(
                &sidebar,
                move |_this, _sidebar, event: &SidebarEvent, _cx| {
                    if matches!(event, SidebarEvent::AddConnectionRequested) {
                        *adds.borrow_mut() += 1;
                    }
                },
            )
            .detach();
            Self {
                sidebar,
                add_connection_events,
            }
        }
    }

    impl Render for SidebarHarness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            self.sidebar.clone()
        }
    }

    #[test]
    fn grouping_mode_labels_read_as_menu_items() {
        let labels: Vec<_> = ServerGroupingMode::all()
            .iter()
            .map(|mode| Sidebar::short_group_label(*mode))
            .collect();
        assert_eq!(
            labels,
            vec![
                "All",
                "By client",
                "By project",
                "By environment",
                "By server"
            ]
        );
        assert_eq!(ServerGroupingMode::all().len(), 5);
    }

    #[gpui::test]
    fn filter_starts_hidden_and_toggles(cx: &mut gpui::TestAppContext) {
        ensure_components_initialized(cx);
        let (view, cx) = cx.add_window_view(SidebarHarness::new);
        view.update_in(cx, |harness, window, cx| {
            assert!(
                !harness.sidebar.read(cx).filter_is_open(),
                "filter must start hidden"
            );
            harness.sidebar.update(cx, |sidebar, cx| {
                sidebar.focus_filter(window, cx);
            });
            assert!(
                harness.sidebar.read(cx).filter_is_open(),
                "⌘⇧F / search must open the inline filter"
            );
            harness.sidebar.update(cx, |sidebar, cx| {
                sidebar.toggle_filter(window, cx);
            });
            assert!(!harness.sidebar.read(cx).filter_is_open());
        });
    }

    #[gpui::test]
    fn dismiss_floating_ui_closes_overflow_menu(cx: &mut gpui::TestAppContext) {
        ensure_components_initialized(cx);
        let (view, cx) = cx.add_window_view(SidebarHarness::new);
        view.update_in(cx, |harness, window, cx| {
            harness.sidebar.update(cx, |sidebar, cx| {
                sidebar.open_overflow_menu_at(
                    gpui::point(gpui::px(8.0), gpui::px(8.0)),
                    window,
                    cx,
                );
            });
            assert!(
                harness.sidebar.read(cx).overflow_menu_is_open(),
                "overflow menu must be open before dismiss"
            );
            harness.sidebar.update(cx, |sidebar, cx| {
                sidebar.dismiss_floating_ui(cx);
            });
            assert!(
                !harness.sidebar.read(cx).overflow_menu_is_open(),
                "hiding the Explorer must drop its overflow menu so the next overlay can open"
            );
        });
    }

    #[gpui::test]
    fn empty_state_add_server_emits_add_connection(cx: &mut gpui::TestAppContext) {
        ensure_components_initialized(cx);
        let (view, cx) = cx.add_window_view(SidebarHarness::new);
        view.update(cx, |harness, cx| {
            harness.sidebar.update(cx, |sidebar, cx| {
                sidebar.request_add_connection(cx);
            });
        });
        cx.run_until_parked();
        view.read_with(cx, |harness, _cx| {
            assert!(
                *harness.add_connection_events.borrow() > 0,
                "empty-state Add a server must emit AddConnectionRequested"
            );
        });
    }

    #[test]
    fn group_labels_cover_every_mode_and_are_distinct() {
        let modes = [
            ServerGroupingMode::None,
            ServerGroupingMode::ByClient,
            ServerGroupingMode::ByProject,
            ServerGroupingMode::ByEnvironment,
            ServerGroupingMode::ByServer,
        ];
        let labels: Vec<String> = modes.into_iter().map(Sidebar::short_group_label).collect();
        // Every mode gets a distinct, non-empty menu label.
        for label in &labels {
            assert!(!label.is_empty());
        }
        let mut unique = labels.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), labels.len(), "labels must be distinct");
    }
}
