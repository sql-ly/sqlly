use clap::{Parser, Subcommand};
use sqlly_core::{
    AuthenticationType, ConnectionProfile, EventBus, EventPayload, RowReceiver, SecretProvider,
    SecretRef, SpillWriter,
};
use sqlly_engines::tds::{ConnectionService, SqlServerTransport, TiberiusTransport};
use std::time::Duration;

mod agent;
mod commands;
mod engine;
mod engine_error;
pub mod intellisense_cmd;
mod mcp;

use agent::context::ContextPurposeArg;
use agent::dispatch_agent;
use agent::format::OutputFormat;
use agent::schema::SchemaKindFilter;

#[derive(Parser)]
#[command(
    name = "sqlly",
    about = "SQLLY command line — run SQL, inspect schemas, and export LLM-ready context against saved connections",
    version,
    after_help = "Exit codes: 0 ok, 1 error, 2 usage, 3 policy refused. Pass --json for a machine-readable envelope on stdout."
)]
struct Cli {
    /// Force the JSON envelope on stdout for every command.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run SQL against a saved connection and print the results.
    Query {
        /// Saved connection name or id (see `connections list`).
        connection: String,
        #[arg(long)]
        database: Option<String>,
        /// SQL text. Mutually exclusive with --file; "-" reads stdin. May
        /// start with a hyphen (a leading `-- comment`, `-1 + …`).
        #[arg(long, conflicts_with = "file", allow_hyphen_values = true)]
        sql: Option<String>,
        #[arg(long)]
        file: Option<std::path::PathBuf>,
        #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
        format: OutputFormat,
        /// Stop after N rows per result set (0 = unlimited). Default 500.
        /// CSV, TSV and JSON Lines stream rows as they arrive (without
        /// --json); other formats hold the result in memory to lay it out.
        #[arg(long, default_value_t = 500)]
        limit: usize,
        /// Statement timeout in seconds (0 = no limit). Default 60.
        #[arg(long, default_value_t = 60)]
        timeout: u64,
        /// Allow statements other than SELECT (refused on production regardless).
        #[arg(long)]
        allow_writes: bool,
    },
    /// Inspect schema objects on a saved connection.
    Schema {
        #[command(subcommand)]
        action: AgentSchemaAction,
    },
    /// Export LLM-ready schema context from the cached catalog.
    Context {
        connection: String,
        #[arg(long)]
        database: Option<String>,
        #[arg(long)]
        budget_tokens: Option<usize>,
        #[arg(long, value_enum, default_value_t = ContextPurposeArg::General)]
        purpose: ContextPurposeArg,
    },
    /// List saved connections from the app store.
    Connections {
        #[command(subcommand)]
        action: AgentConnectionsAction,
    },
    /// Report CLI health: config paths, keyring, drivers, and saved connections.
    Doctor,
    /// Copy table rows (and optionally structure) from one saved connection to another.
    Transfer {
        /// Saved source connection name or id (see `connections list`).
        connection: String,
        #[arg(long)]
        database: Option<String>,
        /// Saved target connection name or id.
        #[arg(long)]
        target_profile: String,
        #[arg(long)]
        target_database: Option<String>,
        #[arg(long)]
        target_schema: Option<String>,
        #[arg(long, value_parser = ["create", "append", "truncate", "upsert"])]
        mode: String,
        #[arg(long, default_value = "as-is", value_parser = ["as-is", "lower", "upper"])]
        casing: String,
        /// Table to copy (`schema.name` or bare name; default schema `dbo`). Repeat for multiple tables.
        #[arg(long, required = true)]
        table: Vec<String>,
    },
    /// Stream a .sql file to a connection statement by statement (GO batches on SQL Server).
    RunFile {
        /// Saved connection name or id (see `connections list`).
        connection: String,
        #[arg(long)]
        database: Option<String>,
        #[arg(long)]
        file: std::path::PathBuf,
        /// stop | continue (default: stop)
        #[arg(long, default_value = "stop")]
        on_error: String,
        /// none | single (default: none)
        #[arg(long, default_value = "none")]
        transaction: String,
        #[arg(long)]
        coalesce_inserts: bool,
        /// MySQL/MariaDB DEFINER clauses: keep | strip | current-user (default: keep)
        #[arg(long, default_value = "keep")]
        definer: String,
    },
    /// Write a whole database as a SQL dump: the portable built-in dump, or pg_dump/mysqldump with --native.
    DumpDatabase {
        /// Saved connection name or id (see `connections list`).
        connection: String,
        /// Database name to dump.
        database: String,
        /// Output file or directory path.
        output: std::path::PathBuf,
        #[arg(long, value_enum, default_value_t = commands::DumpContentArg::StructureAndData)]
        content: commands::DumpContentArg,
        #[arg(long)]
        per_object: bool,
        #[arg(long)]
        drop_if_exists: bool,
        #[arg(long, default_value_t = 500)]
        batch_size: usize,
        #[arg(long)]
        no_views: bool,
        #[arg(long)]
        no_routines: bool,
        #[arg(long)]
        no_triggers: bool,
        #[arg(long)]
        no_sequences: bool,
        /// Restrict the dump to a schema. Repeat for multiple schemas.
        #[arg(long)]
        schema: Vec<String>,
        /// Use the engine's vendor tool (pg_dump / mysqldump) instead of the
        /// portable SQL dump. One file only (--per-object and --batch-size are
        /// refused); requires the tool on PATH or in the usual install
        /// locations.
        #[arg(long)]
        native: bool,
        /// pg_dump output format (PostgreSQL, --native only).
        #[arg(long, value_enum, default_value_t = commands::PgFormatArg::Custom)]
        pg_format: commands::PgFormatArg,
        /// Omit pg_dump's --no-owner (keep ownership statements).
        #[arg(long)]
        include_owner: bool,
        /// Omit mysqldump's --single-transaction.
        #[arg(long)]
        no_single_transaction: bool,
        /// Extra mysqldump option (--native, MySQL/MariaDB), e.g.
        /// `--mysqldump-option=--skip-comments`. Repeat for more; options that
        /// set the output, credentials or server are refused.
        #[arg(long, allow_hyphen_values = true)]
        mysqldump_option: Vec<String>,
        /// Remove DEFINER clauses from the mysqldump output (--native,
        /// MySQL/MariaDB).
        #[arg(long)]
        strip_definer: bool,
        /// PostgreSQL portable dumps: follow each object's CREATE with the
        /// GRANT / REVOKE statements that recreate its privileges (pg_dump
        /// writes its own in --native mode).
        #[arg(long)]
        include_privileges: bool,
    },
    /// Restore a PostgreSQL custom/tar archive through pg_restore. Plain-SQL
    /// files (any engine) belong to `run-file` and are refused with a hint.
    RestoreFile {
        /// Saved connection name or id (see `connections list`).
        connection: String,
        /// Database to restore into.
        database: String,
        /// Backup file to restore.
        input: std::path::PathBuf,
        /// Drop objects before recreating them (pg_restore --clean).
        #[arg(long)]
        clean: bool,
        /// With --clean, use IF EXISTS when dropping (pg_restore --if-exists).
        #[arg(long)]
        if_exists: bool,
        /// Run the restore as a single transaction (pg_restore).
        #[arg(long)]
        single_transaction: bool,
        /// Skip ownership commands (pg_restore --no-owner).
        #[arg(long)]
        no_owner: bool,
    },
    /// Persistent framed IPC engine used by the macOS app for low-latency queries.
    #[command(hide = true)]
    Engine {
        /// Proxy mode: a SQLLY credential file (`format: sqlly-credentials`)
        /// whose connections are the ONLY ones this engine will open. Requests
        /// must name one of them by id (`proxy_profile`) and carry no
        /// credentials of their own. Set by `sqlly-relay` for browser sessions.
        #[arg(long)]
        proxy_credentials: Option<std::path::PathBuf>,
    },
    /// Model Context Protocol server over stdio for external coding agents
    /// (Claude Code, Cursor, …). Read-only by default; policy file under
    /// Settings ▸ AI ▸ MCP. Never prints secrets; logs go to stderr only.
    Mcp {
        /// Exit after this many seconds without a request (overrides the policy file only downward).
        #[arg(long)]
        idle_timeout_secs: Option<u64>,
    },
    #[command(hide = true)]
    Profile(ConnectionArgs),
    #[command(hide = true)]
    Connect(ConnectionArgs),
    #[command(hide = true)]
    Disconnect(ConnectionArgs),
    #[command(hide = true)]
    TestConnection(ConnectionArgs),
    #[command(hide = true)]
    ListDatabases(ConnectionArgs),
    #[command(name = "query-legacy", hide = true)]
    QueryLegacy {
        #[command(flatten)]
        connection: ConnectionArgs,
        #[arg(short, long)]
        text: String,
        /// When set, rows are streamed over a bounded (back-pressured) channel
        /// and spilled to disk under `<data_dir>/SQLLY/spill/` (e.g.
        /// `~/Library/Application Support/SQLLY/spill/` on macOS) instead of
        /// being printed inline, so neither the CLI nor the caller materializes
        /// the full result set in memory. Lifecycle events and a final
        /// per-result-set summary are printed to stdout; read row pages back
        /// with `result-page`.
        #[arg(long)]
        spill: bool,
        /// Channel buffer size (number of row batches) governing backpressure
        /// when `--spill` is set. The producer blocks once this many batches are
        /// queued, guaranteeing zero dropped rows.
        #[arg(long, default_value_t = 1024)]
        spill_buffer: usize,
    },
    #[command(hide = true)]
    CancelTest {
        #[command(flatten)]
        connection: ConnectionArgs,
        #[arg(long, default_value = "WAITFOR DELAY '00:00:30'; SELECT 1")]
        text: String,
        #[arg(long, default_value_t = 1)]
        after_seconds: u64,
    },
    #[command(hide = true)]
    RootNodes,
    #[command(hide = true)]
    Quote { identifier: String },
    /// Object Explorer metadata tree (server roots, db folders, children, filter).
    #[command(hide = true)]
    Explorer {
        #[command(subcommand)]
        action: ExplorerAction,
    },
    #[command(hide = true)]
    ServersExplorer {
        #[arg(long)]
        fixture: String,
    },
    #[command(hide = true)]
    ServersExplorerScan {
        #[arg(long)]
        request: String,
    },
    #[command(hide = true)]
    ObjectProperties {
        #[arg(long)]
        kind: String,
        #[arg(long)]
        name: String,
        #[arg(long)]
        schema: Option<String>,
    },
    #[command(hide = true)]
    Plan {
        #[command(subcommand)]
        action: PlanAction,
    },
    #[command(hide = true)]
    Backup {
        #[arg(long)]
        database: String,
        #[arg(long)]
        destination: String,
        #[arg(long, default_value = "full")]
        kind: String,
        #[arg(long)]
        compress: bool,
        #[arg(long)]
        checksum: bool,
        #[arg(long)]
        copy_only: bool,
    },
    #[command(hide = true)]
    Restore {
        #[arg(long)]
        database: String,
        #[arg(long)]
        source: String,
        #[arg(long)]
        replace: bool,
        #[arg(long, default_value = "true")]
        recovery: std::primitive::bool,
    },
    #[command(hide = true)]
    Activity {
        #[command(subcommand)]
        action: ActivityAction,
    },
    #[command(hide = true)]
    Logs { kind: String },
    #[command(hide = true)]
    Table {
        #[command(subcommand)]
        action: TableAction,
    },
    #[command(hide = true)]
    QueryStore {
        #[command(subcommand)]
        action: QueryStoreAction,
    },
    #[command(hide = true)]
    Xevents {
        kind: String,
        #[arg(long)]
        session: Option<String>,
    },
    #[command(hide = true)]
    Report { name: String },
    #[command(hide = true)]
    Import {
        schema: String,
        table: String,
        file: String,
        #[arg(long)]
        header: bool,
    },
    #[command(hide = true)]
    Export {
        sql: String,
        file: String,
        #[arg(long)]
        header: bool,
    },
    #[command(hide = true)]
    Agent {
        #[command(subcommand)]
        action: AgentAction,
    },
    #[command(hide = true)]
    Security {
        #[command(subcommand)]
        action: SecurityAction,
    },
    #[command(hide = true)]
    Designer {
        schema: String,
        table: String,
        /// Repeatable column spec, e.g. `--column id:int:pk:identity`.
        #[arg(long = "column")]
        columns: Vec<String>,
    },
    #[command(hide = true)]
    DesignerIndex {
        schema: String,
        table: String,
        name: String,
        #[arg(long = "column")]
        columns: Vec<String>,
        #[arg(long = "include")]
        include_columns: Vec<String>,
        #[arg(long)]
        unique: bool,
    },
    #[command(hide = true)]
    DesignerRelationship {
        name: String,
        #[arg(long)]
        child_schema: String,
        #[arg(long)]
        child_table: String,
        #[arg(long = "child-column")]
        child_columns: Vec<String>,
        #[arg(long)]
        parent_schema: String,
        #[arg(long)]
        parent_table: String,
        #[arg(long = "parent-column")]
        parent_columns: Vec<String>,
    },
    #[command(hide = true)]
    SchemaCompare {
        #[arg(long)]
        source: String,
        #[arg(long)]
        target: String,
    },
    #[command(hide = true)]
    RegisteredServers {
        #[command(subcommand)]
        action: RegisteredAction,
    },
    #[command(hide = true)]
    Templates {
        #[command(subcommand)]
        action: TemplateAction,
    },
    #[command(hide = true)]
    Recovery {
        #[command(subcommand)]
        action: RecoveryAction,
    },
    #[command(hide = true)]
    Preferences {
        #[command(subcommand)]
        action: PreferencesAction,
    },
    #[command(hide = true)]
    Parameters {
        #[command(subcommand)]
        action: ParameterAction,
    },
    #[command(hide = true)]
    Safety {
        #[command(subcommand)]
        action: SafetyAction,
    },
    #[command(hide = true)]
    Azure {
        #[command(subcommand)]
        action: AzureAction,
    },
    #[command(hide = true)]
    ResultWindow {
        /// Rows per page.
        #[arg(long, default_value_t = 1000)]
        page_size: usize,
        /// Total rows available.
        #[arg(long)]
        total_rows: usize,
        /// Zero-based page index to resolve.
        #[arg(long, default_value_t = 0)]
        page: usize,
    },
    #[command(hide = true)]
    ResultPage {
        /// Spill file written by `query-legacy --spill`.
        #[arg(long)]
        spill: std::path::PathBuf,
        /// Absolute index of the first row in the page.
        #[arg(long, default_value_t = 0)]
        offset: usize,
        /// Number of rows in the page.
        #[arg(long, default_value_t = 1000)]
        len: usize,
    },
    #[command(hide = true)]
    Connection {
        #[command(subcommand)]
        action: ConnectionAction,
    },
    #[command(hide = true)]
    Parse {
        /// SQL text to parse.
        #[arg(long)]
        sql: String,
    },
    #[command(hide = true)]
    Intellisense {
        #[command(subcommand)]
        action: IntellisenseAction,
    },
}

