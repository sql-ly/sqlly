//! Vector store abstraction.
//!
//! The catalog never calls a specific vector database directly; it talks
//! through [`VectorStore`]. This lets us swap between SQLite (brute-force
//! compatibility), LanceDB (production), and future backends without touching
//! application code.

use async_trait::async_trait;

/// One item to upsert into the vector store.
#[derive(Debug, Clone)]
pub struct VectorItem {
    pub object_key: String,
    pub vector: Vec<f32>,
    pub provider_id: String,
    pub model_id: String,
    pub dimensions: usize,
    pub content_hash: String,
    pub source_type: String,
    pub database: Option<String>,
    pub schema: Option<String>,
    pub table: Option<String>,
    pub column: Option<String>,
    pub object_type: Option<String>,
    pub file_path: Option<String>,
    pub modified_ts: Option<i64>,
    pub workspace_id: Option<String>,
    pub project_id: Option<String>,
    pub tenant_id: Option<String>,
    pub metadata_json: String,
}

/// One hit from vector similarity search.
#[derive(Debug, Clone)]
pub struct VectorHit {
    pub object_key: String,
    pub score: f32,
    pub provider_id: String,
    pub model_id: String,
    pub metadata_json: String,
}

/// Filter applied to vector search.
#[derive(Debug, Clone, Default)]
pub struct VectorFilter {
    pub source_type: Option<String>,
    pub database: Option<String>,
    pub schema: Option<String>,
    pub table: Option<String>,
    pub column: Option<String>,
    pub object_type: Option<String>,
    pub file_path: Option<String>,
    pub modified_after: Option<i64>,
    pub modified_before: Option<i64>,
    pub workspace_id: Option<String>,
    pub project_id: Option<String>,
    pub tenant_id: Option<String>,
}

/// Why a vector-store operation failed. The crate's one typed error for the
/// [`VectorStore`] contract — the rest of `sqlly-catalog` is `thiserror`-based
/// and this trait was the lone `anyhow` holdout.
#[derive(Debug, thiserror::Error)]
pub enum VectorStoreError {
    /// The store's backing storage failed (I/O, SQL, serialization).
    #[error("vector store storage error: {0}")]
    Storage(String),
    /// The request asked for something this store cannot do.
    #[error("{0}")]
    Unsupported(String),
}

impl VectorStoreError {
    pub fn message(&self) -> sqlly_lang::DisplayMessage {
        match self {
            Self::Storage(detail) => sqlly_lang::DisplayMessage::new(
                "services.ai.vector_store.storage",
                self.to_string(),
            )
            .with_arg("detail", detail),
            Self::Unsupported(detail) => sqlly_lang::DisplayMessage::raw(detail),
        }
    }
}

impl From<rusqlite::Error> for VectorStoreError {
    fn from(e: rusqlite::Error) -> Self {
        VectorStoreError::Storage(e.to_string())
    }
}

impl From<crate::catalog::store::CatalogError> for VectorStoreError {
    fn from(e: crate::catalog::store::CatalogError) -> Self {
        VectorStoreError::Storage(e.to_string())
    }
}

/// The contract for anything that can store and search vectors.
#[async_trait]
pub trait VectorStore: Send + Sync {
    /// Insert or update items in the vector store.
    async fn upsert(&self, items: Vec<VectorItem>) -> Result<(), VectorStoreError>;

    /// Search for nearest neighbors of `vector`, returning up to `limit` hits.
    async fn search(
        &self,
        vector: Vec<f32>,
        limit: usize,
        filter: Option<VectorFilter>,
    ) -> Result<Vec<VectorHit>, VectorStoreError>;

    /// Delete items by their object keys.
    async fn delete(&self, keys: &[String]) -> Result<(), VectorStoreError>;

    /// Count items matching `filter`.
    async fn count(&self, filter: Option<VectorFilter>) -> Result<usize, VectorStoreError>;
}

/// Type-erased vector store handle.
pub type DynVectorStore = std::sync::Arc<dyn VectorStore>;

impl VectorFilter {
    pub fn for_scope(_store: &crate::catalog::store::CatalogStore) -> Self {
        Self::default()
    }
}
