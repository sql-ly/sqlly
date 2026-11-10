//! Load a [`TableSnapshot`] from live catalog metadata.

use std::collections::BTreeMap;

use crate::alter::script::comments::string_literal;
use crate::alter::{
    CheckConstraintDef, ColumnDef, ColumnExtras, DependentView, ForeignKeyDef, GeneratedColumn,
    IndexDef, TableSnapshot, TableStorage,
};
use sqlly_core::{CellValue, ConnectionProfile, DisplayMessage, StudioResult};

fn snapshot_message(key: &'static str, fallback: impl Into<String>) -> DisplayMessage {
    DisplayMessage::new(key, fallback)
}
use sqlly_intel::metadata::MetadataQuery;
use sqlly_intel::metadata::ddl::{
    render_sql_type, table_foreign_keys_ddl_sql, table_primary_key_sql,
};
use sqlly_intel::metadata::dialect::{MetadataDialect, sql_for_engine};
use sqlly_intel::metadata::dialect_ddl::{self, GenericDdlColumn};

use crate::metadata::{cell_to_bool, cell_to_i64, cell_to_string, profile_with_db};
use crate::query::{QueryEvent, QueryExecutor};

fn dialect_for(profile: &ConnectionProfile) -> MetadataDialect {
    MetadataDialect::for_database_type(profile.database_type).unwrap_or(MetadataDialect::SqlServer)
}

async fn collect_rows(
    queries: &dyn QueryExecutor,
    profile: ConnectionProfile,
    sql: &str,
) -> StudioResult<Vec<Vec<CellValue>>> {
    // Full values: snapshot rows feed scripts, not a grid, so no preview cut.
    let handle = queries
        .execute_full_values(&profile, None, sql, 1000)
        .await?;
    let mut events = handle.events;
    let mut all_rows = Vec::new();
    while let Some(event) = events.recv().await {
        match event {
            QueryEvent::Rows { rows, .. } => all_rows.extend(rows),
            QueryEvent::Error { message } => {
                return Err(sqlly_core::StudioError::Connection(message));
            }
            QueryEvent::Completed { .. } | QueryEvent::Cancelled => break,
            _ => {}
        }
    }
    Ok(all_rows)
}

async fn run_metadata(
    queries: &dyn QueryExecutor,
    profile: &ConnectionProfile,
    database: &str,
    query: &MetadataQuery,
) -> StudioResult<Vec<Vec<CellValue>>> {
    let Ok(sql) = sql_for_engine(query, profile.database_type) else {
        return Ok(Vec::new());
    };
    collect_rows(queries, profile_with_db(profile, database), &sql).await
}

fn schema_for_queries(schema: Option<&str>, dialect: MetadataDialect) -> String {
    match dialect {
        MetadataDialect::Sqlite | MetadataDialect::DuckDb => schema
            .filter(|s| !s.is_empty())
            .unwrap_or("main")
            .to_string(),
        _ => schema
            .filter(|s| !s.is_empty())
            .unwrap_or("dbo")
            .to_string(),
    }
}

fn parse_fk_actions(ddl: &str) -> (Option<String>, Option<String>) {
    let upper = ddl.to_uppercase();
    (
        extract_fk_action(&upper, "ON DELETE"),
        extract_fk_action(&upper, "ON UPDATE"),
    )
}

fn extract_fk_action(text: &str, label: &str) -> Option<String> {
    let start = text.find(label)?;
    let rest = text[start + label.len()..].trim_start();
    let action = rest
        .split_whitespace()
        .take(2)
        .collect::<Vec<_>>()
        .join(" ");
    if action.is_empty() {
        None
    } else {
        Some(action)
    }
}

/// The column-level subset of a [`TableSnapshot`]: columns with their
/// defaults, plus the primary key. This is everything a typed model or
/// CREATE script needs and skips the index/foreign-key/trigger/dependent-view
/// queries, so it stays cheap when run for every table in a schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableShape {
    pub schema: Option<String>,
    pub table: String,
    pub columns: Vec<ColumnDef>,
    pub primary_key: Vec<String>,
    pub pk_constraint_name: Option<String>,
}

/// Fetch just the columns (with defaults) and primary key of one table.
pub async fn load_table_shape(
    queries: &dyn QueryExecutor,
    profile: &ConnectionProfile,
    database: &str,
    schema: Option<&str>,
    table: &str,
) -> StudioResult<TableShape> {
    Ok(
        load_shape_with_extras(queries, profile, database, schema, table)
            .await?
            .shape,
    )
}

/// [`TableShape`] plus the CREATE-only column attributes the same catalog
/// read returned, and any lower-fidelity fallback it had to take.
struct ShapeWithExtras {
    shape: TableShape,
    extras: BTreeMap<String, ColumnExtras>,
    warnings: Vec<DisplayMessage>,
}

async fn load_shape_with_extras(
    queries: &dyn QueryExecutor,
    profile: &ConnectionProfile,
    database: &str,
    schema: Option<&str>,
    table: &str,
) -> StudioResult<ShapeWithExtras> {
    let dialect = dialect_for(profile);
    let schema_q = schema_for_queries(schema, dialect);
    let snapshot_schema = if dialect == MetadataDialect::Sqlite {
        None
    } else {
        Some(schema_q.clone())
    };
    let loaded = load_columns(queries, profile, database, dialect, &schema_q, table).await?;
    let LoadedColumns {
        mut columns,
        mut extras,
        defaults_included,
        warning,
    } = loaded;
    if !defaults_included {
        let defaults = load_defaults(queries, profile, database, &schema_q, table).await?;
        columns = merge_defaults(columns, defaults);
    }
    if dialect == MetadataDialect::Oracle {
        oracle_virtual_columns(&mut columns, &mut extras);
    }
    let (primary_key, pk_constraint_name) =
        load_primary_key(queries, profile, database, dialect, &schema_q, table).await?;
    Ok(ShapeWithExtras {
        shape: TableShape {
            schema: snapshot_schema,
            table: table.to_string(),
            columns,
            primary_key,
            pk_constraint_name,
        },
        extras,
        warnings: warning.into_iter().collect(),
    })
}

/// Fetch the structure snapshot for one table from catalog metadata.
pub async fn load_table_snapshot(
    queries: &dyn QueryExecutor,
    profile: &ConnectionProfile,
    database: &str,
    schema: Option<&str>,
    table: &str,
) -> StudioResult<TableSnapshot> {
    let dialect = dialect_for(profile);
    let schema_q = schema_for_queries(schema, dialect);

    let ShapeWithExtras {
        shape:
            TableShape {
                schema: snapshot_schema,
                columns: columns_merged,
                primary_key,
                pk_constraint_name,
                ..
            },
        extras: column_extras,
        warnings: mut load_warnings,
    } = load_shape_with_extras(queries, profile, database, schema, table).await?;
    let (indexes, opaque_index_ddl) =
        load_indexes(queries, profile, database, &schema_q, table).await?;
    let foreign_keys =
        load_foreign_keys(queries, profile, database, dialect, &schema_q, table).await?;
    let referenced_by = load_referenced_by(queries, profile, database, &schema_q, table).await?;
    let stored_index_ddl =
        load_stored_index_ddl(queries, profile, database, dialect, &schema_q, table).await?;
    let stored_trigger_ddl =
        load_stored_trigger_ddl(queries, profile, database, dialect, &schema_q, table).await?;
    let sqlite_foreign_keys_on =
        load_sqlite_foreign_keys_on(queries, profile, database, dialect).await?;

    let dependent_views =
        load_dependent_views(queries, profile, database, dialect, &schema_q, table).await?;
    let (table_comment, column_comments, comment_read_error) =
        match load_comments(queries, profile, database, dialect, &schema_q, table).await {
            Ok((table_comment, column_comments)) => (table_comment, column_comments, None),
            Err(err) => {
                load_warnings.push(
                    snapshot_message(
                        "services.table_snapshot.warnings.descriptions_unreadable",
                        format!("Descriptions could not be read: {err}"),
                    )
                    .with_arg("detail", err.to_string()),
                );
                (None, BTreeMap::new(), Some(err.to_string()))
            }
        };
    let stored_table_ddl =
        match load_stored_table_ddl(queries, profile, database, dialect, &schema_q, table).await {
            Ok(ddl) => ddl,
            Err(err) => {
                load_warnings.push(
                    snapshot_message(
                        "services.table_snapshot.warnings.show_create_failed",
                        format!("SHOW CREATE TABLE failed: {err}"),
                    )
                    .with_arg("detail", err.to_string()),
                );
                None
            }
        };
    let check_constraints =
        match load_check_constraints(queries, profile, database, dialect, &schema_q, table).await {
            Ok(checks) => checks,
            Err(err) => {
                load_warnings.push(
                    snapshot_message(
                        "services.table_snapshot.warnings.check_constraints_unreadable",
                        format!("CHECK constraints could not be read: {err}"),
                    )
                    .with_arg("detail", err.to_string()),
                );
                Vec::new()
            }
        };

    let storage =
        match load_table_storage(queries, profile, database, dialect, &schema_q, table).await {
            Ok(storage) => storage,
            Err(err) => {
                load_warnings.push(
                snapshot_message(
                    "services.table_snapshot.warnings.storage_unreadable",
                    format!(
                        "Table storage options (filegroup, compression) could not be read: {err}"
                    ),
                )
                .with_arg("detail", err.to_string()),
            );
                TableStorage::default()
            }
        };

    Ok(TableSnapshot {
        schema: snapshot_schema,
        table: table.to_string(),
        columns: columns_merged,
        primary_key,
        pk_constraint_name,
        indexes,
        foreign_keys,
        stored_index_ddl,
        stored_trigger_ddl,
        opaque_index_ddl,
        sqlite_foreign_keys_on,
        referenced_by,
        dependent_views,
        table_comment,
        column_comments,
        stored_table_ddl,
        column_extras,
        check_constraints,
        comment_read_error,
        load_warnings,
        storage,
    })
}

/// `SHOW CREATE TABLE` on MySQL/MariaDB — the engine's own definition text,
/// which the description-edit DDL restates verbatim (only the COMMENT clause
/// changes) and the CREATE script replays. `Ok(None)` on other engines; an
/// error is surfaced as a load warning, and the comment renderer then
/// refuses the edit instead of rebuilding a lossy definition.
async fn load_stored_table_ddl(
    queries: &dyn QueryExecutor,
    profile: &ConnectionProfile,
    database: &str,
    dialect: MetadataDialect,
    schema: &str,
    table: &str,
) -> StudioResult<Option<String>> {
    if dialect != MetadataDialect::MySql {
        return Ok(None);
    }
    let quote = |part: &str| format!("`{}`", part.replace('`', "``"));
    let qualified = if schema.is_empty() {
        quote(table)
    } else {
        format!("{}.{}", quote(schema), quote(table))
    };
    let rows = collect_rows(
        queries,
        profile_with_db(profile, database),
        &format!("SHOW CREATE TABLE {qualified}"),
    )
    .await?;
    let Some(row) = rows.into_iter().next() else {
        return Ok(None);
    };
    let mut cells = row.into_iter();
    let _name = cell_to_string(cells.next());
    Ok(cell_to_string(cells.next()).filter(|text| !text.trim().is_empty()))
}

/// What [`load_columns`] read: the editable columns, the CREATE-only
/// attributes, whether defaults came with them (else [`load_defaults`] runs),
/// and a warning when the engine-specific read failed and the generic
/// catalog shape was used instead.
struct LoadedColumns {
    columns: Vec<ColumnDef>,
    extras: BTreeMap<String, ColumnExtras>,
    defaults_included: bool,
    warning: Option<DisplayMessage>,
}

impl LoadedColumns {
    fn plain(columns: Vec<ColumnDef>) -> Self {
        Self {
            columns,
            extras: BTreeMap::new(),
            defaults_included: false,
            warning: None,
        }
    }
}

/// `N'…'` object-name argument for `OBJECT_ID`, built from QUOTENAME'd parts
/// so a name containing `.`, `]`, or `'` still resolves to the right object.
fn tds_object_id(schema: &str, table: &str) -> String {
    let lit = |text: &str| format!("N'{}'", text.replace('\'', "''"));
    format!(
        "OBJECT_ID(QUOTENAME({}) + N'.' + QUOTENAME({}))",
        lit(schema),
        lit(table)
    )
}

