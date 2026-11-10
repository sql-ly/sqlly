use super::*;

#[derive(Debug)]
pub(crate) enum EngineRequest {
    Ping {
        id: String,
    },
    ExecuteQuery {
        id: String,
        connection: Box<EngineConnection>,
        sql: String,
        row_buffer: Option<usize>,
        /// Caller-supplied pool key. Each query-editor tab passes a distinct
        /// key so it gets its own warm connection and the tabs operate
        /// independently. When omitted, the engine derives a key from the
        /// connection identity (all queries for that identity share one warm
        /// client). See [`ConnPool`].
        connection_key: Option<String>,
        /// Cut cells over 8 KiB to previews and report them in
        /// `TruncatedCells` frames (the client asked with
        /// `EXECUTE_QUERY_OPTION_CELL_PREVIEWS`). Off for old clients and for
        /// full-value exports: every cell is sent whole.
        cell_previews: bool,
    },
    Cancel {
        id: String,
        target_id: String,
    },
    Shutdown {
        id: String,
    },
    /// A row-editing operation executed engine-side so relay-backed clients
    /// get the same guarded mutations as direct connections. `op` names the
    /// [`RowEditingService`](sqlly_services::row_editing::RowEditingService)
    /// method; `payload` is its request serialized as JSON.
    RowEdit {
        id: String,
        connection: Box<EngineConnection>,
        op: String,
        payload: String,
    },
    /// **Legacy shim (plan 064):** offline editor validation via
    /// `sqlly_intel::modelengine::validate_query` and an inline editor-schema snapshot.
    /// Frozen macOS `EngineClient.validate` (tag 0x10) still sends this. New
    /// surfaces must use [`EngineRequest::ValidateSql`] / catalog
    /// `validate_sql` instead. Schema wire types are shared with the intellisense
    /// command.
    Validate {
        id: String,
        sql: String,
        dialect: Option<String>,
        default_schema: Option<String>,
        objects: Vec<crate::intellisense_cmd::SchemaObject>,
        foreign_keys: Vec<crate::intellisense_cmd::SchemaForeignKey>,
    },
    /// Cache-update trigger: persist freshly-observed schema facts into the
    /// durable catalog. The app collects objects (from the live database and/or
    /// filesystem DDL) with their source timestamps and sends them here; the
    /// engine diffs them against persisted state using a timestamp gate backed
    /// by a stable per-object hash, upserting changed/new objects, tombstoning
    /// removed ones, and invalidating AI cards for anything that changed. No
    /// database I/O is performed — the objects come entirely from the payload.
    SyncCatalog {
        id: String,
        connection: Box<EngineConnection>,
        objects: Vec<CatalogObject>,
        coverage: sqlly_ai::catalog::SyncCoverage,
    },
    /// Run explicit catalog maintenance (tombstone purge, orphan-embedding
    /// cleanup, FTS optimize). Local SQLite I/O; runs inline like SyncCatalog.
    CatalogMaintenance {
        id: String,
        connection: Box<EngineConnection>,
        options: sqlly_ai::catalog::MaintenanceOptions,
    },
    /// Point-in-time AI provider stack status: configured/active providers,
    /// fallback reasons, and a short-timeout runtime probe. No connection —
    /// the provider stack is engine-global.
    ProviderStatus {
        id: String,
    },
    /// Catalog-grounded SQL review: deterministic validation plus an
    /// LLM narrative built from ranked schema context. LLM-bound (spawned).
    ReviewSql {
        id: String,
        connection: Box<EngineConnection>,
        sql: String,
        dialect: Option<String>,
        max_tokens: Option<usize>,
        tenant_scope: Option<sqlly_remote::engine_protocol::TenantScopeWire>,
    },
    /// Catalog-grounded plain-language summary of an executed query.
    /// LLM-bound (spawned).
    SummarizeQuery {
        id: String,
        connection: Box<EngineConnection>,
        sql: String,
        result_columns: Vec<String>,
        row_count: Option<u64>,
        elapsed_ms: Option<u64>,
        max_tokens: Option<usize>,
    },
    /// Opt-in AI-cache builder: precompute a card for every object whose cache
    /// is stale. With this never called, stale flags persist forever and
    /// `ask_schema` synthesizes compact cards on demand. Calling this clears
    /// the flags by generating cards via the configured LLM provider.
    BuildAiCache {
        id: String,
        connection: Box<EngineConnection>,
    },
    /// Ask a natural-language question against the persisted catalog. The
    /// engine ranks objects by relevance, assembles token-budgeted context,
    /// and calls the LLM provider — never sending the full schema.
    AskSchema {
        id: String,
        connection: Box<EngineConnection>,
        question: String,
        max_tokens: Option<usize>,
        system: Option<String>,
        purpose: SchemaPurpose,
    },
    /// Search the persisted catalog without calling the LLM. This is the direct
    /// Rust-side surface for schema search UIs and diagnostics.
    SearchSchema {
        id: String,
        connection: Box<EngineConnection>,
        query: String,
        limit: Option<usize>,
        object_types: Vec<String>,
    },
    /// Find join paths between two schema objects using FK relationships.
    /// Returns up to 3 bounded BFS paths without calling the LLM.
    FindJoinPath {
        id: String,
        connection: Box<EngineConnection>,
        source: String,
        target: String,
        max_depth: usize,
        /// When false, only declared foreign keys are traversed (strict mode).
        /// Defaults to true to preserve historical behavior.
        include_inferred: bool,
        /// Inferred edges below this confidence are skipped. Declared FKs always
        /// pass. Defaults to 0.0 (every persisted inferred edge is eligible).
        min_confidence: f64,
    },
    /// Build the embedding cache for every stale object in this connection's
    /// catalog. Runs as an async task: embedding generation calls the async
    /// embedding provider per batch. After building, `search_schema` will use
    /// hybrid FTS+vector ranking when embeddings exist.
    BuildEmbeddings {
        id: String,
        connection: Box<EngineConnection>,
    },
    /// Generate SQL from a natural-language question, grounded in the persisted
    /// catalog. Reuses the schema-intelligence context-assembly path with the
    /// SQL-generation purpose, then validates the produced statement against the
    /// catalog before returning it. LLM-bound: runs as an async task.
    GenerateSql {
        id: String,
        connection: Box<EngineConnection>,
        question: String,
        dialect: Option<String>,
        max_tokens: Option<usize>,
        system: Option<String>,
        /// Maximum total generation attempts (first generation plus correction
        /// retries). Omitted/`None` uses the catalog default; the catalog clamps
        /// any value to its hard cap.
        max_attempts: Option<usize>,
        tenant_scope: Option<sqlly_remote::engine_protocol::TenantScopeWire>,
    },
    /// Nuke all AI-derived data in this connection's catalog: invalidate every
    /// AI cache card, wipe card content, and delete all embedding rows. Runs
    /// inline like `SyncCatalog`: local SQLite I/O only, no LLM. Returns an
    /// `InvalidateAiCacheReport` frame carrying the `NukeAiDataReport`.
    InvalidateAiCache {
        id: String,
        connection: Box<EngineConnection>,
    },
    /// Validate a SQL statement against the persisted catalog without calling
    /// the LLM. Deterministic and local: parses the SQL, resolves referenced
    /// objects/columns, and flags tenant-scope, destructive, and cross-database
    /// issues.
    ValidateSql {
        id: String,
        connection: Box<EngineConnection>,
        sql: String,
        dialect: Option<String>,
        tenant_scope: Option<sqlly_remote::engine_protocol::TenantScopeWire>,
    },
}

