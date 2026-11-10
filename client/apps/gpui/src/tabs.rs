use gpui::{Entity, SharedString, WindowHandle};
#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;
#[cfg(target_arch = "wasm32")]
use web_time::Instant;

use crate::ddl_viewer::DdlViewerView;
use crate::editor::SqlEditor;
use crate::query_ai_summary::{QueryAISummaryState, QueryConnectionMetadata, QueryResumeMetadata};
use crate::results::{OutputPanelTab, ResultsView};

/// Minimum Explorer sidebar width in logical pixels. Wide enough that the
/// header's Explorer/Library tabs and its three action icons always fit on
/// one row.
pub const SIDEBAR_MIN_WIDTH: f32 = 260.0;

/// Clamp a dragged sidebar width to the shell minimum. Hide is a separate
/// interaction (title-bar toggle / Cmd+B); this only bounds the expanded size.
pub fn clamp_sidebar_width(width: f32) -> f32 {
    width.max(SIDEBAR_MIN_WIDTH)
}

/// Clamp a dragged AI drawer width to the persisted session bounds.
pub fn clamp_ai_width(width: f32) -> f32 {
    width.clamp(
        crate::workspace_session::MIN_AI_ASSISTANT_WIDTH,
        crate::workspace_session::MAX_AI_ASSISTANT_WIDTH,
    )
}

/// Legacy mid-region view names. The editor is always the mid band (9.1);
/// History/DDL/Plan map to the output panel and AI maps to the drawer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MidRegionTab {
    #[default]
    Editor,
    History,
    /// Constructed when DDL opens in the output panel (or via tests).
    Ddl,
    /// Constructed when a plan capture starts (or via tests).
    Plan,
    /// Constructed when the AI drawer opens (or via tests).
    Ai,
}

impl MidRegionTab {
    #[cfg_attr(not(test), allow(dead_code))]
    pub const fn all() -> [Self; 5] {
        [Self::Editor, Self::History, Self::Ddl, Self::Plan, Self::Ai]
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn label(self) -> String {
        let key = match self {
            Self::Editor => "tabs.region.editor",
            Self::History => "tabs.region.history",
            Self::Ddl => "tabs.region.ddl",
            Self::Plan => "tabs.region.plan",
            Self::Ai => "tabs.region.ai",
        };
        crate::localization::text(key)
    }

    /// History / DDL / Plan shortcuts route through the same home map as 9.1.
    pub fn from_output_tab(tab: OutputPanelTab) -> Option<Self> {
        match tab {
            OutputPanelTab::History => Some(Self::History),
            OutputPanelTab::Ddl => Some(Self::Ddl),
            OutputPanelTab::Plan => Some(Self::Plan),
            OutputPanelTab::Results
            | OutputPanelTab::Messages
            | OutputPanelTab::Diff
            | OutputPanelTab::Statistics
            | OutputPanelTab::PreviewSql => None,
        }
    }
}

/// Where a former mid-region tab lives after the Phase 9 IA change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MidRegionDestination {
    /// SQL editor — always the middle band.
    EditorBand,
    /// Bottom unified output panel.
    Output(OutputPanelTab),
    /// Right-hand AI drawer (title-bar toggle / Cmd+Shift+I).
    AiDrawer,
}

/// Map a legacy mid-region tab onto the Phase 9 home. The editor never leaves
/// the mid band; History/DDL/Plan become output tabs; AI is drawer-only.
pub fn mid_region_destination(tab: MidRegionTab) -> MidRegionDestination {
    match tab {
        MidRegionTab::Editor => MidRegionDestination::EditorBand,
        MidRegionTab::History => MidRegionDestination::Output(OutputPanelTab::History),
        MidRegionTab::Ddl => MidRegionDestination::Output(OutputPanelTab::Ddl),
        MidRegionTab::Plan => MidRegionDestination::Output(OutputPanelTab::Plan),
        MidRegionTab::Ai => MidRegionDestination::AiDrawer,
    }
}

/// Whether an already-split-out side panel stays visible when the mid-region
/// tab changes. Split-out panels are never taken away.
pub fn keep_split_out_panel(already_split: bool) -> bool {
    already_split
}

/// Query-tab chrome matches the AI drawer segments: the active tab is a quiet
/// rounded fill (`panel_bg`); inactive tabs are dim text only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TabChromeState {
    pub filled: bool,
    pub dim: bool,
}

pub fn tab_chrome_state(is_active: bool) -> TabChromeState {
    TabChromeState {
        filled: is_active,
        dim: !is_active,
    }
}

/// Connection-status dot on a query tab.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionDotKind {
    Connected,
    Failed,
    Disconnected,
}

#[cfg(test)]
pub fn connection_dot_kind(has_profile: bool, healthy: bool) -> ConnectionDotKind {
    if !has_profile {
        ConnectionDotKind::Disconnected
    } else if healthy {
        ConnectionDotKind::Connected
    } else {
        ConnectionDotKind::Failed
    }
}

/// Which breadcrumb / chip segment the user clicked.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BreadcrumbSegment {
    Server,
    Database,
}

/// Server · database labels for the tab-strip chip. No fake "Database"
/// placeholder: missing server → "Not connected"; server without a database →
/// the server name alone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectionBreadcrumb {
    pub server: String,
    pub database: Option<String>,
    has_server: bool,
}

impl ConnectionBreadcrumb {
    pub fn from_names(server: Option<&str>, database: Option<&str>) -> Self {
        let server = server
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        let database = database
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        match (server, database) {
            (None, _) => Self {
                server: crate::localization::text("tabs.connection.not_connected"),
                database: None,
                has_server: false,
            },
            (Some(server), database) => Self {
                server,
                database,
                has_server: true,
            },
        }
    }

    #[cfg(test)]
    pub fn chip_label(&self) -> String {
        match &self.database {
            Some(database) => format!("{} · {}", self.server, database),
            None => self.server.clone(),
        }
    }

    /// Chip copy for the active tab: live connection state, never a fake
    /// "Database" placeholder. Disconnected profiles read `name — disconnected`;
    /// connected without a database offers `choose database`.
    pub fn chip_label_with_state(&self, connected: bool) -> String {
        if !self.has_server {
            return crate::localization::text("tabs.connection.not_connected");
        }
        if !connected {
            return crate::localization::format(
                "tabs.connection.server_disconnected",
                &[("server", &self.server)],
            );
        }
        match &self.database {
            Some(database) => crate::localization::format(
                "tabs.connection.server_database",
                &[("server", &self.server), ("database", database)],
            ),
            None => crate::localization::text("tabs.connection.choose_database"),
        }
    }

    #[cfg(test)]
    pub fn segment_label(&self, segment: BreadcrumbSegment) -> &str {
        match segment {
            BreadcrumbSegment::Server => self.server.as_str(),
            BreadcrumbSegment::Database => self.database.as_deref().unwrap_or(""),
        }
    }

    #[cfg(test)]
    pub fn has_database(&self) -> bool {
        self.database.is_some()
    }
}

/// Dim server name shown on inactive query tabs.
pub fn inactive_tab_connection_cue(server: Option<&str>) -> Option<String> {
    server
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

/// Run is armed only while the active tab is connected.
#[cfg(test)]
pub fn run_enabled(connected: bool) -> bool {
    connected
}

#[cfg(test)]
pub fn run_disabled_tooltip() -> &'static str {
    "Connect first"
}

/// Indices to close when "Close Others" is chosen, keeping `keep`.
#[cfg(test)]
pub fn close_others_indices(count: usize, keep: usize) -> Vec<usize> {
    (0..count).filter(|&i| i != keep).collect()
}

