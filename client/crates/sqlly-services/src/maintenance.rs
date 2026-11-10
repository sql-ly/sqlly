//! Per-engine database maintenance statements for the explorer's Maintenance
//! submenu: vacuum / analyze / reindex / update statistics / integrity checks
//! and index rebuilds, rendered with the engine's own vocabulary and quoting.
//!
//! Pure text generation. The one `(dialect, scope, op)` table in
//! [`MaintenanceOp::describe`] is the single source of truth for what each
//! engine offers at each node, what the menu calls it, and which operations
//! deserve a lock / long-running warning. `sqlly-gpui` builds the menu from
//! it and the confirmation dialog quotes the SQL it renders.

use crate::identifier_quoting::{qualified_identifier, quote_identifier};
use crate::tool_dialects::MaintenanceDialect;
use sqlly_core::DatabaseType;

/// Which explorer node the operation acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaintenanceScope {
    Database,
    Table,
    Index,
}

/// A maintenance verb, engine-neutral. Each dialect renders it in its own
/// vocabulary (`UpdateStatistics` is `UPDATE STATISTICS` on SQL Server,
/// `ANALYZE` on PostgreSQL and SQLite, `ANALYZE TABLE` on MySQL).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MaintenanceOp {
    /// Consistency check: `DBCC CHECKDB` / `DBCC CHECKTABLE`, MySQL `CHECK
    /// TABLE`, SQLite `PRAGMA integrity_check`.
    CheckIntegrity,
    /// Refresh optimizer statistics.
    UpdateStatistics,
    /// PostgreSQL / SQLite `VACUUM`.
    Vacuum,
    /// PostgreSQL `VACUUM ANALYZE`.
    VacuumAnalyze,
    /// PostgreSQL `VACUUM (FULL)` — rewrites the table under an exclusive lock.
    VacuumFull,
    /// Rebuild indexes: `ALTER INDEX … REBUILD`, `REINDEX`, per-table
    /// rebuilds for a whole SQL Server database.
    Reindex,
    /// SQL Server `ALTER INDEX … REORGANIZE` (online defragmentation).
    Reorganize,
    /// MySQL `OPTIMIZE TABLE`.
    Optimize,
    /// MySQL `REPAIR TABLE`.
    Repair,
    /// DuckDB `CHECKPOINT`: flush the write-ahead log into the database
    /// file, waiting for running transactions.
    Checkpoint,
    /// DuckDB `FORCE CHECKPOINT`: the same flush, aborting any transaction
    /// still running instead of waiting for it.
    ForceCheckpoint,
}

/// Every op, in the order the menu lists them for a given scope.
const ALL_OPS: [MaintenanceOp; 11] = [
    MaintenanceOp::Vacuum,
    MaintenanceOp::VacuumAnalyze,
    MaintenanceOp::UpdateStatistics,
    MaintenanceOp::CheckIntegrity,
    MaintenanceOp::Optimize,
    MaintenanceOp::Repair,
    MaintenanceOp::Reindex,
    MaintenanceOp::Reorganize,
    MaintenanceOp::VacuumFull,
    MaintenanceOp::Checkpoint,
    MaintenanceOp::ForceCheckpoint,
];

/// Menu wording and warning for one `(dialect, scope, op)` cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaintenanceText {
    /// The statement as the user knows it: `VACUUM ANALYZE`, `DBCC CHECKDB`.
    pub label: &'static str,
    /// Menu item that opens the statement in a new query tab.
    pub script_label: &'static str,
    /// Menu item that confirms and executes immediately.
    pub run_label: &'static str,
    /// Set when the operation takes heavy locks or can run for a long time;
    /// the confirmation dialog surfaces it before Run.
    pub warning: Option<&'static str>,
    /// Database-scoped ops that expand to one statement per table (the
    /// engine has no database-wide form); the caller must supply the table
    /// list from the catalog.
    pub expands_tables: bool,
}

impl MaintenanceText {
    pub fn warning_message(&self) -> Option<sqlly_core::DisplayMessage> {
        let warning = self.warning?;
        let key = match warning {
            WARN_LOCKS_LONG => "services.maintenance.warnings.locks_long",
            WARN_CHECKDB => "services.maintenance.warnings.checkdb",
            WARN_VACUUM_FULL => "services.maintenance.warnings.vacuum_full",
            WARN_SQLITE_VACUUM => "services.maintenance.warnings.sqlite_vacuum",
            WARN_OPTIMIZE => "services.maintenance.warnings.optimize",
            WARN_REINDEX_DB => "services.maintenance.warnings.reindex_database",
            WARN_OPTIMIZE_FINAL => "services.maintenance.warnings.optimize_final",
            WARN_ORACLE_VALIDATE => "services.maintenance.warnings.oracle_validate",
            WARN_ORACLE_SCHEMA_STATS => "services.maintenance.warnings.oracle_schema_stats",
            WARN_FORCE_CHECKPOINT => "services.maintenance.warnings.force_checkpoint",
            _ => return Some(sqlly_core::DisplayMessage::raw(warning)),
        };
        Some(sqlly_core::DisplayMessage::new(key, warning))
    }
}

const WARN_LOCKS_LONG: &str =
    "Takes locks on the objects it touches and can run for a long time on large tables.";
const WARN_CHECKDB: &str =
    "Reads the whole database and can run for a long time; it is I/O heavy on large databases.";
const WARN_VACUUM_FULL: &str = "Rewrites the whole table under an exclusive lock; nothing else can read or write it until it finishes.";
const WARN_SQLITE_VACUUM: &str =
    "Rewrites the whole database file and needs exclusive access until it finishes.";
const WARN_OPTIMIZE: &str =
    "Rebuilds the table's storage; the table is locked for the duration on most storage engines.";
const WARN_REINDEX_DB: &str =
    "Rebuilds every index in the database, locking each table in turn; it can run for a long time.";
const WARN_OPTIMIZE_FINAL: &str = "Merges every part of the table into one, rewriting all of its data; it is I/O heavy and can run for a long time on large tables.";
const WARN_ORACLE_VALIDATE: &str =
    "Reads every block of the table and its indexes and holds a shared lock while it runs.";
const WARN_ORACLE_SCHEMA_STATS: &str = "Gathers statistics for every table in the schema; it can run for a long time on large schemas.";
const WARN_FORCE_CHECKPOINT: &str = "Aborts any transaction still running in other connections to this database instead of waiting for it.";

