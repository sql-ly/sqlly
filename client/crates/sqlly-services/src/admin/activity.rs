pub struct ActivityMonitor;

impl ActivityMonitor {
    pub fn sessions_query() -> &'static str {
        "SELECT s.session_id, s.status, s.login_time, s.host_name, s.program_name, \
         s.login_name, s.cpu_time, s.memory_usage, s.total_elapsed_time, \
         s.reads, s.writes, s.logical_reads, \
         r.status AS request_status, r.command, r.wait_type, r.wait_time, r.blocking_session_id, \
         r.sql_handle, t.text AS query_text \
         FROM sys.dm_exec_sessions s \
         LEFT JOIN sys.dm_exec_requests r ON s.session_id = r.session_id \
         OUTER APPLY sys.dm_exec_sql_text(r.sql_handle) t \
         WHERE s.session_id > 50 \
         ORDER BY s.session_id"
    }

    pub fn blocking_query() -> &'static str {
        "SELECT blocking.session_id AS blocking_session_id, \
         blocked.session_id AS blocked_session_id, \
         DB_NAME(blocked.database_id) AS database_name, \
         blocked.wait_type, blocked.wait_time, \
         blocking_sql.text AS blocking_query, \
         blocked_sql.text AS blocked_query \
         FROM sys.dm_exec_requests blocked \
         INNER JOIN sys.dm_exec_sessions blocking ON blocked.blocking_session_id = blocking.session_id \
         OUTER APPLY sys.dm_exec_sql_text(blocked.sql_handle) blocked_sql \
         OUTER APPLY sys.dm_exec_sql_text(blocking.most_recent_sql_handle) blocking_sql \
         WHERE blocked.blocking_session_id <> 0"
    }

    pub fn expensive_queries_query() -> &'static str {
        "SELECT TOP 20 qs.total_elapsed_time / qs.execution_count AS avg_elapsed_time_us, \
         qs.execution_count, qs.total_logical_reads, qs.total_logical_writes, \
         qs.total_elapsed_time, SUBSTRING(st.text, \
         (qs.statement_start_offset/2)+1, \
         ((CASE qs.statement_end_offset WHEN -1 THEN DATALENGTH(st.text) \
         ELSE qs.statement_end_offset END - qs.statement_start_offset)/2) + 1) AS query_text \
         FROM sys.dm_exec_query_stats qs \
         CROSS APPLY sys.dm_exec_sql_text(qs.sql_handle) st \
         ORDER BY qs.total_elapsed_time DESC"
    }

    pub fn kill_session_sql(session_id: i32) -> String {
        format!("KILL {session_id}")
    }

    pub fn waits_query() -> &'static str {
        "SELECT wait_type, waiting_tasks_count, wait_time_ms, signal_wait_time_ms FROM sys.dm_os_wait_stats WHERE wait_type NOT LIKE 'SLEEP%' ORDER BY wait_time_ms DESC"
    }

    pub fn activity_snapshot_queries() -> Vec<&'static str> {
        vec![
            Self::sessions_query(),
            Self::blocking_query(),
            Self::expensive_queries_query(),
            Self::waits_query(),
        ]
    }
}

pub struct DatabaseWorkflow;

impl DatabaseWorkflow {
    pub fn create_database_sql(name: &str) -> String {
        format!("CREATE DATABASE [{}]", quote_name(name))
    }

    pub fn drop_database_sql(name: &str) -> String {
        format!("DROP DATABASE [{}]", quote_name(name))
    }

    pub fn rename_database_sql(old_name: &str, new_name: &str) -> String {
        format!(
            "ALTER DATABASE [{}] MODIFY NAME = [{}]",
            quote_name(old_name),
            quote_name(new_name)
        )
    }

    pub fn detach_database_sql(name: &str) -> String {
        format!("EXEC sp_detach_db N'{}'", name.replace('\'', "''"))
    }

    pub fn attach_database_sql(name: &str, files: &[String]) -> String {
        let file_parts = files
            .iter()
            .map(|file| format!("(FILENAME = N'{}')", file.replace('\'', "''")))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "CREATE DATABASE [{}] ON {file_parts} FOR ATTACH",
            quote_name(name)
        )
    }
}

fn quote_name(name: &str) -> String {
    name.replace(']', "]]")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_snapshot_includes_waits() {
        assert_eq!(ActivityMonitor::activity_snapshot_queries().len(), 4);
        assert!(ActivityMonitor::waits_query().contains("dm_os_wait_stats"));
        assert_eq!(ActivityMonitor::kill_session_sql(57), "KILL 57");
    }

    #[test]
    fn scripts_database_workflows() {
        assert_eq!(
            DatabaseWorkflow::create_database_sql("app]db"),
            "CREATE DATABASE [app]]db]"
        );
        assert!(DatabaseWorkflow::rename_database_sql("old", "new").contains("MODIFY NAME"));
        assert!(
            DatabaseWorkflow::attach_database_sql("app", &["/data/app.mdf".to_string()])
                .contains("FOR ATTACH")
        );
    }
}

