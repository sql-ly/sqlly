//! Query execution history: shared types, structured filters, pinned-snippet
//! storage, and the legacy text-log parser. Native builds persist runs in a
//! local SQLite database; the browser build keeps the append-only text log.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[allow(unused_imports)]
pub use crate::query_history_store::{QueryHistory, apply_retention, read_entries, read_page};

/// File name of the legacy append-only execution log (imported once on native).
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub(crate) const HISTORY_FILE_NAME: &str = "query-history.txt";
/// Pinned snippets live next to the history log as a small JSON file.
const PINS_FILE_NAME: &str = "query-history-pins.json";
/// The timestamp format the logger writes and the reader validates.
pub(crate) const TIMESTAMP_FORMAT: &str = "%Y-%m-%dT%H:%M:%SZ";

fn is_zero(value: &i64) -> bool {
    *value == 0
}

fn is_one(value: &u32) -> bool {
    *value == 1
}

fn one() -> u32 {
    1
}

/// One completed run handed to the history writer.
pub struct HistoryRecord<'a> {
    pub sql: &'a str,
    pub server: &'a str,
    pub database: Option<&'a str>,
    pub duration_ms: Option<u64>,
    pub outcome: HistoryOutcome,
    pub provenance: HistoryProvenance,
    pub affected_rows: Option<u64>,
    pub rollback_sql: Option<&'a str>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HistoryCursor {
    pub executed_at: String,
    pub id: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HistoryPage {
    pub entries: Vec<HistoryEntry>,
    pub next: Option<HistoryCursor>,
}

// -- reading the log ---------------------------------------------------------

/// One logged execution, as read back from the history log.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// Store row id; 0 for pins and legacy entries that never had one.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub id: i64,
    pub sql: String,
    pub server: String,
    pub database: Option<String>,
    /// As stored in the log: UTC in `%Y-%m-%dT%H:%M:%SZ` format.
    pub timestamp: String,
    #[serde(default)]
    pub duration_ms: Option<u64>,
    #[serde(default)]
    pub outcome: HistoryOutcome,
    /// How many consecutive identical runs this row stands for (>= 1).
    #[serde(default = "one", skip_serializing_if = "is_one")]
    pub run_count: u32,
    #[serde(default)]
    pub provenance: HistoryProvenance,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub affected_rows: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollback_sql: Option<String>,
    /// User-assigned labels; meaningful on pinned snippets only (log entries
    /// always carry none). Older pins files without the field load as
    /// untagged.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Optional display name; meaningful on pinned snippets only (a named pin
    /// is a "saved query"). Empty means unnamed. Older pins files without the
    /// field load as unnamed, and export/import/merge carry it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// Optional free-text description; pinned snippets only, same
    /// compatibility story as `name`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HistoryProvenance {
    #[default]
    Manual,
    Ai,
    Tool,
}

impl HistoryProvenance {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Ai => "ai",
            Self::Tool => "tool",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "ai" => Self::Ai,
            "tool" => Self::Tool,
            _ => Self::Manual,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HistoryOutcome {
    Succeeded,
    Failed,
    Cancelled,
    #[default]
    Unknown,
}

impl HistoryOutcome {
    pub(crate) fn as_log_value(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Unknown => "unknown",
        }
    }

    pub(crate) fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "succeeded" | "success" | "ok" => Self::Succeeded,
            "failed" | "failure" | "error" => Self::Failed,
            "cancelled" | "canceled" => Self::Cancelled,
            _ => Self::Unknown,
        }
    }
}

/// Resolve `name` inside the SQLLY data directory (same root the store
/// writes to).
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn data_file(name: &str) -> PathBuf {
    if let Some(app_support) = dirs::data_dir() {
        let dir = app_support.join("SQLLY");
        let _ = std::fs::create_dir_all(&dir);
        return dir.join(name);
    }
    PathBuf::from(name)
}

/// Parse an entry header line (`[<timestamp>] <server>/<database>`) into an
/// entry with an empty SQL body. The timestamp must round-trip through the
/// logger's own format, which is what lets SQL lines that merely start with
/// `[` fall through as body text instead of being mistaken for headers.
fn parse_header(line: &str) -> Option<HistoryEntry> {
    let rest = line.strip_prefix('[')?;
    let (timestamp, tail) = rest.split_once("] ")?;
    chrono::NaiveDateTime::parse_from_str(timestamp, TIMESTAMP_FORMAT).ok()?;
    let mut fields = tail.split('\t');
    let location = fields.next()?;
    let (server, database) = location.rsplit_once('/')?;
    let outcome = fields.next().map(HistoryOutcome::parse).unwrap_or_default();
    let duration_ms = fields.next().and_then(|value| value.parse::<u64>().ok());
    let rollback_sql = fields.next().and_then(decode_rollback_header_field);
    Some(HistoryEntry {
        id: 0,
        sql: String::new(),
        server: server.to_string(),
        database: (!database.is_empty()).then(|| database.to_string()),
        timestamp: timestamp.to_string(),
        duration_ms,
        outcome,
        tags: Vec::new(),
        name: String::new(),
        description: String::new(),
        run_count: 1,
        provenance: HistoryProvenance::Manual,
        affected_rows: None,
        rollback_sql,
    })
}