/// Indices to the right of `from` (not including `from`).
#[cfg(test)]
pub fn close_to_right_indices(count: usize, from: usize) -> Vec<usize> {
    if from >= count {
        return Vec::new();
    }
    ((from + 1)..count).collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisconnectTabAction {
    Close,
    ClearResults,
    Keep,
}

/// Decide what disconnecting should do to one tab under the user's policy.
pub fn disconnect_tab_action(
    policy: crate::preferences::DisconnectTabPolicy,
    targets_connection: bool,
    pinned: bool,
    dirty: bool,
) -> DisconnectTabAction {
    if !targets_connection {
        return DisconnectTabAction::Keep;
    }
    match policy {
        crate::preferences::DisconnectTabPolicy::KeepTabsKeepResults => DisconnectTabAction::Keep,
        crate::preferences::DisconnectTabPolicy::KeepTabsClearResults => {
            DisconnectTabAction::ClearResults
        }
        crate::preferences::DisconnectTabPolicy::CloseTabs => {
            if pinned || dirty {
                DisconnectTabAction::Keep
            } else {
                DisconnectTabAction::Close
            }
        }
    }
}

pub fn tab_targets_connection(tab: &QueryTab, profile_id: Option<&uuid::Uuid>) -> bool {
    if tab.snapshot_only || tab.ddl_view.is_some() {
        return false;
    }
    match &tab.connection_target {
        None => true,
        Some(target) => profile_id.is_some_and(|id| target.profile_id == *id),
    }
}

/// Reorder `from` to `to` in a tab list. Returns None when the move is a no-op
/// or either index is out of range.
pub fn reorder_tab_index(from: usize, to: usize, count: usize) -> Option<(usize, usize)> {
    if from == to || from >= count || to >= count {
        None
    } else {
        Some((from, to))
    }
}

/// Where `index` lands after moving `from` → `to` with `remove` + `insert`.
pub fn remap_index_after_move(index: usize, from: usize, to: usize) -> usize {
    if index == from {
        to
    } else if from < to && from < index && index <= to {
        index - 1
    } else if to < from && to <= index && index < from {
        index + 1
    } else {
        index
    }
}

/// Apply a tab reorder in place. Returns the destination index when the move
/// happens.
pub fn apply_tab_reorder<T>(tabs: &mut Vec<T>, from: usize, to: usize) -> Option<usize> {
    let (from, to) = reorder_tab_index(from, to, tabs.len())?;
    let item = tabs.remove(from);
    tabs.insert(to, item);
    Some(to)
}

/// Overflow / all-tabs row label: name, dirty, connection, running.
pub fn all_tabs_item_label(
    name: &str,
    dirty: bool,
    connection: Option<&str>,
    running: bool,
) -> String {
    let server = connection.map(str::trim).filter(|value| !value.is_empty());
    let key = match (dirty, server.is_some(), running) {
        (false, false, false) => "tabs.all_tabs.name_only",
        (true, false, false) => "tabs.all_tabs.dirty",
        (false, true, false) => "tabs.all_tabs.connected",
        (true, true, false) => "tabs.all_tabs.dirty_connected",
        (false, false, true) => "tabs.all_tabs.running",
        (true, false, true) => "tabs.all_tabs.dirty_running",
        (false, true, true) => "tabs.all_tabs.connected_running",
        (true, true, true) => "tabs.all_tabs.dirty_connected_running",
    };
    if let Some(server) = server {
        crate::localization::format(key, &[("name", name), ("server", server)])
    } else {
        crate::localization::format(key, &[("name", name)])
    }
}

/// Case-insensitive match against the all-tabs label (name, dirty, connection, running).
pub fn all_tabs_matches(query: &str, label: &str) -> bool {
    let q = query.trim();
    if q.is_empty() {
        return true;
    }
    label.to_lowercase().contains(&q.to_lowercase())
}

/// Screen-reader / status copy for a query tab.
pub fn tab_status_announcement(
    name: &str,
    selected: bool,
    dirty: bool,
    connected: bool,
    running: bool,
    on_disk: crate::file_watch::DiskState,
) -> String {
    let mut parts = vec![name.to_string()];
    if selected {
        parts.push(crate::localization::text("tabs.accessibility.selected"));
    }
    if dirty {
        parts.push(crate::localization::text("tabs.accessibility.unsaved"));
    }
    match on_disk {
        crate::file_watch::DiskState::ModifiedOnDisk => parts.push(crate::localization::text(
            "tabs.accessibility.modified_on_disk",
        )),
        crate::file_watch::DiskState::DeletedOnDisk => parts.push(crate::localization::text(
            "tabs.accessibility.deleted_on_disk",
        )),
        crate::file_watch::DiskState::Clean => {}
    }
    parts.push(if connected {
        crate::localization::text("tabs.accessibility.connected")
    } else {
        crate::localization::text("tabs.accessibility.disconnected")
    });
    if running {
        parts.push(crate::localization::text("tabs.accessibility.running"));
    }
    parts.join(", ")
}

/// Per-tab strings painted in the query tab strip. Rebuilt only when the
/// name, index, selection, dirty, connection, or running state changes —
/// not on every AppView frame.
#[derive(Clone, Debug)]
pub struct TabBarLabels {
    pub title: SharedString,
    pub close_label: SharedString,
    pub tooltip: SharedString,
    /// A query is executing on this tab; the strip paints a static accent dot
    /// (motion is banned by the design system) so the running tab stays
    /// findable after switching away.
    pub running: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TabBarLabelsKey {
    name: String,
    localization_revision: u64,
    index: usize,
    active: bool,
    dirty: bool,
    connected: bool,
    running: bool,
    on_disk: crate::file_watch::DiskState,
}

pub fn build_tab_bar_labels(
    name: &str,
    index: usize,
    active: bool,
    dirty: bool,
    connected: bool,
    running: bool,
    on_disk: crate::file_watch::DiskState,
) -> TabBarLabels {
    let title: SharedString = if name.trim().is_empty() {
        crate::localization::format(
            "tabs.automatic.query_title",
            &[("number", &(index + 1).to_string())],
        )
        .into()
    } else {
        name.to_string().into()
    };
    let announcement =
        tab_status_announcement(title.as_ref(), active, dirty, connected, running, on_disk);
    let close_label: SharedString =
        crate::localization::format("tabs.strip.close_label", &[("title", title.as_ref())]).into();
    let tooltip: SharedString = crate::localization::format(
        "tabs.strip.tooltip",
        &[("title", title.as_ref()), ("status", &announcement)],
    )
    .into();
    TabBarLabels {
        title,
        close_label,
        tooltip,
        running,
    }
}

/// The leading word of an automatic tab title (`key` names the catalog
/// prefix) in every compiled language plus the active override, so a title
/// saved while another language was active is still recognised.
fn automatic_title_prefixes(key: &str) -> Vec<String> {
    let mut prefixes: Vec<String> = sqlly_i18n::LANGUAGES
        .iter()
        .filter_map(|info| sqlly_i18n::compiled_value(info.language, key))
        .map(str::to_string)
        .collect();
    prefixes.push(crate::localization::text(key));
    prefixes.retain(|prefix| !prefix.trim().is_empty());
    prefixes.sort();
    prefixes.dedup();
    prefixes
}

fn automatic_title_number(name: &str, prefix_key: &str) -> Option<u32> {
    let trimmed = name.trim();
    automatic_title_prefixes(prefix_key)
        .iter()
        .filter_map(|prefix| trimmed.strip_prefix(prefix.as_str()))
        .find(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()))
        .and_then(|rest| rest.parse().ok())
}

/// The number in an automatic `Query N` title written in any shipped
/// language (`Query 3`, `Consulta 3`), or `None` for any other name.
pub fn auto_query_number(name: &str) -> Option<u32> {
    automatic_title_number(name, "tabs.automatic.query_prefix")
}

/// AI summary / resume metadata key of an untitled automatic tab: stable
/// across language switches, unlike its `Query N` display name.
fn auto_metadata_key(kind: &str, number: u32) -> String {
    format!("sqlly:auto-{kind}:{number}")
}

/// The number in an automatic `Notebook N` title written in any shipped
/// language, or `None` for any other name.
pub fn auto_notebook_number(name: &str) -> Option<u32> {
    automatic_title_number(name, "tabs.automatic.notebook_prefix")
}

/// The automatic `Notebook N` title in the current language.
pub fn auto_notebook_title(number: u32) -> String {
    crate::localization::format(
        "tabs.automatic.notebook_title",
        &[("number", &number.to_string())],
    )
}

/// The current-language title for an unsaved automatic `Notebook N` tab,
/// when it differs from `name`. `None` leaves the name alone.
pub fn relocalized_notebook_title(
    name: &str,
    auto_number: Option<u32>,
    auto_named: bool,
    user_named: bool,
    saved: bool,
) -> Option<String> {
    let number = auto_number.filter(|number| {
        auto_named && !user_named && !saved && auto_notebook_number(name) == Some(*number)
    })?;
    let title = auto_notebook_title(number);
    (title != name).then_some(title)
}

/// The number in an automatic `Query N` or `Notebook N` title in any shipped
/// language, for advancing the new-tab counter past restored tabs.
pub fn auto_title_number(name: &str) -> Option<u32> {
    auto_query_number(name).or_else(|| auto_notebook_number(name))
}

/// The automatic `Query N` title in the current language.
pub fn auto_query_title(number: u32) -> String {
    crate::localization::format(
        "tabs.automatic.query_title",
        &[("number", &number.to_string())],
    )
}

/// Untitled tabs that still carry the auto-generated `Query N` name, in any
/// shipped language. Only used to read sessions saved before tabs persisted
/// their automatic-name marker.
pub fn name_looks_auto(name: &str) -> bool {
    name.trim().is_empty() || auto_query_number(name).is_some()
}

/// The automatic number a tab named `name` still honestly carries: the
/// recorded number when the name is that same `Query N` title (in any
/// language) and the tab is still eligible for automatic naming.
pub fn live_auto_number(
    name: &str,
    auto_number: Option<u32>,
    auto_named: bool,
    user_named: bool,
    file_backed: bool,
) -> Option<u32> {
    auto_number.filter(|number| {
        should_auto_name(auto_named, user_named, file_backed)
            && auto_query_number(name) == Some(*number)
    })
}

/// The current-language title for an automatic `Query N` tab, when it
/// differs from `name`. `None` leaves the name alone.
pub fn relocalized_auto_title(
    name: &str,
    auto_number: Option<u32>,
    auto_named: bool,
    user_named: bool,
    file_backed: bool,
) -> Option<String> {
    let number = live_auto_number(name, auto_number, auto_named, user_named, file_backed)?;
    let title = auto_query_title(number);
    (title != name).then_some(title)
}

/// How a restored tab is named: `(auto_named, auto_number)`. Sessions that
/// carry the language-neutral marker are trusted; older sessions fall back to
/// recognising a `Query N` title in any shipped language.
pub fn restored_auto_naming(
    title: &str,
    persisted_auto_named: Option<bool>,
    persisted_auto_number: Option<u32>,
    user_named: bool,
    file_backed: bool,
) -> (bool, Option<u32>) {
    if user_named || file_backed {
        return (false, None);
    }
    match persisted_auto_named {
        Some(auto_named) => (auto_named, persisted_auto_number.filter(|_| auto_named)),
        None => {
            let auto_named = name_looks_auto(title);
            (
                auto_named,
                auto_named.then(|| auto_query_number(title)).flatten(),
            )
        }
    }
}

/// Apply an explicit user rename. Trims; ignores empty or unchanged names.
pub fn apply_explicit_rename(current: &str, proposed: &str) -> Option<String> {
    let name = proposed.trim();
    if name.is_empty() || name == current {
        None
    } else {
        Some(name.to_string())
    }
}

/// Whether auto-naming is still allowed for this tab.
pub fn should_auto_name(auto_named: bool, user_named: bool, file_backed: bool) -> bool {
    auto_named && !user_named && !file_backed
}

/// Auto-name an unsaved tab from the first SQL statement or referenced object.
/// Falls back to `Query {n}`.
pub fn auto_name_from_sql(sql: &str, fallback_n: usize) -> String {
    let stripped = strip_sql_lead(sql);
    if stripped.is_empty() {
        return crate::localization::format(
            "tabs.automatic.query_title",
            &[("number", &fallback_n.to_string())],
        );
    }
    if let Some(name) = first_sql_object(&stripped) {
        return name;
    }
    crate::localization::format(
        "tabs.automatic.query_title",
        &[("number", &fallback_n.to_string())],
    )
}

fn strip_sql_lead(sql: &str) -> String {
    let mut out = String::new();
    let bytes = sql.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'-' && i + 1 < bytes.len() && bytes[i + 1] == b'-' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out.trim().to_string()
}

fn first_sql_object(sql: &str) -> Option<String> {
    let padded = format!(" {} ", sql);
    let upper = padded.to_ascii_uppercase();
    let keywords = [
        " FROM ",
        " INTO ",
        " UPDATE ",
        " TABLE ",
        " EXEC ",
        " EXECUTE ",
    ];
    let mut best: Option<(usize, &str)> = None;
    for kw in keywords {
        if let Some(pos) = upper.find(kw)
            && best.is_none_or(|(p, _)| pos < p)
        {
            best = Some((pos, kw));
        }
    }
    let (pos, kw) = best?;
    let rest = padded[pos + kw.len()..].trim_start();
    let ident: String = rest
        .chars()
        .take_while(|c| {
            c.is_ascii_alphanumeric()
                || *c == '_'
                || *c == '.'
                || *c == '['
                || *c == ']'
                || *c == '"'
        })
        .collect();
    let ident = ident
        .trim_matches(|c| c == '[' || c == ']' || c == '"')
        .trim();
    if ident.is_empty() {
        None
    } else {
        Some(ident.to_string())
    }
}

/// Hairline under the tab strip stays visible; query tabs no longer punch it.
#[cfg(test)]
pub fn tab_strip_hairline_visible(_is_active_tab: bool) -> bool {
    true
}

/// Dirty indicator ink is ordinary text, never amber.
pub fn dirty_dot_is_neutral() -> bool {
    true
}

/// Clicking a breadcrumb segment reuses the existing server/database switch
/// paths (`connect_profile` / `switch_database`) — it does not invent a store.
#[cfg(test)]
pub fn breadcrumb_switch_target(segment: BreadcrumbSegment) -> &'static str {
    match segment {
        BreadcrumbSegment::Server => "server",
        BreadcrumbSegment::Database => "database",
    }
}

