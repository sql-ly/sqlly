//! AI-card cache: per-object generated cards, the `ai_cache_valid` staleness
//! flag, and the nuclear wipe of all AI-derived data.

use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, ToSql, params, params_from_iter};

use crate::catalog::card::{
    CardType, GeneratedObjectCard, business_meaning_card, extract_prebuilt_text,
    generate_metadata_cards,
};
use crate::catalog::wire::CatalogObject;

use super::sql_util::{chunked_slices, in_placeholders};
use super::{CatalogError, CatalogStore};

/// How many objects still need their AI cache rebuilt.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AiCacheStatus {
    pub valid: usize,
    pub stale: usize,
}

/// Result of [`CatalogStore::nuke_ai_data`]: counts of AI-derived data wiped.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NukeAiDataReport {
    /// Objects whose AI cache was invalidated and card content cleared.
    pub invalidated: usize,
    /// Embedding rows deleted (forcing re-embedding on next build).
    pub embeddings_deleted: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StoredObjectCard {
    pub object_key: String,
    pub card_type: CardType,
    pub content: String,
    pub token_estimate: usize,
    pub content_hash: String,
    pub generated_by: String,
    pub generated_at: i64,
    pub card_format_version: i64,
    pub confidence: f64,
    pub contains_model_inference: bool,
}

impl CatalogStore {
    /// Mark every object's AI cache as stale (e.g. after a forced rebuild of
    /// the datamodel from scratch).
    pub fn invalidate_all_ai_cache(&self) -> Result<usize, CatalogError> {
        let n = self.conn.lock().execute(
            "UPDATE catalog_object SET ai_cache_valid=0 WHERE ai_cache_valid=1",
            [],
        )?;
        Ok(n)
    }

    /// Nuclear option: wipe all AI-derived data so the next rebuild regenerates
    /// everything from scratch. Clears:
    /// - `ai_cache_valid` on every object (marks stale)
    /// - `ai_card_json` on every object (deletes the LLM-generated card text)
    /// - All rows from `catalog_embedding` (forces vector re-embedding)
    ///
    /// Structural data (object definitions, columns, relationships, search
    /// documents) is untouched — only AI-generated/derived content is nuked.
    /// Returns counts of invalidated objects and deleted embeddings.
    pub fn nuke_ai_data(&self) -> Result<NukeAiDataReport, CatalogError> {
        let conn = self.conn.lock();
        let invalidated = conn.execute(
            "UPDATE catalog_object SET ai_cache_valid=0, ai_card_json=NULL WHERE ai_cache_valid=1 OR ai_card_json IS NOT NULL",
            [],
        )?;
        let embeddings_deleted = conn.execute("DELETE FROM catalog_embedding", [])?;
        Ok(NukeAiDataReport {
            invalidated,
            embeddings_deleted,
        })
    }

    /// How many objects have a current vs stale AI cache.
    pub fn ai_cache_status(&self) -> Result<AiCacheStatus, CatalogError> {
        let (valid, stale) = self.conn.lock().query_row(
            "SELECT
                SUM(CASE WHEN ai_cache_valid=1 AND is_deleted=0 THEN 1 ELSE 0 END),
                SUM(CASE WHEN ai_cache_valid=0 AND is_deleted=0 THEN 1 ELSE 0 END)
             FROM catalog_object",
            [],
            |r| {
                Ok((
                    r.get::<_, Option<i64>>(0)?.unwrap_or(0) as usize,
                    r.get::<_, Option<i64>>(1)?.unwrap_or(0) as usize,
                ))
            },
        )?;
        Ok(AiCacheStatus { valid, stale })
    }