/// Parse raw log text in file order (oldest first). An entry's SQL runs from
/// its header to the next header; lines outside any entry and headers with an
/// empty body (corrupt or truncated writes) are skipped.
pub(crate) fn parse_history(text: &str) -> Vec<HistoryEntry> {
    fn flush(entries: &mut Vec<HistoryEntry>, current: Option<(HistoryEntry, Vec<&str>)>) {
        if let Some((mut entry, lines)) = current {
            let sql = lines.join("\n").trim().to_string();
            if !sql.is_empty() {
                entry.sql = sql;
                entries.push(entry);
            }
        }
    }

    let mut entries = Vec::new();
    let mut current: Option<(HistoryEntry, Vec<&str>)> = None;
    for line in text.lines() {
        if let Some(header) = parse_header(line) {
            flush(&mut entries, current.take());
            current = Some((header, Vec::new()));
        } else if let Some((_, sql_lines)) = current.as_mut() {
            sql_lines.push(line);
        }
    }
    flush(&mut entries, current);
    entries
}

#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub(crate) fn newest_first(mut entries: Vec<HistoryEntry>, limit: usize) -> Vec<HistoryEntry> {
    entries.reverse();
    entries.truncate(limit);
    entries
}

#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub(crate) fn format_completed_entry(
    sql: &str,
    server: &str,
    database: Option<&str>,
    duration_ms: Option<u64>,
    outcome: HistoryOutcome,
    rollback_sql: Option<&str>,
) -> String {
    use crate::intellisense_feedback::redact_sql;
    let timestamp = chrono::Utc::now().format(TIMESTAMP_FORMAT);
    let db = database.unwrap_or("");
    let server = server.trim();
    let clean_sql = redact_sql(sql.trim());
    let duration = duration_ms.map_or_else(String::new, |value| value.to_string());
    let rollback_field = rollback_sql
        .map(redact_sql)
        .and_then(|text| {
            if text.is_empty() {
                None
            } else {
                Some(base64_encode(&text))
            }
        })
        .unwrap_or_default();
    format!(
        "[{}] {}/{}\t{}\t{}\t{}\n{}\n\n",
        timestamp,
        server,
        db,
        outcome.as_log_value(),
        duration,
        rollback_field,
        clean_sql
    )
}

fn base64_encode(text: &str) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(text.as_bytes())
}

fn decode_rollback_header_field(encoded: &str) -> Option<String> {
    if encoded.is_empty() {
        return None;
    }
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .ok()?;
    String::from_utf8(bytes).ok()
}

impl QueryHistory {
    pub fn log_completed_with_rollback(
        sql: &str,
        server: &str,
        database: Option<&str>,
        duration_ms: Option<u64>,
        outcome: HistoryOutcome,
        rollback_sql: Option<&str>,
    ) {
        Self::new().log(&HistoryRecord {
            sql,
            server,
            database,
            duration_ms,
            outcome,
            provenance: HistoryProvenance::Manual,
            affected_rows: None,
            rollback_sql,
        });
    }
}

#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub(crate) fn format_existing_entry(entry: &HistoryEntry) -> String {
    let database = entry.database.as_deref().unwrap_or("");
    let duration = entry
        .duration_ms
        .map_or_else(String::new, |value| value.to_string());
    format!(
        "[{}] {}/{}\t{}\t{}\n{}\n\n",
        entry.timestamp,
        entry.server,
        database,
        entry.outcome.as_log_value(),
        duration,
        entry.sql.trim()
    )
}

pub(crate) fn retained_history_entries(
    mut entries: Vec<HistoryEntry>,
    now: chrono::DateTime<chrono::Utc>,
    retention_days: u32,
    max_entries: usize,
) -> Vec<HistoryEntry> {
    if retention_days > 0 {
        let cutoff = now - chrono::Duration::days(i64::from(retention_days));
        entries.retain(|entry| {
            chrono::NaiveDateTime::parse_from_str(&entry.timestamp, TIMESTAMP_FORMAT)
                .map(|timestamp| timestamp.and_utc() >= cutoff)
                .unwrap_or(true)
        });
    }
    if entries.len() > max_entries {
        entries.drain(..entries.len() - max_entries);
    }
    entries
}

