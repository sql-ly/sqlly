//! Dialect-dispatched metadata SQL.
//!
//! [`sql_for_dialect`] generates, per engine family, the catalog queries that
//! [`crate::metadata::sql_for`] generates for SQL Server — with the SAME positional
//! result shape per [`MetadataQuery`] variant, because the metadata service
//! parses rows positionally. Two shape rules every query here follows:
//!
//! - Boolean-ish columns are emitted as integers (`0`/`1`). PostgreSQL's
//!   simple-query path returns booleans as the text `t`/`f`, which the
//!   consumer's lenient cell parsing does not recognize; integers parse
//!   everywhere.
//! - Concepts an engine genuinely lacks return an EMPTY result set of the
//!   right shape (the explorer folder or property page shows empty — e.g.
//!   SQLite routines and permissions) rather than an error, so a page reads
//!   the same whatever you are connected to. Each dialect's match is
//!   exhaustive over [`MetadataQuery`]: a new variant must be answered
//!   everywhere, which the compiler enforces.  [`MetadataUnsupported`] is now
//!   raised only by [`sql_for_engine`], for a [`DatabaseType`] with no native
//!   driver at all.
//!
//! String parameters are embedded as escaped literals with per-dialect rules:
//! PostgreSQL and SQLite double single-quotes; MySQL additionally doubles
//! backslashes (backslash is an escape character in MySQL string literals).

use crate::metadata::MetadataQuery;
use sqlly_core::{ConnectionKind, DatabaseType};

/// The metadata SQL family a connection speaks. Derived from the profile's
/// [`DatabaseType::connection_kind`], so every engine sharing a wire protocol
/// shares its catalog family (CockroachDB speaks Postgres, MariaDB speaks
/// MySQL, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataDialect {
    SqlServer,
    Postgres,
    MySql,
    Sqlite,
    /// DuckDB's own `duckdb_*()` catalog functions.
    DuckDb,
    /// ClickHouse's `system.*` tables.
    ClickHouse,
    /// Oracle's `ALL_*` data-dictionary views (plus `DBMS_METADATA` for
    /// definitions).
    Oracle,
}

impl MetadataDialect {
    /// The metadata family for a database type, or `None` when the type has
    /// no native driver (and so no metadata path) yet.
    pub fn for_database_type(db: DatabaseType) -> Option<MetadataDialect> {
        match db.connection_kind() {
            ConnectionKind::Tds => Some(MetadataDialect::SqlServer),
            ConnectionKind::PostgresWire => Some(MetadataDialect::Postgres),
            ConnectionKind::MySqlWire => Some(MetadataDialect::MySql),
            ConnectionKind::SqliteFile => Some(MetadataDialect::Sqlite),
            // libSQL serves SQLite's own catalog (`sqlite_master`, the
            // `pragma_*` table-valued functions), verified live.
            ConnectionKind::HranaHttp => Some(MetadataDialect::Sqlite),
            ConnectionKind::DuckDbFile => Some(MetadataDialect::DuckDb),
            ConnectionKind::ClickHouseHttp => Some(MetadataDialect::ClickHouse),
            ConnectionKind::OracleNet => Some(MetadataDialect::Oracle),
            _ => None,
        }
    }
}

/// A metadata query with no sensible analogue on the requested engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataUnsupported {
    pub dialect: MetadataDialect,
    pub query: String,
}

impl std::fmt::Display for MetadataUnsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} metadata does not support {}",
            match self.dialect {
                MetadataDialect::SqlServer => "SQL Server",
                MetadataDialect::Postgres => "PostgreSQL",
                MetadataDialect::MySql => "MySQL",
                MetadataDialect::Sqlite => "SQLite",
                MetadataDialect::DuckDb => "DuckDB",
                MetadataDialect::ClickHouse => "ClickHouse",
                MetadataDialect::Oracle => "Oracle",
            },
            self.query
        )
    }
}

impl std::error::Error for MetadataUnsupported {}

/// Generate the catalog SQL for `query` on `dialect`. SQL Server delegates to
/// [`crate::metadata::sql_for`] (the original generator); every other dialect
/// answers every variant, with an empty result set where the engine has no
/// such concept. The `Result` is kept for [`sql_for_engine`], which still
/// fails for a database type with no metadata dialect at all.
pub fn sql_for_dialect(
    query: &MetadataQuery,
    dialect: MetadataDialect,
) -> Result<String, MetadataUnsupported> {
    match dialect {
        MetadataDialect::SqlServer => Ok(crate::metadata::sql_for(query)),
        MetadataDialect::Postgres => Ok(postgres_sql(query)),
        MetadataDialect::MySql => Ok(mysql_sql(query)),
        MetadataDialect::Sqlite => Ok(sqlite_sql(query)),
        MetadataDialect::DuckDb => Ok(duckdb_sql(query)),
        MetadataDialect::ClickHouse => Ok(clickhouse_sql(query)),
        MetadataDialect::Oracle => Ok(oracle_sql(query)),
    }
}

/// Like [`sql_for_dialect`], but applies engine-specific catalog fallbacks
/// (for example CockroachDB routine definitions) when the profile's
/// [`DatabaseType`] is known.
pub fn sql_for_engine(
    query: &MetadataQuery,
    db: DatabaseType,
) -> Result<String, MetadataUnsupported> {
    let dialect = MetadataDialect::for_database_type(db).ok_or_else(|| MetadataUnsupported {
        dialect: MetadataDialect::SqlServer,
        query: format!("{query:?}"),
    })?;
    if dialect == MetadataDialect::Postgres
        && let MetadataQuery::ObjectDefinition { schema, object } = query
    {
        return Ok(postgres_object_definition_sql(schema, object, db));
    }
    // MariaDB alone records whether a variable is settable from SQL
    // (information_schema.SYSTEM_VARIABLES.read_only); MySQL's own catalogs
    // do not, so the generic MySql-dialect query marks everything editable.
    if let MetadataQuery::ServerConfiguration = query
        && db == DatabaseType::MariaDb
    {
        return Ok("SELECT variable_name AS name, global_value AS value, 'GLOBAL' AS source, IF(read_only = 'NO' AND global_value IS NOT NULL, 1, 0) AS is_dynamic FROM information_schema.SYSTEM_VARIABLES ORDER BY variable_name".to_string());
    }
    // CockroachDB's server-wide configuration is its cluster settings (the
    // only thing `SET CLUSTER SETTING` can change); pg_settings there lists
    // session variables. Reading them needs the VIEWCLUSTERSETTING (or
    // admin) privilege, which the page surfaces as its load error.
    if let MetadataQuery::ServerConfiguration = query
        && db == DatabaseType::CockroachDb
    {
        return Ok("SELECT variable AS name, value, description AS source, 1 AS is_dynamic FROM crdb_internal.cluster_settings ORDER BY variable".to_string());
    }
    // MariaDB keeps SHOW GLOBAL STATUS in information_schema (its
    // performance_schema may be off or lack global_status).
    if let MetadataQuery::ServerStatus = query
        && db == DatabaseType::MariaDb
    {
        return Ok("SELECT variable_name AS name, variable_value AS value, 'STATUS' AS source FROM information_schema.GLOBAL_STATUS ORDER BY variable_name".to_string());
    }
    // CockroachDB has no pg_stat_database/pg_stat_bgwriter; its cluster
    // metrics live in crdb_internal and are left to a dedicated surface.
    if let MetadataQuery::ServerStatus = query
        && db == DatabaseType::CockroachDb
    {
        return Ok("SELECT '' AS name, '' AS value, '' AS source WHERE 1 = 0".to_string());
    }
    sql_for_dialect(query, dialect)
}

/// MySQL 8 system variables that are read-only at runtime (set only at
/// startup), per the server system variable reference. `SET GLOBAL` on any
/// of them fails with error 1238, so the Configuration page shows them
/// read-only instead of staging a statement that cannot succeed.
const MYSQL_STATIC_VARIABLES: &[&str] = &[
    "admin_address",
    "admin_port",
    "back_log",
    "basedir",
    "bind_address",
    "character_set_system",
    "character_sets_dir",
    "core_file",
    "datadir",
    "default_authentication_plugin",
    "disabled_storage_engines",
    "ft_max_word_len",
    "ft_min_word_len",
    "ft_query_expansion_limit",
    "ft_stopword_file",
    "hostname",
    "init_file",
    "innodb_autoinc_lock_mode",
    "innodb_buffer_pool_chunk_size",
    "innodb_buffer_pool_instances",
    "innodb_data_file_path",
    "innodb_data_home_dir",
    "innodb_dedicated_server",
    "innodb_directories",
    "innodb_doublewrite_dir",
    "innodb_doublewrite_files",
    "innodb_force_recovery",
    "innodb_ft_max_token_size",
    "innodb_ft_min_token_size",
    "innodb_ft_sort_pll_degree",
    "innodb_log_file_size",
    "innodb_log_files_in_group",
    "innodb_log_group_home_dir",
    "innodb_numa_interleave",
    "innodb_page_cleaners",
    "innodb_page_size",
    "innodb_purge_threads",
    "innodb_read_io_threads",
    "innodb_rollback_on_timeout",
    "innodb_sort_buffer_size",
    "innodb_temp_data_file_path",
    "innodb_temp_tablespaces_dir",
    "innodb_undo_directory",
    "innodb_use_native_aio",
    "innodb_validate_tablespace_paths",
    "innodb_write_io_threads",
    "large_files_support",
    "large_page_size",
    "large_pages",
    "lc_messages_dir",
    "license",
    "locked_in_memory",
    "log_bin",
    "log_bin_basename",
    "log_bin_index",
    "log_error",
    "log_replica_updates",
    "log_slave_updates",
    "lower_case_file_system",
    "lower_case_table_names",
    "max_digest_length",
    "metadata_locks_hash_instances",
    "myisam_mmap_size",
    "myisam_recover_options",
    "mysqlx_bind_address",
    "mysqlx_port",
    "mysqlx_socket",
    "named_pipe",
    "ngram_token_size",
    "old",
    "open_files_limit",
    "performance_schema",
    "persisted_globals_load",
    "pid_file",
    "plugin_dir",
    "port",
    "protocol_version",
    "relay_log",
    "relay_log_basename",
    "relay_log_index",
    "relay_log_recovery",
    "relay_log_space_limit",
    "report_host",
    "report_password",
    "report_port",
    "report_user",
    "secure_file_priv",
    "server_uuid",
    "shared_memory",
    "shared_memory_base_name",
    "skip_external_locking",
    "skip_name_resolve",
    "skip_networking",
    "skip_show_database",
    "socket",
    "system_time_zone",
    "table_open_cache_instances",
    "thread_handling",
    "thread_stack",
    "tmpdir",
];

fn postgres_object_definition_sql(schema: &str, object: &str, db: DatabaseType) -> String {
    let s = lit(schema);
    let o = lit(object);
    let routine_def = if db == DatabaseType::CockroachDb {
        "'CREATE FUNCTION ' || quote_ident(n.nspname) || '.' || quote_ident(p.proname) || E'(...)\nAS $$\n' || p.prosrc || E'\n$$;'".to_string()
    } else {
        "pg_get_functiondef(p.oid)".to_string()
    };
    format!(
        "SELECT definition FROM (SELECT 1 AS ord, c.oid AS obj, CASE c.relkind WHEN 'v' THEN 'CREATE OR REPLACE VIEW ' || quote_ident(n.nspname) || '.' || quote_ident(c.relname) || E' AS\\n' || pg_get_viewdef(c.oid, true) WHEN 'm' THEN 'CREATE MATERIALIZED VIEW ' || quote_ident(n.nspname) || '.' || quote_ident(c.relname) || E' AS\\n' || pg_get_viewdef(c.oid, true) END AS definition FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = '{s}' AND c.relname = '{o}' AND c.relkind IN ('v', 'm') UNION ALL SELECT 2, p.oid, {routine_def} FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace WHERE n.nspname = '{s}' AND p.proname = '{o}' AND p.prokind IN ('f', 'p')) d WHERE definition IS NOT NULL ORDER BY ord, obj LIMIT 1"
    )
}

/// Escape for a PostgreSQL/SQLite string literal (single quotes doubled;
/// backslash is a literal character under standard conforming strings).
fn lit(value: &str) -> String {
    value.replace('\'', "''")
}

/// Escape for a MySQL string literal: backslash IS an escape character, so it
/// is doubled along with single quotes.
fn mysql_lit(value: &str) -> String {
    value.replace('\\', "\\\\").replace('\'', "''")
}

// ---------------------------------------------------------------------------
// PostgreSQL — pg_catalog / information_schema
// ---------------------------------------------------------------------------

/// PostgreSQL (over `information_schema.columns c`): the column is a serial —
/// its default is `nextval` of the sequence the column itself owns. A
/// default drawing from a shared sequence is an ordinary default, not an
/// identity. `regclass::text` renders the owned sequence the same way the
/// default expression does (schema-qualified only when off the search path).
const PG_SERIAL_DEFAULT: &str = "CASE WHEN c.column_default LIKE 'nextval(%' THEN COALESCE(c.column_default = 'nextval(''' || pg_catalog.pg_get_serial_sequence(pg_catalog.quote_ident(c.table_schema) || '.' || pg_catalog.quote_ident(c.table_name), c.column_name)::regclass::text || '''::regclass)', false) ELSE false END";

/// PostgreSQL: whether index position `k.ord` is a descending key. Bit 0 of
/// `pg_index.indoption` (0-based, key columns only) is INDOPTION_DESC;
/// INCLUDE positions have no option entry.
const PG_INDEX_KEY_DESC: &str = "CASE WHEN k.ord <= ix.indnkeyatts AND (ix.indoption[(k.ord - 1)::int]::int & 1) = 1 THEN 1 ELSE 0 END";
/// A key column carrying a modifier a plain column list cannot restate: a
/// NULLS FIRST/LAST that differs from the direction's default (indoption bit
/// 1 set only for DESC by default, so bits 0 and 1 disagree), or an operator
/// class that is not the column type's default (`pg_opclass.opcdefault`).
/// Such an index is kept verbatim from `pg_indexes.indexdef` instead.
const PG_INDEX_KEY_HAS_OPTIONS: &str = "CASE WHEN k.ord <= ix.indnkeyatts AND ((ix.indoption[(k.ord - 1)::int]::int & 3) IN (1, 2) OR EXISTS (SELECT 1 FROM pg_catalog.pg_opclass oc WHERE oc.oid = ix.indclass[(k.ord - 1)::int] AND NOT oc.opcdefault)) THEN 1 ELSE 0 END";