    /// Return keys of all live objects whose AI cache is stale, for the builder
    /// to process.
    pub fn stale_ai_keys(&self) -> Result<Vec<String>, CatalogError> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT object_key FROM catalog_object
             WHERE ai_cache_valid=0 AND is_deleted=0",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Persist a generated AI card for an object and mark its cache valid.
    ///
    /// This is the *only* place `ai_cache_valid` is set to 1.
    pub fn set_ai_card(&self, key: &str, card_json: &str) -> Result<(), CatalogError> {
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE catalog_object
             SET ai_card_json=?1, ai_cache_valid=1
             WHERE object_key=?2",
            params![card_json, key],
        )?;
        let text = extract_prebuilt_text(card_json).unwrap_or_else(|| card_json.to_string());
        let card = business_meaning_card(text, "llm");
        upsert_card(&tx, key, &card, Utc::now().timestamp_millis())?;
        tx.commit()?;
        Ok(())
    }

    pub fn upsert_card(
        &self,
        object_key: &str,
        card: &GeneratedObjectCard,
    ) -> Result<(), CatalogError> {
        upsert_card(
            &self.conn.lock(),
            object_key,
            card,
            Utc::now().timestamp_millis(),
        )
    }

    pub fn card_for_object_type(
        &self,
        object_key: &str,
        card_type: CardType,
    ) -> Result<Option<StoredObjectCard>, CatalogError> {
        self.conn
            .lock()
            .query_row(
                "SELECT object_key, card_type, content, token_estimate, content_hash,
                        generated_by, generated_at, card_format_version, confidence,
                        contains_model_inference
                 FROM catalog_object_card
                 WHERE object_key=?1 AND card_type=?2",
                params![object_key, card_type.as_str()],
                stored_card_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn cards_for_object(
        &self,
        object_key: &str,
    ) -> Result<Vec<StoredObjectCard>, CatalogError> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT object_key, card_type, content, token_estimate, content_hash,
                    generated_by, generated_at, card_format_version, confidence,
                    contains_model_inference
             FROM catalog_object_card
             WHERE object_key=?1
             ORDER BY card_type ASC",
        )?;
        let rows = stmt.query_map(params![object_key], stored_card_from_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Fetch stored cards for many object keys and card types in batched queries.
    pub fn cards_for_keys_and_types(
        &self,
        keys: &[String],
        card_types: &[CardType],
    ) -> Result<std::collections::HashMap<(String, CardType), StoredObjectCard>, CatalogError> {
        let mut out = std::collections::HashMap::new();
        if keys.is_empty() || card_types.is_empty() {
            return Ok(out);
        }
        let conn = self.conn.lock();
        let type_literals: Vec<String> = card_types
            .iter()
            .map(|t| format!("'{}'", t.as_str()))
            .collect();
        let type_sql = type_literals.join(",");
        for key_chunk in chunked_slices(keys) {
            let key_placeholders = in_placeholders(key_chunk.len(), 1);
            let sql = format!(
                "SELECT object_key, card_type, content, token_estimate, content_hash,
                        generated_by, generated_at, card_format_version, confidence,
                        contains_model_inference
                 FROM catalog_object_card
                 WHERE object_key IN ({key_placeholders}) AND card_type IN ({type_sql})"
            );
            let mut stmt = conn.prepare(&sql)?;
            let param_refs: Vec<&dyn ToSql> = key_chunk.iter().map(|k| k as &dyn ToSql).collect();
            let rows = stmt.query_map(params_from_iter(param_refs), stored_card_from_row)?;
            for row in rows {
                let card = row?;
                out.insert((card.object_key.clone(), card.card_type), card);
            }
        }
        Ok(out)
    }

    /// Fetch tiny-card content for a batch of object keys in a single query,
    /// returning a map of `object_key` → tiny card content. Used to populate
    /// `tiny_card` previews on hybrid search hits without an N+1 lookup. Keys
    /// without a tiny card are simply absent from the map.
    pub fn tiny_cards_for_keys(
        &self,
        keys: &[String],
    ) -> Result<std::collections::HashMap<String, String>, CatalogError> {
        let mut out = std::collections::HashMap::new();
        if keys.is_empty() {
            return Ok(out);
        }
        let conn = self.conn.lock();
        for chunk in chunked_slices(keys) {
            let placeholders = in_placeholders(chunk.len(), 1);
            let sql = format!(
                "SELECT object_key, content
                 FROM catalog_object_card
                 WHERE card_type='tiny' AND object_key IN ({placeholders})"
            );
            let mut stmt = conn.prepare(&sql)?;
            let param_refs: Vec<&dyn ToSql> = chunk.iter().map(|k| k as &dyn ToSql).collect();
            let rows = stmt.query_map(params_from_iter(param_refs), |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (key, content) = row?;
                out.insert(key, content);
            }
        }
        Ok(out)
    }

    pub fn delete_cards_for_object(&self, object_key: &str) -> Result<(), CatalogError> {
        delete_cards_for_object(&self.conn.lock(), object_key)
    }
}

pub(super) fn upsert_metadata_cards(
    tx: &Connection,
    obj: &CatalogObject,
    key: &str,
    now: i64,
) -> Result<(), CatalogError> {
    let cards = generate_metadata_cards(obj);
    let has_safety = cards.iter().any(|card| card.card_type == CardType::Safety);
    for card in cards {
        upsert_card(tx, key, &card, now)?;
    }
    if !has_safety {
        tx.execute(
            "DELETE FROM catalog_object_card WHERE object_key=?1 AND card_type='safety'",
            params![key],
        )?;
    }
    Ok(())
}

pub(super) fn upsert_card(
    conn: &Connection,
    object_key: &str,
    card: &GeneratedObjectCard,
    now: i64,
) -> Result<(), CatalogError> {
    conn.execute(
        "INSERT INTO catalog_object_card
            (object_key, card_type, content, token_estimate, content_hash,
             generated_by, generated_at, card_format_version, confidence,
             contains_model_inference)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
         ON CONFLICT(object_key, card_type) DO UPDATE SET
            content=excluded.content,
            token_estimate=excluded.token_estimate,
            content_hash=excluded.content_hash,
            generated_by=excluded.generated_by,
            generated_at=excluded.generated_at,
            card_format_version=excluded.card_format_version,
            confidence=excluded.confidence,
            contains_model_inference=excluded.contains_model_inference
         WHERE catalog_object_card.content_hash <> excluded.content_hash
            OR catalog_object_card.card_format_version <> excluded.card_format_version",
        params![
            object_key,
            card.card_type.as_str(),
            &card.content,
            card.token_estimate as i64,
            &card.content_hash,
            &card.generated_by,
            now,
            card.card_format_version,
            card.confidence,
            card.contains_model_inference as i64,
        ],
    )?;
    Ok(())
}

pub(super) fn delete_cards_for_object(tx: &Connection, key: &str) -> Result<(), CatalogError> {
    tx.execute(
        "DELETE FROM catalog_object_card WHERE object_key=?1",
        params![key],
    )?;
    Ok(())
}

fn stored_card_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<StoredObjectCard> {
    let card_type: String = r.get(1)?;
    Ok(StoredObjectCard {
        object_key: r.get(0)?,
        card_type: CardType::parse(&card_type).unwrap_or(CardType::Compact),
        content: r.get(2)?,
        token_estimate: r.get::<_, i64>(3)? as usize,
        content_hash: r.get(4)?,
        generated_by: r.get(5)?,
        generated_at: r.get(6)?,
        card_format_version: r.get(7)?,
        confidence: r.get(8)?,
        contains_model_inference: r.get::<_, i64>(9)? != 0,
    })
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{db_source, table};
    use super::*;
    use crate::catalog::card::CARD_FORMAT_VERSION;

    #[test]
    fn sync_creates_metadata_cards_for_changed_objects() {
        let store = CatalogStore::open_in_memory().unwrap();
        store
            .sync(&[table(
                "users",
                &["id", "tenant_id"],
                db_source(Some(1), Some(2)),
            )])
            .unwrap();

        let cards = store.cards_for_object("dbo.users").unwrap();

        assert!(cards.iter().any(|card| card.card_type == CardType::Tiny));
        assert!(cards.iter().any(|card| card.card_type == CardType::Compact));
        assert!(
            cards
                .iter()
                .any(|card| card.card_type == CardType::SqlGeneration)
        );
        assert!(cards.iter().any(|card| card.card_type == CardType::Safety));
    }

    #[test]
    fn tiny_cards_for_keys_returns_existing_and_skips_missing() {
        let store = CatalogStore::open_in_memory().unwrap();
        store
            .sync(&[
                table("users", &["id"], db_source(Some(1), Some(2))),
                table("orders", &["id"], db_source(Some(1), Some(2))),
            ])
            .unwrap();
        // Sync generates a tiny card per object.
        let keys = vec![
            "dbo.users".to_string(),
            "dbo.orders".to_string(),
            "dbo.missing".to_string(),
        ];
        let map = store.tiny_cards_for_keys(&keys).unwrap();
        assert!(map.contains_key("dbo.users"));
        assert!(map.contains_key("dbo.orders"));
        assert!(!map.contains_key("dbo.missing"));
    }

    #[test]
    fn tiny_cards_for_keys_empty_input_is_empty() {
        let store = CatalogStore::open_in_memory().unwrap();
        assert!(store.tiny_cards_for_keys(&[]).unwrap().is_empty());
    }

    #[test]
    fn tiny_cards_for_keys_handles_two_thousand_keys() {
        let store = CatalogStore::open_in_memory().unwrap();
        let objects: Vec<_> = (0..2000)
            .map(|i| table(&format!("t{i}"), &["id"], db_source(Some(1), Some(2))))
            .collect();
        store.sync(&objects).unwrap();
        let keys: Vec<String> = (0..2000).map(|i| format!("dbo.t{i}")).collect();
        let map = store.tiny_cards_for_keys(&keys).unwrap();
        assert_eq!(map.len(), 2000);
    }

    #[test]
    fn ai_cache_flag_only_cleared_by_set_ai_card() {
        let store = CatalogStore::open_in_memory().unwrap();
        store
            .sync(&[table("users", &["id"], db_source(Some(1), Some(2)))])
            .unwrap();
        let key = "dbo.users";
        // Builder marks it valid.
        store
            .set_ai_card(key, "{\"summary\":\"users table\"}")
            .unwrap();
        let status = store.ai_cache_status().unwrap();
        assert_eq!(status.valid, 1);
        assert_eq!(status.stale, 0);
        // A clean sync does NOT flip it back to stale.
        store
            .sync(&[table("users", &["id"], db_source(Some(1), Some(2)))])
            .unwrap();
        let status = store.ai_cache_status().unwrap();
        assert_eq!(status.valid, 1);
        // But a body change does.
        store
            .sync(&[table(
                "users",
                &["id", "email"],
                db_source(Some(1), Some(3)),
            )])
            .unwrap();
        let status = store.ai_cache_status().unwrap();
        assert_eq!(status.valid, 0);
        assert_eq!(status.stale, 1);
    }

    #[test]
    fn set_ai_card_writes_business_meaning_card() {
        let store = CatalogStore::open_in_memory().unwrap();
        store
            .sync(&[table("users", &["id"], db_source(Some(1), Some(2)))])
            .unwrap();

        store
            .set_ai_card("dbo.users", r#"{"text":"Users business meaning"}"#)
            .unwrap();

        let card = store
            .card_for_object_type("dbo.users", CardType::BusinessMeaning)
            .unwrap()
            .expect("business meaning card should exist");
        assert_eq!(card.content, "Users business meaning");
        assert!(card.contains_model_inference);
    }

    #[test]
    fn synced_metadata_cards_use_current_format_version() {
        let store = CatalogStore::open_in_memory().unwrap();
        store
            .sync(&[table("users", &["id"], db_source(Some(1), Some(2)))])
            .unwrap();

        let cards = store.cards_for_object("dbo.users").unwrap();

        assert!(cards.iter().any(|card| card.card_type == CardType::Tiny));
        assert!(
            cards
                .iter()
                .all(|card| card.card_format_version == CARD_FORMAT_VERSION)
        );
    }

    #[test]
    fn stale_ai_keys_lists_only_stale_live_objects() {
        let store = CatalogStore::open_in_memory().unwrap();
        store
            .sync(&[
                table("users", &["id"], db_source(Some(1), Some(2))),
                table("orders", &["id"], db_source(Some(1), Some(2))),
            ])
            .unwrap();
        store.set_ai_card("dbo.users", "{}").unwrap();
        let stale = store.stale_ai_keys().unwrap();
        assert_eq!(stale, vec!["dbo.orders".to_string()]);
    }

    #[test]
    fn invalidate_all_clears_every_valid_flag() {
        let store = CatalogStore::open_in_memory().unwrap();
        store
            .sync(&[table("users", &["id"], db_source(Some(1), Some(2)))])
            .unwrap();
        store.set_ai_card("dbo.users", "{}").unwrap();
        assert_eq!(store.ai_cache_status().unwrap().valid, 1);
        let n = store.invalidate_all_ai_cache().unwrap();
        assert_eq!(n, 1);
        assert_eq!(store.ai_cache_status().unwrap().valid, 0);
    }

    #[test]
    fn nuke_ai_data_wipes_cards_and_embeddings() {
        let store = CatalogStore::open_in_memory().unwrap();
        let orders = table(
            "orders",
            &["id", "customer_id"],
            db_source(Some(1), Some(2)),
        );
        let users = table("users", &["id"], db_source(Some(1), Some(2)));
        store.sync(&[users, orders]).unwrap();

        // Simulate a valid AI cache and an embedding row.
        store
            .set_ai_card("dbo.orders", r#"{"card":"orders"}"#)
            .unwrap();
        store
            .set_ai_card("dbo.users", r#"{"card":"users"}"#)
            .unwrap();
        let conn = store.conn.lock();
        conn.execute(
            "INSERT OR REPLACE INTO catalog_embedding (object_key, content_hash, dims, embedding, generated_at, provider_id)
             VALUES ('dbo.orders', 'hash1', 3, X'000000000000000000000000', 1, 'stub')",
            [],
        ).unwrap();
        drop(conn);

        let report = store.nuke_ai_data().unwrap();
        assert_eq!(
            report.invalidated, 2,
            "both AI cache entries should be flipped"
        );
        assert_eq!(
            report.embeddings_deleted, 1,
            "one embedding row should be deleted"
        );

        // Verify cards are gone and cache is stale.
        let obj = store.get_object("dbo.orders").unwrap().unwrap();
        assert!(!obj.ai_cache_valid);
        assert!(obj.ai_card_json.is_none());

        // Verify embeddings are gone.
        let conn = store.conn.lock();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM catalog_embedding", [], |r| r.get(0))
            .unwrap();
        drop(conn);
        assert_eq!(count, 0);
    }

    #[test]
    fn nuke_ai_data_idempotent() {
        let store = CatalogStore::open_in_memory().unwrap();
        // No objects: nuke should succeed with zero counts.
        let report = store.nuke_ai_data().unwrap();
        assert_eq!(report.invalidated, 0);
        assert_eq!(report.embeddings_deleted, 0);
    }
}
