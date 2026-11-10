//! Saved chart library: a versioned JSON store of chart configurations kept
//! with the SQL, connection, and database that produced them, plus the pure
//! rules the Saved Charts panel and the Chart Dashboard share (unique names,
//! runnable SQL, refresh scheduling, tile layout).
//!
//! Desktop-only (the library lives on the local filesystem). Pure store logic
//! is separated from the GPUI views so retention/rename/duplicate rules are
//! unit tested directly against a temp directory.

#![cfg(not(target_arch = "wasm32"))]

use std::path::PathBuf;

use crate::result_chart::{
    ResultChartSpec, chart_comment_body, parse_chart_body, update_or_insert_chart_comment,
};

/// Current on-disk format version. A file written by a newer app (a higher
/// version) is left untouched and reported as an error rather than clobbered.
pub const STORE_VERSION: u32 = 1;

/// File name of the library under the SQLLY data directory.
pub const STORE_FILE_NAME: &str = "saved-charts.json";

/// Dashboard auto-refresh choices; `None` = manual only. Longer than the
/// per-tab result auto-refresh menu because a dashboard fans out one query
/// per tile.
pub const REFRESH_CHOICES: [Option<u64>; 5] = [None, Some(30), Some(60), Some(300), Some(900)];

/// Human label for a dashboard refresh interval.
pub fn refresh_label(secs: Option<u64>) -> String {
    match secs {
        None => crate::localization::text("results.saved_charts.manual_refresh"),
        Some(secs) if secs >= 60 && secs % 60 == 0 => crate::localization::format(
            "results.saved_charts.refresh_every_minutes",
            &[("minutes", &(secs / 60).to_string())],
        ),
        Some(secs) => crate::localization::format(
            "results.saved_charts.refresh_every_seconds",
            &[("seconds", &secs.to_string())],
        ),
    }
}

/// The next choice after `current` in [`REFRESH_CHOICES`] (wrapping), for a
/// cycle button.
pub fn next_refresh_choice(current: Option<u64>) -> Option<u64> {
    let index = REFRESH_CHOICES
        .iter()
        .position(|choice| *choice == current)
        .unwrap_or(0);
    REFRESH_CHOICES[(index + 1) % REFRESH_CHOICES.len()]
}

/// One saved chart.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct SavedChart {
    pub id: String,
    pub name: String,
    pub created_unix_ms: i64,
    pub updated_unix_ms: i64,
    /// Stable id of the connection profile the chart runs on (its UUID as
    /// text). `None` for entries saved before a profile was known.
    #[serde(default)]
    pub connection_id: Option<String>,
    /// Display name of that connection at save time (shown even when the
    /// profile has since been deleted).
    #[serde(default)]
    pub connection_name: String,
    #[serde(default)]
    pub database: Option<String>,
    /// The SQL text that produced the charted result set.
    pub sql: String,
    /// The chart layout as its `-- sqlly chart:` directive line — the app's
    /// stable, name-based serialization of a chart configuration.
    pub chart_directive: String,
    /// Dashboard auto-refresh interval in seconds; `None` = manual only.
    #[serde(default)]
    pub refresh_secs: Option<u64>,
    /// When the chart's query last ran (dashboard refresh or Open).
    #[serde(default)]
    pub last_run_unix_ms: Option<i64>,
}

impl SavedChart {
    /// The chart layout parsed back out of the stored directive.
    pub fn spec(&self) -> Option<ResultChartSpec> {
        let body = chart_comment_body(&self.chart_directive)?;
        parse_chart_body(body).ok()
    }

    /// The SQL to execute so the result opens on the Chart tab with the saved
    /// layout applied: a leading `-- sqlly view: chart` directive (replacing
    /// any view directive the saved text already led with) plus the saved
    /// chart directive in place of whatever chart directive the text had.
    pub fn runnable_sql(&self) -> String {
        let body = strip_leading_view_directive(&self.sql);
        let with_chart = match self.spec() {
            Some(spec) => update_or_insert_chart_comment(&body, &spec),
            None => body,
        };
        format!("-- sqlly view: chart\n{with_chart}")
    }

    /// Connection label for lists and tile headers: "Connection · database".
    pub fn location_label(&self) -> String {
        match self.database.as_deref().filter(|db| !db.is_empty()) {
            Some(db) => format!("{} · {db}", self.connection_name),
            None => self.connection_name.clone(),
        }
    }
}

/// File name of the per-connection dashboard widget pin list under the SQLLY
/// data directory.
pub const WIDGET_PINS_FILE_NAME: &str = "dashboard-widgets.json";

/// Everything needed to create a new entry; the store assigns id and times.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedChartDraft {
    pub name: String,
    pub connection_id: Option<String>,
    pub connection_name: String,
    pub database: Option<String>,
    pub sql: String,
    pub spec: ResultChartSpec,
    pub refresh_secs: Option<u64>,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct StoreFile {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    charts: Vec<SavedChart>,
}

/// Filesystem-backed saved chart library rooted at one JSON file.
pub struct SavedChartStore {
    path: PathBuf,
}