impl MaintenanceOp {
    /// The wording and behavior for this op at a scope on a dialect, or
    /// `None` when the engine has no such operation there. This table IS the
    /// per-engine menu.
    pub fn describe(
        self,
        dialect: MaintenanceDialect,
        scope: MaintenanceScope,
    ) -> Option<MaintenanceText> {
        use MaintenanceDialect as D;
        use MaintenanceOp as O;
        use MaintenanceScope as S;
        let text = |label, script_label, run_label, warning, expands_tables| {
            Some(MaintenanceText {
                label,
                script_label,
                run_label,
                warning,
                expands_tables,
            })
        };
        match (dialect, scope, self) {
            // ── SQL Server / Azure SQL ────────────────────────────────────
            (D::SqlServer, S::Database, O::CheckIntegrity) => text(
                "DBCC CHECKDB",
                "Script DBCC CHECKDB",
                "Run DBCC CHECKDB…",
                Some(WARN_CHECKDB),
                false,
            ),
            (D::SqlServer, S::Database, O::UpdateStatistics) => text(
                "UPDATE STATISTICS (all tables)",
                "Script UPDATE STATISTICS (all tables)",
                "Run UPDATE STATISTICS (all tables)…",
                None,
                false,
            ),
            (D::SqlServer, S::Database, O::Reindex) => text(
                "Rebuild All Indexes",
                "Script Rebuild All Indexes",
                "Run Rebuild All Indexes…",
                Some(WARN_REINDEX_DB),
                true,
            ),
            (D::SqlServer, S::Table, O::UpdateStatistics) => text(
                "UPDATE STATISTICS",
                "Script UPDATE STATISTICS",
                "Run UPDATE STATISTICS…",
                None,
                false,
            ),
            (D::SqlServer, S::Table, O::CheckIntegrity) => text(
                "DBCC CHECKTABLE",
                "Script DBCC CHECKTABLE",
                "Run DBCC CHECKTABLE…",
                None,
                false,
            ),
            (D::SqlServer, S::Table, O::Reindex) => text(
                "ALTER INDEX ALL … REBUILD",
                "Script ALTER INDEX ALL … REBUILD",
                "Run ALTER INDEX ALL … REBUILD…",
                Some(WARN_LOCKS_LONG),
                false,
            ),
            (D::SqlServer, S::Table, O::Reorganize) => text(
                "ALTER INDEX ALL … REORGANIZE",
                "Script ALTER INDEX ALL … REORGANIZE",
                "Run ALTER INDEX ALL … REORGANIZE…",
                None,
                false,
            ),
            (D::SqlServer, S::Index, O::Reindex) => text(
                "ALTER INDEX … REBUILD",
                "Script ALTER INDEX … REBUILD",
                "Run ALTER INDEX … REBUILD…",
                Some(WARN_LOCKS_LONG),
                false,
            ),
            (D::SqlServer, S::Index, O::Reorganize) => text(
                "ALTER INDEX … REORGANIZE",
                "Script ALTER INDEX … REORGANIZE",
                "Run ALTER INDEX … REORGANIZE…",
                None,
                false,
            ),

            // ── PostgreSQL family ─────────────────────────────────────────
            (D::Postgres, S::Database, O::Vacuum) => {
                text("VACUUM", "Script VACUUM", "Run VACUUM…", None, false)
            }
            (D::Postgres, S::Database, O::VacuumAnalyze) => text(
                "VACUUM ANALYZE",
                "Script VACUUM ANALYZE",
                "Run VACUUM ANALYZE…",
                None,
                false,
            ),
            (D::Postgres, S::Database, O::UpdateStatistics) => {
                text("ANALYZE", "Script ANALYZE", "Run ANALYZE…", None, false)
            }
            (D::Postgres, S::Database, O::Reindex) => text(
                "REINDEX DATABASE",
                "Script REINDEX DATABASE",
                "Run REINDEX DATABASE…",
                Some(WARN_REINDEX_DB),
                false,
            ),
            (D::Postgres, S::Table, O::Vacuum) => {
                text("VACUUM", "Script VACUUM", "Run VACUUM…", None, false)
            }
            (D::Postgres, S::Table, O::VacuumAnalyze) => text(
                "VACUUM (ANALYZE)",
                "Script VACUUM (ANALYZE)",
                "Run VACUUM (ANALYZE)…",
                None,
                false,
            ),
            (D::Postgres, S::Table, O::UpdateStatistics) => {
                text("ANALYZE", "Script ANALYZE", "Run ANALYZE…", None, false)
            }
            (D::Postgres, S::Table, O::Reindex) => text(
                "REINDEX TABLE",
                "Script REINDEX TABLE",
                "Run REINDEX TABLE…",
                Some(WARN_LOCKS_LONG),
                false,
            ),
            (D::Postgres, S::Table, O::VacuumFull) => text(
                "VACUUM (FULL)",
                "Script VACUUM (FULL)",
                "Run VACUUM (FULL)…",
                Some(WARN_VACUUM_FULL),
                false,
            ),
            (D::Postgres, S::Index, O::Reindex) => text(
                "REINDEX INDEX",
                "Script REINDEX INDEX",
                "Run REINDEX INDEX…",
                Some(WARN_LOCKS_LONG),
                false,
            ),

            // ── MySQL / MariaDB ───────────────────────────────────────────
            (D::MySql, S::Database, O::UpdateStatistics) => text(
                "ANALYZE TABLE (all tables)",
                "Script ANALYZE TABLE (all tables)",
                "Run ANALYZE TABLE (all tables)…",
                None,
                true,
            ),
            (D::MySql, S::Database, O::Optimize) => text(
                "OPTIMIZE TABLE (all tables)",
                "Script OPTIMIZE TABLE (all tables)",
                "Run OPTIMIZE TABLE (all tables)…",
                Some(WARN_OPTIMIZE),
                true,
            ),
            (D::MySql, S::Database, O::CheckIntegrity) => text(
                "CHECK TABLE (all tables)",
                "Script CHECK TABLE (all tables)",
                "Run CHECK TABLE (all tables)…",
                None,
                true,
            ),
            (D::MySql, S::Table, O::UpdateStatistics) => text(
                "ANALYZE TABLE",
                "Script ANALYZE TABLE",
                "Run ANALYZE TABLE…",
                None,
                false,
            ),
            (D::MySql, S::Table, O::Optimize) => text(
                "OPTIMIZE TABLE",
                "Script OPTIMIZE TABLE",
                "Run OPTIMIZE TABLE…",
                Some(WARN_OPTIMIZE),
                false,
            ),
            (D::MySql, S::Table, O::CheckIntegrity) => text(
                "CHECK TABLE",
                "Script CHECK TABLE",
                "Run CHECK TABLE…",
                None,
                false,
            ),
            (D::MySql, S::Table, O::Repair) => text(
                "REPAIR TABLE",
                "Script REPAIR TABLE",
                "Run REPAIR TABLE…",
                None,
                false,
            ),

            // ── SQLite ────────────────────────────────────────────────────
            (D::Sqlite, S::Database, O::Vacuum) => text(
                "VACUUM",
                "Script VACUUM",
                "Run VACUUM…",
                Some(WARN_SQLITE_VACUUM),
                false,
            ),
            (D::Sqlite, S::Database, O::UpdateStatistics) => {
                text("ANALYZE", "Script ANALYZE", "Run ANALYZE…", None, false)
            }
            (D::Sqlite, S::Database, O::Reindex) => text(
                "REINDEX",
                "Script REINDEX",
                "Run REINDEX…",
                Some(WARN_REINDEX_DB),
                false,
            ),
            (D::Sqlite, S::Database, O::CheckIntegrity) => text(
                "PRAGMA integrity_check",
                "Script PRAGMA integrity_check",
                "Run PRAGMA integrity_check…",
                None,
                false,
            ),
            (D::Sqlite, S::Table, O::UpdateStatistics) => {
                text("ANALYZE", "Script ANALYZE", "Run ANALYZE…", None, false)
            }
            (D::Sqlite, S::Table, O::Reindex) => {
                text("REINDEX", "Script REINDEX", "Run REINDEX…", None, false)
            }
            (D::Sqlite, S::Index, O::Reindex) => {
                text("REINDEX", "Script REINDEX", "Run REINDEX…", None, false)
            }

            // ── libSQL / Turso ────────────────────────────────────────────
            // The server refuses VACUUM, ANALYZE and PRAGMA optimize on both
            // sqld and Turso Cloud; REINDEX and the integrity check remain.
            (D::LibSql, S::Database, O::Reindex) => text(
                "REINDEX",
                "Script REINDEX",
                "Run REINDEX…",
                Some(WARN_REINDEX_DB),
                false,
            ),
            (D::LibSql, S::Database, O::CheckIntegrity) => text(
                "PRAGMA integrity_check",
                "Script PRAGMA integrity_check",
                "Run PRAGMA integrity_check…",
                None,
                false,
            ),
            (D::LibSql, S::Table, O::Reindex) => {
                text("REINDEX", "Script REINDEX", "Run REINDEX…", None, false)
            }
            (D::LibSql, S::Index, O::Reindex) => {
                text("REINDEX", "Script REINDEX", "Run REINDEX…", None, false)
            }

            // ── ClickHouse ────────────────────────────────────────────────
            (D::ClickHouse, S::Database, O::Optimize) => text(
                "OPTIMIZE TABLE … FINAL (all tables)",
                "Script OPTIMIZE TABLE … FINAL (all tables)",
                "Run OPTIMIZE TABLE … FINAL (all tables)…",
                Some(WARN_OPTIMIZE_FINAL),
                true,
            ),
            (D::ClickHouse, S::Table, O::Optimize) => text(
                "OPTIMIZE TABLE … FINAL",
                "Script OPTIMIZE TABLE … FINAL",
                "Run OPTIMIZE TABLE … FINAL…",
                Some(WARN_OPTIMIZE_FINAL),
                false,
            ),
            // ── Oracle ────────────────────────────────────────────────────
            (D::Oracle, S::Database, O::UpdateStatistics) => text(
                "DBMS_STATS.GATHER_SCHEMA_STATS",
                "Script DBMS_STATS.GATHER_SCHEMA_STATS",
                "Run DBMS_STATS.GATHER_SCHEMA_STATS…",
                Some(WARN_ORACLE_SCHEMA_STATS),
                false,
            ),
            (D::Oracle, S::Table, O::UpdateStatistics) => text(
                "DBMS_STATS.GATHER_TABLE_STATS",
                "Script DBMS_STATS.GATHER_TABLE_STATS",
                "Run DBMS_STATS.GATHER_TABLE_STATS…",
                None,
                false,
            ),
            (D::Oracle, S::Table, O::CheckIntegrity) => text(
                "ANALYZE TABLE … VALIDATE STRUCTURE",
                "Script ANALYZE TABLE … VALIDATE STRUCTURE",
                "Run ANALYZE TABLE … VALIDATE STRUCTURE…",
                Some(WARN_ORACLE_VALIDATE),
                false,
            ),
            (D::Oracle, S::Table, O::Reindex) => text(
                "ALTER INDEX … REBUILD (all indexes)",
                "Script ALTER INDEX … REBUILD (all indexes)",
                "Run ALTER INDEX … REBUILD (all indexes)…",
                Some(WARN_LOCKS_LONG),
                false,
            ),
            (D::Oracle, S::Index, O::Reindex) => text(
                "ALTER INDEX … REBUILD",
                "Script ALTER INDEX … REBUILD",
                "Run ALTER INDEX … REBUILD…",
                Some(WARN_LOCKS_LONG),
                false,
            ),
            // ── DuckDB ────────────────────────────────────────────────────
            // VACUUM / VACUUM ANALYZE / ANALYZE refresh the optimizer's
            // statistics (DuckDB reclaims storage on its own); CHECKPOINT
            // folds the write-ahead log into the database file. No REINDEX
            // (indexes are rebuilt on load) and no integrity check exist.
            (D::DuckDb, S::Database, O::Vacuum) => {
                text("VACUUM", "Script VACUUM", "Run VACUUM…", None, false)
            }
            (D::DuckDb, S::Database, O::VacuumAnalyze) => text(
                "VACUUM ANALYZE",
                "Script VACUUM ANALYZE",
                "Run VACUUM ANALYZE…",
                None,
                false,
            ),
            (D::DuckDb, S::Database, O::UpdateStatistics) => {
                text("ANALYZE", "Script ANALYZE", "Run ANALYZE…", None, false)
            }
            (D::DuckDb, S::Database, O::Checkpoint) => text(
                "CHECKPOINT",
                "Script CHECKPOINT",
                "Run CHECKPOINT…",
                None,
                false,
            ),
            (D::DuckDb, S::Database, O::ForceCheckpoint) => text(
                "FORCE CHECKPOINT",
                "Script FORCE CHECKPOINT",
                "Run FORCE CHECKPOINT…",
                Some(WARN_FORCE_CHECKPOINT),
                false,
            ),
            (D::DuckDb, S::Table, O::VacuumAnalyze) => text(
                "VACUUM ANALYZE",
                "Script VACUUM ANALYZE",
                "Run VACUUM ANALYZE…",
                None,
                false,
            ),
            (D::DuckDb, S::Table, O::UpdateStatistics) => {
                text("ANALYZE", "Script ANALYZE", "Run ANALYZE…", None, false)
            }

            _ => None,
        }
    }
}

