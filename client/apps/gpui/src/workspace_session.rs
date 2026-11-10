//! Workspace session persistence — port of `WorkspaceSession.swift`.
//!
//! Pure, serde-serializable. Represents a saved workspace layout (tabs + pane
//! visibility + sidebar/AI widths) that can round-trip through JSON with
//! fault-tolerant decoding. Named workspaces use an atomic manifest plus one
//! isolated snapshot per workspace, so a damaged snapshot cannot hide healthy
//! workspaces.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceError {
    pub key: &'static str,
    pub args: Vec<(&'static str, String)>,
}

impl WorkspaceError {
    fn new(key: &'static str) -> Self {
        Self {
            key,
            args: Vec::new(),
        }
    }

    fn with_maximum(key: &'static str, maximum: usize) -> Self {
        Self {
            key,
            args: vec![("maximum", maximum.to_string())],
        }
    }

    pub fn resolve(&self) -> String {
        let args = self
            .args
            .iter()
            .map(|(name, value)| (*name, value.as_str()))
            .collect::<Vec<_>>();
        crate::localization::format(self.key, &args)
    }

    fn into_io(self, kind: std::io::ErrorKind) -> std::io::Error {
        std::io::Error::new(kind, self)
    }
}

impl std::fmt::Display for WorkspaceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.resolve())
    }
}

impl std::error::Error for WorkspaceError {}

pub const CURRENT_VERSION: u32 = 2;
pub const MIN_AI_ASSISTANT_WIDTH: f32 = 280.0;
pub const MAX_AI_ASSISTANT_WIDTH: f32 = 1000.0;
pub const DEFAULT_SIDEBAR_WIDTH: f32 = 260.0;
pub const DEFAULT_RESULTS_HEIGHT: f32 = 300.0;
pub const MIN_RESULTS_HEIGHT: f32 = 120.0;
pub const MAX_RESULTS_HEIGHT: f32 = 1200.0;
pub const MIN_SIDEBAR_WIDTH: f32 = 40.0;
pub const MAX_SIDEBAR_WIDTH: f32 = 1200.0;
pub const DEFAULT_DDL_PANEL_WIDTH: f32 = 380.0;
pub const MIN_DDL_PANEL_WIDTH: f32 = 240.0;
pub const MAX_DDL_PANEL_WIDTH: f32 = 900.0;
pub const DEFAULT_HISTORY_PANEL_WIDTH: f32 = 300.0;
pub const MIN_HISTORY_PANEL_WIDTH: f32 = 240.0;
pub const MAX_HISTORY_PANEL_WIDTH: f32 = 600.0;
pub const DEFAULT_DIAGNOSTICS_HEIGHT: f32 = 160.0;
pub const MIN_DIAGNOSTICS_HEIGHT: f32 = 80.0;
pub const MAX_DIAGNOSTICS_HEIGHT: f32 = 480.0;
pub const DEFAULT_CELL_DETAIL_WIDTH: f32 = 360.0;
pub const MIN_CELL_DETAIL_WIDTH: f32 = 220.0;
pub const MAX_CELL_DETAIL_WIDTH: f32 = 900.0;
pub const DEFAULT_CELL_DETAIL_HEIGHT: f32 = 220.0;
pub const MIN_CELL_DETAIL_HEIGHT: f32 = 120.0;
pub const MAX_CELL_DETAIL_HEIGHT: f32 = 600.0;
fn default_cell_detail_width() -> f32 {
    DEFAULT_CELL_DETAIL_WIDTH
}
fn default_cell_detail_height() -> f32 {
    DEFAULT_CELL_DETAIL_HEIGHT
}
pub const MAX_NAMED_WORKSPACES: usize = 50;
pub const MAX_WORKSPACE_NAME_CHARS: usize = 80;
const NAMED_WORKSPACE_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SplitMode {
    #[default]
    None,
    Vertical,
    Horizontal,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TabState {
    pub title: String,
    pub split_mode: SplitMode,
    /// File path for file-backed tabs, so restored tabs can find their
    /// metadata record (keyed by file path). `None` for unsaved tabs.
    /// Backward-compatible: old sessions without this field deserialize
    /// to `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    /// Editor text for unsaved tabs, so content survives app restart.
    /// File-backed tabs re-read from disk on restore, so this is only
    /// populated for unsaved tabs. Backward-compatible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// UUID (as a string) of the connection this tab is pinned to, so each
    /// restored tab reconnects to its own server. Backward-compatible:
    /// absent in old sessions → `None` (tab follows the active connection).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_id: Option<String>,
    /// Database selected for this tab's pinned connection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    /// Remember each tab's object-definition panel width even when the panel
    /// itself is not open at shutdown.
    #[serde(default = "default_ddl_panel_width")]
    pub ddl_panel_width: f32,
    /// Remember each tab's query-history panel width without persisting any
    /// query results or history-store internals.
    #[serde(default = "default_history_panel_width")]
    pub history_panel_width: f32,
    /// Pinned tabs sort first and are skipped by bulk close verbs.
    /// Backward-compatible: absent in old sessions → unpinned.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pinned: bool,
    /// Set by Rename Tab…; auto-naming never overwrites a user name.
    /// Backward-compatible: absent in old sessions → false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub user_named: bool,
    /// Whether the tab still carries an automatic name (`Query N` or one
    /// derived from its SQL). Language-neutral, so restoring never depends on
    /// recognising translated title text. Backward-compatible: absent in old
    /// sessions → `None`, and the title is inspected instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_named: Option<bool>,
    /// The `N` of an automatic `Query N` title; the title itself is rebuilt
    /// in the current language on restore. Absent → not a numbered title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_number: Option<u32>,
    /// Key of this tab's cached last result under the result cache; only the
    /// key is persisted here, never rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_cache_key: Option<String>,
    /// On-disk path of a saved SQL notebook (`.sqlnb.json`). Unsaved notebooks
    /// are not persisted. Backward-compatible: absent → `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notebook_path: Option<String>,
    /// Values entered in the tab's `-- sqlly param:` strip, restored onto
    /// the parameters the buffer still declares with the same name and type.
    /// Backward-compatible: absent in old sessions → none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub query_params: Vec<SavedParamValue>,
}

/// Most parameter values kept per tab, and the longest value kept, so a
/// hand-edited or damaged session cannot balloon the restored strip.
pub const MAX_SAVED_PARAMS_PER_TAB: usize = 64;
pub const MAX_SAVED_PARAM_VALUE_CHARS: usize = 16 * 1024;

/// One query-parameter value saved with a tab. The type is its directive
/// label (`int`, `date`, …) kept as text, so a type this build does not know
/// drops just that value instead of failing the whole session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedParamValue {
    /// Includes the leading `@`.
    pub name: String,
    #[serde(rename = "type")]
    pub ty: String,
    pub value: String,
}

fn default_ddl_panel_width() -> f32 {
    DEFAULT_DDL_PANEL_WIDTH
}

fn default_history_panel_width() -> f32 {
    DEFAULT_HISTORY_PANEL_WIDTH
}

/// Editor split persisted with the layout. Stage 1 (two tab groups): the
/// split remembers which tab the unfocused pane shows, which side the focused
/// tab occupies, and the leading pane's size in logical pixels. Stage 2
/// (same-buffer two views): `same_buffer` is true, `other_tab` is meaningless
/// (written as 0), and the split re-attaches to the selected tab on restore.
/// Backward-compatible: sessions written before this struct existed simply
/// restore as a single pane, and sessions written before `same_buffer`
/// existed decode it to `false` (a two-tab split, exactly as before).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EditorSplit {
    pub axis: SplitMode,
    /// Tab index (into the persisted tab list) shown in the unfocused pane.
    /// Ignored when `same_buffer` is true.
    pub other_tab: usize,
    /// True when the focused tab occupies the leading (left/top) pane. For a
    /// same-buffer split: true when the leading (canonical) view is focused.
    pub active_first: bool,
    /// Leading-pane size in logical pixels — width for a vertical split,
    /// height for a horizontal one.
    pub first_size: f32,
    /// True for a same-buffer split (both panes show the selected tab through
    /// two views). Backward-compatible: absent in old sessions → `false`.
    #[serde(default)]
    pub same_buffer: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LayoutState {
    pub object_explorer_visible: bool,
    pub results_pane_visible: bool,
    /// Three-state results pane. Absent in old sessions → derived from
    /// [`Self::results_pane_visible`] via [`Self::effective_results_pane`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub results_pane: Option<crate::results_pane_state::ResultsPaneState>,
    pub sidebar_width: f32,
    /// Height of the shared results pane in logical pixels.
    pub results_height: f32,
    pub ai_assistant_visible: bool,
    pub ai_assistant_width: f32,
    pub ai_assistant_section: Option<String>,
    /// Editor text-zoom multiplier (1.0 == 100%). Backward-compatible: sessions
    /// written before this field existed decode to `0.0`, which `sanitized()`
    /// normalizes to the default zoom.
    #[serde(default)]
    pub editor_zoom: f32,
    /// UUID (as a string) of the connection selected in the query-bar server
    /// dropdown when the session was saved. Restored and reactivated on launch
    /// so the schema tree and IntelliSense model load without a manual metadata
    /// refresh. Backward-compatible: absent in old sessions → `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_connection_id: Option<String>,
    /// Name of the database selected in the query-bar database dropdown when the
    /// session was saved. Restored on launch alongside `active_connection_id`.
    /// Backward-compatible: absent in old sessions → `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_database: Option<String>,
    /// Project root the "Open Project File" picker last used, so the picker
    /// reopens to the same folder after an app restart. Backward-compatible:
    /// absent in old sessions → `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_file_picker_root: Option<String>,
    /// Which sidebar body was showing (`"explorer"` / `"library"`). Backward-compatible:
    /// absent in old sessions → `None` (explorer).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sidebar_panel: Option<String>,
    /// Diagnostic log height in pixels. Backward-compatible: absent / `0.0`
    /// in old sessions → [`DEFAULT_DIAGNOSTICS_HEIGHT`] via `sanitized()`.
    #[serde(default)]
    pub diagnostics_height: f32,
    /// Editor split restored on launch. Backward-compatible: absent in old
    /// sessions → `None` (single pane).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub editor_split: Option<EditorSplit>,
    /// Whether the Cell Details dock is open. Backward-compatible: absent in
    /// old sessions → hidden.
    #[serde(default)]
    pub cell_detail_visible: bool,
    /// Width of the right-side Cell Details dock in logical pixels.
    #[serde(default = "default_cell_detail_width")]
    pub cell_detail_width: f32,
    /// Height of the bottom Cell Details dock in logical pixels.
    #[serde(default = "default_cell_detail_height")]
    pub cell_detail_height: f32,
}

