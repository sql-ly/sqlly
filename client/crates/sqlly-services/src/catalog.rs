use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use sqlly_ai::catalog::{
    BuildReport, CatalogColumn, CatalogForeignKey, CatalogObject, CatalogObjectKind,
    CatalogParameter, CatalogScope, CatalogSource, CatalogStore, DEFAULT_CONTEXT_TOKENS,
    DynEmbeddingProvider, DynLlmProvider, DynVectorStore, EmbeddingBuildOptions,
    EmbeddingBuildReport, JoinPath, JoinPathOptions, LlmResponse, MaintenanceOptions,
    MaintenanceReport, NukeAiDataReport, QuerySummaryReport, SchemaPurpose, SchemaSearchHit,
    SearchFilter, SourceKind, SqlGenerationOptions, SqlGenerationResult, SqlReviewReport,
    SqlValidationOptions, SqlValidationResult, SqliteVectorStore, StubEmbeddingProvider,
    StubLlmProvider, SyncCoverage, SyncReport, ask_with_purpose, ask_with_purpose_focused,
    build_ai_cache, build_embedding_cache, find_join_paths_with_options, generate_sql, review_sql,
    search_store_objects_hybrid, summarize_query, validate_sql,
};
use sqlly_core::{ConnectionProfile, StudioError, StudioResult};
use sqlly_lang::sqlmodel::{Dialect, ModelSnapshot, ObjectBody, ObjectKind, ParamMode};

/// Port: persistent schema catalog + AI-assisted schema Q&A + SQL generation
/// and validation. Mirrors the `EngineRequest` catalog surface but returns
/// native catalog types — the GPUI app calls these directly.
///
/// The default impl is backed by `sqlly_ai::catalog::CatalogStore` (per-connection
/// SQLite) + stub LLM/embedding providers.
#[async_trait]
pub trait CatalogService: Send + Sync {
    /// `coverage` bounds what a missing object means: full-source syncs may
    /// tombstone anything unobserved, scoped syncs only what they covered,
    /// partial syncs nothing.
    async fn sync_catalog(
        &self,
        profile: &ConnectionProfile,
        objects: Vec<CatalogObject>,
        coverage: SyncCoverage,
    ) -> StudioResult<SyncReport>;
    async fn build_ai_cache(&self, profile: &ConnectionProfile) -> StudioResult<()>;
    async fn invalidate_ai_cache(
        &self,
        profile: &ConnectionProfile,
    ) -> StudioResult<NukeAiDataReport>;
    /// Run explicit catalog maintenance for the profile's catalog: tombstone
    /// purge, orphan-embedding cleanup, and search-index optimization, each
    /// opt-in via `options`.
    async fn catalog_maintenance(
        &self,
        profile: &ConnectionProfile,
        options: MaintenanceOptions,
    ) -> StudioResult<MaintenanceReport>;
    /// Catalog-grounded SQL review: deterministic validation plus a grounded
    /// model narrative.
    async fn review_sql(
        &self,
        profile: &ConnectionProfile,
        sql: String,
        dialect: Option<String>,
    ) -> StudioResult<SqlReviewReport>;
    /// Catalog-grounded plain-language summary of an executed query.
    async fn summarize_query(
        &self,
        profile: &ConnectionProfile,
        sql: String,
        result_columns: Vec<String>,
        row_count: Option<u64>,
        elapsed_ms: Option<u64>,
    ) -> StudioResult<QuerySummaryReport>;
    /// Point-in-time AI provider status: configured/active providers,
    /// fallback reasons, and a short-timeout runtime probe.
    #[cfg(not(target_arch = "wasm32"))]
    async fn provider_status(&self) -> StudioResult<crate::providers::ProviderStatusReport>;
    async fn build_embeddings(
        &self,
        profile: &ConnectionProfile,
    ) -> StudioResult<EmbeddingBuildReport>;
    async fn ask_schema(
        &self,
        profile: &ConnectionProfile,
        question: &str,
        max_tokens: Option<usize>,
        system: Option<String>,
        purpose: SchemaPurpose,
    ) -> StudioResult<LlmResponse>;
    async fn search_schema(
        &self,
        profile: &ConnectionProfile,
        query: &str,
        limit: Option<usize>,
        object_types: Vec<String>,
    ) -> StudioResult<Vec<SchemaSearchHit>>;
    async fn find_join_path(
        &self,
        profile: &ConnectionProfile,
        source: &str,
        target: &str,
        options: JoinPathOptions,
    ) -> StudioResult<Vec<JoinPath>>;
    async fn generate_sql(
        &self,
        profile: &ConnectionProfile,
        question: &str,
        dialect: Dialect,
        max_tokens: Option<usize>,
        system: Option<String>,
        max_attempts: Option<usize>,
    ) -> StudioResult<SqlGenerationResult>;
    async fn validate_sql(
        &self,
        profile: &ConnectionProfile,
        sql: &str,
        dialect: Dialect,
    ) -> StudioResult<SqlValidationResult>;
    /// Active LLM provider used for Ask/Generate/agent mode.
    fn llm_provider(&self) -> DynLlmProvider;

    /// Ask with caller-supplied object keys charged first against the token budget.
    /// Default ignores `focus_keys` so relay/remote implementers keep working.
    #[allow(clippy::too_many_arguments)]
    async fn ask_schema_focused(
        &self,
        profile: &ConnectionProfile,
        question: &str,
        _focus_keys: Vec<String>,
        max_tokens: Option<usize>,
        system: Option<String>,
        purpose: SchemaPurpose,
    ) -> StudioResult<LlmResponse> {
        self.ask_schema(profile, question, max_tokens, system, purpose)
            .await
    }

