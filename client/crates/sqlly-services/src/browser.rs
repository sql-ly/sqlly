//! Browser (wasm) implementations of the service ports.
//!
//! A browser tab cannot open TDS sockets, so the wasm build of
//! [`Services::new`](crate::services::Services::new) wires these executors
//! instead of the tds-backed defaults. Everything above the ports —
//! intellisense, the SQL model, metadata tree building, admin tooling — is
//! the same code the desktop app runs; only the engine underneath differs:
//!
//! - **SQLite** (`rusqlite` over `sqlite-wasm-rs`): the primary backend.
//!   Databases live in the in-memory VFS; users load `.db` files as bytes
//!   (`open_sqlite_database`) or start from the bundled sample.
//! - **Demo** (`sqlly-fakedb`): an emulated SQL-Server-flavored database with
//!   tables, views, and stored procedures, for exercising procedure flows
//!   SQLite does not have.
//! - **PGlite** ([`pglite`]): a real Postgres compiled to wasm, booted by the
//!   host page with the Chinook sample loaded. Profiles whose engine speaks
//!   the Postgres wire route to it; metadata for those profiles goes through
//!   the regular dialect-SQL [`DefaultMetadataService`] since the backend is
//!   genuine Postgres (see [`BrowserMetadataRouter`]).
//!
//! Everything runs synchronously on the single browser thread: `execute`
//! materializes the whole result and pre-buffers the [`QueryEvent`] stream
//! into the channel before returning the handle, so no executor task is
//! needed. A grid run first passes the stream through the spill stage ([`spill::stage`]),
//! which moves the rows past the spill threshold to browser storage (OPFS)
//! as they are read. The backend lives in a thread-local because `rusqlite::Connection`
//! is `!Sync` — the service structs themselves stay zero-sized and `Send +
//! Sync` as the ports require, which is fine on wasm's one thread.

pub mod pglite;
pub mod proxy;
pub mod spill;

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use sqlly_core::{
    CellValue, ConnectionProfile, DatabaseSummary, ObjectKind, ResultColumn, StudioError,
    StudioResult,
};
use sqlly_intel::metadata::{MetadataDialect, MetadataQuery, sql_for_dialect};
use tokio::sync::mpsc;

use crate::connection::ConnectionManager;
use crate::intellisense::procedures::ProcedureParameter;
use crate::metadata::{
    BulkColumnRow, DefaultMetadataService, ForeignKeyRow, MetadataNode, MetadataService,
    PrimaryKeyRow,
};
use crate::query::{CellLimits, QueryEvent, QueryExecutor, QueryHandle, truncate_rows};
use crate::row_editing::{
    BatchOutcome, BatchRequest, DeleteRowOutcome, DeleteRowRequest, FkLabelRequest, FkLabelResult,
    FkSearchRequest, FkSearchResult, InsertRowOutcome, InsertRowRequest, RelatedRowsRequest,
    RelatedRowsResult, RowEditOpenOutcome, RowEditOpenRequest, RowEditingService, TableTarget,
    UpdateRowOutcome, UpdateRowRequest,
};

/// Set when this page runs against real servers through a relay proxy or a
/// SQLly Embedded bridge instead of the in-tab samples. The standalone
/// preview steers people to the desktop app; a served session is somebody's
/// product page, where that pitch is noise and the desktop app may not exist.
static SERVED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Record whether this page load is served by a proxy (see [`SERVED`]).
pub fn set_served(served: bool) {
    SERVED.store(served, Ordering::Relaxed);
}

/// Whether this page load is served by a proxy (see [`SERVED`]).
pub fn is_served() -> bool {
    SERVED.load(Ordering::Relaxed)
}

/// Finishes a "<something> is not available" sentence for this page: the
/// preview points at the desktop app, a served session just says why.
pub fn unavailable(what: &str) -> String {
    crate::browser_unavailable(what, is_served())
}

/// The message every disabled capability surfaces so users understand the
/// limitation is the browser, not a bug.
fn unsupported(what: &str) -> StudioError {
    let capability = match what {
        "Object metadata for the demo database" => crate::BrowserCapability::DemoObjectMetadata,
        "Row editing" => crate::BrowserCapability::RowEditing,
        "Foreign-key search" => crate::BrowserCapability::ForeignKeySearch,
        "Foreign-key labels" => crate::BrowserCapability::ForeignKeyLabels,
        "Related-row browsing" => crate::BrowserCapability::RelatedRowBrowsing,
        _ => {
            return StudioError::CapabilityNotSupported(unavailable(&format!(
                "{what} is not available"
            )));
        }
    };
    StudioError::CapabilityNotSupportedMessage(capability.unavailable_message(is_served()))
}

/// The active in-browser database.
pub enum BrowserBackend {
    Sqlite(rusqlite::Connection),
    Demo(crate::fakedb::FakeDatabase),
}

thread_local! {
    static BACKEND: RefCell<Option<BrowserBackend>> = const { RefCell::new(None) };
}

/// Switch to the emulated demo database (Northwind-shaped, with procedures).
pub fn open_demo_database() {
    BACKEND.with(|b| *b.borrow_mut() = Some(BrowserBackend::Demo(crate::fakedb::demo_database())));
}

/// Open a SQLite database from raw file bytes (a fetched sample or a
/// user-chosen local `.db` file). Replaces any previously open backend.
pub fn open_sqlite_database(bytes: &[u8]) -> StudioResult<()> {
    let mut conn = rusqlite::Connection::open_in_memory()
        .map_err(|e| StudioError::Connection(format!("open in-memory SQLite: {e}")))?;
    conn.deserialize_read_exact("main", bytes, bytes.len(), false)
        .map_err(|e| StudioError::Connection(format!("load SQLite image: {e}")))?;
    BACKEND.with(|b| *b.borrow_mut() = Some(BrowserBackend::Sqlite(conn)));
    Ok(())
}

