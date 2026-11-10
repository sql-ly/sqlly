//! Include Statistics via session-counter snapshots.
//!
//! MySQL/MariaDB wrap the run in two `SHOW SESSION STATUS LIKE 'Handler%'`
//! snapshots; SQL Server wraps it in two `sys.dm_exec_sessions` snapshots
//! (own-session row via `@@SPID`, reshaped to the same name/value rows and
//! separated into their own `GO` batches so batch-lead DDL keeps working).
//! Either way the sentinel statements stream back as ordinary result sets,
//! and this module's filter peels those two sets out of the event stream,
//! renumbers the user's own result sets, and turns the counter delta into a
//! Messages-pane summary.
//!
//! The T-SQL sentinel was chosen over `SET STATISTICS TIME, IO` because the
//! driver's row stream silently discards the Info tokens that carry the
//! statistics text — the counters below actually arrive.
//!
//! Pure event-stream logic — no UI, no I/O — so the buffering/renumbering
//! rules are unit tested directly.

use sqlly_core::CellValue;
use sqlly_services::QueryEvent;

/// Which engine's sentinel wrap produced the stream, for the summary's
/// explanatory note.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatsDialect {
    MySql,
    Tds,
}

impl StatsDialect {
    fn note(self) -> String {
        crate::localization::text(match self {
            StatsDialect::MySql => "results.session_statistics.mysql_note",
            StatsDialect::Tds => "results.session_statistics.sql_server_note",
        })
    }
}

/// Wrap `sql` with the pre/post session-status snapshots. The user SQL keeps
/// its own statement structure; a lone `;` line terminates it safely even
/// when it ends in a line comment.
pub fn wrap_mysql_session_stats(sql: &str) -> String {
    let body = sql.trim_end();
    let body = body.strip_suffix(';').unwrap_or(body);
    format!("SHOW SESSION STATUS LIKE 'Handler%';\n{body}\n;\nSHOW SESSION STATUS LIKE 'Handler%'")
}

/// The T-SQL sentinel: this session's cumulative work counters, reshaped to
/// the same `Variable_name`/`Value` rows the filter consumes.
fn tds_sentinel_sql() -> String {
    let counters = [
        ("cpu_time", "cpu_time"),
        ("reads", "reads"),
        ("writes", "writes"),
        ("logical_reads", "logical_reads"),
        ("row_count", "row_count"),
        ("memory_usage_pages", "memory_usage"),
    ];
    counters
        .iter()
        .map(|(label, column)| {
            format!(
                "SELECT '{label}' AS [Variable_name], CAST({column} AS NVARCHAR(32)) AS [Value] FROM sys.dm_exec_sessions WHERE session_id = @@SPID"
            )
        })
        .collect::<Vec<_>>()
        .join("\nUNION ALL\n")
}

/// Wrap `sql` for SQL Server / Azure SQL. The sentinels ride in their own
/// `GO` batches so a user script whose batches begin with batch-lead DDL
/// (`CREATE PROCEDURE`, …) is left structurally untouched.
pub fn wrap_tds_session_stats(sql: &str) -> String {
    let sentinel = tds_sentinel_sql();
    format!("{sentinel}\nGO\n{sql}\nGO\n{sentinel}")
}

/// Streaming filter over a stats-wrapped MySQL run's events.
///
/// Set 0 is the pre-run snapshot (captured, suppressed). The LAST set is the
/// post-run snapshot, which is only identifiable in hindsight — so events for
/// the current highest-numbered set are held back until either a later set
/// starts (flush the held set to the UI, renumbered down by one) or the
/// terminal event arrives (the held set was the post snapshot: capture and
/// drop it, then emit the delta summary as a message before the terminal).
pub struct SessionStatsFilter {
    dialect: StatsDialect,
    pre: Vec<(String, i64)>,
    post: Vec<(String, i64)>,
    held: Vec<QueryEvent>,
    held_index: Option<usize>,
}

