//! Index-usage queries per engine: how often each index is actually read and
//! written, so unused indexes can be found — plus dialect-correct DROP INDEX
//! scripting for the ones the user decides to remove.
//!
//! The queries are read-only; the only mutation this module produces is a
//! DROP INDEX string handed back for script-first review.

/// The engines with index usage counters the analyzer can read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexDialect {
    SqlServer,
    Postgres,
    MySql,
    /// Oracle 12.2+: `ALL_INDEXES` joined to the automatic index usage
    /// tracking in `DBA_INDEX_USAGE` (needs SELECT on that view or
    /// `SELECT ANY DICTIONARY`).
    Oracle,
}

pub struct IndexUsage;

impl IndexUsage {
    /// Every index with its usage counters, least-read first. Each dialect
    /// includes an `is_enforcing` flag (primary key / unique) so callers can
    /// refuse to script drops for constraint-backing indexes.
    pub fn usage_query_for(dialect: IndexDialect) -> &'static str {
        match dialect {
            IndexDialect::SqlServer => {
                "SELECT s.name AS schema_name, o.name AS table_name, i.name AS index_name, i.type_desc, \
                 CAST(CASE WHEN i.is_primary_key = 1 OR i.is_unique = 1 THEN 1 ELSE 0 END AS BIT) AS is_enforcing, \
                 COALESCE(u.user_seeks, 0) AS user_seeks, \
                 COALESCE(u.user_scans, 0) AS user_scans, \
                 COALESCE(u.user_lookups, 0) AS user_lookups, \
                 COALESCE(u.user_seeks, 0) + COALESCE(u.user_scans, 0) + COALESCE(u.user_lookups, 0) AS total_reads, \
                 COALESCE(u.user_updates, 0) AS user_updates, \
                 u.last_user_seek, u.last_user_scan \
                 FROM sys.indexes i \
                 JOIN sys.objects o ON o.object_id = i.object_id AND o.is_ms_shipped = 0 \
                 JOIN sys.schemas s ON s.schema_id = o.schema_id \
                 LEFT JOIN sys.dm_db_index_usage_stats u \
                   ON u.object_id = i.object_id AND u.index_id = i.index_id AND u.database_id = DB_ID() \
                 WHERE i.type > 0 AND i.name IS NOT NULL \
                 ORDER BY total_reads ASC, user_updates DESC"
            }
            IndexDialect::Postgres => {
                "SELECT s.schemaname AS schema_name, s.relname AS table_name, s.indexrelname AS index_name, \
                 (x.indisunique OR x.indisprimary) AS is_enforcing, \
                 s.idx_scan AS index_scans, s.idx_tup_read, s.idx_tup_fetch, \
                 ROUND((pg_catalog.pg_relation_size(s.indexrelid) / 1048576.0)::numeric, 1) AS size_mb \
                 FROM pg_catalog.pg_stat_user_indexes s \
                 JOIN pg_catalog.pg_index x ON x.indexrelid = s.indexrelid \
                 ORDER BY s.idx_scan ASC, size_mb DESC"
            }
            IndexDialect::MySql => {
                "SELECT object_schema AS schema_name, object_name AS table_name, index_name, \
                 (index_name = 'PRIMARY') AS is_enforcing, \
                 count_read AS read_events, count_write AS write_events, count_star AS total_events \
                 FROM performance_schema.table_io_waits_summary_by_index_usage \
                 WHERE index_name IS NOT NULL \
                   AND object_schema NOT IN ('mysql', 'performance_schema', 'sys', 'information_schema') \
                 ORDER BY count_read ASC, count_write DESC \
                 LIMIT 500"
            }
            // The current schema's indexes with the automatic usage tracking
            // Oracle keeps since 12.2 (sampled: counts are approximate,
            // `last_used` is exact). LOB and IOT-internal `SYS_` indexes are
            // skipped (the PK/UNIQUE `SYS_C…` ones stay: they are enforcing);
            // an UNUSABLE index is flagged so it can be rebuilt or
            // dropped. Without SELECT on DBA_INDEX_USAGE the whole query
            // fails with ORA-00942, which the pane shows with a hint.
            IndexDialect::Oracle => {
                "SELECT i.owner AS schema_name, i.table_name, i.index_name, i.index_type, \
                 CASE WHEN EXISTS (SELECT 1 FROM all_constraints c WHERE c.constraint_type IN ('P', 'U') \
                   AND c.index_owner = i.owner AND c.index_name = i.index_name) THEN 1 ELSE 0 END AS is_enforcing, \
                 NVL(u.total_access_count, 0) AS total_access_count, \
                 NVL(u.total_exec_count, 0) AS total_exec_count, \
                 NVL(u.total_rows_returned, 0) AS total_rows_returned, \
                 u.last_used, i.status, \
                 CASE WHEN i.status = 'UNUSABLE' THEN 1 ELSE 0 END AS is_unusable, \
                 i.uniqueness, i.last_analyzed \
                 FROM all_indexes i \
                 LEFT JOIN dba_index_usage u ON u.owner = i.owner AND u.name = i.index_name \
                 WHERE i.owner = SYS_CONTEXT('USERENV', 'CURRENT_SCHEMA') \
                   AND i.index_type NOT IN ('LOB', 'IOT - TOP') \
                 ORDER BY total_access_count ASC, i.table_name, i.index_name"
            }
        }
    }

    /// A reviewed DROP INDEX statement in the dialect's own syntax. Callers
    /// gate on `is_enforcing` before offering this — dropping a constraint's
    /// backing index needs constraint DDL, not DROP INDEX.
    pub fn drop_index_sql_for(
        dialect: IndexDialect,
        schema: &str,
        table: &str,
        index: &str,
    ) -> String {
        match dialect {
            IndexDialect::SqlServer => format!(
                "DROP INDEX {} ON {}.{};",
                bracket(index),
                bracket(schema),
                bracket(table)
            ),
            IndexDialect::Postgres => {
                format!("DROP INDEX {}.{};", pg_quote(schema), pg_quote(index))
            }
            IndexDialect::MySql => format!(
                "DROP INDEX {} ON {}.{};",
                backtick(index),
                backtick(schema),
                backtick(table)
            ),
            IndexDialect::Oracle => {
                format!(
                    "DROP INDEX {}.{};",
                    oracle_quote(schema),
                    oracle_quote(index)
                )
            }
        }
    }
}

