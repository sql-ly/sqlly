#[cfg(not(target_arch = "wasm32"))]
mod imp {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use crate::agent_tools::ServicesToolHost;
    use chrono::Utc;
    use sqlly_ai::catalog::AgentToolHost;
    use sqlly_ai::catalog::card::estimate_tokens;
    use sqlly_core::CellValue;
    use sqlly_core::connection::{AuthenticationType, ConnectionProfile};
    use sqlly_core::redact_sql;
    use sqlly_intel::query::safety::{
        StatementKindHint, classify_select_only, classify_select_only_for, statement_kind_hint,
    };
    use uuid::Uuid;

    use crate::mcp::audit::{AuditApproval, AuditOutcome, AuditRecord};
    use crate::mcp::policy::{MAX_CELL_CHARS_HARD_CAP, McpPolicy};
    use crate::mcp::proposals::{
        Proposal, ProposalKind, new_proposal, read_proposal, write_proposal,
    };
    use crate::mcp::rate_limit::{RateClass, RateLimiter};
    use crate::mcp::store_view::{StoreView, hydrate_profile};
    use crate::mcp::windowing::{WindowSpec, Windowed, clamp_limit, render_table};
    use crate::metadata::MetadataNode;
    use crate::query::QueryEvent;
    use crate::services::Services;

    pub trait AuditSink: Send + Sync {
        fn record(&self, record: AuditRecord);
    }

    pub struct FileAuditSink;

    impl AuditSink for FileAuditSink {
        fn record(&self, record: AuditRecord) {
            if let Err(error) = crate::mcp::audit::append(&record) {
                tracing::warn!("mcp audit append failed: {error}");
            }
        }
    }

    #[cfg(test)]
    struct VecAudit(Arc<Mutex<Vec<AuditRecord>>>);

    #[cfg(test)]
    impl AuditSink for VecAudit {
        fn record(&self, record: AuditRecord) {
            self.0.lock().unwrap().push(record);
        }
    }

    struct RateLimitState {
        limits: (u32, u32, u32),
        limiter: RateLimiter,
    }

    pub trait StoreSource: Send + Sync {
        fn load(&self) -> Result<StoreView, String>;
    }

    pub trait PolicySource: Send + Sync {
        fn load(&self) -> McpPolicy;
    }

    pub struct FileStore;

    impl StoreSource for FileStore {
        fn load(&self) -> Result<StoreView, String> {
            StoreView::load()
        }
    }

    pub struct FilePolicy;

    impl PolicySource for FilePolicy {
        fn load(&self) -> McpPolicy {
            let env: Vec<(String, String)> = std::env::vars().collect();
            crate::mcp::policy::load_policy()
                .narrowed_by_env(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        }
    }

    #[derive(Debug, Clone)]
    pub struct ConnectionSummary {
        pub id: Uuid,
        pub name: String,
        pub engine: String,
        pub server: String,
        pub database: Option<String>,
        pub environment: String,
        pub production: bool,
        pub read_only: bool,
    }

    pub struct McpHost {
        services: Services,
        store: Arc<dyn StoreSource>,
        policy: Arc<dyn PolicySource>,
        audit: Arc<dyn AuditSink>,
        rate_limits: Mutex<RateLimitState>,
    }

    impl McpHost {
        pub fn new(
            services: Services,
            store: Arc<dyn StoreSource>,
            policy: Arc<dyn PolicySource>,
        ) -> Self {
            Self::with_audit(services, store, policy, Arc::new(FileAuditSink))
        }

        pub fn with_audit(
            services: Services,
            store: Arc<dyn StoreSource>,
            policy: Arc<dyn PolicySource>,
            audit: Arc<dyn AuditSink>,
        ) -> Self {
            let policy_loaded = policy.load();
            Self {
                services,
                store,
                policy,
                audit,
                rate_limits: Mutex::new(RateLimitState {
                    limits: (
                        policy_loaded.max_queries_per_minute,
                        policy_loaded.max_writes_per_minute,
                        policy_loaded.max_ddl_per_minute,
                    ),
                    limiter: RateLimiter::from_policy(&policy_loaded),
                }),
            }
        }

        fn sync_rate_limiter(&self, policy: &McpPolicy) {
            let new = (
                policy.max_queries_per_minute,
                policy.max_writes_per_minute,
                policy.max_ddl_per_minute,
            );
            let mut state = self.rate_limits.lock().unwrap();
            if state.limits != new {
                state.limits = new;
                state.limiter = RateLimiter::from_policy(policy);
            }
        }

        fn check_rate_limit(&self, class: RateClass) -> Result<(), String> {
            let policy = self.policy.load();
            self.sync_rate_limiter(&policy);
            let now = Instant::now();
            let limiter = self.rate_limits.lock().unwrap();
            if let Err(wait) = limiter.limiter.check(class, now) {
                let secs = wait.as_secs().max(1);
                return Err(format!("refused: rate limit — try again in {secs}s"));
            }
            Ok(())
        }

        fn audit_connection(&self, connection: Option<&str>) -> (Option<Uuid>, Option<String>) {
            let Some(name) = connection else {
                return (None, None);
            };
            let Ok(store) = self.store.load() else {
                return (None, None);
            };
            let Ok(profile) = resolve_connection(&store, name) else {
                return (None, None);
            };
            (Some(profile.id.0), Some(profile.name.clone()))
        }

        fn sql_hash(sql: &str) -> String {
            let mut hasher = DefaultHasher::new();
            sql.hash(&mut hasher);
            format!("{:016x}", hasher.finish())
        }

        fn sql_redacted(sql: &str) -> String {
            let redacted = redact_sql(sql);
            if redacted.len() > 2_000 {
                redacted[..2_000].to_string()
            } else {
                redacted
            }
        }

