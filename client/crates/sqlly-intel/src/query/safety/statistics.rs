//! Include Statistics wrap handling: recognizing the sentinel statements the
//! GPUI app adds around a user's SQL so classification sees the user's own
//! statements, and deciding when the wrap must be skipped entirely.
//!
//! Sentinels are recognized only as *whole statements* — bounded by the
//! security lexer exactly as the engine bounds them — whose text is exactly a
//! known sentinel. The old textual peel (split on `"\nGO\n"`, strip a prefix
//! and a suffix) looked at raw bytes, so a sentinel-looking string literal
//! could shift which part of a batch was classified.

use super::batch_lead::tsql_batch_must_lead;

/// Whether Include Statistics should leave the batch untouched (T-SQL batches
/// that must lead their batch cannot be prefixed with sentinel batches).
pub fn should_skip_statistics_wrap(sql: &str) -> bool {
    tsql_batch_must_lead(sql.trim())
}

/// Read-only session-snapshot statements the app wraps a run in. The T-SQL
/// sentinel (`sys.dm_exec_sessions` SELECTs in their own `GO` batches) needs
/// no entry: it is a plain SELECT and classifies as one.
const SENTINELS: &[&str] = &[
    // MySQL / MariaDB `wrap_mysql_session_stats`.
    "SHOW SESSION STATUS LIKE 'Handler%'",
    // The retired T-SQL wrap; harmless session switches.
    "SET STATISTICS TIME, IO ON",
    "SET STATISTICS TIME, IO OFF",
];

/// Whether one statement's source text (terminator excluded) is exactly an
/// Include Statistics sentinel, modulo surrounding whitespace and case.
pub(super) fn is_statistics_sentinel(statement: &str) -> bool {
    let statement = statement.trim().trim_end_matches(';').trim_end();
    SENTINELS.iter().any(|s| s.eq_ignore_ascii_case(statement))
}

#[cfg(test)]
mod tests {
    use crate::query::safety::{classify_select_only, classify_select_only_for};
    use sqlly_core::DatabaseType;

    #[test]
    fn select_classifier_ignores_tds_session_stats_wrap() {
        let wrapped = "SELECT 'cpu_time' FROM sys.dm_exec_sessions WHERE session_id = @@SPID\nGO\nSELECT 1\nGO\n\
             SELECT 'cpu_time' FROM sys.dm_exec_sessions WHERE session_id = @@SPID"
            .to_string();
        let result = classify_select_only(&wrapped);
        assert!(result.select_only, "{:?}", result.reason);
        let result = classify_select_only_for(DatabaseType::SqlServer, &wrapped);
        assert!(result.select_only, "{:?}", result.reason);

        let delete_wrapped = "SELECT 'cpu_time' FROM sys.dm_exec_sessions WHERE session_id = @@SPID\nGO\nDELETE FROM t\nGO\n\
             SELECT 'cpu_time' FROM sys.dm_exec_sessions WHERE session_id = @@SPID"
            .to_string();
        assert!(!classify_select_only(&delete_wrapped).select_only);
        assert!(!classify_select_only_for(DatabaseType::SqlServer, &delete_wrapped).select_only);
    }

    /// The textual peel split on `"\nGO\n"` and classified only the middle,
    /// so GO lines inside string literals shifted what was judged: here the
    /// DELETE sits between two literals that the peel treated as sentinel
    /// batches, and the "middle" it classified was a harmless SELECT.
    #[test]
    fn sentinel_lookalikes_in_literals_cannot_shift_the_classified_region() {
        let sql = "SELECT '@@SPID\nGO\nSELECT 1 ' ; DELETE FROM t; SELECT '\nGO\n@@SPID'";
        assert!(!classify_select_only(sql).select_only);
        assert!(!classify_select_only_for(DatabaseType::SqlServer, sql).select_only);
    }

    #[test]
    fn select_classifier_ignores_mysql_session_stats_wrap() {
        let wrapped = "SHOW SESSION STATUS LIKE 'Handler%';\nSELECT 1\n;\nSHOW SESSION STATUS LIKE 'Handler%'";
        let result = classify_select_only(wrapped);
        assert!(result.select_only, "{:?}", result.reason);
        let result = classify_select_only_for(DatabaseType::MySql, wrapped);
        assert!(result.select_only, "{:?}", result.reason);

        let delete_wrapped = "SHOW SESSION STATUS LIKE 'Handler%';\nDELETE FROM t\n;\nSHOW SESSION STATUS LIKE 'Handler%'";
        assert!(!classify_select_only(delete_wrapped).select_only);
        assert!(!classify_select_only_for(DatabaseType::MySql, delete_wrapped).select_only);
    }

    #[test]
    fn legacy_set_statistics_wrap_is_recognized_only_as_whole_statements() {
        let wrapped = "SET STATISTICS TIME, IO ON;\nSELECT 1;\nSET STATISTICS TIME, IO OFF";
        assert!(classify_select_only(wrapped).select_only);

        let delete_wrapped =
            "SET STATISTICS TIME, IO ON;\nDELETE FROM t;\nSET STATISTICS TIME, IO OFF";
        assert!(!classify_select_only(delete_wrapped).select_only);
        // Without a separator the OFF is part of the user statement (a SET),
        // which fails closed.
        let glued = "SET STATISTICS TIME, IO ON;\nSELECT 1\nSET STATISTICS TIME, IO OFF";
        assert!(!classify_select_only(glued).select_only);
    }
}