impl SavedChartStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// The app's library under the SQLLY data directory.
    pub fn default_store() -> Self {
        Self::new(crate::query_history::data_file(STORE_FILE_NAME))
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Load the library. A missing file is an empty library; a corrupt file
    /// or one written by a newer app version is an error so a mutation
    /// cannot clobber it.
    fn load(&self) -> Result<Vec<SavedChart>, String> {
        let file: StoreFile = match crate::fs_safe::load_json_store(&self.path) {
            crate::fs_safe::StoreLoad::Missing => return Ok(Vec::new()),
            crate::fs_safe::StoreLoad::Loaded(file) => file,
            crate::fs_safe::StoreLoad::Corrupt { error } => {
                return Err(crate::localization::message(error));
            }
        };
        if file.version > STORE_VERSION {
            return Err(crate::localization::format(
                "results.saved_charts.library_newer_version_error",
                &[
                    ("file_version", &file.version.to_string()),
                    ("supported_version", &STORE_VERSION.to_string()),
                ],
            ));
        }
        Ok(file.charts)
    }

    fn write(&self, charts: Vec<SavedChart>) -> Result<(), String> {
        let file = StoreFile {
            version: STORE_VERSION,
            charts,
        };
        let text = serde_json::to_string_pretty(&file).map_err(|e| e.to_string())?;
        crate::fs_safe::write_atomic(&self.path, text.as_bytes()).map_err(|e| e.to_string())
    }

    /// Every saved chart, newest first. A corrupt library lists as empty
    /// without rewriting the file.
    pub fn list(&self) -> Vec<SavedChart> {
        let mut charts = self.load().unwrap_or_default();
        charts.sort_by_key(|chart| std::cmp::Reverse(chart.created_unix_ms));
        charts
    }

    /// The library's load error, if any (surfaced by the panel so a corrupt
    /// or too-new file is not mistaken for an empty library).
    pub fn load_error(&self) -> Option<String> {
        self.load().err()
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn get(&self, id: &str) -> Option<SavedChart> {
        self.list().into_iter().find(|chart| chart.id == id)
    }

    /// Save a new chart. A blank name is rejected; a name already in the
    /// library is made unique with a " (n)" suffix.
    pub fn save_new(&self, draft: SavedChartDraft) -> Result<SavedChart, String> {
        let name = normalize_name(&draft.name)?;
        let mut charts = self.load()?;
        let name = unique_name(charts.iter().map(|chart| chart.name.as_str()), &name);
        let now = chrono::Utc::now().timestamp_millis();
        let chart = SavedChart {
            id: uuid::Uuid::new_v4().to_string(),
            name,
            created_unix_ms: now,
            updated_unix_ms: now,
            connection_id: draft.connection_id,
            connection_name: draft.connection_name,
            database: draft.database,
            sql: draft.sql,
            chart_directive: draft.spec.directive(),
            refresh_secs: draft.refresh_secs,
            last_run_unix_ms: None,
        };
        charts.push(chart.clone());
        self.write(charts)?;
        Ok(chart)
    }

    /// Rename a chart. Blank names and names another chart already uses are
    /// rejected (renaming to its own name, with different whitespace, is fine).
    pub fn rename(&self, id: &str, new_name: &str) -> Result<(), String> {
        let name = normalize_name(new_name)?;
        let mut charts = self.load()?;
        if charts
            .iter()
            .any(|chart| chart.id != id && chart.name.eq_ignore_ascii_case(&name))
        {
            return Err(crate::localization::format(
                "results.saved_charts.duplicate_name_error",
                &[("chart_name", &name)],
            ));
        }
        let chart = charts
            .iter_mut()
            .find(|chart| chart.id == id)
            .ok_or_else(|| {
                crate::localization::text("results.saved_charts.rename_not_found_error")
            })?;
        chart.name = name;
        chart.updated_unix_ms = chrono::Utc::now().timestamp_millis();
        self.write(charts)
    }

    pub fn delete(&self, id: &str) -> Result<(), String> {
        let mut charts = self.load()?;
        let before = charts.len();
        charts.retain(|chart| chart.id != id);
        if charts.len() == before {
            return Err(crate::localization::text(
                "results.saved_charts.delete_not_found_error",
            ));
        }
        self.write(charts)
    }

    /// Copy a chart under a fresh id and a unique "<name> (n)" name.
    pub fn duplicate(&self, id: &str) -> Result<SavedChart, String> {
        let mut charts = self.load()?;
        let source = charts
            .iter()
            .find(|chart| chart.id == id)
            .cloned()
            .ok_or_else(|| {
                crate::localization::text("results.saved_charts.duplicate_not_found_error")
            })?;
        let name = unique_name(charts.iter().map(|chart| chart.name.as_str()), &source.name);
        let now = chrono::Utc::now().timestamp_millis();
        let copy = SavedChart {
            id: uuid::Uuid::new_v4().to_string(),
            name,
            created_unix_ms: now,
            updated_unix_ms: now,
            last_run_unix_ms: None,
            ..source
        };
        charts.push(copy.clone());
        self.write(charts)?;
        Ok(copy)
    }

    /// Change a chart's dashboard refresh interval.
    pub fn set_refresh(&self, id: &str, refresh_secs: Option<u64>) -> Result<(), String> {
        let mut charts = self.load()?;
        let chart = charts
            .iter_mut()
            .find(|chart| chart.id == id)
            .ok_or_else(|| {
                crate::localization::text("results.saved_charts.refresh_not_found_error")
            })?;
        chart.refresh_secs = refresh_secs;
        chart.updated_unix_ms = chrono::Utc::now().timestamp_millis();
        self.write(charts)
    }

    /// Record that the chart's query ran now. Best effort: a chart deleted
    /// while its query ran is not an error.
    pub fn touch_last_run(&self, id: &str, unix_ms: i64) -> Result<(), String> {
        let mut charts = self.load()?;
        if let Some(chart) = charts.iter_mut().find(|chart| chart.id == id) {
            chart.last_run_unix_ms = Some(unix_ms);
            self.write(charts)?;
        }
        Ok(())
    }
}

/// Per-connection pin list for server-dashboard widgets: which saved charts
/// sit on a connection's Server Dashboard, in display order. Keyed by the
/// connection profile's stable UUID as text; a chart id appears at most once
/// per connection. Same on-disk contract as the chart library: a missing file
/// is an empty list, a corrupt or too-new file is an error that refuses
/// mutation rather than being clobbered.
pub struct WidgetPinStore {
    path: PathBuf,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct WidgetPinsFile {
    #[serde(default)]
    version: u32,
    /// Connection profile id (UUID as text) → ordered pinned chart ids.
    #[serde(default)]
    pins: std::collections::BTreeMap<String, Vec<String>>,
}

impl WidgetPinStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// The app's pin list under the SQLLY data directory.
    pub fn default_store() -> Self {
        Self::new(crate::query_history::data_file(WIDGET_PINS_FILE_NAME))
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    fn load(&self) -> Result<std::collections::BTreeMap<String, Vec<String>>, String> {
        let file: WidgetPinsFile = match crate::fs_safe::load_json_store(&self.path) {
            crate::fs_safe::StoreLoad::Missing => return Ok(std::collections::BTreeMap::new()),
            crate::fs_safe::StoreLoad::Loaded(file) => file,
            crate::fs_safe::StoreLoad::Corrupt { error } => {
                return Err(crate::localization::message(error));
            }
        };
        if file.version > STORE_VERSION {
            return Err(crate::localization::format(
                "results.saved_charts.widget_pins_newer_version_error",
                &[
                    ("file_version", &file.version.to_string()),
                    ("supported_version", &STORE_VERSION.to_string()),
                ],
            ));
        }
        Ok(file.pins)
    }

    fn write(&self, pins: std::collections::BTreeMap<String, Vec<String>>) -> Result<(), String> {
        let file = WidgetPinsFile {
            version: STORE_VERSION,
            pins,
        };
        let text = serde_json::to_string_pretty(&file).map_err(|e| e.to_string())?;
        crate::fs_safe::write_atomic(&self.path, text.as_bytes()).map_err(|e| e.to_string())
    }

    /// The pinned chart ids for one connection, in display order. A corrupt
    /// file lists as empty without rewriting it (see `load_error`).
    pub fn pins_for(&self, connection_id: &str) -> Vec<String> {
        self.load()
            .unwrap_or_default()
            .get(connection_id)
            .cloned()
            .unwrap_or_default()
    }

    /// The store's load error, if any (surfaced so a corrupt or too-new file
    /// is not mistaken for "nothing pinned").
    pub fn load_error(&self) -> Option<String> {
        self.load().err()
    }

    /// Pin a chart to a connection's dashboard. Idempotent: an already-pinned
    /// chart keeps its position.
    pub fn pin(&self, connection_id: &str, chart_id: &str) -> Result<(), String> {
        let mut pins = self.load()?;
        let list = pins.entry(connection_id.to_string()).or_default();
        if !list.iter().any(|id| id == chart_id) {
            list.push(chart_id.to_string());
        }
        self.write(pins)
    }

    /// Remove a chart from a connection's dashboard. Removing a chart that is
    /// not pinned is not an error.
    pub fn unpin(&self, connection_id: &str, chart_id: &str) -> Result<(), String> {
        let mut pins = self.load()?;
        if let Some(list) = pins.get_mut(connection_id) {
            list.retain(|id| id != chart_id);
            if list.is_empty() {
                pins.remove(connection_id);
            }
        }
        self.write(pins)
    }

    /// Move a pinned chart one slot earlier or later in display order.
    /// Out-of-range moves (already first/last, not pinned) are no-ops.
    pub fn move_pin(
        &self,
        connection_id: &str,
        chart_id: &str,
        earlier: bool,
    ) -> Result<(), String> {
        let mut pins = self.load()?;
        if let Some(list) = pins.get_mut(connection_id)
            && let Some(index) = list.iter().position(|id| id == chart_id)
        {
            let swap_with = if earlier {
                index.checked_sub(1)
            } else {
                (index + 1 < list.len()).then_some(index + 1)
            };
            if let Some(other) = swap_with {
                list.swap(index, other);
            }
        }
        self.write(pins)
    }

    /// Drop pins that name charts no longer in `live_chart_ids` (a chart
    /// deleted from the library must not haunt the dashboard forever).
    /// Returns the surviving ordered list for the connection.
    pub fn prune(&self, connection_id: &str, live_chart_ids: &[String]) -> Vec<String> {
        let Ok(mut pins) = self.load() else {
            return Vec::new();
        };
        let Some(list) = pins.get_mut(connection_id) else {
            return Vec::new();
        };
        let before = list.len();
        list.retain(|id| live_chart_ids.iter().any(|live| live == id));
        let surviving = list.clone();
        if list.is_empty() {
            pins.remove(connection_id);
        }
        if surviving.len() != before {
            // Best effort: if the cleanup cannot be written, the stale pins
            // stay on disk and are filtered out again on the next load (the
            // resolved list never shows them either way).
            let _ = self.write(pins);
        }
        surviving
    }
}

#[cfg(test)]
mod widget_pin_tests {
    use super::*;

    fn temp_store() -> (tempfile::TempDir, WidgetPinStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = WidgetPinStore::new(dir.path().join("nested").join(WIDGET_PINS_FILE_NAME));
        (dir, store)
    }

    #[test]
    fn pins_round_trip_per_connection() {
        let (_dir, store) = temp_store();
        store.pin("conn-a", "chart-1").unwrap();
        store.pin("conn-a", "chart-2").unwrap();
        store.pin("conn-b", "chart-9").unwrap();
        // A fresh store over the same file sees the same pins: persistence.
        let reread = WidgetPinStore::new(store.path().to_path_buf());
        assert_eq!(reread.pins_for("conn-a"), vec!["chart-1", "chart-2"]);
        assert_eq!(reread.pins_for("conn-b"), vec!["chart-9"]);
        assert!(reread.pins_for("conn-c").is_empty());
    }

    #[test]
    fn pin_is_idempotent_and_unpin_tolerates_absence() {
        let (_dir, store) = temp_store();
        store.pin("conn-a", "chart-1").unwrap();
        store.pin("conn-a", "chart-1").unwrap();
        assert_eq!(store.pins_for("conn-a"), vec!["chart-1"]);
        store.unpin("conn-a", "not-pinned").unwrap();
        assert_eq!(store.pins_for("conn-a"), vec!["chart-1"]);
        store.unpin("conn-a", "chart-1").unwrap();
        assert!(store.pins_for("conn-a").is_empty());
        // The empty connection key is dropped rather than left as `"conn-a": []`.
        let text = std::fs::read_to_string(store.path()).unwrap();
        assert!(!text.contains("conn-a"));
    }

    #[test]
    fn move_pin_swaps_with_neighbor_and_clamps_at_edges() {
        let (_dir, store) = temp_store();
        for id in ["a", "b", "c"] {
            store.pin("conn", id).unwrap();
        }
        store.move_pin("conn", "b", true).unwrap();
        assert_eq!(store.pins_for("conn"), vec!["b", "a", "c"]);
        store.move_pin("conn", "b", true).unwrap();
        assert_eq!(store.pins_for("conn"), vec!["b", "a", "c"], "first stays");
        store.move_pin("conn", "a", false).unwrap();
        store.move_pin("conn", "a", false).unwrap();
        assert_eq!(store.pins_for("conn"), vec!["b", "c", "a"]);
        store.move_pin("conn", "a", false).unwrap();
        assert_eq!(store.pins_for("conn"), vec!["b", "c", "a"], "last stays");
        store.move_pin("conn", "missing", true).unwrap();
        assert_eq!(store.pins_for("conn"), vec!["b", "c", "a"]);
    }

    #[test]
    fn prune_drops_deleted_charts_and_keeps_order() {
        let (_dir, store) = temp_store();
        for id in ["a", "b", "c"] {
            store.pin("conn", id).unwrap();
        }
        let live = vec!["c".to_string(), "a".to_string()];
        let surviving = store.prune("conn", &live);
        assert_eq!(surviving, vec!["a", "c"]);
        assert_eq!(store.pins_for("conn"), vec!["a", "c"]);
        assert!(store.prune("other", &live).is_empty());
    }

    #[test]
    fn corrupt_file_is_refused_without_clobbering() {
        let (_dir, store) = temp_store();
        std::fs::create_dir_all(store.path().parent().unwrap()).unwrap();
        let corrupt = "{\"version\":1,\"pins\":{\"conn\":";
        std::fs::write(store.path(), corrupt).unwrap();
        assert!(store.load_error().is_some());
        assert!(store.pins_for("conn").is_empty());
        assert!(store.pin("conn", "x").is_err());
        assert_eq!(std::fs::read_to_string(store.path()).unwrap(), corrupt);
    }

    #[test]
    fn newer_format_version_is_refused_without_clobbering() {
        let (_dir, store) = temp_store();
        std::fs::create_dir_all(store.path().parent().unwrap()).unwrap();
        let text = format!("{{\"version\":{},\"pins\":{{}}}}", STORE_VERSION + 1);
        std::fs::write(store.path(), &text).unwrap();
        assert!(store.load_error().is_some());
        assert!(store.pin("conn", "x").is_err());
        assert_eq!(std::fs::read_to_string(store.path()).unwrap(), text);
    }
}

/// Charts that can be pinned to a connection's server dashboard right now:
/// saved against that connection and not already pinned. A chart saved before
/// profiles were tracked (`connection_id: None`) is never pinnable — the pin
/// list is keyed by connection, so an unknown owner could never be resolved
/// back. Pin order comes from the pin list; the candidate list keeps the
/// library's (newest-first) order.
pub fn pinnable_charts(
    charts: &[SavedChart],
    connection_id: &str,
    pinned_ids: &[String],
) -> Vec<SavedChart> {
    charts
        .iter()
        .filter(|chart| chart.connection_id.as_deref() == Some(connection_id))
        .filter(|chart| !pinned_ids.iter().any(|id| id == &chart.id))
        .cloned()
        .collect()
}

/// Resolve an ordered pin list against the library: charts whose ids are
/// pinned, in pin order, skipping pins that name deleted charts.
pub fn resolve_pinned_charts(charts: &[SavedChart], pin_ids: &[String]) -> Vec<SavedChart> {
    pin_ids
        .iter()
        .filter_map(|id| charts.iter().find(|chart| &chart.id == id).cloned())
        .collect()
}

/// Whether a widget's saved SQL may run from a dashboard: exactly the chart
/// dashboard's read-only SELECT rule, so a pinned chart can never modify data
/// from a server dashboard either.
/// `db` is the dashboard connection's engine, so quoting and comments are read
/// the way that engine reads them (a bracket-quoted name on SQL Server is one
/// identifier, not a statement break).
pub fn widget_sql_is_select_only(chart: &SavedChart, db: sqlly_core::DatabaseType) -> bool {
    sqlly_intel::query::safety::classify_select_only_for(db, &chart.sql).select_only
}

#[cfg(test)]
mod widget_pinnable_tests {
    use super::*;

    fn chart(id: &str, connection_id: Option<&str>) -> SavedChart {
        SavedChart {
            id: id.to_string(),
            name: format!("chart {id}"),
            created_unix_ms: 0,
            updated_unix_ms: 0,
            connection_id: connection_id.map(str::to_string),
            connection_name: String::new(),
            database: None,
            sql: "SELECT 1".into(),
            chart_directive: String::new(),
            refresh_secs: None,
            last_run_unix_ms: None,
        }
    }

    #[test]
    fn pinnable_excludes_other_connections_and_already_pinned() {
        let charts = vec![
            chart("a", Some("conn")),
            chart("b", Some("other")),
            chart("c", Some("conn")),
            chart("d", None),
        ];
        let pinned = vec!["c".to_string()];
        let candidates = pinnable_charts(&charts, "conn", &pinned);
        let ids: Vec<&str> = candidates.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["a"], "only un-pinned charts of this connection");
    }

    #[test]
    fn resolve_keeps_pin_order_and_skips_deleted() {
        let charts = vec![chart("a", Some("conn")), chart("b", Some("conn"))];
        let pins = vec!["b".to_string(), "gone".to_string(), "a".to_string()];
        let resolved = resolve_pinned_charts(&charts, &pins);
        let ids: Vec<&str> = resolved.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["b", "a"]);
    }

    #[test]
    fn widget_sql_enforcement_is_the_dashboard_select_rule() {
        let mut c = chart("a", Some("conn"));
        assert!(
            widget_sql_is_select_only(&c, sqlly_core::DatabaseType::Postgres),
            "plain SELECT runs"
        );
        c.sql = "WITH x AS (SELECT 1) SELECT * FROM x".into();
        assert!(
            widget_sql_is_select_only(&c, sqlly_core::DatabaseType::Postgres),
            "CTE SELECT runs"
        );
        c.sql = "DELETE FROM t WHERE id = 1".into();
        assert!(
            !widget_sql_is_select_only(&c, sqlly_core::DatabaseType::Postgres),
            "DELETE refused"
        );
        c.sql = "SELECT 1; DROP TABLE t".into();
        assert!(
            !widget_sql_is_select_only(&c, sqlly_core::DatabaseType::Postgres),
            "smuggled write refused"
        );
        c.sql = "INSERT INTO t VALUES (1)".into();
        assert!(
            !widget_sql_is_select_only(&c, sqlly_core::DatabaseType::Postgres),
            "INSERT refused"
        );
        // Read with the dashboard engine's quoting: on SQL Server a bracketed
        // name holding `;` is one identifier, not a second statement.
        c.sql = "SELECT * FROM [odd]]; DROP TABLE t;--]".into();
        assert!(
            widget_sql_is_select_only(&c, sqlly_core::DatabaseType::SqlServer),
            "bracket-quoted name is one identifier on SQL Server"
        );
        assert!(
            !widget_sql_is_select_only(&c, sqlly_core::DatabaseType::Postgres),
            "the same text is two statements where [ ] is not a quote"
        );
    }
}

