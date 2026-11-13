//! Engine capability gating for GPUI surfaces.
//!
//! One place that answers "is this app feature available on this engine, and
//! if not, why not" — so menus, toolbars, and dialogs disable engine-specific
//! actions with a truthful reason instead of scattering `matches!` checks or
//! failing at request time. Pure and synchronous: capability answers derive
//! from `DatabaseType` alone and never touch the server, so they are safe in
//! render paths.
//!
//! These describe what the APP implements per engine today, which is
//! narrower than what the engine itself could support — extend the matching
//! arm when a feature becomes portable (the reason strings then serve as a
//! checklist).

use sqlly_lang::sqlmodel::{ConnectionKind, DatabaseType, DbFeature};

use crate::execution_plan_view::PlanKind;

/// Whether an app feature is usable on a given engine, with the reason when
/// it is not. The reason is a catalog key (`explorer.capabilities.reason_*`
/// and friends), never English text: [`Capability::reason`] renders it in the
/// active language, so no caller ever matches or shows untranslated prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    Available,
    /// The catalog key of the user-facing reason.
    Unavailable(&'static str),
}

impl Capability {
    pub fn is_available(self) -> bool {
        matches!(self, Capability::Available)
    }

    /// The catalog key of the reason when unavailable (for logs and tests).
    pub fn reason_key(self) -> Option<&'static str> {
        match self {
            Capability::Available => None,
            Capability::Unavailable(key) => Some(key),
        }
    }

    /// The user-facing reason when unavailable, in the active UI language.
    /// Call on the UI thread (translation state is per thread).
    pub fn reason(self) -> Option<String> {
        self.reason_key().map(crate::localization::text)
    }
}

/// Admin surfaces whose SQL and live-query paths vary by engine. Keeping the
/// command id beside the capability matrix lets menu actions and the command
/// palette share the same launch policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminTool {
    BackupRestore,
    ActivityMonitor,
    ServerDashboard,
    SqlAgent,
    Security,
    Designers,
    StructureEditor,
    SchemaCompare,
    /// Schema snapshots and the history timeline reuse Schema Compare's
    /// per-engine collector, so they share its engine matrix.
    SchemaHistory,
    DataCompare,
    QueryInsights,
    ExtendedEvents,
    DiskUsage,
    ObjectBrowser,
    IndexAnalyzer,
    TestDataGenerator,
    DataTransfer,
    /// The Redis live tools inside the Activity Monitor (their own tabs,
    /// their own dedicated connections).
    RedisProfiler,
    RedisPubSub,
    /// The Redis memory analyzer's bounded report pane (the Disk Usage
    /// drill-down for Redis databases).
    RedisMemoryAnalyzer,
    /// The Redis key view: one key's header, paged value and per-type
    /// edits (the explorer's Open Value; the palette opens it with a prompt).
    RedisKeyView,
    /// The Redis Cluster topology pane: nodes, roles, slots, advertised vs
    /// dialed addresses, and the per-node connection trace.
    RedisClusterTopology,
}

impl AdminTool {
    pub fn from_command_id(id: &str) -> Option<Self> {
        match id {
            "tools.backupRestore" => Some(Self::BackupRestore),
            "tools.activityMonitor" => Some(Self::ActivityMonitor),
            "tools.serverDashboard" => Some(Self::ServerDashboard),
            "tools.sqlAgent" => Some(Self::SqlAgent),
            "tools.security" => Some(Self::Security),
            "tools.designers" => Some(Self::Designers),
            "tools.structure_editor" => Some(Self::StructureEditor),
            "tools.schemaCompare" => Some(Self::SchemaCompare),
            "tools.schemaHistory" | "tools.saveSchemaSnapshot" => Some(Self::SchemaHistory),
            "tools.dataCompare" => Some(Self::DataCompare),
            "tools.queryStore" => Some(Self::QueryInsights),
            "tools.extendedEvents" => Some(Self::ExtendedEvents),
            "tools.diskUsage" => Some(Self::DiskUsage),
            "tools.objectBrowser" => Some(Self::ObjectBrowser),
            "tools.indexAnalyzer" => Some(Self::IndexAnalyzer),
            "tools.testDataGenerator" => Some(Self::TestDataGenerator),
            "tools.dataTransfer" => Some(Self::DataTransfer),
            "tools.redisProfiler" => Some(Self::RedisProfiler),
            "tools.redisPubSub" => Some(Self::RedisPubSub),
            "tools.redisMemoryAnalyzer" => Some(Self::RedisMemoryAnalyzer),
            "tools.redisKeyView" => Some(Self::RedisKeyView),
            "tools.redisClusterTopology" => Some(Self::RedisClusterTopology),
            _ => None,
        }
    }

    pub fn label(self) -> String {
        crate::localization::text(match self {
            Self::BackupRestore => "explorer.capabilities.tool_backup_restore",
            Self::ActivityMonitor => "explorer.capabilities.tool_activity_monitor",
            Self::ServerDashboard => "explorer.capabilities.tool_server_dashboard",
            Self::SqlAgent => "explorer.capabilities.tool_sql_agent",
            Self::Security => "explorer.capabilities.tool_security",
            Self::Designers => "explorer.capabilities.tool_designers",
            Self::StructureEditor => "explorer.capabilities.tool_structure_editor",
            Self::SchemaCompare => "explorer.capabilities.tool_schema_compare",
            Self::SchemaHistory => "explorer.capabilities.tool_schema_history",
            Self::DataCompare => "explorer.capabilities.tool_data_compare",
            Self::QueryInsights => "explorer.capabilities.tool_query_insights",
            Self::ExtendedEvents => "explorer.capabilities.tool_extended_events",
            Self::DiskUsage => "explorer.capabilities.tool_disk_usage",
            Self::ObjectBrowser => "explorer.capabilities.tool_object_browser",
            Self::IndexAnalyzer => "explorer.capabilities.tool_index_analyzer",
            Self::TestDataGenerator => "explorer.capabilities.tool_test_data_generator",
            Self::DataTransfer => "explorer.capabilities.tool_data_transfer",
            Self::RedisProfiler => "explorer.capabilities.tool_redis_profiler",
            Self::RedisPubSub => "explorer.capabilities.tool_redis_pubsub",
            Self::RedisMemoryAnalyzer => "explorer.capabilities.tool_redis_memory_analyzer",
            Self::RedisKeyView => "explorer.capabilities.tool_redis_key_view",
            Self::RedisClusterTopology => "explorer.capabilities.tool_redis_cluster_topology",
        })
    }
}

/// Commands that work on every engine but still require an active
/// connection (they read the loaded schema model or per-database settings).
const CONNECTION_GATED_COMMANDS: [&str; 6] = [
    "tools.exportSchemaDocs",
    "tools.tenantScoping",
    "tools.relationshipGraph",
    "tools.queryBuilder",
    "tools.exportDatabaseSql",
    "tools.searchDatabaseValue",
];

/// The capability for a mapped palette command. `None` means the command is
/// engine-independent and needs no connection gate.
pub fn command_capability(id: &str, db: Option<DatabaseType>) -> Option<Capability> {
    let capability = match plan_command_kind(id) {
        Some(kind) => db.map(|db| plan(db, kind)),
        None if CONNECTION_GATED_COMMANDS.contains(&id) => db.map(|_| Capability::Available),
        None => AdminTool::from_command_id(id).and_then(|tool| db.map(|db| admin_tool(db, tool))),
    };
    let is_gated = plan_command_kind(id).is_some()
        || CONNECTION_GATED_COMMANDS.contains(&id)
        || AdminTool::from_command_id(id).is_some();
    is_gated.then(|| {
        capability.unwrap_or(Capability::Unavailable(
            "explorer.capabilities.reason_connect",
        ))
    })
}

/// The plan kind a palette command captures, or `None` for commands that
/// capture no plan. `plan.profile` runs the statement with ANALYZE, so it is
/// gated like an actual plan. Opening or pasting a saved plan works on any
/// engine (or none) and is deliberately absent.
pub fn plan_command_kind(id: &str) -> Option<PlanKind> {
    match id {
        "query.explainPlan" | "plan.profile" => Some(PlanKind::Actual),
        "query.explainEstimated" => Some(PlanKind::Estimated),
        _ => None,
    }
}

/// The user-facing reason a gated command cannot run right now, or `None`
/// when it can (or is not gated). Plan commands name the engine through
/// [`plan_unavailable_reason`]; everything else uses its capability reason.
/// The palette rows, the in-window menu bar and palette dispatch all read
/// this, so every surface shows the same sentence.
pub fn command_unavailable_reason(id: &str, db: Option<DatabaseType>) -> Option<String> {
    if let (Some(kind), Some(db)) = (plan_command_kind(id), db) {
        return plan_unavailable_reason(db, kind);
    }
    command_capability(id, db).and_then(Capability::reason)
}

/// SQL editing services — the parser-backed validation, SQL IntelliSense,
/// hover, lints, the formatter and statement splitting. Redis and its
/// protocol relatives speak commands, one per line, so every SQL service is
/// switched off for them; the editor still highlights and runs the text,
/// and completes, hovers and shows signatures from the server's own
/// command docs instead (`sqlly_intel::redis_complete`).
pub fn sql_editing(db: DatabaseType) -> Capability {
    if db.is_command_engine() {
        Capability::Unavailable("explorer.capabilities.reason_sql_editing_redis")
    } else {
        Capability::Available
    }
}

/// Row editing: provenance discovery and guarded DML are implemented for
/// the native driver families (each engine describes its own queries —
/// sp_describe_first_result_set, prepared-statement column origins, column
/// metadata, DuckDB's DESCRIBE plus its catalog, or on Oracle a single-table
/// statement analysis plus a server-side describe — and runs its own
/// transactional DML).
pub fn row_editing(db: DatabaseType) -> Capability {
    match db.connection_kind() {
        ConnectionKind::Tds
        | ConnectionKind::PostgresWire
        | ConnectionKind::MySqlWire
        | ConnectionKind::SqliteFile
        | ConnectionKind::HranaHttp
        | ConnectionKind::DuckDbFile
        | ConnectionKind::OracleNet => Capability::Available,
        ConnectionKind::ClickHouseHttp => {
            Capability::Unavailable("explorer.capabilities.reason_row_editing_clickhouse")
        }
        ConnectionKind::Resp => {
            Capability::Unavailable("explorer.capabilities.reason_row_editing_redis")
        }
        _ => Capability::Unavailable("explorer.capabilities.reason_row_editing_unsupported"),
    }
}

