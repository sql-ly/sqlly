//! Server health snapshot queries. Read-only catalog SQL; the client turns two
//! consecutive snapshots into per-second rates.
//!
//! Metrics are split into independent [`MetricGroup`]s rather than one large
//! UNION, because the catalogs they read are not equally available: SQL
//! Server's process-memory and file-size DMVs are absent on Azure SQL Database,
//! PostgreSQL's `pg_size_bytes` is missing on CockroachDB, MySQL's
//! `PROCESSLIST` needs the PROCESS privilege, and a managed engine can revoke
//! any of them. One UNION means one missing view blanks the whole dashboard; a
//! group means the tiles it feeds go quiet and every other tile still reports.
//!
//! Every key is optional by design — the pane draws a tile only for the keys a
//! snapshot actually contains, so an engine simply omitting a concept (SQLite
//! has no sessions, MySQL no replay lag) needs no special casing downstream.

use crate::admin::disk_usage::{DiskDialect, DiskUsage};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DashboardDialect {
    SqlServer,
    Postgres,
    MySql,
    MariaDb,
    /// A local database file: no sessions or throughput counters, but the file
    /// itself has a size, a free list, and contents worth reporting.
    Sqlite,
    /// DuckDB: like SQLite, an in-process file (or memory) database; its
    /// storage counters come from `pragma_database_size()`.
    DuckDb,
    /// ClickHouse: gauges from `system.metrics`, cumulative counters from
    /// `system.events`, sessions from `system.processes`, storage from
    /// `system.parts`, configuration from `system.server_settings`.
    ClickHouse,
    /// Oracle: sessions from `V$SESSION`, cumulative counters from
    /// `V$SYSSTAT`, memory from `V$SGA`; each `V$` group needs
    /// SELECT_CATALOG_ROLE and fails on its own without it.
    Oracle,
    /// Redis: every group is a slice of `INFO` the driver turns into the
    /// shared metric keys (`SQLLY.DASHBOARD <group>`).
    Redis,
}

/// One independently-executed metric query. A group that errors (permission,
/// missing catalog, unsupported edition) costs only its own keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetricGroup {
    /// Human name for the "unavailable" note when this group fails.
    pub name: &'static str,
    /// Two columns: `metric` (text key) and `value` (numeric as text).
    pub sql: &'static str,
}

const fn group(name: &'static str, sql: &'static str) -> MetricGroup {
    MetricGroup { name, sql }
}

/// Every counter key the dashboard understands. Absent keys simply produce no
/// tile, so this is a superset across engines.
pub const KEYS: &[&str] = &[
    // Sessions
    "connections_active",
    "connections_max",
    "sessions_running",
    "sessions_blocked",
    "sessions_idle_in_transaction",
    "longest_query_seconds",
    "oldest_transaction_seconds",
    // Throughput (cumulative counters — the client rates them)
    "transactions_total",
    "queries_total",
    "rows_returned_total",
    "slow_queries_total",
    // Health
    "deadlocks_total",
    "lock_waits_total",
    "temp_files_total",
    "replication_lag_seconds",
    // Resources
    "cache_hit_ratio",
    "page_life_expectancy",
    "memory_used_mb",
    "memory_configured_mb",
    "storage_total_mb",
    "reclaimable_mb",
    "objects_total",
    "indexes_total",
    "uptime_seconds",
];

// ---------------------------------------------------------------------------
// SQL Server — DMVs + performance counters (VIEW SERVER STATE)
// ---------------------------------------------------------------------------