#[derive(Subcommand)]
enum AgentConnectionsAction {
    /// List saved connections.
    List {
        #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
        format: OutputFormat,
    },
}

#[derive(Subcommand)]
enum AgentSchemaAction {
    /// List tables, views, and programmable objects.
    List {
        connection: String,
        #[arg(long)]
        database: Option<String>,
        #[arg(long)]
        schema: Option<String>,
        #[arg(long, value_enum, default_value_t = SchemaKindFilter::All)]
        kind: SchemaKindFilter,
        #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
        format: OutputFormat,
    },
    /// Describe one object's columns, keys, and indexes.
    Describe {
        connection: String,
        object: String,
        #[arg(long)]
        database: Option<String>,
        #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
        format: OutputFormat,
    },
}

#[derive(Subcommand)]
enum IntellisenseAction {
    /// Complete at a cursor position. Reads the request JSON from --request
    /// or, when omitted, from stdin; prints a JSON item list.
    Complete {
        #[arg(long)]
        request: Option<String>,
        /// Append each request and its response to a diagnostic log so a
        /// crash that produces no response leaves a request line with no
        /// matching response line. The log path can also be set via the
        /// SQLLY_INTELLISENSE_LOG env var (which enables logging on its own).
        #[arg(long)]
        log_requests: bool,
    },
    /// Complete via the shared `sqlly-services` enriched pipeline (context,
    /// auto-trigger gate, policy, hints, procedure snippets, rule filtering).
    /// Same JSON request shape as `complete`, plus optional `trigger`,
    /// `manual`, `rules_jsonc`, and `parameters` fields. Returns a richer
    /// response with `replace_start`, `replace_end`, `should_render`,
    /// `reason`, and debug metadata.
    CompleteEnriched {
        #[arg(long)]
        request: Option<String>,
        #[arg(long)]
        log_requests: bool,
    },
}

