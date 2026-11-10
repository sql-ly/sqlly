//! Database engine identity and capability metadata.
//!
//! `Dialect` (lib.rs) is the *parser family* axis — the five grammars the
//! tokenizer/parser understand. `DatabaseType` is the *engine* axis: the
//! concrete product a connection talks to. Twenty engines map onto the five
//! families, and each engine carries its own capability profile: which
//! operations, keywords, and features it supports, how it quotes identifiers,
//! how it limits row counts, and how logical types map onto native storage.
//!
//! Everything here is pure data (serde-serializable, no I/O) so the whole
//! profile can be reported down-stack as part of the data model and later
//! consumed by the UI to enable/disable/rewrite actions per connection.
//! The flagship pattern equivalency is row limiting: "view top 1000" renders
//! `SELECT TOP 1000 …` on SQL Server, `… LIMIT 1000` on SQLite/MySQL,
//! `… FETCH FIRST 1000 ROWS ONLY` on Oracle/Db2, and `SELECT FIRST 1000 …`
//! on Informix — same intent, per-engine SQL.

use crate::sqlmodel::Dialect;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// DatabaseType
// ---------------------------------------------------------------------------

/// The concrete database engine behind a connection. The reference set the
/// product aims to understand; adding an engine means filling in one row of
/// each capability matrix below.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
pub enum DatabaseType {
    #[default]
    SqlServer,
    AzureSqlDatabase,
    Postgres,
    MySql,
    MariaDb,
    Sqlite,
    /// libSQL: SQLite's dialect served over the network by `sqld` or Turso
    /// Cloud (Hrana over HTTP).
    LibSql,
    Oracle,
    Db2,
    Access,
    /// Amazon Aurora. Ships MySQL- and PostgreSQL-compatible editions; we
    /// default to the MySQL wire profile — use `Postgres` for the pg edition.
    AmazonAurora,
    /// Amazon RDS is an engine host, not an engine; profiled here as its
    /// MySQL-compatible default. Connections should prefer the concrete
    /// engine variant when it is known.
    AmazonRds,
    Snowflake,
    CockroachDb,
    /// Google Cloud Spanner (GoogleSQL interface).
    Spanner,
    YugabyteDb,
    /// Amazon Aurora PostgreSQL-compatible edition (PostgreSQL wire).
    AmazonAuroraPostgres,
    /// Amazon RDS for PostgreSQL (PostgreSQL wire).
    AmazonRdsPostgres,
    /// Azure Database for PostgreSQL flexible server (PostgreSQL wire, TLS required).
    AzurePostgresFlexible,
    /// Azure Database for MySQL flexible server (MySQL wire, TLS required).
    AzureMySqlFlexible,
    /// Google Cloud SQL for PostgreSQL.
    CloudSqlPostgres,
    /// Google Cloud SQL for MySQL.
    CloudSqlMySql,
    /// Neon serverless PostgreSQL (TLS required).
    Neon,
    /// Supabase-hosted PostgreSQL.
    Supabase,
    /// PlanetScale (Vitess) MySQL-compatible (TLS required).
    PlanetScale,
    /// TiDB, MySQL-compatible distributed SQL (port 4000, no stored procedures).
    TiDb,
    Teradata,
    DuckDb,
    Informix,
    /// Databricks SQL (Spark SQL surface).
    Databricks,
    SapHana,
    /// ClickHouse, spoken over its HTTP interface.
    ClickHouse,
    /// Redis, the key-value store, spoken over RESP. Not SQL: the editor
    /// runs one command per line and the explorer is a key browser.
    Redis,
    /// Valkey, the Linux Foundation fork of Redis (RESP, Redis-compatible).
    Valkey,
    /// KeyDB, a multithreaded Redis fork (RESP, Redis-compatible).
    KeyDb,
    /// DragonflyDB, a Redis-compatible in-memory store (RESP).
    DragonflyDb,
    /// Amazon ElastiCache for Redis / Valkey (RESP, TLS by default).
    AmazonElastiCache,
    /// Amazon MemoryDB (RESP, TLS required).
    AmazonMemoryDb,
}

impl DatabaseType {
    pub const ALL: [DatabaseType; 38] = [
        DatabaseType::SqlServer,
        DatabaseType::AzureSqlDatabase,
        DatabaseType::Postgres,
        DatabaseType::MySql,
        DatabaseType::MariaDb,
        DatabaseType::Sqlite,
        DatabaseType::LibSql,
        DatabaseType::Oracle,
        DatabaseType::Db2,
        DatabaseType::Access,
        DatabaseType::AmazonAurora,
        DatabaseType::AmazonRds,
        DatabaseType::Snowflake,
        DatabaseType::CockroachDb,
        DatabaseType::Spanner,
        DatabaseType::YugabyteDb,
        DatabaseType::AmazonAuroraPostgres,
        DatabaseType::AmazonRdsPostgres,
        DatabaseType::AzurePostgresFlexible,
        DatabaseType::AzureMySqlFlexible,
        DatabaseType::CloudSqlPostgres,
        DatabaseType::CloudSqlMySql,
        DatabaseType::Neon,
        DatabaseType::Supabase,
        DatabaseType::PlanetScale,
        DatabaseType::TiDb,
        DatabaseType::Teradata,
        DatabaseType::DuckDb,
        DatabaseType::Informix,
        DatabaseType::Databricks,
        DatabaseType::SapHana,
        DatabaseType::ClickHouse,
        DatabaseType::Redis,
        DatabaseType::Valkey,
        DatabaseType::KeyDb,
        DatabaseType::DragonflyDb,
        DatabaseType::AmazonElastiCache,
        DatabaseType::AmazonMemoryDb,
    ];