/// SQL Server columns with the identity seed/increment, computed definition,
/// and collation (next to the database default, so only a differing one is
/// restated) that the CREATE script needs.
fn tds_columns_sql(schema: &str, table: &str) -> String {
    let object = tds_object_id(schema, table);
    format!(
        "SELECT c.name, t.name AS type_name, c.max_length, c.precision, c.scale, c.is_nullable, c.is_identity, \
         CONVERT(nvarchar(64), ic.seed_value) AS identity_seed, CONVERT(nvarchar(64), ic.increment_value) AS identity_increment, \
         c.is_computed, cc.definition AS computed_definition, CAST(COALESCE(cc.is_persisted, 0) AS int) AS is_persisted, \
         c.collation_name, CONVERT(nvarchar(128), DATABASEPROPERTYEX(DB_NAME(), 'Collation')) AS database_collation \
         FROM sys.columns c JOIN sys.types t ON c.user_type_id = t.user_type_id \
         LEFT JOIN sys.identity_columns ic ON ic.object_id = c.object_id AND ic.column_id = c.column_id \
         LEFT JOIN sys.computed_columns cc ON cc.object_id = c.object_id AND cc.column_id = c.column_id \
         WHERE c.object_id = {object} ORDER BY c.column_id"
    )
}

/// A column's collation when the CREATE script must restate it: set, not the
/// database default, and not on a computed column (which inherits its
/// expression's collation and cannot declare one).
fn tds_explicit_collation(
    collation: Option<String>,
    database_collation: Option<&str>,
    is_computed: bool,
) -> Option<String> {
    let collation = collation.filter(|c| !c.trim().is_empty())?;
    if is_computed || database_collation.is_some_and(|db| db.eq_ignore_ascii_case(collation.trim()))
    {
        return None;
    }
    Some(collation.trim().to_string())
}

/// SQL Server table storage: the heap / clustered index's data space and its
/// compression range, the large-value data space, and the table features
/// the CREATE script does not recreate.
fn tds_storage_sql(schema: &str, table: &str) -> String {
    let object = tds_object_id(schema, table);
    format!(
        "SELECT ds.name AS data_space, ds.type AS data_space_type, \
         CAST(COALESCE(fg.is_default, 0) AS int) AS on_default, \
         (SELECT MIN(p.data_compression_desc) FROM sys.partitions p WHERE p.object_id = t.object_id AND p.index_id = i.index_id) AS min_compression, \
         (SELECT MAX(p.data_compression_desc) FROM sys.partitions p WHERE p.object_id = t.object_id AND p.index_id = i.index_id) AS max_compression, \
         lds.name AS lob_data_space, CAST(COALESCE(lfg.is_default, 1) AS int) AS lob_on_default, \
         CAST(t.temporal_type AS int) AS temporal_type, \
         OBJECT_SCHEMA_NAME(t.history_table_id) AS history_schema, OBJECT_NAME(t.history_table_id) AS history_table, \
         CAST(t.is_memory_optimized AS int) AS is_memory_optimized, CAST(i.type AS int) AS base_index_type \
         FROM sys.tables t \
         JOIN sys.indexes i ON i.object_id = t.object_id AND i.index_id IN (0, 1) \
         JOIN sys.data_spaces ds ON ds.data_space_id = i.data_space_id \
         LEFT JOIN sys.filegroups fg ON fg.data_space_id = ds.data_space_id \
         LEFT JOIN sys.data_spaces lds ON lds.data_space_id = NULLIF(t.lob_data_space_id, 0) \
         LEFT JOIN sys.filegroups lfg ON lfg.data_space_id = lds.data_space_id \
         WHERE t.object_id = {object}"
    )
}

/// Turn the [`tds_storage_sql`] row into restatable options plus warnings
/// for what the script leaves out.
fn tds_storage_from_row(row: Vec<CellValue>, schema: &str, table: &str) -> TableStorage {
    let mut cells = row.into_iter();
    let data_space = cell_to_string(cells.next());
    let data_space_type = cell_to_string(cells.next()).unwrap_or_default();
    let on_default = cell_to_bool(cells.next()).unwrap_or(true);
    let min_compression = cell_to_string(cells.next()).unwrap_or_default();
    let max_compression = cell_to_string(cells.next()).unwrap_or_default();
    let lob_data_space = cell_to_string(cells.next());
    let lob_on_default = cell_to_bool(cells.next()).unwrap_or(true);
    let temporal_type = cell_to_i64(cells.next()).unwrap_or(0);
    let history_schema = cell_to_string(cells.next());
    let history_table = cell_to_string(cells.next());
    let memory_optimized = cell_to_bool(cells.next()).unwrap_or(false);
    let base_index_type = cell_to_i64(cells.next()).unwrap_or(0);

    let mut storage = TableStorage::default();
    match data_space_type.trim() {
        "FG" if !on_default => storage.filegroup = data_space,
        "PS" => storage.unscripted.push(
            snapshot_message(
                "services.table_snapshot.storage.partitioned",
                format!(
                    "it is partitioned on scheme {}; the partition scheme and function are not scripted, so the copy is created on the default filegroup",
                    data_space.as_deref().unwrap_or("—")
                ),
            )
            .with_arg("partition_scheme", data_space.as_deref().unwrap_or("—")),
        ),
        _ => {}
    }
    if !lob_on_default {
        storage.lob_filegroup = lob_data_space;
    }
    let compression = |text: &str| text.trim().to_ascii_uppercase();
    let (low, high) = (compression(&min_compression), compression(&max_compression));
    if low != high {
        storage.unscripted.push(
            snapshot_message(
                "services.table_snapshot.storage.mixed_compression",
                format!("its partitions use different compression ({low} to {high}); no DATA_COMPRESSION is scripted"),
            )
            .with_arg("minimum_compression", low.clone())
            .with_arg("maximum_compression", high.clone()),
        );
    } else if matches!(low.as_str(), "ROW" | "PAGE") {
        storage.data_compression = Some(low);
    } else if !low.is_empty() && low != "NONE" && base_index_type != 5 {
        storage.unscripted.push(
            snapshot_message(
                "services.table_snapshot.storage.unsupported_compression",
                format!("its data uses {low} compression, which the script does not restate"),
            )
            .with_arg("compression", low.clone()),
        );
    }
    if base_index_type == 5 {
        storage.unscripted.push(snapshot_message(
            "services.table_snapshot.storage.clustered_columnstore",
            "it is stored as a clustered columnstore index, which is not scripted",
        ));
    }
    match temporal_type {
        2 => {
            if let (Some(history_schema), Some(history_table)) = (history_schema, history_table) {
                storage.unscripted.push(
                    snapshot_message(
                        "services.table_snapshot.storage.temporal_with_history",
                        format!("it is a system-versioned temporal table (history table {history_schema}.{history_table}); PERIOD FOR SYSTEM_TIME and SYSTEM_VERSIONING are not scripted, so the copy is an ordinary table"),
                    )
                    .with_arg("history_schema", history_schema)
                    .with_arg("history_table", history_table),
                );
            } else {
                storage.unscripted.push(snapshot_message(
                    "services.table_snapshot.storage.temporal_without_history",
                    "it is a system-versioned temporal table; PERIOD FOR SYSTEM_TIME and SYSTEM_VERSIONING are not scripted, so the copy is an ordinary table",
                ));
            }
        }
        1 => storage.unscripted.push(
            snapshot_message(
                "services.table_snapshot.storage.history_table",
                format!("it is the history table of a system-versioned table; the copy of {schema}.{table} is an ordinary table"),
            )
            .with_arg("schema_name", schema)
            .with_arg("table_name", table),
        ),
        _ => {}
    }
    if memory_optimized {
        storage.unscripted.push(snapshot_message(
            "services.table_snapshot.storage.memory_optimized",
            "it is memory-optimized; MEMORY_OPTIMIZED and DURABILITY are not scripted, so the copy is disk-based",
        ));
    }
    storage
}

/// Read the SQL Server storage options (empty elsewhere).
async fn load_table_storage(
    queries: &dyn QueryExecutor,
    profile: &ConnectionProfile,
    database: &str,
    dialect: MetadataDialect,
    schema: &str,
    table: &str,
) -> StudioResult<TableStorage> {
    if dialect != MetadataDialect::SqlServer {
        return Ok(TableStorage::default());
    }
    let rows = collect_rows(
        queries,
        profile_with_db(profile, database),
        &tds_storage_sql(schema, table),
    )
    .await?;
    let mut storage = rows
        .into_iter()
        .next()
        .map(|row| tds_storage_from_row(row, schema, table))
        .unwrap_or_default();
    let index_rows = collect_rows(
        queries,
        profile_with_db(profile, database),
        &tds_index_compression_sql(schema, table),
    )
    .await?;
    tds_index_compression_from_rows(index_rows, &mut storage);
    Ok(storage)
}

/// Per-index compression range (partitions can differ) for every named,
/// non-primary-key index of the table.
fn tds_index_compression_sql(schema: &str, table: &str) -> String {
    let object = tds_object_id(schema, table);
    format!(
        "SELECT i.name, MIN(p.data_compression_desc) AS min_compression, MAX(p.data_compression_desc) AS max_compression \
         FROM sys.indexes i JOIN sys.partitions p ON p.object_id = i.object_id AND p.index_id = i.index_id \
         WHERE i.object_id = {object} AND i.index_id > 0 AND i.is_primary_key = 0 AND i.name IS NOT NULL \
         GROUP BY i.name"
    )
}

/// Fold [`tds_index_compression_sql`] rows into `storage`: a uniform `ROW` /
/// `PAGE` index is restated; anything else that is not `NONE` is warned
/// about instead of guessed at.
fn tds_index_compression_from_rows(rows: Vec<Vec<CellValue>>, storage: &mut TableStorage) {
    for row in rows {
        let mut cells = row.into_iter();
        let Some(name) = cell_to_string(cells.next()) else {
            continue;
        };
        let low = cell_to_string(cells.next())
            .unwrap_or_default()
            .trim()
            .to_ascii_uppercase();
        let high = cell_to_string(cells.next())
            .unwrap_or_default()
            .trim()
            .to_ascii_uppercase();
        if low != high {
            storage.unscripted.push(
                snapshot_message(
                    "services.table_snapshot.storage.index_mixed_compression",
                    format!("index {name}'s partitions use different compression ({low} to {high}); it is scripted without DATA_COMPRESSION"),
                )
                .with_arg("index_name", name.clone())
                .with_arg("minimum_compression", low.clone())
                .with_arg("maximum_compression", high.clone()),
            );
        } else if matches!(low.as_str(), "ROW" | "PAGE") {
            storage.index_compression.insert(name, low);
        } else if low.starts_with("COLUMNSTORE") {
            storage.unscripted.push(
                snapshot_message(
                    "services.table_snapshot.storage.index_columnstore",
                    format!(
                        "index {name} is a columnstore index; it is scripted as a rowstore index"
                    ),
                )
                .with_arg("index_name", name.clone()),
            );
        } else if !low.is_empty() && low != "NONE" {
            storage.unscripted.push(
                snapshot_message(
                    "services.table_snapshot.storage.index_unsupported_compression",
                    format!(
                        "index {name} uses {low} compression, which the script does not restate"
                    ),
                )
                .with_arg("index_name", name.clone())
                .with_arg("compression", low.clone()),
            );
        }
    }
}

/// Oracle columns straight from `all_tab_cols`, keeping what the shared
/// `Columns` shape folds away: NULL vs 0 precision/scale (so `INTEGER`, stored
/// as `NUMBER(*,0)`, is told apart from an unconstrained `NUMBER`) and the
/// `CHAR`/`BYTE` length semantics of character columns.
fn oracle_columns_sql(schema: &str, table: &str) -> String {
    let oracle = sqlly_core::DatabaseType::Oracle;
    format!(
        "SELECT c.column_name, c.data_type, c.data_length, c.char_length, c.char_used, c.data_precision, c.data_scale, CASE c.nullable WHEN 'Y' THEN 1 ELSE 0 END AS is_nullable, CASE c.identity_column WHEN 'YES' THEN 1 ELSE 0 END AS is_identity, CASE c.virtual_column WHEN 'YES' THEN 1 ELSE 0 END AS is_computed FROM all_tab_cols c WHERE c.owner = {} AND c.table_name = {} AND c.hidden_column = 'NO' ORDER BY c.column_id",
        string_literal(oracle, schema),
        string_literal(oracle, table)
    )
}

/// One `all_tab_cols` row's facts, as [`oracle_columns_sql`] reads them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct OracleColumnFacts {
    data_type: String,
    data_length: Option<i64>,
    char_length: Option<i64>,
    /// `C` (character semantics), `B` (byte), or NULL for non-character types.
    char_used: Option<String>,
    precision: Option<i64>,
    scale: Option<i64>,
}