/// Open an empty in-memory SQLite database.
pub fn open_empty_sqlite_database() -> StudioResult<()> {
    let conn = rusqlite::Connection::open_in_memory()
        .map_err(|e| StudioError::Connection(format!("open in-memory SQLite: {e}")))?;
    BACKEND.with(|b| *b.borrow_mut() = Some(BrowserBackend::Sqlite(conn)));
    Ok(())
}

/// `Some("sqlite" | "demo")` when a backend is open.
pub fn backend_kind() -> Option<&'static str> {
    BACKEND.with(|b| match &*b.borrow() {
        Some(BrowserBackend::Sqlite(_)) => Some("sqlite"),
        Some(BrowserBackend::Demo(_)) => Some("demo"),
        None => None,
    })
}

fn with_backend<T>(f: impl FnOnce(&BrowserBackend) -> StudioResult<T>) -> StudioResult<T> {
    BACKEND.with(|b| match &*b.borrow() {
        Some(backend) => f(backend),
        None => Err(StudioError::Connection(
            "no browser database is open — load the sample, open a local SQLite file, or switch \
             to the demo database"
                .into(),
        )),
    })
}

fn with_backend_mut<T>(f: impl FnOnce(&mut BrowserBackend) -> StudioResult<T>) -> StudioResult<T> {
    BACKEND.with(|b| match &mut *b.borrow_mut() {
        Some(backend) => f(backend),
        None => Err(StudioError::Connection(
            "no browser database is open — load the sample, open a local SQLite file, or switch \
             to the demo database"
                .into(),
        )),
    })
}

fn fake_value_to_cell(value: &crate::fakedb::Value) -> CellValue {
    match value {
        crate::fakedb::Value::Null => CellValue::Null,
        crate::fakedb::Value::Bool(b) => CellValue::Bool(*b),
        crate::fakedb::Value::Int(i) => CellValue::I64(*i),
        crate::fakedb::Value::Float(f) => CellValue::F64(*f),
        crate::fakedb::Value::Text(s) => CellValue::String(s.clone()),
    }
}