        #[allow(clippy::too_many_arguments)]
        fn emit_audit(
            &self,
            tool: &str,
            connection_id: Option<Uuid>,
            connection_name: Option<String>,
            database: Option<&str>,
            sql: Option<&str>,
            duration_ms: u64,
            rows: Option<usize>,
            outcome: AuditOutcome,
            proposal_id: Option<Uuid>,
            approval: Option<AuditApproval>,
        ) {
            self.audit.record(AuditRecord {
                ts: Utc::now(),
                tool: tool.into(),
                connection_id,
                connection_name,
                database: database.map(str::to_string),
                sql_hash: sql.map(Self::sql_hash),
                sql_redacted: sql.map(Self::sql_redacted),
                duration_ms: Some(duration_ms),
                rows,
                outcome,
                proposal_id,
                approval,
            });
        }

        async fn audited<T>(
            &self,
            tool: &str,
            connection: Option<&str>,
            database: Option<&str>,
            sql: Option<&str>,
            work: impl std::future::Future<Output = Result<(T, Option<usize>), String>>,
        ) -> Result<T, String> {
            let start = Instant::now();
            let (connection_id, connection_name) = self.audit_connection(connection);
            let result = work.await;
            let duration_ms = start.elapsed().as_millis() as u64;
            match result {
                Ok((value, rows)) => {
                    self.emit_audit(
                        tool,
                        connection_id,
                        connection_name,
                        database,
                        sql,
                        duration_ms,
                        rows,
                        AuditOutcome::Ok,
                        None,
                        None,
                    );
                    Ok(value)
                }
                Err(message) => {
                    let outcome = if message.starts_with("refused:") {
                        AuditOutcome::Refused(message.clone())
                    } else {
                        AuditOutcome::Error(message.clone())
                    };
                    self.emit_audit(
                        tool,
                        connection_id,
                        connection_name,
                        database,
                        sql,
                        duration_ms,
                        None,
                        outcome,
                        None,
                        None,
                    );
                    Err(message)
                }
            }
        }

        fn audited_sync<T>(
            &self,
            tool: &str,
            connection: Option<&str>,
            database: Option<&str>,
            sql: Option<&str>,
            work: impl FnOnce() -> Result<(T, Option<usize>), String>,
        ) -> Result<T, String> {
            let start = Instant::now();
            let (connection_id, connection_name) = self.audit_connection(connection);
            let result = work();
            let duration_ms = start.elapsed().as_millis() as u64;
            match result {
                Ok((value, rows)) => {
                    self.emit_audit(
                        tool,
                        connection_id,
                        connection_name,
                        database,
                        sql,
                        duration_ms,
                        rows,
                        AuditOutcome::Ok,
                        None,
                        None,
                    );
                    Ok(value)
                }
                Err(message) => {
                    let outcome = if message.starts_with("refused:") {
                        AuditOutcome::Refused(message.clone())
                    } else {
                        AuditOutcome::Error(message.clone())
                    };
                    self.emit_audit(
                        tool,
                        connection_id,
                        connection_name,
                        database,
                        sql,
                        duration_ms,
                        None,
                        outcome,
                        None,
                        None,
                    );
                    Err(message)
                }
            }
        }

        pub async fn list_connections(&self) -> Result<Vec<ConnectionSummary>, String> {
            self.audited("list_connections", None, None, None, async {
                self.check_rate_limit(RateClass::Query)?;
                let store = self.store.load()?;
                let policy = self.policy.load();
                let mut out = Vec::new();
                for profile in &store.profiles {
                    if !policy.allows_connection(profile.id.0) {
                        continue;
                    }
                    let production = store.production_guard(profile).is_production;
                    out.push(ConnectionSummary {
                        id: profile.id.0,
                        name: profile.name.clone(),
                        engine: format!("{:?}", profile.database_type),
                        server: profile.server.clone(),
                        database: profile.database.clone(),
                        environment: format!("{:?}", profile.environment),
                        production,
                        read_only: true,
                    });
                }
                Ok((out, None))
            })
            .await
        }

        pub async fn list_tables(
            &self,
            connection: &str,
            database: Option<&str>,
            schema: Option<&str>,
        ) -> Result<String, String> {
            self.audited("list_tables", Some(connection), database, None, async {
                self.check_rate_limit(RateClass::Query)?;
                let (profile, _) = self.resolve_runtime_profile(connection, database, true)?;
                let host = self.tool_host(profile);
                let text = host.list_tables(schema).await?;
                Ok((text, None))
            })
            .await
        }

        pub async fn describe_table(
            &self,
            connection: &str,
            database: Option<&str>,
            table: &str,
        ) -> Result<String, String> {
            self.audited("describe_table", Some(connection), database, None, async {
                self.check_rate_limit(RateClass::Query)?;
                let (profile, _) = self.resolve_runtime_profile(connection, database, true)?;
                let host = self.tool_host(profile);
                let text = host.get_columns(table).await?;
                Ok((text, None))
            })
            .await
        }

        pub async fn get_schema_context(
            &self,
            connection: &str,
            database: Option<&str>,
            budget_tokens: usize,
        ) -> Result<String, String> {
            self.audited(
                "get_schema_context",
                Some(connection),
                database,
                None,
                async {
                    self.check_rate_limit(RateClass::Query)?;
                    let budget = budget_tokens.clamp(256, 8_000);
                    let (profile, db) = self.resolve_runtime_profile(connection, database, true)?;
                    let tables = self
                        .services
                        .metadata
                        .list_tables(&profile, &db, None)
                        .await
                        .map_err(|error| error.to_string())?;
                    let mut names: Vec<(String, String)> = tables
                        .iter()
                        .map(|node| {
                            let schema = node
                                .schema
                                .clone()
                                .unwrap_or_else(|| default_schema(&profile));
                            (schema, node.name.clone())
                        })
                        .collect();
                    names.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
                    names.truncate(200);
                    let mut lines = Vec::new();
                    let mut tokens = 0usize;
                    let mut shown = 0usize;
                    for (schema, name) in &names {
                        let columns = self
                            .services
                            .metadata
                            .list_columns(&profile, &db, schema, name)
                            .await
                            .map_err(|error| error.to_string())?;
                        let line = format_schema_line(schema, name, &columns);
                        let next = tokens + estimate_tokens(&line);
                        if !lines.is_empty() && next > budget {
                            break;
                        }
                        tokens = next;
                        lines.push(line);
                        shown += 1;
                    }
                    if shown < names.len() {
                        let remaining = names.len() - shown;
                        lines.push(format!(
                            "… {remaining} more tables (raise budget or pass schema)"
                        ));
                    }
                    Ok((lines.join("\n"), None))
                },
            )
            .await
        }