#[derive(Subcommand)]
enum PreferencesAction {
    /// Print the default preferences document.
    Default,
    /// Normalize a preferences JSON file (clamp ranges, fill defaults).
    Normalize {
        #[arg(long)]
        file: String,
    },
}

#[derive(Subcommand)]
enum ConnectionAction {
    /// Render a secret-free, validated connection preview.
    Preview {
        name: String,
        server: String,
        /// Target database (defaults to master for Azure modes).
        #[arg(long)]
        database: Option<String>,
        /// Auth mode: sql | entra-browser | entra-device-code.
        #[arg(long, default_value = "sql")]
        auth: String,
    },
}

#[derive(Subcommand)]
enum ParameterAction {
    /// Print detected query parameters as JSON.
    Detect {
        #[arg(long)]
        sql: String,
    },
}

#[derive(Subcommand)]
enum SafetyAction {
    /// Preview how the connection safety policy would transform a SQL batch.
    /// Reports whether it is allowed, the block reason + offending span, and the
    /// rollback-wrapped effective SQL — all without touching a server.
    Preview {
        #[arg(long)]
        sql: String,
        /// Data environment: LOCAL | DEV | TEST | STAGING | PRODUCTION.
        #[arg(long, default_value = "DEV")]
        environment: String,
        /// Block all statements except SELECT.
        #[arg(long)]
        block_all_but_select: bool,
        /// Wrap execution in a transaction that rolls back.
        #[arg(long)]
        auto_wrap_rollback: bool,
    },
}

#[derive(Subcommand)]
enum AzureAction {
    /// Add or update an Entra account profile in a registry JSON file.
    AccountAdd {
        #[arg(long)]
        registry: Option<String>,
        #[arg(long)]
        username: String,
        #[arg(long)]
        tenant_id: String,
        #[arg(long)]
        home_account_id: String,
        #[arg(long)]
        display_name: Option<String>,
    },
    /// List Entra accounts from a registry JSON file.
    AccountsList {
        #[arg(long)]
        registry: String,
    },
    /// Remove an Entra account from a registry JSON file.
    AccountRemove {
        #[arg(long)]
        registry: String,
        #[arg(long)]
        account_id: String,
    },
    /// Parse Azure REST fixture JSON into discoverable SQL database nodes.
    DiscoverFixture {
        #[arg(long)]
        fixture: String,
    },
    /// Build the Servers Explorer tree (account → subscription → server → db)
    /// from a discovery fixture and print every node as JSON (CLI smoke).
    DiscoverTree {
        #[arg(long)]
        fixture: String,
    },
    /// Parse one level of raw ARM JSON (fetched live by the app) into the
    /// Servers Explorer child nodes for the expanded parent. Powers lazy
    /// account → subscription → server → database expansion.
    ExplorerChildren {
        /// Path to a JSON file holding the per-level request envelope
        /// (`{level, account_id, …, arm_json}`).
        #[arg(long)]
        file: String,
    },
}

#[derive(Subcommand)]
enum RecoveryAction {
    /// Wrap a JSON array of tabs into a canonical versioned recovery snapshot.
    Snapshot {
        /// Path to a JSON file of `[{title, sql, file_path?, dirty?}]`.
        #[arg(long)]
        tabs: String,
    },
    /// List recoverable (dirty) tabs from a stored snapshot JSON file.
    Recoverable {
        #[arg(long)]
        snapshot: String,
    },
}

#[derive(Subcommand)]
enum ExplorerAction {
    /// Server-level root nodes.
    Server,
    /// Database-level root nodes.
    Database,
    /// Child nodes under a named database root node (e.g. "Tables").
    Children { parent: String },
    /// Filter database nodes by case-insensitive substring.
    Filter { query: String },
}

#[derive(Subcommand)]
enum PlanAction {
    /// Wrap SQL with SET SHOWPLAN_XML for an estimated plan.
    Estimated { sql: String },
    /// Wrap SQL with SET STATISTICS XML for an actual plan.
    Actual { sql: String },
    /// Summarize a ShowPlan XML document as JSON.
    Summarize { xml: String },
    /// Render a ShowPlan XML document as a graphical layout JSON model.
    Render { xml: String },
}

#[derive(Subcommand)]
enum ActivityAction {
    /// DMV query (sessions|blocking|expensive|waits).
    Query { kind: String },
    /// KILL a session by spid.
    Kill { session_id: i32 },
}

#[derive(Subcommand)]
enum TableAction {
    /// SELECT TOP (rows) for a table.
    SelectTop {
        schema: String,
        table: String,
        #[arg(long, default_value_t = 200)]
        rows: u32,
    },
    /// Edit preview (SQL + warnings JSON), optionally with key columns.
    EditPreview {
        schema: String,
        table: String,
        #[arg(long, default_value_t = 200)]
        rows: u32,
        #[arg(long = "key")]
        keys: Vec<String>,
    },
}

#[derive(Subcommand)]
enum QueryStoreAction {
    /// Top queries by average duration.
    Top {
        database: String,
        #[arg(long, default_value_t = 25)]
        limit: u32,
    },
    /// Plan history for a query id.
    Plans { database: String, query_id: i64 },
}

#[derive(Subcommand)]
enum AgentAction {
    /// Query listing all SQL Agent jobs.
    ListJobs,
    /// Script sp_start_job.
    Start { name: String },
    /// Script sp_stop_job.
    Stop { name: String },
    /// Script sp_add_job + one TSQL step.
    Create {
        name: String,
        #[arg(long)]
        step: String,
        #[arg(long)]
        command: String,
    },
}

