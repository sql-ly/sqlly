use super::*;

#[derive(Debug, Deserialize)]
pub(crate) struct EngineConnection {
    pub(crate) server: String,
    #[serde(default = "default_port")]
    pub(crate) port: u16,
    pub(crate) database: Option<String>,
    #[serde(default = "default_auth")]
    pub(crate) auth: String,
    pub(crate) username: String,
    pub(crate) password: String,
    #[serde(default)]
    pub(crate) access_token: Option<String>,
    #[serde(default = "default_true")]
    pub(crate) encrypt: bool,
    #[serde(default)]
    pub(crate) trust_certificate: bool,
    #[serde(default)]
    pub(crate) timeout_seconds: Option<u64>,
    /// Safety policy forwarded by the client. When omitted (old clients),
    /// defaults to `false` so the engine never silently blocks statements the
    /// user intended to run — the constructor default for Entra (`true`) is
    /// overridden below in `into_profile`.
    #[serde(default)]
    pub(crate) block_all_but_select: bool,
    #[serde(default)]
    pub(crate) auto_wrap_rollback: bool,
    /// Opts `validate_sql` into unconstrained-mutation warnings (they are off
    /// by default). Old clients never send it.
    #[serde(default)]
    pub(crate) confirm_data_changes: bool,
    /// Engine family for multi-engine dispatch. Old clients never send it —
    /// they only ever spoke to SQL Server, which stays the default.
    #[serde(default)]
    pub(crate) database_type: Option<sqlly_core::DatabaseType>,
    /// Per-statement execution ceiling in seconds; absent/zero = no limit.
    #[serde(default)]
    pub(crate) statement_timeout_seconds: Option<u64>,
    /// Database-enforced read-only execution for SQL an AI/agent/MCP client
    /// composed (read-only transaction always rolled back; SQLite
    /// `query_only`; SQL Server rollback wrap plus statement screen). Old
    /// clients never send it, so it defaults off and the parser gate alone
    /// applies for them.
    #[serde(default)]
    pub(crate) enforce_read_only: bool,
    /// Proxy mode: the id of the proxied connection to open, from the
    /// catalog the engine was started with (`--proxy-credentials`). When the
    /// engine hosts a catalog this is REQUIRED and every other field of the
    /// request's connection is ignored — the browser never chooses a server
    /// or supplies a credential. Old clients never send it.
    #[serde(default)]
    pub(crate) proxy_profile: Option<uuid::Uuid>,
}

impl EngineConnection {
    pub(crate) fn into_profile(self) -> ConnectionProfile {
        let mut profile = match self.auth.as_str() {
            "entra-browser" => ConnectionProfile::azure_sql_entra_browser(
                &self.server,
                &self.server,
                self.database
                    .clone()
                    .unwrap_or_else(|| "master".to_string()),
            ),
            "entra-device-code" => ConnectionProfile::azure_sql_entra_device_code(
                &self.server,
                &self.server,
                self.database
                    .clone()
                    .unwrap_or_else(|| "master".to_string()),
            ),
            "aws-rds-iam" => ConnectionProfile::aws_rds_iam(
                &self.server,
                &self.server,
                self.port,
                self.database_type
                    .unwrap_or(sqlly_core::DatabaseType::Postgres),
                &self.username,
                String::new(),
            ),
            "gcp-cloud-sql-iam" => ConnectionProfile::gcp_cloud_sql_iam(
                &self.server,
                &self.server,
                self.port,
                self.database_type
                    .unwrap_or(sqlly_core::DatabaseType::Postgres),
                &self.username,
            ),
            // libSQL / Turso: the token travels in the password slot.
            "auth-token" => ConnectionProfile::auth_token(
                &self.server,
                &self.server,
                self.port,
                SecretRef {
                    provider: SecretProvider::InMemory,
                    key: self.password.clone(),
                },
            ),
            _ => ConnectionProfile::sql_login(
                &self.server,
                &self.server,
                self.username,
                SecretRef {
                    provider: SecretProvider::InMemory,
                    key: self.password,
                },
            ),
        };
        profile.port = self.port;
        profile.database = self.database;
        if profile.authentication.is_cloud_iam() {
            profile.runtime_cloud_iam_token = self.access_token;
        } else {
            profile.runtime_entra_access_token = self.access_token;
        }
        profile.encrypt = self.encrypt;
        profile.trust_server_certificate = self.trust_certificate;
        if let Some(seconds) = self.timeout_seconds {
            profile.timeout_seconds = seconds;
        }
        // Override the constructor's safety default (Entra profiles default to
        // block_all_but_select=true) with the client-provided policy. This is
        // the fix for Entra/Azure connections being unable to run non-SELECT
        // statements even when the user disabled "SELECT only" in the editor.
        profile.block_all_but_select = self.block_all_but_select;
        profile.enforce_read_only = self.enforce_read_only;
        profile.auto_wrap_rollback = self.auto_wrap_rollback;
        profile.confirm_data_changes = self.confirm_data_changes;
        if let Some(database_type) = self.database_type {
            profile.database_type = database_type;
        }
        profile.statement_timeout_seconds = self.statement_timeout_seconds;
        profile
    }
}