        pub async fn run_readonly_query(
            &self,
            connection: &str,
            database: Option<&str>,
            sql: &str,
            limit: Option<usize>,
            cell_char_limit: Option<usize>,
        ) -> Result<Windowed, String> {
            self.audited(
                "run_readonly_query",
                Some(connection),
                database,
                Some(sql),
                async {
                    self.check_rate_limit(RateClass::Query)?;
                    let policy = self.policy.load();
                    let row_limit = clamp_limit(limit, policy.max_rows);
                    let cell_limit = cell_char_limit
                        .unwrap_or(policy.cell_char_limit)
                        .min(policy.cell_char_limit)
                        .min(MAX_CELL_CHARS_HARD_CAP);
                    let (profile, _) = self.resolve_runtime_profile(connection, database, true)?;
                    if profile.database_type.is_command_engine() {
                        // Redis: commands are judged by name, not by the SQL
                        // SELECT-only parser.
                        if let Some((line, name)) =
                            sqlly_intel::query::safety::redis::first_write_command(sql)
                        {
                            return Err(format!(
                                "refused: line {line}: {name} is not a read-only Redis command"
                            ));
                        }
                    } else {
                        // Judged as the connection's engine reads the SQL:
                        // every statement it would run, under every lexical
                        // reading (sql_mode, executable comments, …).
                        let classification = classify_select_only_for(profile.database_type, sql);
                        if !classification.select_only {
                            let reason = classification
                                .reason
                                .unwrap_or_else(|| "not a SELECT".into());
                            return Err(format!("refused: {reason}"));
                        }
                    }
                    let (columns, rows, more) =
                        self.collect_readonly_rows(&profile, sql, row_limit).await?;
                    let windowed = render_table(
                        &columns,
                        &rows,
                        more,
                        WindowSpec {
                            max_rows: row_limit,
                            cell_char_limit: cell_limit,
                        },
                    );
                    let rows_shown = windowed.rows_shown;
                    Ok((windowed, Some(rows_shown)))
                },
            )
            .await
        }

        pub fn propose(
            &self,
            kind: ProposalKind,
            connection: &str,
            database: Option<&str>,
            sql: Option<&str>,
        ) -> Result<Proposal, String> {
            let tool = match kind {
                ProposalKind::Write => "propose_write",
                ProposalKind::Open => "open_in_app",
            };
            self.audited_sync(tool, Some(connection), database, sql, || {
                match kind {
                    ProposalKind::Write => {
                        let policy = self.policy.load();
                        if !policy.allows_writes() {
                            return Err(
                                "refused: SQLLY MCP policy is read-only; change it under Settings ▸ AI ▸ MCP"
                                    .into(),
                            );
                        }
                        let sql =
                            sql.ok_or_else(|| "sql is required for write proposals".to_string())?;
                        let class = match statement_kind_hint(sql) {
                            StatementKindHint::Ddl => RateClass::Ddl,
                            _ => RateClass::Write,
                        };
                        self.check_rate_limit(class)?;
                        let classification = classify_select_only(sql);
                        if classification.select_only {
                            return Err(
                                "refused: SELECT statements must use run_readonly_query".into(),
                            );
                        }
                        let (profile, db) =
                            self.resolve_runtime_profile(connection, database, false)?;
                        if !policy.allows_writes_on(profile.id.0) {
                            return Err(
                                "refused: this connection is read-only for MCP; change it under Settings ▸ AI ▸ MCP"
                                    .into(),
                            );
                        }
                        let proposal =
                            new_proposal(kind, profile.id.0, Some(db), Some(sql.to_string()));
                        write_proposal(&proposal)?;
                        Ok((proposal, None))
                    }
                    ProposalKind::Open => {
                        let (profile, db) =
                            self.resolve_runtime_profile(connection, database, false)?;
                        let proposal =
                            new_proposal(kind, profile.id.0, Some(db), sql.map(str::to_string));
                        write_proposal(&proposal)?;
                        Ok((proposal, None))
                    }
                }
            })
        }

        pub fn proposal_status(&self, id: Uuid) -> Result<Option<Proposal>, String> {
            self.audited_sync("get_proposal_status", None, None, None, || {
                read_proposal(id).map(|proposal| (proposal, None))
            })
        }

        fn tool_host(&self, profile: ConnectionProfile) -> ServicesToolHost {
            ServicesToolHost::new(
                profile,
                self.services.metadata.clone(),
                self.services.queries.clone(),
            )
        }

        fn resolve_runtime_profile(
            &self,
            connection: &str,
            database: Option<&str>,
            refuse_production_for_metadata: bool,
        ) -> Result<(ConnectionProfile, String), String> {
            let store = self.store.load()?;
            let policy = self.policy.load();
            let profile = resolve_connection(&store, connection)?;
            if !policy.allows_connection(profile.id.0) {
                if policy.effective_mode(profile.id.0) == crate::mcp::policy::ConnectionMode::Locked
                {
                    return Err(
                        "refused: this connection is locked for MCP in Settings ▸ AI ▸ MCP".into(),
                    );
                }
                return Err("connection not allowed by the SQLLY MCP policy".into());
            }
            if store.production_guard(profile).is_production {
                return Err(
                    "refused: this connection is marked Production; the SQLLY MCP server never reads or writes production"
                        .into(),
                );
            }
            let _ = refuse_production_for_metadata;
            let database = database
                .map(str::to_string)
                .or_else(|| profile.database.clone())
                .filter(|db| !db.is_empty())
                .ok_or_else(|| "connect to a database first: pass database=…".to_string())?;
            let mut runtime = profile.clone();
            runtime.database = Some(database.clone());
            runtime.block_all_but_select = true;
            if matches!(
                runtime.authentication,
                AuthenticationType::EntraBrowser | AuthenticationType::EntraDeviceCode
            ) {
                let token = runtime
                    .runtime_entra_access_token
                    .clone()
                    .or_else(|| std::env::var("SQLLY_ENTRA_ACCESS_TOKEN").ok());
                if token.is_none() {
                    return Err(
                        "this connection needs an interactive Entra sign-in; open it in SQLLY first"
                            .into(),
                    );
                }
                runtime.runtime_entra_access_token = token;
                return Ok((runtime, database));
            }
            if runtime.authentication.is_cloud_iam() && runtime.runtime_cloud_iam_token.is_none() {
                if let Ok(token) = std::env::var("SQLLY_CLOUD_IAM_TOKEN") {
                    runtime.runtime_cloud_iam_token = Some(token);
                } else {
                    let runtime_ref = runtime.clone();
                    let token = tokio::task::block_in_place(|| {
                        tokio::runtime::Handle::current().block_on(async {
                            crate::cloud_auth::token_for_profile(&runtime_ref).await
                        })
                    })
                    .map_err(|error| error.to_string())?
                    .expect("iam profile");
                    runtime.runtime_cloud_iam_token = Some(token.value);
                }
                return Ok((runtime, database));
            }
            let runtime = hydrate_profile(runtime)?;
            Ok((runtime, database))
        }