pub(crate) fn normalized_sql_hash(sql: &str) -> String {
    let mut normalized = String::new();
    let mut last_space = false;
    for ch in sql.trim().to_ascii_lowercase().chars() {
        if ch.is_whitespace() {
            if !last_space {
                normalized.push(' ');
                last_space = true;
            }
        } else {
            normalized.push(ch);
            last_space = false;
        }
    }
    let normalized = normalized.trim();
    let normalized = normalized.strip_suffix(';').unwrap_or(normalized).trim();
    let digest = Sha256::digest(normalized.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct HistoryFilter {
    pub(crate) text: Vec<String>,
    pub(crate) server: Option<String>,
    pub(crate) database: Option<String>,
    pub(crate) outcome: Option<HistoryOutcome>,
    pub(crate) after: Option<chrono::NaiveDate>,
    pub(crate) before: Option<chrono::NaiveDate>,
    pub(crate) min_duration_ms: Option<u64>,
    pub(crate) max_duration_ms: Option<u64>,
    pub(crate) tag: Option<String>,
}

impl HistoryFilter {
    pub(crate) fn parse(query: &str) -> Self {
        let mut filter = Self::default();
        for token in query.split_whitespace() {
            let Some((key, value)) = token.split_once(':') else {
                filter.text.push(token.to_ascii_lowercase());
                continue;
            };
            let key = key.to_ascii_lowercase();
            let value = value.trim();
            let consumed = match key.as_str() {
                "server" | "connection" => {
                    filter.server = Some(value.to_ascii_lowercase());
                    true
                }
                "db" | "database" => {
                    filter.database = Some(value.to_ascii_lowercase());
                    true
                }
                "outcome" | "status" => {
                    let outcome = HistoryOutcome::parse(value);
                    if outcome != HistoryOutcome::Unknown {
                        filter.outcome = Some(outcome);
                    }
                    true
                }
                "after" => {
                    filter.after = parse_filter_date(value);
                    true
                }
                "before" => {
                    filter.before = parse_filter_date(value);
                    true
                }
                "min" | "min-duration" => {
                    filter.min_duration_ms = parse_duration_ms(value);
                    true
                }
                "max" | "max-duration" => {
                    filter.max_duration_ms = parse_duration_ms(value);
                    true
                }
                "tag" => {
                    filter.tag = Some(value.to_ascii_lowercase());
                    true
                }
                _ => false,
            };
            if !consumed {
                filter.text.push(token.to_ascii_lowercase());
            }
        }
        filter
    }

    pub(crate) fn matches(&self, entry: &HistoryEntry) -> bool {
        let haystack = format!(
            "{} {} {} {} {} {}",
            entry.sql,
            entry.server,
            entry.database.as_deref().unwrap_or(""),
            entry.tags.join(" "),
            entry.name,
            entry.description
        )
        .to_ascii_lowercase();
        if !self.text.iter().all(|term| haystack.contains(term)) {
            return false;
        }
        if self.tag.as_ref().is_some_and(|wanted| {
            !entry
                .tags
                .iter()
                .any(|tag| tag.to_ascii_lowercase().contains(wanted))
        }) {
            return false;
        }
        if self
            .server
            .as_ref()
            .is_some_and(|value| !entry.server.to_ascii_lowercase().contains(value))
        {
            return false;
        }
        if self.database.as_ref().is_some_and(|value| {
            !entry
                .database
                .as_deref()
                .unwrap_or("")
                .to_ascii_lowercase()
                .contains(value)
        }) {
            return false;
        }
        if self.outcome.is_some_and(|outcome| entry.outcome != outcome) {
            return false;
        }
        let entry_date = chrono::NaiveDateTime::parse_from_str(&entry.timestamp, TIMESTAMP_FORMAT)
            .ok()
            .map(|timestamp| timestamp.date());
        if self
            .after
            .is_some_and(|after| entry_date.is_none_or(|date| date < after))
        {
            return false;
        }
        if self
            .before
            .is_some_and(|before| entry_date.is_none_or(|date| date > before))
        {
            return false;
        }
        if self
            .min_duration_ms
            .is_some_and(|minimum| entry.duration_ms.is_none_or(|value| value < minimum))
        {
            return false;
        }
        if self
            .max_duration_ms
            .is_some_and(|maximum| entry.duration_ms.is_none_or(|value| value > maximum))
        {
            return false;
        }
        true
    }
}

fn parse_filter_date(value: &str) -> Option<chrono::NaiveDate> {
    chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d").ok()
}

fn parse_duration_ms(value: &str) -> Option<u64> {
    let value = value.trim().to_ascii_lowercase();
    if let Some(ms) = value.strip_suffix("ms") {
        ms.parse().ok()
    } else if let Some(seconds) = value.strip_suffix('s') {
        seconds
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite() && *value >= 0.0)
            .map(|value| (value * 1000.0).round() as u64)
    } else {
        value.parse().ok()
    }
}

/// Case-insensitive text and structured filtering. Structured tokens use
/// `server:`, `db:`, `outcome:`, `after:`, `before:`, `min:`, and `max:`.
#[allow(dead_code)] // used by the history browser once its module is wired
pub fn filter_entries<'a>(entries: &'a [HistoryEntry], needle: &str) -> Vec<&'a HistoryEntry> {
    let filter = HistoryFilter::parse(needle);
    entries
        .iter()
        .filter(|entry| filter.matches(entry))
        .collect()
}