/// Trim a name and reject blanks.
fn normalize_name(raw: &str) -> Result<String, String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(crate::localization::text(
            "results.saved_charts.blank_name_error",
        ));
    }
    Ok(name.chars().take(120).collect())
}

/// `desired` if no existing name matches it (case-insensitively), else the
/// first free "desired (n)" starting at 2.
pub fn unique_name<'a>(existing: impl Iterator<Item = &'a str>, desired: &str) -> String {
    let taken: Vec<String> = existing.map(|name| name.to_ascii_lowercase()).collect();
    let is_free = |candidate: &str| {
        !taken
            .iter()
            .any(|name| name == &candidate.to_ascii_lowercase())
    };
    if is_free(desired) {
        return desired.to_string();
    }
    let mut n = 2u32;
    loop {
        let candidate = format!("{desired} ({n})");
        if is_free(&candidate) {
            return candidate;
        }
        n = n.saturating_add(1);
    }
}

/// Drop a leading `-- sqlly view:` line (the first non-empty line only —
/// that is the only place the parser honours it).
fn strip_leading_view_directive(sql: &str) -> String {
    let mut lines: Vec<&str> = sql.lines().collect();
    if let Some(index) = lines.iter().position(|line| !line.trim().is_empty())
        && matches!(
            crate::sqlly_directive::classify(lines[index].trim()),
            Some(crate::sqlly_directive::Directive::View { .. })
        )
    {
        lines.remove(index);
    }
    let mut joined = lines.join("\n");
    if sql.ends_with('\n') {
        joined.push('\n');
    }
    joined
}