/// The engine family an activity query targets. Local to this crate (mirrors
/// the `sqlly-plans` PlanDialect pattern) so `sqlly-admin` stays dependency
/// free; callers map their `DatabaseType` onto it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityDialect {
    SqlServer,
    Postgres,
    MySql,
    /// MariaDB diverges from MySQL 8 exactly where activity data lives: its
    /// `information_schema.innodb_lock_waits` never moved to
    /// `performance_schema.data_locks`, and the `sys` schema may be absent on
    /// instances upgraded from pre-10.6 — so blocking reads the native
    /// information_schema views instead of `sys.innodb_lock_waits`.
    MariaDb,
    /// ClickHouse: `system.processes` is the session list (one row per
    /// running query, keyed by a text `query_id`), `system.query_log` the
    /// expensive-query history, and `system.metrics` the wait picture.
    /// There are no locks to block on. Sessions are addressed through a
    /// stable numeric hash of the query id so Kill can name them.
    ClickHouse,
    /// Oracle: `V$SESSION` / `V$SQL` / `V$SYSTEM_EVENT` (SELECT_CATALOG_ROLE
    /// or `SELECT ANY DICTIONARY` widens what a login can see); sessions are
    /// addressed by SID, and the kill looks the SERIAL# up itself.
    Oracle,
    /// Redis: `CLIENT LIST` is the session list (the driver shapes it with
    /// `session_id` = the client id and `query_text` = its last command),
    /// `SLOWLOG GET` the expensive-command history, and `INFO` gauges the
    /// wait picture. Redis has no locks. Kill is `CLIENT KILL ID <id>`.
    Redis,
}

/// The `session_id` expression for a ClickHouse process: a positive Int32
/// hash of its `query_id`, usable in `KILL QUERY WHERE …`.
const CLICKHOUSE_SESSION_ID: &str = "toInt32(cityHash64(query_id) % 2147483647)";