impl SessionStatsFilter {
    pub fn new(dialect: StatsDialect) -> Self {
        Self {
            dialect,
            pre: Vec::new(),
            post: Vec::new(),
            held: Vec::new(),
            held_index: None,
        }
    }

    /// Transform one upstream event into zero or more downstream events.
    pub fn process(&mut self, event: QueryEvent) -> Vec<QueryEvent> {
        match event {
            QueryEvent::ResultSet { index: 0, .. }
            | QueryEvent::RowCount { index: 0, .. }
            | QueryEvent::RowsProgress { index: 0, .. }
            | QueryEvent::SpillWindow { index: 0, .. } => Vec::new(),
            QueryEvent::Rows { index: 0, rows } => {
                collect_status_rows(&rows, &mut self.pre);
                Vec::new()
            }
            QueryEvent::ResultSet { index, columns } => {
                let out = self.flush_held();
                self.held_index = Some(index);
                self.held.push(QueryEvent::ResultSet { index, columns });
                out
            }
            event @ (QueryEvent::Rows { .. }
            | QueryEvent::RowCount { .. }
            | QueryEvent::RowsProgress { .. }
            | QueryEvent::SpillWindow { .. }) => {
                if event_index(&event) == self.held_index {
                    self.held.push(event);
                    Vec::new()
                } else {
                    // A straggler for an already-flushed set: renumber and
                    // pass through.
                    vec![renumber(event)]
                }
            }
            QueryEvent::SpillManifest { mut manifest } => {
                // Sentinel sets are tiny and never spill in practice, but be
                // safe: read-and-delete any spilled sentinel so its file is
                // cleaned up, and renumber the user sets.
                manifest.result_sets.retain(|set| {
                    let is_sentinel =
                        set.result_set_index == 0 || Some(set.result_set_index) == self.held_index;
                    if is_sentinel {
                        let target = if set.result_set_index == 0 {
                            &mut self.pre
                        } else {
                            &mut self.post
                        };
                        if let Some(rows) = sqlly_services::take_spilled_rows(set) {
                            collect_status_rows(&rows, target);
                        }
                    }
                    !is_sentinel
                });
                for set in &mut manifest.result_sets {
                    set.result_set_index = set.result_set_index.saturating_sub(1);
                }
                vec![QueryEvent::SpillManifest { manifest }]
            }
            terminal @ (QueryEvent::Completed { .. }
            | QueryEvent::Error { .. }
            | QueryEvent::Cancelled) => {
                // The held set (if any) is the post snapshot.
                for event in std::mem::take(&mut self.held) {
                    if let QueryEvent::Rows { rows, .. } = event {
                        collect_status_rows(&rows, &mut self.post);
                    }
                }
                self.held_index = None;
                let mut out = Vec::new();
                if matches!(terminal, QueryEvent::Completed { .. })
                    && let Some(summary) = delta_summary(&self.pre, &self.post, self.dialect)
                {
                    out.push(QueryEvent::Message { text: summary });
                }
                out.push(terminal);
                out
            }
            other => vec![other],
        }
    }

    /// Flush the held (now known to be a user) set, renumbered down by one.
    fn flush_held(&mut self) -> Vec<QueryEvent> {
        self.held_index = None;
        std::mem::take(&mut self.held)
            .into_iter()
            .map(renumber)
            .collect()
    }
}

fn event_index(event: &QueryEvent) -> Option<usize> {
    match event {
        QueryEvent::ResultSet { index, .. }
        | QueryEvent::Rows { index, .. }
        | QueryEvent::RowCount { index, .. }
        | QueryEvent::RowsProgress { index, .. }
        | QueryEvent::SpillWindow { index, .. } => Some(*index),
        _ => None,
    }
}