const SQL_SERVER_GROUPS: &[MetricGroup] = &[
    group(
        "sessions",
        "SELECT 'connections_active' AS metric, CAST(COUNT(*) AS varchar(32)) AS value \
         FROM sys.dm_exec_connections \
         UNION ALL \
         SELECT 'sessions_running', CAST(COUNT(*) AS varchar(32)) \
         FROM sys.dm_exec_requests WHERE session_id > 50 AND status <> 'background' \
         UNION ALL \
         SELECT 'sessions_blocked', CAST(COUNT(*) AS varchar(32)) \
         FROM sys.dm_exec_requests WHERE blocking_session_id <> 0 \
         UNION ALL \
         SELECT 'longest_query_seconds', CAST(COALESCE(MAX(total_elapsed_time) / 1000.0, 0) AS varchar(32)) \
         FROM sys.dm_exec_requests WHERE session_id > 50 AND status <> 'background' \
         UNION ALL \
         SELECT 'oldest_transaction_seconds', CAST(COALESCE(MAX(DATEDIFF(second, t.transaction_begin_time, GETDATE())), 0) AS varchar(32)) \
         FROM sys.dm_tran_active_transactions t \
         JOIN sys.dm_tran_session_transactions s ON s.transaction_id = t.transaction_id",
    ),
    group(
        "throughput",
        "SELECT 'transactions_total' AS metric, CAST(cntr_value AS varchar(32)) AS value \
         FROM sys.dm_os_performance_counters \
         WHERE RTRIM(counter_name) = 'Transactions/sec' \
           AND object_name LIKE '%:Databases%' \
           AND RTRIM(instance_name) = '_Total' \
         UNION ALL \
         SELECT 'queries_total', CAST(cntr_value AS varchar(32)) \
         FROM sys.dm_os_performance_counters \
         WHERE RTRIM(counter_name) = 'Batch Requests/sec' \
           AND object_name LIKE '%:SQL Statistics%' \
         UNION ALL \
         SELECT 'deadlocks_total', CAST(cntr_value AS varchar(32)) \
         FROM sys.dm_os_performance_counters \
         WHERE RTRIM(counter_name) = 'Number of Deadlocks/sec' \
           AND RTRIM(instance_name) = '_Total' \
         UNION ALL \
         SELECT 'lock_waits_total', CAST(cntr_value AS varchar(32)) \
         FROM sys.dm_os_performance_counters \
         WHERE RTRIM(counter_name) = 'Lock Waits/sec' \
           AND RTRIM(instance_name) = '_Total' \
         UNION ALL \
         SELECT 'page_life_expectancy', CAST(cntr_value AS varchar(32)) \
         FROM sys.dm_os_performance_counters \
         WHERE RTRIM(counter_name) = 'Page life expectancy' \
           AND object_name LIKE '%:Buffer Manager%' \
         UNION ALL \
         SELECT 'cache_hit_ratio', CAST(100.0 * SUM(CASE WHEN RTRIM(counter_name) = 'Buffer cache hit ratio' THEN cntr_value ELSE 0 END) \
           / NULLIF(SUM(CASE WHEN RTRIM(counter_name) = 'Buffer cache hit ratio base' THEN cntr_value ELSE 0 END), 0) AS varchar(32)) \
         FROM sys.dm_os_performance_counters \
         WHERE object_name LIKE '%:Buffer Manager%' \
           AND RTRIM(counter_name) IN ('Buffer cache hit ratio', 'Buffer cache hit ratio base')",
    ),
    group(
        "configuration",
        "SELECT 'connections_max' AS metric, CAST(value_in_use AS varchar(32)) AS value \
         FROM sys.configurations WHERE name = 'user connections' \
         UNION ALL \
         SELECT 'uptime_seconds', CAST(DATEDIFF(second, sqlserver_start_time, GETDATE()) AS varchar(32)) \
         FROM sys.dm_os_sys_info",
    ),
    // `sys.dm_os_process_memory` is instance-level and absent on Azure SQL
    // Database; on its own so Azure loses this tile and nothing else.
    group(
        "memory",
        "SELECT 'memory_used_mb' AS metric, CAST(physical_memory_in_use_kb / 1024 AS varchar(32)) AS value \
         FROM sys.dm_os_process_memory",
    ),
    // `sys.master_files` is likewise instance-level.
    group(
        "storage",
        "SELECT 'storage_total_mb' AS metric, CAST(CAST(SUM(size) * 8.0 / 1024 AS decimal(18,1)) AS varchar(32)) AS value \
         FROM sys.master_files",
    ),
];

// ---------------------------------------------------------------------------
// PostgreSQL — pg_stat_* (pg_monitor widens what other sessions expose)
// ---------------------------------------------------------------------------

