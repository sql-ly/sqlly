#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableTarget {
    pub schema: String,
    pub table: String,
}

impl TableTarget {
    pub fn new(schema: impl Into<String>, table: impl Into<String>) -> Self {
        Self {
            schema: schema.into(),
            table: table.into(),
        }
    }

    fn qualified(&self) -> String {
        format!("{}.{}", quote_name(&self.schema), quote_name(&self.table))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CsvImportPlan {
    pub target: TableTarget,
    pub file_path: String,
    pub has_header: bool,
    pub delimiter: char,
    pub batch_size: usize,
}

impl CsvImportPlan {
    pub fn bulk_insert_sql(&self) -> String {
        let first_row = if self.has_header { 2 } else { 1 };
        format!(
            "BULK INSERT {} FROM N'{}' WITH (FORMAT = 'CSV', FIRSTROW = {first_row}, FIELDTERMINATOR = '{}', ROWTERMINATOR = '0x0a', BATCHSIZE = {});",
            self.target.qualified(),
            literal(&self.file_path),
            escape_delimiter(self.delimiter),
            self.batch_size.max(1)
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CsvExportPlan {
    pub source_sql: String,
    pub file_path: String,
    pub include_header: bool,
    pub delimiter: char,
}

impl CsvExportPlan {
    pub fn command_line_hint(&self) -> String {
        format!(
            "bcp \"{}\" queryout \"{}\" -c -t\"{}\"{}",
            self.source_sql.replace('"', "\"\""),
            self.file_path.replace('"', "\"\""),
            escape_delimiter(self.delimiter),
            if self.include_header { " --header" } else { "" }
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaObject {
    pub schema: String,
    pub name: String,
    pub kind: String,
    pub definition: String,
}

impl SchemaObject {
    fn key(&self) -> String {
        format!(
            "{}.{}.{}",
            self.kind.to_lowercase(),
            self.schema.to_lowercase(),
            self.name.to_lowercase()
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaDifferenceKind {
    MissingInTarget,
    MissingInSource,
    DefinitionChanged,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaDifference {
    pub kind: SchemaDifferenceKind,
    pub object: SchemaObject,
    pub target_definition: Option<String>,
}

pub fn compare_schema(source: &[SchemaObject], target: &[SchemaObject]) -> Vec<SchemaDifference> {
    let mut differences = Vec::new();
    for source_object in source {
        match target
            .iter()
            .find(|target_object| target_object.key() == source_object.key())
        {
            Some(target_object) if target_object.definition != source_object.definition => {
                differences.push(SchemaDifference {
                    kind: SchemaDifferenceKind::DefinitionChanged,
                    object: source_object.clone(),
                    target_definition: Some(target_object.definition.clone()),
                });
            }
            Some(_) => {}
            None => differences.push(SchemaDifference {
                kind: SchemaDifferenceKind::MissingInTarget,
                object: source_object.clone(),
                target_definition: None,
            }),
        }
    }
    for target_object in target {
        if !source
            .iter()
            .any(|source_object| source_object.key() == target_object.key())
        {
            differences.push(SchemaDifference {
                kind: SchemaDifferenceKind::MissingInSource,
                object: target_object.clone(),
                target_definition: None,
            });
        }
    }
    differences
}

pub fn script_diff(differences: &[SchemaDifference]) -> String {
    differences
        .iter()
        .map(|difference| match difference.kind {
            SchemaDifferenceKind::MissingInTarget => format!(
                "-- Missing in target: {}.{}\n{}",
                difference.object.schema, difference.object.name, difference.object.definition
            ),
            SchemaDifferenceKind::MissingInSource => format!(
                "-- Extra in target: {}.{}\n-- Review before dropping this object.",
                difference.object.schema, difference.object.name
            ),
            SchemaDifferenceKind::DefinitionChanged => format!(
                "-- Definition changed: {}.{}\n-- Source definition:\n{}\n-- Target definition:\n{}",
                difference.object.schema,
                difference.object.name,
                difference.object.definition,
                difference.target_definition.as_deref().unwrap_or_default()
            ),
        })
        .collect::<Vec<_>>()
        .join("\nGO\n")
}

pub struct QueryStore;

impl QueryStore {
    pub fn top_queries_query(database: &str, limit: u32) -> String {
        Self::top_queries_query_ranked(database, limit, QueryStoreRank::AvgDuration, None)
    }

    /// Top queries ranked by the chosen metric, optionally restricted to
    /// runtime-stats intervals from the last `hours` hours. The default rank
    /// (average duration, no window) preserves the original ranking while
    /// normalizing SQL Server's microsecond duration into milliseconds.
    pub fn top_queries_query_ranked(
        database: &str,
        limit: u32,
        rank: QueryStoreRank,
        hours: Option<u32>,
    ) -> String {
        let window = match hours {
            Some(h) => format!(
                " WHERE rs.last_execution_time >= DATEADD(HOUR, -{}, SYSUTCDATETIME())",
                h.max(1)
            ),
            None => String::new(),
        };
        format!(
            "USE {}; SELECT TOP ({}) qsq.query_id, qst.query_sql_text, CAST(rs.avg_duration / 1000.0 AS decimal(19, 3)) AS avg_duration_ms, rs.count_executions, rs.avg_logical_io_reads AS avg_logical_reads, rs.last_execution_time FROM sys.query_store_query qsq JOIN sys.query_store_query_text qst ON qsq.query_text_id = qst.query_text_id JOIN sys.query_store_plan qsp ON qsq.query_id = qsp.query_id JOIN sys.query_store_runtime_stats rs ON qsp.plan_id = rs.plan_id{} ORDER BY {} DESC",
            quote_name(database),
            limit.max(1),
            window,
            rank.order_column()
        )
    }

    /// The PostgreSQL counterpart to the Query Store top-queries view, backed
    /// by the `pg_stat_statements` extension (its absence surfaces as the
    /// server's own "relation does not exist" hint naming the extension).
    /// pg_stat_statements is cumulative, so there is no time window; the rank
    /// metrics map onto its columns.
    pub fn pg_stat_statements_top_query(limit: u32, rank: QueryStoreRank) -> String {
        let order = match rank {
            QueryStoreRank::AvgDuration => "mean_exec_time",
            QueryStoreRank::ExecutionCount => "calls",
            QueryStoreRank::AvgLogicalReads => {
                "(shared_blks_hit + shared_blks_read) / GREATEST(calls, 1)"
            }
        };
        format!(
            "SELECT queryid AS query_id, query AS query_sql_text, round(mean_exec_time)::bigint AS avg_duration_ms, calls AS count_executions, (shared_blks_hit + shared_blks_read) / GREATEST(calls, 1) AS avg_logical_reads, rows AS total_rows FROM pg_stat_statements ORDER BY {order} DESC LIMIT {}",
            limit.max(1)
        )
    }

    /// The MySQL counterpart, from the performance-schema statement digests
    /// (timers are picoseconds; rendered as milliseconds).
    pub fn mysql_statement_digest_top_query(limit: u32, rank: QueryStoreRank) -> String {
        let order = match rank {
            QueryStoreRank::AvgDuration => "avg_timer_wait",
            QueryStoreRank::ExecutionCount => "count_star",
            QueryStoreRank::AvgLogicalReads => "sum_rows_examined / GREATEST(count_star, 1)",
        };
        format!(
            "SELECT digest AS query_id, digest_text AS query_sql_text, ROUND(avg_timer_wait / 1000000000) AS avg_duration_ms, count_star AS count_executions, sum_rows_examined / GREATEST(count_star, 1) AS avg_rows_examined, last_seen FROM performance_schema.events_statements_summary_by_digest WHERE digest_text IS NOT NULL ORDER BY {order} DESC LIMIT {}",
            limit.max(1)
        )
    }

    /// ClickHouse `system.query_log`: finished queries grouped by their
    /// normalized text (`normalized_query_hash`), ranked by average duration,
    /// execution count, or average rows read. The log is written by the
    /// server asynchronously (`SYSTEM FLUSH LOGS` forces it), so `hours`
    /// bounds the window on `event_time`. The Insights query's own reads of
    /// `system.query_log` are excluded so the tool does not top its own list.
    pub fn clickhouse_query_log_top_query(
        limit: u32,
        rank: QueryStoreRank,
        hours: Option<u32>,
    ) -> String {
        let order = match rank {
            QueryStoreRank::AvgDuration => "avg_duration_ms",
            QueryStoreRank::ExecutionCount => "count_executions",
            QueryStoreRank::AvgLogicalReads => "avg_read_rows",
        };
        let window = hours
            .filter(|h| *h > 0)
            .map(|h| format!(" AND event_time >= now() - INTERVAL {h} HOUR"))
            .unwrap_or_default();
        format!(
            "SELECT toString(normalized_query_hash) AS query_id, any(normalizeQuery(query)) AS query_sql_text, toInt64(round(avg(query_duration_ms))) AS avg_duration_ms, count() AS count_executions, toInt64(round(avg(read_rows))) AS avg_read_rows, toInt64(round(avg(read_bytes) / 1048576)) AS avg_read_mb, toInt64(round(max(memory_usage) / 1048576)) AS peak_memory_mb, sum(result_rows) AS total_rows, max(event_time) AS last_seen FROM system.query_log WHERE type = 'QueryFinish' AND is_initial_query AND query NOT LIKE '%system.query_log%'{window} GROUP BY normalized_query_hash ORDER BY {order} DESC LIMIT {}",
            limit.max(1)
        )
    }

    /// The Oracle counterpart, from the shared pool's per-statement
    /// statistics (`V$SQLAREA`: one row per SQL_ID, cumulative since the
    /// cursor was loaded; timers are microseconds, rendered as
    /// milliseconds). Needs SELECT on `V_$SQLAREA` (or SELECT_CATALOG_ROLE).
    /// The marker comment keeps this query out of its own results.
    pub fn oracle_top_query(limit: u32, rank: QueryStoreRank) -> String {
        let order = match rank {
            QueryStoreRank::AvgDuration => "s.elapsed_time / GREATEST(s.executions, 1)",
            QueryStoreRank::ExecutionCount => "s.executions",
            QueryStoreRank::AvgLogicalReads => "s.buffer_gets / GREATEST(s.executions, 1)",
        };
        format!(
            "SELECT /* sqlly:insights */ s.sql_id AS query_id, s.sql_fulltext AS query_sql_text, ROUND(s.elapsed_time / GREATEST(s.executions, 1) / 1000) AS avg_duration_ms, s.executions AS count_executions, ROUND(s.buffer_gets / GREATEST(s.executions, 1)) AS avg_logical_reads, ROUND(s.cpu_time / GREATEST(s.executions, 1) / 1000) AS avg_cpu_ms, s.disk_reads AS total_disk_reads, s.rows_processed AS total_rows, s.parsing_schema_name, TO_CHAR(s.last_active_time, 'YYYY-MM-DD HH24:MI:SS') AS last_active FROM v$sqlarea s WHERE s.sql_text NOT LIKE '%sqlly:insights%' AND s.executions > 0 ORDER BY {order} DESC FETCH FIRST {} ROWS ONLY",
            limit.max(1)
        )
    }

    /// ClickHouse `system.query_log`: the most recent failed queries with
    /// the server's exception text, newest first.
    pub fn clickhouse_query_log_failures_query(limit: u32, hours: Option<u32>) -> String {
        let window = hours
            .filter(|h| *h > 0)
            .map(|h| format!(" AND event_time >= now() - INTERVAL {h} HOUR"))
            .unwrap_or_default();
        format!(
            "SELECT event_time AS failed_at, query AS query_sql_text, exception_code, exception, user, query_duration_ms FROM system.query_log WHERE type IN ('ExceptionBeforeStart', 'ExceptionWhileProcessing') AND is_initial_query AND query NOT LIKE '%system.query_log%'{window} ORDER BY event_time DESC LIMIT {}",
            limit.max(1)
        )
    }

    /// The cached plan for one Oracle SQL_ID, one line of `DBMS_XPLAN`
    /// output per row (every child cursor, with predicates).
    pub fn oracle_plan_query(sql_id: &str) -> String {
        format!(
            "SELECT plan_table_output FROM TABLE(DBMS_XPLAN.DISPLAY_CURSOR('{}', NULL, 'TYPICAL'))",
            sql_id.replace('\'', "''")
        )
    }

    pub fn plan_history_query(database: &str, query_id: i64) -> String {
        format!(
            "USE {}; SELECT plan_id, query_plan, is_forced_plan, force_failure_count, last_execution_time FROM sys.query_store_plan WHERE query_id = {} ORDER BY last_execution_time DESC",
            quote_name(database),
            query_id
        )
    }
}

/// Ranking metrics for the Query Store top-queries view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryStoreRank {
    AvgDuration,
    ExecutionCount,
    AvgLogicalReads,
}

impl QueryStoreRank {
    pub fn order_column(self) -> &'static str {
        match self {
            QueryStoreRank::AvgDuration => "rs.avg_duration",
            QueryStoreRank::ExecutionCount => "rs.count_executions",
            QueryStoreRank::AvgLogicalReads => "rs.avg_logical_io_reads",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            QueryStoreRank::AvgDuration => "Avg Duration",
            QueryStoreRank::ExecutionCount => "Executions",
            QueryStoreRank::AvgLogicalReads => "Avg Reads",
        }
    }

    pub fn message(self) -> sqlly_core::DisplayMessage {
        let (key, fallback) = match self {
            Self::AvgDuration => (
                "services.admin.query_store.rank_avg_duration",
                "Avg Duration",
            ),
            Self::ExecutionCount => ("services.admin.query_store.rank_executions", "Executions"),
            Self::AvgLogicalReads => ("services.admin.query_store.rank_avg_reads", "Avg Reads"),
        };
        sqlly_core::DisplayMessage::new(key, fallback)
    }
}

pub struct ExtendedEvents;

impl ExtendedEvents {
    pub fn sessions_query() -> String {
        "SELECT name, event_session_id, startup_state FROM sys.server_event_sessions ORDER BY name"
            .to_string()
    }

    pub fn session_events_query(session_name: &str) -> String {
        format!(
            "SELECT s.name AS session_name, e.name AS event_name, t.target_name FROM sys.server_event_sessions s LEFT JOIN sys.server_event_session_events e ON s.event_session_id = e.event_session_id LEFT JOIN sys.server_event_session_targets t ON s.event_session_id = t.event_session_id WHERE s.name = N'{}' ORDER BY e.name",
            literal(session_name)
        )
    }

    pub fn live_ring_buffer_query(session_name: &str) -> String {
        format!(
            "SELECT CAST(t.target_data AS xml) AS target_data FROM sys.dm_xe_session_targets t JOIN sys.dm_xe_sessions s ON t.event_session_address = s.address WHERE s.name = N'{}' AND t.target_name = N'ring_buffer'",
            literal(session_name)
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StandardReport {
    DiskUsage,
    BackupHistory,
    BlockingSessions,
}

pub fn standard_report_query(report: StandardReport) -> &'static str {
    match report {
        StandardReport::DiskUsage => {
            "SELECT name, size * 8 / 1024 AS size_mb, max_size FROM sys.database_files ORDER BY name"
        }
        StandardReport::BackupHistory => {
            "SELECT TOP (50) database_name, backup_start_date, backup_finish_date, type, backup_size FROM msdb.dbo.backupset ORDER BY backup_finish_date DESC"
        }
        StandardReport::BlockingSessions => {
            "SELECT session_id, blocking_session_id, wait_type, wait_time FROM sys.dm_exec_requests WHERE blocking_session_id <> 0"
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataTierArtifactKind {
    Dacpac,
    Bacpac,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataTierArtifactPlan {
    pub kind: DataTierArtifactKind,
    pub database: String,
    pub path: String,
}

impl DataTierArtifactPlan {
    pub fn sqlpackage_hint(&self) -> String {
        let action = match self.kind {
            DataTierArtifactKind::Dacpac => "Extract",
            DataTierArtifactKind::Bacpac => "Export",
        };
        format!(
            "sqlpackage /Action:{} /SourceDatabaseName:\"{}\" /TargetFile:\"{}\"",
            action,
            self.database.replace('"', "\"\""),
            self.path.replace('"', "\"\""),
        )
    }
}

fn quote_name(name: &str) -> String {
    format!("[{}]", name.replace(']', "]]"))
}

fn literal(value: &str) -> String {
    value.replace('\'', "''")
}

fn escape_delimiter(delimiter: char) -> String {
    match delimiter {
        '\t' => "\\t".to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_csv_import_and_export_plans() {
        let import = CsvImportPlan {
            target: TableTarget::new("dbo", "Orders"),
            file_path: "/tmp/orders'2026.csv".to_string(),
            has_header: true,
            delimiter: ',',
            batch_size: 0,
        };
        let sql = import.bulk_insert_sql();
        assert!(sql.contains("BULK INSERT [dbo].[Orders]"));
        assert!(sql.contains("FIRSTROW = 2"));
        assert!(sql.contains("orders''2026.csv"));
        assert!(sql.contains("BATCHSIZE = 1"));

        let export = CsvExportPlan {
            source_sql: "SELECT * FROM dbo.Orders".to_string(),
            file_path: "/tmp/orders.csv".to_string(),
            include_header: true,
            delimiter: '\t',
        };
        assert!(export.command_line_hint().contains("-t\"\\t\" --header"));
    }

    #[test]
    fn compares_schema_and_scripts_diff() {
        let source = vec![SchemaObject {
            schema: "dbo".to_string(),
            name: "Orders".to_string(),
            kind: "TABLE".to_string(),
            definition: "CREATE TABLE dbo.Orders(id int)".to_string(),
        }];
        let target = vec![SchemaObject {
            schema: "dbo".to_string(),
            name: "Orders".to_string(),
            kind: "TABLE".to_string(),
            definition: "CREATE TABLE dbo.Orders(id bigint)".to_string(),
        }];
        let differences = compare_schema(&source, &target);
        assert_eq!(differences.len(), 1);
        assert_eq!(differences[0].kind, SchemaDifferenceKind::DefinitionChanged);
        assert!(script_diff(&differences).contains("Target definition"));
    }

    #[test]
    fn builds_query_store_and_xevent_queries() {
        assert!(QueryStore::top_queries_query("weird]db", 0).contains("USE [weird]]db]"));
        assert!(QueryStore::top_queries_query("db", 0).contains("TOP (1)"));
        assert!(ExtendedEvents::sessions_query().contains("sys.server_event_sessions"));
        assert!(ExtendedEvents::session_events_query("bad'name").contains("bad''name"));
        assert!(ExtendedEvents::live_ring_buffer_query("session").contains("ring_buffer"));
    }

    #[test]
    fn reports_and_datatier_hints_cover_advanced_tools() {
        assert!(standard_report_query(StandardReport::DiskUsage).contains("sys.database_files"));
        let plan = DataTierArtifactPlan {
            kind: DataTierArtifactKind::Bacpac,
            database: "Sales".to_string(),
            path: "/tmp/sales.bacpac".to_string(),
        };
        assert!(plan.sqlpackage_hint().contains("/Action:Export"));
    }

    /// The Oracle insight reads the shared pool, ranks by the requested
    /// metric, keeps itself out of the results, and reads plans by SQL_ID.
    #[test]
    fn oracle_insights_read_the_shared_pool_and_xplan() {
        let by_duration = QueryStore::oracle_top_query(0, QueryStoreRank::AvgDuration);
        assert!(by_duration.starts_with("SELECT /* sqlly:insights */ s.sql_id AS query_id"));
        assert!(by_duration.contains("FROM v$sqlarea s"));
        assert!(by_duration.contains("sql_fulltext AS query_sql_text"));
        assert!(by_duration.contains("s.sql_text NOT LIKE '%sqlly:insights%'"));
        assert!(by_duration.contains("ORDER BY s.elapsed_time / GREATEST(s.executions, 1) DESC"));
        assert!(by_duration.ends_with("FETCH FIRST 1 ROWS ONLY"));
        let by_count = QueryStore::oracle_top_query(25, QueryStoreRank::ExecutionCount);
        assert!(by_count.contains("ORDER BY s.executions DESC FETCH FIRST 25 ROWS ONLY"));
        assert!(by_count.contains("parsing_schema_name"));
        let plan = QueryStore::oracle_plan_query("g4pkmrqrgxg3b");
        assert_eq!(
            plan,
            "SELECT plan_table_output FROM TABLE(DBMS_XPLAN.DISPLAY_CURSOR('g4pkmrqrgxg3b', NULL, 'TYPICAL'))"
        );
        assert!(QueryStore::oracle_plan_query("x'y").contains("'x''y'"));
    }
}