#[derive(Subcommand)]
enum SecurityAction {
    /// Catalog query (logins|users|roles|role-members|permissions).
    Query { kind: String },
    /// Script CREATE LOGIN.
    CreateLogin { name: String, password: String },
    /// Script CREATE USER.
    CreateUser {
        user: String,
        #[arg(long)]
        login: Option<String>,
        #[arg(long)]
        schema: Option<String>,
    },
    /// Script GRANT/DENY/REVOKE.
    Permission {
        /// grant|deny|revoke
        action: String,
        permission: String,
        object: String,
        principal: String,
    },
}

#[derive(Subcommand)]
enum RegisteredAction {
    /// Add a server group.
    AddGroup {
        #[arg(long)]
        store: String,
        name: String,
    },
    /// Register a server.
    AddServer {
        #[arg(long)]
        store: String,
        name: String,
        server: String,
    },
    /// List the whole store as JSON.
    List {
        #[arg(long)]
        store: String,
    },
    /// Search registered servers.
    Search {
        #[arg(long)]
        store: String,
        query: String,
    },
}

#[derive(Subcommand)]
enum TemplateAction {
    /// List every template grouped by category.
    List,
    /// Search templates by name, category, or description.
    Search { query: String },
    /// Render a template by name, substituting `--set name=value` parameters.
    Render {
        name: String,
        /// Repeatable parameter assignment, e.g. `--set table_name=Orders`.
        #[arg(long = "set", value_parser = parse_key_value)]
        values: Vec<(String, String)>,
    },
}

#[derive(clap::Args)]
struct ConnectionArgs {
    #[arg(short, long, default_value = "localhost")]
    server: String,
    #[arg(short = 'P', long, default_value_t = 1433)]
    port: u16,
    #[arg(short, long)]
    database: Option<String>,
    #[arg(short, long, default_value = "sa")]
    username: String,
    #[arg(short, long)]
    password: String,
    /// Auth mode: sql | entra-browser | entra-device-code.
    #[arg(long, default_value = "sql")]
    auth: String,
    /// Microsoft Entra access token for AAD auth modes, or a cloud IAM database
    /// token for AWS RDS / Cloud SQL IAM auth. When supplied it is used directly
    /// (`runtime_entra_access_token` or `runtime_cloud_iam_token`) so the caller
    /// does not have to export `SQLLY_ENTRA_ACCESS_TOKEN` / `SQLLY_CLOUD_IAM_TOKEN`.
    #[arg(long)]
    access_token: Option<String>,
    /// AWS region for `--auth aws-rds-iam` (e.g. `us-west-2`).
    #[arg(long)]
    aws_region: Option<String>,
    /// Optional AWS named profile for `--auth aws-rds-iam`.
    #[arg(long)]
    aws_profile: Option<String>,
    #[arg(long, default_value_t = true)]
    encrypt: bool,
    #[arg(long, default_value_t = true)]
    trust_certificate: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // reqwest is built with `rustls-no-provider`; install ring as the process
    // default before any AI-provider HTTPS client is constructed.
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Install the global panic hook first — before any work that could panic.
    // Every panic in any thread or async task is logged to the crash file
    // ($SQLLY_CRASH_LOG or $TMPDIR/sqlly-crash.log) with a backtrace, then the
    // default stderr handler runs as usual.
    sqlly_core::install_panic_hook();

    // Logs MUST go to stderr: the `engine` subcommand uses stdout as a binary
    // length-prefixed frame channel. Writing logs to stdout would both corrupt
    // that protocol and (because the engine's stdout writer holds the stdout
    // lock) deadlock any in-flight tiberius call that logs (e.g. a server error
    // token), wedging the query forever.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();

    if let Some(code) = dispatch_agent(&cli).await {
        std::process::exit(code as i32);
    }

    run_legacy(cli.command).await
}