/// Oracle's DDL spelling of one column type from [`OracleColumnFacts`].
fn oracle_type_from_facts(facts: &OracleColumnFacts) -> String {
    let base = facts.data_type.trim();
    let upper = base.to_ascii_uppercase();
    match upper.as_str() {
        "NUMBER" => match (facts.precision, facts.scale) {
            (Some(p), Some(s)) if s != 0 => format!("{base}({p},{s})"),
            (Some(p), _) => format!("{base}({p})"),
            // INTEGER / INT / SMALLINT are stored as NUMBER(*,0).
            (None, Some(0)) => "INTEGER".to_string(),
            (None, Some(s)) => format!("{base}(*,{s})"),
            (None, None) => base.to_string(),
        },
        "FLOAT" => match facts.precision {
            Some(p) if p > 0 && p != 126 => format!("{base}({p})"),
            _ => base.to_string(),
        },
        "VARCHAR2" | "CHAR" => {
            let byte_semantics = facts
                .char_used
                .as_deref()
                .is_some_and(|u| u.trim().eq_ignore_ascii_case("B"));
            let (length, unit) = if byte_semantics {
                (facts.data_length, "BYTE")
            } else {
                (facts.char_length.or(facts.data_length), "CHAR")
            };
            match length {
                Some(n) if n > 0 => format!("{base}({n} {unit})"),
                _ => base.to_string(),
            }
        }
        // National character types always count characters.
        "NVARCHAR2" | "NCHAR" => match facts.char_length.filter(|n| *n > 0) {
            Some(n) => format!("{base}({n})"),
            None => base.to_string(),
        },
        "RAW" => match facts.data_length.filter(|n| *n > 0) {
            Some(n) => format!("{base}({n})"),
            None => base.to_string(),
        },
        // TIMESTAMP(6), INTERVAL DAY(2) TO SECOND(6), DATE, CLOB, BLOB,
        // BINARY_FLOAT, … already carry their arguments in `data_type`.
        _ => base.to_string(),
    }
}

/// Oracle columns with exact type spelling; virtual columns are marked for
/// [`oracle_virtual_columns`] (their expression arrives with the defaults).
fn oracle_columns_from_rows(rows: Vec<Vec<CellValue>>) -> LoadedColumns {
    let mut extras = BTreeMap::new();
    let columns = rows
        .into_iter()
        .filter_map(|row| {
            let mut cells = row.into_iter();
            let name = cell_to_string(cells.next())?;
            let facts = OracleColumnFacts {
                data_type: cell_to_string(cells.next()).unwrap_or_default(),
                data_length: cell_to_i64(cells.next()),
                char_length: cell_to_i64(cells.next()),
                char_used: cell_to_string(cells.next()),
                precision: cell_to_i64(cells.next()),
                scale: cell_to_i64(cells.next()),
            };
            let nullable = cell_to_bool(cells.next()).unwrap_or(true);
            let identity = cell_to_bool(cells.next()).unwrap_or(false);
            let is_computed = cell_to_bool(cells.next()).unwrap_or(false);
            if is_computed {
                extras.insert(
                    name.clone(),
                    ColumnExtras {
                        generated: Some(GeneratedColumn {
                            expression: String::new(),
                            stored: false,
                        }),
                        ..ColumnExtras::default()
                    },
                );
            }
            Some(ColumnDef {
                data_type: oracle_type_from_facts(&facts),
                name,
                nullable,
                identity,
                default: None,
                default_constraint_name: None,
            })
        })
        .collect();
    LoadedColumns {
        columns,
        extras,
        defaults_included: false,
        warning: None,
    }
}

/// PostgreSQL columns from `pg_attribute`: `format_type` spells arrays,
/// domains, enums, and typmods the way DDL needs them (information_schema
/// reports `ARRAY` / `USER-DEFINED`), and identity/generated kinds come
/// with their expressions.
fn pg_columns_sql(schema: &str, table: &str) -> String {
    let regclass = pg_regclass_literal(schema, table);
    format!(
        "SELECT a.attname, pg_catalog.format_type(a.atttypid, a.atttypmod) AS type_name, (NOT a.attnotnull)::int AS is_nullable, a.attidentity::text AS identity_kind, a.attgenerated::text AS generated_kind, pg_catalog.pg_get_expr(d.adbin, d.adrelid) AS default_expr, COALESCE(pg_catalog.pg_get_expr(d.adbin, d.adrelid) = 'nextval(''' || pg_catalog.pg_get_serial_sequence(a.attrelid::regclass::text, a.attname)::regclass::text || '''::regclass)', false)::int AS is_serial FROM pg_catalog.pg_attribute a LEFT JOIN pg_catalog.pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum WHERE a.attrelid = {regclass}::regclass AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum"
    )
}

/// MySQL/MariaDB columns with the full `column_type` (enum/set values,
/// unsigned, fractional-second precision) and the raw default + `extra`
/// needed to spell the default as SQL.
fn mysql_columns_sql(db: sqlly_core::DatabaseType, schema: &str, table: &str) -> String {
    format!(
        "SELECT c.column_name, c.column_type, c.data_type, IF(c.is_nullable = 'YES', 1, 0) AS is_nullable, c.extra, c.column_default, c.generation_expression FROM information_schema.columns c WHERE c.table_schema = {} AND c.table_name = {} ORDER BY c.ordinal_position",
        string_literal(db, schema),
        string_literal(db, table)
    )
}

/// MySQL types whose defaults are bare numbers (everything else is quoted).
fn mysql_numeric_type(data_type: &str) -> bool {
    matches!(
        data_type.to_ascii_lowercase().as_str(),
        "tinyint"
            | "smallint"
            | "mediumint"
            | "int"
            | "integer"
            | "bigint"
            | "decimal"
            | "numeric"
            | "float"
            | "double"
            | "real"
            | "bit"
            | "year"
            | "bool"
            | "boolean"
    )
}

/// Spell a `information_schema.columns.column_default` value as the SQL a
/// `DEFAULT` clause needs. MariaDB (10.2.7+) already reports SQL — quoted
/// literals, `current_timestamp()`, and the text `NULL` for "no default".
/// MySQL reports literals UNQUOTED (`new`, not `'new'`), marks expression
/// defaults `DEFAULT_GENERATED` in `extra`, and escapes quotes inside those
/// expressions (`_utf8mb4\'x\'`).
fn mysql_default_sql(
    db: sqlly_core::DatabaseType,
    raw: Option<String>,
    data_type: &str,
    extra: &str,
) -> Option<String> {
    let raw = raw?;
    if db == sqlly_core::DatabaseType::MariaDb {
        return (!raw.eq_ignore_ascii_case("NULL")).then_some(raw);
    }
    let upper = raw.trim().to_ascii_uppercase();
    let temporal_keyword = upper.starts_with("CURRENT_TIMESTAMP")
        || upper.starts_with("NOW(")
        || upper.starts_with("LOCALTIME")
        || upper.starts_with("CURRENT_DATE")
        || upper.starts_with("CURRENT_TIME");
    if extra.to_ascii_uppercase().contains("DEFAULT_GENERATED") {
        let expr = raw.replace("\\'", "'");
        return Some(if temporal_keyword || expr.starts_with('(') {
            expr
        } else {
            format!("({expr})")
        });
    }
    if temporal_keyword
        && matches!(
            data_type.to_ascii_lowercase().as_str(),
            "timestamp" | "datetime" | "date" | "time"
        )
    {
        return Some(raw);
    }
    if mysql_numeric_type(data_type) {
        return Some(raw);
    }
    Some(string_literal(db, &raw))
}

/// The `on update …` expression MySQL/MariaDB report in `extra`.
fn mysql_on_update(extra: &str) -> Option<String> {
    let lower = extra.to_ascii_lowercase();
    let at = lower.find("on update ")?;
    extra[at + "on update ".len()..]
        .split_whitespace()
        .next()
        .map(str::to_string)
}

/// Oracle type spelling with the arguments the generic renderer drops:
/// `NUMBER(p[,s])`, `FLOAT(p)`, `RAW(n)`, and no bogus length on
/// `BINARY_FLOAT`/`BINARY_DOUBLE` (their names contain "binary").
fn oracle_column_type(generic: &GenericDdlColumn) -> String {
    let base = generic.type_name.trim();
    let upper = base.to_ascii_uppercase();
    let precision = generic.precision.unwrap_or(0);
    let scale = generic.scale.unwrap_or(0);
    let length = generic.max_length.unwrap_or(0);
    match upper.as_str() {
        "NUMBER" if precision > 0 && scale != 0 => format!("{base}({precision},{scale})"),
        "NUMBER" if precision > 0 => format!("{base}({precision})"),
        "NUMBER" if scale != 0 => format!("{base}(*,{scale})"),
        "FLOAT" if precision > 0 => format!("{base}({precision})"),
        "RAW" if length > 0 => format!("{base}({length})"),
        "BINARY_FLOAT" | "BINARY_DOUBLE" => base.to_string(),
        _ => dialect_ddl::render_generic_column_type(generic),
    }
}

/// Oracle keeps a virtual column's expression in `data_default`, which
/// [`merge_defaults`] put in `default`; move it to the generated extras so
/// the CREATE script writes `AS (…)` instead of `DEFAULT …`.
fn oracle_virtual_columns(columns: &mut [ColumnDef], extras: &mut BTreeMap<String, ColumnExtras>) {
    for col in columns.iter_mut() {
        let Some(entry) = extras.get_mut(&col.name) else {
            continue;
        };
        if let Some(generated) = entry.generated.as_mut()
            && generated.expression.is_empty()
            && let Some(expr) = col.default.take()
        {
            generated.expression = expr.trim().to_string();
        }
    }
    extras.retain(|_, e| {
        e.generated
            .as_ref()
            .is_none_or(|g| !g.expression.is_empty())
            || e.identity_always
            || e.identity_seed_increment.is_some()
            || e.on_update.is_some()
    });
}

async fn load_columns(
    queries: &dyn QueryExecutor,
    profile: &ConnectionProfile,
    database: &str,
    dialect: MetadataDialect,
    schema: &str,
    table: &str,
) -> StudioResult<LoadedColumns> {
    let db = profile.database_type;
    match dialect {
        MetadataDialect::SqlServer => {
            let sql = tds_columns_sql(schema, table);
            let rows = collect_rows(queries, profile_with_db(profile, database), &sql).await?;
            let mut extras = BTreeMap::new();
            let columns = rows
                .into_iter()
                .filter_map(|row| {
                    let mut cells = row.into_iter();
                    let name = cell_to_string(cells.next())?;
                    let type_name = cell_to_string(cells.next()).unwrap_or_default();
                    let max_length = cell_to_i64(cells.next());
                    let precision = cell_to_i64(cells.next());
                    let scale = cell_to_i64(cells.next());
                    let nullable = cell_to_bool(cells.next()).unwrap_or(false);
                    let is_identity = cell_to_bool(cells.next()).unwrap_or(false);
                    let seed = cell_to_string(cells.next());
                    let increment = cell_to_string(cells.next());
                    let is_computed = cell_to_bool(cells.next()).unwrap_or(false);
                    let computed_definition = cell_to_string(cells.next());
                    let is_persisted = cell_to_bool(cells.next()).unwrap_or(false);
                    let collation = cell_to_string(cells.next());
                    let database_collation = cell_to_string(cells.next());
                    let mut extra = ColumnExtras {
                        collation: tds_explicit_collation(
                            collation,
                            database_collation.as_deref(),
                            is_computed,
                        ),
                        ..ColumnExtras::default()
                    };
                    if is_identity && let (Some(seed), Some(increment)) = (seed, increment) {
                        extra.identity_seed_increment = Some((seed, increment));
                    }
                    if is_computed && let Some(expression) = computed_definition {
                        extra.generated = Some(GeneratedColumn {
                            expression,
                            stored: is_persisted,
                        });
                    }
                    if extra != ColumnExtras::default() {
                        extras.insert(name.clone(), extra);
                    }
                    Some(ColumnDef {
                        data_type: render_sql_type(&type_name, max_length, precision, scale),
                        name,
                        nullable,
                        identity: is_identity,
                        default: None,
                        default_constraint_name: None,
                    })
                })
                .collect();
            Ok(LoadedColumns {
                columns,
                extras,
                defaults_included: false,
                warning: None,
            })
        }
        MetadataDialect::Sqlite => {
            let rows = run_metadata(
                queries,
                profile,
                database,
                &MetadataQuery::Columns {
                    schema: schema.to_string(),
                    object: table.to_string(),
                },
            )
            .await?;
            Ok(LoadedColumns::plain(
                rows.into_iter()
                    .filter_map(|row| {
                        let mut cells = row.into_iter();
                        let name = cell_to_string(cells.next())?;
                        let type_name = cell_to_string(cells.next()).unwrap_or_default();
                        let nullable = cell_to_bool(cells.next()).unwrap_or(true);
                        Some(ColumnDef {
                            name,
                            data_type: type_name,
                            nullable,
                            identity: false,
                            default: None,
                            default_constraint_name: None,
                        })
                    })
                    .collect(),
            ))
        }
        MetadataDialect::Postgres => {
            let sql = pg_columns_sql(schema, table);
            match collect_rows(queries, profile_with_db(profile, database), &sql).await {
                Ok(rows) => Ok(pg_columns_from_rows(rows)),
                Err(err) => {
                    // Older servers (no attgenerated before PostgreSQL 12) and
                    // wire-compatible engines without these catalog columns
                    // fall back to the information_schema shape.
                    tracing::debug!(%err, schema, table, "pg_attribute column read failed; using information_schema");
                    let mut loaded =
                        load_generic_columns(queries, profile, database, dialect, schema, table)
                            .await?;
                    loaded.warning = Some(
                        snapshot_message(
                            "services.table_snapshot.warnings.postgres_column_fallback",
                            format!("Column types were read from information_schema ({err}); arrays, domains, and generated columns may not be spelled exactly"),
                        )
                        .with_arg("detail", err.to_string()),
                    );
                    Ok(loaded)
                }
            }
        }
        MetadataDialect::MySql => {
            let sql = mysql_columns_sql(db, schema, table);
            let rows = collect_rows(queries, profile_with_db(profile, database), &sql).await?;
            let mut extras = BTreeMap::new();
            let columns = rows
                .into_iter()
                .filter_map(|row| {
                    let mut cells = row.into_iter();
                    let name = cell_to_string(cells.next())?;
                    let column_type = cell_to_string(cells.next()).unwrap_or_default();
                    let data_type = cell_to_string(cells.next()).unwrap_or_default();
                    let nullable = cell_to_bool(cells.next()).unwrap_or(true);
                    let extra_text = cell_to_string(cells.next()).unwrap_or_default();
                    let raw_default = cell_to_string(cells.next());
                    let generation = cell_to_string(cells.next()).filter(|g| !g.is_empty());
                    let mut extra = ColumnExtras {
                        on_update: mysql_on_update(&extra_text),
                        ..ColumnExtras::default()
                    };
                    let default = if let Some(expression) = generation {
                        extra.generated = Some(GeneratedColumn {
                            expression,
                            stored: extra_text.to_ascii_uppercase().contains("STORED")
                                || extra_text.to_ascii_uppercase().contains("PERSISTENT"),
                        });
                        None
                    } else {
                        mysql_default_sql(db, raw_default, &data_type, &extra_text)
                    };
                    if extra != ColumnExtras::default() {
                        extras.insert(name.clone(), extra);
                    }
                    Some(ColumnDef {
                        name,
                        data_type: column_type,
                        nullable,
                        identity: extra_text.to_ascii_lowercase().contains("auto_increment"),
                        default,
                        default_constraint_name: None,
                    })
                })
                .collect();
            Ok(LoadedColumns {
                columns,
                extras,
                defaults_included: true,
                warning: None,
            })
        }
        MetadataDialect::Oracle => {
            let sql = oracle_columns_sql(schema, table);
            match collect_rows(queries, profile_with_db(profile, database), &sql).await {
                Ok(rows) => Ok(oracle_columns_from_rows(rows)),
                Err(err) => {
                    tracing::debug!(%err, schema, table, "all_tab_cols column read failed; using the shared column shape");
                    let mut loaded =
                        load_generic_columns(queries, profile, database, dialect, schema, table)
                            .await?;
                    loaded.warning = Some(
                        snapshot_message(
                            "services.table_snapshot.warnings.oracle_column_fallback",
                            format!("Column types were read through the shared catalog shape ({err}); CHAR/BYTE length semantics and INTEGER columns may not be spelled exactly"),
                        )
                        .with_arg("detail", err.to_string()),
                    );
                    Ok(loaded)
                }
            }
        }
        _ => load_generic_columns(queries, profile, database, dialect, schema, table).await,
    }
}