pub(crate) fn default_port() -> u16 {
    1433
}

pub(crate) fn default_true() -> bool {
    true
}

pub(crate) fn default_auth() -> String {
    "sql".to_string()
}

/// Stable fingerprint of a connection's identity (where + as-whom we connect),
/// used to scope the on-disk catalog. Delegates to the canonical
/// [`ConnectionProfile::catalog_identity`] (format frozen — catalog files are
/// keyed by it). Pool keys instead use [`ConnectionProfile::pool_identity`],
/// which additionally separates Entra accounts.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn connection_identity(profile: &ConnectionProfile) -> String {
    profile.catalog_identity()
}

// ---------------------------------------------------------------------------
// Schema catalog / LLM timeout helpers
// ---------------------------------------------------------------------------
//
// Catalog intelligence runs through [`sqlly_services::DefaultCatalogService`]
// (constructed once per engine process). These helpers bound LLM-backed schema
// requests on the wire.

/// Env var overriding the maximum time the engine waits for an LLM-backed
/// schema request (`ask_schema` / `generate_sql`) before it gives up and emits a
/// terminal `request_failed` frame. Defaults to [`DEFAULT_SCHEMA_LLM_TIMEOUT_SECS`].
pub(crate) const SCHEMA_LLM_TIMEOUT_ENV: &str = "SQLLY_SCHEMA_LLM_TIMEOUT_SECS";

/// Default upper bound for an LLM-backed schema request. Generous enough to
/// cover a cold MLX sidecar that is loading a model into memory, but bounded so
/// the app never hangs forever waiting on a wedged provider. The very first
/// call to a freshly selected MLX chat model can additionally trigger a
/// multi-GB model download; that case is expected to exceed this bound and the
/// resulting failure message tells the user to retry once the model is warm.
pub(crate) const DEFAULT_SCHEMA_LLM_TIMEOUT_SECS: u64 = 180;

/// Resolve the schema-LLM request timeout from the environment, falling back to
/// [`DEFAULT_SCHEMA_LLM_TIMEOUT_SECS`]. A value of `0` disables the bound.
pub(crate) fn schema_llm_timeout() -> Option<Duration> {
    let secs = std::env::var(SCHEMA_LLM_TIMEOUT_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_SCHEMA_LLM_TIMEOUT_SECS);
    if secs == 0 {
        None
    } else {
        Some(Duration::from_secs(secs))
    }
}

/// User-facing message emitted when a schema request exceeds [`schema_llm_timeout`].
pub(crate) fn schema_llm_timeout_message(action: &str, timeout: Duration) -> String {
    format!(
        "{action} timed out after {}s. The local AI model may still be loading \
         or downloading on its first use — wait a moment and try again. (Set \
         {SCHEMA_LLM_TIMEOUT_ENV} to adjust or 0 to disable this limit.)",
        timeout.as_secs()
    )
}

