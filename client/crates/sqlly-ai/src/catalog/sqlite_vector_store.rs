//! SQLite-backed compatibility implementation of [`VectorStore`].
//!
//! This wraps the existing brute-force SQLite vector search in
//! [`crate::catalog::vector`] so the old code path continues to work while the new
//! [`VectorStore`] trait is introduced. Eventually this will become a fallback
//! or be removed in favor of LanceDB.

use async_trait::async_trait;

use crate::catalog::store as catalog_meta;
use crate::catalog::store::CatalogStore;
use crate::catalog::vector_store::{
    VectorFilter, VectorHit, VectorItem, VectorStore, VectorStoreError,
};

/// [`VectorStore`] backed by the existing SQLite `catalog_embedding` table.
pub struct SqliteVectorStore {
    store: std::sync::Arc<CatalogStore>,
    active_provider_id: String,
}

impl SqliteVectorStore {
    pub fn new(store: CatalogStore) -> Self {
        Self::with_arc(std::sync::Arc::new(store))
    }

    pub fn with_provider(store: CatalogStore, provider_id: impl Into<String>) -> Self {
        Self::with_arc_provider(std::sync::Arc::new(store), provider_id)
    }

    pub fn with_arc(store: std::sync::Arc<CatalogStore>) -> Self {
        let active_provider_id = String::new();
        Self {
            store,
            active_provider_id,
        }
    }

    pub fn with_arc_provider(
        store: std::sync::Arc<CatalogStore>,
        provider_id: impl Into<String>,
    ) -> Self {
        Self {
            store,
            active_provider_id: provider_id.into(),
        }
    }

    pub fn set_provider_id(&mut self, provider_id: impl Into<String>) {
        self.active_provider_id = provider_id.into();
    }
}

#[async_trait]
impl VectorStore for SqliteVectorStore {
    async fn upsert(&self, items: Vec<VectorItem>) -> Result<(), VectorStoreError> {
        if items.is_empty() {
            return Ok(());
        }
        let rows: Vec<(&str, &str, &str, &[f32])> = items
            .iter()
            .map(|item| {
                (
                    item.object_key.as_str(),
                    item.content_hash.as_str(),
                    item.provider_id.as_str(),
                    item.vector.as_slice(),
                )
            })
            .collect();
        self.store.upsert_embeddings_batch(&rows)?;
        Ok(())
    }

    async fn search(
        &self,
        vector: Vec<f32>,
        limit: usize,
        filter: Option<VectorFilter>,
    ) -> Result<Vec<VectorHit>, VectorStoreError> {
        let filter = filter.unwrap_or_default();
        ensure_supported_filter(&filter)?;
        let all = self
            .store
            .all_embeddings_with_meta(&self.active_provider_id)?;
        let mut hits: Vec<VectorHit> = all
            .into_iter()
            .filter(|(stored, meta)| {
                stored.embedding.len() == vector.len() && filter_matches(&filter, meta)
            })
            .map(|(stored, _)| {
                let score = crate::catalog::vector::cosine_similarity(&vector, &stored.embedding);
                VectorHit {
                    object_key: stored.object_key,
                    score,
                    provider_id: self.active_provider_id.clone(),
                    model_id: String::new(),
                    metadata_json: String::new(),
                }
            })
            .collect();
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        hits.truncate(limit);
        Ok(hits)
    }

    async fn delete(&self, keys: &[String]) -> Result<(), VectorStoreError> {
        self.store.delete_embeddings(keys)?;
        Ok(())
    }

    async fn count(&self, filter: Option<VectorFilter>) -> Result<usize, VectorStoreError> {
        let filter = filter.unwrap_or_default();
        ensure_supported_filter(&filter)?;
        let source_type = filter
            .source_type
            .as_deref()
            .or(filter.object_type.as_deref());
        let database = filter.database.as_deref();
        let schema = filter.schema.as_deref();
        if source_type.is_none() && database.is_none() && schema.is_none() {
            return Ok(self.store.live_embedding_count(&self.active_provider_id)?);
        }
        Ok(self.store.count_embeddings_filtered(
            &self.active_provider_id,
            source_type,
            database,
            schema,
        )?)
    }
}

/// The supported filter dimensions, matched against the object metadata
/// joined from `catalog_object`: kind (`source_type`/`object_type`), database
/// (`catalog`), and schema (`namespace`). Identifier comparison is
/// case-insensitive, matching the catalog's own lookups.
fn filter_matches(filter: &VectorFilter, meta: &catalog_meta::EmbeddingObjectMeta) -> bool {
    let kind_wanted = filter
        .source_type
        .as_deref()
        .or(filter.object_type.as_deref());
    if !kind_wanted.is_none_or(|w| w.eq_ignore_ascii_case(&meta.kind)) {
        return false;
    }
    if let Some(database) = filter.database.as_deref()
        && !meta
            .catalog
            .as_deref()
            .is_some_and(|c| c.eq_ignore_ascii_case(database))
    {
        return false;
    }
    if let Some(schema) = filter.schema.as_deref()
        && !meta
            .namespace
            .as_deref()
            .is_some_and(|n| n.eq_ignore_ascii_case(schema))
    {
        return false;
    }
    true
}