fn pg_columns_from_rows(rows: Vec<Vec<CellValue>>) -> LoadedColumns {
    let mut extras = BTreeMap::new();
    let columns = rows
        .into_iter()
        .filter_map(|row| {
            let mut cells = row.into_iter();
            let name = cell_to_string(cells.next())?;
            let type_name = cell_to_string(cells.next()).unwrap_or_default();
            let nullable = cell_to_bool(cells.next()).unwrap_or(true);
            let identity_kind = cell_to_string(cells.next()).unwrap_or_default();
            let generated_kind = cell_to_string(cells.next()).unwrap_or_default();
            let default_expr = cell_to_string(cells.next());
            let is_serial = cell_to_bool(cells.next()).unwrap_or(false);
            let mut extra = ColumnExtras {
                identity_always: identity_kind == "a",
                ..ColumnExtras::default()
            };
            let (default, identity) = match generated_kind.as_str() {
                "s" | "v" => {
                    extra.generated = default_expr.map(|expression| GeneratedColumn {
                        expression,
                        stored: generated_kind == "s",
                    });
                    (None, false)
                }
                _ => {
                    // A serial's `nextval(…)` default counts as identity, as
                    // the explorer and structure editor always treated it —
                    // but only when the sequence is the column's own
                    // (`pg_get_serial_sequence`). A default drawing from a
                    // shared sequence is an ordinary default and is kept.
                    (default_expr, !identity_kind.is_empty() || is_serial)
                }
            };
            if extra != ColumnExtras::default() {
                extras.insert(name.clone(), extra);
            }
            Some(ColumnDef {
                name,
                data_type: type_name,
                nullable,
                identity,
                default,
                default_constraint_name: None,
            })
        })
        .collect();
    LoadedColumns {
        columns,
        extras,
        defaults_included: true,
        warning: None,
    }
}

/// The shared `MetadataQuery::Columns` shape (DuckDB, ClickHouse, Oracle,
/// and the PostgreSQL fallback). Oracle types get their NUMBER/RAW
/// arguments back, and its virtual columns are marked for
/// [`oracle_virtual_columns`].
async fn load_generic_columns(
    queries: &dyn QueryExecutor,
    profile: &ConnectionProfile,
    database: &str,
    dialect: MetadataDialect,
    schema: &str,
    table: &str,
) -> StudioResult<LoadedColumns> {
    let rows = run_metadata(
        queries,
        profile,
        database,
        &MetadataQuery::Columns {
            schema: schema.to_string(),
            object: table.to_string(),
        },
    )
    .await?;
    let mut extras = BTreeMap::new();
    let columns = rows
        .into_iter()
        .filter_map(|row| {
            let mut cells = row.into_iter();
            let name = cell_to_string(cells.next())?;
            let type_name = cell_to_string(cells.next()).unwrap_or_default();
            let max_length = cell_to_i64(cells.next());
            let precision = cell_to_i64(cells.next());
            let scale = cell_to_i64(cells.next());
            let nullable = cell_to_bool(cells.next()).unwrap_or(true);
            let is_identity = cell_to_bool(cells.next()).unwrap_or(false);
            let is_computed = cell_to_bool(cells.next()).unwrap_or(false);
            let generic = GenericDdlColumn {
                name,
                type_name,
                max_length,
                precision,
                scale,
                nullable,
                is_identity,
                is_computed,
            };
            let data_type = if dialect == MetadataDialect::Oracle {
                if is_computed {
                    // Expression arrives through the defaults read.
                    extras.insert(
                        generic.name.clone(),
                        ColumnExtras {
                            generated: Some(GeneratedColumn {
                                expression: String::new(),
                                stored: false,
                            }),
                            ..ColumnExtras::default()
                        },
                    );
                }
                oracle_column_type(&generic)
            } else {
                dialect_ddl::render_generic_column_type(&generic)
            };
            Some(ColumnDef {
                name: generic.name.clone(),
                data_type,
                nullable: generic.nullable,
                identity: generic.is_identity,
                default: None,
                default_constraint_name: None,
            })
        })
        .collect();
    Ok(LoadedColumns {
        columns,
        extras,
        defaults_included: false,
        warning: None,
    })
}

fn merge_defaults(
    columns: Vec<ColumnDef>,
    defaults: Vec<(String, Option<String>, Option<String>)>,
) -> Vec<ColumnDef> {
    let map: BTreeMap<String, (Option<String>, Option<String>)> = defaults
        .into_iter()
        .map(|(col, expr, cn)| (col, (expr, cn)))
        .collect();
    columns
        .into_iter()
        .map(|mut col| {
            if let Some((expr, cn)) = map.get(&col.name) {
                col.default = expr.clone();
                col.default_constraint_name = cn.clone();
            }
            col
        })
        .collect()
}

async fn load_defaults(
    queries: &dyn QueryExecutor,
    profile: &ConnectionProfile,
    database: &str,
    schema: &str,
    table: &str,
) -> StudioResult<Vec<(String, Option<String>, Option<String>)>> {
    let rows = run_metadata(
        queries,
        profile,
        database,
        &MetadataQuery::ColumnDefaults {
            schema: schema.to_string(),
            table: table.to_string(),
        },
    )
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let mut cells = row.into_iter();
            let col = cell_to_string(cells.next())?;
            let expr = cell_to_string(cells.next());
            let cn = cell_to_string(cells.next());
            Some((col, expr, cn))
        })
        .collect())
}

async fn load_primary_key(
    queries: &dyn QueryExecutor,
    profile: &ConnectionProfile,
    database: &str,
    dialect: MetadataDialect,
    schema: &str,
    table: &str,
) -> StudioResult<(Vec<String>, Option<String>)> {
    if dialect == MetadataDialect::SqlServer {
        let sql = table_primary_key_sql(schema, table);
        let rows = collect_rows(queries, profile_with_db(profile, database), &sql).await?;
        let mut cols = Vec::new();
        let mut constraint = None;
        for row in rows {
            let mut cells = row.into_iter();
            let cn = cell_to_string(cells.next());
            if constraint.is_none() {
                constraint = cn;
            }
            cells.next();
            if let Some(col) = cell_to_string(cells.next()) {
                cols.push(col);
            }
        }
        return Ok((cols, constraint));
    }

    let rows = run_metadata(queries, profile, database, &MetadataQuery::AllPrimaryKeys).await?;
    let mut cols = Vec::new();
    for row in rows {
        let mut cells = row.into_iter();
        let row_schema = cell_to_string(cells.next()).unwrap_or_default();
        let row_table = cell_to_string(cells.next()).unwrap_or_default();
        if row_schema != schema || row_table != table {
            continue;
        }
        if let Some(col) = cell_to_string(cells.next()) {
            cols.push(col);
        }
    }
    Ok((cols, None))
}

async fn load_indexes(
    queries: &dyn QueryExecutor,
    profile: &ConnectionProfile,
    database: &str,
    schema: &str,
    table: &str,
) -> StudioResult<(Vec<IndexDef>, Vec<String>)> {
    let index_rows = run_metadata(
        queries,
        profile,
        database,
        &MetadataQuery::Indexes {
            schema: schema.to_string(),
            table: table.to_string(),
        },
    )
    .await?;
    let col_rows = run_metadata(
        queries,
        profile,
        database,
        &MetadataQuery::TableIndexColumns {
            schema: schema.to_string(),
            table: table.to_string(),
        },
    )
    .await?;

    let postgres = MetadataDialect::for_database_type(profile.database_type)
        == Some(MetadataDialect::Postgres);
    let mut col_map: BTreeMap<String, (Vec<String>, Vec<String>)> = BTreeMap::new();
    let mut opaque: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for row in col_rows {
        let mut cells = row.into_iter();
        // Every dialect's shape is (index, column, ordinal, is_included,
        // is_descending); the ordinal only orders the rows (the query already
        // sorts by it), so it is skipped — reading it as `is_included` turned
        // every key column into an INCLUDE column. PostgreSQL adds
        // is_expression (an expression key's "column" is its expression
        // text) and has_options (a non-default NULLS FIRST/LAST or operator
        // class); either makes the index opaque, replayed verbatim from
        // pg_indexes.indexdef rather than rebuilt as a plain column list.
        let index_name = cell_to_string(cells.next()).unwrap_or_default();
        let column_name = cell_to_string(cells.next()).unwrap_or_default();
        let _ordinal = cells.next();
        let is_included = cell_to_bool(cells.next()).unwrap_or(false);
        let is_descending = cell_to_bool(cells.next()).unwrap_or(false);
        let is_expression = cell_to_bool(cells.next()).unwrap_or(false);
        let has_options = cell_to_bool(cells.next()).unwrap_or(false);
        if postgres && (is_expression || is_descending || has_options) {
            opaque.insert(index_name.clone());
        }
        let entry = col_map
            .entry(index_name)
            .or_insert((Vec::new(), Vec::new()));
        if is_included {
            entry.1.push(column_name);
        } else {
            entry.0.push(column_name);
        }
    }

    let opaque_ddl = if opaque.is_empty() {
        Vec::new()
    } else {
        let sql = dialect_ddl::pg_table_index_definitions_sql(schema, table);
        collect_rows(queries, profile_with_db(profile, database), &sql)
            .await?
            .into_iter()
            .filter_map(|row| {
                let mut cells = row.into_iter();
                let name = cell_to_string(cells.next())?;
                let ddl = cell_to_string(cells.next())?;
                opaque.contains(&name).then_some(ddl)
            })
            .collect()
    };

    let indexes = index_rows
        .into_iter()
        .filter_map(|row| {
            let mut cells = row.into_iter();
            let name = cell_to_string(cells.next())?;
            let type_desc = cell_to_string(cells.next());
            let unique = cell_to_bool(cells.next()).unwrap_or(false);
            let is_pk = cell_to_bool(cells.next()).unwrap_or(false);
            if is_pk || opaque.contains(&name) {
                return None;
            }
            let filter = cell_to_string(cells.next());
            let fillfactor = cell_to_i64(cells.next())
                .and_then(|n| u8::try_from(n).ok())
                .filter(|n| *n > 0);
            let method = index_method_from_type_desc(type_desc.as_deref(), profile.database_type);
            let (columns, include) = col_map.get(&name).cloned().unwrap_or_default();
            Some(IndexDef {
                name,
                columns,
                unique,
                include,
                filter,
                method,
                fillfactor,
            })
        })
        .collect();
    Ok((indexes, opaque_ddl))
}