impl ActivityMonitor {
    /// The sessions listing in the engine's own catalog language, or `None`
    /// when the engine has no live-session concept worth showing (SQLite).
    /// Shapes are per-engine — the monitor renders whatever columns arrive —
    /// but the SQL-text column is always named so Copy SQL keeps working.
    pub fn sessions_query_for(dialect: ActivityDialect) -> &'static str {
        match dialect {
            ActivityDialect::SqlServer => Self::sessions_query(),
            ActivityDialect::Postgres => {
                "SELECT pid AS session_id, state AS status, backend_start AS login_time, \
                 client_hostname AS host_name, application_name AS program_name, \
                 usename AS login_name, datname AS database_name, wait_event_type, wait_event, \
                 (pg_blocking_pids(pid))[1] AS blocking_session_id, \
                 EXTRACT(EPOCH FROM (clock_timestamp() - COALESCE(query_start, xact_start, backend_start)))::bigint AS seconds_running, state_change, \
                 query AS query_text \
                 FROM pg_stat_activity WHERE pid <> pg_backend_pid() ORDER BY pid"
            }
            ActivityDialect::MySql | ActivityDialect::MariaDb => {
                "SELECT id AS session_id, state AS status, host AS host_name, \
                 user AS login_name, db AS database_name, command, time AS seconds_running, \
                 info AS query_text \
                 FROM information_schema.processlist ORDER BY id"
            }
            ActivityDialect::ClickHouse => {
                "SELECT toInt32(cityHash64(query_id) % 2147483647) AS session_id, \
                 if(is_cancelled, 'cancelling', 'running') AS status, \
                 client_hostname AS host_name, client_name AS program_name, \
                 user AS login_name, current_database AS database_name, \
                 toString(interface) AS interface, query_id, \
                 toInt64(elapsed) AS seconds_running, read_rows, written_rows, \
                 round(memory_usage / 1048576, 1) AS memory_mb, \
                 query AS query_text \
                 FROM system.processes WHERE query NOT LIKE '%system.processes%' ORDER BY elapsed DESC"
            }
            ActivityDialect::Oracle => {
                "SELECT s.sid AS session_id, s.serial# AS serial_no, s.status, s.logon_time AS login_time, \
                 s.machine AS host_name, s.program AS program_name, s.username AS login_name, \
                 s.schemaname AS database_name, s.event AS wait_type, s.seconds_in_wait AS wait_time, \
                 s.blocking_session AS blocking_session_id, s.last_call_et AS seconds_running, \
                 q.sql_text AS query_text \
                 FROM v$session s \
                 LEFT JOIN v$sql q ON q.sql_id = s.sql_id AND q.child_number = 0 \
                 WHERE s.type = 'USER' AND s.sid <> SYS_CONTEXT('USERENV', 'SID') \
                 ORDER BY s.sid"
            }
            // The driver renders CLIENT LIST with session_id, status,
            // login_name, host_name, database_name, seconds_running,
            // idle_seconds and query_text ahead of every raw field.
            ActivityDialect::Redis => "CLIENT LIST\n# columns: session_id … query_text",
        }
    }

    pub fn blocking_query_for(dialect: ActivityDialect) -> &'static str {
        match dialect {
            ActivityDialect::SqlServer => Self::blocking_query(),
            ActivityDialect::Postgres => {
                "SELECT blocking.pid AS blocking_session_id, blocked.pid AS blocked_session_id, \
                 blocked.datname AS database_name, blocked.wait_event_type, blocked.wait_event, \
                 blocking.query AS blocking_query, blocked.query AS blocked_query \
                 FROM pg_stat_activity blocked \
                 JOIN pg_stat_activity blocking ON blocking.pid = ANY(pg_blocking_pids(blocked.pid))"
            }
            ActivityDialect::MySql => {
                "SELECT blocking_pid AS blocking_session_id, waiting_pid AS blocked_session_id, \
                 blocking_query, waiting_query AS blocked_query, wait_age \
                 FROM sys.innodb_lock_waits"
            }
            ActivityDialect::MariaDb => {
                "SELECT blocking.trx_mysql_thread_id AS blocking_session_id, \
                 waiting.trx_mysql_thread_id AS blocked_session_id, \
                 blocking.trx_query AS blocking_query, waiting.trx_query AS blocked_query, \
                 TIMEDIFF(NOW(), waiting.trx_wait_started) AS wait_age \
                 FROM information_schema.innodb_lock_waits w \
                 JOIN information_schema.innodb_trx waiting ON waiting.trx_id = w.requesting_trx_id \
                 JOIN information_schema.innodb_trx blocking ON blocking.trx_id = w.blocking_trx_id"
            }
            // ClickHouse queries do not block each other on locks; the
            // shape is kept so the tab renders an honest empty list.
            ActivityDialect::ClickHouse => {
                "SELECT toInt32(0) AS blocking_session_id, toInt32(0) AS blocked_session_id, \
                 '' AS database_name, '' AS blocking_query, '' AS blocked_query WHERE 0"
            }
            ActivityDialect::Oracle => {
                "SELECT b.sid AS blocking_session_id, w.sid AS blocked_session_id, \
                 w.schemaname AS database_name, w.event AS wait_type, w.seconds_in_wait AS wait_time, \
                 bq.sql_text AS blocking_query, wq.sql_text AS blocked_query \
                 FROM v$session w \
                 JOIN v$session b ON b.sid = w.blocking_session \
                 LEFT JOIN v$sql wq ON wq.sql_id = w.sql_id AND wq.child_number = 0 \
                 LEFT JOIN v$sql bq ON bq.sql_id = NVL(b.sql_id, b.prev_sql_id) AND bq.child_number = 0 \
                 WHERE w.blocking_session IS NOT NULL"
            }
            // Redis is single-threaded and lock-free; the driver answers an
            // empty set with the blocking columns.
            ActivityDialect::Redis => "SQLLY.BLOCKING",
        }
    }

    /// Entries the Redis slow-log tab asks for. The threshold note references
    /// it so the tab says honestly how many commands it is showing.
    pub const REDIS_SLOWLOG_LIMIT: usize = 128;

    /// Expensive/aggregate query statistics. PostgreSQL requires the
    /// `pg_stat_statements` extension; when absent, the server's own error
    /// names it, which the monitor surfaces honestly.
    pub fn expensive_queries_query_for(dialect: ActivityDialect) -> &'static str {
        match dialect {
            ActivityDialect::SqlServer => Self::expensive_queries_query(),
            ActivityDialect::Postgres => {
                "SELECT round(mean_exec_time)::bigint AS avg_elapsed_ms, calls AS execution_count, \
                 rows AS total_rows, round(total_exec_time)::bigint AS total_elapsed_ms, \
                 shared_blks_read AS logical_reads, query AS query_text \
                 FROM pg_stat_statements ORDER BY total_exec_time DESC LIMIT 20"
            }
            ActivityDialect::MySql | ActivityDialect::MariaDb => {
                "SELECT ROUND(avg_timer_wait / 1000000000) AS avg_elapsed_ms, \
                 count_star AS execution_count, sum_rows_examined AS rows_examined, \
                 ROUND(sum_timer_wait / 1000000000) AS total_elapsed_ms, \
                 digest_text AS query_text \
                 FROM performance_schema.events_statements_summary_by_digest \
                 WHERE digest_text IS NOT NULL ORDER BY sum_timer_wait DESC LIMIT 20"
            }
            ActivityDialect::ClickHouse => {
                "SELECT toInt64(avg(query_duration_ms)) AS avg_elapsed_ms, count() AS execution_count, \
                 sum(result_rows) AS total_rows, sum(query_duration_ms) AS total_elapsed_ms, \
                 sum(read_rows) AS rows_examined, any(query) AS query_text \
                 FROM system.query_log WHERE type = 'QueryFinish' AND is_initial_query \
                 AND event_time > now() - INTERVAL 1 DAY \
                 GROUP BY normalized_query_hash ORDER BY total_elapsed_ms DESC LIMIT 20"
            }
            ActivityDialect::Oracle => {
                "SELECT ROUND(elapsed_time / GREATEST(executions, 1) / 1000) AS avg_elapsed_ms, \
                 executions AS execution_count, rows_processed AS total_rows, \
                 ROUND(elapsed_time / 1000) AS total_elapsed_ms, buffer_gets AS logical_reads, \
                 sql_text AS query_text \
                 FROM v$sql WHERE parsing_schema_name NOT IN ('SYS', 'SYSTEM') \
                 ORDER BY elapsed_time DESC FETCH FIRST 20 ROWS ONLY"
            }
            // The slow log: one row per command over slowlog-log-slower-than.
            ActivityDialect::Redis => "SLOWLOG GET 128",
        }
    }

    /// The slow-log threshold read, where the engine keeps one. Only Redis's
    /// expensive-query tab is a slow log; every other dialect answers `None`
    /// so the tab hides the threshold row instead of failing a query.
    pub fn slowlog_threshold_query_for(dialect: ActivityDialect) -> Option<&'static str> {
        match dialect {
            ActivityDialect::Redis => Some("CONFIG GET slowlog-log-slower-than"),
            _ => None,
        }
    }

    /// The reviewed threshold change, in microseconds (`-1` disables the slow
    /// log). `None` for dialects without a slow log and for values below -1,
    /// so a caller's invalid input never reaches the server. Runtime-only
    /// until the user also runs [`Self::slowlog_rewrite_for`] — the tab says
    /// so next to the action.
    pub fn slowlog_threshold_set_for(dialect: ActivityDialect, micros: i64) -> Option<String> {
        match dialect {
            ActivityDialect::Redis if micros >= -1 => {
                Some(format!("CONFIG SET slowlog-log-slower-than {micros}"))
            }
            _ => None,
        }
    }

    /// `CONFIG REWRITE` persists runtime `CONFIG SET` changes to the server's
    /// config file. Offered as its own reviewed action — nothing rewrites the
    /// user's config file implicitly.
    pub fn slowlog_rewrite_for(dialect: ActivityDialect) -> Option<&'static str> {
        match dialect {
            ActivityDialect::Redis => Some("CONFIG REWRITE"),
            _ => None,
        }
    }

    /// The threshold from a shaped `CONFIG GET slowlog-log-slower-than` reply
    /// (field/value rows), `None` when the rows do not carry it.
    pub fn slowlog_threshold_from_rows(rows: &[Vec<sqlly_core::CellValue>]) -> Option<i64> {
        rows.iter().find_map(|row| {
            let field = match row.first() {
                Some(sqlly_core::CellValue::String(s)) => s.as_str(),
                _ => return None,
            };
            if !field.eq_ignore_ascii_case("slowlog-log-slower-than") {
                return None;
            }
            match row.get(1) {
                Some(sqlly_core::CellValue::String(s)) => s.trim().parse().ok(),
                Some(sqlly_core::CellValue::I64(n)) => Some(*n),
                _ => None,
            }
        })
    }

    /// `10000` → `10000 µs (10 ms)`; under a millisecond the µs stand alone.
    pub fn slowlog_threshold_text(micros: i64) -> String {
        if micros < 1000 {
            return format!("{micros} µs");
        }
        let ms = micros as f64 / 1000.0;
        if ms.fract() == 0.0 {
            format!("{micros} µs ({} ms)", ms as i64)
        } else {
            format!("{micros} µs ({ms:.1} ms)")
        }
    }

    pub fn slowlog_threshold_lead_message(threshold: Option<i64>) -> sqlly_core::DisplayMessage {
        use sqlly_core::DisplayMessage;
        match threshold {
            Some(micros) if micros < 0 => DisplayMessage::new(
                "services.admin.activity.slow_log.lead_disabled",
                format!("Slow log disabled ({micros})"),
            )
            .with_arg("threshold_micros", micros.to_string()),
            Some(micros) => DisplayMessage::new(
                "services.admin.activity.slow_log.lead_threshold",
                format!("Threshold: {}", Self::slowlog_threshold_text(micros)),
            )
            .with_arg("threshold", Self::slowlog_threshold_text(micros)),
            None => DisplayMessage::new(
                "services.admin.activity.slow_log.lead_unknown",
                "Threshold unknown",
            ),
        }
    }

    /// Legacy English fallback; localized callers use
    /// [`Self::slowlog_threshold_lead_message`].
    pub fn slowlog_threshold_lead(threshold: Option<i64>) -> String {
        Self::slowlog_threshold_lead_message(threshold).into_fallback()
    }

    pub fn slowlog_threshold_note_message(threshold: Option<i64>) -> sqlly_core::DisplayMessage {
        use sqlly_core::DisplayMessage;
        let limit = Self::REDIS_SLOWLOG_LIMIT;
        match threshold {
            Some(micros) if micros < 0 => DisplayMessage::new(
                "services.admin.activity.slow_log.note_disabled",
                format!("the slow log is disabled (slowlog-log-slower-than = {micros}) — showing up to the {limit} most recent commands recorded before"),
            )
            .with_arg("threshold_micros", micros.to_string())
            .with_arg("command_limit", limit.to_string()),
            Some(micros) => DisplayMessage::new(
                "services.admin.activity.slow_log.note_threshold",
                format!("showing the {limit} most recent commands slower than {}", Self::slowlog_threshold_text(micros)),
            )
            .with_arg("command_limit", limit.to_string())
            .with_arg("threshold", Self::slowlog_threshold_text(micros)),
            None => DisplayMessage::new(
                "services.admin.activity.slow_log.note_unknown",
                format!("showing the {limit} most recent slow-log commands (the slowlog-log-slower-than threshold could not be read)"),
            )
            .with_arg("command_limit", limit.to_string()),
        }
    }

    pub fn slowlog_threshold_note(threshold: Option<i64>) -> String {
        Self::slowlog_threshold_note_message(threshold).into_fallback()
    }

    pub fn slowlog_threshold_error_message(error: &str) -> sqlly_core::DisplayMessage {
        use sqlly_core::DisplayMessage;
        let lower = error.to_ascii_lowercase();
        let (key, hint) = if lower.contains("noperm") || lower.contains("permission denied") {
            (
                "services.admin.activity.slow_log.error_permission",
                "this user lacks permission to run CONFIG GET (+config|get)",
            )
        } else if lower.contains("unknown command") {
            (
                "services.admin.activity.slow_log.error_disabled_or_renamed",
                "CONFIG is disabled or renamed on this server",
            )
        } else {
            (
                "services.admin.activity.slow_log.error_failed",
                "CONFIG GET failed",
            )
        };
        DisplayMessage::new(key, format!("{hint}: {}", error.trim()))
            .with_arg("detail", error.trim())
    }

    pub fn slowlog_threshold_error(error: &str) -> String {
        Self::slowlog_threshold_error_message(error).into_fallback()
    }

    pub fn waits_query_for(dialect: ActivityDialect) -> &'static str {
        match dialect {
            ActivityDialect::SqlServer => Self::waits_query(),
            ActivityDialect::Postgres => {
                "SELECT COALESCE(wait_event_type, 'None') AS wait_type, COUNT(*) AS waiting_sessions \
                 FROM pg_stat_activity GROUP BY 1 ORDER BY 2 DESC"
            }
            ActivityDialect::MySql | ActivityDialect::MariaDb => {
                "SELECT event_name AS wait_type, count_star AS waiting_tasks_count, \
                 ROUND(sum_timer_wait / 1000000000) AS wait_time_ms \
                 FROM performance_schema.events_waits_summary_global_by_event_name \
                 WHERE count_star > 0 ORDER BY sum_timer_wait DESC LIMIT 50"
            }
            // The gauges that stand in for waits: queries in flight, merges,
            // background pool jobs and open connections.
            ActivityDialect::ClickHouse => {
                "SELECT metric AS wait_type, value AS waiting_tasks_count, description \
                 FROM system.metrics \
                 WHERE metric IN ('Query', 'Merge', 'Move', 'PartMutation', 'ReplicatedFetch', \
                 'ReplicatedSend', 'ReplicatedChecks', 'BackgroundMergesAndMutationsPoolTask', \
                 'BackgroundFetchesPoolTask', 'BackgroundCommonPoolTask', 'BackgroundMovePoolTask', \
                 'BackgroundSchedulePoolTask', 'DelayedInserts', 'DistributedSend', \
                 'TCPConnection', 'HTTPConnection', 'InterserverConnection', 'MySQLConnection', \
                 'PostgreSQLConnection', 'QueryThread', 'MemoryTracking') \
                 ORDER BY value DESC"
            }
            ActivityDialect::Oracle => {
                "SELECT event AS wait_type, total_waits AS waiting_tasks_count, \
                 ROUND(time_waited_micro / 1000) AS wait_time_ms \
                 FROM v$system_event WHERE wait_class <> 'Idle' \
                 ORDER BY time_waited_micro DESC FETCH FIRST 50 ROWS ONLY"
            }
            // INFO gauges: blocked clients, evictions, expirations, forks…
            ActivityDialect::Redis => "SQLLY.WAITS",
        }
    }

    /// Stop a session the engine's own way. PostgreSQL terminates the backend
    /// (a SELECT, so it composes into any runner); MySQL kills the connection.
    pub fn kill_session_sql_for(dialect: ActivityDialect, session_id: i32) -> String {
        match dialect {
            ActivityDialect::SqlServer => Self::kill_session_sql(session_id),
            ActivityDialect::Postgres => format!("SELECT pg_terminate_backend({session_id})"),
            ActivityDialect::MySql | ActivityDialect::MariaDb => format!("KILL {session_id}"),
            ActivityDialect::ClickHouse => {
                format!("KILL QUERY WHERE {CLICKHOUSE_SESSION_ID} = {session_id}")
            }
            // ALTER SYSTEM KILL SESSION needs the literal 'sid,serial#'; the
            // block looks the serial up so the caller only needs the SID.
            ActivityDialect::Oracle => format!(
                "BEGIN FOR s IN (SELECT sid, serial# FROM v$session WHERE sid = {session_id}) LOOP EXECUTE IMMEDIATE 'ALTER SYSTEM KILL SESSION ''' || s.sid || ',' || s.serial# || ''' IMMEDIATE'; END LOOP; END;"
            ),
            ActivityDialect::Redis => format!("CLIENT KILL ID {session_id}"),
        }
    }

    /// A read-only, engine-native snapshot for one session. This intentionally
    /// returns reviewable SQL rather than querying on the caller's behalf.
    pub fn diagnostic_sql_for(dialect: ActivityDialect, session_id: i32) -> String {
        match dialect {
            ActivityDialect::SqlServer => format!(
                "-- Read-only diagnostics for SQL Server session {session_id}.\nSELECT s.session_id, s.status, s.login_name, s.host_name, s.program_name, r.command, r.wait_type, r.wait_time, r.blocking_session_id, r.total_elapsed_time, t.text AS query_text\nFROM sys.dm_exec_sessions s\nLEFT JOIN sys.dm_exec_requests r ON r.session_id = s.session_id\nOUTER APPLY sys.dm_exec_sql_text(r.sql_handle) t\nWHERE s.session_id = {session_id};"
            ),
            ActivityDialect::Postgres => format!(
                "-- Read-only diagnostics for PostgreSQL backend {session_id}.\nSELECT pid, usename, datname, application_name, client_addr, state, wait_event_type, wait_event, xact_start, query_start, pg_blocking_pids(pid) AS blocking_pids, query\nFROM pg_stat_activity\nWHERE pid = {session_id};"
            ),
            ActivityDialect::MySql => format!(
                "-- Read-only diagnostics for MySQL connection {session_id}.\nSELECT id, user, host, db, command, time, state, info\nFROM information_schema.processlist\nWHERE id = {session_id};\n\nSELECT *\nFROM sys.innodb_lock_waits\nWHERE blocking_pid = {session_id} OR waiting_pid = {session_id};"
            ),
            ActivityDialect::MariaDb => format!(
                "-- Read-only diagnostics for MariaDB connection {session_id}.\nSELECT id, user, host, db, command, time, state, info\nFROM information_schema.processlist\nWHERE id = {session_id};\n\nSELECT w.*, waiting.trx_mysql_thread_id AS waiting_pid, blocking.trx_mysql_thread_id AS blocking_pid\nFROM information_schema.innodb_lock_waits w\nJOIN information_schema.innodb_trx waiting ON waiting.trx_id = w.requesting_trx_id\nJOIN information_schema.innodb_trx blocking ON blocking.trx_id = w.blocking_trx_id\nWHERE waiting.trx_mysql_thread_id = {session_id} OR blocking.trx_mysql_thread_id = {session_id};"
            ),
            ActivityDialect::ClickHouse => format!(
                "-- Read-only diagnostics for ClickHouse query {session_id} (session id = {CLICKHOUSE_SESSION_ID}).\nSELECT query_id, user, client_hostname, client_name, toString(interface) AS interface, current_database, elapsed, read_rows, read_bytes, written_rows, memory_usage, peak_memory_usage, is_cancelled, query\nFROM system.processes\nWHERE {CLICKHOUSE_SESSION_ID} = {session_id};"
            ),
            ActivityDialect::Oracle => format!(
                "-- Read-only diagnostics for Oracle session {session_id}.\nSELECT s.sid, s.serial#, s.username, s.status, s.machine, s.program, s.event, s.wait_class, s.seconds_in_wait, s.blocking_session, s.last_call_et, q.sql_text\nFROM v$session s\nLEFT JOIN v$sql q ON q.sql_id = s.sql_id AND q.child_number = 0\nWHERE s.sid = {session_id};\n\nSELECT l.type, l.lmode, l.request, l.ctime, o.owner, o.object_name\nFROM v$lock l\nLEFT JOIN all_objects o ON o.object_id = l.id1 AND l.type = 'TM'\nWHERE l.sid = {session_id};"
            ),
            ActivityDialect::Redis => format!(
                "# Read-only diagnostics for Redis client {session_id}: its CLIENT LIST line, then the server's client and memory sections.\nCLIENT LIST ID {session_id}\nINFO clients\nINFO memory"
            ),
        }
    }
}