fn sqlite_value_to_cell(value: rusqlite::types::ValueRef<'_>) -> CellValue {
    use rusqlite::types::ValueRef;
    match value {
        ValueRef::Null => CellValue::Null,
        ValueRef::Integer(i) => CellValue::I64(i),
        ValueRef::Real(f) => CellValue::F64(f),
        ValueRef::Text(t) => CellValue::String(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => CellValue::Bytes(b.to_vec()),
    }
}

/// Rows per [`QueryEvent::Rows`] batch from the SQLite backend, so a grid
/// run's spill stage can move a large result to browser storage as it is
/// read instead of after all of it is in memory.
const SQLITE_BATCH_ROWS: usize = 4_096;

/// Run a SQL batch against the active backend, handing every result set to
/// `emit` as the [`QueryEvent`] sequence the streaming contract promises.
fn execute_into(sql: &str, emit: &mut dyn FnMut(QueryEvent)) {
    let started = js_now_ms();
    emit(QueryEvent::Started);
    let result: StudioResult<()> = with_backend_mut(|backend| match backend {
        BrowserBackend::Demo(db) => {
            let execution = db
                .execute_mut(sql)
                .map_err(|e| StudioError::QueryExecution(e.0))?;
            for (index, set) in execution.result_sets.iter().enumerate() {
                let columns = set
                    .columns
                    .iter()
                    .map(|c| ResultColumn {
                        name: c.name.clone(),
                        data_type: c.type_name.clone(),
                        nullable: true,
                    })
                    .collect();
                emit(QueryEvent::ResultSet { index, columns });
                let mut rows: Vec<Vec<CellValue>> = set
                    .rows
                    .iter()
                    .map(|r| r.iter().map(fake_value_to_cell).collect())
                    .collect();
                let count = rows.len() as u64;
                let truncated = truncate_rows(&mut rows, 0, &CellLimits::default());
                emit(QueryEvent::Rows { index, rows });
                if !truncated.is_empty() {
                    emit(QueryEvent::TruncatedCells {
                        index,
                        cells: truncated,
                    });
                }
                emit(QueryEvent::RowCount {
                    index,
                    rows_affected: count,
                });
            }
            for message in &execution.messages {
                emit(QueryEvent::Message {
                    text: message.clone(),
                });
            }
            Ok(())
        }
        BrowserBackend::Sqlite(conn) => {
            use fallible_iterator::FallibleIterator;
            let mut index = 0usize;
            // `Batch` walks multi-statement SQL exactly the way sqlite's
            // prepare loop does, so scripts pasted from the editor work.
            let mut batch = rusqlite::Batch::new(conn, sql);
            while let Some(mut stmt) = batch
                .next()
                .map_err(|e| StudioError::QueryExecution(e.to_string()))?
            {
                let column_count = stmt.column_count();
                if column_count == 0 {
                    // A statement with no result set (INSERT/UPDATE/DDL).
                    let affected = stmt
                        .raw_execute()
                        .map_err(|e| StudioError::QueryExecution(e.to_string()))?;
                    emit(QueryEvent::Message {
                        text: format!("({affected} row(s) affected)"),
                    });
                    continue;
                }
                let columns: Vec<ResultColumn> = (0..column_count)
                    .map(|i| ResultColumn {
                        name: stmt.column_name(i).unwrap_or("?").to_string(),
                        data_type: "text".to_string(),
                        nullable: true,
                    })
                    .collect();
                emit(QueryEvent::ResultSet { index, columns });
                let mut count = 0usize;
                let mut batch: Vec<Vec<CellValue>> = Vec::new();
                let mut flush = |batch: &mut Vec<Vec<CellValue>>, count: &mut usize| {
                    let mut rows = std::mem::take(batch);
                    let truncated = truncate_rows(&mut rows, *count, &CellLimits::default());
                    *count += rows.len();
                    emit(QueryEvent::Rows { index, rows });
                    if !truncated.is_empty() {
                        emit(QueryEvent::TruncatedCells {
                            index,
                            cells: truncated,
                        });
                    }
                };
                let mut rows = stmt.raw_query();
                while let Some(row) = rows
                    .next()
                    .map_err(|e| StudioError::QueryExecution(e.to_string()))?
                {
                    let mut cells = Vec::with_capacity(column_count);
                    for i in 0..column_count {
                        let value = row
                            .get_ref(i)
                            .map_err(|e| StudioError::QueryExecution(e.to_string()))?;
                        cells.push(sqlite_value_to_cell(value));
                    }
                    batch.push(cells);
                    if batch.len() >= SQLITE_BATCH_ROWS {
                        flush(&mut batch, &mut count);
                    }
                }
                if !batch.is_empty() || count == 0 {
                    flush(&mut batch, &mut count);
                }
                drop(flush);
                emit(QueryEvent::RowCount {
                    index,
                    rows_affected: count as u64,
                });
                index += 1;
            }
            Ok(())
        }
    });
    match result {
        Ok(()) => emit(QueryEvent::Completed {
            elapsed_ms: js_now_ms().saturating_sub(started) as u128,
        }),
        Err(error) => emit(QueryEvent::Error {
            message: error.to_string(),
        }),
    }
}

/// `std::time::Instant` panics on wasm32-unknown-unknown; read the JS clock
/// instead (`Date.now()`, milliseconds since the epoch).
fn js_now_ms() -> u64 {
    js_sys::Date::now() as u64
}

/// Streaming query executor over the in-browser backend. Synchronous under
/// the hood: the full event stream is buffered into the channel up front.
pub struct BrowserQueryExecutor {
    task_counter: AtomicU64,
}

impl BrowserQueryExecutor {
    pub fn new() -> Self {
        Self {
            task_counter: AtomicU64::new(1),
        }
    }
}

impl Default for BrowserQueryExecutor {
    fn default() -> Self {
        Self::new()
    }
}

impl BrowserQueryExecutor {
    /// Runs `sql` and buffers its whole event stream. A grid run (`grid`)
    /// passes the events through the spill stage ([`spill::stage`]), which moves the
    /// rows past the spill threshold to browser storage as they are read.
    async fn run(
        &self,
        profile: &ConnectionProfile,
        sql: &str,
        grid: bool,
    ) -> StudioResult<QueryHandle> {
        let task_id = format!(
            "browser-{}",
            self.task_counter.fetch_add(1, Ordering::Relaxed)
        );
        // Postgres-wire profiles run against the in-tab PGlite instance. The JS
        // bridge is async and `!Send`, so await it through the `Send` wrapper
        // (`pglite::exec` hops onto the browser thread via `spawn_local` and
        // returns the whole result set back over a oneshot). Then buffer the
        // full event stream up front — exactly like the SQLite path below —
        // rather than trickling events across `spawn_local` await turns.
        //
        // PGlite hands back every row of every result set at once, so streaming
        // the events bought nothing. Worse, spreading them (and the nested
        // results-grid `Entity::update`/`emit` each one drives) across frames
        // let them interleave with the user's live mouse / hover / paint borrow
        // of that same gpui entity — a re-entrant borrow that panics with
        // "already borrowed" and then cascades. Pre-buffering keeps every grid
        // mutation inside a single pump drain, matching the SQLite path that
        // never hit this.
        let events = if pglite::is_pglite_profile(profile) {
            let started = js_now_ms();
            let outcome = pglite::exec(sql.to_string()).await;
            staged(grid, |emit| {
                emit(QueryEvent::Started);
                match outcome {
                    Ok(results) => {
                        for event in pglite::events_from_results(results) {
                            emit(event);
                        }
                        emit(QueryEvent::Completed {
                            elapsed_ms: js_now_ms().saturating_sub(started) as u128,
                        });
                    }
                    Err(error) => emit(QueryEvent::Error {
                        message: error.to_string(),
                    }),
                }
            })
        } else {
            staged(grid, |emit| execute_into(sql, emit))
        };
        let (tx, rx) = mpsc::channel(events.len().max(1));
        for event in events {
            // Capacity equals the event count, so try_send cannot fail.
            let _ = tx.try_send(event);
        }
        Ok(QueryHandle {
            task_id,
            events: rx,
        })
    }
}

/// Collects the events `produce` emits, through a spill stage when `grid`.
/// Synchronous, so the (`!Send`) stage never lives across an await.
fn staged(grid: bool, produce: impl FnOnce(&mut dyn FnMut(QueryEvent))) -> Vec<QueryEvent> {
    let mut events = Vec::new();
    match grid.then(spill::stage).flatten() {
        Some(mut stage) => produce(&mut |event| stage.push(event, &mut events)),
        None => produce(&mut |event| events.push(event)),
    }
    events
}

#[async_trait]
impl QueryExecutor for BrowserQueryExecutor {
    async fn execute(
        &self,
        profile: &ConnectionProfile,
        sql: &str,
        _row_buffer: usize,
    ) -> StudioResult<QueryHandle> {
        self.run(profile, sql, false).await
    }

    async fn execute_for_grid(
        &self,
        profile: &ConnectionProfile,
        _session: Option<&str>,
        sql: &str,
        _row_buffer: usize,
    ) -> StudioResult<QueryHandle> {
        self.run(profile, sql, true).await
    }

    async fn cancel(&self, _task_id: &str) -> StudioResult<()> {
        // Execution is synchronous; by the time a cancel arrives it is done.
        Ok(())
    }
}

/// Connection manager over the in-browser backend: "connecting" means having
/// a database open in the tab.
pub struct BrowserConnectionManager;

#[async_trait]
impl ConnectionManager for BrowserConnectionManager {
    async fn test_connection(&self, profile: &ConnectionProfile) -> StudioResult<()> {
        if pglite::is_pglite_profile(profile) {
            return pglite::exec("SELECT 1").await.map(|_| ());
        }
        with_backend(|_| Ok(()))
    }

    async fn list_databases(
        &self,
        profile: &ConnectionProfile,
    ) -> StudioResult<Vec<DatabaseSummary>> {
        if pglite::is_pglite_profile(profile) {
            // PGlite is a single-database Postgres; report what it calls itself.
            let names = pglite::query_scalar_strings("SELECT current_database()").await?;
            return Ok(names
                .into_iter()
                .map(|name| DatabaseSummary {
                    name,
                    state: "ONLINE".to_string(),
                    compatibility_level: None,
                })
                .collect());
        }
        with_backend(|backend| {
            let name = match backend {
                BrowserBackend::Sqlite(_) => "main",
                BrowserBackend::Demo(_) => "demo",
            };
            Ok(vec![DatabaseSummary {
                name: name.to_string(),
                state: "ONLINE".to_string(),
                compatibility_level: None,
            }])
        })
    }
}

/// Metadata service answering from the live backend instead of SQL Server
/// catalog views: `sqlite_master`/PRAGMAs for SQLite, the emulated schema
/// for the demo database.
pub struct BrowserMetadataService;

impl BrowserMetadataService {
    fn sqlite_objects(
        conn: &rusqlite::Connection,
        type_name: &str,
        kind: ObjectKind,
    ) -> StudioResult<Vec<MetadataNode>> {
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = ?1 AND name NOT LIKE 'sqlite_%' ORDER BY name")
            .map_err(|e| StudioError::QueryExecution(e.to_string()))?;
        let names = stmt
            .query_map([type_name], |row| row.get::<_, String>(0))
            .map_err(|e| StudioError::QueryExecution(e.to_string()))?
            .filter_map(Result::ok);
        Ok(names
            .map(|name| MetadataNode::new(name, kind.clone()).with_schema("main"))
            .collect())
    }

    /// Run one catalog statement against the in-tab SQLite connection and
    /// collect every row as raw cells — the shape
    /// [`MetadataService::run_metadata_query`] promises.
    fn sqlite_rows(conn: &rusqlite::Connection, sql: &str) -> StudioResult<Vec<Vec<CellValue>>> {
        let mut stmt = conn
            .prepare(sql)
            .map_err(|e| StudioError::QueryExecution(e.to_string()))?;
        let column_count = stmt.column_count();
        let mut out = Vec::new();
        let mut rows = stmt.raw_query();
        while let Some(row) = rows
            .next()
            .map_err(|e| StudioError::QueryExecution(e.to_string()))?
        {
            let mut cells = Vec::with_capacity(column_count);
            for i in 0..column_count {
                let value = row
                    .get_ref(i)
                    .map_err(|e| StudioError::QueryExecution(e.to_string()))?;
                cells.push(sqlite_value_to_cell(value));
            }
            out.push(cells);
        }
        Ok(out)
    }

    /// Generate the SQLite catalog SQL for `query` and run it. The dialect
    /// generator is the same one the desktop app uses for `.db` files, and it
    /// keeps the per-variant positional result shape the callers parse, so the
    /// object-editor pages and explorer folders read identically here.
    fn sqlite_metadata_rows(
        conn: &rusqlite::Connection,
        query: &MetadataQuery,
    ) -> StudioResult<Vec<Vec<CellValue>>> {
        let sql = sql_for_dialect(query, MetadataDialect::Sqlite)
            .map_err(|unsupported| StudioError::CapabilityNotSupported(unsupported.to_string()))?;
        Self::sqlite_rows(conn, &sql)
    }

    fn sqlite_table_names(conn: &rusqlite::Connection) -> StudioResult<Vec<String>> {
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type IN ('table','view') AND name NOT LIKE 'sqlite_%' ORDER BY name")
            .map_err(|e| StudioError::QueryExecution(e.to_string()))?;
        let names = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| StudioError::QueryExecution(e.to_string()))?
            .filter_map(Result::ok)
            .collect();
        Ok(names)
    }
}

