//! DDL document fetching for "go to definition" navigation.
//!
//! Given a locally-resolved [`NavigationTarget`] (see
//! `sqlly_intel::intellisense::resolve_symbol_at`), fetch or generate the object's
//! CREATE source asynchronously via the [`QueryExecutor`] port, dispatched on
//! the profile's metadata dialect:
//!
//! - **SQL Server routines/views**: `OBJECT_DEFINITION`, falling back to
//!   `sp_helptext` (rows concatenated in order). NULL from both means the
//!   object is encrypted, missing, or the caller lacks `VIEW DEFINITION` —
//!   reported as a controlled document error, never a panic.
//! - **PostgreSQL / MySQL / SQLite**: the dialect-dispatched
//!   `MetadataQuery::ObjectDefinition` reconstruction (`pg_get_viewdef` /
//!   `pg_get_functiondef`, `information_schema` views/routines/triggers,
//!   `sqlite_master.sql`), same honest-`None` contract.
//! - **Tables** without stored source: `CREATE TABLE` DDL is generated from
//!   catalog metadata (`sqlly_intel::metadata::ddl` on SQL Server; the table
//!   snapshot and Script as CREATE renderer on PostgreSQL and MySQL/MariaDB,
//!   so defaults, keys and indexes come along; the dialect `Columns` shape +
//!   `sqlly_intel::metadata::dialect_ddl` elsewhere), with stored index DDL
//!   appended where the engine exposes it.
//! - **User-defined types**: SQL Server alias/table types render CREATE TYPE
//!   forms; other engines report type definitions as unavailable.
//!
//! All identifier/literal embedding goes through `sqlly_intel::metadata`'s
//! `quote_name`/`escape_literal` — never raw clicked text.

use sqlly_core::{ConnectionProfile, StudioError, StudioResult};
use sqlly_intel::intellisense::{ClickedRole, NavigationTarget};
use sqlly_intel::metadata::MetadataQuery;
use sqlly_intel::metadata::ddl as ddl_sql;
use sqlly_intel::metadata::ddl::{PrimaryKeyDdl, TableDdlColumn};
use sqlly_intel::metadata::dialect::{MetadataDialect, sql_for_engine};
use sqlly_intel::metadata::dialect_ddl::{self, GenericDdlColumn};

use crate::metadata::{cell_to_bool, cell_to_i64, cell_to_string, profile_with_db};
use crate::query::{QueryEvent, QueryExecutor};

/// The metadata family the profile's engine speaks; engines without a native
/// metadata path keep the historical SQL Server behavior.
fn dialect_for(profile: &ConnectionProfile) -> MetadataDialect {
    MetadataDialect::for_database_type(profile.database_type).unwrap_or(MetadataDialect::SqlServer)
}

fn dialect_display(dialect: MetadataDialect) -> &'static str {
    match dialect {
        MetadataDialect::SqlServer => "SQL Server",
        MetadataDialect::Postgres => "PostgreSQL",
        MetadataDialect::MySql => "MySQL/MariaDB",
        MetadataDialect::Sqlite => "SQLite",
        MetadataDialect::DuckDb => "DuckDB",
        MetadataDialect::ClickHouse => "ClickHouse",
        MetadataDialect::Oracle => "Oracle Database",
    }
}

/// Where a [`DdlSource`] body came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DdlSourceKind {
    /// Fetched verbatim from the server (`OBJECT_DEFINITION` / `sp_helptext`).
    Server,
    /// Generated from catalog metadata (tables, types).
    Generated,
}

/// One retrievable representation of the target's DDL.
#[derive(Debug, Clone)]
pub struct DdlSource {
    pub kind: DdlSourceKind,
    /// Short label for the source ("Server definition", "Generated DDL").
    pub label: String,
    pub body: String,
}

/// The document shown by the DDL viewer. `error` is set for controlled
/// failures (encrypted object, missing permissions, unknown object) so the
/// UI renders an actionable message instead of an empty page.
#[derive(Debug, Clone)]
pub struct DdlDocument {
    /// Window/tab title, e.g. `DDL: dbo.Users`.
    pub title: String,
    pub target: NavigationTarget,
    pub sources: Vec<DdlSource>,
    /// Provenance: which connection and database served this document.
    pub connection_label: String,
    pub database: String,
    pub error: Option<String>,
}

impl DdlDocument {
    pub fn primary_source(&self) -> Option<&DdlSource> {
        self.sources.first()
    }
}

/// What a DDL document includes beyond the object's own definition.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DdlOptions {
    /// PostgreSQL: end the definition with a `-- Privileges` section of the
    /// GRANT / REVOKE statements that recreate the object's privileges,
    /// including REVOKEs of PostgreSQL's implicit defaults (see
    /// [`crate::pg_privileges`]). Ignored on other engines.
    pub include_privileges: bool,
}

/// Fetch (or generate) the DDL document for `target` on `profile`'s server,
/// in `database`. Transport-level failures return `Err`; domain-level
/// failures (no definition available) return `Ok` with `error` set so the
/// caller can present them in place. The definition alone; see
/// [`fetch_ddl_document_with`] for privileges.
pub async fn fetch_ddl_document(
    queries: &dyn QueryExecutor,
    profile: &ConnectionProfile,
    database: &str,
    target: &NavigationTarget,
) -> StudioResult<DdlDocument> {
    fetch_ddl_document_with(queries, profile, database, target, DdlOptions::default()).await
}

/// [`fetch_ddl_document`] with [`DdlOptions`].
pub async fn fetch_ddl_document_with(
    queries: &dyn QueryExecutor,
    profile: &ConnectionProfile,
    database: &str,
    target: &NavigationTarget,
    options: DdlOptions,
) -> StudioResult<DdlDocument> {
    let mut document = fetch_definition_document(queries, profile, database, target).await?;
    if options.include_privileges {
        append_pg_privileges(queries, profile, database, target, &mut document).await;
    }
    Ok(document)
}

/// Append the PostgreSQL privileges section to the document's definition.
/// Best effort like the other extras: a failed read becomes a one-line
/// comment, never a failed document.
async fn append_pg_privileges(
    queries: &dyn QueryExecutor,
    profile: &ConnectionProfile,
    database: &str,
    target: &NavigationTarget,
    document: &mut DdlDocument,
) {
    if dialect_for(profile) != MetadataDialect::Postgres
        || profile.database_type == sqlly_core::DatabaseType::CockroachDb
        || document.error.is_some()
    {
        return;
    }
    let object = match target {
        NavigationTarget::Table { schema, name, .. } => {
            crate::pg_privileges::PgAclObject::Relation {
                schema: schema.clone(),
                name: name.clone(),
            }
        }
        NavigationTarget::Routine { schema, name, .. } => {
            crate::pg_privileges::PgAclObject::Routine {
                schema: schema.clone(),
                name: name.clone(),
            }
        }
        NavigationTarget::Type { schema, name, .. } => crate::pg_privileges::PgAclObject::Type {
            schema: schema.clone(),
            name: name.clone(),
        },
    };
    let Some(source) = document.sources.first_mut() else {
        return;
    };
    match crate::pg_privileges::fetch_privilege_statements(queries, profile, database, &object)
        .await
    {
        Ok(statements) if !statements.is_empty() => {
            source.body.truncate(source.body.trim_end().len());
            source.body.push_str("\n\n-- Privileges\n");
            source.body.push_str(&statements.join("\n"));
        }
        Ok(_) => {}
        Err(error) => {
            source.body.truncate(source.body.trim_end().len());
            source.body.push_str(&format!(
                "\n\n-- Privileges unavailable: {}",
                comment_safe(&error.to_string())
            ));
        }
    }
}

