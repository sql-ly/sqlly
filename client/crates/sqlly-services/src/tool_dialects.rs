//! One `DatabaseType` → tool-dialect table for admin/plan surfaces.
//!
//! Each tool crate (`sqlly-plans`, `sqlly-admin`) defines its own dialect
//! enum; this module is the single place that maps a connection's
//! [`DatabaseType`] onto those vocabularies so GPUI panes and
//! [`AdminToolsService`] never duplicate `connection_kind()` matches.

use crate::admin::activity::ActivityDialect;
use crate::admin::dashboard::DashboardDialect;
use crate::admin::disk_usage::DiskDialect;
use crate::admin::index_usage::IndexDialect;
use crate::plans::PlanDialect;
use sqlly_lang::sqlmodel::DatabaseType;

/// Procedure-mode grammar family. GPUI's `procedure_mode` module aliases this
/// as `ProcDialect`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProcToolDialect {
    #[default]
    TSql,
    Postgres,
}

/// Security editor dialect family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecurityDialect {
    SqlServer,
    Postgres,
    MySql,
    MariaDb,
    /// Read-only view over `system.users` / `system.roles` / `system.grants`
    /// with scripted `CREATE USER` / `GRANT` statements; ClickHouse access
    /// control is edited in SQL, not in the dialog's forms.
    ClickHouse,
    /// Oracle users, roles and grants from the `DBA_*` views (falling back
    /// to the `USER_*` / `ALL_*` views for logins without dictionary access).
    Oracle,
}

/// Maintenance-statement family (`crate::maintenance`): the engines whose
/// vacuum / analyze / reindex / integrity-check vocabulary the explorer's
/// Maintenance submenu renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaintenanceDialect {
    SqlServer,
    Postgres,
    MySql,
    Sqlite,
    /// SQLite's verbs minus the ones the libSQL server refuses (`VACUUM`,
    /// `ANALYZE`, `PRAGMA optimize`): `REINDEX` and `PRAGMA integrity_check`.
    LibSql,
    /// `OPTIMIZE TABLE … FINAL` merges a MergeTree table's parts.
    ClickHouse,
    /// Oracle: `DBMS_STATS` gathering, index rebuilds, `ANALYZE … VALIDATE
    /// STRUCTURE`.
    Oracle,
    /// DuckDB: `VACUUM` / `VACUUM ANALYZE` / `ANALYZE` plus `CHECKPOINT`;
    /// no REINDEX and no integrity check exist.
    DuckDb,
}

/// All tool dialects resolved for one engine in a single lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolDialects {
    pub plan: Option<PlanDialect>,
    pub index: Option<IndexDialect>,
    pub disk: Option<DiskDialect>,
    pub activity: Option<ActivityDialect>,
    pub dashboard: Option<DashboardDialect>,
    pub proc: ProcToolDialect,
    pub security: Option<SecurityDialect>,
    pub maintenance: Option<MaintenanceDialect>,
}