#[async_trait]
impl MetadataService for BrowserMetadataService {
    async fn list_schemas(
        &self,
        _profile: &ConnectionProfile,
        _database: &str,
    ) -> StudioResult<Vec<String>> {
        with_backend(|backend| {
            Ok(vec![
                match backend {
                    BrowserBackend::Sqlite(_) => "main",
                    BrowserBackend::Demo(_) => "dbo",
                }
                .to_string(),
            ])
        })
    }

    async fn list_tables(
        &self,
        _profile: &ConnectionProfile,
        _database: &str,
        _schema: Option<&str>,
    ) -> StudioResult<Vec<MetadataNode>> {
        with_backend(|backend| match backend {
            BrowserBackend::Sqlite(conn) => Self::sqlite_objects(conn, "table", ObjectKind::Table),
            BrowserBackend::Demo(db) => Ok(db
                .schema()
                .tables
                .iter()
                .map(|t| MetadataNode::new(&t.name, ObjectKind::Table).with_schema("dbo"))
                .collect()),
        })
    }

    async fn list_views(
        &self,
        _profile: &ConnectionProfile,
        _database: &str,
        _schema: Option<&str>,
    ) -> StudioResult<Vec<MetadataNode>> {
        with_backend(|backend| match backend {
            BrowserBackend::Sqlite(conn) => Self::sqlite_objects(conn, "view", ObjectKind::View),
            BrowserBackend::Demo(db) => Ok(db
                .schema()
                .views
                .iter()
                .map(|v| MetadataNode::new(&v.name, ObjectKind::View).with_schema("dbo"))
                .collect()),
        })
    }