    pub fn display_name(self) -> &'static str {
        use DatabaseType::*;
        match self {
            SqlServer => "Microsoft SQL Server",
            AzureSqlDatabase => "Azure SQL Database",
            Postgres => "PostgreSQL",
            MySql => "MySQL",
            MariaDb => "MariaDB",
            Sqlite => "SQLite",
            LibSql => "libSQL / Turso",
            Oracle => "Oracle Database",
            Db2 => "IBM Db2",
            Access => "Microsoft Access",
            AmazonAurora => "Amazon Aurora MySQL",
            AmazonRds => "Amazon RDS for MySQL",
            Snowflake => "Snowflake",
            CockroachDb => "CockroachDB",
            Spanner => "Google Cloud Spanner",
            YugabyteDb => "YugabyteDB",
            AmazonAuroraPostgres => "Amazon Aurora PostgreSQL",
            AmazonRdsPostgres => "Amazon RDS for PostgreSQL",
            AzurePostgresFlexible => "Azure Database for PostgreSQL",
            AzureMySqlFlexible => "Azure Database for MySQL",
            CloudSqlPostgres => "Google Cloud SQL for PostgreSQL",
            CloudSqlMySql => "Google Cloud SQL for MySQL",
            Neon => "Neon",
            Supabase => "Supabase",
            PlanetScale => "PlanetScale",
            TiDb => "TiDB",
            Teradata => "Teradata",
            DuckDb => "DuckDB",
            Informix => "IBM Informix",
            Databricks => "Databricks SQL",
            SapHana => "SAP HANA",
            ClickHouse => "ClickHouse",
            Redis => "Redis",
            Valkey => "Valkey",
            KeyDb => "KeyDB",
            DragonflyDb => "DragonflyDB",
            AmazonElastiCache => "Amazon ElastiCache",
            AmazonMemoryDb => "Amazon MemoryDB",
        }
    }

    /// Nearest parser family. Engines the parser has no dedicated grammar for
    /// borrow the closest one; capability lookups below never go through the
    /// family, so a borrowed grammar does not leak wrong capabilities.
    pub fn family(self) -> Dialect {
        use DatabaseType::*;
        match self {
            SqlServer | AzureSqlDatabase | Access => Dialect::SqlServer,
            Postgres
            | CockroachDb
            | YugabyteDb
            | AmazonAuroraPostgres
            | AmazonRdsPostgres
            | AzurePostgresFlexible
            | CloudSqlPostgres
            | Neon
            | Supabase
            | DuckDb
            | Snowflake
            | Spanner
            | Databricks => Dialect::Postgres,
            // ClickHouse borrows the MySQL grammar: backtick identifiers, the
            // database is the namespace level (no catalog above it), and
            // LIMIT/OFFSET row limiting. Its identifiers are case-sensitive,
            // which no family models exactly; MySQL's case-insensitive fold is
            // the forgiving choice for completion lookups.
            MySql | MariaDb | AmazonAurora | AmazonRds | AzureMySqlFlexible | CloudSqlMySql
            | PlanetScale | TiDb | ClickHouse => Dialect::MySql,
            Oracle | Db2 | Teradata | Informix | SapHana => Dialect::Oracle,
            // libSQL is SQLite's own grammar served over the network.
            Sqlite | LibSql => Dialect::Sqlite,
            // Redis is not SQL. The editor, IntelliSense, validation and the
            // formatter are switched off for it (`ConnectionKind::Resp`);
            // the MySQL grammar is the harmless choice for highlighting
            // because it shares Redis's `#` line comments and double-quoted
            // strings.
            Redis | Valkey | KeyDb | DragonflyDb | AmazonElastiCache | AmazonMemoryDb => {
                Dialect::MySql
            }
        }
    }

    /// True for engines whose "queries" are not SQL at all (Redis and its
    /// protocol relatives): no parser, IntelliSense, validation, formatting,
    /// or statement splitting applies; the editor runs one command per line.
    pub fn is_command_engine(self) -> bool {
        self.connection_kind() == ConnectionKind::Resp
    }

    /// How the engine quotes identifiers that need quoting.
    pub fn identifier_quoting(self) -> IdentifierQuoting {
        use DatabaseType::*;
        match self {
            SqlServer | AzureSqlDatabase | Access => IdentifierQuoting::Brackets,
            MySql | MariaDb | AmazonAurora | AmazonRds | AzureMySqlFlexible | CloudSqlMySql
            | PlanetScale | TiDb | Spanner | Databricks | ClickHouse => {
                IdentifierQuoting::Backticks
            }
            _ => IdentifierQuoting::DoubleQuotes,
        }
    }

    /// Quote one identifier part with the engine's quoting style, doubling
    /// embedded closers so hostile names stay injection-safe. ClickHouse and
    /// Spanner also read backslash escapes inside backquoted identifiers, so
    /// their backslashes are escaped too — otherwise a name ending in `\`
    /// would swallow the closing backtick.
    pub fn quote_identifier(self, identifier: &str) -> String {
        match self {
            DatabaseType::ClickHouse => {
                format!("`{}`", identifier.replace('\\', "\\\\").replace('`', "``"))
            }
            // GoogleSQL has no doubled-backtick form: backslash-escape both.
            DatabaseType::Spanner => {
                format!("`{}`", identifier.replace('\\', "\\\\").replace('`', "\\`"))
            }
            _ => self.identifier_quoting().quote(identifier),
        }
    }

    /// The wire protocol / connection mechanism the engine speaks — the axis
    /// driver crates are organized around. Engines sharing a kind share a
    /// driver: CockroachDB and YugabyteDB speak the PostgreSQL wire protocol,
    /// MariaDB and Aurora speak MySQL's.
    pub fn connection_kind(self) -> ConnectionKind {
        use DatabaseType::*;
        match self {
            SqlServer | AzureSqlDatabase => ConnectionKind::Tds,
            Postgres
            | CockroachDb
            | YugabyteDb
            | AmazonAuroraPostgres
            | AmazonRdsPostgres
            | AzurePostgresFlexible
            | CloudSqlPostgres
            | Neon
            | Supabase => ConnectionKind::PostgresWire,
            MySql | MariaDb | AmazonAurora | AmazonRds | AzureMySqlFlexible | CloudSqlMySql
            | PlanetScale | TiDb => ConnectionKind::MySqlWire,
            Sqlite => ConnectionKind::SqliteFile,
            LibSql => ConnectionKind::HranaHttp,
            DuckDb => ConnectionKind::DuckDbFile,
            Oracle => ConnectionKind::OracleNet,
            Db2 | Informix => ConnectionKind::Drda,
            Access => ConnectionKind::Odbc,
            Snowflake | Databricks | Spanner => ConnectionKind::HttpApi,
            Teradata | SapHana => ConnectionKind::Proprietary,
            ClickHouse => ConnectionKind::ClickHouseHttp,
            Redis | Valkey | KeyDb | DragonflyDb | AmazonElastiCache | AmazonMemoryDb => {
                ConnectionKind::Resp
            }
        }
    }

    /// Whether a native driver crate exists for this engine's connection kind.
    pub fn has_native_driver(self) -> bool {
        matches!(
            self.connection_kind(),
            ConnectionKind::Tds
                | ConnectionKind::PostgresWire
                | ConnectionKind::MySqlWire
                | ConnectionKind::SqliteFile
                | ConnectionKind::HranaHttp
                | ConnectionKind::DuckDbFile
                | ConnectionKind::ClickHouseHttp
                | ConnectionKind::OracleNet
                | ConnectionKind::Resp
        )
    }

    /// Engines whose hosted service rejects plaintext connections. The
    /// connection editor keeps `encrypt` on for these.
    pub fn requires_tls(self) -> bool {
        matches!(
            self,
            DatabaseType::AzurePostgresFlexible
                | DatabaseType::AzureMySqlFlexible
                | DatabaseType::Neon
                | DatabaseType::PlanetScale
                | DatabaseType::AmazonMemoryDb
        )
    }

    /// The upstream engine a wire-compatible label stands in for, or `None`
    /// for first-party engines. Drives the picker grouping and honesty text.
    pub fn compatible_with(self) -> Option<DatabaseType> {
        use DatabaseType::*;
        match self {
            CockroachDb
            | YugabyteDb
            | AmazonAuroraPostgres
            | AmazonRdsPostgres
            | AzurePostgresFlexible
            | CloudSqlPostgres
            | Neon
            | Supabase => Some(Postgres),
            AmazonAurora | AmazonRds | AzureMySqlFlexible | CloudSqlMySql | PlanetScale | TiDb => {
                Some(MySql)
            }
            Valkey | KeyDb | DragonflyDb | AmazonElastiCache | AmazonMemoryDb => Some(Redis),
            _ => None,
        }
    }

    /// The engine's preferred row-limiting pattern (the equivalency behind
    /// "view top N rows" working everywhere).
    pub fn row_limit_syntax(self) -> RowLimitSyntax {
        use DatabaseType::*;
        match self {
            SqlServer | AzureSqlDatabase | Access | Teradata => RowLimitSyntax::Top,
            Oracle | Db2 => RowLimitSyntax::FetchFirst,
            Informix => RowLimitSyntax::First,
            _ => RowLimitSyntax::Limit,
        }
    }

    /// Schema to assume when an object reference has none.
    pub fn default_schema(self) -> Option<&'static str> {
        use DatabaseType::*;
        match self {
            SqlServer | AzureSqlDatabase => Some("dbo"),
            Postgres
            | CockroachDb
            | YugabyteDb
            | AmazonAuroraPostgres
            | AmazonRdsPostgres
            | AzurePostgresFlexible
            | CloudSqlPostgres
            | Neon
            | Supabase
            | Snowflake => Some("public"),
            // DuckDB's (and SQLite's) default schema is `main`.
            DuckDb => Some("main"),
            // libSQL serves one SQLite database; its schema is `main`.
            LibSql => Some("main"),
            _ => None,
        }
    }

    /// Render `SELECT <projection> FROM <from>` limited to `n` rows using the
    /// engine's row-limit pattern. `from` must already be quoted/qualified.
    pub fn render_limited_select(self, projection: &str, from: &str, n: usize) -> String {
        let n = n.max(1);
        match self.row_limit_syntax() {
            RowLimitSyntax::Top => format!("SELECT TOP {n} {projection}\nFROM {from}"),
            RowLimitSyntax::First => format!("SELECT FIRST {n} {projection}\nFROM {from}"),
            RowLimitSyntax::Limit => format!("SELECT {projection}\nFROM {from}\nLIMIT {n}"),
            RowLimitSyntax::FetchFirst => {
                format!("SELECT {projection}\nFROM {from}\nFETCH FIRST {n} ROWS ONLY")
            }
        }
    }

    /// Whether a query-shape operation is available on this engine.
    pub fn supports_operation(self, op: SqlOperation) -> bool {
        use DatabaseType::*;
        use SqlOperation::*;
        // libSQL runs SQLite's SQL surface unchanged; every query-shape
        // answer is SQLite's.
        if self == LibSql {
            return Sqlite.supports_operation(op);
        }
        match op {
            // The relational baseline every reference engine shares.
            WhereIn | WhereNotIn | WhereBetween | WhereLike | WhereExists | GroupBy | Having
            | OrderBy | Union | InnerJoin | LeftOuterJoin => true,
            Cte => !matches!(self, Access),
            RecursiveCte => !matches!(self, Access | Informix | Databricks | SapHana | ClickHouse),
            WindowFunctions => !matches!(self, Access),
            WhereIlike => matches!(
                self,
                Postgres
                    | CockroachDb
                    | YugabyteDb
                    | AmazonAuroraPostgres
                    | AmazonRdsPostgres
                    | AzurePostgresFlexible
                    | CloudSqlPostgres
                    | Neon
                    | Supabase
                    | DuckDb
                    | Snowflake
                    | ClickHouse
            ),
            FullOuterJoin => !matches!(
                self,
                MySql
                    | MariaDb
                    | AmazonAurora
                    | AmazonRds
                    | AzureMySqlFlexible
                    | CloudSqlMySql
                    | PlanetScale
                    | TiDb
                    | Access
                    | Informix
            ),
            Intersect | Except => !matches!(self, Access),
            Merge => matches!(
                self,
                SqlServer
                    | AzureSqlDatabase
                    | Oracle
                    | Db2
                    | Teradata
                    | SapHana
                    | Snowflake
                    | Databricks
                    | Postgres
                    | CockroachDb
                    | YugabyteDb
                    | AmazonAuroraPostgres
                    | AmazonRdsPostgres
                    | AzurePostgresFlexible
                    | CloudSqlPostgres
                    | Neon
                    | Supabase
                    | Informix
            ),
            // Native single-statement upsert (ON CONFLICT / ON DUPLICATE KEY /
            // REPLACE). Engines without it fall back to MERGE when they have
            // that instead — see `upsert_pattern`.
            Upsert => matches!(
                self,
                Postgres
                    | CockroachDb
                    | YugabyteDb
                    | AmazonAuroraPostgres
                    | AmazonRdsPostgres
                    | AzurePostgresFlexible
                    | CloudSqlPostgres
                    | Neon
                    | Supabase
                    | Sqlite
                    | DuckDb
                    | MySql
                    | MariaDb
                    | AmazonAurora
                    | AmazonRds
                    | AzureMySqlFlexible
                    | CloudSqlMySql
                    | PlanetScale
                    | TiDb
                    | Spanner
            ),
            Returning => matches!(
                self,
                Postgres
                    | CockroachDb
                    | YugabyteDb
                    | AmazonAuroraPostgres
                    | AmazonRdsPostgres
                    | AzurePostgresFlexible
                    | CloudSqlPostgres
                    | Neon
                    | Supabase
                    | Sqlite
                    | DuckDb
                    | MariaDb
                    | Oracle
                    | Spanner
            ),
            TruncateTable => !matches!(self, Sqlite | Access),
        }
    }

    /// Whether the engine understands a given keyword in its SQL surface.
    pub fn supports_keyword(self, kw: SqlKeyword) -> bool {
        use DatabaseType::*;
        use SqlKeyword::*;
        if self == LibSql {
            return Sqlite.supports_keyword(kw);
        }
        match kw {
            Select | Insert | Update | Delete | From | Where | Having | GroupBy | OrderBy
            | Distinct | Union | Join => true,
            Top => matches!(
                self,
                SqlServer | AzureSqlDatabase | Access | Teradata | SapHana
            ),
            Limit => matches!(
                self,
                Postgres
                    | MySql
                    | MariaDb
                    | Sqlite
                    | AmazonAurora
                    | AmazonRds
                    | Snowflake
                    | CockroachDb
                    | Spanner
                    | YugabyteDb
                    | DuckDb
                    | Databricks
                    | SapHana
                    | Informix
                    | ClickHouse
            ),
            Offset => !matches!(self, Access),
            FetchFirst => matches!(
                self,
                SqlServer
                    | AzureSqlDatabase
                    | Postgres
                    | Oracle
                    | Db2
                    | Snowflake
                    | CockroachDb
                    | YugabyteDb
                    | DuckDb
                    | Informix
            ),
            Merge => self.supports_operation(SqlOperation::Merge),
            Truncate => self.supports_operation(SqlOperation::TruncateTable),
            Returning => self.supports_operation(SqlOperation::Returning),
            Qualify => matches!(
                self,
                Snowflake | Teradata | Databricks | DuckDb | ClickHouse
            ),
        }
    }

    /// Whether the engine offers a given schema-level feature.
    pub fn supports_feature(self, feature: DbFeature) -> bool {
        use DatabaseType::*;
        use DbFeature::*;
        if self == LibSql {
            return Sqlite.supports_feature(feature);
        }
        match feature {
            Views => true,
            MaterializedViews => matches!(
                self,
                Oracle | Postgres | Db2 | Snowflake | CockroachDb | YugabyteDb | ClickHouse
            ),
            StoredProcedures => !matches!(
                self,
                Sqlite | Access | DuckDb | Spanner | Databricks | TiDb | ClickHouse
            ),
            TempTables => !matches!(self, Access | Spanner | Databricks),
            ScalarUserDefinedFunctions => !matches!(self, Access | Sqlite | Spanner),
            TableUserDefinedFunctions => matches!(
                self,
                SqlServer
                    | AzureSqlDatabase
                    | Postgres
                    | Oracle
                    | Db2
                    | Snowflake
                    | YugabyteDb
                    | Teradata
                    | DuckDb
                    | Informix
                    | Databricks
                    | SapHana
            ),
            TableValuedParameters => matches!(self, SqlServer | AzureSqlDatabase),
            CustomScalarTypes => matches!(
                self,
                SqlServer
                    | AzureSqlDatabase
                    | Postgres
                    | Oracle
                    | Db2
                    | CockroachDb
                    | YugabyteDb
                    | DuckDb
            ),
            CustomTableTypes => matches!(
                self,
                SqlServer | AzureSqlDatabase | Postgres | Oracle | YugabyteDb
            ),
            Triggers => !matches!(self, Snowflake | Spanner | Databricks | ClickHouse),
            Sequences => matches!(
                self,
                SqlServer
                    | AzureSqlDatabase
                    | Postgres
                    | Oracle
                    | Db2
                    | MariaDb
                    | Snowflake
                    | CockroachDb
                    | YugabyteDb
                    | DuckDb
                    | Informix
                    | SapHana
                    | Spanner
            ),
            Schemas => self.family().has_namespaces() && !matches!(self, Access),
            MultipleDatabases => self.family().has_catalogs() && !matches!(self, Sqlite | Access),
            Transactions => true,
        }
    }

    /// How an upsert should be phrased on this engine — the pattern
    /// equivalency behind a future "generate upsert" UI action.
    pub fn upsert_pattern(self) -> UpsertPattern {
        use DatabaseType::*;
        match self {
            Postgres
            | CockroachDb
            | YugabyteDb
            | AmazonAuroraPostgres
            | AmazonRdsPostgres
            | AzurePostgresFlexible
            | CloudSqlPostgres
            | Neon
            | Supabase
            | Sqlite
            | LibSql
            | DuckDb => UpsertPattern::OnConflict,
            MySql | MariaDb | AmazonAurora | AmazonRds | AzureMySqlFlexible | CloudSqlMySql
            | PlanetScale | TiDb => UpsertPattern::OnDuplicateKey,
            Spanner => UpsertPattern::InsertOrUpdate,
            SqlServer | AzureSqlDatabase | Oracle | Db2 | Teradata | SapHana | Snowflake
            | Databricks | Informix => UpsertPattern::Merge,
            // ClickHouse has neither ON CONFLICT nor MERGE; rows are appended
            // and deduplicated by the table engine, so an upsert is a read
            // followed by an INSERT or an ALTER … UPDATE.
            Access | ClickHouse => UpsertPattern::SelectThenInsertOrUpdate,
            // Redis has no rows: every write is a key set. The closest shape
            // is a read followed by a write.
            Redis | Valkey | KeyDb | DragonflyDb | AmazonElastiCache | AmazonMemoryDb => {
                UpsertPattern::SelectThenInsertOrUpdate
            }
        }
    }

    /// Render a string as a quoted SQL literal for this engine, doubling
    /// embedded quotes so hostile values stay injection-safe. SQL Server gets
    /// the `N'…'` national form. Engines whose literals can treat `\` as an
    /// escape must never leave a lone backslash in front of the closing quote:
    ///
    /// - MySQL lineage, ClickHouse, Snowflake: backslashes are doubled. Under
    ///   MySQL's NO_BACKSLASH_ESCAPES mode that stores a doubled backslash
    ///   (a data change, never an injection — the quote is still `''`), and
    ///   default-mode servers read the value exactly.
    /// - PostgreSQL wire: a value containing a backslash is written as
    ///   `E'…'` with backslashes doubled, which reads identically whether
    ///   `standard_conforming_strings` is on or off; other values keep the
    ///   plain form.
    /// - Databricks (Spark SQL) and Spanner (GoogleSQL): backslash escapes
    ///   and no `''` form, so both `\` and `'` are backslash-escaped.
    pub fn string_literal(self, value: &str) -> String {
        use DatabaseType::*;
        match self {
            SqlServer | AzureSqlDatabase => format!("N'{}'", value.replace('\'', "''")),
            MySql | MariaDb | AmazonAurora | AmazonRds | AzureMySqlFlexible | CloudSqlMySql
            | PlanetScale | TiDb | ClickHouse | Snowflake => {
                format!("'{}'", value.replace('\\', "\\\\").replace('\'', "''"))
            }
            Databricks | Spanner => {
                format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
            }
            _ if self.connection_kind() == ConnectionKind::PostgresWire && value.contains('\\') => {
                format!("E'{}'", value.replace('\\', "\\\\").replace('\'', "''"))
            }
            _ => format!("'{}'", value.replace('\'', "''")),
        }
    }

    /// Render a boolean literal: `TRUE`/`FALSE` on engines with a real
    /// boolean literal, `1`/`0` where booleans are bits or encoded integers
    /// (SQL Server `bit` does not accept `TRUE`).
    pub fn bool_literal(self, value: bool) -> &'static str {
        use DatabaseType::*;
        match self {
            Postgres
            | CockroachDb
            | YugabyteDb
            | AmazonAuroraPostgres
            | AmazonRdsPostgres
            | AzurePostgresFlexible
            | CloudSqlPostgres
            | Neon
            | Supabase
            | DuckDb
            | Snowflake
            | Spanner
            | Databricks
            | Access
            | ClickHouse => {
                if value {
                    "TRUE"
                } else {
                    "FALSE"
                }
            }
            _ => {
                if value {
                    "1"
                } else {
                    "0"
                }
            }
        }
    }

    /// Render a binary value as a hex literal in the engine's native form:
    /// `0xDEAD` (SQL Server, MySQL lineage), `'\xDEAD'` (PostgreSQL bytea
    /// lineage), `HEXTORAW('DEAD')` (Oracle), `X'DEAD'` (SQLite and the
    /// standard blob-literal fallback).
    pub fn bytes_literal(self, bytes: &[u8]) -> String {
        use DatabaseType::*;
        let hex: String = bytes.iter().map(|b| format!("{b:02X}")).collect();
        match self {
            SqlServer | AzureSqlDatabase | MySql | MariaDb | AmazonAurora | AmazonRds
            | AzureMySqlFlexible | CloudSqlMySql | PlanetScale | TiDb => {
                format!("0x{hex}")
            }
            Postgres
            | CockroachDb
            | YugabyteDb
            | AmazonAuroraPostgres
            | AmazonRdsPostgres
            | AzurePostgresFlexible
            | CloudSqlPostgres
            | Neon
            | Supabase
            | DuckDb => format!("'\\x{hex}'"),
            Oracle => format!("HEXTORAW('{hex}')"),
            _ => format!("X'{hex}'"),
        }
    }

    /// The two halves of the engine's row-limit pattern for a SELECT that
    /// carries its own WHERE/ORDER BY text between them: what goes right
    /// after `SELECT` and what is appended after everything else. Lets
    /// callers assemble `SELECT {prefix}* FROM t WHERE …{suffix}` without
    /// caring which side the engine's limiter lives on.
    pub fn row_limit_clauses(self, n: usize) -> (String, String) {
        let n = n.max(1);
        match self.row_limit_syntax() {
            RowLimitSyntax::Top => (format!("TOP {n} "), String::new()),
            RowLimitSyntax::First => (format!("FIRST {n} "), String::new()),
            RowLimitSyntax::Limit => (String::new(), format!("LIMIT {n}")),
            RowLimitSyntax::FetchFirst => (String::new(), format!("FETCH FIRST {n} ROWS ONLY")),
        }
    }

    /// The characters a generated LIKE pattern must escape on this engine
    /// (paired with an explicit `ESCAPE '\'`). SQL Server additionally
    /// treats `[`/`]` as character-class markers; escaping them elsewhere is
    /// an error on engines that reject undefined escape sequences.
    pub fn like_special_characters(self) -> &'static [char] {
        use DatabaseType::*;
        match self {
            SqlServer | AzureSqlDatabase | Access => &['\\', '%', '_', '[', ']'],
            _ => &['\\', '%', '_'],
        }
    }

    /// Native storage for a logical type: what to call it in DDL and whether
    /// the engine stores it as a distinct type or encodes it in another one
    /// (SQLite has no date type; Oracle has no boolean column type pre-23ai).
    pub fn map_type(self, ty: LogicalType) -> TypeMapping {
        use DatabaseType::*;
        use LogicalType::*;
        use TypeStorage::*;
        if self == LibSql {
            return Sqlite.map_type(ty);
        }
        let m = |name: &'static str, storage: TypeStorage| TypeMapping {
            native_name: name,
            storage,
        };
        match ty {
            Boolean => match self {
                SqlServer | AzureSqlDatabase => m("bit", Native),
                Oracle => m("number(1)", NumericEncoded),
                Teradata => m("byteint", NumericEncoded),
                MySql | MariaDb | AmazonAurora | AmazonRds | AzureMySqlFlexible | CloudSqlMySql
                | PlanetScale | TiDb => m("tinyint(1)", IntegerEncoded),
                Sqlite => m("integer", IntegerEncoded),
                Access => m("yesno", Native),
                ClickHouse => m("Bool", Native),
                _ => m("boolean", Native),
            },
            Integer => match self {
                Oracle => m("number(10)", NumericEncoded),
                Access => m("long", Native),
                Snowflake => m("number(38,0)", NumericEncoded),
                Sqlite => m("integer", Native),
                Spanner | Databricks => m("int64", Native),
                ClickHouse => m("Int32", Native),
                _ => m("integer", Native),
            },
            BigInt => match self {
                Oracle => m("number(19)", NumericEncoded),
                Access => m("decimal(19,0)", NumericEncoded),
                Sqlite => m("integer", Native),
                Spanner | Databricks => m("int64", Native),
                ClickHouse => m("Int64", Native),
                _ => m("bigint", Native),
            },
            Decimal => match self {
                Oracle => m("number", Native),
                Sqlite => m("real", NumericEncoded),
                Access => m("currency", Native),
                ClickHouse => m("Decimal(38, 10)", Native),
                _ => m("decimal", Native),
            },
            Float => match self {
                SqlServer | AzureSqlDatabase => m("float", Native),
                Oracle => m("binary_double", Native),
                Sqlite => m("real", Native),
                Spanner => m("float64", Native),
                ClickHouse => m("Float64", Native),
                _ => m("double precision", Native),
            },
            Text => match self {
                SqlServer | AzureSqlDatabase => m("nvarchar(max)", Native),
                Oracle => m("clob", Native),
                MySql | MariaDb | AmazonAurora | AmazonRds | AzureMySqlFlexible | CloudSqlMySql
                | PlanetScale | TiDb => m("longtext", Native),
                Db2 => m("clob", Native),
                Access => m("longtext", Native),
                Snowflake => m("varchar", Native),
                Spanner => m("string(max)", Native),
                Databricks => m("string", Native),
                Teradata | Informix | SapHana => m("clob", Native),
                ClickHouse => m("String", Native),
                _ => m("text", Native),
            },
            VarChar => match self {
                SqlServer | AzureSqlDatabase => m("nvarchar", Native),
                Oracle => m("varchar2", Native),
                Sqlite => m("text", TextEncoded),
                Spanner => m("string", Native),
                Databricks => m("string", Native),
                SapHana => m("nvarchar", Native),
                ClickHouse => m("String", Native),
                _ => m("varchar", Native),
            },
            Binary => match self {
                SqlServer | AzureSqlDatabase => m("varbinary(max)", Native),
                Postgres
                | CockroachDb
                | YugabyteDb
                | AmazonAuroraPostgres
                | AmazonRdsPostgres
                | AzurePostgresFlexible
                | CloudSqlPostgres
                | Neon
                | Supabase
                | DuckDb => m("bytea", Native),
                Oracle | Db2 | Informix | SapHana => m("blob", Native),
                MySql | MariaDb | AmazonAurora | AmazonRds | AzureMySqlFlexible | CloudSqlMySql
                | PlanetScale | TiDb => m("longblob", Native),
                Sqlite | LibSql => m("blob", Native),
                Access => m("oleobject", Native),
                Snowflake | Spanner | Databricks => m("binary", Native),
                Teradata => m("blob", Native),
                ClickHouse => m("String", BinaryEncoded),
                // Redis values are binary-safe strings; every logical type
                // below is stored as its text form in one.
                Redis | Valkey | KeyDb | DragonflyDb | AmazonElastiCache | AmazonMemoryDb => {
                    m("string", Native)
                }
            },
            Date => match self {
                Sqlite => m("text", TextEncoded),
                Oracle => m("date", Native), // Oracle DATE carries time too.
                Access => m("datetime", DateTimeEncoded),
                ClickHouse => m("Date32", Native),
                _ => m("date", Native),
            },
            Time => match self {
                Sqlite => m("text", TextEncoded),
                Oracle => m("timestamp", DateTimeEncoded),
                Access => m("datetime", DateTimeEncoded),
                Spanner | Databricks | ClickHouse => m("string", TextEncoded),
                _ => m("time", Native),
            },
            DateTime => match self {
                SqlServer | AzureSqlDatabase => m("datetime2", Native),
                Sqlite | LibSql => m("text", TextEncoded),
                Oracle
                | Db2
                | Postgres
                | CockroachDb
                | YugabyteDb
                | AmazonAuroraPostgres
                | AmazonRdsPostgres
                | AzurePostgresFlexible
                | CloudSqlPostgres
                | Neon
                | Supabase
                | DuckDb
                | Snowflake
                | Teradata
                | SapHana
                | Informix => m("timestamp", Native),
                MySql | MariaDb | AmazonAurora | AmazonRds | AzureMySqlFlexible | CloudSqlMySql
                | PlanetScale | TiDb => m("datetime", Native),
                Access => m("datetime", Native),
                Spanner | Databricks => m("timestamp", Native),
                ClickHouse => m("DateTime64(3)", Native),
                Redis | Valkey | KeyDb | DragonflyDb | AmazonElastiCache | AmazonMemoryDb => {
                    m("string", TextEncoded)
                }
            },
            DateTimeOffset => match self {
                SqlServer | AzureSqlDatabase => m("datetimeoffset", Native),
                Postgres
                | CockroachDb
                | YugabyteDb
                | AmazonAuroraPostgres
                | AmazonRdsPostgres
                | AzurePostgresFlexible
                | CloudSqlPostgres
                | Neon
                | Supabase
                | DuckDb
                | Snowflake => m("timestamp with time zone", Native),
                Oracle | Db2 | Teradata | SapHana | Informix => {
                    m("timestamp with time zone", Native)
                }
                MySql | MariaDb | AmazonAurora | AmazonRds | AzureMySqlFlexible | CloudSqlMySql
                | PlanetScale | TiDb => m("timestamp", DateTimeEncoded),
                Sqlite | LibSql => m("text", TextEncoded),
                Access => m("datetime", DateTimeEncoded),
                Spanner | Databricks => m("timestamp", Native),
                // DateTime64 carries a fixed zone, not a per-value offset.
                ClickHouse => m("DateTime64(3, 'UTC')", DateTimeEncoded),
                Redis | Valkey | KeyDb | DragonflyDb | AmazonElastiCache | AmazonMemoryDb => {
                    m("string", TextEncoded)
                }
            },
            Uuid => match self {
                SqlServer | AzureSqlDatabase => m("uniqueidentifier", Native),
                Postgres
                | CockroachDb
                | YugabyteDb
                | AmazonAuroraPostgres
                | AmazonRdsPostgres
                | AzurePostgresFlexible
                | CloudSqlPostgres
                | Neon
                | Supabase
                | DuckDb => m("uuid", Native),
                Oracle => m("raw(16)", BinaryEncoded),
                Teradata | Db2 | Informix | SapHana => m("char(36)", TextEncoded),
                MySql | MariaDb | AmazonAurora | AmazonRds | AzureMySqlFlexible | CloudSqlMySql
                | PlanetScale | TiDb => m("char(36)", TextEncoded),
                Sqlite | LibSql | Snowflake | Spanner | Databricks | Access => {
                    m("text", TextEncoded)
                }
                ClickHouse => m("UUID", Native),
                Redis | Valkey | KeyDb | DragonflyDb | AmazonElastiCache | AmazonMemoryDb => {
                    m("string", TextEncoded)
                }
            },
            Json => match self {
                Postgres
                | CockroachDb
                | YugabyteDb
                | AmazonAuroraPostgres
                | AmazonRdsPostgres
                | AzurePostgresFlexible
                | CloudSqlPostgres
                | Neon
                | Supabase => m("jsonb", Native),
                MySql | MariaDb | AmazonAurora | AmazonRds | AzureMySqlFlexible | CloudSqlMySql
                | PlanetScale | TiDb => m("json", Native),
                Oracle => m("json", Native),
                Snowflake => m("variant", Native),
                DuckDb | Spanner => m("json", Native),
                ClickHouse => m("JSON", Native),
                Db2 | SapHana | Teradata | Informix => m("clob", TextEncoded),
                SqlServer | AzureSqlDatabase => m("nvarchar(max)", TextEncoded),
                Sqlite | LibSql | Access => m("text", TextEncoded),
                Databricks => m("string", TextEncoded),
                Redis | Valkey | KeyDb | DragonflyDb | AmazonElastiCache | AmazonMemoryDb => {
                    m("string", TextEncoded)
                }
            },
            Xml => match self {
                SqlServer | AzureSqlDatabase => m("xml", Native),
                Postgres
                | YugabyteDb
                | AmazonAuroraPostgres
                | AmazonRdsPostgres
                | AzurePostgresFlexible
                | CloudSqlPostgres
                | Neon
                | Supabase => m("xml", Native),
                Oracle => m("xmltype", Native),
                Db2 => m("xml", Native),
                _ => m("text", TextEncoded),
            },
        }
    }

    /// The full serializable capability profile — what the data model reports
    /// so the UI can gate menus, rewrite SQL, and explain type storage
    /// without re-deriving anything.
    pub fn metadata(self) -> DbTypeMetadata {
        DbTypeMetadata {
            database_type: self,
            display_name: self.display_name().to_string(),
            family: self.family(),
            identifier_quoting: self.identifier_quoting(),
            row_limit_syntax: self.row_limit_syntax(),
            upsert_pattern: self.upsert_pattern(),
            default_schema: self.default_schema().map(str::to_string),
            operations: SqlOperation::ALL
                .iter()
                .copied()
                .filter(|&op| self.supports_operation(op))
                .collect(),
            keywords: SqlKeyword::ALL
                .iter()
                .copied()
                .filter(|&kw| self.supports_keyword(kw))
                .collect(),
            features: DbFeature::ALL
                .iter()
                .copied()
                .filter(|&f| self.supports_feature(f))
                .collect(),
            type_map: LogicalType::ALL
                .iter()
                .map(|&ty| (ty, self.map_type(ty)))
                .collect(),
        }
    }
}

