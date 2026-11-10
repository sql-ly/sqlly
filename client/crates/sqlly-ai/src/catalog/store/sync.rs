//! Sync: diff a freshly-observed schema against the persisted catalog,
//! upserting changes and tombstoning what disappeared.

use std::collections::{HashMap, HashSet};

use chrono::Utc;
use rusqlite::{Connection, ToSql, params, params_from_iter};

use crate::catalog::hash::object_hash;
use crate::catalog::wire::CatalogObject;

use super::ai_cache::{delete_cards_for_object, upsert_metadata_cards};
use super::embeddings::delete_embedding_row;
use super::relationships::rebuild_inferred_relationships;
use super::search::{delete_search_document, upsert_search_document};
use super::sql_util::{batch_update_in, chunked_slices, in_placeholders};
use super::{CatalogError, CatalogStore};

/// What kind of change a single object underwent during a sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum ObjectChange {
    /// Brand new to the catalog.
    Added,
    /// Body hash differs from the previously stored hash.
    Changed,
    /// Hash identical to before — no work to do.
    Unchanged,
    /// Was present, now absent — tombstoned.
    Removed,
    /// Was tombstoned, has reappeared.
    Resurrected,
}

impl ObjectChange {
    /// Does this change invalidate the AI cache for the object?
    pub fn invalidates_ai_cache(self) -> bool {
        matches!(
            self,
            ObjectChange::Added
                | ObjectChange::Changed
                | ObjectChange::Removed
                | ObjectChange::Resurrected
        )
    }
}

/// How much of the source a [`CatalogStore::sync_with_coverage`] observation
/// covers, which bounds what may be tombstoned when missing. Serialized on the
/// engine wire as JSON.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SyncCoverage {
    /// The observation is the complete truth for this catalog's scope:
    /// anything live that was not observed no longer exists at the source.
    #[default]
    FullSource,
    /// The observation covered exactly these object keys; only keys in the
    /// set may be tombstoned when missing from the observation.
    Scoped { object_keys: Vec<String> },
    /// An incomplete observation (failed metadata category, interrupted
    /// scan): upsert what arrived and never infer deletions.
    Partial,
}

/// Summary of one sync operation.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct SyncReport {
    pub added: usize,
    pub changed: usize,
    pub unchanged: usize,
    pub removed: usize,
    pub resurrected: usize,
    /// Number of objects whose AI-cache flag was flipped to stale this sync.
    pub ai_cache_invalidated: usize,
    /// Number of name-inferred relationship edges persisted this sync (rebuilt
    /// wholesale each sync from the live catalog).
    #[serde(default)]
    pub inferred_relationships: usize,
}

impl SyncReport {
    /// Was this sync a no-op (nothing added/changed/removed)?
    pub fn is_clean(&self) -> bool {
        self.added == 0 && self.changed == 0 && self.removed == 0 && self.resurrected == 0
    }

    /// Total objects the caller asserted exist in the source.
    pub fn observed(&self) -> usize {
        self.added + self.changed + self.unchanged + self.resurrected
    }
}

impl CatalogStore {
    /// Persist a freshly-observed set of objects and return what changed.
    ///
    /// This is the "cache update trigger": the caller hands us the objects it
    /// saw when it went back to the source, and we diff them against the stored
    /// state using the timestamp gate + hash. The observation is treated as
    /// the complete truth for this catalog's scope ([`SyncCoverage::FullSource`]);
    /// callers with a narrower observation use [`Self::sync_with_coverage`].
    pub fn sync(&self, objects: &[CatalogObject]) -> Result<SyncReport, CatalogError> {
        self.sync_with_coverage(objects, SyncCoverage::FullSource)
    }