// -- pinned snippets ---------------------------------------------------------

/// Default location of the pins file, next to the history log.
#[cfg(not(target_arch = "wasm32"))]
pub fn pins_path() -> PathBuf {
    data_file(PINS_FILE_NAME)
}

/// Pinned entries from the default store; empty when absent or unparseable.
#[allow(dead_code)] // used by the history browser once its module is wired
pub fn load_pins() -> Vec<HistoryEntry> {
    #[cfg(target_arch = "wasm32")]
    {
        use crate::host_storage::{StorageArea, load_text};
        load_text(StorageArea::Data, PINS_FILE_NAME)
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        load_pins_from(&pins_path())
    }
}

/// Path-parameterized variant of [`load_pins`] (test seam).
#[cfg(not(target_arch = "wasm32"))]
pub fn load_pins_from(path: &Path) -> Vec<HistoryEntry> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Persist `pins` to the default store.
#[allow(dead_code)] // used by the history browser once its module is wired
pub fn save_pins(pins: &[HistoryEntry]) -> Result<(), String> {
    #[cfg(target_arch = "wasm32")]
    {
        use crate::host_storage::{StorageArea, store_text};
        let json = serde_json::to_string_pretty(pins).map_err(|e| e.to_string())?;
        store_text(StorageArea::Data, PINS_FILE_NAME, &json)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        save_pins_to(&pins_path(), pins)
    }
}

/// Path-parameterized variant of [`save_pins`] (test seam).
#[cfg(not(target_arch = "wasm32"))]
pub fn save_pins_to(path: &Path, pins: &[HistoryEntry]) -> Result<(), String> {
    let json = serde_json::to_string_pretty(pins).map_err(|e| e.to_string())?;
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(path, json).map_err(|e| e.to_string())
}

/// Pins are keyed by SQL text + server: the same statement on a different
/// server is a different pin; database and timestamp don't participate.
pub fn is_pinned(pins: &[HistoryEntry], entry: &HistoryEntry) -> bool {
    pins.iter()
        .any(|pin| pin.sql == entry.sql && pin.server == entry.server)
}

/// Toggle `entry` in `pins` (newest pin first); returns whether the entry is
/// pinned afterwards. Callers persist via [`save_pins`]/[`save_pins_to`].
#[allow(dead_code)] // used by the history browser once its module is wired
pub fn toggle_pin(pins: &mut Vec<HistoryEntry>, entry: &HistoryEntry) -> bool {
    if is_pinned(pins, entry) {
        pins.retain(|pin| !(pin.sql == entry.sql && pin.server == entry.server));
        false
    } else {
        pins.insert(0, entry.clone());
        true
    }
}

/// Replace the tags on the pin matching `entry` (same identity as
/// [`is_pinned`]). Tags are trimmed, deduplicated case-insensitively, and
/// empties dropped. Returns whether a pin was updated.
pub fn set_pin_tags(pins: &mut [HistoryEntry], entry: &HistoryEntry, tags: &str) -> bool {
    let mut seen = std::collections::HashSet::new();
    let cleaned: Vec<String> = tags
        .split(',')
        .map(str::trim)
        .filter(|tag| !tag.is_empty())
        .filter(|tag| seen.insert(tag.to_ascii_lowercase()))
        .map(str::to_string)
        .collect();
    match find_pin_mut(pins, entry) {
        Some(pin) => {
            pin.tags = cleaned;
            true
        }
        None => false,
    }
}

/// The pin matching `entry` (same SQL + server identity as [`is_pinned`]).
fn find_pin_mut<'a>(
    pins: &'a mut [HistoryEntry],
    entry: &HistoryEntry,
) -> Option<&'a mut HistoryEntry> {
    pins.iter_mut()
        .find(|pin| pin.sql == entry.sql && pin.server == entry.server)
}

/// Set the display name on the pin matching `entry` (same identity as
/// [`is_pinned`]). The name is trimmed; blank clears it back to unnamed.
/// Returns whether a pin was updated.
pub fn set_pin_name(pins: &mut [HistoryEntry], entry: &HistoryEntry, name: &str) -> bool {
    match find_pin_mut(pins, entry) {
        Some(pin) => {
            pin.name = name.trim().to_string();
            true
        }
        None => false,
    }
}

/// Set the description on the pin matching `entry` (same identity as
/// [`is_pinned`]). Trimmed; blank clears it. Returns whether a pin was
/// updated.
pub fn set_pin_description(
    pins: &mut [HistoryEntry],
    entry: &HistoryEntry,
    description: &str,
) -> bool {
    match find_pin_mut(pins, entry) {
        Some(pin) => {
            pin.description = description.trim().to_string();
            true
        }
        None => false,
    }
}

