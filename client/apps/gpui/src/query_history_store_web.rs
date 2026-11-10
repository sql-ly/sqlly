//! Browser query execution history: the legacy append-only text log in
//! localStorage. Dedupe, provenance, affected rows, and rollback SQL are not
//! persisted in the browser build — the header format is unchanged so older
//! readers still parse entries.

use std::path::PathBuf;

use crate::host_storage::{StorageArea, load_text, store_text};
use crate::query_history::{
    HISTORY_FILE_NAME, HistoryCursor, HistoryEntry, HistoryPage, HistoryRecord, filter_entries,
    format_completed_entry, format_existing_entry, newest_first, parse_history,
    retained_history_entries,
};

pub struct QueryHistory {
    _private: (),
}

impl QueryHistory {
    pub fn new() -> Self {
        Self { _private: () }
    }

    pub fn with_store_path(_path: PathBuf) -> Self {
        Self::new()
    }

    pub fn log(&self, record: &HistoryRecord<'_>) {
        let entry = format_completed_entry(
            record.sql,
            record.server,
            record.database,
            record.duration_ms,
            record.outcome,
            record.rollback_sql,
        );
        let mut history = load_text(StorageArea::Data, HISTORY_FILE_NAME).unwrap_or_default();
        history.push_str(&entry);
        let _ = store_text(StorageArea::Data, HISTORY_FILE_NAME, &history);
    }
}

impl Default for QueryHistory {
    fn default() -> Self {
        Self::new()
    }
}

pub fn read_page(filter: &str, after: Option<&HistoryCursor>, limit: usize) -> HistoryPage {
    let all = newest_first(
        parse_history(&load_text(StorageArea::Data, HISTORY_FILE_NAME).unwrap_or_default()),
        usize::MAX,
    );
    let filtered: Vec<HistoryEntry> = filter_entries(&all, filter)
        .into_iter()
        .enumerate()
        .map(|(index, entry)| {
            let mut entry = (*entry).clone();
            entry.id = i64::try_from(index + 1).unwrap_or(i64::MAX);
            entry
        })
        .collect();
    let start = after.map_or(0, |cursor| {
        filtered
            .iter()
            .position(|entry| entry.timestamp == cursor.executed_at && entry.id == cursor.id)
            .map_or(filtered.len(), |index| index + 1)
    });
    let slice = filtered
        .into_iter()
        .skip(start)
        .take(limit + 1)
        .collect::<Vec<_>>();
    let mut entries = slice;
    let mut next = None;
    if entries.len() > limit {
        let last_returned = entries[limit - 1].clone();
        entries.truncate(limit);
        next = Some(HistoryCursor {
            executed_at: last_returned.timestamp,
            id: last_returned.id,
        });
    }
    HistoryPage { entries, next }
}

pub fn read_entries(limit: usize) -> Vec<HistoryEntry> {
    read_page("", None, limit).entries
}

pub fn apply_retention() -> Result<usize, String> {
    let prefs = crate::preferences::QueryHistoryPreferences::load();
    let days = prefs.execution_retention_days();
    let max_entries = prefs.execution_max_entries();
    let text = load_text(StorageArea::Data, HISTORY_FILE_NAME).unwrap_or_default();
    let before = parse_history(&text);
    let before_len = before.len();
    let retained = retained_history_entries(before, chrono::Utc::now(), days, max_entries);
    let removed = before_len.saturating_sub(retained.len());
    if removed > 0 {
        let text = retained
            .iter()
            .map(format_existing_entry)
            .collect::<String>();
        store_text(StorageArea::Data, HISTORY_FILE_NAME, &text)?;
    }
    Ok(removed)
}