        async fn collect_readonly_rows(
            &self,
            profile: &ConnectionProfile,
            sql: &str,
            limit: usize,
        ) -> Result<(Vec<String>, Vec<Vec<String>>, bool), String> {
            // On top of the SELECT-only parser gate (`block_all_but_select`
            // is already set on the runtime profile), the database itself
            // enforces read-only: a read-only transaction always rolled back,
            // `query_only` on SQLite, rollback wrap plus statement screen on
            // SQL Server.
            let mut profile = profile.clone();
            profile.enforce_read_only = true;
            let explain = |message: String| {
                sqlly_intel::query::safety::explain_read_only_refusal(
                    profile.database_type,
                    &message,
                )
                .unwrap_or(message)
            };
            // Gate here for a clear early refusal; the driver applies the
            // policy (and the read-only wrap) to the RAW SQL exactly once.
            sqlly_intel::query::safety::check_connection_policy_for(&profile, sql)
                .map_err(|error| error.to_string())?;
            let handle = self
                .services
                .queries
                .execute(&profile, sql, 256)
                .await
                .map_err(|error| explain(error.to_string()))?;
            let task_id = handle.task_id.clone();
            let mut events = handle.events;
            let mut columns: Vec<String> = Vec::new();
            let mut rows: Vec<Vec<String>> = Vec::new();
            let mut more = false;
            while let Some(event) = events.recv().await {
                match event {
                    QueryEvent::ResultSet {
                        index,
                        columns: cols,
                    } => {
                        if index > 0 {
                            more = true;
                            break;
                        }
                        columns = cols.iter().map(|c| c.name.clone()).collect();
                    }
                    QueryEvent::Rows { index, rows: batch } => {
                        if index > 0 {
                            more = true;
                            break;
                        }
                        for row in batch {
                            if rows.len() >= limit {
                                more = true;
                                let _ = self.services.queries.cancel(&task_id).await;
                                break;
                            }
                            rows.push(row.iter().map(cell_to_string).collect());
                        }
                        if more {
                            break;
                        }
                    }
                    QueryEvent::Error { message } => return Err(explain(message)),
                    QueryEvent::Completed { .. } | QueryEvent::Cancelled => break,
                    _ => {}
                }
            }
            Ok((columns, rows, more))
        }
    }