/// The canonical map: one match arm per engine group.
pub fn tool_dialects(db: DatabaseType) -> ToolDialects {
    use DatabaseType::*;
    match db {
        MariaDb => ToolDialects {
            plan: Some(PlanDialect::MariaDb),
            index: Some(IndexDialect::MySql),
            disk: Some(DiskDialect::MySql),
            activity: Some(ActivityDialect::MariaDb),
            dashboard: Some(DashboardDialect::MariaDb),
            proc: ProcToolDialect::TSql,
            security: Some(SecurityDialect::MariaDb),
            maintenance: Some(MaintenanceDialect::MySql),
        },
        MySql | AmazonAurora | AmazonRds | AzureMySqlFlexible | CloudSqlMySql => ToolDialects {
            plan: Some(PlanDialect::MySql),
            index: Some(IndexDialect::MySql),
            disk: Some(DiskDialect::MySql),
            activity: Some(ActivityDialect::MySql),
            dashboard: Some(DashboardDialect::MySql),
            proc: ProcToolDialect::TSql,
            security: Some(SecurityDialect::MySql),
            maintenance: Some(MaintenanceDialect::MySql),
        },
        PlanetScale | TiDb => ToolDialects {
            plan: Some(PlanDialect::MySql),
            index: Some(IndexDialect::MySql),
            disk: Some(DiskDialect::MySql),
            activity: Some(ActivityDialect::MySql),
            dashboard: Some(DashboardDialect::MySql),
            proc: ProcToolDialect::TSql,
            security: None,
            maintenance: None,
        },
        SqlServer | AzureSqlDatabase => ToolDialects {
            plan: Some(PlanDialect::SqlServer),
            index: Some(IndexDialect::SqlServer),
            disk: Some(DiskDialect::SqlServer),
            activity: Some(ActivityDialect::SqlServer),
            dashboard: Some(DashboardDialect::SqlServer),
            proc: ProcToolDialect::TSql,
            security: Some(SecurityDialect::SqlServer),
            maintenance: Some(MaintenanceDialect::SqlServer),
        },
        Postgres
        | AmazonAuroraPostgres
        | AmazonRdsPostgres
        | AzurePostgresFlexible
        | CloudSqlPostgres
        | Neon
        | Supabase => ToolDialects {
            plan: Some(PlanDialect::Postgres),
            index: Some(IndexDialect::Postgres),
            disk: Some(DiskDialect::Postgres),
            activity: Some(ActivityDialect::Postgres),
            dashboard: Some(DashboardDialect::Postgres),
            proc: ProcToolDialect::Postgres,
            security: Some(SecurityDialect::Postgres),
            maintenance: Some(MaintenanceDialect::Postgres),
        },
        CockroachDb | YugabyteDb => ToolDialects {
            plan: Some(PlanDialect::Postgres),
            index: Some(IndexDialect::Postgres),
            disk: Some(DiskDialect::Postgres),
            activity: Some(ActivityDialect::Postgres),
            dashboard: Some(DashboardDialect::Postgres),
            proc: ProcToolDialect::Postgres,
            security: None,
            maintenance: None,
        },
        // SQLite has no sessions to monitor, but the database file itself has
        // a size, a free list and contents — enough for a useful dashboard.
        Sqlite => ToolDialects {
            plan: Some(PlanDialect::Sqlite),
            index: None,
            disk: Some(DiskDialect::Sqlite),
            activity: None,
            dashboard: Some(DashboardDialect::Sqlite),
            proc: ProcToolDialect::TSql,
            security: None,
            maintenance: Some(MaintenanceDialect::Sqlite),
        },
        // libSQL / Turso is SQLite over the network: the same EXPLAIN QUERY
        // PLAN, dbstat / page counters and file dashboard, but the server
        // refuses VACUUM and ANALYZE, so maintenance is its own subset. No
        // sessions to monitor, no index usage counters, no security editor.
        LibSql => ToolDialects {
            plan: Some(PlanDialect::Sqlite),
            index: None,
            disk: Some(DiskDialect::Sqlite),
            activity: None,
            dashboard: Some(DashboardDialect::Sqlite),
            proc: ProcToolDialect::TSql,
            security: None,
            maintenance: Some(MaintenanceDialect::LibSql),
        },
        // DuckDB is in-process like SQLite: no sessions, no index usage
        // counters, but real plans (EXPLAIN / EXPLAIN ANALYZE), storage
        // counters from pragma_database_size(), and its own maintenance
        // verbs (VACUUM / ANALYZE / CHECKPOINT).
        DuckDb => ToolDialects {
            plan: Some(PlanDialect::DuckDb),
            index: None,
            disk: Some(DiskDialect::DuckDb),
            activity: None,
            dashboard: Some(DashboardDialect::DuckDb),
            proc: ProcToolDialect::TSql,
            security: None,
            maintenance: Some(MaintenanceDialect::DuckDb),
        },
        // ClickHouse: plans from `EXPLAIN json = 1` (estimated only), live
        // queries from system.processes, storage from system.parts, health
        // from system.metrics / system.events, OPTIMIZE TABLE … FINAL as
        // the one maintenance verb, and a read-only security view over
        // system.users / system.grants. No index usage counters.
        ClickHouse => ToolDialects {
            plan: Some(PlanDialect::ClickHouse),
            index: None,
            disk: Some(DiskDialect::ClickHouse),
            activity: Some(ActivityDialect::ClickHouse),
            dashboard: Some(DashboardDialect::ClickHouse),
            proc: ProcToolDialect::TSql,
            security: Some(SecurityDialect::ClickHouse),
            maintenance: Some(MaintenanceDialect::ClickHouse),
        },
        // Oracle: estimated plans through EXPLAIN PLAN / PLAN_TABLE, live
        // sessions from V$SESSION, storage from USER_SEGMENTS, a dashboard
        // from V$SYSSTAT / V$SGA, DBMS_STATS-style maintenance, index usage
        // from DBA_INDEX_USAGE, and users/roles/grants from the DBA_* views.
        Oracle => ToolDialects {
            plan: Some(PlanDialect::Oracle),
            index: Some(IndexDialect::Oracle),
            disk: Some(DiskDialect::Oracle),
            activity: Some(ActivityDialect::Oracle),
            dashboard: Some(DashboardDialect::Oracle),
            proc: ProcToolDialect::TSql,
            security: Some(SecurityDialect::Oracle),
            maintenance: Some(MaintenanceDialect::Oracle),
        },
        // Redis and its protocol relatives: no SQL, so no plans, indexes,
        // security editor or maintenance verbs. Monitoring comes from INFO
        // (dashboard), CLIENT LIST / SLOWLOG (activity) and a keyspace
        // memory sample (disk).
        Redis | Valkey | KeyDb | DragonflyDb | AmazonElastiCache | AmazonMemoryDb => ToolDialects {
            plan: None,
            index: None,
            disk: Some(DiskDialect::Redis),
            activity: Some(ActivityDialect::Redis),
            dashboard: Some(DashboardDialect::Redis),
            proc: ProcToolDialect::TSql,
            security: None,
            maintenance: None,
        },
        Db2 | Access | Snowflake | Spanner | Teradata | Informix | Databricks | SapHana => {
            ToolDialects {
                plan: None,
                index: None,
                disk: None,
                activity: None,
                dashboard: None,
                proc: ProcToolDialect::TSql,
                security: None,
                maintenance: None,
            }
        }
    }
}