async fn run_legacy(command: Commands) -> anyhow::Result<()> {
    match command {
        Commands::Engine { proxy_credentials } => {
            // The relay sets the env form so the path never appears in a
            // process listing's arguments.
            let proxy_credentials = proxy_credentials.or_else(|| {
                std::env::var_os("SQLLY_ENGINE_PROXY_CREDENTIALS")
                    .filter(|v| !v.is_empty())
                    .map(std::path::PathBuf::from)
            });
            engine::run(proxy_credentials.as_deref()).await?;
        }
        Commands::Mcp { idle_timeout_secs } => {
            mcp::run(idle_timeout_secs).await?;
        }
        Commands::Profile(args) => {
            let profile = profile_from_args(args);
            println!("{}", serde_json::to_string_pretty(&profile)?);
        }
        Commands::Connect(args) => {
            let mut profile = profile_from_args(args);
            mint_cloud_iam_token_if_needed(&mut profile).await?;
            let events = EventBus::default();
            let mut subscription = events.subscribe();
            let service = ConnectionService::new(TiberiusTransport, events);
            service.test_connection(&profile).await?;
            print_pending_events(&mut subscription)?;
        }
        Commands::Disconnect(args) => {
            let profile = profile_from_args(args);
            let events = EventBus::default();
            let mut subscription = events.subscribe();
            let service = ConnectionService::new(TiberiusTransport, events);
            service.disconnect(profile.id)?;
            print_pending_events(&mut subscription)?;
        }
        Commands::TestConnection(args) => {
            let mut profile = profile_from_args(args);
            mint_cloud_iam_token_if_needed(&mut profile).await?;
            let events = EventBus::default();
            let mut subscription = events.subscribe();
            let service = ConnectionService::new(TiberiusTransport, events);
            service.test_connection(&profile).await?;
            print_pending_events(&mut subscription)?;
        }
        Commands::ListDatabases(args) => {
            let mut profile = profile_from_args(args);
            mint_cloud_iam_token_if_needed(&mut profile).await?;
            let events = EventBus::default();
            let mut subscription = events.subscribe();
            let service = ConnectionService::new(TiberiusTransport, events);
            let databases = service.list_databases(&profile).await?;
            print_pending_events(&mut subscription)?;
            println!("{}", serde_json::to_string_pretty(&databases)?);
        }
        Commands::QueryLegacy {
            connection,
            text,
            spill,
            spill_buffer,
        } => {
            let mut profile = profile_from_args(connection);
            mint_cloud_iam_token_if_needed(&mut profile).await?;
            match spill {
                // Back-pressured, spill-to-disk path: rows flow over a bounded
                // mpsc channel (producer blocks instead of dropping) and are
                // streamed to disk, so memory stays O(buffer) on both sides.
                true => {
                    run_query_spill(profile, &text, spill_buffer).await?;
                }
                // Legacy inline path: lifecycle + row events on the broadcast bus.
                false => {
                    let events = EventBus::new(16_384);
                    let mut subscription = events.subscribe();
                    let query_events = events.clone();
                    let handle = tokio::spawn(async move {
                        TiberiusTransport
                            .execute_query(&profile, &text, &query_events, None)
                            .await
                    });
                    drop(events);
                    stream_events(&mut subscription).await?;
                    handle.await??;
                }
            }
        }
        Commands::CancelTest {
            connection,
            text,
            after_seconds,
        } => {
            let mut profile = profile_from_args(connection);
            mint_cloud_iam_token_if_needed(&mut profile).await?;
            let events = EventBus::default();
            let mut subscription = events.subscribe();
            let query_events = events.clone();
            let query = tokio::spawn(async move {
                TiberiusTransport
                    .execute_query(&profile, &text, &query_events, None)
                    .await
            });

            tokio::time::sleep(Duration::from_secs(after_seconds)).await;
            query.abort();
            let session_id = sqlly_core::SessionId::new();
            events.publish(EventPayload::QueryCancelled { session_id })?;
            print_pending_events(&mut subscription)?;
        }
        Commands::RootNodes => {
            println!(
                "{}",
                serde_json::to_string_pretty(&sqlly_intel::metadata::server_root_nodes())?
            );
        }
        Commands::Quote { identifier } => {
            println!(
                "{}",
                sqlly_services::scripting::quote_identifier(&identifier)
            );
        }
        Commands::Templates { action } => run_templates(action)?,
        Commands::Recovery { action } => {
            let out = match action {
                RecoveryAction::Snapshot { tabs } => {
                    let json = std::fs::read_to_string(&tabs)?;
                    commands::recovery_snapshot(&json)?
                }
                RecoveryAction::Recoverable { snapshot } => {
                    let json = std::fs::read_to_string(&snapshot)?;
                    commands::recovery_recoverable(&json)?
                }
            };
            println!("{out}");
        }
        Commands::Preferences { action } => {
            let out = match action {
                PreferencesAction::Default => commands::preferences_default()?,
                PreferencesAction::Normalize { file } => {
                    let json = std::fs::read_to_string(&file)?;
                    commands::preferences_normalize(&json)?
                }
            };
            println!("{out}");
        }
        Commands::Parameters { action } => {
            let out = match action {
                ParameterAction::Detect { sql } => serde_json::to_string_pretty(
                    &sqlly_intel::query::parameters::detect_query_parameters(&sql),
                )?,
            };
            println!("{out}");
        }
        Commands::Safety { action } => {
            let out = match action {
                SafetyAction::Preview {
                    sql,
                    environment,
                    block_all_but_select,
                    auto_wrap_rollback,
                } => commands::safety_preview(
                    &sql,
                    &environment,
                    block_all_but_select,
                    auto_wrap_rollback,
                )?,
            };
            println!("{out}");
        }
        Commands::Azure { action } => {
            let out = match action {
                AzureAction::AccountAdd {
                    registry,
                    username,
                    tenant_id,
                    home_account_id,
                    display_name,
                } => {
                    let registry_json = registry
                        .as_deref()
                        .map(std::fs::read_to_string)
                        .transpose()?;
                    commands::azure_account_add(
                        registry_json.as_deref(),
                        &username,
                        &tenant_id,
                        &home_account_id,
                        display_name.as_deref(),
                    )?
                }
                AzureAction::AccountsList { registry } => {
                    let registry_json = std::fs::read_to_string(registry)?;
                    commands::azure_accounts_list(&registry_json)?
                }
                AzureAction::AccountRemove {
                    registry,
                    account_id,
                } => {
                    let registry_json = std::fs::read_to_string(registry)?;
                    commands::azure_account_remove(&registry_json, &account_id)?
                }
                AzureAction::DiscoverFixture { fixture } => {
                    let fixture_json = std::fs::read_to_string(fixture)?;
                    commands::azure_discover_fixture(&fixture_json)?
                }
                AzureAction::DiscoverTree { fixture } => {
                    let fixture_json = std::fs::read_to_string(fixture)?;
                    commands::azure_discover_tree(&fixture_json)?
                }
                AzureAction::ExplorerChildren { file } => {
                    let request_json = std::fs::read_to_string(file)?;
                    commands::azure_explorer_children(&request_json)?
                }
            };
            println!("{out}");
        }
        Commands::ResultWindow {
            page_size,
            total_rows,
            page,
        } => {
            println!(
                "{}",
                commands::result_window_slice(page_size, total_rows, page)?
            );
        }
        Commands::ResultPage { spill, offset, len } => {
            println!("{}", commands::spill_result_page(&spill, offset, len)?);
        }
        Commands::Connection { action } => {
            let out = match action {
                ConnectionAction::Preview {
                    name,
                    server,
                    database,
                    auth,
                } => commands::connection_preview(&name, &server, database.as_deref(), &auth)?,
            };
            println!("{out}");
        }
        Commands::Parse { sql } => {
            println!("{}", commands::parse_sql(&sql)?);
        }
        Commands::Intellisense { action } => match action {
            IntellisenseAction::Complete {
                request,
                log_requests,
            } => {
                let payload = match request {
                    Some(r) => r,
                    None => {
                        use std::io::Read;
                        let mut buffer = String::new();
                        std::io::stdin().read_to_string(&mut buffer)?;
                        buffer
                    }
                };
                println!(
                    "{}",
                    intellisense_cmd::intellisense_complete_logged(&payload, log_requests)?
                );
            }
            IntellisenseAction::CompleteEnriched {
                request,
                log_requests,
            } => {
                let payload = match request {
                    Some(r) => r,
                    None => {
                        use std::io::Read;
                        let mut buffer = String::new();
                        std::io::stdin().read_to_string(&mut buffer)?;
                        buffer
                    }
                };
                println!(
                    "{}",
                    intellisense_cmd::intellisense_complete_enriched_logged(
                        &payload,
                        log_requests
                    )?
                );
            }
        },
        Commands::Explorer { action } => {
            let out = match action {
                ExplorerAction::Server => commands::explorer_server_nodes()?,
                ExplorerAction::Database => commands::explorer_database_nodes()?,
                ExplorerAction::Children { parent } => commands::explorer_schema_children(&parent)?,
                ExplorerAction::Filter { query } => commands::explorer_filter(&query)?,
            };
            println!("{out}");
        }
        Commands::ServersExplorer { fixture } => {
            let fixture_json = std::fs::read_to_string(fixture)?;
            println!("{}", commands::servers_explorer_tree(&fixture_json)?);
        }
        Commands::ServersExplorerScan { request } => {
            let request_json = std::fs::read_to_string(request)?;
            println!("{}", commands::servers_explorer_live_scan(&request_json)?);
        }
        Commands::ObjectProperties { kind, name, schema } => {
            println!(
                "{}",
                commands::object_properties(&kind, schema.as_deref(), &name)?
            );
        }
        Commands::Plan { action } => {
            let out = match action {
                PlanAction::Estimated { sql } => commands::plan_estimated_sql(&sql),
                PlanAction::Actual { sql } => commands::plan_actual_sql(&sql),
                PlanAction::Summarize { xml } => commands::plan_summarize(&xml)?,
                PlanAction::Render { xml } => commands::plan_render(&xml)?,
            };
            println!("{out}");
        }
        Commands::Backup {
            database,
            destination,
            kind,
            compress,
            checksum,
            copy_only,
        } => {
            let backup_type = parse_backup_type(&kind)?;
            println!(
                "{}",
                commands::backup_sql(
                    &database,
                    &destination,
                    backup_type,
                    compress,
                    checksum,
                    copy_only
                )?
            );
        }
        Commands::Restore {
            database,
            source,
            replace,
            recovery,
        } => {
            println!(
                "{}",
                commands::restore_sql(&database, &source, replace, recovery)?
            );
        }
        Commands::Activity { action } => {
            let out = match action {
                ActivityAction::Query { kind } => commands::activity_query(&kind)?,
                ActivityAction::Kill { session_id } => commands::activity_kill_sql(session_id),
            };
            println!("{out}");
        }
        Commands::Logs { kind } => println!("{}", commands::logs_query(&kind)?),
        Commands::Table { action } => {
            let out = match action {
                TableAction::SelectTop {
                    schema,
                    table,
                    rows,
                } => commands::table_select_top(&schema, &table, rows),
                TableAction::EditPreview {
                    schema,
                    table,
                    rows,
                    keys,
                } => commands::table_edit_preview(&schema, &table, rows, &keys)?,
            };
            println!("{out}");
        }
        Commands::QueryStore { action } => {
            let out = match action {
                QueryStoreAction::Top { database, limit } => {
                    commands::query_store_top(&database, limit)
                }
                QueryStoreAction::Plans { database, query_id } => {
                    commands::query_store_plans(&database, query_id)
                }
            };
            println!("{out}");
        }
        Commands::Xevents { kind, session } => {
            println!("{}", commands::xevents_query(&kind, session.as_deref())?);
        }
        Commands::Report { name } => println!("{}", commands::report_query(&name)?),
        Commands::Import {
            schema,
            table,
            file,
            header,
        } => println!(
            "{}",
            commands::import_csv_sql(&schema, &table, &file, header)
        ),
        Commands::Export { sql, file, header } => {
            println!("{}", commands::export_csv_hint(&sql, &file, header));
        }
        Commands::Agent { action } => {
            let out = match action {
                AgentAction::ListJobs => commands::agent_list_jobs_query(),
                AgentAction::Start { name } => commands::agent_start_job_sql(&name),
                AgentAction::Stop { name } => commands::agent_stop_job_sql(&name),
                AgentAction::Create {
                    name,
                    step,
                    command,
                } => commands::agent_create_job_sql(&name, &step, &command),
            };
            println!("{out}");
        }
        Commands::Security { action } => {
            let out = match action {
                SecurityAction::Query { kind } => commands::security_query(&kind)?,
                SecurityAction::CreateLogin { name, password } => {
                    commands::security_create_login_sql(&name, &password)
                }
                SecurityAction::CreateUser {
                    user,
                    login,
                    schema,
                } => commands::security_create_user_sql(&user, login.as_deref(), schema.as_deref()),
                SecurityAction::Permission {
                    action,
                    permission,
                    object,
                    principal,
                } => commands::security_permission_sql(&action, &permission, &object, &principal)?,
            };
            println!("{out}");
        }
        Commands::Designer {
            schema,
            table,
            columns,
        } => {
            println!(
                "{}",
                commands::designer_create_table_sql(&schema, &table, &columns)?
            );
        }
        Commands::DesignerIndex {
            schema,
            table,
            name,
            columns,
            include_columns,
            unique,
        } => {
            println!(
                "{}",
                commands::designer_create_index_sql(
                    &schema,
                    &table,
                    &name,
                    &columns,
                    &include_columns,
                    unique,
                )?
            );
        }
        Commands::DesignerRelationship {
            name,
            child_schema,
            child_table,
            child_columns,
            parent_schema,
            parent_table,
            parent_columns,
        } => {
            println!(
                "{}",
                commands::designer_create_relationship_sql(
                    &name,
                    &child_schema,
                    &child_table,
                    &child_columns,
                    &parent_schema,
                    &parent_table,
                    &parent_columns,
                )?
            );
        }
        Commands::SchemaCompare { source, target } => {
            let source_json = std::fs::read_to_string(&source)?;
            let target_json = std::fs::read_to_string(&target)?;
            println!(
                "{}",
                commands::schema_compare_script(&source_json, &target_json)?
            );
        }
        Commands::RegisteredServers { action } => {
            let out = match action {
                RegisteredAction::AddGroup { store, name } => {
                    commands::registered_add_group(&store, &name)?
                }
                RegisteredAction::AddServer {
                    store,
                    name,
                    server,
                } => commands::registered_add_server(&store, &name, &server)?,
                RegisteredAction::List { store } => commands::registered_list(&store)?,
                RegisteredAction::Search { store, query } => {
                    commands::registered_search(&store, &query)?
                }
            };
            println!("{out}");
        }
        _ => {}
    }

    Ok(())
}