/// The connection a query tab is pinned to: a stable profile id plus the
/// database selected for that tab. Lightweight by design — never a cloned
/// profile, catalog, or service handle. A tab without a target follows the
/// globally active connection and adopts it on first focus.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TabConnectionTarget {
    pub profile_id: uuid::Uuid,
    pub database: Option<String>,
}

/// Auto-commit (default) or manual commit for a query tab.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum TransactionMode {
    #[default]
    Auto,
    Manual,
}

/// Per-tab manual transaction UI state (the pinned client lives in services).
pub struct ManualTxTabState {
    #[allow(dead_code)]
    pub begun_at: Instant,
    pub last_activity: Instant,
    pub statements: u32,
    pub lost: bool,
}

impl ManualTxTabState {
    pub fn dirty(&self) -> bool {
        self.statements > 0 && !self.lost
    }
}

/// In-flight multi-target run state: task ids for cancel and the per-target
/// services bundle that started each query (ADR-018 relay routing).
pub struct ActiveMultiRun {
    pub task_ids: std::sync::Arc<std::sync::Mutex<Vec<Option<String>>>>,
    pub services: Vec<sqlly_services::Services>,
}

pub struct QueryTab {
    pub id: String,
    pub name: String,
    pub dirty: bool,
    /// Pinned tabs sort to the front of the strip and are skipped by the
    /// bulk close verbs (Close Others / Close All / Close to the Right).
    pub pinned: bool,
    pub file_path: Option<String>,
    /// The connection this tab runs against. Kept in sync with the global
    /// active connection while the tab is focused; switching back to the tab
    /// switches the connection back. `None` follows the active connection.
    pub connection_target: Option<TabConnectionTarget>,
    /// A target loaded from session/workspace metadata remains descriptive
    /// until the user explicitly connects. Merely focusing a restored tab
    /// must never initiate network activity.
    pub restored_connection_inert: bool,
    /// Portable result snapshots are inspection-only: they never execute SQL
    /// or acquire a live connection, even when another tab is connected.
    pub snapshot_only: bool,
    pub editor: Entity<SqlEditor>,
    pub results: Entity<ResultsView>,
    /// When true, `results` is mounted in another OS window (see `detached_results`).
    /// Set before the window handle exists so the main pane paints a placeholder
    /// and cannot mount the entity twice.
    pub results_detached: bool,
    /// Handle of the detached results window, when open. Session-only.
    pub detached_results: Option<WindowHandle<gpui_component::Root>>,
    /// When `Some`, this tab is a read-only DDL viewer tab: it renders the
    /// viewer instead of the editor+results stack. DDL tabs are session-only
    /// (not persisted to the workspace session).
    pub ddl_view: Option<Entity<DdlViewerView>>,
    /// When `Some`, this tab is a SQL notebook: the cell list replaces the
    /// editor+results stack. `editor`/`results` stay allocated (hidden) so
    /// every existing per-tab call site keeps working.
    pub notebook: Option<Entity<crate::notebook_view::NotebookView>>,
    /// On-disk path of the notebook (`.sqlnb.json`), if saved.
    pub notebook_path: Option<String>,
    /// Per-tab DDL side panel (right of the editor). Per-tab, not app-global,
    /// so an async fetch completing after a tab switch updates the
    /// originating tab's panel, never the now-active one.
    pub ddl_panel: Option<Entity<DdlViewerView>>,
    /// Width of the DDL side panel in logical pixels.
    pub ddl_panel_width: f32,
    pub active_query_id: Option<String>,
    /// When set, a multi-target fan-out is draining; cancel uses every task id.
    pub active_multi_run: Option<ActiveMultiRun>,
    /// Last connections chosen in the Run on… dialog for this tab.
    pub last_multi_target_ids: Vec<String>,
    /// Per-tab auto-refresh interval in seconds; `None` is off. Not persisted.
    pub auto_refresh_secs: Option<u64>,
    /// SQL last dispatched through the run pipeline (already substituted).
    pub last_run_sql: Option<String>,
    /// Byte span of the buffer last handed to Run, for the result-source tint.
    pub last_executed_span: Option<(usize, usize)>,
    /// When the last run completed; auto-refresh measures its interval from this.
    pub last_run_completed_at: Option<Instant>,
    /// In-flight COUNT(*) task id for "Calculate total rows", if any.
    pub total_count_query_id: Option<String>,
    pub ai_summary: QueryAISummaryState,
    // ---- Query resume per-tab state (parity with Swift QueryDocumentController) ----
    /// Per-tab override: `None` follows the global scope, `Some(true/false)`
    /// forces resume on/off for this tab.
    pub resume_override_enabled: Option<bool>,
    /// Cached result of `QueryResumePolicy::effective_enabled` for this tab.
    pub resume_effective_enabled: bool,
    /// Per-tab result layout ("stacked" / "tabular"), restored from metadata
    /// when resume is ON.
    pub per_tab_result_layout: Option<String>,
    /// Per-tab diff-mode toggle state, restored from metadata when resume ON.
    pub diff_mode_enabled: bool,
    /// Per-tab FIM autocomplete toggle state, restored from metadata when
    /// resume ON.
    pub fim_autocomplete_enabled: bool,
    /// When true, suppresses the one-time confirmation dialog before
    /// switching to the remembered connection.
    pub suppress_resume_connection_confirmation: bool,
    /// Connection remembered from the last session, armed for deferred
    /// activation on the next query execution.
    pub pending_resume_connection: Option<QueryConnectionMetadata>,
    /// Whether a pending resume connection confirmation is armed (the
    /// connection has not yet been applied).
    pub has_pending_resume_confirmation: bool,
    /// Per-tab query version history side panel (right of the results, next
    /// to the DDL panel). Per-tab, like `ddl_panel`, so restores land in the
    /// tab that asked for them.
    #[cfg(not(target_arch = "wasm32"))]
    pub history_panel: Option<Entity<crate::history_panel::HistoryPanelView>>,
    /// Width of the history side panel in logical pixels.
    #[cfg(not(target_arch = "wasm32"))]
    pub history_panel_width: f32,
    /// Debounce token for the idle edit-seal timer: each keystroke bumps it,
    /// and a stale timer no-ops when its captured token no longer matches.
    pub history_seal_token: u64,
    /// Auto or manual commit mode for this tab.
    pub transaction_mode: TransactionMode,
    /// Open manual transaction bookkeeping; `None` until the first run in Manual mode.
    pub manual_tx: Option<ManualTxTabState>,
    /// The version currently loaded in the editor via a history restore (the
    /// timeline cursor for prev/next stepping).
    pub history_cursor: Option<String>,
    /// Run version sealed at dispatch, awaiting its results snapshot when the
    /// query completes.
    pub pending_run_version_id: Option<String>,
    /// Procedure-mode state (deploy + invoke loop): toggle, parsed header,
    /// parameter rows, rollback flag, captured outputs.
    pub proc_mode: crate::procedure_mode::ProcedureModeState,
    /// `-- sqlly param:` strip state: one row per declared parameter, with
    /// the typed value each input currently holds.
    pub query_params: crate::query_params_strip::ParamStripState,
    /// Action-bar sub-row is gone; the editor is always the mid band.
    pub mid_region_tab: MidRegionTab,
    /// Which tab is selected in the unified bottom output panel.
    pub output_tab: OutputPanelTab,
    /// History panel was opened as a split (divider / shortcut), not as the
    /// tabbed mid-region view. Split-out panels are never taken away.
    pub history_split: bool,
    /// DDL panel was opened as a split (object navigation), not as the tabbed
    /// mid-region view.
    pub ddl_split: bool,
    /// Untitled tab still using an auto-generated name (`Query N` or object).
    pub auto_named: bool,
    /// The `N` of an automatic `Query N` name. The title is derived from it in
    /// the current language (re-derived on a language switch), and the session
    /// persists the number rather than relying on the translated text.
    pub auto_number: Option<u32>,
    /// Set by Rename Tab…; auto-naming and file-save naming never overwrite a user name.
    pub user_named: bool,
    /// Stable key for this tab's on-disk last-result cache; minted on first
    /// cached run and persisted in the workspace session.
    #[cfg(not(target_arch = "wasm32"))]
    pub result_cache_key: Option<String>,
    /// Last classified on-disk state for a file-backed tab.
    #[cfg(not(target_arch = "wasm32"))]
    pub disk_state: crate::file_watch::DiskState,
    /// Snapshot taken after open, save, or reload — compared on each watch tick.
    #[cfg(not(target_arch = "wasm32"))]
    pub disk_snapshot: Option<crate::file_watch::WatchSnapshot>,
    /// On-disk snapshot already prompted for (Keep). A later external edit
    /// produces a different snapshot and prompts again.
    #[cfg(not(target_arch = "wasm32"))]
    pub disk_prompt_ack: Option<crate::file_watch::WatchSnapshot>,
    /// Cached tab-strip strings. Interior mutability so render can refresh
    /// them without requiring `&mut QueryTab` on every paint.
    tab_bar_labels: std::cell::RefCell<Option<(TabBarLabelsKey, TabBarLabels)>>,
}