    fn resolve_connection<'a>(
        store: &'a StoreView,
        connection: &str,
    ) -> Result<&'a ConnectionProfile, String> {
        if let Ok(id) = Uuid::parse_str(connection)
            && let Some(profile) = store.profile(id)
        {
            return Ok(profile);
        }
        store
            .profile_by_name(connection)
            .ok_or_else(|| "unknown connection; call list_connections".to_string())
    }

    fn default_schema(profile: &ConnectionProfile) -> String {
        use sqlly_lang::sqlmodel::DatabaseType;
        match profile.database_type {
            DatabaseType::SqlServer | DatabaseType::AzureSqlDatabase => "dbo".into(),
            DatabaseType::Postgres => "public".into(),
            DatabaseType::MySql | DatabaseType::MariaDb => {
                profile.database.clone().unwrap_or_else(|| "mysql".into())
            }
            DatabaseType::Sqlite => "main".into(),
            other => other.default_schema().unwrap_or("dbo").to_string(),
        }
    }

    fn format_schema_line(schema: &str, name: &str, columns: &[MetadataNode]) -> String {
        let cols = columns
            .iter()
            .map(|col| format!("{} {}", col.name, col.detail.as_deref().unwrap_or("")))
            .collect::<Vec<_>>()
            .join(", ");
        format!("{schema}.{name}({cols})")
    }

    fn cell_to_string(cell: &CellValue) -> String {
        match cell {
            CellValue::Null => "NULL".into(),
            CellValue::Bool(b) => b.to_string(),
            CellValue::I64(n) => n.to_string(),
            CellValue::F64(n) => n.to_string(),
            CellValue::String(s) => s.clone(),
            CellValue::Bytes(b) => format!("<bytes {}>", b.len()),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::metadata::MetadataService;
        use crate::query::QueryExecutor;
        use crate::{
            admin_tools::DefaultAdminToolsService, catalog::DefaultCatalogService,
            connection::DefaultConnectionManager, intellisense::DefaultIntellisense,
            row_editing::DefaultRowEditingService, tds_pool::TdsPool,
        };
        use async_trait::async_trait;
        use sqlly_core::ObjectKind;
        use sqlly_core::connection::ConnectionEnvironment;
        use sqlly_core::{ConnectionId, SecretProvider, SecretRef};
        use sqlly_lang::sqlmodel::DatabaseType;
        use std::sync::Mutex;
        use tokio::sync::mpsc;

        struct MemStore(StoreView);
        impl StoreSource for MemStore {
            fn load(&self) -> Result<StoreView, String> {
                Ok(self.0.clone())
            }
        }

        struct MemPolicy(McpPolicy);
        impl PolicySource for MemPolicy {
            fn load(&self) -> McpPolicy {
                self.0.clone()
            }
        }

        // Test/diagnostic helper retained for future use.
        #[allow(dead_code)]
        struct CountingPolicy {
            loads: Mutex<Vec<McpPolicy>>,
            policies: Vec<McpPolicy>,
        }

        impl PolicySource for CountingPolicy {
            fn load(&self) -> McpPolicy {
                let index = self.loads.lock().unwrap().len();
                let policy = self
                    .policies
                    .get(index)
                    .or_else(|| self.policies.last())
                    .cloned()
                    .unwrap_or_default();
                self.loads.lock().unwrap().push(policy.clone());
                policy
            }
        }

        struct FakeMetadata {
            tables: Vec<MetadataNode>,
            columns: Vec<MetadataNode>,
        }

        #[async_trait]
        impl MetadataService for FakeMetadata {
            async fn list_schemas(
                &self,
                _profile: &ConnectionProfile,
                _database: &str,
            ) -> sqlly_core::StudioResult<Vec<String>> {
                Ok(vec![])
            }

            async fn list_tables(
                &self,
                _profile: &ConnectionProfile,
                _database: &str,
                _schema: Option<&str>,
            ) -> sqlly_core::StudioResult<Vec<MetadataNode>> {
                Ok(self.tables.clone())
            }

            async fn list_columns(
                &self,
                _profile: &ConnectionProfile,
                _database: &str,
                _schema: &str,
                _object: &str,
            ) -> sqlly_core::StudioResult<Vec<MetadataNode>> {
                Ok(self.columns.clone())
            }

            async fn list_views(
                &self,
                _profile: &ConnectionProfile,
                _database: &str,
                _schema: Option<&str>,
            ) -> sqlly_core::StudioResult<Vec<MetadataNode>> {
                Ok(vec![])
            }

            async fn list_programmable_objects(
                &self,
                _profile: &ConnectionProfile,
                _database: &str,
                _schema: Option<&str>,
            ) -> sqlly_core::StudioResult<Vec<MetadataNode>> {
                Ok(vec![])
            }

            async fn list_procedure_parameters(
                &self,
                _profile: &ConnectionProfile,
                _database: &str,
                _schema: &str,
                _object: &str,
            ) -> sqlly_core::StudioResult<Vec<crate::intellisense::procedures::ProcedureParameter>>
            {
                Ok(vec![])
            }

            async fn list_indexes(
                &self,
                _profile: &ConnectionProfile,
                _database: &str,
                _schema: &str,
                _table: &str,
            ) -> sqlly_core::StudioResult<Vec<MetadataNode>> {
                Ok(vec![])
            }

            async fn list_triggers(
                &self,
                _profile: &ConnectionProfile,
                _database: &str,
                _schema: &str,
                _table: &str,
            ) -> sqlly_core::StudioResult<Vec<MetadataNode>> {
                Ok(vec![])
            }

            async fn list_constraints(
                &self,
                _profile: &ConnectionProfile,
                _database: &str,
                _schema: &str,
                _table: &str,
            ) -> sqlly_core::StudioResult<Vec<MetadataNode>> {
                Ok(vec![])
            }

            async fn list_statistics(
                &self,
                _profile: &ConnectionProfile,
                _database: &str,
                _schema: &str,
                _table: &str,
            ) -> sqlly_core::StudioResult<Vec<MetadataNode>> {
                Ok(vec![])
            }
        }

        struct CapturingQueries {
            routes: Vec<(&'static str, usize)>,
            executed: Mutex<Vec<String>>,
            profiles: Mutex<Vec<ConnectionProfile>>,
            row_count: usize,
            long_cell: Option<String>,
        }

        #[async_trait]
        impl QueryExecutor for CapturingQueries {
            async fn execute(
                &self,
                profile: &ConnectionProfile,
                sql: &str,
                _row_buffer: usize,
            ) -> sqlly_core::StudioResult<crate::query::QueryHandle> {
                self.executed.lock().unwrap().push(sql.to_string());
                self.profiles.lock().unwrap().push(profile.clone());
                let (tx, rx) = mpsc::channel(16);
                let row_count = self
                    .routes
                    .iter()
                    .find(|(needle, _)| sql.contains(needle))
                    .map(|(_, count)| *count)
                    .unwrap_or(self.row_count);
                let long_cell = self.long_cell.clone();
                tokio::spawn(async move {
                    let _ = tx
                        .send(QueryEvent::ResultSet {
                            index: 0,
                            columns: vec![sqlly_core::ResultColumn {
                                name: "c".into(),
                                data_type: "text".into(),
                                nullable: true,
                            }],
                        })
                        .await;
                    for index in 0..row_count {
                        let value = long_cell.clone().unwrap_or_else(|| index.to_string());
                        let _ = tx
                            .send(QueryEvent::Rows {
                                index: 0,
                                rows: vec![vec![CellValue::String(value)]],
                            })
                            .await;
                    }
                    let _ = tx.send(QueryEvent::Completed { elapsed_ms: 1 }).await;
                });
                Ok(crate::query::QueryHandle {
                    task_id: "mcp-test".into(),
                    events: rx,
                })
            }

            async fn cancel(&self, _task_id: &str) -> sqlly_core::StudioResult<()> {
                Ok(())
            }
        }

        fn dev_profile() -> ConnectionProfile {
            let mut profile = ConnectionProfile::sql_login(
                "dev",
                "localhost",
                "sa",
                SecretRef {
                    provider: SecretProvider::InMemory,
                    key: "k".into(),
                },
            );
            profile.id =
                ConnectionId(Uuid::parse_str("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa").unwrap());
            profile.database = Some("app".into());
            profile.database_type = DatabaseType::SqlServer;
            profile.environment = ConnectionEnvironment::Dev;
            profile
        }

        fn prod_profile() -> ConnectionProfile {
            let mut profile = dev_profile();
            profile.id =
                ConnectionId(Uuid::parse_str("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb").unwrap());
            profile.name = "prod".into();
            profile.environment = ConnectionEnvironment::Production;
            profile
        }

        fn host_with(
            store: StoreView,
            policy: McpPolicy,
            metadata: Arc<dyn MetadataService>,
            queries: Arc<CapturingQueries>,
        ) -> (McpHost, Arc<Mutex<Vec<AuditRecord>>>) {
            let audit_records = Arc::new(Mutex::new(Vec::new()));
            let services = Services::from_parts(
                Arc::new(DefaultConnectionManager::new()),
                Arc::new(DefaultIntellisense),
                queries.clone() as Arc<dyn QueryExecutor>,
                Arc::new(DefaultCatalogService::new()),
                metadata,
                Arc::new(DefaultAdminToolsService),
                Arc::new(DefaultRowEditingService::new(TdsPool::new())),
            );
            let host = McpHost::with_audit(
                services,
                Arc::new(MemStore(store)),
                Arc::new(MemPolicy(policy)),
                Arc::new(VecAudit(audit_records.clone())),
            );
            (host, audit_records)
        }

        #[tokio::test]
        async fn unknown_connection_is_rejected_without_executing() {
            let queries = Arc::new(CapturingQueries {
                routes: vec![],
                executed: Mutex::new(vec![]),
                profiles: Mutex::new(vec![]),
                row_count: 0,
                long_cell: None,
            });
            let (host, _) = host_with(
                StoreView {
                    profiles: vec![dev_profile()],
                    ..Default::default()
                },
                McpPolicy::default(),
                Arc::new(FakeMetadata {
                    tables: vec![],
                    columns: vec![],
                }),
                queries.clone(),
            );
            let err = host
                .list_tables("missing", Some("app"), None)
                .await
                .unwrap_err();
            assert!(err.contains("unknown connection"));
            assert!(queries.executed.lock().unwrap().is_empty());
        }

        #[tokio::test]
        async fn policy_allowlist_blocks_other_connections() {
            let mut other = dev_profile();
            other.id = ConnectionId(Uuid::new_v4());
            other.name = "other".into();
            let allowed = dev_profile().id.0;
            let (host, _) = host_with(
                StoreView {
                    profiles: vec![dev_profile(), other],
                    ..Default::default()
                },
                McpPolicy {
                    connections: crate::mcp::policy::ConnectionScope::Allow(vec![allowed]),
                    ..Default::default()
                },
                Arc::new(FakeMetadata {
                    tables: vec![],
                    columns: vec![],
                }),
                Arc::new(CapturingQueries {
                    routes: vec![],
                    executed: Mutex::new(vec![]),
                    profiles: Mutex::new(vec![]),
                    row_count: 0,
                    long_cell: None,
                }),
            );
            let err = host
                .list_tables("other", Some("app"), None)
                .await
                .unwrap_err();
            assert!(err.contains("not allowed"));
        }

        #[tokio::test]
        async fn production_profile_refused_for_every_tool() {
            let queries = Arc::new(CapturingQueries {
                routes: vec![],
                executed: Mutex::new(vec![]),
                profiles: Mutex::new(vec![]),
                row_count: 0,
                long_cell: None,
            });
            let (host, _) = host_with(
                StoreView {
                    profiles: vec![prod_profile()],
                    ..Default::default()
                },
                McpPolicy {
                    mode: crate::mcp::policy::McpMode::ReadWriteConfirm,
                    ..Default::default()
                },
                Arc::new(FakeMetadata {
                    tables: vec![MetadataNode {
                        name: "t".into(),
                        schema: Some("dbo".into()),
                        kind: ObjectKind::Table,
                        detail: None,
                    }],
                    columns: vec![],
                }),
                queries.clone(),
            );
            assert!(
                host.list_tables("prod", Some("app"), None)
                    .await
                    .unwrap_err()
                    .contains("Production")
            );
            assert!(
                host.describe_table("prod", Some("app"), "dbo.t")
                    .await
                    .unwrap_err()
                    .contains("Production")
            );
            assert!(
                host.get_schema_context("prod", Some("app"), 500)
                    .await
                    .unwrap_err()
                    .contains("Production")
            );
            assert!(
                host.run_readonly_query("prod", Some("app"), "SELECT 1", None, None)
                    .await
                    .unwrap_err()
                    .contains("Production")
            );
            let open_err = host
                .propose(ProposalKind::Open, "prod", Some("app"), None)
                .unwrap_err();
            assert!(open_err.contains("Production"));
            let write_err = host
                .propose(
                    ProposalKind::Write,
                    "prod",
                    Some("app"),
                    Some("DELETE FROM t"),
                )
                .unwrap_err();
            assert!(write_err.contains("Production"));
            assert!(queries.executed.lock().unwrap().is_empty());
        }

        #[tokio::test]
        async fn readonly_query_refuses_delete_and_never_executes() {
            let queries = Arc::new(CapturingQueries {
                routes: vec![],
                executed: Mutex::new(vec![]),
                profiles: Mutex::new(vec![]),
                row_count: 0,
                long_cell: None,
            });
            let (host, _) = host_with(
                StoreView {
                    profiles: vec![dev_profile()],
                    ..Default::default()
                },
                McpPolicy::default(),
                Arc::new(FakeMetadata {
                    tables: vec![],
                    columns: vec![],
                }),
                queries.clone(),
            );
            let err = host
                .run_readonly_query("dev", Some("app"), "DELETE FROM t", None, None)
                .await
                .unwrap_err();
            assert!(err.contains("refused"));
            assert!(queries.executed.lock().unwrap().is_empty());
        }

        #[tokio::test]
        async fn readonly_query_runs_raw_sql_under_database_enforced_read_only() {
            let queries = Arc::new(CapturingQueries {
                routes: vec![],
                executed: Mutex::new(vec![]),
                profiles: Mutex::new(vec![]),
                row_count: 1,
                long_cell: None,
            });
            let (host, _) = host_with(
                StoreView {
                    profiles: vec![dev_profile()],
                    ..Default::default()
                },
                McpPolicy::default(),
                Arc::new(FakeMetadata {
                    tables: vec![],
                    columns: vec![],
                }),
                queries.clone(),
            );
            host.run_readonly_query("dev", Some("app"), "SELECT 1", None, None)
                .await
                .unwrap();
            // Raw SQL to the driver (it wraps exactly once) under a profile
            // carrying both the parser gate and the database-enforced mode.
            assert_eq!(queries.executed.lock().unwrap()[0], "SELECT 1");
            let ran_under = &queries.profiles.lock().unwrap()[0];
            assert!(ran_under.enforce_read_only);
            assert!(ran_under.block_all_but_select);
        }

        #[tokio::test]
        async fn readonly_query_screens_sql_server_side_effects_without_executing() {
            let queries = Arc::new(CapturingQueries {
                routes: vec![],
                executed: Mutex::new(vec![]),
                profiles: Mutex::new(vec![]),
                row_count: 0,
                long_cell: None,
            });
            let (host, audit) = host_with(
                StoreView {
                    profiles: vec![dev_profile()],
                    ..Default::default()
                },
                McpPolicy::default(),
                Arc::new(FakeMetadata {
                    tables: vec![],
                    columns: vec![],
                }),
                queries.clone(),
            );
            let err = host
                .run_readonly_query(
                    "dev",
                    Some("app"),
                    "SELECT * FROM OPENQUERY(linked, 'SELECT 1')",
                    None,
                    None,
                )
                .await
                .unwrap_err();
            assert!(err.contains("blocked: AI queries run read-only"), "{err}");
            assert!(queries.executed.lock().unwrap().is_empty());
            // Audited as an error outcome carrying the plain reason.
            let records = audit.lock().unwrap();
            assert!(records.iter().any(|record| {
                record.tool == "run_readonly_query"
                    && matches!(&record.outcome, AuditOutcome::Error(m) if m.contains("read-only"))
            }));
        }

        #[tokio::test]
        async fn readonly_query_caps_rows_at_policy() {
            let queries = Arc::new(CapturingQueries {
                routes: vec![("SELECT", 500)],
                executed: Mutex::new(vec![]),
                profiles: Mutex::new(vec![]),
                row_count: 500,
                long_cell: None,
            });
            let (host, _) = host_with(
                StoreView {
                    profiles: vec![dev_profile()],
                    ..Default::default()
                },
                McpPolicy::default(),
                Arc::new(FakeMetadata {
                    tables: vec![],
                    columns: vec![],
                }),
                queries,
            );
            let windowed = host
                .run_readonly_query("dev", Some("app"), "SELECT 1", Some(1000), None)
                .await
                .unwrap();
            assert_eq!(windowed.rows_shown, 100);
            assert!(windowed.truncated_rows);
        }

        #[test]
        fn write_proposal_refused_in_read_only_mode() {
            let (host, _) = host_with(
                StoreView {
                    profiles: vec![dev_profile()],
                    ..Default::default()
                },
                McpPolicy::default(),
                Arc::new(FakeMetadata {
                    tables: vec![],
                    columns: vec![],
                }),
                Arc::new(CapturingQueries {
                    routes: vec![],
                    executed: Mutex::new(vec![]),
                    profiles: Mutex::new(vec![]),
                    row_count: 0,
                    long_cell: None,
                }),
            );
            let err = host
                .propose(
                    ProposalKind::Write,
                    "dev",
                    Some("app"),
                    Some("UPDATE t SET x = 1"),
                )
                .unwrap_err();
            assert!(err.contains("read-only"));
        }

        #[tokio::test]
        async fn write_proposal_rejects_select_sql() {
            let (host, _) = host_with(
                StoreView {
                    profiles: vec![dev_profile()],
                    ..Default::default()
                },
                McpPolicy {
                    mode: crate::mcp::policy::McpMode::ReadWriteConfirm,
                    ..Default::default()
                },
                Arc::new(FakeMetadata {
                    tables: vec![],
                    columns: vec![],
                }),
                Arc::new(CapturingQueries {
                    routes: vec![],
                    executed: Mutex::new(vec![]),
                    profiles: Mutex::new(vec![]),
                    row_count: 0,
                    long_cell: None,
                }),
            );
            let err = host
                .propose(ProposalKind::Write, "dev", Some("app"), Some("SELECT 1"))
                .unwrap_err();
            assert!(err.contains("run_readonly_query"));
        }

        #[tokio::test]
        async fn runtime_profile_carries_block_all_but_select() {
            let queries = Arc::new(CapturingQueries {
                routes: vec![("SELECT", 1)],
                executed: Mutex::new(vec![]),
                profiles: Mutex::new(vec![]),
                row_count: 1,
                long_cell: None,
            });
            let (host, _) = host_with(
                StoreView {
                    profiles: vec![dev_profile()],
                    ..Default::default()
                },
                McpPolicy::default(),
                Arc::new(FakeMetadata {
                    tables: vec![],
                    columns: vec![],
                }),
                queries.clone(),
            );
            host.run_readonly_query("dev", Some("app"), "SELECT 1", None, None)
                .await
                .unwrap();
            assert!(queries.profiles.lock().unwrap()[0].block_all_but_select);
        }

        #[tokio::test]
        async fn every_tool_call_writes_one_audit_record() {
            let queries = Arc::new(CapturingQueries {
                routes: vec![("SELECT", 1)],
                executed: Mutex::new(vec![]),
                profiles: Mutex::new(vec![]),
                row_count: 1,
                long_cell: None,
            });
            let (host, audit) = host_with(
                StoreView {
                    profiles: vec![dev_profile()],
                    ..Default::default()
                },
                McpPolicy::default(),
                Arc::new(FakeMetadata {
                    tables: vec![],
                    columns: vec![],
                }),
                queries,
            );
            host.list_connections().await.unwrap();
            host.run_readonly_query("dev", Some("app"), "SELECT 1", None, None)
                .await
                .unwrap();
            let records = audit.lock().unwrap();
            assert_eq!(records.len(), 2);
            assert_eq!(records[0].tool, "list_connections");
            assert_eq!(records[1].tool, "run_readonly_query");
        }

        #[tokio::test]
        async fn refusal_is_audited_with_reason() {
            let (host, audit) = host_with(
                StoreView {
                    profiles: vec![dev_profile()],
                    ..Default::default()
                },
                McpPolicy::default(),
                Arc::new(FakeMetadata {
                    tables: vec![],
                    columns: vec![],
                }),
                Arc::new(CapturingQueries {
                    routes: vec![],
                    executed: Mutex::new(vec![]),
                    profiles: Mutex::new(vec![]),
                    row_count: 0,
                    long_cell: None,
                }),
            );
            host.run_readonly_query("dev", Some("app"), "DELETE FROM t", None, None)
                .await
                .unwrap_err();
            let records = audit.lock().unwrap();
            assert_eq!(records.len(), 1);
            assert!(matches!(records[0].outcome, AuditOutcome::Refused(_)));
        }

        #[tokio::test]
        async fn sql_is_redacted_and_hashed_in_audit() {
            let queries = Arc::new(CapturingQueries {
                routes: vec![("SELECT", 1)],
                executed: Mutex::new(vec![]),
                profiles: Mutex::new(vec![]),
                row_count: 1,
                long_cell: None,
            });
            let (host, audit) = host_with(
                StoreView {
                    profiles: vec![dev_profile()],
                    ..Default::default()
                },
                McpPolicy::default(),
                Arc::new(FakeMetadata {
                    tables: vec![],
                    columns: vec![],
                }),
                queries,
            );
            host.run_readonly_query("dev", Some("app"), "SELECT 'hunter2'", None, None)
                .await
                .unwrap();
            let records = audit.lock().unwrap();
            assert!(
                records[0]
                    .sql_redacted
                    .as_ref()
                    .unwrap()
                    .contains("<redacted>")
            );
            assert!(
                !records[0]
                    .sql_redacted
                    .as_ref()
                    .unwrap()
                    .contains("hunter2")
            );
            assert!(records[0].sql_hash.is_some());
        }

        #[tokio::test]
        async fn query_rate_limit_refuses_and_audits() {
            let queries = Arc::new(CapturingQueries {
                routes: vec![("SELECT", 1)],
                executed: Mutex::new(vec![]),
                profiles: Mutex::new(vec![]),
                row_count: 1,
                long_cell: None,
            });
            let (host, audit) = host_with(
                StoreView {
                    profiles: vec![dev_profile()],
                    ..Default::default()
                },
                McpPolicy {
                    max_queries_per_minute: 1,
                    ..Default::default()
                },
                Arc::new(FakeMetadata {
                    tables: vec![],
                    columns: vec![],
                }),
                queries,
            );
            host.run_readonly_query("dev", Some("app"), "SELECT 1", None, None)
                .await
                .unwrap();
            host.run_readonly_query("dev", Some("app"), "SELECT 1", None, None)
                .await
                .unwrap_err();
            assert_eq!(audit.lock().unwrap().len(), 2);
        }

        #[test]
        fn ddl_proposal_uses_ddl_bucket() {
            let (host, _) = host_with(
                StoreView {
                    profiles: vec![dev_profile()],
                    ..Default::default()
                },
                McpPolicy {
                    mode: crate::mcp::policy::McpMode::ReadWriteConfirm,
                    max_ddl_per_minute: 0,
                    ..Default::default()
                },
                Arc::new(FakeMetadata {
                    tables: vec![],
                    columns: vec![],
                }),
                Arc::new(CapturingQueries {
                    routes: vec![],
                    executed: Mutex::new(vec![]),
                    profiles: Mutex::new(vec![]),
                    row_count: 0,
                    long_cell: None,
                }),
            );
            let ddl_err = host
                .propose(
                    ProposalKind::Write,
                    "dev",
                    Some("app"),
                    Some("CREATE TABLE t (id int)"),
                )
                .unwrap_err();
            assert!(ddl_err.contains("rate limit"));
            host.propose(
                ProposalKind::Write,
                "dev",
                Some("app"),
                Some("INSERT INTO t VALUES (1)"),
            )
            .unwrap();
        }

        #[tokio::test]
        async fn locked_connection_refused_for_reads() {
            let id = dev_profile().id.0;
            let (host, _) = host_with(
                StoreView {
                    profiles: vec![dev_profile()],
                    ..Default::default()
                },
                McpPolicy {
                    connection_modes: std::collections::BTreeMap::from([(
                        id,
                        crate::mcp::policy::ConnectionMode::Locked,
                    )]),
                    ..Default::default()
                },
                Arc::new(FakeMetadata {
                    tables: vec![],
                    columns: vec![],
                }),
                Arc::new(CapturingQueries {
                    routes: vec![],
                    executed: Mutex::new(vec![]),
                    profiles: Mutex::new(vec![]),
                    row_count: 0,
                    long_cell: None,
                }),
            );
            let err = host
                .list_tables("dev", Some("app"), None)
                .await
                .unwrap_err();
            assert!(err.contains("locked"));
        }

        #[test]
        fn readonly_connection_refuses_write_proposal_under_global_rwc() {
            let id = dev_profile().id.0;
            let (host, _) = host_with(
                StoreView {
                    profiles: vec![dev_profile()],
                    ..Default::default()
                },
                McpPolicy {
                    mode: crate::mcp::policy::McpMode::ReadWriteConfirm,
                    connection_modes: std::collections::BTreeMap::from([(
                        id,
                        crate::mcp::policy::ConnectionMode::ReadOnly,
                    )]),
                    ..Default::default()
                },
                Arc::new(FakeMetadata {
                    tables: vec![],
                    columns: vec![],
                }),
                Arc::new(CapturingQueries {
                    routes: vec![],
                    executed: Mutex::new(vec![]),
                    profiles: Mutex::new(vec![]),
                    row_count: 0,
                    long_cell: None,
                }),
            );
            let err = host
                .propose(
                    ProposalKind::Write,
                    "dev",
                    Some("app"),
                    Some("UPDATE t SET x = 1"),
                )
                .unwrap_err();
            assert!(err.contains("read-only for MCP"));
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub use imp::*;