const POSTGRES_GROUPS: &[MetricGroup] = &[
    group(
        "sessions",
        "SELECT 'connections_active' AS metric, count(*)::text AS value \
         FROM pg_stat_activity WHERE pid <> pg_backend_pid() \
         UNION ALL \
         SELECT 'sessions_running', count(*)::text \
         FROM pg_stat_activity WHERE state = 'active' AND pid <> pg_backend_pid() \
         UNION ALL \
         SELECT 'sessions_idle_in_transaction', count(*)::text \
         FROM pg_stat_activity WHERE state = 'idle in transaction' \
         UNION ALL \
         SELECT 'sessions_blocked', count(*)::text \
         FROM pg_stat_activity WHERE wait_event_type = 'Lock' \
         UNION ALL \
         SELECT 'longest_query_seconds', COALESCE(EXTRACT(EPOCH FROM max(now() - query_start) FILTER (WHERE state = 'active')), 0)::bigint::text \
         FROM pg_stat_activity WHERE pid <> pg_backend_pid() \
         UNION ALL \
         SELECT 'oldest_transaction_seconds', COALESCE(EXTRACT(EPOCH FROM max(now() - xact_start)), 0)::bigint::text \
         FROM pg_stat_activity WHERE xact_start IS NOT NULL AND pid <> pg_backend_pid()",
    ),
    group(
        "throughput",
        "SELECT 'transactions_total' AS metric, COALESCE(sum(xact_commit + xact_rollback), 0)::text AS value FROM pg_stat_database \
         UNION ALL \
         SELECT 'rows_returned_total', COALESCE(sum(tup_returned + tup_fetched), 0)::text FROM pg_stat_database \
         UNION ALL \
         SELECT 'deadlocks_total', COALESCE(sum(deadlocks), 0)::text FROM pg_stat_database \
         UNION ALL \
         SELECT 'temp_files_total', COALESCE(sum(temp_files), 0)::text FROM pg_stat_database \
         UNION ALL \
         SELECT 'cache_hit_ratio', round(100.0 * sum(blks_hit) / NULLIF(sum(blks_hit) + sum(blks_read), 0), 2)::text FROM pg_stat_database",
    ),
    group(
        "configuration",
        "SELECT 'connections_max' AS metric, current_setting('max_connections') AS value \
         UNION ALL \
         SELECT 'uptime_seconds', EXTRACT(EPOCH FROM (now() - pg_postmaster_start_time()))::bigint::text",
    ),
    // `pg_size_bytes` is a PostgreSQL builtin that CockroachDB does not have.
    group(
        "memory",
        "SELECT 'memory_configured_mb' AS metric, (pg_size_bytes(current_setting('shared_buffers')) / 1048576)::bigint::text AS value",
    ),
    group(
        "replication",
        "SELECT 'replication_lag_seconds' AS metric, \
         CASE WHEN pg_is_in_recovery() THEN EXTRACT(EPOCH FROM (now() - pg_last_xact_replay_timestamp()))::bigint END::text AS value",
    ),
    // Databases the login cannot connect to are not measurable; skip them
    // rather than failing the group.
    group(
        "storage",
        "SELECT 'storage_total_mb' AS metric, ROUND(COALESCE(sum(pg_catalog.pg_database_size(datname)), 0) / 1048576.0, 1)::text AS value \
         FROM pg_catalog.pg_database WHERE pg_catalog.has_database_privilege(datname, 'CONNECT')",
    ),
];

// ---------------------------------------------------------------------------
// MySQL / MariaDB — the same metrics off different status tables
// ---------------------------------------------------------------------------

