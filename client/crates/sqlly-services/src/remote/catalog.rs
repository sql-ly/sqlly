//! Remote [`CatalogService`]: all nine methods map 1:1 to engine requests. The
//! request bodies are encoded to match `engine.rs`'s `decode_*_binary` decoders
//! byte-for-byte; the report frames' JSON payloads are decoded back into the
//! native catalog types the trait returns.
//!
//! Several report types (`SyncReport`, `LlmResponse`, `EmbeddingBuildReport`,
//! `SchemaSearchHit`, `JoinPath`) are `Serialize`-only in `sqlly-catalog`, so
//! they are reconstructed field-by-field from the JSON `Value` rather than via
//! `serde` `Deserialize`. The others (`NukeAiDataReport`, `SqlGenerationResult`,
//! `SqlValidationResult`) round-trip through `serde_json::from_str`.

use std::sync::Arc;

use crate::catalog::{CatalogService, TenantScopePolicy, TenantScopeResolver};
use async_trait::async_trait;
use sqlly_ai::catalog::{
    CatalogObject, CatalogObjectKind, EmbeddingBuildReport, JoinDirection, JoinPath,
    JoinPathOptions, JoinPathStep, LlmResponse, NukeAiDataReport, SchemaPurpose, SchemaSearchHit,
    SourceKind, SqlGenerationResult, SqlValidationResult, SyncReport,
};
use sqlly_core::{ConnectionProfile, StudioError, StudioResult};
use sqlly_lang::sqlmodel::Dialect;
use sqlly_remote::engine_protocol::{
    FRAME_TAG_AI_CACHE_BUILD_REPORT, FRAME_TAG_CATALOG_SYNC_REPORT,
    FRAME_TAG_EMBEDDINGS_BUILD_REPORT, FRAME_TAG_INVALIDATE_AI_CACHE_REPORT,
    FRAME_TAG_JOIN_PATH_REPORT, FRAME_TAG_SCHEMA_ANSWER, FRAME_TAG_SCHEMA_SEARCH_RESULTS,
    FRAME_TAG_SQL_GENERATION_REPORT, FRAME_TAG_SQL_VALIDATION_REPORT, FramedTransport,
    REQUEST_TAG_ASK_SCHEMA, REQUEST_TAG_BUILD_AI_CACHE, REQUEST_TAG_BUILD_EMBEDDINGS,
    REQUEST_TAG_FIND_JOIN_PATH, REQUEST_TAG_GENERATE_SQL, REQUEST_TAG_INVALIDATE_AI_CACHE,
    REQUEST_TAG_SEARCH_SCHEMA, REQUEST_TAG_SYNC_CATALOG, REQUEST_TAG_VALIDATE_SQL, TenantScopeWire,
    append_tenant_scope_trailing,
};

use crate::remote::engine::{
    RemoteEngine, decode_report_json, dialect_str, engine_connection_json, request_header,
};

/// Remote catalog service bound to one [`RemoteEngine`] session.
pub struct RemoteCatalogService<T> {
    engine: RemoteEngine<T>,
    tenant_scope: Option<TenantScopeResolver>,
}

impl<T> Clone for RemoteCatalogService<T> {
    fn clone(&self) -> Self {
        Self {
            engine: self.engine.clone(),
            tenant_scope: self.tenant_scope.clone(),
        }
    }
}

impl<T: FramedTransport + 'static> RemoteCatalogService<T> {
    pub fn new(engine: RemoteEngine<T>) -> Self {
        Self {
            engine,
            tenant_scope: None,
        }
    }

    /// Install the per-database tenant-scope resolver so validate/generate/review
    /// requests carry the same Required/Warn policy as the Direct catalog path.
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

    fn tenant_scope_wire(&self, profile: &ConnectionProfile) -> TenantScopeWire {
        let policy = self.tenant_policy(profile);
        TenantScopeWire {
            require_tenant_filter: !policy.disabled,
            fail_closed_tenant_filter: policy.fail_closed,
            tenant_column_folds: policy.tenant_column_folds,
        }
    }
}