/// The engine's warm-connection pools — one per pooled driver family (TDS,
/// PostgreSQL wire, MySQL wire) — plus the server-side cancel hooks for
/// in-flight queries. SQLite and DuckDB open their file per request, and
/// ClickHouse, Oracle, libSQL / Turso and Redis open a client per request
/// through the same `sqlly_services` producer, so they need no pool here. The pools,
/// checkin rules, and stale-warm retry contract live in `sqlly_services` (the
/// same producer the direct app path uses), so bridged and direct connections
/// share one warm-pool contract: checkout removes the entry, clean completion
/// checks the client back in, and an errored/cancelled client is never
/// re-pooled.
#[derive(Clone)]
pub(crate) struct EnginePools {
    /// Proxy mode: the only connections this engine may open, keyed by the
    /// deterministic ids the relay advertised to the browser. `None` for an
    /// ordinary engine, which opens whatever connection each request carries.
    pub(crate) proxy: Option<Arc<sqlly_core::credential_proxy::ProxyCatalog>>,
    pub(crate) tds: sqlly_services::TdsPool,
    pub(crate) pg: sqlly_services::PgPool,
    pub(crate) mysql: sqlly_services::MySqlPool,
    /// Cancel hooks for in-flight queries, by request id. Fired (SQL Server
    /// `KILL`, PostgreSQL wire cancel, MySQL `KILL QUERY`, SQLite and DuckDB
    /// interrupt, ClickHouse `KILL QUERY`, Oracle break; libSQL drops the
    /// request and Redis drops the connection) before the local task is
    /// aborted so the server stops the statement too.
    pub(crate) query_cancels: Arc<Mutex<HashMap<String, sqlly_services::CancelSlot>>>,
}

impl EnginePools {
    /// Pools for an ordinary (non-proxy) engine; production goes through
    /// [`Self::with_proxy`] with the catalog the binary was started with.
    #[cfg(test)]
    pub(crate) fn new() -> Self {
        Self::with_proxy(None)
    }