fn postgres_sql(query: &MetadataQuery) -> String {
    match query {
        MetadataQuery::Schemas => "SELECT nspname AS name FROM pg_catalog.pg_namespace WHERE nspname NOT LIKE 'pg_temp%' AND nspname NOT LIKE 'pg_toast%' ORDER BY nspname".to_string(),
        MetadataQuery::SchemaObjectCounts => "SELECT n.nspname AS name, (SELECT COUNT(*) FROM pg_catalog.pg_class c WHERE c.relnamespace = n.oid AND c.relkind IN ('r','p','v','m')) + (SELECT COUNT(*) FROM pg_catalog.pg_proc p WHERE p.pronamespace = n.oid) AS object_count FROM pg_catalog.pg_namespace n WHERE n.nspname NOT LIKE 'pg_temp%' AND n.nspname NOT LIKE 'pg_toast%' ORDER BY n.nspname".to_string(),
        MetadataQuery::Tables { schema } => pg_relations("'r','p'", schema.as_deref()),
        MetadataQuery::Views { schema } => pg_relations("'v','m'", schema.as_deref()),
        MetadataQuery::ProgrammableObjects { schema } => {
            // Type codes reuse the SQL Server vocabulary the consumer maps:
            // P = procedure, TF = set-returning function, FN = function.
            let filter = match schema.as_deref() {
                Some(s) => format!("n.nspname = '{}'", lit(s)),
                None => "n.nspname NOT IN ('pg_catalog', 'information_schema')".to_string(),
            };
            format!(
                "SELECT n.nspname AS schema_name, p.proname AS name, CASE WHEN p.prokind = 'p' THEN 'P' WHEN p.proretset THEN 'TF' ELSE 'FN' END AS type, NULL AS create_date, NULL AS modify_date FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace WHERE {filter} ORDER BY schema_name, name"
            )
        }
        MetadataQuery::ObjectInventory { schema } => pg_object_inventory(schema.as_deref()),
        MetadataQuery::Columns { schema, object } => format!(
            "SELECT c.column_name AS name, c.data_type AS type_name, COALESCE(c.character_maximum_length, -1) AS max_length, COALESCE(c.numeric_precision, 0) AS precision, COALESCE(c.numeric_scale, 0) AS scale, (c.is_nullable = 'YES')::int AS is_nullable, (c.is_identity = 'YES' OR {PG_SERIAL_DEFAULT})::int AS is_identity, (c.is_generated <> 'NEVER')::int AS is_computed FROM information_schema.columns c WHERE c.table_schema = '{}' AND c.table_name = '{}' ORDER BY c.ordinal_position",
            lit(schema),
            lit(object)
        ),
        MetadataQuery::AllColumns => format!("SELECT c.table_schema AS schema_name, c.table_name AS object_name, c.column_name, c.data_type AS type_name, (c.is_nullable = 'YES')::int AS is_nullable, (c.is_identity = 'YES' OR {PG_SERIAL_DEFAULT})::int AS is_identity FROM information_schema.columns c WHERE c.table_schema NOT IN ('pg_catalog', 'information_schema') ORDER BY schema_name, object_name, c.ordinal_position"),
        MetadataQuery::AllForeignKeys => "SELECT con.conname AS constraint_name, ns.nspname AS from_schema, cl.relname AS from_table, att.attname AS from_column, fns.nspname AS to_schema, fcl.relname AS to_table, fatt.attname AS to_column FROM pg_catalog.pg_constraint con JOIN pg_catalog.pg_class cl ON cl.oid = con.conrelid JOIN pg_catalog.pg_namespace ns ON ns.oid = cl.relnamespace JOIN pg_catalog.pg_class fcl ON fcl.oid = con.confrelid JOIN pg_catalog.pg_namespace fns ON fns.oid = fcl.relnamespace JOIN LATERAL unnest(con.conkey, con.confkey) WITH ORDINALITY AS k(attnum, fattnum, ord) ON true JOIN pg_catalog.pg_attribute att ON att.attrelid = con.conrelid AND att.attnum = k.attnum JOIN pg_catalog.pg_attribute fatt ON fatt.attrelid = con.confrelid AND fatt.attnum = k.fattnum WHERE con.contype = 'f' ORDER BY constraint_name, k.ord".to_string(),
        MetadataQuery::AllPrimaryKeys => "SELECT ns.nspname AS schema_name, cl.relname AS table_name, att.attname AS column_name, k.ord AS key_ordinal FROM pg_catalog.pg_constraint con JOIN pg_catalog.pg_class cl ON cl.oid = con.conrelid JOIN pg_catalog.pg_namespace ns ON ns.oid = cl.relnamespace JOIN LATERAL unnest(con.conkey) WITH ORDINALITY AS k(attnum, ord) ON true JOIN pg_catalog.pg_attribute att ON att.attrelid = con.conrelid AND att.attnum = k.attnum WHERE con.contype = 'p' ORDER BY schema_name, table_name, key_ordinal".to_string(),
        MetadataQuery::ProcedureParameters { schema, object } => format!(
            "SELECT COALESCE(p.parameter_name, '') AS name, p.data_type AS type_name, COALESCE(p.character_maximum_length, -1) AS max_length, COALESCE(p.numeric_precision, 0) AS precision, COALESCE(p.numeric_scale, 0) AS scale, (p.parameter_mode IN ('OUT', 'INOUT'))::int AS is_output, (p.parameter_default IS NOT NULL)::int AS has_default_value FROM information_schema.parameters p WHERE p.specific_schema = '{schema}' AND p.specific_name IN (SELECT r.specific_name FROM information_schema.routines r WHERE r.routine_schema = '{schema}' AND r.routine_name = '{object}') ORDER BY p.ordinal_position",
            schema = lit(schema),
            object = lit(object)
        ),
        MetadataQuery::Indexes { schema, table } => format!(
            "SELECT ic.relname AS name, am.amname AS type_desc, ix.indisunique::int AS is_unique, ix.indisprimary::int AS is_primary_key, pg_get_expr(ix.indpred, ix.indrelid) AS filter_definition, (SELECT substring(o FROM 'fillfactor=(\\d+)')::int FROM unnest(ic.reloptions) o WHERE o LIKE 'fillfactor=%' LIMIT 1) AS fill_factor FROM pg_catalog.pg_index ix JOIN pg_catalog.pg_class ic ON ic.oid = ix.indexrelid JOIN pg_catalog.pg_class tc ON tc.oid = ix.indrelid JOIN pg_catalog.pg_namespace ns ON ns.oid = tc.relnamespace JOIN pg_catalog.pg_am am ON am.oid = ic.relam WHERE ns.nspname = '{}' AND tc.relname = '{}' ORDER BY name",
            lit(schema),
            lit(table)
        ),
        MetadataQuery::DependentViews { schema, table } => {
            let target = if schema.is_empty() {
                table.clone()
            } else {
                format!("{schema}.{table}")
            };
            format!(
                "SELECT n.nspname, c.relname, (c.relkind = 'm')::int, 'CREATE ' || CASE WHEN c.relkind = 'm' THEN 'MATERIALIZED VIEW ' ELSE 'VIEW ' END || quote_ident(n.nspname) || '.' || quote_ident(c.relname) || ' AS ' || pg_get_viewdef(c.oid, true) FROM pg_catalog.pg_depend d JOIN pg_catalog.pg_rewrite rw ON rw.oid = d.objid JOIN pg_catalog.pg_class c ON c.oid = rw.ev_class JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE d.classid = 'pg_rewrite'::regclass AND d.refclassid = 'pg_class'::regclass AND d.refobjid = to_regclass('{target}') AND c.relkind IN ('v', 'm') AND c.oid <> to_regclass('{target}') ORDER BY 1, 2",
                target = lit(&target)
            )
        }
        MetadataQuery::Triggers { schema, table } => format!(
            "SELECT tg.tgname AS name, NULL AS create_date, NULL AS modify_date, (tg.tgenabled = 'D')::int AS is_disabled FROM pg_catalog.pg_trigger tg JOIN pg_catalog.pg_class tc ON tc.oid = tg.tgrelid JOIN pg_catalog.pg_namespace ns ON ns.oid = tc.relnamespace WHERE ns.nspname = '{}' AND tc.relname = '{}' AND NOT tg.tgisinternal ORDER BY name",
            lit(schema),
            lit(table)
        ),
        MetadataQuery::Constraints { schema, table } => format!(
            "SELECT con.conname AS name, CASE con.contype WHEN 'p' THEN 'PRIMARY_KEY_CONSTRAINT' WHEN 'u' THEN 'UNIQUE_CONSTRAINT' WHEN 'f' THEN 'FOREIGN_KEY_CONSTRAINT' WHEN 'c' THEN 'CHECK_CONSTRAINT' ELSE UPPER(con.contype::text) END AS type_desc FROM pg_catalog.pg_constraint con JOIN pg_catalog.pg_class cl ON cl.oid = con.conrelid JOIN pg_catalog.pg_namespace ns ON ns.oid = cl.relnamespace WHERE ns.nspname = '{}' AND cl.relname = '{}' ORDER BY name",
            lit(schema),
            lit(table)
        ),
        MetadataQuery::Statistics { schema, table } => format!(
            "SELECT sx.stxname AS name, 0 AS auto_created, 1 AS user_created, 0 AS no_recompute, NULL AS updated_at FROM pg_catalog.pg_statistic_ext sx JOIN pg_catalog.pg_class cl ON cl.oid = sx.stxrelid JOIN pg_catalog.pg_namespace ns ON ns.oid = cl.relnamespace WHERE ns.nspname = '{}' AND cl.relname = '{}' ORDER BY name",
            lit(schema),
            lit(table)
        ),
        MetadataQuery::Types { schema } => {
            let schema_filter = schema
                .as_deref()
                .map(|s| format!(" AND n.nspname = '{}'", lit(s)))
                .unwrap_or_default();
            format!(
                "SELECT n.nspname AS schema_name, t.typname AS name, CASE t.typtype WHEN 'd' THEN pg_catalog.format_type(t.typbasetype, t.typtypmod) WHEN 'e' THEN 'enum' WHEN 'c' THEN 'composite' ELSE t.typtype::text END AS base_type, -1 AS max_length, 0 AS precision, 0 AS scale, (NOT t.typnotnull)::int AS is_nullable FROM pg_catalog.pg_type t JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace WHERE t.typtype IN ('d', 'e', 'c') AND n.nspname NOT IN ('pg_catalog', 'information_schema') AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_class c WHERE c.oid = t.typrelid AND c.relkind <> 'c'){schema_filter} ORDER BY schema_name, name"
            )
        }
        MetadataQuery::DatabaseTriggers { schema } => {
            let event_triggers = "SELECT CAST(NULL AS text) AS schema_name, evt.evtname AS name, CAST(NULL AS text) AS parent_name, (evt.evtenabled = 'D')::int AS is_disabled FROM pg_catalog.pg_event_trigger evt";
            match schema.as_deref() {
                Some(s) => format!(
                    "{event_triggers} UNION ALL SELECT n.nspname AS schema_name, tg.tgname AS name, c.relname AS parent_name, (tg.tgenabled = 'D')::int AS is_disabled FROM pg_catalog.pg_trigger tg JOIN pg_catalog.pg_class c ON c.oid = tg.tgrelid JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE NOT tg.tgisinternal AND n.nspname = '{s}' ORDER BY schema_name, name",
                    s = lit(s)
                ),
                None => format!("{event_triggers} ORDER BY name"),
            }
        }
        MetadataQuery::Sequences { schema } => {
            let schema_filter = schema
                .as_deref()
                .map(|s| format!(" AND schemaname = '{s}'", s = lit(s)))
                .unwrap_or_default();
            format!(
                "SELECT schemaname AS schema_name, sequencename AS name, data_type, start_value, increment_by AS increment, last_value AS current_value FROM pg_catalog.pg_sequences WHERE 1 = 1{schema_filter} ORDER BY schema_name, name"
            )
        }
        MetadataQuery::Synonyms { .. } => {
            "SELECT '' AS schema_name, '' AS name, '' AS base_object_name WHERE 1 = 0".to_string()
        }
        MetadataQuery::Extensions => {
            "SELECT e.extname AS name, e.extversion AS version, n.nspname AS schema_name, ae.comment AS description FROM pg_catalog.pg_extension e JOIN pg_catalog.pg_namespace n ON n.oid = e.extnamespace LEFT JOIN pg_catalog.pg_available_extensions ae ON ae.name = e.extname ORDER BY name".to_string()
        }
        MetadataQuery::AvailableExtensions => {
            "SELECT name, default_version AS version, comment AS description, (installed_version IS NOT NULL)::int AS installed FROM pg_catalog.pg_available_extensions ORDER BY name".to_string()
        }
        MetadataQuery::Events { .. } => {
            "SELECT '' AS schema_name, '' AS name, '' AS status, '' AS event_type, '' AS interval_or_at, '' AS definition WHERE 1 = 0".to_string()
        }
        MetadataQuery::MaterializedViews { schema } => {
            let schema_filter = schema
                .as_deref()
                .map(|s| format!(" AND schemaname = '{s}'", s = lit(s)))
                .unwrap_or_default();
            format!(
                "SELECT schemaname AS schema_name, matviewname AS name, ispopulated::int AS is_populated FROM pg_catalog.pg_matviews WHERE 1 = 1{schema_filter} ORDER BY schema_name, name"
            )
        }
        MetadataQuery::TriggerDefinition { schema, name } => format!(
            "SELECT pg_catalog.pg_get_triggerdef(tg.oid, true) AS definition FROM pg_catalog.pg_trigger tg JOIN pg_catalog.pg_class c ON c.oid = tg.tgrelid JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = '{schema}' AND tg.tgname = '{name}' AND NOT tg.tgisinternal",
            schema = lit(schema),
            name = lit(name)
        ),
        MetadataQuery::ConstraintDefinition { schema, table, name } => format!(
            "SELECT pg_catalog.pg_get_constraintdef(c.oid, true) AS definition FROM pg_catalog.pg_constraint c JOIN pg_catalog.pg_class cl ON cl.oid = c.conrelid JOIN pg_catalog.pg_namespace n ON n.oid = cl.relnamespace WHERE n.nspname = '{schema}' AND cl.relname = '{table}' AND c.conname = '{name}'",
            schema = lit(schema),
            table = lit(table),
            name = lit(name)
        ),
        // Templates (template0/template1) are listed too so the server explorer
        // can group them under "System Databases" per the placement preference,
        // mirroring SQL Server's shipped databases.
        MetadataQuery::Databases => "SELECT datname AS name, 'ONLINE' AS state_desc, NULL AS compatibility_level FROM pg_catalog.pg_database ORDER BY datname".to_string(),
        MetadataQuery::ServerCapability => "SELECT current_setting('server_version') AS product_version, 'PostgreSQL' AS edition, NULL AS engine_edition, NULL AS product_level, NULL AS product_update_level, NULL AS machine_name, 0 AS integrated_security_only".to_string(),
        MetadataQuery::TypeDetail { schema, type_name } => format!(
            "SELECT n.nspname AS schema_name, t.typname AS name, CASE t.typtype WHEN 'd' THEN pg_catalog.format_type(t.typbasetype, t.typtypmod) WHEN 'e' THEN 'enum' WHEN 'c' THEN 'composite' ELSE t.typtype::text END AS base_type, -1 AS max_length, 0 AS precision, 0 AS scale, (NOT t.typnotnull)::int AS is_nullable FROM pg_catalog.pg_type t JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace WHERE t.typtype IN ('d', 'e', 'c') AND n.nspname = '{}' AND t.typname = '{}'",
            lit(schema),
            lit(type_name)
        ),
        MetadataQuery::TableGeneral { schema, table } => format!(
            "SELECT n.nspname AS schema_name, c.relname AS name, NULL AS create_date, NULL AS modify_date, GREATEST(c.reltuples, 0)::bigint AS row_count FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = '{}' AND c.relname = '{}'",
            lit(schema),
            lit(table)
        ),
        MetadataQuery::DatabaseGeneral { database } => format!(
            "SELECT d.datname AS name, CASE WHEN d.datallowconn THEN 'ONLINE' ELSE 'OFFLINE' END AS state_desc, d.datcollate AS collation_name, NULL AS recovery_model, NULL AS compatibility_level, NULL AS create_date, ROUND(pg_catalog.pg_database_size(d.datname) / 1024.0 / 1024.0, 2) AS size_mb, pg_catalog.pg_get_userbyid(d.datdba) AS owner FROM pg_catalog.pg_database d WHERE d.datname = '{}'",
            lit(database)
        ),
        MetadataQuery::ColumnGeneral { schema, table, column } => format!(
            "SELECT c.table_schema AS schema_name, c.table_name, c.column_name AS name, c.ordinal_position AS ordinal, c.data_type, c.character_maximum_length AS max_length, c.numeric_precision AS precision, c.numeric_scale AS scale, (c.is_nullable = 'YES')::int AS is_nullable, c.column_default AS default_definition, (c.is_identity = 'YES')::int AS is_identity, (c.is_generated = 'ALWAYS')::int AS is_computed, c.collation_name FROM information_schema.columns c WHERE c.table_schema = '{}' AND c.table_name = '{}' AND c.column_name = '{}'",
            lit(schema),
            lit(table),
            lit(column)
        ),
        MetadataQuery::ObjectGeneral { schema, object } => format!(
            "SELECT schema_name, name, type_desc, create_date, modify_date FROM (SELECT n.nspname AS schema_name, c.relname AS name, CASE c.relkind WHEN 'r' THEN 'USER_TABLE' WHEN 'p' THEN 'USER_TABLE' WHEN 'v' THEN 'VIEW' WHEN 'm' THEN 'MATERIALIZED_VIEW' ELSE UPPER(c.relkind::text) END AS type_desc, NULL AS create_date, NULL AS modify_date FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = '{s}' AND c.relname = '{o}' UNION ALL SELECT n.nspname, p.proname, CASE WHEN p.prokind = 'p' THEN 'SQL_STORED_PROCEDURE' ELSE 'SQL_SCALAR_FUNCTION' END, NULL, NULL FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace WHERE n.nspname = '{s}' AND p.proname = '{o}') objects LIMIT 1",
            s = lit(schema),
            o = lit(object)
        ),
        MetadataQuery::IndexGeneral { schema, table, index } => format!(
            "SELECT ic.relname AS name, am.amname AS type_desc, ix.indisunique::int AS is_unique, ix.indisprimary::int AS is_primary_key, 0 AS is_disabled, pg_get_expr(ix.indpred, ix.indrelid) AS filter_definition, NULL AS fill_factor FROM pg_catalog.pg_index ix JOIN pg_catalog.pg_class ic ON ic.oid = ix.indexrelid JOIN pg_catalog.pg_class tc ON tc.oid = ix.indrelid JOIN pg_catalog.pg_namespace ns ON ns.oid = tc.relnamespace JOIN pg_catalog.pg_am am ON am.oid = ic.relam WHERE ns.nspname = '{}' AND tc.relname = '{}' AND ic.relname = '{}'",
            lit(schema),
            lit(table),
            lit(index)
        ),
        MetadataQuery::IndexColumns { schema, table, index } => format!(
            "SELECT COALESCE(a.attname, pg_catalog.pg_get_indexdef(ix.indexrelid, k.ord::int, true)) AS name, k.ord AS key_ordinal, {PG_INDEX_KEY_DESC} AS is_descending_key, CASE WHEN k.ord > ix.indnkeyatts THEN 1 ELSE 0 END AS is_included_column FROM pg_catalog.pg_index ix JOIN pg_catalog.pg_class ic ON ic.oid = ix.indexrelid JOIN pg_catalog.pg_class tc ON tc.oid = ix.indrelid JOIN pg_catalog.pg_namespace ns ON ns.oid = tc.relnamespace JOIN LATERAL unnest(ix.indkey) WITH ORDINALITY AS k(attnum, ord) ON true LEFT JOIN pg_catalog.pg_attribute a ON a.attrelid = tc.oid AND a.attnum = k.attnum AND k.attnum <> 0 WHERE ns.nspname = '{}' AND tc.relname = '{}' AND ic.relname = '{}' ORDER BY k.ord",
            lit(schema),
            lit(table),
            lit(index)
        ),
        MetadataQuery::ColumnDefaults { schema, table } => format!(
            "SELECT c.column_name, c.column_default, NULL AS constraint_name FROM information_schema.columns c WHERE c.table_schema = '{}' AND c.table_name = '{}' AND c.column_default IS NOT NULL ORDER BY c.ordinal_position",
            lit(schema),
            lit(table)
        ),
        MetadataQuery::TableIndexColumns { schema, table } => format!(
            "SELECT ic.relname AS index_name, COALESCE(a.attname, pg_catalog.pg_get_indexdef(ix.indexrelid, k.ord::int, true)) AS column_name, k.ord AS ordinal, CASE WHEN k.ord > ix.indnkeyatts THEN 1 ELSE 0 END AS is_included, {PG_INDEX_KEY_DESC} AS is_descending, CASE WHEN k.attnum = 0 THEN 1 ELSE 0 END AS is_expression, {PG_INDEX_KEY_HAS_OPTIONS} AS has_options FROM pg_catalog.pg_index ix JOIN pg_catalog.pg_class ic ON ic.oid = ix.indexrelid JOIN pg_catalog.pg_class tc ON tc.oid = ix.indrelid JOIN pg_catalog.pg_namespace ns ON ns.oid = tc.relnamespace JOIN LATERAL unnest(ix.indkey) WITH ORDINALITY AS k(attnum, ord) ON true LEFT JOIN pg_catalog.pg_attribute a ON a.attrelid = tc.oid AND a.attnum = k.attnum AND k.attnum <> 0 WHERE ns.nspname = '{}' AND tc.relname = '{}' ORDER BY index_name, is_included, ordinal",
            lit(schema),
            lit(table)
        ),
        MetadataQuery::StatisticGeneral { schema, table, statistic } => format!(
            "SELECT sx.stxname AS name, 0 AS auto_created, 1 AS user_created, 0 AS no_recompute, NULL AS updated_at FROM pg_catalog.pg_statistic_ext sx JOIN pg_catalog.pg_class cl ON cl.oid = sx.stxrelid JOIN pg_catalog.pg_namespace ns ON ns.oid = cl.relnamespace WHERE ns.nspname = '{}' AND cl.relname = '{}' AND sx.stxname = '{}'",
            lit(schema),
            lit(table),
            lit(statistic)
        ),
        MetadataQuery::StatisticColumns { schema, table, statistic } => format!(
            "SELECT a.attname AS name FROM pg_catalog.pg_statistic_ext sx JOIN pg_catalog.pg_class cl ON cl.oid = sx.stxrelid JOIN pg_catalog.pg_namespace ns ON ns.oid = cl.relnamespace JOIN LATERAL unnest(sx.stxkeys) WITH ORDINALITY AS k(attnum, ord) ON true JOIN pg_catalog.pg_attribute a ON a.attrelid = cl.oid AND a.attnum = k.attnum WHERE ns.nspname = '{}' AND cl.relname = '{}' AND sx.stxname = '{}' ORDER BY k.ord",
            lit(schema),
            lit(table),
            lit(statistic)
        ),
        MetadataQuery::ObjectPermissions { schema, object } => format!(
            "SELECT DISTINCT a.privilege_type AS permission_name, 'GRANT' AS state_desc, 'OBJECT' AS class_desc FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace CROSS JOIN LATERAL pg_catalog.aclexplode(COALESCE(c.relacl, pg_catalog.acldefault('r', c.relowner))) a WHERE n.nspname = '{}' AND c.relname = '{}' AND (a.grantee = 0 OR a.grantee = (SELECT r.oid FROM pg_catalog.pg_roles r WHERE r.rolname = CURRENT_USER)) ORDER BY permission_name",
            lit(schema),
            lit(object)
        ),
        // PostgreSQL has no server-permission catalog; the role attributes are
        // the equivalent grants, reported one per row like SQL Server's
        // `fn_my_permissions`.
        MetadataQuery::ServerPermissions => "SELECT x.permission_name, 'GRANT' AS state_desc, 'SERVER' AS class_desc FROM pg_catalog.pg_roles r CROSS JOIN LATERAL (VALUES ('SUPERUSER', r.rolsuper), ('CREATEDB', r.rolcreatedb), ('CREATEROLE', r.rolcreaterole), ('REPLICATION', r.rolreplication), ('BYPASSRLS', r.rolbypassrls), ('LOGIN', r.rolcanlogin)) AS x(permission_name, granted) WHERE r.rolname = CURRENT_USER AND x.granted ORDER BY 1".to_string(),
        MetadataQuery::DatabasePermissions { database } => format!(
            "SELECT x.permission_name, 'GRANT' AS state_desc, 'DATABASE' AS class_desc FROM (VALUES ('CONNECT'), ('CREATE'), ('TEMPORARY')) AS x(permission_name) WHERE pg_catalog.has_database_privilege(CURRENT_USER, '{}', x.permission_name) ORDER BY 1",
            lit(database)
        ),
        // The value is `current_setting(name)` — SHOW's form, units folded
        // in ("128MB", not "16384 8kB") — so the edit strip prefills, and the
        // review sheet's revert value is, input ALTER SYSTEM accepts.
        MetadataQuery::ServerConfiguration => "SELECT name, current_setting(name) AS value, source, (context IN ('user', 'superuser', 'sighup', 'superuser-backend', 'backend'))::int AS is_dynamic FROM pg_catalog.pg_settings ORDER BY name".to_string(),
        // Cluster-wide totals: every counter is a sum over pg_stat_database
        // (all databases) plus the bgwriter totals. Columns are chosen from
        // the set present across supported versions (PG 12+), so the query
        // never names a column a newer release moved to another view.
        MetadataQuery::ServerStatus => "SELECT name, value, source FROM (SELECT 'connections' AS name, SUM(numbackends)::text AS value, 'pg_stat_database' AS source FROM pg_catalog.pg_stat_database UNION ALL SELECT 'xact_committed', SUM(xact_commit)::text, 'pg_stat_database' FROM pg_catalog.pg_stat_database UNION ALL SELECT 'xact_rolled_back', SUM(xact_rollback)::text, 'pg_stat_database' FROM pg_catalog.pg_stat_database UNION ALL SELECT 'blocks_read', SUM(blks_read)::text, 'pg_stat_database' FROM pg_catalog.pg_stat_database UNION ALL SELECT 'blocks_hit', SUM(blks_hit)::text, 'pg_stat_database' FROM pg_catalog.pg_stat_database UNION ALL SELECT 'tuples_returned', SUM(tup_returned)::text, 'pg_stat_database' FROM pg_catalog.pg_stat_database UNION ALL SELECT 'tuples_fetched', SUM(tup_fetched)::text, 'pg_stat_database' FROM pg_catalog.pg_stat_database UNION ALL SELECT 'tuples_inserted', SUM(tup_inserted)::text, 'pg_stat_database' FROM pg_catalog.pg_stat_database UNION ALL SELECT 'tuples_updated', SUM(tup_updated)::text, 'pg_stat_database' FROM pg_catalog.pg_stat_database UNION ALL SELECT 'tuples_deleted', SUM(tup_deleted)::text, 'pg_stat_database' FROM pg_catalog.pg_stat_database UNION ALL SELECT 'deadlocks', SUM(deadlocks)::text, 'pg_stat_database' FROM pg_catalog.pg_stat_database UNION ALL SELECT 'temp_files', SUM(temp_files)::text, 'pg_stat_database' FROM pg_catalog.pg_stat_database UNION ALL SELECT 'temp_bytes', SUM(temp_bytes)::text, 'pg_stat_database' FROM pg_catalog.pg_stat_database UNION ALL SELECT 'buffers_clean', SUM(buffers_clean)::text, 'pg_stat_bgwriter' FROM pg_catalog.pg_stat_bgwriter UNION ALL SELECT 'maxwritten_clean', SUM(maxwritten_clean)::text, 'pg_stat_bgwriter' FROM pg_catalog.pg_stat_bgwriter UNION ALL SELECT 'buffers_allocated', SUM(buffers_alloc)::text, 'pg_stat_bgwriter' FROM pg_catalog.pg_stat_bgwriter) counters ORDER BY source, name".to_string(),
        // Files are a cluster-level concept in PostgreSQL and their paths are
        // superuser-only; report the database itself as one storage row.
        MetadataQuery::DatabaseFiles { database } => format!(
            "SELECT d.datname AS logical_name, 'DATABASE' AS type_desc, NULL AS physical_name, ROUND(pg_catalog.pg_database_size(d.datname) / 1024.0 / 1024.0, 2) AS size_mb, NULL AS max_size_mb, NULL AS growth FROM pg_catalog.pg_database d WHERE d.datname = '{}'",
            lit(database)
        ),
        // Single-column, single-row shape matching SQL Server's
        // OBJECT_DEFINITION: the reconstructed CREATE source, or no rows when
        // the object has none (tables here — their DDL is generated from
        // catalog metadata instead).
        MetadataQuery::ObjectDefinition { schema, object } => {
            postgres_object_definition_sql(schema, object, DatabaseType::Postgres)
        }
    }
}

fn pg_object_inventory(schema: Option<&str>) -> String {
    let ns = match schema {
        Some(s) => format!("n.nspname = '{}'", lit(s)),
        None => "n.nspname NOT IN ('pg_catalog', 'information_schema') AND n.nspname NOT LIKE 'pg_toast%' AND n.nspname NOT LIKE 'pg_temp%'".to_string(),
    };
    format!(
        "SELECT kind, schema_name, name, row_estimate, size_kb, create_date, modify_date, comment FROM (\
         SELECT CASE c.relkind WHEN 'r' THEN 'TABLE' WHEN 'p' THEN 'TABLE' WHEN 'v' THEN 'VIEW' WHEN 'm' THEN 'MATERIALIZED VIEW' END AS kind, n.nspname AS schema_name, c.relname AS name, GREATEST(c.reltuples, 0)::bigint AS row_estimate, (pg_total_relation_size(c.oid) / 1024)::bigint AS size_kb, NULL::timestamp AS create_date, NULL::timestamp AS modify_date, obj_description(c.oid, 'pg_class') AS comment FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE c.relkind IN ('r','p','v','m') AND {ns} \
         UNION ALL \
         SELECT CASE p.prokind WHEN 'p' THEN 'PROCEDURE' ELSE 'FUNCTION' END, n.nspname, p.proname, NULL::bigint, NULL::bigint, NULL::timestamp, NULL::timestamp, obj_description(p.oid, 'pg_proc') FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace WHERE {ns} \
         UNION ALL \
         SELECT 'TRIGGER', n.nspname, t.tgname, NULL::bigint, NULL::bigint, NULL::timestamp, NULL::timestamp, obj_description(t.oid, 'pg_trigger') FROM pg_catalog.pg_trigger t JOIN pg_catalog.pg_class c ON c.oid = t.tgrelid JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE NOT t.tgisinternal AND {ns}\
         ) inventory ORDER BY kind, schema_name, name"
    )
}

fn pg_relations(relkinds: &str, schema: Option<&str>) -> String {
    let filter = match schema {
        // A system schema explored on its own (via the System Schemas folder)
        // still lists its relations; the unscoped/flat listing excludes the
        // catalog schemas so system tables aren't mixed in with the user's,
        // matching how SQL Server's flat listing draws only from `sys.objects`.
        Some(s) => format!("n.nspname = '{}'", lit(s)),
        None => "n.nspname NOT IN ('pg_catalog', 'information_schema') AND n.nspname NOT LIKE 'pg_temp%' AND n.nspname NOT LIKE 'pg_toast%'".to_string(),
    };
    format!(
        "SELECT n.nspname AS schema_name, c.relname AS name, NULL AS create_date, NULL AS modify_date FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE c.relkind IN ({relkinds}) AND {filter} ORDER BY schema_name, name"
    )
}