/// Whether a dashboard tile is due for a timed refresh. Delegates to the
/// per-tab auto-refresh rules: no interval → never; a run in flight or a
/// non-SELECT query → never; otherwise once the interval has elapsed since
/// the previous completion (a tile that never completed refreshes at once).
pub fn tile_refresh_due(
    refresh_secs: Option<u64>,
    in_flight: bool,
    select_only: bool,
    secs_since_last_completion: Option<u64>,
) -> bool {
    use crate::auto_refresh::{TickDecision, TickInput, tick_decision};
    if refresh_secs.is_some() && !in_flight && select_only && secs_since_last_completion.is_none() {
        return true;
    }
    tick_decision(TickInput {
        interval_secs: refresh_secs,
        tab_is_active: true,
        run_in_flight: in_flight,
        mutation_review_pending: false,
        tab_snapshot_only: false,
        last_sql_is_select_only: select_only,
        secs_since_last_completion,
    }) == TickDecision::Refresh
}

/// Tile columns for a dashboard body of `width` logical pixels: one column
/// on narrow layouts, two up to a comfortable desktop width, three beyond.
pub fn dashboard_columns(width: f32) -> usize {
    if width < 620.0 {
        1
    } else if width < 1100.0 {
        2
    } else {
        3
    }
}

/// Split `count` tiles into rows of `columns` (each row lists tile indices).
pub fn tile_rows(count: usize, columns: usize) -> Vec<Vec<usize>> {
    let columns = columns.max(1);
    (0..count)
        .collect::<Vec<_>>()
        .chunks(columns)
        .map(|chunk| chunk.to_vec())
        .collect()
}