    async fn list_programmable_objects(
        &self,
        _profile: &ConnectionProfile,
        _database: &str,
        _schema: Option<&str>,
    ) -> StudioResult<Vec<MetadataNode>> {
        with_backend(|backend| match backend {
            // SQLite has no stored procedures.
            BrowserBackend::Sqlite(_) => Ok(Vec::new()),
            BrowserBackend::Demo(db) => Ok(db
                .schema()
                .procedures
                .iter()
                .map(|p| MetadataNode::new(&p.name, ObjectKind::StoredProcedure).with_schema("dbo"))
                .collect()),
        })
    }

    async fn list_columns(
        &self,
        _profile: &ConnectionProfile,
        _database: &str,
        _schema: &str,
        object: &str,
    ) -> StudioResult<Vec<MetadataNode>> {
        with_backend(|backend| match backend {
            BrowserBackend::Sqlite(conn) => {
                let mut stmt = conn
                    .prepare("SELECT name, type, \"notnull\" FROM pragma_table_info(?1)")
                    .map_err(|e| StudioError::QueryExecution(e.to_string()))?;
                let cols = stmt
                    .query_map([object], |row| {
                        let name: String = row.get(0)?;
                        let data_type: String = row.get(1)?;
                        let not_null: i64 = row.get(2)?;
                        Ok((name, data_type, not_null))
                    })
                    .map_err(|e| StudioError::QueryExecution(e.to_string()))?
                    .filter_map(Result::ok);
                Ok(cols
                    .map(|(name, data_type, not_null)| {
                        let type_label = if data_type.is_empty() {
                            "any".to_string()
                        } else {
                            data_type.to_lowercase()
                        };
                        let detail = if not_null == 0 {
                            format!("{type_label}, nullable")
                        } else {
                            type_label
                        };
                        MetadataNode::new(name, ObjectKind::Column).with_detail(detail)
                    })
                    .collect())
            }
            BrowserBackend::Demo(db) => {
                let schema = db.schema();
                let columns = schema
                    .tables
                    .iter()
                    .find(|t| t.name.eq_ignore_ascii_case(object))
                    .map(|t| t.columns.clone())
                    .or_else(|| {
                        schema
                            .views
                            .iter()
                            .find(|v| v.name.eq_ignore_ascii_case(object))
                            .map(|v| v.columns.clone())
                    })
                    .unwrap_or_default();
                Ok(columns
                    .iter()
                    .map(|c| {
                        MetadataNode::new(&c.name, ObjectKind::Column)
                            .with_detail(c.type_name.clone())
                    })
                    .collect())
            }
        })
    }

