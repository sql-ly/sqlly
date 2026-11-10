use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use parking_lot::Mutex;
use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock, ServerCapabilities, ServerConfig},
    tool, tool_handler, tool_router,
    transport::stdio,
};
use sqlly_services::Services;
use sqlly_services::mcp::host::{FilePolicy, FileStore, McpHost, PolicySource};
use sqlly_services::mcp::proposals::{ProposalKind, sweep_expired};

use super::tools::{
    DescribeTableRequest, ListTablesRequest, OpenInAppRequest, ProposalStatusRequest,
    ProposeWriteRequest, ReadonlyQueryRequest, SchemaContextRequest,
};

#[derive(Clone)]
pub struct SqllyMcp {
    host: Arc<McpHost>,
    last_activity: Arc<Mutex<Instant>>,
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

impl SqllyMcp {
    fn new(host: Arc<McpHost>, last_activity: Arc<Mutex<Instant>>) -> Self {
        Self {
            host,
            last_activity,
            tool_router: Self::tool_router(),
        }
    }

    fn touch(&self) {
        *self.last_activity.lock() = Instant::now();
    }

    fn success(text: impl Into<String>) -> Result<CallToolResult, McpError> {
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    fn tool_error(message: impl Into<String>) -> Result<CallToolResult, McpError> {
        Ok(CallToolResult::error(vec![ContentBlock::text(message)]))
    }
}

#[tool_router]
impl SqllyMcp {
    #[tool(
        description = "List the database connections SQLLY exposes to this agent (name, engine, database, environment). Production connections are listed but refused for every other tool.",
        annotations(read_only_hint = true)
    )]
    async fn list_connections(&self) -> Result<CallToolResult, McpError> {
        self.touch();
        match self.host.list_connections().await {
            Ok(rows) => {
                let text = rows
                    .into_iter()
                    .map(|row| {
                        format!(
                            "{} id={} engine={} server={} database={} environment={} production={} read_only={}",
                            row.name,
                            row.id,
                            row.engine,
                            row.server,
                            row.database.as_deref().unwrap_or("(none)"),
                            row.environment,
                            row.production,
                            row.read_only
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                Self::success(if text.is_empty() {
                    "(no connections exposed by policy)".to_string()
                } else {
                    text
                })
            }
            Err(message) => Self::tool_error(message),
        }
    }

    #[tool(
        description = "List tables (schema.name) in a connection/database.",
        annotations(read_only_hint = true)
    )]
    async fn list_tables(
        &self,
        Parameters(req): Parameters<ListTablesRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.touch();
        match self
            .host
            .list_tables(
                &req.connection,
                req.database.as_deref(),
                req.schema.as_deref(),
            )
            .await
        {
            Ok(text) => Self::success(text),
            Err(message) => Self::tool_error(message),
        }
    }

    #[tool(
        description = "Columns, types, and nullability for one table.",
        annotations(read_only_hint = true)
    )]
    async fn describe_table(
        &self,
        Parameters(req): Parameters<DescribeTableRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.touch();
        match self
            .host
            .describe_table(&req.connection, req.database.as_deref(), &req.table)
            .await
        {
            Ok(text) => Self::success(text),
            Err(message) => Self::tool_error(message),
        }
    }

    #[tool(
        description = "Token-budgeted schema summary for prompting.",
        annotations(read_only_hint = true)
    )]
    async fn get_schema_context(
        &self,
        Parameters(req): Parameters<SchemaContextRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.touch();
        let budget = req.budget.unwrap_or(2000);
        match self
            .host
            .get_schema_context(&req.connection, req.database.as_deref(), budget)
            .await
        {
            Ok(text) => Self::success(text),
            Err(message) => Self::tool_error(message),
        }
    }

    #[tool(
        description = "Run ONE SELECT and return up to `limit` rows as a text table. Non-SELECT SQL is refused without executing, and the database runs the SELECT in a read-only transaction that is always rolled back.",
        annotations(read_only_hint = true)
    )]
    async fn run_readonly_query(
        &self,
        Parameters(req): Parameters<ReadonlyQueryRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.touch();
        match self
            .host
            .run_readonly_query(
                &req.connection,
                req.database.as_deref(),
                &req.sql,
                req.limit,
                req.cell_char_limit,
            )
            .await
        {
            Ok(windowed) => Self::success(format!(
                "{}\nrows_shown={} truncated_rows={} truncated_cells={}",
                windowed.text,
                windowed.rows_shown,
                windowed.truncated_rows,
                windowed.truncated_cells
            )),
            Err(message) => Self::tool_error(message),
        }
    }

    #[tool(
        description = "Open a query tab in the SQLLY app for this connection, optionally pre-filled with SQL. Nothing executes."
    )]
    async fn open_in_app(
        &self,
        Parameters(req): Parameters<OpenInAppRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.touch();
        match self.host.propose(
            ProposalKind::Open,
            &req.connection,
            req.database.as_deref(),
            req.sql.as_deref(),
        ) {
            Ok(proposal) => Self::success(format!(
                "opened in SQLLY (proposal id {}); if SQLLY is not running, start it — the request is kept for 10 minutes",
                proposal.id
            )),
            Err(message) => Self::tool_error(message),
        }
    }

    #[tool(
        description = "Propose a data-changing statement. It is never executed by this server: SQLLY shows the exact SQL to the user, who confirms or declines. Returns a proposal id; poll get_proposal_status.",
        annotations(destructive_hint = true)
    )]
    async fn propose_write(
        &self,
        Parameters(req): Parameters<ProposeWriteRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.touch();
        match self.host.propose(
            ProposalKind::Write,
            &req.connection,
            req.database.as_deref(),
            Some(req.sql.as_str()),
        ) {
            Ok(proposal) => Self::success(format!(
                "pending confirmation in SQLLY — proposal id {}; poll get_proposal_status",
                proposal.id
            )),
            Err(message) => Self::tool_error(message),
        }
    }

    #[tool(
        description = "Status of a proposal: pending | opened | executed | rejected | expired.",
        annotations(read_only_hint = true)
    )]
    async fn get_proposal_status(
        &self,
        Parameters(req): Parameters<ProposalStatusRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.touch();
        match self.host.proposal_status(req.id) {
            Ok(Some(proposal)) => {
                let note = proposal
                    .note
                    .as_deref()
                    .map(|n| format!(" note={n}"))
                    .unwrap_or_default();
                Self::success(format!("status={:?}{note}", proposal.status))
            }
            Ok(None) => Self::tool_error("proposal not found"),
            Err(message) => Self::tool_error(message),
        }
    }
}