    /// Pools for an engine that, when `proxy` is set, resolves every request's
    /// connection against that catalog instead of trusting the request.
    pub(crate) fn with_proxy(
        proxy: Option<Arc<sqlly_core::credential_proxy::ProxyCatalog>>,
    ) -> Self {
        let tds = sqlly_services::TdsPool::new();
        tds.spawn_reaper();
        let pg = sqlly_services::PgPool::new();
        pg.spawn_reaper();
        let mysql = sqlly_services::MySqlPool::new();
        mysql.spawn_reaper();
        Self {
            proxy,
            tds,
            pg,
            mysql,
            query_cancels: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The profile a request may open. Outside proxy mode this is the
    /// request's own connection. In proxy mode the request must name a
    /// catalog entry, whose full profile (server, credentials, tunnel, safety
    /// flags) replaces whatever the request carried — the client's copy is
    /// secret-free and untrusted by design. The one thing the request still
    /// chooses is the database, and only when the entry is server-level (no
    /// database of its own): the explorer lists every database such a login
    /// can reach, and without this every query on one of them ran in the
    /// login's default database. An entry that names a database stays pinned.
    ///
    /// The resolved profile's `block_all_but_select` is the effective
    /// SELECT-only policy: the entry's own flag or any of its client /
    /// project / environment labels' `only_allow_select`, so the engine
    /// enforces what the credential file promises whatever the browser
    /// claims (see [`labels_select_only`]).
    pub(crate) fn resolve_profile(
        &self,
        connection: EngineConnection,
    ) -> Result<ConnectionProfile, EngineError> {
        match (&self.proxy, connection.proxy_profile) {
            (None, None) => Ok(connection.into_profile()),
            (None, Some(_)) => Err(EngineError::unsupported(
                "this engine hosts no proxied connections; connect to a relay proxy",
            )),
            (Some(_), None) => Err(EngineError::protocol(
                "proxy mode: the request must name a proxied connection (proxy_profile)",
            )),
            (Some(catalog), Some(id)) => catalog
                .find(sqlly_core::ConnectionId(id))
                .cloned()
                .map(|mut profile| {
                    if profile.database.is_none() {
                        profile.database = connection
                            .database
                            .filter(|database| !database.trim().is_empty());
                    }
                    profile.block_all_but_select |= labels_select_only(catalog, &profile);
                    profile
                })
                .ok_or_else(|| {
                    EngineError::protocol(format!(
                        "proxy mode: no proxied connection with id {id}; the relay's credential file may have changed — reload"
                    ))
                }),
        }
    }
}

/// Whether any client, project or environment label the proxied `profile`
/// belongs to sets `only_allow_select`. Label names match the way the
/// credential file resolves them: case-insensitively, and the environment
/// through its entity name (`Production`, `Staging`, …).
pub(crate) fn labels_select_only(
    catalog: &sqlly_core::credential_proxy::ProxyCatalog,
    profile: &ConnectionProfile,
) -> bool {
    use sqlly_core::credential_proxy::ProxyLabel;
    let by_name = |labels: &[ProxyLabel], name: Option<&str>| -> bool {
        name.is_some_and(|name| {
            labels
                .iter()
                .filter(|label| label.name.eq_ignore_ascii_case(name))
                .any(|label| label.safety.only_allow_select == Some(true))
        })
    };
    by_name(&catalog.clients, profile.client.as_deref())
        || by_name(&catalog.projects, profile.project.as_deref())
        || profile
            .environment
            .find_entity(&catalog.environments, |label| label.name.as_str())
            .is_some_and(|label| label.safety.only_allow_select == Some(true))
}

/// The `request_failed` a proxy-mode engine answers when a connection's
/// SELECT-only policy refuses a request before anything runs.
pub(crate) fn read_only_policy_error(reason: Option<&str>) -> EngineError {
    match reason {
        Some(reason) if !reason.trim().is_empty() => EngineError::query(format!(
            "this connection is read-only by policy: only SELECT statements may run ({reason})"
        )),
        _ => EngineError::query(
            "this connection is read-only by policy: only SELECT statements may run",
        ),
    }
}

#[derive(Debug)]
pub(crate) enum EngineFrame {
    Ready {
        protocol_version: u32,
    },
    Pong {
        id: String,
    },
    RequestStarted {
        id: String,
    },
    Event {
        id: String,
        event: Event,
    },
    RowsBatch {
        id: String,
        result_set_index: usize,
        rows: Vec<Vec<CellValue>>,
    },
    /// Cells of the preceding `RowsBatch` frames cut to previews — sent only
    /// for an `execute_query` that asked for cell previews.
    TruncatedCells {
        id: String,
        result_set_index: usize,
        cells: Vec<sqlly_services::TruncatedCell>,
    },
    RequestCompleted {
        id: String,
    },
    RequestFailed {
        id: String,
        error: EngineError,
    },
    RequestCancelled {
        id: String,
        target_id: String,
    },
    ShutdownAck {
        id: String,
    },
    /// Result of a [`EngineRequest::Validate`]. This frame is NOT serialized as
    /// JSON like the others; [`encode_frame`] special-cases it into a compact
    /// flat binary payload (tag byte `0x01`) for zero-copy decoding on the
    /// Swift side. The `Serialize` derive is satisfied but never exercised.
    ValidationReport {
        id: String,
        diagnostics: Vec<WireDiag>,
        /// Spans (UTF-8 byte start/len) of references that resolved to a real
        /// object. The Swift side colors these as confirmed schema symbols.
        resolved: Vec<WireSpan>,
    },
    /// Result of [`EngineRequest::SyncCatalog`]: counts of
    /// added/changed/removed objects and how many AI cards were invalidated.
    /// Serialized as JSON (not the flat-binary validation layout).
    CatalogSyncReport {
        id: String,
        report: SyncReport,
    },
    /// Result of [`EngineRequest::CatalogMaintenance`], serialized as JSON.
    CatalogMaintenanceReport {
        id: String,
        report: sqlly_ai::catalog::MaintenanceReport,
    },
    /// Result of [`EngineRequest::ProviderStatus`], serialized as JSON.
    ProviderStatusReport {
        id: String,
        report: sqlly_services::providers::ProviderStatusReport,
    },
    /// Result of [`EngineRequest::ReviewSql`], serialized as JSON.
    SqlReviewReport {
        id: String,
        report: sqlly_ai::catalog::SqlReviewReport,
    },
    /// Result of [`EngineRequest::SummarizeQuery`], serialized as JSON.
    QuerySummaryReport {
        id: String,
        report: sqlly_ai::catalog::QuerySummaryReport,
    },
    /// Result of [`EngineRequest::RowEdit`]: the row-editing outcome for the
    /// requested op, serialized as JSON.
    RowEditReport {
        id: String,
        report: serde_json::Value,
    },
    /// Result of [`EngineRequest::BuildAiCache`]: how many stale cards were
    /// built and how many remain. Serialized as JSON.
    AiCacheBuildReport {
        id: String,
        report: BuildReport,
    },
    /// Result of [`EngineRequest::InvalidateAiCache`]: counts of AI-derived
    /// data wiped (cache entries invalidated + embeddings deleted).
    /// Serialized as JSON.
    InvalidateAiCacheReport {
        id: String,
        report: sqlly_ai::catalog::NukeAiDataReport,
    },
    /// Result of [`EngineRequest::AskSchema`]: the LLM provider's answer text
    /// plus provider metadata. Serialized as JSON.
    SchemaAnswer {
        id: String,
        response: LlmResponse,
    },
    /// Result of [`EngineRequest::SearchSchema`]. Serialized as JSON.
    SchemaSearchResults {
        id: String,
        results: Vec<SchemaSearchHit>,
    },
    /// Result of [`EngineRequest::FindJoinPath`]. Serialized as JSON.
    JoinPathReport {
        id: String,
        paths: Vec<sqlly_ai::catalog::JoinPath>,
    },
    /// Result of [`EngineRequest::BuildEmbeddings`]: how many embeddings were
    /// built and how many remain stale. Serialized as JSON.
    EmbeddingsBuildReport {
        id: String,
        report: EmbeddingBuildReport,
        /// Optional advisory: present when the embedding provider that was
        /// requested (e.g. MLX) was unavailable and a fallback (stub) was used.
        note: Option<String>,
    },
    /// Result of [`EngineRequest::GenerateSql`]: the generated SQL statement,
    /// its validation report, and grounding metadata. Serialized as JSON.
    SqlGenerationReport {
        id: String,
        report: sqlly_ai::catalog::SqlGenerationResult,
    },
    /// Result of [`EngineRequest::ValidateSql`]: the deterministic validation
    /// report for a SQL statement. Serialized as JSON.
    SqlValidationReport {
        id: String,
        report: sqlly_ai::catalog::SqlValidationResult,
    },
}

/// A resolved-symbol span carried in the validation report. Both fields are
/// UTF-8 byte offsets into the validated text.
#[derive(Debug, Serialize)]
pub(crate) struct WireSpan {
    pub(crate) start: u32,
    pub(crate) len: u32,
}

/// Flat, transport-ready diagnostic. Enum discriminants are pinned to explicit
/// `u8` values in [`wire_severity`]/[`wire_class`] so the wire format never
/// silently shifts if the source enums are reordered.
#[derive(Debug, Serialize)]
pub(crate) struct WireDiag {
    pub(crate) severity: u8,
    pub(crate) class: u8,
    /// UTF-8 byte offset of the span start.
    pub(crate) span_start: u32,
    /// Span length in UTF-8 bytes (`span.end - span.start`).
    pub(crate) span_len: u32,
    pub(crate) line: u32,
    pub(crate) col: u32,
    pub(crate) message: String,
}

/// Pin severity -> u8. Decl order in `validate.rs` is Error, Warning, Hint.
pub(crate) fn wire_severity(severity: Severity) -> u8 {
    match severity {
        Severity::Error => 0,
        Severity::Warning => 1,
        Severity::Hint => 2,
    }
}

/// Pin issue-class -> u8. Must match `IssueClass` decl order in `validate.rs`.
pub(crate) fn wire_class(class: IssueClass) -> u8 {
    match class {
        IssueClass::Syntax => 0,
        IssueClass::UnresolvedObject => 1,
        IssueClass::UnresolvedColumn => 2,
        IssueClass::AmbiguousReference => 3,
        IssueClass::TypeMismatch => 4,
        IssueClass::TypeRisk => 5,
        IssueClass::ArityMismatch => 6,
        IssueClass::CapabilityViolation => 7,
        IssueClass::ScopeInconsistency => 8,
        IssueClass::Incomplete => 9,
    }
}

pub(crate) type FrameSender = mpsc::Sender<EngineFrame>;
pub(crate) type ActiveTasks = Arc<Mutex<HashMap<String, JoinHandle<()>>>>;