fn renumber(event: QueryEvent) -> QueryEvent {
    match event {
        QueryEvent::ResultSet { index, columns } => QueryEvent::ResultSet {
            index: index.saturating_sub(1),
            columns,
        },
        QueryEvent::Rows { index, rows } => QueryEvent::Rows {
            index: index.saturating_sub(1),
            rows,
        },
        QueryEvent::RowCount {
            index,
            rows_affected,
        } => QueryEvent::RowCount {
            index: index.saturating_sub(1),
            rows_affected,
        },
        QueryEvent::RowsProgress { index, rows_seen } => QueryEvent::RowsProgress {
            index: index.saturating_sub(1),
            rows_seen,
        },
        QueryEvent::SpillWindow {
            index,
            spill_file,
            rows_available,
        } => QueryEvent::SpillWindow {
            index: index.saturating_sub(1),
            spill_file,
            rows_available,
        },
        other => other,
    }
}

/// Parse `SHOW SESSION STATUS` rows (`Variable_name`, `Value`) into counters.
fn collect_status_rows(rows: &[Vec<CellValue>], into: &mut Vec<(String, i64)>) {
    for row in rows {
        let name = match row.first() {
            Some(CellValue::String(name)) => name.clone(),
            _ => continue,
        };
        let value = match row.get(1) {
            Some(CellValue::String(value)) => value.parse::<i64>().unwrap_or(0),
            Some(CellValue::I64(value)) => *value,
            _ => 0,
        };
        into.push((name, value));
    }
}