// ---------------------------------------------------------------------------
// MySQL / MariaDB — information_schema, database-as-namespace
// ---------------------------------------------------------------------------

/// The schema scope for a MySQL query: the explicit schema when the caller
/// passed one (MySQL "schemas" are databases), else the connection's current
/// database.
/// MySQL/MariaDB (joined as `p` to `information_schema.triggers t`): the
/// trigger that fires just before `t` for the same table, timing and event.
/// Triggers sharing those fire in ACTION_ORDER, and a plain CREATE TRIGGER
/// goes last, so a re-created trigger must say which one it `FOLLOWS`.
const MYSQL_TRIGGER_PREVIOUS: &str = "LEFT JOIN information_schema.triggers p ON p.trigger_schema = t.trigger_schema AND p.event_object_schema = t.event_object_schema AND p.event_object_table = t.event_object_table AND p.action_timing = t.action_timing AND p.event_manipulation = t.event_manipulation AND p.action_order = t.action_order - 1";

/// ` FOLLOWS `previous`` (or nothing for the first trigger), over
/// [`MYSQL_TRIGGER_PREVIOUS`].
const MYSQL_TRIGGER_FOLLOWS: &str =
    "IFNULL(CONCAT(' FOLLOWS `', REPLACE(p.trigger_name, '`', '``'), '`'), '')";

fn mysql_schema_scope(schema: Option<&str>) -> String {
    match schema {
        Some(s) => format!("'{}'", mysql_lit(s)),
        None => "DATABASE()".to_string(),
    }
}

