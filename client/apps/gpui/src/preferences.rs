use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

use crate::masking_rules::MaskingRules;

#[cfg(not(target_arch = "wasm32"))]
pub fn preferences_path() -> PathBuf {
    let mut p = dirs::data_dir().unwrap_or_else(std::env::temp_dir);
    p.push("SQLLY");
    if !p.exists() {
        let _ = std::fs::create_dir_all(&p);
    }
    p
}

/// Browser build: nominal path only — every store below reads and writes
/// localStorage through `host_storage` instead of the filesystem.
#[cfg(target_arch = "wasm32")]
pub fn preferences_path() -> PathBuf {
    PathBuf::from("SQLLY")
}

/// Browser build: fault-tolerant localStorage load, mirroring the native
/// "missing or corrupt file → default" contract.
#[cfg(target_arch = "wasm32")]
pub(crate) fn wasm_load_json<T: serde::de::DeserializeOwned + Default>(name: &str) -> T {
    crate::host_storage::load_text(crate::host_storage::StorageArea::Data, name)
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Browser build: best-effort localStorage save (mirrors the native
/// best-effort `std::fs::write`).
#[cfg(target_arch = "wasm32")]
pub(crate) fn wasm_save_json<T: serde::Serialize>(name: &str, value: &T) {
    if let Ok(text) = crate::config_version::to_string_pretty(value) {
        let _ =
            crate::host_storage::store_text(crate::host_storage::StorageArea::Data, name, &text);
    }
}

fn editor_prefs_path() -> PathBuf {
    let mut p = preferences_path();
    p.push("editor-preferences.json");
    p
}

fn appearance_prefs_path() -> PathBuf {
    let mut p = preferences_path();
    p.push("appearance-preferences.json");
    p
}

fn result_formatting_path() -> PathBuf {
    let mut p = preferences_path();
    p.push("result-formatting.json");
    p
}

fn masking_rules_path() -> PathBuf {
    let mut p = preferences_path();
    p.push("masking-rules.json");
    p
}

fn export_path() -> PathBuf {
    let mut p = preferences_path();
    p.push("export.json");
    p
}

fn export_options_prefs_path() -> PathBuf {
    let mut p = preferences_path();
    p.push("export-options.json");
    p
}

fn sql_formatting_path() -> PathBuf {
    let mut p = preferences_path();
    p.push("sql-formatting.json");
    p
}

/// The FIM prefs live here with the other path fns so every prefs file this app
/// writes is discoverable in one place; the load/save that use it are next to
/// `FimPreferences` itself in `fim_completion`.
pub(crate) fn fim_path() -> PathBuf {
    let mut p = preferences_path();
    p.push("fim.json");
    p
}

/// Command-palette recency list; load/save live beside `CommandMru` in
/// `command_palette`.
pub(crate) fn command_mru_path() -> PathBuf {
    let mut p = preferences_path();
    p.push("command-palette-recents.json");
    p
}

/// Tip of the Day state; load/save live beside `TipPreferences` in
/// `tip_of_the_day`.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn tip_of_the_day_path() -> PathBuf {
    let mut p = preferences_path();
    p.push(crate::tip_of_the_day::FILE_NAME);
    p
}

pub(crate) fn param_history_path() -> PathBuf {
    let mut p = preferences_path();
    p.push("param-history.json");
    p
}

fn query_resume_path() -> PathBuf {
    let mut p = preferences_path();
    p.push("query-resume.json");
    p
}

fn tab_prefs_path() -> PathBuf {
    let mut p = preferences_path();
    p.push("tab-preferences.json");
    p
}

fn detached_results_path() -> PathBuf {
    let mut p = preferences_path();
    p.push("detached-results.json");
    p
}

fn explain_prefs_path() -> PathBuf {
    let mut p = preferences_path();
    p.push("explain.json");
    p
}

fn kubectl_prefs_path() -> PathBuf {
    let mut p = preferences_path();
    p.push("kubectl-preferences.json");
    p
}

pub(crate) fn native_tools_path() -> PathBuf {
    let mut p = preferences_path();
    p.push("native-tools.json");
    p
}

fn update_prefs_path() -> PathBuf {
    let mut p = preferences_path();
    p.push("update-preferences.json");
    p
}

fn diagnostic_logging_path() -> PathBuf {
    let mut p = preferences_path();
    p.push("diagnostic-logging.json");
    p
}

fn ddl_navigation_path() -> PathBuf {
    let mut p = preferences_path();
    p.push("ddl-navigation.json");
    p
}

fn performance_path() -> PathBuf {
    let mut p = preferences_path();
    p.push("performance-preferences.json");
    p
}

fn data_model_path() -> PathBuf {
    let mut p = preferences_path();
    p.push("data-model-preferences.json");
    p
}

fn preferences_ui_state_path() -> PathBuf {
    let mut p = preferences_path();
    p.push("preferences-ui-state.json");
    p
}

fn ai_summary_prefs_path() -> PathBuf {
    let mut p = preferences_path();
    p.push("ai-summary-preferences.json");
    p
}

// ============================================================================
// Preferences dialog UI state (reopen exactly where you left off)
// ============================================================================

/// Snapshot of the Settings dialog's UI state, persisted on close so the
/// dialog reopens exactly as it was left: selected pane, sidebar folder
/// expansion, and both scroll positions. Scroll offsets are stored as the raw
/// (negative-when-scrolled) GPUI offsets so restore is a straight set.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct PreferencesUiState {
    pub selected_pane: Option<PreferencesPane>,
    pub expanded: Vec<String>,
    pub sidebar_scroll_y: f32,
    pub pane_scroll_y: f32,
}

impl PreferencesUiState {
    /// `None` when no state has been saved yet (first run), so the caller can
    /// apply its first-run defaults instead.
    pub fn load() -> Option<Self> {
        #[cfg(target_arch = "wasm32")]
        return crate::host_storage::load_text(
            crate::host_storage::StorageArea::Data,
            "preferences-ui-state.json",
        )
        .and_then(|text| serde_json::from_str(&text).ok());
        #[cfg(not(target_arch = "wasm32"))]
        {
            let data = std::fs::read(preferences_ui_state_path()).ok()?;
            serde_json::from_slice(&data).ok()
        }
    }

    pub fn save(&self) {
        #[cfg(target_arch = "wasm32")]
        return wasm_save_json("preferences-ui-state.json", self);
        #[cfg(not(target_arch = "wasm32"))]
        if let Ok(data) = crate::config_version::to_vec_pretty(self) {
            let _ = crate::fs_safe::write_atomic(&preferences_ui_state_path(), &data);
        }
    }
}

// ============================================================================
// Data Model Preferences (Server Explorer organization)
// ============================================================================

/// How schema membership is presented in the server explorer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum SchemaDisplayMode {
    /// server → database → schema → (Tables, Views, Programmability) → item,
    /// items shown with just their name.
    SchemaFolders,
    /// server → database → (Tables, Views, Programmability) → `schema.item`,
    /// all schemas mixed together with the schema name as a prefix.
    #[default]
    FlatPrefixed,
}

/// Where system databases (master, tempdb, model, msdb) appear under a server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum SystemDatabasePlacement {
    /// Grouped into a "System Databases" folder below the user databases.
    #[default]
    Folder,
    /// Mixed in with user databases, as the server returns them.
    Inline,
    /// Not shown at all.
    Hidden,
}

/// How system schemas (sys, INFORMATION_SCHEMA, db_* roles, guest) appear
/// under a database when schema folders are shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum SystemSchemaPlacement {
    /// Grouped into a "System Schemas" folder below the general schemas.
    #[default]
    Folder,
    /// Not shown at all.
    Hidden,
    /// Mixed in with general schemas, but only when they contain objects.
    OnlyIfNotEmpty,
}

/// How general (non-system) schemas appear under a database when schema
/// folders are shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum GeneralSchemaPlacement {
    /// Every schema is shown, empty or not.
    #[default]
    Show,
    /// Empty schemas are not shown.
    ShowIfNotEmpty,
    /// Non-empty schemas are shown inline; empty schemas are grouped into an
    /// "Empty Schemas" folder.
    ShowWithEmptyFolder,
}

/// Horizontal alignment of a metadata segment within its padded column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum MetadataAlignment {
    #[default]
    Left,
    Right,
}

/// Alignment + spacing for one metadata segment (data type or nullability).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct MetadataColumnFormat {
    pub alignment: MetadataAlignment,
    /// Width of the segment's column in characters. 0 = auto (size to the
    /// widest sibling value).
    pub width_chars: usize,
}

impl Default for MetadataColumnFormat {
    fn default() -> Self {
        Self {
            alignment: MetadataAlignment::Left,
            width_chars: 0,
        }
    }
}

/// Server-explorer organization settings (Settings ▸ Data Model ▸ Server
/// Explorer).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct DataModelPreferences {
    pub schema_display: SchemaDisplayMode,
    /// Master switch for vertically aligning `{name} {data type} {nullable}`
    /// metadata under expanded objects.
    pub align_metadata: bool,
    pub align_table_columns: bool,
    pub align_view_columns: bool,
    pub align_procedure_parameters: bool,
    pub align_function_parameters: bool,
    pub align_type_columns: bool,
    /// Keep the server explorer focused on what you are working on:
    /// go-to-definition jumps also reveal the object's node in the tree.
    pub auto_navigate_into_view: bool,
    pub system_databases: SystemDatabasePlacement,
    pub system_schemas: SystemSchemaPlacement,
    pub general_schemas: GeneralSchemaPlacement,
    pub data_type_format: MetadataColumnFormat,
    pub nullability_format: MetadataColumnFormat,
}

impl Default for DataModelPreferences {
    fn default() -> Self {
        Self {
            schema_display: SchemaDisplayMode::default(),
            align_metadata: true,
            align_table_columns: true,
            align_view_columns: true,
            align_procedure_parameters: true,
            align_function_parameters: true,
            align_type_columns: true,
            auto_navigate_into_view: false,
            system_databases: SystemDatabasePlacement::default(),
            system_schemas: SystemSchemaPlacement::default(),
            general_schemas: GeneralSchemaPlacement::default(),
            data_type_format: MetadataColumnFormat::default(),
            nullability_format: MetadataColumnFormat::default(),
        }
    }
}

impl DataModelPreferences {
    /// Whether metadata alignment applies to the given explorer surface —
    /// the master switch AND the per-surface switch.
    pub fn align_enabled(&self, surface: AlignSurface) -> bool {
        self.align_metadata
            && match surface {
                AlignSurface::TableColumns => self.align_table_columns,
                AlignSurface::ViewColumns => self.align_view_columns,
                AlignSurface::ProcedureParameters => self.align_procedure_parameters,
                AlignSurface::FunctionParameters => self.align_function_parameters,
                AlignSurface::TypeColumns => self.align_type_columns,
            }
    }

    pub fn load() -> Self {
        #[cfg(target_arch = "wasm32")]
        return wasm_load_json("data-model-preferences.json");
        #[cfg(not(target_arch = "wasm32"))]
        {
            let path = data_model_path();
            match std::fs::read(&path) {
                Ok(data) => serde_json::from_slice(&data).unwrap_or_default(),
                Err(_) => Self::default(),
            }
        }
    }

    pub fn save(&self) {
        #[cfg(target_arch = "wasm32")]
        return wasm_save_json("data-model-preferences.json", self);
        #[cfg(not(target_arch = "wasm32"))]
        {
            let path = data_model_path();
            if let Ok(data) = crate::config_version::to_vec_pretty(self) {
                let _ = crate::fs_safe::write_atomic(&path, &data);
            }
        }
    }
}

/// Explorer surfaces whose metadata columns can be vertically aligned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlignSurface {
    TableColumns,
    ViewColumns,
    ProcedureParameters,
    FunctionParameters,
    /// User-defined table types — the explorer does not surface them yet,
    /// but the preference exists so it applies as soon as it does.
    #[allow(dead_code)]
    TypeColumns,
}

// ============================================================================
// Performance Preferences
// ============================================================================

/// Performance tuning, currently the "Query Result Optimizations" group: how
/// large result sets spill to disk and how the grid pages the spill file in
/// and out of memory while scrolling.
/// When a completed run is added to the result run-history strip. Lets the
/// user avoid cluttering history with identical re-runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum RunHistoryCaptureMode {
    /// Store only when the query text differs from the previous run.
    QueryChange,
    /// Store only when the results differ from the previous run.
    ResultsChange,
    /// Store when the query text OR the results changed.
    #[default]
    Either,
    /// Store only when the query text AND the results both changed.
    Both,
}

impl RunHistoryCaptureMode {
    pub const ALL: [RunHistoryCaptureMode; 4] = [
        RunHistoryCaptureMode::QueryChange,
        RunHistoryCaptureMode::ResultsChange,
        RunHistoryCaptureMode::Either,
        RunHistoryCaptureMode::Both,
    ];

    pub fn label(self) -> String {
        match self {
            RunHistoryCaptureMode::QueryChange => crate::localization::text(
                "settings.preferences.run_history_capture_mode.label.only_query_changes",
            ),
            RunHistoryCaptureMode::ResultsChange => crate::localization::text(
                "settings.preferences.run_history_capture_mode.label.only_results_change",
            ),
            RunHistoryCaptureMode::Either => crate::localization::text(
                "settings.preferences.run_history_capture_mode.label.query_results_change",
            ),
            RunHistoryCaptureMode::Both => crate::localization::text(
                "settings.preferences.run_history_capture_mode.label.query_and_results_change",
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PerformancePreferences {
    /// Rows at which a result set stops streaming into memory and is served
    /// from its complete spill file instead.
    pub spill_row_threshold: usize,
    /// Approximate cap on preview bytes held in memory per result set — the
    /// row threshold caps preview ROWS, this budget stops wide rows earlier.
    pub result_memory_budget_bytes: usize,
    /// Rows kept resident before AND after the visible viewport.
    pub window_rows: usize,
    /// When the viewport comes within this many rows of the resident
    /// window's edge, the next chunk is paged in.
    pub page_margin_rows: usize,
    /// Rows paged in (and evicted from the far side) per load.
    pub page_chunk_rows: usize,
    /// Show the full per-query timing breakdown in the results status bar
    /// (First / Query / Server / Paint / Wall / Latency, the latency-subtract
    /// toggle, and the hh:MM:ss format toggle). Off by default: the breakdown
    /// is a diagnostics aid, not something most runs need cluttering the bar.
    pub show_detailed_query_timing: bool,
    /// Pace results-grid repaints during wheel and trackpad scrolling to 60
    /// per second. On 120 Hz displays this roughly halves the CPU a scroll
    /// costs, at a slight loss of smoothness; no effect at 60 Hz. Off by
    /// default.
    pub limit_scroll_repaints: bool,
    /// How many completed runs each query tab keeps in the result run strip.
    /// 0 keeps only the current run unless Execute into New Result Tab forces a one-shot keep.
    #[serde(default = "default_result_run_history_runs")]
    pub result_run_history_runs: usize,
    /// Master switch for the result run-history strip. On by default; when off
    /// no runs are automatically retained and the strip is hidden.
    #[serde(default = "default_result_run_history_enabled")]
    pub result_run_history_enabled: bool,
    /// When a completed run should be added to history: on query change, on
    /// results change, on either, or only when both changed.
    #[serde(default)]
    pub result_run_history_capture: RunHistoryCaptureMode,
}

fn default_result_run_history_runs() -> usize {
    5
}

fn default_result_run_history_enabled() -> bool {
    true
}

impl Default for PerformancePreferences {
    fn default() -> Self {
        Self {
            spill_row_threshold: 50_000,
            result_memory_budget_bytes: 64 * 1024 * 1024,
            window_rows: 3_000,
            page_margin_rows: 500,
            page_chunk_rows: 3_000,
            show_detailed_query_timing: false,
            limit_scroll_repaints: false,
            result_run_history_runs: default_result_run_history_runs(),
            result_run_history_enabled: default_result_run_history_enabled(),
            result_run_history_capture: RunHistoryCaptureMode::default(),
        }
    }
}

impl PerformancePreferences {
    /// Clamped accessors — hand-edited or out-of-range values are pulled back
    /// into bounds that keep the paging machinery sound (the margin must stay
    /// inside the window or paging would never trigger/always trigger).
    /// Minimum interval between scroll-driven grid repaints, for
    /// `GridConfig::scroll_repaint_interval`; `None` paints at display rate.
    pub fn scroll_repaint_interval(&self) -> Option<std::time::Duration> {
        self.limit_scroll_repaints
            .then(|| std::time::Duration::from_micros(16_667))
    }

    pub fn spill_row_threshold(&self) -> usize {
        self.spill_row_threshold.clamp(1_000, 10_000_000)
    }

    pub fn result_memory_budget_bytes(&self) -> usize {
        self.result_memory_budget_bytes
            .clamp(1024 * 1024, 2 * 1024 * 1024 * 1024)
    }

    /// UI-facing view of the byte budget in whole MiB (rounded). The stored
    /// preference stays in bytes for on-disk compatibility.
    pub fn result_memory_budget_mib(&self) -> usize {
        const MIB: usize = 1024 * 1024;
        (self.result_memory_budget_bytes() + MIB / 2) / MIB
    }

    /// Stores a MiB value from the UI as bytes, clamped to 1..=2048 MiB so
    /// the stored value stays inside the byte accessor's clamp range.
    pub fn set_result_memory_budget_mib(&mut self, mib: usize) {
        self.result_memory_budget_bytes = mib.clamp(1, 2048) * 1024 * 1024;
    }

    pub fn window_rows(&self) -> usize {
        self.window_rows.clamp(200, 100_000)
    }

    pub fn page_margin_rows(&self) -> usize {
        self.page_margin_rows.clamp(50, self.window_rows() / 2)
    }

    pub fn page_chunk_rows(&self) -> usize {
        self.page_chunk_rows.clamp(100, self.window_rows())
    }

    pub fn result_run_history_runs(&self) -> usize {
        self.result_run_history_runs.clamp(0, 20)
    }

    pub fn result_run_history_enabled(&self) -> bool {
        self.result_run_history_enabled
    }

    pub fn result_run_history_capture(&self) -> RunHistoryCaptureMode {
        self.result_run_history_capture
    }

    pub fn load() -> Self {
        #[cfg(target_arch = "wasm32")]
        return wasm_load_json("performance-preferences.json");
        #[cfg(not(target_arch = "wasm32"))]
        {
            if let Some(cached) = performance_prefs_cache().read().as_ref() {
                return *cached;
            }
            let prefs = Self::load_from_disk();
            *performance_prefs_cache().write() = Some(prefs);
            prefs
        }
    }

    pub fn save(&self) {
        #[cfg(target_arch = "wasm32")]
        return wasm_save_json("performance-preferences.json", self);
        #[cfg(not(target_arch = "wasm32"))]
        {
            let path = performance_path();
            if let Ok(data) = crate::config_version::to_vec_pretty(self) {
                let _ = crate::fs_safe::write_atomic(&path, &data);
            }
            *performance_prefs_cache().write() = Some(*self);
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn load_from_disk() -> Self {
        let path = performance_path();
        match std::fs::read(&path) {
            Ok(data) => serde_json::from_slice(&data).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Clears the in-memory cache so the next [`Self::load`] re-reads disk.
    #[cfg(test)]
    pub(crate) fn invalidate_cache() {
        *performance_prefs_cache().write() = None;
    }
}

#[cfg(not(target_arch = "wasm32"))]
static PERFORMANCE_PREFS_CACHE: OnceLock<RwLock<Option<PerformancePreferences>>> = OnceLock::new();

#[cfg(not(target_arch = "wasm32"))]
fn performance_prefs_cache() -> &'static RwLock<Option<PerformancePreferences>> {
    PERFORMANCE_PREFS_CACHE.get_or_init(|| RwLock::new(None))
}

// ============================================================================
// Query History Preferences
// ============================================================================

/// Per-tab query version history: when versions seal, whether result sets are
/// captured with them, and the storage caps that bound the on-disk store.
/// Mirrors the query version history feature brief.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct QueryHistoryPreferences {
    /// Master switch: when off, nothing seals and the panel stays empty.
    pub enabled: bool,
    /// Seconds of editing idle time before an edit burst seals as a version.
    pub idle_seal_secs: u64,
    /// Capture each run's result sets (binary-encoded + gzipped) with its
    /// version so a restore can re-render them read-only.
    pub store_results: bool,
    /// Result payloads larger than this are skipped, never truncated.
    pub max_results_bytes_per_version: u64,
    /// Oldest versions are evicted beyond this count (payloads first).
    pub max_versions: usize,
    /// Tab-level cap on stored result payload bytes.
    pub max_results_bytes_per_tab: u64,
    /// File-backed tabs: interleave read-only git local history in the panel.
    pub show_git_history: bool,
    /// Execution-log entries older than this many days are removed when the
    /// history browser opens. Zero disables age-based retention.
    pub execution_retention_days: u32,
    /// Newest execution-log entries retained. Pinned copies live separately
    /// and are never removed by this cap.
    pub execution_max_entries: usize,
}

impl Default for QueryHistoryPreferences {
    fn default() -> Self {
        // Version-history storage is native-only (`query_version_history` is
        // `#![cfg(not(wasm))]`); mirror its caps here so the preference
        // defaults still resolve in the browser build, where the feature is
        // inert.
        #[cfg(not(target_arch = "wasm32"))]
        use crate::query_version_history::{
            DEFAULT_MAX_RESULTS_BYTES_PER_TAB, DEFAULT_MAX_RESULTS_BYTES_PER_VERSION,
            DEFAULT_MAX_VERSIONS,
        };
        #[cfg(target_arch = "wasm32")]
        const DEFAULT_MAX_VERSIONS: usize = 100;
        #[cfg(target_arch = "wasm32")]
        const DEFAULT_MAX_RESULTS_BYTES_PER_VERSION: u64 = 1024 * 1024;
        #[cfg(target_arch = "wasm32")]
        const DEFAULT_MAX_RESULTS_BYTES_PER_TAB: u64 = 25 * 1024 * 1024;
        Self {
            enabled: true,
            idle_seal_secs: 5,
            store_results: false,
            max_results_bytes_per_version: DEFAULT_MAX_RESULTS_BYTES_PER_VERSION,
            max_versions: DEFAULT_MAX_VERSIONS,
            max_results_bytes_per_tab: DEFAULT_MAX_RESULTS_BYTES_PER_TAB,
            show_git_history: false,
            execution_retention_days: 90,
            execution_max_entries: 10_000,
        }
    }
}

impl QueryHistoryPreferences {
    /// Clamped: the debounce must never be zero (a seal per keystroke).
    pub fn idle_seal_secs(&self) -> u64 {
        self.idle_seal_secs.clamp(1, 3600)
    }

    pub fn max_versions(&self) -> usize {
        self.max_versions.clamp(1, 10_000)
    }

    pub fn execution_retention_days(&self) -> u32 {
        self.execution_retention_days.min(3_650)
    }

    pub fn execution_max_entries(&self) -> usize {
        self.execution_max_entries.clamp(100, 100_000)
    }

    pub fn max_results_bytes_per_version(&self) -> u64 {
        self.max_results_bytes_per_version
            .clamp(1024, 256 * 1024 * 1024)
    }

    pub fn max_results_bytes_per_tab(&self) -> u64 {
        self.max_results_bytes_per_tab
            .clamp(1024, 1024 * 1024 * 1024)
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn caps(&self) -> crate::query_version_history::HistoryCaps {
        crate::query_version_history::HistoryCaps {
            max_versions: self.max_versions(),
            max_results_bytes_per_tab: self.max_results_bytes_per_tab(),
            max_results_bytes_per_version: self.max_results_bytes_per_version(),
        }
    }

    pub fn load() -> Self {
        #[cfg(target_arch = "wasm32")]
        return wasm_load_json("query-history-preferences.json");
        #[cfg(not(target_arch = "wasm32"))]
        {
            let mut path = preferences_path();
            path.push("query-history-preferences.json");
            match std::fs::read(&path) {
                Ok(data) => serde_json::from_slice(&data).unwrap_or_default(),
                Err(_) => Self::default(),
            }
        }
    }

    pub fn save(&self) {
        #[cfg(target_arch = "wasm32")]
        return wasm_save_json("query-history-preferences.json", self);
        #[cfg(not(target_arch = "wasm32"))]
        {
            let mut path = preferences_path();
            path.push("query-history-preferences.json");
            if let Ok(data) = crate::config_version::to_vec_pretty(self) {
                let _ = crate::fs_safe::write_atomic(&path, &data);
            }
        }
    }
}

// ============================================================================
// Result Cache Preferences
// ============================================================================

/// Per-tab last-result cache written after each run and rehydrated on launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ResultCachePreferences {
    /// Master switch: when off nothing is written and launch shows empty results.
    pub enabled: bool,
    /// Result sets with more rows than this are not cached (never truncated).
    pub max_rows: usize,
    /// Compressed payload cap per tab in bytes.
    pub max_bytes: u64,
}

impl Default for ResultCachePreferences {
    fn default() -> Self {
        Self {
            enabled: true,
            max_rows: 10_000,
            max_bytes: 32 * 1024 * 1024,
        }
    }
}

impl ResultCachePreferences {
    pub fn max_rows(&self) -> usize {
        self.max_rows.clamp(100, 1_000_000)
    }

    pub fn max_bytes(&self) -> u64 {
        self.max_bytes.clamp(1024 * 1024, 1024 * 1024 * 1024)
    }

    pub fn load() -> Self {
        #[cfg(target_arch = "wasm32")]
        return Self::default();
        #[cfg(not(target_arch = "wasm32"))]
        {
            let mut path = preferences_path();
            path.push("result-cache-preferences.json");
            match std::fs::read(&path) {
                Ok(data) => serde_json::from_slice(&data).unwrap_or_default(),
                Err(_) => Self::default(),
            }
        }
    }

    pub fn save(&self) {
        #[cfg(target_arch = "wasm32")]
        return;
        #[cfg(not(target_arch = "wasm32"))]
        {
            let mut path = preferences_path();
            path.push("result-cache-preferences.json");
            if let Ok(data) = crate::config_version::to_vec_pretty(self) {
                let _ = crate::fs_safe::write_atomic(&path, &data);
            }
        }
    }
}

// ============================================================================
// SQL Library Preferences
// ============================================================================

/// Folder that the sidebar Library panel mirrors, plus whether file rows show
/// the optional front-matter connection hint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SqlLibraryPreferences {
    /// Absolute path of the library root. `None` uses the app-data default.
    pub root: Option<String>,
    /// When true, Library rows show the `-- sqlly:` connection name if present.
    pub show_front_matter_hint: bool,
}

impl Default for SqlLibraryPreferences {
    fn default() -> Self {
        Self {
            root: None,
            show_front_matter_hint: true,
        }
    }
}

impl SqlLibraryPreferences {
    pub fn load() -> Self {
        #[cfg(target_arch = "wasm32")]
        return wasm_load_json("sql-library-preferences.json");
        #[cfg(not(target_arch = "wasm32"))]
        {
            let mut path = preferences_path();
            path.push("sql-library-preferences.json");
            match std::fs::read(&path) {
                Ok(data) => serde_json::from_slice(&data).unwrap_or_default(),
                Err(_) => Self::default(),
            }
        }
    }

    pub fn save(&self) {
        #[cfg(target_arch = "wasm32")]
        return wasm_save_json("sql-library-preferences.json", self);
        #[cfg(not(target_arch = "wasm32"))]
        {
            let mut path = preferences_path();
            path.push("sql-library-preferences.json");
            if let Ok(data) = crate::config_version::to_vec_pretty(self) {
                let _ = crate::fs_safe::write_atomic(&path, &data);
            }
        }
    }

    /// `root` when it names an existing directory; otherwise the app-data library folder.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn effective_root(&self) -> PathBuf {
        if let Some(root) = self.root.as_deref().map(PathBuf::from)
            && root.is_dir()
        {
            return root;
        }
        crate::sql_library_model::default_library_root()
    }
}

// ============================================================================
// Map Preferences
// ============================================================================

/// Whether the map view may download OpenStreetMap tiles. `Ask` until the
/// user answers the first-use prompt (or flips the Settings toggle).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MapTileChoice {
    #[default]
    Ask,
    Allowed,
    Declined,
}

/// Map view settings (results "Show on Map…" and the value viewer's Map mode).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct MapPreferences {
    /// Tile download consent; never contact the tile server unless `Allowed`.
    pub tiles: MapTileChoice,
}

impl MapPreferences {
    pub fn load() -> Self {
        #[cfg(target_arch = "wasm32")]
        return wasm_load_json("map-preferences.json");
        #[cfg(not(target_arch = "wasm32"))]
        {
            let path = preferences_path().join("map-preferences.json");
            match std::fs::read(&path) {
                Ok(data) => serde_json::from_slice(&data).unwrap_or_default(),
                Err(_) => Self::default(),
            }
        }
    }

    pub fn save(&self) {
        #[cfg(target_arch = "wasm32")]
        return wasm_save_json("map-preferences.json", self);
        #[cfg(not(target_arch = "wasm32"))]
        {
            let path = preferences_path().join("map-preferences.json");
            if let Ok(data) = crate::config_version::to_vec_pretty(self) {
                let _ = crate::fs_safe::write_atomic(&path, &data);
            }
        }
    }
}

// ============================================================================
// Transaction Preferences
// ============================================================================

/// Idle auto-rollback for tabs in Manual commit mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TransactionPreferences {
    /// Seconds of idle time before an open manual transaction is rolled back.
    pub idle_rollback_secs: u64,
}

impl Default for TransactionPreferences {
    fn default() -> Self {
        Self {
            idle_rollback_secs: 300,
        }
    }
}

impl TransactionPreferences {
    pub fn idle_rollback_secs(&self) -> u64 {
        self.idle_rollback_secs.clamp(30, 86_400)
    }