/// Status-counter metrics, parameterised by the table the server exposes them
/// in: MySQL retired `information_schema.GLOBAL_STATUS`, MariaDB never adopted
/// `performance_schema.global_status`.
macro_rules! mysql_status_group {
    ($status:literal) => {
        concat!(
            "SELECT 'connections_active' AS metric, MAX(CASE WHEN LOWER(VARIABLE_NAME) = 'threads_connected' THEN VARIABLE_VALUE END) AS value FROM ", $status,
            " UNION ALL SELECT 'sessions_running', MAX(CASE WHEN LOWER(VARIABLE_NAME) = 'threads_running' THEN VARIABLE_VALUE END) FROM ", $status,
            " UNION ALL SELECT 'sessions_blocked', MAX(CASE WHEN LOWER(VARIABLE_NAME) = 'innodb_row_lock_current_waits' THEN VARIABLE_VALUE END) FROM ", $status,
            " UNION ALL SELECT 'uptime_seconds', MAX(CASE WHEN LOWER(VARIABLE_NAME) = 'uptime' THEN VARIABLE_VALUE END) FROM ", $status,
            " UNION ALL SELECT 'queries_total', MAX(CASE WHEN LOWER(VARIABLE_NAME) = 'questions' THEN VARIABLE_VALUE END) FROM ", $status,
            " UNION ALL SELECT 'transactions_total', CAST(SUM(CASE WHEN LOWER(VARIABLE_NAME) IN ('com_commit', 'com_rollback') THEN CAST(VARIABLE_VALUE AS DECIMAL(20, 0)) ELSE 0 END) AS CHAR) FROM ", $status,
            " UNION ALL SELECT 'slow_queries_total', MAX(CASE WHEN LOWER(VARIABLE_NAME) = 'slow_queries' THEN VARIABLE_VALUE END) FROM ", $status,
            " UNION ALL SELECT 'deadlocks_total', MAX(CASE WHEN LOWER(VARIABLE_NAME) = 'innodb_deadlocks' THEN VARIABLE_VALUE END) FROM ", $status,
            " UNION ALL SELECT 'temp_files_total', MAX(CASE WHEN LOWER(VARIABLE_NAME) = 'created_tmp_disk_tables' THEN VARIABLE_VALUE END) FROM ", $status,
            " UNION ALL SELECT 'memory_used_mb', CAST(ROUND(MAX(CASE WHEN LOWER(VARIABLE_NAME) = 'innodb_buffer_pool_bytes_data' THEN CAST(VARIABLE_VALUE AS DECIMAL(30, 0)) END) / 1048576, 1) AS CHAR) FROM ", $status,
            // Clamped: on a cold buffer pool the server can report more disk
            // reads than read requests, which would put the ratio below zero
            // (a freshly started MariaDB reports -700 %).
            " UNION ALL SELECT 'cache_hit_ratio', CAST(ROUND(GREATEST(0, LEAST(100, 100.0 * (1.0 - ( \
                MAX(CASE WHEN LOWER(VARIABLE_NAME) = 'innodb_buffer_pool_reads' THEN CAST(VARIABLE_VALUE AS DECIMAL(20, 6)) END) \
                / NULLIF(MAX(CASE WHEN LOWER(VARIABLE_NAME) = 'innodb_buffer_pool_read_requests' THEN CAST(VARIABLE_VALUE AS DECIMAL(20, 6)) END), 0) \
            )))), 2) AS CHAR) FROM ", $status,
        )
    };
}

/// Longest running statement — needs the PROCESS privilege, so it is its own
/// group.
const MYSQL_SESSIONS_SQL: &str = "SELECT 'longest_query_seconds' AS metric, CAST(COALESCE(MAX(TIME), 0) AS CHAR) AS value \
     FROM information_schema.PROCESSLIST \
     WHERE COMMAND NOT IN ('Sleep', 'Daemon', 'Binlog Dump', 'Binlog Dump GTID')";

const MYSQL_STORAGE_SQL: &str = "SELECT 'storage_total_mb' AS metric, CAST(ROUND(COALESCE(SUM(DATA_LENGTH + INDEX_LENGTH), 0) / 1048576, 1) AS CHAR) AS value \
     FROM information_schema.TABLES";

const MYSQL_GROUPS: &[MetricGroup] = &[
    group(
        "status counters",
        mysql_status_group!("performance_schema.global_status"),
    ),
    group(
        "configuration",
        "SELECT 'connections_max' AS metric, VARIABLE_VALUE AS value \
         FROM performance_schema.global_variables WHERE LOWER(VARIABLE_NAME) = 'max_connections'",
    ),
    group("sessions", MYSQL_SESSIONS_SQL),
    group("storage", MYSQL_STORAGE_SQL),
];

const MARIADB_GROUPS: &[MetricGroup] = &[
    group(
        "status counters",
        mysql_status_group!("information_schema.GLOBAL_STATUS"),
    ),
    group(
        "configuration",
        "SELECT 'connections_max' AS metric, VARIABLE_VALUE AS value \
         FROM information_schema.GLOBAL_VARIABLES WHERE LOWER(VARIABLE_NAME) = 'max_connections'",
    ),
    group("sessions", MYSQL_SESSIONS_SQL),
    group("storage", MYSQL_STORAGE_SQL),
];