/// Map a `u8` editor object kind to the wire string used by
/// `SchemaObject` / intellisense. Values must match Swift
/// `EditorSchema.ObjectKind` ordering.
pub(crate) fn decode_editor_kind(byte: u8) -> Result<String> {
    Ok(match byte {
        0 => "table",
        1 => "view",
        2 => "sproc",
        3 => "function",
        4 => "type",
        v => anyhow::bail!("unknown editor object kind 0x{v:02x} (expected 0-4)"),
    }
    .to_string())
}

/// Map a `u8` catalog object kind to `CatalogObjectKind`. Values must match
/// Swift `catalogKindName` ordering and `CatalogObjectKind` discriminant order.
pub(crate) fn decode_catalog_kind(byte: u8) -> Result<sqlly_ai::catalog::CatalogObjectKind> {
    Ok(match byte {
        0 => sqlly_ai::catalog::CatalogObjectKind::Table,
        1 => sqlly_ai::catalog::CatalogObjectKind::View,
        2 => sqlly_ai::catalog::CatalogObjectKind::StoredProcedure,
        3 => sqlly_ai::catalog::CatalogObjectKind::Function,
        4 => sqlly_ai::catalog::CatalogObjectKind::Type,
        v => anyhow::bail!("unknown catalog object kind 0x{v:02x} (expected 0-4)"),
    })
}