#[tool_handler]
impl ServerHandler for SqllyMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "SQLLY exposes saved database connections read-only, and the database enforces it: every query runs in a read-only transaction that is rolled back (on SQL Server, a rolled-back transaction plus a statement screen; on SQLite the session is switched to query-only; on ClickHouse, which has no transactions, only the SELECT-only screen applies; on Redis each command is checked against a read-only allowlist). Use list_connections first. Writes are proposals the user confirms inside SQLLY.",
        )
    }
}

pub async fn run(idle_override: Option<u64>) -> anyhow::Result<()> {
    let services = Services::new();
    let host = Arc::new(McpHost::new(
        services,
        Arc::new(FileStore),
        Arc::new(FilePolicy),
    ));
    let policy = FilePolicy.load();
    let idle = idle_override
        .map(|override_secs| policy.idle_timeout_secs.min(override_secs))
        .unwrap_or(policy.idle_timeout_secs)
        .max(30);

    let last_activity = Arc::new(Mutex::new(Instant::now()));
    let server = SqllyMcp::new(host, last_activity.clone());
    let running = server.serve(stdio()).await?;
    let cancel = running.cancellation_token();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let _ = sweep_expired(Utc::now());
            if last_activity.lock().elapsed() > Duration::from_secs(idle) {
                tracing::info!("idle timeout; exiting");
                cancel.cancel();
                break;
            }
        }
    });

    running.waiting().await?;
    Ok(())
}