#[async_trait]
impl<T: FramedTransport + 'static> CatalogService for RemoteCatalogService<T> {
    async fn sync_catalog(
        &self,
        profile: &ConnectionProfile,
        objects: Vec<CatalogObject>,
        coverage: sqlly_ai::catalog::SyncCoverage,
    ) -> StudioResult<SyncReport> {
        let conn = engine_connection_json(profile)?;
        // Full-source syncs stay byte-identical to old clients; only narrower
        // coverage is encoded, so losing it to an old engine can only happen
        // for the callers that need it (and the version handshake ages those
        // engines out) — the common path never changes shape.
        let coverage_json = match &coverage {
            sqlly_ai::catalog::SyncCoverage::FullSource => None,
            other => Some(
                serde_json::to_string(other)
                    .map_err(|e| StudioError::Connection(format!("encode sync coverage: {e}")))?,
            ),
        };
        let body = self
            .engine
            .report_request(
                REQUEST_TAG_SYNC_CATALOG,
                FRAME_TAG_CATALOG_SYNC_REPORT,
                |id| {
                    let wires: Vec<_> = objects.iter().map(catalog_object_wire).collect();
                    sqlly_remote::engine_protocol::layouts::encode_sync_catalog_with_coverage(
                        id,
                        &conn,
                        &wires,
                        coverage_json.as_deref(),
                    )
                },
            )
            .await?;
        let value = report_value(&body)?;
        Ok(sync_report_from_json(&value))
    }

    async fn build_ai_cache(&self, profile: &ConnectionProfile) -> StudioResult<()> {
        let conn = engine_connection_json(profile)?;
        // The report (`BuildReport`) is not exposed by `sqlly-catalog`; the
        // trait returns `()`, so receiving the report frame is success enough.
        self.engine
            .report_request(
                REQUEST_TAG_BUILD_AI_CACHE,
                FRAME_TAG_AI_CACHE_BUILD_REPORT,
                |id| request_header(id).string(&conn).done(),
            )
            .await?;
        Ok(())
    }

    async fn review_sql(
        &self,
        profile: &ConnectionProfile,
        sql: String,
        dialect: Option<String>,
    ) -> StudioResult<sqlly_ai::catalog::SqlReviewReport> {
        let conn = engine_connection_json(profile)?;
        let tenant = self.tenant_scope_wire(profile);
        let args_json = serde_json::json!({ "sql": sql, "dialect": dialect }).to_string();
        let body = self
            .engine
            .report_request(
                sqlly_remote::engine_protocol::REQUEST_TAG_REVIEW_SQL,
                sqlly_remote::engine_protocol::FRAME_TAG_SQL_REVIEW_REPORT,
                |id| {
                    append_tenant_scope_trailing(
                        request_header(id).string(&conn).string(&args_json),
                        &tenant,
                    )
                    .done()
                },
            )
            .await?;
        let json = decode_report_json(&body)?;
        serde_json::from_str(&json)
            .map_err(|e| StudioError::Serialization(format!("decode review_sql report: {e}")))
    }

    async fn summarize_query(
        &self,
        profile: &ConnectionProfile,
        sql: String,
        result_columns: Vec<String>,
        row_count: Option<u64>,
        elapsed_ms: Option<u64>,
    ) -> StudioResult<sqlly_ai::catalog::QuerySummaryReport> {
        let conn = engine_connection_json(profile)?;
        let args_json = serde_json::json!({
            "sql": sql,
            "result_columns": result_columns,
            "row_count": row_count,
            "elapsed_ms": elapsed_ms,
        })
        .to_string();
        let body = self
            .engine
            .report_request(
                sqlly_remote::engine_protocol::REQUEST_TAG_SUMMARIZE_QUERY,
                sqlly_remote::engine_protocol::FRAME_TAG_QUERY_SUMMARY_REPORT,
                |id| request_header(id).string(&conn).string(&args_json).done(),
            )
            .await?;
        let json = decode_report_json(&body)?;
        serde_json::from_str(&json)
            .map_err(|e| StudioError::Serialization(format!("decode query summary report: {e}")))
    }

    #[cfg(not(target_arch = "wasm32"))]
    async fn provider_status(&self) -> StudioResult<crate::providers::ProviderStatusReport> {
        let body = self
            .engine
            .report_request(
                sqlly_remote::engine_protocol::REQUEST_TAG_PROVIDER_STATUS,
                sqlly_remote::engine_protocol::FRAME_TAG_PROVIDER_STATUS_REPORT,
                sqlly_remote::engine_protocol::layouts::encode_header_only,
            )
            .await?;
        let json = decode_report_json(&body)?;
        serde_json::from_str(&json)
            .map_err(|e| StudioError::Serialization(format!("decode provider status: {e}")))
    }

    async fn catalog_maintenance(
        &self,
        profile: &ConnectionProfile,
        options: sqlly_ai::catalog::MaintenanceOptions,
    ) -> StudioResult<sqlly_ai::catalog::MaintenanceReport> {
        let conn = engine_connection_json(profile)?;
        let options_json = serde_json::to_string(&options)
            .map_err(|e| StudioError::Serialization(format!("encode maintenance options: {e}")))?;
        let body = self
            .engine
            .report_request(
                sqlly_remote::engine_protocol::REQUEST_TAG_CATALOG_MAINTENANCE,
                sqlly_remote::engine_protocol::FRAME_TAG_CATALOG_MAINTENANCE_REPORT,
                |id| {
                    request_header(id)
                        .string(&conn)
                        .string(&options_json)
                        .done()
                },
            )
            .await?;
        let json = decode_report_json(&body)?;
        serde_json::from_str(&json).map_err(|e| {
            StudioError::Serialization(format!("decode catalog_maintenance report: {e}"))
        })
    }

    async fn invalidate_ai_cache(
        &self,
        profile: &ConnectionProfile,
    ) -> StudioResult<NukeAiDataReport> {
        let conn = engine_connection_json(profile)?;
        let body = self
            .engine
            .report_request(
                REQUEST_TAG_INVALIDATE_AI_CACHE,
                FRAME_TAG_INVALIDATE_AI_CACHE_REPORT,
                |id| request_header(id).string(&conn).done(),
            )
            .await?;
        let json = decode_report_json(&body)?;
        // NukeAiDataReport derives Deserialize.
        serde_json::from_str(&json).map_err(|e| {
            StudioError::Serialization(format!("decode invalidate_ai_cache report: {e}"))
        })
    }

    async fn build_embeddings(
        &self,
        profile: &ConnectionProfile,
    ) -> StudioResult<EmbeddingBuildReport> {
        let conn = engine_connection_json(profile)?;
        let body = self
            .engine
            .report_request(
                REQUEST_TAG_BUILD_EMBEDDINGS,
                FRAME_TAG_EMBEDDINGS_BUILD_REPORT,
                |id| request_header(id).string(&conn).done(),
            )
            .await?;
        let value = report_value(&body)?;
        Ok(embedding_report_from_json(&value))
    }

    async fn ask_schema(
        &self,
        profile: &ConnectionProfile,
        question: &str,
        max_tokens: Option<usize>,
        system: Option<String>,
        purpose: SchemaPurpose,
    ) -> StudioResult<LlmResponse> {
        let conn = engine_connection_json(profile)?;
        let purpose_byte = match purpose {
            SchemaPurpose::SqlGeneration => 1u8,
            _ => 0u8,
        };
        let body = self
            .engine
            .report_request(REQUEST_TAG_ASK_SCHEMA, FRAME_TAG_SCHEMA_ANSWER, |id| {
                request_header(id)
                    .string(&conn)
                    .string(question)
                    .opt_u32(max_tokens.map(|v| v as u32))
                    .opt_string(system.as_deref())
                    .u8(purpose_byte)
                    .done()
            })
            .await?;
        let value = report_value(&body)?;
        Ok(LlmResponse {
            text: value
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            meta: value
                .get("meta")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        })
    }

    async fn search_schema(
        &self,
        profile: &ConnectionProfile,
        query: &str,
        limit: Option<usize>,
        object_types: Vec<String>,
    ) -> StudioResult<Vec<SchemaSearchHit>> {
        let conn = engine_connection_json(profile)?;
        let body = self
            .engine
            .report_request(
                REQUEST_TAG_SEARCH_SCHEMA,
                FRAME_TAG_SCHEMA_SEARCH_RESULTS,
                |id| {
                    let mut w = request_header(id)
                        .string(&conn)
                        .string(query)
                        .opt_u32(limit.map(|v| v as u32))
                        .u32_le(object_types.len() as u32);
                    for ot in &object_types {
                        w = w.string(ot);
                    }
                    w.done()
                },
            )
            .await?;
        let value = report_value(&body)?;
        let hits = value
            .as_array()
            .map(|arr| arr.iter().map(search_hit_from_json).collect())
            .unwrap_or_default();
        Ok(hits)
    }

    async fn find_join_path(
        &self,
        profile: &ConnectionProfile,
        source: &str,
        target: &str,
        options: JoinPathOptions,
    ) -> StudioResult<Vec<JoinPath>> {
        let conn = engine_connection_json(profile)?;
        let body = self
            .engine
            .report_request(
                REQUEST_TAG_FIND_JOIN_PATH,
                FRAME_TAG_JOIN_PATH_REPORT,
                |id| {
                    request_header(id)
                        .string(&conn)
                        .string(source)
                        .string(target)
                        .u32_le(options.max_depth as u32)
                        .bool(options.include_inferred)
                        .f64_le(options.min_confidence)
                        .done()
                },
            )
            .await?;
        let value = report_value(&body)?;
        let paths = value
            .as_array()
            .map(|arr| arr.iter().map(join_path_from_json).collect())
            .unwrap_or_default();
        Ok(paths)
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
        let conn = engine_connection_json(profile)?;
        let tenant = self.tenant_scope_wire(profile);
        let body = self
            .engine
            .report_request(
                REQUEST_TAG_GENERATE_SQL,
                FRAME_TAG_SQL_GENERATION_REPORT,
                |id| {
                    append_tenant_scope_trailing(
                        request_header(id)
                            .string(&conn)
                            .string(question)
                            .opt_string(Some(dialect_str(dialect)))
                            .opt_u32(max_tokens.map(|v| v as u32))
                            .opt_string(system.as_deref())
                            .opt_u32(max_attempts.map(|v| v as u32)),
                        &tenant,
                    )
                    .done()
                },
            )
            .await?;
        let json = decode_report_json(&body)?;
        serde_json::from_str(&json)
            .map_err(|e| StudioError::Serialization(format!("decode generate_sql report: {e}")))
    }

    async fn validate_sql(
        &self,
        profile: &ConnectionProfile,
        sql: &str,
        dialect: Dialect,
    ) -> StudioResult<SqlValidationResult> {
        let conn = engine_connection_json(profile)?;
        let tenant = self.tenant_scope_wire(profile);
        let body = self
            .engine
            .report_request(
                REQUEST_TAG_VALIDATE_SQL,
                FRAME_TAG_SQL_VALIDATION_REPORT,
                |id| {
                    append_tenant_scope_trailing(
                        request_header(id)
                            .string(&conn)
                            .string(sql)
                            .opt_string(Some(dialect_str(dialect))),
                        &tenant,
                    )
                    .done()
                },
            )
            .await?;
        let json = decode_report_json(&body)?;
        serde_json::from_str(&json)
            .map_err(|e| StudioError::Serialization(format!("decode validate_sql report: {e}")))
    }

    fn llm_provider(&self) -> sqlly_ai::catalog::DynLlmProvider {
        Arc::new(sqlly_ai::catalog::StubLlmProvider::new())
    }
}