impl QueryTab {
    pub fn new(
        id: String,
        name: String,
        editor: Entity<SqlEditor>,
        results: Entity<ResultsView>,
    ) -> Self {
        Self {
            id,
            name,
            dirty: false,
            pinned: false,
            file_path: None,
            connection_target: None,
            restored_connection_inert: false,
            snapshot_only: false,
            editor,
            results,
            results_detached: false,
            detached_results: None,
            ddl_view: None,
            notebook: None,
            notebook_path: None,
            ddl_panel: None,
            ddl_panel_width: 380.0,
            active_query_id: None,
            active_multi_run: None,
            last_multi_target_ids: Vec::new(),
            auto_refresh_secs: None,
            last_run_sql: None,
            last_executed_span: None,
            last_run_completed_at: None,
            total_count_query_id: None,
            ai_summary: QueryAISummaryState::new(),
            resume_override_enabled: None,
            resume_effective_enabled: false,
            per_tab_result_layout: None,
            diff_mode_enabled: false,
            fim_autocomplete_enabled: false,
            suppress_resume_connection_confirmation: false,
            pending_resume_connection: None,
            has_pending_resume_confirmation: false,
            #[cfg(not(target_arch = "wasm32"))]
            history_panel: None,
            #[cfg(not(target_arch = "wasm32"))]
            history_panel_width: 300.0,
            history_seal_token: 0,
            transaction_mode: TransactionMode::default(),
            manual_tx: None,
            history_cursor: None,
            pending_run_version_id: None,
            proc_mode: crate::procedure_mode::ProcedureModeState::default(),
            query_params: crate::query_params_strip::ParamStripState::default(),
            mid_region_tab: MidRegionTab::Editor,
            output_tab: OutputPanelTab::Results,
            history_split: false,
            ddl_split: false,
            auto_named: true,
            auto_number: None,
            user_named: false,
            #[cfg(not(target_arch = "wasm32"))]
            result_cache_key: None,
            #[cfg(not(target_arch = "wasm32"))]
            disk_state: crate::file_watch::DiskState::Clean,
            #[cfg(not(target_arch = "wasm32"))]
            disk_snapshot: None,
            #[cfg(not(target_arch = "wasm32"))]
            disk_prompt_ack: None,
            tab_bar_labels: std::cell::RefCell::new(None),
        }
    }