fn index_method_from_type_desc(
    type_desc: Option<&str>,
    db: sqlly_core::DatabaseType,
) -> Option<String> {
    let raw = type_desc?.trim();
    if raw.is_empty() {
        return None;
    }
    match db.family() {
        sqlly_lang::sqlmodel::Dialect::Postgres => Some(raw.to_ascii_lowercase()),
        sqlly_lang::sqlmodel::Dialect::MySql => Some(raw.to_ascii_uppercase()),
        sqlly_lang::sqlmodel::Dialect::SqlServer => {
            let upper = raw.to_ascii_uppercase();
            if upper.contains("CLUSTERED") && !upper.contains("NONCLUSTERED") {
                Some("CLUSTERED".to_string())
            } else if upper.contains("NONCLUSTERED") {
                Some("NONCLUSTERED".to_string())
            } else {
                None
            }
        }
        _ => None,
    }
}

async fn load_dependent_views(
    queries: &dyn QueryExecutor,
    profile: &ConnectionProfile,
    database: &str,
    dialect: MetadataDialect,
    schema: &str,
    table: &str,
) -> StudioResult<Vec<DependentView>> {
    let rows = match run_metadata(
        queries,
        profile,
        database,
        &MetadataQuery::DependentViews {
            schema: schema.to_string(),
            table: table.to_string(),
        },
    )
    .await
    {
        Ok(rows) => rows,
        Err(err) => {
            tracing::debug!(%err, schema, table, "dependent views query failed; continuing without");
            return Ok(Vec::new());
        }
    };
    let _ = dialect;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let mut cells = row.into_iter();
            let schema_name = cell_to_string(cells.next());
            let name = cell_to_string(cells.next())?;
            let materialized = cell_to_bool(cells.next()).unwrap_or(false);
            let definition = cell_to_string(cells.next());
            Some(DependentView {
                schema: schema_name,
                name,
                materialized,
                definition,
            })
        })
        .collect())
}

async fn load_foreign_keys(
    queries: &dyn QueryExecutor,
    profile: &ConnectionProfile,
    database: &str,
    dialect: MetadataDialect,
    schema: &str,
    table: &str,
) -> StudioResult<Vec<ForeignKeyDef>> {
    let fk_rows = run_metadata(queries, profile, database, &MetadataQuery::AllForeignKeys).await?;

    let mut map: BTreeMap<String, ForeignKeyDef> = BTreeMap::new();
    for row in fk_rows {
        let mut cells = row.into_iter();
        let constraint_name = cell_to_string(cells.next()).unwrap_or_default();
        let from_schema = cell_to_string(cells.next()).unwrap_or_default();
        let from_table = cell_to_string(cells.next()).unwrap_or_default();
        if from_schema != schema || from_table != table {
            continue;
        }
        let from_column = cell_to_string(cells.next()).unwrap_or_default();
        let to_schema = cell_to_string(cells.next());
        let to_table = cell_to_string(cells.next()).unwrap_or_default();
        let to_column = cell_to_string(cells.next()).unwrap_or_default();
        let entry = map
            .entry(constraint_name.clone())
            .or_insert_with(|| ForeignKeyDef {
                name: constraint_name,
                columns: Vec::new(),
                ref_schema: to_schema,
                ref_table: to_table.clone(),
                ref_columns: Vec::new(),
                on_delete: None,
                on_update: None,
            });
        entry.columns.push(from_column);
        entry.ref_columns.push(to_column);
    }

    if dialect == MetadataDialect::SqlServer && !map.is_empty() {
        let ddl_rows = collect_rows(
            queries,
            profile_with_db(profile, database),
            &table_foreign_keys_ddl_sql(schema, table),
        )
        .await?;
        for row in ddl_rows {
            let ddl = cell_to_string(row.into_iter().next()).unwrap_or_default();
            for fk in map.values_mut() {
                if ddl.contains(&fk.name) {
                    let (on_delete, on_update) = parse_fk_actions(&ddl);
                    fk.on_delete = on_delete;
                    fk.on_update = on_update;
                }
            }
        }
    }

    Ok(map.into_values().collect())
}

async fn load_referenced_by(
    queries: &dyn QueryExecutor,
    profile: &ConnectionProfile,
    database: &str,
    schema: &str,
    table: &str,
) -> StudioResult<Vec<String>> {
    let rows = run_metadata(queries, profile, database, &MetadataQuery::AllForeignKeys).await?;
    let mut tables = BTreeMap::<String, ()>::new();
    for row in rows {
        let mut cells = row.into_iter();
        cells.next();
        let _from_schema = cell_to_string(cells.next()).unwrap_or_default();
        let from_table = cell_to_string(cells.next()).unwrap_or_default();
        cells.next();
        let to_schema = cell_to_string(cells.next()).unwrap_or_default();
        let to_table = cell_to_string(cells.next()).unwrap_or_default();
        if to_schema == schema && to_table == table {
            tables.insert(from_table, ());
        }
    }
    Ok(tables.keys().cloned().collect())
}

async fn load_stored_index_ddl(
    queries: &dyn QueryExecutor,
    profile: &ConnectionProfile,
    database: &str,
    dialect: MetadataDialect,
    schema: &str,
    table: &str,
) -> StudioResult<Vec<String>> {
    let sql = match dialect {
        MetadataDialect::Postgres => dialect_ddl::pg_table_indexes_ddl_sql(schema, table),
        MetadataDialect::Sqlite => dialect_ddl::sqlite_table_indexes_ddl_sql(table),
        MetadataDialect::SqlServer => {
            sqlly_intel::metadata::ddl::table_indexes_ddl_sql(schema, table)
        }
        _ => return Ok(Vec::new()),
    };
    let rows = collect_rows(queries, profile_with_db(profile, database), &sql).await?;
    Ok(rows
        .into_iter()
        .filter_map(|row| cell_to_string(row.into_iter().next()))
        .filter(|s| !s.trim().is_empty())
        .collect())
}

async fn load_stored_trigger_ddl(
    queries: &dyn QueryExecutor,
    profile: &ConnectionProfile,
    database: &str,
    dialect: MetadataDialect,
    schema: &str,
    table: &str,
) -> StudioResult<Vec<String>> {
    if dialect != MetadataDialect::Sqlite {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT sql FROM sqlite_master WHERE type = 'trigger' AND tbl_name = '{}' AND sql IS NOT NULL ORDER BY name",
        table.replace('\'', "''")
    );
    let _ = schema;
    let rows = collect_rows(queries, profile_with_db(profile, database), &sql).await?;
    Ok(rows
        .into_iter()
        .filter_map(|row| cell_to_string(row.into_iter().next()))
        .filter(|s| !s.trim().is_empty())
        .collect())
}

/// The table's and its columns' server-side descriptions, in the dialect's
/// own catalog. Engines without a comment store (SQLite/libSQL) return empty
/// without querying. A failed read is returned as an error — the caller
/// records it on the snapshot so description edits are refused rather than
/// scripted against a guessed "no description" (which on SQL Server picks
/// `sp_addextendedproperty` for a property that already exists).
async fn load_comments(
    queries: &dyn QueryExecutor,
    profile: &ConnectionProfile,
    database: &str,
    dialect: MetadataDialect,
    schema: &str,
    table: &str,
) -> StudioResult<(Option<String>, BTreeMap<String, String>)> {
    let esc = |text: &str| text.replace('\'', "''");
    let (table_sql, column_sql) = match dialect {
        MetadataDialect::SqlServer => {
            let object = tds_object_id(schema, table);
            (
                format!(
                    "SELECT CAST(ep.value AS nvarchar(4000)) FROM sys.extended_properties ep WHERE ep.class = 1 AND ep.major_id = {object} AND ep.minor_id = 0 AND ep.name = N'MS_Description'"
                ),
                format!(
                    "SELECT c.name, CAST(ep.value AS nvarchar(4000)) FROM sys.extended_properties ep JOIN sys.columns c ON c.object_id = ep.major_id AND c.column_id = ep.minor_id WHERE ep.class = 1 AND ep.major_id = {object} AND ep.minor_id > 0 AND ep.name = N'MS_Description' ORDER BY c.column_id"
                ),
            )
        }
        MetadataDialect::Postgres => {
            let regclass = pg_regclass_literal(schema, table);
            (
                format!("SELECT obj_description({regclass}::regclass, 'pg_class')"),
                format!(
                    "SELECT a.attname, col_description({regclass}::regclass, a.attnum) FROM pg_attribute a WHERE a.attrelid = {regclass}::regclass AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum"
                ),
            )
        }
        MetadataDialect::MySql => {
            let db = profile.database_type;
            let scope = if schema.is_empty() {
                "DATABASE()".to_string()
            } else {
                string_literal(db, schema)
            };
            let table_lit = string_literal(db, table);
            (
                format!(
                    "SELECT table_comment FROM information_schema.tables WHERE table_schema = {scope} AND table_name = {table_lit}"
                ),
                format!(
                    "SELECT column_name, column_comment FROM information_schema.columns WHERE table_schema = {scope} AND table_name = {table_lit} ORDER BY ordinal_position"
                ),
            )
        }
        MetadataDialect::ClickHouse => {
            // ClickHouse string literals interpret backslashes too.
            let db = profile.database_type;
            let scope = if schema.is_empty() {
                "currentDatabase()".to_string()
            } else {
                string_literal(db, schema)
            };
            let table_lit = string_literal(db, table);
            (
                format!(
                    "SELECT comment FROM system.tables WHERE database = {scope} AND name = {table_lit}"
                ),
                format!(
                    "SELECT name, comment FROM system.columns WHERE database = {scope} AND table = {table_lit} ORDER BY position"
                ),
            )
        }
        MetadataDialect::DuckDb => (
            format!(
                "SELECT comment FROM duckdb_tables() WHERE schema_name = '{}' AND table_name = '{}'",
                esc(schema),
                esc(table)
            ),
            format!(
                "SELECT column_name, comment FROM duckdb_columns() WHERE schema_name = '{}' AND table_name = '{}' ORDER BY column_index",
                esc(schema),
                esc(table)
            ),
        ),
        MetadataDialect::Oracle => (
            format!(
                "SELECT comments FROM all_tab_comments WHERE owner = '{}' AND table_name = '{}'",
                esc(schema),
                esc(table)
            ),
            format!(
                "SELECT column_name, comments FROM all_col_comments WHERE owner = '{}' AND table_name = '{}' ORDER BY column_id",
                esc(schema),
                esc(table)
            ),
        ),
        // SQLite (and libSQL over the same dialect) has no comment store.
        MetadataDialect::Sqlite => return Ok((None, BTreeMap::new())),
    };

    // SQL Server's extended property can exist with an empty value; keep it
    // (as `Some("")`) so a later edit updates it instead of adding a
    // duplicate. Everywhere else '' is how the engine spells "no comment".
    let keep_empty = dialect == MetadataDialect::SqlServer;
    let keep = |text: &String| keep_empty || !text.is_empty();
    let table_rows = collect_rows(queries, profile_with_db(profile, database), &table_sql).await?;
    let table_comment = table_rows
        .into_iter()
        .next()
        .and_then(|row| cell_to_string(row.into_iter().next()))
        .filter(keep);
    let column_rows =
        collect_rows(queries, profile_with_db(profile, database), &column_sql).await?;
    let mut column_comments = BTreeMap::new();
    for row in column_rows {
        let mut cells = row.into_iter();
        let Some(name) = cell_to_string(cells.next()) else {
            continue;
        };
        if let Some(text) = cell_to_string(cells.next()).filter(keep) {
            column_comments.insert(name, text);
        }
    }
    Ok((table_comment, column_comments))
}