/// `UPDATE … SET col = DEFAULT` is not valid SQLite (nor libSQL, which
/// runs SQLite's SQL); inserts omit the column instead.
pub fn cell_default_in_update(db: DatabaseType) -> Capability {
    match db.connection_kind() {
        ConnectionKind::SqliteFile => {
            Capability::Unavailable("explorer.capabilities.reason_cell_default_sqlite")
        }
        ConnectionKind::HranaHttp => {
            Capability::Unavailable("explorer.capabilities.reason_cell_default_libsql")
        }
        _ => row_editing(db),
    }
}

/// Create/Drop Database dialogs render engine DDL for the three server
/// engines; SQLite databases are files and are created from New Connection.
pub fn database_ddl(db: DatabaseType) -> Capability {
    match db.connection_kind() {
        ConnectionKind::Tds | ConnectionKind::PostgresWire | ConnectionKind::MySqlWire => {
            Capability::Available
        }
        ConnectionKind::SqliteFile => {
            Capability::Unavailable("explorer.capabilities.reason_database_ddl_sqlite")
        }
        ConnectionKind::HranaHttp => {
            Capability::Unavailable("explorer.capabilities.reason_database_ddl_libsql")
        }
        ConnectionKind::DuckDbFile => {
            Capability::Unavailable("explorer.capabilities.reason_database_ddl_duckdb")
        }
        ConnectionKind::ClickHouseHttp => {
            Capability::Unavailable("explorer.capabilities.reason_database_ddl_clickhouse")
        }
        ConnectionKind::OracleNet => {
            Capability::Unavailable("explorer.capabilities.reason_database_ddl_oracle")
        }
        ConnectionKind::Resp => {
            Capability::Unavailable("explorer.capabilities.reason_database_ddl_redis")
        }
        _ => Capability::Unavailable("explorer.capabilities.reason_database_ddl_unsupported"),
    }
}

/// Explorer Maintenance submenu (vacuum / analyze / reindex / update
/// statistics / integrity checks / index rebuilds / checkpoints): the five
/// native driver families each have a statement vocabulary wired in
/// `sqlly-services`. PostgreSQL-wire and MySQL-wire engines that lack the
/// verbs (CockroachDB has no VACUUM, TiDB no OPTIMIZE TABLE) stay
/// unavailable with the reason.
pub fn maintenance(db: DatabaseType) -> Capability {
    if sqlly_services::tool_dialects::maintenance_dialect(db).is_some() {
        Capability::Available
    } else if db.is_command_engine() {
        Capability::Unavailable("explorer.capabilities.reason_maintenance_redis")
    } else {
        Capability::Unavailable("explorer.capabilities.reason_maintenance_unsupported")
    }
}

/// Procedure Mode: T-SQL and PL/pgSQL pipelines are implemented. MySQL and
/// MariaDB stay unavailable because their DDL commits immediately, so the
/// auto-rollback deploy sandbox cannot be honored there.
pub fn procedure_mode(db: DatabaseType) -> Capability {
    if !db.supports_feature(DbFeature::StoredProcedures) {
        return Capability::Unavailable("explorer.capabilities.reason_procedure_no_procedures");
    }
    match db.connection_kind() {
        ConnectionKind::Tds | ConnectionKind::PostgresWire => Capability::Available,
        ConnectionKind::MySqlWire => {
            Capability::Unavailable("explorer.capabilities.reason_procedure_mysql")
        }
        _ => Capability::Unavailable("explorer.capabilities.reason_procedure_unsupported"),
    }
}

/// Include Statistics: T-SQL wraps runs in `SET STATISTICS TIME, IO`;
/// MySQL/MariaDB snapshot session handler counters around the run.
/// PostgreSQL stays unavailable honestly — its only statistics mechanism
/// (`EXPLAIN ANALYZE`) would execute the query a second time.
pub fn include_statistics(db: DatabaseType) -> Capability {
    match db.connection_kind() {
        ConnectionKind::Tds | ConnectionKind::MySqlWire => Capability::Available,
        ConnectionKind::PostgresWire => {
            Capability::Unavailable("explorer.capabilities.reason_statistics_postgres")
        }
        _ => Capability::Unavailable("explorer.capabilities.reason_statistics_unsupported"),
    }
}

/// Manual commit mode: pinned warm client per tab on SQL Server, PostgreSQL, and MySQL.
pub fn manual_transactions(db: DatabaseType) -> Capability {
    match db.connection_kind() {
        ConnectionKind::Tds | ConnectionKind::PostgresWire | ConnectionKind::MySqlWire => {
            Capability::Available
        }
        _ => Capability::Unavailable("explorer.capabilities.reason_manual_transactions"),
    }
}

/// Actual plan capture executes the statement and is implemented only where
/// the parser receives runtime measurements.
pub fn actual_plan(db: DatabaseType) -> Capability {
    match db.connection_kind() {
        // MySQL 8 (EXPLAIN ANALYZE), MariaDB (ANALYZE FORMAT=JSON) and DuckDB
        // (EXPLAIN ANALYZE) all execute the statement and report runtime row
        // counts/timings.
        // Oracle runs the statement with row-source statistics on and
        // reads the cursor's plan back from V$SQL_PLAN_STATISTICS_ALL.
        ConnectionKind::Tds
        | ConnectionKind::PostgresWire
        | ConnectionKind::MySqlWire
        | ConnectionKind::DuckDbFile
        | ConnectionKind::OracleNet => Capability::Available,
        ConnectionKind::SqliteFile => {
            Capability::Unavailable("explorer.capabilities.reason_actual_plan_sqlite")
        }
        ConnectionKind::HranaHttp => {
            Capability::Unavailable("explorer.capabilities.reason_actual_plan_libsql")
        }
        ConnectionKind::ClickHouseHttp => {
            Capability::Unavailable("explorer.capabilities.reason_actual_plan_clickhouse")
        }
        ConnectionKind::Resp => Capability::Unavailable("explorer.capabilities.reason_plan_redis"),
        _ => Capability::Unavailable("explorer.capabilities.reason_plan_unsupported"),
    }
}

/// Estimated plan capture asks the optimizer without executing the statement
/// on every native-driver engine.
pub fn estimated_plan(db: DatabaseType) -> Capability {
    match db.connection_kind() {
        ConnectionKind::Tds
        | ConnectionKind::PostgresWire
        | ConnectionKind::MySqlWire
        | ConnectionKind::SqliteFile
        | ConnectionKind::HranaHttp
        | ConnectionKind::DuckDbFile
        | ConnectionKind::ClickHouseHttp
        | ConnectionKind::OracleNet => Capability::Available,
        ConnectionKind::Resp => Capability::Unavailable("explorer.capabilities.reason_plan_redis"),
        _ => Capability::Unavailable("explorer.capabilities.reason_plan_unsupported"),
    }
}

pub fn plan(db: DatabaseType, kind: PlanKind) -> Capability {
    match kind {
        PlanKind::Actual => actual_plan(db),
        PlanKind::Estimated => estimated_plan(db),
    }
}

/// Both plan-kind reasons for one engine, precomputed so a view that cannot
/// see the active connection (the plan pane) can gate its own capture
/// affordances. The default (no engine, or disconnected) gates nothing —
/// the capture path itself reports "connect first".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlanCaptureGate {
    pub actual: Option<String>,
    pub estimated: Option<String>,
}

impl PlanCaptureGate {
    pub fn for_engine(db: Option<DatabaseType>) -> Self {
        let Some(db) = db else {
            return Self::default();
        };
        Self {
            actual: plan_unavailable_reason(db, PlanKind::Actual),
            estimated: plan_unavailable_reason(db, PlanKind::Estimated),
        }
    }

    /// Why a `kind` capture is disabled, or `None` when it can run.
    pub fn reason(&self, kind: PlanKind) -> Option<&str> {
        match kind {
            PlanKind::Actual => self.actual.as_deref(),
            PlanKind::Estimated => self.estimated.as_deref(),
        }
    }
}

/// The single sentence every plan affordance shows when `kind` cannot be
/// captured on `db`: the toolbar tooltip, the in-window menu hint, the
/// palette row, the overflow menu, the plan pane's Re-capture button and
/// empty state, and the status message a shortcut leaves. It names the
/// engine so the user knows the gate is the database type, not their query,
/// then says what to do instead. `None` when the plan can be captured.
///
/// An engine with no plans at all reads "Query plans are disabled…"; one
/// that only lacks runtime (actual) plans reads "Actual execution plans are
/// disabled…" so the Estimated route stays discoverable.
pub fn plan_unavailable_reason(db: DatabaseType, kind: PlanKind) -> Option<String> {
    let detail = plan(db, kind).reason()?;
    let key = if estimated_plan(db).is_available() {
        "explorer.capabilities.actual_plans_disabled"
    } else {
        "explorer.capabilities.query_plans_disabled"
    };
    let engine = db.display_name().to_string();
    let detail = detail.trim_end_matches('.').to_string();
    Some(crate::localization::format(
        key,
        &[("engine", &engine), ("reason", &detail)],
    ))
}