// ---------------------------------------------------------------------------
// Report JSON → native type reconstruction (for Serialize-only catalog types)
// ---------------------------------------------------------------------------

fn report_value(body: &[u8]) -> StudioResult<serde_json::Value> {
    let json = decode_report_json(body)?;
    serde_json::from_str(&json)
        .map_err(|e| StudioError::Serialization(format!("decode report json: {e}")))
}

fn as_usize(v: &serde_json::Value, key: &str) -> usize {
    v.get(key).and_then(|v| v.as_u64()).unwrap_or(0) as usize
}

fn sync_report_from_json(v: &serde_json::Value) -> SyncReport {
    SyncReport {
        added: as_usize(v, "added"),
        changed: as_usize(v, "changed"),
        unchanged: as_usize(v, "unchanged"),
        removed: as_usize(v, "removed"),
        resurrected: as_usize(v, "resurrected"),
        ai_cache_invalidated: as_usize(v, "ai_cache_invalidated"),
        inferred_relationships: as_usize(v, "inferred_relationships"),
    }
}

fn embedding_report_from_json(v: &serde_json::Value) -> EmbeddingBuildReport {
    EmbeddingBuildReport {
        processed: as_usize(v, "processed"),
        embedded: as_usize(v, "embedded"),
        failed: as_usize(v, "failed"),
        remaining_stale: as_usize(v, "remaining_stale"),
        dims: as_usize(v, "dims"),
    }
}