// ---------------------------------------------------------------------------
// SQLite — a file, not a service: what it costs on disk and what is in it
// ---------------------------------------------------------------------------

const SQLITE_GROUPS: &[MetricGroup] = &[group(
    "file",
    "SELECT 'storage_total_mb' AS metric, CAST(ROUND((SELECT page_count FROM pragma_page_count()) * (SELECT page_size FROM pragma_page_size()) / 1048576.0, 2) AS TEXT) AS value \
     UNION ALL \
     SELECT 'reclaimable_mb', CAST(ROUND((SELECT freelist_count FROM pragma_freelist_count()) * (SELECT page_size FROM pragma_page_size()) / 1048576.0, 2) AS TEXT) \
     UNION ALL \
     SELECT 'objects_total', CAST((SELECT COUNT(*) FROM sqlite_master WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite_%') AS TEXT) \
     UNION ALL \
     SELECT 'indexes_total', CAST((SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name NOT LIKE 'sqlite_%') AS TEXT)",
)];

const DUCKDB_GROUPS: &[MetricGroup] = &[group(
    "file",
    "SELECT 'storage_total_mb' AS metric, CAST(ROUND((SELECT COALESCE(SUM(used_blocks * block_size), 0) FROM pragma_database_size()) / 1048576.0, 2) AS VARCHAR) AS value \
     UNION ALL \
     SELECT 'reclaimable_mb', CAST(ROUND((SELECT COALESCE(SUM(free_blocks * block_size), 0) FROM pragma_database_size()) / 1048576.0, 2) AS VARCHAR) \
     UNION ALL \
     SELECT 'objects_total', CAST((SELECT COUNT(*) FROM duckdb_tables() WHERE NOT internal) + (SELECT COUNT(*) FROM duckdb_views() WHERE NOT internal) AS VARCHAR) \
     UNION ALL \
     SELECT 'indexes_total', CAST((SELECT COUNT(*) FROM duckdb_indexes()) AS VARCHAR)",
)];

// ---------------------------------------------------------------------------
// ClickHouse — system.metrics / system.events / system.processes
// ---------------------------------------------------------------------------

const CLICKHOUSE_GROUPS: &[MetricGroup] = &[
    group(
        "metrics",
        "SELECT 'connections_active' AS metric, toString(sum(value)) AS value FROM system.metrics WHERE metric IN ('TCPConnection', 'HTTPConnection', 'MySQLConnection', 'PostgreSQLConnection', 'InterserverConnection') \
         UNION ALL \
         SELECT 'memory_used_mb', toString(round(sum(value) / 1048576, 1)) FROM system.metrics WHERE metric = 'MemoryTracking'",
    ),
    group(
        "events",
        "SELECT 'queries_total' AS metric, toString(sum(value)) AS value FROM system.events WHERE event = 'Query' \
         UNION ALL \
         SELECT 'rows_returned_total', toString(sum(value)) FROM system.events WHERE event = 'SelectedRows' \
         UNION ALL \
         SELECT 'cache_hit_ratio', toString(round(100 * sumIf(value, event = 'MarkCacheHits') / greatest(1, sumIf(value, event IN ('MarkCacheHits', 'MarkCacheMisses'))), 1)) FROM system.events WHERE event IN ('MarkCacheHits', 'MarkCacheMisses')",
    ),
    group(
        "sessions",
        "SELECT 'sessions_running' AS metric, toString(count()) AS value FROM system.processes WHERE query NOT LIKE '%system.processes%' \
         UNION ALL \
         SELECT 'longest_query_seconds', toString(toInt64(max(elapsed))) FROM system.processes WHERE query NOT LIKE '%system.processes%'",
    ),
    group(
        "configuration",
        "SELECT 'connections_max' AS metric, toString(value) AS value FROM system.server_settings WHERE name = 'max_connections' \
         UNION ALL \
         SELECT 'uptime_seconds', toString(toInt64(value)) FROM system.asynchronous_metrics WHERE metric = 'Uptime' \
         UNION ALL \
         SELECT 'memory_configured_mb', toString(round(value / 1048576, 1)) FROM system.asynchronous_metrics WHERE metric = 'OSMemoryTotal'",
    ),
    group(
        "storage",
        "SELECT 'storage_total_mb' AS metric, toString(round(sum(bytes_on_disk) / 1048576, 2)) AS value FROM system.parts WHERE active \
         UNION ALL \
         SELECT 'objects_total', toString(count()) FROM system.tables WHERE database NOT IN ('system', 'INFORMATION_SCHEMA', 'information_schema') \
         UNION ALL \
         SELECT 'indexes_total', toString(count()) FROM system.data_skipping_indices",
    ),
];