// ---------------------------------------------------------------------------
// Capability axes
// ---------------------------------------------------------------------------

/// The truly distinct connection mechanisms across the reference engines.
/// One driver crate per kind with a native driver: `sqlly-tds` (Tds),
/// `sqlly-pg` (PostgresWire), `sqlly-mysql` (MySqlWire), `sqlly-sqlite`
/// (SqliteFile). The remaining kinds have no driver yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ConnectionKind {
    /// Tabular Data Stream — SQL Server family.
    Tds,
    /// PostgreSQL wire protocol (also Cockroach, Yugabyte).
    PostgresWire,
    /// MySQL client/server protocol (also MariaDB, Aurora, RDS defaults).
    MySqlWire,
    /// In-process SQLite file/memory database.
    SqliteFile,
    /// libSQL / Turso over the Hrana HTTP pipeline (`POST /v2/pipeline`,
    /// port 8080 for a self-hosted `sqld`, HTTPS 443 for Turso Cloud).
    HranaHttp,
    /// In-process DuckDB file/memory database.
    DuckDbFile,
    /// ClickHouse over its HTTP interface (port 8123, or 8443 with TLS).
    ClickHouseHttp,
    /// RESP, the Redis serialization protocol (port 6379): Redis, Valkey,
    /// KeyDB, DragonflyDB, Amazon ElastiCache and MemoryDB. Not SQL.
    Resp,
    /// Oracle Net (TNS/OCI).
    OracleNet,
    /// IBM DRDA (Db2, Informix).
    Drda,
    /// ODBC/ACE bridge (Access).
    Odbc,
    /// HTTP/REST SQL APIs (Snowflake, Databricks, Spanner).
    HttpApi,
    /// Vendor-proprietary protocols (Teradata, SAP HANA).
    Proprietary,
}