/// This backend persists object kind, database, and schema; any other filter
/// dimension would be silently wrong, so it is rejected explicitly instead of
/// ignored.
fn ensure_supported_filter(filter: &VectorFilter) -> Result<(), VectorStoreError> {
    let unsupported = [
        ("table", filter.table.is_some()),
        ("column", filter.column.is_some()),
        ("file_path", filter.file_path.is_some()),
        ("modified_after", filter.modified_after.is_some()),
        ("modified_before", filter.modified_before.is_some()),
        ("workspace_id", filter.workspace_id.is_some()),
        ("project_id", filter.project_id.is_some()),
        ("tenant_id", filter.tenant_id.is_some()),
    ];
    if let Some((name, _)) = unsupported.iter().find(|(_, set)| *set) {
        return Err(VectorStoreError::Unsupported(format!(
            "the SQLite vector store does not support filtering by {name}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::store::CatalogStore;

    #[tokio::test]
    async fn sqlite_vector_store_round_trip() {
        use crate::catalog::wire::{
            CatalogColumn, CatalogObject, CatalogObjectKind, CatalogSource, SourceKind,
        };

        let store = CatalogStore::open_in_memory().unwrap();
        // Sync an object so the embedding JOIN finds it.
        store
            .sync(&[CatalogObject {
                schema: Some("dbo".into()),
                name: "users".into(),
                kind: CatalogObjectKind::Table,
                columns: vec![CatalogColumn {
                    name: "id".into(),
                    type_name: Some("INT".into()),
                    ordinal: 1,
                    nullable: false,
                }],
                primary_key: vec![],
                foreign_keys: vec![],
                parameters: vec![],
                definition: None,
                source: CatalogSource {
                    kind: SourceKind::Database,
                    path: None,
                    create_date: Some(1),
                    modify_date: Some(2),
                },
            }])
            .unwrap();
        let vs = SqliteVectorStore::with_provider(store, "stub");

        let item = VectorItem {
            object_key: "dbo.users".to_string(),
            vector: vec![1.0f32, 0.0, 0.0],
            provider_id: "stub".to_string(),
            model_id: "stub".to_string(),
            dimensions: 3,
            content_hash: "abc".to_string(),
            source_type: "table".to_string(),
            database: None,
            schema: Some("dbo".to_string()),
            table: Some("users".to_string()),
            column: None,
            object_type: None,
            file_path: None,
            modified_ts: None,
            workspace_id: None,
            project_id: None,
            tenant_id: None,
            metadata_json: String::new(),
        };

        vs.upsert(vec![item]).await.unwrap();
        let count = vs.count(None).await.unwrap();
        assert_eq!(count, 1);

        let hits = vs.search(vec![1.0f32, 0.0, 0.0], 10, None).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].object_key, "dbo.users");
        assert!((hits[0].score - 1.0).abs() < 1e-6);
    }

    /// Filter dimensions this backend does not persist must error loudly on
    /// both `search` and `count` — silently ignoring one would return results
    /// the caller believes were filtered.
    #[tokio::test]
    async fn unsupported_filter_dimensions_error_instead_of_silently_matching() {
        let store = CatalogStore::open_in_memory().unwrap();
        let vs = SqliteVectorStore::with_provider(store, "stub");

        let filter = VectorFilter {
            table: Some("users".to_string()),
            ..VectorFilter::default()
        };
        let err = vs
            .search(vec![1.0f32, 0.0, 0.0], 10, Some(filter.clone()))
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("does not support filtering by table"),
            "{err}"
        );
        let err = vs.count(Some(filter)).await.unwrap_err();
        assert!(
            err.to_string()
                .contains("does not support filtering by table"),
            "{err}"
        );

        // The supported dimension still passes through the same gate.
        let supported = VectorFilter {
            source_type: Some("table".to_string()),
            ..VectorFilter::default()
        };
        assert_eq!(vs.count(Some(supported)).await.unwrap(), 0);
    }

    /// `source_type` filters by the catalog object's kind (it used to be
    /// compared against the object key, matching nothing), and `count`
    /// honors the same filter (it used to ignore it).
    #[tokio::test]
    async fn source_type_filter_matches_object_kind() {
        use crate::catalog::wire::{
            CatalogColumn, CatalogObject, CatalogObjectKind, CatalogSource, SourceKind,
        };

        let store = CatalogStore::open_in_memory().unwrap();
        let object = |name: &str, kind| CatalogObject {
            schema: Some("dbo".into()),
            name: name.into(),
            kind,
            columns: vec![CatalogColumn {
                name: "id".into(),
                type_name: Some("INT".into()),
                ordinal: 1,
                nullable: false,
            }],
            primary_key: vec![],
            foreign_keys: vec![],
            parameters: vec![],
            definition: None,
            source: CatalogSource {
                kind: SourceKind::Database,
                path: None,
                create_date: Some(1),
                modify_date: Some(2),
            },
        };
        store
            .sync(&[
                object("users", CatalogObjectKind::Table),
                object("get_users", CatalogObjectKind::StoredProcedure),
            ])
            .unwrap();
        let vs = SqliteVectorStore::with_provider(store, "stub");
        let item = |key: &str, kind: &str| VectorItem {
            object_key: key.to_string(),
            vector: vec![1.0f32, 0.0, 0.0],
            provider_id: "stub".to_string(),
            model_id: "stub".to_string(),
            dimensions: 3,
            content_hash: "abc".to_string(),
            source_type: kind.to_string(),
            database: None,
            schema: Some("dbo".to_string()),
            table: None,
            column: None,
            object_type: None,
            file_path: None,
            modified_ts: None,
            workspace_id: None,
            project_id: None,
            tenant_id: None,
            metadata_json: String::new(),
        };
        vs.upsert(vec![
            item("dbo.users", "table"),
            item("dbo.get_users", "procedure"),
        ])
        .await
        .unwrap();

        let table_filter = VectorFilter {
            source_type: Some("table".to_string()),
            ..VectorFilter::default()
        };
        let hits = vs
            .search(vec![1.0f32, 0.0, 0.0], 10, Some(table_filter.clone()))
            .await
            .unwrap();
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].object_key, "dbo.users");

        assert_eq!(vs.count(Some(table_filter)).await.unwrap(), 1);
        assert_eq!(vs.count(None).await.unwrap(), 2);

        // Unsupported dimensions fail loudly instead of silently matching all.
        let bad = VectorFilter {
            table: Some("users".to_string()),
            ..VectorFilter::default()
        };
        assert!(vs.count(Some(bad)).await.is_err());

        // Schema and database filters match the object metadata joined from
        // the catalog. The synced objects live in schema "dbo" with no
        // catalog, so a schema hit matches and a database filter matches
        // nothing (honestly — not everything).
        let schema_filter = VectorFilter {
            schema: Some("DBO".to_string()),
            ..VectorFilter::default()
        };
        assert_eq!(
            vs.count(Some(schema_filter)).await.unwrap(),
            2,
            "schema filter is case-insensitive"
        );
        let other_schema = VectorFilter {
            schema: Some("sales".to_string()),
            ..VectorFilter::default()
        };
        assert_eq!(vs.count(Some(other_schema)).await.unwrap(), 0);
        let database_filter = VectorFilter {
            database: Some("appdb".to_string()),
            ..VectorFilter::default()
        };
        assert_eq!(
            vs.count(Some(database_filter)).await.unwrap(),
            0,
            "objects without a catalog never match a database filter"
        );
    }

    #[tokio::test]
    async fn count_scopes_by_provider_without_loading_blobs() {
        use crate::catalog::wire::{
            CatalogColumn, CatalogObject, CatalogObjectKind, CatalogSource, SourceKind,
        };

        let store = std::sync::Arc::new(CatalogStore::open_in_memory().unwrap());
        let object = |name: &str| CatalogObject {
            schema: Some("dbo".into()),
            name: name.into(),
            kind: CatalogObjectKind::Table,
            columns: vec![CatalogColumn {
                name: "id".into(),
                type_name: Some("INT".into()),
                ordinal: 1,
                nullable: false,
            }],
            primary_key: vec![],
            foreign_keys: vec![],
            parameters: vec![],
            definition: None,
            source: CatalogSource {
                kind: SourceKind::Database,
                path: None,
                create_date: Some(1),
                modify_date: Some(2),
            },
        };
        store.sync(&[object("users"), object("orders")]).unwrap();
        store
            .upsert_embedding("dbo.users", "h1", "stub", &[0.1, 0.2])
            .unwrap();
        store
            .upsert_embedding("dbo.orders", "h2", "other", &[0.3, 0.4])
            .unwrap();
        assert_eq!(store.embedding_count().unwrap(), 2);

        let vs_stub = SqliteVectorStore::with_arc_provider(store.clone(), "stub");
        let vs_other = SqliteVectorStore::with_arc_provider(store, "other");
        assert_eq!(vs_stub.count(None).await.unwrap(), 1);
        assert_eq!(vs_other.count(None).await.unwrap(), 1);
    }
}