    /// Generate SQL with caller-supplied object keys charged first against the budget.
    /// Default ignores `focus_keys` so relay/remote implementers keep working.
    #[allow(clippy::too_many_arguments)]
    async fn generate_sql_focused(
        &self,
        profile: &ConnectionProfile,
        question: &str,
        _focus_keys: Vec<String>,
        dialect: Dialect,
        max_tokens: Option<usize>,
        system: Option<String>,
        max_attempts: Option<usize>,
    ) -> StudioResult<SqlGenerationResult> {
        self.generate_sql(profile, question, dialect, max_tokens, system, max_attempts)
            .await
    }
}

/// Real catalog service backed by per-connection `CatalogStore` (SQLite) +
/// stub LLM/embedding providers. Owns the catalog root directory and the
/// provider handles so the same providers are reused across calls.
/// Per-database tenant-scope policy the frontend resolves for a profile:
/// whether the missing-tenant-filter check runs, and (optionally) the exact
/// discriminator column names (folded/lowercase) instead of the built-in
/// convention list. The default is disabled: tenant scoping never surfaces
/// unless a database explicitly enables it.
#[derive(Clone, Debug)]
pub struct TenantScopePolicy {
    pub disabled: bool,
    /// When true, a missing tenant predicate is a blocking error (Required
    /// mode). When false with the guard enabled, it stays a warning (Warn).
    pub fail_closed: bool,
    pub tenant_column_folds: Option<Vec<String>>,
}

impl Default for TenantScopePolicy {
    fn default() -> Self {
        Self {
            disabled: true,
            fail_closed: false,
            tenant_column_folds: None,
        }
    }
}

/// Frontend-supplied resolver from a connection profile to its tenant-scope
/// policy. `None` (the default) leaves the check disabled.
pub type TenantScopeResolver = Arc<dyn Fn(&ConnectionProfile) -> TenantScopePolicy + Send + Sync>;

pub struct DefaultCatalogService {
    root: PathBuf,
    llm: DynLlmProvider,
    embedding: DynEmbeddingProvider,
    tenant_scope: Option<TenantScopeResolver>,
    #[cfg(not(target_arch = "wasm32"))]
    diagnostics: crate::providers::ProviderDiagnostics,
}

impl DefaultCatalogService {
    /// Construct with stub providers and the catalog root resolved from the
    /// environment (`$SQLLY_CATALOG_DIR` → platform data dir `…/SQLLY/Catalogs`
    /// → temp fallback), matching the engine's resolution.
    pub fn new() -> Self {
        Self {
            root: catalog_root(),
            llm: Arc::new(StubLlmProvider::new()),
            embedding: Arc::new(StubEmbeddingProvider::new()),
            tenant_scope: None,
            #[cfg(not(target_arch = "wasm32"))]
            diagnostics: crate::providers::ProviderDiagnostics::stub_default(),
        }
    }

    /// Construct with the providers selected by the environment (the same
    /// `SQLLY_LLM_PROVIDER` / `SQLLY_EMBEDDING_PROVIDER` selection the CLI
    /// engine uses), so a direct services consumer gets real local-AI
    /// providers instead of silently running on stubs. Falls back to stubs on
    /// any provider problem; the returned diagnostics say what happened.
    #[cfg(not(target_arch = "wasm32"))]
    pub async fn from_env() -> (Self, crate::providers::ProviderDiagnostics) {
        let (llm, embedding, _vector_store, diagnostics) =
            crate::providers::build_catalog_providers_from_env().await;
        (
            Self {
                root: catalog_root(),
                llm,
                embedding,
                tenant_scope: None,
                diagnostics: diagnostics.clone(),
            },
            diagnostics,
        )
    }

    /// Swap the LLM provider, keeping the root, embedding provider, and
    /// diagnostics. Used when the user routes app AI through a cloud
    /// provider chosen in preferences.
    pub fn with_llm(mut self, llm: DynLlmProvider) -> Self {
        self.llm = llm;
        self
    }

    /// Install the per-database tenant-scope policy resolver, consulted on
    /// every validation/review/generation call.
    pub fn with_tenant_scope_resolver(mut self, resolver: TenantScopeResolver) -> Self {
        self.tenant_scope = Some(resolver);
        self
    }

    fn tenant_policy(&self, profile: &ConnectionProfile) -> TenantScopePolicy {
        self.tenant_scope
            .as_ref()
            .map(|resolver| resolver(profile))
            .unwrap_or_default()
    }

    /// Provider-selection diagnostics (fallback notes, active provider ids).
    #[cfg(not(target_arch = "wasm32"))]
    pub fn diagnostics(&self) -> &crate::providers::ProviderDiagnostics {
        &self.diagnostics
    }

    /// Build stale AI cards and return the build report (wire protocol surface).
    pub async fn build_ai_cache_report(
        &self,
        profile: &ConnectionProfile,
    ) -> StudioResult<BuildReport> {
        let store = self.open_store(profile)?;
        build_ai_cache(&store, &self.llm)
            .await
            .map_err(|e| StudioError::Connection(e.to_string()))
    }