    /// Title, close tooltip, announcement, and hover text for the tab strip.
    /// Hits a per-tab cache unless name, index, or status inputs changed.
    pub(crate) fn tab_bar_labels(
        &self,
        index: usize,
        active: bool,
        connected: bool,
    ) -> TabBarLabels {
        let running = self.active_query_id.is_some();
        #[cfg(not(target_arch = "wasm32"))]
        let on_disk = self.disk_state;
        #[cfg(target_arch = "wasm32")]
        let on_disk = crate::file_watch::DiskState::Clean;
        {
            let slot = self.tab_bar_labels.borrow();
            if let Some((cached_key, labels)) = slot.as_ref()
                && cached_key.index == index
                && cached_key.active == active
                && cached_key.dirty == self.dirty
                && cached_key.connected == connected
                && cached_key.running == running
                && cached_key.on_disk == on_disk
                && cached_key.name == self.name
                && cached_key.localization_revision == crate::localization::chrome_revision()
            {
                return labels.clone();
            }
        }
        let key = TabBarLabelsKey {
            name: self.name.clone(),
            localization_revision: crate::localization::chrome_revision(),
            index,
            active,
            dirty: self.dirty,
            connected,
            running,
            on_disk,
        };
        let labels = build_tab_bar_labels(
            &key.name,
            key.index,
            key.active,
            key.dirty,
            key.connected,
            key.running,
            key.on_disk,
        );
        *self.tab_bar_labels.borrow_mut() = Some((key, labels.clone()));
        labels
    }

    /// Snapshot the per-tab resume metadata for persistence. Mirrors Swift
    /// `currentResumeMetadata()`. When resume is effective ON, includes the
    /// full set of fields. When OFF, only includes the override (so an
    /// opt-out survives across sessions).
    pub fn current_resume_metadata(&self) -> Option<QueryResumeMetadata> {
        Some(Self::build_resume_metadata(
            self.resume_effective_enabled,
            self.resume_override_enabled,
            self.per_tab_result_layout.as_deref(),
            self.diff_mode_enabled,
            self.fim_autocomplete_enabled,
            self.suppress_resume_connection_confirmation,
            &self.last_multi_target_ids,
        ))
    }

    pub fn is_notebook(&self) -> bool {
        self.notebook.is_some()
    }

    /// Whether this tab is backed by a file (has a non-empty `file_path`).
    pub fn is_file_backed(&self) -> bool {
        self.file_path
            .as_ref()
            .map(|p| !p.is_empty())
            .unwrap_or(false)
    }