// ---------------------------------------------------------------------------
// Oracle — V$ views need SELECT_CATALOG_ROLE; ALL_* views are always readable
// ---------------------------------------------------------------------------

const ORACLE_GROUPS: &[MetricGroup] = &[
    group(
        "sessions",
        "SELECT 'connections_active' AS metric, TO_CHAR(COUNT(*)) AS value \
         FROM v$session WHERE type = 'USER' \
         UNION ALL \
         SELECT 'sessions_running', TO_CHAR(COUNT(*)) \
         FROM v$session WHERE type = 'USER' AND status = 'ACTIVE' AND sid <> SYS_CONTEXT('USERENV', 'SID') \
         UNION ALL \
         SELECT 'sessions_blocked', TO_CHAR(COUNT(*)) \
         FROM v$session WHERE blocking_session IS NOT NULL \
         UNION ALL \
         SELECT 'longest_query_seconds', TO_CHAR(NVL(MAX(last_call_et), 0)) \
         FROM v$session WHERE type = 'USER' AND status = 'ACTIVE' AND sid <> SYS_CONTEXT('USERENV', 'SID') \
         UNION ALL \
         SELECT 'oldest_transaction_seconds', TO_CHAR(NVL(MAX(ROUND((SYSDATE - TO_DATE(t.start_time, 'MM/DD/YY HH24:MI:SS')) * 86400)), 0)) \
         FROM v$transaction t",
    ),
    group(
        "throughput",
        "SELECT 'transactions_total' AS metric, TO_CHAR(SUM(value)) AS value \
         FROM v$sysstat WHERE name IN ('user commits', 'user rollbacks') \
         UNION ALL \
         SELECT 'queries_total', TO_CHAR(value) FROM v$sysstat WHERE name = 'execute count' \
         UNION ALL \
         SELECT 'deadlocks_total', TO_CHAR(value) FROM v$sysstat WHERE name = 'enqueue deadlocks' \
         UNION ALL \
         SELECT 'lock_waits_total', TO_CHAR(value) FROM v$sysstat WHERE name = 'enqueue waits' \
         UNION ALL \
         SELECT 'cache_hit_ratio', TO_CHAR(ROUND(100 * (1 - (SUM(CASE WHEN name = 'physical reads' THEN value ELSE 0 END) / NULLIF(SUM(CASE WHEN name IN ('db block gets', 'consistent gets') THEN value ELSE 0 END), 0))), 2)) \
         FROM v$sysstat WHERE name IN ('physical reads', 'db block gets', 'consistent gets')",
    ),
    group(
        "memory",
        "SELECT 'memory_used_mb' AS metric, TO_CHAR(ROUND(SUM(value) / 1048576)) AS value FROM v$sga",
    ),
    group(
        "configuration",
        "SELECT 'connections_max' AS metric, value AS value FROM v$parameter WHERE name = 'sessions' \
         UNION ALL \
         SELECT 'uptime_seconds', TO_CHAR(ROUND((SYSDATE - startup_time) * 86400)) FROM v$instance",
    ),
    // DBA_SEGMENTS is DBA-only; on its own so ordinary logins lose one tile.
    group(
        "storage",
        "SELECT 'storage_total_mb' AS metric, TO_CHAR(ROUND(SUM(bytes) / 1048576, 1)) AS value FROM dba_segments",
    ),
    // ALL_* views are readable by every login.
    group(
        "objects",
        "SELECT 'objects_total' AS metric, TO_CHAR(COUNT(*)) AS value \
         FROM all_objects WHERE object_type IN ('TABLE', 'VIEW') AND owner IN (SELECT username FROM all_users WHERE oracle_maintained = 'N' OR username = USER) \
         UNION ALL \
         SELECT 'indexes_total', TO_CHAR(COUNT(*)) \
         FROM all_indexes WHERE owner IN (SELECT username FROM all_users WHERE oracle_maintained = 'N' OR username = USER)",
    ),
];