/// The ops a dialect offers at a scope, in menu order. Empty when the engine
/// has nothing for that node (MySQL indexes, for instance).
pub fn ops_for(dialect: MaintenanceDialect, scope: MaintenanceScope) -> Vec<MaintenanceOp> {
    ALL_OPS
        .into_iter()
        .filter(|op| op.describe(dialect, scope).is_some())
        .collect()
}

/// One table named by the catalog, for database-wide ops that expand to a
/// statement per table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRef {
    pub schema: Option<String>,
    pub name: String,
}

impl TableRef {
    pub fn new(schema: Option<&str>, name: &str) -> Self {
        Self {
            schema: schema.filter(|s| !s.is_empty()).map(str::to_string),
            name: name.to_string(),
        }
    }
}

/// The explorer node a maintenance op targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MaintenanceTarget {
    Database {
        name: String,
    },
    Table {
        schema: Option<String>,
        name: String,
    },
    Index {
        schema: Option<String>,
        table: String,
        name: String,
    },
}

impl MaintenanceTarget {
    pub fn scope(&self) -> MaintenanceScope {
        match self {
            Self::Database { .. } => MaintenanceScope::Database,
            Self::Table { .. } => MaintenanceScope::Table,
            Self::Index { .. } => MaintenanceScope::Index,
        }
    }

    /// Plain (unquoted) name for titles and status text: `orders`,
    /// `public.orders`, `public.orders.ix_orders_customer`.
    pub fn display_name(&self) -> String {
        let with_schema = |schema: &Option<String>, name: &str| match schema.as_deref() {
            Some(s) if !s.is_empty() => format!("{s}.{name}"),
            _ => name.to_string(),
        };
        match self {
            Self::Database { name } => name.clone(),
            Self::Table { schema, name } => with_schema(schema, name),
            Self::Index {
                schema,
                table,
                name,
            } => format!("{}.{name}", with_schema(schema, table)),
        }
    }
}

/// Why no statement could be rendered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MaintenanceError {
    /// The engine has no such operation at this scope.
    Unsupported,
    /// A per-table expansion was asked for but the catalog listed no tables.
    NoTables,
}

impl std::fmt::Display for MaintenanceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported => {
                write!(
                    f,
                    "This maintenance operation is not available on this engine"
                )
            }
            Self::NoTables => write!(f, "The database has no tables to run this on"),
        }
    }
}

impl std::error::Error for MaintenanceError {}

impl MaintenanceError {
    pub fn message(&self) -> sqlly_core::DisplayMessage {
        match self {
            Self::Unsupported => sqlly_core::DisplayMessage::new(
                "services.maintenance.errors.unsupported",
                self.to_string(),
            ),
            Self::NoTables => sqlly_core::DisplayMessage::new(
                "services.maintenance.errors.no_tables",
                self.to_string(),
            ),
        }
    }
}