    /// Validate SQL with an explicit tenant-scope policy (engine wire tail).
    pub fn validate_sql_with_policy(
        &self,
        profile: &ConnectionProfile,
        sql: &str,
        dialect: Dialect,
        tenant: TenantScopePolicy,
    ) -> StudioResult<SqlValidationResult> {
        let store = self.open_store(profile)?;
        let options = validation_options(dialect, profile, tenant);
        validate_sql(&store, sql, &options).map_err(|e| StudioError::Connection(e.to_string()))
    }

    /// Generate SQL with an explicit tenant-scope policy (engine wire tail).
    #[allow(clippy::too_many_arguments)]
    pub async fn generate_sql_with_policy(
        &self,
        profile: &ConnectionProfile,
        question: &str,
        dialect: Dialect,
        max_tokens: Option<usize>,
        system: Option<String>,
        max_attempts: Option<usize>,
        tenant: TenantScopePolicy,
    ) -> StudioResult<SqlGenerationResult> {
        let store = self.open_store(profile)?;
        let options =
            generation_options(dialect, profile, max_tokens, system, max_attempts, tenant);
        generate_sql(&store, &self.llm, question, &options)
            .await
            .map_err(|e| StudioError::Connection(e.to_string()))
    }

    /// Review SQL with explicit tenant policy and token budget (engine wire).
    pub async fn review_sql_with_policy(
        &self,
        profile: &ConnectionProfile,
        sql: &str,
        dialect: Dialect,
        tenant: TenantScopePolicy,
        max_tokens: usize,
    ) -> StudioResult<SqlReviewReport> {
        let store = self.open_store(profile)?;
        let mut options = validation_options(dialect, profile, tenant);
        // An explicit AI safety review always reports unconstrained mutations,
        // whatever the editor's warning default is.
        options.flag_unconstrained_mutations = true;
        review_sql(&store, &self.llm, sql, &options, max_tokens)
            .await
            .map_err(|e| StudioError::Connection(format!("review sql: {e}")))
    }

    /// Summarize a query with an explicit context token budget (engine wire).
    pub async fn summarize_query_with_budget(
        &self,
        profile: &ConnectionProfile,
        sql: &str,
        result_columns: &[String],
        row_count: Option<u64>,
        elapsed_ms: Option<u64>,
        max_tokens: usize,
    ) -> StudioResult<QuerySummaryReport> {
        let store = self.open_store(profile)?;
        summarize_query(
            &store,
            &self.llm,
            sql,
            result_columns,
            row_count,
            elapsed_ms,
            max_tokens,
        )
        .await
        .map_err(|e| StudioError::Connection(format!("summarize query: {e}")))
    }

    /// Construct with explicit root + providers (for tests / injection).
    pub fn from_parts(root: PathBuf, llm: DynLlmProvider, embedding: DynEmbeddingProvider) -> Self {
        Self {
            root,
            llm,
            embedding,
            tenant_scope: None,
            #[cfg(not(target_arch = "wasm32"))]
            diagnostics: crate::providers::ProviderDiagnostics::stub_default(),
        }
    }

    pub fn open_store(&self, profile: &ConnectionProfile) -> StudioResult<CatalogStore> {
        let scope = CatalogScope {
            root: self.root.clone(),
            connection_fingerprint: connection_identity(profile),
            database: profile.database.clone(),
        };
        CatalogStore::open(&scope).map_err(|e| StudioError::Connection(e.to_string()))
    }

    pub fn llm_provider(&self) -> DynLlmProvider {
        Arc::clone(&self.llm)
    }
}

impl Default for DefaultCatalogService {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for DefaultCatalogService {
    fn clone(&self) -> Self {
        Self {
            root: self.root.clone(),
            llm: Arc::clone(&self.llm),
            embedding: Arc::clone(&self.embedding),
            tenant_scope: self.tenant_scope.clone(),
            #[cfg(not(target_arch = "wasm32"))]
            diagnostics: self.diagnostics.clone(),
        }
    }
}

#[async_trait]
impl CatalogService for DefaultCatalogService {
    async fn sync_catalog(
        &self,
        profile: &ConnectionProfile,
        objects: Vec<CatalogObject>,
        coverage: SyncCoverage,
    ) -> StudioResult<SyncReport> {
        let store = self.open_store(profile)?;
        store
            .sync_with_coverage(&objects, coverage)
            .map_err(|e| StudioError::Connection(e.to_string()))
    }

    async fn catalog_maintenance(
        &self,
        profile: &ConnectionProfile,
        options: MaintenanceOptions,
    ) -> StudioResult<MaintenanceReport> {
        let store = self.open_store(profile)?;
        store
            .maintain(&options)
            .map_err(|e| StudioError::Connection(e.to_string()))
    }

    async fn review_sql(
        &self,
        profile: &ConnectionProfile,
        sql: String,
        dialect: Option<String>,
    ) -> StudioResult<SqlReviewReport> {
        let dialect = parse_dialect(dialect.as_deref())
            .map_err(|e| StudioError::Connection(format!("invalid dialect: {e}")))?;
        self.review_sql_with_policy(
            profile,
            &sql,
            dialect,
            self.tenant_policy(profile),
            DEFAULT_CONTEXT_TOKENS,
        )
        .await
    }

