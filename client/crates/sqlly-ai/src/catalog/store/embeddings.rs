//! Embedding persistence: per-object vectors keyed by search-document content
//! hash and provider id.

use rusqlite::{Connection, ToSql, params, params_from_iter};

use super::sql_util::{chunked_slices, in_placeholders};
use super::{CatalogError, CatalogStore};

/// The object metadata joined onto each stored embedding for vector-filter
/// matching: kind, catalog (database), and namespace (schema).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EmbeddingObjectMeta {
    pub kind: String,
    pub catalog: Option<String>,
    pub namespace: Option<String>,
}

impl CatalogStore {
    /// Upsert an embedding for an object. Stores the vector as a little-endian
    /// f32 blob keyed by `object_key`.
    pub fn upsert_embedding(
        &self,
        object_key: &str,
        content_hash: &str,
        provider_id: &str,
        vector: &[f32],
    ) -> Result<(), CatalogError> {
        let dims = vector.len() as i64;
        let blob = crate::catalog::vector::encode_embedding(vector);
        let now = chrono::Utc::now().timestamp();
        let conn = self.conn.lock();
        conn.execute(
            "INSERT OR REPLACE INTO catalog_embedding
             (object_key, content_hash, dims, embedding, generated_at, provider_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![object_key, content_hash, dims, blob, now, provider_id],
        )?;
        Ok(())
    }

    /// Retrieve the stored embedding for a single object key.
    pub fn embedding_for(
        &self,
        object_key: &str,
    ) -> Result<Option<crate::catalog::vector::StoredEmbedding>, CatalogError> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT object_key, content_hash, dims, embedding, provider_id
             FROM catalog_embedding WHERE object_key = ?1",
        )?;
        let result = stmt.query_row(params![object_key], |r| {
            let object_key: String = r.get(0)?;
            let content_hash: String = r.get(1)?;
            let dims: i64 = r.get(2)?;
            let blob: Vec<u8> = r.get(3)?;
            let provider_id: String = r.get(4)?;
            Ok((object_key, content_hash, dims, blob, provider_id))
        });
        match result {
            Ok((object_key, content_hash, dims, blob, provider_id)) => {
                let embedding = crate::catalog::vector::decode_embedding(&blob);
                Ok(Some(crate::catalog::vector::StoredEmbedding {
                    object_key,
                    content_hash,
                    dims: dims as usize,
                    embedding,
                    provider_id,
                }))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(CatalogError::Sqlite(e)),
        }
    }

    /// Find object keys whose embeddings are stale: either no embedding row
    /// exists, or the `content_hash` in `catalog_embedding` doesn't match the
    /// current `catalog_search_document`, or the `provider_id` doesn't match
    /// the active provider. Only considers live (non-deleted) objects.
    pub fn stale_embedding_keys(
        &self,
        limit: usize,
        active_provider_id: &str,
    ) -> Result<Vec<String>, CatalogError> {
        let conn = self.conn.lock();
        let mut sql = String::from(
            "SELECT d.object_key
             FROM catalog_search_document d
             JOIN catalog_object o ON o.object_key = d.object_key AND o.is_deleted = 0
             LEFT JOIN catalog_embedding e ON e.object_key = d.object_key
             WHERE e.object_key IS NULL
                OR e.content_hash != d.content_hash
                OR e.provider_id != ?1",
        );
        if limit > 0 {
            sql.push_str(&format!(" LIMIT {}", limit.clamp(1, 100_000)));
        }
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![active_provider_id], |r| r.get(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Count how many objects still need their embedding built.
    pub fn stale_embedding_count(&self, active_provider_id: &str) -> Result<usize, CatalogError> {
        let conn = self.conn.lock();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*)
             FROM catalog_search_document d
             JOIN catalog_object o ON o.object_key = d.object_key AND o.is_deleted = 0
             LEFT JOIN catalog_embedding e ON e.object_key = d.object_key
             WHERE e.object_key IS NULL
                OR e.content_hash != d.content_hash
                OR e.provider_id != ?1",
            params![active_provider_id],
            |r| r.get(0),
        )?;
        Ok(count as usize)
    }