/// Table-level CHECK constraints in the engine's own expression text.
/// MySQL/MariaDB keep theirs in the `SHOW CREATE TABLE` text the CREATE
/// script replays, and the file engines replay their stored DDL, so only the
/// snapshot-rendered engines read them here.
async fn load_check_constraints(
    queries: &dyn QueryExecutor,
    profile: &ConnectionProfile,
    database: &str,
    dialect: MetadataDialect,
    schema: &str,
    table: &str,
) -> StudioResult<Vec<CheckConstraintDef>> {
    let esc = |text: &str| text.replace('\'', "''");
    let sql = match dialect {
        MetadataDialect::SqlServer => format!(
            "SELECT cc.name, cc.definition, cc.is_system_named FROM sys.check_constraints cc WHERE cc.parent_object_id = {} ORDER BY cc.name",
            tds_object_id(schema, table)
        ),
        MetadataDialect::Postgres => format!(
            "SELECT con.conname, pg_catalog.pg_get_constraintdef(con.oid), 0 FROM pg_catalog.pg_constraint con WHERE con.conrelid = {}::regclass AND con.contype = 'c' ORDER BY con.conname",
            pg_regclass_literal(schema, table)
        ),
        MetadataDialect::Oracle => format!(
            "SELECT constraint_name, search_condition_vc, CASE generated WHEN 'GENERATED NAME' THEN 1 ELSE 0 END FROM all_constraints WHERE owner = '{}' AND table_name = '{}' AND constraint_type = 'C' ORDER BY constraint_name",
            esc(schema),
            esc(table)
        ),
        _ => return Ok(Vec::new()),
    };
    let rows = collect_rows(queries, profile_with_db(profile, database), &sql).await?;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let mut cells = row.into_iter();
            let name = cell_to_string(cells.next())?;
            let text = cell_to_string(cells.next())?;
            let system_named = cell_to_bool(cells.next()).unwrap_or(false);
            check_constraint_from_catalog(dialect, name, &text, system_named)
        })
        .collect())
}

/// Normalise one catalog CHECK row to `(name, parenthesised expression)`.
/// PostgreSQL's `pg_get_constraintdef` includes the `CHECK` keyword and may
/// end `NOT VALID` (not accepted inside CREATE TABLE); Oracle's condition is
/// bare and its column NOT NULLs are system-named CHECKs the column line
/// already states.
fn check_constraint_from_catalog(
    dialect: MetadataDialect,
    name: String,
    text: &str,
    system_named: bool,
) -> Option<CheckConstraintDef> {
    let text = text.trim();
    let expression = match dialect {
        MetadataDialect::Postgres => text
            .strip_prefix("CHECK ")
            .unwrap_or(text)
            .trim_end_matches(" NOT VALID")
            .trim()
            .to_string(),
        MetadataDialect::Oracle => {
            if system_named && is_oracle_not_null_check(text) {
                return None;
            }
            format!("({text})")
        }
        _ => text.to_string(),
    };
    if expression.is_empty() {
        return None;
    }
    Some(CheckConstraintDef {
        name: (!system_named).then_some(name),
        expression,
    })
}

/// `"COL" IS NOT NULL` — the form Oracle stores a column NOT NULL in.
fn is_oracle_not_null_check(text: &str) -> bool {
    let Some(rest) = text.strip_prefix('"') else {
        return false;
    };
    match rest.find('"') {
        Some(end) => rest[end + 1..].trim().eq_ignore_ascii_case("IS NOT NULL"),
        None => false,
    }
}

/// `' "schema"."table" '` as a string literal usable as `::regclass` input.
fn pg_regclass_literal(schema: &str, table: &str) -> String {
    let inner = format!(
        "\"{}\".\"{}\"",
        schema.replace('"', "\"\""),
        table.replace('"', "\"\"")
    );
    format!("'{}'", inner.replace('\'', "''"))
}