// ---------------------------------------------------------------------------
// Redis — INFO sections through the driver's SQLLY.DASHBOARD virtual command
// ---------------------------------------------------------------------------

const REDIS_GROUPS: &[MetricGroup] = &[
    group("server", "SQLLY.DASHBOARD server"),
    group("clients", "SQLLY.DASHBOARD clients"),
    group("memory", "SQLLY.DASHBOARD memory"),
    group("stats", "SQLLY.DASHBOARD stats"),
    group("replication", "SQLLY.DASHBOARD replication"),
    group("keyspace", "SQLLY.DASHBOARD keyspace"),
];

pub struct Dashboard;

impl Dashboard {
    /// The independently-executed metric groups for an engine. Run them all;
    /// merge what succeeds; report the names of what did not.
    pub fn metric_groups_for(dialect: DashboardDialect) -> &'static [MetricGroup] {
        match dialect {
            DashboardDialect::SqlServer => SQL_SERVER_GROUPS,
            DashboardDialect::Postgres => POSTGRES_GROUPS,
            DashboardDialect::MySql => MYSQL_GROUPS,
            DashboardDialect::MariaDb => MARIADB_GROUPS,
            DashboardDialect::Sqlite => SQLITE_GROUPS,
            DashboardDialect::DuckDb => DUCKDB_GROUPS,
            DashboardDialect::ClickHouse => CLICKHOUSE_GROUPS,
            DashboardDialect::Oracle => ORACLE_GROUPS,
            DashboardDialect::Redis => REDIS_GROUPS,
        }
    }

    /// Per-database sizes: columns follow [`DiskUsage::databases_query_for`].
    pub fn database_sizes_query_for(dialect: DashboardDialect) -> &'static str {
        DiskUsage::databases_query_for(match dialect {
            DashboardDialect::SqlServer => DiskDialect::SqlServer,
            DashboardDialect::Postgres => DiskDialect::Postgres,
            DashboardDialect::MySql | DashboardDialect::MariaDb => DiskDialect::MySql,
            DashboardDialect::Sqlite => DiskDialect::Sqlite,
            DashboardDialect::DuckDb => DiskDialect::DuckDb,
            DashboardDialect::ClickHouse => DiskDialect::ClickHouse,
            DashboardDialect::Oracle => DiskDialect::Oracle,
            DashboardDialect::Redis => DiskDialect::Redis,
        })
    }

    /// Every query the dashboard issues, metric groups first.
    pub fn all_queries_for(dialect: DashboardDialect) -> Vec<&'static str> {
        Self::metric_groups_for(dialect)
            .iter()
            .map(|g| g.sql)
            .chain(std::iter::once(Self::database_sizes_query_for(dialect)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIALECTS: [DashboardDialect; 8] = [
        DashboardDialect::SqlServer,
        DashboardDialect::Postgres,
        DashboardDialect::MySql,
        DashboardDialect::MariaDb,
        DashboardDialect::Sqlite,
        DashboardDialect::DuckDb,
        DashboardDialect::ClickHouse,
        DashboardDialect::Oracle,
    ];

    #[test]
    fn every_dialect_has_metric_groups_and_a_sizes_query() {
        for dialect in DIALECTS {
            let groups = Dashboard::metric_groups_for(dialect);
            assert!(!groups.is_empty(), "{dialect:?} has no metric groups");
            for sql in Dashboard::all_queries_for(dialect) {
                assert!(
                    sql.to_ascii_uppercase().contains("SELECT"),
                    "{dialect:?} query is not a SELECT: {sql}"
                );
            }
        }
        assert!(!KEYS.is_empty());
    }

    /// Every group selects the two columns the snapshot parser reads, and
    /// every key it emits is one the tile table knows about — a typo would
    /// otherwise vanish silently into "no tile".
    #[test]
    fn groups_emit_known_keys_in_the_metric_value_shape() {
        for dialect in DIALECTS {
            for group in Dashboard::metric_groups_for(dialect) {
                assert!(
                    group.sql.contains("AS metric") && group.sql.contains("AS value"),
                    "{dialect:?}/{} is not a metric,value query",
                    group.name
                );
                for fragment in group.sql.split("SELECT '").skip(1) {
                    let key = fragment.split('\'').next().unwrap_or_default();
                    assert!(
                        KEYS.contains(&key),
                        "{dialect:?}/{} emits unknown key {key}",
                        group.name
                    );
                }
            }
        }
    }

    /// Redis groups are driver virtual commands rather than SELECTs; each
    /// names an INFO section the driver knows, and the sizes query is the
    /// per-database keyspace listing.
    #[test]
    fn redis_groups_are_dashboard_virtual_commands() {
        let groups = Dashboard::metric_groups_for(DashboardDialect::Redis);
        assert_eq!(groups.len(), 6);
        for group in groups {
            assert_eq!(group.sql, format!("SQLLY.DASHBOARD {}", group.name));
        }
        assert!(
            Dashboard::database_sizes_query_for(DashboardDialect::Redis)
                .starts_with("SQLLY.DATABASES")
        );
    }

    /// Splitting by catalog is the whole point: a group must not mix a widely
    /// available view with one a managed engine withholds, or the available
    /// metrics go down with the unavailable one.
    #[test]
    fn instance_only_catalogs_are_isolated_in_their_own_groups() {
        let sql_server = Dashboard::metric_groups_for(DashboardDialect::SqlServer);
        let memory = sql_server.iter().find(|g| g.name == "memory").unwrap();
        assert!(memory.sql.contains("dm_os_process_memory"));
        assert_eq!(
            memory.sql.matches("SELECT '").count(),
            1,
            "Azure SQL Database must lose only the memory tile"
        );
        let storage = sql_server.iter().find(|g| g.name == "storage").unwrap();
        assert!(storage.sql.contains("sys.master_files"));

        let postgres = Dashboard::metric_groups_for(DashboardDialect::Postgres);
        let memory = postgres.iter().find(|g| g.name == "memory").unwrap();
        assert!(
            memory.sql.contains("pg_size_bytes"),
            "CockroachDB lacks pg_size_bytes and must lose only this group"
        );
    }

    #[test]
    fn dialect_queries_target_native_catalogs() {
        let pg: String = Dashboard::all_queries_for(DashboardDialect::Postgres).join(" ");
        assert!(pg.contains("pg_stat_database"));
        assert!(pg.contains("pg_stat_activity"));
        let mysql: String = Dashboard::all_queries_for(DashboardDialect::MySql).join(" ");
        assert!(mysql.contains("performance_schema.global_status"));
        let maria: String = Dashboard::all_queries_for(DashboardDialect::MariaDb).join(" ");
        assert!(maria.contains("information_schema.GLOBAL_STATUS"));
        let sql_server: String = Dashboard::all_queries_for(DashboardDialect::SqlServer).join(" ");
        assert!(sql_server.contains("dm_os_performance_counters"));
        assert!(sql_server.contains("dm_exec_requests"));
        let sqlite: String = Dashboard::all_queries_for(DashboardDialect::Sqlite).join(" ");
        assert!(sqlite.contains("pragma_page_count"));
        assert!(sqlite.contains("pragma_freelist_count"));
        let clickhouse: String = Dashboard::all_queries_for(DashboardDialect::ClickHouse).join(" ");
        assert!(clickhouse.contains("system.metrics"));
        assert!(clickhouse.contains("system.events"));
        assert!(clickhouse.contains("system.processes"));
        assert!(clickhouse.contains("system.parts"));
        let oracle: String = Dashboard::all_queries_for(DashboardDialect::Oracle).join(" ");
        assert!(oracle.contains("v$sysstat"));
        assert!(oracle.contains("v$session"));
        assert!(oracle.contains("user_segments"));
        // DBA-only storage lives in its own group so ordinary logins lose one
        // tile, not the dashboard.
        let storage = Dashboard::metric_groups_for(DashboardDialect::Oracle)
            .iter()
            .find(|g| g.name == "storage")
            .unwrap();
        assert!(storage.sql.contains("dba_segments"));
        assert_eq!(storage.sql.matches("SELECT '").count(), 1);
    }

    #[test]
    fn queries_are_read_only() {
        for dialect in DIALECTS {
            let sql = Dashboard::all_queries_for(dialect)
                .join(" ")
                .to_ascii_uppercase();
            for verb in ["INSERT", "UPDATE ", "DELETE", "KILL", "ALTER", "SET "] {
                assert!(!sql.contains(verb), "{dialect:?} query contains {verb}");
            }
        }
    }
}