#[cfg(test)]
mod dialect_tests {
    use super::*;

    #[test]
    fn every_dialect_covers_every_tab() {
        for dialect in [
            ActivityDialect::SqlServer,
            ActivityDialect::Postgres,
            ActivityDialect::MySql,
            ActivityDialect::MariaDb,
            ActivityDialect::ClickHouse,
            ActivityDialect::Oracle,
            ActivityDialect::Redis,
        ] {
            assert!(!ActivityMonitor::sessions_query_for(dialect).is_empty());
            assert!(!ActivityMonitor::blocking_query_for(dialect).is_empty());
            assert!(!ActivityMonitor::expensive_queries_query_for(dialect).is_empty());
            assert!(!ActivityMonitor::waits_query_for(dialect).is_empty());
            assert!(!ActivityMonitor::kill_session_sql_for(dialect, 42).is_empty());
        }
    }

    #[test]
    fn sql_text_columns_stay_discoverable() {
        // The monitor's Copy SQL finds text by column name; every engine's
        // sessions query must keep a query-ish column name.
        for dialect in [
            ActivityDialect::SqlServer,
            ActivityDialect::Postgres,
            ActivityDialect::MySql,
            ActivityDialect::MariaDb,
            ActivityDialect::ClickHouse,
            ActivityDialect::Oracle,
            ActivityDialect::Redis,
        ] {
            assert!(
                ActivityMonitor::sessions_query_for(dialect).contains("query_text"),
                "{dialect:?}"
            );
        }
    }