    async fn list_all_columns(
        &self,
        profile: &ConnectionProfile,
        database: &str,
    ) -> StudioResult<Vec<BulkColumnRow>> {
        let _ = (profile, database);
        with_backend(|backend| match backend {
            BrowserBackend::Sqlite(conn) => {
                let mut out = Vec::new();
                for table in Self::sqlite_table_names(conn)? {
                    let mut stmt = conn
                        .prepare("SELECT name, type, \"notnull\" FROM pragma_table_info(?1)")
                        .map_err(|e| StudioError::QueryExecution(e.to_string()))?;
                    let cols = stmt
                        .query_map([table.as_str()], |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, String>(1).ok(),
                                row.get::<_, i64>(2).ok(),
                            ))
                        })
                        .map_err(|e| StudioError::QueryExecution(e.to_string()))?
                        .filter_map(Result::ok);
                    for (column, type_name, notnull) in cols {
                        out.push(BulkColumnRow {
                            schema: "main".to_string(),
                            object: table.clone(),
                            column,
                            type_name: type_name.filter(|t| !t.is_empty()),
                            nullable: notnull.map(|n| n == 0),
                            is_identity: false,
                        });
                    }
                }
                Ok(out)
            }
            BrowserBackend::Demo(db) => {
                let schema = db.schema();
                let mut out = Vec::new();
                for t in &schema.tables {
                    for c in &t.columns {
                        out.push(BulkColumnRow {
                            schema: "dbo".to_string(),
                            object: t.name.clone(),
                            column: c.name.clone(),
                            type_name: Some(c.type_name.clone()),
                            nullable: None,
                            is_identity: false,
                        });
                    }
                }
                for v in &schema.views {
                    for c in &v.columns {
                        out.push(BulkColumnRow {
                            schema: "dbo".to_string(),
                            object: v.name.clone(),
                            column: c.name.clone(),
                            type_name: Some(c.type_name.clone()),
                            nullable: None,
                            is_identity: false,
                        });
                    }
                }
                Ok(out)
            }
        })
    }

    async fn list_foreign_keys(
        &self,
        profile: &ConnectionProfile,
        database: &str,
    ) -> StudioResult<Vec<ForeignKeyRow>> {
        let _ = (profile, database);
        with_backend(|backend| match backend {
            BrowserBackend::Sqlite(conn) => {
                let mut out = Vec::new();
                for table in Self::sqlite_table_names(conn)? {
                    let mut stmt = conn
                        .prepare(
                            "SELECT \"table\", \"from\", \"to\" FROM pragma_foreign_key_list(?1)",
                        )
                        .map_err(|e| StudioError::QueryExecution(e.to_string()))?;
                    let fks = stmt
                        .query_map([table.as_str()], |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, Option<String>>(2)?,
                            ))
                        })
                        .map_err(|e| StudioError::QueryExecution(e.to_string()))?
                        .filter_map(Result::ok);
                    for (to_table, from_column, to_column) in fks {
                        // SQLite foreign keys are unnamed; synthesize a stable id
                        // from the table pair so composite columns group together.
                        let constraint_name = format!("{table}_{to_table}");
                        out.push(ForeignKeyRow {
                            constraint_name,
                            from_schema: "main".to_string(),
                            from_table: table.clone(),
                            from_column,
                            to_schema: "main".to_string(),
                            to_column: to_column.unwrap_or_default(),
                            to_table,
                        });
                    }
                }
                Ok(out)
            }
            BrowserBackend::Demo(_) => Ok(Vec::new()),
        })
    }

    async fn list_procedure_parameters(
        &self,
        _profile: &ConnectionProfile,
        _database: &str,
        _schema: &str,
        object: &str,
    ) -> StudioResult<Vec<ProcedureParameter>> {
        with_backend(|backend| match backend {
            BrowserBackend::Sqlite(_) => Ok(Vec::new()),
            BrowserBackend::Demo(db) => Ok(db
                .schema()
                .procedures
                .iter()
                .find(|p| p.name.eq_ignore_ascii_case(object))
                .map(|p| {
                    p.params
                        .iter()
                        .map(|param| ProcedureParameter::new(param.clone(), "nvarchar"))
                        .collect()
                })
                .unwrap_or_default()),
        })
    }

    async fn list_indexes(
        &self,
        _profile: &ConnectionProfile,
        _database: &str,
        _schema: &str,
        table: &str,
    ) -> StudioResult<Vec<MetadataNode>> {
        with_backend(|backend| match backend {
            BrowserBackend::Sqlite(conn) => {
                let mut stmt = conn
                    .prepare("SELECT name, \"unique\" FROM pragma_index_list(?1)")
                    .map_err(|e| StudioError::QueryExecution(e.to_string()))?;
                let indexes = stmt
                    .query_map([table], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                    })
                    .map_err(|e| StudioError::QueryExecution(e.to_string()))?
                    .filter_map(Result::ok);
                Ok(indexes
                    .map(|(name, unique)| {
                        let detail = if unique != 0 { "unique" } else { "non-unique" };
                        MetadataNode::new(name, ObjectKind::Index).with_detail(detail)
                    })
                    .collect())
            }
            BrowserBackend::Demo(_) => Ok(Vec::new()),
        })
    }

    async fn list_triggers(
        &self,
        _profile: &ConnectionProfile,
        _database: &str,
        _schema: &str,
        table: &str,
    ) -> StudioResult<Vec<MetadataNode>> {
        with_backend(|backend| match backend {
            BrowserBackend::Sqlite(conn) => {
                let mut stmt = conn
                    .prepare("SELECT name FROM sqlite_master WHERE type = 'trigger' AND tbl_name = ?1 ORDER BY name")
                    .map_err(|e| StudioError::QueryExecution(e.to_string()))?;
                let names = stmt
                    .query_map([table], |row| row.get::<_, String>(0))
                    .map_err(|e| StudioError::QueryExecution(e.to_string()))?
                    .filter_map(Result::ok);
                Ok(names
                    .map(|name| MetadataNode::new(name, ObjectKind::Trigger))
                    .collect())
            }
            BrowserBackend::Demo(_) => Ok(Vec::new()),
        })
    }

    async fn list_constraints(
        &self,
        _profile: &ConnectionProfile,
        _database: &str,
        _schema: &str,
        _table: &str,
    ) -> StudioResult<Vec<MetadataNode>> {
        // SQLite exposes constraints through table DDL, not a catalog; the
        // browser preview leaves this branch of the tree empty.
        Ok(Vec::new())
    }

    async fn list_statistics(
        &self,
        _profile: &ConnectionProfile,
        _database: &str,
        _schema: &str,
        _table: &str,
    ) -> StudioResult<Vec<MetadataNode>> {
        Ok(Vec::new())
    }

    async fn list_primary_keys(
        &self,
        _profile: &ConnectionProfile,
        _database: &str,
    ) -> StudioResult<Vec<PrimaryKeyRow>> {
        with_backend(|backend| match backend {
            BrowserBackend::Sqlite(conn) => {
                let rows = Self::sqlite_metadata_rows(conn, &MetadataQuery::AllPrimaryKeys)?;
                Ok(rows
                    .into_iter()
                    .filter_map(|row| {
                        let mut cells = row.into_iter();
                        let schema = cell_string(cells.next())?;
                        let table = cell_string(cells.next())?;
                        let column = cell_string(cells.next())?;
                        let key_ordinal = match cells.next() {
                            Some(CellValue::I64(n)) => n,
                            _ => 1,
                        };
                        Some(PrimaryKeyRow {
                            schema,
                            table,
                            column,
                            key_ordinal,
                        })
                    })
                    .collect())
            }
            // The emulated demo schema carries no key metadata.
            BrowserBackend::Demo(_) => Ok(Vec::new()),
        })
    }

    async fn object_definition_text(
        &self,
        _profile: &ConnectionProfile,
        _database: &str,
        query: &MetadataQuery,
    ) -> StudioResult<Option<String>> {
        with_backend(|backend| match backend {
            BrowserBackend::Sqlite(conn) => {
                let rows = Self::sqlite_metadata_rows(conn, query)?;
                Ok(rows
                    .into_iter()
                    .next()
                    .and_then(|row| cell_string(row.into_iter().next())))
            }
            BrowserBackend::Demo(_) => Ok(None),
        })
    }

    async fn run_metadata_query(
        &self,
        _profile: &ConnectionProfile,
        _database: &str,
        query: &MetadataQuery,
    ) -> StudioResult<Vec<Vec<CellValue>>> {
        with_backend(|backend| match backend {
            BrowserBackend::Sqlite(conn) => Self::sqlite_metadata_rows(conn, query),
            // The demo backend emulates a fixed schema, not a catalog; it has
            // nothing to answer arbitrary metadata queries with.
            BrowserBackend::Demo(_) => Err(unsupported("Object metadata for the demo database")),
        })
    }
}