    pub fn load() -> Self {
        #[cfg(target_arch = "wasm32")]
        return wasm_load_json("transaction-preferences.json");
        #[cfg(not(target_arch = "wasm32"))]
        {
            let mut path = preferences_path();
            path.push("transaction-preferences.json");
            match std::fs::read(&path) {
                Ok(data) => serde_json::from_slice(&data).unwrap_or_default(),
                Err(_) => Self::default(),
            }
        }
    }

    pub fn save(&self) {
        #[cfg(target_arch = "wasm32")]
        return wasm_save_json("transaction-preferences.json", self);
        #[cfg(not(target_arch = "wasm32"))]
        {
            let mut path = preferences_path();
            path.push("transaction-preferences.json");
            if let Ok(data) = crate::config_version::to_vec_pretty(self) {
                let _ = crate::fs_safe::write_atomic(&path, &data);
            }
        }
    }
}

// ============================================================================
// Editor Preferences
// ============================================================================

// The editor/formatter preference types and the SQL formatter itself live in
// the `sqlly-sqlformat` crate so the LSP can format too; re-exported here so
// every `crate::preferences::X` path keeps working (serde shapes unchanged).
/// Load the persisted formatter settings (`sql-formatting.json`). Lives here
/// (not on the type, which moved to `sqlly-sqlformat`) because the storage
/// location is the app's concern.
///
/// Cached in memory after the first read, like [`PerformancePreferences`]:
/// the editor consults these on every Enter (format as you go) and every
/// accepted completion, which must not cost a disk read and a JSON parse.
/// [`save_sql_formatting_preferences`] refreshes the cache.
pub fn load_sql_formatting_preferences() -> SqlFormattingPreferences {
    #[cfg(target_arch = "wasm32")]
    return wasm_load_json("sql-formatting.json");
    #[cfg(not(target_arch = "wasm32"))]
    {
        if let Some(cached) = sql_formatting_prefs_cache().read().as_ref() {
            return cached.clone();
        }
        let prefs = load_sql_formatting_preferences_from_disk();
        *sql_formatting_prefs_cache().write() = Some(prefs.clone());
        prefs
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn load_sql_formatting_preferences_from_disk() -> SqlFormattingPreferences {
    let path = sql_formatting_path();
    match std::fs::read(&path) {
        Ok(data) => serde_json::from_slice(&data).unwrap_or_default(),
        Err(_) => SqlFormattingPreferences::default(),
    }
}

pub fn save_sql_formatting_preferences(prefs: &SqlFormattingPreferences) {
    #[cfg(target_arch = "wasm32")]
    return wasm_save_json("sql-formatting.json", prefs);
    #[cfg(not(target_arch = "wasm32"))]
    {
        let path = sql_formatting_path();
        if let Ok(data) = crate::config_version::to_vec_pretty(prefs) {
            let _ = crate::fs_safe::write_atomic(&path, &data);
        }
        *sql_formatting_prefs_cache().write() = Some(prefs.clone());
    }
}

/// Clears the in-memory formatter settings so the next load re-reads disk.
#[cfg(all(test, not(target_arch = "wasm32")))]
pub(crate) fn invalidate_sql_formatting_cache() {
    *sql_formatting_prefs_cache().write() = None;
}

#[cfg(not(target_arch = "wasm32"))]
static SQL_FORMATTING_PREFS_CACHE: OnceLock<RwLock<Option<SqlFormattingPreferences>>> =
    OnceLock::new();

#[cfg(not(target_arch = "wasm32"))]
fn sql_formatting_prefs_cache() -> &'static RwLock<Option<SqlFormattingPreferences>> {
    SQL_FORMATTING_PREFS_CACHE.get_or_init(|| RwLock::new(None))
}

pub use sqlly_lang::sqlformat::{
    BooleanOperatorPlacement, CaseExpressionStyle, ClauseBodyPlacement, CommaStyle, CommentStyle,
    CteStyle, DdlStyle, DmlStyle, FunctionArgStyle, FunctionCase, IdentifierCase,
    IdentifierQuoting, InListStyle, IndentStyle, JoinLayout, KeywordCase, KeywordGroupCasing,
    MajorClausePlacement, OnClausePlacement, OrderGroupLayout, Preset, SelectListStyle,
    SemicolonStyle, SqlDialect, SqlFormattingPreferences, SubqueryStyle, ValuesStyle,
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[derive(Default)]
pub enum IntelliSenseSource {
    BuiltIn,
    #[default]
    RustEngine,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[derive(Default)]
pub enum AliasGeneration {
    #[default]
    CapitalLettersLowercase,
    AutoBasedOnJoin,
    DoNotSuggest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum AutomagicCommaPlacement {
    #[default]
    Trailing,
    Leading,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct EditorPreferences {
    pub show_line_numbers: bool,
    /// Draw a hairline frame around the statement under the caret.
    #[serde(default = "default_true")]
    pub show_statement_frame: bool,
    /// Show schema hover cards for tables, columns, and routines.
    #[serde(default = "default_true")]
    pub show_hover_info: bool,
    /// Show parameter help while the caret is inside a call's parentheses.
    #[serde(default = "default_true")]
    pub show_signature_help: bool,
    /// Paint dim `column:` labels before each value in INSERT … VALUES.
    #[serde(default = "default_true")]
    pub show_insert_value_hints: bool,
    /// Opt-in Vim-style modal editing (Normal / Insert / Visual). Off by
    /// default; when on, new editors start in Normal mode.
    #[serde(default)]
    pub vim_mode: bool,
    pub indent_style: IndentStyle,
    pub spaces_per_tab_raw: i32,
    pub auto_spacing: bool,
    pub automagic_select_commas: bool,
    pub automagic_group_by_commas: bool,
    pub automagic_order_by_commas: bool,
    pub automagic_comma_placement: AutomagicCommaPlacement,
    /// Insert a missing `AND` when a new predicate starts on a fresh line in
    /// WHERE or a JOIN's ON. Opt-in like the comma repairs.
    #[serde(default)]
    pub automagic_where_connectors: bool,
    pub alias_generation: AliasGeneration,
    pub include_schema_in_alias: bool,
    /// Accepting a table/view completion after FROM or JOIN writes the
    /// derived alias with it (`Orders AS o`). When off, the alias is offered
    /// as a one-tap follow-up suggestion that a second acceptance inserts.
    /// On by default; replaces the never-surfaced
    /// `autoAliasOnTableCompletion` key (default off), which is ignored.
    #[serde(default = "default_true")]
    pub auto_add_table_alias: bool,
    /// Write the alias with the `AS` keyword (`Orders AS o`) rather than
    /// bare (`Orders o`). Applies to both alias insertion modes.
    #[serde(default = "default_true")]
    pub alias_as_keyword: bool,
    /// Camel-hump / acronym completion matching (`SoTa` → `SomeTable`).
    /// When off, completions match case-insensitive prefixes and substrings.
    /// Pushed to the engine by [`EditorPreferences::apply_match_options`].
    #[serde(default = "default_true")]
    pub hump_matching: bool,
    /// Search schema-qualified names too, so `dbom` finds `dbo.Make`.
    #[serde(default = "default_true")]
    pub schema_in_search: bool,
    pub intellisense_source: IntelliSenseSource,
    /// Pre-tick "Include all" in the Improve IntelliSense feedback dialog so
    /// the full data model (names only, never data) is offered by default.
    pub feedback_include_full_data_model: bool,
    /// Default visibility of each object kind in the Improve IntelliSense
    /// dialog's disclosure list. Adjustable per report in the dialog itself;
    /// purely list visibility — a picked table always sends all its columns.
    #[serde(default)]
    pub feedback_show_procedures: bool,
    #[serde(default = "default_true")]
    pub feedback_show_views: bool,
    #[serde(default)]
    pub feedback_show_functions: bool,
    #[serde(default = "default_true")]
    pub feedback_show_tables: bool,
    #[serde(default)]
    pub feedback_show_columns: bool,
    #[serde(default)]
    pub feedback_show_types: bool,
    /// Which kinds of words the editor draws in bold.
    #[serde(default)]
    pub syntax_bold: SyntaxBoldPreferences,
    /// Word movement (Ctrl+Left/Right; Option on macOS) also stops at each
    /// capital letter inside a word: `Sales|Order|Line|Id`. Off by default.
    #[serde(default)]
    pub camel_hump_word_movement: bool,
    /// Selecting text softly highlights every other occurrence of it in the
    /// buffer (VS Code / Sublime style). On by default.
    #[serde(default = "default_true")]
    pub highlight_occurrences: bool,
    /// `:name`, `?`, and `$1` placeholders written in the SQL get a row in
    /// the Parameters strip, and Run fills in the entered values. On by
    /// default; off, only `-- sqlly param:` directives declare parameters.
    #[serde(default = "default_true")]
    pub detect_query_placeholders: bool,
    /// Natural-language dates in date comparisons (Ctrl+Shift+Space after
    /// `OrderDate =`): how phrases are read and what they are written as.
    #[serde(default)]
    pub natural_dates: NaturalDatePreferences,
}

/// The first day of the week, for "this week" / "last week".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum WeekStart {
    #[default]
    Sunday,
    Monday,
}

/// Editor ▸ IntelliSense ▸ Date & time convenience.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct NaturalDatePreferences {
    /// Ctrl+Shift+Space after a date comparison reads a typed phrase
    /// ("last wed at 3", "november") instead of looking up column values.
    pub enabled: bool,
    /// A period ("today", "last week") becomes a range for `=` / `<>`
    /// rather than its first instant.
    pub convert_ranges: bool,
    pub range_style: sqlly_intel::natural_date::RangeStyle,
    /// Which one an unqualified "wed" / "november" means.
    pub prefer: sqlly_intel::natural_date::DatePreference,
    pub week_start: WeekStart,
    /// What "at 3" means without am/pm.
    pub bare_hour: sqlly_intel::natural_date::BareHour,
    /// Where "ytd", "past 7 days" and the like end.
    pub to_date_end: sqlly_intel::natural_date::ToDateEnd,
    /// 1–12; the month a fiscal year starts in ("this fiscal year", "fytd").
    pub fiscal_year_start_month: u32,
    /// Keep the typed phrase in a `--sqlly date:` comment after the literal,
    /// so the date can be refreshed later.
    pub add_marker: bool,
    /// Show the refresh button on marker comments and the "Refresh dates"
    /// chip on statements that have them.
    pub show_refresh_buttons: bool,
    /// Re-evaluate every marked date in a statement right before it runs,
    /// so a saved "yesterday" query always means yesterday.
    pub refresh_before_run: bool,
    /// Treat columns named like dates (`OrderDate`, `created_at`) as dates
    /// when the data model doesn't know their type.
    pub guess_from_column_names: bool,
}

impl Default for NaturalDatePreferences {
    fn default() -> Self {
        Self {
            enabled: true,
            convert_ranges: true,
            range_style: sqlly_intel::natural_date::RangeStyle::Between,
            prefer: sqlly_intel::natural_date::DatePreference::Past,
            week_start: WeekStart::Sunday,
            bare_hour: sqlly_intel::natural_date::BareHour::BusinessHours,
            to_date_end: sqlly_intel::natural_date::ToDateEnd::EndOfToday,
            fiscal_year_start_month: 1,
            add_marker: true,
            show_refresh_buttons: true,
            refresh_before_run: false,
            guess_from_column_names: true,
        }
    }
}

impl NaturalDatePreferences {
    /// The parser's options for these preferences.
    pub fn parse_options(&self) -> sqlly_intel::natural_date::DateParseOptions {
        sqlly_intel::natural_date::DateParseOptions {
            week_start: match self.week_start {
                WeekStart::Sunday => chrono::Weekday::Sun,
                WeekStart::Monday => chrono::Weekday::Mon,
            },
            prefer: self.prefer,
            bare_hour: self.bare_hour,
            to_date_end: self.to_date_end,
            fiscal_year_start_month: self.fiscal_year_start_month.clamp(1, 12),
        }
    }

    /// The predicate writer's options for these preferences.
    pub fn predicate_options(&self) -> sqlly_intel::natural_date::PredicateOptions {
        sqlly_intel::natural_date::PredicateOptions {
            convert_ranges: self.convert_ranges,
            range_style: self.range_style,
        }
    }
}

/// Bold weight per highlighted word kind in the SQL editor. Keywords,
/// literals, and built-in function names are bold by default; names that
/// resolve against the connection's data model are opt-in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SyntaxBoldPreferences {
    pub keywords: bool,
    /// Strings, numbers, and literal constants (`NULL`, `TRUE`, date parts).
    pub literals: bool,
    /// Built-in function names.
    pub functions: bool,
    pub procedures: bool,
    pub schemas: bool,
    pub user_functions: bool,
    /// Tables and views.
    pub tables: bool,
    pub user_types: bool,
    /// Built-in data types (`INT`, `VARCHAR`, …).
    pub system_types: bool,
}

impl Default for SyntaxBoldPreferences {
    fn default() -> Self {
        Self {
            keywords: true,
            literals: true,
            functions: true,
            procedures: false,
            schemas: false,
            user_functions: false,
            tables: false,
            user_types: false,
            system_types: false,
        }
    }
}

impl SyntaxBoldPreferences {
    /// Whether any bolding depends on data-model names.
    pub fn uses_data_model(&self) -> bool {
        self.procedures || self.schemas || self.user_functions || self.tables || self.user_types
    }
}

impl Default for EditorPreferences {
    fn default() -> Self {
        Self {
            show_line_numbers: true,
            show_statement_frame: true,
            show_hover_info: true,
            show_signature_help: true,
            show_insert_value_hints: true,
            vim_mode: false,
            indent_style: IndentStyle::Tabs,
            spaces_per_tab_raw: 4,
            auto_spacing: true,
            automagic_select_commas: false,
            automagic_group_by_commas: false,
            automagic_order_by_commas: false,
            automagic_comma_placement: AutomagicCommaPlacement::Trailing,
            automagic_where_connectors: false,
            alias_generation: AliasGeneration::CapitalLettersLowercase,
            include_schema_in_alias: false,
            auto_add_table_alias: true,
            alias_as_keyword: true,
            hump_matching: true,
            schema_in_search: true,
            intellisense_source: IntelliSenseSource::RustEngine,
            feedback_include_full_data_model: false,
            feedback_show_procedures: false,
            feedback_show_views: true,
            feedback_show_functions: false,
            feedback_show_tables: true,
            feedback_show_columns: false,
            feedback_show_types: false,
            syntax_bold: SyntaxBoldPreferences::default(),
            camel_hump_word_movement: false,
            highlight_occurrences: true,
            detect_query_placeholders: true,
            natural_dates: NaturalDatePreferences::default(),
        }
    }
}

impl EditorPreferences {
    /// The feedback dialog's default kind-visibility toggles, as one value.
    pub fn feedback_kind_visibility(&self) -> crate::feedback_tree::KindVisibility {
        crate::feedback_tree::KindVisibility {
            procedures: self.feedback_show_procedures,
            views: self.feedback_show_views,
            functions: self.feedback_show_functions,
            tables: self.feedback_show_tables,
            columns: self.feedback_show_columns,
            types: self.feedback_show_types,
        }
    }

    pub fn spaces_per_tab(&self) -> i32 {
        self.spaces_per_tab_raw.clamp(1, 16)
    }

    pub fn indent_unit(&self) -> String {
        match self.indent_style {
            IndentStyle::Tabs => "\t".to_string(),
            IndentStyle::Spaces => " ".repeat(self.spaces_per_tab() as usize),
        }
    }

    /// Apply the completion-matching settings to the IntelliSense engine.
    /// They are process-wide, so every editor and connection shares them.
    pub fn apply_match_options(&self) {
        sqlly_intel::matching::set_match_options(sqlly_intel::matching::MatchOptions {
            hump_matching: self.hump_matching,
            schema_in_search: self.schema_in_search,
        });
    }

    /// `table` followed by `alias`, with or without `AS` per the user's choice.
    pub fn aliased_table(&self, table: &str, alias: &str) -> String {
        if self.alias_as_keyword {
            format!("{table} AS {alias}")
        } else {
            format!("{table} {alias}")
        }
    }

    pub fn alias_suggestion(&self, raw_name: &str) -> Option<String> {
        let style = match self.alias_generation {
            AliasGeneration::DoNotSuggest => return None,
            AliasGeneration::CapitalLettersLowercase => {
                sqlly_intel::intellisense::AliasStyle::CapitalLetters {
                    include_schema: self.include_schema_in_alias,
                }
            }
            AliasGeneration::AutoBasedOnJoin => sqlly_intel::intellisense::AliasStyle::FirstLetter,
        };
        sqlly_intel::intellisense::suggest_alias(raw_name, style, &[])
    }

    #[allow(dead_code)] // Retained for future use.
    pub fn capital_letter_alias(raw_name: &str, include_schema: bool) -> Option<String> {
        if !raw_name.chars().any(|ch| ch.is_uppercase()) {
            return None;
        }
        sqlly_intel::intellisense::suggest_alias(
            raw_name,
            sqlly_intel::intellisense::AliasStyle::CapitalLetters { include_schema },
            &[],
        )
    }

    pub fn load() -> Self {
        #[cfg(target_arch = "wasm32")]
        return wasm_load_json("editor-preferences.json");
        #[cfg(not(target_arch = "wasm32"))]
        {
            let path = editor_prefs_path();
            match std::fs::read(&path) {
                Ok(data) => serde_json::from_slice(&data).unwrap_or_default(),
                Err(_) => Self::default(),
            }
        }
    }

    pub fn save(&self) {
        #[cfg(target_arch = "wasm32")]
        return wasm_save_json("editor-preferences.json", self);
        #[cfg(not(target_arch = "wasm32"))]
        {
            let path = editor_prefs_path();
            if let Ok(data) = crate::config_version::to_vec_pretty(self) {
                let _ = crate::fs_safe::write_atomic(&path, &data);
            }
        }
    }
}

/// Persisted appearance mode choice. Mirrors `crate::theme::ThemeKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum ThemeModePreference {
    #[default]
    System,
    Light,
    Dark,
    HighContrast,
}

impl ThemeModePreference {
    pub fn to_theme(self) -> crate::theme::ThemeKind {
        match self {
            Self::System => crate::theme::ThemeKind::System,
            Self::Light => crate::theme::ThemeKind::Light,
            Self::Dark => crate::theme::ThemeKind::Dark,
            Self::HighContrast => crate::theme::ThemeKind::HighContrast,
        }
    }

    pub fn from_theme(kind: crate::theme::ThemeKind) -> Self {
        match kind {
            crate::theme::ThemeKind::System => Self::System,
            crate::theme::ThemeKind::Light => Self::Light,
            crate::theme::ThemeKind::Dark => Self::Dark,
            crate::theme::ThemeKind::HighContrast => Self::HighContrast,
        }
    }

    pub fn label(self) -> String {
        match self {
            Self::System => {
                crate::localization::text("settings.preferences.theme_mode_preference.label.system")
            }
            Self::Light => {
                crate::localization::text("settings.preferences.theme_mode_preference.label.light")
            }
            Self::Dark => {
                crate::localization::text("settings.preferences.theme_mode_preference.label.dark")
            }
            Self::HighContrast => crate::localization::text(
                "settings.preferences.theme_mode_preference.label.high_contrast",
            ),
        }
    }

    pub const MODE_PILLS: [Self; 3] = [Self::Light, Self::Dark, Self::System];

    #[allow(dead_code)]
    pub const ALL: [Self; 4] = [Self::System, Self::Light, Self::Dark, Self::HighContrast];
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AppearancePreferences {
    /// Stable application language code (`en` or `es`).
    pub language: String,
    /// Optional nested translation catalog whose values override `language`.
    pub custom_translation_file: Option<String>,
    pub interface_font: String,
    pub interface_font_size: f32,
    pub editor_font: String,
    pub editor_font_size: f32,
    /// Result grid, pivot, and chart text. [`RESULTS_FONT_DEFAULT`] keeps the
    /// grid's platform monospace (Menlo / Consolas / DejaVu Sans Mono).
    pub results_font: String,
    /// Result text size. `None` follows the Results → Grid density preset;
    /// a set size also scales that preset's row and header heights.
    pub results_font_size: Option<f32>,
    pub theme_mode: ThemeModePreference,
    /// Palette used when the resolved mode is light.
    pub light_theme: crate::theme::ColorTheme,
    /// Palette used when the resolved mode is dark.
    pub dark_theme: crate::theme::ColorTheme,
    /// Keep the free-tier "supporter" status-bar/banner message visible even
    /// after paying. Off by default: paying makes the message disappear unless
    /// the user opts back in here. Ignored while unpaid (the message is driven
    /// by the day gate then). See [`crate::license`].
    pub keep_supporter_message: bool,
}

/// `results_font` value meaning "the grid's built-in platform monospace".
pub const RESULTS_FONT_DEFAULT: &str = "Default";

/// Default point size for both the interface and editor fonts. The browser
/// build renders on a canvas whose logical pixels run larger than native
/// windows, so it defaults smaller.
const DEFAULT_FONT_SIZE: f32 = if cfg!(target_arch = "wasm32") {
    10.0
} else {
    13.0
};

impl Default for AppearancePreferences {
    fn default() -> Self {
        Self {
            language: "en".to_string(),
            custom_translation_file: None,
            interface_font: "System".to_string(),
            interface_font_size: DEFAULT_FONT_SIZE,
            editor_font: crate::DEFAULT_EDITOR_FONT.to_string(),
            editor_font_size: DEFAULT_FONT_SIZE,
            results_font: RESULTS_FONT_DEFAULT.to_string(),
            results_font_size: None,
            theme_mode: ThemeModePreference::System,
            light_theme: crate::theme::ColorTheme::Light,
            dark_theme: crate::theme::ColorTheme::Dark,
            keep_supporter_message: false,
        }
    }
}

impl AppearancePreferences {
    /// Push the persisted theme choice into the global theme state.
    pub fn apply_theme(&self) {
        let (mode, dark) = self.resolved_mode_and_dark_theme();
        crate::theme::set_kind(mode.to_theme());
        crate::theme::set_light_theme(self.light_theme.sanitize_light());
        crate::theme::set_dark_theme(dark);
    }

    /// Old files stored High Contrast as a fourth *mode*. It is now a named
    /// palette; this maps that legacy choice onto Dark + High Contrast Dark.
    fn resolved_mode_and_dark_theme(&self) -> (ThemeModePreference, crate::theme::ColorTheme) {
        if self.theme_mode == ThemeModePreference::HighContrast {
            (
                ThemeModePreference::Dark,
                crate::theme::ColorTheme::HighContrastDark,
            )
        } else {
            (self.theme_mode, self.dark_theme.sanitize_dark())
        }
    }

    pub(crate) fn migrate_high_contrast_mode(&mut self) {
        if self.theme_mode == ThemeModePreference::HighContrast {
            self.theme_mode = ThemeModePreference::Dark;
            self.dark_theme = crate::theme::ColorTheme::HighContrastDark;
        }
        self.light_theme = self.light_theme.sanitize_light();
        self.dark_theme = self.dark_theme.sanitize_dark();
    }

    /// View → Theme: High Contrast. Picks the High Contrast palette for the
    /// current Light/Dark/System mode without introducing a fourth painted mode.
    pub fn apply_high_contrast_shortcut(&mut self) {
        self.migrate_high_contrast_mode();
        match self.theme_mode {
            ThemeModePreference::Light => {
                self.light_theme = crate::theme::ColorTheme::HighContrastLight;
            }
            ThemeModePreference::Dark => {
                self.dark_theme = crate::theme::ColorTheme::HighContrastDark;
            }
            ThemeModePreference::System => {
                self.light_theme = crate::theme::ColorTheme::HighContrastLight;
                self.dark_theme = crate::theme::ColorTheme::HighContrastDark;
            }
            ThemeModePreference::HighContrast => {
                self.theme_mode = ThemeModePreference::Dark;
                self.dark_theme = crate::theme::ColorTheme::HighContrastDark;
            }
        }
    }

    pub fn load() -> Self {
        #[cfg(target_arch = "wasm32")]
        let mut prefs: Self = wasm_load_json("appearance-preferences.json");
        #[cfg(not(target_arch = "wasm32"))]
        let mut prefs: Self = {
            let path = appearance_prefs_path();
            match std::fs::read(&path) {
                Ok(data) => serde_json::from_slice(&data).unwrap_or_default(),
                Err(_) => Self::default(),
            }
        };
        prefs.migrate_high_contrast_mode();
        prefs
    }

    pub fn save(&self) {
        let _ = self.try_save();
    }

    pub fn try_save(&self) -> Result<(), String> {
        #[cfg(target_arch = "wasm32")]
        {
            let text =
                crate::config_version::to_string_pretty(self).map_err(|error| error.to_string())?;
            return crate::host_storage::store_text(
                crate::host_storage::StorageArea::Data,
                "appearance-preferences.json",
                &text,
            );
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let data =
                crate::config_version::to_vec_pretty(self).map_err(|error| error.to_string())?;
            crate::fs_safe::write_atomic(&appearance_prefs_path(), &data)
                .map_err(|error| error.to_string())
        }
    }
}

// ============================================================================
// Result Formatting Preferences
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[derive(Default)]
pub enum ResultKind {
    Date,
    Bool,
    Uuid,
    Integer,
    Decimal,
    Mask,
    Epoch,
    JsonPath,
    Template,
    Number,
    #[default]
    Text,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[derive(Default)]
pub enum ResultTextAlignment {
    #[default]
    Left,
    Center,
    Right,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[derive(Default)]
pub enum DateTimeZoneMode {
    #[default]
    AlwaysUtc,
    NativeAsStored,
    AlwaysLocal,
    AlwaysInTimeZone,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct DateFormat {
    pub format: String,
    pub time_zone_mode: DateTimeZoneMode,
    pub time_zone_identifier: String,
    pub alignment: ResultTextAlignment,
    /// Column overrides only (meaningless on `defaults.date_format`): the
    /// zone above was chosen for this column. Unpinned overrides whose mode
    /// is still the `AlwaysUtc` default follow the global Date zone — every
    /// Date override used to be saved with that default and no setting let
    /// anyone choose it, so it never expressed a choice. See
    /// [`DateFormat::follows_global_zone`].
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub zone_pinned: bool,
}

impl Default for DateFormat {
    fn default() -> Self {
        Self {
            format: "yyyy-MM-dd HH:mm:ss".to_string(),
            time_zone_mode: DateTimeZoneMode::AlwaysUtc,
            time_zone_identifier: "UTC".to_string(),
            alignment: ResultTextAlignment::Left,
            zone_pinned: false,
        }
    }
}

impl DateFormat {
    /// For a column override's date format: whether it shows values in the
    /// global (or connection) display zone rather than its own.
    pub fn follows_global_zone(&self) -> bool {
        !self.zone_pinned && self.time_zone_mode == DateTimeZoneMode::AlwaysUtc
    }

    /// The per-column zone choice this override's date format expresses.
    pub fn zone_choice(&self) -> ColumnZoneChoice {
        if self.follows_global_zone() {
            ColumnZoneChoice::FollowGlobal
        } else {
            ColumnZoneChoice::from_mode(&self.time_zone_mode)
        }
    }

    /// Apply a per-column zone choice to this override's date format.
    pub fn set_zone_choice(&mut self, choice: ColumnZoneChoice) {
        match choice.to_mode() {
            None => {
                self.time_zone_mode = DateTimeZoneMode::AlwaysUtc;
                self.zone_pinned = false;
            }
            Some(mode) => {
                self.time_zone_mode = mode;
                self.zone_pinned = true;
            }
        }
    }
}

/// The display-zone choices a Date or Epoch column override offers.
/// `AsStored` only means something for Date overrides (an epoch is an
/// absolute instant).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnZoneChoice {
    FollowGlobal,
    Utc,
    Local,
    AsStored,
    Named,
}

impl ColumnZoneChoice {
    pub const DATE: [ColumnZoneChoice; 5] = [
        ColumnZoneChoice::FollowGlobal,
        ColumnZoneChoice::Utc,
        ColumnZoneChoice::Local,
        ColumnZoneChoice::AsStored,
        ColumnZoneChoice::Named,
    ];
    pub const EPOCH: [ColumnZoneChoice; 4] = [
        ColumnZoneChoice::FollowGlobal,
        ColumnZoneChoice::Utc,
        ColumnZoneChoice::Local,
        ColumnZoneChoice::Named,
    ];

    pub fn from_mode(mode: &DateTimeZoneMode) -> Self {
        match mode {
            DateTimeZoneMode::AlwaysUtc => ColumnZoneChoice::Utc,
            DateTimeZoneMode::NativeAsStored => ColumnZoneChoice::AsStored,
            DateTimeZoneMode::AlwaysLocal => ColumnZoneChoice::Local,
            DateTimeZoneMode::AlwaysInTimeZone => ColumnZoneChoice::Named,
        }
    }

    /// `None` follows the global zone.
    pub fn to_mode(self) -> Option<DateTimeZoneMode> {
        match self {
            ColumnZoneChoice::FollowGlobal => None,
            ColumnZoneChoice::Utc => Some(DateTimeZoneMode::AlwaysUtc),
            ColumnZoneChoice::Local => Some(DateTimeZoneMode::AlwaysLocal),
            ColumnZoneChoice::AsStored => Some(DateTimeZoneMode::NativeAsStored),
            ColumnZoneChoice::Named => Some(DateTimeZoneMode::AlwaysInTimeZone),
        }
    }

    pub fn label(self) -> String {
        match self {
            ColumnZoneChoice::FollowGlobal => {
                crate::localization::text("settings.preferences.column_zone.label.follow_global")
            }
            ColumnZoneChoice::Utc => crate::localization::text("settings.mod.option.always_utc"),
            ColumnZoneChoice::Local => {
                crate::localization::text("settings.mod.option.always_local")
            }
            ColumnZoneChoice::AsStored => {
                crate::localization::text("settings.mod.option.native_as_stored")
            }
            ColumnZoneChoice::Named => {
                crate::localization::text("settings.mod.option.specific_time_zone")
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[derive(Default)]
pub enum BoolFormatStyle {
    YN,
    YesNo,
    ZeroOne,
    TF,
    #[default]
    TrueFalse,
    Custom,
}

impl BoolFormatStyle {
    pub fn literal(&self, value: bool, custom_true: &str, custom_false: &str) -> String {
        match self {
            BoolFormatStyle::YN => if value { "y" } else { "n" }.to_string(),
            BoolFormatStyle::YesNo => if value { "yes" } else { "no" }.to_string(),
            BoolFormatStyle::ZeroOne => if value { "1" } else { "0" }.to_string(),
            BoolFormatStyle::TF => if value { "t" } else { "f" }.to_string(),
            BoolFormatStyle::TrueFalse => if value { "true" } else { "false" }.to_string(),
            BoolFormatStyle::Custom => if value { custom_true } else { custom_false }.to_string(),
        }
    }

    pub fn display_title(&self) -> String {
        match self {
            BoolFormatStyle::YN => crate::localization::text(
                "settings.preferences.bool_format_style.display_title.y_n",
            ),
            BoolFormatStyle::YesNo => crate::localization::text(
                "settings.preferences.bool_format_style.display_title.yes_no",
            ),
            BoolFormatStyle::ZeroOne => "0 / 1".to_string(),
            BoolFormatStyle::TF => crate::localization::text(
                "settings.preferences.bool_format_style.display_title.t_f",
            ),
            BoolFormatStyle::TrueFalse => crate::localization::text(
                "settings.preferences.bool_format_style.display_title.true_false",
            ),
            BoolFormatStyle::Custom => crate::localization::text(
                "settings.preferences.bool_format_style.display_title.custom",
            ),
        }
    }

    pub fn all() -> [BoolFormatStyle; 6] {
        [
            BoolFormatStyle::YN,
            BoolFormatStyle::YesNo,
            BoolFormatStyle::ZeroOne,
            BoolFormatStyle::TF,
            BoolFormatStyle::TrueFalse,
            BoolFormatStyle::Custom,
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct BoolFormat {
    pub style: BoolFormatStyle,
    pub custom_true: String,
    pub custom_false: String,
    pub alignment: ResultTextAlignment,
}

impl Default for BoolFormat {
    fn default() -> Self {
        Self {
            style: BoolFormatStyle::TrueFalse,
            custom_true: "yes".to_string(),
            custom_false: "no".to_string(),
            alignment: ResultTextAlignment::Left,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct UuidFormat {
    pub show_dashes: bool,
    pub alignment: ResultTextAlignment,
}

impl Default for UuidFormat {
    fn default() -> Self {
        Self {
            show_dashes: true,
            alignment: ResultTextAlignment::Left,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[derive(Default)]
pub enum ThousandsSeparator {
    #[default]
    Comma,
    Period,
    Space,
    Apostrophe,
    None,
}

impl ThousandsSeparator {
    pub fn literal(&self) -> &'static str {
        match self {
            ThousandsSeparator::Comma => ",",
            ThousandsSeparator::Period => ".",
            ThousandsSeparator::Space => " ",
            ThousandsSeparator::Apostrophe => "'",
            ThousandsSeparator::None => "",
        }
    }

    pub fn display_title(&self) -> String {
        match self {
            ThousandsSeparator::Comma => "1,234".to_string(),
            ThousandsSeparator::Period => "1.234".to_string(),
            ThousandsSeparator::Space => "1 234".to_string(),
            ThousandsSeparator::Apostrophe => "1'234".to_string(),
            ThousandsSeparator::None => crate::localization::text(
                "settings.preferences.thousands_separator.display_title.example_1234_none",
            ),
        }
    }

    pub fn all() -> [ThousandsSeparator; 5] {
        [
            ThousandsSeparator::Comma,
            ThousandsSeparator::Period,
            ThousandsSeparator::Space,
            ThousandsSeparator::Apostrophe,
            ThousandsSeparator::None,
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[derive(Default)]
pub enum DecimalSeparator {
    #[default]
    Period,
    Comma,
}

impl DecimalSeparator {
    pub fn literal(&self) -> &'static str {
        match self {
            DecimalSeparator::Period => ".",
            DecimalSeparator::Comma => ",",
        }
    }

    pub fn display_title(&self) -> &'static str {
        match self {
            DecimalSeparator::Period => ". (1.23)",
            DecimalSeparator::Comma => ", (1,23)",
        }
    }

    pub fn all() -> [DecimalSeparator; 2] {
        [DecimalSeparator::Period, DecimalSeparator::Comma]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[derive(Default)]
pub enum NegativeFormatStyle {
    #[default]
    Sign,
    Parentheses,
}

impl NegativeFormatStyle {
    pub fn display_title(&self) -> String {
        match self {
            NegativeFormatStyle::Sign => crate::localization::text(
                "settings.preferences.negative_format_style.display_title.minus_sign_42",
            ),
            NegativeFormatStyle::Parentheses => crate::localization::text(
                "settings.preferences.negative_format_style.display_title.parentheses_42",
            ),
        }
    }

    pub fn all() -> [NegativeFormatStyle; 2] {
        [NegativeFormatStyle::Sign, NegativeFormatStyle::Parentheses]
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct MetricLabels {
    pub kilo: String,
    pub mega: String,
    pub giga: String,
    pub tera: String,
    pub peta: String,
}

impl Default for MetricLabels {
    fn default() -> Self {
        Self {
            kilo: "K".to_string(),
            mega: "M".to_string(),
            giga: "G".to_string(),
            tera: "T".to_string(),
            peta: "P".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct IntegerFormat {
    pub use_thousands_separators: bool,
    pub reduce_large_to_metric: bool,
    pub metric_threshold: i64,
    pub metric_labels: MetricLabels,
    pub metric_decimals: i32,
    pub metric_step_up_ratio: f64,
    pub negative_format: NegativeFormatStyle,
    pub show_red_for_negative: bool,
    pub alignment: ResultTextAlignment,
}

impl Default for IntegerFormat {
    fn default() -> Self {
        Self {
            use_thousands_separators: true,
            reduce_large_to_metric: false,
            metric_threshold: 1000,
            metric_labels: MetricLabels::default(),
            metric_decimals: 2,
            metric_step_up_ratio: 0.5,
            negative_format: NegativeFormatStyle::Sign,
            show_red_for_negative: false,
            alignment: ResultTextAlignment::Right,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct DecimalFormat {
    pub decimal_places: i32,
    pub use_thousands_separators: bool,
    pub decimal_separator: DecimalSeparator,
    pub thousands_separator: ThousandsSeparator,
    pub reduce_large_to_metric: bool,
    pub metric_threshold: i64,
    pub metric_labels: MetricLabels,
    pub metric_decimals: i32,
    pub metric_step_up_ratio: f64,
    pub negative_format: NegativeFormatStyle,
    pub show_red_for_negative: bool,
    pub alignment: ResultTextAlignment,
}

impl Default for DecimalFormat {
    fn default() -> Self {
        Self {
            decimal_places: 6,
            use_thousands_separators: true,
            decimal_separator: DecimalSeparator::Period,
            thousands_separator: ThousandsSeparator::Comma,
            reduce_large_to_metric: false,
            metric_threshold: 1000,
            metric_labels: MetricLabels::default(),
            metric_decimals: 2,
            metric_step_up_ratio: 0.5,
            negative_format: NegativeFormatStyle::Sign,
            show_red_for_negative: false,
            alignment: ResultTextAlignment::Right,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
#[derive(Default)]
pub struct ResultTypeDefaults {
    pub date_format: DateFormat,
    pub bool_format: BoolFormat,
    pub uuid_format: UuidFormat,
    pub integer_format: IntegerFormat,
    pub decimal_format: DecimalFormat,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ColumnFormat {
    pub kind: ResultKind,
    pub date_format: DateFormat,
    pub bool_format: BoolFormat,
    pub uuid_format: UuidFormat,
    pub integer_format: IntegerFormat,
    pub decimal_format: DecimalFormat,
    #[serde(default)]
    pub mask_format: MaskFormat,
    #[serde(default)]
    pub epoch_format: EpochFormat,
    #[serde(default)]
    pub json_path_format: JsonPathFormat,
    #[serde(default)]
    pub template_format: TemplateFormat,
    #[serde(default)]
    pub number_style_format: NumberStyleFormat,
}

impl Default for ColumnFormat {
    fn default() -> Self {
        Self {
            kind: ResultKind::Text,
            date_format: DateFormat::default(),
            bool_format: BoolFormat::default(),
            uuid_format: UuidFormat::default(),
            integer_format: IntegerFormat::default(),
            decimal_format: DecimalFormat::default(),
            mask_format: MaskFormat::default(),
            epoch_format: EpochFormat::default(),
            json_path_format: JsonPathFormat::default(),
            template_format: TemplateFormat::default(),
            number_style_format: NumberStyleFormat::default(),
        }
    }
}

impl ColumnFormat {
    pub fn is_inherit(&self) -> bool {
        self.kind == ResultKind::Text
    }

    pub fn default_for_kind(kind: ResultKind) -> Self {
        Self {
            kind,
            ..Default::default()
        }
    }
}

/// Visible prefix/suffix around a mask character. Never persist unmasked secrets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct MaskFormat {
    pub keep_prefix: usize,
    pub keep_suffix: usize,
    pub mask_char: char,
}

impl Default for MaskFormat {
    fn default() -> Self {
        Self {
            keep_prefix: 0,
            keep_suffix: 4,
            mask_char: '•',
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum EpochUnit {
    #[default]
    Auto,
    Seconds,
    Milliseconds,
}

impl EpochUnit {
    pub const ALL: [EpochUnit; 3] = [EpochUnit::Auto, EpochUnit::Seconds, EpochUnit::Milliseconds];

    pub fn label(self) -> String {
        match self {
            EpochUnit::Auto => {
                crate::localization::text("settings.preferences.epoch_unit.label.auto")
            }
            EpochUnit::Seconds => {
                crate::localization::text("settings.preferences.epoch_unit.label.seconds")
            }
            EpochUnit::Milliseconds => {
                crate::localization::text("settings.preferences.epoch_unit.label.milliseconds")
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EpochFormat {
    pub unit: EpochUnit,
    pub pattern: String,
    /// `None` follows the global Date display zone
    /// (`defaults.date_format`); `Some` pins this column to a zone. Saved as
    /// `displayZone` so an explicit UTC choice survives a reload; the older
    /// `timeZoneMode` key is still read (see the `Deserialize` impl).
    #[serde(rename = "displayZone", skip_serializing_if = "Option::is_none")]
    pub time_zone_mode: Option<DateTimeZoneMode>,
    pub time_zone_identifier: String,
}

impl EpochFormat {
    /// The per-column zone choice this epoch override expresses.
    pub fn zone_choice(&self) -> ColumnZoneChoice {
        match &self.time_zone_mode {
            None => ColumnZoneChoice::FollowGlobal,
            // "As stored" means UTC for an absolute instant.
            Some(DateTimeZoneMode::NativeAsStored) => ColumnZoneChoice::Utc,
            Some(mode) => ColumnZoneChoice::from_mode(mode),
        }
    }

    pub fn set_zone_choice(&mut self, choice: ColumnZoneChoice) {
        self.time_zone_mode = match choice {
            ColumnZoneChoice::AsStored => Some(DateTimeZoneMode::AlwaysUtc),
            other => other.to_mode(),
        };
    }
}

impl Default for EpochFormat {
    fn default() -> Self {
        Self {
            unit: EpochUnit::Auto,
            pattern: "%Y-%m-%d %H:%M:%S".to_string(),
            time_zone_mode: None,
            time_zone_identifier: "UTC".to_string(),
        }
    }
}

// Hand-written so stored preferences from before the display-time-zone work
// still load: the old `utc: false` maps to AlwaysLocal. A stored `alwaysUtc`
// under the old `timeZoneMode` key (or legacy `utc: true`) loads as "follow
// the global zone": it was the default every new epoch override was saved
// with, and no setting ever let anyone choose it, so it never expressed a
// choice. Other explicit zone modes are kept. A `displayZone` (written since
// the per-column zone picker exists) is always taken as is.
impl<'de> Deserialize<'de> for EpochFormat {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Default, Deserialize)]
        #[serde(default, rename_all = "camelCase")]
        struct Legacy {
            unit: EpochUnit,
            pattern: Option<String>,
            utc: Option<bool>,
            display_zone: Option<DateTimeZoneMode>,
            time_zone_mode: Option<DateTimeZoneMode>,
            time_zone_identifier: Option<String>,
        }
        let raw = Legacy::deserialize(deserializer)?;
        let defaults = EpochFormat::default();
        let time_zone_mode = match (raw.time_zone_mode, raw.utc) {
            _ if raw.display_zone.is_some() => raw.display_zone,
            (Some(DateTimeZoneMode::AlwaysUtc), _) => None,
            (Some(mode), _) => Some(mode),
            (None, Some(false)) => Some(DateTimeZoneMode::AlwaysLocal),
            (None, _) => None,
        };
        Ok(Self {
            unit: raw.unit,
            pattern: raw.pattern.unwrap_or(defaults.pattern),
            time_zone_mode,
            time_zone_identifier: raw
                .time_zone_identifier
                .unwrap_or(defaults.time_zone_identifier),
        })
    }
}

/// `$.a.b[0].c` — dot segments and bracket indices only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct JsonPathFormat {
    pub path: String,
    /// Shown when the path does not resolve.
    pub missing_text: String,
}

impl Default for JsonPathFormat {
    fn default() -> Self {
        Self {
            path: "$".into(),
            missing_text: String::new(),
        }
    }
}

/// `{value}` plus helpers: `{value|upper}`, `{value|trunc:20}`, `{value|prefix:$}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TemplateFormat {
    pub template: String,
}

impl Default for TemplateFormat {
    fn default() -> Self {
        Self {
            template: "{value}".into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum NumberStyle {
    #[default]
    Fixed,
    Percent,
    Bytes,
}

impl NumberStyle {
    pub const ALL: [NumberStyle; 3] =
        [NumberStyle::Fixed, NumberStyle::Percent, NumberStyle::Bytes];

    pub fn label(self) -> String {
        match self {
            NumberStyle::Fixed => {
                crate::localization::text("settings.preferences.number_style.label.fixed")
            }
            NumberStyle::Percent => {
                crate::localization::text("settings.preferences.number_style.label.percent")
            }
            NumberStyle::Bytes => {
                crate::localization::text("settings.preferences.number_style.label.bytes")
            }
        }
    }

    pub fn directive_token(self) -> &'static str {
        match self {
            NumberStyle::Fixed => "fixed",
            NumberStyle::Percent => "percent",
            NumberStyle::Bytes => "bytes",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct NumberStyleFormat {
    pub style: NumberStyle,
    pub decimals: u8,
    pub use_thousands_separators: bool,
}

impl Default for NumberStyleFormat {
    fn default() -> Self {
        Self {
            style: NumberStyle::Fixed,
            decimals: 2,
            use_thousands_separators: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ResultFormattingPreferences {
    pub defaults: ResultTypeDefaults,
    pub column_overrides: BTreeMap<String, ColumnFormat>,
    pub exports_masked: bool,
    /// Turns off the results grid's motion — cell/selection fades, dialog and
    /// scrim transitions, and the macOS-style scroll physics (rubber-band,
    /// bounce-back, wheel glide). Stored as the negative so a missing field in
    /// an existing `result-formatting.json` (serde default `false`) keeps
    /// animations on, matching the grid's own default.
    pub disable_animations: bool,
    /// Result-grid row height preset.
    pub density: GridDensity,
    /// Which edge of the results grid the Cell Details dock sits on.
    #[serde(default)]
    pub cell_detail_side: CellDetailSide,
    /// Show referenced-row labels beside foreign-key values on single-table data pages.
    #[serde(default = "default_true")]
    pub show_fk_labels: bool,
    /// Paint FK labels beside the raw key or replace the visible text.
    #[serde(default)]
    pub fk_label_mode: FkLabelMode,
    /// Widest a column grows when a run sizes it to its name and values, in
    /// characters; longer content is cut off with an ellipsis. 0 sizes every
    /// column to its full content.
    #[serde(default = "default_auto_size_max_chars")]
    pub auto_size_max_chars: u32,
    /// Opt-in: tint every result cell's background by its column's data type
    /// (numbers, dates, booleans, …). The user's own conditional-formatting
    /// rules still win; masked columns are never tinted.
    #[serde(default)]
    pub color_cells_by_type: bool,
    /// Per-kind colors for "Color cells by data type". A kind missing here
    /// (or a missing fill/text color) uses the theme's tint.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub type_colors: BTreeMap<TypeColorKind, TypeColorStyle>,
    /// Opt-in: paint JSON/XML values inside grid cells with the theme's
    /// syntax colors.
    #[serde(default)]
    pub syntax_color_structured: bool,
    /// Opt-in: show integer timestamp columns (picked by name, optionally
    /// confirmed by each value's range) as dates.
    #[serde(default)]
    pub epoch_auto_detect: EpochAutoDetect,
}

/// The kinds "Color cells by data type" distinguishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TypeColorKind {
    Text,
    Number,
    Date,
    Boolean,
    Uuid,
    Binary,
}

impl TypeColorKind {
    pub const ALL: [TypeColorKind; 6] = [
        TypeColorKind::Text,
        TypeColorKind::Number,
        TypeColorKind::Date,
        TypeColorKind::Boolean,
        TypeColorKind::Uuid,
        TypeColorKind::Binary,
    ];

    pub fn label(self) -> String {
        crate::localization::text(match self {
            TypeColorKind::Text => "settings.preferences.type_color_kind.label.text",
            TypeColorKind::Number => "settings.preferences.type_color_kind.label.number",
            TypeColorKind::Date => "settings.preferences.type_color_kind.label.date",
            TypeColorKind::Boolean => "settings.preferences.type_color_kind.label.boolean",
            TypeColorKind::Uuid => "settings.preferences.type_color_kind.label.uuid",
            TypeColorKind::Binary => "settings.preferences.type_color_kind.label.binary",
        })
    }

    /// Stable id fragment for per-kind widgets.
    pub fn key(self) -> &'static str {
        match self {
            TypeColorKind::Text => "text",
            TypeColorKind::Number => "number",
            TypeColorKind::Date => "date",
            TypeColorKind::Boolean => "boolean",
            TypeColorKind::Uuid => "uuid",
            TypeColorKind::Binary => "binary",
        }
    }
}

/// A user's colors for one data kind. The same colors apply in light and
/// dark themes, like conditional-formatting colors.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TypeColorStyle {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background: Option<sqlly_datatable::RgbaColor>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub foreground: Option<sqlly_datatable::RgbaColor>,
}

impl TypeColorStyle {
    pub fn is_empty(&self) -> bool {
        self.background.is_none() && self.foreground.is_none()
    }
}

/// "Show integer timestamps as dates".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EpochAutoDetect {
    #[default]
    Off,
    /// Integer columns whose name reads like a timestamp.
    ByName,
    /// As `ByName`, and only values that fall in 2000–2100 as epoch seconds
    /// or milliseconds; others stay numbers.
    ByNameAndRange,
}

impl EpochAutoDetect {
    pub const ALL: [EpochAutoDetect; 3] = [
        EpochAutoDetect::Off,
        EpochAutoDetect::ByName,
        EpochAutoDetect::ByNameAndRange,
    ];

    pub fn label(self) -> String {
        crate::localization::text(match self {
            EpochAutoDetect::Off => "settings.preferences.epoch_auto_detect.label.off",
            EpochAutoDetect::ByName => "settings.preferences.epoch_auto_detect.label.by_name",
            EpochAutoDetect::ByNameAndRange => {
                "settings.preferences.epoch_auto_detect.label.by_name_and_range"
            }
        })
    }
}

/// Default for [`ResultFormattingPreferences::auto_size_max_chars`].
pub const DEFAULT_AUTO_SIZE_MAX_CHARS: u32 = 30;

fn default_auto_size_max_chars() -> u32 {
    DEFAULT_AUTO_SIZE_MAX_CHARS
}

/// How foreign-key display labels are painted in the result grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum FkLabelMode {
    #[default]
    Beside,
    Replace,
}

impl FkLabelMode {
    pub const ALL: [FkLabelMode; 2] = [FkLabelMode::Beside, FkLabelMode::Replace];

    pub fn label(self) -> String {
        match self {
            FkLabelMode::Beside => {
                crate::localization::text("settings.preferences.fk_label_mode.label.beside_raw_key")
            }
            FkLabelMode::Replace => {
                crate::localization::text("settings.preferences.fk_label_mode.label.replace_key")
            }
        }
    }

    pub fn to_datatable(self) -> sqlly_datatable::LabelMode {
        match self {
            FkLabelMode::Beside => sqlly_datatable::LabelMode::Beside,
            FkLabelMode::Replace => sqlly_datatable::LabelMode::Replace,
        }
    }
}

impl Default for ResultFormattingPreferences {
    fn default() -> Self {
        Self {
            defaults: ResultTypeDefaults::default(),
            column_overrides: BTreeMap::new(),
            exports_masked: true,
            disable_animations: false,
            density: GridDensity::default(),
            cell_detail_side: CellDetailSide::default(),
            show_fk_labels: true,
            fk_label_mode: FkLabelMode::default(),
            auto_size_max_chars: DEFAULT_AUTO_SIZE_MAX_CHARS,
            color_cells_by_type: false,
            type_colors: BTreeMap::new(),
            syntax_color_structured: false,
            epoch_auto_detect: EpochAutoDetect::Off,
        }
    }
}

/// Which edge of the results grid the Cell Details panel docks to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum CellDetailSide {
    #[default]
    Right,
    Bottom,
}

impl CellDetailSide {
    pub const ALL: [CellDetailSide; 2] = [CellDetailSide::Right, CellDetailSide::Bottom];

    pub fn label(self) -> String {
        match self {
            CellDetailSide::Right => {
                crate::localization::text("settings.preferences.cell_detail_side.label.right")
            }
            CellDetailSide::Bottom => {
                crate::localization::text("settings.preferences.cell_detail_side.label.bottom")
            }
        }
    }

    pub fn as_dock_side(self) -> crate::cell_detail_model::DockSide {
        match self {
            CellDetailSide::Right => crate::cell_detail_model::DockSide::Right,
            CellDetailSide::Bottom => crate::cell_detail_model::DockSide::Bottom,
        }
    }
}

/// Result-grid row height / font size preset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum GridDensity {
    Compact,
    #[default]
    Normal,
    Comfortable,
}

/// Pixel metrics for a [`GridDensity`] preset.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GridDensityMetrics {
    pub row_height: f32,
    pub header_height: f32,
    pub font_size: f32,
}

impl GridDensity {
    pub const ALL: [GridDensity; 3] = [
        GridDensity::Compact,
        GridDensity::Normal,
        GridDensity::Comfortable,
    ];

    pub fn label(self) -> String {
        match self {
            GridDensity::Compact => {
                crate::localization::text("settings.preferences.grid_density.label.compact")
            }
            GridDensity::Normal => {
                crate::localization::text("settings.preferences.grid_density.label.normal")
            }
            GridDensity::Comfortable => {
                crate::localization::text("settings.preferences.grid_density.label.comfortable")
            }
        }
    }

    pub fn metrics(self) -> GridDensityMetrics {
        // The browser build defaults smaller across the board (see
        // [`DEFAULT_FONT_SIZE`]); the grid's density ladder shrinks to match
        // so the results pane doesn't dwarf the 10pt UI around it.
        if cfg!(target_arch = "wasm32") {
            return match self {
                GridDensity::Compact => GridDensityMetrics {
                    row_height: 16.0,
                    header_height: 22.0,
                    font_size: 9.0,
                },
                GridDensity::Normal => GridDensityMetrics {
                    row_height: 19.0,
                    header_height: 25.0,
                    font_size: 10.0,
                },
                GridDensity::Comfortable => GridDensityMetrics {
                    row_height: 23.0,
                    header_height: 28.0,
                    font_size: 11.0,
                },
            };
        }
        match self {
            GridDensity::Compact => GridDensityMetrics {
                row_height: 20.0,
                header_height: 28.0,
                font_size: 12.0,
            },
            GridDensity::Normal => GridDensityMetrics {
                row_height: 24.0,
                header_height: 32.0,
                font_size: 14.0,
            },
            GridDensity::Comfortable => GridDensityMetrics {
                row_height: 30.0,
                header_height: 36.0,
                font_size: 15.0,
            },
        }
    }
}

impl ResultFormattingPreferences {
    pub fn load() -> Self {
        #[cfg(target_arch = "wasm32")]
        return wasm_load_json("result-formatting.json");
        #[cfg(not(target_arch = "wasm32"))]
        {
            let path = result_formatting_path();
            match std::fs::read(&path) {
                Ok(data) => serde_json::from_slice(&data).unwrap_or_default(),
                Err(_) => Self::default(),
            }
        }
    }

    pub fn save(&self) {
        #[cfg(target_arch = "wasm32")]
        return wasm_save_json("result-formatting.json", self);
        #[cfg(not(target_arch = "wasm32"))]
        {
            let path = result_formatting_path();
            if let Ok(data) = crate::config_version::to_vec_pretty(self) {
                let _ = crate::fs_safe::write_atomic(&path, &data);
            }
        }
    }

    pub fn format_bool(&self, value: bool) -> String {
        let fmt = &self.defaults.bool_format;
        fmt.style
            .literal(value, &fmt.custom_true, &fmt.custom_false)
    }

    pub fn format_uuid(&self, uuid_str: &str) -> String {
        if self.defaults.uuid_format.show_dashes {
            uuid_str.to_string()
        } else {
            uuid_str.replace('-', "")
        }
    }

    /// Format an integer with the Integer defaults — the Settings sample and
    /// message counts. Shares the grid's formatter (via the same bridged
    /// number format) so the sample always shows what integer cells show.
    pub fn format_integer(&self, value: i64) -> String {
        let fmt = &self.defaults.integer_format;
        let mut s = sqlly_datatable::format::format_integer(
            value,
            &crate::grid_bridge::integer_number_format(fmt),
        );
        if fmt.show_red_for_negative && value < 0 {
            s = format!("\x1b[31m{}\x1b[0m", s);
        }
        s
    }

    /// Format a decimal with the Decimal defaults — the Settings sample.
    /// Shares the grid's formatter so the sample always shows what decimal
    /// cells show (separator characters and metric reduction included).
    pub fn format_decimal(&self, value: f64) -> String {
        sqlly_datatable::format::format_number(
            value,
            &crate::grid_bridge::decimal_number_format(&self.defaults.decimal_format),
        )
    }
}

// ============================================================================
// Query Resume Preferences
// ============================================================================

/// Mirrors the Swift `QueryResumeScope` enum (QueryResumePreferences.swift).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[derive(Default)]
pub enum QueryResumeScope {
    #[default]
    Both,
    FileOnly,
    UnsavedOnly,
    None,
    Manual,
}

impl QueryResumeScope {
    pub fn display_title(&self) -> String {
        match self {
            QueryResumeScope::Both => crate::localization::text(
                "settings.preferences.query_resume_scope.display_title.both_saved_unsaved_queries",
            ),
            QueryResumeScope::FileOnly => crate::localization::text(
                "settings.preferences.query_resume_scope.display_title.saved_queries_only",
            ),
            QueryResumeScope::UnsavedOnly => crate::localization::text(
                "settings.preferences.query_resume_scope.display_title.unsaved_queries_only",
            ),
            QueryResumeScope::None => crate::localization::text(
                "settings.preferences.query_resume_scope.display_title.none",
            ),
            QueryResumeScope::Manual => crate::localization::text(
                "settings.preferences.query_resume_scope.display_title.manually",
            ),
        }
    }

    /// Explanatory copy shown below the scope popup. Mirrors Swift `explanation`.
    pub fn explanation(&self) -> String {
        match self {
            QueryResumeScope::Both => crate::localization::text(
                "settings.preferences.query_resume_scope.explanation.both",
            ),
            QueryResumeScope::FileOnly => crate::localization::text(
                "settings.preferences.query_resume_scope.explanation.saved_only",
            ),
            QueryResumeScope::UnsavedOnly => crate::localization::text(
                "settings.preferences.query_resume_scope.explanation.unsaved_only",
            ),
            QueryResumeScope::None => crate::localization::text(
                "settings.preferences.query_resume_scope.explanation.none",
            ),
            QueryResumeScope::Manual => crate::localization::text(
                "settings.preferences.query_resume_scope.explanation.manual",
            ),
        }
    }

    /// Whether individual queries may override the global default. Mirrors Swift
    /// `allowsPerTabOverride` (true for everything except `.none`).
    #[allow(dead_code)]
    pub fn allows_per_tab_override(&self) -> bool {
        !matches!(self, QueryResumeScope::None)
    }

    pub fn all() -> [QueryResumeScope; 5] {
        [
            QueryResumeScope::Both,
            QueryResumeScope::FileOnly,
            QueryResumeScope::UnsavedOnly,
            QueryResumeScope::None,
            QueryResumeScope::Manual,
        ]
    }

    /// Default per-query enabled state for a freshly opened query.
    /// Mirrors Swift `QueryResumePolicy.defaultEnabled(scope:isFileBacked:)`.
    pub fn default_enabled(&self, is_file_backed: bool) -> bool {
        match self {
            QueryResumeScope::Both => true,
            QueryResumeScope::FileOnly => is_file_backed,
            QueryResumeScope::UnsavedOnly => !is_file_backed,
            QueryResumeScope::None | QueryResumeScope::Manual => false,
        }
    }
}

/// Policy helpers mirroring Swift `QueryResumePolicy`. These are the pure
/// decision functions the app calls to compute the effective resume state
/// from the global scope, the per-tab override, and whether the query is
/// backed by a file.
pub struct QueryResumePolicy;

impl QueryResumePolicy {
    /// Effective resume state for a query, resolving the per-tab override
    /// against the global scope. `None` scope always returns false (the
    /// toggle is disabled). An explicit override wins over the default.
    /// Mirrors Swift `QueryResumePolicy.effectiveEnabled(scope:isFileBacked:override:)`.
    pub fn effective_enabled(
        scope: QueryResumeScope,
        is_file_backed: bool,
        override_enabled: Option<bool>,
    ) -> bool {
        if scope == QueryResumeScope::None {
            return false;
        }
        match override_enabled {
            Some(v) => v,
            None => scope.default_enabled(is_file_backed),
        }
    }

    /// Resolve the result-set layout to use. When resume is enabled the
    /// per-tab layout wins; otherwise the global layout is used.
    /// Mirrors Swift `QueryResumePolicy.resolvedLayout(resumeEnabled:perTab:global:)`.
    #[allow(dead_code)]
    pub fn resolved_layout(
        resume_enabled: bool,
        per_tab: Option<&str>,
        global: Option<&str>,
    ) -> Option<String> {
        if resume_enabled {
            per_tab
                .map(|s| s.to_string())
                .or_else(|| global.map(|s| s.to_string()))
        } else {
            global.map(|s| s.to_string())
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct QueryResumePreferences {
    pub scope: QueryResumeScope,
}

impl Default for QueryResumePreferences {
    fn default() -> Self {
        Self {
            scope: QueryResumeScope::Both,
        }
    }
}

impl QueryResumePreferences {
    pub fn load() -> Self {
        #[cfg(target_arch = "wasm32")]
        return wasm_load_json("query-resume.json");
        #[cfg(not(target_arch = "wasm32"))]
        {
            let path = query_resume_path();
            match std::fs::read(&path) {
                Ok(data) => serde_json::from_slice(&data).unwrap_or_default(),
                Err(_) => Self::default(),
            }
        }
    }

    pub fn save(&self) {
        #[cfg(target_arch = "wasm32")]
        return wasm_save_json("query-resume.json", self);
        #[cfg(not(target_arch = "wasm32"))]
        {
            let path = query_resume_path();
            if let Ok(data) = crate::config_version::to_vec_pretty(self) {
                let _ = crate::fs_safe::write_atomic(&path, &data);
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum TabRestorePolicy {
    #[default]
    All,
    PinnedOnly,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum DisconnectTabPolicy {
    CloseTabs,
    KeepTabsClearResults,
    #[default]
    KeepTabsKeepResults,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct TabPreferences {
    pub restore_policy: TabRestorePolicy,
    pub disconnect_policy: DisconnectTabPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct ExplainPreferences {
    /// Keyed by `ConnectionId` string (`profile.id.0.to_string()`).
    pub per_connection: std::collections::HashMap<String, sqlly_services::plans::ExplainOptions>,
}

impl ExplainPreferences {
    pub fn load() -> Self {
        #[cfg(target_arch = "wasm32")]
        return wasm_load_json("explain.json");
        #[cfg(not(target_arch = "wasm32"))]
        {
            let path = explain_prefs_path();
            match std::fs::read(&path) {
                Ok(data) => serde_json::from_slice(&data).unwrap_or_default(),
                Err(_) => Self::default(),
            }
        }
    }

    pub fn save(&self) {
        #[cfg(target_arch = "wasm32")]
        return wasm_save_json("explain.json", self);
        #[cfg(not(target_arch = "wasm32"))]
        {
            let path = explain_prefs_path();
            if let Ok(data) = crate::config_version::to_vec_pretty(self) {
                let _ = crate::fs_safe::write_atomic(&path, &data);
            }
        }
    }

    pub fn options_for(&self, id: &str) -> Option<sqlly_services::plans::ExplainOptions> {
        self.per_connection.get(id).copied()
    }

    pub fn set_options(&mut self, id: &str, opts: sqlly_services::plans::ExplainOptions) {
        self.per_connection.insert(id.to_string(), opts);
    }
}

impl TabPreferences {
    pub fn load() -> Self {
        #[cfg(target_arch = "wasm32")]
        return wasm_load_json("tab-preferences.json");
        #[cfg(not(target_arch = "wasm32"))]
        {
            let path = tab_prefs_path();
            match std::fs::read(&path) {
                Ok(data) => serde_json::from_slice(&data).unwrap_or_default(),
                Err(_) => Self::default(),
            }
        }
    }

    pub fn save(&self) {
        #[cfg(target_arch = "wasm32")]
        return wasm_save_json("tab-preferences.json", self);
        #[cfg(not(target_arch = "wasm32"))]
        {
            let path = tab_prefs_path();
            if let Ok(data) = crate::config_version::to_vec_pretty(self) {
                let _ = crate::fs_safe::write_atomic(&path, &data);
            }
        }
    }
}

const MAX_DETACHED_RESULTS_BOUNDS: usize = 200;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct DetachedResultsPreferences {
    /// Last window bounds per tab key (`file path` for file-backed tabs, else the tab name), logical pixels: (x, y, width, height).
    pub bounds_by_tab: std::collections::BTreeMap<String, (f32, f32, f32, f32)>,
    /// Fallback for tabs never detached before.
    pub last_bounds: Option<(f32, f32, f32, f32)>,
}

impl DetachedResultsPreferences {
    pub fn load() -> Self {
        #[cfg(target_arch = "wasm32")]
        return wasm_load_json("detached-results.json");
        #[cfg(not(target_arch = "wasm32"))]
        {
            let path = detached_results_path();
            match std::fs::read(&path) {
                Ok(data) => serde_json::from_slice(&data).unwrap_or_default(),
                Err(_) => Self::default(),
            }
        }
    }

    pub fn save(&self) {
        #[cfg(target_arch = "wasm32")]
        return wasm_save_json("detached-results.json", self);
        #[cfg(not(target_arch = "wasm32"))]
        {
            let path = detached_results_path();
            if let Ok(data) = crate::config_version::to_vec_pretty(self) {
                let _ = crate::fs_safe::write_atomic(&path, &data);
            }
        }
    }

    pub fn remember(&mut self, key: &str, b: (f32, f32, f32, f32)) {
        self.bounds_by_tab.insert(key.to_string(), b);
        self.last_bounds = Some(b);
        while self.bounds_by_tab.len() > MAX_DETACHED_RESULTS_BOUNDS {
            let victim = self
                .bounds_by_tab
                .keys()
                .find(|k| k.as_str() != key)
                .cloned();
            match victim {
                Some(k) => {
                    self.bounds_by_tab.remove(&k);
                }
                None => break,
            }
        }
    }

    pub fn bounds_for(&self, key: &str) -> Option<(f32, f32, f32, f32)> {
        self.bounds_by_tab.get(key).copied()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct KubectlPreferences {
    pub kubectl_path: Option<String>,
}

impl KubectlPreferences {
    pub fn load() -> Self {
        #[cfg(target_arch = "wasm32")]
        return wasm_load_json("kubectl-preferences.json");
        #[cfg(not(target_arch = "wasm32"))]
        {
            let path = kubectl_prefs_path();
            match std::fs::read(&path) {
                Ok(data) => serde_json::from_slice(&data).unwrap_or_default(),
                Err(_) => Self::default(),
            }
        }
    }

    pub fn save(&self) {
        #[cfg(target_arch = "wasm32")]
        return wasm_save_json("kubectl-preferences.json", self);
        #[cfg(not(target_arch = "wasm32"))]
        {
            let path = kubectl_prefs_path();
            if let Ok(data) = crate::config_version::to_vec_pretty(self) {
                let _ = crate::fs_safe::write_atomic(&path, &data);
            }
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn apply_override(&self) {
        sqlly_engines::ssh::set_kubectl_override(
            self.kubectl_path
                .as_ref()
                .filter(|p| !p.trim().is_empty())
                .map(std::path::PathBuf::from),
        );
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeToolPreferences {
    /// Absolute path per tool; absent = auto-discover.
    #[serde(default)]
    pub overrides: std::collections::BTreeMap<String, String>,
    /// The user ticked "Don't show this again" on the macOS Open in Terminal
    /// notice explaining the short-lived launcher file that carries the
    /// password. Stored as an opt-out so the default is to explain.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub terminal_secret_notice_dismissed: bool,
}

impl NativeToolPreferences {
    pub fn load() -> Self {
        #[cfg(target_arch = "wasm32")]
        return wasm_load_json("native-tools.json");
        #[cfg(not(target_arch = "wasm32"))]
        {
            let path = native_tools_path();
            match std::fs::read(&path) {
                Ok(data) => serde_json::from_slice(&data).unwrap_or_default(),
                Err(_) => Self::default(),
            }
        }
    }

    pub fn save(&self) {
        #[cfg(target_arch = "wasm32")]
        return wasm_save_json("native-tools.json", self);
        #[cfg(not(target_arch = "wasm32"))]
        {
            let path = native_tools_path();
            if let Ok(data) = crate::config_version::to_vec_pretty(self) {
                let _ = crate::fs_safe::write_atomic(&path, &data);
            }
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn override_map(&self) -> std::collections::HashMap<sqlly_services::NativeTool, PathBuf> {
        self.overrides
            .iter()
            .filter_map(|(key, value)| {
                let tool = sqlly_services::NativeTool::from_snake_case(key)?;
                let trimmed = value.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some((tool, PathBuf::from(trimmed)))
                }
            })
            .collect()
    }
}

#[cfg(not(target_arch = "wasm32"))]
static NATIVE_TOOL_LOCATIONS: std::sync::OnceLock<
    std::sync::Mutex<Vec<sqlly_services::ToolLocation>>,
> = std::sync::OnceLock::new();

#[cfg(not(target_arch = "wasm32"))]
pub async fn refresh_native_tool_locations() {
    let prefs = NativeToolPreferences::load();
    let locations = sqlly_services::locate_all(&prefs.override_map()).await;
    let lock = NATIVE_TOOL_LOCATIONS.get_or_init(|| std::sync::Mutex::new(Vec::new()));
    if let Ok(mut guard) = lock.lock() {
        *guard = locations;
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub fn native_tool_locations() -> Vec<sqlly_services::ToolLocation> {
    NATIVE_TOOL_LOCATIONS
        .get_or_init(|| std::sync::Mutex::new(Vec::new()))
        .lock()
        .map(|g| g.clone())
        .unwrap_or_default()
}

/// Persisted answers to the update prompt. Not part of `PreferencesData` —
/// only `AppView` reads it, and it changes from the dialog, not the Preferences window.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UpdatePreferences {
    /// `major.minor.patch` the user chose to skip; re-prompt only for newer.
    pub skipped_version: Option<String>,
    /// "Remind me later" hides updates until this instant (UTC, RFC 3339).
    pub snoozed_until: Option<chrono::DateTime<chrono::Utc>>,
    /// The user unticked "Show at startup" in the update dialog: a launch
    /// check that finds an update only marks the status bar instead of
    /// opening the dialog. Stored as an opt-out so the default is to notify.
    pub startup_notice_disabled: bool,
}

impl UpdatePreferences {
    pub fn load() -> Self {
        #[cfg(target_arch = "wasm32")]
        return wasm_load_json("update-preferences.json");
        #[cfg(not(target_arch = "wasm32"))]
        {
            let path = update_prefs_path();
            match std::fs::read(&path) {
                Ok(data) => serde_json::from_slice(&data).unwrap_or_default(),
                Err(_) => Self::default(),
            }
        }
    }

    pub fn save(&self) {
        #[cfg(target_arch = "wasm32")]
        return wasm_save_json("update-preferences.json", self);
        #[cfg(not(target_arch = "wasm32"))]
        {
            let path = update_prefs_path();
            if let Ok(data) = crate::config_version::to_vec_pretty(self) {
                let _ = crate::fs_safe::write_atomic(&path, &data);
            }
        }
    }
}

// ============================================================================
// Export Preferences
// ============================================================================

/// Default format used by Cmd+C / the grid "Copy" item. One-off alternatives
/// stay in the context menu; this is the user's everyday clipboard shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum CopyFormat {
    #[default]
    Tsv,
    Csv,
    Json,
    JsonLines,
    Markdown,
    SqlInsert,
}

impl CopyFormat {
    pub const ALL: [CopyFormat; 6] = [
        CopyFormat::Tsv,
        CopyFormat::Csv,
        CopyFormat::Json,
        CopyFormat::JsonLines,
        CopyFormat::Markdown,
        CopyFormat::SqlInsert,
    ];

    pub fn export_format(self) -> crate::results::ExportFormat {
        use crate::results::ExportFormat as F;
        match self {
            CopyFormat::Tsv => F::Tsv,
            CopyFormat::Csv => F::Csv,
            CopyFormat::Json => F::Json,
            CopyFormat::JsonLines => F::JsonLines,
            CopyFormat::Markdown => F::Markdown,
            CopyFormat::SqlInsert => F::Insert,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            CopyFormat::Tsv => "TSV",
            CopyFormat::Csv => "CSV",
            CopyFormat::Json => "JSON",
            CopyFormat::JsonLines => "JSON Lines",
            CopyFormat::Markdown => "Markdown",
            CopyFormat::SqlInsert => "INSERT",
        }
    }
}

/// Which export formats appear in the results-grid right-click "Export" menu.
/// The twelve standard formats default to ON; the whole Programming section
/// (record-type code snippets) defaults to OFF, with each language
/// individually toggleable but only offered while the section is enabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ExportPreferences {
    pub csv: bool,
    pub tsv: bool,
    pub json: bool,
    pub json_lines: bool,
    pub markdown: bool,
    pub html: bool,
    pub xml: bool,
    pub insert: bool,
    pub temp_table: bool,
    pub sql_values: bool,
    pub xlsx: bool,
    pub sqlly_export: bool,
    pub programming_enabled: bool,
    pub csharp: bool,
    pub python: bool,
    pub rust: bool,
    pub javascript: bool,
    pub typescript: bool,
    pub kotlin: bool,
    pub swift: bool,
    pub java: bool,
    pub go: bool,
    pub php: bool,
    pub cpp: bool,
    pub default_copy_format: CopyFormat,
    /// Guard CSV/TSV text cells a spreadsheet would run as a formula with a
    /// leading apostrophe. Applies to file exports and clipboard copies
    /// alike; defaults to on.
    pub formula_safe: bool,
}

impl Default for ExportPreferences {
    fn default() -> Self {
        Self {
            csv: true,
            tsv: true,
            json: true,
            json_lines: true,
            markdown: true,
            html: true,
            xml: true,
            insert: true,
            temp_table: true,
            sql_values: true,
            xlsx: true,
            sqlly_export: true,
            programming_enabled: false,
            csharp: true,
            python: true,
            rust: true,
            javascript: true,
            typescript: true,
            kotlin: true,
            swift: true,
            java: true,
            go: true,
            php: true,
            cpp: true,
            default_copy_format: CopyFormat::Tsv,
            formula_safe: true,
        }
    }
}

impl ExportPreferences {
    pub fn load() -> Self {
        #[cfg(target_arch = "wasm32")]
        return wasm_load_json("export.json");
        #[cfg(not(target_arch = "wasm32"))]
        {
            let path = export_path();
            match std::fs::read(&path) {
                Ok(data) => serde_json::from_slice(&data).unwrap_or_default(),
                Err(_) => Self::default(),
            }
        }
    }

    pub fn save(&self) {
        #[cfg(target_arch = "wasm32")]
        return wasm_save_json("export.json", self);
        #[cfg(not(target_arch = "wasm32"))]
        {
            let path = export_path();
            if let Ok(data) = crate::config_version::to_vec_pretty(self) {
                let _ = crate::fs_safe::write_atomic(&path, &data);
            }
        }
    }

    /// Whether a standard (non-language) export format is enabled. Language
    /// formats return `true` here; they are gated via `language_enabled`.
    pub fn standard_enabled(&self, format: crate::results::ExportFormat) -> bool {
        use crate::results::ExportFormat as F;
        match format {
            F::Csv => self.csv,
            F::Tsv => self.tsv,
            F::Json => self.json,
            F::JsonLines => self.json_lines,
            F::Markdown => self.markdown,
            F::Html => self.html,
            F::Xml => self.xml,
            F::Insert => self.insert,
            F::TempTable => self.temp_table,
            F::SqlValues => self.sql_values,
            F::Xlsx => self.xlsx,
            F::SqllyExport => self.sqlly_export,
            _ => true,
        }
    }

    /// Whether a language export appears in the menu: the Programming master
    /// switch AND the per-language toggle must both be on.
    pub fn language_enabled(&self, language: crate::result_export_code::CodeLanguage) -> bool {
        self.programming_enabled && self.language_flag(language)
    }

    /// The raw per-language toggle, independent of the Programming master
    /// switch — used for the checkbox state so turning the section off and on
    /// preserves each language's choice.
    pub fn language_flag(&self, language: crate::result_export_code::CodeLanguage) -> bool {
        use crate::result_export_code::CodeLanguage as L;
        match language {
            L::CSharp => self.csharp,
            L::Python => self.python,
            L::Rust => self.rust,
            L::JavaScript => self.javascript,
            L::TypeScript => self.typescript,
            L::Kotlin => self.kotlin,
            L::Swift => self.swift,
            L::Java => self.java,
            L::Go => self.go,
            L::Php => self.php,
            L::Cpp => self.cpp,
        }
    }

    /// Flips a standard (non-language) format's toggle. Language formats are
    /// ignored — flip those with `toggle_language`.
    pub fn toggle_standard(&mut self, format: crate::results::ExportFormat) {
        use crate::results::ExportFormat as F;
        let field = match format {
            F::Csv => &mut self.csv,
            F::Tsv => &mut self.tsv,
            F::Json => &mut self.json,
            F::JsonLines => &mut self.json_lines,
            F::Markdown => &mut self.markdown,
            F::Html => &mut self.html,
            F::Xml => &mut self.xml,
            F::Insert => &mut self.insert,
            F::TempTable => &mut self.temp_table,
            F::SqlValues => &mut self.sql_values,
            F::Xlsx => &mut self.xlsx,
            F::SqllyExport => &mut self.sqlly_export,
            _ => return,
        };
        *field = !*field;
    }

    /// Flips a language's per-language toggle.
    pub fn toggle_language(&mut self, language: crate::result_export_code::CodeLanguage) {
        use crate::result_export_code::CodeLanguage as L;
        let field = match language {
            L::CSharp => &mut self.csharp,
            L::Python => &mut self.python,
            L::Rust => &mut self.rust,
            L::JavaScript => &mut self.javascript,
            L::TypeScript => &mut self.typescript,
            L::Kotlin => &mut self.kotlin,
            L::Swift => &mut self.swift,
            L::Java => &mut self.java,
            L::Go => &mut self.go,
            L::Php => &mut self.php,
            L::Cpp => &mut self.cpp,
        };
        *field = !*field;
    }
}

/// Default NULL text, line-break handling, header row, gzip, and batch filename
/// pattern for file exports (not clipboard copies).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ExportOptionsPreferences {
    /// `format-default`, `empty`, `literal`, or `custom`.
    pub null_mode: String,
    pub null_custom: String,
    /// `keep`, `space`, or `escape`.
    pub line_breaks: String,
    pub header_row: bool,
    pub gzip: bool,
    pub filename_pattern: String,
}

impl Default for ExportOptionsPreferences {
    fn default() -> Self {
        Self {
            null_mode: "format-default".to_string(),
            null_custom: String::new(),
            line_breaks: "keep".to_string(),
            header_row: true,
            gzip: false,
            filename_pattern: crate::export_options::DEFAULT_PATTERN.to_string(),
        }
    }
}

impl ExportOptionsPreferences {
    pub fn load() -> Self {
        #[cfg(target_arch = "wasm32")]
        return wasm_load_json("export-options.json");
        #[cfg(not(target_arch = "wasm32"))]
        {
            let path = export_options_prefs_path();
            match std::fs::read(&path) {
                Ok(data) => serde_json::from_slice(&data).unwrap_or_default(),
                Err(_) => Self::default(),
            }
        }
    }

    pub fn save(&self) {
        #[cfg(target_arch = "wasm32")]
        return wasm_save_json("export-options.json", self);
        #[cfg(not(target_arch = "wasm32"))]
        {
            let path = export_options_prefs_path();
            if let Ok(data) = crate::config_version::to_vec_pretty(self) {
                let _ = crate::fs_safe::write_atomic(&path, &data);
            }
        }
    }

    /// File-export options for `format`: these preferences over the format's
    /// defaults, plus the formula guard from the general export preferences
    /// (it is one switch for file exports and clipboard copies alike).
    pub fn to_options(
        &self,
        format: crate::results::ExportFormat,
    ) -> crate::export_options::ExportOptions {
        self.to_options_with(format, ExportPreferences::load().formula_safe)
    }

    /// [`Self::to_options`] with the formula guard supplied by the caller —
    /// the Export Tables dialog's own checkbox, or a test that must not read
    /// the on-disk preferences.
    pub fn to_options_with(
        &self,
        format: crate::results::ExportFormat,
        formula_safe: bool,
    ) -> crate::export_options::ExportOptions {
        use crate::export_options::{ExportOptions, LineBreaks, NullAs};
        let mut opts = ExportOptions::default_for(format);
        opts.formula_safe = formula_safe;
        opts.null_as = match self.null_mode.as_str() {
            "empty" => NullAs::Empty,
            "literal" => NullAs::Literal,
            "custom" => NullAs::Custom(self.null_custom.clone()),
            _ => opts.null_as,
        };
        opts.line_breaks = match self.line_breaks.as_str() {
            "space" => LineBreaks::Space,
            "escape" => LineBreaks::Escape,
            _ => LineBreaks::Keep,
        };
        opts.header_row = self.header_row;
        opts.gzip = self.gzip;
        opts
    }
}

// ============================================================================
// Diagnostic Logging Preferences
// ============================================================================

/// Mirrors the Swift `DiagnosticLogLevel` enum. Persisted via a stable string
/// `storage_key` (decoupled from any numeric value) so re-ordering never breaks
/// saved settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DiagnosticLogLevelPref {
    #[default]
    Off,
    Error,
    Verbose,
}

impl DiagnosticLogLevelPref {
    pub fn display_title(&self) -> String {
        match self {
            DiagnosticLogLevelPref::Off => crate::localization::text(
                "settings.preferences.diagnostic_log_level_pref.display_title.off",
            ),
            DiagnosticLogLevelPref::Error => crate::localization::text(
                "settings.preferences.diagnostic_log_level_pref.display_title.errors_only",
            ),
            DiagnosticLogLevelPref::Verbose => crate::localization::text(
                "settings.preferences.diagnostic_log_level_pref.display_title.verbose_timing",
            ),
        }
    }

    pub fn storage_key(&self) -> &'static str {
        match self {
            DiagnosticLogLevelPref::Off => "off",
            DiagnosticLogLevelPref::Error => "error",
            DiagnosticLogLevelPref::Verbose => "verbose",
        }
    }

    pub fn from_storage_key(key: &str) -> Self {
        match key {
            "error" => DiagnosticLogLevelPref::Error,
            "verbose" => DiagnosticLogLevelPref::Verbose,
            _ => DiagnosticLogLevelPref::Off,
        }
    }

    pub fn all() -> [DiagnosticLogLevelPref; 3] {
        [
            DiagnosticLogLevelPref::Off,
            DiagnosticLogLevelPref::Error,
            DiagnosticLogLevelPref::Verbose,
        ]
    }
}

impl Serialize for DiagnosticLogLevelPref {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.storage_key())
    }
}

impl<'de> Deserialize<'de> for DiagnosticLogLevelPref {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let key = String::deserialize(deserializer)?;
        Ok(DiagnosticLogLevelPref::from_storage_key(&key))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct DiagnosticLoggingPreferences {
    pub level: DiagnosticLogLevelPref,
}

impl DiagnosticLoggingPreferences {
    pub fn load() -> Self {
        #[cfg(target_arch = "wasm32")]
        return wasm_load_json("diagnostic-logging.json");
        #[cfg(not(target_arch = "wasm32"))]
        {
            let path = diagnostic_logging_path();
            match std::fs::read(&path) {
                Ok(data) => serde_json::from_slice(&data).unwrap_or_default(),
                Err(_) => Self::default(),
            }
        }
    }

    pub fn save(&self) {
        #[cfg(target_arch = "wasm32")]
        return wasm_save_json("diagnostic-logging.json", self);
        #[cfg(not(target_arch = "wasm32"))]
        {
            let path = diagnostic_logging_path();
            if let Ok(data) = crate::config_version::to_vec_pretty(self) {
                let _ = crate::fs_safe::write_atomic(&path, &data);
            }
        }
    }
}

// ============================================================================
// DDL Navigation Preferences
// ============================================================================

/// Which held modifier turns a left click into "go to definition". Stored
/// here (not in the keyboard keybinding registry) because pointer gestures
/// don't fit GPUI's keyboard-shortcut storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum DdlNavigationModifier {
    /// Command on macOS, Control elsewhere (resolved at use time).
    #[default]
    PlatformDefault,
    Command,
    Control,
    Alt,
    /// Accepted from hand-edited config, but not offered in the settings UI
    /// — shift-click conventionally extends selections.
    Shift,
}

impl DdlNavigationModifier {
    pub fn resolved(&self) -> DdlNavigationModifier {
        match self {
            DdlNavigationModifier::PlatformDefault => {
                if cfg!(target_os = "macos") {
                    DdlNavigationModifier::Command
                } else {
                    DdlNavigationModifier::Control
                }
            }
            other => *other,
        }
    }

    /// Exact-match against the held modifier flags: the configured modifier
    /// must be down and every other one up, so the gesture never collides
    /// with combo gestures added later.
    pub fn matches_flags(&self, platform: bool, control: bool, alt: bool, shift: bool) -> bool {
        match self.resolved() {
            DdlNavigationModifier::Command => platform && !control && !alt && !shift,
            DdlNavigationModifier::Control => control && !platform && !alt && !shift,
            DdlNavigationModifier::Alt => alt && !platform && !control && !shift,
            DdlNavigationModifier::Shift => shift && !platform && !control && !alt,
            // `resolved()` never returns PlatformDefault.
            DdlNavigationModifier::PlatformDefault => false,
        }
    }

    pub fn display_title(&self) -> String {
        match self {
            DdlNavigationModifier::PlatformDefault => {
                if cfg!(target_os = "macos") {
                    crate::localization::text(
                        "settings.preferences.ddl_navigation_modifier.display_title.platform_default_command",
                    )
                } else {
                    crate::localization::text(
                        "settings.preferences.ddl_navigation_modifier.display_title.platform_default_ctrl",
                    )
                }
            }
            DdlNavigationModifier::Command => crate::localization::text(
                "settings.preferences.ddl_navigation_modifier.display_title.command",
            ),
            DdlNavigationModifier::Control => crate::localization::text(
                "settings.preferences.ddl_navigation_modifier.display_title.control",
            ),
            DdlNavigationModifier::Alt => crate::localization::text(
                "settings.preferences.ddl_navigation_modifier.display_title.alt_option",
            ),
            DdlNavigationModifier::Shift => crate::localization::text(
                "settings.preferences.ddl_navigation_modifier.display_title.shift",
            ),
        }
    }

    /// Options offered in the settings UI. `Shift` is intentionally
    /// excluded (see the variant docs).
    pub fn all() -> [DdlNavigationModifier; 4] {
        [
            DdlNavigationModifier::PlatformDefault,
            DdlNavigationModifier::Command,
            DdlNavigationModifier::Control,
            DdlNavigationModifier::Alt,
        ]
    }
}

/// Where a fetched DDL document is presented.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum DdlNavigationPresentation {
    #[default]
    OpenInNewTab,
    SidePanel,
    Popup,
}

impl DdlNavigationPresentation {
    pub fn display_title(&self) -> String {
        match self {
            DdlNavigationPresentation::OpenInNewTab => crate::localization::text(
                "settings.preferences.ddl_navigation_presentation.display_title.open_new_tab",
            ),
            DdlNavigationPresentation::SidePanel => crate::localization::text(
                "settings.preferences.ddl_navigation_presentation.display_title.panel_right",
            ),
            DdlNavigationPresentation::Popup => crate::localization::text(
                "settings.preferences.ddl_navigation_presentation.display_title.popup_window",
            ),
        }
    }
}

/// Which monitor the popup opens on. Monitors are persisted by their stable
/// display UUID (best-effort — monitor sets change between launches, so use
/// falls back to the current window's display when the id is gone).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum DdlPopupMonitorPreference {
    #[default]
    CurrentWindow,
    Monitor {
        id: String,
    },
}

pub const DDL_POPUP_MIN_WIDTH: u32 = 320;
pub const DDL_POPUP_MIN_HEIGHT: u32 = 240;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct DdlPopupPreferences {
    /// Popup size in logical pixels.
    pub width: u32,
    pub height: u32,
    /// Placement as a fraction (0.0–1.0) of the leftover monitor space, so
    /// switching monitors never puts the popup offscreen. 0.5/0.5 = centered.
    pub x_percent: f32,
    pub y_percent: f32,
    pub monitor: DdlPopupMonitorPreference,
}

impl Default for DdlPopupPreferences {
    fn default() -> Self {
        Self {
            width: 900,
            height: 700,
            x_percent: 0.5,
            y_percent: 0.5,
            monitor: DdlPopupMonitorPreference::CurrentWindow,
        }
    }
}

impl DdlPopupPreferences {
    /// Sanitized copy: size at least the minimums, placement fractions in
    /// 0.0–1.0. Final clamping to the selected monitor's bounds happens at
    /// open time (`ddl_navigation::popup_bounds`).
    pub fn clamped(&self) -> Self {
        Self {
            width: self.width.max(DDL_POPUP_MIN_WIDTH),
            height: self.height.max(DDL_POPUP_MIN_HEIGHT),
            x_percent: if self.x_percent.is_finite() {
                self.x_percent.clamp(0.0, 1.0)
            } else {
                0.5
            },
            y_percent: if self.y_percent.is_finite() {
                self.y_percent.clamp(0.0, 1.0)
            } else {
                0.5
            },
            monitor: self.monitor.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct DdlNavigationPreferences {
    pub enabled: bool,
    pub activation_modifier: DdlNavigationModifier,
    pub presentation: DdlNavigationPresentation,
    pub popup: DdlPopupPreferences,
    /// PostgreSQL definitions (the definition viewer and Script as CREATE)
    /// end with the GRANT / REVOKE statements that recreate the object's
    /// privileges, including REVOKEs of PostgreSQL's implicit defaults.
    pub include_privileges: bool,
}

impl Default for DdlNavigationPreferences {
    fn default() -> Self {
        Self {
            enabled: true,
            activation_modifier: DdlNavigationModifier::PlatformDefault,
            presentation: DdlNavigationPresentation::OpenInNewTab,
            popup: DdlPopupPreferences::default(),
            include_privileges: true,
        }
    }
}

impl DdlNavigationPreferences {
    /// The definition options the explorer and the definition viewer use.
    pub fn ddl_options(&self) -> sqlly_services::DdlOptions {
        sqlly_services::DdlOptions {
            include_privileges: self.include_privileges,
        }
    }
}

impl DdlNavigationPreferences {
    pub fn load() -> Self {
        #[cfg(target_arch = "wasm32")]
        return wasm_load_json("ddl-navigation.json");
        #[cfg(not(target_arch = "wasm32"))]
        {
            let path = ddl_navigation_path();
            match std::fs::read(&path) {
                Ok(data) => serde_json::from_slice(&data).unwrap_or_default(),
                Err(_) => Self::default(),
            }
        }
    }

    pub fn save(&self) {
        #[cfg(target_arch = "wasm32")]
        return wasm_save_json("ddl-navigation.json", self);
        #[cfg(not(target_arch = "wasm32"))]
        {
            let path = ddl_navigation_path();
            if let Ok(data) = crate::config_version::to_vec_pretty(self) {
                let _ = crate::fs_safe::write_atomic(&path, &data);
            }
        }
    }
}

// ============================================================================
// Preferences Tree Model
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PreferencesPane {
    #[serde(alias = "Appearance")]
    Themes,
    Display,
    Language,
    EditorGeneral,
    EditorTabs,
    /// SQL formatting. Was its own SQL ▸ Formatting pane until 2026-10-22;
    /// the alias keeps saved dialog state landing on it.
    #[serde(alias = "SqlFormatting")]
    EditorFormatting,
    #[serde(alias = "EditorDefaults")]
    EditorIntelliSense,
    EditorQueryHistory,
    EditorExternalEditors,
    EditorSnippets,
    DdlNavigation,
    AiOnDevice,
    AiFim,
    AiCloud,
    AiDecisionModel,
    AiCodingAgentCli,
    AiMcp,
    AiPlayground,
    ResultsGeneral,
    ResultsHistory,
    ResultsDate,
    ResultsBool,
    ResultsUuid,
    ResultsInteger,
    ResultsDecimal,
    ResultsOverrides,
    ResultsMasking,
    ResultsExport,
    /// Custom .proto schemas for decoding Protobuf values.
    ResultsProtobufSchemas,
    DataModelServerExplorer,
    IntelliSenseCache,
    Performance,
    Gestures,
    Keybindings,
    WorkspaceQueryResume,
    ScheduledDumps,
    /// iCloud sync (macOS only; the tree leaves it out elsewhere).
    WorkspaceICloudSync,
    DebugLogging,
    Advanced,
}

impl PreferencesPane {
    /// Every pane, so a test can assert each one has a tree entry. `Keybindings`
    /// shipped without one and was reachable only from its menu action.
    ///
    /// Test-only today; kept `pub` (rather than `#[cfg(test)]`) because it is
    /// the natural place to add a pane, and the reachability test is the point.
    #[allow(dead_code)]
    pub const ALL: [Self; 39] = [
        Self::Themes,
        Self::Display,
        Self::Language,
        Self::EditorGeneral,
        Self::EditorTabs,
        Self::EditorFormatting,
        Self::EditorIntelliSense,
        Self::EditorQueryHistory,
        Self::EditorExternalEditors,
        Self::EditorSnippets,
        Self::DdlNavigation,
        Self::AiOnDevice,
        Self::AiFim,
        Self::AiCloud,
        Self::AiDecisionModel,
        Self::AiCodingAgentCli,
        Self::AiMcp,
        Self::AiPlayground,
        Self::ResultsGeneral,
        Self::ResultsHistory,
        Self::ResultsDate,
        Self::ResultsBool,
        Self::ResultsUuid,
        Self::ResultsInteger,
        Self::ResultsDecimal,
        Self::ResultsOverrides,
        Self::ResultsMasking,
        Self::ResultsExport,
        Self::ResultsProtobufSchemas,
        Self::DataModelServerExplorer,
        Self::IntelliSenseCache,
        Self::Performance,
        Self::Gestures,
        Self::Keybindings,
        Self::WorkspaceQueryResume,
        Self::ScheduledDumps,
        Self::WorkspaceICloudSync,
        Self::DebugLogging,
        Self::Advanced,
    ];

    pub fn display_title(&self) -> String {
        let key = match self {
            PreferencesPane::Themes => "settings.navigation.appearance.themes",
            PreferencesPane::Display => "settings.navigation.appearance.display",
            PreferencesPane::Language => "settings.appearance.language.title",
            PreferencesPane::EditorGeneral => "settings.navigation.editor.general",
            PreferencesPane::EditorTabs => "settings.navigation.editor.tabs",
            PreferencesPane::EditorFormatting => "settings.navigation.editor.formatting",
            PreferencesPane::EditorIntelliSense => "settings.navigation.editor.intellisense",
            PreferencesPane::EditorQueryHistory => "settings.navigation.editor.query_history",
            PreferencesPane::EditorExternalEditors => "settings.navigation.editor.external_editors",
            PreferencesPane::EditorSnippets => "settings.navigation.editor.snippets",
            PreferencesPane::DdlNavigation => "settings.navigation.editor.ddl_navigation",
            PreferencesPane::AiOnDevice => "settings.navigation.ai.on_device",
            PreferencesPane::AiFim => "settings.navigation.ai.fim",
            PreferencesPane::AiCloud => "settings.navigation.ai.cloud",
            PreferencesPane::AiDecisionModel => "settings.navigation.ai.decision_model",
            PreferencesPane::AiCodingAgentCli => "settings.navigation.ai.coding_agent_cli",
            PreferencesPane::AiMcp => "settings.navigation.ai.mcp",
            PreferencesPane::AiPlayground => "settings.navigation.ai.playground",
            PreferencesPane::ResultsGeneral => "settings.navigation.results.general",
            PreferencesPane::ResultsHistory => "settings.navigation.results.history",
            PreferencesPane::ResultsDate => "settings.navigation.results.date",
            PreferencesPane::ResultsBool => "settings.navigation.results.boolean",
            PreferencesPane::ResultsUuid => "settings.navigation.results.unique_id",
            PreferencesPane::ResultsInteger => "settings.navigation.results.integer",
            PreferencesPane::ResultsDecimal => "settings.navigation.results.decimal",
            PreferencesPane::ResultsOverrides => "settings.navigation.results.column_overrides",
            PreferencesPane::ResultsMasking => "settings.navigation.results.sensitive_columns",
            PreferencesPane::ResultsExport => "settings.navigation.results.export",
            PreferencesPane::ResultsProtobufSchemas => {
                "settings.navigation.results.protobuf_schemas"
            }
            PreferencesPane::DataModelServerExplorer => {
                "settings.navigation.data_model.server_explorer"
            }
            PreferencesPane::IntelliSenseCache => "settings.navigation.intellisense_cache",
            PreferencesPane::Performance => "settings.navigation.results.performance",
            PreferencesPane::Gestures => "settings.navigation.results.gestures",
            PreferencesPane::Keybindings => "settings.navigation.workspace.keyboard_shortcuts",
            PreferencesPane::WorkspaceQueryResume => "settings.navigation.workspace.query_resume",
            PreferencesPane::ScheduledDumps => "settings.navigation.workspace.scheduled_dumps",
            PreferencesPane::WorkspaceICloudSync => "settings.navigation.workspace.icloud_sync",
            PreferencesPane::DebugLogging => "settings.navigation.debug.logging",
            PreferencesPane::Advanced => "settings.navigation.workspace.advanced",
        };
        crate::localization::text(key)
    }

    #[allow(dead_code)]
    pub fn icon(&self) -> &'static str {
        match self {
            PreferencesPane::Themes => "Theme",
            PreferencesPane::Display => "Disp",
            PreferencesPane::Language => "Lang",
            PreferencesPane::EditorGeneral => "Editor",
            PreferencesPane::EditorTabs => "Tabs",
            PreferencesPane::EditorFormatting => "Format",
            PreferencesPane::EditorIntelliSense => "Intel",
            PreferencesPane::EditorQueryHistory => "Hist",
            PreferencesPane::EditorExternalEditors => "External",
            PreferencesPane::EditorSnippets => "Snippets",
            PreferencesPane::DdlNavigation => "DDL",
            PreferencesPane::AiOnDevice => "AI",
            PreferencesPane::AiFim => "FIM",
            PreferencesPane::AiCloud => "Cloud",
            PreferencesPane::AiDecisionModel => "Decide",
            PreferencesPane::AiCodingAgentCli => "CLI",
            PreferencesPane::AiMcp => "MCP",
            PreferencesPane::AiPlayground => "Play",
            PreferencesPane::ResultsGeneral => "Gen",
            PreferencesPane::ResultsHistory => "Hist",
            PreferencesPane::ResultsDate => "Date",
            PreferencesPane::ResultsBool => "Bool",
            PreferencesPane::ResultsUuid => "UUID",
            PreferencesPane::ResultsInteger => "Int",
            PreferencesPane::ResultsDecimal => "Dec",
            PreferencesPane::ResultsOverrides => "Ovr",
            PreferencesPane::ResultsMasking => "Mask",
            PreferencesPane::ResultsExport => "Exp",
            PreferencesPane::ResultsProtobufSchemas => "Proto",
            PreferencesPane::DataModelServerExplorer => "Expl",
            PreferencesPane::IntelliSenseCache => "Cache",
            PreferencesPane::Performance => "Perf",
            PreferencesPane::Gestures => "Ges",
            PreferencesPane::Keybindings => "Kbd",
            PreferencesPane::WorkspaceQueryResume => "Resume",
            PreferencesPane::ScheduledDumps => "Dumps",
            PreferencesPane::WorkspaceICloudSync => "iCloud",
            PreferencesPane::DebugLogging => "Log",
            PreferencesPane::Advanced => "Adv",
        }
    }
}

#[derive(Debug, Clone)]
pub struct PreferencesTreeNode {
    /// Stable across languages and safe to persist for expansion state.
    pub id: String,
    pub title: String,
    pub pane: Option<PreferencesPane>,
    pub children: Vec<PreferencesTreeNode>,
}

impl PreferencesTreeNode {
    pub fn leaf(title: &str, pane: PreferencesPane) -> Self {
        let id = format!("pane:{pane:?}");
        Self {
            id,
            title: title.to_string(),
            pane: Some(pane),
            children: Vec::new(),
        }
    }

    pub fn folder_with_id(id: &str, title: &str, children: Vec<PreferencesTreeNode>) -> Self {
        Self {
            id: id.to_string(),
            title: title.to_string(),
            pane: None,
            children,
        }
    }

    pub fn is_folder(&self) -> bool {
        !self.children.is_empty()
    }

    pub fn matches(&self, query: &str) -> bool {
        let q = query.trim();
        if q.is_empty() {
            return true;
        }
        let lowered = q.to_lowercase();
        if self.title.to_lowercase().contains(&lowered) {
            return true;
        }
        if let Some(p) = &self.pane
            && p.display_title().to_lowercase().contains(&lowered)
        {
            return true;
        }
        self.children.iter().any(|c| c.matches(query))
    }
}

pub fn build_preferences_tree() -> Vec<PreferencesTreeNode> {
    fn leaf(pane: PreferencesPane) -> PreferencesTreeNode {
        PreferencesTreeNode::leaf(&pane.display_title(), pane)
    }
    let appearance = PreferencesTreeNode::folder_with_id(
        "Appearance",
        &crate::localization::text("settings.navigation.appearance.title"),
        vec![
            leaf(PreferencesPane::Themes),
            leaf(PreferencesPane::Display),
            leaf(PreferencesPane::Language),
        ],
    );
    let editor = PreferencesTreeNode::folder_with_id(
        "Editor",
        &crate::localization::text("settings.navigation.editor.title"),
        vec![
            leaf(PreferencesPane::EditorGeneral),
            leaf(PreferencesPane::EditorTabs),
            leaf(PreferencesPane::EditorFormatting),
            leaf(PreferencesPane::EditorIntelliSense),
            leaf(PreferencesPane::EditorQueryHistory),
            leaf(PreferencesPane::EditorExternalEditors),
            leaf(PreferencesPane::EditorSnippets),
            leaf(PreferencesPane::DdlNavigation),
        ],
    );
    let ai = PreferencesTreeNode::folder_with_id(
        "AI",
        &crate::localization::text("settings.navigation.ai.title"),
        vec![
            leaf(PreferencesPane::AiOnDevice),
            leaf(PreferencesPane::AiFim),
            leaf(PreferencesPane::AiCloud),
            leaf(PreferencesPane::AiDecisionModel),
            leaf(PreferencesPane::AiCodingAgentCli),
            leaf(PreferencesPane::AiMcp),
            leaf(PreferencesPane::AiPlayground),
        ],
    );
    let results = PreferencesTreeNode::folder_with_id(
        "Results",
        &crate::localization::text("settings.navigation.results.title"),
        vec![
            leaf(PreferencesPane::ResultsGeneral),
            leaf(PreferencesPane::ResultsHistory),
            leaf(PreferencesPane::ResultsDate),
            leaf(PreferencesPane::ResultsBool),
            leaf(PreferencesPane::ResultsUuid),
            leaf(PreferencesPane::ResultsInteger),
            leaf(PreferencesPane::ResultsDecimal),
            leaf(PreferencesPane::ResultsOverrides),
            leaf(PreferencesPane::ResultsMasking),
            leaf(PreferencesPane::ResultsExport),
            leaf(PreferencesPane::ResultsProtobufSchemas),
            leaf(PreferencesPane::Performance),
            leaf(PreferencesPane::Gestures),
        ],
    );
    let data_model = PreferencesTreeNode::folder_with_id(
        "Data Model",
        &crate::localization::text("settings.navigation.data_model.title"),
        vec![leaf(PreferencesPane::DataModelServerExplorer)],
    );
    let intellisense_cache = leaf(PreferencesPane::IntelliSenseCache);
    let mut workspace_panes = vec![
        leaf(PreferencesPane::WorkspaceQueryResume),
        leaf(PreferencesPane::ScheduledDumps),
        leaf(PreferencesPane::Keybindings),
    ];
    if icloud_sync_pane_listed() {
        workspace_panes.push(leaf(PreferencesPane::WorkspaceICloudSync));
    }
    workspace_panes.push(leaf(PreferencesPane::Advanced));
    let workspace = PreferencesTreeNode::folder_with_id(
        "Workspace",
        &crate::localization::text("settings.navigation.workspace.title"),
        workspace_panes,
    );
    let debug = PreferencesTreeNode::folder_with_id(
        "Debug",
        &crate::localization::text("settings.navigation.debug.title"),
        vec![leaf(PreferencesPane::DebugLogging)],
    );

    vec![
        appearance,
        editor,
        ai,
        results,
        data_model,
        intellisense_cache,
        workspace,
        debug,
    ]
}

/// iCloud sync exists on macOS only, so its pane is listed only there (in
/// the App Store edition too, where it explains why sync is unavailable).
pub fn icloud_sync_pane_listed() -> bool {
    cfg!(target_os = "macos")
}

/// Flattens the tree into visible rows (only expanded folders and their children).
pub fn flatten_tree(
    nodes: &[PreferencesTreeNode],
    expanded: &std::collections::HashSet<String>,
) -> Vec<(usize, String, String, Option<PreferencesPane>)> {
    let mut result = Vec::new();
    for node in nodes {
        flatten_node(node, 0, expanded, &mut result);
    }
    result
}

fn flatten_node(
    node: &PreferencesTreeNode,
    depth: usize,
    expanded: &std::collections::HashSet<String>,
    result: &mut Vec<(usize, String, String, Option<PreferencesPane>)>,
) {
    result.push((
        depth,
        node.id.clone(),
        node.title.clone(),
        node.pane.clone(),
    ));
    if node.is_folder() && expanded.contains(&node.id) {
        for child in &node.children {
            flatten_node(child, depth + 1, expanded, result);
        }
    }
}

/// Stable folder identities used to preserve expansion across language rebuilds.
pub fn tree_id_path_to(
    nodes: &[PreferencesTreeNode],
    pane: &PreferencesPane,
) -> Option<Vec<String>> {
    fn walk(node: &PreferencesTreeNode, pane: &PreferencesPane, path: &mut Vec<String>) -> bool {
        if node.pane.as_ref() == Some(pane) {
            return true;
        }
        if node.is_folder() {
            path.push(node.id.clone());
            for child in &node.children {
                if walk(child, pane, path) {
                    return true;
                }
            }
            path.pop();
        }
        false
    }
    let mut path = Vec::new();
    for node in nodes {
        if walk(node, pane, &mut path) {
            return Some(path);
        }
    }
    None
}

/// Localized folder titles from the root down to the node holding `pane`.
pub fn tree_path_to(nodes: &[PreferencesTreeNode], pane: &PreferencesPane) -> Option<Vec<String>> {
    fn walk(node: &PreferencesTreeNode, pane: &PreferencesPane, path: &mut Vec<String>) -> bool {
        if node.pane.as_ref() == Some(pane) {
            return true;
        }
        if node.is_folder() {
            path.push(node.title.clone());
            for child in &node.children {
                if walk(child, pane, path) {
                    return true;
                }
            }
            path.pop();
        }
        false
    }
    let mut path = Vec::new();
    for node in nodes {
        if walk(node, pane, &mut path) {
            return Some(path);
        }
    }
    None
}

impl PreferencesTreeNode {
    /// Like [`Self::matches`], but a leaf also matches when `content_matches`
    /// says its pane's *rendered text* (descriptions, section headers, setting
    /// names — including names inside collections) contains the query. This is
    /// what lets the sidebar search find "thousands separator" even though no
    /// tree title mentions it.
    pub fn matches_with(
        &self,
        query: &str,
        content_matches: &dyn Fn(&PreferencesPane) -> bool,
    ) -> bool {
        if self.matches(query) {
            return true;
        }
        if let Some(p) = &self.pane
            && content_matches(p)
        {
            return true;
        }
        self.children
            .iter()
            .any(|c| c.matches_with(query, content_matches))
    }

    /// Filtered copy under the [`Self::matches_with`] rule: folders keep only
    /// children whose subtree matches; folders left with no children are
    /// dropped by the caller (a folder whose own title matches keeps all its
    /// children, since everything under it is "related to" the match).
    pub fn filtered_with(
        &self,
        query: &str,
        content_matches: &dyn Fn(&PreferencesPane) -> bool,
    ) -> Self {
        if query.trim().is_empty() {
            return self.clone();
        }
        // A folder whose own title matches keeps its whole subtree.
        let lowered = query.trim().to_lowercase();
        if self.is_folder() && self.title.to_lowercase().contains(&lowered) {
            return self.clone();
        }
        let kept: Vec<PreferencesTreeNode> = self
            .children
            .iter()
            .filter(|c| c.matches_with(query, content_matches))
            .map(|c| c.filtered_with(query, content_matches))
            .collect();
        PreferencesTreeNode {
            id: self.id.clone(),
            title: self.title.clone(),
            pane: self.pane.clone(),
            children: kept,
        }
    }
}

// ============================================================================
// AI Query Summary Preferences
// ============================================================================

/// Settings for the on-device AI query summaries (the plain-language "what
/// does this query do" line generated by the local AI runtime). Persisted to
/// its own file like the other preference sets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct AiSummaryPreferences {
    /// Master switch for the AI query summary bar: when off, no summaries
    /// are requested and the bar above the results is hidden. Off by
    /// default — it is opt-in.
    pub enabled: bool,
    /// Model name override for summary generation. Empty means "use the
    /// app's default summary model".
    pub model: String,
}

impl AiSummaryPreferences {
    /// The model override, or `None` when the field is empty/whitespace and
    /// the app's default summary model should be used. `allow(dead_code)`
    /// until the app's summary scheduler is rewired to consume it.
    #[allow(dead_code)]
    pub fn model_override(&self) -> Option<&str> {
        let trimmed = self.model.trim();
        (!trimmed.is_empty()).then_some(trimmed)
    }

    pub fn load() -> Self {
        #[cfg(target_arch = "wasm32")]
        return wasm_load_json("ai-summary-preferences.json");
        #[cfg(not(target_arch = "wasm32"))]
        {
            let path = ai_summary_prefs_path();
            match std::fs::read(&path) {
                Ok(data) => serde_json::from_slice(&data).unwrap_or_default(),
                Err(_) => Self::default(),
            }
        }
    }

    pub fn save(&self) {
        #[cfg(target_arch = "wasm32")]
        return wasm_save_json("ai-summary-preferences.json", self);
        #[cfg(not(target_arch = "wasm32"))]
        {
            let path = ai_summary_prefs_path();
            if let Ok(data) = crate::config_version::to_vec_pretty(self) {
                let _ = crate::fs_safe::write_atomic(&path, &data);
            }
        }
    }
}

/// Agent-mode turn budget for the AI drawer read-only tool loop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AgentPreferences {
    pub max_turns: usize,
}

impl Default for AgentPreferences {
    fn default() -> Self {
        Self { max_turns: 30 }
    }
}

impl AgentPreferences {
    pub fn clamped_max_turns(&self) -> usize {
        self.max_turns.clamp(5, 100)
    }

    pub fn load() -> Self {
        #[cfg(target_arch = "wasm32")]
        return wasm_load_json("agent-preferences.json");
        #[cfg(not(target_arch = "wasm32"))]
        {
            let path = agent_prefs_path();
            match std::fs::read(&path) {
                Ok(data) => serde_json::from_slice(&data).unwrap_or_default(),
                Err(_) => Self::default(),
            }
        }
    }

    pub fn save(&self) {
        #[cfg(target_arch = "wasm32")]
        return wasm_save_json("agent-preferences.json", self);
        #[cfg(not(target_arch = "wasm32"))]
        {
            let path = agent_prefs_path();
            if let Ok(data) = crate::config_version::to_vec_pretty(self) {
                let _ = crate::fs_safe::write_atomic(&path, &data);
            }
        }
    }
}

fn agent_prefs_path() -> PathBuf {
    let mut p = preferences_path();
    p.push("agent-preferences.json");
    p
}

impl MaskingRules {
    pub fn load() -> Self {
        #[cfg(target_arch = "wasm32")]
        return wasm_load_json("masking-rules.json");
        #[cfg(not(target_arch = "wasm32"))]
        {
            let path = masking_rules_path();
            match std::fs::read(&path) {
                Ok(data) => serde_json::from_slice(&data).unwrap_or_default(),
                Err(_) => Self::default(),
            }
        }
    }

    pub fn save(&self) {
        #[cfg(target_arch = "wasm32")]
        return wasm_save_json("masking-rules.json", self);
        #[cfg(not(target_arch = "wasm32"))]
        {
            let path = masking_rules_path();
            if let Ok(data) = crate::config_version::to_vec_pretty(self) {
                let _ = crate::fs_safe::write_atomic(&path, &data);
            }
        }
    }
}

// ============================================================================
// Shared preferences store
// ============================================================================

#[derive(Clone)]
pub struct PreferencesStore {
    inner: Arc<RwLock<PreferencesData>>,
}

#[derive(Debug, Clone)]
pub struct PreferencesData {
    pub appearance: AppearancePreferences,
    pub editor: EditorPreferences,
    pub result_formatting: ResultFormattingPreferences,
    pub masking_rules: MaskingRules,
    pub export: ExportPreferences,
    pub query_resume: QueryResumePreferences,
    pub tabs: TabPreferences,
    pub diagnostic_logging: DiagnosticLoggingPreferences,
    pub ddl_navigation: DdlNavigationPreferences,
    pub performance: PerformancePreferences,
    pub query_history: QueryHistoryPreferences,
    pub sql_library: SqlLibraryPreferences,
    #[cfg(not(target_arch = "wasm32"))]
    pub result_cache: ResultCachePreferences,
    pub transaction: TransactionPreferences,
    pub data_model: DataModelPreferences,
    pub sql_formatting: SqlFormattingPreferences,
    pub fim: crate::fim_completion::FimPreferences,
    pub ai_summary: AiSummaryPreferences,
}

impl Default for PreferencesData {
    fn default() -> Self {
        Self {
            appearance: AppearancePreferences::load(),
            editor: EditorPreferences::load(),
            result_formatting: ResultFormattingPreferences::load(),
            masking_rules: MaskingRules::load(),
            export: ExportPreferences::load(),
            query_resume: QueryResumePreferences::load(),
            tabs: TabPreferences::load(),
            diagnostic_logging: DiagnosticLoggingPreferences::load(),
            ddl_navigation: DdlNavigationPreferences::load(),
            performance: PerformancePreferences::load(),
            query_history: QueryHistoryPreferences::load(),
            sql_library: SqlLibraryPreferences::load(),
            #[cfg(not(target_arch = "wasm32"))]
            result_cache: ResultCachePreferences::load(),
            transaction: TransactionPreferences::load(),
            data_model: DataModelPreferences::load(),
            sql_formatting: load_sql_formatting_preferences(),
            fim: crate::fim_completion::FimPreferences::load(),
            ai_summary: AiSummaryPreferences::load(),
        }
    }
}

impl PreferencesStore {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(PreferencesData::default())),
        }
    }

    pub fn editor(&self) -> EditorPreferences {
        self.inner.read().editor.clone()
    }

    pub fn appearance(&self) -> AppearancePreferences {
        self.inner.read().appearance.clone()
    }

    pub fn set_appearance(&self, prefs: AppearancePreferences) {
        self.update_appearance(prefs.clone());
        prefs.save();
    }

    pub fn update_appearance(&self, prefs: AppearancePreferences) {
        self.inner.write().appearance = prefs;
    }

    pub fn result_formatting(&self) -> ResultFormattingPreferences {
        self.inner.read().result_formatting.clone()
    }

    pub fn set_editor(&self, prefs: EditorPreferences) {
        prefs.apply_match_options();
        self.inner.write().editor = prefs.clone();
        prefs.save();
    }

    pub fn set_result_formatting(&self, prefs: ResultFormattingPreferences) {
        self.inner.write().result_formatting = prefs.clone();
        prefs.save();
    }

    pub fn masking_rules(&self) -> MaskingRules {
        self.inner.read().masking_rules.clone()
    }

    pub fn set_masking_rules(&self, prefs: MaskingRules) {
        self.inner.write().masking_rules = prefs.clone();
        prefs.save();
    }

    pub fn export(&self) -> ExportPreferences {
        self.inner.read().export
    }

    pub fn set_export(&self, prefs: ExportPreferences) {
        self.inner.write().export = prefs;
        prefs.save();
    }

    pub fn query_resume(&self) -> QueryResumePreferences {
        self.inner.read().query_resume
    }

    pub fn set_query_resume(&self, prefs: QueryResumePreferences) {
        self.inner.write().query_resume = prefs;
        prefs.save();
    }

    pub fn tabs(&self) -> TabPreferences {
        self.inner.read().tabs
    }

    pub fn set_tabs(&self, prefs: TabPreferences) {
        self.inner.write().tabs = prefs;
        prefs.save();
    }

    pub fn diagnostic_logging(&self) -> DiagnosticLoggingPreferences {
        self.inner.read().diagnostic_logging
    }

    pub fn set_diagnostic_logging(&self, prefs: DiagnosticLoggingPreferences) {
        self.inner.write().diagnostic_logging = prefs;
        prefs.save();
    }

    pub fn ddl_navigation(&self) -> DdlNavigationPreferences {
        self.inner.read().ddl_navigation.clone()
    }

    pub fn set_ddl_navigation(&self, prefs: DdlNavigationPreferences) {
        self.inner.write().ddl_navigation = prefs.clone();
        prefs.save();
    }

    pub fn performance(&self) -> PerformancePreferences {
        self.inner.read().performance
    }

    pub fn query_history(&self) -> QueryHistoryPreferences {
        self.inner.read().query_history
    }

    pub fn set_query_history(&self, prefs: QueryHistoryPreferences) {
        self.inner.write().query_history = prefs;
        prefs.save();
    }

    pub fn sql_library(&self) -> SqlLibraryPreferences {
        self.inner.read().sql_library.clone()
    }

    pub fn set_sql_library(&self, prefs: SqlLibraryPreferences) {
        self.inner.write().sql_library = prefs.clone();
        prefs.save();
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn result_cache(&self) -> ResultCachePreferences {
        self.inner.read().result_cache
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn set_result_cache(&self, prefs: ResultCachePreferences) {
        self.inner.write().result_cache = prefs;
        prefs.save();
    }

    pub fn transaction(&self) -> TransactionPreferences {
        self.inner.read().transaction
    }

    pub fn set_transaction(&self, prefs: TransactionPreferences) {
        self.inner.write().transaction = prefs;
        prefs.save();
    }

    pub fn set_performance(&self, prefs: PerformancePreferences) {
        self.inner.write().performance = prefs;
        prefs.save();
        // Push the limits into the query executor so the next execution
        // picks them up without an app restart.
        sqlly_services::set_spill_row_threshold(prefs.spill_row_threshold());
        sqlly_services::set_result_memory_budget_bytes(prefs.result_memory_budget_bytes());
    }

    pub fn data_model(&self) -> DataModelPreferences {
        self.inner.read().data_model
    }

    pub fn set_data_model(&self, prefs: DataModelPreferences) {
        self.inner.write().data_model = prefs;
        prefs.save();
    }

    pub fn sql_formatting(&self) -> SqlFormattingPreferences {
        self.inner.read().sql_formatting.clone()
    }

    pub fn set_sql_formatting(&self, prefs: SqlFormattingPreferences) {
        self.inner.write().sql_formatting = prefs.clone();
        save_sql_formatting_preferences(&prefs);
    }

    pub fn fim(&self) -> crate::fim_completion::FimPreferences {
        self.inner.read().fim.clone()
    }

    pub fn set_fim(&self, prefs: crate::fim_completion::FimPreferences) {
        self.inner.write().fim = prefs.clone();
        prefs.save();
    }

    pub fn ai_summary(&self) -> AiSummaryPreferences {
        self.inner.read().ai_summary.clone()
    }

    pub fn set_ai_summary(&self, prefs: AiSummaryPreferences) {
        self.inner.write().ai_summary = prefs.clone();
        prefs.save();
    }
}

impl Default for PreferencesStore {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sql_library_preferences_default_and_effective_root() {
        let p = SqlLibraryPreferences::default();
        assert!(p.show_front_matter_hint);
        assert!(p.root.is_none());
        #[cfg(not(target_arch = "wasm32"))]
        {
            let root = p.effective_root();
            assert!(
                root.ends_with("library"),
                "default library root should end with library, got {}",
                root.display()
            );
            let missing = SqlLibraryPreferences {
                root: Some("/definitely/not-a-sqlly-library-dir".into()),
                show_front_matter_hint: true,
            };
            assert_eq!(
                missing.effective_root(),
                crate::sql_library_model::default_library_root()
            );
        }
    }

    #[test]
    fn result_cache_preferences_default_and_clamp() {
        let p = ResultCachePreferences::default();
        assert!(p.enabled);
        assert_eq!(p.max_rows(), 10_000);
        assert_eq!(p.max_bytes(), 32 * 1024 * 1024);
        assert_eq!(
            ResultCachePreferences {
                max_rows: 10,
                ..Default::default()
            }
            .max_rows(),
            100
        );
        assert_eq!(
            ResultCachePreferences {
                max_bytes: 1,
                ..Default::default()
            }
            .max_bytes(),
            1024 * 1024
        );
    }

    #[test]
    fn transaction_preferences_default_and_clamp() {
        let p = TransactionPreferences::default();
        assert_eq!(p.idle_rollback_secs, 300);
        assert_eq!(
            TransactionPreferences {
                idle_rollback_secs: 5,
            }
            .idle_rollback_secs(),
            30
        );
        assert_eq!(
            TransactionPreferences {
                idle_rollback_secs: 1_000_000_000,
            }
            .idle_rollback_secs(),
            86_400
        );
    }

    #[test]
    fn tab_preferences_default_matches_legacy_behaviour() {
        let p = TabPreferences::default();
        assert_eq!(p.restore_policy, TabRestorePolicy::All);
        assert_eq!(
            p.disconnect_policy,
            DisconnectTabPolicy::KeepTabsKeepResults
        );
    }

    #[test]
    fn tab_preferences_round_trip_json() {
        let p = TabPreferences {
            restore_policy: TabRestorePolicy::PinnedOnly,
            disconnect_policy: DisconnectTabPolicy::CloseTabs,
        };
        let json = serde_json::to_string(&p).unwrap();
        let back: TabPreferences = serde_json::from_str(&json).unwrap();
        assert_eq!(back, p);
        let empty: TabPreferences = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, TabPreferences::default());
    }

    #[test]
    fn update_preferences_default_is_empty() {
        let p = UpdatePreferences::default();
        assert!(p.skipped_version.is_none());
        assert!(p.snoozed_until.is_none());
        // A missing file or key means "notify at startup".
        assert!(!p.startup_notice_disabled);
    }

    #[test]
    fn update_preferences_round_trip_json() {
        let p = UpdatePreferences {
            skipped_version: Some("0.40.0".to_string()),
            snoozed_until: Some(chrono::Utc::now()),
            startup_notice_disabled: true,
        };
        let json = serde_json::to_string(&p).unwrap();
        let back: UpdatePreferences = serde_json::from_str(&json).unwrap();
        assert_eq!(back, p);
        let empty: UpdatePreferences = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, UpdatePreferences::default());
    }

    #[test]
    fn editor_preferences_default() {
        let p = EditorPreferences::default();
        assert!(p.show_line_numbers);
        assert!(p.show_statement_frame);
        assert!(p.show_hover_info);
        assert!(p.show_signature_help);
        assert!(p.show_insert_value_hints);
        assert!(!p.vim_mode);
        assert_eq!(p.indent_style, IndentStyle::Tabs);
        assert_eq!(p.spaces_per_tab(), 4);
        assert_eq!(p.indent_unit(), "\t");
        assert!(!p.automagic_select_commas);
        assert!(!p.automagic_group_by_commas);
        assert!(!p.automagic_order_by_commas);
        assert_eq!(
            p.automagic_comma_placement,
            AutomagicCommaPlacement::Trailing
        );
    }

    #[test]
    fn editor_preferences_spaces_indent() {
        let p = EditorPreferences {
            indent_style: IndentStyle::Spaces,
            spaces_per_tab_raw: 2,
            ..Default::default()
        };
        assert_eq!(p.indent_unit(), "  ");
    }

    #[test]
    fn editor_preferences_clamp_spaces() {
        let mut p = EditorPreferences {
            spaces_per_tab_raw: 0,
            ..Default::default()
        };
        assert_eq!(p.spaces_per_tab(), 1);
        p.spaces_per_tab_raw = 100;
        assert_eq!(p.spaces_per_tab(), 16);
    }

    #[test]
    fn agent_preferences_clamped_max_turns() {
        assert_eq!(AgentPreferences { max_turns: 0 }.clamped_max_turns(), 5);
        assert_eq!(AgentPreferences { max_turns: 5 }.clamped_max_turns(), 5);
        assert_eq!(AgentPreferences { max_turns: 30 }.clamped_max_turns(), 30);
        assert_eq!(AgentPreferences { max_turns: 100 }.clamped_max_turns(), 100);
        assert_eq!(
            AgentPreferences { max_turns: 1000 }.clamped_max_turns(),
            100
        );
    }

    #[test]
    fn ai_summary_preferences_default() {
        let p = AiSummaryPreferences::default();
        // The summary bar is opt-in.
        assert!(!p.enabled);
        assert!(p.model.is_empty());
        assert_eq!(p.model_override(), None);
    }

    #[test]
    fn ai_summary_preferences_serde_round_trip() {
        let p = AiSummaryPreferences {
            enabled: false,
            model: "qwen2.5-coder:3b".to_string(),
        };
        let json = serde_json::to_string(&p).unwrap();
        let back: AiSummaryPreferences = serde_json::from_str(&json).unwrap();
        assert_eq!(back, p);
        assert_eq!(back.model_override(), Some("qwen2.5-coder:3b"));
    }

    #[test]
    fn ai_summary_preferences_partial_json_uses_defaults() {
        // Struct-level #[serde(default)]: a prefs file missing a key still
        // loads, with the missing field at its default.
        let p: AiSummaryPreferences = serde_json::from_str(r#"{ "enabled": false }"#).unwrap();
        assert!(!p.enabled);
        assert!(p.model.is_empty());
        // A file without the switch (or no file) leaves the bar off; one
        // saved with it on keeps it on.
        let p: AiSummaryPreferences = serde_json::from_str(r#"{ "model": "m" }"#).unwrap();
        assert!(!p.enabled);
        let p: AiSummaryPreferences = serde_json::from_str(r#"{ "enabled": true }"#).unwrap();
        assert!(p.enabled);
    }

    #[test]
    fn ai_summary_model_override_ignores_whitespace() {
        let p = AiSummaryPreferences {
            model: "   ".to_string(),
            ..Default::default()
        };
        assert_eq!(p.model_override(), None);
    }

    #[test]
    fn editor_preferences_vim_mode_defaults_off_and_round_trips() {
        let empty: EditorPreferences = serde_json::from_str("{}").unwrap();
        assert!(!empty.vim_mode);
        assert!(empty.show_statement_frame);
        assert!(empty.show_hover_info);
        assert!(empty.show_signature_help);
        assert!(empty.show_insert_value_hints);
        let on: EditorPreferences = serde_json::from_str(r#"{"vimMode":true}"#).unwrap();
        assert!(on.vim_mode);
        let json = serde_json::to_string(&EditorPreferences {
            vim_mode: true,
            ..Default::default()
        })
        .unwrap();
        assert!(json.contains("vimMode"));
        let back: EditorPreferences = serde_json::from_str(&json).unwrap();
        assert!(back.vim_mode);
    }

    #[test]
    fn editor_preferences_ignore_removed_indentation_keys() {
        // Prefs files written before the four inert indentation steppers were
        // removed still carry their keys; serde ignores unknown fields (no
        // deny_unknown_fields), so those files must load cleanly.
        let p: EditorPreferences = serde_json::from_str(
            r#"{ "showLineNumbers": false, "sectionIndentation": 3,
                 "keywordIndentation": 2, "joinIndentation": 1,
                 "columnIndentation": 4 }"#,
        )
        .unwrap();
        assert!(!p.show_line_numbers);
    }

    #[test]
    fn performance_load_returns_cached_value_without_disk_reread() {
        PerformancePreferences::invalidate_cache();
        let prefs = PerformancePreferences {
            window_rows: 12_345,
            ..Default::default()
        };
        *performance_prefs_cache().write() = Some(prefs);
        assert_eq!(PerformancePreferences::load().window_rows, 12_345);
        PerformancePreferences::invalidate_cache();
    }

    #[test]
    fn sql_formatting_load_returns_cached_value_without_disk_reread() {
        invalidate_sql_formatting_cache();
        let prefs = SqlFormattingPreferences {
            format_as_you_go: true,
            indent_width_raw: 7,
            ..Default::default()
        };
        *sql_formatting_prefs_cache().write() = Some(prefs);
        let loaded = load_sql_formatting_preferences();
        assert!(loaded.format_as_you_go);
        assert_eq!(loaded.indent_width_raw, 7);
        invalidate_sql_formatting_cache();
    }

    #[test]
    fn result_run_history_runs_default_and_clamp() {
        let p = PerformancePreferences::default();
        assert_eq!(p.result_run_history_runs(), 5);
        let high = PerformancePreferences {
            result_run_history_runs: 99,
            ..Default::default()
        };
        assert_eq!(high.result_run_history_runs(), 20);
        let parsed: PerformancePreferences =
            serde_json::from_str(r#"{ "spillRowThreshold": 9000 }"#).unwrap();
        assert_eq!(parsed.result_run_history_runs(), 5);
    }

    #[test]
    fn performance_budget_default_and_clamp() {
        // Prefs files written before the byte budget existed must load with
        // the 64 MiB default (struct-level #[serde(default)]).
        let p: PerformancePreferences =
            serde_json::from_str(r#"{ "spillRowThreshold": 9000 }"#).unwrap();
        assert_eq!(p.spill_row_threshold, 9000);
        assert_eq!(p.result_memory_budget_bytes(), 64 * 1024 * 1024);

        let low = PerformancePreferences {
            result_memory_budget_bytes: 0,
            ..Default::default()
        };
        assert_eq!(low.result_memory_budget_bytes(), 1024 * 1024);
        let high = PerformancePreferences {
            result_memory_budget_bytes: usize::MAX,
            ..Default::default()
        };
        assert_eq!(high.result_memory_budget_bytes(), 2 * 1024 * 1024 * 1024);
    }

    #[test]
    fn performance_budget_mib_display() {
        let p = PerformancePreferences {
            result_memory_budget_bytes: 64 * 1024 * 1024,
            ..Default::default()
        };
        assert_eq!(p.result_memory_budget_mib(), 64);
        // Fractional MiB rounds to the nearest whole MiB for display.
        let odd = PerformancePreferences {
            result_memory_budget_bytes: 64 * 1024 * 1024 + 700 * 1024,
            ..Default::default()
        };
        assert_eq!(odd.result_memory_budget_mib(), 65);
    }

    #[test]
    fn performance_budget_mib_input_stores_bytes_and_clamps() {
        let mut p = PerformancePreferences::default();
        p.set_result_memory_budget_mib(128);
        assert_eq!(p.result_memory_budget_bytes, 128 * 1024 * 1024);

        // Both edges clamp to the byte accessor's 1 MiB ..= 2 GiB range.
        p.set_result_memory_budget_mib(0);
        assert_eq!(p.result_memory_budget_bytes, 1024 * 1024);
        assert_eq!(p.result_memory_budget_mib(), 1);
        p.set_result_memory_budget_mib(usize::MAX);
        assert_eq!(p.result_memory_budget_bytes, 2 * 1024 * 1024 * 1024);
        assert_eq!(p.result_memory_budget_mib(), 2048);
    }

    #[test]
    fn performance_budget_old_prefs_bytes_on_disk_still_load() {
        // Prefs files written when the UI took raw bytes keep the same field
        // name and unit on disk.
        let p: PerformancePreferences =
            serde_json::from_str(r#"{ "resultMemoryBudgetBytes": 33554432 }"#).unwrap();
        assert_eq!(p.result_memory_budget_bytes, 32 * 1024 * 1024);
        assert_eq!(p.result_memory_budget_mib(), 32);
    }

    #[test]
    fn capital_letter_alias_basic() {
        let alias = EditorPreferences::capital_letter_alias("PaymentMethod", false);
        assert_eq!(alias.as_deref(), Some("pm"));
    }

    #[test]
    fn capital_letter_alias_with_schema() {
        let alias = EditorPreferences::capital_letter_alias("Shared.PaymentMethod", true);
        assert_eq!(alias.as_deref(), Some("spm"));
    }

    #[test]
    fn capital_letter_alias_no_schema() {
        let alias = EditorPreferences::capital_letter_alias("Shared.PaymentMethod", false);
        assert_eq!(alias.as_deref(), Some("pm"));
    }

    #[test]
    fn capital_letter_alias_empty() {
        let alias = EditorPreferences::capital_letter_alias("", false);
        assert_eq!(alias, None);
    }

    #[test]
    fn capital_letter_alias_no_uppercase() {
        let alias = EditorPreferences::capital_letter_alias("lowercase", false);
        assert_eq!(alias, None);
    }

    #[test]
    fn alias_suggestion_do_not_suggest() {
        let p = EditorPreferences {
            alias_generation: AliasGeneration::DoNotSuggest,
            ..Default::default()
        };
        assert_eq!(p.alias_suggestion("PaymentMethod"), None);
    }

    #[test]
    fn alias_suggestion_capital_letters() {
        let p = EditorPreferences::default();
        assert_eq!(p.alias_suggestion("PaymentMethod").as_deref(), Some("pm"));
    }

    #[test]
    fn alias_suggestion_auto_based_on_join() {
        let p = EditorPreferences {
            alias_generation: AliasGeneration::AutoBasedOnJoin,
            ..EditorPreferences::default()
        };
        assert_eq!(p.alias_suggestion("PaymentMethod").as_deref(), Some("p"));
    }

    /// The IntelliSense pane's switches all default on, including for
    /// settings files saved before they existed.
    #[test]
    fn intellisense_matching_and_as_keyword_default_on() {
        for p in [
            EditorPreferences::default(),
            serde_json::from_str::<EditorPreferences>("{}").unwrap(),
        ] {
            assert!(p.alias_as_keyword && p.hump_matching && p.schema_in_search);
        }
        assert_eq!(
            EditorPreferences::default().aliased_table("Orders", "o"),
            "Orders AS o"
        );
    }

    #[test]
    fn saved_editor_defaults_pane_opens_the_intellisense_pane() {
        let pane: PreferencesPane = serde_json::from_str(r#""EditorDefaults""#).unwrap();
        assert_eq!(pane, PreferencesPane::EditorIntelliSense);
    }

    #[test]
    fn auto_add_table_alias_defaults_on() {
        let p = EditorPreferences::default();
        assert!(p.auto_add_table_alias);
    }

    /// Saved files predating the setting carry the retired, never-surfaced
    /// `autoAliasOnTableCompletion: false`; they pick up the new default
    /// (add automatically) rather than that stale value.
    #[test]
    fn auto_add_table_alias_ignores_the_retired_key_and_round_trips() {
        let legacy: EditorPreferences =
            serde_json::from_str(r#"{"autoAliasOnTableCompletion": false}"#).unwrap();
        assert!(legacy.auto_add_table_alias);

        let off = EditorPreferences {
            auto_add_table_alias: false,
            ..EditorPreferences::default()
        };
        let json = serde_json::to_string(&off).unwrap();
        assert!(json.contains("\"autoAddTableAlias\":false"), "{json}");
        let back: EditorPreferences = serde_json::from_str(&json).unwrap();
        assert!(!back.auto_add_table_alias);
    }

    #[test]
    fn result_formatting_default() {
        let p = ResultFormattingPreferences::default();
        assert_eq!(p.defaults.bool_format.style, BoolFormatStyle::TrueFalse);
        assert!(p.defaults.uuid_format.show_dashes);
        assert_eq!(p.defaults.decimal_format.decimal_places, 6);
        assert!(p.column_overrides.is_empty());
        assert!(p.exports_masked);
        assert!(p.show_fk_labels);
        assert_eq!(p.fk_label_mode, FkLabelMode::Beside);
    }

    #[test]
    fn result_formatting_fk_label_defaults() {
        let p = ResultFormattingPreferences::default();
        assert!(p.show_fk_labels);
        assert_eq!(p.fk_label_mode, FkLabelMode::Beside);
        let from_json: ResultFormattingPreferences = serde_json::from_str("{}").unwrap();
        assert!(from_json.show_fk_labels);
        assert_eq!(from_json.fk_label_mode, FkLabelMode::Beside);
    }

    #[test]
    fn result_formatting_empty_json_defaults_exports_masked() {
        let p: ResultFormattingPreferences = serde_json::from_str("{}").unwrap();
        assert!(p.exports_masked);
        assert!(p.column_overrides.is_empty());
        assert_eq!(p.density, GridDensity::Normal);
        assert_eq!(p.cell_detail_side, CellDetailSide::Right);
    }

    #[test]
    fn result_formatting_defaults_cell_detail_side_to_right() {
        let p: ResultFormattingPreferences = serde_json::from_str("{}").unwrap();
        assert_eq!(p.cell_detail_side, CellDetailSide::Right);
        assert_eq!(
            ResultFormattingPreferences::default().cell_detail_side,
            CellDetailSide::Right
        );
    }

    #[test]
    fn grid_density_metrics_are_monotonic() {
        let compact = GridDensity::Compact.metrics();
        let normal = GridDensity::Normal.metrics();
        let comfortable = GridDensity::Comfortable.metrics();
        assert!(compact.row_height < normal.row_height);
        assert!(normal.row_height < comfortable.row_height);
        assert!(compact.header_height < normal.header_height);
        assert!(normal.header_height < comfortable.header_height);
        assert!(compact.font_size < normal.font_size);
        assert!(normal.font_size < comfortable.font_size);
    }

    #[test]
    fn grid_density_default_is_normal_and_round_trips() {
        assert_eq!(GridDensity::default(), GridDensity::Normal);
        let json = serde_json::to_string(&GridDensity::Comfortable).unwrap();
        let back: GridDensity = serde_json::from_str(&json).unwrap();
        assert_eq!(back, GridDensity::Comfortable);
        let from_empty: ResultFormattingPreferences = serde_json::from_str("{}").unwrap();
        assert_eq!(from_empty.density, GridDensity::Normal);
    }

    #[test]
    fn masking_rules_missing_file_is_default() {
        // load() maps a missing or unreadable file to Default, the same
        // contract as decoding empty JSON.
        let p: crate::masking_rules::MaskingRules = serde_json::from_str("{}").unwrap();
        assert_eq!(p, crate::masking_rules::MaskingRules::default());
        assert!(p.enabled);
        assert!(!p.patterns.is_empty());
    }

    #[test]
    fn column_format_round_trips_new_kinds() {
        let fmt = ColumnFormat {
            kind: ResultKind::JsonPath,
            json_path_format: JsonPathFormat {
                path: "$.a".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        let json = serde_json::to_string(&fmt).unwrap();
        let back: ColumnFormat = serde_json::from_str(&json).unwrap();
        assert_eq!(fmt, back);
    }

    #[test]
    fn legacy_column_format_json_without_new_fields_still_loads() {
        let fmt: ColumnFormat = serde_json::from_str(r#"{"kind":"mask"}"#).unwrap();
        assert_eq!(fmt.kind, ResultKind::Mask);
        assert_eq!(fmt.number_style_format, NumberStyleFormat::default());
    }

    #[test]
    fn format_bool_true_false() {
        let p = ResultFormattingPreferences::default();
        assert_eq!(p.format_bool(true), "true");
        assert_eq!(p.format_bool(false), "false");
    }

    #[test]
    fn format_bool_yn() {
        let mut p = ResultFormattingPreferences::default();
        p.defaults.bool_format.style = BoolFormatStyle::YN;
        assert_eq!(p.format_bool(true), "y");
        assert_eq!(p.format_bool(false), "n");
    }

    #[test]
    fn format_bool_custom() {
        let mut p = ResultFormattingPreferences::default();
        p.defaults.bool_format.style = BoolFormatStyle::Custom;
        p.defaults.bool_format.custom_true = "✓".to_string();
        p.defaults.bool_format.custom_false = "✗".to_string();
        assert_eq!(p.format_bool(true), "✓");
        assert_eq!(p.format_bool(false), "✗");
    }

    #[test]
    fn format_uuid_with_dashes() {
        let p = ResultFormattingPreferences::default();
        let uuid = "550e8400-e29b-41d4-a716-446655440000";
        assert_eq!(p.format_uuid(uuid), uuid);
    }

    #[test]
    fn format_uuid_no_dashes() {
        let mut p = ResultFormattingPreferences::default();
        p.defaults.uuid_format.show_dashes = false;
        let uuid = "550e8400-e29b-41d4-a716-446655440000";
        assert_eq!(p.format_uuid(uuid), "550e8400e29b41d4a716446655440000");
    }

    #[test]
    fn format_integer_with_separators() {
        let p = ResultFormattingPreferences::default();
        assert_eq!(p.format_integer(1234567), "1,234,567");
    }

    #[test]
    fn format_integer_no_separators() {
        let mut p = ResultFormattingPreferences::default();
        p.defaults.integer_format.use_thousands_separators = false;
        assert_eq!(p.format_integer(1234567), "1234567");
    }

    #[test]
    fn format_integer_negative() {
        let p = ResultFormattingPreferences::default();
        assert_eq!(p.format_integer(-1234), "-1,234");
    }

    #[test]
    fn format_integer_negative_parens() {
        let mut p = ResultFormattingPreferences::default();
        p.defaults.integer_format.negative_format = NegativeFormatStyle::Parentheses;
        assert_eq!(p.format_integer(-1234), "(1,234)");
    }

    #[test]
    fn format_integer_metric() {
        let mut p = ResultFormattingPreferences::default();
        p.defaults.integer_format.reduce_large_to_metric = true;
        p.defaults.integer_format.metric_threshold = 1000;
        p.defaults.integer_format.metric_decimals = 2;
        assert_eq!(p.format_integer(1500), "1.50K");
    }

    #[test]
    fn format_integer_metric_no_decimals() {
        let mut p = ResultFormattingPreferences::default();
        p.defaults.integer_format.reduce_large_to_metric = true;
        p.defaults.integer_format.metric_threshold = 1000;
        p.defaults.integer_format.metric_decimals = 0;
        assert_eq!(p.format_integer(1500), "2K");
    }

    #[test]
    #[allow(clippy::approx_constant)]
    fn format_decimal_basic() {
        let p = ResultFormattingPreferences::default();
        assert_eq!(p.format_decimal(3.141592), "3.141592");
    }

    #[test]
    #[allow(clippy::approx_constant)]
    fn format_decimal_places() {
        let mut p = ResultFormattingPreferences::default();
        p.defaults.decimal_format.decimal_places = 2;
        assert_eq!(p.format_decimal(3.141592), "3.14");
    }

    #[test]
    fn format_decimal_negative_parens() {
        let mut p = ResultFormattingPreferences::default();
        p.defaults.decimal_format.negative_format = NegativeFormatStyle::Parentheses;
        p.defaults.decimal_format.decimal_places = 2;
        assert_eq!(p.format_decimal(-42.5), "(42.50)");
    }

    #[test]
    fn column_format_is_inherit() {
        assert!(ColumnFormat::default().is_inherit());
        assert!(!ColumnFormat::default_for_kind(ResultKind::Bool).is_inherit());
    }

    #[test]
    fn preferences_tree_builds() {
        let tree = build_preferences_tree();
        // Performance and Gestures live under Results now.
        assert_eq!(tree.len(), 8); // Appearance, Editor, AI, Results, Data Model, IntelliSense Cache, Workspace, Debug
    }

    #[test]
    fn preferences_tree_appearance_splits_themes_display_and_language() {
        let tree = build_preferences_tree();
        let appearance = tree.iter().find(|n| n.id == "Appearance").unwrap();
        assert!(appearance.is_folder());
        assert_eq!(appearance.children.len(), 3);
        assert_eq!(appearance.children[0].pane, Some(PreferencesPane::Themes));
        assert_eq!(appearance.children[1].pane, Some(PreferencesPane::Display));
        assert_eq!(appearance.children[2].pane, Some(PreferencesPane::Language));
    }

    #[test]
    fn appearance_prefs_migrate_legacy_high_contrast_mode() {
        let json = r#"{"themeMode":"highContrast"}"#;
        let mut prefs: AppearancePreferences = serde_json::from_str(json).unwrap();
        prefs.migrate_high_contrast_mode();
        assert_eq!(prefs.theme_mode, ThemeModePreference::Dark);
        assert_eq!(prefs.dark_theme, crate::theme::ColorTheme::HighContrastDark);
        assert_eq!(prefs.light_theme, crate::theme::ColorTheme::Light);
    }

    #[test]
    fn high_contrast_shortcut_keeps_light_dark_system_mode() {
        let mut light = AppearancePreferences {
            theme_mode: ThemeModePreference::Light,
            ..Default::default()
        };
        light.apply_high_contrast_shortcut();
        assert_eq!(light.theme_mode, ThemeModePreference::Light);
        assert_eq!(
            light.light_theme,
            crate::theme::ColorTheme::HighContrastLight
        );
        assert_eq!(light.dark_theme, crate::theme::ColorTheme::Dark);

        let mut dark = AppearancePreferences {
            theme_mode: ThemeModePreference::Dark,
            ..Default::default()
        };
        dark.apply_high_contrast_shortcut();
        assert_eq!(dark.theme_mode, ThemeModePreference::Dark);
        assert_eq!(dark.dark_theme, crate::theme::ColorTheme::HighContrastDark);

        let mut system = AppearancePreferences::default();
        system.apply_high_contrast_shortcut();
        assert_eq!(system.theme_mode, ThemeModePreference::System);
        assert_eq!(
            system.light_theme,
            crate::theme::ColorTheme::HighContrastLight
        );
        assert_eq!(
            system.dark_theme,
            crate::theme::ColorTheme::HighContrastDark
        );
    }

    #[test]
    fn appearance_prefs_deserialize_named_themes() {
        let json = r#"{"themeMode":"system","lightTheme":"lavenderLight","darkTheme":"dark"}"#;
        let prefs: AppearancePreferences = serde_json::from_str(json).unwrap();
        assert_eq!(prefs.theme_mode, ThemeModePreference::System);
        assert_eq!(prefs.light_theme, crate::theme::ColorTheme::LavenderLight);
        assert_eq!(prefs.dark_theme, crate::theme::ColorTheme::Dark);
    }

    #[test]
    fn appearance_prefs_results_font_defaults_and_round_trips() {
        // Files written before the results font existed keep the grid's own
        // platform monospace and follow the density preset's size.
        let prefs: AppearancePreferences =
            serde_json::from_str(r#"{"editorFont":"Menlo","editorFontSize":14}"#).unwrap();
        assert_eq!(prefs.results_font, RESULTS_FONT_DEFAULT);
        assert_eq!(prefs.results_font_size, None);

        let custom = AppearancePreferences {
            results_font: "JetBrains Mono".to_string(),
            results_font_size: Some(16.0),
            ..AppearancePreferences::default()
        };
        let json = serde_json::to_string(&custom).unwrap();
        assert!(json.contains(r#""resultsFont":"JetBrains Mono""#), "{json}");
        let back: AppearancePreferences = serde_json::from_str(&json).unwrap();
        assert_eq!(back, custom);
    }

    #[test]
    fn appearance_language_defaults_and_camel_case_round_trip() {
        let legacy: AppearancePreferences =
            serde_json::from_str(r#"{"editorFont":"Menlo"}"#).unwrap();
        assert_eq!(legacy.language, "en");
        assert_eq!(legacy.custom_translation_file, None);

        let custom = AppearancePreferences {
            language: "es".to_string(),
            custom_translation_file: Some("/tmp/sqlly-es.json".to_string()),
            ..AppearancePreferences::default()
        };
        let json = serde_json::to_string(&custom).unwrap();
        assert!(json.contains(r#""language":"es""#), "{json}");
        assert!(
            json.contains(r#""customTranslationFile":"/tmp/sqlly-es.json""#),
            "{json}"
        );
        assert_eq!(
            serde_json::from_str::<AppearancePreferences>(&json).unwrap(),
            custom
        );
    }

    #[test]
    fn theme_mode_pills_are_light_dark_system() {
        use ThemeModePreference as M;
        assert_eq!(M::MODE_PILLS, [M::Light, M::Dark, M::System]);
        assert!(M::ALL.contains(&M::HighContrast));
        assert_eq!(M::Light.label(), "Light");
        assert_eq!(M::Dark.label(), "Dark");
        assert_eq!(M::System.label(), "System");
        assert_eq!(M::HighContrast.label(), "High Contrast");
    }

    #[test]
    fn preferences_tree_editor_has_8_children() {
        let tree = build_preferences_tree();
        let editor = tree.iter().find(|n| n.title == "Editor").unwrap();
        assert!(editor.is_folder());
        assert_eq!(editor.children.len(), 8);
        assert!(
            editor
                .children
                .iter()
                .any(|c| c.title == "Tabs" && c.pane == Some(PreferencesPane::EditorTabs))
        );
        assert!(
            editor
                .children
                .iter()
                .any(|c| c.title == "DDL Navigation"
                    && c.pane == Some(PreferencesPane::DdlNavigation))
        );
        assert!(editor.children.iter().any(|c| {
            c.title == "Snippets & Quick Actions" && c.pane == Some(PreferencesPane::EditorSnippets)
        }));
    }

    #[test]
    fn ddl_privileges_default_on_including_older_settings_files() {
        assert!(DdlNavigationPreferences::default().include_privileges);
        let older: DdlNavigationPreferences = serde_json::from_str(r#"{"enabled": true}"#).unwrap();
        assert!(older.include_privileges);
        let off: DdlNavigationPreferences =
            serde_json::from_str(r#"{"includePrivileges": false}"#).unwrap();
        assert!(!off.ddl_options().include_privileges);
    }

    #[test]
    fn export_preferences_defaults() {
        let prefs = ExportPreferences::default();
        // All twelve standard formats on by default.
        for format in crate::results::ExportFormat::all() {
            assert!(
                prefs.standard_enabled(*format),
                "{:?} should default to enabled",
                format
            );
        }
        // Programming section off by default, so no language appears...
        assert!(!prefs.programming_enabled);
        for lang in crate::result_export_code::CodeLanguage::all() {
            assert!(!prefs.language_enabled(*lang));
        }
        // ...even though every per-language toggle defaults to on.
        let mut on = prefs;
        on.programming_enabled = true;
        for lang in crate::result_export_code::CodeLanguage::all() {
            assert!(on.language_enabled(*lang));
        }
    }

    #[test]
    fn export_preferences_per_format_gating() {
        let mut prefs = ExportPreferences {
            csv: false,
            xlsx: false,
            ..Default::default()
        };
        assert!(!prefs.standard_enabled(crate::results::ExportFormat::Csv));
        assert!(!prefs.standard_enabled(crate::results::ExportFormat::Xlsx));
        assert!(prefs.standard_enabled(crate::results::ExportFormat::Json));

        prefs.programming_enabled = true;
        prefs.rust = false;
        assert!(!prefs.language_enabled(crate::result_export_code::CodeLanguage::Rust));
        assert!(prefs.language_enabled(crate::result_export_code::CodeLanguage::Go));
    }

    #[test]
    fn export_preferences_serde_round_trip_and_missing_fields() {
        let prefs = ExportPreferences::default();
        let json = serde_json::to_string(&prefs).unwrap();
        let back: ExportPreferences = serde_json::from_str(&json).unwrap();
        assert_eq!(prefs, back);

        // A file from an older version (missing fields) gets the defaults.
        let partial: ExportPreferences = serde_json::from_str("{\"csv\":false}").unwrap();
        assert!(!partial.csv);
        assert!(partial.json);
        assert!(!partial.programming_enabled);
    }

    #[test]
    fn export_preferences_empty_json_defaults_copy_format_and_json_lines() {
        let prefs: ExportPreferences = serde_json::from_str("{}").unwrap();
        assert_eq!(prefs.default_copy_format, CopyFormat::Tsv);
        assert!(prefs.json_lines);
    }

    #[test]
    fn export_preferences_formula_safe_defaults_on_and_round_trips() {
        // A file from before the option existed gets the safe default.
        let prefs: ExportPreferences = serde_json::from_str("{}").unwrap();
        assert!(prefs.formula_safe);
        let off: ExportPreferences = serde_json::from_str("{\"formulaSafe\":false}").unwrap();
        assert!(!off.formula_safe);
        let json = serde_json::to_string(&off).unwrap();
        assert!(json.contains("\"formulaSafe\":false"));
    }

    #[test]
    fn export_options_preferences_carry_formula_safe_into_options() {
        use crate::results::ExportFormat;
        let prefs = ExportOptionsPreferences::default();
        assert!(prefs.to_options_with(ExportFormat::Csv, true).formula_safe);
        assert!(!prefs.to_options_with(ExportFormat::Tsv, false).formula_safe);
        // The other file-export fields still come from these preferences.
        let custom = ExportOptionsPreferences {
            header_row: false,
            gzip: true,
            ..Default::default()
        };
        let opts = custom.to_options_with(ExportFormat::Csv, false);
        assert!(!opts.header_row);
        assert!(opts.gzip);
        assert!(!opts.formula_safe);
    }

    #[test]
    fn preferences_tree_results_has_13_children() {
        let tree = build_preferences_tree();
        let results = tree.iter().find(|n| n.title == "Results").unwrap();
        assert!(results.is_folder());
        // General + History + 6 formatting panes + Column overrides + Sensitive columns + Export + Protobuf Schemas + Performance + Gestures.
        assert_eq!(results.children.len(), 13);
        assert!(
            results
                .children
                .iter()
                .any(|c| c.title == "Protobuf Schemas")
        );
        assert!(
            results
                .children
                .iter()
                .any(|c| c.title == "Sensitive columns")
        );
        assert!(results.children.iter().any(|c| c.title == "History"));
        assert!(results.children.iter().any(|c| c.title == "General"));
        assert!(results.children.iter().any(|c| c.title == "Export"));
        assert!(results.children.iter().any(|c| c.title == "Performance"));
        assert!(results.children.iter().any(|c| c.title == "Gestures"));
    }

    #[test]
    fn preferences_tree_node_matches() {
        let tree = build_preferences_tree();
        let editor = tree.iter().find(|n| n.title == "Editor").unwrap();
        assert!(editor.matches("editor"));
        assert!(editor.matches("general"));
        assert!(editor.matches("formatting"));
        assert!(!editor.matches("xyzabc"));
    }

    #[test]
    fn preferences_tree_empty_query_matches_all() {
        let tree = build_preferences_tree();
        for node in &tree {
            assert!(node.matches(""));
        }
    }

    // ------------------------------------------------------------------
    // SQL formatting preferences (port of SQLFormatterPreferencesTests.swift)
    // ------------------------------------------------------------------

    #[test]
    fn sql_formatting_default_values() {
        let p = SqlFormattingPreferences::default();
        assert_eq!(p.preset, Preset::Default);
        assert_eq!(p.keyword_case, KeywordCase::Uppercase);
        assert!(!p.keyword_group_casing.enabled);
        assert_eq!(p.identifier_case, IdentifierCase::Preserve);
        assert_eq!(p.identifier_quoting, IdentifierQuoting::PreserveExisting);
        assert_eq!(
            p.major_clause_placement,
            MajorClausePlacement::EachOnNewLine
        );
        assert!(p.break_after_select);
        assert!(!p.blank_line_before_major_clauses);
        assert_eq!(p.select_column_layout, SelectListStyle::Stacked);
        assert_eq!(p.inline_column_threshold_raw, 3);
        assert_eq!(p.comma_style, CommaStyle::Trailing);
        assert_eq!(
            p.boolean_operator_placement,
            BooleanOperatorPlacement::LineLeading
        );
        assert!(!p.parenthesize_mixed_boolean_logic);
        assert!(p.spaces_around_operators);
        assert!(!p.align_comparison_operators);
        assert_eq!(p.join_layout, JoinLayout::Aligned);
        assert_eq!(p.on_clause_placement, OnClausePlacement::AfterJoin);
        assert!(!p.multi_condition_on);
        assert_eq!(p.function_case, FunctionCase::Uppercase);
        assert!(!p.space_before_function_parentheses);
        assert_eq!(p.function_arg_style, FunctionArgStyle::Inline);
        assert_eq!(p.subquery_style, SubqueryStyle::Indented);
        assert_eq!(p.cte_style, CteStyle::Stacked);
        assert!(p.new_line_after_with);
        assert_eq!(p.dml_style, DmlStyle::Standard);
        assert_eq!(p.values_style, ValuesStyle::Stacked);
        assert!(!p.insert_one_column_per_line);
        assert!(!p.align_values_rows);
        assert!(!p.align_set_operators);
        assert!(!p.merge_section_separation);
        assert_eq!(p.ddl_style, DdlStyle::Standard);
        assert!(!p.align_data_types);
        assert!(!p.align_nullability);
        assert!(!p.constraints_after_columns);
        assert!(!p.multiline_index_columns);
        assert_eq!(p.comment_style, CommentStyle::Preserve);
        assert!(p.keep_inline_comments_inline);
        assert!(!p.move_trailing_comments_with_expression);
        assert!(!p.format_inside_block_comments);
        assert!(p.respect_control_comments);
        assert_eq!(p.control_comment_prefix, "formatter");
        assert!(!p.format_on_save);
        assert!(!p.format_on_paste);
        assert!(p.use_editor_indentation);
        assert_eq!(p.indent_style, IndentStyle::Spaces);
        assert_eq!(p.indent_width_raw, 4);
        assert_eq!(p.continuation_indent_size_raw, 4);
        assert!(p.indent_nested_queries);
        assert!(p.indent_join_on);
        assert_eq!(p.max_line_length_raw, 120);
        assert!(p.wrap_long_lines);
        assert!(p.keep_short_queries_on_one_line);
        assert_eq!(p.short_query_max_length_raw, 80);
        assert_eq!(p.dialect, SqlDialect::AutoDetect);
        assert_eq!(p.case_expression_style, CaseExpressionStyle::Stacked);
        assert_eq!(p.in_list_style, InListStyle::Wrap);
        assert_eq!(p.order_group_layout, OrderGroupLayout::MatchSelectList);
        assert_eq!(p.semicolon_style, SemicolonStyle::EndOfStatement);
        assert_eq!(p.max_consecutive_blank_lines_raw, 1);
        assert!(!p.align_column_aliases);
    }

    #[test]
    fn sql_formatting_keyword_group_casing_defaults_all_uppercase() {
        let g = SqlFormattingPreferences::default().keyword_group_casing;
        assert!(!g.enabled);
        for i in 0..KeywordGroupCasing::CATEGORY_LABELS.len() {
            assert_eq!(g.category(i), Some(KeywordCase::Uppercase));
        }
    }

    #[test]
    fn sql_formatting_default_preset_matches_default() {
        assert_eq!(
            SqlFormattingPreferences::for_preset(Preset::Default),
            SqlFormattingPreferences::default()
        );
    }

    #[test]
    fn sql_formatting_custom_preset_returns_default() {
        assert_eq!(
            SqlFormattingPreferences::for_preset(Preset::Custom),
            SqlFormattingPreferences::default()
        );
    }

    #[test]
    fn sql_formatting_all_presets_have_distinct_settings() {
        let presets = [
            Preset::Default,
            Preset::Compact,
            Preset::Expanded,
            Preset::UppercaseKeywords,
            Preset::LowercaseKeywords,
            Preset::PreserveExisting,
        ];
        let settings: Vec<_> = presets
            .iter()
            .map(|p| SqlFormattingPreferences::for_preset(*p))
            .collect();
        for i in 0..settings.len() {
            for j in (i + 1)..settings.len() {
                assert_ne!(
                    settings[i], settings[j],
                    "presets {:?} and {:?} should differ",
                    presets[i], presets[j]
                );
            }
        }
    }

    #[test]
    fn sql_formatting_compact_preset_differs_from_default() {
        let c = SqlFormattingPreferences::for_preset(Preset::Compact);
        assert_eq!(c.preset, Preset::Compact);
        assert_eq!(
            c.major_clause_placement,
            MajorClausePlacement::CompactWhenShort
        );
        assert_eq!(c.select_column_layout, SelectListStyle::OneLineWhenShort);
        assert_eq!(
            c.boolean_operator_placement,
            BooleanOperatorPlacement::LineTrailing
        );
        assert_eq!(c.join_layout, JoinLayout::Compact);
        assert_eq!(c.subquery_style, SubqueryStyle::Aligned);
        assert_eq!(c.cte_style, CteStyle::Inline);
        assert_eq!(c.dml_style, DmlStyle::Compact);
        assert_eq!(c.values_style, ValuesStyle::Inline);
        assert_eq!(c.ddl_style, DdlStyle::Compact);
        assert!(!c.wrap_long_lines);
        // Deltas the Swift factory spells out that the test above doesn't cover.
        assert_eq!(c.indent_style, IndentStyle::Tabs);
        assert_eq!(c.indent_width_raw, 2);
        assert_eq!(c.continuation_indent_size_raw, 2);
        assert!(!c.break_after_select);
        assert!(!c.new_line_after_with);
        assert!(!c.indent_nested_queries);
        assert!(!c.indent_join_on);
        assert_eq!(c.short_query_max_length_raw, 100);
    }

    #[test]
    fn sql_formatting_expanded_preset_differs_from_default() {
        let e = SqlFormattingPreferences::for_preset(Preset::Expanded);
        assert_eq!(e.preset, Preset::Expanded);
        assert!(e.blank_line_before_major_clauses);
        assert_eq!(e.comma_style, CommaStyle::Leading);
        assert!(e.parenthesize_mixed_boolean_logic);
        assert!(e.align_comparison_operators);
        assert_eq!(e.join_layout, JoinLayout::Stacked);
        assert_eq!(e.on_clause_placement, OnClausePlacement::OnNewLine);
        assert!(e.multi_condition_on);
        assert_eq!(e.function_arg_style, FunctionArgStyle::Stacked);
        assert_eq!(e.subquery_style, SubqueryStyle::Expanded);
        assert!(e.insert_one_column_per_line);
        assert!(e.align_values_rows);
        assert!(e.align_data_types);
        assert!(e.align_nullability);
        assert_eq!(e.comment_style, CommentStyle::Align);
        assert!(e.format_inside_block_comments);
        assert!(!e.use_editor_indentation);
        assert_eq!(e.continuation_indent_size_raw, 8);
        assert!(!e.keep_short_queries_on_one_line);
        // Remaining Swift deltas.
        assert_eq!(e.inline_column_threshold_raw, 1);
        assert_eq!(e.dml_style, DmlStyle::Expanded);
        assert_eq!(e.ddl_style, DdlStyle::Expanded);
        assert!(e.align_set_operators);
        assert!(e.merge_section_separation);
        assert!(e.constraints_after_columns);
        assert!(e.multiline_index_columns);
        assert!(e.move_trailing_comments_with_expression);
        assert_eq!(e.max_line_length_raw, 100);
        assert_eq!(e.short_query_max_length_raw, 60);
    }

    #[test]
    fn sql_formatting_uppercase_keywords_preset() {
        let p = SqlFormattingPreferences::for_preset(Preset::UppercaseKeywords);
        assert_eq!(p.preset, Preset::UppercaseKeywords);
        assert_eq!(p.keyword_case, KeywordCase::Uppercase);
        assert_eq!(p.function_case, FunctionCase::Uppercase);
    }

    #[test]
    fn sql_formatting_lowercase_keywords_preset() {
        let p = SqlFormattingPreferences::for_preset(Preset::LowercaseKeywords);
        assert_eq!(p.preset, Preset::LowercaseKeywords);
        assert_eq!(p.keyword_case, KeywordCase::Lowercase);
        assert_eq!(p.function_case, FunctionCase::Lowercase);
    }

    #[test]
    fn sql_formatting_preserve_existing_preset() {
        let p = SqlFormattingPreferences::for_preset(Preset::PreserveExisting);
        assert_eq!(p.preset, Preset::PreserveExisting);
        assert_eq!(p.keyword_case, KeywordCase::Preserve);
        assert_eq!(p.identifier_case, IdentifierCase::Preserve);
        assert_eq!(p.identifier_quoting, IdentifierQuoting::PreserveExisting);
        assert_eq!(
            p.major_clause_placement,
            MajorClausePlacement::PreserveExisting
        );
        assert_eq!(p.select_column_layout, SelectListStyle::PreserveExisting);
        assert_eq!(p.comma_style, CommaStyle::PreserveExisting);
        assert_eq!(
            p.boolean_operator_placement,
            BooleanOperatorPlacement::PreserveExisting
        );
        assert_eq!(p.on_clause_placement, OnClausePlacement::PreserveExisting);
        assert_eq!(p.function_case, FunctionCase::Preserve);
        assert_eq!(p.dialect, SqlDialect::AutoDetect);
        assert_eq!(p.max_line_length_raw, 0);
        // Every keyword group follows the preset, not the uppercase default.
        for i in 0..KeywordGroupCasing::CATEGORY_LABELS.len() {
            assert_eq!(
                p.keyword_group_casing.category(i),
                Some(KeywordCase::Preserve)
            );
        }
    }

    #[test]
    fn sql_formatting_spec_compliant_defaults() {
        let d = SqlFormattingPreferences::default();
        assert!(!d.format_on_paste);
        assert_eq!(d.max_line_length_raw, 120);
        assert_eq!(d.function_case, FunctionCase::Uppercase);
        assert_eq!(d.identifier_quoting, IdentifierQuoting::PreserveExisting);
        assert_eq!(d.dialect, SqlDialect::AutoDetect);
        assert_eq!(d.identifier_case, IdentifierCase::Preserve);
    }

    #[test]
    fn sql_formatting_enum_case_counts() {
        assert_eq!(Preset::all().len(), 7);
        assert_eq!(
            Preset::all()
                .iter()
                .map(|p| p.display_title())
                .collect::<Vec<_>>(),
            [
                "Default",
                "Compact",
                "Expanded",
                "Uppercase Keywords",
                "Lowercase Keywords",
                "Preserve Existing",
                "Custom",
            ]
        );

        assert_eq!(KeywordCase::all().len(), 5);
        assert!(KeywordCase::all().contains(&KeywordCase::MatchSurroundingStyle));

        assert_eq!(IdentifierCase::all().len(), 4);
        assert!(IdentifierCase::all().contains(&IdentifierCase::PascalCase));

        assert_eq!(IdentifierQuoting::all().len(), 4);
        for v in [
            IdentifierQuoting::PreserveExisting,
            IdentifierQuoting::QuoteOnlyWhenRequired,
            IdentifierQuoting::AlwaysQuote,
            IdentifierQuoting::RemoveUnnecessaryQuotes,
        ] {
            assert!(IdentifierQuoting::all().contains(&v));
        }

        assert_eq!(CommaStyle::all().len(), 3);
        assert!(CommaStyle::all().contains(&CommaStyle::PreserveExisting));

        assert_eq!(BooleanOperatorPlacement::all().len(), 3);
        assert_eq!(MajorClausePlacement::all().len(), 3);
        assert_eq!(SelectListStyle::all().len(), 4);
        assert!(SelectListStyle::all().contains(&SelectListStyle::OneLineWhenShort));

        assert_eq!(JoinLayout::all().len(), 3);
        assert_eq!(OnClausePlacement::all().len(), 3);
        assert_eq!(FunctionCase::all().len(), 3);
        assert_eq!(FunctionArgStyle::all().len(), 3);
        assert_eq!(SubqueryStyle::all().len(), 3);
        assert_eq!(CteStyle::all().len(), 2);
        assert_eq!(DmlStyle::all().len(), 3);
        assert_eq!(ValuesStyle::all().len(), 2);
        assert_eq!(DdlStyle::all().len(), 3);
        assert_eq!(IndentStyle::all().len(), 2);

        assert!(SqlDialect::all().contains(&SqlDialect::AutoDetect));
        assert_eq!(SqlDialect::AutoDetect.display_title(), "Auto Detect");
        assert_eq!(SqlDialect::all().len(), 9);

        // A formatter must never delete a comment: there is no Remove option.
        assert_eq!(CommentStyle::all().len(), 2);
        assert!(
            !CommentStyle::all()
                .iter()
                .any(|c| c.display_title().contains("Remove"))
        );

        assert_eq!(KeywordGroupCasing::CATEGORY_LABELS.len(), 10);

        assert_eq!(CaseExpressionStyle::all().len(), 3);
        assert_eq!(InListStyle::all().len(), 4);
        assert_eq!(OrderGroupLayout::all().len(), 4);
        assert_eq!(SemicolonStyle::all().len(), 2);
    }

    #[test]
    fn sql_formatting_all_dropdown_options_have_non_empty_display_titles() {
        macro_rules! assert_titled {
            ($($ty:ty),+ $(,)?) => {
                $(
                    assert!(
                        !<$ty>::all().is_empty(),
                        "{} has no variants", stringify!($ty)
                    );
                    for v in <$ty>::all() {
                        assert!(
                            !v.display_title().is_empty(),
                            "{} has an empty display_title", stringify!($ty)
                        );
                    }
                )+
            };
        }
        assert_titled!(
            Preset,
            KeywordCase,
            IdentifierCase,
            IdentifierQuoting,
            MajorClausePlacement,
            SelectListStyle,
            CommaStyle,
            BooleanOperatorPlacement,
            JoinLayout,
            OnClausePlacement,
            FunctionCase,
            FunctionArgStyle,
            SubqueryStyle,
            CteStyle,
            DmlStyle,
            ValuesStyle,
            DdlStyle,
            CommentStyle,
            SqlDialect,
            IndentStyle,
            CaseExpressionStyle,
            InListStyle,
            OrderGroupLayout,
            SemicolonStyle,
        );
    }

    #[test]
    fn sql_formatting_indent_width_clamps() {
        let mut p = SqlFormattingPreferences {
            indent_width_raw: 0,
            ..Default::default()
        };
        assert_eq!(p.indent_width(), 1);
        p.indent_width_raw = 999;
        assert_eq!(p.indent_width(), 8);
        p.indent_width_raw = 4;
        assert_eq!(p.indent_width(), 4);
    }

    #[test]
    fn sql_formatting_continuation_indent_size_clamps() {
        let mut p = SqlFormattingPreferences {
            continuation_indent_size_raw: 0,
            ..Default::default()
        };
        assert_eq!(p.continuation_indent_size(), 1);
        p.continuation_indent_size_raw = 999;
        assert_eq!(p.continuation_indent_size(), 8);
    }

    #[test]
    fn sql_formatting_max_line_length_clamps() {
        // 0 is the sentinel for "never wrap" and must survive the clamp.
        let mut p = SqlFormattingPreferences {
            max_line_length_raw: 0,
            ..Default::default()
        };
        assert_eq!(p.max_line_length(), 0);
        p.max_line_length_raw = 10;
        assert_eq!(p.max_line_length(), 40);
        p.max_line_length_raw = 9999;
        assert_eq!(p.max_line_length(), 500);
        p.max_line_length_raw = 120;
        assert_eq!(p.max_line_length(), 120);
    }

    #[test]
    fn sql_formatting_inline_column_threshold_clamps() {
        let mut p = SqlFormattingPreferences {
            inline_column_threshold_raw: -1,
            ..Default::default()
        };
        assert_eq!(p.inline_column_threshold(), 0);
        p.inline_column_threshold_raw = 999;
        assert_eq!(p.inline_column_threshold(), 20);
    }

    #[test]
    fn sql_formatting_short_query_max_length_clamps() {
        let mut p = SqlFormattingPreferences {
            short_query_max_length_raw: -1,
            ..Default::default()
        };
        assert_eq!(p.short_query_max_length(), 0);
        p.short_query_max_length_raw = 9999;
        assert_eq!(p.short_query_max_length(), 500);
    }

    #[test]
    fn sql_formatting_codable_round_trip() {
        let p = SqlFormattingPreferences {
            keyword_case: KeywordCase::Lowercase,
            identifier_case: IdentifierCase::Lowercase,
            identifier_quoting: IdentifierQuoting::AlwaysQuote,
            major_clause_placement: MajorClausePlacement::CompactWhenShort,
            select_column_layout: SelectListStyle::OneLineWhenShort,
            comma_style: CommaStyle::Leading,
            boolean_operator_placement: BooleanOperatorPlacement::LineTrailing,
            parenthesize_mixed_boolean_logic: true,
            spaces_around_operators: false,
            align_comparison_operators: true,
            join_layout: JoinLayout::Stacked,
            on_clause_placement: OnClausePlacement::OnNewLine,
            multi_condition_on: true,
            function_case: FunctionCase::Uppercase,
            space_before_function_parentheses: true,
            function_arg_style: FunctionArgStyle::Stacked,
            subquery_style: SubqueryStyle::Aligned,
            cte_style: CteStyle::Inline,
            new_line_after_with: false,
            dml_style: DmlStyle::Expanded,
            values_style: ValuesStyle::Inline,
            insert_one_column_per_line: true,
            align_values_rows: true,
            align_set_operators: true,
            merge_section_separation: true,
            ddl_style: DdlStyle::Compact,
            align_data_types: true,
            align_nullability: true,
            constraints_after_columns: true,
            multiline_index_columns: true,
            comment_style: CommentStyle::Align,
            keep_inline_comments_inline: false,
            move_trailing_comments_with_expression: true,
            format_inside_block_comments: true,
            respect_control_comments: false,
            control_comment_prefix: "fmt".to_string(),
            format_as_you_go: true,
            format_on_save: true,
            format_on_paste: true,
            use_editor_indentation: false,
            indent_style: IndentStyle::Tabs,
            indent_width_raw: 2,
            continuation_indent_size_raw: 3,
            indent_nested_queries: false,
            indent_join_on: false,
            max_line_length_raw: 100,
            wrap_long_lines: false,
            keep_short_queries_on_one_line: false,
            short_query_max_length_raw: 60,
            dialect: SqlDialect::Postgresql,
            case_expression_style: CaseExpressionStyle::Compact,
            in_list_style: InListStyle::Stacked,
            order_group_layout: OrderGroupLayout::Inline,
            semicolon_style: SemicolonStyle::OwnLine,
            max_consecutive_blank_lines_raw: 2,
            align_column_aliases: true,
            ..Default::default()
        };
        let json = serde_json::to_vec(&p).unwrap();
        let decoded: SqlFormattingPreferences = serde_json::from_slice(&json).unwrap();
        assert_eq!(decoded, p);
    }

    #[test]
    fn sql_formatting_keyword_group_casing_round_trip() {
        let mut p = SqlFormattingPreferences::default();
        p.keyword_group_casing.enabled = true;
        p.keyword_group_casing.data_types = KeywordCase::Lowercase;
        p.keyword_group_casing.functions = KeywordCase::Preserve;
        p.keyword_group_casing.transaction = KeywordCase::TitleCase;
        let json = serde_json::to_vec(&p).unwrap();
        let decoded: SqlFormattingPreferences = serde_json::from_slice(&json).unwrap();
        assert_eq!(decoded.keyword_group_casing, p.keyword_group_casing);
    }

    #[test]
    fn sql_formatting_partial_json_decodes_with_defaults() {
        let decoded: SqlFormattingPreferences =
            serde_json::from_str(r#"{"keywordCase": "lowercase", "maxLineLength": 100}"#).unwrap();
        let d = SqlFormattingPreferences::default();
        assert_eq!(decoded.keyword_case, KeywordCase::Lowercase);
        assert_eq!(decoded.max_line_length_raw, 100);
        assert_eq!(decoded.identifier_case, d.identifier_case);
        assert_eq!(decoded.comma_style, d.comma_style);
        assert_eq!(decoded.dialect, d.dialect);
        assert_eq!(decoded.preset, d.preset);
    }

    #[test]
    fn sql_formatting_empty_json_decodes_with_defaults() {
        let decoded: SqlFormattingPreferences = serde_json::from_str("{}").unwrap();
        assert_eq!(decoded, SqlFormattingPreferences::default());
    }

    #[test]
    fn sql_formatting_json_keys_match_the_swift_profile_contract() {
        // These keys are the cross-app contract (Swift `CodingKeys`). A serde
        // rename_all slip would silently reset a user's settings to defaults on
        // load, so each one is pinned.
        let json = serde_json::to_string(&SqlFormattingPreferences::default()).unwrap();

        // The dialect key is `dialect`, NOT `sqlDialect`.
        assert!(json.contains("\"dialect\""));
        assert!(!json.contains("\"sqlDialect\""));

        assert!(json.contains("\"majorClausePlacement\""));
        assert!(json.contains("\"selectColumnLayout\""));
        assert!(json.contains("\"spacesAroundOperators\""));
        assert!(json.contains("\"booleanOperatorPlacement\""));
        assert!(json.contains("\"controlCommentPrefix\""));
        assert!(json.contains("formatter"));

        // The four keys camelCase would mangle: Swift keeps the acronyms
        // uppercase, and the `*_raw` fields must not leak "Raw" onto disk.
        assert!(json.contains("\"multiConditionON\""));
        assert!(json.contains("\"newLineAfterWITH\""));
        assert!(json.contains("\"indentJoinON\""));
        assert!(json.contains("\"inlineColumnThreshold\""));
        assert!(json.contains("\"indentWidth\""));
        assert!(json.contains("\"continuationIndentSize\""));
        assert!(json.contains("\"maxLineLength\""));
        assert!(json.contains("\"shortQueryMaxLength\""));
        assert!(!json.contains("Raw\""));

        // §21 gpui-first extensions: plain camelCase keys, no "Raw" leak.
        assert!(json.contains("\"caseExpressionStyle\""));
        assert!(json.contains("\"inListStyle\""));
        assert!(json.contains("\"orderGroupLayout\""));
        assert!(json.contains("\"semicolonStyle\""));
        assert!(json.contains("\"maxConsecutiveBlankLines\""));
        assert!(json.contains("\"alignColumnAliases\""));
        assert!(json.contains("\"fromTablePlacement\":\"newLineIndented\""));
        assert!(json.contains("\"whereConditionPlacement\":\"newLineIndented\""));
    }

    #[test]
    fn sql_formatting_max_consecutive_blank_lines_clamps() {
        let mut p = SqlFormattingPreferences {
            max_consecutive_blank_lines_raw: -1,
            ..Default::default()
        };
        assert_eq!(p.max_consecutive_blank_lines(), 0);
        p.max_consecutive_blank_lines_raw = 99;
        assert_eq!(p.max_consecutive_blank_lines(), 5);
        p.max_consecutive_blank_lines_raw = 2;
        assert_eq!(p.max_consecutive_blank_lines(), 2);
    }

    #[test]
    fn sql_formatting_malformed_json_falls_back_to_defaults() {
        let decoded: SqlFormattingPreferences =
            serde_json::from_str("{ not json").unwrap_or_default();
        assert_eq!(decoded, SqlFormattingPreferences::default());
    }

    #[test]
    fn preferences_tree_workspace_exposes_keyboard_shortcuts() {
        // The pane existed but had no tree entry, so it was reachable only from
        // the menu action -- not by browsing, and not by sidebar search.
        let tree = build_preferences_tree();
        let workspace = tree.iter().find(|n| n.title == "Workspace").unwrap();
        // iCloud Sync is listed on macOS only.
        let expected = if icloud_sync_pane_listed() { 5 } else { 4 };
        assert_eq!(workspace.children.len(), expected);
        assert!(
            workspace
                .children
                .iter()
                .any(|c| c.pane == Some(PreferencesPane::Keybindings))
        );
        assert_eq!(
            workspace
                .children
                .iter()
                .any(|c| c.pane == Some(PreferencesPane::WorkspaceICloudSync)),
            icloud_sync_pane_listed()
        );
        assert!(workspace.matches("keyboard"));
        assert!(workspace.matches("shortcuts"));
    }

    #[test]
    fn every_pane_is_reachable_from_the_tree() {
        fn collect(nodes: &[PreferencesTreeNode], out: &mut Vec<PreferencesPane>) {
            for n in nodes {
                if let Some(p) = &n.pane {
                    out.push(p.clone());
                }
                collect(&n.children, out);
            }
        }
        let mut panes = Vec::new();
        collect(&build_preferences_tree(), &mut panes);
        // Guards the Keybindings regression class: a pane with no tree entry is
        // invisible to browsing and to sidebar search.
        for pane in PreferencesPane::ALL {
            if pane == PreferencesPane::WorkspaceICloudSync && !icloud_sync_pane_listed() {
                assert!(!panes.contains(&pane), "iCloud sync is listed off macOS");
                continue;
            }
            assert!(
                panes.contains(&pane),
                "{:?} has no entry in the preferences tree",
                pane
            );
        }
    }

    #[test]
    fn preferences_pane_display_title() {
        assert_eq!(PreferencesPane::EditorGeneral.display_title(), "General");
        assert_eq!(PreferencesPane::ResultsDate.display_title(), "Date");
        assert_eq!(PreferencesPane::DebugLogging.display_title(), "Logging");
        assert_eq!(
            PreferencesPane::DdlNavigation.display_title(),
            "DDL Navigation"
        );
    }

    #[test]
    fn ddl_navigation_defaults_are_platform_correct() {
        let prefs = DdlNavigationPreferences::default();
        assert!(prefs.enabled);
        assert_eq!(
            prefs.activation_modifier,
            DdlNavigationModifier::PlatformDefault
        );
        let resolved = prefs.activation_modifier.resolved();
        if cfg!(target_os = "macos") {
            assert_eq!(resolved, DdlNavigationModifier::Command);
        } else {
            assert_eq!(resolved, DdlNavigationModifier::Control);
        }
        assert_eq!(prefs.presentation, DdlNavigationPresentation::OpenInNewTab);
        assert_eq!(prefs.popup.width, 900);
        assert_eq!(prefs.popup.height, 700);
        assert_eq!(
            prefs.popup.monitor,
            DdlPopupMonitorPreference::CurrentWindow
        );
    }

    #[test]
    fn ddl_navigation_modifier_exact_matching() {
        let cmd = DdlNavigationModifier::Command;
        assert!(cmd.matches_flags(true, false, false, false));
        assert!(!cmd.matches_flags(true, false, false, true)); // extra shift
        assert!(!cmd.matches_flags(false, true, false, false)); // wrong key
        assert!(!cmd.matches_flags(false, false, false, false)); // nothing held

        let alt = DdlNavigationModifier::Alt;
        assert!(alt.matches_flags(false, false, true, false));
        assert!(!alt.matches_flags(true, false, true, false));

        // PlatformDefault matches through its resolved key.
        let default = DdlNavigationModifier::PlatformDefault;
        if cfg!(target_os = "macos") {
            assert!(default.matches_flags(true, false, false, false));
        } else {
            assert!(default.matches_flags(false, true, false, false));
        }
    }

    #[test]
    fn serde_ddl_navigation_roundtrip() {
        let prefs = DdlNavigationPreferences {
            enabled: false,
            activation_modifier: DdlNavigationModifier::Alt,
            presentation: DdlNavigationPresentation::Popup,
            popup: DdlPopupPreferences {
                width: 1024,
                height: 768,
                x_percent: 0.25,
                y_percent: 0.75,
                monitor: DdlPopupMonitorPreference::Monitor {
                    id: "display-uuid".into(),
                },
            },
            include_privileges: false,
        };
        let json = serde_json::to_string(&prefs).unwrap();
        let back: DdlNavigationPreferences = serde_json::from_str(&json).unwrap();
        assert_eq!(back, prefs);
    }

    #[test]
    fn serde_ddl_navigation_partial_json_uses_defaults() {
        let prefs: DdlNavigationPreferences =
            serde_json::from_str(r#"{"presentation":"sidePanel"}"#).unwrap();
        assert!(prefs.enabled);
        assert_eq!(prefs.presentation, DdlNavigationPresentation::SidePanel);
        assert_eq!(
            prefs.activation_modifier,
            DdlNavigationModifier::PlatformDefault
        );
        assert_eq!(prefs.popup, DdlPopupPreferences::default());

        let empty: DdlNavigationPreferences = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, DdlNavigationPreferences::default());
    }

    #[test]
    fn ddl_popup_preferences_clamp() {
        let clamped = DdlPopupPreferences {
            width: 10,
            height: 0,
            x_percent: 3.0,
            y_percent: -1.0,
            monitor: DdlPopupMonitorPreference::CurrentWindow,
        }
        .clamped();
        assert_eq!(clamped.width, DDL_POPUP_MIN_WIDTH);
        assert_eq!(clamped.height, DDL_POPUP_MIN_HEIGHT);
        assert_eq!(clamped.x_percent, 1.0);
        assert_eq!(clamped.y_percent, 0.0);

        let nan = DdlPopupPreferences {
            x_percent: f32::NAN,
            y_percent: f32::INFINITY,
            ..DdlPopupPreferences::default()
        }
        .clamped();
        assert_eq!(nan.x_percent, 0.5);
        assert_eq!(nan.y_percent, 0.5);
    }

    #[test]
    fn flatten_tree_respects_expanded() {
        let tree = build_preferences_tree();
        let mut expanded = std::collections::HashSet::new();
        // Nothing expanded -> just top-level nodes
        let flat = flatten_tree(&tree, &expanded);
        assert_eq!(flat.len(), 8); // 8 top-level nodes

        // Expand Editor
        expanded.insert("Editor".to_string());
        let flat = flatten_tree(&tree, &expanded);
        assert_eq!(flat.len(), 16); // 8 top-level + 8 editor children
        // Find the General row (first Editor child)
        let general_row = flat.iter().find(|r| r.2 == "General").unwrap();
        assert_eq!(general_row.0, 1); // depth 1
    }

    #[test]
    fn language_rebuild_preserves_stable_expansion_identity() {
        let _guard = crate::localization::test_language(crate::localization::Language::English);
        let mut expanded = std::collections::HashSet::from(["Appearance".to_string()]);
        let english = build_preferences_tree();
        assert!(
            flatten_tree(&english, &expanded)
                .iter()
                .any(|row| row.3 == Some(PreferencesPane::Language))
        );

        crate::localization::activate(crate::localization::Language::Spanish, None).unwrap();
        let spanish = build_preferences_tree();
        assert!(
            flatten_tree(&spanish, &expanded)
                .iter()
                .any(|row| row.3 == Some(PreferencesPane::Language))
        );
        assert_eq!(english[0].id, spanish[0].id);
        expanded.clear();
        crate::localization::activate(crate::localization::Language::English, None).unwrap();
    }

    #[test]
    fn preferences_store_roundtrip() {
        let store = PreferencesStore::new();
        let mut editor = store.editor();
        editor.show_line_numbers = false;
        store.set_editor(editor);
        assert!(!store.editor().show_line_numbers);

        // Restore
        let mut editor = store.editor();
        editor.show_line_numbers = true;
        store.set_editor(editor);
    }

    #[test]
    fn serde_editor_preferences_roundtrip() {
        let p = EditorPreferences::default();
        let json = serde_json::to_string(&p).unwrap();
        let p2: EditorPreferences = serde_json::from_str(&json).unwrap();
        assert_eq!(p, p2);
    }

    #[test]
    fn serde_result_formatting_roundtrip() {
        let p = ResultFormattingPreferences::default();
        let json = serde_json::to_string(&p).unwrap();
        let p2: ResultFormattingPreferences = serde_json::from_str(&json).unwrap();
        assert_eq!(p, p2);
    }

    #[test]
    fn serde_editor_partial_json_uses_defaults() {
        let json = r#"{"showLineNumbers":false}"#;
        let p: EditorPreferences = serde_json::from_str(json).unwrap();
        assert!(!p.show_line_numbers);
        assert_eq!(p.indent_style, IndentStyle::Tabs); // default
        assert_eq!(p.intellisense_source, IntelliSenseSource::RustEngine); // default
    }

    #[test]
    fn editor_preferences_insert_value_hints_default_true() {
        let p = EditorPreferences::default();
        assert!(p.show_insert_value_hints);
        let missing: EditorPreferences = serde_json::from_str("{}").unwrap();
        assert!(missing.show_insert_value_hints);
        let off: EditorPreferences =
            serde_json::from_str(r#"{"showInsertValueHints":false}"#).unwrap();
        assert!(!off.show_insert_value_hints);
    }

    #[test]
    fn editor_preferences_highlight_occurrences_default_true() {
        let p = EditorPreferences::default();
        assert!(p.highlight_occurrences);
        let missing: EditorPreferences = serde_json::from_str("{}").unwrap();
        assert!(missing.highlight_occurrences);
        let off: EditorPreferences =
            serde_json::from_str(r#"{"highlightOccurrences":false}"#).unwrap();
        assert!(!off.highlight_occurrences);
    }

    #[test]
    fn editor_preferences_detect_query_placeholders_default_true() {
        assert!(EditorPreferences::default().detect_query_placeholders);
        let missing: EditorPreferences = serde_json::from_str("{}").unwrap();
        assert!(missing.detect_query_placeholders);
        let off: EditorPreferences =
            serde_json::from_str(r#"{"detectQueryPlaceholders":false}"#).unwrap();
        assert!(!off.detect_query_placeholders);
    }

    #[test]
    fn thousands_separator_display_titles() {
        assert_eq!(ThousandsSeparator::Comma.display_title(), "1,234");
        assert_eq!(ThousandsSeparator::None.display_title(), "1234 (none)");
    }

    #[test]
    fn metric_labels_reach_the_grid_formatter() {
        let mut p = ResultFormattingPreferences::default();
        p.defaults.integer_format.reduce_large_to_metric = true;
        p.defaults.integer_format.metric_labels.mega = " Mn".into();
        assert_eq!(p.format_integer(1_500), "1.50K");
        assert_eq!(p.format_integer(2_500_000), "2.50 Mn");
        assert_eq!(p.format_integer(3_000_000_000_000_000), "3.00P");
    }

    // ---- Query resume ----

    #[test]
    fn query_resume_default_is_both() {
        assert_eq!(
            QueryResumePreferences::default().scope,
            QueryResumeScope::Both
        );
    }

    #[test]
    fn query_resume_scope_display_titles() {
        assert_eq!(
            QueryResumeScope::Both.display_title(),
            "Both saved and unsaved queries"
        );
        assert_eq!(
            QueryResumeScope::FileOnly.display_title(),
            "Saved queries only"
        );
        assert_eq!(
            QueryResumeScope::UnsavedOnly.display_title(),
            "Unsaved queries only"
        );
        assert_eq!(QueryResumeScope::None.display_title(), "None");
        assert_eq!(QueryResumeScope::Manual.display_title(), "Manually");
    }

    #[test]
    fn query_resume_scope_all_has_five_in_order() {
        let all = QueryResumeScope::all();
        assert_eq!(all.len(), 5);
        assert_eq!(all[0], QueryResumeScope::Both);
        assert_eq!(all[4], QueryResumeScope::Manual);
    }

    #[test]
    fn query_resume_allows_per_tab_override() {
        assert!(QueryResumeScope::Both.allows_per_tab_override());
        assert!(QueryResumeScope::FileOnly.allows_per_tab_override());
        assert!(QueryResumeScope::UnsavedOnly.allows_per_tab_override());
        assert!(QueryResumeScope::Manual.allows_per_tab_override());
        assert!(!QueryResumeScope::None.allows_per_tab_override());
    }

    #[test]
    fn query_resume_default_enabled_matches_swift_policy() {
        assert!(QueryResumeScope::Both.default_enabled(true));
        assert!(QueryResumeScope::Both.default_enabled(false));
        assert!(QueryResumeScope::FileOnly.default_enabled(true));
        assert!(!QueryResumeScope::FileOnly.default_enabled(false));
        assert!(!QueryResumeScope::UnsavedOnly.default_enabled(true));
        assert!(QueryResumeScope::UnsavedOnly.default_enabled(false));
        assert!(!QueryResumeScope::None.default_enabled(true));
        assert!(!QueryResumeScope::Manual.default_enabled(false));
    }

    #[test]
    fn query_resume_every_scope_has_explanation() {
        for scope in QueryResumeScope::all() {
            assert!(!scope.explanation().is_empty());
        }
    }

    #[test]
    fn serde_query_resume_roundtrip() {
        let p = QueryResumePreferences {
            scope: QueryResumeScope::UnsavedOnly,
        };
        let json = serde_json::to_string(&p).unwrap();
        let p2: QueryResumePreferences = serde_json::from_str(&json).unwrap();
        assert_eq!(p, p2);
    }

    #[test]
    fn serde_query_resume_partial_uses_default() {
        let p: QueryResumePreferences = serde_json::from_str("{}").unwrap();
        assert_eq!(p.scope, QueryResumeScope::Both);
    }

    #[test]
    fn serde_query_resume_bad_scope_falls_back() {
        // Unknown variant should not blow up the whole decode (we fall back to default).
        let p: QueryResumePreferences = serde_json::from_str(r#"{"scope":"both"}"#).unwrap();
        assert_eq!(p.scope, QueryResumeScope::Both);
    }

    #[test]
    fn query_resume_policy_effective_enabled_none_scope_always_false() {
        assert!(!QueryResumePolicy::effective_enabled(
            QueryResumeScope::None,
            true,
            None
        ));
        assert!(!QueryResumePolicy::effective_enabled(
            QueryResumeScope::None,
            false,
            Some(true)
        ));
    }

    #[test]
    fn query_resume_policy_effective_enabled_override_wins() {
        assert!(!QueryResumePolicy::effective_enabled(
            QueryResumeScope::Both,
            true,
            Some(false)
        ));
        assert!(QueryResumePolicy::effective_enabled(
            QueryResumeScope::Manual,
            false,
            Some(true)
        ));
    }

    #[test]
    fn query_resume_policy_effective_enabled_no_override_uses_default() {
        assert!(QueryResumePolicy::effective_enabled(
            QueryResumeScope::Both,
            true,
            None
        ));
        assert!(QueryResumePolicy::effective_enabled(
            QueryResumeScope::FileOnly,
            true,
            None
        ));
        assert!(!QueryResumePolicy::effective_enabled(
            QueryResumeScope::FileOnly,
            false,
            None
        ));
        assert!(!QueryResumePolicy::effective_enabled(
            QueryResumeScope::UnsavedOnly,
            true,
            None
        ));
        assert!(QueryResumePolicy::effective_enabled(
            QueryResumeScope::UnsavedOnly,
            false,
            None
        ));
        assert!(!QueryResumePolicy::effective_enabled(
            QueryResumeScope::Manual,
            true,
            None
        ));
    }

    #[test]
    fn query_resume_policy_resolved_layout_resume_on_per_tab_wins() {
        assert_eq!(
            QueryResumePolicy::resolved_layout(true, Some("stacked"), Some("tabular")),
            Some("stacked".to_string())
        );
    }

    #[test]
    fn query_resume_policy_resolved_layout_resume_on_falls_back_to_global() {
        assert_eq!(
            QueryResumePolicy::resolved_layout(true, None, Some("tabular")),
            Some("tabular".to_string())
        );
    }

    #[test]
    fn query_resume_policy_resolved_layout_resume_off_uses_global() {
        assert_eq!(
            QueryResumePolicy::resolved_layout(false, Some("stacked"), Some("tabular")),
            Some("tabular".to_string())
        );
    }

    #[test]
    fn query_resume_policy_resolved_layout_both_none() {
        assert_eq!(QueryResumePolicy::resolved_layout(true, None, None), None);
    }

    // ---- Diagnostic logging ----

    #[test]
    fn diagnostic_logging_default_is_off() {
        assert_eq!(
            DiagnosticLoggingPreferences::default().level,
            DiagnosticLogLevelPref::Off
        );
    }

    #[test]
    fn diagnostic_log_level_storage_keys_stable() {
        assert_eq!(DiagnosticLogLevelPref::Off.storage_key(), "off");
        assert_eq!(DiagnosticLogLevelPref::Error.storage_key(), "error");
        assert_eq!(DiagnosticLogLevelPref::Verbose.storage_key(), "verbose");
    }

    #[test]
    fn diagnostic_log_level_display_titles() {
        assert_eq!(DiagnosticLogLevelPref::Off.display_title(), "Off");
        assert_eq!(DiagnosticLogLevelPref::Error.display_title(), "Errors only");
        assert_eq!(
            DiagnosticLogLevelPref::Verbose.display_title(),
            "Verbose (with timing)"
        );
    }

    #[test]
    fn diagnostic_log_level_from_storage_key_tolerant() {
        assert_eq!(
            DiagnosticLogLevelPref::from_storage_key("verbose"),
            DiagnosticLogLevelPref::Verbose
        );
        assert_eq!(
            DiagnosticLogLevelPref::from_storage_key("garbage"),
            DiagnosticLogLevelPref::Off
        );
    }

    #[test]
    fn serde_diagnostic_logging_roundtrip() {
        let p = DiagnosticLoggingPreferences {
            level: DiagnosticLogLevelPref::Verbose,
        };
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains("verbose"));
        let p2: DiagnosticLoggingPreferences = serde_json::from_str(&json).unwrap();
        assert_eq!(p, p2);
    }

    #[test]
    fn terminal_notice_opt_out_defaults_off_and_stays_out_of_old_files() {
        let old: NativeToolPreferences = serde_json::from_str(r#"{"overrides":{}}"#).unwrap();
        assert!(!old.terminal_secret_notice_dismissed);
        let json = serde_json::to_string(&NativeToolPreferences::default()).unwrap();
        assert!(!json.contains("terminalSecretNoticeDismissed"));
    }

    #[test]
    fn native_tool_preferences_round_trip() {
        let p = NativeToolPreferences {
            overrides: BTreeMap::from([("pg_dump".into(), "/opt/homebrew/bin/pg_dump".into())]),
            terminal_secret_notice_dismissed: true,
        };
        let json = serde_json::to_string(&p).unwrap();
        let back: NativeToolPreferences = serde_json::from_str(&json).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    #[cfg(not(target_arch = "wasm32"))]
    fn override_map_ignores_unknown_keys() {
        let p = NativeToolPreferences {
            overrides: BTreeMap::from([
                ("pg_dump".into(), "/bin/pg_dump".into()),
                ("bogus".into(), "/bin/bogus".into()),
            ]),
            ..Default::default()
        };
        let map = p.override_map();
        assert_eq!(map.len(), 1);
        assert!(map.contains_key(&sqlly_services::NativeTool::PgDump));
    }
}