    #[test]
    fn redis_speaks_commands_not_sql() {
        assert_eq!(
            ActivityMonitor::kill_session_sql_for(ActivityDialect::Redis, 42),
            "CLIENT KILL ID 42"
        );
        assert!(
            ActivityMonitor::sessions_query_for(ActivityDialect::Redis).starts_with("CLIENT LIST")
        );
        assert_eq!(
            ActivityMonitor::blocking_query_for(ActivityDialect::Redis),
            "SQLLY.BLOCKING"
        );
        assert!(
            ActivityMonitor::expensive_queries_query_for(ActivityDialect::Redis)
                .starts_with("SLOWLOG GET")
        );
        assert!(
            ActivityMonitor::diagnostic_sql_for(ActivityDialect::Redis, 42)
                .contains("CLIENT LIST ID 42")
        );
    }

    #[test]
    fn redis_slowlog_threshold_read_set_and_rewrite() {
        // Only Redis has a slow log; every other dialect hides the row.
        for dialect in [
            ActivityDialect::SqlServer,
            ActivityDialect::Postgres,
            ActivityDialect::MySql,
            ActivityDialect::MariaDb,
            ActivityDialect::ClickHouse,
            ActivityDialect::Oracle,
        ] {
            assert!(ActivityMonitor::slowlog_threshold_query_for(dialect).is_none());
            assert!(ActivityMonitor::slowlog_threshold_set_for(dialect, 1000).is_none());
            assert!(ActivityMonitor::slowlog_rewrite_for(dialect).is_none());
        }
        assert_eq!(
            ActivityMonitor::slowlog_threshold_query_for(ActivityDialect::Redis),
            Some("CONFIG GET slowlog-log-slower-than")
        );
        assert_eq!(
            ActivityMonitor::slowlog_threshold_set_for(ActivityDialect::Redis, 2500),
            Some("CONFIG SET slowlog-log-slower-than 2500".to_string())
        );
        // -1 disables the slow log; anything below that never reaches the server.
        assert_eq!(
            ActivityMonitor::slowlog_threshold_set_for(ActivityDialect::Redis, -1),
            Some("CONFIG SET slowlog-log-slower-than -1".to_string())
        );
        assert!(ActivityMonitor::slowlog_threshold_set_for(ActivityDialect::Redis, -2).is_none());
        assert_eq!(
            ActivityMonitor::slowlog_rewrite_for(ActivityDialect::Redis),
            Some("CONFIG REWRITE")
        );
        // The tab reads the same number of entries the note claims.
        assert!(
            ActivityMonitor::expensive_queries_query_for(ActivityDialect::Redis)
                .ends_with(&ActivityMonitor::REDIS_SLOWLOG_LIMIT.to_string())
        );
    }