    async fn summarize_query(
        &self,
        profile: &ConnectionProfile,
        sql: String,
        result_columns: Vec<String>,
        row_count: Option<u64>,
        elapsed_ms: Option<u64>,
    ) -> StudioResult<QuerySummaryReport> {
        let store = self.open_store(profile)?;
        sqlly_ai::catalog::summarize_query(
            &store,
            &self.llm,
            &sql,
            &result_columns,
            row_count,
            elapsed_ms,
            sqlly_ai::catalog::DEFAULT_CONTEXT_TOKENS,
        )
        .await
        .map_err(|e| StudioError::Connection(format!("summarize query: {e}")))
    }

    #[cfg(not(target_arch = "wasm32"))]
    async fn provider_status(&self) -> StudioResult<crate::providers::ProviderStatusReport> {
        Ok(crate::providers::provider_status(&self.diagnostics).await)
    }

    async fn build_ai_cache(&self, profile: &ConnectionProfile) -> StudioResult<()> {
        let store = self.open_store(profile)?;
        build_ai_cache(&store, &self.llm)
            .await
            .map_err(|e| StudioError::Connection(e.to_string()))?;
        Ok(())
    }

    async fn invalidate_ai_cache(
        &self,
        profile: &ConnectionProfile,
    ) -> StudioResult<NukeAiDataReport> {
        let store = self.open_store(profile)?;
        store
            .nuke_ai_data()
            .map_err(|e| StudioError::Connection(e.to_string()))
    }

    async fn build_embeddings(
        &self,
        profile: &ConnectionProfile,
    ) -> StudioResult<EmbeddingBuildReport> {
        let store = self.open_store(profile)?;
        let store = Arc::new(store);
        let vector_store: DynVectorStore = Arc::new(SqliteVectorStore::with_arc_provider(
            Arc::clone(&store),
            self.embedding.provider_id(),
        ));
        build_embedding_cache(
            &store,
            &self.embedding,
            EmbeddingBuildOptions::default(),
            Some(&vector_store),
        )
        .await
        .map_err(|e| StudioError::Connection(e.to_string()))
    }

    async fn ask_schema(
        &self,
        profile: &ConnectionProfile,
        question: &str,
        max_tokens: Option<usize>,
        system: Option<String>,
        purpose: SchemaPurpose,
    ) -> StudioResult<LlmResponse> {
        let store = self.open_store(profile)?;
        ask_with_purpose(
            &store,
            &self.llm,
            question,
            max_tokens.unwrap_or(DEFAULT_CONTEXT_TOKENS),
            system,
            purpose,
        )
        .await
        .map_err(|e| StudioError::Connection(e.to_string()))
    }

    #[allow(clippy::too_many_arguments)]
    async fn ask_schema_focused(
        &self,
        profile: &ConnectionProfile,
        question: &str,
        focus_keys: Vec<String>,
        max_tokens: Option<usize>,
        system: Option<String>,
        purpose: SchemaPurpose,
    ) -> StudioResult<LlmResponse> {
        let store = self.open_store(profile)?;
        ask_with_purpose_focused(
            &store,
            &self.llm,
            question,
            &focus_keys,
            max_tokens.unwrap_or(DEFAULT_CONTEXT_TOKENS),
            system,
            purpose,
        )
        .await
        .map_err(|e| StudioError::Connection(e.to_string()))
    }

    async fn search_schema(
        &self,
        profile: &ConnectionProfile,
        query: &str,
        limit: Option<usize>,
        object_types: Vec<String>,
    ) -> StudioResult<Vec<SchemaSearchHit>> {
        let store = self.open_store(profile)?;
        let limit = limit.unwrap_or(20);
        let filter = SearchFilter { object_types };

        // If embeddings exist for the active provider, use hybrid (FTS + vector);
        // otherwise fall back to FTS-only.
        let embedding_count = store
            .live_embedding_count(self.embedding.provider_id())
            .map_err(|e| StudioError::Connection(e.to_string()))?;

        if embedding_count > 0 {
            let store = Arc::new(store);
            let vector_store: DynVectorStore = Arc::new(SqliteVectorStore::with_arc_provider(
                Arc::clone(&store),
                self.embedding.provider_id(),
            ));
            let ranked = search_store_objects_hybrid(
                &store,
                query,
                limit,
                filter,
                Some(&self.embedding),
                Some(&vector_store),
            )
            .await
            .map_err(|e| StudioError::Connection(e.to_string()))?;
            let keys: Vec<String> = ranked.iter().map(|r| r.object.object_key.clone()).collect();
            let tiny_cards = store
                .tiny_cards_for_keys(&keys)
                .map_err(|e| StudioError::Connection(e.to_string()))?;
            let hits = ranked
                .into_iter()
                .map(|r| SchemaSearchHit {
                    object_key: r.object.object_key.clone(),
                    schema: r.object.namespace.clone(),
                    name: r.object.name.clone(),
                    object_type: r.object.kind.as_str().to_string(),
                    score: r.score,
                    tiny_card: tiny_cards.get(&r.object.object_key).cloned(),
                })
                .collect();
            Ok(hits)
        } else {
            store
                .search_schema(query, limit, filter)
                .map_err(|e| StudioError::Connection(e.to_string()))
        }
    }

    async fn find_join_path(
        &self,
        profile: &ConnectionProfile,
        source: &str,
        target: &str,
        options: JoinPathOptions,
    ) -> StudioResult<Vec<JoinPath>> {
        let store = self.open_store(profile)?;
        find_join_paths_with_options(&store, source, target, &options)
            .map_err(|e| StudioError::Connection(e.to_string()))
    }