/// Honesty text for wire-compatible engine labels: what the app verified and
/// what it does not promise. `None` for first-party engines.
#[cfg(test)]
pub fn compatibility_notes(db: DatabaseType) -> Option<&'static str> {
    use DatabaseType::*;
    Some(match db {
        CockroachDb => {
            "Connects over the PostgreSQL protocol. Catalog details may differ (routine definitions use a CockroachDB-specific query); Procedure Mode, designers, and the Security editor are unavailable."
        }
        YugabyteDb => {
            "Connects over the PostgreSQL protocol (YSQL, port 5433). Catalog details may differ; designers and the Security editor are unavailable."
        }
        TiDb => {
            "Connects over the MySQL protocol (port 4000). TiDB has no stored procedures, so routine folders are empty; designers and the Security editor are unavailable."
        }
        PlanetScale => {
            "Connects over the MySQL protocol; TLS is required. Foreign keys appear only if enabled for the database; designers and the Security editor are unavailable."
        }
        AzurePostgresFlexible | Neon => {
            "Plain PostgreSQL over a TLS-required connection. Designers are unavailable."
        }
        AzureMySqlFlexible => {
            "Plain MySQL over a TLS-required connection. Designers are unavailable."
        }
        AmazonAuroraPostgres | AmazonRdsPostgres | CloudSqlPostgres | Supabase => {
            "Plain PostgreSQL. Designers are unavailable."
        }
        AmazonAurora | AmazonRds | CloudSqlMySql => "Plain MySQL. Designers are unavailable.",
        Valkey | KeyDb | DragonflyDb => {
            "Speaks the Redis protocol: the command editor, key browser, Server Dashboard, Activity Monitor, Disk Usage and the Security tool's ACL page work as on Redis. Some commands differ (KeyDB and DragonflyDB have no FUNCTION LIST; DragonflyDB omits user= in CLIENT LIST). Redis Stack module replies (JSON, Search, TimeSeries, Bloom) are shaped whenever the server answers them: Valkey and KeyDB only with the modules loaded, DragonflyDB with its built-in JSON and Search."
        }
        AmazonElastiCache => {
            "Amazon ElastiCache for Redis or Valkey over the Redis protocol; in-transit encryption means TLS Require or Verify full. Cluster mode is supported: point the connection at the configuration endpoint and SQLLY routes each command to the node that owns its slot (one logical database, cross-slot multi-key commands refused); the Security editor is unavailable (Amazon manages its users in the AWS console). Redis Stack modules are not available on ElastiCache, so the Indexes folder and module shapes do not apply."
        }
        AmazonMemoryDb => {
            "Amazon MemoryDB over the Redis protocol; TLS is required and the ACL user name goes in the Username field. MemoryDB is always a cluster: SQLLY routes each command to the shard that owns its slot (one logical database, cross-slot multi-key commands refused) and the Cluster Topology view lists the shards; the Security editor is unavailable (Amazon manages its users in the AWS console). MemoryDB offers JSON and vector search as managed features; their JSON.* and FT.* replies are shaped and the Indexes folder appears when FT._LIST answers."
        }
        _ => return None,
    })
}

/// Setup text for first-party engines that need something outside SQLLY
/// (a client library, a naming convention). `None` where nothing applies.
#[cfg(test)]
pub fn engine_notes(db: DatabaseType) -> Option<&'static str> {
    match db.connection_kind() {
        ConnectionKind::OracleNet => Some(
            "Needs Oracle Instant Client (Basic or Basic Light) installed on this machine; SQLLY loads it when it connects. Database is the service name — prefix it with SID: to connect by SID, add \" AS SYSDBA\" after the username for an administrative session, or put a full connect descriptor in Server for TCPS or wallets. SSH tunnels, proxies and port-forwards reroute a plain host and port; a connect descriptor or tnsnames alias dials the listener directly. Backups run through RMAN or Data Pump on the server; SQL Agent, the guided designers and Extended Events are SQL Server features.",
        ),
        _ => None,
    }
}

/// What the GPUI admin panes implement today. This intentionally describes
/// the UI path, not merely SQL generators that exist in a lower crate.
pub fn admin_tool(db: DatabaseType, tool: AdminTool) -> Capability {
    use AdminTool::*;
    match tool {
        ActivityMonitor => match db.connection_kind() {
            ConnectionKind::Tds
            | ConnectionKind::PostgresWire
            | ConnectionKind::MySqlWire
            | ConnectionKind::ClickHouseHttp
            | ConnectionKind::OracleNet
            | ConnectionKind::Resp => Capability::Available,
            ConnectionKind::DuckDbFile => {
                Capability::Unavailable("explorer.capabilities.reason_activity_duckdb")
            }
            ConnectionKind::HranaHttp => {
                Capability::Unavailable("explorer.capabilities.reason_activity_libsql")
            }
            _ => Capability::Unavailable("explorer.capabilities.reason_activity_unsupported"),
        },
        // The live tools run MONITOR / SUBSCRIBE on dedicated connections —
        // every RESP-compatible engine (Redis, Valkey, KeyDB, Dragonfly,
        // ElastiCache, MemoryDB), desktop app only (the browser has no Redis
        // sockets; the tab says so there). The Activity Monitor's tab strip
        // reads this same entry, so the palette and the tabs agree. A server
        // that disables MONITOR says so when the capture starts.
        RedisProfiler | RedisPubSub => match db.connection_kind() {
            ConnectionKind::Resp => Capability::Available,
            _ => Capability::Unavailable("explorer.capabilities.reason_redis_live_unsupported"),
        },
        RedisMemoryAnalyzer => match db.connection_kind() {
            ConnectionKind::Resp => Capability::Available,
            _ => Capability::Unavailable("explorer.capabilities.reason_redis_memory_unsupported"),
        },
        // The key view is the row editor's counterpart for a command engine:
        // one key, its header, a paged value and reviewed per-type edits.
        RedisKeyView => match db.connection_kind() {
            ConnectionKind::Resp => Capability::Available,
            _ => Capability::Unavailable("explorer.capabilities.reason_redis_key_unsupported"),
        },
        // Every RESP engine: a standalone server answers that it is not a
        // cluster, which the pane shows as its error line.
        RedisClusterTopology => match db.connection_kind() {
            ConnectionKind::Resp => Capability::Available,
            _ => Capability::Unavailable("explorer.capabilities.reason_redis_cluster_unsupported"),
        },
        // SQLite has no sessions or counters, but the file's size, free list
        // and contents make a real (if smaller) dashboard.
        ServerDashboard => match db.connection_kind() {
            ConnectionKind::Tds
            | ConnectionKind::PostgresWire
            | ConnectionKind::MySqlWire
            | ConnectionKind::SqliteFile
            | ConnectionKind::HranaHttp
            | ConnectionKind::DuckDbFile
            | ConnectionKind::ClickHouseHttp
            | ConnectionKind::OracleNet
            | ConnectionKind::Resp => Capability::Available,
            _ => Capability::Unavailable("explorer.capabilities.reason_dashboard_unsupported"),
        },
        QueryInsights => match db.connection_kind() {
            ConnectionKind::Tds
            | ConnectionKind::PostgresWire
            | ConnectionKind::MySqlWire
            | ConnectionKind::ClickHouseHttp
            | ConnectionKind::OracleNet
            | ConnectionKind::Resp => Capability::Available,
            ConnectionKind::DuckDbFile => {
                Capability::Unavailable("explorer.capabilities.reason_insights_duckdb")
            }
            ConnectionKind::HranaHttp => {
                Capability::Unavailable("explorer.capabilities.reason_insights_libsql")
            }
            _ => Capability::Unavailable("explorer.capabilities.reason_insights_unsupported"),
        },
        SqlAgent => match db {
            DatabaseType::SqlServer => Capability::Available,
            DatabaseType::AzureSqlDatabase => {
                Capability::Unavailable("explorer.capabilities.reason_agent_azure")
            }
            DatabaseType::ClickHouse => {
                Capability::Unavailable("explorer.capabilities.reason_agent_clickhouse")
            }
            _ => Capability::Unavailable("explorer.capabilities.reason_agent_unsupported"),
        },
        BackupRestore => match db.connection_kind() {
            ConnectionKind::Tds
            | ConnectionKind::PostgresWire
            | ConnectionKind::MySqlWire
            | ConnectionKind::ClickHouseHttp => Capability::Available,
            ConnectionKind::SqliteFile => {
                Capability::Unavailable("explorer.capabilities.reason_backup_sqlite")
            }
            ConnectionKind::HranaHttp => {
                Capability::Unavailable("explorer.capabilities.reason_backup_libsql")
            }
            ConnectionKind::DuckDbFile => {
                Capability::Unavailable("explorer.capabilities.reason_backup_duckdb")
            }
            ConnectionKind::OracleNet => {
                Capability::Unavailable("explorer.capabilities.reason_backup_oracle")
            }
            ConnectionKind::Resp => {
                Capability::Unavailable("explorer.capabilities.reason_backup_redis")
            }
            _ => Capability::Unavailable("explorer.capabilities.reason_backup_unsupported"),
        },
        // libSQL reuses the SQLite collectors: they are plain SQL over
        // sqlite_master and the pragma table-valued functions.
        SchemaCompare => match db.connection_kind() {
            ConnectionKind::Tds
            | ConnectionKind::PostgresWire
            | ConnectionKind::MySqlWire
            | ConnectionKind::SqliteFile
            | ConnectionKind::DuckDbFile
            | ConnectionKind::HranaHttp
            | ConnectionKind::ClickHouseHttp
            | ConnectionKind::OracleNet => Capability::Available,
            ConnectionKind::Resp => {
                Capability::Unavailable("explorer.capabilities.reason_schema_compare_redis")
            }
            _ => Capability::Unavailable("explorer.capabilities.reason_schema_compare_unsupported"),
        },
        SchemaHistory => match db.connection_kind() {
            ConnectionKind::Tds
            | ConnectionKind::PostgresWire
            | ConnectionKind::MySqlWire
            | ConnectionKind::SqliteFile
            | ConnectionKind::DuckDbFile
            | ConnectionKind::HranaHttp
            | ConnectionKind::ClickHouseHttp
            | ConnectionKind::OracleNet => Capability::Available,
            ConnectionKind::Resp => {
                Capability::Unavailable("explorer.capabilities.reason_schema_history_redis")
            }
            _ => Capability::Unavailable("explorer.capabilities.reason_schema_history_unsupported"),
        },
        DataCompare => match db.connection_kind() {
            ConnectionKind::Tds
            | ConnectionKind::PostgresWire
            | ConnectionKind::MySqlWire
            | ConnectionKind::SqliteFile
            | ConnectionKind::DuckDbFile
            | ConnectionKind::HranaHttp
            | ConnectionKind::ClickHouseHttp
            | ConnectionKind::OracleNet => Capability::Available,
            ConnectionKind::Resp => {
                Capability::Unavailable("explorer.capabilities.reason_data_compare_redis")
            }
            _ => Capability::Unavailable("explorer.capabilities.reason_data_compare_unsupported"),
        },
        IndexAnalyzer => match db.connection_kind() {
            ConnectionKind::Tds
            | ConnectionKind::PostgresWire
            | ConnectionKind::MySqlWire
            | ConnectionKind::OracleNet => Capability::Available,
            ConnectionKind::SqliteFile => {
                Capability::Unavailable("explorer.capabilities.reason_index_sqlite")
            }
            ConnectionKind::HranaHttp => {
                Capability::Unavailable("explorer.capabilities.reason_index_libsql")
            }
            ConnectionKind::DuckDbFile => {
                Capability::Unavailable("explorer.capabilities.reason_index_duckdb")
            }
            ConnectionKind::ClickHouseHttp => {
                Capability::Unavailable("explorer.capabilities.reason_index_clickhouse")
            }
            ConnectionKind::Resp => {
                Capability::Unavailable("explorer.capabilities.reason_index_redis")
            }
            _ => Capability::Unavailable("explorer.capabilities.reason_index_unsupported"),
        },
        DiskUsage => match db.connection_kind() {
            ConnectionKind::Tds
            | ConnectionKind::PostgresWire
            | ConnectionKind::MySqlWire
            | ConnectionKind::SqliteFile
            | ConnectionKind::HranaHttp
            | ConnectionKind::DuckDbFile
            | ConnectionKind::ClickHouseHttp
            | ConnectionKind::OracleNet
            | ConnectionKind::Resp => Capability::Available,
            _ => Capability::Unavailable("explorer.capabilities.reason_disk_unsupported"),
        },
        ObjectBrowser => match db.connection_kind() {
            ConnectionKind::Tds
            | ConnectionKind::PostgresWire
            | ConnectionKind::MySqlWire
            | ConnectionKind::SqliteFile
            | ConnectionKind::HranaHttp
            | ConnectionKind::DuckDbFile
            | ConnectionKind::ClickHouseHttp
            | ConnectionKind::OracleNet => Capability::Available,
            ConnectionKind::Resp => {
                Capability::Unavailable("explorer.capabilities.reason_object_browser_redis")
            }
            _ => Capability::Unavailable("explorer.capabilities.reason_object_browser_unsupported"),
        },
        TestDataGenerator => match db.connection_kind() {
            ConnectionKind::Tds
            | ConnectionKind::PostgresWire
            | ConnectionKind::MySqlWire
            | ConnectionKind::SqliteFile
            | ConnectionKind::DuckDbFile
            | ConnectionKind::HranaHttp
            | ConnectionKind::ClickHouseHttp
            | ConnectionKind::OracleNet => Capability::Available,
            ConnectionKind::Resp => {
                Capability::Unavailable("explorer.capabilities.reason_test_data_redis")
            }
            _ => Capability::Unavailable("explorer.capabilities.reason_test_data_unsupported"),
        },
        DataTransfer => match db.connection_kind() {
            ConnectionKind::Tds
            | ConnectionKind::PostgresWire
            | ConnectionKind::MySqlWire
            | ConnectionKind::SqliteFile
            | ConnectionKind::HranaHttp
            | ConnectionKind::DuckDbFile
            | ConnectionKind::ClickHouseHttp
            | ConnectionKind::OracleNet => Capability::Available,
            ConnectionKind::Resp => {
                Capability::Unavailable("explorer.capabilities.reason_data_transfer_redis")
            }
            _ => Capability::Unavailable("explorer.capabilities.reason_data_transfer_unsupported"),
        },
        StructureEditor => match db.connection_kind() {
            ConnectionKind::Tds
            | ConnectionKind::PostgresWire
            | ConnectionKind::MySqlWire
            | ConnectionKind::SqliteFile
            | ConnectionKind::DuckDbFile
            | ConnectionKind::HranaHttp
            | ConnectionKind::ClickHouseHttp
            | ConnectionKind::OracleNet => Capability::Available,
            ConnectionKind::Resp => {
                Capability::Unavailable("explorer.capabilities.reason_structure_redis")
            }
            _ => Capability::Unavailable("explorer.capabilities.reason_structure_unsupported"),
        },
        Security | Designers | ExtendedEvents => {
            if tool == AdminTool::Security
                && sqlly_services::tool_dialects::security_dialect(db).is_some()
            {
                return Capability::Available;
            }
            match db.connection_kind() {
                ConnectionKind::Tds => Capability::Available,
                ConnectionKind::DuckDbFile if tool == AdminTool::Security => {
                    Capability::Unavailable("explorer.capabilities.reason_security_duckdb")
                }
                ConnectionKind::HranaHttp if tool == AdminTool::Security => {
                    Capability::Unavailable("explorer.capabilities.reason_security_libsql")
                }
                // Redis ACL users: the Security tool opens the ACL page
                // (ACL LIST / ACL LOG, reviewed ACL SETUSER / DELUSER
                // scripts). AWS manages ElastiCache and MemoryDB users
                // outside the server, which refuses ACL SETUSER.
                ConnectionKind::Resp if tool == AdminTool::Security => match db {
                    DatabaseType::AmazonElastiCache | DatabaseType::AmazonMemoryDb => {
                        Capability::Unavailable("explorer.capabilities.reason_security_aws")
                    }
                    _ => Capability::Available,
                },
                ConnectionKind::ClickHouseHttp => Capability::Unavailable(match tool {
                    Designers => "explorer.capabilities.reason_designers_clickhouse",
                    ExtendedEvents => "explorer.capabilities.reason_xevents_clickhouse",
                    _ => unreachable!(),
                }),
                _ => Capability::Unavailable(match tool {
                    Security => "explorer.capabilities.reason_security_unsupported",
                    Designers => "explorer.capabilities.reason_designers_unsupported",
                    ExtendedEvents => "explorer.capabilities.reason_xevents_unsupported",
                    _ => unreachable!(),
                }),
            }
        }
    }
}