/// Map a `u8` source kind to `SourceKind`.
pub(crate) fn decode_source_kind(byte: u8) -> Result<sqlly_ai::catalog::SourceKind> {
    Ok(match byte {
        0 => sqlly_ai::catalog::SourceKind::Database,
        1 => sqlly_ai::catalog::SourceKind::File,
        v => anyhow::bail!("unknown source kind 0x{v:02x} (expected 0 or 1)"),
    })
}

/// Decode a binary validate request body (after the `0x10` tag byte) into the
/// existing `EngineRequest::Validate` variant.
///
/// Wire layout (all little-endian):
/// ```text
/// u16 layout_version
/// string id
/// string sql
/// optional_string dialect
/// optional_string default_schema
/// u32 object_count
///   repeated editor object:
///     string schema
///     string name
///     u8 kind  (0=table, 1=view, 2=sproc, 3=function, 4=type)
///     u32 column_count
///       repeated string column_name
///     u32 parameter_count
///       repeated parameter:
///         string name
///         string type_name
///         bool is_output
/// u32 foreign_key_count
///   repeated FK:
///     string from_schema
///     string from_table
///     string from_column
///     string to_schema
///     string to_table
///     string to_column
/// ```
pub(crate) fn decode_validate_binary(body: &[u8]) -> Result<EngineRequest> {
    use crate::intellisense_cmd::{SchemaForeignKey, SchemaObject, SchemaParameter};

    let mut r = BinaryFrameReader::new(body);
    let version = r.read_u16_le("layout_version")?;
    if version != BINARY_REQUEST_LAYOUT_VERSION {
        anyhow::bail!(
            "unsupported binary validate layout version {version} (expected {BINARY_REQUEST_LAYOUT_VERSION})"
        );
    }
    let id = r.read_string("id")?;
    let sql = r.read_string("sql")?;
    let dialect = r.read_optional_string("dialect")?;
    let default_schema = r.read_optional_string("default_schema")?;

    let object_count = r.read_count_with_min("object_count", 17)?;
    let mut objects = Vec::with_capacity(object_count);
    for i in 0..object_count {
        let p = format!("objects[{i}]");
        let schema = r.read_string(&format!("{p}.schema"))?;
        let name = r.read_string(&format!("{p}.name"))?;
        let kind = decode_editor_kind(r.read_u8(&format!("{p}.kind"))?)?;

        let column_count = r.read_count_with_min(&format!("{p}.column_count"), 4)?;
        let mut columns = Vec::with_capacity(column_count);
        for j in 0..column_count {
            columns.push(r.read_string(&format!("{p}.columns[{j}]"))?);
        }

        let parameter_count = r.read_count_with_min(&format!("{p}.parameter_count"), 9)?;
        let mut parameters = Vec::with_capacity(parameter_count);
        for j in 0..parameter_count {
            let pp = format!("{p}.parameters[{j}]");
            let name = r.read_string(&format!("{pp}.name"))?;
            let type_name = r.read_string(&format!("{pp}.type_name"))?;
            let is_output = r.read_bool(&format!("{pp}.is_output"))?;
            parameters.push(SchemaParameter {
                name,
                type_name,
                is_output,
            });
        }

        objects.push(SchemaObject {
            schema,
            name,
            kind,
            columns,
            parameters,
        });
    }

    let fk_count = r.read_count_with_min("foreign_key_count", 24)?;
    let mut foreign_keys = Vec::with_capacity(fk_count);
    for i in 0..fk_count {
        let p = format!("foreign_keys[{i}]");
        let from_schema = r.read_string(&format!("{p}.from_schema"))?;
        let from_table = r.read_string(&format!("{p}.from_table"))?;
        let from_column = r.read_string(&format!("{p}.from_column"))?;
        let to_schema = r.read_string(&format!("{p}.to_schema"))?;
        let to_table = r.read_string(&format!("{p}.to_table"))?;
        let to_column = r.read_string(&format!("{p}.to_column"))?;
        foreign_keys.push(SchemaForeignKey {
            from_schema,
            from_table,
            from_column,
            to_schema,
            to_table,
            to_column,
        });
    }

    r.ensure_at_end()?;

    Ok(EngineRequest::Validate {
        id,
        sql,
        dialect,
        default_schema,
        objects,
        foreign_keys,
    })
}