/// Relative age label for "last run" stamps.
pub fn relative_age(then_unix_ms: i64, now_unix_ms: i64) -> String {
    let seconds = (now_unix_ms - then_unix_ms).max(0) / 1000;
    match seconds {
        0..=59 => crate::localization::text("results.saved_charts.age_just_now"),
        60..=3_599 => crate::localization::format(
            "results.saved_charts.age_minutes",
            &[("minutes", &(seconds / 60).to_string())],
        ),
        3_600..=86_399 => crate::localization::format(
            "results.saved_charts.age_hours",
            &[("hours", &(seconds / 3_600).to_string())],
        ),
        _ => crate::localization::format(
            "results.saved_charts.age_days",
            &[("days", &(seconds / 86_400).to_string())],
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_datatable::{ChartAggregate, ChartKind, ChartOrder};

    fn temp_store() -> (tempfile::TempDir, SavedChartStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = SavedChartStore::new(dir.path().join("nested").join(STORE_FILE_NAME));
        (dir, store)
    }

    fn spec() -> ResultChartSpec {
        ResultChartSpec {
            kind: ChartKind::Line,
            label_name: Some("Month".into()),
            value_names: vec!["Sales".into(), "Profit".into()],
            aggregate: ChartAggregate::Sum,
            order: ChartOrder::ValueDescending,
            top_n: 12,
            navigate_on_click: false,
            result_set_index: 0,
        }
    }

    fn draft(name: &str) -> SavedChartDraft {
        SavedChartDraft {
            name: name.to_string(),
            connection_id: Some("11111111-2222-3333-4444-555555555555".into()),
            connection_name: "Prod (RO)".into(),
            database: Some("sales".into()),
            sql: "SELECT Month, Sales, Profit FROM revenue".into(),
            spec: spec(),
            refresh_secs: Some(60),
        }
    }

    #[test]
    fn save_list_get_round_trips_every_field() {
        let (_dir, store) = temp_store();
        let saved = store.save_new(draft("Monthly revenue")).unwrap();
        assert_eq!(saved.name, "Monthly revenue");
        assert_eq!(saved.connection_name, "Prod (RO)");
        assert_eq!(saved.database.as_deref(), Some("sales"));
        assert_eq!(saved.refresh_secs, Some(60));
        assert!(saved.last_run_unix_ms.is_none());
        let listed = store.list();
        assert_eq!(listed, vec![saved.clone()]);
        assert_eq!(store.get(&saved.id), Some(saved.clone()));
        // The stored directive rebuilds the exact spec that was saved.
        assert_eq!(saved.spec(), Some(spec()));
        assert_eq!(saved.chart_directive, spec().directive());
        // The file carries the current format version.
        let text = std::fs::read_to_string(store.path()).unwrap();
        let file: StoreFile = serde_json::from_str(&text).unwrap();
        assert_eq!(file.version, STORE_VERSION);
    }

    #[test]
    fn newest_lists_first() {
        let (_dir, store) = temp_store();
        let first = store.save_new(draft("first")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let second = store.save_new(draft("second")).unwrap();
        let ids: Vec<String> = store.list().into_iter().map(|c| c.id).collect();
        assert_eq!(ids, vec![second.id, first.id]);
    }

    #[test]
    fn blank_names_are_rejected_and_duplicates_made_unique() {
        let (_dir, store) = temp_store();
        assert!(store.save_new(draft("   ")).is_err());
        let a = store.save_new(draft("Revenue")).unwrap();
        let b = store.save_new(draft("revenue")).unwrap();
        let c = store.save_new(draft("Revenue ")).unwrap();
        assert_eq!(a.name, "Revenue");
        assert_eq!(b.name, "revenue (2)");
        assert_eq!(c.name, "Revenue (3)");
    }

    #[test]
    fn rename_enforces_uniqueness_and_allows_own_name() {
        let (_dir, store) = temp_store();
        let a = store.save_new(draft("Alpha")).unwrap();
        let b = store.save_new(draft("Beta")).unwrap();
        assert!(store.rename(&a.id, "beta").is_err(), "clashes with Beta");
        assert!(store.rename(&a.id, "  ").is_err(), "blank rejected");
        store.rename(&a.id, "  alpha  ").unwrap();
        assert_eq!(store.get(&a.id).unwrap().name, "alpha");
        store.rename(&b.id, "Gamma").unwrap();
        assert_eq!(store.get(&b.id).unwrap().name, "Gamma");
        assert!(store.rename("missing", "x").is_err());
    }

    #[test]
    fn duplicate_copies_everything_but_identity_and_run_stamp() {
        let (_dir, store) = temp_store();
        let a = store.save_new(draft("Alpha")).unwrap();
        store.touch_last_run(&a.id, 42).unwrap();
        let copy = store.duplicate(&a.id).unwrap();
        assert_ne!(copy.id, a.id);
        assert_eq!(copy.name, "Alpha (2)");
        assert_eq!(copy.sql, a.sql);
        assert_eq!(copy.chart_directive, a.chart_directive);
        assert_eq!(copy.connection_id, a.connection_id);
        assert_eq!(copy.refresh_secs, a.refresh_secs);
        assert!(copy.last_run_unix_ms.is_none());
        assert_eq!(store.list().len(), 2);
    }

    #[test]
    fn delete_set_refresh_and_touch_last_run() {
        let (_dir, store) = temp_store();
        let a = store.save_new(draft("Alpha")).unwrap();
        store.set_refresh(&a.id, None).unwrap();
        assert_eq!(store.get(&a.id).unwrap().refresh_secs, None);
        store.touch_last_run(&a.id, 1_000).unwrap();
        assert_eq!(store.get(&a.id).unwrap().last_run_unix_ms, Some(1_000));
        // Touching a deleted chart is not an error.
        store.delete(&a.id).unwrap();
        assert!(store.list().is_empty());
        assert!(store.delete(&a.id).is_err());
        store.touch_last_run(&a.id, 2_000).unwrap();
    }

    #[test]
    fn newer_format_version_is_refused_without_clobbering() {
        let (_dir, store) = temp_store();
        std::fs::create_dir_all(store.path().parent().unwrap()).unwrap();
        let text = format!("{{\"version\":{},\"charts\":[]}}", STORE_VERSION + 1);
        std::fs::write(store.path(), &text).unwrap();
        assert!(store.load_error().is_some());
        assert!(store.list().is_empty());
        assert!(store.save_new(draft("x")).is_err());
        assert_eq!(std::fs::read_to_string(store.path()).unwrap(), text);
    }

    #[test]
    fn corrupt_file_is_refused_without_clobbering() {
        let (_dir, store) = temp_store();
        std::fs::create_dir_all(store.path().parent().unwrap()).unwrap();
        let corrupt = "{\"version\":1,\"charts\":[{\"id\":";
        std::fs::write(store.path(), corrupt).unwrap();
        assert!(store.load_error().is_some());
        assert!(store.save_new(draft("x")).is_err());
        assert_eq!(std::fs::read_to_string(store.path()).unwrap(), corrupt);
    }

    #[test]
    fn older_entries_missing_optional_fields_still_load() {
        let (_dir, store) = temp_store();
        std::fs::create_dir_all(store.path().parent().unwrap()).unwrap();
        let text = r#"{"version":1,"charts":[{"id":"a","name":"Old","created_unix_ms":1,"updated_unix_ms":1,"sql":"SELECT 1","chart_directive":"-- sqlly chart: kind:pie"}]}"#;
        std::fs::write(store.path(), text).unwrap();
        let charts = store.list();
        assert_eq!(charts.len(), 1);
        assert_eq!(charts[0].connection_id, None);
        assert_eq!(charts[0].refresh_secs, None);
        assert_eq!(charts[0].spec().unwrap().kind, ChartKind::Pie);
    }

    #[test]
    fn writes_are_staged_then_renamed() {
        let (_dir, store) = temp_store();
        store.save_new(draft("a")).unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(store.path().parent().unwrap())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files must be renamed away");
    }

    /// The library holds query text and connection names: owner-only on Unix,
    /// for both stores (they share the atomic writer).
    #[cfg(unix)]
    #[test]
    fn stores_are_written_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, store) = temp_store();
        store.save_new(draft("a")).unwrap();
        let mode = std::fs::metadata(store.path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        let pins = WidgetPinStore::new(dir.path().join(WIDGET_PINS_FILE_NAME));
        pins.pin("conn", "chart").unwrap();
        let mode = std::fs::metadata(pins.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn runnable_sql_leads_with_view_chart_and_carries_the_directive() {
        let (_dir, store) = temp_store();
        let saved = store.save_new(draft("a")).unwrap();
        let sql = saved.runnable_sql();
        let lines: Vec<&str> = sql.lines().collect();
        assert_eq!(lines[0], "-- sqlly view: chart");
        assert_eq!(lines[1], spec().directive());
        assert_eq!(lines[2], "SELECT Month, Sales, Profit FROM revenue");
        // The hint parser agrees: chart view by default with the saved spec.
        let hints = crate::result_pivot::parse_view_hints(&sql);
        assert_eq!(
            hints.default_view,
            crate::result_pivot::ResultViewKind::Chart
        );
        assert_eq!(hints.chart, Some(spec()));
    }

    #[test]
    fn runnable_sql_replaces_existing_view_and_chart_directives() {
        let chart = SavedChart {
            id: "x".into(),
            name: "x".into(),
            created_unix_ms: 0,
            updated_unix_ms: 0,
            connection_id: None,
            connection_name: String::new(),
            database: None,
            sql: "\n-- sqlly view: pivot\n-- sqlly chart: kind:bar\nSELECT 1;\n".into(),
            chart_directive: "-- sqlly chart: kind:donut".into(),
            refresh_secs: None,
            last_run_unix_ms: None,
        };
        let sql = chart.runnable_sql();
        assert_eq!(
            sql,
            "-- sqlly view: chart\n\n-- sqlly chart: kind:donut\nSELECT 1;\n"
        );
        assert!(!sql.contains("view: pivot"));
        assert!(!sql.contains("kind:bar"));
    }

    #[test]
    fn runnable_sql_without_a_parsable_directive_still_opens_the_chart_tab() {
        let chart = SavedChart {
            id: "x".into(),
            name: "x".into(),
            created_unix_ms: 0,
            updated_unix_ms: 0,
            connection_id: None,
            connection_name: String::new(),
            database: None,
            sql: "SELECT 1".into(),
            chart_directive: "garbage".into(),
            refresh_secs: None,
            last_run_unix_ms: None,
        };
        assert!(chart.spec().is_none());
        assert_eq!(chart.runnable_sql(), "-- sqlly view: chart\nSELECT 1");
    }

    #[test]
    fn location_label_joins_connection_and_database() {
        let mut chart = SavedChart {
            id: "x".into(),
            name: "x".into(),
            created_unix_ms: 0,
            updated_unix_ms: 0,
            connection_id: None,
            connection_name: "Prod".into(),
            database: Some("sales".into()),
            sql: String::new(),
            chart_directive: String::new(),
            refresh_secs: None,
            last_run_unix_ms: None,
        };
        assert_eq!(chart.location_label(), "Prod · sales");
        chart.database = Some(String::new());
        assert_eq!(chart.location_label(), "Prod");
    }

    #[test]
    fn unique_name_is_case_insensitive_and_counts_up() {
        let taken = ["Sales", "sales (2)", "Other"];
        assert_eq!(unique_name(taken.iter().copied(), "Profit"), "Profit");
        assert_eq!(unique_name(taken.iter().copied(), "SALES"), "SALES (3)");
        assert_eq!(unique_name(std::iter::empty(), "x"), "x");
    }

    #[test]
    fn refresh_choices_cycle_and_label() {
        assert_eq!(refresh_label(None), "Manual refresh");
        assert_eq!(refresh_label(Some(30)), "Every 30s");
        assert_eq!(refresh_label(Some(60)), "Every 1 min");
        assert_eq!(refresh_label(Some(900)), "Every 15 min");
        assert_eq!(next_refresh_choice(None), Some(30));
        assert_eq!(next_refresh_choice(Some(900)), None);
        assert_eq!(next_refresh_choice(Some(7)), Some(30), "unknown restarts");
    }

    #[test]
    fn tile_refresh_rules() {
        assert!(!tile_refresh_due(None, false, true, Some(1_000)), "manual");
        assert!(
            tile_refresh_due(Some(30), false, true, None),
            "never completed → run now"
        );
        assert!(!tile_refresh_due(Some(30), true, true, None), "in flight");
        assert!(
            !tile_refresh_due(Some(30), false, false, None),
            "not SELECT"
        );
        assert!(!tile_refresh_due(Some(30), false, true, Some(29)));
        assert!(tile_refresh_due(Some(30), false, true, Some(30)));
    }

    #[test]
    fn dashboard_layout_math() {
        assert_eq!(dashboard_columns(400.0), 1);
        assert_eq!(dashboard_columns(619.9), 1);
        assert_eq!(dashboard_columns(620.0), 2);
        assert_eq!(dashboard_columns(1099.0), 2);
        assert_eq!(dashboard_columns(1100.0), 3);
        assert_eq!(dashboard_columns(2560.0), 3);
        assert_eq!(tile_rows(0, 3), Vec::<Vec<usize>>::new());
        assert_eq!(tile_rows(5, 2), vec![vec![0, 1], vec![2, 3], vec![4]]);
        assert_eq!(tile_rows(3, 0), vec![vec![0], vec![1], vec![2]]);
    }

    #[test]
    fn relative_age_buckets() {
        assert_eq!(relative_age(0, 30_000), "just now");
        assert_eq!(relative_age(0, 5 * 60_000), "5 min ago");
        assert_eq!(relative_age(0, 3 * 3_600_000), "3 h ago");
        assert_eq!(relative_age(0, 2 * 86_400_000), "2 d ago");
        assert_eq!(relative_age(10_000, 0), "just now");
    }
}
