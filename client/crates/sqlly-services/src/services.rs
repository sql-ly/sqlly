use std::sync::Arc;

use crate::{
    admin_tools::{AdminToolsService, DefaultAdminToolsService},
    catalog::{CatalogService, DefaultCatalogService},
    connection::ConnectionManager,
    intellisense::{DefaultIntellisense, IntellisenseService},
    metadata::MetadataService,
    query::QueryExecutor,
    row_editing::RowEditingService,
};
#[cfg(not(target_arch = "wasm32"))]
use crate::{
    connection::DefaultConnectionManager, metadata::DefaultMetadataService,
    query::DefaultQueryExecutor, row_editing::DefaultRowEditingService,
};

/// The headless facade every frontend holds. Native applications should use
/// [`Services::from_env`] so the catalog receives the configured AI providers;
/// [`Services::new`] remains a deterministic stub-provider constructor for
/// tests. Use [`Services::from_parts`] to substitute individual ports.
///
/// Each field is a trait object behind an `Arc`, so `Services` is `Clone` and
/// cheap to share across GPUI views / tasks. The GPUI app calls these methods
/// directly with native types — no serialization between UI and core.
#[derive(Clone)]
pub struct Services {
    pub connections: Arc<dyn ConnectionManager>,
    pub intellisense: Arc<dyn IntellisenseService>,
    pub queries: Arc<dyn QueryExecutor>,
    pub catalog: Arc<dyn CatalogService>,
    pub metadata: Arc<dyn MetadataService>,
    pub admin_tools: Arc<dyn AdminToolsService>,
    pub row_editing: Arc<dyn RowEditingService>,
}

impl Services {
    /// Build the native facade around an explicit catalog service — the seam
    /// the app uses to install a cloud-backed LLM provider chosen in
    /// preferences.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn from_catalog(catalog: Arc<dyn CatalogService>) -> Self {
        Self::native_with_catalog(catalog)
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn native_with_catalog(catalog: Arc<dyn CatalogService>) -> Self {
        // One warm-client pool shared by the query executor and row editing
        // (separate lanes on the same map) so the idle reaper and profile
        // eviction cover every warm TDS client the app holds.
        let pool = crate::tds_pool::TdsPool::new();
        let queries: Arc<dyn QueryExecutor> =
            Arc::new(DefaultQueryExecutor::with_pool(pool.clone()));
        Self {
            connections: Arc::new(DefaultConnectionManager::new()),
            intellisense: Arc::new(DefaultIntellisense),
            queries: queries.clone(),
            catalog,
            metadata: Arc::new(DefaultMetadataService::new(queries)),
            admin_tools: Arc::new(DefaultAdminToolsService),
            row_editing: Arc::new(DefaultRowEditingService::new(pool)),
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn new() -> Self {
        Self::native_with_catalog(Arc::new(DefaultCatalogService::new()))
    }

    /// Build the native service facade with the same environment-selected AI
    /// provider factory as the CLI engine. Provider failures degrade to the
    /// deterministic stubs and are returned as diagnostics rather than
    /// preventing the application from starting.
    #[cfg(not(target_arch = "wasm32"))]
    pub async fn from_env() -> (Self, crate::providers::ProviderDiagnostics) {
        let (catalog, diagnostics) = DefaultCatalogService::from_env().await;
        (Self::native_with_catalog(Arc::new(catalog)), diagnostics)
    }

    /// Browser wiring: same facade, same intellisense/catalog/admin logic,
    /// but queries, connections, and metadata answer from the in-tab
    /// backends (`crate::browser`: SQLite/fakedb, or PGlite for
    /// Postgres-wire profiles) instead of real sockets, and row editing
    /// reports itself unavailable. Frontend code calls `Services::new()`
    /// identically on every platform.
    #[cfg(target_arch = "wasm32")]
    pub fn new() -> Self {
        let queries: Arc<dyn QueryExecutor> = Arc::new(crate::browser::BrowserQueryExecutor::new());
        Self {
            connections: Arc::new(crate::browser::BrowserConnectionManager),
            intellisense: Arc::new(DefaultIntellisense),
            queries: queries.clone(),
            catalog: Arc::new(DefaultCatalogService::new()),
            metadata: Arc::new(crate::browser::BrowserMetadataRouter::new(queries)),
            admin_tools: Arc::new(DefaultAdminToolsService),
            row_editing: Arc::new(crate::browser::BrowserRowEditingService),
        }
    }

    pub fn from_parts(
        connections: Arc<dyn ConnectionManager>,
        intellisense: Arc<dyn IntellisenseService>,
        queries: Arc<dyn QueryExecutor>,
        catalog: Arc<dyn CatalogService>,
        metadata: Arc<dyn MetadataService>,
        admin_tools: Arc<dyn AdminToolsService>,
        row_editing: Arc<dyn RowEditingService>,
    ) -> Self {
        Self {
            connections,
            intellisense,
            queries,
            catalog,
            metadata,
            admin_tools,
            row_editing,
        }
    }
}

impl Default for Services {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use crate::providers::{EMBEDDING_PROVIDER_ENV, LLM_PROVIDER_ENV};

    #[tokio::test]
    async fn from_env_wires_selected_catalog_providers() {
        let mut env = sqlly_testkit::EnvGuard::acquire();
        env.set(LLM_PROVIDER_ENV, "stub");
        env.set(EMBEDDING_PROVIDER_ENV, "stub");

        let (services, diagnostics) = Services::from_env().await;
        assert_eq!(diagnostics.llm_requested.as_deref(), Some("stub"));
        assert_eq!(diagnostics.llm_active, "stub");
        assert_eq!(diagnostics.embedding_requested.as_deref(), Some("stub"));
        assert_eq!(diagnostics.embedding_active, "stub");

        let status = services.catalog.provider_status().await.unwrap();
        assert_eq!(status.llm_requested.as_deref(), Some("stub"));
        assert_eq!(status.llm_active, "stub");
        assert_eq!(status.embedding_requested.as_deref(), Some("stub"));
        assert_eq!(status.embedding_active, "stub");
    }
}