async fn load_sqlite_foreign_keys_on(
    queries: &dyn QueryExecutor,
    profile: &ConnectionProfile,
    database: &str,
    dialect: MetadataDialect,
) -> StudioResult<bool> {
    if dialect != MetadataDialect::Sqlite {
        return Ok(false);
    }
    let rows = collect_rows(
        queries,
        profile_with_db(profile, database),
        "PRAGMA foreign_keys",
    )
    .await?;
    Ok(rows
        .into_iter()
        .next()
        .and_then(|row| cell_to_bool(row.into_iter().next()))
        .unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use sqlly_core::{CellValue, SecretProvider, SecretRef};
    use std::sync::Mutex;
    use tokio::sync::mpsc;

    struct FakeQueries {
        routes: Vec<(String, Vec<Vec<CellValue>>)>,
        executed: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl QueryExecutor for FakeQueries {
        async fn execute(
            &self,
            _profile: &ConnectionProfile,
            sql: &str,
            _row_buffer: usize,
        ) -> sqlly_core::StudioResult<crate::query::QueryHandle> {
            self.executed.lock().unwrap().push(sql.to_string());
            let (tx, rx) = mpsc::channel(16);
            let rows = self
                .routes
                .iter()
                .find(|(needle, _)| sql.contains(needle))
                .map(|(_, rows)| rows.clone())
                .unwrap_or_default();
            tx.send(QueryEvent::Rows { index: 0, rows }).await.ok();
            tx.send(QueryEvent::Completed { elapsed_ms: 1 }).await.ok();
            Ok(crate::query::QueryHandle {
                task_id: "t".into(),
                events: rx,
            })
        }

        async fn cancel(&self, _task_id: &str) -> sqlly_core::StudioResult<()> {
            Ok(())
        }
    }

    fn profile_sqlite() -> ConnectionProfile {
        let mut p = ConnectionProfile::sql_login(
            "sqlite-test",
            "localhost",
            "",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: String::new(),
            },
        );
        p.database_type = sqlly_core::DatabaseType::Sqlite;
        p
    }

    fn s(text: &str) -> CellValue {
        CellValue::String(text.to_string())
    }

    #[tokio::test]
    async fn sqlite_snapshot_loads_defaults_indexes_and_fk() {
        let queries = FakeQueries {
            routes: vec![
                (
                    "c.type AS type_name".into(),
                    vec![
                        vec![
                            s("id"),
                            s("INTEGER"),
                            CellValue::I64(-1),
                            CellValue::I64(0),
                            CellValue::I64(0),
                            CellValue::Bool(false),
                            CellValue::Bool(false),
                            CellValue::Bool(false),
                        ],
                        vec![
                            s("org_id"),
                            s("INTEGER"),
                            CellValue::I64(-1),
                            CellValue::I64(0),
                            CellValue::I64(0),
                            CellValue::Bool(true),
                            CellValue::Bool(false),
                            CellValue::Bool(false),
                        ],
                    ],
                ),
                (
                    "dflt_value IS NOT NULL".into(),
                    vec![vec![s("org_id"), s("0"), CellValue::Null]],
                ),
                (
                    "pragma_index_list".into(),
                    vec![vec![
                        s("IX_org"),
                        s("BTREE"),
                        CellValue::Bool(false),
                        CellValue::Bool(false),
                        CellValue::Null,
                        CellValue::Null,
                    ]],
                ),
                ("sqlite_master WHERE type = 'view'".into(), vec![]),
                (
                    "pragma_index_info".into(),
                    vec![vec![s("org_id"), CellValue::I64(1), CellValue::Bool(false)]],
                ),
                (
                    "pragma_foreign_key_list".into(),
                    vec![vec![
                        s("fk_t_org"),
                        s("main"),
                        s("t"),
                        s("org_id"),
                        s("main"),
                        s("orgs"),
                        s("id"),
                    ]],
                ),
                (
                    "PRAGMA foreign_keys".into(),
                    vec![vec![CellValue::Bool(true)]],
                ),
            ],
            executed: Mutex::new(Vec::new()),
        };

        let snap = load_table_snapshot(&queries, &profile_sqlite(), "main", None, "t")
            .await
            .unwrap();

        assert_eq!(snap.table, "t");
        assert!(
            snap.columns
                .iter()
                .any(|c| c.name == "org_id" && c.default == Some("0".into()))
        );
        assert!(snap.indexes.iter().any(|i| i.name == "IX_org"));
        assert!(snap.foreign_keys.iter().any(|f| f.name == "fk_t_org"));
        assert!(snap.sqlite_foreign_keys_on);
    }

    #[test]
    fn index_row_maps_method_and_fillfactor() {
        let row = vec![
            s("IX_hash"),
            s("hash"),
            CellValue::Bool(false),
            CellValue::Bool(false),
            CellValue::Null,
            CellValue::I64(80),
        ];
        let mut cells = row.into_iter();
        let name = cell_to_string(cells.next()).unwrap();
        let type_desc = cell_to_string(cells.next());
        let unique = cell_to_bool(cells.next()).unwrap_or(false);
        let is_pk = cell_to_bool(cells.next()).unwrap_or(false);
        assert!(!is_pk);
        let filter = cell_to_string(cells.next());
        let fillfactor = cell_to_i64(cells.next())
            .and_then(|n| u8::try_from(n).ok())
            .filter(|n| *n > 0);
        let method =
            index_method_from_type_desc(type_desc.as_deref(), sqlly_core::DatabaseType::Postgres);
        let idx = IndexDef {
            name,
            columns: vec![],
            unique,
            include: vec![],
            filter,
            method,
            fillfactor,
        };
        assert_eq!(idx.method, Some("hash".into()));
        assert_eq!(idx.fillfactor, Some(80));
    }

    fn profile_for(database_type: sqlly_core::DatabaseType) -> ConnectionProfile {
        let mut p = ConnectionProfile::sql_login(
            "comment-test",
            "localhost",
            "",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: String::new(),
            },
        );
        p.database_type = database_type;
        p
    }

    /// Regression: the index-column rows are (index, column, ordinal,
    /// is_included, …); the loader read the ordinal as `is_included`, so
    /// every key column became an INCLUDE column and the CREATE script wrote
    /// `CREATE INDEX … ()` .
    #[tokio::test]
    async fn sql_server_snapshot_keeps_index_key_and_include_columns_apart() {
        let queries = FakeQueries {
            routes: vec![
                (
                    "ic.is_included_column AS is_included".into(),
                    vec![
                        vec![
                            s("IX_code"),
                            s("code"),
                            CellValue::I64(1),
                            CellValue::Bool(false),
                            CellValue::Bool(false),
                        ],
                        vec![
                            s("IX_code"),
                            s("region"),
                            CellValue::I64(2),
                            CellValue::Bool(false),
                            CellValue::Bool(false),
                        ],
                        vec![
                            s("IX_code"),
                            s("note"),
                            CellValue::I64(0),
                            CellValue::Bool(true),
                            CellValue::Bool(false),
                        ],
                    ],
                ),
                (
                    "i.is_primary_key, i.filter_definition".into(),
                    vec![vec![
                        s("IX_code"),
                        s("NONCLUSTERED"),
                        CellValue::Bool(false),
                        CellValue::Bool(false),
                        CellValue::Null,
                        CellValue::I64(0),
                    ]],
                ),
            ],
            executed: Mutex::new(Vec::new()),
        };
        let snap = load_table_snapshot(
            &queries,
            &profile_for(sqlly_core::DatabaseType::SqlServer),
            "app",
            Some("dbo"),
            "people",
        )
        .await
        .unwrap();
        let index = snap.indexes.iter().find(|i| i.name == "IX_code").unwrap();
        assert_eq!(index.columns, vec!["code", "region"]);
        assert_eq!(index.include, vec!["note"]);
    }

    /// PostgreSQL expression keys (and descending keys) cannot be written
    /// back as plain column lists, so those indexes stay out of the editable
    /// list and keep the engine's own CREATE INDEX text instead; a plain
    /// index is still read column by column.
    #[tokio::test]
    async fn postgres_expression_and_desc_indexes_are_kept_opaque() {
        let index_col = |ix: &str, col: &str, ord: i64, desc: i64, expr: i64| {
            vec![
                s(ix),
                s(col),
                CellValue::I64(ord),
                CellValue::I64(0),
                CellValue::I64(desc),
                CellValue::I64(expr),
                CellValue::I64(0),
            ]
        };
        let index = |name: &str| {
            vec![
                s(name),
                s("btree"),
                CellValue::I64(0),
                CellValue::I64(0),
                CellValue::Null,
                CellValue::Null,
            ]
        };
        let queries = FakeQueries {
            routes: vec![
                (
                    "AS is_expression".into(),
                    vec![
                        index_col("ix_lower", "lower(email)", 1, 0, 1),
                        index_col("ix_name_desc", "name", 1, 1, 0),
                        index_col("ix_plain", "name", 1, 0, 0),
                        index_col("ix_plain", "email", 2, 0, 0),
                    ],
                ),
                (
                    "SELECT indexname, indexdef".into(),
                    vec![
                        vec![
                            s("ix_lower"),
                            s("CREATE INDEX ix_lower ON public.users USING btree (lower(email));"),
                        ],
                        vec![
                            s("ix_name_desc"),
                            s("CREATE INDEX ix_name_desc ON public.users USING btree (name DESC);"),
                        ],
                        vec![
                            s("ix_plain"),
                            s("CREATE INDEX ix_plain ON public.users USING btree (name, email);"),
                        ],
                    ],
                ),
                (
                    "ix.indisprimary::int AS is_primary_key, pg_get_expr".into(),
                    vec![index("ix_lower"), index("ix_name_desc"), index("ix_plain")],
                ),
            ],
            executed: Mutex::new(Vec::new()),
        };
        let snap = load_table_snapshot(
            &queries,
            &profile_for(sqlly_core::DatabaseType::Postgres),
            "app",
            Some("public"),
            "users",
        )
        .await
        .unwrap();
        let names: Vec<&str> = snap.indexes.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec!["ix_plain"]);
        assert_eq!(snap.indexes[0].columns, vec!["name", "email"]);
        assert_eq!(
            snap.opaque_index_ddl,
            vec![
                "CREATE INDEX ix_lower ON public.users USING btree (lower(email));".to_string(),
                "CREATE INDEX ix_name_desc ON public.users USING btree (name DESC);".to_string(),
            ]
        );
    }

    /// `NULLS FIRST` on an ascending key and a non-default operator class
    /// (`text_pattern_ops`) are modifiers a plain column list cannot restate,
    /// so those indexes are replayed verbatim from pg_indexes.indexdef too.
    #[tokio::test]
    async fn postgres_nulls_order_and_opclass_indexes_are_kept_opaque() {
        let index_col = |ix: &str, col: &str, ord: i64, options: i64| {
            vec![
                s(ix),
                s(col),
                CellValue::I64(ord),
                CellValue::I64(0),
                CellValue::I64(0),
                CellValue::I64(0),
                CellValue::I64(options),
            ]
        };
        let index = |name: &str| {
            vec![
                s(name),
                s("btree"),
                CellValue::I64(0),
                CellValue::I64(0),
                CellValue::Null,
                CellValue::Null,
            ]
        };
        let queries = FakeQueries {
            routes: vec![
                (
                    "AS has_options".into(),
                    vec![
                        index_col("ix_nulls_first", "name", 1, 1),
                        index_col("ix_pattern", "email", 1, 1),
                        index_col("ix_plain", "name", 1, 0),
                        index_col("ix_plain", "email", 2, 0),
                    ],
                ),
                (
                    "SELECT indexname, indexdef".into(),
                    vec![
                        vec![
                            s("ix_nulls_first"),
                            s(
                                "CREATE INDEX ix_nulls_first ON public.users USING btree (name NULLS FIRST);",
                            ),
                        ],
                        vec![
                            s("ix_pattern"),
                            s(
                                "CREATE INDEX ix_pattern ON public.users USING btree (email text_pattern_ops);",
                            ),
                        ],
                        vec![
                            s("ix_plain"),
                            s("CREATE INDEX ix_plain ON public.users USING btree (name, email);"),
                        ],
                    ],
                ),
                (
                    "ix.indisprimary::int AS is_primary_key, pg_get_expr".into(),
                    vec![
                        index("ix_nulls_first"),
                        index("ix_pattern"),
                        index("ix_plain"),
                    ],
                ),
            ],
            executed: Mutex::new(Vec::new()),
        };
        let snap = load_table_snapshot(
            &queries,
            &profile_for(sqlly_core::DatabaseType::Postgres),
            "app",
            Some("public"),
            "users",
        )
        .await
        .unwrap();
        let names: Vec<&str> = snap.indexes.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec!["ix_plain"]);
        assert_eq!(
            snap.opaque_index_ddl,
            vec![
                "CREATE INDEX ix_nulls_first ON public.users USING btree (name NULLS FIRST);"
                    .to_string(),
                "CREATE INDEX ix_pattern ON public.users USING btree (email text_pattern_ops);"
                    .to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn sql_server_snapshot_loads_table_and_column_comments() {
        let queries = FakeQueries {
            routes: vec![
                (
                    "ep.minor_id > 0".into(),
                    vec![
                        vec![s("org_id"), s("Owning organization")],
                        vec![s("note"), CellValue::Null],
                    ],
                ),
                ("ep.minor_id = 0".into(), vec![vec![s("People table")]]),
            ],
            executed: Mutex::new(Vec::new()),
        };

        let snap = load_table_snapshot(
            &queries,
            &profile_for(sqlly_core::DatabaseType::SqlServer),
            "app",
            Some("dbo"),
            "people",
        )
        .await
        .unwrap();

        assert_eq!(snap.table_comment.as_deref(), Some("People table"));
        assert_eq!(
            snap.column_comments.get("org_id").map(String::as_str),
            Some("Owning organization")
        );
        assert!(!snap.column_comments.contains_key("note"));
    }

    #[tokio::test]
    async fn mysql_empty_comments_read_as_unset() {
        let queries = FakeQueries {
            routes: vec![
                ("information_schema.tables".into(), vec![vec![s("")]]),
                (
                    "information_schema.columns".into(),
                    vec![vec![s("id"), s("surrogate key")], vec![s("name"), s("")]],
                ),
            ],
            executed: Mutex::new(Vec::new()),
        };

        let snap = load_table_snapshot(
            &queries,
            &profile_for(sqlly_core::DatabaseType::MySql),
            "shop",
            Some("shop"),
            "items",
        )
        .await
        .unwrap();

        assert_eq!(snap.table_comment, None);
        assert_eq!(
            snap.column_comments.get("id").map(String::as_str),
            Some("surrogate key")
        );
        assert!(!snap.column_comments.contains_key("name"));
    }

    #[tokio::test]
    async fn sqlite_snapshot_reads_no_comments() {
        let queries = FakeQueries {
            routes: vec![],
            executed: Mutex::new(Vec::new()),
        };
        let snap = load_table_snapshot(&queries, &profile_sqlite(), "main", None, "t")
            .await
            .unwrap();
        assert_eq!(snap.table_comment, None);
        assert!(snap.column_comments.is_empty());
        let executed = queries.executed.lock().unwrap();
        assert!(
            !executed.iter().any(|sql| sql.contains("comment")),
            "SQLite must not issue comment reads: {executed:?}"
        );
    }

    #[tokio::test]
    async fn comment_read_failure_does_not_fail_the_snapshot() {
        struct FailingQueries;
        #[async_trait]
        impl QueryExecutor for FailingQueries {
            async fn execute(
                &self,
                _profile: &ConnectionProfile,
                sql: &str,
                _row_buffer: usize,
            ) -> sqlly_core::StudioResult<crate::query::QueryHandle> {
                let (tx, rx) = mpsc::channel(16);
                if sql.contains("extended_properties") {
                    tx.send(QueryEvent::Error {
                        message: "permission denied".into(),
                    })
                    .await
                    .ok();
                } else {
                    tx.send(QueryEvent::Rows {
                        index: 0,
                        rows: vec![],
                    })
                    .await
                    .ok();
                    tx.send(QueryEvent::Completed { elapsed_ms: 1 }).await.ok();
                }
                Ok(crate::query::QueryHandle {
                    task_id: "t".into(),
                    events: rx,
                })
            }

            async fn cancel(&self, _task_id: &str) -> sqlly_core::StudioResult<()> {
                Ok(())
            }
        }

        let snap = load_table_snapshot(
            &FailingQueries,
            &profile_for(sqlly_core::DatabaseType::SqlServer),
            "app",
            Some("dbo"),
            "people",
        )
        .await
        .unwrap();
        assert_eq!(snap.table_comment, None);
        assert!(snap.column_comments.is_empty());
        // …but the snapshot knows the descriptions are unknown, not unset.
        assert!(
            snap.comment_read_error
                .as_deref()
                .is_some_and(|e| e.contains("permission denied"))
        );
        assert!(
            snap.load_warnings
                .iter()
                .any(|w| w.starts_with("Descriptions could not be read"))
        );
    }

    fn i(n: i64) -> CellValue {
        CellValue::I64(n)
    }

    #[tokio::test]
    async fn sql_server_snapshot_quotes_names_keeps_empty_descriptions_and_identity_seed() {
        let queries = FakeQueries {
            routes: vec![
                (
                    "sys.identity_columns".into(),
                    vec![
                        vec![
                            s("id"),
                            s("int"),
                            i(4),
                            i(10),
                            i(0),
                            i(0),
                            i(1),
                            s("1000"),
                            s("5"),
                            i(0),
                            CellValue::Null,
                            i(0),
                        ],
                        vec![
                            s("twice"),
                            s("int"),
                            i(4),
                            i(10),
                            i(0),
                            i(1),
                            i(0),
                            CellValue::Null,
                            CellValue::Null,
                            i(1),
                            s("([id]*(2))"),
                            i(1),
                        ],
                    ],
                ),
                ("ep.minor_id > 0".into(), vec![vec![s("id"), s("")]]),
                ("ep.minor_id = 0".into(), vec![vec![s("")]]),
            ],
            executed: Mutex::new(Vec::new()),
        };
        let snap = load_table_snapshot(
            &queries,
            &profile_for(sqlly_core::DatabaseType::SqlServer),
            "app",
            Some("dbo"),
            "we'ird.table",
        )
        .await
        .unwrap();
        let extras = snap.column_extras.get("id").unwrap();
        assert_eq!(
            extras.identity_seed_increment,
            Some(("1000".to_string(), "5".to_string()))
        );
        let twice = snap.column_extras.get("twice").unwrap();
        assert_eq!(
            twice.generated,
            Some(GeneratedColumn {
                expression: "([id]*(2))".into(),
                stored: true
            })
        );
        // An MS_Description stored as '' still exists on the server.
        assert_eq!(snap.table_comment.as_deref(), Some(""));
        assert_eq!(snap.column_comments.get("id").map(String::as_str), Some(""));
        let executed = queries.executed.lock().unwrap();
        let comment_sql = executed
            .iter()
            .find(|sql| sql.contains("ep.minor_id = 0"))
            .unwrap();
        assert!(
            comment_sql
                .contains("OBJECT_ID(QUOTENAME(N'dbo') + N'.' + QUOTENAME(N'we''ird.table'))"),
            "{comment_sql}"
        );
    }

    #[tokio::test]
    async fn mysql_snapshot_uses_column_type_and_spells_defaults_as_sql() {
        let queries = FakeQueries {
            routes: vec![
                (
                    "c.column_type".into(),
                    vec![
                        vec![
                            s("id"),
                            s("int unsigned"),
                            s("int"),
                            i(0),
                            s("auto_increment"),
                            CellValue::Null,
                            s(""),
                        ],
                        vec![
                            s("state"),
                            s("enum('new','done')"),
                            s("enum"),
                            i(0),
                            s(""),
                            s("new"),
                            s(""),
                        ],
                        vec![s("qty"), s("int"), s("int"), i(0), s(""), s("0"), s("")],
                        vec![
                            s("at"),
                            s("datetime(3)"),
                            s("datetime"),
                            i(0),
                            s("DEFAULT_GENERATED on update CURRENT_TIMESTAMP(3)"),
                            s("CURRENT_TIMESTAMP(3)"),
                            s(""),
                        ],
                        vec![
                            s("tag"),
                            s("varchar(9)"),
                            s("varchar"),
                            i(1),
                            s("DEFAULT_GENERATED"),
                            s("concat(_utf8mb4\\'a\\',_utf8mb4\\'b\\')"),
                            s(""),
                        ],
                        vec![
                            s("total"),
                            s("int"),
                            s("int"),
                            i(1),
                            s("STORED GENERATED"),
                            CellValue::Null,
                            s("(`qty` * 2)"),
                        ],
                    ],
                ),
                (
                    "SHOW CREATE TABLE".into(),
                    vec![vec![s("t"), s("CREATE TABLE `t` (\n)")]],
                ),
            ],
            executed: Mutex::new(Vec::new()),
        };
        let snap = load_table_snapshot(
            &queries,
            &profile_for(sqlly_core::DatabaseType::MySql),
            "shop",
            Some("shop"),
            "t",
        )
        .await
        .unwrap();
        let col = |name: &str| snap.columns.iter().find(|c| c.name == name).unwrap();
        assert_eq!(col("id").data_type, "int unsigned");
        assert!(col("id").identity);
        assert_eq!(col("state").data_type, "enum('new','done')");
        assert_eq!(col("state").default.as_deref(), Some("'new'"));
        assert_eq!(col("qty").default.as_deref(), Some("0"));
        assert_eq!(col("at").default.as_deref(), Some("CURRENT_TIMESTAMP(3)"));
        assert_eq!(
            snap.column_extras
                .get("at")
                .and_then(|e| e.on_update.as_deref()),
            Some("CURRENT_TIMESTAMP(3)")
        );
        assert_eq!(
            col("tag").default.as_deref(),
            Some("(concat(_utf8mb4'a',_utf8mb4'b'))")
        );
        assert_eq!(col("total").default, None);
        assert_eq!(
            snap.column_extras
                .get("total")
                .and_then(|e| e.generated.clone()),
            Some(GeneratedColumn {
                expression: "(`qty` * 2)".into(),
                stored: true
            })
        );
        assert!(snap.stored_table_ddl.is_some());
    }

    #[test]
    fn mariadb_defaults_are_already_sql() {
        use sqlly_core::DatabaseType as D;
        assert_eq!(
            mysql_default_sql(D::MariaDb, Some("'new'".into()), "varchar", ""),
            Some("'new'".into())
        );
        assert_eq!(
            mysql_default_sql(D::MariaDb, Some("NULL".into()), "varchar", ""),
            None
        );
        assert_eq!(
            mysql_default_sql(
                D::MariaDb,
                Some("current_timestamp()".into()),
                "timestamp",
                ""
            ),
            Some("current_timestamp()".into())
        );
        // MySQL 5.7: CURRENT_TIMESTAMP without the DEFAULT_GENERATED marker.
        assert_eq!(
            mysql_default_sql(D::MySql, Some("CURRENT_TIMESTAMP".into()), "timestamp", ""),
            Some("CURRENT_TIMESTAMP".into())
        );
        assert_eq!(
            mysql_default_sql(D::MySql, Some("it's".into()), "char", ""),
            Some("'it''s'".into())
        );
        assert_eq!(
            mysql_default_sql(D::MySql, Some("b'101'".into()), "bit", ""),
            Some("b'101'".into())
        );
    }

    #[tokio::test]
    async fn postgres_snapshot_reads_format_type_identity_and_generated() {
        let queries = FakeQueries {
            routes: vec![(
                "format_type".into(),
                vec![
                    vec![s("id"), s("bigint"), i(0), s("a"), s(""), CellValue::Null],
                    vec![s("tags"), s("text[]"), i(1), s(""), s(""), CellValue::Null],
                    vec![
                        s("mood"),
                        s("mood_enum"),
                        i(1),
                        s(""),
                        s(""),
                        s("'ok'::mood_enum"),
                    ],
                    vec![
                        s("n"),
                        s("integer"),
                        i(0),
                        s(""),
                        s(""),
                        s("nextval('t_n_seq'::regclass)"),
                        i(1),
                    ],
                    vec![
                        s("shared"),
                        s("integer"),
                        i(0),
                        s(""),
                        s(""),
                        s("nextval('shared_seq'::regclass)"),
                        i(0),
                    ],
                    vec![
                        s("gross"),
                        s("numeric(10,2)"),
                        i(1),
                        s(""),
                        s("s"),
                        s("(price * 1.2)"),
                    ],
                ],
            )],
            executed: Mutex::new(Vec::new()),
        };
        let snap = load_table_snapshot(
            &queries,
            &profile_for(sqlly_core::DatabaseType::Postgres),
            "app",
            Some("public"),
            "t",
        )
        .await
        .unwrap();
        let col = |name: &str| snap.columns.iter().find(|c| c.name == name).unwrap();
        assert_eq!(col("tags").data_type, "text[]");
        assert_eq!(col("mood").data_type, "mood_enum");
        assert_eq!(col("mood").default.as_deref(), Some("'ok'::mood_enum"));
        assert!(col("id").identity);
        assert!(snap.column_extras.get("id").unwrap().identity_always);
        assert!(col("n").identity, "a serial reads as identity");
        assert!(
            !col("shared").identity,
            "a default from a sequence the column does not own is not identity"
        );
        assert_eq!(
            col("shared").default.as_deref(),
            Some("nextval('shared_seq'::regclass)")
        );
        assert_eq!(col("gross").default, None);
        assert_eq!(
            snap.column_extras
                .get("gross")
                .and_then(|e| e.generated.clone()),
            Some(GeneratedColumn {
                expression: "(price * 1.2)".into(),
                stored: true
            })
        );
        // Defaults came with the column read; no separate defaults query.
        let executed = queries.executed.lock().unwrap();
        assert!(
            !executed
                .iter()
                .any(|sql| sql.contains("column_default IS NOT NULL"))
        );
    }

    #[test]
    fn oracle_types_keep_their_arguments() {
        let col = |ty: &str, len: i64, p: i64, s: i64| GenericDdlColumn {
            name: "c".into(),
            type_name: ty.into(),
            max_length: Some(len),
            precision: Some(p),
            scale: Some(s),
            nullable: true,
            is_identity: false,
            is_computed: false,
        };
        assert_eq!(
            oracle_column_type(&col("NUMBER", 22, 10, 2)),
            "NUMBER(10,2)"
        );
        assert_eq!(oracle_column_type(&col("NUMBER", 22, 9, 0)), "NUMBER(9)");
        assert_eq!(oracle_column_type(&col("NUMBER", 22, 0, 0)), "NUMBER");
        assert_eq!(oracle_column_type(&col("RAW", 16, 0, 0)), "RAW(16)");
        assert_eq!(
            oracle_column_type(&col("BINARY_DOUBLE", 8, 0, 0)),
            "BINARY_DOUBLE"
        );
        assert_eq!(
            oracle_column_type(&col("VARCHAR2", 40, 0, 0)),
            "VARCHAR2(40)"
        );
    }

    fn facts(ty: &str) -> OracleColumnFacts {
        OracleColumnFacts {
            data_type: ty.into(),
            ..OracleColumnFacts::default()
        }
    }

    /// Oracle stores INTEGER as NUMBER with a NULL precision and scale 0,
    /// and an unconstrained NUMBER with both NULL; the shared column shape
    /// folded NULL to 0, so INTEGER came back as a plain NUMBER.
    #[test]
    fn oracle_integer_and_number_are_told_apart() {
        let number = |p: Option<i64>, s: Option<i64>| {
            oracle_type_from_facts(&OracleColumnFacts {
                precision: p,
                scale: s,
                ..facts("NUMBER")
            })
        };
        assert_eq!(number(None, Some(0)), "INTEGER");
        assert_eq!(number(None, None), "NUMBER");
        assert_eq!(number(Some(10), Some(2)), "NUMBER(10,2)");
        assert_eq!(number(Some(9), Some(0)), "NUMBER(9)");
        assert_eq!(number(None, Some(-2)), "NUMBER(*,-2)");
    }

    /// VARCHAR2/CHAR restate their length semantics so the copy holds the
    /// same data whatever NLS_LENGTH_SEMANTICS the target session uses.
    #[test]
    fn oracle_character_types_keep_char_or_byte_semantics() {
        let text = |ty: &str, data_len: i64, char_len: i64, used: &str| {
            oracle_type_from_facts(&OracleColumnFacts {
                data_length: Some(data_len),
                char_length: Some(char_len),
                char_used: Some(used.into()),
                ..facts(ty)
            })
        };
        assert_eq!(text("VARCHAR2", 40, 10, "C"), "VARCHAR2(10 CHAR)");
        assert_eq!(text("VARCHAR2", 10, 10, "B"), "VARCHAR2(10 BYTE)");
        assert_eq!(text("CHAR", 12, 3, "C"), "CHAR(3 CHAR)");
        assert_eq!(text("NVARCHAR2", 10, 5, "C"), "NVARCHAR2(5)");
        assert_eq!(
            oracle_type_from_facts(&OracleColumnFacts {
                data_length: Some(16),
                ..facts("RAW")
            }),
            "RAW(16)"
        );
        assert_eq!(
            oracle_type_from_facts(&facts("TIMESTAMP(6) WITH TIME ZONE")),
            "TIMESTAMP(6) WITH TIME ZONE"
        );
        assert_eq!(
            oracle_type_from_facts(&OracleColumnFacts {
                precision: Some(126),
                ..facts("FLOAT")
            }),
            "FLOAT"
        );
    }

    #[test]
    fn oracle_column_rows_mark_virtual_columns_and_quote_names() {
        let sql = oracle_columns_sql("O'NEIL", "T");
        assert!(sql.contains("c.owner = 'O''NEIL'"), "{sql}");
        let loaded = oracle_columns_from_rows(vec![vec![
            s("TOTAL"),
            s("NUMBER"),
            CellValue::I64(22),
            CellValue::I64(0),
            CellValue::Null,
            CellValue::I64(12),
            CellValue::I64(2),
            CellValue::I64(1),
            CellValue::I64(0),
            CellValue::I64(1),
        ]]);
        assert_eq!(loaded.columns[0].data_type, "NUMBER(12,2)");
        assert!(loaded.extras["TOTAL"].generated.is_some());
    }

    #[test]
    fn tds_collation_is_kept_only_when_it_differs_from_the_database() {
        let db = Some("SQL_Latin1_General_CP1_CI_AS");
        assert_eq!(
            tds_explicit_collation(Some("Latin1_General_CS_AS".into()), db, false),
            Some("Latin1_General_CS_AS".into())
        );
        assert_eq!(
            tds_explicit_collation(Some("SQL_Latin1_General_CP1_CI_AS".into()), db, false),
            None
        );
        assert_eq!(tds_explicit_collation(None, db, false), None);
        assert_eq!(
            tds_explicit_collation(Some("Latin1_General_CS_AS".into()), db, true),
            None,
            "a computed column cannot declare a collation"
        );
    }

    fn storage_row(
        space: &str,
        space_type: &str,
        on_default: bool,
        compression: (&str, &str),
        temporal: i64,
        memory_optimized: bool,
        index_type: i64,
    ) -> Vec<CellValue> {
        vec![
            s(space),
            s(space_type),
            CellValue::I64(on_default as i64),
            s(compression.0),
            s(compression.1),
            CellValue::Null,
            CellValue::I64(1),
            CellValue::I64(temporal),
            if temporal == 2 {
                s("dbo")
            } else {
                CellValue::Null
            },
            if temporal == 2 {
                s("orders_history")
            } else {
                CellValue::Null
            },
            CellValue::I64(memory_optimized as i64),
            CellValue::I64(index_type),
        ]
    }

    #[test]
    fn tds_storage_restates_filegroup_and_compression() {
        let storage = tds_storage_from_row(
            storage_row("ARCHIVE", "FG", false, ("PAGE", "PAGE"), 0, false, 1),
            "dbo",
            "orders",
        );
        assert_eq!(storage.filegroup.as_deref(), Some("ARCHIVE"));
        assert_eq!(storage.data_compression.as_deref(), Some("PAGE"));
        assert!(storage.unscripted.is_empty());

        let plain = tds_storage_from_row(
            storage_row("PRIMARY", "FG", true, ("NONE", "NONE"), 0, false, 0),
            "dbo",
            "orders",
        );
        assert_eq!(plain, TableStorage::default());
    }

    #[test]
    fn tds_index_compression_restates_row_and_page_and_warns_otherwise() {
        let mut storage = TableStorage::default();
        tds_index_compression_from_rows(
            vec![
                vec![s("IX_page"), s("PAGE"), s("PAGE")],
                vec![s("IX_none"), s("NONE"), s("NONE")],
                vec![s("IX_mixed"), s("NONE"), s("ROW")],
                vec![s("NCCI_sales"), s("COLUMNSTORE"), s("COLUMNSTORE")],
            ],
            &mut storage,
        );
        assert_eq!(
            storage.index_compression,
            [("IX_page".to_string(), "PAGE".to_string())].into()
        );
        let all = storage
            .unscripted
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(" | ");
        assert!(
            all.contains("index IX_mixed's partitions use different compression"),
            "{all}"
        );
        assert!(
            all.contains("index NCCI_sales is a columnstore index"),
            "{all}"
        );
        assert!(!all.contains("IX_none"), "{all}");
    }

    #[test]
    fn tds_storage_warns_about_what_it_cannot_script() {
        let storage = tds_storage_from_row(
            storage_row("ps_by_year", "PS", false, ("NONE", "PAGE"), 2, true, 1),
            "dbo",
            "orders",
        );
        assert!(storage.filegroup.is_none());
        assert!(storage.data_compression.is_none());
        let all = storage
            .unscripted
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(" | ");
        assert!(all.contains("partitioned on scheme ps_by_year"), "{all}");
        assert!(
            all.contains("different compression (NONE to PAGE)"),
            "{all}"
        );
        assert!(all.contains("history table dbo.orders_history"), "{all}");
        assert!(all.contains("memory-optimized"), "{all}");

        let columnstore = tds_storage_from_row(
            storage_row(
                "PRIMARY",
                "FG",
                true,
                ("COLUMNSTORE", "COLUMNSTORE"),
                0,
                false,
                5,
            ),
            "dbo",
            "facts",
        );
        assert!(columnstore.data_compression.is_none());
        assert_eq!(
            columnstore.unscripted.len(),
            1,
            "{:?}",
            columnstore.unscripted
        );
        assert!(columnstore.unscripted[0].contains("clustered columnstore"));
    }

    #[test]
    fn check_rows_normalise_per_engine() {
        let pg = check_constraint_from_catalog(
            MetadataDialect::Postgres,
            "qty_ok".into(),
            "CHECK ((qty > 0)) NOT VALID",
            false,
        )
        .unwrap();
        assert_eq!(pg.name.as_deref(), Some("qty_ok"));
        assert_eq!(pg.expression, "((qty > 0))");
        assert!(
            check_constraint_from_catalog(
                MetadataDialect::Oracle,
                "SYS_C001".into(),
                "\"QTY\" IS NOT NULL",
                true,
            )
            .is_none()
        );
        let ora = check_constraint_from_catalog(
            MetadataDialect::Oracle,
            "SYS_C002".into(),
            "qty > 0",
            true,
        )
        .unwrap();
        assert_eq!(ora.name, None);
        assert_eq!(ora.expression, "(qty > 0)");
    }
}