fn search_hit_from_json(v: &serde_json::Value) -> SchemaSearchHit {
    SchemaSearchHit {
        object_key: v
            .get("object_key")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        schema: v.get("schema").and_then(|v| v.as_str()).map(str::to_string),
        name: v
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        object_type: v
            .get("object_type")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        score: v.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0),
        tiny_card: v
            .get("tiny_card")
            .and_then(|v| v.as_str())
            .map(str::to_string),
    }
}

fn join_path_from_json(v: &serde_json::Value) -> JoinPath {
    let steps = v
        .get("steps")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().map(join_step_from_json).collect())
        .unwrap_or_default();
    JoinPath {
        steps,
        score: v.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0),
    }
}

fn string_vec(v: &serde_json::Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|s| s.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn join_step_from_json(v: &serde_json::Value) -> JoinPathStep {
    let direction = match v.get("direction").and_then(|v| v.as_str()) {
        Some("reverse") => JoinDirection::Reverse,
        _ => JoinDirection::Forward,
    };
    JoinPathStep {
        from_object_key: v
            .get("from_object_key")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        from_columns: string_vec(v, "from_columns"),
        to_object_key: v
            .get("to_object_key")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        to_columns: string_vec(v, "to_columns"),
        direction,
        is_inferred: v
            .get("is_inferred")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        confidence: v.get("confidence").and_then(|v| v.as_f64()).unwrap_or(0.0),
    }
}

// ---------------------------------------------------------------------------
// Request encoding: CatalogObject → binary body (mirrors decode_sync_catalog_binary)
// ---------------------------------------------------------------------------

fn catalog_kind_byte(kind: CatalogObjectKind) -> u8 {
    match kind {
        CatalogObjectKind::Table => 0,
        CatalogObjectKind::View => 1,
        CatalogObjectKind::StoredProcedure => 2,
        CatalogObjectKind::Function => 3,
        CatalogObjectKind::Type => 4,
    }
}

fn source_kind_byte(kind: SourceKind) -> u8 {
    match kind {
        SourceKind::Database => 0,
        SourceKind::File => 1,
    }
}

/// Map a catalog object onto its protocol wire shape (the byte layout itself
/// is owned by `sqlly_remote::engine_protocol::layouts`).
fn catalog_object_wire(
    obj: &CatalogObject,
) -> sqlly_remote::engine_protocol::layouts::CatalogObjectWire {
    use sqlly_remote::engine_protocol::layouts::{
        CatalogColumnWire, CatalogForeignKeyWire, CatalogObjectWire, CatalogParameterWire,
    };
    CatalogObjectWire {
        schema: obj.schema.clone(),
        name: obj.name.clone(),
        kind: catalog_kind_byte(obj.kind),
        columns: obj
            .columns
            .iter()
            .map(|c| CatalogColumnWire {
                name: c.name.clone(),
                type_name: c.type_name.clone(),
                ordinal: c.ordinal,
                nullable: c.nullable,
            })
            .collect(),
        primary_key: obj.primary_key.clone(),
        foreign_keys: obj
            .foreign_keys
            .iter()
            .map(|fk| CatalogForeignKeyWire {
                columns: fk.columns.clone(),
                target_schema: fk.target_schema.clone(),
                target_table: fk.target_table.clone(),
                target_columns: fk.target_columns.clone(),
            })
            .collect(),
        parameters: obj
            .parameters
            .iter()
            .map(|p| CatalogParameterWire {
                name: p.name.clone(),
                type_name: p.type_name.clone(),
                is_output: p.is_output,
            })
            .collect(),
        definition: obj.definition.clone(),
        source_kind: source_kind_byte(obj.source.kind),
        source_path: obj.source.path.clone(),
        source_create_date: obj.source.create_date,
        source_modify_date: obj.source.modify_date,
    }
}