    #[test]
    fn redis_slowlog_threshold_parsing_and_display() {
        use sqlly_core::CellValue;
        let rows = vec![vec![
            CellValue::String("slowlog-log-slower-than".into()),
            CellValue::String("2500".into()),
        ]];
        assert_eq!(
            ActivityMonitor::slowlog_threshold_from_rows(&rows),
            Some(2500)
        );
        let int_rows = vec![vec![
            CellValue::String("slowlog-log-slower-than".into()),
            CellValue::I64(-1),
        ]];
        assert_eq!(
            ActivityMonitor::slowlog_threshold_from_rows(&int_rows),
            Some(-1)
        );
        assert_eq!(
            ActivityMonitor::slowlog_threshold_from_rows(&[vec![CellValue::String(
                "databases".into()
            )]]),
            None
        );
        assert_eq!(ActivityMonitor::slowlog_threshold_from_rows(&[]), None);

        assert_eq!(ActivityMonitor::slowlog_threshold_text(500), "500 µs");
        assert_eq!(
            ActivityMonitor::slowlog_threshold_text(10_000),
            "10000 µs (10 ms)"
        );
        assert_eq!(
            ActivityMonitor::slowlog_threshold_text(2500),
            "2500 µs (2.5 ms)"
        );

        let note = ActivityMonitor::slowlog_threshold_note(Some(10_000));
        assert!(
            note.contains("128 most recent commands slower than 10000 µs"),
            "{note}"
        );
        assert!(ActivityMonitor::slowlog_threshold_note(Some(-1)).contains("disabled"));
        assert!(ActivityMonitor::slowlog_threshold_note(None).contains("could not be read"));
        // Every negative threshold disables logging — never "slower than -5 µs".
        let note = ActivityMonitor::slowlog_threshold_note(Some(-5));
        assert!(
            note.contains("disabled") && !note.contains("slower than"),
            "{note}"
        );
        assert_eq!(
            ActivityMonitor::slowlog_threshold_lead(Some(-5)),
            "Slow log disabled (-5)"
        );
        assert_eq!(
            ActivityMonitor::slowlog_threshold_lead(Some(0)),
            "Threshold: 0 µs"
        );
        assert_eq!(
            ActivityMonitor::slowlog_threshold_lead(None),
            "Threshold unknown"
        );
    }