pub fn plan_dialect(db: DatabaseType) -> Option<PlanDialect> {
    tool_dialects(db).plan
}

pub fn index_dialect(db: DatabaseType) -> Option<IndexDialect> {
    tool_dialects(db).index
}

pub fn disk_dialect(db: DatabaseType) -> Option<DiskDialect> {
    tool_dialects(db).disk
}

pub fn activity_dialect(db: DatabaseType) -> Option<ActivityDialect> {
    tool_dialects(db).activity
}

pub fn dashboard_dialect(db: DatabaseType) -> Option<DashboardDialect> {
    tool_dialects(db).dashboard
}

pub fn security_dialect(db: DatabaseType) -> Option<SecurityDialect> {
    tool_dialects(db).security
}

pub fn proc_dialect(db: DatabaseType) -> ProcToolDialect {
    tool_dialects(db).proc
}

/// The maintenance vocabulary for an engine, or `None` where the verbs do
/// not exist (CockroachDB has no VACUUM, TiDB no OPTIMIZE TABLE, and the
/// unimplemented engines have nothing wired), so the menu shows a reason
/// instead of a statement that would fail. DuckDB has its own small set
/// (VACUUM, ANALYZE, CHECKPOINT).
pub fn maintenance_dialect(db: DatabaseType) -> Option<MaintenanceDialect> {
    tool_dialects(db).maintenance
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redis_family_shares_the_monitoring_dialects_only() {
        for db in [
            DatabaseType::Redis,
            DatabaseType::Valkey,
            DatabaseType::KeyDb,
            DatabaseType::DragonflyDb,
            DatabaseType::AmazonElastiCache,
            DatabaseType::AmazonMemoryDb,
        ] {
            let dialects = tool_dialects(db);
            assert_eq!(dialects.disk, Some(DiskDialect::Redis), "{db:?}");
            assert_eq!(dialects.activity, Some(ActivityDialect::Redis));
            assert_eq!(dialects.dashboard, Some(DashboardDialect::Redis));
            assert!(dialects.plan.is_none());
            assert!(dialects.index.is_none());
            assert!(dialects.security.is_none());
            assert!(dialects.maintenance.is_none());
        }
    }

    #[test]
    fn maria_db_plan_and_activity_differ_from_mysql() {
        let mysql = tool_dialects(DatabaseType::MySql);
        let maria = tool_dialects(DatabaseType::MariaDb);
        assert_eq!(mysql.plan, Some(PlanDialect::MySql));
        assert_eq!(maria.plan, Some(PlanDialect::MariaDb));
        assert_eq!(mysql.activity, Some(ActivityDialect::MySql));
        assert_eq!(maria.activity, Some(ActivityDialect::MariaDb));
        assert_eq!(mysql.index, maria.index);
        assert_eq!(mysql.disk, maria.disk);
    }

    #[test]
    fn proc_dialect_is_postgres_only_on_postgres_wire_family() {
        assert_eq!(
            proc_dialect(DatabaseType::Postgres),
            ProcToolDialect::Postgres
        );
        assert_eq!(
            proc_dialect(DatabaseType::CockroachDb),
            ProcToolDialect::Postgres
        );
        assert_eq!(proc_dialect(DatabaseType::SqlServer), ProcToolDialect::TSql);
        assert_eq!(proc_dialect(DatabaseType::MariaDb), ProcToolDialect::TSql);
    }

    /// A file database has no sessions to watch, so no activity monitor — but
    /// its file statistics make a real dashboard.
    #[test]
    fn sqlite_has_disk_plan_and_dashboard_but_not_index_or_activity() {
        let sqlite = tool_dialects(DatabaseType::Sqlite);
        assert_eq!(sqlite.plan, Some(PlanDialect::Sqlite));
        assert_eq!(sqlite.disk, Some(DiskDialect::Sqlite));
        assert_eq!(sqlite.dashboard, Some(DashboardDialect::Sqlite));
        assert!(sqlite.index.is_none());
        assert!(sqlite.activity.is_none());
    }

    #[test]
    fn maintenance_dialect_covers_the_native_families_only() {
        use MaintenanceDialect::*;
        assert_eq!(maintenance_dialect(DatabaseType::DuckDb), Some(DuckDb));
        assert_eq!(
            maintenance_dialect(DatabaseType::SqlServer),
            Some(SqlServer)
        );
        assert_eq!(
            maintenance_dialect(DatabaseType::AzureSqlDatabase),
            Some(SqlServer)
        );
        assert_eq!(maintenance_dialect(DatabaseType::Postgres), Some(Postgres));
        assert_eq!(maintenance_dialect(DatabaseType::Supabase), Some(Postgres));
        assert_eq!(maintenance_dialect(DatabaseType::MySql), Some(MySql));
        assert_eq!(maintenance_dialect(DatabaseType::MariaDb), Some(MySql));
        assert_eq!(maintenance_dialect(DatabaseType::Sqlite), Some(Sqlite));
        assert_eq!(maintenance_dialect(DatabaseType::LibSql), Some(LibSql));
        assert_eq!(
            maintenance_dialect(DatabaseType::ClickHouse),
            Some(ClickHouse)
        );
        assert!(maintenance_dialect(DatabaseType::PlanetScale).is_none());
        assert!(maintenance_dialect(DatabaseType::TiDb).is_none());
        assert!(maintenance_dialect(DatabaseType::CockroachDb).is_none());
        assert!(maintenance_dialect(DatabaseType::YugabyteDb).is_none());
        assert_eq!(maintenance_dialect(DatabaseType::Oracle), Some(Oracle));
        assert!(maintenance_dialect(DatabaseType::Db2).is_none());
    }

    /// libSQL shares SQLite's plan, disk and dashboard SQL but not its
    /// maintenance vocabulary (the server refuses VACUUM / ANALYZE).
    #[test]
    fn libsql_reuses_sqlite_tools_with_its_own_maintenance_subset() {
        let libsql = tool_dialects(DatabaseType::LibSql);
        let sqlite = tool_dialects(DatabaseType::Sqlite);
        assert_eq!(libsql.plan, sqlite.plan);
        assert_eq!(libsql.disk, sqlite.disk);
        assert_eq!(libsql.dashboard, sqlite.dashboard);
        assert_eq!(libsql.maintenance, Some(MaintenanceDialect::LibSql));
        assert!(libsql.index.is_none());
        assert!(libsql.activity.is_none());
        assert!(libsql.security.is_none());
    }

    #[test]
    fn clickhouse_has_every_tool_but_index_usage() {
        let ch = tool_dialects(DatabaseType::ClickHouse);
        assert_eq!(ch.plan, Some(PlanDialect::ClickHouse));
        assert_eq!(ch.disk, Some(DiskDialect::ClickHouse));
        assert_eq!(ch.activity, Some(ActivityDialect::ClickHouse));
        assert_eq!(ch.dashboard, Some(DashboardDialect::ClickHouse));
        assert_eq!(ch.maintenance, Some(MaintenanceDialect::ClickHouse));
        assert_eq!(ch.security, Some(SecurityDialect::ClickHouse));
        assert!(ch.index.is_none());
    }

    #[test]
    fn unsupported_engines_map_to_none() {
        assert!(maintenance_dialect(DatabaseType::Db2).is_none());
        assert!(plan_dialect(DatabaseType::Db2).is_none());
        assert!(index_dialect(DatabaseType::Db2).is_none());
        assert!(disk_dialect(DatabaseType::Db2).is_none());
        assert!(activity_dialect(DatabaseType::Db2).is_none());
        assert!(security_dialect(DatabaseType::Db2).is_none());
        assert!(dashboard_dialect(DatabaseType::Db2).is_none());
    }

    /// Oracle has every tool dialect: plans, sessions, storage, dashboard,
    /// maintenance, index usage and the security editor.
    #[test]
    fn oracle_has_every_tool_dialect() {
        let oracle = tool_dialects(DatabaseType::Oracle);
        assert_eq!(oracle.plan, Some(PlanDialect::Oracle));
        assert_eq!(oracle.disk, Some(DiskDialect::Oracle));
        assert_eq!(oracle.activity, Some(ActivityDialect::Oracle));
        assert_eq!(oracle.dashboard, Some(DashboardDialect::Oracle));
        assert_eq!(oracle.maintenance, Some(MaintenanceDialect::Oracle));
        assert_eq!(oracle.index, Some(IndexDialect::Oracle));
        assert_eq!(oracle.security, Some(SecurityDialect::Oracle));
        // DuckDB keeps its own vocabulary, never Oracle's.
        assert_eq!(
            tool_dialects(DatabaseType::DuckDb).maintenance,
            Some(MaintenanceDialect::DuckDb)
        );
    }

    /// Every engine with a live activity monitor also has a dashboard; SQLite
    /// additionally has one built from file statistics alone.
    #[test]
    fn dashboard_dialect_covers_every_engine_with_an_activity_monitor() {
        for db in DatabaseType::ALL {
            if activity_dialect(db).is_some() {
                assert!(dashboard_dialect(db).is_some(), "{db:?}");
            }
        }
        assert!(activity_dialect(DatabaseType::Sqlite).is_none());
        assert!(dashboard_dialect(DatabaseType::Sqlite).is_some());
    }

    #[test]
    fn security_dialect_maps_engine_groups() {
        use SecurityDialect::*;
        assert_eq!(security_dialect(DatabaseType::SqlServer), Some(SqlServer));
        assert_eq!(security_dialect(DatabaseType::Postgres), Some(Postgres));
        assert_eq!(security_dialect(DatabaseType::MySql), Some(MySql));
        assert_eq!(security_dialect(DatabaseType::MariaDb), Some(MariaDb));
        assert_eq!(security_dialect(DatabaseType::ClickHouse), Some(ClickHouse));
        assert_eq!(security_dialect(DatabaseType::Oracle), Some(Oracle));
        assert!(security_dialect(DatabaseType::PlanetScale).is_none());
        assert!(security_dialect(DatabaseType::TiDb).is_none());
        assert!(security_dialect(DatabaseType::CockroachDb).is_none());
        assert!(security_dialect(DatabaseType::YugabyteDb).is_none());
    }
}