/// Maps the `--kind` backup flag onto the core `BackupType`.
fn parse_backup_type(kind: &str) -> anyhow::Result<sqlly_services::admin::backup::BackupType> {
    use sqlly_services::admin::backup::BackupType;
    match kind {
        "full" => Ok(BackupType::Full),
        "differential" | "diff" => Ok(BackupType::Differential),
        "log" => Ok(BackupType::Log),
        other => Err(anyhow::anyhow!(
            "unknown backup kind \"{other}\" (expected full|differential|log)"
        )),
    }
}

fn run_templates(action: TemplateAction) -> anyhow::Result<()> {
    match action {
        TemplateAction::List => println!("{}", commands::templates_list()),
        TemplateAction::Search { query } => println!("{}", commands::templates_search(&query)),
        TemplateAction::Render { name, values } => {
            let map: std::collections::HashMap<String, String> = values.into_iter().collect();
            println!("{}", commands::templates_render(&name, map)?);
        }
    }
    Ok(())
}

/// Parses a `name=value` CLI argument into a key/value pair.
fn parse_key_value(raw: &str) -> Result<(String, String), String> {
    match raw.split_once('=') {
        Some((key, value)) if !key.is_empty() => Ok((key.to_string(), value.to_string())),
        _ => Err(format!("expected name=value, got \"{raw}\"")),
    }
}

fn print_pending_events(subscription: &mut sqlly_core::EventSubscription) -> anyhow::Result<()> {
    while let Ok(event) = subscription.try_recv() {
        println!("{}", serde_json::to_string(&event)?);
    }
    Ok(())
}

/// Drains events as they are published, concurrently with the producing task.
/// Unlike `print_pending_events` (a post-hoc `try_recv` sweep), this awaits each
/// event so a long stream of per-row events is emitted incrementally and never
/// overflows the broadcast ring. `Lagged` is tolerated (skip and keep going)
/// and `Closed` ends the loop once every sender has been dropped.
async fn stream_events(subscription: &mut sqlly_core::EventSubscription) -> anyhow::Result<()> {
    use tokio::sync::broadcast::error::RecvError;
    loop {
        match subscription.recv().await {
            Ok(event) => println!("{}", serde_json::to_string(&event)?),
            Err(RecvError::Lagged(_)) => continue,
            Err(RecvError::Closed) => break,
        }
    }
    Ok(())
}

/// Back-pressured, spill-to-disk query runner. Rows arrive over a bounded mpsc
/// channel (the producer blocks when the buffer is full — zero dropped rows) and
/// are streamed to one spill file per result set under `<data_dir>/SQLLY/spill/`.
/// Only lifecycle events and a final JSON manifest are printed to stdout; peak
/// memory is O(channel buffer + write buffer), independent of total rows. The
/// caller pages rows back with `result-page`.
async fn run_query_spill(
    profile: ConnectionProfile,
    text: &str,
    buffer: usize,
) -> anyhow::Result<()> {
    let events = EventBus::new(1024);
    let mut subscription = events.subscribe();
    let (rows_tx, rows_rx) = sqlly_core::row_channel(buffer);
    let query_events = events.clone();
    let text_owned = text.to_string();
    let handle = tokio::spawn(async move {
        TiberiusTransport
            .execute_query(&profile, &text_owned, &query_events, Some(&rows_tx))
            .await
    });
    drop(events);

    // Consume lifecycle events (columns/messages/errors) and the bounded row
    // stream concurrently, spilling rows to disk as they arrive.
    let base = spill_dir().join(format!("sqlly-spill-{}", uuid::Uuid::new_v4()));
    let manifest = spill_consume(&base, &mut subscription, rows_rx).await?;
    handle.await??;
    println!("{}", serde_json::to_string(&manifest)?);
    Ok(())
}