/// Identifier quoting styles across the reference engines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IdentifierQuoting {
    /// `[name]`, `]` doubled — SQL Server family, Access.
    Brackets,
    /// `"name"`, `"` doubled — the ANSI default.
    DoubleQuotes,
    /// `` `name` ``, backtick doubled — MySQL family, Spanner, Databricks.
    Backticks,
}

impl IdentifierQuoting {
    pub fn quote(self, identifier: &str) -> String {
        match self {
            IdentifierQuoting::Brackets => format!("[{}]", identifier.replace(']', "]]")),
            IdentifierQuoting::DoubleQuotes => format!("\"{}\"", identifier.replace('"', "\"\"")),
            IdentifierQuoting::Backticks => format!("`{}`", identifier.replace('`', "``")),
        }
    }
}

/// Row-limiting patterns — the equivalency set behind "view top N".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RowLimitSyntax {
    /// `SELECT TOP n …` — SQL Server, Azure SQL, Access, Teradata.
    Top,
    /// `… LIMIT n` — Postgres/MySQL/SQLite lineages.
    Limit,
    /// `… FETCH FIRST n ROWS ONLY` — Oracle 12c+, Db2 (ISO syntax).
    FetchFirst,
    /// `SELECT FIRST n …` — Informix.
    First,
}