/// Render the statement(s) for `op` on `target`, quoted for `db`. Ops that
/// expand per table (`describe(...).expands_tables`) read `tables`; every
/// other op ignores it. Statements are newline-separated and each ends in
/// `;`, ready for a query tab or the execution path as-is.
pub fn maintenance_sql(
    db: DatabaseType,
    op: MaintenanceOp,
    target: &MaintenanceTarget,
    tables: &[TableRef],
) -> Result<String, MaintenanceError> {
    use MaintenanceDialect as D;
    use MaintenanceOp as O;
    use MaintenanceTarget as T;

    let dialect =
        crate::tool_dialects::maintenance_dialect(db).ok_or(MaintenanceError::Unsupported)?;
    let text = op
        .describe(dialect, target.scope())
        .ok_or(MaintenanceError::Unsupported)?;

    let table_name = |schema: &Option<String>, name: &str| match dialect {
        // SQLite has one namespace worth naming; `main.t` is legal but noise.
        D::Sqlite | D::LibSql => quote_identifier(db, name),
        _ => qualified_identifier(db, schema.as_deref(), name),
    };
    let per_table = |render: &dyn Fn(String) -> String| -> Result<String, MaintenanceError> {
        if tables.is_empty() {
            return Err(MaintenanceError::NoTables);
        }
        Ok(tables
            .iter()
            .map(|t| render(table_name(&t.schema, &t.name)))
            .collect::<Vec<_>>()
            .join("\n"))
    };
    debug_assert_eq!(
        text.expands_tables,
        matches!(
            (dialect, target, op),
            (D::SqlServer, T::Database { .. }, O::Reindex)
                | (
                    D::MySql,
                    T::Database { .. },
                    O::UpdateStatistics | O::Optimize | O::CheckIntegrity
                )
                | (D::ClickHouse, T::Database { .. }, O::Optimize)
        ),
        "describe() and maintenance_sql() disagree on table expansion"
    );

    let sql = match (dialect, target, op) {
        // ── SQL Server ────────────────────────────────────────────────────
        (D::SqlServer, T::Database { name }, O::CheckIntegrity) => {
            format!(
                "DBCC CHECKDB ({}) WITH NO_INFOMSGS;",
                quote_identifier(db, name)
            )
        }
        (D::SqlServer, T::Database { .. }, O::UpdateStatistics) => {
            "EXEC sp_updatestats;".to_string()
        }
        (D::SqlServer, T::Database { .. }, O::Reindex) => {
            per_table(&|t| format!("ALTER INDEX ALL ON {t} REBUILD;"))?
        }
        (D::SqlServer, T::Table { schema, name }, O::UpdateStatistics) => {
            format!("UPDATE STATISTICS {};", table_name(schema, name))
        }
        (D::SqlServer, T::Table { schema, name }, O::CheckIntegrity) => format!(
            "DBCC CHECKTABLE ('{}') WITH NO_INFOMSGS;",
            table_name(schema, name).replace('\'', "''")
        ),
        (D::SqlServer, T::Table { schema, name }, O::Reindex) => {
            format!("ALTER INDEX ALL ON {} REBUILD;", table_name(schema, name))
        }
        (D::SqlServer, T::Table { schema, name }, O::Reorganize) => {
            format!(
                "ALTER INDEX ALL ON {} REORGANIZE;",
                table_name(schema, name)
            )
        }
        (
            D::SqlServer,
            T::Index {
                schema,
                table,
                name,
            },
            O::Reindex,
        ) => format!(
            "ALTER INDEX {} ON {} REBUILD;",
            quote_identifier(db, name),
            table_name(schema, table)
        ),
        (
            D::SqlServer,
            T::Index {
                schema,
                table,
                name,
            },
            O::Reorganize,
        ) => format!(
            "ALTER INDEX {} ON {} REORGANIZE;",
            quote_identifier(db, name),
            table_name(schema, table)
        ),

        // ── PostgreSQL ────────────────────────────────────────────────────
        (D::Postgres, T::Database { .. }, O::Vacuum) => "VACUUM;".to_string(),
        (D::Postgres, T::Database { .. }, O::VacuumAnalyze) => "VACUUM ANALYZE;".to_string(),
        (D::Postgres, T::Database { .. }, O::UpdateStatistics) => "ANALYZE;".to_string(),
        (D::Postgres, T::Database { name }, O::Reindex) => {
            format!("REINDEX DATABASE {};", quote_identifier(db, name))
        }
        (D::Postgres, T::Table { schema, name }, O::Vacuum) => {
            format!("VACUUM {};", table_name(schema, name))
        }
        (D::Postgres, T::Table { schema, name }, O::VacuumAnalyze) => {
            format!("VACUUM (ANALYZE) {};", table_name(schema, name))
        }
        (D::Postgres, T::Table { schema, name }, O::VacuumFull) => {
            format!("VACUUM (FULL) {};", table_name(schema, name))
        }
        (D::Postgres, T::Table { schema, name }, O::UpdateStatistics) => {
            format!("ANALYZE {};", table_name(schema, name))
        }
        (D::Postgres, T::Table { schema, name }, O::Reindex) => {
            format!("REINDEX TABLE {};", table_name(schema, name))
        }
        (D::Postgres, T::Index { schema, name, .. }, O::Reindex) => format!(
            "REINDEX INDEX {};",
            qualified_identifier(db, schema.as_deref(), name)
        ),

        // ── MySQL / MariaDB ───────────────────────────────────────────────
        (D::MySql, T::Database { .. }, O::UpdateStatistics) => {
            per_table(&|t| format!("ANALYZE TABLE {t};"))?
        }
        (D::MySql, T::Database { .. }, O::Optimize) => {
            per_table(&|t| format!("OPTIMIZE TABLE {t};"))?
        }
        (D::MySql, T::Database { .. }, O::CheckIntegrity) => {
            per_table(&|t| format!("CHECK TABLE {t};"))?
        }
        (D::MySql, T::Table { schema, name }, O::UpdateStatistics) => {
            format!("ANALYZE TABLE {};", table_name(schema, name))
        }
        (D::MySql, T::Table { schema, name }, O::Optimize) => {
            format!("OPTIMIZE TABLE {};", table_name(schema, name))
        }
        (D::MySql, T::Table { schema, name }, O::CheckIntegrity) => {
            format!("CHECK TABLE {};", table_name(schema, name))
        }
        (D::MySql, T::Table { schema, name }, O::Repair) => {
            format!("REPAIR TABLE {};", table_name(schema, name))
        }

        // ── SQLite ────────────────────────────────────────────────────────
        (D::Sqlite, T::Database { .. }, O::Vacuum) => "VACUUM;".to_string(),
        (D::Sqlite, T::Database { .. }, O::UpdateStatistics) => "ANALYZE;".to_string(),
        (D::Sqlite, T::Database { .. }, O::Reindex) => "REINDEX;".to_string(),
        (D::Sqlite, T::Database { .. }, O::CheckIntegrity) => "PRAGMA integrity_check;".to_string(),
        (D::Sqlite, T::Table { name, .. }, O::UpdateStatistics) => {
            format!("ANALYZE {};", quote_identifier(db, name))
        }
        (D::Sqlite, T::Table { name, .. }, O::Reindex) => {
            format!("REINDEX {};", quote_identifier(db, name))
        }
        (D::Sqlite, T::Index { name, .. }, O::Reindex) => {
            format!("REINDEX {};", quote_identifier(db, name))
        }

        // ── libSQL / Turso ────────────────────────────────────────────────
        (D::LibSql, T::Database { .. }, O::Reindex) => "REINDEX;".to_string(),
        (D::LibSql, T::Database { .. }, O::CheckIntegrity) => "PRAGMA integrity_check;".to_string(),
        (D::LibSql, T::Table { name, .. }, O::Reindex) => {
            format!("REINDEX {};", quote_identifier(db, name))
        }
        (D::LibSql, T::Index { name, .. }, O::Reindex) => {
            format!("REINDEX {};", quote_identifier(db, name))
        }

        // ── ClickHouse ────────────────────────────────────────────────────
        (D::ClickHouse, T::Database { .. }, O::Optimize) => {
            per_table(&|t| format!("OPTIMIZE TABLE {t} FINAL;"))?
        }
        (D::ClickHouse, T::Table { schema, name }, O::Optimize) => {
            format!("OPTIMIZE TABLE {} FINAL;", table_name(schema, name))
        }
        // ── Oracle ────────────────────────────────────────────────────────
        // DBMS_STATS takes the names as string literals, exactly as the
        // catalog spells them; the explorer's schema is the owner.
        (D::Oracle, T::Database { .. }, O::UpdateStatistics) => {
            "BEGIN DBMS_STATS.GATHER_SCHEMA_STATS(ownname => USER); END;".to_string()
        }
        (D::Oracle, T::Table { schema, name }, O::UpdateStatistics) => format!(
            "BEGIN DBMS_STATS.GATHER_TABLE_STATS(ownname => {}, tabname => '{}', cascade => TRUE); END;",
            oracle_owner_literal(schema.as_deref()),
            name.replace('\'', "''")
        ),
        (D::Oracle, T::Table { schema, name }, O::CheckIntegrity) => format!(
            "ANALYZE TABLE {} VALIDATE STRUCTURE CASCADE;",
            table_name(schema, name)
        ),
        // No table-wide rebuild statement exists; loop over the catalog.
        (D::Oracle, T::Table { schema, name }, O::Reindex) => format!(
            "BEGIN FOR i IN (SELECT owner, index_name FROM all_indexes WHERE table_owner = {} AND table_name = '{}' AND index_type <> 'LOB') LOOP EXECUTE IMMEDIATE 'ALTER INDEX \"' || i.owner || '\".\"' || i.index_name || '\" REBUILD'; END LOOP; END;",
            oracle_owner_literal(schema.as_deref()),
            name.replace('\'', "''")
        ),
        (D::Oracle, T::Index { schema, name, .. }, O::Reindex) => format!(
            "ALTER INDEX {} REBUILD;",
            qualified_identifier(db, schema.as_deref(), name)
        ),
        // ── DuckDB ────────────────────────────────────────────────────────
        (D::DuckDb, T::Database { .. }, O::Vacuum) => "VACUUM;".to_string(),
        (D::DuckDb, T::Database { .. }, O::VacuumAnalyze) => "VACUUM ANALYZE;".to_string(),
        (D::DuckDb, T::Database { .. }, O::UpdateStatistics) => "ANALYZE;".to_string(),
        (D::DuckDb, T::Database { .. }, O::Checkpoint) => "CHECKPOINT;".to_string(),
        (D::DuckDb, T::Database { .. }, O::ForceCheckpoint) => "FORCE CHECKPOINT;".to_string(),
        (D::DuckDb, T::Table { schema, name }, O::VacuumAnalyze) => {
            format!("VACUUM ANALYZE {};", table_name(schema, name))
        }
        (D::DuckDb, T::Table { schema, name }, O::UpdateStatistics) => {
            format!("ANALYZE {};", table_name(schema, name))
        }

        _ => return Err(MaintenanceError::Unsupported),
    };
    Ok(sql)
}