/// Decode a binary sync-catalog request body (after the `0x11` tag byte) into
/// the existing `EngineRequest::SyncCatalog` variant.
///
/// The `EngineConnection` is carried as length-prefixed JSON bytes inside the
/// binary body — it is small, sensitive, and evolves independently from the
/// schema object array, so duplicating its shape in binary would add risk
/// without meaningful performance benefit. The large `objects` array is the
/// part that benefits from binary encoding.
///
/// Wire layout (all little-endian):
/// ```text
/// u16 layout_version
/// string id
/// string connection_json   (length-prefixed UTF-8 JSON for EngineConnection)
/// u32 object_count
///   repeated catalog object:
///     optional_string schema
///     string name
///     u8 kind  (0=table, 1=view, 2=stored_procedure, 3=function, 4=type)
///     u32 column_count
///       repeated column:
///         string name
///         optional_string type_name
///         i64 ordinal
///         bool nullable
///     u32 foreign_key_count
///       repeated FK:
///         u32 local_column_count
///           repeated string local_column
///         optional_string target_schema
///         string target_table
///         u32 target_column_count
///           repeated string target_column
///     u32 parameter_count
///       repeated parameter:
///         string name
///         optional_string type_name
///         bool is_output
///     optional_string definition
///     u8 source_kind  (0=database, 1=file)
///     optional_string path
///     optional_i64 create_date
///     optional_i64 modify_date
/// ```
pub(crate) fn decode_sync_catalog_binary(body: &[u8]) -> Result<EngineRequest> {
    use sqlly_ai::catalog::{
        CatalogColumn, CatalogForeignKey, CatalogObject, CatalogParameter, CatalogSource,
    };

    // The byte layout is protocol-owned (`sqlly_remote::engine_protocol::layouts`);
    // this decoder only maps wire structs onto catalog types and validates
    // the kind bytes the engine understands.
    let wire = sqlly_remote::engine_protocol::layouts::decode_sync_catalog(body)
        .map_err(|e| anyhow::anyhow!("decode sync_catalog body: {e}"))?;
    let connection: Box<EngineConnection> = serde_json::from_str(&wire.connection_json)
        .context("decode connection_json in sync_catalog binary body")?;

    let mut objects = Vec::with_capacity(wire.objects.len());
    for obj in wire.objects {
        objects.push(CatalogObject {
            schema: obj.schema,
            name: obj.name,
            kind: decode_catalog_kind(obj.kind)?,
            columns: obj
                .columns
                .into_iter()
                .map(|c| CatalogColumn {
                    name: c.name,
                    type_name: c.type_name,
                    ordinal: c.ordinal,
                    nullable: c.nullable,
                })
                .collect(),
            primary_key: obj.primary_key,
            foreign_keys: obj
                .foreign_keys
                .into_iter()
                .map(|fk| CatalogForeignKey {
                    columns: fk.columns,
                    target_schema: fk.target_schema,
                    target_table: fk.target_table,
                    target_columns: fk.target_columns,
                })
                .collect(),
            parameters: obj
                .parameters
                .into_iter()
                .map(|p| CatalogParameter {
                    name: p.name,
                    type_name: p.type_name,
                    is_output: p.is_output,
                })
                .collect(),
            definition: obj.definition,
            source: CatalogSource {
                kind: decode_source_kind(obj.source_kind)?,
                path: obj.source_path,
                create_date: obj.source_create_date,
                modify_date: obj.source_modify_date,
            },
        });
    }

    let coverage = match wire.coverage_json.as_deref() {
        Some(json) => serde_json::from_str(json)
            .context("decode coverage_json in sync_catalog binary body")?,
        None => sqlly_ai::catalog::SyncCoverage::FullSource,
    };
    Ok(EngineRequest::SyncCatalog {
        id: wire.id,
        connection,
        objects,
        coverage,
    })
}