/// Query-shape operations the UI or generators may want to gate on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum SqlOperation {
    Cte,
    RecursiveCte,
    WhereIn,
    WhereNotIn,
    WhereBetween,
    WhereLike,
    WhereIlike,
    WhereExists,
    GroupBy,
    Having,
    OrderBy,
    Union,
    Intersect,
    Except,
    InnerJoin,
    LeftOuterJoin,
    FullOuterJoin,
    WindowFunctions,
    Merge,
    Upsert,
    Returning,
    TruncateTable,
}

impl SqlOperation {
    pub const ALL: [SqlOperation; 22] = [
        SqlOperation::Cte,
        SqlOperation::RecursiveCte,
        SqlOperation::WhereIn,
        SqlOperation::WhereNotIn,
        SqlOperation::WhereBetween,
        SqlOperation::WhereLike,
        SqlOperation::WhereIlike,
        SqlOperation::WhereExists,
        SqlOperation::GroupBy,
        SqlOperation::Having,
        SqlOperation::OrderBy,
        SqlOperation::Union,
        SqlOperation::Intersect,
        SqlOperation::Except,
        SqlOperation::InnerJoin,
        SqlOperation::LeftOuterJoin,
        SqlOperation::FullOuterJoin,
        SqlOperation::WindowFunctions,
        SqlOperation::Merge,
        SqlOperation::Upsert,
        SqlOperation::Returning,
        SqlOperation::TruncateTable,
    ];
}