    /// Retrieve search document text and content_hash for the given object keys,
    /// for embedding input.
    pub fn embedding_documents_for_keys(
        &self,
        keys: &[String],
    ) -> Result<Vec<crate::catalog::vector::EmbeddingDocument>, CatalogError> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.conn.lock();
        let mut out = Vec::new();
        for chunk in chunked_slices(keys) {
            let placeholders = in_placeholders(chunk.len(), 1);
            let sql = format!(
                "SELECT object_key, content_hash, title, body, keywords
                 FROM catalog_search_document WHERE object_key IN ({placeholders})"
            );
            let mut stmt = conn.prepare(&sql)?;
            let param_refs: Vec<&dyn ToSql> = chunk.iter().map(|k| k as &dyn ToSql).collect();
            let rows = stmt.query_map(params_from_iter(param_refs), |r| {
                let object_key: String = r.get(0)?;
                let content_hash: String = r.get(1)?;
                let title: String = r.get(2)?;
                let body: String = r.get(3)?;
                let keywords: String = r.get(4)?;
                Ok((object_key, content_hash, title, body, keywords))
            })?;
            for row in rows {
                let (object_key, content_hash, title, body, keywords) = row?;
                let text = crate::catalog::vector::embedding_text(&title, &body, &keywords);
                out.push(crate::catalog::vector::EmbeddingDocument {
                    object_key,
                    content_hash,
                    text,
                });
            }
        }
        Ok(out)
    }

    /// Upsert many embeddings in one transaction, using multi-row statements per chunk.
    pub fn upsert_embeddings_batch(
        &self,
        rows: &[(&str, &str, &str, &[f32])],
    ) -> Result<(), CatalogError> {
        if rows.is_empty() {
            return Ok(());
        }
        let now = chrono::Utc::now().timestamp();
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        const ROW_CHUNK: usize = 50;
        for chunk in super::sql_util::chunked_slices_with_size(rows, ROW_CHUNK) {
            let mut sql = String::from(
                "INSERT OR REPLACE INTO catalog_embedding
                 (object_key, content_hash, dims, embedding, generated_at, provider_id) VALUES ",
            );
            let mut params: Vec<Box<dyn ToSql>> = Vec::new();
            let mut idx = 1;
            for (i, (object_key, content_hash, provider_id, vector)) in chunk.iter().enumerate() {
                if i > 0 {
                    sql.push(',');
                }
                sql.push_str(&format!(
                    "(?{},?{},?{},?{},?{},?{})",
                    idx,
                    idx + 1,
                    idx + 2,
                    idx + 3,
                    idx + 4,
                    idx + 5
                ));
                idx += 6;
                let blob = crate::catalog::vector::encode_embedding(vector);
                params.push(Box::new(object_key.to_string()));
                params.push(Box::new(content_hash.to_string()));
                params.push(Box::new(vector.len() as i64));
                params.push(Box::new(blob));
                params.push(Box::new(now));
                params.push(Box::new(provider_id.to_string()));
            }
            let param_refs: Vec<&dyn ToSql> = params.iter().map(|p| p.as_ref()).collect();
            tx.execute(&sql, params_from_iter(param_refs))?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Delete embedding rows for many object keys in chunked `IN` lists.
    pub fn delete_embeddings(&self, keys: &[String]) -> Result<(), CatalogError> {
        if keys.is_empty() {
            return Ok(());
        }
        let conn = self.conn.lock();
        for chunk in chunked_slices(keys) {
            let placeholders = in_placeholders(chunk.len(), 1);
            let sql = format!("DELETE FROM catalog_embedding WHERE object_key IN ({placeholders})");
            let param_refs: Vec<&dyn ToSql> = chunk.iter().map(|k| k as &dyn ToSql).collect();
            conn.execute(&sql, params_from_iter(param_refs))?;
        }
        Ok(())
    }

    /// Load all embeddings for live objects produced by the active provider,
    /// ordered by `object_key` for deterministic search behavior. Skips
    /// embeddings for tombstoned objects and embeddings written by a different
    /// provider (e.g. stale stub vectors after switching to a real model).
    /// Like [`Self::all_embeddings`], but each embedding is paired with its
    /// catalog object's kind token (`"table"`, `"view"`, `"procedure"`,
    /// `"function"`, `"type"`) so vector-store filters can act on object
    /// kind without a second lookup.
    pub fn all_embeddings_with_kind(
        &self,
        active_provider_id: &str,
    ) -> Result<Vec<(crate::catalog::vector::StoredEmbedding, String)>, CatalogError> {
        Ok(self
            .all_embeddings_with_meta(active_provider_id)?
            .into_iter()
            .map(|(stored, meta)| (stored, meta.kind))
            .collect())
    }

    /// Every live embedding for the provider, joined with the object metadata
    /// the vector filters match on (kind, catalog/database, namespace/schema).
    pub fn all_embeddings_with_meta(
        &self,
        active_provider_id: &str,
    ) -> Result<Vec<(crate::catalog::vector::StoredEmbedding, EmbeddingObjectMeta)>, CatalogError>
    {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT e.object_key, e.content_hash, e.dims, e.embedding, e.provider_id, o.kind, o.catalog, o.namespace
             FROM catalog_embedding e
             JOIN catalog_object o ON o.object_key = e.object_key AND o.is_deleted = 0
             WHERE e.provider_id = ?1
             ORDER BY e.object_key",
        )?;
        let rows = stmt.query_map(params![active_provider_id], |r| {
            let object_key: String = r.get(0)?;
            let content_hash: String = r.get(1)?;
            let dims: i64 = r.get(2)?;
            let blob: Vec<u8> = r.get(3)?;
            let provider_id: String = r.get(4)?;
            let kind: String = r.get(5)?;
            let catalog: Option<String> = r.get(6)?;
            let namespace: Option<String> = r.get(7)?;
            Ok((
                object_key,
                content_hash,
                dims,
                blob,
                provider_id,
                kind,
                catalog,
                namespace,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (object_key, content_hash, dims, blob, provider_id, kind, catalog, namespace) =
                row?;
            let embedding = crate::catalog::vector::decode_embedding(&blob);
            out.push((
                crate::catalog::vector::StoredEmbedding {
                    object_key,
                    content_hash,
                    dims: dims as usize,
                    embedding,
                    provider_id,
                },
                EmbeddingObjectMeta {
                    kind,
                    catalog,
                    namespace,
                },
            ));
        }
        Ok(out)
    }

    pub fn all_embeddings(
        &self,
        active_provider_id: &str,
    ) -> Result<Vec<crate::catalog::vector::StoredEmbedding>, CatalogError> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT e.object_key, e.content_hash, e.dims, e.embedding, e.provider_id
             FROM catalog_embedding e
             JOIN catalog_object o ON o.object_key = e.object_key AND o.is_deleted = 0
             WHERE e.provider_id = ?1
             ORDER BY e.object_key",
        )?;
        let rows = stmt.query_map(params![active_provider_id], |r| {
            let object_key: String = r.get(0)?;
            let content_hash: String = r.get(1)?;
            let dims: i64 = r.get(2)?;
            let blob: Vec<u8> = r.get(3)?;
            let provider_id: String = r.get(4)?;
            Ok((object_key, content_hash, dims, blob, provider_id))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (object_key, content_hash, dims, blob, provider_id) = row?;
            let embedding = crate::catalog::vector::decode_embedding(&blob);
            out.push(crate::catalog::vector::StoredEmbedding {
                object_key,
                content_hash,
                dims: dims as usize,
                embedding,
                provider_id,
            });
        }
        Ok(out)
    }

    /// Delete a single embedding row.
    pub fn delete_embedding(&self, object_key: &str) -> Result<(), CatalogError> {
        let conn = self.conn.lock();
        conn.execute(
            "DELETE FROM catalog_embedding WHERE object_key = ?1",
            params![object_key],
        )?;
        Ok(())
    }

    /// Count total embeddings (including stale ones).
    pub fn embedding_count(&self) -> Result<usize, CatalogError> {
        let conn = self.conn.lock();
        let count: i64 =
            conn.query_row("SELECT COUNT(*) FROM catalog_embedding", [], |r| r.get(0))?;
        Ok(count as usize)
    }

    /// Count live (non-tombstoned) embeddings produced by the active provider.
    /// Scoping by `provider_id` prevents stale embeddings from a previous
    /// provider (e.g. stub vectors after switching to a real model) from
    /// triggering hybrid search that would later discard the mismatched vectors.
    pub fn live_embedding_count(&self, active_provider_id: &str) -> Result<usize, CatalogError> {
        let conn = self.conn.lock();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM catalog_embedding e
             JOIN catalog_object o ON o.object_key = e.object_key AND o.is_deleted = 0
             WHERE e.provider_id = ?1",
            params![active_provider_id],
            |r| r.get(0),
        )?;
        Ok(count as usize)
    }

    /// Count live embeddings for a provider, optionally filtered by object metadata.
    pub fn count_embeddings_filtered(
        &self,
        provider_id: &str,
        source_type: Option<&str>,
        database: Option<&str>,
        schema: Option<&str>,
    ) -> Result<usize, CatalogError> {
        let conn = self.conn.lock();
        let mut sql = String::from(
            "SELECT COUNT(*) FROM catalog_embedding e
             JOIN catalog_object o ON o.object_key = e.object_key AND o.is_deleted = 0
             WHERE e.provider_id = ?1",
        );
        let mut query_params: Vec<Box<dyn ToSql>> = vec![Box::new(provider_id.to_string())];
        let mut next_idx = 2;
        if let Some(kind) = source_type {
            sql.push_str(&format!(" AND lower(o.kind) = lower(?{next_idx})"));
            query_params.push(Box::new(kind.to_string()));
            next_idx += 1;
        }
        if let Some(db) = database {
            sql.push_str(&format!(" AND lower(o.catalog) = lower(?{next_idx})"));
            query_params.push(Box::new(db.to_string()));
            next_idx += 1;
        }
        if let Some(ns) = schema {
            sql.push_str(&format!(" AND lower(o.namespace) = lower(?{next_idx})"));
            query_params.push(Box::new(ns.to_string()));
        }
        let param_refs: Vec<&dyn ToSql> = query_params.iter().map(|p| p.as_ref()).collect();
        let count: i64 = conn.query_row(&sql, params_from_iter(param_refs), |r| r.get(0))?;
        Ok(count as usize)
    }
}

/// Delete an object's embedding row inside a sync transaction. Mirrors
/// `delete_search_document`/`delete_cards_for_object` so tombstoned objects
/// do not leave orphaned vectors behind in `catalog_embedding`.
pub(super) fn delete_embedding_row(tx: &Connection, key: &str) -> Result<(), CatalogError> {
    tx.execute(
        "DELETE FROM catalog_embedding WHERE object_key=?1",
        params![key],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{db_source, table};
    use super::*;

    #[test]
    fn sync_tombstone_deletes_embeddings() {
        let store = CatalogStore::open_in_memory().unwrap();
        store
            .sync(&[
                table("users", &["id"], db_source(Some(1), Some(2))),
                table("orders", &["id"], db_source(Some(1), Some(2))),
            ])
            .unwrap();
        // Persist embeddings for both live objects.
        store
            .upsert_embedding("dbo.users", "h1", "stub-hash-v1", &[0.1, 0.2])
            .unwrap();
        store
            .upsert_embedding("dbo.orders", "h2", "stub-hash-v1", &[0.3, 0.4])
            .unwrap();
        assert_eq!(store.embedding_count().unwrap(), 2);

        // Tombstone "orders" by syncing without it.
        store
            .sync(&[table("users", &["id"], db_source(Some(1), Some(2)))])
            .unwrap();

        // The tombstoned object's embedding must be removed, not leaked.
        assert_eq!(store.embedding_count().unwrap(), 1);
        assert!(store.embedding_for("dbo.orders").unwrap().is_none());
        assert!(store.embedding_for("dbo.users").unwrap().is_some());
    }

    #[test]
    fn live_embedding_count_is_scoped_to_active_provider() {
        let store = CatalogStore::open_in_memory().unwrap();
        store
            .sync(&[table("users", &["id"], db_source(Some(1), Some(2)))])
            .unwrap();
        store
            .upsert_embedding("dbo.users", "h1", "stub-hash-v1", &[0.1, 0.2])
            .unwrap();
        // The active provider sees its embedding; a different provider sees none,
        // so it will not trigger no-op hybrid search over mismatched vectors.
        assert_eq!(store.live_embedding_count("stub-hash-v1").unwrap(), 1);
        assert_eq!(store.live_embedding_count("real-model-v2").unwrap(), 0);
    }

    #[test]
    fn all_embeddings_filters_by_provider() {
        let store = CatalogStore::open_in_memory().unwrap();
        store
            .sync(&[
                table("users", &["id"], db_source(Some(1), Some(2))),
                table("orders", &["id"], db_source(Some(1), Some(2))),
            ])
            .unwrap();
        store
            .upsert_embedding("dbo.users", "h1", "stub-hash-v1", &[0.1, 0.2])
            .unwrap();
        store
            .upsert_embedding("dbo.orders", "h2", "real-model-v2", &[0.3, 0.4])
            .unwrap();

        let stub = store.all_embeddings("stub-hash-v1").unwrap();
        assert_eq!(stub.len(), 1);
        assert_eq!(stub[0].object_key, "dbo.users");

        let real = store.all_embeddings("real-model-v2").unwrap();
        assert_eq!(real.len(), 1);
        assert_eq!(real[0].object_key, "dbo.orders");
    }
}