fn mysql_sql(query: &MetadataQuery) -> String {
    // `information_schema.*_privileges.grantee` is the quoted `'user'@'host'`
    // form, which CURRENT_USER() returns unquoted — rebuild it to compare.
    let current_grantee = "CONCAT('''', SUBSTRING_INDEX(CURRENT_USER(), '@', 1), '''@''', SUBSTRING_INDEX(CURRENT_USER(), '@', -1), '''')";
    match query {
        MetadataQuery::Schemas => "SELECT DATABASE() AS name".to_string(),
        MetadataQuery::SchemaObjectCounts => "SELECT DATABASE() AS name, (SELECT COUNT(*) FROM information_schema.tables t WHERE t.table_schema = DATABASE()) + (SELECT COUNT(*) FROM information_schema.routines r WHERE r.routine_schema = DATABASE()) AS object_count".to_string(),
        MetadataQuery::Tables { schema } => format!(
            "SELECT t.table_schema AS schema_name, t.table_name AS name, t.create_time AS create_date, t.update_time AS modify_date FROM information_schema.tables t WHERE t.table_schema = {} AND t.table_type = 'BASE TABLE' ORDER BY schema_name, name",
            mysql_schema_scope(schema.as_deref())
        ),
        MetadataQuery::Views { schema } => format!(
            "SELECT t.table_schema AS schema_name, t.table_name AS name, t.create_time AS create_date, t.update_time AS modify_date FROM information_schema.tables t WHERE t.table_schema = {} AND t.table_type = 'VIEW' ORDER BY schema_name, name",
            mysql_schema_scope(schema.as_deref())
        ),
        MetadataQuery::ProgrammableObjects { schema } => {
            let scope = mysql_schema_scope(schema.as_deref());
            format!(
                "SELECT r.routine_schema AS schema_name, r.routine_name AS name, CASE r.routine_type WHEN 'PROCEDURE' THEN 'P' ELSE 'FN' END AS type, r.created AS create_date, r.last_altered AS modify_date FROM information_schema.routines r WHERE r.routine_schema = {scope} UNION ALL SELECT t.trigger_schema, t.trigger_name, 'TR', t.created, NULL FROM information_schema.triggers t WHERE t.trigger_schema = {scope} ORDER BY 1, 2"
            )
        }
        MetadataQuery::ObjectInventory { schema } => {
            let scope = mysql_schema_scope(schema.as_deref());
            format!(
                "SELECT kind, schema_name, name, row_estimate, size_kb, create_date, modify_date, comment FROM (\
                 SELECT CASE t.table_type WHEN 'VIEW' THEN 'VIEW' ELSE 'TABLE' END AS kind, t.table_schema AS schema_name, t.table_name AS name, t.table_rows AS row_estimate, (t.data_length + t.index_length) DIV 1024 AS size_kb, t.create_time AS create_date, t.update_time AS modify_date, IF(t.table_type = 'VIEW', NULL, t.table_comment) AS comment FROM information_schema.tables t WHERE t.table_schema = {scope} \
                 UNION ALL \
                 SELECT r.routine_type, r.routine_schema, r.routine_name, NULL, NULL, r.created, r.last_altered, r.routine_comment FROM information_schema.routines r WHERE r.routine_schema = {scope} \
                 UNION ALL \
                 SELECT 'TRIGGER', t.trigger_schema, t.trigger_name, NULL, NULL, t.created, NULL, NULL FROM information_schema.triggers t WHERE t.trigger_schema = {scope}\
                 ) inventory ORDER BY kind, schema_name, name"
            )
        }
        MetadataQuery::Columns { schema, object } => format!(
            "SELECT c.column_name AS name, c.data_type AS type_name, COALESCE(c.character_maximum_length, -1) AS max_length, COALESCE(c.numeric_precision, 0) AS num_precision, COALESCE(c.numeric_scale, 0) AS num_scale, IF(c.is_nullable = 'YES', 1, 0) AS is_nullable, IF(c.extra LIKE '%auto_increment%', 1, 0) AS is_identity, IF(COALESCE(c.generation_expression, '') <> '', 1, 0) AS is_computed FROM information_schema.columns c WHERE c.table_schema = '{}' AND c.table_name = '{}' ORDER BY c.ordinal_position",
            mysql_lit(schema),
            mysql_lit(object)
        ),
        MetadataQuery::AllColumns => "SELECT c.table_schema AS schema_name, c.table_name AS object_name, c.column_name, c.data_type AS type_name, IF(c.is_nullable = 'YES', 1, 0) AS is_nullable, IF(c.extra LIKE '%auto_increment%', 1, 0) AS is_identity FROM information_schema.columns c WHERE c.table_schema = DATABASE() ORDER BY schema_name, object_name, c.ordinal_position".to_string(),
        MetadataQuery::AllForeignKeys => "SELECT kcu.constraint_name, kcu.table_schema AS from_schema, kcu.table_name AS from_table, kcu.column_name AS from_column, kcu.referenced_table_schema AS to_schema, kcu.referenced_table_name AS to_table, kcu.referenced_column_name AS to_column FROM information_schema.key_column_usage kcu WHERE kcu.table_schema = DATABASE() AND kcu.referenced_table_name IS NOT NULL ORDER BY kcu.constraint_name, kcu.ordinal_position".to_string(),
        MetadataQuery::AllPrimaryKeys => "SELECT kcu.table_schema AS schema_name, kcu.table_name, kcu.column_name, kcu.ordinal_position AS key_ordinal FROM information_schema.key_column_usage kcu WHERE kcu.table_schema = DATABASE() AND kcu.constraint_name = 'PRIMARY' ORDER BY schema_name, kcu.table_name, key_ordinal".to_string(),
        MetadataQuery::ProcedureParameters { schema, object } => format!(
            "SELECT COALESCE(p.parameter_name, '') AS name, p.data_type AS type_name, COALESCE(p.character_maximum_length, -1) AS max_length, COALESCE(p.numeric_precision, 0) AS num_precision, COALESCE(p.numeric_scale, 0) AS num_scale, IF(p.parameter_mode IN ('OUT', 'INOUT'), 1, 0) AS is_output, 0 AS has_default_value FROM information_schema.parameters p WHERE p.specific_schema = '{}' AND p.specific_name = '{}' AND p.parameter_mode IS NOT NULL ORDER BY p.ordinal_position",
            mysql_lit(schema),
            mysql_lit(object)
        ),
        MetadataQuery::Indexes { schema, table } => format!(
            "SELECT s.index_name AS name, s.index_type AS type_desc, IF(MIN(s.non_unique) = 0, 1, 0) AS is_unique, IF(s.index_name = 'PRIMARY', 1, 0) AS is_primary_key, NULL AS filter_definition, NULL AS fill_factor FROM information_schema.statistics s WHERE s.table_schema = '{}' AND s.table_name = '{}' GROUP BY s.index_name, s.index_type ORDER BY name",
            mysql_lit(schema),
            mysql_lit(table)
        ),
        MetadataQuery::DependentViews { schema, table } => format!(
            "SELECT v.TABLE_SCHEMA, v.TABLE_NAME, 0, CONCAT('CREATE OR REPLACE VIEW ', CONCAT('`', v.TABLE_SCHEMA, '`.`', v.TABLE_NAME, '`'), ' AS ', v.VIEW_DEFINITION) FROM information_schema.VIEW_TABLE_USAGE u JOIN information_schema.VIEWS v ON v.TABLE_SCHEMA = u.VIEW_SCHEMA AND v.TABLE_NAME = u.VIEW_NAME WHERE u.TABLE_SCHEMA = '{}' AND u.TABLE_NAME = '{}' ORDER BY 1, 2",
            mysql_lit(schema),
            mysql_lit(table)
        ),
        MetadataQuery::Triggers { schema, table } => format!(
            "SELECT t.trigger_name AS name, t.created AS create_date, NULL AS modify_date, 0 AS is_disabled FROM information_schema.triggers t WHERE t.trigger_schema = '{}' AND t.event_object_table = '{}' ORDER BY name",
            mysql_lit(schema),
            mysql_lit(table)
        ),
        MetadataQuery::Constraints { schema, table } => format!(
            "SELECT tc.constraint_name AS name, tc.constraint_type AS type_desc FROM information_schema.table_constraints tc WHERE tc.table_schema = '{}' AND tc.table_name = '{}' ORDER BY name",
            mysql_lit(schema),
            mysql_lit(table)
        ),
        // MySQL has no user-defined types; an empty set keeps the explorer
        // folder honest without erroring.
        MetadataQuery::Types { .. } => "SELECT '' AS schema_name, '' AS name, '' AS base_type, -1 AS max_length, 0 AS num_precision, 0 AS num_scale, 0 AS is_nullable FROM DUAL WHERE 1 = 0".to_string(),
        MetadataQuery::DatabaseTriggers { schema } => format!(
            "SELECT t.trigger_schema AS schema_name, t.trigger_name AS name, t.event_object_table AS parent_name, 0 AS is_disabled FROM information_schema.triggers t WHERE t.trigger_schema = {} ORDER BY schema_name, name",
            mysql_schema_scope(schema.as_deref())
        ),
        MetadataQuery::Sequences { schema } => format!(
            "SELECT t.table_schema AS schema_name, t.table_name AS name, COALESCE(seq.data_type, 'bigint') AS data_type, seq.start_value, seq.increment, seq.last_value AS current_value FROM information_schema.tables t LEFT JOIN information_schema.sequences seq ON seq.sequence_schema = t.table_schema AND seq.sequence_name = t.table_name WHERE t.table_type = 'SEQUENCE' AND t.table_schema = {} ORDER BY schema_name, name",
            mysql_schema_scope(schema.as_deref())
        ),
        MetadataQuery::Synonyms { .. } => "SELECT '' AS schema_name, '' AS name, '' AS base_object_name FROM DUAL WHERE 1 = 0".to_string(),
        MetadataQuery::Extensions => "SELECT '' AS name, '' AS version, '' AS schema_name, '' AS description FROM DUAL WHERE 1 = 0".to_string(),
        MetadataQuery::AvailableExtensions => "SELECT '' AS name, '' AS version, '' AS description, 0 AS installed FROM DUAL WHERE 1 = 0".to_string(),
        MetadataQuery::Events { schema } => format!(
            "SELECT e.event_schema AS schema_name, e.event_name AS name, e.status, e.event_type, COALESCE(CONCAT(e.interval_value, ' ', e.interval_field), CAST(e.execute_at AS CHAR)) AS interval_or_at, e.event_definition AS definition FROM information_schema.events e WHERE e.event_schema = {} ORDER BY schema_name, name",
            mysql_schema_scope(schema.as_deref())
        ),
        MetadataQuery::MaterializedViews { .. } => "SELECT '' AS schema_name, '' AS name, 0 AS is_populated FROM DUAL WHERE 1 = 0".to_string(),
        MetadataQuery::TriggerDefinition { schema, name } => format!(
            "SELECT CONCAT('CREATE TRIGGER `', REPLACE(t.trigger_name, '`', '``'), '` ', t.action_timing, ' ', t.event_manipulation, ' ON `', REPLACE(t.event_object_table, '`', '``'), '` FOR EACH ROW', {MYSQL_TRIGGER_FOLLOWS}, ' ', t.action_statement) AS definition FROM information_schema.triggers t {MYSQL_TRIGGER_PREVIOUS} WHERE t.trigger_schema = '{schema}' AND t.trigger_name = '{name}'",
            schema = mysql_lit(schema),
            name = mysql_lit(name)
        ),
        MetadataQuery::ConstraintDefinition { schema, table, name } => format!(
            "SELECT cc.check_clause AS definition FROM information_schema.check_constraints cc JOIN information_schema.table_constraints tc ON tc.constraint_schema = cc.constraint_schema AND tc.constraint_name = cc.constraint_name WHERE tc.table_schema = '{schema}' AND tc.table_name = '{table}' AND tc.constraint_name = '{name}' UNION ALL SELECT GROUP_CONCAT(kcu.column_name ORDER BY kcu.ordinal_position SEPARATOR ', ') FROM information_schema.table_constraints tc JOIN information_schema.key_column_usage kcu ON kcu.constraint_schema = tc.constraint_schema AND kcu.constraint_name = tc.constraint_name AND kcu.table_schema = tc.table_schema AND kcu.table_name = tc.table_name WHERE tc.constraint_type = 'UNIQUE' AND tc.table_schema = '{schema}' AND tc.table_name = '{table}' AND tc.constraint_name = '{name}' GROUP BY tc.constraint_name",
            schema = mysql_lit(schema),
            table = mysql_lit(table),
            name = mysql_lit(name)
        ),
        MetadataQuery::Databases => "SELECT schema_name AS name, 'ONLINE' AS state_desc, NULL AS compatibility_level FROM information_schema.schemata ORDER BY schema_name".to_string(),
        MetadataQuery::ServerCapability => "SELECT VERSION() AS product_version, 'MySQL' AS edition, NULL AS engine_edition, NULL AS product_level, NULL AS product_update_level, @@hostname AS machine_name, 0 AS integrated_security_only".to_string(),
        // MySQL has no user-defined types; the detail page mirrors the empty
        // Types list.
        MetadataQuery::TypeDetail { .. } => "SELECT '' AS schema_name, '' AS name, '' AS base_type, -1 AS max_length, 0 AS num_precision, 0 AS num_scale, 0 AS is_nullable FROM DUAL WHERE 1 = 0".to_string(),
        MetadataQuery::TableGeneral { schema, table } => format!(
            "SELECT t.table_schema AS schema_name, t.table_name AS name, t.create_time AS create_date, t.update_time AS modify_date, t.table_rows AS row_count FROM information_schema.tables t WHERE t.table_schema = '{}' AND t.table_name = '{}'",
            mysql_lit(schema),
            mysql_lit(table)
        ),
        MetadataQuery::DatabaseGeneral { database } => format!(
            "SELECT s.SCHEMA_NAME AS name, 'ONLINE' AS state_desc, s.DEFAULT_COLLATION_NAME AS collation_name, NULL AS recovery_model, NULL AS compatibility_level, NULL AS create_date, ROUND(COALESCE(SUM(t.DATA_LENGTH + t.INDEX_LENGTH), 0) / 1024 / 1024, 2) AS size_mb, NULL AS owner FROM information_schema.SCHEMATA s LEFT JOIN information_schema.TABLES t ON t.TABLE_SCHEMA = s.SCHEMA_NAME WHERE s.SCHEMA_NAME = '{d}' GROUP BY s.SCHEMA_NAME, s.DEFAULT_COLLATION_NAME",
            d = mysql_lit(database)
        ),
        MetadataQuery::ColumnGeneral { schema, table, column } => format!(
            "SELECT c.TABLE_SCHEMA AS schema_name, c.TABLE_NAME AS table_name, c.COLUMN_NAME AS name, c.ORDINAL_POSITION AS ordinal, c.COLUMN_TYPE AS data_type, c.CHARACTER_MAXIMUM_LENGTH AS max_length, c.NUMERIC_PRECISION AS `precision`, c.NUMERIC_SCALE AS `scale`, IF(c.IS_NULLABLE = 'YES', 1, 0) AS is_nullable, c.COLUMN_DEFAULT AS default_definition, IF(c.EXTRA LIKE '%auto_increment%', 1, 0) AS is_identity, IF(COALESCE(c.GENERATION_EXPRESSION, '') <> '', 1, 0) AS is_computed, c.COLLATION_NAME AS collation_name FROM information_schema.COLUMNS c WHERE c.TABLE_SCHEMA = '{s}' AND c.TABLE_NAME = '{t}' AND c.COLUMN_NAME = '{c}'",
            s = mysql_lit(schema),
            t = mysql_lit(table),
            c = mysql_lit(column)
        ),
        MetadataQuery::ObjectGeneral { schema, object } => format!(
            "SELECT schema_name, name, type_desc, create_date, modify_date FROM (SELECT t.table_schema AS schema_name, t.table_name AS name, IF(t.table_type = 'VIEW', 'VIEW', 'USER_TABLE') AS type_desc, t.create_time AS create_date, t.update_time AS modify_date FROM information_schema.tables t WHERE t.table_schema = '{s}' AND t.table_name = '{o}' UNION ALL SELECT r.routine_schema, r.routine_name, IF(r.routine_type = 'PROCEDURE', 'SQL_STORED_PROCEDURE', 'SQL_SCALAR_FUNCTION'), r.created, r.last_altered FROM information_schema.routines r WHERE r.routine_schema = '{s}' AND r.routine_name = '{o}') objects LIMIT 1",
            s = mysql_lit(schema),
            o = mysql_lit(object)
        ),
        MetadataQuery::IndexGeneral { schema, table, index } => format!(
            "SELECT s.index_name AS name, s.index_type AS type_desc, IF(MIN(s.non_unique) = 0, 1, 0) AS is_unique, IF(s.index_name = 'PRIMARY', 1, 0) AS is_primary_key, 0 AS is_disabled, NULL AS filter_definition, NULL AS fill_factor FROM information_schema.statistics s WHERE s.table_schema = '{}' AND s.table_name = '{}' AND s.index_name = '{}' GROUP BY s.index_name, s.index_type",
            mysql_lit(schema),
            mysql_lit(table),
            mysql_lit(index)
        ),
        MetadataQuery::IndexColumns { schema, table, index } => format!(
            "SELECT s.column_name AS name, s.seq_in_index AS key_ordinal, IF(s.collation = 'D', 1, 0) AS is_descending_key, 0 AS is_included_column FROM information_schema.statistics s WHERE s.table_schema = '{}' AND s.table_name = '{}' AND s.index_name = '{}' ORDER BY s.seq_in_index",
            mysql_lit(schema),
            mysql_lit(table),
            mysql_lit(index)
        ),
        MetadataQuery::ColumnDefaults { schema, table } => format!(
            "SELECT c.column_name, c.column_default, NULL AS constraint_name FROM information_schema.columns c WHERE c.table_schema = '{}' AND c.table_name = '{}' AND c.column_default IS NOT NULL ORDER BY c.ordinal_position",
            mysql_lit(schema),
            mysql_lit(table)
        ),
        MetadataQuery::TableIndexColumns { schema, table } => format!(
            "SELECT s.index_name, s.column_name, s.seq_in_index AS ordinal, 0 AS is_included, IF(s.collation = 'D', 1, 0) AS is_descending FROM information_schema.statistics s WHERE s.table_schema = '{}' AND s.table_name = '{}' ORDER BY s.index_name, s.seq_in_index",
            mysql_lit(schema),
            mysql_lit(table)
        ),
        // MySQL's per-index cardinality statistics stand in for statistics
        // objects: same positional shape, one row per index.
        MetadataQuery::Statistics { schema, table } => format!(
            "SELECT DISTINCT s.index_name AS name, 0 AS auto_created, 0 AS user_created, 0 AS no_recompute, NULL AS updated_at FROM information_schema.statistics s WHERE s.table_schema = '{}' AND s.table_name = '{}' ORDER BY name",
            mysql_lit(schema),
            mysql_lit(table)
        ),
        MetadataQuery::StatisticGeneral { schema, table, statistic } => format!(
            "SELECT DISTINCT s.index_name AS name, 0 AS auto_created, 0 AS user_created, 0 AS no_recompute, NULL AS updated_at FROM information_schema.statistics s WHERE s.table_schema = '{}' AND s.table_name = '{}' AND s.index_name = '{}'",
            mysql_lit(schema),
            mysql_lit(table),
            mysql_lit(statistic)
        ),
        MetadataQuery::StatisticColumns { schema, table, statistic } => format!(
            "SELECT s.column_name AS name FROM information_schema.statistics s WHERE s.table_schema = '{}' AND s.table_name = '{}' AND s.index_name = '{}' ORDER BY s.seq_in_index",
            mysql_lit(schema),
            mysql_lit(table),
            mysql_lit(statistic)
        ),
        MetadataQuery::ObjectPermissions { schema, object } => format!(
            "SELECT privilege_type AS permission_name, 'GRANT' AS state_desc, 'OBJECT' AS class_desc FROM information_schema.table_privileges WHERE table_schema = '{}' AND table_name = '{}' ORDER BY privilege_type",
            mysql_lit(schema),
            mysql_lit(object)
        ),
        // `grantee` is stored quoted as 'user'@'host'; rebuild that form from
        // CURRENT_USER() so the page reports this session's own grants.
        MetadataQuery::ServerPermissions => format!(
            "SELECT privilege_type AS permission_name, IF(is_grantable = 'YES', 'GRANT_WITH_GRANT_OPTION', 'GRANT') AS state_desc, 'SERVER' AS class_desc FROM information_schema.user_privileges WHERE grantee = {current_grantee} ORDER BY privilege_type"
        ),
        MetadataQuery::DatabasePermissions { database } => format!(
            "SELECT privilege_type AS permission_name, IF(is_grantable = 'YES', 'GRANT_WITH_GRANT_OPTION', 'GRANT') AS state_desc, 'DATABASE' AS class_desc FROM information_schema.schema_privileges WHERE table_schema = '{}' AND grantee = {current_grantee} ORDER BY privilege_type",
            mysql_lit(database)
        ),
        // MySQL's catalogs record no dynamic/read-only flag for a variable
        // (performance_schema.variables_info tracks the *source*, not
        // mutability), so the documented startup-only variables are marked
        // read-only by name (MYSQL_STATIC_VARIABLES plus the have_*/version*
        // build facts); anything else is offered, and the server itself
        // refuses a read-only one when the reviewed SET GLOBAL runs. MariaDB
        // records the flag; `sql_for_engine` swaps this query for the
        // information_schema.SYSTEM_VARIABLES form there.
        MetadataQuery::ServerConfiguration => format!(
            "SELECT variable_name AS name, variable_value AS value, 'GLOBAL' AS source, CASE WHEN LOWER(variable_name) IN ({}) OR LOWER(variable_name) LIKE 'have\\_%' OR LOWER(variable_name) LIKE 'version%' THEN 0 ELSE 1 END AS is_dynamic FROM performance_schema.global_variables ORDER BY variable_name",
            MYSQL_STATIC_VARIABLES
                .iter()
                .map(|name| format!("'{name}'"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        MetadataQuery::ServerStatus => "SELECT variable_name AS name, variable_value AS value, 'STATUS' AS source FROM performance_schema.global_status ORDER BY variable_name".to_string(),
        // MySQL's per-file view (information_schema.files) covers InnoDB
        // tablespaces only and is empty on many builds; report the schema's
        // total storage as one row instead.
        MetadataQuery::DatabaseFiles { database } => format!(
            "SELECT '{d}' AS logical_name, 'DATABASE' AS type_desc, @@datadir AS physical_name, ROUND(COALESCE(SUM(t.DATA_LENGTH + t.INDEX_LENGTH), 0) / 1024 / 1024, 2) AS size_mb, NULL AS max_size_mb, NULL AS growth FROM information_schema.TABLES t WHERE t.TABLE_SCHEMA = '{d}'",
            d = mysql_lit(database)
        ),
        // Reconstructed CREATE source for views, routines, and triggers from
        // information_schema (SHOW CREATE cannot be reshaped to the one-column
        // contract). The routine body is NOT null-coalesced: when the server
        // withholds it (definer rights), the row collapses to NULL and the
        // caller reports "no definition" instead of a bodyless header.
        MetadataQuery::ObjectDefinition { schema, object } => format!(
            "SELECT definition FROM (SELECT 1 AS ord, CONCAT('CREATE OR REPLACE VIEW `', v.table_schema, '`.`', v.table_name, '` AS\\n', v.view_definition) AS definition FROM information_schema.views v WHERE v.table_schema = '{s}' AND v.table_name = '{o}' UNION ALL SELECT 2, CONCAT('CREATE ', r.routine_type, ' `', r.routine_schema, '`.`', r.routine_name, '`(', COALESCE(p.params, ''), ')', IF(r.routine_type = 'FUNCTION', CONCAT(' RETURNS ', r.dtd_identifier), ''), '\\n', r.routine_definition) FROM information_schema.routines r LEFT JOIN (SELECT specific_schema, specific_name, GROUP_CONCAT(CONCAT_WS(' ', parameter_mode, parameter_name, dtd_identifier) ORDER BY ordinal_position SEPARATOR ', ') AS params FROM information_schema.parameters WHERE parameter_mode IS NOT NULL GROUP BY specific_schema, specific_name) p ON p.specific_schema = r.routine_schema AND p.specific_name = r.routine_name WHERE r.routine_schema = '{s}' AND r.routine_name = '{o}' UNION ALL SELECT 3, CONCAT('CREATE TRIGGER `', REPLACE(t.trigger_schema, '`', '``'), '`.`', REPLACE(t.trigger_name, '`', '``'), '` ', t.action_timing, ' ', t.event_manipulation, ' ON `', REPLACE(t.event_object_table, '`', '``'), '` FOR EACH ROW', {MYSQL_TRIGGER_FOLLOWS}, '\\n', t.action_statement) FROM information_schema.triggers t {MYSQL_TRIGGER_PREVIOUS} WHERE t.trigger_schema = '{s}' AND t.trigger_name = '{o}') d WHERE definition IS NOT NULL ORDER BY ord LIMIT 1",
            s = mysql_lit(schema),
            o = mysql_lit(object)
        ),
    }
}

// ---------------------------------------------------------------------------
// SQLite — sqlite_master + schema pragmas (main database)
// ---------------------------------------------------------------------------

// Every `pragma_*` table-valued function is spelled `main.pragma_*`: a user
// table of the same name (a TEMP one shadows the built-in function) would
// otherwise be read instead of the pragma and break metadata loading.

fn sqlite_sql(query: &MetadataQuery) -> String {
    match query {
        MetadataQuery::Schemas => "SELECT name FROM main.pragma_database_list ORDER BY seq".to_string(),
        MetadataQuery::SchemaObjectCounts => "SELECT 'main' AS name, (SELECT COUNT(*) FROM sqlite_master WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite_%') AS object_count".to_string(),
        MetadataQuery::Tables { .. } => "SELECT 'main' AS schema_name, name, NULL AS create_date, NULL AS modify_date FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name".to_string(),
        MetadataQuery::ObjectInventory { .. } => "SELECT CASE type WHEN 'table' THEN 'TABLE' WHEN 'view' THEN 'VIEW' WHEN 'trigger' THEN 'TRIGGER' END AS kind, 'main' AS schema_name, name, NULL AS row_estimate, NULL AS size_kb, NULL AS create_date, NULL AS modify_date, NULL AS comment FROM sqlite_master WHERE type IN ('table','view','trigger') AND name NOT LIKE 'sqlite_%' ORDER BY kind, schema_name, name".to_string(),
        MetadataQuery::Views { .. } => "SELECT 'main' AS schema_name, name, NULL AS create_date, NULL AS modify_date FROM sqlite_master WHERE type = 'view' ORDER BY name".to_string(),
        // No stored routines in SQLite: an empty set keeps the folder honest.
        MetadataQuery::ProgrammableObjects { .. } => "SELECT '' AS schema_name, '' AS name, '' AS type, NULL AS create_date, NULL AS modify_date WHERE 1 = 0".to_string(),
        // Nullability: `pragma_table_info` reports `notnull = 0` even for
        // INTEGER PRIMARY KEY columns, which are rowid aliases and therefore
        // implicitly NOT NULL — model that; other PK columns really are
        // nullable in SQLite unless declared NOT NULL.
        MetadataQuery::Columns { object, .. } => format!(
            "SELECT c.name, c.type AS type_name, -1 AS max_length, 0 AS precision, 0 AS scale, CASE WHEN c.\"notnull\" <> 0 OR (c.pk > 0 AND UPPER(c.type) = 'INTEGER') THEN 0 ELSE 1 END AS is_nullable, 0 AS is_identity, 0 AS is_computed FROM main.pragma_table_info('{}') c ORDER BY c.cid",
            lit(object)
        ),
        MetadataQuery::AllColumns => "SELECT 'main' AS schema_name, m.name AS object_name, c.name AS column_name, c.type AS type_name, CASE WHEN c.\"notnull\" <> 0 OR (c.pk > 0 AND UPPER(c.type) = 'INTEGER') THEN 0 ELSE 1 END AS is_nullable, CASE WHEN c.pk > 0 AND UPPER(c.type) = 'INTEGER' THEN 1 ELSE 0 END AS is_identity FROM sqlite_master m JOIN main.pragma_table_info(m.name) c WHERE m.type IN ('table', 'view') AND m.name NOT LIKE 'sqlite_%' ORDER BY object_name, c.cid".to_string(),
        MetadataQuery::AllForeignKeys => "SELECT 'fk_' || m.name || '_' || f.id AS constraint_name, 'main' AS from_schema, m.name AS from_table, f.\"from\" AS from_column, 'main' AS to_schema, f.\"table\" AS to_table, f.\"to\" AS to_column FROM sqlite_master m JOIN main.pragma_foreign_key_list(m.name) f WHERE m.type = 'table' AND m.name NOT LIKE 'sqlite_%' ORDER BY constraint_name, f.seq".to_string(),
        MetadataQuery::AllPrimaryKeys => "SELECT 'main' AS schema_name, m.name AS table_name, c.name AS column_name, c.pk AS key_ordinal FROM sqlite_master m JOIN main.pragma_table_info(m.name) c WHERE m.type = 'table' AND m.name NOT LIKE 'sqlite_%' AND c.pk > 0 ORDER BY m.name, c.pk".to_string(),
        MetadataQuery::ProcedureParameters { .. } => "SELECT '' AS name, '' AS type_name, -1 AS max_length, 0 AS precision, 0 AS scale, 0 AS is_output, 0 AS has_default_value WHERE 1 = 0".to_string(),
        MetadataQuery::Indexes { table, .. } => format!(
            "SELECT il.name, 'BTREE' AS type_desc, il.\"unique\" AS is_unique, CASE il.origin WHEN 'pk' THEN 1 ELSE 0 END AS is_primary_key, NULL AS filter_definition, NULL AS fill_factor FROM main.pragma_index_list('{}') il ORDER BY il.seq",
            lit(table)
        ),
        MetadataQuery::DependentViews { table, .. } => format!(
            "SELECT NULL AS schema_name, name, 0 AS is_materialized, sql AS definition FROM sqlite_master WHERE type = 'view' AND sql LIKE '%' || '{}' || '%' ORDER BY name",
            lit(table)
        ),
        MetadataQuery::Triggers { table, .. } => format!(
            "SELECT name, NULL AS create_date, NULL AS modify_date, 0 AS is_disabled FROM sqlite_master WHERE type = 'trigger' AND tbl_name = '{}' ORDER BY name",
            lit(table)
        ),
        MetadataQuery::Constraints { table, .. } => format!(
            "SELECT il.name, CASE il.origin WHEN 'pk' THEN 'PRIMARY_KEY_CONSTRAINT' WHEN 'u' THEN 'UNIQUE_CONSTRAINT' ELSE 'INDEX' END AS type_desc FROM main.pragma_index_list('{}') il WHERE il.origin IN ('pk', 'u') ORDER BY name",
            lit(table)
        ),
        MetadataQuery::Types { .. } => "SELECT '' AS schema_name, '' AS name, '' AS base_type, -1 AS max_length, 0 AS precision, 0 AS scale, 0 AS is_nullable WHERE 1 = 0".to_string(),
        MetadataQuery::DatabaseTriggers { .. } => "SELECT 'main' AS schema_name, name, tbl_name AS parent_name, 0 AS is_disabled FROM sqlite_master WHERE type = 'trigger' ORDER BY name".to_string(),
        MetadataQuery::Sequences { .. } => "SELECT '' AS schema_name, '' AS name, '' AS data_type, NULL AS start_value, NULL AS increment, NULL AS current_value WHERE 1 = 0".to_string(),
        MetadataQuery::Synonyms { .. } => "SELECT '' AS schema_name, '' AS name, '' AS base_object_name WHERE 1 = 0".to_string(),
        MetadataQuery::Extensions => "SELECT '' AS name, '' AS version, '' AS schema_name, '' AS description WHERE 1 = 0".to_string(),
        MetadataQuery::AvailableExtensions => "SELECT '' AS name, '' AS version, '' AS description, 0 AS installed WHERE 1 = 0".to_string(),
        MetadataQuery::Events { .. } => "SELECT '' AS schema_name, '' AS name, '' AS status, '' AS event_type, '' AS interval_or_at, '' AS definition WHERE 1 = 0".to_string(),
        MetadataQuery::MaterializedViews { .. } => "SELECT '' AS schema_name, '' AS name, 0 AS is_populated WHERE 1 = 0".to_string(),
        MetadataQuery::TriggerDefinition { name, .. } => format!(
            "SELECT sql AS definition FROM sqlite_master WHERE type = 'trigger' AND name = '{}'",
            lit(name)
        ),
        MetadataQuery::ConstraintDefinition { table, name, .. } => format!(
            // The columns are ordered in a subquery rather than with an
            // aggregate ORDER BY (SQLite 3.44+ only, and refused by the libSQL
            // server's parser).
            "SELECT 'UNIQUE (' || GROUP_CONCAT(name, ', ') || ')' AS definition FROM (SELECT ii.name FROM main.pragma_index_list('{table}') il JOIN main.pragma_index_info(il.name) ii WHERE il.name = '{name}' AND il.origin = 'u' ORDER BY ii.seqno) UNION ALL SELECT sql AS definition FROM sqlite_master WHERE type = 'table' AND name = '{table}'",
            table = lit(table),
            name = lit(name)
        ),
        MetadataQuery::Databases => "SELECT name, 'ONLINE' AS state_desc, NULL AS compatibility_level FROM main.pragma_database_list ORDER BY seq".to_string(),
        MetadataQuery::ServerCapability => "SELECT sqlite_version() AS product_version, 'SQLite' AS edition, NULL AS engine_edition, NULL AS product_level, NULL AS product_update_level, NULL AS machine_name, 0 AS integrated_security_only".to_string(),
        MetadataQuery::TypeDetail { .. } => "SELECT '' AS schema_name, '' AS name, '' AS base_type, -1 AS max_length, 0 AS precision, 0 AS scale, 0 AS is_nullable WHERE 1 = 0".to_string(),
        MetadataQuery::TableGeneral { table, .. } => format!(
            "SELECT 'main' AS schema_name, '{t}' AS name, NULL AS create_date, NULL AS modify_date, (SELECT COUNT(*) FROM \"{q}\") AS row_count FROM sqlite_master WHERE type = 'table' AND name = '{t}'",
            t = lit(table),
            q = table.replace('"', "\"\"")
        ),
        MetadataQuery::DatabaseGeneral { database } => format!(
            "SELECT name, 'ONLINE' AS state_desc, NULL AS collation_name, NULL AS recovery_model, NULL AS compatibility_level, NULL AS create_date, NULL AS size_mb, file AS owner FROM main.pragma_database_list WHERE name = '{}'",
            lit(database)
        ),
        MetadataQuery::ColumnGeneral { table, column, .. } => format!(
            "SELECT 'main' AS schema_name, '{t}' AS table_name, ti.name, ti.cid + 1 AS ordinal, ti.type AS data_type, NULL AS max_length, NULL AS precision, NULL AS scale, CASE ti.\"notnull\" WHEN 0 THEN 1 ELSE 0 END AS is_nullable, ti.dflt_value AS default_definition, 0 AS is_identity, 0 AS is_computed, NULL AS collation_name FROM main.pragma_table_info('{t}') ti WHERE ti.name = '{c}'",
            t = lit(table),
            c = lit(column)
        ),
        MetadataQuery::ObjectGeneral { object, .. } => format!(
            "SELECT 'main' AS schema_name, name, UPPER(type) AS type_desc, NULL AS create_date, NULL AS modify_date FROM sqlite_master WHERE name = '{}'",
            lit(object)
        ),
        MetadataQuery::IndexGeneral { table, index, .. } => format!(
            "SELECT il.name, 'BTREE' AS type_desc, il.\"unique\" AS is_unique, CASE il.origin WHEN 'pk' THEN 1 ELSE 0 END AS is_primary_key, 0 AS is_disabled, NULL AS filter_definition, NULL AS fill_factor FROM main.pragma_index_list('{}') il WHERE il.name = '{}'",
            lit(table),
            lit(index)
        ),
        MetadataQuery::IndexColumns { index, .. } => format!(
            "SELECT ii.name, ii.seqno + 1 AS key_ordinal, ii.\"desc\" AS is_descending_key, 0 AS is_included_column FROM main.pragma_index_xinfo('{}') ii WHERE ii.key = 1 ORDER BY ii.seqno",
            lit(index)
        ),
        MetadataQuery::ColumnDefaults { table, .. } => format!(
            "SELECT name AS column_name, dflt_value AS default_expr, NULL AS constraint_name FROM main.pragma_table_info('{}') WHERE dflt_value IS NOT NULL ORDER BY cid",
            lit(table)
        ),
        MetadataQuery::TableIndexColumns { table, .. } => format!(
            // `desc` exists only in pragma_index_xinfo (index_info has
            // seqno/cid/name); `key = 1` keeps the key columns like IndexColumns.
            "SELECT il.name AS index_name, ii.name AS column_name, ii.seqno + 1 AS ordinal, 0 AS is_included, ii.\"desc\" AS is_descending FROM main.pragma_index_list('{}') il JOIN main.pragma_index_xinfo(il.name) ii WHERE ii.key = 1 ORDER BY il.name, ii.seqno",
            lit(table)
        ),
        // SQLite keeps no statistics objects (sqlite_stat1 appears only after
        // ANALYZE and may not exist); honest empty sets keep the pages quiet.
        MetadataQuery::Statistics { .. } => "SELECT '' AS name, 0 AS auto_created, 0 AS user_created, 0 AS no_recompute, NULL AS updated_at WHERE 1 = 0".to_string(),
        MetadataQuery::StatisticGeneral { .. } => "SELECT '' AS name, 0 AS auto_created, 0 AS user_created, 0 AS no_recompute, NULL AS updated_at WHERE 1 = 0".to_string(),
        MetadataQuery::StatisticColumns { .. } => "SELECT '' AS name WHERE 1 = 0".to_string(),
        // SQLite has no permission system; an empty set keeps the page honest.
        MetadataQuery::ObjectPermissions { .. }
        | MetadataQuery::ServerPermissions
        | MetadataQuery::DatabasePermissions { .. } => {
            "SELECT '' AS permission_name, '' AS state_desc, '' AS class_desc WHERE 1 = 0"
                .to_string()
        }
        // The closest thing SQLite has to server settings: the version, the
        // pragmas that shape the file, and the build's compile options.
        MetadataQuery::ServerConfiguration => "SELECT 'sqlite_version' AS name, sqlite_version() AS value, 'BUILD' AS source, 0 AS is_dynamic UNION ALL SELECT 'page_size', CAST((SELECT page_size FROM main.pragma_page_size()) AS TEXT), 'PRAGMA', 0 UNION ALL SELECT 'page_count', CAST((SELECT page_count FROM main.pragma_page_count()) AS TEXT), 'PRAGMA', 0 UNION ALL SELECT 'encoding', (SELECT encoding FROM main.pragma_encoding()), 'PRAGMA', 0 UNION ALL SELECT 'journal_mode', (SELECT journal_mode FROM main.pragma_journal_mode()), 'PRAGMA', 0 UNION ALL SELECT compile_options, 'on', 'COMPILE', 0 FROM main.pragma_compile_options() ORDER BY 1".to_string(),
        // SQLite is an in-process file: it has no server-wide counters. The
        // empty set is the honest answer; the page explains why.
        MetadataQuery::ServerStatus => "SELECT '' AS name, '' AS value, '' AS source WHERE 1 = 0".to_string(),
        // Every attached database is one file; page_count/page_size describe
        // the main one, which is the database the explorer targets.
        MetadataQuery::DatabaseFiles { .. } => "SELECT d.name AS logical_name, 'DATABASE' AS type_desc, d.file AS physical_name, ROUND((SELECT page_count FROM main.pragma_page_count()) * (SELECT page_size FROM main.pragma_page_size()) / 1048576.0, 2) AS size_mb, NULL AS max_size_mb, NULL AS growth FROM main.pragma_database_list d ORDER BY d.seq".to_string(),
        // sqlite_master stores verbatim CREATE source for tables, views,
        // triggers, and named indexes alike.
        MetadataQuery::ObjectDefinition { object, .. } => format!(
            "SELECT sql AS definition FROM sqlite_master WHERE name = '{}' AND sql IS NOT NULL LIMIT 1",
            lit(object)
        ),
    }
}

// ---------------------------------------------------------------------------
// DuckDB — duckdb_*() catalog functions
// ---------------------------------------------------------------------------

/// Scope every DuckDB catalog query to the connection's current database
/// (attached catalogs are listed by `Databases`, not mixed into the tree)
/// and, when the caller passed one, to a schema.
fn duckdb_scope(schema: Option<&str>) -> String {
    match schema {
        Some(s) => format!(
            "database_name = current_database() AND NOT internal AND schema_name = '{}'",
            lit(s)
        ),
        None => "database_name = current_database() AND NOT internal".to_string(),
    }
}

/// `constraint_name` is NULL for unnamed constraints; fall back to a stable
/// synthetic name so the explorer can address the constraint again.
const DUCKDB_CONSTRAINT_NAME: &str =
    "COALESCE(constraint_name, constraint_type || '_' || CAST(constraint_index AS VARCHAR))";

/// The columns of an index, split out of `duckdb_indexes().expressions`
/// (rendered as `[a, b]`): a LIST of column names.
const DUCKDB_INDEX_COLUMNS: &str = "string_split(trim(CAST(expressions AS VARCHAR), '[]'), ', ')";

/// Schemas of the current database. `duckdb_schemas().internal` is true for
/// `main` itself, so the system schemas are excluded by name instead.
const DUCKDB_SCHEMA_SCOPE: &str = "database_name = current_database() AND schema_name NOT IN ('information_schema', 'pg_catalog')";

fn duckdb_sql(query: &MetadataQuery) -> String {
    match query {
        MetadataQuery::Schemas => format!(
            "SELECT schema_name AS name FROM duckdb_schemas() WHERE {DUCKDB_SCHEMA_SCOPE} ORDER BY schema_name"
        ),
        MetadataQuery::SchemaObjectCounts => format!(
            "SELECT s.schema_name AS name, (SELECT COUNT(*) FROM duckdb_tables() t WHERE t.database_name = s.database_name AND t.schema_name = s.schema_name AND NOT t.internal) + (SELECT COUNT(*) FROM duckdb_views() v WHERE v.database_name = s.database_name AND v.schema_name = s.schema_name AND NOT v.internal) + (SELECT COUNT(*) FROM duckdb_functions() f WHERE f.database_name = s.database_name AND f.schema_name = s.schema_name AND NOT f.internal) AS object_count FROM duckdb_schemas() s WHERE s.{} ORDER BY s.schema_name",
            DUCKDB_SCHEMA_SCOPE.replace(" AND schema_name", " AND s.schema_name")
        ),
        MetadataQuery::Tables { schema } => format!(
            "SELECT schema_name, table_name AS name, NULL AS create_date, NULL AS modify_date FROM duckdb_tables() WHERE {} ORDER BY schema_name, table_name",
            duckdb_scope(schema.as_deref())
        ),
        MetadataQuery::Views { schema } => format!(
            "SELECT schema_name, view_name AS name, NULL AS create_date, NULL AS modify_date FROM duckdb_views() WHERE {} ORDER BY schema_name, view_name",
            duckdb_scope(schema.as_deref())
        ),
        // DuckDB's routines are macros: scalar macros map to FN, table macros
        // to TF. Built-in functions are `internal` and stay out of the tree.
        MetadataQuery::ProgrammableObjects { schema } => format!(
            "SELECT schema_name, function_name AS name, CASE function_type WHEN 'table_macro' THEN 'TF' ELSE 'FN' END AS type, NULL AS create_date, NULL AS modify_date FROM duckdb_functions() WHERE {} AND function_type IN ('macro', 'table_macro') ORDER BY schema_name, function_name",
            duckdb_scope(schema.as_deref())
        ),
        MetadataQuery::ObjectInventory { schema } => {
            let scope = duckdb_scope(schema.as_deref());
            format!(
                "SELECT kind, schema_name, name, row_estimate, size_kb, create_date, modify_date, comment FROM (\
                 SELECT 'TABLE' AS kind, schema_name, table_name AS name, estimated_size AS row_estimate, NULL AS size_kb, NULL AS create_date, NULL AS modify_date, comment FROM duckdb_tables() WHERE {scope} \
                 UNION ALL SELECT 'VIEW', schema_name, view_name, NULL, NULL, NULL, NULL, comment FROM duckdb_views() WHERE {scope} \
                 UNION ALL SELECT 'FUNCTION', schema_name, function_name, NULL, NULL, NULL, NULL, comment FROM duckdb_functions() WHERE {scope} AND function_type IN ('macro', 'table_macro')\
                 ) inventory ORDER BY kind, schema_name, name"
            )
        }
        MetadataQuery::Columns { schema, object } => format!(
            "SELECT column_name AS name, data_type AS type_name, COALESCE(character_maximum_length, -1) AS max_length, COALESCE(numeric_precision, 0) AS precision, COALESCE(numeric_scale, 0) AS scale, CASE WHEN is_nullable THEN 1 ELSE 0 END AS is_nullable, CASE WHEN COALESCE(column_default, '') LIKE 'nextval(%' THEN 1 ELSE 0 END AS is_identity, 0 AS is_computed FROM duckdb_columns() WHERE {} AND table_name = '{}' ORDER BY column_index",
            duckdb_scope(Some(schema)),
            lit(object)
        ),
        MetadataQuery::AllColumns => "SELECT schema_name, table_name AS object_name, column_name, data_type AS type_name, CASE WHEN is_nullable THEN 1 ELSE 0 END AS is_nullable, CASE WHEN COALESCE(column_default, '') LIKE 'nextval(%' THEN 1 ELSE 0 END AS is_identity FROM duckdb_columns() WHERE database_name = current_database() AND NOT internal ORDER BY schema_name, object_name, column_index".to_string(),
        // Referenced tables carry no schema in duckdb_constraints(); DuckDB
        // foreign keys are same-schema, so the child's schema is reported.
        MetadataQuery::AllForeignKeys => format!(
            "SELECT {name} AS constraint_name, schema_name AS from_schema, table_name AS from_table, unnest(constraint_column_names) AS from_column, schema_name AS to_schema, referenced_table AS to_table, unnest(referenced_column_names) AS to_column FROM duckdb_constraints() WHERE database_name = current_database() AND constraint_type = 'FOREIGN KEY' ORDER BY constraint_name, from_table",
            name = DUCKDB_CONSTRAINT_NAME
        ),
        MetadataQuery::AllPrimaryKeys => "SELECT schema_name, table_name, unnest(constraint_column_names) AS column_name, unnest(range(1, len(constraint_column_names) + 1)) AS key_ordinal FROM duckdb_constraints() WHERE database_name = current_database() AND constraint_type = 'PRIMARY KEY' ORDER BY schema_name, table_name, key_ordinal".to_string(),
        // Macro parameters are untyped; report ANY where DuckDB records no type.
        MetadataQuery::ProcedureParameters { schema, object } => format!(
            "SELECT unnest(parameters) AS name, COALESCE(unnest(parameter_types), 'ANY') AS type_name, -1 AS max_length, 0 AS precision, 0 AS scale, 0 AS is_output, 0 AS has_default_value FROM duckdb_functions() WHERE {} AND function_type IN ('macro', 'table_macro') AND function_name = '{}'",
            duckdb_scope(Some(schema)),
            lit(object)
        ),
        MetadataQuery::Indexes { schema, table } => format!(
            "SELECT index_name AS name, 'ART' AS type_desc, CASE WHEN is_unique THEN 1 ELSE 0 END AS is_unique, CASE WHEN is_primary THEN 1 ELSE 0 END AS is_primary_key, NULL AS filter_definition, NULL AS fill_factor FROM duckdb_indexes() WHERE database_name = current_database() AND schema_name = '{}' AND table_name = '{}' ORDER BY index_name",
            lit(schema),
            lit(table)
        ),
        // DuckDB does not record view→table dependencies; match the view
        // source text, as the SQLite dialect does. Views anywhere in the
        // database may reference the table, so the search is not scoped to
        // the table's own schema.
        MetadataQuery::DependentViews { table, .. } => format!(
            "SELECT schema_name, view_name AS name, 0 AS is_materialized, sql AS definition FROM duckdb_views() WHERE {} AND sql LIKE '%' || '{}' || '%' ORDER BY schema_name, view_name",
            duckdb_scope(None),
            lit(table)
        ),
        // No triggers in DuckDB.
        MetadataQuery::Triggers { .. } => "SELECT '' AS name, NULL AS create_date, NULL AS modify_date, 0 AS is_disabled WHERE 1 = 0".to_string(),
        MetadataQuery::Constraints { schema, table } => format!(
            "SELECT {name} AS name, CASE constraint_type WHEN 'PRIMARY KEY' THEN 'PRIMARY_KEY_CONSTRAINT' WHEN 'UNIQUE' THEN 'UNIQUE_CONSTRAINT' WHEN 'FOREIGN KEY' THEN 'FOREIGN_KEY_CONSTRAINT' WHEN 'CHECK' THEN 'CHECK_CONSTRAINT' ELSE constraint_type END AS type_desc FROM duckdb_constraints() WHERE database_name = current_database() AND schema_name = '{}' AND table_name = '{}' AND constraint_type <> 'NOT NULL' ORDER BY constraint_index",
            lit(schema),
            lit(table),
            name = DUCKDB_CONSTRAINT_NAME
        ),
        MetadataQuery::Types { schema } => format!(
            "SELECT schema_name, type_name AS name, logical_type AS base_type, -1 AS max_length, 0 AS precision, 0 AS scale, 1 AS is_nullable FROM duckdb_types() WHERE {} ORDER BY schema_name, type_name",
            duckdb_scope(schema.as_deref())
        ),
        MetadataQuery::DatabaseTriggers { .. } => "SELECT '' AS schema_name, '' AS name, NULL AS parent_name, 0 AS is_disabled WHERE 1 = 0".to_string(),
        MetadataQuery::Sequences { schema } => {
            let schema_filter = schema
                .as_deref()
                .map(|s| format!(" AND schema_name = '{}'", lit(s)))
                .unwrap_or_default();
            format!(
                "SELECT schema_name, sequence_name AS name, 'BIGINT' AS data_type, start_value, increment_by AS increment, last_value AS current_value FROM duckdb_sequences() WHERE database_name = current_database(){schema_filter} ORDER BY schema_name, sequence_name"
            )
        }
        MetadataQuery::Synonyms { .. } => "SELECT '' AS schema_name, '' AS name, '' AS base_object_name WHERE 1 = 0".to_string(),
        MetadataQuery::Extensions => "SELECT extension_name AS name, COALESCE(extension_version, '') AS version, '' AS schema_name, COALESCE(description, '') AS description FROM duckdb_extensions() WHERE loaded ORDER BY extension_name".to_string(),
        MetadataQuery::AvailableExtensions => "SELECT extension_name AS name, COALESCE(extension_version, '') AS version, COALESCE(description, '') AS description, CASE WHEN installed THEN 1 ELSE 0 END AS installed FROM duckdb_extensions() ORDER BY extension_name".to_string(),
        MetadataQuery::Events { .. } => "SELECT '' AS schema_name, '' AS name, '' AS status, '' AS event_type, '' AS interval_or_at, '' AS definition WHERE 1 = 0".to_string(),
        MetadataQuery::MaterializedViews { .. } => "SELECT '' AS schema_name, '' AS name, 0 AS is_populated WHERE 1 = 0".to_string(),
        MetadataQuery::TriggerDefinition { .. } => "SELECT '' AS definition WHERE 1 = 0".to_string(),
        MetadataQuery::ConstraintDefinition { schema, table, name } => format!(
            "SELECT constraint_text AS definition FROM duckdb_constraints() WHERE database_name = current_database() AND schema_name = '{}' AND table_name = '{}' AND {cname} = '{}'",
            lit(schema),
            lit(table),
            lit(name),
            cname = DUCKDB_CONSTRAINT_NAME
        ),
        MetadataQuery::Databases => "SELECT database_name AS name, 'ONLINE' AS state_desc, NULL AS compatibility_level FROM duckdb_databases() WHERE NOT internal ORDER BY database_name".to_string(),
        MetadataQuery::ServerCapability => "SELECT version() AS product_version, 'DuckDB' AS edition, NULL AS engine_edition, NULL AS product_level, NULL AS product_update_level, NULL AS machine_name, 0 AS integrated_security_only".to_string(),
        MetadataQuery::TypeDetail { schema, type_name } => format!(
            "SELECT schema_name, type_name AS name, logical_type AS base_type, -1 AS max_length, 0 AS precision, 0 AS scale, 1 AS is_nullable FROM duckdb_types() WHERE {} AND type_name = '{}'",
            duckdb_scope(Some(schema)),
            lit(type_name)
        ),
        MetadataQuery::TableGeneral { schema, table } => format!(
            "SELECT schema_name, table_name AS name, NULL AS create_date, NULL AS modify_date, estimated_size AS row_count FROM duckdb_tables() WHERE database_name = current_database() AND schema_name = '{}' AND table_name = '{}'",
            lit(schema),
            lit(table)
        ),
        MetadataQuery::DatabaseGeneral { database } => format!(
            "SELECT d.database_name AS name, 'ONLINE' AS state_desc, NULL AS collation_name, NULL AS recovery_model, NULL AS compatibility_level, NULL AS create_date, (SELECT ROUND(s.used_blocks * s.block_size / 1048576.0, 2) FROM pragma_database_size() s WHERE s.database_name = d.database_name) AS size_mb, d.path AS owner FROM duckdb_databases() d WHERE d.database_name = '{}'",
            lit(database)
        ),
        MetadataQuery::ColumnGeneral { schema, table, column } => format!(
            "SELECT schema_name, table_name, column_name AS name, column_index AS ordinal, data_type, character_maximum_length AS max_length, numeric_precision AS precision, numeric_scale AS scale, CASE WHEN is_nullable THEN 1 ELSE 0 END AS is_nullable, column_default AS default_definition, CASE WHEN COALESCE(column_default, '') LIKE 'nextval(%' THEN 1 ELSE 0 END AS is_identity, 0 AS is_computed, NULL AS collation_name FROM duckdb_columns() WHERE database_name = current_database() AND schema_name = '{}' AND table_name = '{}' AND column_name = '{}'",
            lit(schema),
            lit(table),
            lit(column)
        ),
        MetadataQuery::ObjectGeneral { schema, object } => format!(
            "SELECT schema_name, name, type_desc, NULL AS create_date, NULL AS modify_date FROM (SELECT schema_name, table_name AS name, 'USER_TABLE' AS type_desc, 1 AS ord FROM duckdb_tables() WHERE {scope} AND table_name = '{o}' UNION ALL SELECT schema_name, view_name, 'VIEW', 2 FROM duckdb_views() WHERE {scope} AND view_name = '{o}' UNION ALL SELECT schema_name, function_name, CASE function_type WHEN 'table_macro' THEN 'SQL_TABLE_VALUED_FUNCTION' ELSE 'SQL_SCALAR_FUNCTION' END, 3 FROM duckdb_functions() WHERE {scope} AND function_name = '{o}') objects ORDER BY ord LIMIT 1",
            scope = duckdb_scope(Some(schema)),
            o = lit(object)
        ),
        MetadataQuery::IndexGeneral { schema, table, index } => format!(
            "SELECT index_name AS name, 'ART' AS type_desc, CASE WHEN is_unique THEN 1 ELSE 0 END AS is_unique, CASE WHEN is_primary THEN 1 ELSE 0 END AS is_primary_key, 0 AS is_disabled, NULL AS filter_definition, NULL AS fill_factor FROM duckdb_indexes() WHERE database_name = current_database() AND schema_name = '{}' AND table_name = '{}' AND index_name = '{}'",
            lit(schema),
            lit(table),
            lit(index)
        ),
        MetadataQuery::IndexColumns { schema, table, index } => format!(
            "SELECT unnest(cols) AS name, unnest(range(1, len(cols) + 1)) AS key_ordinal, 0 AS is_descending_key, 0 AS is_included_column FROM (SELECT {cols} AS cols FROM duckdb_indexes() WHERE database_name = current_database() AND schema_name = '{}' AND table_name = '{}' AND index_name = '{}') i",
            lit(schema),
            lit(table),
            lit(index),
            cols = DUCKDB_INDEX_COLUMNS
        ),
        MetadataQuery::ColumnDefaults { schema, table } => format!(
            "SELECT column_name, column_default AS default_expr, NULL AS constraint_name FROM duckdb_columns() WHERE database_name = current_database() AND schema_name = '{}' AND table_name = '{}' AND column_default IS NOT NULL ORDER BY column_index",
            lit(schema),
            lit(table)
        ),
        MetadataQuery::TableIndexColumns { schema, table } => format!(
            "SELECT index_name, unnest(cols) AS column_name, unnest(range(1, len(cols) + 1)) AS ordinal, 0 AS is_included, 0 AS is_descending FROM (SELECT index_name, {cols} AS cols FROM duckdb_indexes() WHERE database_name = current_database() AND schema_name = '{}' AND table_name = '{}') i ORDER BY index_name, ordinal",
            lit(schema),
            lit(table),
            cols = DUCKDB_INDEX_COLUMNS
        ),
        // DuckDB keeps table statistics inside its storage, not as objects.
        MetadataQuery::Statistics { .. } => "SELECT '' AS name, 0 AS auto_created, 0 AS user_created, 0 AS no_recompute, NULL AS updated_at WHERE 1 = 0".to_string(),
        MetadataQuery::StatisticGeneral { .. } => "SELECT '' AS name, 0 AS auto_created, 0 AS user_created, 0 AS no_recompute, NULL AS updated_at WHERE 1 = 0".to_string(),
        MetadataQuery::StatisticColumns { .. } => "SELECT '' AS name WHERE 1 = 0".to_string(),
        // No permission system in an in-process database.
        MetadataQuery::ObjectPermissions { .. }
        | MetadataQuery::ServerPermissions
        | MetadataQuery::DatabasePermissions { .. } => {
            "SELECT '' AS permission_name, '' AS state_desc, '' AS class_desc WHERE 1 = 0"
                .to_string()
        }
        MetadataQuery::ServerConfiguration => "SELECT name, value, scope AS source, 0 AS is_dynamic FROM duckdb_settings() ORDER BY name".to_string(),
        // DuckDB runs in-process; there are no server-wide counters to read.
        MetadataQuery::ServerStatus => "SELECT '' AS name, '' AS value, '' AS source WHERE 1 = 0".to_string(),
        MetadataQuery::DatabaseFiles { database } => format!(
            "SELECT d.database_name AS logical_name, 'DATABASE' AS type_desc, d.path AS physical_name, (SELECT ROUND(s.used_blocks * s.block_size / 1048576.0, 2) FROM pragma_database_size() s WHERE s.database_name = d.database_name) AS size_mb, NULL AS max_size_mb, NULL AS growth FROM duckdb_databases() d WHERE d.database_name = '{}'",
            lit(database)
        ),
        // Views and tables keep their CREATE source; macros are rebuilt from
        // their definition.
        MetadataQuery::ObjectDefinition { schema, object } => format!(
            "SELECT definition FROM (SELECT sql AS definition, 1 AS ord FROM duckdb_views() WHERE {scope} AND view_name = '{o}' UNION ALL SELECT sql, 2 FROM duckdb_tables() WHERE {scope} AND table_name = '{o}' UNION ALL SELECT 'CREATE MACRO ' || function_name || '(' || COALESCE(array_to_string(parameters, ', '), '') || ') AS ' || CASE function_type WHEN 'table_macro' THEN 'TABLE ' ELSE '' END || macro_definition, 3 FROM duckdb_functions() WHERE {scope} AND function_type IN ('macro', 'table_macro') AND function_name = '{o}') d WHERE definition IS NOT NULL ORDER BY ord LIMIT 1",
            scope = duckdb_scope(Some(schema)),
            o = lit(object)
        ),
    }
}

// ---------------------------------------------------------------------------
// ClickHouse — system.* tables
// ---------------------------------------------------------------------------

/// A ClickHouse string literal: backslash is an escape character, so it is
/// doubled along with the single quote (which ClickHouse also accepts
/// doubled).
fn ch_lit(value: &str) -> String {
    mysql_lit(value)
}

/// The database a ClickHouse catalog query is scoped to: the caller's
/// schema (ClickHouse databases are the namespace level, like MySQL's) or
/// the connection's current database.
fn ch_scope(schema: Option<&str>) -> String {
    match schema {
        Some(s) => format!("'{}'", ch_lit(s)),
        None => "currentDatabase()".to_string(),
    }
}

/// Materialized views keep their rows in a hidden `.inner_id.<uuid>` table
/// (or `.inner.<name>` on older servers); those never appear as tables.
///
/// Engines that make a `system.tables` row a view rather than a table.
const CH_VIEW_ENGINES: &str = "('View', 'LiveView', 'WindowView')";

/// User-defined functions: everything `system.functions` did not ship with
/// the server (SQL, executable and wasm UDFs).
const CH_USER_FUNCTIONS: &str = "origin <> 'System'";

/// The primary key columns of every table, one row per key part with its
/// ordinal, from `system.tables.primary_key` (`a, toYYYYMM(d)`).
const CH_PRIMARY_KEY_PARTS: &str = "system.tables ARRAY JOIN splitByString(', ', primary_key) AS key_part, arrayEnumerate(splitByString(', ', primary_key)) AS key_ordinal";

/// ClickHouse resolves SELECT aliases inside WHERE, so no arm below aliases
/// an expression as `name` while also filtering on the `name` column.
///
/// The one-column, zero-row shape shared by the concepts ClickHouse lacks.
fn ch_empty(columns: &str) -> String {
    format!("SELECT {columns} WHERE 0")
}

fn clickhouse_sql(query: &MetadataQuery) -> String {
    match query {
        MetadataQuery::Schemas => "SELECT currentDatabase() AS name".to_string(),
        MetadataQuery::SchemaObjectCounts => "SELECT currentDatabase() AS name, toInt64((SELECT count() FROM system.tables WHERE database = currentDatabase() AND NOT is_temporary AND name NOT LIKE '.inner%')) AS object_count".to_string(),
        MetadataQuery::Tables { schema } => format!(
            "SELECT database AS schema_name, name, NULL AS create_date, metadata_modification_time AS modify_date FROM system.tables WHERE database = {} AND NOT is_temporary AND name NOT LIKE '.inner%' AND engine NOT IN ('View', 'LiveView', 'WindowView', 'MaterializedView') ORDER BY database, name",
            ch_scope(schema.as_deref())
        ),
        MetadataQuery::Views { schema } => format!(
            "SELECT database AS schema_name, name, NULL AS create_date, metadata_modification_time AS modify_date FROM system.tables WHERE database = {} AND engine IN {CH_VIEW_ENGINES} ORDER BY database, name",
            ch_scope(schema.as_deref())
        ),
        // User-defined functions are server-wide; they are listed under the
        // requested database so the explorer has somewhere to hang them.
        MetadataQuery::ProgrammableObjects { schema } => format!(
            "SELECT {} AS schema_name, name, 'FN' AS type, NULL AS create_date, NULL AS modify_date FROM system.functions WHERE {CH_USER_FUNCTIONS} ORDER BY name",
            ch_scope(schema.as_deref())
        ),
        MetadataQuery::ObjectInventory { schema } => {
            let scope = ch_scope(schema.as_deref());
            format!(
                "SELECT kind, schema_name, name, row_estimate, size_kb, create_date, modify_date, comment FROM (\
                 SELECT multiIf(engine IN {CH_VIEW_ENGINES}, 'VIEW', engine = 'MaterializedView', 'MATERIALIZED VIEW', 'TABLE') AS kind, database AS schema_name, name, total_rows AS row_estimate, intDiv(total_bytes, 1024) AS size_kb, NULL AS create_date, metadata_modification_time AS modify_date, comment FROM system.tables WHERE database = {scope} AND NOT is_temporary AND name NOT LIKE '.inner%' \
                 UNION ALL \
                 SELECT 'FUNCTION', {scope}, name, NULL, NULL, NULL, NULL, description FROM system.functions WHERE {CH_USER_FUNCTIONS}\
                 ) inventory ORDER BY kind, schema_name, name"
            )
        }
        MetadataQuery::Columns { schema, object } => format!(
            "SELECT name, type AS type_name, ifNull(toInt64(character_octet_length), -1) AS max_length, ifNull(toInt64(numeric_precision), 0) AS num_precision, ifNull(toInt64(numeric_scale), 0) AS num_scale, if(startsWith(type, 'Nullable('), 1, 0) AS is_nullable, 0 AS is_identity, if(default_kind IN ('ALIAS', 'MATERIALIZED'), 1, 0) AS is_computed FROM system.columns WHERE database = '{}' AND table = '{}' ORDER BY position",
            ch_lit(schema),
            ch_lit(object)
        ),
        MetadataQuery::AllColumns => "SELECT database AS schema_name, table AS object_name, name AS column_name, type AS type_name, if(startsWith(type, 'Nullable('), 1, 0) AS is_nullable, 0 AS is_identity FROM system.columns WHERE database = currentDatabase() AND table NOT LIKE '.inner%' ORDER BY schema_name, object_name, position".to_string(),
        // No foreign keys in ClickHouse.
        MetadataQuery::AllForeignKeys => ch_empty("'' AS constraint_name, '' AS from_schema, '' AS from_table, '' AS from_column, '' AS to_schema, '' AS to_table, '' AS to_column"),
        MetadataQuery::AllPrimaryKeys => format!(
            "SELECT database AS schema_name, name AS table_name, key_part AS column_name, key_ordinal FROM {CH_PRIMARY_KEY_PARTS} WHERE database = currentDatabase() AND primary_key <> '' ORDER BY schema_name, table_name, key_ordinal"
        ),
        // A UDF is a lambda (`CREATE FUNCTION f AS (a, b) -> …`); its
        // parameters are untyped. The filter sits in a subquery because
        // ClickHouse resolves the `name` alias inside WHERE.
        MetadataQuery::ProcedureParameters { object, .. } => format!(
            "SELECT arrayJoin(arrayFilter(x -> x <> '', splitByRegexp('[(), ]+', extract(create_query, 'AS ([^>]*) ->')))) AS name, 'ANY' AS type_name, -1 AS max_length, 0 AS num_precision, 0 AS num_scale, 0 AS is_output, 0 AS has_default_value FROM (SELECT create_query FROM system.functions WHERE {CH_USER_FUNCTIONS} AND name = '{}') f",
            ch_lit(object)
        ),
        // Data-skipping indexes plus the primary key, which ClickHouse keeps
        // as the table's sparse index.
        MetadataQuery::Indexes { schema, table } => format!(
            "SELECT name, type_desc, is_unique, is_primary_key, filter_definition, fill_factor FROM (SELECT name, type AS type_desc, 0 AS is_unique, 0 AS is_primary_key, NULL AS filter_definition, NULL AS fill_factor FROM system.data_skipping_indices WHERE database = '{s}' AND table = '{t}' UNION ALL SELECT 'PRIMARY', 'PrimaryKey', 0, 1, NULL, NULL FROM system.tables WHERE database = '{s}' AND name = '{t}' AND primary_key <> '') indexes ORDER BY is_primary_key DESC, name",
            s = ch_lit(schema),
            t = ch_lit(table)
        ),
        // ClickHouse records dependencies for materialized views only; plain
        // views are matched on their SELECT text, as the SQLite dialect does.
        MetadataQuery::DependentViews { schema, table } => format!(
            "SELECT database AS schema_name, name, if(engine = 'MaterializedView', 1, 0) AS is_materialized, create_table_query AS definition FROM system.tables WHERE (database, name) IN (SELECT dependent_database, dependent_table FROM system.tables ARRAY JOIN dependencies_database AS dependent_database, dependencies_table AS dependent_table WHERE database = '{s}' AND name = '{t}') OR (engine IN {CH_VIEW_ENGINES} AND as_select LIKE '%{t}%') ORDER BY schema_name, name",
            s = ch_lit(schema),
            t = ch_lit(table)
        ),
        // No triggers in ClickHouse.
        MetadataQuery::Triggers { .. } => ch_empty("'' AS name, NULL AS create_date, NULL AS modify_date, 0 AS is_disabled"),
        // The primary key (from system.tables) plus CHECK / ASSUME
        // constraints (system.constraints).
        MetadataQuery::Constraints { schema, table } => format!(
            "SELECT constraint_name AS name, type_desc FROM (SELECT 'PRIMARY' AS constraint_name, 'PRIMARY_KEY_CONSTRAINT' AS type_desc, 1 AS ord FROM system.tables WHERE database = '{s}' AND name = '{t}' AND primary_key <> '' UNION ALL SELECT name, if(type = 'CHECK', 'CHECK_CONSTRAINT', 'ASSUME_CONSTRAINT'), 2 FROM system.constraints WHERE database = '{s}' AND table = '{t}') constraints ORDER BY ord, constraint_name",
            s = ch_lit(schema),
            t = ch_lit(table)
        ),
        MetadataQuery::Types { .. } => ch_empty("'' AS schema_name, '' AS name, '' AS base_type, -1 AS max_length, 0 AS num_precision, 0 AS num_scale, 0 AS is_nullable"),
        MetadataQuery::DatabaseTriggers { .. } => ch_empty("'' AS schema_name, '' AS name, NULL AS parent_name, 0 AS is_disabled"),
        MetadataQuery::Sequences { .. } => ch_empty("'' AS schema_name, '' AS name, '' AS data_type, 0 AS start_value, 0 AS increment, NULL AS current_value"),
        MetadataQuery::Synonyms { .. } => ch_empty("'' AS schema_name, '' AS name, '' AS base_object_name"),
        MetadataQuery::Extensions => ch_empty("'' AS name, '' AS version, '' AS schema_name, '' AS description"),
        MetadataQuery::AvailableExtensions => ch_empty("'' AS name, '' AS version, '' AS description, 0 AS installed"),
        MetadataQuery::Events { .. } => ch_empty("'' AS schema_name, '' AS name, '' AS status, '' AS event_type, '' AS interval_or_at, '' AS definition"),
        MetadataQuery::MaterializedViews { schema } => format!(
            "SELECT database AS schema_name, name, 1 AS is_populated FROM system.tables WHERE database = {} AND engine = 'MaterializedView' ORDER BY database, name",
            ch_scope(schema.as_deref())
        ),
        MetadataQuery::TriggerDefinition { .. } => ch_empty("'' AS definition"),
        MetadataQuery::ConstraintDefinition { schema, table, name } => format!(
            "SELECT definition FROM (SELECT concat('PRIMARY KEY (', primary_key, ')') AS definition FROM system.tables WHERE database = '{s}' AND name = '{t}' AND primary_key <> '' AND '{n}' = 'PRIMARY' UNION ALL SELECT expression FROM system.constraints WHERE database = '{s}' AND table = '{t}' AND name = '{n}') d LIMIT 1",
            s = ch_lit(schema),
            t = ch_lit(table),
            n = ch_lit(name)
        ),
        MetadataQuery::Databases => "SELECT name, 'ONLINE' AS state_desc, NULL AS compatibility_level FROM system.databases ORDER BY name".to_string(),
        MetadataQuery::ServerCapability => "SELECT version() AS product_version, 'ClickHouse' AS edition, NULL AS engine_edition, NULL AS product_level, NULL AS product_update_level, hostName() AS machine_name, 0 AS integrated_security_only".to_string(),
        MetadataQuery::TypeDetail { .. } => ch_empty("'' AS schema_name, '' AS name, '' AS base_type, -1 AS max_length, 0 AS num_precision, 0 AS num_scale, 0 AS is_nullable"),
        MetadataQuery::TableGeneral { schema, table } => format!(
            "SELECT database AS schema_name, name, NULL AS create_date, metadata_modification_time AS modify_date, total_rows AS row_count FROM system.tables WHERE database = '{}' AND name = '{}'",
            ch_lit(schema),
            ch_lit(table)
        ),
        MetadataQuery::DatabaseGeneral { database } => format!(
            "SELECT d.name AS name, 'ONLINE' AS state_desc, NULL AS collation_name, NULL AS recovery_model, NULL AS compatibility_level, NULL AS create_date, (SELECT round(sum(bytes_on_disk) / 1048576, 2) FROM system.parts WHERE active AND database = '{d}') AS size_mb, d.engine AS owner FROM system.databases d WHERE d.name = '{d}'",
            d = ch_lit(database)
        ),
        MetadataQuery::ColumnGeneral { schema, table, column } => format!(
            "SELECT database AS schema_name, table AS table_name, name, position AS ordinal, type AS data_type, character_octet_length AS max_length, numeric_precision AS `precision`, numeric_scale AS `scale`, if(startsWith(type, 'Nullable('), 1, 0) AS is_nullable, default_expression AS default_definition, 0 AS is_identity, if(default_kind IN ('ALIAS', 'MATERIALIZED'), 1, 0) AS is_computed, NULL AS collation_name FROM system.columns WHERE database = '{}' AND table = '{}' AND name = '{}'",
            ch_lit(schema),
            ch_lit(table),
            ch_lit(column)
        ),
        MetadataQuery::ObjectGeneral { schema, object } => format!(
            "SELECT schema_name, name, type_desc, create_date, modify_date FROM (SELECT database AS schema_name, name, multiIf(engine IN {CH_VIEW_ENGINES}, 'VIEW', engine = 'MaterializedView', 'MATERIALIZED_VIEW', 'USER_TABLE') AS type_desc, NULL AS create_date, metadata_modification_time AS modify_date, 1 AS ord FROM system.tables WHERE database = '{s}' AND name = '{o}' UNION ALL SELECT '{s}', name, 'SQL_SCALAR_FUNCTION', NULL, NULL, 2 FROM system.functions WHERE {CH_USER_FUNCTIONS} AND name = '{o}') objects ORDER BY ord LIMIT 1",
            s = ch_lit(schema),
            o = ch_lit(object)
        ),
        MetadataQuery::IndexGeneral { schema, table, index } => format!(
            "SELECT name, type_desc, is_unique, is_primary_key, is_disabled, filter_definition, fill_factor FROM (SELECT name, type AS type_desc, 0 AS is_unique, 0 AS is_primary_key, 0 AS is_disabled, expr AS filter_definition, toInt64(granularity) AS fill_factor FROM system.data_skipping_indices WHERE database = '{s}' AND table = '{t}' AND name = '{i}' UNION ALL SELECT 'PRIMARY', 'PrimaryKey', 0, 1, 0, primary_key, NULL FROM system.tables WHERE database = '{s}' AND name = '{t}' AND primary_key <> '' AND '{i}' = 'PRIMARY') indexes LIMIT 1",
            s = ch_lit(schema),
            t = ch_lit(table),
            i = ch_lit(index)
        ),
        MetadataQuery::IndexColumns { schema, table, index } => format!(
            "SELECT column_name AS name, key_ordinal, is_descending_key, is_included_column FROM (SELECT expr AS column_name, 1 AS key_ordinal, 0 AS is_descending_key, 0 AS is_included_column FROM system.data_skipping_indices WHERE database = '{s}' AND table = '{t}' AND name = '{i}' UNION ALL SELECT key_part, key_ordinal, 0, 0 FROM {CH_PRIMARY_KEY_PARTS} WHERE database = '{s}' AND name = '{t}' AND primary_key <> '' AND '{i}' = 'PRIMARY') columns ORDER BY key_ordinal",
            s = ch_lit(schema),
            t = ch_lit(table),
            i = ch_lit(index)
        ),
        MetadataQuery::ColumnDefaults { schema, table } => format!(
            "SELECT name AS column_name, default_expression AS default_expr, NULL AS constraint_name FROM system.columns WHERE database = '{}' AND table = '{}' AND default_expression <> '' ORDER BY position",
            ch_lit(schema),
            ch_lit(table)
        ),
        MetadataQuery::TableIndexColumns { schema, table } => format!(
            "SELECT index_name, column_name, ordinal, is_included, is_descending FROM (SELECT name AS index_name, expr AS column_name, 1 AS ordinal, 0 AS is_included, 0 AS is_descending FROM system.data_skipping_indices WHERE database = '{s}' AND table = '{t}' UNION ALL SELECT 'PRIMARY', key_part, key_ordinal, 0, 0 FROM {CH_PRIMARY_KEY_PARTS} WHERE database = '{s}' AND name = '{t}' AND primary_key <> '') columns ORDER BY index_name, ordinal",
            s = ch_lit(schema),
            t = ch_lit(table)
        ),
        // ClickHouse keeps column statistics inside parts, not as objects.
        MetadataQuery::Statistics { .. } | MetadataQuery::StatisticGeneral { .. } => ch_empty("'' AS name, 0 AS auto_created, 0 AS user_created, 0 AS no_recompute, NULL AS updated_at"),
        MetadataQuery::StatisticColumns { .. } => ch_empty("'' AS name"),
        MetadataQuery::ObjectPermissions { schema, object } => format!(
            "SELECT toString(access_type) AS permission_name, if(is_partial_revoke, 'DENY', 'GRANT') AS state_desc, 'OBJECT' AS class_desc FROM system.grants WHERE user_name = currentUser() AND (database IS NULL OR database = '{}') AND (table IS NULL OR table = '{}') ORDER BY permission_name",
            ch_lit(schema),
            ch_lit(object)
        ),
        MetadataQuery::ServerPermissions => "SELECT toString(access_type) AS permission_name, if(grant_option, 'GRANT_WITH_GRANT_OPTION', 'GRANT') AS state_desc, 'SERVER' AS class_desc FROM system.grants WHERE user_name = currentUser() AND database IS NULL AND NOT is_partial_revoke ORDER BY permission_name".to_string(),
        MetadataQuery::DatabasePermissions { database } => format!(
            "SELECT toString(access_type) AS permission_name, if(grant_option, 'GRANT_WITH_GRANT_OPTION', 'GRANT') AS state_desc, 'DATABASE' AS class_desc FROM system.grants WHERE user_name = currentUser() AND (database IS NULL OR database = '{}') AND table IS NULL AND NOT is_partial_revoke ORDER BY permission_name",
            ch_lit(database)
        ),
        MetadataQuery::ServerConfiguration => "SELECT name, value, if(changed, 'changed', 'default') AS source, 0 AS is_dynamic FROM system.settings ORDER BY name".to_string(),
        // Gauges from system.metrics, cumulative counters from system.events —
        // the same two catalogs the dashboard samples, uncurated here.
        MetadataQuery::ServerStatus => "SELECT name, value, source FROM (SELECT metric AS name, toString(value) AS value, 'metric' AS source FROM system.metrics UNION ALL SELECT event AS name, toString(value) AS value, 'event' AS source FROM system.events) ORDER BY source, name".to_string(),
        MetadataQuery::DatabaseFiles { database } => format!(
            "SELECT d.name AS logical_name, d.engine AS type_desc, d.data_path AS physical_name, (SELECT round(sum(bytes_on_disk) / 1048576, 2) FROM system.parts WHERE active AND database = '{d}') AS size_mb, NULL AS max_size_mb, NULL AS growth FROM system.databases d WHERE d.name = '{d}'",
            d = ch_lit(database)
        ),
        // Tables and views keep their CREATE text; UDFs their CREATE FUNCTION.
        MetadataQuery::ObjectDefinition { schema, object } => format!(
            "SELECT definition FROM (SELECT create_table_query AS definition, 1 AS ord FROM system.tables WHERE database = '{s}' AND name = '{o}' UNION ALL SELECT create_query, 2 FROM system.functions WHERE {CH_USER_FUNCTIONS} AND name = '{o}') d WHERE definition <> '' ORDER BY ord LIMIT 1",
            s = ch_lit(schema),
            o = ch_lit(object)
        ),
    }
}

// ── Oracle ───────────────────────────────────────────────────────────────
//
// Everything reads the `ALL_*` dictionary views, which show what the login
// can see, so the tree never errors on privileges: a schema the user cannot
// read is simply empty. Identifiers are compared as stored (Oracle folds
// unquoted names to upper case, and the explorer feeds back the names the
// catalog reported). `LONG` columns (`ALL_VIEWS.TEXT`, `DATA_DEFAULT`) are
// selected bare — Oracle refuses them inside expressions, `WHERE`, or a
// `UNION` — and definitions come from `DBMS_METADATA.GET_DDL` (a CLOB).

/// The schemas the explorer shows: every user Oracle did not create, plus
/// the login's own schema (a DBA connecting as SYSTEM still sees SYSTEM).
const ORACLE_USER_SCHEMAS: &str =
    "SELECT username FROM all_users WHERE oracle_maintained = 'N' OR username = USER";

/// `owner` filter for `alias`: one schema, or every user schema.
fn oracle_scope(alias: &str, schema: Option<&str>) -> String {
    match schema {
        Some(s) => format!("{alias}.owner = '{}'", lit(s)),
        None => format!("{alias}.owner IN ({ORACLE_USER_SCHEMAS})"),
    }
}

/// Recycle-bin tables (`BIN$…`) are dropped objects awaiting purge.
const ORACLE_NOT_RECYCLED: &str = "NOT LIKE 'BIN$%'";

/// Empty result set of the given column list.
fn oracle_empty(columns: &str) -> String {
    format!("SELECT {columns} FROM DUAL WHERE 1 = 0")
}

/// `all_objects.object_type` → the SQL Server-style `type_desc` the property
/// pages already know.
const ORACLE_TYPE_DESC: &str = "CASE o.object_type WHEN 'TABLE' THEN 'USER_TABLE' WHEN 'VIEW' THEN 'VIEW' WHEN 'MATERIALIZED VIEW' THEN 'MATERIALIZED_VIEW' WHEN 'PROCEDURE' THEN 'SQL_STORED_PROCEDURE' WHEN 'FUNCTION' THEN 'SQL_SCALAR_FUNCTION' WHEN 'PACKAGE' THEN 'PACKAGE' WHEN 'TRIGGER' THEN 'SQL_TRIGGER' WHEN 'SEQUENCE' THEN 'SEQUENCE_OBJECT' WHEN 'SYNONYM' THEN 'SYNONYM' WHEN 'TYPE' THEN 'TYPE' ELSE o.object_type END";

fn oracle_sql(query: &MetadataQuery) -> String {
    match query {
        MetadataQuery::Schemas => format!(
            "SELECT username AS name FROM ({ORACLE_USER_SCHEMAS}) ORDER BY username"
        ),
        MetadataQuery::SchemaObjectCounts => format!(
            "SELECT u.username AS name, (SELECT COUNT(*) FROM all_objects o WHERE o.owner = u.username AND o.object_type IN ('TABLE', 'VIEW', 'MATERIALIZED VIEW', 'PROCEDURE', 'FUNCTION', 'PACKAGE') AND o.object_name {ORACLE_NOT_RECYCLED}) AS object_count FROM ({ORACLE_USER_SCHEMAS}) u ORDER BY u.username"
        ),
        MetadataQuery::Tables { schema } => format!(
            "SELECT t.owner AS schema_name, t.table_name AS name, o.created AS create_date, o.last_ddl_time AS modify_date FROM all_tables t JOIN all_objects o ON o.owner = t.owner AND o.object_name = t.table_name AND o.object_type = 'TABLE' WHERE {} AND t.table_name {ORACLE_NOT_RECYCLED} AND NOT EXISTS (SELECT 1 FROM all_mviews m WHERE m.owner = t.owner AND m.mview_name = t.table_name) ORDER BY t.owner, t.table_name",
            oracle_scope("t", schema.as_deref())
        ),
        MetadataQuery::Views { schema } => format!(
            "SELECT v.owner AS schema_name, v.view_name AS name, o.created AS create_date, o.last_ddl_time AS modify_date FROM all_views v JOIN all_objects o ON o.owner = v.owner AND o.object_name = v.view_name AND o.object_type = 'VIEW' WHERE {} ORDER BY v.owner, v.view_name",
            oracle_scope("v", schema.as_deref())
        ),
        // Packages ride along as procedures ('P'): they hold the routines
        // Oracle users actually call.
        MetadataQuery::ProgrammableObjects { schema } => format!(
            "SELECT o.owner AS schema_name, o.object_name AS name, CASE o.object_type WHEN 'FUNCTION' THEN 'FN' WHEN 'TRIGGER' THEN 'TR' ELSE 'P' END AS type, o.created AS create_date, o.last_ddl_time AS modify_date FROM all_objects o WHERE {} AND o.object_type IN ('PROCEDURE', 'FUNCTION', 'PACKAGE', 'TRIGGER') ORDER BY o.owner, o.object_name",
            oracle_scope("o", schema.as_deref())
        ),
        MetadataQuery::ObjectInventory { schema } => {
            let scope = oracle_scope("o", schema.as_deref());
            format!(
                "SELECT CASE o.object_type WHEN 'TABLE' THEN 'TABLE' WHEN 'VIEW' THEN 'VIEW' WHEN 'PROCEDURE' THEN 'PROCEDURE' WHEN 'PACKAGE' THEN 'PROCEDURE' WHEN 'FUNCTION' THEN 'FUNCTION' WHEN 'TRIGGER' THEN 'TRIGGER' END AS kind, o.owner AS schema_name, o.object_name AS name, (SELECT t.num_rows FROM all_tables t WHERE t.owner = o.owner AND t.table_name = o.object_name AND o.object_type = 'TABLE') AS row_estimate, NULL AS size_kb, o.created AS create_date, o.last_ddl_time AS modify_date, (SELECT c.comments FROM all_tab_comments c WHERE c.owner = o.owner AND c.table_name = o.object_name AND o.object_type IN ('TABLE', 'VIEW')) AS \"COMMENT\" FROM all_objects o WHERE {scope} AND o.object_type IN ('TABLE', 'VIEW', 'PROCEDURE', 'PACKAGE', 'FUNCTION', 'TRIGGER') AND o.object_name {ORACLE_NOT_RECYCLED} ORDER BY kind, o.owner, o.object_name"
            )
        }
        MetadataQuery::Columns { schema, object } => format!(
            "SELECT c.column_name AS name, c.data_type AS type_name, NVL(c.char_length, c.data_length) AS max_length, NVL(c.data_precision, 0) AS precision, NVL(c.data_scale, 0) AS scale, CASE c.nullable WHEN 'Y' THEN 1 ELSE 0 END AS is_nullable, CASE c.identity_column WHEN 'YES' THEN 1 ELSE 0 END AS is_identity, CASE c.virtual_column WHEN 'YES' THEN 1 ELSE 0 END AS is_computed FROM all_tab_cols c WHERE c.owner = '{}' AND c.table_name = '{}' AND c.hidden_column = 'NO' ORDER BY c.column_id",
            lit(schema),
            lit(object)
        ),
        MetadataQuery::AllColumns => format!(
            "SELECT c.owner AS schema_name, c.table_name AS object_name, c.column_name, c.data_type AS type_name, CASE c.nullable WHEN 'Y' THEN 1 ELSE 0 END AS is_nullable, CASE c.identity_column WHEN 'YES' THEN 1 ELSE 0 END AS is_identity FROM all_tab_cols c WHERE {} AND c.hidden_column = 'NO' AND c.table_name {ORACLE_NOT_RECYCLED} ORDER BY c.owner, c.table_name, c.column_id",
            oracle_scope("c", None)
        ),
        MetadataQuery::AllForeignKeys => format!(
            "SELECT c.constraint_name, c.owner AS from_schema, c.table_name AS from_table, cc.column_name AS from_column, r.owner AS to_schema, r.table_name AS to_table, rc.column_name AS to_column FROM all_constraints c JOIN all_constraints r ON r.owner = c.r_owner AND r.constraint_name = c.r_constraint_name JOIN all_cons_columns cc ON cc.owner = c.owner AND cc.constraint_name = c.constraint_name JOIN all_cons_columns rc ON rc.owner = r.owner AND rc.constraint_name = r.constraint_name AND rc.position = cc.position WHERE c.constraint_type = 'R' AND {} ORDER BY c.constraint_name, cc.position",
            oracle_scope("c", None)
        ),
        MetadataQuery::AllPrimaryKeys => format!(
            "SELECT c.owner AS schema_name, c.table_name, cc.column_name, cc.position AS key_ordinal FROM all_constraints c JOIN all_cons_columns cc ON cc.owner = c.owner AND cc.constraint_name = c.constraint_name WHERE c.constraint_type = 'P' AND {} ORDER BY c.owner, c.table_name, cc.position",
            oracle_scope("c", None)
        ),
        // A standalone routine's arguments, or every argument of every
        // subprogram in a package of that name.
        MetadataQuery::ProcedureParameters { schema, object } => format!(
            "SELECT a.argument_name AS name, a.data_type AS type_name, NVL(a.data_length, -1) AS max_length, NVL(a.data_precision, 0) AS precision, NVL(a.data_scale, 0) AS scale, CASE a.in_out WHEN 'IN' THEN 0 ELSE 1 END AS is_output, CASE a.defaulted WHEN 'Y' THEN 1 ELSE 0 END AS has_default_value FROM all_arguments a WHERE a.owner = '{s}' AND ((a.package_name IS NULL AND a.object_name = '{o}') OR a.package_name = '{o}') AND a.argument_name IS NOT NULL ORDER BY a.subprogram_id, a.position",
            s = lit(schema),
            o = lit(object)
        ),
        MetadataQuery::Indexes { schema, table } => format!(
            "SELECT i.index_name AS name, i.index_type AS type_desc, CASE i.uniqueness WHEN 'UNIQUE' THEN 1 ELSE 0 END AS is_unique, CASE WHEN EXISTS (SELECT 1 FROM all_constraints c WHERE c.constraint_type = 'P' AND c.index_owner = i.owner AND c.index_name = i.index_name) THEN 1 ELSE 0 END AS is_primary_key, NULL AS filter_definition, NULL AS fill_factor FROM all_indexes i WHERE i.table_owner = '{}' AND i.table_name = '{}' ORDER BY i.index_name",
            lit(schema),
            lit(table)
        ),
        // Views only: ALL_VIEWS.TEXT is a LONG, which cannot join a UNION
        // with the materialized-view query text.
        MetadataQuery::DependentViews { schema, table } => format!(
            "SELECT v.owner AS schema_name, v.view_name AS name, 0 AS is_materialized, v.text AS definition FROM all_views v WHERE (v.owner, v.view_name) IN (SELECT d.owner, d.name FROM all_dependencies d WHERE d.referenced_owner = '{}' AND d.referenced_name = '{}' AND d.type = 'VIEW') ORDER BY v.owner, v.view_name",
            lit(schema),
            lit(table)
        ),
        MetadataQuery::Triggers { schema, table } => format!(
            "SELECT t.trigger_name AS name, o.created AS create_date, o.last_ddl_time AS modify_date, CASE t.status WHEN 'DISABLED' THEN 1 ELSE 0 END AS is_disabled FROM all_triggers t JOIN all_objects o ON o.owner = t.owner AND o.object_name = t.trigger_name AND o.object_type = 'TRIGGER' WHERE t.table_owner = '{}' AND t.table_name = '{}' ORDER BY t.trigger_name",
            lit(schema),
            lit(table)
        ),
        // Optimizer column statistics stand in for statistics objects.
        MetadataQuery::Statistics { schema, table } => format!(
            "SELECT s.column_name AS name, 1 AS auto_created, 0 AS user_created, 0 AS no_recompute, s.last_analyzed AS updated_at FROM all_tab_col_statistics s WHERE s.owner = '{}' AND s.table_name = '{}' ORDER BY s.column_name",
            lit(schema),
            lit(table)
        ),
        // System-generated CHECK constraints are the NOT NULL rules Oracle
        // adds per column; they stay out like every other dialect's.
        MetadataQuery::Constraints { schema, table } => format!(
            "SELECT c.constraint_name AS name, CASE c.constraint_type WHEN 'P' THEN 'PRIMARY_KEY_CONSTRAINT' WHEN 'U' THEN 'UNIQUE_CONSTRAINT' WHEN 'R' THEN 'FOREIGN_KEY_CONSTRAINT' WHEN 'C' THEN 'CHECK_CONSTRAINT' ELSE c.constraint_type END AS type_desc FROM all_constraints c WHERE c.owner = '{}' AND c.table_name = '{}' AND (c.constraint_type IN ('P', 'U', 'R') OR (c.constraint_type = 'C' AND c.generated = 'USER NAME')) ORDER BY c.constraint_name",
            lit(schema),
            lit(table)
        ),
        MetadataQuery::Types { schema } => format!(
            "SELECT t.owner AS schema_name, t.type_name AS name, t.typecode AS base_type, -1 AS max_length, 0 AS precision, 0 AS scale, 1 AS is_nullable FROM all_types t WHERE {} ORDER BY t.owner, t.type_name",
            oracle_scope("t", schema.as_deref())
        ),
        MetadataQuery::DatabaseTriggers { schema } => match schema.as_deref() {
            Some(s) => format!(
                "SELECT t.owner AS schema_name, t.trigger_name AS name, t.table_name AS parent_name, CASE t.status WHEN 'DISABLED' THEN 1 ELSE 0 END AS is_disabled FROM all_triggers t WHERE t.owner = '{}' ORDER BY t.trigger_name",
                lit(s)
            ),
            None => "SELECT t.owner AS schema_name, t.trigger_name AS name, NULL AS parent_name, CASE t.status WHEN 'DISABLED' THEN 1 ELSE 0 END AS is_disabled FROM all_triggers t WHERE t.base_object_type IN ('DATABASE', 'SCHEMA') ORDER BY t.trigger_name".to_string(),
        },
        MetadataQuery::Sequences { schema } => {
            let scope = match schema.as_deref() {
                Some(s) => format!("s.sequence_owner = '{}'", lit(s)),
                None => format!("s.sequence_owner IN ({ORACLE_USER_SCHEMAS})"),
            };
            format!(
                "SELECT s.sequence_owner AS schema_name, s.sequence_name AS name, 'NUMBER' AS data_type, s.min_value AS start_value, s.increment_by AS increment_value, s.last_number AS current_value FROM all_sequences s WHERE {scope} ORDER BY s.sequence_owner, s.sequence_name"
            )
        }
        MetadataQuery::Synonyms { schema } => format!(
            "SELECT s.owner AS schema_name, s.synonym_name AS name, s.table_owner || '.' || s.table_name || CASE WHEN s.db_link IS NULL THEN '' ELSE '@' || s.db_link END AS base_object_name FROM all_synonyms s WHERE {} ORDER BY s.owner, s.synonym_name",
            oracle_scope("s", schema.as_deref())
        ),
        MetadataQuery::Extensions => oracle_empty(
            "CAST(NULL AS VARCHAR2(128)) AS name, CAST(NULL AS VARCHAR2(128)) AS version, CAST(NULL AS VARCHAR2(128)) AS schema_name, CAST(NULL AS VARCHAR2(4000)) AS description",
        ),
        MetadataQuery::AvailableExtensions => oracle_empty(
            "CAST(NULL AS VARCHAR2(128)) AS name, CAST(NULL AS VARCHAR2(128)) AS version, CAST(NULL AS VARCHAR2(4000)) AS description, 0 AS installed",
        ),
        MetadataQuery::Events { .. } => oracle_empty(
            "CAST(NULL AS VARCHAR2(128)) AS schema_name, CAST(NULL AS VARCHAR2(128)) AS name, CAST(NULL AS VARCHAR2(60)) AS status, CAST(NULL AS VARCHAR2(60)) AS event_type, CAST(NULL AS VARCHAR2(4000)) AS interval_or_at, CAST(NULL AS VARCHAR2(4000)) AS definition",
        ),
        MetadataQuery::MaterializedViews { schema } => format!(
            "SELECT m.owner AS schema_name, m.mview_name AS name, CASE WHEN m.last_refresh_date IS NULL THEN 0 ELSE 1 END AS is_populated FROM all_mviews m WHERE {} ORDER BY m.owner, m.mview_name",
            oracle_scope("m", schema.as_deref())
        ),
        MetadataQuery::TriggerDefinition { schema, name } => format!(
            "SELECT DBMS_METADATA.GET_DDL('TRIGGER', t.trigger_name, t.owner) AS definition FROM all_triggers t WHERE t.owner = '{}' AND t.trigger_name = '{}'",
            lit(schema),
            lit(name)
        ),
        MetadataQuery::ConstraintDefinition { schema, table, name } => format!(
            "SELECT CASE c.constraint_type WHEN 'R' THEN DBMS_METADATA.GET_DDL('REF_CONSTRAINT', c.constraint_name, c.owner) ELSE DBMS_METADATA.GET_DDL('CONSTRAINT', c.constraint_name, c.owner) END AS definition FROM all_constraints c WHERE c.owner = '{}' AND c.table_name = '{}' AND c.constraint_name = '{}'",
            lit(schema),
            lit(table),
            lit(name)
        ),
        MetadataQuery::TypeDetail { schema, type_name } => format!(
            "SELECT t.owner AS schema_name, t.type_name AS name, t.typecode AS base_type, -1 AS max_length, 0 AS precision, 0 AS scale, 1 AS is_nullable FROM all_types t WHERE t.owner = '{}' AND t.type_name = '{}'",
            lit(schema),
            lit(type_name)
        ),
        MetadataQuery::TableGeneral { schema, table } => format!(
            "SELECT t.owner AS schema_name, t.table_name AS name, o.created AS create_date, o.last_ddl_time AS modify_date, t.num_rows AS row_count FROM all_tables t JOIN all_objects o ON o.owner = t.owner AND o.object_name = t.table_name AND o.object_type = 'TABLE' WHERE t.owner = '{}' AND t.table_name = '{}'",
            lit(schema),
            lit(table)
        ),
        // The container the session is in; v$ views need extra grants, so
        // the row is built from public dictionary functions.
        MetadataQuery::DatabaseGeneral { database } => format!(
            "SELECT '{}' AS name, 'ONLINE' AS state_desc, (SELECT p.value FROM nls_database_parameters p WHERE p.parameter = 'NLS_CHARACTERSET') AS collation_name, NULL AS recovery_model, (SELECT v.version FROM product_component_version v WHERE v.product LIKE 'Oracle%' FETCH FIRST 1 ROWS ONLY) AS compatibility_level, NULL AS create_date, (SELECT ROUND(SUM(s.bytes) / 1048576, 2) FROM user_segments s) AS size_mb, USER AS owner FROM DUAL",
            lit(database)
        ),
        // Storage the login owns, per tablespace (USER_SEGMENTS needs no
        // privilege; datafile detail does).
        MetadataQuery::DatabaseFiles { .. } => "SELECT s.tablespace_name AS logical_name, 'TABLESPACE' AS type_desc, NULL AS physical_name, ROUND(SUM(s.bytes) / 1048576, 1) AS size_mb, NULL AS max_size_mb, NULL AS growth FROM user_segments s GROUP BY s.tablespace_name ORDER BY s.tablespace_name".to_string(),
        MetadataQuery::DatabasePermissions { .. } => "SELECT p.privilege AS permission_name, 'GRANT' AS state_desc, 'DATABASE' AS class_desc FROM user_sys_privs p UNION ALL SELECT r.granted_role, 'GRANT', 'ROLE' FROM user_role_privs r ORDER BY 1".to_string(),
        MetadataQuery::ServerPermissions => "SELECT p.privilege AS permission_name, 'GRANT' AS state_desc, 'SERVER' AS class_desc FROM user_sys_privs p UNION ALL SELECT r.granted_role, 'GRANT', 'ROLE' FROM user_role_privs r ORDER BY 1".to_string(),
        MetadataQuery::ColumnGeneral { schema, table, column } => format!(
            "SELECT c.owner AS schema_name, c.table_name, c.column_name AS name, c.column_id AS ordinal, c.data_type, NVL(c.char_length, c.data_length) AS max_length, c.data_precision AS precision, c.data_scale AS scale, CASE c.nullable WHEN 'Y' THEN 1 ELSE 0 END AS is_nullable, c.data_default AS default_definition, CASE c.identity_column WHEN 'YES' THEN 1 ELSE 0 END AS is_identity, CASE c.virtual_column WHEN 'YES' THEN 1 ELSE 0 END AS is_computed, NULL AS collation_name FROM all_tab_cols c WHERE c.owner = '{}' AND c.table_name = '{}' AND c.column_name = '{}'",
            lit(schema),
            lit(table),
            lit(column)
        ),
        MetadataQuery::ObjectGeneral { schema, object } => format!(
            "SELECT o.owner AS schema_name, o.object_name AS name, {ORACLE_TYPE_DESC} AS type_desc, o.created AS create_date, o.last_ddl_time AS modify_date FROM all_objects o WHERE o.owner = '{}' AND o.object_name = '{}' AND o.object_type NOT IN ('PACKAGE BODY', 'TYPE BODY', 'INDEX', 'LOB') ORDER BY CASE o.object_type WHEN 'TABLE' THEN 1 WHEN 'VIEW' THEN 2 WHEN 'MATERIALIZED VIEW' THEN 3 ELSE 4 END FETCH FIRST 1 ROWS ONLY",
            lit(schema),
            lit(object)
        ),
        MetadataQuery::IndexGeneral { schema, table, index } => format!(
            "SELECT i.index_name AS name, i.index_type AS type_desc, CASE i.uniqueness WHEN 'UNIQUE' THEN 1 ELSE 0 END AS is_unique, CASE WHEN EXISTS (SELECT 1 FROM all_constraints c WHERE c.constraint_type = 'P' AND c.index_owner = i.owner AND c.index_name = i.index_name) THEN 1 ELSE 0 END AS is_primary_key, CASE i.status WHEN 'UNUSABLE' THEN 1 ELSE 0 END AS is_disabled, NULL AS filter_definition, NULL AS fill_factor FROM all_indexes i WHERE i.table_owner = '{}' AND i.table_name = '{}' AND i.index_name = '{}'",
            lit(schema),
            lit(table),
            lit(index)
        ),
        MetadataQuery::IndexColumns { schema, table, index } => format!(
            "SELECT ic.column_name AS name, ic.column_position AS key_ordinal, CASE ic.descend WHEN 'DESC' THEN 1 ELSE 0 END AS is_descending_key, 0 AS is_included_column FROM all_ind_columns ic WHERE ic.table_owner = '{}' AND ic.table_name = '{}' AND ic.index_name = '{}' ORDER BY ic.column_position",
            lit(schema),
            lit(table),
            lit(index)
        ),
        // DATA_DEFAULT is a LONG: selected bare, filtered by DEFAULT_LENGTH.
        MetadataQuery::ColumnDefaults { schema, table } => format!(
            "SELECT c.column_name, c.data_default AS default_expr, NULL AS constraint_name FROM all_tab_cols c WHERE c.owner = '{}' AND c.table_name = '{}' AND c.default_length IS NOT NULL AND c.hidden_column = 'NO' ORDER BY c.column_id",
            lit(schema),
            lit(table)
        ),
        MetadataQuery::TableIndexColumns { schema, table } => format!(
            "SELECT ic.index_name, ic.column_name, ic.column_position AS ordinal, 0 AS is_included, CASE ic.descend WHEN 'DESC' THEN 1 ELSE 0 END AS is_descending FROM all_ind_columns ic WHERE ic.table_owner = '{}' AND ic.table_name = '{}' ORDER BY ic.index_name, ic.column_position",
            lit(schema),
            lit(table)
        ),
        MetadataQuery::StatisticGeneral { schema, table, statistic } => format!(
            "SELECT s.column_name AS name, 1 AS auto_created, 0 AS user_created, 0 AS no_recompute, s.last_analyzed AS updated_at FROM all_tab_col_statistics s WHERE s.owner = '{}' AND s.table_name = '{}' AND s.column_name = '{}'",
            lit(schema),
            lit(table),
            lit(statistic)
        ),
        MetadataQuery::StatisticColumns { schema, table, statistic } => format!(
            "SELECT s.column_name AS name FROM all_tab_col_statistics s WHERE s.owner = '{}' AND s.table_name = '{}' AND s.column_name = '{}'",
            lit(schema),
            lit(table),
            lit(statistic)
        ),
        // GET_DDL('PACKAGE') already includes the body, so body rows are
        // skipped; a table wins over a same-named index or LOB segment.
        MetadataQuery::ObjectDefinition { schema, object } => format!(
            "SELECT DBMS_METADATA.GET_DDL(REPLACE(o.object_type, ' ', '_'), o.object_name, o.owner) AS definition FROM all_objects o WHERE o.owner = '{}' AND o.object_name = '{}' AND o.object_type IN ('TABLE', 'VIEW', 'MATERIALIZED VIEW', 'PROCEDURE', 'FUNCTION', 'PACKAGE', 'TRIGGER', 'SEQUENCE', 'SYNONYM', 'TYPE', 'INDEX') ORDER BY CASE o.object_type WHEN 'TABLE' THEN 1 WHEN 'VIEW' THEN 2 WHEN 'MATERIALIZED VIEW' THEN 3 WHEN 'INDEX' THEN 9 ELSE 4 END FETCH FIRST 1 ROWS ONLY",
            lit(schema),
            lit(object)
        ),
        MetadataQuery::ObjectPermissions { schema, object } => format!(
            "SELECT p.privilege AS permission_name, 'GRANT' AS state_desc, 'OBJECT' AS class_desc FROM all_tab_privs p WHERE p.table_schema = '{s}' AND p.table_name = '{o}' AND p.grantee IN (USER, 'PUBLIC') UNION ALL SELECT 'OWNER', 'GRANT', 'OBJECT' FROM DUAL WHERE USER = '{s}' AND EXISTS (SELECT 1 FROM all_objects o WHERE o.owner = '{s}' AND o.object_name = '{o}') ORDER BY 1",
            s = lit(schema),
            o = lit(object)
        ),
        MetadataQuery::Databases => "SELECT SYS_CONTEXT('USERENV', 'CON_NAME') AS name, 'ONLINE' AS state_desc, NULL AS compatibility_level FROM DUAL".to_string(),
        MetadataQuery::ServerCapability => "SELECT (SELECT v.version FROM product_component_version v WHERE v.product LIKE 'Oracle%' FETCH FIRST 1 ROWS ONLY) AS product_version, 'Oracle Database' AS edition, NULL AS engine_edition, NULL AS product_level, NULL AS product_update_level, SYS_CONTEXT('USERENV', 'SERVER_HOST') AS machine_name, 0 AS integrated_security_only FROM DUAL".to_string(),
        MetadataQuery::ServerConfiguration => "SELECT p.parameter AS name, p.value, 'NLS session' AS source, 0 AS is_dynamic FROM nls_session_parameters p UNION ALL SELECT d.parameter, d.value, 'NLS database', 0 FROM nls_database_parameters d ORDER BY 1, 3".to_string(),
        // v$sysstat's two headline classes: user (1) and cache (8). Like the
        // dashboard's other V$ reads this needs SELECT_CATALOG_ROLE and fails
        // on its own without it — the page reports that error rather than
        // substituting a partial answer.
        MetadataQuery::ServerStatus => "SELECT name, TO_CHAR(value) AS value, DECODE(class, 1, 'user', 8, 'cache', 'other') AS source FROM v$sysstat WHERE class IN (1, 8) ORDER BY name".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clickhouse_dialect_scopes_by_database_escapes_literals_and_returns_empty_sets() {
        assert_eq!(
            MetadataDialect::for_database_type(DatabaseType::ClickHouse),
            Some(MetadataDialect::ClickHouse)
        );
        let tables = sql_for_dialect(
            &MetadataQuery::Tables {
                schema: Some("it's\\".into()),
            },
            MetadataDialect::ClickHouse,
        )
        .unwrap();
        assert!(tables.contains("system.tables"));
        assert!(tables.contains("database = 'it''s\\\\'"), "{tables}");
        assert!(
            tables.contains("'MaterializedView'"),
            "views are not tables"
        );
        let all = sql_for_dialect(
            &MetadataQuery::Tables { schema: None },
            MetadataDialect::ClickHouse,
        )
        .unwrap();
        assert!(all.contains("database = currentDatabase()"));
        let views = sql_for_dialect(
            &MetadataQuery::Views { schema: None },
            MetadataDialect::ClickHouse,
        )
        .unwrap();
        assert!(views.contains("engine IN ('View', 'LiveView', 'WindowView')"));
        let materialized = sql_for_dialect(
            &MetadataQuery::MaterializedViews { schema: None },
            MetadataDialect::ClickHouse,
        )
        .unwrap();
        assert!(materialized.contains("engine = 'MaterializedView'"));
        for query in [
            MetadataQuery::AllForeignKeys,
            MetadataQuery::Triggers {
                schema: "d".into(),
                table: "t".into(),
            },
            MetadataQuery::Sequences { schema: None },
            MetadataQuery::Types { schema: None },
            MetadataQuery::Extensions,
            MetadataQuery::Events { schema: None },
        ] {
            let sql = sql_for_dialect(&query, MetadataDialect::ClickHouse).unwrap();
            assert!(sql.ends_with("WHERE 0"), "{query:?}: {sql}");
        }
        let indexes = sql_for_dialect(
            &MetadataQuery::Indexes {
                schema: "d".into(),
                table: "t".into(),
            },
            MetadataDialect::ClickHouse,
        )
        .unwrap();
        assert!(indexes.contains("system.data_skipping_indices"));
        assert!(indexes.contains("'PRIMARY'"));
        let columns = sql_for_dialect(
            &MetadataQuery::Columns {
                schema: "d".into(),
                object: "t".into(),
            },
            MetadataDialect::ClickHouse,
        )
        .unwrap();
        assert!(columns.contains("system.columns"));
        assert!(columns.contains("startsWith(type, 'Nullable(')"));
        assert!(columns.contains("default_kind IN ('ALIAS', 'MATERIALIZED')"));
    }

    #[test]
    fn duckdb_dialect_scopes_to_the_current_database_and_escapes_literals() {
        assert_eq!(
            MetadataDialect::for_database_type(DatabaseType::DuckDb),
            Some(MetadataDialect::DuckDb)
        );
        let tables = sql_for_dialect(
            &MetadataQuery::Tables {
                schema: Some("it's".into()),
            },
            MetadataDialect::DuckDb,
        )
        .unwrap();
        assert!(tables.contains("duckdb_tables()"));
        assert!(tables.contains("current_database()"));
        assert!(tables.contains("schema_name = 'it''s'"));
        let all = sql_for_dialect(
            &MetadataQuery::Tables { schema: None },
            MetadataDialect::DuckDb,
        )
        .unwrap();
        assert!(!all.contains("schema_name ="));
        let empty = sql_for_dialect(
            &MetadataQuery::Triggers {
                schema: "main".into(),
                table: "t".into(),
            },
            MetadataDialect::DuckDb,
        )
        .unwrap();
        assert!(empty.ends_with("WHERE 1 = 0"));
    }

    #[test]
    fn dialect_derives_from_connection_kind() {
        assert_eq!(
            MetadataDialect::for_database_type(DatabaseType::SqlServer),
            Some(MetadataDialect::SqlServer)
        );
        assert_eq!(
            MetadataDialect::for_database_type(DatabaseType::CockroachDb),
            Some(MetadataDialect::Postgres)
        );
        assert_eq!(
            MetadataDialect::for_database_type(DatabaseType::MariaDb),
            Some(MetadataDialect::MySql)
        );
        assert_eq!(
            MetadataDialect::for_database_type(DatabaseType::Sqlite),
            Some(MetadataDialect::Sqlite)
        );
        assert_eq!(
            MetadataDialect::for_database_type(DatabaseType::Oracle),
            Some(MetadataDialect::Oracle)
        );
        assert_eq!(MetadataDialect::for_database_type(DatabaseType::Db2), None);
    }

    #[test]
    fn oracle_dialect_scopes_by_owner_and_escapes_literals() {
        let tables = sql_for_dialect(
            &MetadataQuery::Tables {
                schema: Some("IT'S".into()),
            },
            MetadataDialect::Oracle,
        )
        .unwrap();
        assert!(tables.contains("all_tables"), "{tables}");
        assert!(tables.contains("t.owner = 'IT''S'"), "{tables}");
        assert!(tables.contains("BIN$"), "{tables}");
        let all = sql_for_dialect(
            &MetadataQuery::Tables { schema: None },
            MetadataDialect::Oracle,
        )
        .unwrap();
        assert!(
            all.contains("oracle_maintained = 'N' OR username = USER"),
            "{all}"
        );
        let fks = sql_for_dialect(&MetadataQuery::AllForeignKeys, MetadataDialect::Oracle).unwrap();
        assert!(fks.contains("constraint_type = 'R'"), "{fks}");
        assert!(fks.contains("rc.position = cc.position"), "{fks}");
        let def = sql_for_dialect(
            &MetadataQuery::ObjectDefinition {
                schema: "HR".into(),
                object: "EMP".into(),
            },
            MetadataDialect::Oracle,
        )
        .unwrap();
        assert!(def.contains("DBMS_METADATA.GET_DDL"), "{def}");
        assert!(!def.contains("'PACKAGE BODY'"), "{def}");
        let empty = sql_for_dialect(&MetadataQuery::Extensions, MetadataDialect::Oracle).unwrap();
        assert!(empty.ends_with("FROM DUAL WHERE 1 = 0"), "{empty}");
        // LONG columns are never wrapped in expressions or filtered on.
        let defaults = sql_for_dialect(
            &MetadataQuery::ColumnDefaults {
                schema: "HR".into(),
                table: "EMP".into(),
            },
            MetadataDialect::Oracle,
        )
        .unwrap();
        assert!(
            defaults.contains("c.data_default AS default_expr"),
            "{defaults}"
        );
        assert!(
            defaults.contains("default_length IS NOT NULL"),
            "{defaults}"
        );
        assert!(!defaults.contains("data_default IS"), "{defaults}");
    }

    #[test]
    fn sql_server_delegates_to_the_original_generator() {
        let q = MetadataQuery::Schemas;
        assert_eq!(
            sql_for_dialect(&q, MetadataDialect::SqlServer).unwrap(),
            crate::metadata::sql_for(&q)
        );
    }

    #[test]
    fn every_service_variant_is_covered_on_every_dialect() {
        // The variants DefaultMetadataService issues. Every dialect must
        // produce SQL for each (empty result sets count as support).
        let service_variants = [
            MetadataQuery::Schemas,
            MetadataQuery::SchemaObjectCounts,
            MetadataQuery::Tables { schema: None },
            MetadataQuery::Views { schema: None },
            MetadataQuery::ProgrammableObjects { schema: None },
            MetadataQuery::Columns {
                schema: "s".into(),
                object: "o".into(),
            },
            MetadataQuery::AllColumns,
            MetadataQuery::AllForeignKeys,
            MetadataQuery::AllPrimaryKeys,
            MetadataQuery::ProcedureParameters {
                schema: "s".into(),
                object: "o".into(),
            },
            MetadataQuery::Indexes {
                schema: "s".into(),
                table: "t".into(),
            },
            MetadataQuery::ColumnDefaults {
                schema: "s".into(),
                table: "t".into(),
            },
            MetadataQuery::TableIndexColumns {
                schema: "s".into(),
                table: "t".into(),
            },
            MetadataQuery::Triggers {
                schema: "s".into(),
                table: "t".into(),
            },
            MetadataQuery::Constraints {
                schema: "s".into(),
                table: "t".into(),
            },
            MetadataQuery::Types { schema: None },
            MetadataQuery::DatabaseTriggers { schema: None },
            MetadataQuery::Sequences { schema: None },
            MetadataQuery::Synonyms { schema: None },
            MetadataQuery::Extensions,
            MetadataQuery::AvailableExtensions,
            MetadataQuery::Events { schema: None },
            MetadataQuery::MaterializedViews { schema: None },
            MetadataQuery::TriggerDefinition {
                schema: "s".into(),
                name: "tr".into(),
            },
            MetadataQuery::ConstraintDefinition {
                schema: "s".into(),
                table: "t".into(),
                name: "c".into(),
            },
        ];
        for dialect in [
            MetadataDialect::SqlServer,
            MetadataDialect::Postgres,
            MetadataDialect::MySql,
            MetadataDialect::Sqlite,
            MetadataDialect::DuckDb,
            MetadataDialect::Oracle,
        ] {
            for variant in &service_variants {
                assert!(
                    sql_for_dialect(variant, dialect).is_ok(),
                    "{dialect:?} must cover {variant:?}"
                );
            }
        }
        // Statistics now has an analogue everywhere: extended statistics on
        // PostgreSQL, per-index cardinality statistics on MySQL, and an
        // honest empty set on SQLite.
        let stats = MetadataQuery::Statistics {
            schema: "s".into(),
            table: "t".into(),
        };
        for dialect in [
            MetadataDialect::Postgres,
            MetadataDialect::MySql,
            MetadataDialect::Sqlite,
            MetadataDialect::DuckDb,
            MetadataDialect::Oracle,
        ] {
            assert!(sql_for_dialect(&stats, dialect).is_ok(), "{dialect:?}");
        }
    }

    #[test]
    fn property_page_variants_are_covered_on_every_dialect() {
        let variants = [
            MetadataQuery::Databases,
            MetadataQuery::ServerCapability,
            MetadataQuery::TypeDetail {
                schema: "s".into(),
                type_name: "t".into(),
            },
            MetadataQuery::TableGeneral {
                schema: "s".into(),
                table: "t".into(),
            },
            MetadataQuery::ObjectGeneral {
                schema: "s".into(),
                object: "o".into(),
            },
            MetadataQuery::IndexGeneral {
                schema: "s".into(),
                table: "t".into(),
                index: "i".into(),
            },
            MetadataQuery::IndexColumns {
                schema: "s".into(),
                table: "t".into(),
                index: "i".into(),
            },
            MetadataQuery::ColumnDefaults {
                schema: "s".into(),
                table: "t".into(),
            },
            MetadataQuery::TableIndexColumns {
                schema: "s".into(),
                table: "t".into(),
            },
            MetadataQuery::StatisticGeneral {
                schema: "s".into(),
                table: "t".into(),
                statistic: "st".into(),
            },
            MetadataQuery::StatisticColumns {
                schema: "s".into(),
                table: "t".into(),
                statistic: "st".into(),
            },
            MetadataQuery::ObjectPermissions {
                schema: "s".into(),
                object: "o".into(),
            },
            MetadataQuery::ObjectDefinition {
                schema: "s".into(),
                object: "o".into(),
            },
        ];
        for dialect in [
            MetadataDialect::SqlServer,
            MetadataDialect::Postgres,
            MetadataDialect::MySql,
            MetadataDialect::Sqlite,
            MetadataDialect::DuckDb,
            MetadataDialect::Oracle,
        ] {
            for variant in &variants {
                assert!(
                    sql_for_dialect(variant, dialect).is_ok(),
                    "{dialect:?} must cover {variant:?}"
                );
            }
        }
    }

    #[test]
    fn literals_are_escaped_per_dialect() {
        let q = MetadataQuery::Columns {
            schema: "we'ird".into(),
            object: "ta\\ble".into(),
        };
        let pg = sql_for_dialect(&q, MetadataDialect::Postgres).unwrap();
        assert!(pg.contains("we''ird"));
        assert!(pg.contains("ta\\ble"), "backslash is literal in pg: {pg}");
        let my = sql_for_dialect(&q, MetadataDialect::MySql).unwrap();
        assert!(my.contains("we''ird"));
        assert!(my.contains("ta\\\\ble"), "backslash doubled in mysql: {my}");
    }

    /// The server and database property pages render the same rows whatever
    /// the engine is, so every dialect must answer all six of their queries —
    /// with an empty set where the engine has no such concept, never an error.
    #[test]
    fn server_and_database_property_variants_are_covered_everywhere() {
        let queries = [
            MetadataQuery::ServerCapability,
            MetadataQuery::ServerPermissions,
            MetadataQuery::ServerConfiguration,
            MetadataQuery::ServerStatus,
            MetadataQuery::Databases,
            MetadataQuery::DatabaseGeneral {
                database: "we'ird".into(),
            },
            MetadataQuery::DatabaseFiles {
                database: "we'ird".into(),
            },
            MetadataQuery::DatabasePermissions {
                database: "we'ird".into(),
            },
        ];
        for dialect in [
            MetadataDialect::SqlServer,
            MetadataDialect::Postgres,
            MetadataDialect::MySql,
            MetadataDialect::Sqlite,
            MetadataDialect::DuckDb,
            MetadataDialect::Oracle,
        ] {
            for query in &queries {
                let sql = sql_for_dialect(query, dialect)
                    .unwrap_or_else(|err| panic!("{dialect:?} lacks {query:?}: {err}"));
                assert!(
                    sql.to_ascii_uppercase().contains("SELECT"),
                    "{dialect:?} {query:?} is not a SELECT: {sql}"
                );
            }
        }
    }

    #[test]
    fn database_scoped_property_queries_escape_their_literal() {
        for dialect in [
            MetadataDialect::SqlServer,
            MetadataDialect::Postgres,
            MetadataDialect::MySql,
            MetadataDialect::Oracle,
        ] {
            let sql = sql_for_dialect(
                &MetadataQuery::DatabaseGeneral {
                    database: "we'ird".into(),
                },
                dialect,
            )
            .unwrap();
            assert!(sql.contains("we''ird"), "{dialect:?}: {sql}");
        }
    }

    #[test]
    fn object_definition_is_covered_on_every_dialect() {
        let q = MetadataQuery::ObjectDefinition {
            schema: "we'ird".into(),
            object: "obj".into(),
        };
        let pg = sql_for_dialect(&q, MetadataDialect::Postgres).unwrap();
        assert!(pg.contains("pg_get_viewdef"), "{pg}");
        assert!(pg.contains("pg_get_functiondef"), "{pg}");
        assert!(pg.contains("we''ird"), "{pg}");
        let my = sql_for_dialect(&q, MetadataDialect::MySql).unwrap();
        assert!(my.contains("information_schema.views"), "{my}");
        assert!(my.contains("information_schema.routines"), "{my}");
        assert!(my.contains("information_schema.triggers"), "{my}");
        assert!(my.contains("we''ird"), "{my}");
        let sq = sql_for_dialect(&q, MetadataDialect::Sqlite).unwrap();
        assert!(sq.contains("sqlite_master"), "{sq}");
        assert!(sq.contains("'obj'"), "{sq}");
        // The routine body must not be null-coalesced on MySQL: a withheld
        // body collapses the row so callers report "no definition".
        assert!(!my.contains("COALESCE(r.routine_definition"), "{my}");
    }

    #[test]
    fn cockroach_object_definition_avoids_pg_get_functiondef() {
        let q = MetadataQuery::ObjectDefinition {
            schema: "public".into(),
            object: "fn".into(),
        };
        let sql = sql_for_engine(&q, DatabaseType::CockroachDb).unwrap();
        assert!(!sql.contains("pg_get_functiondef"), "{sql}");
        assert!(sql.contains("prosrc"), "{sql}");
        assert!(sql.contains("pg_get_viewdef"), "{sql}");
    }

    /// Expression keys (`CREATE INDEX … ((lower(email)))`) have attnum 0, so
    /// an inner join on pg_attribute dropped them: a functional index showed
    /// no key at all. The key text comes from pg_get_indexdef per position,
    /// INCLUDE columns are the positions past indnkeyatts, and DESC comes from
    /// indoption.
    #[test]
    fn postgres_index_columns_keep_expression_keys_include_and_desc() {
        let one = sql_for_dialect(
            &MetadataQuery::IndexColumns {
                schema: "public".into(),
                table: "users".into(),
                index: "ix_users_lower_email".into(),
            },
            MetadataDialect::Postgres,
        )
        .unwrap();
        let all = sql_for_dialect(
            &MetadataQuery::TableIndexColumns {
                schema: "public".into(),
                table: "users".into(),
            },
            MetadataDialect::Postgres,
        )
        .unwrap();
        for sql in [&one, &all] {
            assert!(sql.contains("LEFT JOIN pg_catalog.pg_attribute a"), "{sql}");
            assert!(
                sql.contains("pg_catalog.pg_get_indexdef(ix.indexrelid, k.ord::int, true)"),
                "{sql}"
            );
            assert!(sql.contains("k.ord > ix.indnkeyatts"), "{sql}");
            assert!(sql.contains("ix.indoption"), "{sql}");
        }
        assert!(!one.contains("0 AS is_descending_key"), "{one}");
        assert!(!one.contains("0 AS is_included_column"), "{one}");
        assert!(!all.contains("0 AS is_descending"), "{all}");
        // The snapshot needs to know which keys are expressions, and which
        // carry a NULLS FIRST/LAST or operator class a column list would lose.
        assert!(all.contains("AS is_expression, "), "{all}");
        assert!(all.contains("AS has_options FROM"), "{all}");
        assert!(
            all.contains("(ix.indoption[(k.ord - 1)::int]::int & 3) IN (1, 2)"),
            "{all}"
        );
        assert!(
            all.contains("oc.oid = ix.indclass[(k.ord - 1)::int] AND NOT oc.opcdefault"),
            "{all}"
        );
        assert!(!one.contains("has_options"), "{one}");
    }

    /// Several MySQL/MariaDB triggers on the same table, timing and event fire
    /// in ACTION_ORDER; a CREATE without `FOLLOWS` puts a re-created trigger
    /// last. Both trigger definitions name the trigger it follows, and quote
    /// every identifier.
    #[test]
    fn mysql_trigger_definitions_keep_follows_and_quote_names() {
        let object = sql_for_dialect(
            &MetadataQuery::ObjectDefinition {
                schema: "app".into(),
                object: "trg_b".into(),
            },
            MetadataDialect::MySql,
        )
        .unwrap();
        let trigger = sql_for_dialect(
            &MetadataQuery::TriggerDefinition {
                schema: "app".into(),
                name: "trg_b".into(),
            },
            MetadataDialect::MySql,
        )
        .unwrap();
        for sql in [&object, &trigger] {
            assert!(sql.contains("' FOLLOWS `'"), "{sql}");
            assert!(sql.contains("p.action_order = t.action_order - 1"), "{sql}");
            assert!(
                sql.contains("p.event_object_table = t.event_object_table"),
                "{sql}"
            );
            assert!(sql.contains("REPLACE(t.trigger_name, '`', '``')"), "{sql}");
            assert!(
                sql.contains("REPLACE(t.event_object_table, '`', '``')"),
                "{sql}"
            );
        }
    }

    /// The effective object permissions are the current user's own grants
    /// plus PUBLIC's, read from the ACL by oid: a real role named "PUBLIC"
    /// is not the PUBLIC pseudo-role.
    #[test]
    fn postgres_object_permissions_tell_public_from_a_role_named_public() {
        let sql = sql_for_dialect(
            &MetadataQuery::ObjectPermissions {
                schema: "public".into(),
                object: "orders".into(),
            },
            MetadataDialect::Postgres,
        )
        .unwrap();
        assert!(!sql.contains("'PUBLIC'"), "{sql}");
        assert!(sql.contains("aclexplode("), "{sql}");
        assert!(sql.contains("a.grantee = 0"), "{sql}");
        assert!(sql.contains("rolname = CURRENT_USER"), "{sql}");
    }

    #[test]
    fn postgres_extensions_query_reads_pg_extension() {
        let sql = sql_for_dialect(&MetadataQuery::Extensions, MetadataDialect::Postgres).unwrap();
        assert!(sql.contains("pg_extension"));
        assert!(sql.contains("pg_available_extensions"));
        assert!(sql.contains("extname"));
    }

    /// The Configuration page's Edit facet keys on the is_dynamic column every
    /// dialect now reports: SQL Server and PostgreSQL from their catalogs,
    /// MariaDB from its read_only flag, MySQL from the documented list of
    /// startup-only variables (its catalogs record no mutability flag),
    /// CockroachDB from its cluster settings, and the file/session engines as
    /// read-only.
    #[test]
    fn server_configuration_sql_marks_dynamic_rows_per_engine() {
        let tds = sql_for_dialect(
            &MetadataQuery::ServerConfiguration,
            MetadataDialect::SqlServer,
        )
        .unwrap();
        assert!(tds.contains("sys.configurations"), "{tds}");
        assert!(tds.contains("is_dynamic"), "{tds}");
        assert!(tds.contains("is_advanced"), "{tds}");
        let pg = sql_for_dialect(
            &MetadataQuery::ServerConfiguration,
            MetadataDialect::Postgres,
        )
        .unwrap();
        assert!(
            pg.contains("context IN ('user', 'superuser', 'sighup'"),
            "{pg}"
        );
        assert!(
            !pg.contains("'postmaster'"),
            "restart-only settings stay read-only: {pg}"
        );
        assert!(
            pg.contains("current_setting(name) AS value"),
            "values are valid ALTER SYSTEM input, units folded in: {pg}"
        );
        let my = sql_for_engine(&MetadataQuery::ServerConfiguration, DatabaseType::MySql).unwrap();
        assert!(my.contains("performance_schema.global_variables"), "{my}");
        for read_only in ["'datadir'", "'port'", "'innodb_buffer_pool_instances'"] {
            assert!(
                my.contains(read_only),
                "{read_only} must be read-only: {my}"
            );
        }
        assert!(
            !my.contains("'max_connections'"),
            "dynamic variables stay editable: {my}"
        );
        assert!(my.contains("THEN 0 ELSE 1 END AS is_dynamic"), "{my}");
        // CockroachDB edits cluster settings, so it must list them — not
        // pg_settings' session variables, which SET CLUSTER SETTING rejects.
        let crdb = sql_for_engine(
            &MetadataQuery::ServerConfiguration,
            DatabaseType::CockroachDb,
        )
        .unwrap();
        assert!(crdb.contains("crdb_internal.cluster_settings"), "{crdb}");
        assert!(!crdb.contains("pg_settings"), "{crdb}");
        let maria =
            sql_for_engine(&MetadataQuery::ServerConfiguration, DatabaseType::MariaDb).unwrap();
        assert!(
            maria.contains("information_schema.SYSTEM_VARIABLES"),
            "{maria}"
        );
        assert!(maria.contains("read_only = 'NO'"), "{maria}");
        for dialect in [
            MetadataDialect::Sqlite,
            MetadataDialect::DuckDb,
            MetadataDialect::ClickHouse,
            MetadataDialect::Oracle,
        ] {
            let sql = sql_for_dialect(&MetadataQuery::ServerConfiguration, dialect).unwrap();
            assert!(sql.contains("0 AS is_dynamic"), "{dialect:?}: {sql}");
        }
    }

    /// The Status page reads each engine's real counter catalog — bounded on
    /// SQL Server to curated classes and self-describing counter types — and
    /// answers with an empty set where the engine has no counters at all.
    #[test]
    fn server_status_sql_reads_each_engines_counters() {
        let tds =
            sql_for_dialect(&MetadataQuery::ServerStatus, MetadataDialect::SqlServer).unwrap();
        assert!(tds.contains("sys.dm_os_performance_counters"), "{tds}");
        assert!(tds.contains("cntr_type IN (65792, 272696576)"), "{tds}");
        assert!(tds.contains(":Buffer Manager"), "{tds}");
        let pg = sql_for_dialect(&MetadataQuery::ServerStatus, MetadataDialect::Postgres).unwrap();
        assert!(pg.contains("pg_stat_database"), "{pg}");
        assert!(pg.contains("pg_stat_bgwriter"), "{pg}");
        let my = sql_for_engine(&MetadataQuery::ServerStatus, DatabaseType::MySql).unwrap();
        assert!(my.contains("performance_schema.global_status"), "{my}");
        let maria = sql_for_engine(&MetadataQuery::ServerStatus, DatabaseType::MariaDb).unwrap();
        assert!(
            maria.contains("information_schema.GLOBAL_STATUS"),
            "{maria}"
        );
        let ch =
            sql_for_dialect(&MetadataQuery::ServerStatus, MetadataDialect::ClickHouse).unwrap();
        assert!(ch.contains("system.metrics"), "{ch}");
        assert!(ch.contains("system.events"), "{ch}");
        let ora = sql_for_dialect(&MetadataQuery::ServerStatus, MetadataDialect::Oracle).unwrap();
        assert!(ora.contains("v$sysstat"), "{ora}");
        for (dialect, db) in [
            (MetadataDialect::Sqlite, DatabaseType::Sqlite),
            (MetadataDialect::DuckDb, DatabaseType::DuckDb),
            (MetadataDialect::Postgres, DatabaseType::CockroachDb),
        ] {
            let sql = sql_for_engine(&MetadataQuery::ServerStatus, db).unwrap();
            assert!(
                sql.contains("WHERE 1 = 0"),
                "{dialect:?}/{db:?} should report no counters: {sql}"
            );
        }
    }

    #[test]
    fn mysql_events_query_reads_information_schema_events() {
        let sql = sql_for_dialect(
            &MetadataQuery::Events {
                schema: Some("app".into()),
            },
            MetadataDialect::MySql,
        )
        .unwrap();
        assert!(sql.contains("information_schema.events"));
        assert!(sql.contains("event_definition"));
        assert!(sql.contains("interval_value"));
    }

    #[test]
    fn sqlite_trigger_definition_reads_sqlite_master() {
        let sql = sql_for_dialect(
            &MetadataQuery::TriggerDefinition {
                schema: "main".into(),
                name: "trg_orders".into(),
            },
            MetadataDialect::Sqlite,
        )
        .unwrap();
        assert!(sql.contains("sqlite_master"));
        assert!(sql.contains("type = 'trigger'"));
        assert!(sql.contains("'trg_orders'"));
    }

    #[test]
    fn literals_are_escaped_in_new_queries() {
        let pg = sql_for_dialect(
            &MetadataQuery::TriggerDefinition {
                schema: "we'ird".into(),
                name: "tr".into(),
            },
            MetadataDialect::Postgres,
        )
        .unwrap();
        assert!(pg.contains("we''ird"), "{pg}");

        let my = sql_for_dialect(
            &MetadataQuery::TriggerDefinition {
                schema: "we'ird".into(),
                name: "tr".into(),
            },
            MetadataDialect::MySql,
        )
        .unwrap();
        assert!(my.contains("we''ird"), "{my}");

        let sq = sql_for_dialect(
            &MetadataQuery::TriggerDefinition {
                schema: "main".into(),
                name: "we'ird".into(),
            },
            MetadataDialect::Sqlite,
        )
        .unwrap();
        assert!(sq.contains("we''ird"), "{sq}");
    }

    #[test]
    fn sql_for_engine_matches_sql_for_dialect_for_postgres_family() {
        let variants = [
            MetadataQuery::Schemas,
            MetadataQuery::Tables { schema: None },
            MetadataQuery::ObjectDefinition {
                schema: "s".into(),
                object: "o".into(),
            },
        ];
        for db in [DatabaseType::Postgres, DatabaseType::YugabyteDb] {
            for variant in &variants {
                if matches!(variant, MetadataQuery::ObjectDefinition { .. }) {
                    continue;
                }
                assert_eq!(
                    sql_for_engine(variant, db).unwrap(),
                    sql_for_dialect(variant, MetadataDialect::Postgres).unwrap(),
                    "{db:?} {variant:?}"
                );
            }
        }
        let def = MetadataQuery::ObjectDefinition {
            schema: "s".into(),
            object: "o".into(),
        };
        assert_eq!(
            sql_for_engine(&def, DatabaseType::Postgres).unwrap(),
            sql_for_dialect(&def, MetadataDialect::Postgres).unwrap()
        );
    }

    #[test]
    fn mysql_inventory_hides_the_literal_view_comment() {
        // MySQL/MariaDB report every view's table_comment as 'VIEW'; the
        // inventory must not surface that as the view's description.
        let sql = sql_for_dialect(
            &MetadataQuery::ObjectInventory { schema: None },
            MetadataDialect::MySql,
        )
        .unwrap();
        assert!(sql.contains("IF(t.table_type = 'VIEW', NULL, t.table_comment) AS comment"));
    }
}