/// Human summary of the handler-counter delta, or `None` when either
/// snapshot is missing.
fn delta_summary(
    pre: &[(String, i64)],
    post: &[(String, i64)],
    dialect: StatsDialect,
) -> Option<String> {
    if pre.is_empty() || post.is_empty() {
        return None;
    }
    let mut deltas: Vec<(String, i64)> = Vec::new();
    for (name, post_value) in post {
        let pre_value = pre
            .iter()
            .find(|(pre_name, _)| pre_name == name)
            .map(|(_, value)| *value)
            .unwrap_or(0);
        let delta = post_value - pre_value;
        if delta != 0 {
            deltas.push((name.clone(), delta));
        }
    }
    if deltas.is_empty() {
        return Some(crate::localization::text(
            "results.session_statistics.no_activity",
        ));
    }
    deltas.sort_by(|a, b| a.0.cmp(&b.0));
    let list = deltas
        .iter()
        .map(|(name, delta)| format!("{name} +{delta}"))
        .collect::<Vec<_>>()
        .join(", ");
    Some(crate::localization::format(
        "results.session_statistics.delta_summary",
        &[("counter_deltas", &list), ("counter_note", &dialect.note())],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::ResultColumn;

    fn status_columns() -> Vec<ResultColumn> {
        ["Variable_name", "Value"]
            .into_iter()
            .map(|name| ResultColumn {
                name: name.to_string(),
                data_type: "text".to_string(),
                nullable: false,
            })
            .collect()
    }

    fn status_rows(values: &[(&str, i64)]) -> Vec<Vec<CellValue>> {
        values
            .iter()
            .map(|(name, value)| {
                vec![
                    CellValue::String(name.to_string()),
                    CellValue::String(value.to_string()),
                ]
            })
            .collect()
    }

    fn user_columns() -> Vec<ResultColumn> {
        vec![ResultColumn {
            name: "id".to_string(),
            data_type: "int".to_string(),
            nullable: false,
        }]
    }

    #[test]
    fn tds_wrap_isolates_sentinels_in_their_own_batches() {
        let wrapped = wrap_tds_session_stats("CREATE PROCEDURE dbo.P AS SELECT 1");
        assert!(wrapped.contains("@@SPID"));
        assert_eq!(
            wrapped.matches("\nGO\n").count(),
            2,
            "one GO before and one after the user script: {wrapped}"
        );
        // The user's batch still leads its own batch (batch-lead DDL safe).
        let middle = wrapped.split("\nGO\n").nth(1).expect("user batch present");
        assert!(middle.trim_start().starts_with("CREATE PROCEDURE"));
        // Six counters in each sentinel.
        assert_eq!(wrapped.matches("UNION ALL").count(), 10, "{wrapped}");
    }

    #[test]
    fn tds_summary_note_explains_the_counters() {
        let pre = vec![("cpu_time".to_string(), 100)];
        let post = vec![("cpu_time".to_string(), 250)];
        let text = delta_summary(&pre, &post, StatsDialect::Tds).unwrap();
        assert!(text.contains("cpu_time +150"), "{text}");
        assert!(text.contains("milliseconds"), "{text}");
    }

    /// Env-gated live proof: run a stats-wrapped query against a real
    /// server and assert the filter peels the sentinels, renumbers the user
    /// set to 0, and produces a counter-delta message.
    #[cfg(not(target_arch = "wasm32"))]
    fn run_live_stats(
        profile: sqlly_core::ConnectionProfile,
        wrapped: String,
        expect_counter: &str,
        dialect: StatsDialect,
    ) {
        crate::ensure_test_tokio_runtime();
        let outputs = crate::tokio_handle().block_on(async move {
            use sqlly_services::QueryExecutor as _;
            let executor = sqlly_services::query::DefaultQueryExecutor::new();
            let handle = executor
                .execute(&profile, &wrapped, 256)
                .await
                .expect("execute");
            let mut events = handle.events;
            let mut filter = SessionStatsFilter::new(dialect);
            let mut out = Vec::new();
            while let Some(event) = events.recv().await {
                out.extend(filter.process(event));
            }
            out
        });
        let message = outputs.iter().find_map(|event| match event {
            QueryEvent::Message { text } if text.contains("Include Statistics") => {
                Some(text.clone())
            }
            _ => None,
        });
        let message = message.expect("delta message emitted");
        assert!(message.contains(expect_counter), "{message}");
        // The user's own result set was renumbered down to 0.
        assert!(
            outputs
                .iter()
                .any(|event| matches!(event, QueryEvent::ResultSet { index: 0, .. })),
            "user set present at index 0"
        );
        assert!(
            !outputs
                .iter()
                .any(|event| matches!(event, QueryEvent::ResultSet { index, .. } if *index > 0)),
            "sentinel sets were peeled"
        );
    }

    #[test]
    #[cfg(not(target_arch = "wasm32"))]
    fn mysql_include_statistics_live() {
        let Ok(server) = std::env::var("SQLLY_MYSQL_SERVER") else {
            eprintln!("skipping: SQLLY_MYSQL_* env vars not set");
            return;
        };
        let mut profile = sqlly_core::ConnectionProfile::sql_login(
            "mysql-stats",
            server,
            std::env::var("SQLLY_MYSQL_USER").unwrap_or_else(|_| "root".into()),
            sqlly_core::SecretRef {
                provider: sqlly_core::SecretProvider::Environment,
                key: "SQLLY_MYSQL_PASSWORD".into(),
            },
        );
        profile.port = std::env::var("SQLLY_MYSQL_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3306);
        profile.database_type = sqlly_core::DatabaseType::MySql;
        profile.encrypt = false;
        let wrapped = wrap_mysql_session_stats("SELECT table_name FROM information_schema.tables");
        run_live_stats(profile, wrapped, "Handler_", StatsDialect::MySql);
    }

    #[test]
    #[cfg(not(target_arch = "wasm32"))]
    fn sqlserver_include_statistics_live() {
        let Ok(server) = std::env::var("SQLLY_SQL_SERVER") else {
            eprintln!("skipping: SQLLY_SQL_* env vars not set");
            return;
        };
        let mut profile = sqlly_core::ConnectionProfile::sql_login(
            "tds-stats",
            server,
            std::env::var("SQLLY_SQL_USERNAME").unwrap_or_else(|_| "sa".into()),
            sqlly_core::SecretRef {
                provider: sqlly_core::SecretProvider::Environment,
                key: "SQLLY_SQL_PASSWORD".into(),
            },
        );
        profile.port = std::env::var("SQLLY_SQL_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1433);
        profile.database_type = sqlly_core::DatabaseType::SqlServer;
        profile.trust_server_certificate = true;
        let wrapped = wrap_tds_session_stats("SELECT name FROM sys.objects");
        run_live_stats(profile, wrapped, "cpu_time", StatsDialect::Tds);
    }

    #[test]
    fn wrap_terminates_bodies_safely() {
        let wrapped = wrap_mysql_session_stats("SELECT 1;");
        assert!(wrapped.starts_with("SHOW SESSION STATUS"));
        assert!(wrapped.ends_with("SHOW SESSION STATUS LIKE 'Handler%'"));
        assert!(!wrapped.contains(";;"), "{wrapped}");
        // A trailing line comment must not eat the terminator.
        let commented = wrap_mysql_session_stats("SELECT 1 -- done");
        assert!(commented.contains("-- done\n;"), "{commented}");
    }

    #[test]
    fn filter_peels_sentinels_renumbers_and_summarizes() {
        let mut filter = SessionStatsFilter::new(StatsDialect::MySql);
        // Pre snapshot (set 0): suppressed.
        assert!(
            filter
                .process(QueryEvent::ResultSet {
                    index: 0,
                    columns: status_columns()
                })
                .is_empty()
        );
        assert!(
            filter
                .process(QueryEvent::Rows {
                    index: 0,
                    rows: status_rows(&[("Handler_read_key", 10), ("Handler_write", 5)])
                })
                .is_empty()
        );
        // User set (set 1): held until the next set starts.
        assert!(
            filter
                .process(QueryEvent::ResultSet {
                    index: 1,
                    columns: user_columns()
                })
                .is_empty()
        );
        assert!(
            filter
                .process(QueryEvent::Rows {
                    index: 1,
                    rows: vec![vec![CellValue::I64(7)]]
                })
                .is_empty()
        );
        // Post snapshot starts (set 2): the held user set flushes as set 0.
        let flushed = filter.process(QueryEvent::ResultSet {
            index: 2,
            columns: status_columns(),
        });
        assert_eq!(flushed.len(), 2);
        assert!(matches!(flushed[0], QueryEvent::ResultSet { index: 0, .. }));
        assert!(matches!(flushed[1], QueryEvent::Rows { index: 0, .. }));
        assert!(
            filter
                .process(QueryEvent::Rows {
                    index: 2,
                    rows: status_rows(&[("Handler_read_key", 13), ("Handler_write", 5)])
                })
                .is_empty()
        );
        // Terminal: post snapshot captured, delta message precedes Completed.
        let end = filter.process(QueryEvent::Completed { elapsed_ms: 12 });
        assert_eq!(end.len(), 2);
        match &end[0] {
            QueryEvent::Message { text } => {
                assert!(text.contains("Handler_read_key +3"), "{text}");
                assert!(
                    !text.contains("Handler_write +"),
                    "zero deltas are omitted: {text}"
                );
            }
            other => panic!("expected Message, got {other:?}"),
        }
        assert!(matches!(end[1], QueryEvent::Completed { .. }));
    }

    #[test]
    fn error_terminal_skips_the_summary() {
        let mut filter = SessionStatsFilter::new(StatsDialect::MySql);
        let _ = filter.process(QueryEvent::ResultSet {
            index: 0,
            columns: status_columns(),
        });
        let out = filter.process(QueryEvent::Error {
            message: "boom".to_string(),
        });
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0], QueryEvent::Error { .. }));
    }

    #[test]
    fn no_activity_still_reports_honestly() {
        let pre = vec![("Handler_read_key".to_string(), 4)];
        let post = vec![("Handler_read_key".to_string(), 4)];
        let text = delta_summary(&pre, &post, StatsDialect::MySql).unwrap();
        assert!(text.contains("no session activity"), "{text}");
    }
}