fn oracle_quote(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn bracket(identifier: &str) -> String {
    format!("[{}]", identifier.replace(']', "]]"))
}

fn pg_quote(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn backtick(identifier: &str) -> String {
    format!("`{}`", identifier.replace('`', "``"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_queries_read_native_counters_and_flag_enforcing_indexes() {
        let tds = IndexUsage::usage_query_for(IndexDialect::SqlServer);
        assert!(tds.contains("dm_db_index_usage_stats"));
        assert!(tds.contains("is_enforcing"));
        let pg = IndexUsage::usage_query_for(IndexDialect::Postgres);
        assert!(pg.contains("pg_stat_user_indexes"));
        assert!(pg.contains("indisprimary"));
        let mysql = IndexUsage::usage_query_for(IndexDialect::MySql);
        assert!(mysql.contains("table_io_waits_summary_by_index_usage"));
        assert!(mysql.contains("is_enforcing"));
        let oracle = IndexUsage::usage_query_for(IndexDialect::Oracle);
        assert!(oracle.contains("dba_index_usage"));
        assert!(oracle.contains("all_indexes"));
        assert!(oracle.contains("is_enforcing"));
        assert!(oracle.contains("is_unusable"));
        assert!(oracle.contains("CURRENT_SCHEMA"));
        // The grid columns the pane's DROP/REBUILD buttons read.
        for column in ["schema_name", "table_name", "index_name"] {
            assert!(oracle.contains(column), "{column}");
        }
    }

    #[test]
    fn drop_scripts_use_each_dialects_quoting() {
        assert_eq!(
            IndexUsage::drop_index_sql_for(IndexDialect::SqlServer, "dbo", "Orders", "IX_Old"),
            "DROP INDEX [IX_Old] ON [dbo].[Orders];"
        );
        assert_eq!(
            IndexUsage::drop_index_sql_for(IndexDialect::Postgres, "public", "orders", "ix_old"),
            "DROP INDEX \"public\".\"ix_old\";"
        );
        assert_eq!(
            IndexUsage::drop_index_sql_for(IndexDialect::MySql, "shop", "orders", "ix_old"),
            "DROP INDEX `ix_old` ON `shop`.`orders`;"
        );
        assert_eq!(
            IndexUsage::drop_index_sql_for(IndexDialect::Oracle, "HR", "EMP", "EMP_NAME_IX"),
            "DROP INDEX \"HR\".\"EMP_NAME_IX\";"
        );
    }

    #[test]
    fn drop_scripts_escape_hostile_identifiers() {
        let sql = IndexUsage::drop_index_sql_for(IndexDialect::SqlServer, "d]bo", "T", "I");
        assert!(sql.contains("[d]]bo]"));
        let sql = IndexUsage::drop_index_sql_for(IndexDialect::Postgres, "pu\"blic", "t", "i");
        assert!(sql.contains("\"pu\"\"blic\""));
        let sql = IndexUsage::drop_index_sql_for(IndexDialect::MySql, "s`hop", "t", "i");
        assert!(sql.contains("`s``hop`"));
        let sql = IndexUsage::drop_index_sql_for(IndexDialect::Oracle, "H\"R", "t", "i");
        assert!(sql.contains("\"H\"\"R\""));
    }

    #[test]
    fn usage_queries_are_read_only() {
        for dialect in [
            IndexDialect::SqlServer,
            IndexDialect::Postgres,
            IndexDialect::MySql,
            IndexDialect::Oracle,
        ] {
            let sql = IndexUsage::usage_query_for(dialect).to_ascii_uppercase();
            for verb in ["INSERT", "UPDATE ", "DELETE", "DROP", "ALTER", "CREATE"] {
                assert!(!sql.contains(verb), "{dialect:?} query contains {verb}");
            }
        }
    }
}