    async fn generate_sql(
        &self,
        profile: &ConnectionProfile,
        question: &str,
        dialect: Dialect,
        max_tokens: Option<usize>,
        system: Option<String>,
        max_attempts: Option<usize>,
    ) -> StudioResult<SqlGenerationResult> {
        self.generate_sql_with_policy(
            profile,
            question,
            dialect,
            max_tokens,
            system,
            max_attempts,
            self.tenant_policy(profile),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn generate_sql_focused(
        &self,
        profile: &ConnectionProfile,
        question: &str,
        focus_keys: Vec<String>,
        dialect: Dialect,
        max_tokens: Option<usize>,
        system: Option<String>,
        max_attempts: Option<usize>,
    ) -> StudioResult<SqlGenerationResult> {
        let store = self.open_store(profile)?;
        let mut options = generation_options(
            dialect,
            profile,
            max_tokens,
            system,
            max_attempts,
            self.tenant_policy(profile),
        );
        options.focus_object_keys = focus_keys;
        generate_sql(&store, &self.llm, question, &options)
            .await
            .map_err(|e| StudioError::Connection(e.to_string()))
    }

    async fn validate_sql(
        &self,
        profile: &ConnectionProfile,
        sql: &str,
        dialect: Dialect,
    ) -> StudioResult<SqlValidationResult> {
        self.validate_sql_with_policy(profile, sql, dialect, self.tenant_policy(profile))
    }

    fn llm_provider(&self) -> DynLlmProvider {
        DefaultCatalogService::llm_provider(self)
    }
}

/// Convert a live metadata [`ModelSnapshot`] into wire [`CatalogObject`]s so
/// the validation catalog can be kept in lockstep with what completion sees.
/// Only object kinds the catalog models (tables, views, procedures, functions)
/// are emitted; everything else is skipped.
pub fn catalog_objects_from_snapshot(snapshot: &ModelSnapshot) -> Vec<CatalogObject> {
    let mut out = Vec::with_capacity(snapshot.len());
    for entry in snapshot.objects() {
        let kind = match entry.kind {
            ObjectKind::Table => CatalogObjectKind::Table,
            ObjectKind::View | ObjectKind::MaterializedView => CatalogObjectKind::View,
            ObjectKind::Procedure => CatalogObjectKind::StoredProcedure,
            ObjectKind::Function
            | ObjectKind::ScalarFunction
            | ObjectKind::TableFunction
            | ObjectKind::AggregateFunction => CatalogObjectKind::Function,
            _ => continue,
        };

        let map_columns = |columns: &[sqlly_lang::sqlmodel::ColumnDef]| -> Vec<CatalogColumn> {
            columns
                .iter()
                .map(|c| CatalogColumn {
                    name: c.name.spelling().to_string(),
                    type_name: c.ty.as_ref().map(|t| t.native.clone()),
                    ordinal: c.ordinal as i64 + 1,
                    nullable: c.nullable.unwrap_or(true),
                })
                .collect()
        };

        let (columns, primary_key, foreign_keys, parameters, definition) = match &entry.body {
            ObjectBody::Table(t) => (
                map_columns(&t.columns),
                t.primary_key
                    .as_ref()
                    .map(|k| k.columns.iter().map(|c| c.spelling().to_string()).collect())
                    .unwrap_or_default(),
                t.foreign_keys
                    .iter()
                    .map(|fk| CatalogForeignKey {
                        columns: fk
                            .columns
                            .iter()
                            .map(|c| c.spelling().to_string())
                            .collect(),
                        target_schema: fk
                            .target
                            .namespace
                            .as_ref()
                            .map(|n| n.spelling().to_string()),
                        target_table: fk.target.name.spelling().to_string(),
                        target_columns: fk
                            .target_columns
                            .iter()
                            .map(|c| c.spelling().to_string())
                            .collect(),
                    })
                    .collect(),
                Vec::new(),
                None,
            ),
            ObjectBody::View(v) => (
                map_columns(&v.columns),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                v.definition.clone(),
            ),
            ObjectBody::Routine(r) => (
                Vec::new(),
                Vec::new(),
                Vec::new(),
                r.params
                    .iter()
                    .map(|p| CatalogParameter {
                        name: p.name.spelling().to_string(),
                        type_name: p.ty.as_ref().map(|t| t.native.clone()),
                        is_output: matches!(p.mode, ParamMode::Out | ParamMode::InOut),
                    })
                    .collect(),
                r.body_text.clone(),
            ),
            _ => continue,
        };

        let profile_id = match &entry.provenance {
            sqlly_lang::sqlmodel::Provenance::Database { profile } => Some(profile.clone()),
            sqlly_lang::sqlmodel::Provenance::File { path, .. } => Some(path.clone()),
        };

        out.push(CatalogObject {
            schema: entry
                .reference
                .namespace
                .as_ref()
                .map(|n| n.spelling().to_string()),
            name: entry.reference.name.spelling().to_string(),
            kind,
            columns,
            primary_key,
            foreign_keys,
            parameters,
            definition,
            // No create/modify timestamps from live metadata: change detection
            // falls back to the object hash, which is the source of truth.
            source: CatalogSource {
                kind: SourceKind::Database,
                path: profile_id,
                create_date: None,
                modify_date: None,
            },
        });
    }
    out
}

fn validation_options(
    dialect: Dialect,
    profile: &ConnectionProfile,
    tenant: TenantScopePolicy,
) -> SqlValidationOptions {
    SqlValidationOptions {
        dialect,
        engine: Some(profile.database_type),
        default_database: profile.database.clone(),
        default_schema: None,
        allow_multiple_statements: true,
        require_tenant_filter: !tenant.disabled,
        fail_closed_tenant_filter: tenant.fail_closed,
        tenant_column_folds: tenant.tenant_column_folds,
        // Unconstrained-mutation warnings are off by default; a connection
        // whose safety asks to confirm data changes opts back in.
        flag_unconstrained_mutations: profile.confirm_data_changes,
    }
}

fn generation_options(
    dialect: Dialect,
    profile: &ConnectionProfile,
    max_tokens: Option<usize>,
    system: Option<String>,
    max_attempts: Option<usize>,
    tenant: TenantScopePolicy,
) -> SqlGenerationOptions {
    SqlGenerationOptions {
        dialect,
        max_tokens,
        system,
        default_database: profile.database.clone(),
        max_attempts,
        require_tenant_filter: !tenant.disabled,
        fail_closed_tenant_filter: tenant.fail_closed,
        tenant_column_folds: tenant.tenant_column_folds,
        focus_object_keys: Vec::new(),
    }
}

/// Stable identity for a connection's catalog file. Delegates to the
/// canonical [`ConnectionProfile::catalog_identity`] so the GPUI app and the
/// engine share the same catalog SQLite file for a given connection.
fn connection_identity(profile: &ConnectionProfile) -> String {
    profile.catalog_identity()
}

/// Environment variable that overrides the catalog root directory.
pub const CATALOG_DIR_ENV: &str = "SQLLY_CATALOG_DIR";

/// Resolve the catalog root directory from the environment, matching the
/// engine's resolution: `$SQLLY_CATALOG_DIR` → platform data dir
/// `…/SQLLY/Catalogs` → temp fallback.
#[cfg(target_arch = "wasm32")]
pub fn catalog_root() -> PathBuf {
    // Names a subtree in sqlite-wasm-rs's in-memory VFS; nothing touches a
    // real filesystem in the browser, and the catalog lives for the tab.
    PathBuf::from("/sqlly/catalogs")
}

/// Pre-beta Linux/Windows builds defaulted to a Mac-style path under `$HOME`.
#[cfg(all(not(target_arch = "wasm32"), not(target_os = "macos")))]
fn legacy_mac_style_catalog_root() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(|home| PathBuf::from(home).join("Library/Application Support/SQLLY/Catalogs"))
}

#[cfg(not(target_arch = "wasm32"))]
pub fn catalog_root() -> PathBuf {
    if let Some(value) = std::env::var_os(CATALOG_DIR_ENV)
        && !value.is_empty()
    {
        return PathBuf::from(value);
    }

    let platform = dirs::data_dir().map(|data| data.join("SQLLY/Catalogs"));

    // Honor catalogs already created under the mistaken Mac-style path on
    // Linux/Windows until the user migrates manually.
    //
    // NB for tests: this branch consults the *ambient* `$HOME` via
    // `legacy_mac_style_catalog_root()`. On a machine where
    // `~/Library/Application Support/SQLLY/Catalogs` happens to exist (e.g. the
    // Linux build agent, whose home carried one), it fires and shadows the
    // platform/XDG path. Any test that exercises `catalog_root()` without
    // setting `SQLLY_CATALOG_DIR` MUST pin `HOME` to an isolated empty temp dir,
    // or it is at the mercy of the host filesystem. See the tests below.
    #[cfg(not(target_os = "macos"))]
    if let Some(ref path) = platform
        && !path.exists()
        && let Some(legacy) = legacy_mac_style_catalog_root()
        && legacy.exists()
    {
        return legacy;
    }

    platform.unwrap_or_else(|| std::env::temp_dir().join("sqlly-catalogs"))
}

/// Parse a dialect name string into a [`Dialect`]. Matches the engine's
/// `parse_dialect`: `sqlserver`/`mssql`/`tsql` → SqlServer, etc. Defaults
/// to SqlServer when `None`.
pub fn parse_dialect(name: Option<&str>) -> StudioResult<Dialect> {
    Ok(
        match name.unwrap_or("sqlserver").to_ascii_lowercase().as_str() {
            "sqlserver" | "mssql" | "tsql" => Dialect::SqlServer,
            "postgres" | "postgresql" | "pg" => Dialect::Postgres,
            "mysql" => Dialect::MySql,
            "oracle" => Dialect::Oracle,
            "sqlite" => Dialect::Sqlite,
            other => {
                return Err(StudioError::InvalidInput(format!(
                    "unknown dialect `{other}`"
                )));
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_ai::catalog::{SqlIssueCode, SqlIssueSeverity};
    use sqlly_core::{SecretProvider, SecretRef};
    use sqlly_testkit::env::EnvGuard;

    #[test]
    fn parse_dialect_defaults_to_sqlserver() {
        assert_eq!(parse_dialect(None).unwrap(), Dialect::SqlServer);
        assert_eq!(parse_dialect(Some("mssql")).unwrap(), Dialect::SqlServer);
        assert_eq!(parse_dialect(Some("tsql")).unwrap(), Dialect::SqlServer);
    }

    #[test]
    fn parse_dialect_handles_known_dialects() {
        assert_eq!(parse_dialect(Some("postgres")).unwrap(), Dialect::Postgres);
        assert_eq!(parse_dialect(Some("pg")).unwrap(), Dialect::Postgres);
        assert_eq!(parse_dialect(Some("mysql")).unwrap(), Dialect::MySql);
        assert_eq!(parse_dialect(Some("oracle")).unwrap(), Dialect::Oracle);
        assert_eq!(parse_dialect(Some("sqlite")).unwrap(), Dialect::Sqlite);
    }

    #[test]
    fn parse_dialect_rejects_unknown() {
        assert!(parse_dialect(Some("foo")).is_err());
    }

    #[test]
    fn catalog_root_smoke_returns_nonempty_path() {
        let root = catalog_root();
        assert!(!root.as_os_str().is_empty());
    }

    #[test]
    fn catalog_root_resolves_sqlly_catalog_dir_env() {
        let mut env = EnvGuard::acquire();
        let temp = tempfile::tempdir().unwrap();
        let custom = temp.path().join("custom-catalogs");
        env.set(CATALOG_DIR_ENV, custom.to_str().unwrap());
        assert_eq!(catalog_root(), custom);
    }

    #[cfg(unix)]
    #[test]
    fn catalog_root_uses_xdg_data_home_when_unset() {
        let mut env = EnvGuard::acquire();
        let temp = tempfile::tempdir().unwrap();
        let xdg = temp.path().join("xdg");
        std::fs::create_dir_all(&xdg).unwrap();
        // Pin HOME to an empty temp dir. Without this the non-mac legacy-path
        // branch in `catalog_root()` reads the real `$HOME` and, on a host
        // where `~/Library/Application Support/SQLLY/Catalogs` exists (the
        // Linux build agent did), returns that instead of the XDG path —
        // failing this assertion. See the "NB for tests" note on that branch;
        // the two legacy-path tests below pin HOME for the same reason.
        let home = temp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        env.set("HOME", home.to_str().unwrap());
        env.remove(CATALOG_DIR_ENV);
        env.set("XDG_DATA_HOME", xdg.to_str().unwrap());
        let root = catalog_root();
        if cfg!(target_os = "macos") {
            assert!(root.ends_with("SQLLY/Catalogs"));
        } else {
            assert_eq!(root, xdg.join("SQLLY/Catalogs"));
        }
    }

    // Gated to unix-non-mac (Linux/BSD), NOT all non-mac. These tests drive the
    // platform data dir through `XDG_DATA_HOME`, which only steers
    // `dirs::data_dir()` on unix-non-mac. On Windows `dirs::data_dir()` returns
    // the real `%APPDATA%` and ignores `XDG_DATA_HOME`, so the "platform" dir
    // they create under a temp XDG root is invisible to `catalog_root()` and the
    // legacy branch wins instead — a false failure (this is exactly how
    // `catalog_root_prefers_platform_path_when_it_exists` broke the Windows Test
    // workspace step). There is no hermetic way to redirect `%APPDATA%` via env,
    // so the XDG-driven resolution is only asserted where XDG actually applies.
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn catalog_root_honors_legacy_mac_style_path_when_platform_missing() {
        let mut env = EnvGuard::acquire();
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let xdg = temp.path().join("xdg");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&xdg).unwrap();
        let legacy = home.join("Library/Application Support/SQLLY/Catalogs");
        std::fs::create_dir_all(&legacy).unwrap();

        env.set("HOME", home.to_str().unwrap());
        env.set("XDG_DATA_HOME", xdg.to_str().unwrap());
        env.remove(CATALOG_DIR_ENV);

        assert_eq!(catalog_root(), legacy);
    }

    // See the note above `catalog_root_honors_legacy_mac_style_path_when_platform_missing`:
    // XDG-driven, so unix-non-mac only (fails on Windows, where dirs uses %APPDATA%).
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn catalog_root_prefers_platform_path_when_it_exists() {
        let mut env = EnvGuard::acquire();
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let xdg = temp.path().join("xdg");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&xdg).unwrap();
        let legacy = home.join("Library/Application Support/SQLLY/Catalogs");
        let platform = xdg.join("SQLLY/Catalogs");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::create_dir_all(&platform).unwrap();

        env.set("HOME", home.to_str().unwrap());
        env.set("XDG_DATA_HOME", xdg.to_str().unwrap());
        env.remove(CATALOG_DIR_ENV);

        assert_eq!(catalog_root(), platform);
    }

    fn orders_table() -> CatalogObject {
        CatalogObject {
            schema: Some("dbo".into()),
            name: "orders".into(),
            kind: CatalogObjectKind::Table,
            columns: vec![
                CatalogColumn {
                    name: "order_id".into(),
                    type_name: Some("INT".into()),
                    ordinal: 1,
                    nullable: false,
                },
                CatalogColumn {
                    name: "tenant_id".into(),
                    type_name: Some("INT".into()),
                    ordinal: 2,
                    nullable: false,
                },
            ],
            primary_key: vec!["order_id".into()],
            foreign_keys: vec![],
            parameters: vec![],
            definition: None,
            source: CatalogSource {
                kind: SourceKind::Database,
                path: None,
                create_date: Some(1),
                modify_date: Some(2),
            },
        }
    }

    #[tokio::test]
    async fn validate_sql_honors_fail_closed_from_tenant_resolver() {
        let dir = tempfile::tempdir().expect("tempdir");
        let service = DefaultCatalogService::from_parts(
            dir.path().to_path_buf(),
            Arc::new(StubLlmProvider::new()),
            Arc::new(StubEmbeddingProvider::new()),
        )
        .with_tenant_scope_resolver(Arc::new(|_| TenantScopePolicy {
            disabled: false,
            fail_closed: true,
            tenant_column_folds: None,
        }));
        let mut profile = ConnectionProfile::sql_login(
            "tenant-test",
            "localhost",
            "sa",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: "k".into(),
            },
        );
        profile.database = Some("app".into());
        service
            .sync_catalog(&profile, vec![orders_table()], SyncCoverage::FullSource)
            .await
            .expect("sync");
        let result = service
            .validate_sql(
                &profile,
                "SELECT order_id FROM dbo.orders",
                Dialect::SqlServer,
            )
            .await
            .expect("validate");
        assert!(
            result.issues.iter().any(|issue| {
                issue.code == SqlIssueCode::TenantScopeMissingPredicate
                    && issue.severity == SqlIssueSeverity::Error
            }),
            "{result:?}"
        );
    }

    struct DelegatingCatalog;

    fn unused<T>() -> StudioResult<T> {
        Err(StudioError::Connection(
            "DelegatingCatalog: unused".to_string(),
        ))
    }

    #[async_trait]
    impl CatalogService for DelegatingCatalog {
        async fn sync_catalog(
            &self,
            _profile: &ConnectionProfile,
            _objects: Vec<CatalogObject>,
            _coverage: SyncCoverage,
        ) -> StudioResult<SyncReport> {
            unused()
        }
        async fn build_ai_cache(&self, _profile: &ConnectionProfile) -> StudioResult<()> {
            unused()
        }
        async fn invalidate_ai_cache(
            &self,
            _profile: &ConnectionProfile,
        ) -> StudioResult<NukeAiDataReport> {
            unused()
        }
        async fn catalog_maintenance(
            &self,
            _profile: &ConnectionProfile,
            _options: MaintenanceOptions,
        ) -> StudioResult<MaintenanceReport> {
            unused()
        }
        async fn review_sql(
            &self,
            _profile: &ConnectionProfile,
            _sql: String,
            _dialect: Option<String>,
        ) -> StudioResult<SqlReviewReport> {
            unused()
        }
        async fn summarize_query(
            &self,
            _profile: &ConnectionProfile,
            _sql: String,
            _result_columns: Vec<String>,
            _row_count: Option<u64>,
            _elapsed_ms: Option<u64>,
        ) -> StudioResult<QuerySummaryReport> {
            unused()
        }
        #[cfg(not(target_arch = "wasm32"))]
        async fn provider_status(&self) -> StudioResult<crate::providers::ProviderStatusReport> {
            unused()
        }
        async fn build_embeddings(
            &self,
            _profile: &ConnectionProfile,
        ) -> StudioResult<EmbeddingBuildReport> {
            unused()
        }
        async fn ask_schema(
            &self,
            _profile: &ConnectionProfile,
            _question: &str,
            _max_tokens: Option<usize>,
            _system: Option<String>,
            _purpose: SchemaPurpose,
        ) -> StudioResult<LlmResponse> {
            Ok(LlmResponse::new("delegated"))
        }
        async fn search_schema(
            &self,
            _profile: &ConnectionProfile,
            _query: &str,
            _limit: Option<usize>,
            _object_types: Vec<String>,
        ) -> StudioResult<Vec<SchemaSearchHit>> {
            unused()
        }
        async fn find_join_path(
            &self,
            _profile: &ConnectionProfile,
            _source: &str,
            _target: &str,
            _options: JoinPathOptions,
        ) -> StudioResult<Vec<JoinPath>> {
            unused()
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
            Ok(SqlGenerationResult {
                sql: "SELECT 1".into(),
                intent: sqlly_ai::catalog::SqlIntent::Select,
                validation: SqlValidationResult {
                    is_valid: true,
                    requires_confirmation: false,
                    detected_intent: sqlly_ai::catalog::SqlIntent::Select,
                    issues: Vec::new(),
                    referenced_objects: Vec::new(),
                    referenced_columns: Vec::new(),
                },
                grounded_in: Vec::new(),
                provider_meta: Default::default(),
                attempts: Vec::new(),
            })
        }
        async fn validate_sql(
            &self,
            _profile: &ConnectionProfile,
            _sql: &str,
            _dialect: Dialect,
        ) -> StudioResult<SqlValidationResult> {
            unused()
        }
        fn llm_provider(&self) -> DynLlmProvider {
            Arc::new(StubLlmProvider::new())
        }
    }

    #[tokio::test]
    async fn ask_schema_focused_default_delegates() {
        let service = DelegatingCatalog;
        let profile = ConnectionProfile::sql_login(
            "focus-test",
            "localhost",
            "sa",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: "k".into(),
            },
        );
        let plain = service
            .ask_schema(&profile, "q", None, None, SchemaPurpose::General)
            .await
            .expect("ask");
        let focused = service
            .ask_schema_focused(
                &profile,
                "q",
                vec!["dbo.orders".into()],
                None,
                None,
                SchemaPurpose::General,
            )
            .await
            .expect("ask focused");
        assert_eq!(plain.text, focused.text);
        assert_eq!(focused.text, "delegated");
        let generated = service
            .generate_sql_focused(
                &profile,
                "q",
                vec!["dbo.orders".into()],
                Dialect::SqlServer,
                None,
                None,
                None,
            )
            .await
            .expect("generate focused");
        assert_eq!(generated.sql, "SELECT 1");
    }
}