/// Read the common u16 layout_version + string id prefix shared by all
/// binary request bodies. Returns `(id, reader)` on success.
pub(crate) fn read_request_header(body: &[u8]) -> Result<(String, BinaryFrameReader<'_>)> {
    let mut r = BinaryFrameReader::new(body);
    let version = r.read_u16_le("layout_version")?;
    if version != BINARY_REQUEST_LAYOUT_VERSION {
        anyhow::bail!(
            "unsupported binary request layout version {version} (expected {BINARY_REQUEST_LAYOUT_VERSION})"
        );
    }
    let id = r.read_string("id")?;
    Ok((id, r))
}

/// Read a connection JSON string from the reader and deserialize it into an
/// `EngineConnection`. Shared by all connection-bearing request decoders.
pub(crate) fn read_connection(r: &mut BinaryFrameReader) -> Result<Box<EngineConnection>> {
    let connection_json = r.read_string("connection_json")?;
    let connection: Box<EngineConnection> = serde_json::from_str(&connection_json)
        .context("decode connection_json in binary request body")?;
    Ok(connection)
}

pub(crate) fn decode_row_edit_binary(body: &[u8]) -> Result<EngineRequest> {
    let (id, mut r) = sqlly_remote::engine_protocol::layouts::read_request_header(body)
        .map_err(|e| anyhow::anyhow!("decode row_edit body: {e}"))?;
    let connection_json = r.read_string("connection_json")?;
    let connection: Box<EngineConnection> = serde_json::from_str(&connection_json)
        .context("decode connection_json in row_edit body")?;
    let op = r.read_string("op")?;
    let payload = r.read_string("payload")?;
    r.ensure_at_end()?;
    Ok(EngineRequest::RowEdit {
        id,
        connection,
        op,
        payload,
    })
}

pub(crate) fn decode_catalog_maintenance_binary(body: &[u8]) -> Result<EngineRequest> {
    let (id, mut r) = sqlly_remote::engine_protocol::layouts::read_request_header(body)
        .map_err(|e| anyhow::anyhow!("decode catalog_maintenance body: {e}"))?;
    let connection_json = r.read_string("connection_json")?;
    let connection: Box<EngineConnection> = serde_json::from_str(&connection_json)
        .context("decode connection_json in catalog_maintenance body")?;
    let options_json = r.read_string("options_json")?;
    let options: sqlly_ai::catalog::MaintenanceOptions = serde_json::from_str(&options_json)
        .context("decode options_json in catalog_maintenance body")?;
    r.ensure_at_end()?;
    Ok(EngineRequest::CatalogMaintenance {
        id,
        connection,
        options,
    })
}

