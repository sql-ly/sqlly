//! A [`Services`] bundle for a `ViaRelay` profile whose relay session is NOT
//! established: every engine-backed operation fails fast with one typed
//! [`StudioError::Connection`] carrying the caller-supplied reason, instead of
//! silently attempting a direct database connection (which is unreachable by
//! definition for a relay-routed profile).
//!
//! Client-side ports keep the documented delegation design: intellisense and
//! admin tools are the in-process defaults (pure, no engine), metadata and
//! connection probes compose over the erroring query executor so they surface
//! the same reason, and row editing reports the existing relay gap.

use std::sync::Arc;

use crate::catalog::CatalogService;
use crate::{
    DefaultAdminToolsService, DefaultIntellisense, DefaultMetadataService, QueryExecutor,
    QueryHandle, Services,
};
use async_trait::async_trait;
use sqlly_ai::catalog::{
    CatalogObject, EmbeddingBuildReport, JoinPath, JoinPathOptions, LlmResponse, NukeAiDataReport,
    SchemaPurpose, SchemaSearchHit, SqlGenerationResult, SqlValidationResult, SyncReport,
};
use sqlly_core::{ConnectionProfile, StudioError, StudioResult};
use sqlly_lang::sqlmodel::Dialect;

use crate::remote::connection::RemoteConnectionManager;
use crate::remote::row_editing::UnavailableRowEditingService;

/// Build the erroring bundle. `reason` is the exact user-facing message every
/// engine-backed call returns (e.g. "connect the relay session first" copy).
#[must_use]
pub fn unavailable_services(reason: impl Into<String>) -> Services {
    let reason: Arc<str> = Arc::from(reason.into());
    let queries: Arc<dyn QueryExecutor> = Arc::new(UnavailableQueryExecutor {
        reason: Arc::clone(&reason),
    });
    Services::from_parts(
        // Connection probes and metadata compose over the erroring executor,
        // so Test Connection / db lists / schema loads all surface `reason`.
        Arc::new(RemoteConnectionManager::new(Arc::clone(&queries))),
        Arc::new(DefaultIntellisense),
        Arc::clone(&queries),
        Arc::new(UnavailableCatalogService {
            reason: Arc::clone(&reason),
        }),
        Arc::new(DefaultMetadataService::new(Arc::clone(&queries))),
        Arc::new(DefaultAdminToolsService),
        Arc::new(UnavailableRowEditingService),
    )
}

struct UnavailableQueryExecutor {
    reason: Arc<str>,
}

impl UnavailableQueryExecutor {
    fn err<T>(&self) -> StudioResult<T> {
        Err(StudioError::Connection(self.reason.to_string()))
    }
}

#[async_trait]
impl QueryExecutor for UnavailableQueryExecutor {
    async fn execute(
        &self,
        _profile: &ConnectionProfile,
        _sql: &str,
        _row_buffer: usize,
    ) -> StudioResult<QueryHandle> {
        self.err()
    }

    async fn cancel(&self, _task_id: &str) -> StudioResult<()> {
        self.err()
    }
}

struct UnavailableCatalogService {
    reason: Arc<str>,
}

impl UnavailableCatalogService {
    fn err<T>(&self) -> StudioResult<T> {
        Err(StudioError::Connection(self.reason.to_string()))
    }
}

#[async_trait]
impl CatalogService for UnavailableCatalogService {
    async fn sync_catalog(
        &self,
        _profile: &ConnectionProfile,
        _objects: Vec<CatalogObject>,
        _coverage: sqlly_ai::catalog::SyncCoverage,
    ) -> StudioResult<SyncReport> {
        self.err()
    }

    async fn catalog_maintenance(
        &self,
        _profile: &ConnectionProfile,
        _options: sqlly_ai::catalog::MaintenanceOptions,
    ) -> StudioResult<sqlly_ai::catalog::MaintenanceReport> {
        self.err()
    }

    async fn review_sql(
        &self,
        _profile: &ConnectionProfile,
        _sql: String,
        _dialect: Option<String>,
    ) -> StudioResult<sqlly_ai::catalog::SqlReviewReport> {
        self.err()
    }