/// The text of a cell, or `None` for NULL / non-text values — the metadata
/// rows this module parses are all text or integer catalog columns.
fn cell_string(cell: Option<CellValue>) -> Option<String> {
    match cell? {
        CellValue::String(s) => Some(s),
        CellValue::I64(n) => Some(n.to_string()),
        CellValue::F64(n) => Some(n.to_string()),
        CellValue::Bool(b) => Some(b.to_string()),
        CellValue::Null | CellValue::Bytes(_) => None,
    }
}

/// Metadata routing for the browser: Postgres-wire profiles get the regular
/// [`DefaultMetadataService`] — its dialect-generated Postgres catalog SQL
/// executes through the browser query executor, which sends it to PGlite (a
/// real Postgres) — while SQLite/demo profiles keep the hand-rolled
/// [`BrowserMetadataService`] answers.
pub struct BrowserMetadataRouter {
    pglite: DefaultMetadataService,
    local: BrowserMetadataService,
}

impl BrowserMetadataRouter {
    pub fn new(queries: Arc<dyn crate::query::QueryExecutor>) -> Self {
        Self {
            pglite: DefaultMetadataService::new(queries),
            local: BrowserMetadataService,
        }
    }
}

/// Delegate one `MetadataService` method to the PGlite-backed or local
/// implementation based on the profile's wire protocol.
macro_rules! route_metadata {
    ($self:ident, $profile:ident, $method:ident($($arg:expr),*)) => {
        if pglite::is_pglite_profile($profile) {
            $self.pglite.$method($profile, $($arg),*).await
        } else {
            $self.local.$method($profile, $($arg),*).await
        }
    };
}

#[async_trait]
impl MetadataService for BrowserMetadataRouter {
    async fn list_schemas(
        &self,
        profile: &ConnectionProfile,
        database: &str,
    ) -> StudioResult<Vec<String>> {
        route_metadata!(self, profile, list_schemas(database))
    }

    async fn list_schemas_with_counts(
        &self,
        profile: &ConnectionProfile,
        database: &str,
    ) -> StudioResult<Vec<(String, Option<u64>)>> {
        route_metadata!(self, profile, list_schemas_with_counts(database))
    }

    async fn list_tables(
        &self,
        profile: &ConnectionProfile,
        database: &str,
        schema: Option<&str>,
    ) -> StudioResult<Vec<MetadataNode>> {
        route_metadata!(self, profile, list_tables(database, schema))
    }

    async fn list_views(
        &self,
        profile: &ConnectionProfile,
        database: &str,
        schema: Option<&str>,
    ) -> StudioResult<Vec<MetadataNode>> {
        route_metadata!(self, profile, list_views(database, schema))
    }

    async fn list_programmable_objects(
        &self,
        profile: &ConnectionProfile,
        database: &str,
        schema: Option<&str>,
    ) -> StudioResult<Vec<MetadataNode>> {
        route_metadata!(self, profile, list_programmable_objects(database, schema))
    }

    async fn list_columns(
        &self,
        profile: &ConnectionProfile,
        database: &str,
        schema: &str,
        object: &str,
    ) -> StudioResult<Vec<MetadataNode>> {
        route_metadata!(self, profile, list_columns(database, schema, object))
    }

    async fn list_all_columns(
        &self,
        profile: &ConnectionProfile,
        database: &str,
    ) -> StudioResult<Vec<BulkColumnRow>> {
        route_metadata!(self, profile, list_all_columns(database))
    }

    async fn list_foreign_keys(
        &self,
        profile: &ConnectionProfile,
        database: &str,
    ) -> StudioResult<Vec<ForeignKeyRow>> {
        route_metadata!(self, profile, list_foreign_keys(database))
    }

    async fn list_primary_keys(
        &self,
        profile: &ConnectionProfile,
        database: &str,
    ) -> StudioResult<Vec<PrimaryKeyRow>> {
        route_metadata!(self, profile, list_primary_keys(database))
    }

    async fn list_procedure_parameters(
        &self,
        profile: &ConnectionProfile,
        database: &str,
        schema: &str,
        object: &str,
    ) -> StudioResult<Vec<ProcedureParameter>> {
        route_metadata!(
            self,
            profile,
            list_procedure_parameters(database, schema, object)
        )
    }

    async fn list_indexes(
        &self,
        profile: &ConnectionProfile,
        database: &str,
        schema: &str,
        table: &str,
    ) -> StudioResult<Vec<MetadataNode>> {
        route_metadata!(self, profile, list_indexes(database, schema, table))
    }

    async fn list_triggers(
        &self,
        profile: &ConnectionProfile,
        database: &str,
        schema: &str,
        table: &str,
    ) -> StudioResult<Vec<MetadataNode>> {
        route_metadata!(self, profile, list_triggers(database, schema, table))
    }