async fn fetch_definition_document(
    queries: &dyn QueryExecutor,
    profile: &ConnectionProfile,
    database: &str,
    target: &NavigationTarget,
) -> StudioResult<DdlDocument> {
    let schema = target.schema().to_string();
    let name = target.name().to_string();
    let mut document = DdlDocument {
        title: format!("DDL: {}.{}", schema, name),
        target: target.clone(),
        sources: Vec::new(),
        connection_label: profile.name.clone(),
        database: database.to_string(),
        error: None,
    };

    let dialect = dialect_for(profile);

    match target {
        NavigationTarget::Routine { .. } => {
            let source = match dialect {
                MetadataDialect::SqlServer => {
                    fetch_object_source(queries, profile, database, &schema, &name).await?
                }
                other => {
                    fetch_dialect_definition(queries, profile, database, other, &schema, &name)
                        .await?
                }
            };
            match source {
                Some(mut body) => document.sources.push(DdlSource {
                    kind: DdlSourceKind::Server,
                    label: "Server definition".to_string(),
                    body: {
                        terminate_statement(dialect, &mut body);
                        body
                    },
                }),
                None => {
                    document.error = Some(format!(
                        "No definition available for {schema}.{name}. The object may be \
                         encrypted, dropped, or you may lack permission to read its source."
                    ));
                }
            }
        }
        NavigationTarget::Table { clicked, .. } => {
            // Views (including view columns reached through a click) have a
            // real server definition; tables without stored source fall
            // through to generated DDL. On SQLite the stored source covers
            // tables too, so the generated path is a rarely-hit fallback.
            let server_definition = match dialect {
                MetadataDialect::SqlServer => {
                    fetch_object_definition(queries, profile, database, &schema, &name).await?
                }
                other => {
                    fetch_dialect_definition(queries, profile, database, other, &schema, &name)
                        .await?
                }
            };
            match server_definition {
                Some(mut body) => {
                    terminate_statement(dialect, &mut body);
                    if matches!(dialect, MetadataDialect::Sqlite | MetadataDialect::DuckDb) {
                        // The stored CREATE TABLE omits the table's named
                        // indexes; surface them the way the generated path does.
                        append_dialect_index_extras(
                            queries, profile, database, dialect, &schema, &name, &mut body,
                        )
                        .await?;
                    }
                    document.sources.push(DdlSource {
                        kind: DdlSourceKind::Server,
                        label: "Server definition".to_string(),
                        body,
                    });
                }
                None if dialect == MetadataDialect::SqlServer => {
                    let columns =
                        fetch_table_columns(queries, profile, database, &schema, &name).await?;
                    if columns.is_empty() {
                        document.error = Some(format!(
                            "No metadata found for {schema}.{name}. The object may have been \
                             dropped or you may lack permission to read its catalog entries."
                        ));
                    } else {
                        let primary_key =
                            fetch_primary_key(queries, profile, database, &schema, &name).await?;
                        let mut body = ddl_sql::build_create_table_ddl(
                            &schema,
                            &name,
                            &columns,
                            primary_key.as_ref(),
                        );
                        body.push(';');
                        append_table_extras(queries, profile, database, &schema, &name, &mut body)
                            .await?;
                        document.sources.push(DdlSource {
                            kind: DdlSourceKind::Generated,
                            label: "Generated from catalog metadata".to_string(),
                            body,
                        });
                    }
                }
                None if matches!(dialect, MetadataDialect::Postgres | MetadataDialect::MySql) => {
                    fetch_snapshot_table_document(
                        queries,
                        profile,
                        database,
                        &schema,
                        &name,
                        &mut document,
                    )
                    .await?;
                }
                None => {
                    let columns = fetch_generic_table_columns(
                        queries, profile, database, dialect, &schema, &name,
                    )
                    .await?;
                    if columns.is_empty() {
                        document.error = Some(format!(
                            "No metadata found for {schema}.{name}. The object may have been \
                             dropped or you may lack permission to read its catalog entries."
                        ));
                    } else {
                        let ddl_schema = match dialect {
                            MetadataDialect::Sqlite => None,
                            _ => Some(schema.as_str()),
                        };
                        let mut body = dialect_ddl::build_generic_create_table_ddl(
                            profile.database_type,
                            ddl_schema,
                            &name,
                            &columns,
                        );
                        body.push(';');
                        append_dialect_index_extras(
                            queries, profile, database, dialect, &schema, &name, &mut body,
                        )
                        .await?;
                        document.sources.push(DdlSource {
                            kind: DdlSourceKind::Generated,
                            label: "Generated from catalog metadata".to_string(),
                            body,
                        });
                    }
                }
            }
            if let ClickedRole::Column { name: column } = clicked {
                document.title = format!("DDL: {}.{} · column {}", schema, name, column);
            }
        }
        NavigationTarget::Type { .. } => match dialect {
            MetadataDialect::SqlServer => {
                fetch_type_document(queries, profile, database, &schema, &name, &mut document)
                    .await?;
            }
            MetadataDialect::DuckDb => {
                let sql = dialect_ddl::duckdb_type_ddl_sql(&schema, &name);
                match fetch_ddl_statements(queries, profile, database, &sql)
                    .await?
                    .into_iter()
                    .next()
                {
                    Some(body) => document.sources.push(DdlSource {
                        kind: DdlSourceKind::Server,
                        label: "Server definition".to_string(),
                        body,
                    }),
                    None => {
                        document.error = Some(format!(
                            "Type {schema}.{name} was not found in the database's user-defined types."
                        ));
                    }
                }
            }
            other => {
                document.error = Some(format!(
                    "User-defined type definitions are not available on {}.",
                    dialect_display(other)
                ));
            }
        },
    }

    Ok(document)
}

/// PostgreSQL and MySQL/MariaDB tables: the CREATE TABLE built from the
/// same table snapshot as Script as CREATE, so it carries every column
/// default (a shared-sequence `nextval` included), the primary key, foreign
/// keys, indexes, CHECK constraints and descriptions; on MySQL/MariaDB it is
/// the server's own `SHOW CREATE TABLE` (exact `unsigned` types, engine,
/// charset) minus the live AUTO_INCREMENT counter. Anything the renderer
/// could not restate is listed as a leading `-- WARNING:` comment.
async fn fetch_snapshot_table_document(
    queries: &dyn QueryExecutor,
    profile: &ConnectionProfile,
    database: &str,
    schema: &str,
    name: &str,
    document: &mut DdlDocument,
) -> StudioResult<()> {
    let snapshot_schema = Some(schema).filter(|s| !s.is_empty());
    let snapshot = crate::table_snapshot::load_table_snapshot(
        queries,
        profile,
        database,
        snapshot_schema,
        name,
    )
    .await?;
    if snapshot.columns.is_empty() {
        document.error = Some(format!(
            "No metadata found for {schema}.{name}. The object may have been \
             dropped or you may lack permission to read its catalog entries."
        ));
        return Ok(());
    }
    let rendered = crate::alter::render_create_tables(
        profile.database_type,
        std::slice::from_ref(&snapshot),
        &std::collections::HashSet::new(),
    );
    let mut body = String::new();
    for warning in &rendered.warnings {
        body.push_str(&format!("-- WARNING: {}\n", comment_safe(warning)));
    }
    body.push_str(rendered.sql.trim_end());
    document.sources.push(DdlSource {
        kind: DdlSourceKind::Generated,
        label: "Generated from catalog metadata".to_string(),
        body,
    });
    Ok(())
}

/// End a stored definition with `;` unless it already ends with one, so
/// definitions written back to back (a dump, a multi-object script, the stored
/// CREATE TABLE followed by its index DDL) stay separate statements. Catalogs
/// hand the source back unterminated (`sqlite_master.sql`,
/// `pg_get_functiondef`, `information_schema` bodies). SQL Server batches are
/// separated with `GO` instead, and Oracle PL/SQL keeps its own terminators.
fn terminate_statement(dialect: MetadataDialect, body: &mut String) {
    if matches!(
        dialect,
        MetadataDialect::SqlServer | MetadataDialect::Oracle
    ) {
        return;
    }
    let trimmed = body.trim_end().len();
    body.truncate(trimmed);
    if body.ends_with(';') {
        return;
    }
    // SQLite keeps a view's trailing `-- note` in sqlite_master.sql (MySQL a
    // `# note`); a `;` on that line is part of the comment, so it goes on
    // its own.
    let lexer_dialect = match dialect {
        MetadataDialect::MySql => sqlly_lang::sqlmodel::Dialect::MySql,
        MetadataDialect::Sqlite => sqlly_lang::sqlmodel::Dialect::Sqlite,
        _ => sqlly_lang::sqlmodel::Dialect::Postgres,
    };
    if crate::dump_render::ends_with_line_comment(lexer_dialect, body) {
        body.push('\n');
    }
    body.push(';');
}

// ---------------------------------------------------------------------------
// Server fetch helpers
// ---------------------------------------------------------------------------

/// Run `sql` and collect all rows. Mirrors
/// `DefaultMetadataService::collect_rows` but works against the raw
/// executor port so the DDL path needs no service handle.
pub(crate) async fn collect_rows(
    queries: &dyn QueryExecutor,
    profile: ConnectionProfile,
    sql: &str,
) -> StudioResult<Vec<Vec<sqlly_core::CellValue>>> {
    // Full values, not grid previews: the streaming `execute` path cuts
    // every text cell to an 8 KiB preview for the grid, which silently
    // truncated any stored procedure, view, or trigger source longer than
    // that. Definitions are written into an editor whole, so fetch them whole.
    let handle = queries
        .execute_full_values(&profile, None, sql, 1000)
        .await?;
    let mut events = handle.events;
    let mut all_rows = Vec::new();
    while let Some(event) = events.recv().await {
        match event {
            QueryEvent::Rows { rows, .. } => all_rows.extend(rows),
            QueryEvent::Error { message } => return Err(StudioError::Connection(message)),
            QueryEvent::Completed { .. } | QueryEvent::Cancelled => break,
            _ => {}
        }
    }
    Ok(all_rows)
}

/// `OBJECT_DEFINITION` for one object — `Ok(None)` when NULL/absent.
async fn fetch_object_definition(
    queries: &dyn QueryExecutor,
    profile: &ConnectionProfile,
    database: &str,
    schema: &str,
    name: &str,
) -> StudioResult<Option<String>> {
    let sql = ddl_sql::object_definition_sql(schema, name);
    let rows = collect_rows(queries, profile_with_db(profile, database), &sql).await?;
    Ok(rows
        .into_iter()
        .next()
        .and_then(|row| cell_to_string(row.into_iter().next()))
        .filter(|s| !s.trim().is_empty()))
}

/// `sp_helptext` fallback — rows are ordered fragments of the source text
/// and must be concatenated verbatim (they carry their own line breaks).
async fn fetch_helptext(
    queries: &dyn QueryExecutor,
    profile: &ConnectionProfile,
    database: &str,
    schema: &str,
    name: &str,
) -> StudioResult<Option<String>> {
    let sql = ddl_sql::sp_helptext_sql(schema, name);
    // sp_helptext raises an error for unknown objects; treat that as "no
    // source" rather than a transport failure.
    let rows = match collect_rows(queries, profile_with_db(profile, database), &sql).await {
        Ok(rows) => rows,
        Err(StudioError::Connection(_)) => return Ok(None),
        Err(e) => return Err(e),
    };
    let text: String = rows
        .into_iter()
        .filter_map(|row| cell_to_string(row.into_iter().next()))
        .collect();
    Ok(if text.trim().is_empty() {
        None
    } else {
        Some(text)
    })
}

