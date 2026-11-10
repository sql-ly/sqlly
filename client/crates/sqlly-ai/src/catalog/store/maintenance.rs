//! Opt-in catalog maintenance: tombstone purging, orphan-embedding cleanup,
//! and FTS index optimization.

use chrono::Utc;
use rusqlite::params;

use super::ai_cache::delete_cards_for_object;
use super::embeddings::delete_embedding_row;
use super::search::delete_search_document;
use super::{CatalogError, CatalogStore};

/// What [`CatalogStore::maintain`] should do. Every operation is opt-in so
/// callers pay only for what they ask.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct MaintenanceOptions {
    /// Purge tombstoned objects whose last sync is older than this many days.
    pub purge_tombstones_older_than_days: Option<u32>,
    /// Remove embedding rows whose object no longer exists live.
    pub drop_orphan_embeddings: bool,
    /// Ask FTS5 to merge its index segments for faster searches.
    pub optimize_search_index: bool,
}

/// What [`CatalogStore::maintain`] actually did.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MaintenanceReport {
    pub tombstones_purged: u64,
    pub orphan_embeddings_removed: u64,
    pub search_index_optimized: bool,
}

impl CatalogStore {
    /// Run the requested maintenance operations. Explicit-only — nothing here
    /// runs on open or on sync — and every deletion touches only derived or
    /// tombstoned state, never live catalog knowledge.
    pub fn maintain(
        &self,
        options: &MaintenanceOptions,
    ) -> Result<MaintenanceReport, CatalogError> {
        let mut report = MaintenanceReport::default();
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;

        if let Some(days) = options.purge_tombstones_older_than_days {
            let keys: Vec<String> = if days == 0 {
                let mut stmt =
                    tx.prepare("SELECT object_key FROM catalog_object WHERE is_deleted = 1")?;
                let rows = stmt.query_map([], |r| r.get(0))?;
                rows.collect::<Result<_, _>>()?
            } else {
                let cutoff = Utc::now().timestamp_millis() - i64::from(days) * 86_400_000;
                let mut stmt = tx.prepare(
                    "SELECT object_key FROM catalog_object
                     WHERE is_deleted = 1 AND last_synced_at < ?1",
                )?;
                let rows = stmt.query_map(params![cutoff], |r| r.get(0))?;
                rows.collect::<Result<_, _>>()?
            };
            for key in &keys {
                delete_search_document(&tx, key)?;
                delete_cards_for_object(&tx, key)?;
                delete_embedding_row(&tx, key)?;
                tx.execute(
                    "DELETE FROM catalog_relationship WHERE from_object_key = ?1 OR to_object_key = ?1",
                    params![key],
                )?;
            }
            report.tombstones_purged = if days == 0 {
                tx.execute("DELETE FROM catalog_object WHERE is_deleted = 1", [])? as u64
            } else {
                let cutoff = Utc::now().timestamp_millis() - i64::from(days) * 86_400_000;
                tx.execute(
                    "DELETE FROM catalog_object WHERE is_deleted = 1 AND last_synced_at < ?1",
                    params![cutoff],
                )? as u64
            };
        }

        if options.drop_orphan_embeddings {
            report.orphan_embeddings_removed = tx.execute(
                "DELETE FROM catalog_embedding
                 WHERE object_key NOT IN (
                     SELECT object_key FROM catalog_object WHERE is_deleted = 0
                 )",
                [],
            )? as u64;
        }

        if options.optimize_search_index {
            tx.execute(
                "INSERT INTO catalog_search_fts(catalog_search_fts) VALUES('optimize')",
                [],
            )?;
            report.search_index_optimized = true;
        }

        tx.commit()?;
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{db_source, table};
    use super::*;

    #[test]
    fn maintenance_purges_old_tombstones_and_orphans() {
        let store = CatalogStore::open_in_memory().unwrap();
        store
            .sync(&[table("users", &["id"], db_source(Some(1), Some(2)))])
            .unwrap();
        store.sync(&[]).unwrap(); // tombstone dbo.users

        // Fresh tombstone survives a 30-day purge window…
        let report = store
            .maintain(&MaintenanceOptions {
                purge_tombstones_older_than_days: Some(30),
                drop_orphan_embeddings: true,
                optimize_search_index: true,
            })
            .unwrap();
        assert_eq!(report.tombstones_purged, 0);
        assert!(report.search_index_optimized);

        // …and a zero-day window purges it entirely.
        let report = store
            .maintain(&MaintenanceOptions {
                purge_tombstones_older_than_days: Some(0),
                drop_orphan_embeddings: false,
                optimize_search_index: false,
            })
            .unwrap();
        assert_eq!(report.tombstones_purged, 1);
    }
}