/// Object dependency listings (View Dependencies / Find Usages server side).
pub fn dependencies(db: DatabaseType) -> Capability {
    match db.connection_kind() {
        ConnectionKind::Tds
        | ConnectionKind::PostgresWire
        | ConnectionKind::MySqlWire
        | ConnectionKind::SqliteFile
        | ConnectionKind::DuckDbFile
        | ConnectionKind::HranaHttp
        | ConnectionKind::ClickHouseHttp
        | ConnectionKind::OracleNet => Capability::Available,
        ConnectionKind::Resp => {
            Capability::Unavailable("explorer.capabilities.reason_dependencies_redis")
        }
        _ => Capability::Unavailable("explorer.capabilities.reason_dependencies_unsupported"),
    }
}

/// Why a TCP transport hop (SSH tunnel, Kubernetes port-forward) cannot be
/// used for an engine. Typed so the connection editor picks its own catalog
/// key without matching on the English capability reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportBlock {
    /// The standalone browser preview: the desktop app has it.
    DesktopRequired,
    /// A browser page served by a relay or SQLly Embedded bridge.
    BrowserUnavailable,
    /// SQLite opens a local file; there is no server endpoint.
    SqliteFile,
    /// DuckDB opens a local file; there is no server endpoint.
    DuckDbFile,
    /// The engine's driver does not support the transport yet.
    EngineUnsupported,
}

/// The engine matrix for SSH tunnels (and named tunnel profiles) and
/// Kubernetes port-forward: both wrap a TCP endpoint, which file engines lack
/// and the browser build cannot open.
pub fn transport_block(db: DatabaseType) -> Option<TransportBlock> {
    if cfg!(target_arch = "wasm32") {
        return Some(if crate::browser_preview::served() {
            TransportBlock::BrowserUnavailable
        } else {
            TransportBlock::DesktopRequired
        });
    }
    match db.connection_kind() {
        // Oracle: the tunnel's local endpoint replaces a plain host + port
        // in the connect string; descriptors and aliases pass through.
        ConnectionKind::Tds
        | ConnectionKind::PostgresWire
        | ConnectionKind::MySqlWire
        | ConnectionKind::HranaHttp
        | ConnectionKind::ClickHouseHttp
        | ConnectionKind::Resp
        | ConnectionKind::OracleNet => None,
        ConnectionKind::SqliteFile => Some(TransportBlock::SqliteFile),
        ConnectionKind::DuckDbFile => Some(TransportBlock::DuckDbFile),
        _ => Some(TransportBlock::EngineUnsupported),
    }
}

/// AWS RDS / Google Cloud SQL IAM database authentication.
pub fn cloud_iam_auth(db: DatabaseType) -> Capability {
    if cfg!(target_arch = "wasm32") {
        return Capability::Unavailable(crate::browser_preview::pick(
            "explorer.capabilities.reason_iam_desktop",
            "explorer.capabilities.reason_iam_browser",
        ));
    }
    match db.connection_kind() {
        ConnectionKind::PostgresWire | ConnectionKind::MySqlWire => Capability::Available,
        ConnectionKind::Tds => Capability::Unavailable("explorer.capabilities.reason_iam_tds"),
        ConnectionKind::SqliteFile => {
            Capability::Unavailable("explorer.capabilities.reason_iam_sqlite")
        }
        ConnectionKind::HranaHttp => {
            Capability::Unavailable("explorer.capabilities.reason_iam_libsql")
        }
        ConnectionKind::DuckDbFile => {
            Capability::Unavailable("explorer.capabilities.reason_iam_duckdb")
        }
        ConnectionKind::ClickHouseHttp => {
            Capability::Unavailable("explorer.capabilities.reason_iam_clickhouse")
        }
        ConnectionKind::OracleNet => {
            Capability::Unavailable("explorer.capabilities.reason_iam_oracle")
        }
        ConnectionKind::Resp => Capability::Available,
        _ => Capability::Unavailable("explorer.capabilities.reason_iam_unsupported"),
    }
}

