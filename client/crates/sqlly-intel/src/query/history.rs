use std::time::SystemTime;

#[derive(Debug, Clone)]
pub struct QueryHistoryEntry {
    pub sql: String,
    pub database: Option<String>,
    pub executed_at: SystemTime,
    pub elapsed_ms: u128,
    pub rows_returned: u64,
}

#[derive(Debug, Clone, Default)]
pub struct QueryHistory {
    entries: Vec<QueryHistoryEntry>,
    max_entries: usize,
}

impl QueryHistory {
    pub fn new(max_entries: usize) -> Self {
        Self {
            entries: Vec::new(),
            max_entries,
        }
    }

    pub fn push(&mut self, entry: QueryHistoryEntry) {
        self.entries.push(entry);
        if self.max_entries > 0 && self.entries.len() > self.max_entries {
            let overflow = self.entries.len() - self.max_entries;
            self.entries.drain(0..overflow);
        }
    }

    pub fn recent(&self, limit: usize) -> Vec<&QueryHistoryEntry> {
        self.entries.iter().rev().take(limit).collect()
    }

    pub fn entries(&self) -> &[QueryHistoryEntry] {
        &self.entries
    }

    pub fn search(&self, query: &str) -> Vec<&QueryHistoryEntry> {
        let needle = query.to_ascii_lowercase();
        self.entries
            .iter()
            .rev()
            .filter(|entry| {
                entry.sql.to_ascii_lowercase().contains(&needle)
                    || entry
                        .database
                        .as_deref()
                        .is_some_and(|database| database.to_ascii_lowercase().contains(&needle))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_caps_and_searches_recent_entries() {
        let mut history = QueryHistory::new(2);
        for sql in [
            "SELECT 1",
            "SELECT * FROM sales.Orders",
            "EXEC dbo.RefreshCache",
        ] {
            history.push(QueryHistoryEntry {
                sql: sql.to_string(),
                database: Some("appdb".to_string()),
                executed_at: SystemTime::UNIX_EPOCH,
                elapsed_ms: 1,
                rows_returned: 0,
            });
        }

        assert_eq!(history.search("select").len(), 1);
        assert_eq!(history.search("appdb").len(), 2);
        assert_eq!(history.recent(1)[0].sql, "EXEC dbo.RefreshCache");
        assert_eq!(history.entries().len(), 2);
    }
}