/// Serialize pins for export: the same JSON shape the pins store uses, so an
/// exported file can also be dropped in as a pins file.
pub fn export_pins_json(pins: &[HistoryEntry]) -> String {
    serde_json::to_string_pretty(pins).unwrap_or_else(|_| "[]".to_string())
}

/// Parse an exported pins file. Rejects unparseable JSON with a readable
/// error instead of silently importing nothing.
pub fn import_pins_json(text: &str) -> Result<Vec<HistoryEntry>, String> {
    serde_json::from_str::<Vec<HistoryEntry>>(text).map_err(|error| {
        crate::localization::format(
            "query.history.invalid_pins_file",
            &[("error", &error.to_string())],
        )
    })
}

/// Merge imported pins into `pins`, skipping entries already pinned (same
/// SQL + server identity as [`is_pinned`]). Returns how many were added.
pub fn merge_pins(pins: &mut Vec<HistoryEntry>, imported: Vec<HistoryEntry>) -> usize {
    let mut added = 0;
    for entry in imported {
        if entry.sql.trim().is_empty() || is_pinned(pins, &entry) {
            continue;
        }
        pins.push(entry);
        added += 1;
    }
    added
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_entry_includes_timestamp_server_db_and_sql() {
        let entry = format_completed_entry(
            "SELECT 1",
            "localhost",
            Some("master"),
            None,
            HistoryOutcome::Unknown,
            None,
        );
        assert!(entry.contains("SELECT 1"));
        assert!(entry.contains("localhost/master"));
        assert!(entry.starts_with('['));
    }

    #[test]
    fn format_entry_handles_no_database() {
        let entry =
            format_completed_entry("SELECT 1", "srv", None, None, HistoryOutcome::Unknown, None);
        assert!(entry.contains("srv/"));
        assert!(entry.contains("SELECT 1"));
    }

    #[test]
    fn log_writes_to_file() {
        let _serial = crate::query_history_store::store_path_test_guard();
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("history.db");
        crate::query_history_store::set_store_path_override(Some(db_path.clone()));
        let history = QueryHistory::with_store_path(db_path.clone());
        history.log(&HistoryRecord {
            sql: "SELECT 42",
            server: "testserver",
            database: Some("testdb"),
            duration_ms: None,
            outcome: HistoryOutcome::Unknown,
            provenance: HistoryProvenance::Manual,
            affected_rows: None,
            rollback_sql: None,
        });
        let entries = crate::query_history_store::read_entries_from_path(&db_path, 10);
        assert_eq!(entries[0].sql, "SELECT 42");
        assert_eq!(entries[0].server, "testserver");
        assert_eq!(entries[0].database.as_deref(), Some("testdb"));
        crate::query_history_store::set_store_path_override(None);
    }

    #[test]
    fn redact_sql_masks_password_literals_and_preserves_structure() {
        let _serial = crate::query_history_store::store_path_test_guard();
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("history.db");
        crate::query_history_store::set_store_path_override(Some(db_path.clone()));
        let history = QueryHistory::with_store_path(db_path.clone());
        history.log(&HistoryRecord {
            sql: "CREATE LOGIN x WITH PASSWORD = 'secret'",
            server: "prod",
            database: Some("sales"),
            duration_ms: None,
            outcome: HistoryOutcome::Unknown,
            provenance: HistoryProvenance::Manual,
            affected_rows: None,
            rollback_sql: None,
        });
        let entries = crate::query_history_store::read_entries_from_path(&db_path, 10);
        assert!(!entries[0].sql.contains("secret"));
        assert!(entries[0].sql.contains("'<redacted>'"));
        crate::query_history_store::set_store_path_override(None);
    }

    #[test]
    fn completed_log_round_trips_duration_and_outcome() {
        let _serial = crate::query_history_store::store_path_test_guard();
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("history.db");
        crate::query_history_store::set_store_path_override(Some(db_path.clone()));
        let history = QueryHistory::with_store_path(db_path.clone());
        history.log(&HistoryRecord {
            sql: "SELECT 42",
            server: "prod",
            database: Some("sales"),
            duration_ms: Some(1_250),
            outcome: HistoryOutcome::Succeeded,
            provenance: HistoryProvenance::Manual,
            affected_rows: None,
            rollback_sql: None,
        });
        let entries = crate::query_history_store::read_entries_from_path(&db_path, 10);
        assert_eq!(entries[0].duration_ms, Some(1_250));
        assert_eq!(entries[0].outcome, HistoryOutcome::Succeeded);
        crate::query_history_store::set_store_path_override(None);
    }

    // -- read API ------------------------------------------------------------

    fn entry(sql: &str, server: &str, database: Option<&str>) -> HistoryEntry {
        HistoryEntry {
            id: 0,
            sql: sql.to_string(),
            server: server.to_string(),
            database: database.map(str::to_string),
            timestamp: "2026-08-24T10:00:00Z".to_string(),
            duration_ms: None,
            outcome: HistoryOutcome::Unknown,
            tags: Vec::new(),
            name: String::new(),
            description: String::new(),
            run_count: 1,
            provenance: HistoryProvenance::Manual,
            affected_rows: None,
            rollback_sql: None,
        }
    }

    #[test]
    fn pin_tags_set_filter_and_survive_serialization() {
        let mut pins = vec![entry("SELECT 1", "srv", None)];
        assert!(set_pin_tags(
            &mut pins,
            &entry("SELECT 1", "srv", None),
            " billing , Reports, billing ,"
        ));
        assert_eq!(pins[0].tags, vec!["billing", "Reports"]);
        // tag: filter matches case-insensitively, and untagged entries drop.
        let entries = vec![pins[0].clone(), entry("SELECT 2", "srv", None)];
        let matched = filter_entries(&entries, "tag:billing");
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].sql, "SELECT 1");
        // Free-text search also sees tags.
        assert_eq!(filter_entries(&entries, "reports").len(), 1);
        // Round-trip through the export JSON, and legacy JSON without the
        // field still parses as untagged.
        let restored = import_pins_json(&export_pins_json(&pins)).unwrap();
        assert_eq!(restored[0].tags, vec!["billing", "Reports"]);
        let legacy = r#"[{"sql":"SELECT 9","server":"s","database":null,"timestamp":"2026-08-24T10:00:00Z"}]"#;
        assert_eq!(
            import_pins_json(legacy).unwrap()[0].tags,
            Vec::<String>::new()
        );
        assert!(import_pins_json("not json").is_err());
    }

    #[test]
    fn pin_names_and_descriptions_set_search_and_round_trip() {
        let mut pins = vec![entry("SELECT 1", "srv", None)];
        let identity = entry("SELECT 1", "srv", None);
        assert!(set_pin_name(&mut pins, &identity, "  Daily revenue  "));
        assert!(set_pin_description(
            &mut pins,
            &identity,
            " Totals per region, run each morning "
        ));
        assert_eq!(pins[0].name, "Daily revenue");
        assert_eq!(pins[0].description, "Totals per region, run each morning");

        // Free-text search matches the name and the description.
        let entries = vec![pins[0].clone(), entry("SELECT 2", "srv", None)];
        let by_name = filter_entries(&entries, "REVENUE");
        assert_eq!(by_name.len(), 1);
        assert_eq!(by_name[0].sql, "SELECT 1");
        let by_description = filter_entries(&entries, "region");
        assert_eq!(by_description.len(), 1);
        assert_eq!(by_description[0].sql, "SELECT 1");
        assert!(filter_entries(&entries, "unrelated-needle").is_empty());

        // Round-trip through export/import JSON keeps both fields, and merge
        // carries them into the destination.
        let restored = import_pins_json(&export_pins_json(&pins)).unwrap();
        assert_eq!(restored[0].name, "Daily revenue");
        assert_eq!(
            restored[0].description,
            "Totals per region, run each morning"
        );
        let mut destination: Vec<HistoryEntry> = Vec::new();
        assert_eq!(merge_pins(&mut destination, restored), 1);
        assert_eq!(destination[0].name, "Daily revenue");

        // Legacy pins JSON without the fields loads as unnamed.
        let legacy = r#"[{"sql":"SELECT 9","server":"s","database":null,"timestamp":"2026-08-24T10:00:00Z"}]"#;
        let legacy = import_pins_json(legacy).unwrap();
        assert_eq!(legacy[0].name, "");
        assert_eq!(legacy[0].description, "");

        // Blank input clears; a non-pinned identity updates nothing.
        assert!(set_pin_name(&mut pins, &identity, "   "));
        assert_eq!(pins[0].name, "");
        assert!(!set_pin_name(
            &mut pins,
            &entry("SELECT 999", "srv", None),
            "x"
        ));
        assert!(!set_pin_description(
            &mut pins,
            &entry("SELECT 999", "srv", None),
            "x"
        ));
    }

    #[test]
    fn merge_pins_deduplicates_by_identity() {
        let mut pins = vec![entry("SELECT 1", "srv", None)];
        let added = merge_pins(
            &mut pins,
            vec![
                entry("SELECT 1", "srv", None),
                entry("SELECT 2", "srv", None),
                entry("   ", "srv", None),
            ],
        );
        assert_eq!(added, 1);
        assert_eq!(pins.len(), 2);
    }

    #[test]
    fn read_entries_round_trips_the_logger_newest_first() {
        let _serial = crate::query_history_store::store_path_test_guard();
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("history.db");
        crate::query_history_store::set_store_path_override(Some(db_path.clone()));
        let history = QueryHistory::with_store_path(db_path.clone());
        history.log(&HistoryRecord {
            sql: "SELECT 1",
            server: "alpha",
            database: Some("db1"),
            duration_ms: None,
            outcome: HistoryOutcome::Unknown,
            provenance: HistoryProvenance::Manual,
            affected_rows: None,
            rollback_sql: None,
        });
        history.log(&HistoryRecord {
            sql: "SELECT 2\nFROM t",
            server: "beta",
            database: None,
            duration_ms: None,
            outcome: HistoryOutcome::Unknown,
            provenance: HistoryProvenance::Manual,
            affected_rows: None,
            rollback_sql: None,
        });
        history.log(&HistoryRecord {
            sql: "SELECT 3",
            server: "gamma",
            database: Some("db3"),
            duration_ms: None,
            outcome: HistoryOutcome::Unknown,
            provenance: HistoryProvenance::Manual,
            affected_rows: None,
            rollback_sql: None,
        });

        let entries = crate::query_history_store::read_entries_from_path(&db_path, 10);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].sql, "SELECT 3");
        assert_eq!(entries[0].server, "gamma");
        assert_eq!(entries[0].database.as_deref(), Some("db3"));
        assert_eq!(entries[1].sql, "SELECT 2\nFROM t");
        assert_eq!(entries[1].database, None);
        assert_eq!(entries[2].server, "alpha");
        assert!(
            chrono::NaiveDateTime::parse_from_str(&entries[0].timestamp, TIMESTAMP_FORMAT).is_ok()
        );
        crate::query_history_store::set_store_path_override(None);
    }

    #[test]
    fn read_entries_applies_the_limit_to_the_newest() {
        let _serial = crate::query_history_store::store_path_test_guard();
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("history.db");
        crate::query_history_store::set_store_path_override(Some(db_path.clone()));
        let history = QueryHistory::with_store_path(db_path.clone());
        for i in 0..5 {
            history.log(&HistoryRecord {
                sql: &format!("SELECT {i}"),
                server: "srv",
                database: None,
                duration_ms: None,
                outcome: HistoryOutcome::Unknown,
                provenance: HistoryProvenance::Manual,
                affected_rows: None,
                rollback_sql: None,
            });
        }
        let entries = crate::query_history_store::read_entries_from_path(&db_path, 2);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].sql, "SELECT 4");
        assert_eq!(entries[1].sql, "SELECT 3");
        crate::query_history_store::set_store_path_override(None);
    }

    #[test]
    fn read_entries_skips_corrupt_lines_and_keeps_multiline_sql() {
        let text = "garbage before any header\n\
                    [2026-08-24T09:00:00Z] srv/db\n\
                    SELECT a\n\
                    \n\
                    FROM t -- internal blank line survives\n\
                    \n\
                    [not a timestamp] junk/header\n\
                    [2026-08-24T09:05:00Z] srv2/\n\
                    UPDATE [x] SET v = 1\n\
                    \n\
                    [2026-08-24T09:06:00Z] srv3/db3\n\n";
        let entries = parse_history(text);
        assert_eq!(entries.len(), 2, "corrupt header and empty body skipped");
        assert_eq!(
            entries[0].sql,
            "SELECT a\n\nFROM t -- internal blank line survives\n\n[not a timestamp] junk/header"
        );
        assert_eq!(entries[0].database.as_deref(), Some("db"));
        assert_eq!(entries[1].server, "srv2");
        assert_eq!(entries[1].database, None, "empty database reads as None");
        assert_eq!(entries[1].sql, "UPDATE [x] SET v = 1");
    }

    #[test]
    fn read_entries_from_missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            crate::query_history_store::read_entries_from_path(&dir.path().join("nope.db"), 10)
                .is_empty()
        );
    }

    #[test]
    fn filter_entries_is_case_insensitive_over_all_fields() {
        let entries = vec![
            entry("SELECT * FROM Orders", "Prod-01", Some("Sales")),
            entry("UPDATE users SET active = 1", "dev-box", None),
        ];
        let hits = |needle: &str| {
            filter_entries(&entries, needle)
                .into_iter()
                .map(|e| e.server.as_str())
                .collect::<Vec<_>>()
        };
        assert_eq!(hits("orders"), vec!["Prod-01"], "matches SQL");
        assert_eq!(hits("PROD"), vec!["Prod-01"], "matches server");
        assert_eq!(hits("sales"), vec!["Prod-01"], "matches database");
        assert_eq!(hits("USERS"), vec!["dev-box"]);
        assert_eq!(hits("").len(), 2, "blank needle matches everything");
        assert_eq!(hits("   ").len(), 2, "whitespace needle matches everything");
        assert!(hits("nothing-here").is_empty());
    }

    #[test]
    fn structured_history_filters_compose() {
        let mut success = entry("SELECT * FROM orders", "prod-01", Some("sales"));
        success.duration_ms = Some(1_250);
        success.outcome = HistoryOutcome::Succeeded;
        let mut failed = entry("UPDATE users SET active = 1", "dev-box", Some("app"));
        failed.timestamp = "2026-08-20T10:00:00Z".to_string();
        failed.duration_ms = Some(50);
        failed.outcome = HistoryOutcome::Failed;
        let entries = vec![success, failed];

        let hits = filter_entries(
            &entries,
            "orders server:prod db:sales outcome:success after:2026-08-23 min:1s max:2s",
        );
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].server, "prod-01");
        assert!(
            filter_entries(&entries, "outcome:failed before:2026-08-21")[0]
                .sql
                .starts_with("UPDATE")
        );
    }

    #[test]
    fn duration_filter_parser_accepts_milliseconds_and_fractional_seconds() {
        assert_eq!(parse_duration_ms("250ms"), Some(250));
        assert_eq!(parse_duration_ms("1.5s"), Some(1_500));
        assert_eq!(parse_duration_ms("42"), Some(42));
        assert_eq!(parse_duration_ms("later"), None);
    }

    #[test]
    fn retention_applies_age_then_newest_count_and_rewrites_atomically() {
        let _serial = crate::query_history_store::store_path_test_guard();
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("history.db");
        crate::query_history_store::set_store_path_override(Some(db_path.clone()));
        let history = QueryHistory::with_store_path(db_path.clone());
        for (day, sql) in [(1, "old"), (20, "middle"), (24, "newest")] {
            history.insert_at(
                &HistoryRecord {
                    sql,
                    server: "srv",
                    database: Some("db"),
                    duration_ms: None,
                    outcome: HistoryOutcome::Unknown,
                    provenance: HistoryProvenance::Manual,
                    affected_rows: None,
                    rollback_sql: None,
                },
                &format!("2026-08-{day:02}T10:00:00Z"),
            );
        }

        let now = chrono::DateTime::parse_from_rfc3339("2026-08-24T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(
            crate::query_history_store::apply_retention_to_path(&db_path, now, 10, 1).unwrap(),
            2
        );
        let retained = crate::query_history_store::read_entries_from_path(&db_path, 10);
        assert_eq!(retained.len(), 1);
        assert_eq!(retained[0].sql, "newest");
        crate::query_history_store::set_store_path_override(None);
    }

    // -- pins ----------------------------------------------------------------

    #[test]
    fn pins_round_trip_through_the_json_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pins.json");
        let pins = vec![
            entry("SELECT 1", "srv", Some("db")),
            entry("SELECT 2\nFROM t", "other", None),
        ];
        save_pins_to(&path, &pins).unwrap();
        assert_eq!(load_pins_from(&path), pins);
    }

    #[test]
    fn load_pins_tolerates_missing_and_corrupt_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pins.json");
        assert!(load_pins_from(&path).is_empty());
        std::fs::write(&path, "{ not json").unwrap();
        assert!(load_pins_from(&path).is_empty());
    }

    #[test]
    fn toggle_pin_matches_on_sql_and_server_only() {
        let mut pins = Vec::new();
        let original = entry("SELECT 1", "srv", Some("db"));
        assert!(toggle_pin(&mut pins, &original), "first toggle pins");
        assert!(is_pinned(&pins, &original));

        // Same SQL + server with a different database/timestamp still counts
        // as the same pin…
        let mut same_statement = entry("SELECT 1", "srv", None);
        same_statement.timestamp = "2026-08-24T11:00:00Z".to_string();
        assert!(is_pinned(&pins, &same_statement));

        // …while a different server does not.
        let other_server = entry("SELECT 1", "other", Some("db"));
        assert!(!is_pinned(&pins, &other_server));
        assert!(toggle_pin(&mut pins, &other_server));
        assert_eq!(pins.len(), 2);
        assert_eq!(pins[0].server, "other", "newest pin lands first");

        assert!(
            !toggle_pin(&mut pins, &same_statement),
            "second toggle unpins"
        );
        assert!(!is_pinned(&pins, &original));
        assert_eq!(pins.len(), 1);
    }

    #[test]
    fn rollback_field_round_trips() {
        let rollback = "BEGIN;\nUPDATE t SET x = 1;\nCOMMIT;";
        let line = format_completed_entry(
            "SELECT 1",
            "srv",
            Some("db"),
            Some(10),
            HistoryOutcome::Succeeded,
            Some(rollback),
        );
        let header = line.lines().next().expect("header line");
        let parsed = parse_header(header).expect("parsed header");
        assert_eq!(parsed.rollback_sql.as_deref(), Some(rollback));
    }

    #[test]
    fn five_field_header_still_parses() {
        let header = "[2026-08-24T10:00:00Z] srv/db\tsucceeded\t42";
        let parsed = parse_header(header).expect("parsed header");
        assert_eq!(parsed.server, "srv");
        assert_eq!(parsed.database.as_deref(), Some("db"));
        assert_eq!(parsed.duration_ms, Some(42));
        assert!(parsed.rollback_sql.is_none());
    }
}