/// Returns the directory where spill files are written:
/// `<data_dir>/SQLLY/spill/` — `~/Library/Application Support/SQLLY/spill/` on
/// macOS, `~/.local/share/SQLLY/spill/` on Linux, `%APPDATA%/SQLLY/spill/` on
/// Windows — falling back to the system temp dir if the data dir is unavailable.
fn spill_dir() -> std::path::PathBuf {
    let dir = dirs::data_dir().unwrap_or_else(std::env::temp_dir);
    let spill = dir.join("SQLLY").join("spill");
    let _ = std::fs::create_dir_all(&spill);
    spill
}

/// Drives the lifecycle-event subscription and the bounded row receiver
/// together, writing each result set to `<base>.<index>.bin` and returning a
/// manifest describing columns + row counts + file paths per result set.
async fn spill_consume(
    base: &std::path::Path,
    subscription: &mut sqlly_core::EventSubscription,
    mut rows_rx: RowReceiver,
) -> anyhow::Result<serde_json::Value> {
    use tokio::sync::broadcast::error::RecvError;

    // Per-result-set column metadata, captured from lifecycle events.
    let mut columns_by_set: Vec<Vec<sqlly_core::ResultColumn>> = Vec::new();
    let mut writers: std::collections::HashMap<usize, SpillWriter> =
        std::collections::HashMap::new();
    let mut row_counts: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    let mut messages: Vec<String> = Vec::new();
    let mut error: Option<String> = None;
    let mut elapsed_ms: Option<u128> = None;

    let spill_path = |idx: usize| -> std::path::PathBuf {
        let mut name = base.as_os_str().to_os_string();
        name.push(format!(".{idx}.bin"));
        std::path::PathBuf::from(name)
    };

    // Lifecycle events end (Closed) when the producer task finishes; rows end
    // (None) at the same point. We poll both until each is exhausted.
    let mut events_done = false;
    let mut rows_done = false;
    loop {
        tokio::select! {
            ev = subscription.recv(), if !events_done => match ev {
                Ok(event) => match event.payload {
                    EventPayload::ResultSetStarted { result_set_index, columns, .. } => {
                        if columns_by_set.len() <= result_set_index {
                            columns_by_set.resize(result_set_index + 1, Vec::new());
                        }
                        columns_by_set[result_set_index] = columns;
                        let path = spill_path(result_set_index);
                        writers.insert(
                            result_set_index,
                            SpillWriter::create(&path)
                                .map_err(|e| anyhow::anyhow!("create spill: {e}"))?,
                        );
                        row_counts.entry(result_set_index).or_insert(0);
                    }
                    EventPayload::Message { text, .. } => messages.push(text),
                    EventPayload::Error { error: e } => error = Some(e),
                    EventPayload::QueryCompleted { elapsed_ms: e, .. } => elapsed_ms = Some(e),
                    _ => {}
                },
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => events_done = true,
            },
            batch = rows_rx.recv(), if !rows_done => match batch {
                Some(batch) => {
                    let idx = batch.result_set_index;
                    let writer = writers.entry(idx).or_insert_with(|| {
                        SpillWriter::create(spill_path(idx)).expect("spill file creates")
                    });
                    for row in &batch.rows {
                        writer
                            .append(row)
                            .map_err(|e| anyhow::anyhow!("append spill: {e}"))?;
                    }
                    *row_counts.entry(idx).or_insert(0) += batch.len();
                }
                None => rows_done = true,
            },
            else => break,
        }
        if events_done && rows_done {
            break;
        }
    }

    // Flush all writers to disk.
    for (_idx, writer) in writers.drain() {
        writer
            .finish()
            .map_err(|e| anyhow::anyhow!("finish spill: {e}"))?;
    }

    let result_sets: Vec<serde_json::Value> = (0..columns_by_set.len())
        .map(|idx| {
            serde_json::json!({
                "result_set_index": idx,
                "columns": columns_by_set[idx],
                "row_count": row_counts.get(&idx).copied().unwrap_or(0),
                "spill_file": spill_path(idx).to_string_lossy(),
            })
        })
        .collect();

    Ok(serde_json::json!({
        "kind": "spill_manifest",
        "result_sets": result_sets,
        "messages": messages,
        "error": error,
        "elapsed_ms": elapsed_ms,
    }))
}

fn profile_from_args(args: ConnectionArgs) -> ConnectionProfile {
    let mut profile = match args.auth.as_str() {
        "entra-browser" => ConnectionProfile::azure_sql_entra_browser(
            &args.server,
            &args.server,
            args.database
                .clone()
                .unwrap_or_else(|| "master".to_string()),
        ),
        "entra-device-code" => ConnectionProfile::azure_sql_entra_device_code(
            &args.server,
            &args.server,
            args.database
                .clone()
                .unwrap_or_else(|| "master".to_string()),
        ),
        "aws-rds-iam" => {
            let region = args.aws_region.clone().unwrap_or_default();
            let mut profile = ConnectionProfile::aws_rds_iam(
                &args.server,
                &args.server,
                args.port,
                sqlly_core::DatabaseType::Postgres,
                &args.username,
                region,
            );
            if let AuthenticationType::AwsRdsIam {
                profile: aws_profile,
                ..
            } = &mut profile.authentication
            {
                *aws_profile = args.aws_profile.clone();
            }
            profile
        }
        "gcp-cloud-sql-iam" => ConnectionProfile::gcp_cloud_sql_iam(
            &args.server,
            &args.server,
            args.port,
            sqlly_core::DatabaseType::Postgres,
            &args.username,
        ),
        // libSQL / Turso: `--password` carries the auth token (blank for an
        // anonymous local sqld).
        "auth-token" => ConnectionProfile::auth_token(
            &args.server,
            &args.server,
            args.port,
            SecretRef {
                provider: SecretProvider::InMemory,
                key: args.password,
            },
        ),
        _ => ConnectionProfile::sql_login(
            &args.server,
            &args.server,
            &args.username,
            SecretRef {
                provider: SecretProvider::InMemory,
                key: args.password,
            },
        ),
    };
    profile.port = args.port;
    profile.database = args.database;
    if profile.authentication.is_cloud_iam() {
        profile.runtime_cloud_iam_token = args
            .access_token
            .clone()
            .or_else(|| std::env::var("SQLLY_CLOUD_IAM_TOKEN").ok());
    } else {
        profile.runtime_entra_access_token = args.access_token;
    }
    profile.encrypt = args.encrypt;
    profile.trust_server_certificate = args.trust_certificate;
    profile
}