    async fn summarize_query(
        &self,
        _profile: &ConnectionProfile,
        _sql: String,
        _result_columns: Vec<String>,
        _row_count: Option<u64>,
        _elapsed_ms: Option<u64>,
    ) -> StudioResult<sqlly_ai::catalog::QuerySummaryReport> {
        self.err()
    }

    #[cfg(not(target_arch = "wasm32"))]
    async fn provider_status(&self) -> StudioResult<crate::providers::ProviderStatusReport> {
        self.err()
    }

    async fn build_ai_cache(&self, _profile: &ConnectionProfile) -> StudioResult<()> {
        self.err()
    }

    async fn invalidate_ai_cache(
        &self,
        _profile: &ConnectionProfile,
    ) -> StudioResult<NukeAiDataReport> {
        self.err()
    }

    async fn build_embeddings(
        &self,
        _profile: &ConnectionProfile,
    ) -> StudioResult<EmbeddingBuildReport> {
        self.err()
    }

    async fn ask_schema(
        &self,
        _profile: &ConnectionProfile,
        _question: &str,
        _max_tokens: Option<usize>,
        _system: Option<String>,
        _purpose: SchemaPurpose,
    ) -> StudioResult<LlmResponse> {
        self.err()
    }

    async fn search_schema(
        &self,
        _profile: &ConnectionProfile,
        _query: &str,
        _limit: Option<usize>,
        _object_types: Vec<String>,
    ) -> StudioResult<Vec<SchemaSearchHit>> {
        self.err()
    }

    async fn find_join_path(
        &self,
        _profile: &ConnectionProfile,
        _source: &str,
        _target: &str,
        _options: JoinPathOptions,
    ) -> StudioResult<Vec<JoinPath>> {
        self.err()
    }

    async fn generate_sql(
        &self,
        _profile: &ConnectionProfile,
        _question: &str,
        _dialect: Dialect,
        _max_tokens: Option<usize>,
        _system: Option<String>,
        _max_attempts: Option<usize>,
    ) -> StudioResult<SqlGenerationResult> {
        self.err()
    }

    async fn validate_sql(
        &self,
        _profile: &ConnectionProfile,
        _sql: &str,
        _dialect: Dialect,
    ) -> StudioResult<SqlValidationResult> {
        self.err()
    }

    fn llm_provider(&self) -> sqlly_ai::catalog::DynLlmProvider {
        Arc::new(sqlly_ai::catalog::StubLlmProvider::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::{SecretProvider, SecretRef};

    const REASON: &str = "connect the relay session first";

    fn profile() -> ConnectionProfile {
        ConnectionProfile::sql_login(
            "p",
            "localhost",
            "sa",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: "pw".to_string(),
            },
        )
    }

    fn assert_reason<T>(result: StudioResult<T>) {
        match result {
            Err(StudioError::Connection(message)) => assert_eq!(message, REASON),
            Err(other) => panic!("expected Connection({REASON:?}), got {other:?}"),
            Ok(_) => panic!("expected Connection({REASON:?}), got Ok"),
        }
    }

    /// Every engine-backed port must fail with the exact reason — never dial
    /// anything. Test Connection, execution, validation, and schema loads are
    /// the paths the app actually exercises against this bundle.
    #[tokio::test]
    async fn engine_backed_ports_fail_with_the_reason() {
        let services = unavailable_services(REASON);
        let profile = profile();

        assert_reason(services.queries.execute(&profile, "SELECT 1", 8).await);
        assert_reason(services.connections.test_connection(&profile).await);
        assert_reason(services.connections.list_databases(&profile).await);
        assert_reason(
            services
                .catalog
                .validate_sql(&profile, "SELECT 1", Dialect::SqlServer)
                .await,
        );
        assert_reason(services.metadata.list_schemas(&profile, "master").await);
    }

    /// Client-side ports keep working: admin tools generate SQL text without
    /// an engine, matching the delegation design of the live remote bundle.
    #[test]
    fn client_side_ports_stay_functional() {
        let services = unavailable_services(REASON);
        assert!(!services.admin_tools.templates_list().is_empty());
    }
}