/// Dialect-dispatched definition fetch: one column, first row, honest `None`
/// when the engine has no stored source for the object (or withholds it).
async fn fetch_dialect_definition(
    queries: &dyn QueryExecutor,
    profile: &ConnectionProfile,
    database: &str,
    _dialect: MetadataDialect,
    schema: &str,
    name: &str,
) -> StudioResult<Option<String>> {
    let query = MetadataQuery::ObjectDefinition {
        schema: schema.to_string(),
        object: name.to_string(),
    };
    let Ok(sql) = sql_for_engine(&query, profile.database_type) else {
        return Ok(None);
    };
    let rows = collect_rows(queries, profile_with_db(profile, database), &sql).await?;
    Ok(rows
        .into_iter()
        .next()
        .and_then(|row| cell_to_string(row.into_iter().next()))
        .filter(|s| !s.trim().is_empty()))
}

/// Table columns via the dialect-dispatched `Columns` query (name, type,
/// max_length, precision, scale, nullable, identity, computed — positionally).
async fn fetch_generic_table_columns(
    queries: &dyn QueryExecutor,
    profile: &ConnectionProfile,
    database: &str,
    _dialect: MetadataDialect,
    schema: &str,
    name: &str,
) -> StudioResult<Vec<GenericDdlColumn>> {
    let query = MetadataQuery::Columns {
        schema: schema.to_string(),
        object: name.to_string(),
    };
    let Ok(sql) = sql_for_engine(&query, profile.database_type) else {
        return Ok(Vec::new());
    };
    let rows = collect_rows(queries, profile_with_db(profile, database), &sql).await?;
    Ok(rows
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
            Some(GenericDdlColumn {
                name,
                type_name,
                max_length,
                precision,
                scale,
                nullable,
                is_identity,
                is_computed,
            })
        })
        .collect())
}

/// Append a `-- Indexes` section from the engine's stored index DDL
/// (PostgreSQL `pg_indexes.indexdef`, SQLite `sqlite_master`, DuckDB
/// `duckdb_indexes()`). Best-effort like the SQL Server extras: a failed
/// section degrades to a comment.
async fn append_dialect_index_extras(
    queries: &dyn QueryExecutor,
    profile: &ConnectionProfile,
    database: &str,
    dialect: MetadataDialect,
    schema: &str,
    name: &str,
    body: &mut String,
) -> StudioResult<()> {
    let sql = match dialect {
        MetadataDialect::Postgres => dialect_ddl::pg_table_indexes_ddl_sql(schema, name),
        MetadataDialect::Sqlite => dialect_ddl::sqlite_table_indexes_ddl_sql(name),
        MetadataDialect::DuckDb => dialect_ddl::duckdb_table_indexes_ddl_sql(schema, name),
        _ => return Ok(()),
    };
    match fetch_ddl_statements(queries, profile, database, &sql).await {
        Ok(indexes) if !indexes.is_empty() => {
            body.push_str("\n\n-- Indexes\n");
            body.push_str(&indexes.join("\n"));
        }
        Ok(_) => {}
        Err(error) => {
            body.push_str(&format!(
                "\n\n-- Indexes unavailable: {}",
                comment_safe(&error.to_string())
            ));
        }
    }
    Ok(())
}

/// Definition via `OBJECT_DEFINITION`, then `sp_helptext`.
async fn fetch_object_source(
    queries: &dyn QueryExecutor,
    profile: &ConnectionProfile,
    database: &str,
    schema: &str,
    name: &str,
) -> StudioResult<Option<String>> {
    if let Some(text) = fetch_object_definition(queries, profile, database, schema, name).await? {
        return Ok(Some(text));
    }
    fetch_helptext(queries, profile, database, schema, name).await
}

async fn fetch_table_columns(
    queries: &dyn QueryExecutor,
    profile: &ConnectionProfile,
    database: &str,
    schema: &str,
    name: &str,
) -> StudioResult<Vec<TableDdlColumn>> {
    let sql = ddl_sql::table_ddl_columns_sql(schema, name);
    let rows = collect_rows(queries, profile_with_db(profile, database), &sql).await?;
    Ok(rows
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
            let identity_seed = cell_to_i64(cells.next());
            let identity_increment = cell_to_i64(cells.next());
            let is_computed = cell_to_bool(cells.next()).unwrap_or(false);
            let computed_definition = cell_to_string(cells.next());
            let default_definition = cell_to_string(cells.next());
            let collation = cell_to_string(cells.next());
            Some(TableDdlColumn {
                name,
                type_name,
                max_length,
                precision,
                scale,
                nullable,
                is_identity,
                identity_seed,
                identity_increment,
                is_computed,
                computed_definition,
                default_definition,
                collation,
            })
        })
        .collect())
}

async fn fetch_primary_key(
    queries: &dyn QueryExecutor,
    profile: &ConnectionProfile,
    database: &str,
    schema: &str,
    name: &str,
) -> StudioResult<Option<PrimaryKeyDdl>> {
    let sql = ddl_sql::table_primary_key_sql(schema, name);
    let rows = collect_rows(queries, profile_with_db(profile, database), &sql).await?;
    let mut pk: Option<PrimaryKeyDdl> = None;
    for row in rows {
        let mut cells = row.into_iter();
        let Some(constraint_name) = cell_to_string(cells.next()) else {
            continue;
        };
        let type_desc = cell_to_string(cells.next()).unwrap_or_default();
        let Some(column_name) = cell_to_string(cells.next()) else {
            continue;
        };
        let descending = cell_to_bool(cells.next()).unwrap_or(false);
        let entry = pk.get_or_insert_with(|| PrimaryKeyDdl {
            constraint_name,
            clustered: type_desc.eq_ignore_ascii_case("CLUSTERED"),
            columns: Vec::new(),
        });
        entry.columns.push((column_name, descending));
    }
    Ok(pk)
}

/// Flatten a diagnostic string into a single physical line so it is safe to
/// embed after a `--` comment marker. A raw server/error message can contain
/// `\r`/`\n`; without this every line after the first would escape the comment
/// and become executable text in the assembled script. Runs of whitespace
/// (including the CR/LF we replace) collapse to a single space.
fn comment_safe(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last_was_space = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            if !last_was_space {
                out.push(' ');
                last_was_space = true;
            }
        } else {
            out.push(ch);
            last_was_space = false;
        }
    }
    out.trim().to_string()
}

/// Append `-- Indexes`, `-- Foreign Keys`, and `-- Triggers` sections to a
/// generated `CREATE TABLE` body. Each section is omitted when empty. The index
/// and FK DDL is rendered server-side (one statement per row); trigger sources
/// come from `OBJECT_DEFINITION` and are separated with `GO`.
async fn append_table_extras(
    queries: &dyn QueryExecutor,
    profile: &ConnectionProfile,
    database: &str,
    schema: &str,
    name: &str,
    body: &mut String,
) -> StudioResult<()> {
    // Each section is best-effort: a low-privilege user (e.g. no
    // `VIEW DEFINITION`) who could previously read the plain CREATE TABLE must
    // still get one. On a section query error we append a `-- … unavailable`
    // note and keep going instead of failing the whole document.
    match fetch_ddl_statements(
        queries,
        profile,
        database,
        &ddl_sql::table_indexes_ddl_sql(schema, name),
    )
    .await
    {
        Ok(indexes) if !indexes.is_empty() => {
            body.push_str("\n\n-- Indexes\n");
            body.push_str(&indexes.join("\n"));
        }
        Ok(_) => {}
        Err(error) => {
            body.push_str(&format!(
                "\n\n-- Indexes unavailable: {}",
                comment_safe(&error.to_string())
            ));
        }
    }

    match fetch_ddl_statements(
        queries,
        profile,
        database,
        &ddl_sql::table_foreign_keys_ddl_sql(schema, name),
    )
    .await
    {
        Ok(foreign_keys) if !foreign_keys.is_empty() => {
            body.push_str("\n\n-- Foreign Keys\n");
            body.push_str(&foreign_keys.join("\n"));
        }
        Ok(_) => {}
        Err(error) => {
            body.push_str(&format!(
                "\n\n-- Foreign Keys unavailable: {}",
                comment_safe(&error.to_string())
            ));
        }
    }

    match fetch_trigger_definitions(
        queries,
        profile,
        database,
        &ddl_sql::table_triggers_definition_sql(schema, name),
    )
    .await
    {
        Ok(triggers) if !triggers.is_empty() => {
            // `CREATE TRIGGER` must be the first statement of its batch, so a
            // `GO` separates the triggers from the CREATE TABLE / indexes / FKs
            // above (and each trigger from the next) — otherwise the assembled
            // script fails to execute.
            body.push_str("\n\n-- Triggers\nGO\n");
            for (i, (trigger_name, definition)) in triggers.iter().enumerate() {
                if i > 0 {
                    body.push_str("\nGO\n");
                }
                match definition {
                    Some(text) => body.push_str(text.trim_end()),
                    None => body.push_str(&format!(
                        "-- {trigger_name}: definition unavailable (encrypted or not readable)"
                    )),
                }
            }
        }
        Ok(_) => {}
        Err(error) => {
            body.push_str(&format!(
                "\n\n-- Triggers unavailable: {}",
                comment_safe(&error.to_string())
            ));
        }
    }

    Ok(())
}

/// Run a query whose first column is a rendered DDL statement per row; collect
/// the non-empty ones in order.
async fn fetch_ddl_statements(
    queries: &dyn QueryExecutor,
    profile: &ConnectionProfile,
    database: &str,
    sql: &str,
) -> StudioResult<Vec<String>> {
    let rows = collect_rows(queries, profile_with_db(profile, database), sql).await?;
    Ok(rows
        .into_iter()
        .filter_map(|row| cell_to_string(row.into_iter().next()))
        .filter(|s| !s.trim().is_empty())
        .collect())
}