/// SOCKS5 / HTTP CONNECT proxy hops share the same engine gate as SSH tunnels.
pub fn proxy_transport(db: DatabaseType) -> Capability {
    if cfg!(target_arch = "wasm32") {
        return Capability::Unavailable(crate::browser_preview::pick(
            "explorer.capabilities.reason_proxy_desktop",
            "explorer.capabilities.reason_proxy_browser",
        ));
    }
    match db.connection_kind() {
        ConnectionKind::Tds
        | ConnectionKind::PostgresWire
        | ConnectionKind::MySqlWire
        | ConnectionKind::HranaHttp
        | ConnectionKind::ClickHouseHttp
        | ConnectionKind::Resp
        | ConnectionKind::OracleNet => Capability::Available,
        ConnectionKind::SqliteFile => {
            Capability::Unavailable("explorer.capabilities.reason_proxy_sqlite")
        }
        ConnectionKind::DuckDbFile => {
            Capability::Unavailable("explorer.capabilities.reason_proxy_duckdb")
        }
        _ => Capability::Unavailable("explorer.capabilities.reason_proxy_unsupported"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every reason is a catalog key that both locales render; no capability
    /// carries English prose for a caller to show or match.
    #[test]
    fn every_unavailable_reason_is_a_known_catalog_key() {
        use AdminTool::*;
        let tools = [
            BackupRestore,
            ActivityMonitor,
            ServerDashboard,
            SqlAgent,
            Security,
            Designers,
            StructureEditor,
            SchemaCompare,
            SchemaHistory,
            DataCompare,
            QueryInsights,
            ExtendedEvents,
            DiskUsage,
            ObjectBrowser,
            IndexAnalyzer,
            TestDataGenerator,
            DataTransfer,
            RedisProfiler,
            RedisPubSub,
            RedisMemoryAnalyzer,
            RedisKeyView,
            RedisClusterTopology,
        ];
        let mut checked = 0;
        for db in DatabaseType::ALL {
            let mut caps = vec![
                sql_editing(db),
                row_editing(db),
                cell_default_in_update(db),
                database_ddl(db),
                maintenance(db),
                procedure_mode(db),
                include_statistics(db),
                manual_transactions(db),
                actual_plan(db),
                estimated_plan(db),
                dependencies(db),
                cloud_iam_auth(db),
                proxy_transport(db),
            ];
            caps.extend(tools.iter().map(|tool| admin_tool(db, *tool)));
            for key in caps.into_iter().filter_map(Capability::reason_key) {
                assert!(
                    sqlly_i18n::contains_key(key),
                    "{db:?}: unknown reason key {key}"
                );
                checked += 1;
            }
        }
        assert!(sqlly_i18n::contains_key(
            command_capability("tools.sqlAgent", None)
                .and_then(Capability::reason_key)
                .unwrap()
        ));
        assert!(checked > 100);
    }

    #[test]
    fn row_editing_covers_native_driver_engines() {
        assert!(row_editing(DatabaseType::SqlServer).is_available());
        assert!(row_editing(DatabaseType::AzureSqlDatabase).is_available());
        assert!(row_editing(DatabaseType::Postgres).is_available());
        assert!(row_editing(DatabaseType::MySql).is_available());
        assert!(row_editing(DatabaseType::Sqlite).is_available());
        assert!(row_editing(DatabaseType::DuckDb).is_available());
        assert!(row_editing(DatabaseType::Oracle).is_available());
        assert!(cell_default_in_update(DatabaseType::Oracle).is_available());
        assert!(row_editing(DatabaseType::Db2).reason().is_some());
    }

    #[test]
    fn maintenance_covers_the_four_native_families_with_a_reason_elsewhere() {
        assert!(maintenance(DatabaseType::SqlServer).is_available());
        assert!(maintenance(DatabaseType::AzureSqlDatabase).is_available());
        assert!(maintenance(DatabaseType::Postgres).is_available());
        assert!(maintenance(DatabaseType::MariaDb).is_available());
        assert!(maintenance(DatabaseType::Sqlite).is_available());
        // Wire-compatible engines without the verbs get the honest reason,
        // not a VACUUM that CockroachDB would reject.
        let crdb = maintenance(DatabaseType::CockroachDb);
        assert!(crdb.reason().unwrap().contains("SQL Server, PostgreSQL"));
        assert!(maintenance(DatabaseType::TiDb).reason().is_some());
        assert!(maintenance(DatabaseType::Oracle).is_available());
        assert!(maintenance(DatabaseType::Db2).reason().is_some());
    }

    #[test]
    fn procedure_mode_distinguishes_no_procs_from_unported() {
        assert!(procedure_mode(DatabaseType::SqlServer).is_available());
        assert!(procedure_mode(DatabaseType::Postgres).is_available());
        // SQLite has no stored procedures at all.
        assert_eq!(
            procedure_mode(DatabaseType::Sqlite),
            Capability::Unavailable("explorer.capabilities.reason_procedure_no_procedures")
        );
        // MySQL/MariaDB have procedures, but DDL auto-commits — the rollback
        // sandbox cannot exist there, so the mode stays honestly unavailable.
        let mysql = procedure_mode(DatabaseType::MySql);
        assert!(!mysql.is_available());
        assert!(mysql.reason().unwrap().contains("rolled back"));
    }

    #[test]
    fn include_statistics_covers_tds_and_mysql_lineage() {
        assert!(include_statistics(DatabaseType::SqlServer).is_available());
        assert!(include_statistics(DatabaseType::MySql).is_available());
        assert!(include_statistics(DatabaseType::MariaDb).is_available());
        // PostgreSQL's reason explains WHY (double execution), not just "no".
        let pg = include_statistics(DatabaseType::Postgres);
        assert!(pg.reason().unwrap().contains("twice"));
        assert!(!include_statistics(DatabaseType::Sqlite).is_available());
    }

    #[test]
    fn plan_unavailable_reason_names_the_engine_for_command_stores() {
        for db in [
            DatabaseType::Redis,
            DatabaseType::Valkey,
            DatabaseType::KeyDb,
            DatabaseType::DragonflyDb,
            DatabaseType::AmazonElastiCache,
            DatabaseType::AmazonMemoryDb,
        ] {
            for kind in [PlanKind::Actual, PlanKind::Estimated] {
                let reason = plan_unavailable_reason(db, kind).expect("gated");
                assert!(
                    reason.starts_with(&format!(
                        "Query plans are disabled for this database type ({}).",
                        db.display_name()
                    )),
                    "{db:?} {kind:?}: {reason}"
                );
                assert!(reason.contains("SLOWLOG"), "{reason}");
                assert!(reason.ends_with('.'), "{reason}");
            }
        }
    }

    #[test]
    fn estimated_only_engines_name_the_actual_gap_and_point_to_estimated() {
        for db in [
            DatabaseType::Sqlite,
            DatabaseType::LibSql,
            DatabaseType::ClickHouse,
        ] {
            let reason = plan_unavailable_reason(db, PlanKind::Actual).expect("actual gated");
            assert!(
                reason.starts_with(&format!(
                    "Actual execution plans are disabled for this database type ({}).",
                    db.display_name()
                )),
                "{reason}"
            );
            assert!(
                reason.contains("Explain Query (Estimated, No Execution)"),
                "{reason}"
            );
            assert_eq!(plan_unavailable_reason(db, PlanKind::Estimated), None);
        }
    }

    /// Every engine the connection editor offers either captures a plan
    /// kind or says, by engine name, that plans are disabled — never an
    /// anonymous "not available" that leaves the user guessing.
    #[test]
    fn every_selectable_engine_gets_a_named_reason_or_a_plan() {
        for db in crate::engine_form::SELECTABLE_ENGINES {
            for kind in [PlanKind::Actual, PlanKind::Estimated] {
                match plan_unavailable_reason(db, kind) {
                    None => {
                        assert!(plan(db, kind).is_available(), "{db:?} {kind:?}");
                        assert!(
                            sqlly_services::plan_dialect(db).is_some(),
                            "{db:?} claims {kind:?} plans but has no plan dialect"
                        );
                    }
                    Some(reason) => {
                        assert!(!plan(db, kind).is_available());
                        assert!(
                            reason.contains(&format!(
                                "disabled for this database type ({})",
                                db.display_name()
                            )),
                            "{db:?} {kind:?}: {reason}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn unimplemented_engines_still_name_themselves() {
        let reason = plan_unavailable_reason(DatabaseType::Db2, PlanKind::Estimated).unwrap();
        assert_eq!(
            reason,
            "Query plans are disabled for this database type (IBM Db2). SQLLY cannot capture plans from this engine yet."
        );
    }

    #[test]
    fn plan_commands_share_the_plan_reason() {
        let redis = Some(DatabaseType::Redis);
        for (id, kind) in [
            ("query.explainPlan", PlanKind::Actual),
            ("plan.profile", PlanKind::Actual),
            ("query.explainEstimated", PlanKind::Estimated),
        ] {
            assert_eq!(plan_command_kind(id), Some(kind));
            assert_eq!(
                command_unavailable_reason(id, redis),
                plan_unavailable_reason(DatabaseType::Redis, kind),
                "{id}"
            );
            assert!(command_capability(id, redis).is_some_and(|c| !c.is_available()));
            assert_eq!(
                command_unavailable_reason(id, Some(DatabaseType::SqlServer)),
                None
            );
        }
        // SQLite: the actual-plan commands are gated, estimated is not.
        let sqlite = Some(DatabaseType::Sqlite);
        assert!(command_unavailable_reason("plan.profile", sqlite).is_some());
        assert!(command_unavailable_reason("query.explainEstimated", sqlite).is_none());
        // Opening or pasting a saved plan works on any engine.
        for id in ["plan.open", "plan.paste"] {
            assert_eq!(plan_command_kind(id), None);
            assert_eq!(command_unavailable_reason(id, redis), None);
        }
        // Disconnected: the connect prompt, not an engine reason.
        assert_eq!(
            command_unavailable_reason("query.explainPlan", None).as_deref(),
            Some("Connect to a database to use this command")
        );
        // Non-plan commands keep their own capability reason.
        assert_eq!(
            command_unavailable_reason("tools.sqlAgent", Some(DatabaseType::Postgres)).as_deref(),
            admin_tool(DatabaseType::Postgres, AdminTool::SqlAgent)
                .reason()
                .as_deref()
        );
    }

    #[test]
    fn plan_capture_gate_matches_the_per_kind_reasons() {
        assert_eq!(
            PlanCaptureGate::for_engine(None),
            PlanCaptureGate::default()
        );
        assert_eq!(
            PlanCaptureGate::for_engine(Some(DatabaseType::SqlServer)),
            PlanCaptureGate::default()
        );
        let sqlite = PlanCaptureGate::for_engine(Some(DatabaseType::Sqlite));
        assert!(sqlite.reason(PlanKind::Actual).is_some());
        assert!(sqlite.reason(PlanKind::Estimated).is_none());
        let redis = PlanCaptureGate::for_engine(Some(DatabaseType::Valkey));
        for kind in [PlanKind::Actual, PlanKind::Estimated] {
            assert_eq!(
                redis.reason(kind).map(str::to_string),
                plan_unavailable_reason(DatabaseType::Valkey, kind)
            );
        }
    }

    #[test]
    fn plans_reflect_shipped_dialects() {
        for db in [
            DatabaseType::SqlServer,
            DatabaseType::Postgres,
            DatabaseType::MySql,
            DatabaseType::MariaDb,
        ] {
            assert!(actual_plan(db).is_available(), "{db:?}");
        }
        assert!(!actual_plan(DatabaseType::Sqlite).is_available());
        assert!(actual_plan(DatabaseType::Oracle).is_available());
        assert!(!actual_plan(DatabaseType::Db2).is_available());
        for db in [
            DatabaseType::SqlServer,
            DatabaseType::Postgres,
            DatabaseType::MySql,
            DatabaseType::Sqlite,
            DatabaseType::Oracle,
        ] {
            assert!(estimated_plan(db).is_available(), "{db:?}");
        }
        assert!(!estimated_plan(DatabaseType::Db2).is_available());
    }

    #[test]
    fn plan_reasons_are_specific_to_requested_kind() {
        let sqlite_actual = plan(DatabaseType::Sqlite, PlanKind::Actual)
            .reason()
            .unwrap();
        assert!(sqlite_actual.contains("estimated query plan"));
        assert!(plan(DatabaseType::Sqlite, PlanKind::Estimated).is_available());
        assert!(plan(DatabaseType::SqlServer, PlanKind::Actual).is_available());
        assert!(plan(DatabaseType::SqlServer, PlanKind::Estimated).is_available());
    }

    #[test]
    fn structure_editor_available_on_sqlite_while_designers_stay_tds_only() {
        assert!(admin_tool(DatabaseType::Sqlite, AdminTool::StructureEditor).is_available());
        assert!(admin_tool(DatabaseType::Sqlite, AdminTool::ObjectBrowser).is_available());
        assert!(!admin_tool(DatabaseType::Sqlite, AdminTool::Designers).is_available());
    }

    #[test]
    fn database_ddl_unavailable_on_sqlite() {
        let cap = database_ddl(DatabaseType::Sqlite);
        assert!(!cap.is_available());
        assert_eq!(
            cap.reason_key(),
            Some("explorer.capabilities.reason_database_ddl_sqlite")
        );
        assert!(database_ddl(DatabaseType::Postgres).is_available());
    }

    #[test]
    fn admin_matrix_matches_wired_panes() {
        for db in [
            DatabaseType::SqlServer,
            DatabaseType::Postgres,
            DatabaseType::MySql,
        ] {
            assert!(admin_tool(db, AdminTool::ActivityMonitor).is_available());
            assert_eq!(
                admin_tool(db, AdminTool::ServerDashboard).is_available(),
                admin_tool(db, AdminTool::ActivityMonitor).is_available(),
            );
        }
        // A file database has no sessions to monitor, but its file statistics
        // still make a dashboard.
        assert!(!admin_tool(DatabaseType::Sqlite, AdminTool::ActivityMonitor).is_available());
        assert!(admin_tool(DatabaseType::Sqlite, AdminTool::ServerDashboard).is_available());

        assert!(admin_tool(DatabaseType::SqlServer, AdminTool::QueryInsights).is_available());
        assert!(admin_tool(DatabaseType::Postgres, AdminTool::QueryInsights).is_available());
        assert!(admin_tool(DatabaseType::MySql, AdminTool::QueryInsights).is_available());

        assert!(admin_tool(DatabaseType::SqlServer, AdminTool::SqlAgent).is_available());
        assert!(!admin_tool(DatabaseType::AzureSqlDatabase, AdminTool::SqlAgent).is_available());
        assert!(
            admin_tool(DatabaseType::AzureSqlDatabase, AdminTool::ExtendedEvents).is_available()
        );

        for db in [
            DatabaseType::SqlServer,
            DatabaseType::Postgres,
            DatabaseType::MySql,
            DatabaseType::MariaDb,
            DatabaseType::Sqlite,
        ] {
            assert!(
                admin_tool(db, AdminTool::SchemaCompare).is_available(),
                "{db:?}"
            );
            assert!(
                admin_tool(db, AdminTool::DataCompare).is_available(),
                "{db:?}"
            );
        }
        assert!(admin_tool(DatabaseType::Oracle, AdminTool::SchemaCompare).is_available());
        assert!(admin_tool(DatabaseType::Oracle, AdminTool::DataCompare).is_available());
        assert!(admin_tool(DatabaseType::Sqlite, AdminTool::DataTransfer).is_available());
        assert!(admin_tool(DatabaseType::Oracle, AdminTool::DataTransfer).is_available());
        assert!(!admin_tool(DatabaseType::Db2, AdminTool::DataTransfer).is_available());

        for db in [
            DatabaseType::CockroachDb,
            DatabaseType::TiDb,
            DatabaseType::PlanetScale,
        ] {
            assert!(
                admin_tool(db, AdminTool::ActivityMonitor).is_available(),
                "{db:?}"
            );
            assert_eq!(
                admin_tool(db, AdminTool::ServerDashboard).is_available(),
                admin_tool(db, AdminTool::ActivityMonitor).is_available(),
                "{db:?}"
            );
            assert!(
                admin_tool(db, AdminTool::QueryInsights).is_available(),
                "{db:?}"
            );
            assert!(
                admin_tool(db, AdminTool::SchemaCompare).is_available(),
                "{db:?}"
            );
            assert!(
                admin_tool(db, AdminTool::DiskUsage).is_available(),
                "{db:?}"
            );
            assert!(
                !admin_tool(db, AdminTool::Security).is_available(),
                "{db:?}"
            );
            assert!(
                !admin_tool(db, AdminTool::Designers).is_available(),
                "{db:?}"
            );
            assert!(
                !admin_tool(db, AdminTool::ExtendedEvents).is_available(),
                "{db:?}"
            );
            assert!(
                !admin_tool(db, AdminTool::SqlAgent).is_available(),
                "{db:?}"
            );
            assert!(
                admin_tool(db, AdminTool::BackupRestore).is_available(),
                "{db:?}"
            );
        }
    }

    #[test]
    fn compatibility_notes_cover_every_compatible_engine() {
        for db in DatabaseType::ALL {
            let has_note = compatibility_notes(db).is_some();
            assert_eq!(has_note, db.compatible_with().is_some(), "{db:?}");
            if let Some(note) = compatibility_notes(db) {
                if !admin_tool(db, AdminTool::Security).is_available() {
                    assert!(note.contains("Security editor"), "{db:?}");
                }
                if db.requires_tls() {
                    assert!(note.contains("TLS"), "{db:?}");
                }
            }
        }
    }

    #[test]
    fn procedure_mode_on_tidb_reports_no_stored_procedures() {
        assert_eq!(
            procedure_mode(DatabaseType::TiDb),
            Capability::Unavailable("explorer.capabilities.reason_procedure_no_procedures")
        );
    }

    #[test]
    fn mysql_family_procedure_mode_reason_covers_tidb() {
        let tidb = procedure_mode(DatabaseType::TiDb);
        assert!(!tidb.is_available());
        let planet = procedure_mode(DatabaseType::PlanetScale);
        assert!(!planet.is_available());
        assert!(planet.reason().unwrap().contains("MySQL-family"));
    }

    #[test]
    fn command_ids_share_the_central_matrix() {
        assert_eq!(
            AdminTool::from_command_id("tools.activityMonitor"),
            Some(AdminTool::ActivityMonitor)
        );
        assert_eq!(
            AdminTool::from_command_id("tools.serverDashboard"),
            Some(AdminTool::ServerDashboard)
        );
        assert!(
            command_capability("tools.activityMonitor", Some(DatabaseType::Postgres))
                .unwrap()
                .is_available()
        );
        assert!(
            command_capability("tools.queryStore", Some(DatabaseType::Postgres))
                .unwrap()
                .is_available()
        );
        assert!(
            command_capability("tools.activityMonitor", None)
                .unwrap()
                .reason()
                .unwrap()
                .contains("Connect")
        );
        assert!(command_capability("file.newQuery", None).is_none());
    }

    #[test]
    fn connection_gated_commands_need_only_a_connection() {
        for id in ["tools.exportSchemaDocs", "tools.tenantScoping"] {
            assert!(
                command_capability(id, Some(DatabaseType::Sqlite))
                    .unwrap()
                    .is_available(),
                "{id} should be available on every engine once connected"
            );
            assert!(
                command_capability(id, None)
                    .unwrap()
                    .reason()
                    .unwrap()
                    .contains("Connect"),
                "{id} should require a connection"
            );
        }
    }

    #[test]
    fn dependencies_cover_native_engines() {
        assert!(dependencies(DatabaseType::SqlServer).is_available());
        assert!(dependencies(DatabaseType::Postgres).is_available());
        assert!(dependencies(DatabaseType::MySql).is_available());
        assert!(dependencies(DatabaseType::MariaDb).is_available());
        assert!(dependencies(DatabaseType::Sqlite).is_available());
        assert!(dependencies(DatabaseType::DuckDb).is_available());
        assert!(dependencies(DatabaseType::ClickHouse).is_available());
        assert!(dependencies(DatabaseType::Oracle).is_available());
        assert!(dependencies(DatabaseType::Db2).reason().is_some());
    }

    #[test]
    fn sql_agent_is_tds_only() {
        assert!(admin_tool(DatabaseType::SqlServer, AdminTool::SqlAgent).is_available());
        assert!(!admin_tool(DatabaseType::Postgres, AdminTool::SqlAgent).is_available());
    }

    #[test]
    fn plan_dialect_selectable_engines_match_tool_dialect_table() {
        use crate::engine_form::SELECTABLE_ENGINES;
        use sqlly_services::plans::PlanDialect;
        use sqlly_services::{
            ActivityDialect, DiskDialect, IndexDialect, ProcToolDialect, activity_dialect,
            disk_dialect, index_dialect, plan_dialect, proc_dialect,
        };

        let expected = |db: DatabaseType| {
            let base = db.compatible_with().unwrap_or(db);
            match base {
                DatabaseType::SqlServer | DatabaseType::AzureSqlDatabase => (
                    Some(PlanDialect::SqlServer),
                    Some(IndexDialect::SqlServer),
                    Some(DiskDialect::SqlServer),
                    Some(ActivityDialect::SqlServer),
                    ProcToolDialect::TSql,
                ),
                DatabaseType::Postgres => (
                    Some(PlanDialect::Postgres),
                    Some(IndexDialect::Postgres),
                    Some(DiskDialect::Postgres),
                    Some(ActivityDialect::Postgres),
                    ProcToolDialect::Postgres,
                ),
                DatabaseType::MySql => (
                    Some(PlanDialect::MySql),
                    Some(IndexDialect::MySql),
                    Some(DiskDialect::MySql),
                    Some(ActivityDialect::MySql),
                    ProcToolDialect::TSql,
                ),
                DatabaseType::MariaDb => (
                    Some(PlanDialect::MariaDb),
                    Some(IndexDialect::MySql),
                    Some(DiskDialect::MySql),
                    Some(ActivityDialect::MariaDb),
                    ProcToolDialect::TSql,
                ),
                DatabaseType::Sqlite => (
                    Some(PlanDialect::Sqlite),
                    None,
                    Some(DiskDialect::Sqlite),
                    None,
                    ProcToolDialect::TSql,
                ),
                DatabaseType::DuckDb => (
                    Some(PlanDialect::DuckDb),
                    None,
                    Some(DiskDialect::DuckDb),
                    None,
                    ProcToolDialect::TSql,
                ),
                DatabaseType::ClickHouse => (
                    Some(PlanDialect::ClickHouse),
                    None,
                    Some(DiskDialect::ClickHouse),
                    Some(ActivityDialect::ClickHouse),
                    ProcToolDialect::TSql,
                ),
                DatabaseType::Oracle => (
                    Some(PlanDialect::Oracle),
                    Some(IndexDialect::Oracle),
                    Some(DiskDialect::Oracle),
                    Some(ActivityDialect::Oracle),
                    ProcToolDialect::TSql,
                ),
                DatabaseType::LibSql => (
                    Some(PlanDialect::Sqlite),
                    None,
                    Some(DiskDialect::Sqlite),
                    None,
                    ProcToolDialect::TSql,
                ),
                DatabaseType::Redis
                | DatabaseType::Valkey
                | DatabaseType::KeyDb
                | DatabaseType::DragonflyDb
                | DatabaseType::AmazonElastiCache
                | DatabaseType::AmazonMemoryDb => (
                    None,
                    None,
                    Some(DiskDialect::Redis),
                    Some(ActivityDialect::Redis),
                    ProcToolDialect::TSql,
                ),
                _ => unreachable!("SELECTABLE_ENGINES only lists supported engines"),
            }
        };

        for db in SELECTABLE_ENGINES {
            let (plan, index, disk, activity, proc) = expected(db);
            assert_eq!(plan_dialect(db), plan, "{db:?} plan");
            assert_eq!(index_dialect(db), index, "{db:?} index");
            assert_eq!(disk_dialect(db), disk, "{db:?} disk");
            assert_eq!(activity_dialect(db), activity, "{db:?} activity");
            assert_eq!(proc_dialect(db), proc, "{db:?} proc");
        }
    }

    #[test]
    fn duckdb_is_gated_like_a_file_engine_with_real_plans() {
        let db = DatabaseType::DuckDb;
        assert!(estimated_plan(db).is_available());
        assert!(actual_plan(db).is_available());
        assert!(row_editing(db).is_available());
        assert!(cell_default_in_update(db).is_available());
        assert!(maintenance(db).is_available());
        assert!(dependencies(db).is_available());
        assert!(database_ddl(db).reason().unwrap().contains("DuckDB"));
        for tool in [
            AdminTool::ServerDashboard,
            AdminTool::DiskUsage,
            AdminTool::ObjectBrowser,
            AdminTool::SchemaCompare,
            AdminTool::SchemaHistory,
            AdminTool::DataCompare,
            AdminTool::TestDataGenerator,
            AdminTool::DataTransfer,
            AdminTool::StructureEditor,
        ] {
            assert!(admin_tool(db, tool).is_available(), "{tool:?}");
        }
        // Tools with no DuckDB counterpart name DuckDB in their reason.
        for tool in [
            AdminTool::ActivityMonitor,
            AdminTool::Security,
            AdminTool::QueryInsights,
            AdminTool::IndexAnalyzer,
            AdminTool::BackupRestore,
        ] {
            let reason = admin_tool(db, tool).reason().unwrap_or_default();
            assert!(reason.contains("DuckDB"), "{tool:?}: {reason}");
        }
        assert!(!admin_tool(db, AdminTool::SqlAgent).is_available());
        assert!(!admin_tool(db, AdminTool::Designers).is_available());
        assert!(!admin_tool(db, AdminTool::ExtendedEvents).is_available());
        assert!(
            admin_tool(db, AdminTool::BackupRestore)
                .reason()
                .unwrap()
                .contains("EXPORT DATABASE")
        );
        assert_eq!(transport_block(db), Some(TransportBlock::DuckDbFile));
        assert!(proxy_transport(db).reason().unwrap().contains("DuckDB"));
        assert!(cloud_iam_auth(db).reason().unwrap().contains("DuckDB"));
        // First-party engine: no wire-compatibility caveat.
        assert!(compatibility_notes(db).is_none());
    }

    #[test]
    fn clickhouse_is_gated_as_a_network_engine_with_estimated_plans_only() {
        let db = DatabaseType::ClickHouse;
        assert!(estimated_plan(db).is_available());
        assert!(
            actual_plan(db)
                .reason()
                .unwrap()
                .contains("EXPLAIN ANALYZE")
        );
        assert!(row_editing(db).reason().unwrap().contains("ClickHouse"));
        assert!(
            database_ddl(db)
                .reason()
                .unwrap()
                .contains("CREATE DATABASE")
        );
        assert!(maintenance(db).is_available());
        assert!(admin_tool(db, AdminTool::ActivityMonitor).is_available());
        assert!(admin_tool(db, AdminTool::ServerDashboard).is_available());
        assert!(admin_tool(db, AdminTool::DiskUsage).is_available());
        assert!(admin_tool(db, AdminTool::ObjectBrowser).is_available());
        // Every tool with a ClickHouse counterpart is wired: query log
        // insights, the compare/history/transfer family, the structure
        // editor, the read-only security view, and BACKUP / RESTORE.
        assert!(admin_tool(db, AdminTool::QueryInsights).is_available());
        assert!(admin_tool(db, AdminTool::Security).is_available());
        assert!(admin_tool(db, AdminTool::SchemaCompare).is_available());
        assert!(admin_tool(db, AdminTool::SchemaHistory).is_available());
        assert!(admin_tool(db, AdminTool::DataCompare).is_available());
        assert!(admin_tool(db, AdminTool::DataTransfer).is_available());
        assert!(admin_tool(db, AdminTool::StructureEditor).is_available());
        assert!(admin_tool(db, AdminTool::TestDataGenerator).is_available());
        assert!(admin_tool(db, AdminTool::BackupRestore).is_available());
        assert!(dependencies(db).is_available());
        // What has no counterpart names ClickHouse in its reason.
        assert!(
            admin_tool(db, AdminTool::IndexAnalyzer)
                .reason()
                .unwrap()
                .contains("ClickHouse")
        );
        assert!(
            admin_tool(db, AdminTool::SqlAgent)
                .reason()
                .unwrap()
                .contains("ClickHouse")
        );
        assert!(
            admin_tool(db, AdminTool::Designers)
                .reason()
                .unwrap()
                .contains("ClickHouse")
        );
        assert!(
            admin_tool(db, AdminTool::ExtendedEvents)
                .reason()
                .unwrap()
                .contains("ClickHouse")
        );
        assert!(transport_block(db).is_none());
        assert!(proxy_transport(db).is_available());
        assert!(cloud_iam_auth(db).reason().unwrap().contains("ClickHouse"));
        assert!(!manual_transactions(db).is_available());
        assert!(!include_statistics(db).is_available());
        assert!(compatibility_notes(db).is_none());
    }

    /// libSQL / Turso: SQLite's SQL-only tools (plans, dashboard, disk,
    /// compare, generator, structure editor, dependencies), row editing and
    /// the Data Transfer sink come along; server-side admin (activity,
    /// query insights, index usage, security, backup) stays off with a
    /// reason naming libSQL; network transports apply because it is a
    /// server.
    #[test]
    fn libsql_is_gated_as_sqlite_over_the_network() {
        let db = DatabaseType::LibSql;
        assert!(estimated_plan(db).is_available());
        assert!(actual_plan(db).reason().unwrap().contains("libSQL"));
        assert!(row_editing(db).is_available());
        assert!(
            cell_default_in_update(db)
                .reason()
                .unwrap()
                .contains("libSQL")
        );
        assert!(
            database_ddl(db)
                .reason()
                .unwrap()
                .contains("turso db create")
        );
        assert!(maintenance(db).is_available());
        assert_eq!(
            procedure_mode(db),
            Capability::Unavailable("explorer.capabilities.reason_procedure_no_procedures")
        );
        assert!(!include_statistics(db).is_available());
        assert!(!manual_transactions(db).is_available());
        assert!(admin_tool(db, AdminTool::ServerDashboard).is_available());
        assert!(admin_tool(db, AdminTool::DiskUsage).is_available());
        assert!(admin_tool(db, AdminTool::ObjectBrowser).is_available());
        assert!(admin_tool(db, AdminTool::SchemaCompare).is_available());
        assert!(admin_tool(db, AdminTool::SchemaHistory).is_available());
        assert!(admin_tool(db, AdminTool::DataCompare).is_available());
        assert!(admin_tool(db, AdminTool::TestDataGenerator).is_available());
        assert!(admin_tool(db, AdminTool::StructureEditor).is_available());
        assert!(dependencies(db).is_available());
        assert!(
            admin_tool(db, AdminTool::ActivityMonitor)
                .reason()
                .unwrap()
                .contains("libSQL")
        );
        assert!(
            admin_tool(db, AdminTool::IndexAnalyzer)
                .reason()
                .unwrap()
                .contains("libSQL")
        );
        assert!(
            admin_tool(db, AdminTool::BackupRestore)
                .reason()
                .unwrap()
                .contains("Turso")
        );
        assert!(admin_tool(db, AdminTool::DataTransfer).is_available());
        assert!(
            admin_tool(db, AdminTool::Security)
                .reason()
                .unwrap()
                .contains("libSQL")
        );
        assert!(
            admin_tool(db, AdminTool::QueryInsights)
                .reason()
                .unwrap()
                .contains("libSQL")
        );
        assert!(!admin_tool(db, AdminTool::Designers).is_available());
        assert!(transport_block(db).is_none());
        assert!(proxy_transport(db).is_available());
        assert!(cloud_iam_auth(db).reason().unwrap().contains("auth token"));
        assert!(compatibility_notes(db).is_none());
    }

    /// Every tool with an Oracle counterpart is on; only the SQL Server-only
    /// surfaces and server-side backups stay off, each naming Oracle.
    #[test]
    fn oracle_has_every_tool_with_a_counterpart() {
        let db = DatabaseType::Oracle;
        assert!(estimated_plan(db).is_available());
        assert!(actual_plan(db).is_available());
        assert!(row_editing(db).is_available());
        assert!(maintenance(db).is_available());
        assert!(database_ddl(db).reason().unwrap().contains("PLUGGABLE"));
        for tool in [
            AdminTool::ActivityMonitor,
            AdminTool::ServerDashboard,
            AdminTool::DiskUsage,
            AdminTool::ObjectBrowser,
            AdminTool::QueryInsights,
            AdminTool::IndexAnalyzer,
            AdminTool::SchemaCompare,
            AdminTool::SchemaHistory,
            AdminTool::DataCompare,
            AdminTool::DataTransfer,
            AdminTool::StructureEditor,
            AdminTool::TestDataGenerator,
            AdminTool::Security,
        ] {
            assert!(admin_tool(db, tool).is_available(), "{tool:?}");
        }
        for tool in [
            AdminTool::SqlAgent,
            AdminTool::Designers,
            AdminTool::ExtendedEvents,
        ] {
            assert!(admin_tool(db, tool).reason().is_some(), "{tool:?}");
        }
        assert!(
            admin_tool(db, AdminTool::BackupRestore)
                .reason()
                .unwrap()
                .contains("RMAN")
        );
        assert!(dependencies(db).is_available());
        assert!(transport_block(db).is_none());
        assert!(proxy_transport(db).is_available());
        assert!(cloud_iam_auth(db).reason().unwrap().contains("Oracle"));
        // First-party engine: no wire-compatibility caveat, but a setup note.
        assert!(compatibility_notes(db).is_none());
        let note = engine_notes(db).unwrap();
        assert!(note.contains("Instant Client"));
        assert!(note.contains("SID:"));
        assert!(note.contains("AS SYSDBA"));
        assert!(note.contains("tnsnames"));
        assert!(!note.contains("not available"));
        assert!(engine_notes(DatabaseType::Postgres).is_none());
    }

    #[test]
    fn redis_is_gated_as_a_command_engine_with_monitoring_only() {
        for db in [
            DatabaseType::Redis,
            DatabaseType::Valkey,
            DatabaseType::AmazonMemoryDb,
        ] {
            assert!(sql_editing(db).reason().unwrap().contains("command"));
            // Row editing stays off: the key view is the editor for a
            // command engine, and the reason points there.
            let row_reason = row_editing(db).reason().unwrap();
            assert!(row_reason.contains("Redis"), "{row_reason}");
            assert!(row_reason.contains("key view"), "{row_reason}");
            assert!(admin_tool(db, AdminTool::RedisKeyView).is_available());
            assert!(database_ddl(db).reason().unwrap().contains("numbered"));
            assert!(maintenance(db).reason().unwrap().contains("BGSAVE"));
            assert!(actual_plan(db).reason().is_some());
            assert!(estimated_plan(db).reason().is_some());
            assert!(admin_tool(db, AdminTool::ActivityMonitor).is_available());
            assert!(admin_tool(db, AdminTool::ServerDashboard).is_available());
            assert!(admin_tool(db, AdminTool::DiskUsage).is_available());
            assert!(admin_tool(db, AdminTool::QueryInsights).is_available());
            // Security opens the ACL page on servers that manage their own
            // ACL users (Redis, Valkey); AWS-managed MemoryDB says why not.
            if db == DatabaseType::AmazonMemoryDb {
                let reason = admin_tool(db, AdminTool::Security).reason().unwrap();
                assert!(reason.contains("Redis users"), "{reason}");
                assert!(reason.contains("AWS"), "{reason}");
            } else {
                assert!(admin_tool(db, AdminTool::Security).is_available(), "{db:?}");
            }
            for tool in [
                AdminTool::ObjectBrowser,
                AdminTool::SchemaCompare,
                AdminTool::SchemaHistory,
                AdminTool::DataCompare,
                AdminTool::DataTransfer,
                AdminTool::StructureEditor,
                AdminTool::TestDataGenerator,
                AdminTool::IndexAnalyzer,
                AdminTool::BackupRestore,
            ] {
                assert!(
                    admin_tool(db, tool).reason().unwrap().contains("Redis"),
                    "{db:?} {tool:?}"
                );
            }
            assert!(dependencies(db).reason().unwrap().contains("Redis"));
            assert!(transport_block(db).is_none());
            assert!(proxy_transport(db).is_available());
            assert!(cloud_iam_auth(db).is_available());
            assert!(!manual_transactions(db).is_available());
            assert!(!include_statistics(db).is_available());
        }
        assert!(compatibility_notes(DatabaseType::Redis).is_none());
        assert!(compatibility_notes(DatabaseType::Valkey).is_some());
        assert!(sql_editing(DatabaseType::Postgres).is_available());
        assert!(
            admin_tool(DatabaseType::Postgres, AdminTool::RedisKeyView)
                .reason()
                .unwrap()
                .contains("Redis")
        );
        assert_eq!(
            AdminTool::from_command_id("tools.redisKeyView"),
            Some(AdminTool::RedisKeyView)
        );
    }

    #[test]
    fn transports_available_for_network_engines() {
        assert!(transport_block(DatabaseType::SqlServer).is_none());
        assert!(transport_block(DatabaseType::Postgres).is_none());
        assert!(transport_block(DatabaseType::MySql).is_none());
        assert!(transport_block(DatabaseType::MariaDb).is_none());
    }

    #[test]
    fn transports_unavailable_for_sqlite_file_engine() {
        assert_eq!(
            transport_block(DatabaseType::Sqlite),
            Some(TransportBlock::SqliteFile)
        );
    }

    #[test]
    fn cloud_iam_auth_available_for_pg_and_mysql() {
        assert!(cloud_iam_auth(DatabaseType::Postgres).is_available());
        assert!(cloud_iam_auth(DatabaseType::MySql).is_available());
        assert!(cloud_iam_auth(DatabaseType::MariaDb).is_available());
    }

    #[test]
    fn cloud_iam_auth_available_for_redis() {
        // ElastiCache / MemoryDB IAM: SigV4 token as the AUTH password.
        assert!(cloud_iam_auth(DatabaseType::Redis).is_available());
        assert!(cloud_iam_auth(DatabaseType::Valkey).is_available());
    }

    #[test]
    fn cloud_iam_auth_unavailable_for_tds_and_sqlite() {
        let tds = cloud_iam_auth(DatabaseType::SqlServer);
        assert!(!tds.is_available());
        assert!(tds.reason().unwrap().contains("Microsoft Entra"));
        let sqlite = cloud_iam_auth(DatabaseType::Sqlite);
        assert!(!sqlite.is_available());
        assert!(sqlite.reason().unwrap().contains("SQLite"));
    }

    #[test]
    fn proxy_transport_available_for_network_engines() {
        assert!(proxy_transport(DatabaseType::SqlServer).is_available());
        assert!(proxy_transport(DatabaseType::Postgres).is_available());
        assert!(proxy_transport(DatabaseType::MySql).is_available());
    }

    #[test]
    fn proxy_transport_unavailable_for_sqlite() {
        let cap = proxy_transport(DatabaseType::Sqlite);
        assert!(!cap.is_available());
        assert!(cap.reason().unwrap().contains("SQLite opens a local file"));
    }

    #[test]
    fn security_available_for_postgres_and_mysql_families() {
        for db in [
            DatabaseType::Postgres,
            DatabaseType::MySql,
            DatabaseType::MariaDb,
            DatabaseType::Neon,
            DatabaseType::AmazonAuroraPostgres,
            DatabaseType::AzureMySqlFlexible,
        ] {
            assert!(admin_tool(db, AdminTool::Security).is_available(), "{db:?}");
        }
    }

    #[test]
    fn security_unavailable_for_cockroach_tidb_planetscale() {
        for db in [
            DatabaseType::CockroachDb,
            DatabaseType::TiDb,
            DatabaseType::PlanetScale,
            DatabaseType::YugabyteDb,
        ] {
            assert!(
                !admin_tool(db, AdminTool::Security).is_available(),
                "{db:?}"
            );
        }
    }
}