/// Keywords whose presence differs across engines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum SqlKeyword {
    Select,
    Insert,
    Update,
    Delete,
    From,
    Where,
    Having,
    GroupBy,
    OrderBy,
    Distinct,
    Union,
    Join,
    Top,
    Limit,
    Offset,
    FetchFirst,
    Merge,
    Truncate,
    Returning,
    Qualify,
}

impl SqlKeyword {
    pub const ALL: [SqlKeyword; 20] = [
        SqlKeyword::Select,
        SqlKeyword::Insert,
        SqlKeyword::Update,
        SqlKeyword::Delete,
        SqlKeyword::From,
        SqlKeyword::Where,
        SqlKeyword::Having,
        SqlKeyword::GroupBy,
        SqlKeyword::OrderBy,
        SqlKeyword::Distinct,
        SqlKeyword::Union,
        SqlKeyword::Join,
        SqlKeyword::Top,
        SqlKeyword::Limit,
        SqlKeyword::Offset,
        SqlKeyword::FetchFirst,
        SqlKeyword::Merge,
        SqlKeyword::Truncate,
        SqlKeyword::Returning,
        SqlKeyword::Qualify,
    ];
}

/// Schema-level features an engine may or may not offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum DbFeature {
    Views,
    MaterializedViews,
    StoredProcedures,
    TempTables,
    ScalarUserDefinedFunctions,
    TableUserDefinedFunctions,
    TableValuedParameters,
    CustomScalarTypes,
    CustomTableTypes,
    Triggers,
    Sequences,
    Schemas,
    MultipleDatabases,
    Transactions,
}

impl DbFeature {
    pub const ALL: [DbFeature; 14] = [
        DbFeature::Views,
        DbFeature::MaterializedViews,
        DbFeature::StoredProcedures,
        DbFeature::TempTables,
        DbFeature::ScalarUserDefinedFunctions,
        DbFeature::TableUserDefinedFunctions,
        DbFeature::TableValuedParameters,
        DbFeature::CustomScalarTypes,
        DbFeature::CustomTableTypes,
        DbFeature::Triggers,
        DbFeature::Sequences,
        DbFeature::Schemas,
        DbFeature::MultipleDatabases,
        DbFeature::Transactions,
    ];
}

/// How an upsert is phrased on a given engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UpsertPattern {
    /// `INSERT … ON CONFLICT DO UPDATE` (Postgres lineage, SQLite, DuckDB).
    OnConflict,
    /// `INSERT … ON DUPLICATE KEY UPDATE` (MySQL lineage).
    OnDuplicateKey,
    /// `MERGE INTO …` (SQL Server, Oracle, Db2, Snowflake, …).
    Merge,
    /// `INSERT OR UPDATE` (Spanner).
    InsertOrUpdate,
    /// No native form — client must SELECT then branch.
    SelectThenInsertOrUpdate,
}

// ---------------------------------------------------------------------------
// Data type mapping
// ---------------------------------------------------------------------------

/// Engine-neutral logical types the app reasons in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum LogicalType {
    Boolean,
    Integer,
    BigInt,
    Decimal,
    Float,
    VarChar,
    Text,
    Binary,
    Date,
    Time,
    DateTime,
    DateTimeOffset,
    Uuid,
    Json,
    Xml,
}

impl LogicalType {
    pub const ALL: [LogicalType; 15] = [
        LogicalType::Boolean,
        LogicalType::Integer,
        LogicalType::BigInt,
        LogicalType::Decimal,
        LogicalType::Float,
        LogicalType::VarChar,
        LogicalType::Text,
        LogicalType::Binary,
        LogicalType::Date,
        LogicalType::Time,
        LogicalType::DateTime,
        LogicalType::DateTimeOffset,
        LogicalType::Uuid,
        LogicalType::Json,
        LogicalType::Xml,
    ];
}

/// Whether the engine stores a logical type natively or encodes it inside a
/// broader type — SQLite dates in TEXT, Oracle booleans in NUMBER(1), MySQL
/// UUIDs in CHAR(36). The UI can surface this ("dates here are text").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TypeStorage {
    /// A distinct native type exists.
    Native,
    /// Encoded in a character type.
    TextEncoded,
    /// Encoded in an integer type.
    IntegerEncoded,
    /// Encoded in a generic numeric/decimal type.
    NumericEncoded,
    /// Encoded in a binary type.
    BinaryEncoded,
    /// Encoded in a broader date/time type (precision or zone lost).
    DateTimeEncoded,
}

/// Native representation of a logical type on one engine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TypeMapping {
    pub native_name: &'static str,
    pub storage: TypeStorage,
}