    /// [`Self::sync`] with explicit coverage semantics: only objects the
    /// observation provably covered may be tombstoned when missing, so a
    /// partial scan (failed metadata category, interrupted refresh, scoped
    /// re-read) can never destroy knowledge it simply didn't look at.
    pub fn sync_with_coverage(
        &self,
        objects: &[CatalogObject],
        coverage: SyncCoverage,
    ) -> Result<SyncReport, CatalogError> {
        let now = Utc::now().timestamp_millis();
        let mut report = SyncReport::default();
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;

        let mut seen_keys: HashSet<String> = HashSet::with_capacity(objects.len());
        let keys: Vec<String> = objects.iter().map(CatalogObject::object_key).collect();
        let existing_rows = load_existing_rows(&tx, &keys)?;
        let mut unchanged_by_ts: HashMap<Option<i64>, Vec<String>> = HashMap::new();

        for obj in objects {
            let key = obj.object_key();
            seen_keys.insert(key.clone());
            let incoming_hash = object_hash(obj);
            let incoming_ts = obj.source_timestamp();

            let existing = existing_rows.get(&key);

            let change = classify(existing, &incoming_hash, incoming_ts);
            let invalidated = change.invalidates_ai_cache();

            match change {
                ObjectChange::Unchanged => {
                    unchanged_by_ts.entry(incoming_ts).or_default().push(key);
                    report.unchanged += 1;
                }
                ObjectChange::Changed | ObjectChange::Resurrected | ObjectChange::Added => {
                    upsert_object(&tx, obj, &key, &incoming_hash, now)?;
                    upsert_search_document(&tx, obj, &key, now)?;
                    upsert_metadata_cards(&tx, obj, &key, now)?;
                    match change {
                        ObjectChange::Changed => report.changed += 1,
                        ObjectChange::Resurrected => report.resurrected += 1,
                        ObjectChange::Added => report.added += 1,
                        _ => unreachable!(),
                    }
                    if invalidated {
                        report.ai_cache_invalidated += 1;
                    }
                }
                ObjectChange::Removed => unreachable!("removed is handled after the loop"),
            }
        }

        for (incoming_ts, keys) in unchanged_by_ts {
            let ts_param: Option<i64> = incoming_ts;
            batch_update_in(
                &tx,
                "UPDATE catalog_object
                 SET source_timestamp=?1, last_synced_at=?2, is_deleted=0
                 WHERE object_key IN",
                "",
                &[&ts_param as &dyn ToSql, &now],
                &keys,
            )?;
        }

        // Tombstone anything we previously knew about that didn't show up —
        // but only within what the observation provably covered.
        let known_live_keys = match &coverage {
            SyncCoverage::FullSource => live_keys(&tx)?,
            SyncCoverage::Scoped { object_keys } => live_keys(&tx)?
                .into_iter()
                .filter(|k| object_keys.iter().any(|scoped| scoped == k))
                .collect(),
            SyncCoverage::Partial => Vec::new(),
        };
        let tombstone_keys: Vec<String> = known_live_keys
            .into_iter()
            .filter(|k| !seen_keys.contains(k))
            .collect();
        if !tombstone_keys.is_empty() {
            batch_update_in(
                &tx,
                "UPDATE catalog_object
                 SET is_deleted=1, ai_cache_valid=0, last_synced_at=?1
                 WHERE object_key IN",
                " AND is_deleted=0",
                &[&now],
                &tombstone_keys,
            )?;
            for k in &tombstone_keys {
                delete_search_document(&tx, k)?;
                delete_cards_for_object(&tx, k)?;
                delete_embedding_row(&tx, k)?;
            }
            report.removed = tombstone_keys.len();
            report.ai_cache_invalidated += tombstone_keys.len();
        }

        // Recompute name-inferred relationships across the (now fully-upserted)
        // live catalog. Declared FKs are untouched; only `is_inferred=1` rows are
        // rebuilt, so a newly-added target also wires up references from
        // previously-unchanged sources within the same full sync. Partial
        // coverage always rebuilds (unchanged behavior); full/scoped syncs skip
        // when no objects were added, changed, removed, or resurrected.
        let structural_change =
            report.added + report.changed + report.removed + report.resurrected > 0;
        let should_rebuild_inferred =
            matches!(coverage, SyncCoverage::Partial) || structural_change;
        report.inferred_relationships = if should_rebuild_inferred {
            rebuild_inferred_relationships(&tx)?
        } else {
            0
        };

        tx.commit()?;
        Ok(report)
    }
}

struct StoredRow {
    hash: String,
    ts: Option<i64>,
    is_deleted: bool,
}