    async fn list_constraints(
        &self,
        profile: &ConnectionProfile,
        database: &str,
        schema: &str,
        table: &str,
    ) -> StudioResult<Vec<MetadataNode>> {
        route_metadata!(self, profile, list_constraints(database, schema, table))
    }

    async fn list_statistics(
        &self,
        profile: &ConnectionProfile,
        database: &str,
        schema: &str,
        table: &str,
    ) -> StudioResult<Vec<MetadataNode>> {
        route_metadata!(self, profile, list_statistics(database, schema, table))
    }

    async fn list_types(
        &self,
        profile: &ConnectionProfile,
        database: &str,
        schema: Option<&str>,
    ) -> StudioResult<Vec<MetadataNode>> {
        route_metadata!(self, profile, list_types(database, schema))
    }

    async fn list_database_triggers(
        &self,
        profile: &ConnectionProfile,
        database: &str,
        schema: Option<&str>,
    ) -> StudioResult<Vec<MetadataNode>> {
        route_metadata!(self, profile, list_database_triggers(database, schema))
    }

    async fn list_sequences(
        &self,
        profile: &ConnectionProfile,
        database: &str,
        schema: Option<&str>,
    ) -> StudioResult<Vec<MetadataNode>> {
        route_metadata!(self, profile, list_sequences(database, schema))
    }

    async fn list_synonyms(
        &self,
        profile: &ConnectionProfile,
        database: &str,
        schema: Option<&str>,
    ) -> StudioResult<Vec<MetadataNode>> {
        route_metadata!(self, profile, list_synonyms(database, schema))
    }

    async fn list_extensions(
        &self,
        profile: &ConnectionProfile,
        database: &str,
    ) -> StudioResult<Vec<MetadataNode>> {
        route_metadata!(self, profile, list_extensions(database))
    }

    async fn list_available_extensions(
        &self,
        profile: &ConnectionProfile,
        database: &str,
    ) -> StudioResult<Vec<MetadataNode>> {
        route_metadata!(self, profile, list_available_extensions(database))
    }

    async fn list_events(
        &self,
        profile: &ConnectionProfile,
        database: &str,
        schema: Option<&str>,
    ) -> StudioResult<Vec<MetadataNode>> {
        route_metadata!(self, profile, list_events(database, schema))
    }

    async fn list_materialized_views(
        &self,
        profile: &ConnectionProfile,
        database: &str,
        schema: Option<&str>,
    ) -> StudioResult<Vec<MetadataNode>> {
        route_metadata!(self, profile, list_materialized_views(database, schema))
    }

    async fn object_definition_text(
        &self,
        profile: &ConnectionProfile,
        database: &str,
        query: &MetadataQuery,
    ) -> StudioResult<Option<String>> {
        route_metadata!(self, profile, object_definition_text(database, query))
    }

    async fn run_metadata_query(
        &self,
        profile: &ConnectionProfile,
        database: &str,
        query: &MetadataQuery,
    ) -> StudioResult<Vec<Vec<CellValue>>> {
        route_metadata!(self, profile, run_metadata_query(database, query))
    }
}

/// Row editing requires the desktop app: the guarded transactional UPDATE
/// path is built on parameterized TDS execution.
pub struct BrowserRowEditingService;

#[async_trait]
impl RowEditingService for BrowserRowEditingService {
    async fn open_session(
        &self,
        _profile: &ConnectionProfile,
        _request: RowEditOpenRequest,
    ) -> StudioResult<RowEditOpenOutcome> {
        Err(unsupported("Row editing"))
    }

    async fn open_insert_session(
        &self,
        _profile: &ConnectionProfile,
        _request: RowEditOpenRequest,
    ) -> StudioResult<RowEditOpenOutcome> {
        Err(unsupported("Row editing"))
    }

    async fn fetch_row(
        &self,
        _profile: &ConnectionProfile,
        _target: &TableTarget,
        _key: &[(String, CellValue)],
    ) -> StudioResult<Option<(Vec<String>, Vec<CellValue>)>> {
        Err(unsupported("Row editing"))
    }

    async fn update_row(
        &self,
        _profile: &ConnectionProfile,
        _request: UpdateRowRequest,
    ) -> StudioResult<UpdateRowOutcome> {
        Err(unsupported("Row editing"))
    }

    async fn fk_search(
        &self,
        _profile: &ConnectionProfile,
        _request: FkSearchRequest,
    ) -> StudioResult<FkSearchResult> {
        Err(unsupported("Foreign-key search"))
    }

    async fn fk_labels(
        &self,
        _profile: &ConnectionProfile,
        _request: FkLabelRequest,
    ) -> StudioResult<FkLabelResult> {
        Err(unsupported("Foreign-key labels"))
    }

    async fn related_rows(
        &self,
        _profile: &ConnectionProfile,
        _request: RelatedRowsRequest,
    ) -> StudioResult<RelatedRowsResult> {
        Err(unsupported("Related-row browsing"))
    }

    async fn insert_row(
        &self,
        _profile: &ConnectionProfile,
        _request: InsertRowRequest,
    ) -> StudioResult<InsertRowOutcome> {
        Err(unsupported("Row editing"))
    }

    async fn delete_row(
        &self,
        _profile: &ConnectionProfile,
        _request: DeleteRowRequest,
    ) -> StudioResult<DeleteRowOutcome> {
        Err(unsupported("Row editing"))
    }

    async fn apply_batch(
        &self,
        _profile: &ConnectionProfile,
        _request: BatchRequest,
    ) -> StudioResult<BatchOutcome> {
        Err(unsupported("Row editing"))
    }
}