/// The owner argument for Oracle catalog calls: the explorer's schema as a
/// string literal, or the session user when the node carries none.
fn oracle_owner_literal(schema: Option<&str>) -> String {
    match schema.filter(|s| !s.is_empty()) {
        Some(s) => format!("'{}'", s.replace('\'', "''")),
        None => "USER".to_string(),
    }
}

/// The script-tab form: a one-line header naming the operation and target
/// above the statement(s), so the tab is self-describing when it is found
/// again later.
pub fn maintenance_script(label: &str, target: &MaintenanceTarget, sql: &str) -> String {
    format!("-- {label} on {}\n{sql}", target.display_name())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_dialects::maintenance_dialect;

    fn db_target(name: &str) -> MaintenanceTarget {
        MaintenanceTarget::Database {
            name: name.to_string(),
        }
    }

    fn table(schema: Option<&str>, name: &str) -> MaintenanceTarget {
        MaintenanceTarget::Table {
            schema: schema.map(str::to_string),
            name: name.to_string(),
        }
    }

    fn index(schema: Option<&str>, table: &str, name: &str) -> MaintenanceTarget {
        MaintenanceTarget::Index {
            schema: schema.map(str::to_string),
            table: table.to_string(),
            name: name.to_string(),
        }
    }

    fn sql(db: DatabaseType, op: MaintenanceOp, target: &MaintenanceTarget) -> String {
        maintenance_sql(db, op, target, &[]).unwrap()
    }

    #[test]
    fn dialect_maps_engine_families_and_refuses_the_rest() {
        use DatabaseType::*;
        assert_eq!(
            maintenance_dialect(SqlServer),
            Some(MaintenanceDialect::SqlServer)
        );
        assert_eq!(
            maintenance_dialect(AzureSqlDatabase),
            Some(MaintenanceDialect::SqlServer)
        );
        assert_eq!(
            maintenance_dialect(Postgres),
            Some(MaintenanceDialect::Postgres)
        );
        assert_eq!(
            maintenance_dialect(Neon),
            Some(MaintenanceDialect::Postgres)
        );
        assert_eq!(maintenance_dialect(MySql), Some(MaintenanceDialect::MySql));
        assert_eq!(
            maintenance_dialect(MariaDb),
            Some(MaintenanceDialect::MySql)
        );
        assert_eq!(
            maintenance_dialect(Sqlite),
            Some(MaintenanceDialect::Sqlite)
        );
        assert_eq!(
            maintenance_dialect(DuckDb),
            Some(MaintenanceDialect::DuckDb)
        );
        // Engines that lack the verbs (no VACUUM on CockroachDB, no OPTIMIZE
        // on TiDB) stay honest rather than offering a broken item.
        assert!(maintenance_dialect(CockroachDb).is_none());
        assert!(maintenance_dialect(TiDb).is_none());
        assert_eq!(
            maintenance_dialect(Oracle),
            Some(MaintenanceDialect::Oracle)
        );
        assert!(maintenance_dialect(Db2).is_none());
    }

    #[test]
    fn database_menu_per_dialect_matches_the_brief() {
        use MaintenanceOp::*;
        let labels = |d, s| -> Vec<&'static str> {
            ops_for(d, s)
                .into_iter()
                .map(|op| op.describe(d, s).unwrap().label)
                .collect()
        };
        assert_eq!(
            labels(MaintenanceDialect::SqlServer, MaintenanceScope::Database),
            vec![
                "UPDATE STATISTICS (all tables)",
                "DBCC CHECKDB",
                "Rebuild All Indexes"
            ]
        );
        assert_eq!(
            ops_for(MaintenanceDialect::Postgres, MaintenanceScope::Database),
            vec![Vacuum, VacuumAnalyze, UpdateStatistics, Reindex]
        );
        assert_eq!(
            labels(MaintenanceDialect::MySql, MaintenanceScope::Database),
            vec![
                "ANALYZE TABLE (all tables)",
                "CHECK TABLE (all tables)",
                "OPTIMIZE TABLE (all tables)"
            ]
        );
        assert_eq!(
            labels(MaintenanceDialect::Sqlite, MaintenanceScope::Database),
            vec!["VACUUM", "ANALYZE", "PRAGMA integrity_check", "REINDEX"]
        );
        assert_eq!(
            labels(MaintenanceDialect::DuckDb, MaintenanceScope::Database),
            vec![
                "VACUUM",
                "VACUUM ANALYZE",
                "ANALYZE",
                "CHECKPOINT",
                "FORCE CHECKPOINT"
            ]
        );
    }

    #[test]
    fn table_and_index_menus_per_dialect() {
        use MaintenanceOp::*;
        assert_eq!(
            ops_for(MaintenanceDialect::SqlServer, MaintenanceScope::Table),
            vec![UpdateStatistics, CheckIntegrity, Reindex, Reorganize]
        );
        assert_eq!(
            ops_for(MaintenanceDialect::Postgres, MaintenanceScope::Table),
            vec![Vacuum, VacuumAnalyze, UpdateStatistics, Reindex, VacuumFull]
        );
        assert_eq!(
            ops_for(MaintenanceDialect::MySql, MaintenanceScope::Table),
            vec![UpdateStatistics, CheckIntegrity, Optimize, Repair]
        );
        assert_eq!(
            ops_for(MaintenanceDialect::Sqlite, MaintenanceScope::Table),
            vec![UpdateStatistics, Reindex]
        );
        assert_eq!(
            ops_for(MaintenanceDialect::SqlServer, MaintenanceScope::Index),
            vec![Reindex, Reorganize]
        );
        assert_eq!(
            ops_for(MaintenanceDialect::Postgres, MaintenanceScope::Index),
            vec![Reindex]
        );
        assert!(ops_for(MaintenanceDialect::MySql, MaintenanceScope::Index).is_empty());
        assert_eq!(
            ops_for(MaintenanceDialect::Sqlite, MaintenanceScope::Index),
            vec![Reindex]
        );
        // DuckDB rebuilds indexes itself and has no integrity check, so only
        // the statistics verbs remain at table scope and nothing at index scope.
        assert_eq!(
            ops_for(MaintenanceDialect::DuckDb, MaintenanceScope::Table),
            vec![VacuumAnalyze, UpdateStatistics]
        );
        assert!(ops_for(MaintenanceDialect::DuckDb, MaintenanceScope::Index).is_empty());
    }

    #[test]
    fn locking_and_long_running_ops_carry_a_warning() {
        use MaintenanceDialect as D;
        use MaintenanceOp as O;
        use MaintenanceScope as S;
        let warns = |d, s, op: O| op.describe(d, s).unwrap().warning.is_some();
        assert!(warns(D::Postgres, S::Table, O::VacuumFull));
        assert!(warns(D::Postgres, S::Database, O::Reindex));
        assert!(warns(D::Postgres, S::Index, O::Reindex));
        assert!(warns(D::SqlServer, S::Database, O::CheckIntegrity));
        assert!(warns(D::SqlServer, S::Table, O::Reindex));
        assert!(warns(D::MySql, S::Table, O::Optimize));
        assert!(warns(D::MySql, S::Database, O::Optimize));
        assert!(warns(D::Sqlite, S::Database, O::Vacuum));
        assert!(warns(D::DuckDb, S::Database, O::ForceCheckpoint));
        assert!(!warns(D::DuckDb, S::Database, O::Checkpoint));
        assert!(!warns(D::DuckDb, S::Database, O::Vacuum));
        // Everyday statistics refreshes are cheap and unflagged.
        assert!(!warns(D::Postgres, S::Table, O::UpdateStatistics));
        assert!(!warns(D::SqlServer, S::Table, O::UpdateStatistics));
        assert!(!warns(D::MySql, S::Table, O::UpdateStatistics));
        assert!(!warns(D::SqlServer, S::Table, O::Reorganize));
    }

    #[test]
    fn menu_labels_follow_the_script_and_run_conventions() {
        let text = MaintenanceOp::Vacuum
            .describe(MaintenanceDialect::Postgres, MaintenanceScope::Table)
            .unwrap();
        assert_eq!(text.script_label, "Script VACUUM");
        assert_eq!(text.run_label, "Run VACUUM…");
        for d in [
            MaintenanceDialect::SqlServer,
            MaintenanceDialect::Postgres,
            MaintenanceDialect::MySql,
            MaintenanceDialect::Sqlite,
            MaintenanceDialect::Oracle,
            MaintenanceDialect::DuckDb,
        ] {
            for s in [
                MaintenanceScope::Database,
                MaintenanceScope::Table,
                MaintenanceScope::Index,
            ] {
                for op in ops_for(d, s) {
                    let t = op.describe(d, s).unwrap();
                    assert_eq!(
                        t.script_label,
                        format!("Script {}", t.label),
                        "{d:?} {s:?} {op:?}"
                    );
                    assert_eq!(
                        t.run_label,
                        format!("Run {}…", t.label),
                        "{d:?} {s:?} {op:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn sql_server_statements_are_bracket_quoted() {
        let db = DatabaseType::SqlServer;
        assert_eq!(
            sql(db, MaintenanceOp::CheckIntegrity, &db_target("Shop")),
            "DBCC CHECKDB ([Shop]) WITH NO_INFOMSGS;"
        );
        assert_eq!(
            sql(db, MaintenanceOp::UpdateStatistics, &db_target("Shop")),
            "EXEC sp_updatestats;"
        );
        let t = table(Some("dbo"), "Order Lines");
        assert_eq!(
            sql(db, MaintenanceOp::UpdateStatistics, &t),
            "UPDATE STATISTICS [dbo].[Order Lines];"
        );
        assert_eq!(
            sql(db, MaintenanceOp::CheckIntegrity, &t),
            "DBCC CHECKTABLE ('[dbo].[Order Lines]') WITH NO_INFOMSGS;"
        );
        assert_eq!(
            sql(db, MaintenanceOp::Reindex, &t),
            "ALTER INDEX ALL ON [dbo].[Order Lines] REBUILD;"
        );
        assert_eq!(
            sql(db, MaintenanceOp::Reorganize, &t),
            "ALTER INDEX ALL ON [dbo].[Order Lines] REORGANIZE;"
        );
        let ix = index(Some("dbo"), "Orders", "IX_Orders_Customer");
        assert_eq!(
            sql(db, MaintenanceOp::Reindex, &ix),
            "ALTER INDEX [IX_Orders_Customer] ON [dbo].[Orders] REBUILD;"
        );
        assert_eq!(
            sql(db, MaintenanceOp::Reorganize, &ix),
            "ALTER INDEX [IX_Orders_Customer] ON [dbo].[Orders] REORGANIZE;"
        );
    }

    #[test]
    fn sql_server_hostile_names_stay_quoted() {
        let db = DatabaseType::SqlServer;
        let t = table(Some("dbo"), "we]ird'name");
        assert_eq!(
            sql(db, MaintenanceOp::UpdateStatistics, &t),
            "UPDATE STATISTICS [dbo].[we]]ird'name];"
        );
        assert_eq!(
            sql(db, MaintenanceOp::CheckIntegrity, &t),
            "DBCC CHECKTABLE ('[dbo].[we]]ird''name]') WITH NO_INFOMSGS;"
        );
    }

    #[test]
    fn sql_server_rebuild_all_indexes_expands_per_table() {
        let tables = vec![
            TableRef::new(Some("dbo"), "Orders"),
            TableRef::new(Some("sales"), "Lines"),
        ];
        let sql = maintenance_sql(
            DatabaseType::SqlServer,
            MaintenanceOp::Reindex,
            &db_target("Shop"),
            &tables,
        )
        .unwrap();
        assert_eq!(
            sql,
            "ALTER INDEX ALL ON [dbo].[Orders] REBUILD;\nALTER INDEX ALL ON [sales].[Lines] REBUILD;"
        );
        assert_eq!(
            maintenance_sql(
                DatabaseType::SqlServer,
                MaintenanceOp::Reindex,
                &db_target("Shop"),
                &[]
            ),
            Err(MaintenanceError::NoTables)
        );
    }

    #[test]
    fn postgres_statements_are_double_quoted() {
        let db = DatabaseType::Postgres;
        assert_eq!(
            sql(db, MaintenanceOp::Vacuum, &db_target("shop")),
            "VACUUM;"
        );
        assert_eq!(
            sql(db, MaintenanceOp::VacuumAnalyze, &db_target("shop")),
            "VACUUM ANALYZE;"
        );
        assert_eq!(
            sql(db, MaintenanceOp::UpdateStatistics, &db_target("shop")),
            "ANALYZE;"
        );
        assert_eq!(
            sql(db, MaintenanceOp::Reindex, &db_target("Shop DB")),
            "REINDEX DATABASE \"Shop DB\";"
        );
        let t = table(Some("public"), "orders");
        assert_eq!(
            sql(db, MaintenanceOp::Vacuum, &t),
            "VACUUM \"public\".\"orders\";"
        );
        assert_eq!(
            sql(db, MaintenanceOp::VacuumAnalyze, &t),
            "VACUUM (ANALYZE) \"public\".\"orders\";"
        );
        assert_eq!(
            sql(db, MaintenanceOp::VacuumFull, &t),
            "VACUUM (FULL) \"public\".\"orders\";"
        );
        assert_eq!(
            sql(db, MaintenanceOp::UpdateStatistics, &t),
            "ANALYZE \"public\".\"orders\";"
        );
        assert_eq!(
            sql(db, MaintenanceOp::Reindex, &t),
            "REINDEX TABLE \"public\".\"orders\";"
        );
        // An index lives in its schema, not under its table, on PostgreSQL.
        assert_eq!(
            sql(
                db,
                MaintenanceOp::Reindex,
                &index(Some("public"), "orders", "ix_orders_customer")
            ),
            "REINDEX INDEX \"public\".\"ix_orders_customer\";"
        );
        // Embedded quotes double.
        assert_eq!(
            sql(db, MaintenanceOp::Vacuum, &table(None, "we\"ird")),
            "VACUUM \"we\"\"ird\";"
        );
    }

    #[test]
    fn mysql_statements_are_backtick_quoted_and_database_ops_expand() {
        let db = DatabaseType::MariaDb;
        let t = table(Some("shop"), "orders");
        assert_eq!(
            sql(db, MaintenanceOp::UpdateStatistics, &t),
            "ANALYZE TABLE `shop`.`orders`;"
        );
        assert_eq!(
            sql(db, MaintenanceOp::Optimize, &t),
            "OPTIMIZE TABLE `shop`.`orders`;"
        );
        assert_eq!(
            sql(db, MaintenanceOp::CheckIntegrity, &t),
            "CHECK TABLE `shop`.`orders`;"
        );
        assert_eq!(
            sql(db, MaintenanceOp::Repair, &t),
            "REPAIR TABLE `shop`.`orders`;"
        );

        let tables = vec![
            TableRef::new(Some("shop"), "orders"),
            TableRef::new(Some("shop"), "we`ird"),
        ];
        assert_eq!(
            maintenance_sql(
                db,
                MaintenanceOp::CheckIntegrity,
                &db_target("shop"),
                &tables
            )
            .unwrap(),
            "CHECK TABLE `shop`.`orders`;\nCHECK TABLE `shop`.`we``ird`;"
        );
        assert_eq!(
            maintenance_sql(db, MaintenanceOp::Optimize, &db_target("shop"), &tables).unwrap(),
            "OPTIMIZE TABLE `shop`.`orders`;\nOPTIMIZE TABLE `shop`.`we``ird`;"
        );
        assert_eq!(
            maintenance_sql(
                db,
                MaintenanceOp::UpdateStatistics,
                &db_target("shop"),
                &tables
            )
            .unwrap(),
            "ANALYZE TABLE `shop`.`orders`;\nANALYZE TABLE `shop`.`we``ird`;"
        );
        assert_eq!(
            maintenance_sql(db, MaintenanceOp::UpdateStatistics, &db_target("shop"), &[]),
            Err(MaintenanceError::NoTables)
        );
    }

    /// The libSQL server refuses VACUUM / ANALYZE / PRAGMA optimize, so its
    /// menu is REINDEX plus the integrity check, with SQLite's bare names.
    #[test]
    fn libsql_offers_reindex_and_integrity_check_only() {
        let db = DatabaseType::LibSql;
        assert_eq!(
            ops_for(MaintenanceDialect::LibSql, MaintenanceScope::Database),
            vec![MaintenanceOp::CheckIntegrity, MaintenanceOp::Reindex]
        );
        assert_eq!(
            ops_for(MaintenanceDialect::LibSql, MaintenanceScope::Table),
            vec![MaintenanceOp::Reindex]
        );
        assert_eq!(
            ops_for(MaintenanceDialect::LibSql, MaintenanceScope::Index),
            vec![MaintenanceOp::Reindex]
        );
        assert_eq!(
            sql(db, MaintenanceOp::Reindex, &db_target("shop")),
            "REINDEX;"
        );
        assert_eq!(
            sql(db, MaintenanceOp::CheckIntegrity, &db_target("shop")),
            "PRAGMA integrity_check;"
        );
        assert_eq!(
            sql(db, MaintenanceOp::Reindex, &table(Some("main"), "orders")),
            "REINDEX \"orders\";"
        );
        assert_eq!(
            sql(
                db,
                MaintenanceOp::Reindex,
                &index(Some("main"), "orders", "ix_orders")
            ),
            "REINDEX \"ix_orders\";"
        );
        assert_eq!(
            maintenance_sql(db, MaintenanceOp::Vacuum, &db_target("shop"), &[]),
            Err(MaintenanceError::Unsupported)
        );
        assert_eq!(
            maintenance_sql(
                db,
                MaintenanceOp::UpdateStatistics,
                &table(None, "orders"),
                &[]
            ),
            Err(MaintenanceError::Unsupported)
        );
    }

    #[test]
    fn sqlite_statements_use_bare_names() {
        let db = DatabaseType::Sqlite;
        assert_eq!(
            sql(db, MaintenanceOp::Vacuum, &db_target("app.db")),
            "VACUUM;"
        );
        assert_eq!(
            sql(db, MaintenanceOp::UpdateStatistics, &db_target("app.db")),
            "ANALYZE;"
        );
        assert_eq!(
            sql(db, MaintenanceOp::Reindex, &db_target("app.db")),
            "REINDEX;"
        );
        assert_eq!(
            sql(db, MaintenanceOp::CheckIntegrity, &db_target("app.db")),
            "PRAGMA integrity_check;"
        );
        let t = table(Some("main"), "orders");
        assert_eq!(
            sql(db, MaintenanceOp::UpdateStatistics, &t),
            "ANALYZE \"orders\";"
        );
        assert_eq!(sql(db, MaintenanceOp::Reindex, &t), "REINDEX \"orders\";");
        assert_eq!(
            sql(
                db,
                MaintenanceOp::Reindex,
                &index(Some("main"), "orders", "ix_orders")
            ),
            "REINDEX \"ix_orders\";"
        );
    }

    #[test]
    fn duckdb_statements_are_double_quoted_and_schema_qualified() {
        let db = DatabaseType::DuckDb;
        assert_eq!(
            sql(db, MaintenanceOp::Vacuum, &db_target("shop")),
            "VACUUM;"
        );
        assert_eq!(
            sql(db, MaintenanceOp::VacuumAnalyze, &db_target("shop")),
            "VACUUM ANALYZE;"
        );
        assert_eq!(
            sql(db, MaintenanceOp::UpdateStatistics, &db_target("shop")),
            "ANALYZE;"
        );
        assert_eq!(
            sql(db, MaintenanceOp::Checkpoint, &db_target("shop")),
            "CHECKPOINT;"
        );
        assert_eq!(
            sql(db, MaintenanceOp::ForceCheckpoint, &db_target("shop")),
            "FORCE CHECKPOINT;"
        );
        let t = table(Some("main"), "we\"ird");
        assert_eq!(
            sql(db, MaintenanceOp::VacuumAnalyze, &t),
            "VACUUM ANALYZE \"main\".\"we\"\"ird\";"
        );
        assert_eq!(
            sql(db, MaintenanceOp::UpdateStatistics, &table(None, "orders")),
            "ANALYZE \"orders\";"
        );
        // Nothing DuckDB cannot do is offered: no REINDEX, no integrity
        // check, no per-table VACUUM FULL, and checkpoints are database-wide.
        for op in [
            MaintenanceOp::Reindex,
            MaintenanceOp::CheckIntegrity,
            MaintenanceOp::VacuumFull,
            MaintenanceOp::Checkpoint,
        ] {
            assert_eq!(
                maintenance_sql(db, op, &t, &[]),
                Err(MaintenanceError::Unsupported),
                "{op:?}"
            );
        }
        assert_eq!(
            maintenance_sql(
                db,
                MaintenanceOp::UpdateStatistics,
                &index(Some("main"), "orders", "ix"),
                &[]
            ),
            Err(MaintenanceError::Unsupported)
        );
        // CHECKPOINT is DuckDB's alone.
        assert_eq!(
            maintenance_sql(
                DatabaseType::Postgres,
                MaintenanceOp::Checkpoint,
                &db_target("shop"),
                &[]
            ),
            Err(MaintenanceError::Unsupported)
        );
    }

    #[test]
    fn unsupported_combinations_are_refused_not_guessed() {
        let t = table(Some("public"), "orders");
        assert_eq!(
            maintenance_sql(DatabaseType::Postgres, MaintenanceOp::Optimize, &t, &[]),
            Err(MaintenanceError::Unsupported)
        );
        assert_eq!(
            maintenance_sql(DatabaseType::SqlServer, MaintenanceOp::Vacuum, &t, &[]),
            Err(MaintenanceError::Unsupported)
        );
        assert_eq!(
            maintenance_sql(
                DatabaseType::MySql,
                MaintenanceOp::Reindex,
                &index(Some("shop"), "orders", "ix"),
                &[]
            ),
            Err(MaintenanceError::Unsupported)
        );
        assert_eq!(
            maintenance_sql(DatabaseType::Db2, MaintenanceOp::UpdateStatistics, &t, &[]),
            Err(MaintenanceError::Unsupported)
        );
        assert_eq!(
            maintenance_sql(DatabaseType::Oracle, MaintenanceOp::Vacuum, &t, &[]),
            Err(MaintenanceError::Unsupported)
        );
    }

    /// Every op the menu offers renders SQL — no dangling menu item.
    #[test]
    fn every_offered_op_renders_sql() {
        let cases = [
            (DatabaseType::SqlServer, MaintenanceDialect::SqlServer),
            (DatabaseType::Postgres, MaintenanceDialect::Postgres),
            (DatabaseType::MySql, MaintenanceDialect::MySql),
            (DatabaseType::Sqlite, MaintenanceDialect::Sqlite),
            (DatabaseType::LibSql, MaintenanceDialect::LibSql),
            (DatabaseType::ClickHouse, MaintenanceDialect::ClickHouse),
            (DatabaseType::Oracle, MaintenanceDialect::Oracle),
            (DatabaseType::DuckDb, MaintenanceDialect::DuckDb),
        ];
        let tables = vec![TableRef::new(Some("s"), "t")];
        for (db, dialect) in cases {
            for target in [
                db_target("db"),
                table(Some("s"), "t"),
                index(Some("s"), "t", "ix"),
            ] {
                for op in ops_for(dialect, target.scope()) {
                    let sql = maintenance_sql(db, op, &target, &tables)
                        .unwrap_or_else(|e| panic!("{db:?} {op:?} {target:?}: {e}"));
                    assert!(sql.trim_end().ends_with(';'), "{db:?} {op:?}: {sql}");
                }
            }
        }
    }

    #[test]
    fn script_form_leads_with_a_header_naming_the_target() {
        let t = index(Some("public"), "orders", "ix_orders_customer");
        assert_eq!(t.display_name(), "public.orders.ix_orders_customer");
        assert_eq!(
            maintenance_script(
                "REINDEX INDEX",
                &t,
                "REINDEX INDEX \"public\".\"ix_orders_customer\";"
            ),
            "-- REINDEX INDEX on public.orders.ix_orders_customer\nREINDEX INDEX \"public\".\"ix_orders_customer\";"
        );
        assert_eq!(table(None, "orders").display_name(), "orders");
        assert_eq!(db_target("shop").display_name(), "shop");
    }

    #[test]
    fn oracle_renders_dbms_stats_rebuilds_and_validate_structure() {
        use MaintenanceOp::*;
        assert_eq!(
            ops_for(MaintenanceDialect::Oracle, MaintenanceScope::Database),
            vec![UpdateStatistics]
        );
        assert_eq!(
            ops_for(MaintenanceDialect::Oracle, MaintenanceScope::Table),
            vec![UpdateStatistics, CheckIntegrity, Reindex]
        );
        assert_eq!(
            ops_for(MaintenanceDialect::Oracle, MaintenanceScope::Index),
            vec![Reindex]
        );
        assert_eq!(
            sql(
                DatabaseType::Oracle,
                UpdateStatistics,
                &db_target("FREEPDB1")
            ),
            "BEGIN DBMS_STATS.GATHER_SCHEMA_STATS(ownname => USER); END;"
        );
        assert_eq!(
            sql(
                DatabaseType::Oracle,
                UpdateStatistics,
                &table(Some("HR"), "EMP'S")
            ),
            "BEGIN DBMS_STATS.GATHER_TABLE_STATS(ownname => 'HR', tabname => 'EMP''S', cascade => TRUE); END;"
        );
        assert_eq!(
            sql(DatabaseType::Oracle, UpdateStatistics, &table(None, "EMP")),
            "BEGIN DBMS_STATS.GATHER_TABLE_STATS(ownname => USER, tabname => 'EMP', cascade => TRUE); END;"
        );
        assert_eq!(
            sql(
                DatabaseType::Oracle,
                CheckIntegrity,
                &table(Some("HR"), "EMP")
            ),
            "ANALYZE TABLE \"HR\".\"EMP\" VALIDATE STRUCTURE CASCADE;"
        );
        let rebuild_all = sql(DatabaseType::Oracle, Reindex, &table(Some("HR"), "EMP"));
        assert!(rebuild_all.starts_with("BEGIN FOR i IN (SELECT owner, index_name FROM all_indexes WHERE table_owner = 'HR' AND table_name = 'EMP'"), "{rebuild_all}");
        assert!(rebuild_all.contains("REBUILD"), "{rebuild_all}");
        assert!(rebuild_all.ends_with("END;"), "{rebuild_all}");
        assert_eq!(
            sql(
                DatabaseType::Oracle,
                Reindex,
                &index(Some("HR"), "EMP", "EMP_IX")
            ),
            "ALTER INDEX \"HR\".\"EMP_IX\" REBUILD;"
        );
        assert!(
            UpdateStatistics
                .describe(MaintenanceDialect::Oracle, MaintenanceScope::Database)
                .unwrap()
                .warning
                .is_some()
        );
    }
}