/// Decide what kind of change an object underwent.
///
/// - When the stored timestamp matches the incoming one, we trust it and skip
///   the hash entirely (fast path). Equal timestamp ⇒ unchanged.
/// - When the timestamp moved (or is unknown on either side), recompute: the
///   hash is the final arbiter. Equal hash ⇒ unchanged (the timestamp lie was a
///   false positive). Different hash ⇒ changed.
/// - A tombstoned object that reappears is `Resurrected`.
fn classify(
    existing: Option<&StoredRow>,
    incoming_hash: &str,
    incoming_ts: Option<i64>,
) -> ObjectChange {
    match existing {
        None => ObjectChange::Added,
        Some(row) if row.is_deleted => ObjectChange::Resurrected,
        Some(row) => {
            // Fast path: identical observed timestamp ⇒ unchanged.
            if ts_equal(row.ts, incoming_ts) {
                return ObjectChange::Unchanged;
            }
            // Timestamp moved or unknown → hash decides.
            if row.hash == incoming_hash {
                ObjectChange::Unchanged
            } else {
                ObjectChange::Changed
            }
        }
    }
}

/// Timestamps are "equal" for gating purposes when both are present and match.
/// When either side is `None` (engine can't report times) we treat them as NOT
/// equal so we always fall through to a hash comparison.
fn ts_equal(a: Option<i64>, b: Option<i64>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

fn upsert_object(
    tx: &Connection,
    obj: &CatalogObject,
    key: &str,
    hash: &str,
    now: i64,
) -> Result<(), CatalogError> {
    let body_json = serde_json::to_string(obj)?;
    // Wipe existing columns/relationships for this object (upsert semantics).
    tx.execute(
        "DELETE FROM catalog_column WHERE object_key=?1",
        params![key],
    )?;
    tx.execute(
        "DELETE FROM catalog_relationship WHERE from_object_key=?1",
        params![key],
    )?;

    let catalog: Option<&str> = None;
    tx.execute(
        "INSERT INTO catalog_object
            (object_key, catalog, namespace, name, kind, source_kind, source_path,
             source_timestamp, object_hash, body_json, ai_cache_valid, ai_card_json,
             is_deleted, first_seen_at, last_synced_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,0,NULL,0,?11,?11)
         ON CONFLICT(object_key) DO UPDATE SET
            catalog=excluded.catalog,
            namespace=excluded.namespace,
            name=excluded.name,
            kind=excluded.kind,
            source_kind=excluded.source_kind,
            source_path=excluded.source_path,
            source_timestamp=excluded.source_timestamp,
            object_hash=excluded.object_hash,
            body_json=excluded.body_json,
            ai_cache_valid=0,
            ai_card_json=NULL,
            is_deleted=0,
            last_synced_at=excluded.last_synced_at",
        params![
            key,
            catalog,
            obj.schema,
            obj.name,
            obj.kind.as_str(),
            obj.source.kind.as_str(),
            obj.source.path,
            obj.source_timestamp(),
            hash,
            body_json,
            now,
        ],
    )?;

    insert_columns_bulk(tx, key, obj)?;

    let mut fk_stmt = tx.prepare_cached(
        "INSERT INTO catalog_relationship
            (from_object_key, from_column, to_object_key, to_namespace, to_name, to_column,
             is_inferred, confidence, relationship_type)
         VALUES (?1,?2,
            (SELECT object_key FROM catalog_object WHERE namespace IS ?3 AND name=?4 LIMIT 1),
            ?3, ?4, ?5, 0, 1.0, 'ForeignKey')",
    )?;
    for fk in &obj.foreign_keys {
        for (i, from_col) in fk.columns.iter().enumerate() {
            let to_col = fk.target_columns.get(i).map(|s| s.as_str());
            fk_stmt.execute(params![
                key,
                from_col,
                fk.target_schema.as_deref(),
                fk.target_table,
                to_col
            ])?;
        }
    }

    Ok(())
}

fn insert_columns_bulk(
    tx: &Connection,
    key: &str,
    obj: &CatalogObject,
) -> Result<(), CatalogError> {
    if obj.columns.is_empty() {
        return Ok(());
    }
    let mut sql = String::from(
        "INSERT INTO catalog_column (object_key, ordinal, name, type_name, nullable) VALUES ",
    );
    let mut params: Vec<Box<dyn ToSql>> = Vec::new();
    let mut idx = 1;
    for (i, col) in obj.columns.iter().enumerate() {
        if i > 0 {
            sql.push(',');
        }
        sql.push_str(&format!(
            "(?{},?{},?{},?{},?{})",
            idx,
            idx + 1,
            idx + 2,
            idx + 3,
            idx + 4
        ));
        idx += 5;
        params.push(Box::new(key.to_string()));
        params.push(Box::new(col.ordinal));
        params.push(Box::new(col.name.clone()));
        params.push(Box::new(col.type_name.clone()));
        params.push(Box::new(col.nullable as i64));
    }
    let param_refs: Vec<&dyn ToSql> = params.iter().map(|p| p.as_ref()).collect();
    tx.execute(&sql, params_from_iter(param_refs))?;
    Ok(())
}

fn load_existing_rows(
    tx: &Connection,
    keys: &[String],
) -> Result<HashMap<String, StoredRow>, CatalogError> {
    if keys.is_empty() {
        return Ok(HashMap::new());
    }
    let mut out = HashMap::with_capacity(keys.len());
    for chunk in chunked_slices(keys) {
        let placeholders = in_placeholders(chunk.len(), 1);
        let sql = format!(
            "SELECT object_key, object_hash, source_timestamp, is_deleted
             FROM catalog_object WHERE object_key IN ({placeholders})"
        );
        let mut stmt = tx.prepare(&sql)?;
        let param_refs: Vec<&dyn ToSql> = chunk.iter().map(|k| k as &dyn ToSql).collect();
        let rows = stmt.query_map(params_from_iter(param_refs), |r| {
            Ok((
                r.get::<_, String>(0)?,
                StoredRow {
                    hash: r.get(1)?,
                    ts: r.get(2)?,
                    is_deleted: r.get::<_, i64>(3)? != 0,
                },
            ))
        })?;
        for row in rows {
            let (key, stored) = row?;
            out.insert(key, stored);
        }
    }
    Ok(out)
}

fn live_keys(tx: &Connection) -> Result<Vec<String>, CatalogError> {
    let mut stmt = tx.prepare("SELECT object_key FROM catalog_object WHERE is_deleted=0")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{db_source, file_source, table};
    use super::*;
    use crate::catalog::wire::CatalogForeignKey;

    #[test]
    fn classify_added_for_new_object() {
        assert_eq!(classify(None, "abc", Some(1)), ObjectChange::Added);
    }

    #[test]
    fn classify_unchanged_when_timestamp_matches() {
        let row = StoredRow {
            hash: "abc".into(),
            ts: Some(5),
            is_deleted: false,
        };
        assert_eq!(
            classify(Some(&row), "abc", Some(5)),
            ObjectChange::Unchanged
        );
        // even if hash differs — ts gate short-circuits
        assert_eq!(
            classify(Some(&row), "xyz", Some(5)),
            ObjectChange::Unchanged
        );
    }

    #[test]
    fn classify_changed_when_timestamp_moves_and_hash_differs() {
        let row = StoredRow {
            hash: "abc".into(),
            ts: Some(5),
            is_deleted: false,
        };
        assert_eq!(classify(Some(&row), "xyz", Some(6)), ObjectChange::Changed);
    }

    #[test]
    fn classify_unchanged_when_timestamp_moves_but_hash_same() {
        // false positive eliminated by hash
        let row = StoredRow {
            hash: "abc".into(),
            ts: Some(5),
            is_deleted: false,
        };
        assert_eq!(
            classify(Some(&row), "abc", Some(6)),
            ObjectChange::Unchanged
        );
    }

    #[test]
    fn classify_falls_back_to_hash_when_ts_unknown() {
        // DB engine reports no timestamps → always compare hash
        let row = StoredRow {
            hash: "abc".into(),
            ts: None,
            is_deleted: false,
        };
        assert_eq!(classify(Some(&row), "abc", None), ObjectChange::Unchanged);
        assert_eq!(classify(Some(&row), "xyz", None), ObjectChange::Changed);
    }

    #[test]
    fn classify_resurrected_when_previously_tombstoned() {
        let row = StoredRow {
            hash: "abc".into(),
            ts: Some(5),
            is_deleted: true,
        };
        assert_eq!(
            classify(Some(&row), "abc", Some(5)),
            ObjectChange::Resurrected
        );
    }

    #[test]
    fn sync_adds_objects_and_marks_ai_stale() {
        let store = CatalogStore::open_in_memory().unwrap();
        let report = store
            .sync(&[table("users", &["id"], db_source(Some(1), Some(2)))])
            .unwrap();
        assert_eq!(report.added, 1);
        assert_eq!(report.ai_cache_invalidated, 1);
        assert_eq!(store.live_object_count().unwrap(), 1);
        let status = store.ai_cache_status().unwrap();
        assert_eq!(status.stale, 1);
        assert_eq!(status.valid, 0);
    }

    #[test]
    fn tombstoning_object_deletes_card_rows() {
        let store = CatalogStore::open_in_memory().unwrap();
        store
            .sync(&[table("users", &["id"], db_source(Some(1), Some(2)))])
            .unwrap();
        assert!(!store.cards_for_object("dbo.users").unwrap().is_empty());

        store.sync(&[]).unwrap();

        assert!(store.cards_for_object("dbo.users").unwrap().is_empty());
    }

    #[test]
    fn partial_coverage_never_tombstones() {
        let store = CatalogStore::open_in_memory().unwrap();
        store
            .sync(&[
                table("users", &["id"], db_source(Some(1), Some(2))),
                table("orders", &["id"], db_source(Some(1), Some(2))),
            ])
            .unwrap();

        // An interrupted/incomplete observation — even a fully EMPTY one —
        // must not destroy knowledge it didn't look at.
        let report = store
            .sync_with_coverage(&[], SyncCoverage::Partial)
            .unwrap();
        assert_eq!(report.removed, 0);
        assert!(!store.cards_for_object("dbo.users").unwrap().is_empty());
        assert!(!store.cards_for_object("dbo.orders").unwrap().is_empty());
    }

    #[test]
    fn scoped_coverage_tombstones_only_inside_the_scope() {
        let store = CatalogStore::open_in_memory().unwrap();
        store
            .sync(&[
                table("users", &["id"], db_source(Some(1), Some(2))),
                table("orders", &["id"], db_source(Some(1), Some(2))),
            ])
            .unwrap();

        // The observation covered only dbo.users and found it gone; dbo.orders
        // was outside the scope and must survive.
        let report = store
            .sync_with_coverage(
                &[],
                SyncCoverage::Scoped {
                    object_keys: vec!["dbo.users".to_string()],
                },
            )
            .unwrap();
        assert_eq!(report.removed, 1);
        assert!(store.cards_for_object("dbo.users").unwrap().is_empty());
        assert!(!store.cards_for_object("dbo.orders").unwrap().is_empty());
    }

    #[test]
    fn tombstoned_object_resurrects_on_reobservation() {
        let store = CatalogStore::open_in_memory().unwrap();
        let obj = table("users", &["id"], db_source(Some(1), Some(2)));
        store.sync(std::slice::from_ref(&obj)).unwrap();
        store.sync(&[]).unwrap();

        let report = store.sync(std::slice::from_ref(&obj)).unwrap();
        assert_eq!(report.resurrected, 1);
        assert!(!store.cards_for_object("dbo.users").unwrap().is_empty());
    }

    #[test]
    fn sync_is_clean_when_nothing_changed() {
        let store = CatalogStore::open_in_memory().unwrap();
        let obj = table("users", &["id"], db_source(Some(1), Some(2)));
        store.sync(std::slice::from_ref(&obj)).unwrap();
        let report = store.sync(&[obj]).unwrap();
        assert_eq!(report.unchanged, 1);
        assert!(report.is_clean());
        assert_eq!(report.ai_cache_invalidated, 0);
    }

    #[test]
    fn sync_detects_column_change_and_invalidates_ai_cache() {
        let store = CatalogStore::open_in_memory().unwrap();
        store
            .sync(&[table("users", &["id"], db_source(Some(1), Some(2)))])
            .unwrap();
        let report = store
            .sync(&[table(
                "users",
                &["id", "email"],
                db_source(Some(1), Some(3)),
            )])
            .unwrap();
        assert_eq!(report.changed, 1);
        assert_eq!(report.ai_cache_invalidated, 1);
    }

    #[test]
    fn sync_eliminates_false_positive_via_hash() {
        // Timestamp moved but body identical → unchanged.
        let store = CatalogStore::open_in_memory().unwrap();
        store
            .sync(&[table("users", &["id"], db_source(Some(1), Some(2)))])
            .unwrap();
        let report = store
            .sync(&[table("users", &["id"], db_source(Some(1), Some(99)))])
            .unwrap();
        assert_eq!(report.unchanged, 1);
        assert_eq!(report.changed, 0);
        assert_eq!(report.ai_cache_invalidated, 0);
    }

    #[test]
    fn sync_relies_on_hash_when_timestamps_absent() {
        let store = CatalogStore::open_in_memory().unwrap();
        store
            .sync(&[table("users", &["id"], db_source(None, None))])
            .unwrap();
        let report = store
            .sync(&[table("users", &["id"], db_source(None, None))])
            .unwrap();
        assert_eq!(report.unchanged, 1);
        let report = store
            .sync(&[table("users", &["id", "name"], db_source(None, None))])
            .unwrap();
        assert_eq!(report.changed, 1);
    }

    #[test]
    fn sync_tombstones_removed_objects() {
        let store = CatalogStore::open_in_memory().unwrap();
        store
            .sync(&[
                table("users", &["id"], db_source(Some(1), Some(2))),
                table("orders", &["id"], db_source(Some(1), Some(2))),
            ])
            .unwrap();
        let report = store
            .sync(&[table("users", &["id"], db_source(Some(1), Some(2)))])
            .unwrap();
        assert_eq!(report.removed, 1);
        assert_eq!(store.live_object_count().unwrap(), 1);
    }

    #[test]
    fn sync_resurrects_reappearing_object() {
        let store = CatalogStore::open_in_memory().unwrap();
        let users = table("users", &["id"], db_source(Some(1), Some(2)));
        store.sync(std::slice::from_ref(&users)).unwrap();
        // Drop it.
        store.sync(&[]).unwrap();
        assert_eq!(store.live_object_count().unwrap(), 0);
        // Bring it back.
        let report = store.sync(&[users]).unwrap();
        assert_eq!(report.resurrected, 1);
        assert_eq!(store.live_object_count().unwrap(), 1);
    }

    #[test]
    fn foreign_keys_are_persisted_as_relationships() {
        let store = CatalogStore::open_in_memory().unwrap();
        store
            .sync(&[table("orders", &[], db_source(Some(1), Some(2)))])
            .unwrap();
        let mut orders = table("orders", &["id", "user_id"], db_source(Some(1), Some(3)));
        orders.foreign_keys.push(CatalogForeignKey {
            columns: vec!["user_id".into()],
            target_schema: Some("dbo".into()),
            target_table: "users".into(),
            target_columns: vec!["id".into()],
        });
        store.sync(&[orders]).unwrap();
        let obj = store.get_object("dbo.orders").unwrap().unwrap();
        assert!(obj.body_json.contains("user_id"));
    }

    #[test]
    fn file_source_timestamp_uses_max_of_create_and_modify() {
        // max(100, 300) = 300
        let obj = table("t", &["id"], file_source(Some(100), Some(300)));
        assert_eq!(obj.source_timestamp(), Some(300));
    }

    #[test]
    fn sync_many_objects_preserves_row_counts() {
        let store = CatalogStore::open_in_memory().unwrap();
        let objects: Vec<CatalogObject> = (0..50)
            .map(|i| table(&format!("t{i}"), &["id"], db_source(Some(1), Some(2))))
            .collect();
        let report = store.sync(&objects).unwrap();
        assert_eq!(report.added, 50);
        assert_eq!(store.live_object_count().unwrap(), 50);

        let column_count: i64 = store
            .conn
            .lock()
            .query_row("SELECT COUNT(*) FROM catalog_column", [], |r| r.get(0))
            .unwrap();
        assert_eq!(column_count, 50);

        let resync = store.sync(&objects).unwrap();
        assert_eq!(resync.unchanged, 50);
        assert_eq!(store.live_object_count().unwrap(), 50);
        assert_eq!(
            store
                .conn
                .lock()
                .query_row("SELECT COUNT(*) FROM catalog_column", [], |r| {
                    r.get::<_, i64>(0)
                })
                .unwrap(),
            50
        );
    }
}