    #[test]
    fn redis_slowlog_threshold_errors_say_why() {
        let noperm = ActivityMonitor::slowlog_threshold_error(
            "Permission denied: line 1: NOPERM User reader has no permissions to run the 'config|get' command",
        );
        assert!(noperm.starts_with("this user lacks permission"), "{noperm}");
        assert!(noperm.contains("NOPERM User reader"), "{noperm}");
        let renamed = ActivityMonitor::slowlog_threshold_error(
            "Query execution error: line 1: ERR unknown command 'CONFIG', with args beginning with: 'GET'",
        );
        assert!(
            renamed.starts_with("CONFIG is disabled or renamed"),
            "{renamed}"
        );
        let other = ActivityMonitor::slowlog_threshold_error("Connection error: reset");
        assert!(
            other.starts_with("CONFIG GET failed: Connection error"),
            "{other}"
        );
    }

    #[test]
    fn kill_maps_to_the_engine_mechanism() {
        assert_eq!(
            ActivityMonitor::kill_session_sql_for(ActivityDialect::SqlServer, 7),
            "KILL 7"
        );
        assert!(
            ActivityMonitor::kill_session_sql_for(ActivityDialect::Postgres, 7)
                .contains("pg_terminate_backend(7)")
        );
        assert_eq!(
            ActivityMonitor::kill_session_sql_for(ActivityDialect::MySql, 7),
            "KILL 7"
        );
        let oracle = ActivityMonitor::kill_session_sql_for(ActivityDialect::Oracle, 7);
        assert!(oracle.starts_with("BEGIN"), "{oracle}");
        assert!(oracle.contains("WHERE sid = 7"), "{oracle}");
        assert!(oracle.contains("ALTER SYSTEM KILL SESSION"), "{oracle}");
        assert!(oracle.ends_with("END;"), "{oracle}");
    }