#[cfg(not(target_arch = "wasm32"))]
async fn mint_cloud_iam_token_if_needed(profile: &mut ConnectionProfile) -> anyhow::Result<()> {
    if !profile.authentication.is_cloud_iam() || profile.runtime_cloud_iam_token.is_some() {
        return Ok(());
    }
    let token = sqlly_services::cloud_auth::token_for_profile(profile)
        .await?
        .ok_or_else(|| anyhow::anyhow!("expected cloud IAM token"))?;
    profile.runtime_cloud_iam_token = Some(token.value);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::AuthenticationType;

    fn entra_args(access_token: Option<String>) -> ConnectionArgs {
        ConnectionArgs {
            server: "sql-contoso-prod.database.windows.net".to_string(),
            port: 1433,
            database: Some("master".to_string()),
            username: String::new(),
            password: String::new(),
            auth: "entra-browser".to_string(),
            access_token,
            aws_region: None,
            aws_profile: None,
            encrypt: true,
            trust_certificate: false,
        }
    }

    #[test]
    fn aws_rds_iam_args_build_profile_and_take_access_token() {
        let args = ConnectionArgs {
            server: "db.us-west-2.rds.amazonaws.com".to_string(),
            port: 5432,
            database: Some("app".to_string()),
            username: "jane".to_string(),
            password: String::new(),
            auth: "aws-rds-iam".to_string(),
            access_token: Some("iam-token".to_string()),
            aws_region: Some("us-west-2".to_string()),
            aws_profile: Some("dev".to_string()),
            encrypt: true,
            trust_certificate: false,
        };
        let profile = profile_from_args(args);
        match profile.authentication {
            AuthenticationType::AwsRdsIam {
                username,
                region,
                profile: aws_profile,
            } => {
                assert_eq!(username, "jane");
                assert_eq!(region, "us-west-2");
                assert_eq!(aws_profile.as_deref(), Some("dev"));
            }
            other => panic!("unexpected auth: {other:?}"),
        }
        assert_eq!(
            profile.runtime_cloud_iam_token.as_deref(),
            Some("iam-token")
        );
    }

    #[test]
    fn gcp_iam_args_require_username() {
        let args = ConnectionArgs {
            server: "10.0.0.1".to_string(),
            port: 5432,
            database: None,
            username: "alice@example.com".to_string(),
            password: String::new(),
            auth: "gcp-cloud-sql-iam".to_string(),
            access_token: None,
            aws_region: None,
            aws_profile: None,
            encrypt: true,
            trust_certificate: false,
        };
        let profile = profile_from_args(args);
        match profile.authentication {
            AuthenticationType::GcpCloudSqlIam { username } => {
                assert_eq!(username, "alice@example.com");
            }
            other => panic!("unexpected auth: {other:?}"),
        }
    }

    #[test]
    fn access_token_flag_populates_runtime_entra_token() {
        let profile = profile_from_args(entra_args(Some("flag-token".to_string())));
        assert!(matches!(
            profile.authentication,
            sqlly_core::AuthenticationType::EntraBrowser
        ));
        assert_eq!(
            profile.runtime_entra_access_token.as_deref(),
            Some("flag-token"),
            "the --access-token flag must flow into runtime_entra_access_token so AAD login \
             does not fall back to env-var lookup"
        );
    }

    /// Split the app's copied-CLI fixture into commands: a `### name` line
    /// starts a case; a command may span lines inside quotes (multi-line
    /// SQL), and a case may hold several commands (one per table export).
    fn copied_cli_commands() -> Vec<(String, String)> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/copied-cli/lines.txt");
        let text = std::fs::read_to_string(&path)
            .expect("fixture written by sqlly-gpui's copied_lines_match_cli_fixture")
            .replace("\r\n", "\n");
        let mut commands = Vec::new();
        let mut name = String::new();
        let mut pending = String::new();
        for line in text.lines() {
            if let Some(case) = line.strip_prefix("### ") {
                assert!(pending.is_empty(), "unterminated command in {name}");
                name = case.to_string();
                continue;
            }
            if name.is_empty() || (pending.is_empty() && line.trim().is_empty()) {
                continue;
            }
            if !pending.is_empty() {
                pending.push('\n');
            }
            pending.push_str(line);
            if shlex::split(&shell_expand_stamps(&pending)).is_some() {
                commands.push((name.clone(), std::mem::take(&mut pending)));
            }
        }
        assert!(pending.is_empty(), "unterminated command in {name}");
        commands
    }

    /// Stand-in for the shell's command substitution: the copied lines use
    /// `$(date …)` stamps, which a word splitter would otherwise cut at the
    /// space inside them.
    fn shell_expand_stamps(command: &str) -> String {
        command
            .replace("$(date +%Y%m%d-%H%M%S)", "20260928-140500")
            .replace("$(date +%Y%m%d)", "20260928")
    }

    #[test]
    fn copied_cli_lines_parse() {
        let commands = copied_cli_commands();
        assert!(commands.len() >= 12, "fixture has every generator");
        for (name, command) in commands {
            let words = shlex::split(&shell_expand_stamps(&command))
                .unwrap_or_else(|| panic!("{name}: not valid POSIX shell words: {command}"));
            // The redirect / pipe belongs to the shell, not the CLI.
            let argv: Vec<String> = words
                .into_iter()
                .take_while(|word| word != ">" && word != "|")
                .collect();
            assert!(
                argv[0].ends_with("sqlly-cli"),
                "{name}: must invoke the sqlly-cli binary, got {}",
                argv[0]
            );
            let cli =
                Cli::try_parse_from(&argv).unwrap_or_else(|err| panic!("{name}: {command}\n{err}"));
            match (name.as_str(), &cli.command) {
                ("query-leading-comment", Commands::Query { sql, .. }) => {
                    let sql = sql.as_deref().expect("--sql");
                    assert!(sql.starts_with("-- daily totals\nSELECT -1"), "{sql}");
                    assert!(sql.contains("'it''s'"), "{sql}");
                }
                ("query-negative-literal", Commands::Query { sql, .. }) => {
                    assert_eq!(sql.as_deref(), Some("-1 + 2"));
                }
                ("run-file-hyphen-connection", Commands::RunFile { connection, .. }) => {
                    assert_eq!(connection, "-dev");
                }
                (
                    "admin-backup-script",
                    Commands::Query {
                        sql, allow_writes, ..
                    },
                ) => {
                    assert!(*allow_writes);
                    assert_eq!(
                        sql.as_deref(),
                        Some("BACKUP DATABASE [Sales]\nTO DISK = N'/b/Sales.bak'")
                    );
                }
                (
                    "schedule-per-object-dump",
                    Commands::DumpDatabase {
                        output, per_object, ..
                    },
                ) => {
                    assert!(*per_object);
                    assert_eq!(
                        output,
                        &std::path::PathBuf::from(
                            "/var/backups/it's here/sales_3f2a9c1b_20260928-140500"
                        )
                    );
                }
                (
                    "dump-native-mysql",
                    Commands::DumpDatabase {
                        native,
                        no_triggers,
                        mysqldump_option,
                        strip_definer,
                        ..
                    },
                ) => {
                    assert!(*native && *no_triggers && *strip_definer);
                    assert_eq!(
                        mysqldump_option,
                        &vec![
                            "--where=id > 5".to_string(),
                            "--column-statistics=0".to_string()
                        ]
                    );
                }
                _ => {}
            }
        }
    }

    #[test]
    fn sql_values_may_start_with_a_hyphen() {
        for argv in [
            vec!["sqlly-cli", "query", "dev", "--sql", "-- note\nselect 1"],
            vec!["sqlly-cli", "query", "dev", "--sql=-1 + 2"],
        ] {
            let cli = Cli::try_parse_from(&argv).expect("parses");
            assert!(matches!(
                cli.command,
                Commands::Query { sql: Some(ref s), .. } if s.starts_with('-')
            ));
        }
    }

    #[test]
    fn missing_access_token_leaves_runtime_token_none() {
        let profile = profile_from_args(entra_args(None));
        assert_eq!(profile.runtime_entra_access_token, None);
    }
}