/// Run the trigger query, returning `(name, definition)` pairs. The definition
/// is `None` when `OBJECT_DEFINITION` is NULL (encrypted / unreadable).
async fn fetch_trigger_definitions(
    queries: &dyn QueryExecutor,
    profile: &ConnectionProfile,
    database: &str,
    sql: &str,
) -> StudioResult<Vec<(String, Option<String>)>> {
    let rows = collect_rows(queries, profile_with_db(profile, database), sql).await?;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let mut cells = row.into_iter();
            let name = cell_to_string(cells.next())?;
            let definition = cell_to_string(cells.next()).filter(|s| !s.trim().is_empty());
            Some((name, definition))
        })
        .collect())
}

async fn fetch_type_document(
    queries: &dyn QueryExecutor,
    profile: &ConnectionProfile,
    database: &str,
    schema: &str,
    name: &str,
    document: &mut DdlDocument,
) -> StudioResult<()> {
    let sql = ddl_sql::type_definition_sql(schema, name);
    let rows = collect_rows(queries, profile_with_db(profile, database), &sql).await?;
    let Some(row) = rows.into_iter().next() else {
        document.error = Some(format!(
            "Type {schema}.{name} was not found in the database's user-defined types."
        ));
        return Ok(());
    };

    let mut cells = row.into_iter();
    let _type_name = cell_to_string(cells.next());
    let base_type = cell_to_string(cells.next()).unwrap_or_default();
    let max_length = cell_to_i64(cells.next());
    let precision = cell_to_i64(cells.next());
    let scale = cell_to_i64(cells.next());
    let nullable = cell_to_bool(cells.next()).unwrap_or(true);
    let is_table_type = cell_to_bool(cells.next()).unwrap_or(false);
    let is_assembly_type = cell_to_bool(cells.next()).unwrap_or(false);

    if is_table_type {
        let cols_sql = ddl_sql::table_type_columns_sql(schema, name);
        let col_rows = collect_rows(queries, profile_with_db(profile, database), &cols_sql).await?;
        let columns: Vec<TableDdlColumn> = col_rows
            .into_iter()
            .filter_map(|row| {
                let mut cells = row.into_iter();
                Some(TableDdlColumn {
                    name: cell_to_string(cells.next())?,
                    type_name: cell_to_string(cells.next()).unwrap_or_default(),
                    max_length: cell_to_i64(cells.next()),
                    precision: cell_to_i64(cells.next()),
                    scale: cell_to_i64(cells.next()),
                    nullable: cell_to_bool(cells.next()).unwrap_or(false),
                    ..TableDdlColumn::default()
                })
            })
            .collect();
        document.sources.push(DdlSource {
            kind: DdlSourceKind::Generated,
            label: "Generated from catalog metadata".to_string(),
            body: ddl_sql::build_table_type_ddl(schema, name, &columns),
        });
    } else if is_assembly_type {
        // CLR type bodies live in the assembly; exact DDL can't be rebuilt
        // from the catalog.
        document.sources.push(DdlSource {
            kind: DdlSourceKind::Generated,
            label: "Catalog description".to_string(),
            body: format!(
                "-- {schema}.{name} is a CLR (assembly) type; its definition cannot be \
                 reconstructed from catalog metadata.\n-- Base: {base_type}"
            ),
        });
    } else {
        document.sources.push(DdlSource {
            kind: DdlSourceKind::Generated,
            label: "Generated from catalog metadata".to_string(),
            body: ddl_sql::build_alias_type_ddl(
                schema, name, &base_type, max_length, precision, scale, nullable,
            ),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use sqlly_core::{CellValue, SecretProvider, SecretRef};
    use std::sync::Mutex;
    use tokio::sync::mpsc;

    /// Canned route: a substring to match and the rows (or error) it returns.
    type FakeRoute = (&'static str, Result<Vec<Vec<CellValue>>, String>);

    /// Fake executor: routes each SQL string to canned rows by substring
    /// match, recording executed SQL for assertions.
    struct FakeQueries {
        routes: Vec<FakeRoute>,
        executed: Mutex<Vec<String>>,
    }

    impl FakeQueries {
        fn new(routes: Vec<FakeRoute>) -> Self {
            Self {
                routes,
                executed: Mutex::new(Vec::new()),
            }
        }
    }

    impl FakeQueries {
        async fn execute_whole(
            &self,
            _profile: &ConnectionProfile,
            sql: &str,
            _row_buffer: usize,
        ) -> StudioResult<crate::query::QueryHandle> {
            self.executed.lock().unwrap().push(sql.to_string());
            let (tx, rx) = mpsc::channel(16);
            let route = self
                .routes
                .iter()
                .find(|(needle, _)| sql.contains(needle))
                .map(|(_, result)| result.clone());
            match route {
                Some(Ok(rows)) => {
                    tx.send(QueryEvent::Rows { index: 0, rows }).await.ok();
                    tx.send(QueryEvent::Completed { elapsed_ms: 1 }).await.ok();
                }
                Some(Err(message)) => {
                    tx.send(QueryEvent::Error { message }).await.ok();
                }
                None => {
                    tx.send(QueryEvent::Completed { elapsed_ms: 1 }).await.ok();
                }
            }
            Ok(crate::query::QueryHandle {
                task_id: "t".into(),
                events: rx,
            })
        }
    }

    #[async_trait]
    impl QueryExecutor for FakeQueries {
        /// Mirror the real executor: the grid path cuts text cells to an
        /// 8 KiB preview, and only the full-values path delivers them whole.
        async fn execute(
            &self,
            profile: &ConnectionProfile,
            sql: &str,
            row_buffer: usize,
        ) -> StudioResult<crate::query::QueryHandle> {
            let handle = self.execute_whole(profile, sql, row_buffer).await?;
            let (tx, rx) = mpsc::channel(16);
            let mut events = handle.events;
            tokio::spawn(async move {
                while let Some(event) = events.recv().await {
                    let event = match event {
                        QueryEvent::Rows { index, mut rows } => {
                            let limits = crate::query::CellLimits::default();
                            crate::query::truncate_rows(&mut rows, 0, &limits);
                            QueryEvent::Rows { index, rows }
                        }
                        other => other,
                    };
                    if tx.send(event).await.is_err() {
                        break;
                    }
                }
            });
            Ok(crate::query::QueryHandle {
                task_id: "t".into(),
                events: rx,
            })
        }

        async fn execute_full_values(
            &self,
            profile: &ConnectionProfile,
            _session: Option<&str>,
            sql: &str,
            row_buffer: usize,
        ) -> StudioResult<crate::query::QueryHandle> {
            self.execute_whole(profile, sql, row_buffer).await
        }

        async fn cancel(&self, _task_id: &str) -> StudioResult<()> {
            Ok(())
        }
    }

    fn profile() -> ConnectionProfile {
        ConnectionProfile::sql_login(
            "test-conn",
            "localhost",
            "sa",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: "k".into(),
            },
        )
    }

    fn s(text: &str) -> CellValue {
        CellValue::String(text.to_string())
    }

    fn routine_target(name: &str) -> NavigationTarget {
        NavigationTarget::Routine {
            catalog: None,
            schema: "dbo".into(),
            name: name.into(),
            kind: sqlly_intel::intellisense::RoutineKind::Procedure,
            clicked: sqlly_intel::intellisense::ClickedRole::Procedure,
        }
    }

    fn table_target(name: &str, clicked: ClickedRole) -> NavigationTarget {
        NavigationTarget::Table {
            catalog: None,
            schema: "dbo".into(),
            name: name.into(),
            clicked,
        }
    }

    /// A stored procedure longer than the grid's 8 KiB cell preview must
    /// arrive whole: the DDL fetch used the preview-cutting `execute` path,
    /// so long procedure/view/trigger sources came back silently truncated.
    #[tokio::test]
    async fn routine_definition_longer_than_cell_preview_is_not_truncated() {
        let header = "/* ".to_string() + &"x".repeat(20_000) + " */\n";
        let body = format!("{header}CREATE PROCEDURE dbo.Long AS SELECT 1 -- END");
        let queries = FakeQueries::new(vec![("OBJECT_DEFINITION", Ok(vec![vec![s(&body)]]))]);
        let doc = fetch_ddl_document(&queries, &profile(), "appdb", &routine_target("Long"))
            .await
            .unwrap();
        let source = doc.primary_source().unwrap();
        assert_eq!(source.body.len(), body.len());
        assert!(source.body.ends_with("-- END"));
    }

    #[tokio::test]
    async fn routine_uses_object_definition() {
        let queries = FakeQueries::new(vec![(
            "OBJECT_DEFINITION",
            Ok(vec![vec![s("CREATE PROCEDURE dbo.GetUser AS SELECT 1")]]),
        )]);
        let doc = fetch_ddl_document(&queries, &profile(), "appdb", &routine_target("GetUser"))
            .await
            .unwrap();
        assert!(doc.error.is_none());
        let source = doc.primary_source().unwrap();
        assert_eq!(source.kind, DdlSourceKind::Server);
        assert!(source.body.contains("CREATE PROCEDURE dbo.GetUser"));
        assert_eq!(doc.title, "DDL: dbo.GetUser");
        assert_eq!(doc.connection_label, "test-conn");
        assert_eq!(doc.database, "appdb");
    }

    #[tokio::test]
    async fn routine_falls_back_to_sp_helptext_in_row_order() {
        let queries = FakeQueries::new(vec![
            ("OBJECT_DEFINITION", Ok(vec![vec![CellValue::Null]])),
            (
                "sp_helptext",
                Ok(vec![
                    vec![s("CREATE PROCEDURE dbo.GetUser\n")],
                    vec![s("AS\n")],
                    vec![s("SELECT 1\n")],
                ]),
            ),
        ]);
        let doc = fetch_ddl_document(&queries, &profile(), "appdb", &routine_target("GetUser"))
            .await
            .unwrap();
        let source = doc.primary_source().unwrap();
        assert_eq!(source.body, "CREATE PROCEDURE dbo.GetUser\nAS\nSELECT 1\n");
    }

    #[tokio::test]
    async fn encrypted_routine_reports_controlled_error() {
        let queries = FakeQueries::new(vec![
            ("OBJECT_DEFINITION", Ok(vec![vec![CellValue::Null]])),
            (
                "sp_helptext",
                Err("The text for object is encrypted.".into()),
            ),
        ]);
        let doc = fetch_ddl_document(&queries, &profile(), "appdb", &routine_target("Secret"))
            .await
            .unwrap();
        assert!(doc.sources.is_empty());
        let error = doc.error.unwrap();
        assert!(error.contains("encrypted"), "got: {error}");
    }

    #[tokio::test]
    async fn table_generates_create_table_from_metadata() {
        let queries = FakeQueries::new(vec![
            ("OBJECT_DEFINITION", Ok(vec![vec![CellValue::Null]])),
            (
                "FROM sys.columns c",
                Ok(vec![
                    vec![
                        s("UserId"),
                        s("int"),
                        CellValue::I64(4),
                        CellValue::I64(10),
                        CellValue::I64(0),
                        CellValue::Bool(false), // nullable
                        CellValue::Bool(true),  // identity
                        CellValue::I64(1),
                        CellValue::I64(1),
                        CellValue::Bool(false), // computed
                        CellValue::Null,
                        CellValue::Null,
                        CellValue::Null,
                    ],
                    vec![
                        s("Name"),
                        s("nvarchar"),
                        CellValue::I64(200),
                        CellValue::I64(0),
                        CellValue::I64(0),
                        CellValue::Bool(true),
                        CellValue::Bool(false),
                        CellValue::Null,
                        CellValue::Null,
                        CellValue::Bool(false),
                        CellValue::Null,
                        CellValue::Null,
                        CellValue::Null,
                    ],
                ]),
            ),
            (
                "sys.key_constraints",
                Ok(vec![vec![
                    s("PK_Users"),
                    s("CLUSTERED"),
                    s("UserId"),
                    CellValue::Bool(false),
                ]]),
            ),
        ]);
        let doc = fetch_ddl_document(
            &queries,
            &profile(),
            "appdb",
            &table_target("Users", ClickedRole::Table),
        )
        .await
        .unwrap();
        assert!(doc.error.is_none());
        let source = doc.primary_source().unwrap();
        assert_eq!(source.kind, DdlSourceKind::Generated);
        assert!(source.body.contains("CREATE TABLE [dbo].[Users]"));
        assert!(source.body.contains("[UserId] int IDENTITY(1,1) NOT NULL"));
        assert!(source.body.contains("[Name] nvarchar(100) NULL"));
        assert!(
            source
                .body
                .contains("CONSTRAINT [PK_Users] PRIMARY KEY CLUSTERED ([UserId])")
        );
    }

    #[tokio::test]
    async fn table_ddl_appends_indexes_foreign_keys_and_triggers() {
        // Route order matters: the trigger query embeds OBJECT_DEFINITION and
        // the PK query embeds sys.indexes, so the more specific needles come
        // first (FakeQueries picks the first substring match).
        let queries = FakeQueries::new(vec![
            (
                "sys.triggers tr",
                Ok(vec![vec![
                    s("trg_Users_Audit"),
                    s("CREATE TRIGGER dbo.trg_Users_Audit ON dbo.Users AFTER INSERT AS SELECT 1"),
                ]]),
            ),
            (
                "sys.key_constraints",
                Ok(vec![vec![
                    s("PK_Users"),
                    s("CLUSTERED"),
                    s("UserId"),
                    CellValue::Bool(false),
                ]]),
            ),
            (
                "sys.foreign_keys fk",
                Ok(vec![vec![s(
                    "ALTER TABLE [dbo].[Users] ADD CONSTRAINT [FK_Users_Org] FOREIGN KEY ([OrgId]) REFERENCES [dbo].[Orgs] ([OrgId]);",
                )]]),
            ),
            (
                "sys.indexes i",
                Ok(vec![vec![s(
                    "CREATE NONCLUSTERED INDEX [IX_Users_Name] ON [dbo].[Users] ([Name] ASC);",
                )]]),
            ),
            (
                "FROM sys.columns c",
                Ok(vec![vec![
                    s("UserId"),
                    s("int"),
                    CellValue::I64(4),
                    CellValue::I64(10),
                    CellValue::I64(0),
                    CellValue::Bool(false),
                    CellValue::Bool(true),
                    CellValue::I64(1),
                    CellValue::I64(1),
                    CellValue::Bool(false),
                    CellValue::Null,
                    CellValue::Null,
                    CellValue::Null,
                ]]),
            ),
            ("OBJECT_DEFINITION", Ok(vec![vec![CellValue::Null]])),
        ]);
        let doc = fetch_ddl_document(
            &queries,
            &profile(),
            "appdb",
            &table_target("Users", ClickedRole::Table),
        )
        .await
        .unwrap();
        let body = &doc.primary_source().unwrap().body;
        assert!(body.contains("CREATE TABLE [dbo].[Users]"), "{body}");
        assert!(body.contains("-- Indexes"), "{body}");
        assert!(
            body.contains("CREATE NONCLUSTERED INDEX [IX_Users_Name]"),
            "{body}"
        );
        assert!(body.contains("-- Foreign Keys"), "{body}");
        assert!(
            body.contains("ADD CONSTRAINT [FK_Users_Org] FOREIGN KEY"),
            "{body}"
        );
        assert!(body.contains("-- Triggers"), "{body}");
        assert!(
            body.contains("CREATE TRIGGER dbo.trg_Users_Audit"),
            "{body}"
        );
    }

    #[test]
    fn comment_safe_flattens_multiline_errors() {
        assert_eq!(
            comment_safe("Connection error: first line\r\nsecond line"),
            "Connection error: first line second line"
        );
        assert_eq!(comment_safe("a\n\n\nb"), "a b");
        assert_eq!(comment_safe("  padded \t line  "), "padded line");
        assert_eq!(comment_safe("single line"), "single line");
    }

    /// Routes for the table-extras sections. `pk`/`columns`/`definition` are the
    /// always-needed CREATE TABLE routes; `indexes`/`fks`/`triggers` can each be
    /// swapped for an `Err` to simulate a section that a low-privilege user (or a
    /// server hiccup) can't read.
    fn table_extras_routes(
        indexes: Result<Vec<Vec<CellValue>>, String>,
        fks: Result<Vec<Vec<CellValue>>, String>,
        triggers: Result<Vec<Vec<CellValue>>, String>,
    ) -> Vec<FakeRoute> {
        vec![
            ("sys.triggers tr", triggers),
            (
                "sys.key_constraints",
                Ok(vec![vec![
                    s("PK_Users"),
                    s("CLUSTERED"),
                    s("UserId"),
                    CellValue::Bool(false),
                ]]),
            ),
            ("sys.foreign_keys fk", fks),
            ("sys.indexes i", indexes),
            (
                "FROM sys.columns c",
                Ok(vec![vec![
                    s("UserId"),
                    s("int"),
                    CellValue::I64(4),
                    CellValue::I64(10),
                    CellValue::I64(0),
                    CellValue::Bool(false),
                    CellValue::Bool(true),
                    CellValue::I64(1),
                    CellValue::I64(1),
                    CellValue::Bool(false),
                    CellValue::Null,
                    CellValue::Null,
                    CellValue::Null,
                ]]),
            ),
            ("OBJECT_DEFINITION", Ok(vec![vec![CellValue::Null]])),
        ]
    }

    fn index_row() -> Vec<CellValue> {
        vec![s(
            "CREATE NONCLUSTERED INDEX [IX_Users_Name] ON [dbo].[Users] ([Name] ASC);",
        )]
    }

    fn fk_row() -> Vec<CellValue> {
        vec![s(
            "ALTER TABLE [dbo].[Users] ADD CONSTRAINT [FK_Users_Org] FOREIGN KEY ([OrgId]) REFERENCES [dbo].[Orgs] ([OrgId]);",
        )]
    }

    fn trigger_row(name: &str) -> Vec<CellValue> {
        vec![
            s(name),
            s(&format!(
                "CREATE TRIGGER dbo.{name} ON dbo.Users AFTER INSERT AS SELECT 1"
            )),
        ]
    }

    async fn extras_body(routes: Vec<FakeRoute>) -> String {
        let queries = FakeQueries::new(routes);
        let doc = fetch_ddl_document(
            &queries,
            &profile(),
            "appdb",
            &table_target("Users", ClickedRole::Table),
        )
        .await
        .unwrap();
        doc.primary_source().unwrap().body.clone()
    }

    #[tokio::test]
    async fn table_ddl_survives_index_section_error() {
        let body = extras_body(table_extras_routes(
            Err("Connection error: perm denied\nline two".into()),
            Ok(vec![fk_row()]),
            Ok(vec![trigger_row("trg_Users_Audit")]),
        ))
        .await;
        // Index section degrades to a single-line comment; FKs + triggers render.
        assert!(body.contains("-- Indexes unavailable:"), "{body}");
        // The whole error, including "line two", stays on the one comment line.
        for line in body.lines() {
            if line.contains("line two") {
                assert!(line.trim_start().starts_with("--"), "leaked: {line}");
            }
        }
        assert!(body.contains("ADD CONSTRAINT [FK_Users_Org]"), "{body}");
        assert!(
            body.contains("CREATE TRIGGER dbo.trg_Users_Audit"),
            "{body}"
        );
    }

    #[tokio::test]
    async fn table_ddl_survives_fk_section_error() {
        let body = extras_body(table_extras_routes(
            Ok(vec![index_row()]),
            Err("Connection error: fk read failed".into()),
            Ok(vec![trigger_row("trg_Users_Audit")]),
        ))
        .await;
        assert!(
            body.contains("CREATE NONCLUSTERED INDEX [IX_Users_Name]"),
            "{body}"
        );
        assert!(body.contains("-- Foreign Keys unavailable:"), "{body}");
        assert!(
            body.contains("CREATE TRIGGER dbo.trg_Users_Audit"),
            "{body}"
        );
    }

    #[tokio::test]
    async fn table_ddl_survives_trigger_section_error() {
        let body = extras_body(table_extras_routes(
            Ok(vec![index_row()]),
            Ok(vec![fk_row()]),
            Err("Connection error: trigger read failed".into()),
        ))
        .await;
        assert!(
            body.contains("CREATE NONCLUSTERED INDEX [IX_Users_Name]"),
            "{body}"
        );
        assert!(body.contains("ADD CONSTRAINT [FK_Users_Org]"), "{body}");
        assert!(body.contains("-- Triggers unavailable:"), "{body}");
    }

    #[tokio::test]
    async fn table_ddl_error_comment_never_leaks_multiline_sql() {
        let body = extras_body(table_extras_routes(
            Err("Msg 229, Level 14, State 5, Line 1\nThe SELECT permission was denied.".into()),
            Ok(vec![fk_row()]),
            Ok(vec![trigger_row("trg_Users_Audit")]),
        ))
        .await;
        // Every physical line that carries error text must stay comment-prefixed:
        // the only place the message appears is on the `-- Indexes unavailable:` line.
        for line in body.lines() {
            if line.contains("The SELECT permission was denied.") {
                assert!(line.trim_start().starts_with("--"), "leaked to SQL: {line}");
            }
        }
    }

    #[tokio::test]
    async fn table_ddl_separates_triggers_with_go() {
        let body = extras_body(table_extras_routes(
            Ok(vec![]),
            Ok(vec![]),
            Ok(vec![trigger_row("trg_One"), trigger_row("trg_Two")]),
        ))
        .await;
        // GO must appear before the first trigger (own batch) and between the two.
        let go_count = body.matches("\nGO\n").count();
        assert_eq!(go_count, 2, "expected two GO separators: {body}");
        let first = body.find("CREATE TRIGGER dbo.trg_One").unwrap();
        let second = body.find("CREATE TRIGGER dbo.trg_Two").unwrap();
        let between = &body[first..second];
        assert!(between.contains("\nGO\n"), "no GO between triggers: {body}");
    }

    #[tokio::test]
    async fn view_target_uses_server_definition() {
        let queries = FakeQueries::new(vec![(
            "OBJECT_DEFINITION",
            Ok(vec![vec![s(
                "CREATE VIEW dbo.ActiveUsers AS SELECT 1 AS x",
            )]]),
        )]);
        let doc = fetch_ddl_document(
            &queries,
            &profile(),
            "appdb",
            &table_target("ActiveUsers", ClickedRole::View),
        )
        .await
        .unwrap();
        let source = doc.primary_source().unwrap();
        assert_eq!(source.kind, DdlSourceKind::Server);
        assert!(source.body.contains("CREATE VIEW dbo.ActiveUsers"));
    }

    #[tokio::test]
    async fn column_click_titles_the_column() {
        let queries = FakeQueries::new(vec![
            ("OBJECT_DEFINITION", Ok(vec![vec![CellValue::Null]])),
            (
                "FROM sys.columns c",
                Ok(vec![vec![
                    s("Email"),
                    s("nvarchar"),
                    CellValue::I64(-1),
                    CellValue::I64(0),
                    CellValue::I64(0),
                    CellValue::Bool(true),
                    CellValue::Bool(false),
                    CellValue::Null,
                    CellValue::Null,
                    CellValue::Bool(false),
                    CellValue::Null,
                    CellValue::Null,
                    CellValue::Null,
                ]]),
            ),
        ]);
        let doc = fetch_ddl_document(
            &queries,
            &profile(),
            "appdb",
            &table_target(
                "Users",
                ClickedRole::Column {
                    name: "Email".into(),
                },
            ),
        )
        .await
        .unwrap();
        assert_eq!(doc.title, "DDL: dbo.Users · column Email");
        assert!(doc.primary_source().unwrap().body.contains("nvarchar(max)"));
    }

    #[tokio::test]
    async fn missing_table_metadata_reports_error() {
        let queries = FakeQueries::new(vec![
            ("OBJECT_DEFINITION", Ok(vec![vec![CellValue::Null]])),
            ("FROM sys.columns c", Ok(vec![])),
        ]);
        let doc = fetch_ddl_document(
            &queries,
            &profile(),
            "appdb",
            &table_target("Gone", ClickedRole::Table),
        )
        .await
        .unwrap();
        assert!(doc.sources.is_empty());
        assert!(doc.error.unwrap().contains("No metadata found"));
    }

    #[tokio::test]
    async fn alias_type_generates_create_type() {
        let queries = FakeQueries::new(vec![(
            "sys.types t",
            Ok(vec![vec![
                s("Phone"),
                s("varchar"),
                CellValue::I64(20),
                CellValue::I64(0),
                CellValue::I64(0),
                CellValue::Bool(false), // nullable
                CellValue::Bool(false), // table type
                CellValue::Bool(false), // assembly type
            ]]),
        )]);
        let target = NavigationTarget::Type {
            catalog: None,
            schema: "dbo".into(),
            name: "Phone".into(),
        };
        let doc = fetch_ddl_document(&queries, &profile(), "appdb", &target)
            .await
            .unwrap();
        assert_eq!(
            doc.primary_source().unwrap().body,
            "CREATE TYPE [dbo].[Phone] FROM varchar(20) NOT NULL"
        );
    }

    #[tokio::test]
    async fn table_type_generates_as_table() {
        let queries = FakeQueries::new(vec![
            (
                "sys.types t",
                Ok(vec![vec![
                    s("OrderLines"),
                    s("table type"),
                    CellValue::Null,
                    CellValue::Null,
                    CellValue::Null,
                    CellValue::Bool(false),
                    CellValue::Bool(true), // table type
                    CellValue::Bool(false),
                ]]),
            ),
            (
                "sys.table_types",
                Ok(vec![vec![
                    s("OrderId"),
                    s("int"),
                    CellValue::I64(4),
                    CellValue::I64(10),
                    CellValue::I64(0),
                    CellValue::Bool(false),
                ]]),
            ),
        ]);
        let target = NavigationTarget::Type {
            catalog: None,
            schema: "dbo".into(),
            name: "OrderLines".into(),
        };
        let doc = fetch_ddl_document(&queries, &profile(), "appdb", &target)
            .await
            .unwrap();
        let body = &doc.primary_source().unwrap().body;
        assert!(body.contains("CREATE TYPE [dbo].[OrderLines] AS TABLE"));
        assert!(body.contains("[OrderId] int NOT NULL"));
    }

    #[tokio::test]
    async fn unknown_type_reports_error() {
        let queries = FakeQueries::new(vec![("sys.types t", Ok(vec![]))]);
        let target = NavigationTarget::Type {
            catalog: None,
            schema: "dbo".into(),
            name: "Nope".into(),
        };
        let doc = fetch_ddl_document(&queries, &profile(), "appdb", &target)
            .await
            .unwrap();
        assert!(doc.error.unwrap().contains("not found"));
    }

    fn profile_for(db: sqlly_core::DatabaseType) -> ConnectionProfile {
        let mut p = profile();
        p.database_type = db;
        p
    }

    #[tokio::test]
    async fn pg_routine_uses_pg_get_functiondef() {
        let queries = FakeQueries::new(vec![(
            "pg_get_functiondef",
            Ok(vec![vec![s(
                "CREATE OR REPLACE FUNCTION public.get_user() RETURNS int AS $$ SELECT 1 $$ LANGUAGE sql",
            )]]),
        )]);
        let profile = profile_for(sqlly_core::DatabaseType::Postgres);
        let mut target = routine_target("get_user");
        if let NavigationTarget::Routine { schema, .. } = &mut target {
            *schema = "public".into();
        }
        let doc = fetch_ddl_document(&queries, &profile, "appdb", &target)
            .await
            .unwrap();
        assert!(doc.error.is_none(), "{:?}", doc.error);
        let source = doc.primary_source().unwrap();
        assert_eq!(source.kind, DdlSourceKind::Server);
        assert!(source.body.contains("CREATE OR REPLACE FUNCTION"));
        // The T-SQL path must not run on PostgreSQL.
        let executed = queries.executed.lock().unwrap();
        assert!(
            executed
                .iter()
                .all(|sql| !sql.contains("OBJECT_DEFINITION")),
            "T-SQL leaked to PostgreSQL: {executed:?}"
        );
    }

    #[tokio::test]
    async fn mysql_view_uses_information_schema_definition() {
        let queries = FakeQueries::new(vec![(
            "information_schema.views",
            Ok(vec![vec![s(
                "CREATE OR REPLACE VIEW `app`.`active_users` AS\nselect 1",
            )]]),
        )]);
        let profile = profile_for(sqlly_core::DatabaseType::MySql);
        let doc = fetch_ddl_document(
            &queries,
            &profile,
            "app",
            &table_target("active_users", ClickedRole::View),
        )
        .await
        .unwrap();
        let source = doc.primary_source().unwrap();
        assert_eq!(source.kind, DdlSourceKind::Server);
        assert!(source.body.contains("CREATE OR REPLACE VIEW"));
    }

    #[tokio::test]
    async fn sqlite_table_definition_appends_index_ddl() {
        let queries = FakeQueries::new(vec![
            (
                "type = 'index'",
                Ok(vec![vec![s(
                    "CREATE INDEX idx_users_name ON users (name);",
                )]]),
            ),
            (
                "FROM sqlite_master WHERE name",
                Ok(vec![vec![s("CREATE TABLE users (id INTEGER PRIMARY KEY)")]]),
            ),
        ]);
        let profile = profile_for(sqlly_core::DatabaseType::Sqlite);
        let doc = fetch_ddl_document(
            &queries,
            &profile,
            "main",
            &table_target("users", ClickedRole::Table),
        )
        .await
        .unwrap();
        let source = doc.primary_source().unwrap();
        assert_eq!(source.kind, DdlSourceKind::Server);
        assert!(
            source.body.contains("CREATE TABLE users"),
            "{}",
            source.body
        );
        assert!(source.body.contains("-- Indexes"), "{}", source.body);
        assert!(source.body.contains("idx_users_name"), "{}", source.body);
        // The stored CREATE TABLE carries no terminator; without one the
        // index DDL glued onto it makes a single broken statement and a
        // sqlite3 restore fails.
        let statements = sqlly_lang::sqllex::split_statements(
            &source.body,
            sqlly_lang::sqlmodel::Dialect::Sqlite,
        );
        assert_eq!(statements.len(), 2, "{}", source.body);
    }

    /// SQLite stores a view's source verbatim, trailing `-- note` included;
    /// `;` appended to that line was swallowed by the comment, and the next
    /// statement in a dump ran into the view.
    #[tokio::test]
    async fn sqlite_view_ending_in_a_line_comment_is_terminated_on_its_own_line() {
        let queries = FakeQueries::new(vec![(
            "FROM sqlite_master WHERE name",
            Ok(vec![vec![s(
                "CREATE VIEW v AS SELECT 1 AS one -- trailing note\n",
            )]]),
        )]);
        let profile = profile_for(sqlly_core::DatabaseType::Sqlite);
        let doc = fetch_ddl_document(
            &queries,
            &profile,
            "main",
            &table_target("v", ClickedRole::View),
        )
        .await
        .unwrap();
        let body = &doc.primary_source().unwrap().body;
        assert!(body.ends_with("-- trailing note\n;"), "{body}");
        let script = format!("{body}\nSELECT 2;");
        let statements =
            sqlly_lang::sqllex::split_statements(&script, sqlly_lang::sqlmodel::Dialect::Sqlite);
        let texts: Vec<&str> = statements.iter().map(|s| s.text.trim()).collect();
        assert_eq!(texts.len(), 2, "{texts:#?}");
        assert_eq!(texts[1], "SELECT 2");

        // A body already ending in `;` or in a block comment is untouched.
        let mut body = "CREATE VIEW v AS SELECT 1 /* note */".to_string();
        terminate_statement(MetadataDialect::Sqlite, &mut body);
        assert_eq!(body, "CREATE VIEW v AS SELECT 1 /* note */;");
        let mut body = "CREATE VIEW v AS SELECT '-- not a comment'".to_string();
        terminate_statement(MetadataDialect::Sqlite, &mut body);
        assert_eq!(body, "CREATE VIEW v AS SELECT '-- not a comment';");
        let mut body = "CREATE VIEW v AS SELECT 1 # note".to_string();
        terminate_statement(MetadataDialect::MySql, &mut body);
        assert_eq!(body, "CREATE VIEW v AS SELECT 1 # note\n;");
    }

    #[tokio::test]
    async fn pg_routine_definition_is_terminated() {
        // pg_get_functiondef returns no trailing `;`, so two functions written
        // back to back into a dump ran together and the restore failed.
        let queries = FakeQueries::new(vec![(
            "pg_get_functiondef",
            Ok(vec![vec![s(
                "CREATE OR REPLACE FUNCTION public.get_user()\n RETURNS integer\n LANGUAGE sql\nAS $function$ SELECT 1; $function$\n",
            )]]),
        )]);
        let profile = profile_for(sqlly_core::DatabaseType::Postgres);
        let mut target = routine_target("get_user");
        if let NavigationTarget::Routine { schema, .. } = &mut target {
            *schema = "public".into();
        }
        let doc = fetch_ddl_document(&queries, &profile, "appdb", &target)
            .await
            .unwrap();
        let body = &doc.primary_source().unwrap().body;
        assert!(body.trim_end().ends_with("$function$;"), "{body}");
        let twice = format!("{body}\n{body}");
        assert_eq!(
            sqlly_lang::sqllex::split_statements(&twice, sqlly_lang::sqlmodel::Dialect::Postgres)
                .len(),
            2,
            "{twice}"
        );
    }

    #[tokio::test]
    async fn already_terminated_definition_is_not_double_terminated() {
        let queries = FakeQueries::new(vec![(
            "information_schema.views",
            Ok(vec![vec![s(
                "CREATE OR REPLACE VIEW `app`.`v` AS\nselect 1;\n",
            )]]),
        )]);
        let profile = profile_for(sqlly_core::DatabaseType::MySql);
        let doc = fetch_ddl_document(
            &queries,
            &profile,
            "app",
            &table_target("v", ClickedRole::View),
        )
        .await
        .unwrap();
        let body = &doc.primary_source().unwrap().body;
        assert!(body.trim_end().ends_with("select 1;"), "{body}");
        assert!(!body.contains(";;"), "{body}");
    }

    #[tokio::test]
    async fn pg_table_generates_create_with_identity_types_and_indexes() {
        let i = CellValue::I64;
        let queries = FakeQueries::new(vec![
            // Definition query matches but returns no rows: tables have no
            // stored source on PostgreSQL.
            ("pg_get_functiondef", Ok(vec![])),
            (
                "format_type",
                Ok(vec![
                    vec![
                        s("id"),
                        s("integer"),
                        i(0),
                        s("d"),
                        s(""),
                        CellValue::Null,
                        i(0),
                    ],
                    vec![
                        s("name"),
                        s("character varying(80)"),
                        i(1),
                        s(""),
                        s(""),
                        CellValue::Null,
                        i(0),
                    ],
                ]),
            ),
            (
                "AS is_expression",
                Ok(vec![vec![
                    s("ix_users_name"),
                    s("name"),
                    i(1),
                    i(0),
                    i(0),
                    i(0),
                ]]),
            ),
            (
                "ix.indisprimary::int AS is_primary_key, pg_get_expr",
                Ok(vec![vec![
                    s("ix_users_name"),
                    s("btree"),
                    i(0),
                    i(0),
                    CellValue::Null,
                    CellValue::Null,
                ]]),
            ),
        ]);
        let profile = profile_for(sqlly_core::DatabaseType::Postgres);
        let mut target = table_target("users", ClickedRole::Table);
        if let NavigationTarget::Table { schema, .. } = &mut target {
            *schema = "public".into();
        }
        let doc = fetch_ddl_document(&queries, &profile, "appdb", &target)
            .await
            .unwrap();
        assert!(doc.error.is_none(), "{:?}", doc.error);
        let source = doc.primary_source().unwrap();
        assert_eq!(source.kind, DdlSourceKind::Generated);
        let body = &source.body;
        assert!(body.contains("CREATE TABLE \"public\".\"users\""), "{body}");
        assert!(
            body.contains("\"id\" integer GENERATED BY DEFAULT AS IDENTITY NOT NULL"),
            "{body}"
        );
        assert!(
            body.contains("\"name\" character varying(80) NULL"),
            "{body}"
        );
        assert!(
            body.contains("CREATE INDEX \"ix_users_name\" ON \"public\".\"users\" (\"name\");"),
            "{body}"
        );
    }

    /// A dropped table: the column read casts the name to `regclass`, which
    /// raises `relation does not exist` (42P01). That is the controlled
    /// "No metadata found" document error, not a transport failure.
    #[tokio::test]
    async fn pg_dropped_table_reports_no_metadata_not_a_transport_error() {
        let queries = FakeQueries::new(vec![
            ("pg_get_functiondef", Ok(vec![])),
            (
                "format_type",
                Err("ERROR: relation \"public.gone\" does not exist (SQLSTATE 42P01)".into()),
            ),
            (
                "obj_description",
                Err("ERROR: relation \"public.gone\" does not exist (SQLSTATE 42P01)".into()),
            ),
            (
                "con.contype = 'c'",
                Err("ERROR: relation \"public.gone\" does not exist (SQLSTATE 42P01)".into()),
            ),
        ]);
        let profile = profile_for(sqlly_core::DatabaseType::Postgres);
        let mut target = table_target("gone", ClickedRole::Table);
        if let NavigationTarget::Table { schema, .. } = &mut target {
            *schema = "public".into();
        }
        let doc = fetch_ddl_document(&queries, &profile, "appdb", &target)
            .await
            .unwrap();
        assert!(doc.sources.is_empty(), "{:?}", doc.sources);
        let error = doc.error.unwrap();
        assert!(
            error.contains("No metadata found for public.gone"),
            "{error}"
        );
    }

    /// A column whose default draws from a shared sequence is not a serial:
    /// only a sequence the column owns makes it one. The generated CREATE
    /// keeps that default, and carries the primary key, which the old
    /// column-only builder dropped.
    #[tokio::test]
    async fn pg_table_keeps_shared_sequence_default_and_primary_key() {
        let i = CellValue::I64;
        let queries = FakeQueries::new(vec![
            ("pg_get_functiondef", Ok(vec![])),
            (
                "format_type",
                Ok(vec![
                    vec![
                        s("id"),
                        s("integer"),
                        i(0),
                        s(""),
                        s(""),
                        s("nextval('orders_id_seq'::regclass)"),
                        i(1), // the column owns orders_id_seq: a serial
                    ],
                    vec![
                        s("ticket"),
                        s("bigint"),
                        i(0),
                        s(""),
                        s(""),
                        s("nextval('shared_ticket_seq'::regclass)"),
                        i(0), // shared sequence, owned by nobody here
                    ],
                    vec![s("note"), s("text"), i(1), s(""), s(""), s("'n/a'::text")],
                ]),
            ),
            (
                "con.contype = 'p'",
                Ok(vec![vec![s("public"), s("orders"), s("id"), i(1)]]),
            ),
        ]);
        let profile = profile_for(sqlly_core::DatabaseType::Postgres);
        let mut target = table_target("orders", ClickedRole::Table);
        if let NavigationTarget::Table { schema, .. } = &mut target {
            *schema = "public".into();
        }
        let doc = fetch_ddl_document(&queries, &profile, "appdb", &target)
            .await
            .unwrap();
        assert!(doc.error.is_none(), "{:?}", doc.error);
        let body = &doc.primary_source().unwrap().body;
        assert!(
            body.contains(
                "\"ticket\" bigint NOT NULL DEFAULT nextval('shared_ticket_seq'::regclass)"
            ),
            "{body}"
        );
        assert!(
            !body.contains("\"ticket\" bigint GENERATED"),
            "a shared-sequence default is not an identity: {body}"
        );
        assert!(
            body.contains("\"id\" integer GENERATED BY DEFAULT AS IDENTITY NOT NULL"),
            "{body}"
        );
        assert!(
            body.contains("\"note\" text NULL DEFAULT 'n/a'::text"),
            "{body}"
        );
        assert!(body.contains("PRIMARY KEY (\"id\")"), "{body}");
    }

    /// MySQL/MariaDB tables come from the server's own SHOW CREATE TABLE:
    /// `unsigned`, every DEFAULT (including MariaDB's `nextval(seq)`), the
    /// primary key, and the engine all survive.
    #[tokio::test]
    async fn mysql_table_keeps_unsigned_defaults_primary_key_and_engine() {
        let i = CellValue::I64;
        let show_create = "CREATE TABLE `orders` (\n  `id` int(10) unsigned NOT NULL AUTO_INCREMENT,\n  `ticket` bigint(20) NOT NULL DEFAULT nextval(`app`.`ticket_seq`),\n  `status` varchar(20) NOT NULL DEFAULT 'new',\n  PRIMARY KEY (`id`)\n) ENGINE=InnoDB AUTO_INCREMENT=7 DEFAULT CHARSET=utf8mb4";
        let queries = FakeQueries::new(vec![
            ("information_schema.views", Ok(vec![])),
            (
                "SHOW CREATE TABLE",
                Ok(vec![vec![s("orders"), s(show_create)]]),
            ),
            (
                "c.column_type",
                Ok(vec![
                    vec![
                        s("id"),
                        s("int(10) unsigned"),
                        s("int"),
                        i(0),
                        s("auto_increment"),
                        CellValue::Null,
                        s(""),
                    ],
                    vec![
                        s("ticket"),
                        s("bigint(20)"),
                        s("bigint"),
                        i(0),
                        s(""),
                        s("nextval(`app`.`ticket_seq`)"),
                        s(""),
                    ],
                    vec![
                        s("status"),
                        s("varchar(20)"),
                        s("varchar"),
                        i(0),
                        s(""),
                        s("'new'"),
                        s(""),
                    ],
                ]),
            ),
            (
                "constraint_name = 'PRIMARY'",
                Ok(vec![vec![s("app"), s("orders"), s("id"), i(1)]]),
            ),
        ]);
        let profile = profile_for(sqlly_core::DatabaseType::MariaDb);
        let mut target = table_target("orders", ClickedRole::Table);
        if let NavigationTarget::Table { schema, .. } = &mut target {
            *schema = "app".into();
        }
        let doc = fetch_ddl_document(&queries, &profile, "app", &target)
            .await
            .unwrap();
        assert!(doc.error.is_none(), "{:?}", doc.error);
        let body = &doc.primary_source().unwrap().body;
        assert!(
            body.contains("`id` int(10) unsigned NOT NULL AUTO_INCREMENT"),
            "{body}"
        );
        assert!(
            body.contains("`ticket` bigint(20) NOT NULL DEFAULT nextval(`app`.`ticket_seq`)"),
            "{body}"
        );
        assert!(body.contains("DEFAULT 'new'"), "{body}");
        assert!(body.contains("PRIMARY KEY (`id`)"), "{body}");
        assert!(body.contains("ENGINE=InnoDB"), "{body}");
        assert!(!body.contains("AUTO_INCREMENT=7"), "{body}");
    }

    fn pg_routine_doc_routes(privileges: Vec<Vec<CellValue>>) -> Vec<FakeRoute> {
        vec![
            ("pg_catalog.aclexplode(", Ok(privileges)),
            (
                "pg_get_functiondef",
                Ok(vec![vec![s(
                    "CREATE OR REPLACE FUNCTION public.acltest()\n RETURNS integer\n LANGUAGE sql\nAS $function$ SELECT 1 $function$\n",
                )]]),
            ),
        ]
    }

    fn pg_routine() -> NavigationTarget {
        let mut target = routine_target("acltest");
        if let NavigationTarget::Routine { schema, .. } = &mut target {
            *schema = "public".into();
        }
        target
    }

    #[tokio::test]
    async fn pg_privileges_section_ends_the_definition_when_asked() {
        let f = || s("FUNCTION public.acltest()");
        let queries = FakeQueries::new(pg_routine_doc_routes(vec![
            vec![
                s("REVOKE"),
                s("EXECUTE"),
                CellValue::Null,
                CellValue::Bool(false),
                f(),
            ],
            vec![
                s("GRANT"),
                s("EXECUTE"),
                s("app_reader"),
                CellValue::Bool(false),
                f(),
            ],
        ]));
        let profile = profile_for(sqlly_core::DatabaseType::Postgres);
        let doc = fetch_ddl_document_with(
            &queries,
            &profile,
            "appdb",
            &pg_routine(),
            DdlOptions {
                include_privileges: true,
            },
        )
        .await
        .unwrap();
        let body = &doc.primary_source().unwrap().body;
        assert!(
            body.ends_with(
                "$function$;\n\n-- Privileges\nREVOKE EXECUTE ON FUNCTION public.acltest() FROM PUBLIC;\nGRANT EXECUTE ON FUNCTION public.acltest() TO app_reader;"
            ),
            "{body}"
        );
    }

    #[tokio::test]
    async fn pg_privileges_are_left_out_by_default_and_when_unchanged() {
        let profile = profile_for(sqlly_core::DatabaseType::Postgres);
        let queries = FakeQueries::new(pg_routine_doc_routes(vec![]));
        let plain = fetch_ddl_document(&queries, &profile, "appdb", &pg_routine())
            .await
            .unwrap();
        assert!(
            queries
                .executed
                .lock()
                .unwrap()
                .iter()
                .all(|sql| !sql.contains("aclexplode")),
            "privileges are only read when asked for"
        );
        let asked = fetch_ddl_document_with(
            &queries,
            &profile,
            "appdb",
            &pg_routine(),
            DdlOptions {
                include_privileges: true,
            },
        )
        .await
        .unwrap();
        // Default privileges (or a NULL ACL): no section at all.
        assert_eq!(
            asked.primary_source().unwrap().body,
            plain.primary_source().unwrap().body
        );
        assert!(
            !asked
                .primary_source()
                .unwrap()
                .body
                .contains("-- Privileges")
        );
    }

    #[tokio::test]
    async fn privileges_option_is_ignored_off_postgres() {
        let queries = FakeQueries::new(vec![(
            "OBJECT_DEFINITION",
            Ok(vec![vec![s("CREATE PROCEDURE dbo.p AS SELECT 1")]]),
        )]);
        fetch_ddl_document_with(
            &queries,
            &profile(),
            "appdb",
            &routine_target("p"),
            DdlOptions {
                include_privileges: true,
            },
        )
        .await
        .unwrap();
        assert!(
            queries
                .executed
                .lock()
                .unwrap()
                .iter()
                .all(|sql| !sql.contains("aclexplode"))
        );
    }

    #[tokio::test]
    async fn non_tds_type_target_reports_unsupported() {
        let queries = FakeQueries::new(vec![]);
        let profile = profile_for(sqlly_core::DatabaseType::Postgres);
        let target = NavigationTarget::Type {
            catalog: None,
            schema: "public".into(),
            name: "phone".into(),
        };
        let doc = fetch_ddl_document(&queries, &profile, "appdb", &target)
            .await
            .unwrap();
        assert!(doc.error.unwrap().contains("PostgreSQL"));
        assert!(queries.executed.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn definition_query_uses_safe_escaping() {
        let queries = FakeQueries::new(vec![(
            "OBJECT_DEFINITION",
            Ok(vec![vec![s("CREATE PROCEDURE x AS SELECT 1")]]),
        )]);
        fetch_ddl_document(&queries, &profile(), "appdb", &routine_target("We'ird"))
            .await
            .unwrap();
        let executed = queries.executed.lock().unwrap();
        assert!(
            executed[0].contains("OBJECT_ID(N'dbo.We''ird')"),
            "got: {}",
            executed[0]
        );
    }
}