    #[test]
    fn oracle_queries_read_the_v_dollar_views_and_address_sessions_by_sid() {
        let sessions = ActivityMonitor::sessions_query_for(ActivityDialect::Oracle);
        assert!(sessions.contains("FROM v$session s"));
        assert!(sessions.contains("s.sid AS session_id"));
        assert!(sessions.contains("blocking_session_id"));
        assert!(sessions.contains("seconds_running"));
        let blocking = ActivityMonitor::blocking_query_for(ActivityDialect::Oracle);
        assert!(blocking.contains("w.blocking_session IS NOT NULL"));
        let expensive = ActivityMonitor::expensive_queries_query_for(ActivityDialect::Oracle);
        assert!(expensive.contains("FROM v$sql"));
        assert!(expensive.contains("FETCH FIRST 20 ROWS ONLY"));
        let waits = ActivityMonitor::waits_query_for(ActivityDialect::Oracle);
        assert!(waits.contains("v$system_event"));
        let diag = ActivityMonitor::diagnostic_sql_for(ActivityDialect::Oracle, 12);
        assert!(diag.contains("v$lock"));
        assert!(diag.contains("s.sid = 12"));
        assert!(!diag.to_ascii_uppercase().contains("KILL SESSION"));
    }

    #[test]
    fn mariadb_blocking_avoids_sys_and_uses_native_information_schema() {
        let sql = ActivityMonitor::blocking_query_for(ActivityDialect::MariaDb);
        assert!(
            !sql.contains("sys.innodb_lock_waits"),
            "MariaDB must not depend on the optional sys schema: {sql}"
        );
        assert!(
            sql.contains("information_schema.innodb_lock_waits"),
            "{sql}"
        );
        assert!(sql.contains("information_schema.innodb_trx"), "{sql}");
    }

    #[test]
    fn diagnostic_scripts_are_read_only_and_engine_specific() {
        let sql_server = ActivityMonitor::diagnostic_sql_for(ActivityDialect::SqlServer, 57);
        assert!(sql_server.contains("dm_exec_sessions"));
        assert!(sql_server.contains("session_id = 57"));
        let postgres = ActivityMonitor::diagnostic_sql_for(ActivityDialect::Postgres, 123);
        assert!(postgres.contains("pg_stat_activity"));
        assert!(postgres.contains("pg_blocking_pids"));
        let mysql = ActivityMonitor::diagnostic_sql_for(ActivityDialect::MySql, 88);
        assert!(mysql.contains("information_schema.processlist"));
        assert!(mysql.contains("blocking_pid = 88"));
        for script in [sql_server, postgres, mysql] {
            let upper = script.to_ascii_uppercase();
            assert!(!upper.contains("KILL "));
            assert!(!upper.contains("TERMINATE_BACKEND"));
        }
    }

    #[test]
    fn postgres_sessions_expose_blockers_and_normalized_runtime() {
        let query = ActivityMonitor::sessions_query_for(ActivityDialect::Postgres);
        assert!(query.contains("blocking_session_id"));
        assert!(query.contains("seconds_running"));
    }
}