    /// File-backed query tabs that should be watched for external edits.
    /// Snapshot-only and DDL viewer tabs are never watched.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn is_watchable(&self) -> bool {
        self.is_file_backed() && !self.snapshot_only && self.ddl_view.is_none()
    }

    /// Record the current on-disk bytes as the clean baseline (open, save, reload).
    #[cfg(not(target_arch = "wasm32"))]
    pub fn record_clean_disk(&mut self, path: &std::path::Path) {
        self.disk_snapshot = Some(crate::file_watch::baseline(path));
        self.disk_state = crate::file_watch::DiskState::Clean;
        self.disk_prompt_ack = None;
    }

    /// The automatic `Query N` number this tab's name still reflects, for
    /// persisting in the session; `None` once renamed, saved, or auto-named.
    pub fn persisted_auto_number(&self) -> Option<u32> {
        live_auto_number(
            &self.name,
            self.auto_number,
            self.auto_named,
            self.user_named,
            self.is_file_backed(),
        )
    }

    /// Restore automatic naming from a saved tab. Call after `name`,
    /// `file_path` and `user_named` are restored; a numbered automatic tab
    /// gets its title rebuilt in the current language.
    pub fn apply_restored_auto_naming(&mut self, state: &crate::workspace_session::TabState) {
        let (auto_named, auto_number) = restored_auto_naming(
            &state.title,
            state.auto_named,
            state.auto_number,
            self.user_named,
            self.is_file_backed(),
        );
        self.auto_named = auto_named;
        self.auto_number = auto_number;
        if let Some(number) = auto_number {
            self.name = auto_query_title(number);
        }
    }

    /// Re-derive an automatic `Query N` (or unsaved `Notebook N`) name in
    /// the current language. Returns whether the name changed.
    pub fn relocalize_auto_name(&mut self) -> bool {
        if self.is_notebook() {
            let Some(title) = relocalized_notebook_title(
                &self.name,
                self.auto_number,
                self.auto_named,
                self.user_named,
                self.notebook_path.is_some(),
            ) else {
                return false;
            };
            self.name = title;
            return true;
        }
        match relocalized_auto_title(
            &self.name,
            self.auto_number,
            self.auto_named,
            self.user_named,
            self.is_file_backed(),
        ) {
            Some(title) => {
                self.name = title;
                true
            }
            None => false,
        }
    }

    /// Metadata key used for the per-tab AI summary / resume record.
    /// Matches the key used at save time: the file path for a file tab, the
    /// language-neutral automatic number for an untitled `Query N` /
    /// `Notebook N` tab (its display name changes with the language), else
    /// the tab name.
    pub fn metadata_key(&self) -> String {
        if let Some(path) = &self.file_path {
            return path.clone();
        }
        if let Some(number) = self.persisted_auto_number() {
            return auto_metadata_key("query", number);
        }
        if self.is_notebook()
            && self.notebook_path.is_none()
            && let Some(number) = self
                .auto_number
                .filter(|n| self.auto_named && auto_notebook_number(&self.name) == Some(*n))
        {
            return auto_metadata_key("notebook", number);
        }
        self.name.clone()
    }

    /// Keys older builds stored this tab's record under (the display name it
    /// had), checked when no record exists under [`Self::metadata_key`].
    /// `saved_title` is the title the session saved for the tab, if any.
    pub fn legacy_metadata_keys(&self, saved_title: Option<&str>) -> Vec<String> {
        let mut keys: Vec<String> = Vec::new();
        if self.file_path.is_some() {
            return keys;
        }
        for title in saved_title
            .into_iter()
            .chain(std::iter::once(self.name.as_str()))
        {
            if !keys.iter().any(|key| key == title) {
                keys.push(title.to_string());
            }
        }
        keys
    }

    /// Build a `QueryResumeMetadata` from the per-tab resume fields. This is
    /// the pure-logic core of `current_resume_metadata`, extracted so it can
    /// be unit-tested without constructing a full `QueryTab` (which requires
    /// gpui `Entity` handles).
    fn build_resume_metadata(
        resume_effective_enabled: bool,
        resume_override_enabled: Option<bool>,
        per_tab_result_layout: Option<&str>,
        diff_mode_enabled: bool,
        fim_autocomplete_enabled: bool,
        suppress_resume_connection_confirmation: bool,
        last_multi_target_ids: &[String],
    ) -> QueryResumeMetadata {
        let multi_target_ids =
            (!last_multi_target_ids.is_empty()).then(|| last_multi_target_ids.to_vec());
        if resume_effective_enabled {
            QueryResumeMetadata {
                override_enabled: resume_override_enabled,
                result_layout: per_tab_result_layout.map(|s| s.to_string()),
                diff_mode_enabled: Some(diff_mode_enabled),
                fim_autocomplete_enabled: Some(fim_autocomplete_enabled),
                suppress_first_run_confirmation: if suppress_resume_connection_confirmation {
                    Some(true)
                } else {
                    None
                },
                multi_target_ids,
            }
        } else {
            QueryResumeMetadata {
                override_enabled: resume_override_enabled,
                result_layout: None,
                diff_mode_enabled: None,
                fim_autocomplete_enabled: None,
                suppress_first_run_confirmation: None,
                multi_target_ids,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::results::TabVisibility;

    #[test]
    fn build_resume_metadata_when_enabled_includes_all_fields() {
        let m = QueryTab::build_resume_metadata(
            true,            // resume_effective_enabled
            Some(true),      // override
            Some("stacked"), // layout
            true,            // diff
            false,           // fim
            true,            // suppress
            &[],
        );
        assert_eq!(m.override_enabled, Some(true));
        assert_eq!(m.result_layout.as_deref(), Some("stacked"));
        assert_eq!(m.diff_mode_enabled, Some(true));
        assert_eq!(m.fim_autocomplete_enabled, Some(false));
        assert_eq!(m.suppress_first_run_confirmation, Some(true));
    }

    #[test]
    fn build_resume_metadata_when_disabled_only_keeps_override() {
        let m = QueryTab::build_resume_metadata(
            false,           // resume_effective_enabled
            Some(false),     // override (opt-out survives)
            Some("stacked"), // layout - should NOT be included
            true,            // diff
            false,           // fim
            true,            // suppress
            &[],
        );
        assert_eq!(m.override_enabled, Some(false));
        assert!(m.result_layout.is_none());
        assert!(m.diff_mode_enabled.is_none());
        assert!(m.fim_autocomplete_enabled.is_none());
        assert!(m.suppress_first_run_confirmation.is_none());
    }

    #[test]
    fn build_resume_metadata_no_override_when_enabled() {
        let m = QueryTab::build_resume_metadata(
            true,  // resume_effective_enabled
            None,  // no override (following global scope)
            None,  // no per-tab layout
            false, // diff off
            false, // fim off
            false, // no suppress
            &[],
        );
        assert!(m.override_enabled.is_none());
        assert!(m.result_layout.is_none());
        assert_eq!(m.diff_mode_enabled, Some(false));
        assert_eq!(m.fim_autocomplete_enabled, Some(false));
        assert!(m.suppress_first_run_confirmation.is_none());
    }

    #[test]
    fn build_resume_metadata_suppress_false_produces_none() {
        let m = QueryTab::build_resume_metadata(
            true,  // resume_effective_enabled
            None,  // no override
            None,  // no layout
            false, // diff off
            false, // fim off
            false, // suppress NOT set
            &[],
        );
        assert!(m.suppress_first_run_confirmation.is_none());
    }

    #[test]
    fn build_resume_metadata_suppress_true_produces_some() {
        let m = QueryTab::build_resume_metadata(
            true,  // resume_effective_enabled
            None,  // no override
            None,  // no layout
            false, // diff off
            false, // fim off
            true,  // suppress IS set
            &[],
        );
        assert_eq!(m.suppress_first_run_confirmation, Some(true));
    }

    #[test]
    fn build_resume_metadata_disabled_no_override_produces_empty() {
        let m = QueryTab::build_resume_metadata(
            false, // resume_effective_enabled
            None,  // no override
            Some("stacked"),
            true,
            true,
            true,
            &[],
        );
        assert!(m.is_empty());
    }

    #[test]
    fn sidebar_width_clamps_to_260px_minimum() {
        assert_eq!(clamp_sidebar_width(100.0), SIDEBAR_MIN_WIDTH);
        assert_eq!(clamp_sidebar_width(260.0), 260.0);
        assert_eq!(clamp_sidebar_width(400.0), 400.0);
    }

    #[test]
    fn ai_drawer_width_clamps_to_session_bounds() {
        assert_eq!(
            clamp_ai_width(10.0),
            crate::workspace_session::MIN_AI_ASSISTANT_WIDTH
        );
        assert_eq!(clamp_ai_width(350.0), 350.0);
        assert_eq!(
            clamp_ai_width(9999.0),
            crate::workspace_session::MAX_AI_ASSISTANT_WIDTH
        );
    }

    #[test]
    fn active_query_tab_uses_quiet_fill_like_ai_segments() {
        let active = tab_chrome_state(true);
        assert!(active.filled);
        assert!(!active.dim);
        let rest = tab_chrome_state(false);
        assert!(!rest.filled);
        assert!(rest.dim);
    }

    #[test]
    fn mid_region_tabs_map_to_output_or_drawer() {
        assert_eq!(
            mid_region_destination(MidRegionTab::Editor),
            MidRegionDestination::EditorBand
        );
        assert_eq!(
            mid_region_destination(MidRegionTab::History),
            MidRegionDestination::Output(OutputPanelTab::History)
        );
        assert_eq!(
            mid_region_destination(MidRegionTab::Ddl),
            MidRegionDestination::Output(OutputPanelTab::Ddl)
        );
        assert_eq!(
            mid_region_destination(MidRegionTab::Plan),
            MidRegionDestination::Output(OutputPanelTab::Plan)
        );
        assert_eq!(
            mid_region_destination(MidRegionTab::Ai),
            MidRegionDestination::AiDrawer
        );
        assert_eq!(MidRegionTab::default(), MidRegionTab::Editor);
        for tab in MidRegionTab::all() {
            let _ = tab.label();
            let _ = mid_region_destination(tab);
        }
        let labels: Vec<_> = OutputPanelTab::core_tabs()
            .iter()
            .map(|t| t.label())
            .collect();
        assert_eq!(labels, vec!["Results", "Messages", "History"]);
        let hidden = TabVisibility {
            chrome_visible: false,
            diff: true,
            ..TabVisibility::default()
        };
        assert!(OutputPanelTab::visible_tabs(hidden).is_empty());
        assert_eq!(
            OutputPanelTab::visible_tabs(TabVisibility::chrome_only()),
            OutputPanelTab::core_tabs().to_vec()
        );
        let with_diff = OutputPanelTab::visible_tabs(TabVisibility {
            diff: true,
            ..TabVisibility::chrome_only()
        });
        assert_eq!(with_diff.last().copied(), Some(OutputPanelTab::Diff));
        assert_eq!(with_diff.len(), 4);
    }

    #[test]
    fn split_out_panels_stay_when_mid_tab_changes() {
        assert!(keep_split_out_panel(true));
        assert!(!keep_split_out_panel(false));
    }

    #[test]
    fn breadcrumb_reuses_server_and_database_switch_targets() {
        let crumb = ConnectionBreadcrumb::from_names(Some("Prod"), Some("Reporting"));
        assert_eq!(crumb.chip_label(), "Prod · Reporting");
        assert_eq!(crumb.segment_label(BreadcrumbSegment::Server), "Prod");
        assert_eq!(
            crumb.segment_label(BreadcrumbSegment::Database),
            "Reporting"
        );
        assert_eq!(
            breadcrumb_switch_target(BreadcrumbSegment::Server),
            "server"
        );
        assert_eq!(
            breadcrumb_switch_target(BreadcrumbSegment::Database),
            "database"
        );
        let empty = ConnectionBreadcrumb::from_names(None, Some("  "));
        assert_eq!(empty.chip_label(), "Not connected");
        assert_eq!(empty.chip_label_with_state(false), "Not connected");
        assert!(!empty.has_database());
        let server_only = ConnectionBreadcrumb::from_names(Some("Prod"), None);
        assert_eq!(server_only.chip_label(), "Prod");
        assert_eq!(
            server_only.chip_label_with_state(false),
            "Prod — disconnected"
        );
        assert_eq!(server_only.chip_label_with_state(true), "choose database");
        assert!(!server_only.has_database());
        assert_eq!(server_only.segment_label(BreadcrumbSegment::Database), "");
        let named = ConnectionBreadcrumb::from_names(Some("test"), Some("db"));
        assert_eq!(named.chip_label_with_state(false), "test — disconnected");
        assert_eq!(named.chip_label_with_state(true), "test · db");
        assert_ne!(named.chip_label_with_state(true), "Database");
    }

    #[test]
    fn run_is_disabled_when_disconnected() {
        assert!(!run_enabled(false));
        assert!(run_enabled(true));
        assert_eq!(run_disabled_tooltip(), "Connect first");
    }

    #[test]
    fn close_and_reorder_helpers() {
        assert_eq!(close_others_indices(4, 1), vec![0, 2, 3]);
        assert_eq!(close_to_right_indices(4, 1), vec![2, 3]);
        assert_eq!(close_to_right_indices(4, 3), Vec::<usize>::new());
        assert_eq!(reorder_tab_index(0, 2, 3), Some((0, 2)));
        assert_eq!(reorder_tab_index(1, 1, 3), None);
        assert_eq!(reorder_tab_index(5, 0, 3), None);
        assert_eq!(remap_index_after_move(0, 0, 2), 2);
        assert_eq!(remap_index_after_move(1, 0, 2), 0);
        assert_eq!(remap_index_after_move(2, 0, 2), 1);
        let mut tabs = vec!["a", "b", "c"];
        assert_eq!(apply_tab_reorder(&mut tabs, 0, 2), Some(2));
        assert_eq!(tabs, vec!["b", "c", "a"]);
    }

    #[test]
    fn close_policy_closes_targeted_clean_unpinned() {
        assert_eq!(
            disconnect_tab_action(
                crate::preferences::DisconnectTabPolicy::CloseTabs,
                true,
                false,
                false
            ),
            DisconnectTabAction::Close
        );
    }

    #[test]
    fn close_policy_keeps_pinned_and_dirty() {
        assert_eq!(
            disconnect_tab_action(
                crate::preferences::DisconnectTabPolicy::CloseTabs,
                true,
                true,
                false
            ),
            DisconnectTabAction::Keep
        );
        assert_eq!(
            disconnect_tab_action(
                crate::preferences::DisconnectTabPolicy::CloseTabs,
                true,
                false,
                true
            ),
            DisconnectTabAction::Keep
        );
    }

    #[test]
    fn clear_policy_clears_targeted_only() {
        assert_eq!(
            disconnect_tab_action(
                crate::preferences::DisconnectTabPolicy::KeepTabsClearResults,
                true,
                false,
                false
            ),
            DisconnectTabAction::ClearResults
        );
        assert_eq!(
            disconnect_tab_action(
                crate::preferences::DisconnectTabPolicy::KeepTabsClearResults,
                false,
                false,
                false
            ),
            DisconnectTabAction::Keep
        );
    }

    #[test]
    fn keep_policy_never_touches() {
        assert_eq!(
            disconnect_tab_action(
                crate::preferences::DisconnectTabPolicy::KeepTabsKeepResults,
                true,
                false,
                false
            ),
            DisconnectTabAction::Keep
        );
    }

    #[test]
    fn untargeted_tab_is_always_keep() {
        assert_eq!(
            disconnect_tab_action(
                crate::preferences::DisconnectTabPolicy::CloseTabs,
                false,
                false,
                false
            ),
            DisconnectTabAction::Keep
        );
    }

    #[test]
    fn all_tabs_label_is_searchable() {
        let label = all_tabs_item_label("Orders", true, Some("prod"), true);
        assert!(label.contains("• "));
        assert!(label.contains("Orders"));
        assert!(label.contains("prod"));
        assert!(label.contains("running"));
        assert!(all_tabs_matches("ord", &label));
        assert!(all_tabs_matches("PROD", &label));
        assert!(all_tabs_matches("running", &label));
        assert!(!all_tabs_matches("staging", &label));
    }

    #[test]
    fn tab_announcement_and_auto_name_shape() {
        assert_eq!(
            tab_status_announcement(
                "Query 1",
                true,
                true,
                false,
                true,
                crate::file_watch::DiskState::Clean
            ),
            "Query 1, selected, unsaved, disconnected, running"
        );
        assert!(name_looks_auto("Query 3"));
        assert!(name_looks_auto(""));
        assert!(!name_looks_auto("Orders"));
        assert!(!name_looks_auto("Query bak"));
        assert!(!name_looks_auto("Query "));
    }

    #[test]
    fn automatic_titles_are_recognised_in_every_language() {
        // English is active, yet a Spanish title saved before a switch is
        // still an automatic one (and vice versa once Spanish is active).
        assert_eq!(auto_query_number("Query 3"), Some(3));
        assert_eq!(auto_query_number("Consulta 12"), Some(12));
        assert_eq!(auto_query_number("Consulta"), None);
        assert_eq!(auto_query_number("Orders"), None);
        assert!(name_looks_auto("Consulta 4"));
        assert_eq!(auto_title_number("Notebook 7"), Some(7));
        assert_eq!(auto_title_number("Cuaderno 8"), Some(8));
        assert_eq!(auto_title_number("Cuaderno de ventas"), None);
        crate::localization::activate(crate::localization::Language::Spanish, None).unwrap();
        let spanish = (
            auto_query_number("Query 3"),
            auto_query_number("Consulta 5"),
        );
        crate::localization::activate(crate::localization::Language::English, None).unwrap();
        assert_eq!(spanish, (Some(3), Some(5)));
    }

    #[test]
    fn automatic_query_titles_follow_the_active_language() {
        crate::localization::activate(crate::localization::Language::Spanish, None).unwrap();
        let to_spanish = relocalized_auto_title("Query 2", Some(2), true, false, false);
        let unchanged = relocalized_auto_title("Consulta 2", Some(2), true, false, false);
        crate::localization::activate(crate::localization::Language::English, None).unwrap();
        assert_eq!(to_spanish.as_deref(), Some("Consulta 2"));
        assert_eq!(unchanged, None);
        assert_eq!(
            relocalized_auto_title("Consulta 2", Some(2), true, false, false).as_deref(),
            Some("Query 2")
        );
        // Renamed, file-backed, SQL-named, or stale-number tabs keep their name.
        assert_eq!(
            relocalized_auto_title("Consulta 2", Some(2), false, true, false),
            None
        );
        assert_eq!(
            relocalized_auto_title("Consulta 2", Some(2), true, false, true),
            None
        );
        assert_eq!(
            relocalized_auto_title("Orders", Some(2), true, false, false),
            None
        );
        assert_eq!(
            relocalized_auto_title("Consulta 3", Some(2), true, false, false),
            None
        );
        assert_eq!(
            relocalized_auto_title("Consulta 2", None, true, false, false),
            None
        );
    }

    #[test]
    fn automatic_notebook_titles_follow_the_active_language() {
        let english = auto_notebook_title(3);
        crate::localization::activate(crate::localization::Language::Spanish, None).unwrap();
        let spanish = auto_notebook_title(3);
        let to_spanish = relocalized_notebook_title(&english, Some(3), true, false, false);
        let unchanged = relocalized_notebook_title(&spanish, Some(3), true, false, false);
        crate::localization::activate(crate::localization::Language::English, None).unwrap();
        assert_ne!(english, spanish);
        assert_eq!(to_spanish.as_deref(), Some(spanish.as_str()));
        assert_eq!(unchanged, None);
        assert_eq!(
            relocalized_notebook_title(&spanish, Some(3), true, false, false).as_deref(),
            Some(english.as_str())
        );
        // Renamed, saved, or stale-number notebooks keep their name.
        assert_eq!(
            relocalized_notebook_title(&spanish, Some(3), false, true, false),
            None
        );
        assert_eq!(
            relocalized_notebook_title(&spanish, Some(3), true, false, true),
            None
        );
        assert_eq!(
            relocalized_notebook_title(&spanish, Some(4), true, false, false),
            None
        );
        assert_eq!(
            relocalized_notebook_title("analysis", Some(3), true, false, false),
            None
        );
        assert_eq!(auto_title_number(&spanish), Some(3));
    }

    #[test]
    fn restored_naming_prefers_the_persisted_marker_over_the_title() {
        // New sessions: the marker decides, whatever the title's language.
        assert_eq!(
            restored_auto_naming("Consulta 4", Some(true), Some(4), false, false),
            (true, Some(4))
        );
        assert_eq!(
            restored_auto_naming("Orders", Some(true), None, false, false),
            (true, None)
        );
        assert_eq!(
            restored_auto_naming("Query 4", Some(false), Some(4), false, false),
            (false, None)
        );
        // Legacy sessions (no marker): recognise either language's title.
        assert_eq!(
            restored_auto_naming("Consulta 4", None, None, false, false),
            (true, Some(4))
        );
        assert_eq!(
            restored_auto_naming("Query 9", None, None, false, false),
            (true, Some(9))
        );
        assert_eq!(
            restored_auto_naming("Orders", None, None, false, false),
            (false, None)
        );
        // User names and files are never automatic.
        assert_eq!(
            restored_auto_naming("Query 4", Some(true), Some(4), true, false),
            (false, None)
        );
        assert_eq!(
            restored_auto_naming("Query 4", None, None, false, true),
            (false, None)
        );
    }

    #[test]
    fn auto_name_uses_first_sql_object_or_query_n() {
        assert_eq!(
            auto_name_from_sql("SELECT TOP 100 * FROM sys.tables", 1),
            "sys.tables"
        );
        assert_eq!(
            auto_name_from_sql("INSERT INTO Orders (id) VALUES (1)", 2),
            "Orders"
        );
        assert_eq!(auto_name_from_sql("SELECT 1", 3), "Query 3");
        assert_eq!(auto_name_from_sql("   -- comment\n", 4), "Query 4");
        assert_eq!(
            auto_name_from_sql("/* x */ UPDATE dbo.Customers SET a = 1", 5),
            "dbo.Customers"
        );
    }

    #[test]
    fn rename_sets_user_named_and_blocks_auto_name() {
        let name = apply_explicit_rename("Query 1", "incident 4123").unwrap();
        assert_eq!(name, "incident 4123");
        assert!(!should_auto_name(false, true, false));
        assert!(should_auto_name(true, false, false));
        assert!(!should_auto_name(true, true, false));
        assert!(!should_auto_name(true, false, true));
    }

    #[test]
    fn rename_rejects_blank() {
        assert!(apply_explicit_rename("Query 1", "   ").is_none());
        assert!(apply_explicit_rename("Query 1", "").is_none());
        assert!(apply_explicit_rename("Query 1", "Query 1").is_none());
        assert!(apply_explicit_rename("incident 4123", " incident 4123 ").is_none());
    }

    #[test]
    fn inactive_tab_shows_dim_server_cue() {
        assert_eq!(
            inactive_tab_connection_cue(Some("prod-sql")),
            Some("prod-sql".into())
        );
        assert_eq!(inactive_tab_connection_cue(Some("  ")), None);
        assert_eq!(inactive_tab_connection_cue(None), None);
    }

    #[test]
    fn tab_bar_labels_match_strip_copy() {
        let labels = build_tab_bar_labels(
            "Orders",
            0,
            true,
            true,
            false,
            true,
            crate::file_watch::DiskState::Clean,
        );
        assert_eq!(labels.title.as_ref(), "Orders");
        assert_eq!(labels.close_label.as_ref(), "Close Orders");
        assert_eq!(
            labels.tooltip.as_ref(),
            format!(
                "Orders — {}",
                tab_status_announcement(
                    "Orders",
                    true,
                    true,
                    false,
                    true,
                    crate::file_watch::DiskState::Clean
                )
            )
        );
        let empty = build_tab_bar_labels(
            "  ",
            2,
            false,
            false,
            true,
            false,
            crate::file_watch::DiskState::Clean,
        );
        assert_eq!(empty.title.as_ref(), "Query 3");
        assert_eq!(empty.close_label.as_ref(), "Close Query 3");
    }

    #[test]
    fn labels_flag_running() {
        let running = build_tab_bar_labels(
            "Orders",
            0,
            false,
            false,
            true,
            true,
            crate::file_watch::DiskState::Clean,
        );
        assert!(running.running, "running flag must reach the strip paint");
        assert!(
            running.tooltip.as_ref().contains("running"),
            "tooltip={}",
            running.tooltip.as_ref()
        );
        let idle = build_tab_bar_labels(
            "Orders",
            0,
            false,
            false,
            true,
            false,
            crate::file_watch::DiskState::Clean,
        );
        assert!(!idle.running);
    }

    #[test]
    fn labels_mention_modified_on_disk() {
        let labels = build_tab_bar_labels(
            "Orders",
            0,
            true,
            false,
            true,
            false,
            crate::file_watch::DiskState::ModifiedOnDisk,
        );
        assert!(
            labels.tooltip.as_ref().contains("modified on disk"),
            "tooltip={}",
            labels.tooltip.as_ref()
        );
        assert!(
            tab_status_announcement(
                "Orders",
                true,
                false,
                true,
                false,
                crate::file_watch::DiskState::ModifiedOnDisk
            )
            .contains("modified on disk")
        );
    }

    #[test]
    fn labels_mention_deleted_on_disk() {
        let labels = build_tab_bar_labels(
            "Orders",
            0,
            false,
            true,
            false,
            false,
            crate::file_watch::DiskState::DeletedOnDisk,
        );
        assert!(
            labels.tooltip.as_ref().contains("deleted on disk"),
            "tooltip={}",
            labels.tooltip.as_ref()
        );
        assert!(
            tab_status_announcement(
                "Orders",
                false,
                true,
                false,
                false,
                crate::file_watch::DiskState::DeletedOnDisk
            )
            .contains("deleted on disk")
        );
    }

    #[test]
    fn tab_strip_hairline_stays_under_segmented_tabs() {
        assert!(tab_strip_hairline_visible(true));
        assert!(tab_strip_hairline_visible(false));
        assert!(dirty_dot_is_neutral());
    }

    #[test]
    fn connection_dot_follows_profile_health() {
        assert_eq!(
            connection_dot_kind(false, true),
            ConnectionDotKind::Disconnected
        );
        assert_eq!(
            connection_dot_kind(true, true),
            ConnectionDotKind::Connected
        );
        assert_eq!(connection_dot_kind(true, false), ConnectionDotKind::Failed);
    }
}