#[derive(Debug, Deserialize)]
pub(crate) struct ReviewSqlArgs {
    sql: String,
    #[serde(default)]
    dialect: Option<String>,
    #[serde(default)]
    max_tokens: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct SummarizeQueryArgs {
    sql: String,
    #[serde(default)]
    result_columns: Vec<String>,
    #[serde(default)]
    row_count: Option<u64>,
    #[serde(default)]
    elapsed_ms: Option<u64>,
    #[serde(default)]
    max_tokens: Option<usize>,
}

pub(crate) fn decode_provider_status_binary(body: &[u8]) -> Result<EngineRequest> {
    let id = sqlly_remote::engine_protocol::layouts::decode_header_only(body)
        .map_err(|e| anyhow::anyhow!("decode provider_status body: {e}"))?;
    Ok(EngineRequest::ProviderStatus { id })
}

pub(crate) fn decode_review_sql_binary(body: &[u8]) -> Result<EngineRequest> {
    let (id, mut r) = sqlly_remote::engine_protocol::layouts::read_request_header(body)
        .map_err(|e| anyhow::anyhow!("decode review_sql body: {e}"))?;
    let connection = read_connection(&mut r)?;
    let args_json = r.read_string("args_json")?;
    let args: ReviewSqlArgs =
        serde_json::from_str(&args_json).context("decode args_json in review_sql body")?;
    let tenant_scope =
        sqlly_remote::engine_protocol::decode_tenant_scope_trailing_if_present(&mut r)
            .map_err(|e| anyhow::anyhow!("decode review_sql tenant scope: {e}"))?;
    r.ensure_at_end()?;
    Ok(EngineRequest::ReviewSql {
        id,
        connection,
        sql: args.sql,
        dialect: args.dialect,
        max_tokens: args.max_tokens,
        tenant_scope,
    })
}

pub(crate) fn decode_summarize_query_binary(body: &[u8]) -> Result<EngineRequest> {
    let (id, mut r) = sqlly_remote::engine_protocol::layouts::read_request_header(body)
        .map_err(|e| anyhow::anyhow!("decode summarize_query body: {e}"))?;
    let connection = read_connection(&mut r)?;
    let args_json = r.read_string("args_json")?;
    let args: SummarizeQueryArgs =
        serde_json::from_str(&args_json).context("decode args_json in summarize_query body")?;
    r.ensure_at_end()?;
    Ok(EngineRequest::SummarizeQuery {
        id,
        connection,
        sql: args.sql,
        result_columns: args.result_columns,
        row_count: args.row_count,
        elapsed_ms: args.elapsed_ms,
        max_tokens: args.max_tokens,
    })
}

pub(crate) fn decode_ping_binary(body: &[u8]) -> Result<EngineRequest> {
    let id = sqlly_remote::engine_protocol::layouts::decode_header_only(body)
        .map_err(|e| anyhow::anyhow!("decode ping body: {e}"))?;
    Ok(EngineRequest::Ping { id })
}

pub(crate) fn decode_shutdown_binary(body: &[u8]) -> Result<EngineRequest> {
    let id = sqlly_remote::engine_protocol::layouts::decode_header_only(body)
        .map_err(|e| anyhow::anyhow!("decode shutdown body: {e}"))?;
    Ok(EngineRequest::Shutdown { id })
}

pub(crate) fn decode_cancel_binary(body: &[u8]) -> Result<EngineRequest> {
    let (id, target_id) = sqlly_remote::engine_protocol::layouts::decode_cancel(body)
        .map_err(|e| anyhow::anyhow!("decode cancel body: {e}"))?;
    Ok(EngineRequest::Cancel { id, target_id })
}

pub(crate) fn decode_execute_query_binary(body: &[u8]) -> Result<EngineRequest> {
    let wire = sqlly_remote::engine_protocol::layouts::decode_execute_query(body)
        .map_err(|e| anyhow::anyhow!("decode execute_query body: {e}"))?;
    let connection: Box<EngineConnection> = serde_json::from_str(&wire.connection_json)
        .context("decode connection_json in binary request body")?;
    Ok(EngineRequest::ExecuteQuery {
        id: wire.id,
        connection,
        sql: wire.sql,
        row_buffer: wire.row_buffer.map(|v| v as usize),
        connection_key: wire.connection_key,
        cell_previews: wire.options
            & sqlly_remote::engine_protocol::EXECUTE_QUERY_OPTION_CELL_PREVIEWS
            != 0,
    })
}

pub(crate) fn decode_build_ai_cache_binary(body: &[u8]) -> Result<EngineRequest> {
    let (id, mut r) = read_request_header(body)?;
    let connection = read_connection(&mut r)?;
    r.ensure_at_end()?;
    Ok(EngineRequest::BuildAiCache { id, connection })
}

pub(crate) fn decode_invalidate_ai_cache_binary(body: &[u8]) -> Result<EngineRequest> {
    let (id, mut r) = read_request_header(body)?;
    let connection = read_connection(&mut r)?;
    r.ensure_at_end()?;
    Ok(EngineRequest::InvalidateAiCache { id, connection })
}

pub(crate) fn decode_ask_schema_binary(body: &[u8]) -> Result<EngineRequest> {
    let (id, mut r) = read_request_header(body)?;
    let connection = read_connection(&mut r)?;
    let question = r.read_string("question")?;
    let max_tokens = r.read_optional_u32("max_tokens")?.map(|v| v as usize);
    let system = r.read_optional_string("system")?;
    let purpose_byte = r.read_u8("purpose")?;
    let purpose = match purpose_byte {
        0 => SchemaPurpose::General,
        1 => SchemaPurpose::SqlGeneration,
        v => anyhow::bail!("unknown schema purpose 0x{v:02x} (expected 0 or 1)"),
    };
    r.ensure_at_end()?;
    Ok(EngineRequest::AskSchema {
        id,
        connection,
        question,
        max_tokens,
        system,
        purpose,
    })
}

pub(crate) fn decode_search_schema_binary(body: &[u8]) -> Result<EngineRequest> {
    let (id, mut r) = read_request_header(body)?;
    let connection = read_connection(&mut r)?;
    let query = r.read_string("query")?;
    let limit = r.read_optional_u32("limit")?.map(|v| v as usize);
    let object_type_count = r.read_count_with_min("object_type_count", 4)?;
    let mut object_types = Vec::with_capacity(object_type_count);
    for i in 0..object_type_count {
        object_types.push(r.read_string(&format!("object_types[{i}]"))?);
    }
    r.ensure_at_end()?;
    Ok(EngineRequest::SearchSchema {
        id,
        connection,
        query,
        limit,
        object_types,
    })
}

pub(crate) fn decode_find_join_path_binary(body: &[u8]) -> Result<EngineRequest> {
    let (id, mut r) = read_request_header(body)?;
    let connection = read_connection(&mut r)?;
    let source = r.read_string("source")?;
    let target = r.read_string("target")?;
    let max_depth = r.read_u32_le("max_depth")? as usize;
    let include_inferred = r.read_bool("include_inferred")?;
    let min_confidence = r.read_f64_le("min_confidence")?;
    r.ensure_at_end()?;
    Ok(EngineRequest::FindJoinPath {
        id,
        connection,
        source,
        target,
        max_depth,
        include_inferred,
        min_confidence,
    })
}

pub(crate) fn decode_build_embeddings_binary(body: &[u8]) -> Result<EngineRequest> {
    let (id, mut r) = read_request_header(body)?;
    let connection = read_connection(&mut r)?;
    r.ensure_at_end()?;
    Ok(EngineRequest::BuildEmbeddings { id, connection })
}

pub(crate) fn decode_generate_sql_binary(body: &[u8]) -> Result<EngineRequest> {
    let (id, mut r) = read_request_header(body)?;
    let connection = read_connection(&mut r)?;
    let question = r.read_string("question")?;
    let dialect = r.read_optional_string("dialect")?;
    let max_tokens = r.read_optional_u32("max_tokens")?.map(|v| v as usize);
    let system = r.read_optional_string("system")?;
    let max_attempts = r.read_optional_u32("max_attempts")?.map(|v| v as usize);
    let tenant_scope =
        sqlly_remote::engine_protocol::decode_tenant_scope_trailing_if_present(&mut r)
            .map_err(|e| anyhow::anyhow!("decode generate_sql tenant scope: {e}"))?;
    r.ensure_at_end()?;
    Ok(EngineRequest::GenerateSql {
        id,
        connection,
        question,
        dialect,
        max_tokens,
        system,
        max_attempts,
        tenant_scope,
    })
}

pub(crate) fn decode_validate_sql_binary(body: &[u8]) -> Result<EngineRequest> {
    let (id, mut r) = read_request_header(body)?;
    let connection = read_connection(&mut r)?;
    let sql = r.read_string("sql")?;
    let dialect = r.read_optional_string("dialect")?;
    let tenant_scope =
        sqlly_remote::engine_protocol::decode_tenant_scope_trailing_if_present(&mut r)
            .map_err(|e| anyhow::anyhow!("decode validate_sql tenant scope: {e}"))?;
    r.ensure_at_end()?;
    Ok(EngineRequest::ValidateSql {
        id,
        connection,
        sql,
        dialect,
        tenant_scope,
    })
}
