//! Disk-usage queries per engine: how big every database is, and (where the
//! engine exposes it) the per-file layout behind the current database.
//!
//! Read-only by design — these queries observe sizes and growth settings and
//! never change anything.

/// The engines the disk-usage pane knows how to ask.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskDialect {
    SqlServer,
    Postgres,
    MySql,
    Sqlite,
    DuckDb,
    /// ClickHouse: active parts in `system.parts`, disks in `system.disks`.
    ClickHouse,
    /// Oracle: the login's own segments per tablespace (`USER_SEGMENTS`
    /// needs no privilege; `DBA_*` detail does).
    Oracle,
    /// Redis: memory per logical database — key counts from `INFO keyspace`
    /// and a bounded `MEMORY USAGE` sample over a `SCAN` of each database.
    Redis,
}

pub struct DiskUsage;

impl DiskUsage {
    /// Per-database sizes for the whole server (or, for SQLite, the one
    /// attached file). Column names differ by engine because the concepts
    /// differ; each query orders largest-first where it can.
    pub fn databases_query_for(dialect: DiskDialect) -> &'static str {
        match dialect {
            DiskDialect::SqlServer => {
                "SELECT d.name AS database_name, d.state_desc, d.recovery_model_desc, \
                 CAST(SUM(CASE WHEN f.type = 0 THEN f.size ELSE 0 END) * 8 / 1024.0 AS DECIMAL(18,1)) AS data_mb, \
                 CAST(SUM(CASE WHEN f.type = 1 THEN f.size ELSE 0 END) * 8 / 1024.0 AS DECIMAL(18,1)) AS log_mb, \
                 COUNT(*) AS file_count \
                 FROM sys.databases d JOIN sys.master_files f ON f.database_id = d.database_id \
                 GROUP BY d.name, d.state_desc, d.recovery_model_desc \
                 ORDER BY data_mb DESC"
            }
            DiskDialect::Postgres => {
                // Sizes of databases the login cannot connect to are not
                // readable; report them as NULL instead of failing the query.
                "SELECT d.datname AS database_name, \
                 pg_catalog.pg_get_userbyid(d.datdba) AS owner, \
                 CASE WHEN pg_catalog.has_database_privilege(d.datname, 'CONNECT') \
                      THEN ROUND((pg_catalog.pg_database_size(d.datname) / 1048576.0)::numeric, 1) \
                      ELSE NULL END AS size_mb \
                 FROM pg_catalog.pg_database d \
                 WHERE NOT d.datistemplate \
                 ORDER BY size_mb DESC NULLS LAST"
            }
            DiskDialect::MySql => {
                "SELECT table_schema AS database_name, \
                 ROUND(SUM(data_length) / 1048576, 1) AS data_mb, \
                 ROUND(SUM(index_length) / 1048576, 1) AS index_mb, \
                 ROUND(SUM(data_free) / 1048576, 1) AS free_mb, \
                 COUNT(*) AS table_count \
                 FROM information_schema.tables \
                 GROUP BY table_schema \
                 ORDER BY data_mb DESC"
            }
            DiskDialect::Sqlite => {
                "SELECT ROUND((SELECT page_count FROM pragma_page_count()) * (SELECT page_size FROM pragma_page_size()) / 1048576.0, 2) AS size_mb, \
                 ROUND((SELECT freelist_count FROM pragma_freelist_count()) * (SELECT page_size FROM pragma_page_size()) / 1048576.0, 2) AS reclaimable_mb, \
                 (SELECT page_count FROM pragma_page_count()) AS page_count, \
                 (SELECT page_size FROM pragma_page_size()) AS page_size"
            }
            DiskDialect::DuckDb => {
                // One row per attached database: blocks in use, free blocks
                // reclaimable by CHECKPOINT, the write-ahead log, and memory.
                // `wal_size`, `memory_usage` and `memory_limit` are already
                // human-readable text in DuckDB ("1.2 MB").
                "SELECT database_name, \
                 ROUND(used_blocks * block_size / 1048576.0, 2) AS size_mb, \
                 ROUND(free_blocks * block_size / 1048576.0, 2) AS reclaimable_mb, \
                 wal_size, total_blocks, block_size, memory_usage, memory_limit \
                 FROM pragma_database_size() \
                 ORDER BY used_blocks DESC"
            }
            DiskDialect::ClickHouse => {
                // One row per database: bytes on disk of its active parts,
                // the uncompressed size those bytes hold, row and part counts.
                "SELECT database AS database_name, \
                 round(sum(bytes_on_disk) / 1048576, 1) AS size_mb, \
                 round(sum(data_uncompressed_bytes) / 1048576, 1) AS uncompressed_mb, \
                 sum(rows) AS row_count, \
                 uniqExact(table) AS table_count, \
                 count() AS part_count \
                 FROM system.parts WHERE active \
                 GROUP BY database \
                 ORDER BY size_mb DESC"
            }
            DiskDialect::Oracle => {
                // Per tablespace, the storage the connected schema owns —
                // readable by every login. DBA-only datafile totals would
                // fail for ordinary users, so they are left to the
                // dashboard's own (independently failing) storage group.
                "SELECT s.tablespace_name AS database_name, \
                 ROUND(SUM(s.bytes) / 1048576, 1) AS size_mb, \
                 ROUND(SUM(CASE WHEN s.segment_type LIKE 'TABLE%' THEN s.bytes ELSE 0 END) / 1048576, 1) AS data_mb, \
                 ROUND(SUM(CASE WHEN s.segment_type LIKE 'INDEX%' THEN s.bytes ELSE 0 END) / 1048576, 1) AS index_mb, \
                 ROUND(SUM(CASE WHEN s.segment_type LIKE 'LOB%' THEN s.bytes ELSE 0 END) / 1048576, 1) AS lob_mb, \
                 COUNT(*) AS segment_count \
                 FROM user_segments s \
                 GROUP BY s.tablespace_name \
                 ORDER BY size_mb DESC"
            }
            // One row per logical database: key counts, expiries, and the
            // memory the driver samples from up to 200 keys (estimated_mb
            // scales the sample to the key count).
            DiskDialect::Redis => "SQLLY.DATABASES MEMORY 200",
        }
    }

    /// Per-file detail for the current database, where the engine has files
    /// as a first-class concept. `None` means the tab does not apply.
    pub fn files_query_for(dialect: DiskDialect) -> Option<&'static str> {
        match dialect {
            DiskDialect::SqlServer => Some(
                "SELECT name AS logical_name, type_desc, physical_name, \
                 CAST(size * 8 / 1024.0 AS DECIMAL(18,1)) AS size_mb, \
                 CASE WHEN max_size = -1 THEN NULL ELSE CAST(CAST(max_size AS BIGINT) * 8 / 1024.0 AS DECIMAL(18,1)) END AS max_size_mb, \
                 CASE WHEN is_percent_growth = 1 THEN CAST(growth AS varchar(20)) + '%' \
                      ELSE CAST(CAST(growth * 8 / 1024.0 AS DECIMAL(18,1)) AS varchar(24)) + ' MB' END AS growth \
                 FROM sys.database_files ORDER BY type, name",
            ),
            // ClickHouse's storage volumes stand in for files: one row per
            // configured disk with its used and total space.
            DiskDialect::ClickHouse => Some(
                "SELECT name AS logical_name, type AS type_desc, path AS physical_name, \
                 round((total_space - free_space) / 1048576, 1) AS size_mb, \
                 round(total_space / 1048576, 1) AS max_size_mb, \
                 NULL AS growth \
                 FROM system.disks ORDER BY name",
            ),
            DiskDialect::Postgres
            | DiskDialect::MySql
            | DiskDialect::Sqlite
            | DiskDialect::DuckDb
            | DiskDialect::Oracle
            | DiskDialect::Redis => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_dialect_has_a_databases_query() {
        for dialect in [
            DiskDialect::SqlServer,
            DiskDialect::Postgres,
            DiskDialect::MySql,
            DiskDialect::Sqlite,
            DiskDialect::DuckDb,
            DiskDialect::ClickHouse,
            DiskDialect::Oracle,
        ] {
            let sql = DiskUsage::databases_query_for(dialect);
            assert!(
                sql.to_ascii_uppercase().contains("SELECT"),
                "{dialect:?} query is not a SELECT"
            );
        }
    }

    #[test]
    fn dialect_queries_target_native_catalogs() {
        assert!(
            DiskUsage::databases_query_for(DiskDialect::SqlServer).contains("sys.master_files")
        );
        assert!(DiskUsage::databases_query_for(DiskDialect::Postgres).contains("pg_database_size"));
        assert!(
            DiskUsage::databases_query_for(DiskDialect::MySql)
                .contains("information_schema.tables")
        );
        assert!(DiskUsage::databases_query_for(DiskDialect::Sqlite).contains("pragma_page_count"));
        assert!(DiskUsage::databases_query_for(DiskDialect::ClickHouse).contains("system.parts"));
        assert!(
            DiskUsage::files_query_for(DiskDialect::ClickHouse)
                .unwrap()
                .contains("system.disks")
        );
        assert!(DiskUsage::databases_query_for(DiskDialect::Oracle).contains("FROM user_segments"));
    }

    #[test]
    fn files_detail_is_sql_server_only() {
        assert!(
            DiskUsage::files_query_for(DiskDialect::SqlServer)
                .unwrap()
                .contains("sys.database_files")
        );
        assert!(DiskUsage::files_query_for(DiskDialect::Postgres).is_none());
        assert!(DiskUsage::files_query_for(DiskDialect::MySql).is_none());
        assert!(DiskUsage::files_query_for(DiskDialect::Sqlite).is_none());
        assert!(DiskUsage::files_query_for(DiskDialect::Oracle).is_none());
    }

    #[test]
    fn queries_are_read_only() {
        for dialect in [
            DiskDialect::SqlServer,
            DiskDialect::Postgres,
            DiskDialect::MySql,
            DiskDialect::Sqlite,
            DiskDialect::DuckDb,
            DiskDialect::Oracle,
        ] {
            let mut sql = DiskUsage::databases_query_for(dialect).to_ascii_uppercase();
            if let Some(files) = DiskUsage::files_query_for(dialect) {
                sql.push(' ');
                sql.push_str(&files.to_ascii_uppercase());
            }
            for verb in ["INSERT", "UPDATE ", "DELETE", "DROP", "ALTER", "CREATE"] {
                assert!(!sql.contains(verb), "{dialect:?} query contains {verb}");
            }
        }
    }
}