impl Default for LayoutState {
    /// First-launch layout (no saved session): server explorer open at
    /// [`DEFAULT_SIDEBAR_WIDTH`]. Saved sessions always carry these fields
    /// explicitly, so this only applies when no preference exists yet.
    fn default() -> Self {
        Self {
            object_explorer_visible: true,
            results_pane_visible: false,
            results_pane: None,
            sidebar_width: DEFAULT_SIDEBAR_WIDTH,
            results_height: DEFAULT_RESULTS_HEIGHT,
            ai_assistant_visible: false,
            ai_assistant_width: 0.0,
            ai_assistant_section: None,
            editor_zoom: 0.0,
            active_connection_id: None,
            active_database: None,
            project_file_picker_root: None,
            sidebar_panel: None,
            diagnostics_height: 0.0,
            editor_split: None,
            cell_detail_visible: false,
            cell_detail_width: DEFAULT_CELL_DETAIL_WIDTH,
            cell_detail_height: DEFAULT_CELL_DETAIL_HEIGHT,
        }
    }
}

impl LayoutState {
    /// Prefer the three-state field; fall back to the legacy bool for sessions
    /// written before [`Self::results_pane`] existed.
    pub fn effective_results_pane(&self) -> crate::results_pane_state::ResultsPaneState {
        self.results_pane.unwrap_or_else(|| {
            crate::results_pane_state::ResultsPaneState::from_legacy_visible(
                self.results_pane_visible,
            )
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct WorkspaceSession {
    pub version: u32,
    pub tabs: Vec<TabState>,
    pub selected_tab: usize,
    pub layout: LayoutState,
}

impl WorkspaceSession {
    pub fn new() -> Self {
        Self {
            version: CURRENT_VERSION,
            ..Default::default()
        }
    }

    /// Clamps/sanitizes fields that could be out of bounds from a corrupted
    /// or hand-edited JSON file.
    pub fn sanitized(mut self) -> Self {
        self.version = CURRENT_VERSION;
        for tab in &mut self.tabs {
            tab.title = truncate_chars(tab.title.trim(), 200);
            if tab.title.is_empty() {
                tab.title = "Query".to_string();
            }
            tab.file_path = tab
                .file_path
                .take()
                .map(|path| truncate_chars(&path, 4096))
                .filter(|path| !path.is_empty());
            tab.connection_id = tab
                .connection_id
                .take()
                .map(|id| truncate_chars(id.trim(), 128))
                .filter(|id| !id.is_empty());
            tab.database = tab
                .database
                .take()
                .map(|database| truncate_chars(database.trim(), 512))
                .filter(|database| !database.is_empty());
            tab.ddl_panel_width = finite_clamp(
                tab.ddl_panel_width,
                DEFAULT_DDL_PANEL_WIDTH,
                MIN_DDL_PANEL_WIDTH,
                MAX_DDL_PANEL_WIDTH,
            );
            tab.history_panel_width = finite_clamp(
                tab.history_panel_width,
                DEFAULT_HISTORY_PANEL_WIDTH,
                MIN_HISTORY_PANEL_WIDTH,
                MAX_HISTORY_PANEL_WIDTH,
            );
            tab.query_params
                .retain(|param| param.name.starts_with('@') && !param.value.is_empty());
            tab.query_params.truncate(MAX_SAVED_PARAMS_PER_TAB);
            for param in &mut tab.query_params {
                param.name = truncate_chars(&param.name, 128);
                param.ty = truncate_chars(&param.ty, 32);
                param.value = truncate_chars(&param.value, MAX_SAVED_PARAM_VALUE_CHARS);
            }
        }
        if self.tabs.is_empty() {
            self.tabs.push(TabState {
                title: "Query 1".to_string(),
                split_mode: SplitMode::None,
                file_path: None,
                content: None,
                connection_id: None,
                database: None,
                ddl_panel_width: DEFAULT_DDL_PANEL_WIDTH,
                history_panel_width: DEFAULT_HISTORY_PANEL_WIDTH,
                pinned: false,
                user_named: false,
                ..Default::default()
            });
        }
        if self.selected_tab >= self.tabs.len() {
            self.selected_tab = 0;
        }
        self.layout.ai_assistant_width = finite_clamp(
            self.layout.ai_assistant_width,
            MIN_AI_ASSISTANT_WIDTH,
            MIN_AI_ASSISTANT_WIDTH,
            MAX_AI_ASSISTANT_WIDTH,
        );
        // A non-positive sidebar width is never a real preference — it's the
        // pre-default-width era's 0.0 or a corrupted value. Restore the default
        // so the explorer isn't invisible when expanded.
        self.layout.sidebar_width = if self.layout.sidebar_width <= 0.0 {
            DEFAULT_SIDEBAR_WIDTH
        } else {
            finite_clamp(
                self.layout.sidebar_width,
                DEFAULT_SIDEBAR_WIDTH,
                MIN_SIDEBAR_WIDTH,
                MAX_SIDEBAR_WIDTH,
            )
        };
        self.layout.results_height = finite_clamp(
            self.layout.results_height,
            DEFAULT_RESULTS_HEIGHT,
            MIN_RESULTS_HEIGHT,
            MAX_RESULTS_HEIGHT,
        );
        // Normalize the editor zoom: legacy/absent value (0.0) → default, and
        // any out-of-range value is clamped into the supported band.
        self.layout.editor_zoom = if self.layout.editor_zoom <= 0.0 {
            crate::editor_zoom::DEFAULT_ZOOM
        } else {
            crate::editor_zoom::clamp_zoom(self.layout.editor_zoom)
        };
        self.layout.diagnostics_height = if self.layout.diagnostics_height <= 0.0 {
            DEFAULT_DIAGNOSTICS_HEIGHT
        } else {
            self.layout
                .diagnostics_height
                .clamp(MIN_DIAGNOSTICS_HEIGHT, MAX_DIAGNOSTICS_HEIGHT)
        };
        // An editor split only survives when its indices are coherent for the
        // restored tab list; anything else restores as a single pane. A
        // same-buffer split carries no unfocused-tab index — it re-attaches
        // to the (already-clamped) selected tab, so only the axis matters.
        if let Some(split) = &mut self.layout.editor_split {
            split.first_size = finite_clamp(split.first_size, 400.0, 120.0, 4000.0);
            let valid = split.axis != SplitMode::None
                && (split.same_buffer
                    || (split.other_tab < self.tabs.len() && split.other_tab != self.selected_tab));
            if !valid {
                self.layout.editor_split = None;
            }
        }
        self.layout.cell_detail_width = finite_clamp(
            self.layout.cell_detail_width,
            DEFAULT_CELL_DETAIL_WIDTH,
            MIN_CELL_DETAIL_WIDTH,
            MAX_CELL_DETAIL_WIDTH,
        );
        self.layout.cell_detail_height = finite_clamp(
            self.layout.cell_detail_height,
            DEFAULT_CELL_DETAIL_HEIGHT,
            MIN_CELL_DETAIL_HEIGHT,
            MAX_CELL_DETAIL_HEIGHT,
        );
        self
    }
}

fn finite_clamp(value: f32, default: f32, min: f32, max: f32) -> f32 {
    if value.is_finite() {
        value.clamp(min, max)
    } else {
        default
    }
}

fn truncate_chars(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}

/// Fault-tolerant decode: falls back to `default()` (then `sanitized()`) on any
/// decode error, missing fields, or invalid values.
#[cfg(any(test, target_arch = "wasm32"))]
pub fn decode_session(json: &str) -> WorkspaceSession {
    match serde_json::from_str::<WorkspaceSession>(json) {
        Ok(s) => s.sanitized(),
        Err(_) => WorkspaceSession::new().sanitized(),
    }
}

/// Encodes a session to pretty JSON.
pub fn encode_session(session: &WorkspaceSession) -> String {
    serde_json::to_string_pretty(&session.clone().sanitized()).unwrap_or_else(|_| "{}".to_string())
}

/// Load from a file path (fault-tolerant → default).
/// Browser build: reads localStorage instead of the filesystem.
pub fn load_session(path: &std::path::Path) -> WorkspaceSession {
    #[cfg(target_arch = "wasm32")]
    return {
        let _ = path;
        match crate::host_storage::load_text(
            crate::host_storage::StorageArea::Data,
            "workspace-session.json",
        ) {
            Some(content) => decode_session(&content),
            None => WorkspaceSession::new().sanitized(),
        }
    };
    #[cfg(not(target_arch = "wasm32"))]
    load_session_preserving(path).0
}

/// Like [`load_session`], but a session file that exists and cannot be read
/// or parsed is moved aside (and its new path returned) rather than left for
/// the next save to overwrite — it holds the user's unsaved query text.
#[cfg(not(target_arch = "wasm32"))]
pub fn load_session_preserving(
    path: &std::path::Path,
) -> (WorkspaceSession, Option<std::path::PathBuf>) {
    let parsed = match std::fs::read(path) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return (WorkspaceSession::new().sanitized(), None);
        }
        Err(err) => Err(err.to_string()),
        Ok(bytes) => serde_json::from_slice::<WorkspaceSession>(&bytes).map_err(|e| e.to_string()),
    };
    match parsed {
        Ok(session) => (session.sanitized(), None),
        Err(detail) => {
            let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
            let kept = path.with_file_name(format!("workspace-session.unreadable-{stamp}.json"));
            let preserved = match std::fs::rename(path, &kept) {
                Ok(()) => Some(kept),
                Err(err) => {
                    tracing::warn!("could not move unreadable session file aside: {err}");
                    None
                }
            };
            tracing::warn!(
                "workspace session at {} is unreadable: {detail}",
                path.display()
            );
            (WorkspaceSession::new().sanitized(), preserved)
        }
    }
}

/// Save to a file path (atomic via `.tmp` rename).
/// Browser build: writes localStorage instead of the filesystem.
pub fn save_session(session: &WorkspaceSession, path: &std::path::Path) -> std::io::Result<()> {
    let json = encode_session(session);
    #[cfg(target_arch = "wasm32")]
    return {
        let _ = path;
        crate::host_storage::store_text(
            crate::host_storage::StorageArea::Data,
            "workspace-session.json",
            &json,
        )
        .map_err(std::io::Error::other)
    };
    #[cfg(not(target_arch = "wasm32"))]
    {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        crate::fs_safe::write_atomic(path, json.as_bytes())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamedWorkspaceMetadata {
    pub id: String,
    pub name: String,
    pub updated_at_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NamedWorkspaceIndex {
    pub version: u32,
    pub active_workspace_id: Option<String>,
    pub workspaces: Vec<NamedWorkspaceMetadata>,
}

impl Default for NamedWorkspaceIndex {
    fn default() -> Self {
        Self {
            version: NAMED_WORKSPACE_VERSION,
            active_workspace_id: None,
            workspaces: Vec::new(),
        }
    }
}

impl NamedWorkspaceIndex {
    fn sanitized(mut self) -> Self {
        self.version = NAMED_WORKSPACE_VERSION;
        let mut ids = HashSet::new();
        self.workspaces.retain_mut(|workspace| {
            let Ok(id) = uuid::Uuid::parse_str(&workspace.id) else {
                return false;
            };
            workspace.id = id.to_string();
            workspace.name = truncate_chars(workspace.name.trim(), MAX_WORKSPACE_NAME_CHARS);
            !workspace.name.is_empty() && ids.insert(workspace.id.clone())
        });
        self.workspaces.truncate(MAX_NAMED_WORKSPACES);
        if self
            .active_workspace_id
            .as_ref()
            .is_some_and(|active| !self.workspaces.iter().any(|item| &item.id == active))
        {
            self.active_workspace_id = None;
        }
        self
    }
}

/// Filesystem-backed named workspace collection. The manifest contains only
/// safe UUID ids, display names, and timestamps; each session is isolated in
/// its own JSON file beneath the store root.
#[derive(Debug, Clone)]
pub struct NamedWorkspaceStore {
    root: std::path::PathBuf,
}

impl NamedWorkspaceStore {
    pub fn new(root: std::path::PathBuf) -> Self {
        Self { root }
    }

    pub fn at_default() -> Self {
        let mut root = session_file();
        root.set_file_name("workspaces");
        Self::new(root)
    }

    fn manifest_path(&self) -> std::path::PathBuf {
        self.root.join("index.json")
    }

    fn snapshot_path(&self, id: &str) -> Option<std::path::PathBuf> {
        uuid::Uuid::parse_str(id)
            .ok()
            .map(|id| self.root.join(format!("{id}.json")))
    }

    pub fn load_index(&self) -> NamedWorkspaceIndex {
        #[cfg(target_arch = "wasm32")]
        let content = crate::host_storage::load_text(
            crate::host_storage::StorageArea::Data,
            "named-workspaces/index.json",
        );
        #[cfg(not(target_arch = "wasm32"))]
        let content = std::fs::read_to_string(self.manifest_path()).ok();
        content
            .and_then(|json| serde_json::from_str::<NamedWorkspaceIndex>(&json).ok())
            .unwrap_or_default()
            .sanitized()
    }

    pub fn load(&self, id: &str) -> Option<WorkspaceSession> {
        let path = self.snapshot_path(id)?;
        #[cfg(target_arch = "wasm32")]
        let content = {
            let _ = path;
            crate::host_storage::load_text(
                crate::host_storage::StorageArea::Data,
                &format!("named-workspaces/{id}.json"),
            )
        };
        #[cfg(not(target_arch = "wasm32"))]
        let content = std::fs::read_to_string(path).ok();
        content
            .and_then(|json| serde_json::from_str::<WorkspaceSession>(&json).ok())
            .map(WorkspaceSession::sanitized)
    }

    pub fn save(
        &self,
        index: &mut NamedWorkspaceIndex,
        id: Option<&str>,
        name: &str,
        session: &WorkspaceSession,
        now_unix_ms: i64,
    ) -> std::io::Result<String> {
        let name = valid_workspace_name(name)?;
        let id = match id {
            Some(id) => uuid::Uuid::parse_str(id)
                .map_err(|_| {
                    WorkspaceError::new("workspace.store.invalid_id")
                        .into_io(std::io::ErrorKind::InvalidInput)
                })?
                .to_string(),
            None => uuid::Uuid::new_v4().to_string(),
        };
        if !index.workspaces.iter().any(|item| item.id == id)
            && index.workspaces.len() >= MAX_NAMED_WORKSPACES
        {
            return Err(WorkspaceError::with_maximum(
                "workspace.store.capacity",
                MAX_NAMED_WORKSPACES,
            )
            .into_io(std::io::ErrorKind::InvalidInput));
        }
        let snapshot = encode_session(session);
        self.write_snapshot(&id, snapshot.as_bytes())?;
        if let Some(item) = index.workspaces.iter_mut().find(|item| item.id == id) {
            item.name = name;
            item.updated_at_unix_ms = now_unix_ms;
        } else {
            index.workspaces.push(NamedWorkspaceMetadata {
                id: id.clone(),
                name,
                updated_at_unix_ms: now_unix_ms,
            });
        }
        index.active_workspace_id = Some(id.clone());
        self.save_index(index)?;
        Ok(id)
    }

    pub fn rename(
        &self,
        index: &mut NamedWorkspaceIndex,
        id: &str,
        name: &str,
        now_unix_ms: i64,
    ) -> std::io::Result<()> {
        let name = valid_workspace_name(name)?;
        let item = index
            .workspaces
            .iter_mut()
            .find(|item| item.id == id)
            .ok_or_else(|| {
                WorkspaceError::new("workspace.store.not_found")
                    .into_io(std::io::ErrorKind::NotFound)
            })?;
        item.name = name;
        item.updated_at_unix_ms = now_unix_ms;
        self.save_index(index)
    }

    pub fn duplicate(
        &self,
        index: &mut NamedWorkspaceIndex,
        id: &str,
        name: &str,
        now_unix_ms: i64,
    ) -> std::io::Result<String> {
        let session = self.load(id).ok_or_else(|| {
            WorkspaceError::new("workspace.store.snapshot_corrupt")
                .into_io(std::io::ErrorKind::InvalidData)
        })?;
        let previous_active = index.active_workspace_id.clone();
        let id = self.save(index, None, name, &session, now_unix_ms)?;
        index.active_workspace_id = previous_active;
        self.save_index(index)?;
        Ok(id)
    }

    pub fn delete(&self, index: &mut NamedWorkspaceIndex, id: &str) -> std::io::Result<()> {
        let before = index.workspaces.len();
        index.workspaces.retain(|item| item.id != id);
        if index.workspaces.len() == before {
            return Err(WorkspaceError::new("workspace.store.not_found")
                .into_io(std::io::ErrorKind::NotFound));
        }
        if index.active_workspace_id.as_deref() == Some(id) {
            index.active_workspace_id = None;
        }
        self.save_index(index)?;
        self.remove_snapshot(id)
    }

    pub fn set_active(
        &self,
        index: &mut NamedWorkspaceIndex,
        id: Option<&str>,
    ) -> std::io::Result<()> {
        if id.is_some_and(|id| !index.workspaces.iter().any(|item| item.id == id)) {
            return Err(WorkspaceError::new("workspace.store.not_found")
                .into_io(std::io::ErrorKind::NotFound));
        }
        index.active_workspace_id = id.map(str::to_string);
        self.save_index(index)
    }

    fn save_index(&self, index: &mut NamedWorkspaceIndex) -> std::io::Result<()> {
        *index = index.clone().sanitized();
        let json = serde_json::to_string_pretty(index).map_err(std::io::Error::other)?;
        #[cfg(target_arch = "wasm32")]
        return crate::host_storage::store_text(
            crate::host_storage::StorageArea::Data,
            "named-workspaces/index.json",
            &json,
        )
        .map_err(std::io::Error::other);
        #[cfg(not(target_arch = "wasm32"))]
        crate::fs_safe::write_atomic(&self.manifest_path(), json.as_bytes())
    }

    fn write_snapshot(&self, id: &str, bytes: &[u8]) -> std::io::Result<()> {
        let path = self.snapshot_path(id).ok_or_else(|| {
            WorkspaceError::new("workspace.store.invalid_id")
                .into_io(std::io::ErrorKind::InvalidInput)
        })?;
        #[cfg(target_arch = "wasm32")]
        return {
            let _ = path;
            let json = std::str::from_utf8(bytes).map_err(std::io::Error::other)?;
            crate::host_storage::store_text(
                crate::host_storage::StorageArea::Data,
                &format!("named-workspaces/{id}.json"),
                json,
            )
            .map_err(std::io::Error::other)
        };
        #[cfg(not(target_arch = "wasm32"))]
        crate::fs_safe::write_atomic(&path, bytes)
    }

    fn remove_snapshot(&self, id: &str) -> std::io::Result<()> {
        let path = self.snapshot_path(id).ok_or_else(|| {
            WorkspaceError::new("workspace.store.invalid_id")
                .into_io(std::io::ErrorKind::InvalidInput)
        })?;
        #[cfg(target_arch = "wasm32")]
        {
            let _ = path;
            crate::host_storage::remove(
                crate::host_storage::StorageArea::Data,
                &format!("named-workspaces/{id}.json"),
            );
            Ok(())
        }
        #[cfg(not(target_arch = "wasm32"))]
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
}

fn valid_workspace_name(name: &str) -> std::io::Result<String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(WorkspaceError::new("workspace.store.name_empty")
            .into_io(std::io::ErrorKind::InvalidInput));
    }
    if name.chars().count() > MAX_WORKSPACE_NAME_CHARS {
        return Err(WorkspaceError::with_maximum(
            "workspace.store.name_too_long",
            MAX_WORKSPACE_NAME_CHARS,
        )
        .into_io(std::io::ErrorKind::InvalidInput));
    }
    Ok(name.to_string())
}

/// Default session file path: `<data_dir>/SQLLY/workspace-session.json`.
/// Mirrors the Swift `WorkspaceSession.storeURL`.
pub fn session_file() -> std::path::PathBuf {
    #[cfg(not(target_arch = "wasm32"))]
    let mut p = dirs::data_dir().unwrap_or_else(std::env::temp_dir);
    // Browser build: nominal path only — load/save above use localStorage.
    #[cfg(target_arch = "wasm32")]
    let mut p = std::path::PathBuf::new();
    p.push("SQLLY");
    p.push("workspace-session.json");
    p
}

/// Apply the launch restore policy: returns the tabs to recreate and the
/// selected index remapped into the filtered list (clamped; 0 when empty).
pub fn filter_tabs_for_restore(
    tabs: &[TabState],
    selected: usize,
    policy: crate::preferences::TabRestorePolicy,
) -> (Vec<TabState>, usize) {
    match policy {
        crate::preferences::TabRestorePolicy::All => {
            let sel = if tabs.is_empty() {
                0
            } else {
                selected.min(tabs.len() - 1)
            };
            (tabs.to_vec(), sel)
        }
        crate::preferences::TabRestorePolicy::PinnedOnly => {
            let selected_survived = tabs.get(selected).is_some_and(|tab| tab.pinned);
            let kept: Vec<TabState> = tabs.iter().filter(|tab| tab.pinned).cloned().collect();
            let new_selected = if selected_survived {
                tabs.iter().take(selected).filter(|tab| tab.pinned).count()
            } else {
                0
            };
            let sel = if kept.is_empty() {
                0
            } else {
                new_selected.min(kept.len() - 1)
            };
            (kept, sel)
        }
        crate::preferences::TabRestorePolicy::None => (Vec::new(), 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_version() {
        let s = WorkspaceSession::new();
        assert_eq!(s.version, CURRENT_VERSION);
    }

    fn restore_tab(title: &str, pinned: bool) -> TabState {
        TabState {
            title: title.into(),
            pinned,
            ..Default::default()
        }
    }

    #[test]
    fn restore_all_is_identity() {
        let tabs = vec![restore_tab("a", false), restore_tab("b", true)];
        let (out, sel) =
            filter_tabs_for_restore(&tabs, 1, crate::preferences::TabRestorePolicy::All);
        assert_eq!(out, tabs);
        assert_eq!(sel, 1);
    }

    #[test]
    fn pinned_only_keeps_pinned_and_remaps_selected() {
        let tabs = vec![
            restore_tab("u", false),
            restore_tab("p1", true),
            restore_tab("p2", true),
        ];
        let (out, sel) =
            filter_tabs_for_restore(&tabs, 2, crate::preferences::TabRestorePolicy::PinnedOnly);
        assert_eq!(
            out.iter().map(|t| t.title.as_str()).collect::<Vec<_>>(),
            vec!["p1", "p2"]
        );
        assert_eq!(sel, 1);
    }

    #[test]
    fn pinned_only_selected_dropped_falls_back_to_zero() {
        let tabs = vec![restore_tab("u", false), restore_tab("p", true)];
        let (out, sel) =
            filter_tabs_for_restore(&tabs, 0, crate::preferences::TabRestorePolicy::PinnedOnly);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].title, "p");
        assert_eq!(sel, 0);
    }

    #[test]
    fn none_drops_everything() {
        let tabs = vec![restore_tab("a", true), restore_tab("b", false)];
        let (out, sel) =
            filter_tabs_for_restore(&tabs, 0, crate::preferences::TabRestorePolicy::None);
        assert!(out.is_empty());
        assert_eq!(sel, 0);
    }

    #[test]
    fn round_trip() {
        let s = WorkspaceSession {
            version: CURRENT_VERSION,
            tabs: vec![
                TabState {
                    title: "Query 1".into(),
                    split_mode: SplitMode::None,
                    file_path: None,
                    content: None,
                    connection_id: None,
                    database: None,
                    ddl_panel_width: DEFAULT_DDL_PANEL_WIDTH,
                    history_panel_width: DEFAULT_HISTORY_PANEL_WIDTH,
                    pinned: false,
                    user_named: false,
                    ..Default::default()
                },
                TabState {
                    title: "Query 2".into(),
                    split_mode: SplitMode::Vertical,
                    file_path: Some("/tmp/test.sql".into()),
                    content: None,
                    connection_id: None,
                    database: None,
                    ddl_panel_width: DEFAULT_DDL_PANEL_WIDTH,
                    history_panel_width: DEFAULT_HISTORY_PANEL_WIDTH,
                    pinned: false,
                    user_named: false,
                    ..Default::default()
                },
            ],
            selected_tab: 1,
            layout: LayoutState {
                object_explorer_visible: true,
                results_pane_visible: true,
                sidebar_width: 200.0,
                results_height: DEFAULT_RESULTS_HEIGHT,
                ai_assistant_visible: false,
                ai_assistant_width: 350.0,
                ai_assistant_section: Some("chat".into()),
                editor_zoom: 1.5,
                active_connection_id: Some("11111111-2222-3333-4444-555555555555".into()),
                active_database: Some("Orders".into()),
                project_file_picker_root: Some("/proj/sql".into()),
                sidebar_panel: Some("library".into()),
                diagnostics_height: 200.0,
                editor_split: Some(EditorSplit {
                    axis: SplitMode::Vertical,
                    other_tab: 0,
                    active_first: false,
                    first_size: 480.0,
                    same_buffer: false,
                }),
                ..Default::default()
            },
        };
        let json = encode_session(&s);
        let decoded = decode_session(&json);
        assert_eq!(decoded, s);
    }

    #[test]
    fn sidebar_panel_round_trips_and_defaults_none() {
        let s = WorkspaceSession {
            version: 1,
            tabs: vec![TabState::default()],
            selected_tab: 0,
            layout: LayoutState {
                sidebar_panel: Some("library".into()),
                ..Default::default()
            },
        };
        let decoded = decode_session(&encode_session(&s));
        assert_eq!(decoded.layout.sidebar_panel.as_deref(), Some("library"));
        let json = r#"{"version":1,"tabs":[{"title":"Query 1","split_mode":"none"}],"selected_tab":0,"layout":{}}"#;
        let old = decode_session(json);
        assert_eq!(old.layout.sidebar_panel, None);
    }

    #[test]
    fn decode_corrupt_falls_back() {
        let s = decode_session("not json");
        assert_eq!(s.version, CURRENT_VERSION);
        assert!(!s.tabs.is_empty());
    }

    #[test]
    fn first_load_defaults_explorer_open_at_default_width() {
        // No saved preference (fresh install / corrupt file) → explorer
        // visible at the default width, not collapsed.
        let s = decode_session("not json");
        assert!(s.layout.object_explorer_visible);
        assert_eq!(s.layout.sidebar_width, DEFAULT_SIDEBAR_WIDTH);
    }

    #[test]
    fn sanitized_restores_default_width_for_zero_sidebar() {
        let s = WorkspaceSession {
            version: 1,
            tabs: vec![TabState::default()],
            selected_tab: 0,
            layout: LayoutState {
                sidebar_width: 0.0,
                ..Default::default()
            },
        };
        assert_eq!(s.sanitized().layout.sidebar_width, DEFAULT_SIDEBAR_WIDTH);
    }

    #[test]
    fn sanitized_preserves_sidebar_width() {
        let s = WorkspaceSession {
            version: 1,
            tabs: vec![TabState::default()],
            selected_tab: 0,
            layout: LayoutState {
                sidebar_width: 50.0,
                ..Default::default()
            },
        };
        let sanitized = s.sanitized();
        assert_eq!(sanitized.layout.sidebar_width, 50.0);
    }

    #[test]
    fn sanitized_clamps_ai_width() {
        let s = WorkspaceSession {
            version: 1,
            tabs: vec![TabState::default()],
            selected_tab: 0,
            layout: LayoutState {
                ai_assistant_width: 10.0,
                ..Default::default()
            },
        };
        let sanitized = s.sanitized();
        assert!(sanitized.layout.ai_assistant_width >= MIN_AI_ASSISTANT_WIDTH);
    }

    #[test]
    fn sanitized_clamps_ai_width_max() {
        let s = WorkspaceSession {
            version: 1,
            tabs: vec![TabState::default()],
            selected_tab: 0,
            layout: LayoutState {
                ai_assistant_width: 9999.0,
                ..Default::default()
            },
        };
        let sanitized = s.sanitized();
        assert!(sanitized.layout.ai_assistant_width <= MAX_AI_ASSISTANT_WIDTH);
    }

    #[test]
    fn sanitized_clamps_selected_tab() {
        let s = WorkspaceSession {
            version: 1,
            tabs: vec![TabState::default()],
            selected_tab: 5,
            layout: LayoutState::default(),
        };
        let sanitized = s.sanitized();
        assert_eq!(sanitized.selected_tab, 0);
    }

    #[test]
    fn sanitized_adds_default_tab_if_empty() {
        let s = WorkspaceSession {
            version: 1,
            tabs: vec![],
            selected_tab: 0,
            layout: LayoutState::default(),
        };
        let sanitized = s.sanitized();
        assert_eq!(sanitized.tabs.len(), 1);
    }

    #[test]
    fn split_mode_default_is_none() {
        assert_eq!(SplitMode::default(), SplitMode::None);
    }

    fn two_tab_session(editor_split: Option<EditorSplit>) -> WorkspaceSession {
        WorkspaceSession {
            version: 1,
            tabs: vec![TabState::default(), TabState::default()],
            selected_tab: 0,
            layout: LayoutState {
                editor_split,
                ..Default::default()
            },
        }
    }

    #[test]
    fn editor_split_backward_compat_absent_field() {
        // JSON from before `editor_split` existed must decode to a single pane.
        let json = r#"{"version":1,"tabs":[{"title":"Query 1","split_mode":"none"}],"selected_tab":0,"layout":{"sidebar_width":200.0}}"#;
        let s = decode_session(json);
        assert_eq!(s.layout.editor_split, None);
    }

    #[test]
    fn sanitized_keeps_a_coherent_editor_split() {
        let s = two_tab_session(Some(EditorSplit {
            axis: SplitMode::Horizontal,
            other_tab: 1,
            active_first: true,
            first_size: 300.0,
            same_buffer: false,
        }))
        .sanitized();
        assert!(s.layout.editor_split.is_some());
    }

    #[test]
    fn sanitized_drops_editor_split_with_stale_indices() {
        // Unfocused tab out of range.
        let s = two_tab_session(Some(EditorSplit {
            axis: SplitMode::Vertical,
            other_tab: 7,
            active_first: true,
            first_size: 300.0,
            same_buffer: false,
        }))
        .sanitized();
        assert_eq!(s.layout.editor_split, None);
        // Both panes on the same tab.
        let s = two_tab_session(Some(EditorSplit {
            axis: SplitMode::Vertical,
            other_tab: 0,
            active_first: true,
            first_size: 300.0,
            same_buffer: false,
        }))
        .sanitized();
        assert_eq!(s.layout.editor_split, None);
        // A `none` axis is not a split.
        let s = two_tab_session(Some(EditorSplit {
            axis: SplitMode::None,
            other_tab: 1,
            active_first: true,
            first_size: 300.0,
            same_buffer: false,
        }))
        .sanitized();
        assert_eq!(s.layout.editor_split, None);
    }

    #[test]
    fn same_buffer_split_round_trips() {
        let s = two_tab_session(Some(EditorSplit {
            axis: SplitMode::Vertical,
            other_tab: 0,
            active_first: false,
            first_size: 420.0,
            same_buffer: true,
        }));
        let decoded = decode_session(&encode_session(&s));
        let split = decoded.layout.editor_split.expect("split survives");
        assert!(split.same_buffer);
        assert!(!split.active_first);
        assert_eq!(split.axis, SplitMode::Vertical);
    }

    #[test]
    fn sanitized_keeps_a_same_buffer_split_regardless_of_other_tab() {
        // A same-buffer split has no unfocused-tab index; `other_tab` colliding
        // with the selected tab (the stage-1 invalidity) must not drop it.
        let s = two_tab_session(Some(EditorSplit {
            axis: SplitMode::Horizontal,
            other_tab: 0,
            active_first: true,
            first_size: 300.0,
            same_buffer: true,
        }))
        .sanitized();
        assert!(s.layout.editor_split.is_some_and(|split| split.same_buffer));
        // But a `none` axis is still not a split.
        let s = two_tab_session(Some(EditorSplit {
            axis: SplitMode::None,
            other_tab: 0,
            active_first: true,
            first_size: 300.0,
            same_buffer: true,
        }))
        .sanitized();
        assert_eq!(s.layout.editor_split, None);
    }

    #[test]
    fn editor_split_same_buffer_backward_compat_absent_field() {
        // A stage-1 session written before `same_buffer` existed must decode
        // as a plain two-tab split.
        let json = r#"{"version":1,"tabs":[{"title":"A","split_mode":"none"},{"title":"B","split_mode":"none"}],"selected_tab":0,"layout":{"object_explorer_visible":true,"results_pane_visible":false,"sidebar_width":200.0,"ai_assistant_visible":false,"ai_assistant_width":350.0,"ai_assistant_section":null,"editor_split":{"axis":"vertical","other_tab":1,"active_first":true,"first_size":400.0}}}"#;
        let s = decode_session(json);
        let split = s.layout.editor_split.expect("split survives");
        assert!(!split.same_buffer);
        assert_eq!(split.other_tab, 1);
    }

    #[test]
    fn layout_default() {
        let l = LayoutState::default();
        assert!(l.object_explorer_visible);
        assert!(!l.results_pane_visible);
        assert!(!l.ai_assistant_visible);
        assert_eq!(l.sidebar_width, DEFAULT_SIDEBAR_WIDTH);
        assert_eq!(l.ai_assistant_width, 0.0);
        assert_eq!(l.ai_assistant_section, None);
        // Raw default is 0.0; `sanitized()` maps it to the default zoom.
        assert_eq!(l.editor_zoom, 0.0);
        assert_eq!(l.active_connection_id, None);
        assert_eq!(l.active_database, None);
        assert_eq!(l.results_pane, None);
        assert_eq!(
            l.effective_results_pane(),
            crate::results_pane_state::ResultsPaneState::Hidden
        );
    }

    #[test]
    fn layout_without_results_pane_falls_back_to_legacy_bool() {
        let json = r#"{"version":1,"tabs":[{"title":"A","split_mode":"none"}],"selected_tab":0,"layout":{"object_explorer_visible":true,"results_pane_visible":false,"sidebar_width":200.0,"ai_assistant_visible":false,"ai_assistant_width":350.0,"ai_assistant_section":null}}"#;
        let s = decode_session(json);
        assert!(!s.layout.results_pane_visible);
        assert_eq!(s.layout.results_pane, None);
        assert_eq!(
            s.layout.effective_results_pane(),
            crate::results_pane_state::ResultsPaneState::Hidden
        );
        let json_on = r#"{"version":1,"tabs":[{"title":"A","split_mode":"none"}],"selected_tab":0,"layout":{"object_explorer_visible":true,"results_pane_visible":true,"sidebar_width":200.0,"ai_assistant_visible":false,"ai_assistant_width":350.0}}"#;
        let on = decode_session(json_on);
        assert_eq!(
            on.layout.effective_results_pane(),
            crate::results_pane_state::ResultsPaneState::Split
        );
    }

    #[test]
    fn layout_with_results_pane_maximized_round_trips() {
        let s = WorkspaceSession {
            version: 1,
            tabs: vec![TabState::default()],
            selected_tab: 0,
            layout: LayoutState {
                results_pane_visible: true,
                results_pane: Some(crate::results_pane_state::ResultsPaneState::Maximized),
                ..Default::default()
            },
        };
        let decoded = decode_session(&encode_session(&s));
        assert_eq!(
            decoded.layout.results_pane,
            Some(crate::results_pane_state::ResultsPaneState::Maximized)
        );
        assert_eq!(
            decoded.layout.effective_results_pane(),
            crate::results_pane_state::ResultsPaneState::Maximized
        );
        assert!(decoded.layout.results_pane_visible);
    }

    #[test]
    fn editor_zoom_round_trips() {
        let s = WorkspaceSession {
            version: 1,
            tabs: vec![TabState::default()],
            selected_tab: 0,
            layout: LayoutState {
                editor_zoom: 2.2,
                ..Default::default()
            },
        };
        let decoded = decode_session(&encode_session(&s));
        assert!((decoded.layout.editor_zoom - 2.2).abs() < 1e-4);
    }

    #[test]
    fn sanitized_defaults_missing_editor_zoom() {
        // Old sessions without the field (0.0) become the default zoom.
        let s = WorkspaceSession {
            version: 1,
            tabs: vec![TabState::default()],
            selected_tab: 0,
            layout: LayoutState::default(),
        }
        .sanitized();
        assert!((s.layout.editor_zoom - crate::editor_zoom::DEFAULT_ZOOM).abs() < 1e-4);
    }

    #[test]
    fn sanitized_clamps_out_of_range_editor_zoom() {
        let s = WorkspaceSession {
            version: 1,
            tabs: vec![TabState::default()],
            selected_tab: 0,
            layout: LayoutState {
                editor_zoom: 99.0,
                ..Default::default()
            },
        }
        .sanitized();
        assert!((s.layout.editor_zoom - crate::editor_zoom::MAX_ZOOM).abs() < 1e-4);
    }

    #[test]
    fn editor_zoom_backward_compat_absent_field() {
        // JSON from before `editor_zoom` existed must decode to the default.
        let json = r#"{"version":1,"tabs":[{"title":"Query 1","split_mode":"none"}],"selected_tab":0,"layout":{"sidebar_width":200.0}}"#;
        let s = decode_session(json);
        assert!((s.layout.editor_zoom - crate::editor_zoom::DEFAULT_ZOOM).abs() < 1e-4);
    }

    #[test]
    fn layout_state_defaults_cell_detail_fields() {
        let s = LayoutState::default();
        assert!(!s.cell_detail_visible);
        assert!((s.cell_detail_width - DEFAULT_CELL_DETAIL_WIDTH).abs() < 1e-4);
        assert!((s.cell_detail_height - DEFAULT_CELL_DETAIL_HEIGHT).abs() < 1e-4);
        let json = r#"{"version":1,"tabs":[{"title":"Query 1","split_mode":"none"}],"selected_tab":0,"layout":{"sidebar_width":200.0}}"#;
        let decoded = decode_session(json);
        assert!(!decoded.layout.cell_detail_visible);
        assert!((decoded.layout.cell_detail_width - DEFAULT_CELL_DETAIL_WIDTH).abs() < 1e-4);
        assert!((decoded.layout.cell_detail_height - DEFAULT_CELL_DETAIL_HEIGHT).abs() < 1e-4);
    }

    #[test]
    fn sanitized_clamps_cell_detail_sizes() {
        let too_small = WorkspaceSession {
            version: 1,
            tabs: vec![TabState::default()],
            selected_tab: 0,
            layout: LayoutState {
                cell_detail_width: 10.0,
                cell_detail_height: 10.0,
                ..Default::default()
            },
        }
        .sanitized();
        assert!((too_small.layout.cell_detail_width - MIN_CELL_DETAIL_WIDTH).abs() < 1e-4);
        assert!((too_small.layout.cell_detail_height - MIN_CELL_DETAIL_HEIGHT).abs() < 1e-4);
        let too_big = WorkspaceSession {
            version: 1,
            tabs: vec![TabState::default()],
            selected_tab: 0,
            layout: LayoutState {
                cell_detail_width: 9_999.0,
                cell_detail_height: 9_999.0,
                ..Default::default()
            },
        }
        .sanitized();
        assert!((too_big.layout.cell_detail_width - MAX_CELL_DETAIL_WIDTH).abs() < 1e-4);
        assert!((too_big.layout.cell_detail_height - MAX_CELL_DETAIL_HEIGHT).abs() < 1e-4);
        let nan = WorkspaceSession {
            version: 1,
            tabs: vec![TabState::default()],
            selected_tab: 0,
            layout: LayoutState {
                cell_detail_width: f32::NAN,
                cell_detail_height: f32::INFINITY,
                ..Default::default()
            },
        }
        .sanitized();
        assert!((nan.layout.cell_detail_width - DEFAULT_CELL_DETAIL_WIDTH).abs() < 1e-4);
        assert!((nan.layout.cell_detail_height - DEFAULT_CELL_DETAIL_HEIGHT).abs() < 1e-4);
    }

    #[test]
    fn session_default() {
        let s = WorkspaceSession::default();
        assert_eq!(s.version, 0);
        assert!(s.tabs.is_empty());
    }

    #[test]
    fn tab_state_file_path_round_trip() {
        let s = WorkspaceSession {
            version: 1,
            tabs: vec![TabState {
                title: "file.sql".into(),
                split_mode: SplitMode::None,
                file_path: Some("/path/to/file.sql".into()),
                content: None,
                connection_id: None,
                database: None,
                ddl_panel_width: DEFAULT_DDL_PANEL_WIDTH,
                history_panel_width: DEFAULT_HISTORY_PANEL_WIDTH,
                pinned: false,
                user_named: false,
                ..Default::default()
            }],
            selected_tab: 0,
            layout: LayoutState {
                object_explorer_visible: true,
                results_pane_visible: true,
                sidebar_width: 200.0,
                results_height: DEFAULT_RESULTS_HEIGHT,
                ai_assistant_visible: false,
                ai_assistant_width: 350.0,
                ai_assistant_section: None,
                editor_zoom: 1.0,
                active_connection_id: None,
                active_database: None,
                project_file_picker_root: None,
                sidebar_panel: None,
                diagnostics_height: 0.0,
                editor_split: None,
                ..Default::default()
            },
        };
        let json = encode_session(&s);
        let decoded = decode_session(&json);
        assert_eq!(
            decoded.tabs[0].file_path.as_deref(),
            Some("/path/to/file.sql")
        );
        assert_eq!(decoded.tabs[0].title, "file.sql");
    }

    #[test]
    fn tab_state_notebook_path_round_trips() {
        let s = WorkspaceSession {
            version: 1,
            tabs: vec![TabState {
                title: "analysis.sqlnb.json".into(),
                split_mode: SplitMode::None,
                notebook_path: Some("/path/to/analysis.sqlnb.json".into()),
                ..Default::default()
            }],
            selected_tab: 0,
            layout: LayoutState {
                object_explorer_visible: true,
                results_pane_visible: true,
                sidebar_width: 200.0,
                results_height: DEFAULT_RESULTS_HEIGHT,
                ai_assistant_visible: false,
                ai_assistant_width: 350.0,
                ai_assistant_section: None,
                editor_zoom: 1.0,
                active_connection_id: None,
                active_database: None,
                project_file_picker_root: None,
                sidebar_panel: None,
                diagnostics_height: 0.0,
                editor_split: None,
                ..Default::default()
            },
        };
        let json = encode_session(&s);
        let decoded = decode_session(&json);
        assert_eq!(
            decoded.tabs[0].notebook_path.as_deref(),
            Some("/path/to/analysis.sqlnb.json")
        );
        assert!(decoded.tabs[0].file_path.is_none());
        assert!(decoded.tabs[0].content.is_none());
    }

    #[test]
    fn tab_state_query_params_round_trip() {
        let s = WorkspaceSession {
            version: 1,
            tabs: vec![TabState {
                title: "Query 1".into(),
                query_params: vec![
                    SavedParamValue {
                        name: "@from".into(),
                        ty: "date".into(),
                        value: "2026-01-31".into(),
                    },
                    SavedParamValue {
                        name: "@status".into(),
                        ty: "text".into(),
                        value: "open".into(),
                    },
                ],
                ..Default::default()
            }],
            ..Default::default()
        };
        let json = encode_session(&s);
        assert!(json.contains("\"type\": \"date\""), "{json}");
        let decoded = decode_session(&json);
        assert_eq!(decoded.tabs[0].query_params, s.tabs[0].query_params);
    }

    #[test]
    fn tab_state_query_params_backward_compat_and_skipped_when_empty() {
        let json = r#"{"version":1,"tabs":[{"title":"Query 1","split_mode":"none"}],"selected_tab":0,"layout":{}}"#;
        let s = decode_session(json);
        assert!(s.tabs[0].query_params.is_empty());
        assert!(!encode_session(&s).contains("query_params"));
    }

    #[test]
    fn tab_state_query_params_are_sanitized() {
        let long = "x".repeat(MAX_SAVED_PARAM_VALUE_CHARS + 10);
        let mut params: Vec<SavedParamValue> = (0..MAX_SAVED_PARAMS_PER_TAB + 5)
            .map(|i| SavedParamValue {
                name: format!("@p{i}"),
                ty: "text".into(),
                value: long.clone(),
            })
            .collect();
        params.insert(
            0,
            SavedParamValue {
                name: "no_sigil".into(),
                ty: "text".into(),
                value: "v".into(),
            },
        );
        params.insert(
            0,
            SavedParamValue {
                name: "@empty".into(),
                ty: "text".into(),
                value: String::new(),
            },
        );
        let s = WorkspaceSession {
            tabs: vec![TabState {
                title: "Query 1".into(),
                query_params: params,
                ..Default::default()
            }],
            ..Default::default()
        }
        .sanitized();
        let kept = &s.tabs[0].query_params;
        assert_eq!(kept.len(), MAX_SAVED_PARAMS_PER_TAB);
        assert_eq!(kept[0].name, "@p0");
        assert!(
            kept.iter()
                .all(|p| p.value.chars().count() == MAX_SAVED_PARAM_VALUE_CHARS)
        );
    }

    #[test]
    fn tab_state_file_path_backward_compat() {
        // Old sessions without `file_path` should deserialize to None.
        let json = r#"{"version":1,"tabs":[{"title":"Query 1","split_mode":"none"}],"selected_tab":0,"layout":{}}"#;
        let s = decode_session(json);
        assert_eq!(s.tabs[0].title, "Query 1");
        assert!(s.tabs[0].file_path.is_none());
    }

    #[test]
    fn tab_state_result_cache_key_round_trip() {
        let key = uuid::Uuid::new_v4().to_string();
        let s = WorkspaceSession {
            version: 1,
            tabs: vec![TabState {
                title: "Query 1".into(),
                split_mode: SplitMode::None,
                file_path: None,
                content: None,
                connection_id: None,
                database: None,
                ddl_panel_width: DEFAULT_DDL_PANEL_WIDTH,
                history_panel_width: DEFAULT_HISTORY_PANEL_WIDTH,
                pinned: false,
                user_named: false,
                auto_named: None,
                auto_number: None,
                result_cache_key: Some(key.clone()),
                notebook_path: None,
                query_params: Vec::new(),
            }],
            selected_tab: 0,
            layout: LayoutState::default(),
        };
        let json = encode_session(&s);
        let decoded = decode_session(&json);
        assert_eq!(
            decoded.tabs[0].result_cache_key.as_deref(),
            Some(key.as_str())
        );
    }

    #[test]
    fn tab_state_result_cache_key_backward_compat() {
        let json = r#"{"version":1,"tabs":[{"title":"Query 1","split_mode":"none"}],"selected_tab":0,"layout":{}}"#;
        let s = decode_session(json);
        assert!(s.tabs[0].result_cache_key.is_none());
    }

    #[test]
    fn tab_state_result_cache_key_skipped_when_none() {
        let s = WorkspaceSession {
            version: 1,
            tabs: vec![TabState {
                title: "Query 1".into(),
                split_mode: SplitMode::None,
                file_path: None,
                content: None,
                connection_id: None,
                database: None,
                ddl_panel_width: DEFAULT_DDL_PANEL_WIDTH,
                history_panel_width: DEFAULT_HISTORY_PANEL_WIDTH,
                pinned: false,
                user_named: false,
                ..Default::default()
            }],
            selected_tab: 0,
            layout: LayoutState::default(),
        };
        let json = encode_session(&s);
        assert!(!json.contains("resultCacheKey"));
    }

    #[test]
    fn tab_state_file_path_skipped_when_none() {
        let s = WorkspaceSession {
            version: 1,
            tabs: vec![TabState {
                title: "Query 1".into(),
                split_mode: SplitMode::None,
                file_path: None,
                content: None,
                connection_id: None,
                database: None,
                ddl_panel_width: DEFAULT_DDL_PANEL_WIDTH,
                history_panel_width: DEFAULT_HISTORY_PANEL_WIDTH,
                pinned: false,
                user_named: false,
                ..Default::default()
            }],
            selected_tab: 0,
            layout: LayoutState::default(),
        };
        let json = encode_session(&s);
        // `file_path` should not appear in JSON when None.
        assert!(!json.contains("file_path"));
    }

    #[test]
    fn tab_state_content_round_trip() {
        let s = WorkspaceSession {
            version: 1,
            tabs: vec![TabState {
                title: "Query 1".into(),
                split_mode: SplitMode::None,
                file_path: None,
                content: Some("SELECT 1".into()),
                connection_id: None,
                database: None,
                ddl_panel_width: DEFAULT_DDL_PANEL_WIDTH,
                history_panel_width: DEFAULT_HISTORY_PANEL_WIDTH,
                pinned: false,
                user_named: false,
                ..Default::default()
            }],
            selected_tab: 0,
            layout: LayoutState {
                object_explorer_visible: true,
                results_pane_visible: true,
                sidebar_width: 200.0,
                results_height: DEFAULT_RESULTS_HEIGHT,
                ai_assistant_visible: false,
                ai_assistant_width: 350.0,
                ai_assistant_section: None,
                editor_zoom: 1.0,
                active_connection_id: None,
                active_database: None,
                project_file_picker_root: None,
                sidebar_panel: None,
                diagnostics_height: 0.0,
                editor_split: None,
                ..Default::default()
            },
        };
        let json = encode_session(&s);
        let decoded = decode_session(&json);
        assert_eq!(decoded.tabs[0].content.as_deref(), Some("SELECT 1"));
    }

    #[test]
    fn large_unsaved_content_survives_save_and_load_in_full() {
        // HeidiSQL #2583: very large scripts must survive restart in full.
        let line = format!("SELECT 1, {} FROM big_table;\n", "x".repeat(120));
        let mut text = String::new();
        while text.len() < 2 * 1024 * 1024 {
            text.push_str(&line);
        }
        text.push_str("-- the final line must survive\n");
        let s = WorkspaceSession {
            version: CURRENT_VERSION,
            tabs: vec![TabState {
                title: "Big".into(),
                split_mode: SplitMode::None,
                file_path: None,
                content: Some(text.clone()),
                connection_id: None,
                database: None,
                ddl_panel_width: DEFAULT_DDL_PANEL_WIDTH,
                history_panel_width: DEFAULT_HISTORY_PANEL_WIDTH,
                pinned: false,
                user_named: false,
                ..Default::default()
            }],
            selected_tab: 0,
            layout: LayoutState::default(),
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        save_session(&s, &path).unwrap();
        assert!(std::fs::metadata(&path).unwrap().len() > 2 * 1024 * 1024);
        let restored = load_session(&path);
        assert_eq!(restored.tabs[0].content.as_deref(), Some(text.as_str()));
    }

    #[test]
    fn unreadable_session_file_is_moved_aside_not_overwritten() {
        // A session that fails to parse (truncated, or written by a newer
        // build) still holds unsaved scripts; the next save must not eat it.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace-session.json");
        let original = r#"{"version":1,"tabs":[{"title":"Keep","content":"SELECT 'precious'"#;
        std::fs::write(&path, original).unwrap();
        let (session, kept) = load_session_preserving(&path);
        assert_eq!(
            session.tabs.len(),
            WorkspaceSession::new().sanitized().tabs.len()
        );
        let kept = kept.expect("unreadable session should be preserved");
        assert_eq!(std::fs::read_to_string(&kept).unwrap(), original);
        save_session(&session, &path).unwrap();
        assert_eq!(std::fs::read_to_string(&kept).unwrap(), original);
    }

    #[test]
    fn missing_or_valid_session_file_is_not_moved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace-session.json");
        assert!(load_session_preserving(&path).1.is_none());
        save_session(&WorkspaceSession::new(), &path).unwrap();
        assert!(load_session_preserving(&path).1.is_none());
        assert!(path.exists());
    }

    #[test]
    fn tab_state_content_backward_compat() {
        // Old sessions without `content` should deserialize to None.
        let json = r#"{"version":1,"tabs":[{"title":"Query 1","split_mode":"none"}],"selected_tab":0,"layout":{}}"#;
        let s = decode_session(json);
        assert!(s.tabs[0].content.is_none());
    }

    #[test]
    fn tab_state_content_skipped_when_none() {
        let s = WorkspaceSession {
            version: 1,
            tabs: vec![TabState {
                title: "Query 1".into(),
                split_mode: SplitMode::None,
                file_path: None,
                content: None,
                connection_id: None,
                database: None,
                ddl_panel_width: DEFAULT_DDL_PANEL_WIDTH,
                history_panel_width: DEFAULT_HISTORY_PANEL_WIDTH,
                pinned: false,
                user_named: false,
                ..Default::default()
            }],
            selected_tab: 0,
            layout: LayoutState::default(),
        };
        let json = encode_session(&s);
        assert!(!json.contains("content"));
    }

    #[test]
    fn tab_state_user_named_round_trips_and_defaults_false() {
        let named = WorkspaceSession {
            version: 1,
            tabs: vec![TabState {
                title: "incident 4123".into(),
                user_named: true,
                ..Default::default()
            }],
            selected_tab: 0,
            layout: LayoutState::default(),
        };
        let json = encode_session(&named);
        assert!(
            json.contains("user_named"),
            "user_named:true must be serialized: {json}"
        );
        let decoded = decode_session(&json);
        assert!(decoded.tabs[0].user_named);

        let old = r#"{"version":1,"tabs":[{"title":"Query 1","split_mode":"none"}],"selected_tab":0,"layout":{}}"#;
        let s = decode_session(old);
        assert!(!s.tabs[0].user_named);
        let omitted = encode_session(&WorkspaceSession {
            version: 1,
            tabs: vec![TabState::default()],
            selected_tab: 0,
            layout: LayoutState::default(),
        });
        assert!(
            !omitted.contains("user_named"),
            "false user_named must be skipped: {omitted}"
        );
    }

    #[test]
    fn tab_state_auto_name_marker_round_trips_and_defaults_absent() {
        let session = WorkspaceSession {
            version: 1,
            tabs: vec![TabState {
                title: "Consulta 3".into(),
                auto_named: Some(true),
                auto_number: Some(3),
                ..Default::default()
            }],
            selected_tab: 0,
            layout: LayoutState::default(),
        };
        let decoded = decode_session(&encode_session(&session));
        assert_eq!(decoded.tabs[0].auto_named, Some(true));
        assert_eq!(decoded.tabs[0].auto_number, Some(3));

        let old = r#"{"version":1,"tabs":[{"title":"Query 1","split_mode":"none"}],"selected_tab":0,"layout":{}}"#;
        let legacy = decode_session(old);
        assert_eq!(legacy.tabs[0].auto_named, None);
        assert_eq!(legacy.tabs[0].auto_number, None);
        let omitted = encode_session(&WorkspaceSession {
            version: 1,
            tabs: vec![TabState::default()],
            selected_tab: 0,
            layout: LayoutState::default(),
        });
        assert!(!omitted.contains("auto_named"), "{omitted}");
        assert!(!omitted.contains("auto_number"), "{omitted}");
    }

    #[test]
    fn active_connection_and_database_round_trip() {
        let s = WorkspaceSession {
            version: 1,
            tabs: vec![TabState::default()],
            selected_tab: 0,
            layout: LayoutState {
                active_connection_id: Some("abc-123".into()),
                active_database: Some("Sales".into()),
                ..Default::default()
            },
        };
        let decoded = decode_session(&encode_session(&s));
        assert_eq!(
            decoded.layout.active_connection_id.as_deref(),
            Some("abc-123")
        );
        assert_eq!(decoded.layout.active_database.as_deref(), Some("Sales"));
    }

    #[test]
    fn active_connection_and_database_backward_compat() {
        // Old sessions without these fields decode to None.
        let json = r#"{"version":1,"tabs":[{"title":"Query 1","split_mode":"none"}],"selected_tab":0,"layout":{"sidebar_width":200.0}}"#;
        let s = decode_session(json);
        assert!(s.layout.active_connection_id.is_none());
        assert!(s.layout.active_database.is_none());
    }

    #[test]
    fn active_connection_and_database_skipped_when_none() {
        let s = WorkspaceSession {
            version: 1,
            tabs: vec![TabState::default()],
            selected_tab: 0,
            layout: LayoutState::default(),
        };
        let json = encode_session(&s);
        assert!(!json.contains("active_connection_id"));
        assert!(!json.contains("active_database"));
    }

    #[test]
    fn legacy_partial_layout_preserves_fields_that_are_present() {
        let json = r#"{"version":1,"tabs":[{"title":"Legacy","split_mode":"none"}],"selected_tab":0,"layout":{"object_explorer_visible":false,"sidebar_width":444.0}}"#;
        let session = decode_session(json);
        assert!(!session.layout.object_explorer_visible);
        assert_eq!(session.layout.sidebar_width, 444.0);
        assert_eq!(session.layout.results_height, DEFAULT_RESULTS_HEIGHT);
        assert_eq!(session.tabs[0].ddl_panel_width, DEFAULT_DDL_PANEL_WIDTH);
        assert_eq!(
            session.tabs[0].history_panel_width,
            DEFAULT_HISTORY_PANEL_WIDTH
        );
    }

    #[test]
    fn sanitization_preserves_all_tabs_and_content_and_bounds_non_finite_dimensions() {
        let mut tabs = (0..105)
            .map(|index| TabState {
                title: format!("Query {index}"),
                content: (index < 6).then(|| "é".repeat(600_000)),
                ddl_panel_width: f32::INFINITY,
                history_panel_width: f32::NAN,
                pinned: false,
                ..Default::default()
            })
            .collect::<Vec<_>>();
        tabs[0].title = "x".repeat(300);
        let session = WorkspaceSession {
            version: CURRENT_VERSION,
            tabs,
            selected_tab: usize::MAX,
            layout: LayoutState {
                sidebar_width: f32::NAN,
                results_height: f32::INFINITY,
                ..Default::default()
            },
        }
        .sanitized();
        assert_eq!(session.tabs.len(), 105);
        assert_eq!(session.tabs[0].title.chars().count(), 200);
        assert_eq!(session.selected_tab, 0);
        assert_eq!(session.layout.sidebar_width, DEFAULT_SIDEBAR_WIDTH);
        assert_eq!(session.layout.results_height, DEFAULT_RESULTS_HEIGHT);
        assert_eq!(session.tabs[0].content.as_ref().unwrap().len(), 1_200_000);
        assert_eq!(session.tabs[104].title, "Query 104");
        assert_eq!(session.tabs[0].ddl_panel_width, DEFAULT_DDL_PANEL_WIDTH);
        assert_eq!(
            session.tabs[0].history_panel_width,
            DEFAULT_HISTORY_PANEL_WIDTH
        );
    }

    #[test]
    fn diagnostics_height_defaults_and_clamps() {
        let missing = decode_session(
            r#"{"version":1,"tabs":[{"title":"Query 1","split_mode":"none"}],"selected_tab":0,"layout":{"sidebar_width":200.0}}"#,
        );
        assert!((missing.layout.diagnostics_height - DEFAULT_DIAGNOSTICS_HEIGHT).abs() < 1e-4);

        let too_small = WorkspaceSession {
            version: 1,
            tabs: vec![TabState::default()],
            selected_tab: 0,
            layout: LayoutState {
                diagnostics_height: 10.0,
                ..Default::default()
            },
        }
        .sanitized();
        assert!((too_small.layout.diagnostics_height - MIN_DIAGNOSTICS_HEIGHT).abs() < 1e-4);

        let too_big = WorkspaceSession {
            version: 1,
            tabs: vec![TabState::default()],
            selected_tab: 0,
            layout: LayoutState {
                diagnostics_height: 9999.0,
                ..Default::default()
            },
        }
        .sanitized();
        assert!((too_big.layout.diagnostics_height - MAX_DIAGNOSTICS_HEIGHT).abs() < 1e-4);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn named_workspaces_round_trip_rename_duplicate_and_delete() {
        let temp = tempfile::tempdir().unwrap();
        let store = NamedWorkspaceStore::new(temp.path().join("workspaces"));
        let mut index = store.load_index();
        let session = WorkspaceSession {
            version: CURRENT_VERSION,
            tabs: vec![TabState {
                title: "Analysis".into(),
                content: Some("SELECT 42".into()),
                pinned: false,
                ..Default::default()
            }],
            selected_tab: 0,
            layout: LayoutState::default(),
        };
        let id = store
            .save(&mut index, None, "Morning work", &session, 10)
            .unwrap();
        assert_eq!(
            store.load_index().active_workspace_id.as_deref(),
            Some(id.as_str())
        );
        assert_eq!(
            store.load(&id).unwrap().tabs[0].content.as_deref(),
            Some("SELECT 42")
        );

        store.rename(&mut index, &id, "Renamed", 20).unwrap();
        assert_eq!(store.load_index().workspaces[0].name, "Renamed");
        let copy = store
            .duplicate(&mut index, &id, "Renamed copy", 30)
            .unwrap();
        assert_ne!(copy, id);
        assert_eq!(index.active_workspace_id.as_deref(), Some(id.as_str()));
        assert_eq!(store.load(&copy).unwrap().tabs[0].title, "Analysis");

        store.delete(&mut index, &id).unwrap();
        assert!(store.load(&id).is_none());
        assert_eq!(store.load_index().workspaces.len(), 1);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn corrupt_named_snapshot_does_not_hide_healthy_workspaces() {
        let temp = tempfile::tempdir().unwrap();
        let store = NamedWorkspaceStore::new(temp.path().join("workspaces"));
        let mut index = NamedWorkspaceIndex::default();
        let first = store
            .save(&mut index, None, "First", &WorkspaceSession::new(), 10)
            .unwrap();
        let second = store
            .save(&mut index, None, "Second", &WorkspaceSession::new(), 20)
            .unwrap();
        std::fs::write(store.snapshot_path(&first).unwrap(), "not json").unwrap();
        assert!(store.load(&first).is_none());
        assert!(store.load(&second).is_some());
        assert_eq!(store.load_index().workspaces.len(), 2);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn corrupt_or_legacy_named_manifest_loads_safely() {
        let temp = tempfile::tempdir().unwrap();
        let store = NamedWorkspaceStore::new(temp.path().join("workspaces"));
        std::fs::create_dir_all(&store.root).unwrap();
        std::fs::write(store.manifest_path(), "not json").unwrap();
        assert_eq!(store.load_index(), NamedWorkspaceIndex::default());

        let id = uuid::Uuid::new_v4().to_string();
        std::fs::write(
            store.manifest_path(),
            format!(
                r#"{{"workspaces":[{{"id":"{id}","name":" Legacy ","updated_at_unix_ms":1}}]}}"#
            ),
        )
        .unwrap();
        let index = store.load_index();
        assert_eq!(index.version, NAMED_WORKSPACE_VERSION);
        assert_eq!(index.workspaces[0].name, "Legacy");
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn named_workspace_names_and_capacity_are_bounded() {
        let temp = tempfile::tempdir().unwrap();
        let store = NamedWorkspaceStore::new(temp.path().join("workspaces"));
        let mut index = NamedWorkspaceIndex::default();
        let empty_name = store
            .save(&mut index, None, "  ", &WorkspaceSession::new(), 0)
            .unwrap_err();
        assert_eq!(
            empty_name
                .get_ref()
                .and_then(|error| error.downcast_ref::<WorkspaceError>())
                .map(|error| error.key),
            Some("workspace.store.name_empty")
        );
        let long_name = store
            .save(
                &mut index,
                None,
                &"x".repeat(MAX_WORKSPACE_NAME_CHARS + 1),
                &WorkspaceSession::new(),
                0,
            )
            .unwrap_err();
        let long_name = long_name
            .get_ref()
            .and_then(|error| error.downcast_ref::<WorkspaceError>())
            .unwrap();
        assert_eq!(long_name.key, "workspace.store.name_too_long");
        assert_eq!(
            long_name.args,
            vec![("maximum", MAX_WORKSPACE_NAME_CHARS.to_string())]
        );
        index.workspaces = (0..MAX_NAMED_WORKSPACES)
            .map(|i| NamedWorkspaceMetadata {
                id: uuid::Uuid::new_v4().to_string(),
                name: format!("Workspace {i}"),
                updated_at_unix_ms: i as i64,
            })
            .collect();
        let capacity = store
            .save(
                &mut index,
                None,
                "One too many",
                &WorkspaceSession::new(),
                0,
            )
            .unwrap_err();
        let capacity = capacity
            .get_ref()
            .and_then(|error| error.downcast_ref::<WorkspaceError>())
            .unwrap();
        assert_eq!(capacity.key, "workspace.store.capacity");
        assert_eq!(
            capacity.args,
            vec![("maximum", MAX_NAMED_WORKSPACES.to_string())]
        );
    }
}