/// The full, serializable capability profile for one engine — the shape the
/// data model reports down-stack for the UI to consume.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DbTypeMetadata {
    pub database_type: DatabaseType,
    pub display_name: String,
    pub family: Dialect,
    pub identifier_quoting: IdentifierQuoting,
    pub row_limit_syntax: RowLimitSyntax,
    pub upsert_pattern: UpsertPattern,
    pub default_schema: Option<String>,
    pub operations: Vec<SqlOperation>,
    pub keywords: Vec<SqlKeyword>,
    pub features: Vec<DbFeature>,
    pub type_map: BTreeMap<LogicalType, TypeMapping>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_engine_has_a_family_and_name() {
        for db in DatabaseType::ALL {
            assert!(!db.display_name().is_empty());
            let _ = db.family();
        }
    }

    #[test]
    fn row_limit_equivalency_examples() {
        assert_eq!(
            DatabaseType::SqlServer.render_limited_select("*", "[dbo].[Orders]", 1000),
            "SELECT TOP 1000 *\nFROM [dbo].[Orders]"
        );
        assert_eq!(
            DatabaseType::Sqlite.render_limited_select("*", "\"Orders\"", 1000),
            "SELECT *\nFROM \"Orders\"\nLIMIT 1000"
        );
        assert_eq!(
            DatabaseType::Oracle.render_limited_select("*", "\"ORDERS\"", 1000),
            "SELECT *\nFROM \"ORDERS\"\nFETCH FIRST 1000 ROWS ONLY"
        );
        assert_eq!(
            DatabaseType::Informix.render_limited_select("*", "orders", 50),
            "SELECT FIRST 50 *\nFROM orders"
        );
    }

    #[test]
    fn render_limited_select_clamps_to_one() {
        assert!(
            DatabaseType::MySql
                .render_limited_select("*", "t", 0)
                .ends_with("LIMIT 1")
        );
    }

    #[test]
    fn string_literal_per_engine() {
        assert_eq!(
            DatabaseType::SqlServer.string_literal("O'Brien"),
            "N'O''Brien'"
        );
        assert_eq!(
            DatabaseType::Postgres.string_literal("O'Brien"),
            "'O''Brien'"
        );
        assert_eq!(DatabaseType::Sqlite.string_literal("a'b"), "'a''b'");
        // MySQL doubles backslashes so a trailing one can't eat the quote.
        assert_eq!(DatabaseType::MySql.string_literal(r"a\"), r"'a\\'");
        assert_eq!(DatabaseType::MySql.string_literal(r"a\'b"), r"'a\\''b'");
    }

    #[test]
    fn bool_literal_per_engine() {
        assert_eq!(DatabaseType::SqlServer.bool_literal(true), "1");
        assert_eq!(DatabaseType::Postgres.bool_literal(true), "TRUE");
        assert_eq!(DatabaseType::Postgres.bool_literal(false), "FALSE");
        assert_eq!(DatabaseType::MySql.bool_literal(false), "0");
        assert_eq!(DatabaseType::Sqlite.bool_literal(true), "1");
    }

    #[test]
    fn bytes_literal_per_engine() {
        let b = [0xDEu8, 0xAD];
        assert_eq!(DatabaseType::SqlServer.bytes_literal(&b), "0xDEAD");
        assert_eq!(DatabaseType::MySql.bytes_literal(&b), "0xDEAD");
        assert_eq!(DatabaseType::Postgres.bytes_literal(&b), r"'\xDEAD'");
        assert_eq!(DatabaseType::Sqlite.bytes_literal(&b), "X'DEAD'");
        assert_eq!(DatabaseType::Oracle.bytes_literal(&b), "HEXTORAW('DEAD')");
        assert_eq!(DatabaseType::Db2.bytes_literal(&b), "X'DEAD'");
    }

    #[test]
    fn row_limit_clauses_split_prefix_and_suffix() {
        assert_eq!(
            DatabaseType::SqlServer.row_limit_clauses(1000),
            ("TOP 1000 ".to_string(), String::new())
        );
        assert_eq!(
            DatabaseType::Postgres.row_limit_clauses(1000),
            (String::new(), "LIMIT 1000".to_string())
        );
        assert_eq!(
            DatabaseType::Oracle.row_limit_clauses(10),
            (String::new(), "FETCH FIRST 10 ROWS ONLY".to_string())
        );
        assert_eq!(
            DatabaseType::Informix.row_limit_clauses(0),
            ("FIRST 1 ".to_string(), String::new())
        );
    }

    #[test]
    fn like_special_characters_only_bracket_engines_escape_brackets() {
        assert!(
            DatabaseType::SqlServer
                .like_special_characters()
                .contains(&'[')
        );
        assert!(
            !DatabaseType::Postgres
                .like_special_characters()
                .contains(&'[')
        );
        assert!(DatabaseType::MySql.like_special_characters().contains(&'%'));
    }

    #[test]
    fn identifier_quoting_doubles_closers() {
        assert_eq!(DatabaseType::SqlServer.quote_identifier("a]b"), "[a]]b]");
        assert_eq!(
            DatabaseType::Postgres.quote_identifier("a\"b"),
            "\"a\"\"b\""
        );
        assert_eq!(DatabaseType::MySql.quote_identifier("a`b"), "`a``b`");
    }

    /// ClickHouse and Spanner read backslash escapes inside backquoted
    /// identifiers, so a name ending in `\` would escape the closing backtick
    /// and let the rest of the statement run as code.
    #[test]
    fn backslash_escaping_engines_escape_backslashes_in_identifiers() {
        assert_eq!(DatabaseType::ClickHouse.quote_identifier(r"a\"), r"`a\\`");
        assert_eq!(
            DatabaseType::ClickHouse.quote_identifier(r"x\`; DROP TABLE t; --"),
            r"`x\\``; DROP TABLE t; --`"
        );
        assert_eq!(DatabaseType::Spanner.quote_identifier(r"a\"), r"`a\\`");
        assert_eq!(DatabaseType::Spanner.quote_identifier("a`b"), r"`a\`b`");
        // MySQL does not treat backslash as an escape inside backticks.
        assert_eq!(DatabaseType::MySql.quote_identifier(r"a\"), r"`a\`");
    }

    /// Engines whose plain string literals honour backslash escapes (always,
    /// or under a server mode the client cannot see) must never emit a lone
    /// backslash before the closing quote.
    #[test]
    fn string_literals_survive_backslash_escaping_modes() {
        // PostgreSQL wire: an E'' literal reads the same whether
        // standard_conforming_strings is on or off. Backslash-free values
        // keep the plain form.
        assert_eq!(
            DatabaseType::Postgres.string_literal(r"C:\dir\"),
            r"E'C:\\dir\\'"
        );
        assert_eq!(
            DatabaseType::CockroachDb.string_literal(r"a\'b"),
            r"E'a\\''b'"
        );
        assert_eq!(DatabaseType::Supabase.string_literal("plain"), "'plain'");
        // Snowflake always processes backslash escapes in '…'.
        assert_eq!(DatabaseType::Snowflake.string_literal(r"a\"), r"'a\\'");
        // Spark SQL (Databricks) and GoogleSQL (Spanner) escape with a
        // backslash and do not accept '' as an escaped quote.
        assert_eq!(
            DatabaseType::Databricks.string_literal(r"x\' OR 1=1 --"),
            r"'x\\\' OR 1=1 --'"
        );
        assert_eq!(DatabaseType::Spanner.string_literal("it's"), r"'it\'s'");
        // Engines without backslash escapes are unchanged.
        assert_eq!(DatabaseType::Sqlite.string_literal(r"a\"), r"'a\'");
        assert_eq!(DatabaseType::Oracle.string_literal(r"a\'b"), r"'a\''b'");
        assert_eq!(DatabaseType::DuckDb.string_literal(r"a\"), r"'a\'");
    }

    #[test]
    fn keyword_matrix_spot_checks() {
        assert!(DatabaseType::SqlServer.supports_keyword(SqlKeyword::Top));
        assert!(!DatabaseType::Sqlite.supports_keyword(SqlKeyword::Top));
        assert!(DatabaseType::Sqlite.supports_keyword(SqlKeyword::Limit));
        assert!(!DatabaseType::SqlServer.supports_keyword(SqlKeyword::Limit));
        assert!(DatabaseType::Db2.supports_keyword(SqlKeyword::FetchFirst));
        assert!(DatabaseType::Snowflake.supports_keyword(SqlKeyword::Qualify));
    }

    #[test]
    fn feature_matrix_spot_checks() {
        assert!(DatabaseType::SqlServer.supports_feature(DbFeature::TableValuedParameters));
        assert!(!DatabaseType::Postgres.supports_feature(DbFeature::TableValuedParameters));
        assert!(!DatabaseType::Sqlite.supports_feature(DbFeature::StoredProcedures));
        assert!(DatabaseType::Postgres.supports_feature(DbFeature::MaterializedViews));
        assert!(!DatabaseType::Access.supports_operation(SqlOperation::Cte));
        for db in DatabaseType::ALL {
            assert!(db.supports_feature(DbFeature::Views));
            assert!(db.supports_operation(SqlOperation::WhereIn));
        }
    }

    #[test]
    fn type_map_examples() {
        let d = DatabaseType::Sqlite.map_type(LogicalType::Date);
        assert_eq!(d.storage, TypeStorage::TextEncoded);
        let b = DatabaseType::Oracle.map_type(LogicalType::Boolean);
        assert_eq!(b.storage, TypeStorage::NumericEncoded);
        let u = DatabaseType::Postgres.map_type(LogicalType::Uuid);
        assert_eq!(u.native_name, "uuid");
        assert_eq!(u.storage, TypeStorage::Native);
    }

    #[test]
    fn metadata_is_complete_and_serializable() {
        for db in DatabaseType::ALL {
            let meta = db.metadata();
            assert_eq!(meta.type_map.len(), LogicalType::ALL.len());
            assert!(!meta.features.is_empty());
            let json = serde_json::to_string(&meta).expect("metadata serializes");
            assert!(json.contains(&meta.display_name));
        }
    }

    #[test]
    fn default_is_sql_server() {
        assert_eq!(DatabaseType::default(), DatabaseType::SqlServer);
    }

    #[test]
    fn connection_kind_groups_wire_compatible_engines() {
        assert_eq!(
            DatabaseType::CockroachDb.connection_kind(),
            ConnectionKind::PostgresWire
        );
        assert_eq!(
            DatabaseType::YugabyteDb.connection_kind(),
            DatabaseType::Postgres.connection_kind()
        );
        assert_eq!(
            DatabaseType::MariaDb.connection_kind(),
            ConnectionKind::MySqlWire
        );
        assert_eq!(
            DatabaseType::AzureSqlDatabase.connection_kind(),
            ConnectionKind::Tds
        );
        assert_eq!(
            DatabaseType::Sqlite.connection_kind(),
            ConnectionKind::SqliteFile
        );
    }

    #[test]
    fn compatible_engines_share_wire_family_and_driver() {
        for db in DatabaseType::ALL {
            if let Some(base) = db.compatible_with() {
                assert_eq!(db.connection_kind(), base.connection_kind(), "{db:?}");
                assert_eq!(db.family(), base.family(), "{db:?}");
                assert!(db.has_native_driver(), "{db:?}");
            }
        }
    }

    #[test]
    fn all_registry_is_complete_and_unique() {
        assert_eq!(DatabaseType::ALL.len(), 38);
        let mut seen = std::collections::BTreeSet::new();
        for db in DatabaseType::ALL {
            assert!(seen.insert(db), "duplicate {db:?}");
        }
        let mut names = std::collections::BTreeSet::new();
        for db in DatabaseType::ALL {
            assert!(names.insert(db.display_name()), "duplicate name for {db:?}");
        }
    }

    #[test]
    fn requires_tls_only_on_compatible_hosted_services() {
        for db in DatabaseType::ALL {
            if db.requires_tls() {
                assert!(db.compatible_with().is_some(), "{db:?}");
            }
        }
    }

    /// libSQL is SQLite over the network: same grammar, quoting, literals,
    /// type map and feature answers, but its own connection kind and a
    /// `main` default schema for object resolution.
    #[test]
    fn libsql_mirrors_sqlite_capabilities_over_its_own_transport() {
        let libsql = DatabaseType::LibSql;
        let sqlite = DatabaseType::Sqlite;
        assert_eq!(libsql.display_name(), "libSQL / Turso");
        assert_eq!(libsql.family(), Dialect::Sqlite);
        // One database, no schemas: the SQLite family's catalog model
        // (attached databases) and its lack of namespaces both carry over.
        assert_eq!(
            libsql.family().has_catalogs(),
            Dialect::Sqlite.has_catalogs()
        );
        assert!(!libsql.family().has_namespaces());
        assert!(!libsql.supports_feature(DbFeature::MultipleDatabases));
        assert!(!libsql.supports_feature(DbFeature::Schemas));
        assert_eq!(libsql.identifier_quoting(), sqlite.identifier_quoting());
        assert_eq!(libsql.row_limit_syntax(), sqlite.row_limit_syntax());
        assert_eq!(libsql.upsert_pattern(), sqlite.upsert_pattern());
        assert_eq!(libsql.default_schema(), Some("main"));
        assert_eq!(libsql.compatible_with(), None);
        assert!(!libsql.requires_tls());
        assert_eq!(libsql.string_literal("a'b"), sqlite.string_literal("a'b"));
        assert_eq!(libsql.bool_literal(true), sqlite.bool_literal(true));
        assert_eq!(libsql.bytes_literal(&[1]), sqlite.bytes_literal(&[1]));
        for op in SqlOperation::ALL {
            assert_eq!(
                libsql.supports_operation(op),
                sqlite.supports_operation(op),
                "{op:?}"
            );
        }
        for kw in SqlKeyword::ALL {
            assert_eq!(
                libsql.supports_keyword(kw),
                sqlite.supports_keyword(kw),
                "{kw:?}"
            );
        }
        for feature in DbFeature::ALL {
            assert_eq!(
                libsql.supports_feature(feature),
                sqlite.supports_feature(feature),
                "{feature:?}"
            );
        }
        for ty in LogicalType::ALL {
            assert_eq!(libsql.map_type(ty), sqlite.map_type(ty), "{ty:?}");
        }
        assert_ne!(libsql.connection_kind(), sqlite.connection_kind());
    }

    #[test]
    fn tidb_has_no_stored_procedures() {
        assert!(!DatabaseType::TiDb.supports_feature(DbFeature::StoredProcedures));
    }

    #[test]
    fn native_driver_coverage() {
        for db in [
            DatabaseType::SqlServer,
            DatabaseType::AzureSqlDatabase,
            DatabaseType::Postgres,
            DatabaseType::CockroachDb,
            DatabaseType::YugabyteDb,
            DatabaseType::AmazonAuroraPostgres,
            DatabaseType::AmazonRdsPostgres,
            DatabaseType::AzurePostgresFlexible,
            DatabaseType::CloudSqlPostgres,
            DatabaseType::Neon,
            DatabaseType::Supabase,
            DatabaseType::MySql,
            DatabaseType::MariaDb,
            DatabaseType::AmazonAurora,
            DatabaseType::AmazonRds,
            DatabaseType::AzureMySqlFlexible,
            DatabaseType::CloudSqlMySql,
            DatabaseType::PlanetScale,
            DatabaseType::TiDb,
            DatabaseType::Sqlite,
        ] {
            assert!(db.has_native_driver(), "{db:?} should have a driver");
        }
        assert!(DatabaseType::DuckDb.has_native_driver());
        assert_eq!(DatabaseType::DuckDb.default_schema(), Some("main"));
        assert!(DatabaseType::ClickHouse.has_native_driver());
        assert_eq!(
            DatabaseType::ClickHouse.connection_kind(),
            ConnectionKind::ClickHouseHttp
        );
        // Oracle Net has a driver; its default schema is the login's own
        // schema, which only the server knows.
        assert!(DatabaseType::Oracle.has_native_driver());
        assert_eq!(DatabaseType::Oracle.default_schema(), None);
        assert!(DatabaseType::LibSql.has_native_driver());
        assert_eq!(
            DatabaseType::LibSql.connection_kind(),
            ConnectionKind::HranaHttp
        );
        for db in [
            DatabaseType::Redis,
            DatabaseType::Valkey,
            DatabaseType::KeyDb,
            DatabaseType::DragonflyDb,
            DatabaseType::AmazonElastiCache,
            DatabaseType::AmazonMemoryDb,
        ] {
            assert!(db.has_native_driver(), "{db:?}");
            assert_eq!(db.connection_kind(), ConnectionKind::Resp);
            assert!(db.is_command_engine());
            assert_eq!(db.default_schema(), None);
        }
        assert_eq!(DatabaseType::Redis.compatible_with(), None);
        assert_eq!(
            DatabaseType::Valkey.compatible_with(),
            Some(DatabaseType::Redis)
        );
        assert!(DatabaseType::AmazonMemoryDb.requires_tls());
        assert!(!DatabaseType::Postgres.is_command_engine());
        for db in [
            DatabaseType::Db2,
            DatabaseType::Access,
            DatabaseType::Snowflake,
            DatabaseType::Teradata,
        ] {
            assert!(!db.has_native_driver(), "{db:?} has no driver yet");
        }
    }
}
