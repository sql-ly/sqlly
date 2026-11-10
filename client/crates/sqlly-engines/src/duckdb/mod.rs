//! # DuckDB driver
//!
//! In-process DuckDB, mirroring the SQLite driver's shape: [`open_client`] /
//! [`execute_on_client`] for the streaming query path,
//! [`query_rows_on_client`] / [`execute_params_on_client`] for parameterized
//! reads and writes, and [`DuckDbTransport`] implementing
//! `crate::driver::SqlTransport` for the lifecycle service.
//!
//! The `duckdb` crate is synchronous, so each [`DuckDbClient`] owns a
//! dedicated OS thread that holds the `duckdb::Connection` for its whole
//! life; async callers talk to it over channels. Query results stream back
//! item-by-item through a bounded channel (`blocking_send` on the thread side
//! gives real backpressure), and [`execute_on_client`] pumps those items
//! through the shared [`crate::driver::QueryStreamer`], so the event/row
//! contract is identical to the other drivers'. Dropping the client (or the
//! item receiver, on cancellation) stops the thread at the next
//! statement/row boundary; DuckDB's own interrupt handle backs Cancel.
//!
//! Scripts are split into statements with the PostgreSQL-family splitter and
//! run one at a time, so every statement reports its own result set or
//! affected-row count (the `duckdb` crate's `prepare` would otherwise execute
//! all but the last statement silently).
//!
//! Values come off the connection as Arrow batches; [`arrow_cell`] maps them
//! into [`CellValue`]: every integer width into `I64` (HUGEINT/UBIGINT values
//! beyond `i64` become digit text), FLOAT/DOUBLE into `F64`, BOOLEAN into
//! `Bool`, BLOB into `Bytes`, and DECIMAL, DATE, TIME, TIMESTAMP, TIMESTAMPTZ,
//! INTERVAL, UUID, ENUM and JSON into text. LIST, ARRAY, STRUCT and MAP
//! values render as JSON text (field order preserved) so the grid, export and
//! copy paths all see one faithful string.

#![forbid(unsafe_code)]

use crate::driver::QueryStreamer;
use ::duckdb::arrow::array::{Array, AsArray, StructArray};
use ::duckdb::arrow::datatypes::{
    ArrowDictionaryKeyType, ArrowNativeType, DataType, Date32Type, Date64Type, Decimal128Type,
    Float16Type, Float32Type, Float64Type, Int8Type, Int16Type, Int32Type, Int64Type,
    IntervalMonthDayNanoType, IntervalUnit, Schema, Time32MillisecondType, Time32SecondType,
    Time64MicrosecondType, Time64NanosecondType, TimeUnit, TimestampMicrosecondType,
    TimestampMillisecondType, TimestampNanosecondType, TimestampSecondType, UInt8Type, UInt16Type,
    UInt32Type, UInt64Type,
};
use ::duckdb::arrow::util::display::{ArrayFormatter, FormatOptions};
use ::duckdb::core::LogicalTypeId;
use sqlly_core::{
    CellValue, ConnectionProfile, DatabaseSummary, EventBus, MaterializedResultSet, ResultColumn,
    RowSender, SqlParam, StudioError, StudioResult,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Weak};
use tokio::sync::{mpsc, oneshot};

pub use crate::driver::{ConnectionService, SqlTransport};

/// Items streamed from the connection thread while a statement batch runs.
#[derive(Debug)]
enum Item {
    ResultSet(Vec<ResultColumn>),
    Row(Vec<CellValue>),
    /// A statement that produced no result set, with its affected-row count.
    Affected(u64),
    Error(String),
}

enum Request {
    Execute {
        sql: String,
        items: mpsc::Sender<Item>,
    },
    QueryRows {
        sql: String,
        params: Vec<SqlParam>,
        respond: oneshot::Sender<Result<Vec<MaterializedResultSet>, String>>,
    },
    ExecuteParams {
        sql: String,
        params: Vec<SqlParam>,
        respond: oneshot::Sender<Result<u64, String>>,
    },
}

/// A live DuckDB "connection": a dedicated thread owning the
/// `duckdb::Connection`, reachable over a request channel. Dropping the
/// client closes the channel and ends the thread.
pub struct DuckDbClient {
    requests: mpsc::Sender<Request>,
    interrupt: Arc<::duckdb::InterruptHandle>,
}

/// Cloneable interruption handle: stops the connection's currently executing
/// statement (DuckDB `interrupt()`), which surfaces as a query error on the
/// running batch.
#[derive(Clone)]
pub struct DuckDbInterruptHandle(Arc<::duckdb::InterruptHandle>);

impl DuckDbInterruptHandle {
    pub fn interrupt(&self) {
        self.0.interrupt();
    }
}

/// Obtain the interruption handle for a client's connection.
pub fn interrupt_handle(client: &DuckDbClient) -> DuckDbInterruptHandle {
    DuckDbInterruptHandle(Arc::clone(&client.interrupt))
}

/// The spelling of an in-memory database in the connection editor.
pub const MEMORY_DATABASE: &str = ":memory:";

/// Resolve the database file path from a profile: `server` (the connection
/// editor's single field for file databases) wins, then `database`.
/// `:memory:` is honored.
///
/// Unlike SQLite, DuckDB's `database` is a real catalog: the explorer scopes
/// its queries by the catalog names `list_databases` reported, and an
/// `ATTACH`ed file is a second catalog. So when both fields are set and
/// `database` is not the path itself, the connection opens the file in
/// `server` and switches to that catalog (see [`catalog_to_use`]).
pub fn database_path(profile: &ConnectionProfile) -> StudioResult<String> {
    let server = profile.server.trim();
    let path = if server.is_empty() {
        profile
            .database
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
    } else {
        Some(server)
    };
    path.map(str::to_string).ok_or_else(|| {
        StudioError::InvalidProfile(
            "DuckDB profile needs a file path (or :memory:) in database/server".to_string(),
        )
    })
}

/// The catalog a freshly opened connection should `USE`: the profile's
/// `database` when it names something other than the file itself.
pub fn catalog_to_use(profile: &ConnectionProfile, path: &str) -> Option<String> {
    let database = profile.database.as_deref().map(str::trim)?;
    if database.is_empty() || database == path.trim() {
        return None;
    }
    Some(database.to_string())
}

fn quote_identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Open a client for the profile's database file. Fails fast if the file
/// cannot be opened (bad directory, locked by another process, not a
/// DuckDB database).
pub async fn open_client(profile: &ConnectionProfile) -> StudioResult<DuckDbClient> {
    let path = database_path(profile)?;
    let catalog = catalog_to_use(profile, &path);
    let (tx, rx) = mpsc::channel::<Request>(16);
    let (ready_tx, ready_rx) = oneshot::channel::<Result<Arc<::duckdb::InterruptHandle>, String>>();
    std::thread::Builder::new()
        .name("sqlly-duckdb".to_string())
        .spawn(move || connection_thread(path, rx, ready_tx))
        .map_err(|e| StudioError::Connection(format!("failed to spawn duckdb thread: {e}")))?;
    let interrupt = ready_rx
        .await
        .map_err(|_| StudioError::Connection("duckdb thread exited before ready".to_string()))?
        .map_err(StudioError::Connection)?;
    let client = DuckDbClient {
        requests: tx,
        interrupt,
    };
    if let Some(catalog) = catalog {
        execute_batch_on_client(&client, &format!("USE {}", quote_identifier(&catalog)))
            .await
            .map_err(|e| {
                StudioError::Connection(format!("cannot switch to catalog '{catalog}': {e}"))
            })?;
    }
    apply_startup_sql(&client, profile).await?;
    Ok(client)
}

// ── Extension downloads ─────────────────────────────────────────────────

/// Whether DuckDB may download and load extensions. The Mac App Store
/// edition turns this off at startup: App Store apps may not download code
/// (App Review guideline 2.5.2), and DuckDB otherwise fetches extension
/// binaries on demand (autoinstall) or on `INSTALL`.
static EXTENSION_DOWNLOADS_ALLOWED: AtomicBool = AtomicBool::new(true);

/// Allow or forbid DuckDB extension downloads for every instance opened
/// from now on. Call once at startup, before any DuckDB connection opens.
pub fn set_extension_downloads_allowed(allowed: bool) {
    EXTENSION_DOWNLOADS_ALLOWED.store(allowed, Ordering::Relaxed);
}

fn extension_downloads_allowed() -> bool {
    EXTENSION_DOWNLOADS_ALLOWED.load(Ordering::Relaxed)
}

/// Settings that control extension download/loading; changing them is
/// refused while downloads are forbidden.
const EXTENSION_SETTINGS: [&str; 6] = [
    "autoinstall_known_extensions",
    "autoload_known_extensions",
    "allow_community_extensions",
    "allow_unsigned_extensions",
    "custom_extension_repository",
    "extension_directory",
];

/// Applied to every new instance while downloads are forbidden.
const NO_EXTENSION_DOWNLOADS_SQL: &str = "SET autoinstall_known_extensions = false; \
     SET autoload_known_extensions = false; \
     SET allow_community_extensions = false;";

// ── External access ─────────────────────────────────────────────────────

/// Whether DuckDB may touch state outside its database file: scanning files
/// (`read_csv`, `read_parquet`, `COPY … FROM`), writing them (`COPY … TO`),
/// attaching other databases, and loading extensions. A proxy-mode engine
/// serving browsers turns this off at startup so a remote user with a DuckDB
/// connection cannot read or write the host's filesystem.
static EXTERNAL_ACCESS_ALLOWED: AtomicBool = AtomicBool::new(true);

/// Allow or forbid external access for every instance opened from now on.
/// Call once at startup, before any DuckDB connection opens: DuckDB lets an
/// instance switch `enable_external_access` off at most once and never back
/// on, so the setting is applied as each instance opens.
pub fn set_external_access_allowed(allowed: bool) {
    EXTERNAL_ACCESS_ALLOWED.store(allowed, Ordering::Relaxed);
}

fn external_access_allowed() -> bool {
    EXTERNAL_ACCESS_ALLOWED.load(Ordering::Relaxed)
}

/// Applied to every new instance while external access is forbidden. The
/// main database file is already open by then, so it keeps working.
const NO_EXTERNAL_ACCESS_SQL: &str = "SET enable_external_access = false;";

/// Apply the process-wide instance policy to a freshly opened instance:
/// extension downloads and external access as configured.
fn apply_instance_policy(
    conn: &::duckdb::Connection,
    downloads_allowed: bool,
    external_access_allowed: bool,
) {
    if !downloads_allowed && let Err(error) = conn.execute_batch(NO_EXTENSION_DOWNLOADS_SQL) {
        tracing::warn!(%error, "could not turn off DuckDB extension downloads");
    }
    if !external_access_allowed && let Err(error) = conn.execute_batch(NO_EXTERNAL_ACCESS_SQL) {
        tracing::warn!(%error, "could not turn off DuckDB external access");
    }
}

/// The error shown when a statement would install or load an extension
/// while downloads are forbidden (the Mac App Store edition, and engines
/// serving browsers through a proxy).
pub const EXTENSION_DOWNLOADS_DISABLED: &str = "Installing or loading DuckDB extensions isn't available in this SQLLY engine: App Store apps may \
     not download code, and an engine serving browsers through a proxy must not fetch code either. \
     DuckDB's built-in features still work.";

pub fn extension_downloads_disabled_message() -> sqlly_core::DisplayMessage {
    sqlly_core::DisplayMessage::new(
        "services.duckdb.extensions_disabled",
        EXTENSION_DOWNLOADS_DISABLED,
    )
}

/// Remove `--` and `/* */` comments (outside quotes) so a comment can't hide
/// the statement keyword. Pure.
fn strip_sql_comments(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    let mut quote: Option<char> = None;
    while let Some(c) = chars.next() {
        if let Some(q) = quote {
            out.push(c);
            if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '\'' | '"' => {
                quote = Some(c);
                out.push(c);
            }
            '-' if chars.peek() == Some(&'-') => {
                for next in chars.by_ref() {
                    if next == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut prev = '\0';
                for next in chars.by_ref() {
                    if prev == '*' && next == '/' {
                        break;
                    }
                    prev = next;
                }
                out.push(' ');
            }
            _ => out.push(c),
        }
    }
    out
}

/// Whether `sql` would install/load an extension or re-enable downloads.
/// Pure; deliberately errs toward refusing.
pub fn installs_or_loads_extension(sql: &str) -> bool {
    let cleaned = strip_sql_comments(sql).to_ascii_lowercase();
    cleaned.split(';').any(|statement| {
        let mut words = statement.split_whitespace();
        let first = words.next().unwrap_or_default();
        let second = words.next().unwrap_or_default();
        match first {
            "install" | "load" => true,
            "force" => second == "install",
            "update" => second == "extensions",
            "set" | "reset" | "pragma" => EXTENSION_SETTINGS
                .iter()
                .any(|setting| statement.contains(setting)),
            _ => false,
        }
    })
}

fn refuse_extension_statements(sql: &str) -> StudioResult<()> {
    if !extension_downloads_allowed() && installs_or_loads_extension(sql) {
        return Err(StudioError::QueryExecution(
            EXTENSION_DOWNLOADS_DISABLED.to_string(),
        ));
    }
    Ok(())
}

/// Run a raw statement batch with no safety policy and no result decoding.
pub async fn execute_batch_on_client(client: &DuckDbClient, sql: &str) -> StudioResult<()> {
    refuse_extension_statements(sql)?;
    let (items_tx, mut items_rx) = mpsc::channel::<Item>(16);
    client
        .requests
        .send(Request::Execute {
            sql: sql.to_string(),
            items: items_tx,
        })
        .await
        .map_err(|_| StudioError::Connection("duckdb connection thread gone".to_string()))?;
    while let Some(item) = items_rx.recv().await {
        if let Item::Error(message) = item {
            return Err(StudioError::QueryExecution(message));
        }
    }
    Ok(())
}

async fn apply_startup_sql(client: &DuckDbClient, profile: &ConnectionProfile) -> StudioResult<()> {
    let Some(batches) = sqlly_core::startup_sql_batches(profile) else {
        return Ok(());
    };
    for batch in batches {
        execute_batch_on_client(client, batch)
            .await
            .map_err(sqlly_core::startup_sql_failed)?;
    }
    Ok(())
}

/// Run a statement batch on an open client, streaming results through the
/// shared driver contract (events on the bus, rows on the bounded channel).
/// Applies the profile's safety policy with `BEGIN`/`ROLLBACK` syntax.
pub async fn execute_on_client(
    client: &DuckDbClient,
    profile: &ConnectionProfile,
    sql: &str,
    events: &EventBus,
    rows_tx: Option<&RowSender>,
) -> StudioResult<()> {
    refuse_extension_statements(sql)?;
    let sql = sqlly_intel::query::safety::apply_connection_policy_for(profile, sql)?;
    let (items_tx, mut items_rx) = mpsc::channel::<Item>(256);
    client
        .requests
        .send(Request::Execute {
            sql,
            items: items_tx,
        })
        .await
        .map_err(|_| StudioError::Connection("duckdb connection thread gone".to_string()))?;

    let mut streamer = QueryStreamer::begin(events, rows_tx)?;
    let mut failure: Option<StudioError> = None;
    while let Some(item) = items_rx.recv().await {
        match item {
            Item::ResultSet(columns) => streamer.begin_result_set(columns).await?,
            Item::Row(cells) => {
                if !streamer.push_row(cells).await? {
                    // Consumer gone: drop the receiver so the connection
                    // thread stops producing, then stop draining.
                    break;
                }
            }
            Item::Affected(n) => streamer.statement_completed(n)?,
            Item::Error(message) => {
                failure = Some(StudioError::QueryExecution(message));
                break;
            }
        }
    }
    drop(items_rx);
    match failure {
        Some(error) => Err(error),
        None => streamer.finish().await,
    }
}

/// Parameterized, materialized read on an open client (`?` or `$1`
/// placeholders). Small targeted reads, no safety policy applied.
pub async fn query_rows_on_client(
    client: &DuckDbClient,
    sql: &str,
    params: &[SqlParam],
) -> StudioResult<Vec<MaterializedResultSet>> {
    let (tx, rx) = oneshot::channel();
    client
        .requests
        .send(Request::QueryRows {
            sql: sql.to_string(),
            params: params.to_vec(),
            respond: tx,
        })
        .await
        .map_err(|_| StudioError::Connection("duckdb connection thread gone".to_string()))?;
    rx.await
        .map_err(|_| StudioError::Connection("duckdb connection thread gone".to_string()))?
        .map_err(StudioError::QueryExecution)
}

/// Parameterized non-query on an open client; returns affected rows.
pub async fn execute_params_on_client(
    client: &DuckDbClient,
    sql: &str,
    params: &[SqlParam],
) -> StudioResult<u64> {
    let (tx, rx) = oneshot::channel();
    client
        .requests
        .send(Request::ExecuteParams {
            sql: sql.to_string(),
            params: params.to_vec(),
            respond: tx,
        })
        .await
        .map_err(|_| StudioError::Connection("duckdb connection thread gone".to_string()))?;
    rx.await
        .map_err(|_| StudioError::Connection("duckdb connection thread gone".to_string()))?
        .map_err(StudioError::QueryExecution)
}

/// `SqlTransport` implementation for DuckDB, usable with the shared
/// [`ConnectionService`].
#[derive(Debug, Clone, Default)]
pub struct DuckDbTransport;

#[async_trait::async_trait]
impl SqlTransport for DuckDbTransport {
    async fn test_connection(&self, profile: &ConnectionProfile) -> StudioResult<()> {
        let _client = open_client(profile).await?;
        Ok(())
    }

    async fn list_databases(
        &self,
        profile: &ConnectionProfile,
    ) -> StudioResult<Vec<DatabaseSummary>> {
        let client = open_client(profile).await?;
        let sets = query_rows_on_client(
            &client,
            "SELECT database_name FROM duckdb_databases() WHERE NOT internal ORDER BY database_name",
            &[],
        )
        .await?;
        let mut out = Vec::new();
        for set in sets {
            for row in set.rows {
                if let Some(CellValue::String(name)) = row.first() {
                    out.push(DatabaseSummary {
                        name: name.clone(),
                        state: "ONLINE".to_string(),
                        compatibility_level: None,
                    });
                }
            }
        }
        Ok(out)
    }

    async fn execute_query(
        &self,
        profile: &ConnectionProfile,
        sql: &str,
        events: &EventBus,
        rows_tx: Option<&RowSender>,
    ) -> StudioResult<()> {
        let client = open_client(profile).await?;
        execute_on_client(&client, profile, sql, events, rows_tx).await
    }
}

// ── One database instance per path ──────────────────────────────────────

/// The root connection that owns a database instance. Every client is a
/// `try_clone` of it, so all of them share one instance.
struct SharedDatabase {
    /// Taken (and so closed) in `Drop`, before the registry is told the
    /// instance is gone.
    root: Option<parking_lot::Mutex<::duckdb::Connection>>,
    /// Registry key for file databases; `None` for the `:memory:` scratch.
    key: Option<PathBuf>,
}

impl SharedDatabase {
    fn new(conn: ::duckdb::Connection, key: Option<PathBuf>) -> Self {
        apply_instance_policy(
            &conn,
            extension_downloads_allowed(),
            external_access_allowed(),
        );
        Self {
            root: Some(parking_lot::Mutex::new(conn)),
            key,
        }
    }

    fn root(&self) -> &parking_lot::Mutex<::duckdb::Connection> {
        self.root
            .as_ref()
            .expect("root connection lives until drop")
    }
}

impl Drop for SharedDatabase {
    /// Close the instance first, then drop its registry entry and wake any
    /// `acquire` waiting to reopen the file. Until this finishes the file is
    /// still open here: a second instance opened meanwhile would write the
    /// same file concurrently (index corruption on Linux, a sharing
    /// violation on Windows).
    fn drop(&mut self) {
        drop(self.root.take());
        if let Some(key) = &self.key {
            let mut registry = REGISTRY.lock();
            if registry
                .files
                .get(key)
                .is_some_and(|weak| weak.strong_count() == 0)
            {
                registry.files.remove(key);
            }
            drop(registry);
            INSTANCE_CLOSED.notify_all();
        }
    }
}

#[derive(Default)]
struct Registry {
    /// File databases, released once the last client drops: another process
    /// (the DuckDB CLI, a notebook) can then open the file.
    files: HashMap<PathBuf, Weak<SharedDatabase>>,
    /// The process-lifetime `:memory:` scratch database.
    memory: Option<Arc<SharedDatabase>>,
}

static REGISTRY: LazyLock<parking_lot::Mutex<Registry>> =
    LazyLock::new(|| parking_lot::Mutex::new(Registry::default()));

/// Signalled whenever a file instance finishes closing (see
/// `SharedDatabase::drop`).
static INSTANCE_CLOSED: parking_lot::Condvar = parking_lot::Condvar::new();

/// How long `acquire` waits for this process's previous instance of a file to
/// finish closing before opening anyway.
const CLOSE_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// Open (or join) the database instance for `path` and hand back a private
/// connection on it plus the instance it must keep alive.
///
/// DuckDB's C API creates a fresh instance per open, and same-process file
/// locks never conflict with each other, so opening one file twice from the
/// app (query, catalog, row editing) would silently yield two instances
/// writing the same file. The registry keeps one root per canonical path and
/// clones connections off it. `:memory:` is one shared scratch database for
/// the whole process, so a table created in one editor tab is visible to
/// the next query, the explorer, and IntelliSense.
fn acquire(path: &str) -> ::duckdb::Result<(Arc<SharedDatabase>, ::duckdb::Connection)> {
    let mut registry = REGISTRY.lock();
    let shared = if path == MEMORY_DATABASE {
        match &registry.memory {
            Some(shared) => Arc::clone(shared),
            None => {
                let shared = Arc::new(SharedDatabase::new(
                    ::duckdb::Connection::open_in_memory()?,
                    None,
                ));
                registry.memory = Some(Arc::clone(&shared));
                shared
            }
        }
    } else {
        let key = registry_key(path);
        // A dead entry means this process's previous instance of the file is
        // still closing on its connection thread: wait for it to finish, so
        // two instances never hold the same file at once.
        let deadline = std::time::Instant::now() + CLOSE_WAIT;
        while registry
            .files
            .get(&key)
            .is_some_and(|weak| weak.strong_count() == 0)
        {
            if INSTANCE_CLOSED
                .wait_until(&mut registry, deadline)
                .timed_out()
            {
                break;
            }
        }
        match registry.files.get(&key).and_then(Weak::upgrade) {
            Some(shared) => shared,
            None => {
                let own_instance_closing = registry.files.contains_key(&key);
                let shared = Arc::new(SharedDatabase::new(
                    open_file(path, own_instance_closing)?,
                    Some(key.clone()),
                ));
                // Dead entries are removed by their own instance's `Drop`
                // once it has closed; sweeping them here would let a reopen
                // skip the wait for an instance that is still closing.
                registry.files.insert(key, Arc::downgrade(&shared));
                shared
            }
        }
    };
    // Release the registry before anything can drop `shared`: its `Drop`
    // takes the registry lock.
    drop(registry);
    let conn = shared.root().lock().try_clone()?;
    Ok((shared, conn))
}

/// Open a database file. When this process's own previous instance of the
/// same file is still closing (its last client just dropped and the
/// connection thread is releasing it), Windows reports the file as locked by
/// "another process" until that close finishes, so wait briefly for it.
/// Any other failure, including a lock held by a different program, is
/// returned at once.
fn open_file(path: &str, own_instance_closing: bool) -> ::duckdb::Result<::duckdb::Connection> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match ::duckdb::Connection::open(path) {
            Ok(conn) => return Ok(conn),
            Err(err)
                if own_instance_closing
                    && is_file_lock_conflict(&err.to_string())
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(err) => return Err(err),
        }
    }
}

/// Whether a DuckDB open error is a file-lock conflict (Windows sharing
/// violation or DuckDB's own lock), as opposed to a missing file or a
/// corrupt database.
fn is_file_lock_conflict(message: &str) -> bool {
    message.contains("being used by another process")
        || message.contains("Could not set lock")
        || message.contains("Conflicting lock")
}

/// Whether this process currently holds the DuckDB file at `path` open (a
/// live client shares its instance). DuckDB's file lock is per process, so
/// another program — the DuckDB CLI — cannot open the file meanwhile.
pub fn file_in_use(path: &str) -> bool {
    let key = registry_key(path);
    REGISTRY
        .lock()
        .files
        .get(&key)
        .is_some_and(|weak| weak.strong_count() > 0)
}

/// Canonical registry key: the resolved path when the file exists, else the
/// resolved parent directory plus the file name, else the path as given.
fn registry_key(path: &str) -> PathBuf {
    let raw = Path::new(path);
    if let Ok(resolved) = raw.canonicalize() {
        return resolved;
    }
    match (raw.parent(), raw.file_name()) {
        (Some(parent), Some(name)) if !parent.as_os_str().is_empty() => parent
            .canonicalize()
            .map(|dir| dir.join(name))
            .unwrap_or_else(|_| raw.to_path_buf()),
        _ => raw.to_path_buf(),
    }
}

// ── Connection thread ────────────────────────────────────────────────────

fn connection_thread(
    path: String,
    mut requests: mpsc::Receiver<Request>,
    ready: oneshot::Sender<Result<Arc<::duckdb::InterruptHandle>, String>>,
) {
    let (_shared, conn) = match acquire(&path) {
        Ok(opened) => opened,
        Err(e) => {
            let _ = ready.send(Err(format!("cannot open '{path}': {e}")));
            return;
        }
    };
    if let Err(e) = conn.query_row("SELECT 1", [], |_| Ok(())) {
        let _ = ready.send(Err(format!("'{path}' is not a usable database: {e}")));
        return;
    }
    let _ = ready.send(Ok(conn.interrupt_handle()));

    while let Some(request) = requests.blocking_recv() {
        match request {
            Request::Execute { sql, items } => run_batch(&conn, &sql, &items),
            Request::QueryRows {
                sql,
                params,
                respond,
            } => {
                let _ = respond.send(run_query_rows(&conn, &sql, &params));
            }
            Request::ExecuteParams {
                sql,
                params,
                respond,
            } => {
                let _ = respond.send(run_execute_params(&conn, &sql, &params));
            }
        }
    }
}

/// Run a multi-statement batch, streaming items. A send failure means the
/// async side hung up (cancellation) — stop quietly; an engine error is sent
/// and ends the batch.
fn run_batch(conn: &::duckdb::Connection, sql: &str, items: &mpsc::Sender<Item>) {
    // `seclex::execution_units` is the one split the safety gates judge, so
    // DuckDB receives exactly the statements that were approved (leading
    // comments stripped the way DuckDB nests them).
    for unit in sqlly_lang::seclex::execution_units(sql, sqlly_core::DatabaseType::DuckDb) {
        if run_statement(conn, &unit.text, items).is_err() {
            return;
        }
    }
}

/// Execute one statement and stream its outcome. `Err(())` means stop the
/// batch: either the consumer went away or an error was already reported.
fn run_statement(
    conn: &::duckdb::Connection,
    text: &str,
    items: &mpsc::Sender<Item>,
) -> Result<(), ()> {
    let mut stmt = conn.prepare(text).map_err(|e| report(items, e))?;
    let changed = stmt.execute([]).map_err(|e| report(items, e))?;
    let schema = stmt.schema();
    if !is_result_set(&schema, text) {
        let affected = affected_from_report(&stmt).unwrap_or(changed as u64);
        return items
            .blocking_send(Item::Affected(affected))
            .map_err(|_| ());
    }
    let (columns, hints) = describe_columns(&stmt);
    items
        .blocking_send(Item::ResultSet(columns))
        .map_err(|_| ())?;
    let options = format_options();
    loop {
        match stmt.step() {
            Ok(Some(batch)) => {
                for row in 0..batch.len() {
                    let cells = batch_row(&batch, row, &hints, &options);
                    items.blocking_send(Item::Row(cells)).map_err(|_| ())?;
                }
            }
            Ok(None) => return Ok(()),
            Err(e) => {
                report(items, e);
                return Err(());
            }
        }
    }
}

fn report(items: &mpsc::Sender<Item>, error: ::duckdb::Error) {
    let _ = items.blocking_send(Item::Error(error.to_string()));
}

fn batch_row(
    batch: &StructArray,
    row: usize,
    hints: &[Option<LogicalTypeId>],
    options: &FormatOptions<'_>,
) -> Vec<CellValue> {
    batch
        .columns()
        .iter()
        .zip(hints)
        .map(|(column, hint)| arrow_cell(column.as_ref(), row, *hint, options))
        .collect()
}

/// Statement keywords whose single `Count` column is a genuine result, not
/// DuckDB's affected-row report.
const QUERY_KEYWORDS: [&str; 14] = [
    "SELECT",
    "WITH",
    "VALUES",
    "FROM",
    "SHOW",
    "DESCRIBE",
    "DESC",
    "SUMMARIZE",
    "PRAGMA",
    "EXPLAIN",
    "CALL",
    "TABLE",
    "PIVOT",
    "UNPIVOT",
];

/// DuckDB answers INSERT/UPDATE/DELETE/COPY/`CREATE … AS` with a one-column
/// `Count` (BIGINT) result set, SET/RESET with a zero-row `Success`
/// (BOOLEAN) column, and most DDL with no columns at all. Only a statement
/// that starts with a query keyword keeps such a report-shaped result as a
/// grid (`SELECT count(*) AS "Count"`).
fn is_result_set(schema: &Schema, text: &str) -> bool {
    let fields = schema.fields();
    if fields.is_empty() {
        return false;
    }
    let report_shape = fields.len() == 1
        && ((fields[0].name() == "Count" && *fields[0].data_type() == DataType::Int64)
            || (fields[0].name() == "Success" && *fields[0].data_type() == DataType::Boolean));
    !report_shape || starts_with_query_keyword(text)
}

/// The affected-row count from a `Count`-shaped report, read from the row
/// itself: `duckdb_rows_changed` stays 0 for `CREATE TABLE … AS` and `COPY`,
/// whose `Count` row still carries the real number.
fn affected_from_report(stmt: &::duckdb::Statement<'_>) -> Option<u64> {
    let batch = stmt.step().ok().flatten()?;
    if batch.num_columns() != 1 || batch.len() == 0 {
        return None;
    }
    let column = batch.column(0);
    if column.is_null(0) {
        return None;
    }
    let count = column.as_primitive_opt::<Int64Type>()?.value(0);
    u64::try_from(count).ok()
}

/// Skip leading whitespace and comments as DuckDB reads them.
fn strip_leading_trivia(text: &str) -> &str {
    sqlly_lang::seclex::strip_leading_trivia(text, sqlly_lang::seclex::LexEngine::DuckDb)
}

fn leading_keyword(text: &str) -> String {
    strip_leading_trivia(text)
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect::<String>()
        .to_ascii_uppercase()
}

fn starts_with_query_keyword(text: &str) -> bool {
    let keyword = leading_keyword(text);
    QUERY_KEYWORDS.contains(&keyword.as_str())
}

/// Result columns (name + DuckDB type name) and the per-column logical type
/// hint the cell converter uses to tell HUGEINT from DECIMAL(38,0).
fn describe_columns(
    stmt: &::duckdb::Statement<'_>,
) -> (Vec<ResultColumn>, Vec<Option<LogicalTypeId>>) {
    let count = stmt.column_count();
    let mut columns = Vec::with_capacity(count);
    let mut hints = Vec::with_capacity(count);
    for idx in 0..count {
        let name = stmt
            .column_name(idx)
            .map(|n| n.to_string())
            .unwrap_or_else(|_| format!("column{idx}"));
        let handle = stmt.column_logical_type(idx);
        let id = handle.try_id().ok();
        let data_type = match handle.get_alias() {
            Some(alias) => alias.to_ascii_lowercase(),
            None => type_name(id, &handle),
        };
        columns.push(ResultColumn {
            name,
            data_type,
            nullable: true,
        });
        hints.push(id);
    }
    (columns, hints)
}

/// DuckDB's own spelling of a logical type, lower-cased.
fn type_name(id: Option<LogicalTypeId>, handle: &::duckdb::core::LogicalTypeHandle) -> String {
    let Some(id) = id else {
        return "unknown".to_string();
    };
    match id {
        LogicalTypeId::Boolean => "boolean",
        LogicalTypeId::Tinyint => "tinyint",
        LogicalTypeId::Smallint => "smallint",
        LogicalTypeId::Integer => "integer",
        LogicalTypeId::Bigint => "bigint",
        LogicalTypeId::UTinyint => "utinyint",
        LogicalTypeId::USmallint => "usmallint",
        LogicalTypeId::UInteger => "uinteger",
        LogicalTypeId::UBigint => "ubigint",
        LogicalTypeId::Hugeint => "hugeint",
        LogicalTypeId::UHugeint => "uhugeint",
        LogicalTypeId::Float => "float",
        LogicalTypeId::Double => "double",
        LogicalTypeId::Decimal => {
            return format!(
                "decimal({},{})",
                handle.decimal_width(),
                handle.decimal_scale()
            );
        }
        LogicalTypeId::Varchar => "varchar",
        LogicalTypeId::Blob => "blob",
        LogicalTypeId::Date => "date",
        LogicalTypeId::Time => "time",
        LogicalTypeId::TimeTZ => "time with time zone",
        LogicalTypeId::TimeNs => "time_ns",
        LogicalTypeId::Timestamp => "timestamp",
        LogicalTypeId::TimestampS => "timestamp_s",
        LogicalTypeId::TimestampMs => "timestamp_ms",
        LogicalTypeId::TimestampNs => "timestamp_ns",
        LogicalTypeId::TimestampTZ => "timestamp with time zone",
        LogicalTypeId::Interval => "interval",
        LogicalTypeId::Enum => "enum",
        LogicalTypeId::List => "list",
        LogicalTypeId::Array => "array",
        LogicalTypeId::Struct => "struct",
        LogicalTypeId::Map => "map",
        LogicalTypeId::Union => "union",
        LogicalTypeId::Uuid => "uuid",
        LogicalTypeId::Bit => "bit",
        LogicalTypeId::Bignum => "bignum",
        LogicalTypeId::Geometry => "geometry",
        LogicalTypeId::Variant => "variant",
        LogicalTypeId::SqlNull => "null",
        LogicalTypeId::Any => "any",
        LogicalTypeId::StringLiteral => "varchar",
        LogicalTypeId::IntegerLiteral => "integer",
        LogicalTypeId::Invalid | LogicalTypeId::Unsupported => "unknown",
        // `LogicalTypeId` is non-exhaustive: a type added by a newer DuckDB
        // still gets a name.
        _ => "unknown",
    }
    .to_string()
}

fn run_query_rows(
    conn: &::duckdb::Connection,
    sql: &str,
    params: &[SqlParam],
) -> Result<Vec<MaterializedResultSet>, String> {
    let mut stmt = conn.prepare(sql).map_err(|e| e.to_string())?;
    let values = params.iter().map(param_to_value).collect::<Vec<_>>();
    stmt.execute(::duckdb::params_from_iter(values))
        .map_err(|e| e.to_string())?;
    let (columns, hints) = describe_columns(&stmt);
    let options = format_options();
    let mut rows = Vec::new();
    loop {
        match stmt.step() {
            Ok(Some(batch)) => {
                for row in 0..batch.len() {
                    rows.push(batch_row(&batch, row, &hints, &options));
                }
            }
            Ok(None) => break,
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(vec![MaterializedResultSet { columns, rows }])
}

fn run_execute_params(
    conn: &::duckdb::Connection,
    sql: &str,
    params: &[SqlParam],
) -> Result<u64, String> {
    let values = params.iter().map(param_to_value).collect::<Vec<_>>();
    conn.execute(sql, ::duckdb::params_from_iter(values))
        .map(|n| n as u64)
        .map_err(|e| e.to_string())
}

fn param_to_value(param: &SqlParam) -> ::duckdb::types::Value {
    use ::duckdb::types::Value;
    match param {
        SqlParam::Null => Value::Null,
        SqlParam::Bool(b) => Value::Boolean(*b),
        SqlParam::I64(n) => Value::BigInt(*n),
        SqlParam::F64(f) => Value::Double(*f),
        SqlParam::String(s) => Value::Text(s.clone()),
        SqlParam::Bytes(b) => Value::Blob(b.clone()),
    }
}

// ── Arrow → CellValue ───────────────────────────────────────────────────

/// Fallback formatting for the Arrow types with no dedicated arm below.
fn format_options() -> FormatOptions<'static> {
    FormatOptions::new().with_null("NULL")
}

/// One Arrow cell as a [`CellValue`]. `hint` is the DuckDB logical type of
/// the top-level column (Arrow gives HUGEINT and DECIMAL(38,0) the same
/// physical shape); nested values carry no hint.
fn arrow_cell(
    array: &dyn Array,
    idx: usize,
    hint: Option<LogicalTypeId>,
    options: &FormatOptions<'_>,
) -> CellValue {
    if array.is_null(idx) {
        return CellValue::Null;
    }
    match array.data_type() {
        DataType::Null => CellValue::Null,
        DataType::Boolean => CellValue::Bool(array.as_boolean().value(idx)),
        DataType::Int8 => CellValue::I64(array.as_primitive::<Int8Type>().value(idx).into()),
        DataType::Int16 => CellValue::I64(array.as_primitive::<Int16Type>().value(idx).into()),
        DataType::Int32 => CellValue::I64(array.as_primitive::<Int32Type>().value(idx).into()),
        DataType::Int64 => CellValue::I64(array.as_primitive::<Int64Type>().value(idx)),
        DataType::UInt8 => CellValue::I64(array.as_primitive::<UInt8Type>().value(idx).into()),
        DataType::UInt16 => CellValue::I64(array.as_primitive::<UInt16Type>().value(idx).into()),
        DataType::UInt32 => CellValue::I64(array.as_primitive::<UInt32Type>().value(idx).into()),
        DataType::UInt64 => {
            let v = array.as_primitive::<UInt64Type>().value(idx);
            i64::try_from(v)
                .map(CellValue::I64)
                .unwrap_or_else(|_| CellValue::String(v.to_string()))
        }
        DataType::Float16 => {
            CellValue::F64(f64::from(array.as_primitive::<Float16Type>().value(idx)))
        }
        DataType::Float32 => {
            CellValue::F64(f64::from(array.as_primitive::<Float32Type>().value(idx)))
        }
        DataType::Float64 => CellValue::F64(array.as_primitive::<Float64Type>().value(idx)),
        DataType::Decimal128(_, scale) => {
            let v = array.as_primitive::<Decimal128Type>().value(idx);
            if matches!(hint, Some(LogicalTypeId::Hugeint | LogicalTypeId::UHugeint)) {
                i64::try_from(v)
                    .map(CellValue::I64)
                    .unwrap_or_else(|_| CellValue::String(v.to_string()))
            } else {
                CellValue::String(decimal_text(v, *scale))
            }
        }
        DataType::Utf8 => CellValue::String(array.as_string::<i32>().value(idx).to_string()),
        DataType::LargeUtf8 => CellValue::String(array.as_string::<i64>().value(idx).to_string()),
        DataType::Utf8View => CellValue::String(array.as_string_view().value(idx).to_string()),
        DataType::Binary => CellValue::Bytes(array.as_binary::<i32>().value(idx).to_vec()),
        DataType::LargeBinary => CellValue::Bytes(array.as_binary::<i64>().value(idx).to_vec()),
        DataType::BinaryView => CellValue::Bytes(array.as_binary_view().value(idx).to_vec()),
        DataType::FixedSizeBinary(_) => {
            CellValue::Bytes(array.as_fixed_size_binary().value(idx).to_vec())
        }
        DataType::Date32 => {
            CellValue::String(date32_text(array.as_primitive::<Date32Type>().value(idx)))
        }
        DataType::Date64 => {
            CellValue::String(date64_text(array.as_primitive::<Date64Type>().value(idx)))
        }
        DataType::Time32(TimeUnit::Second) => CellValue::String(time_text(
            i64::from(array.as_primitive::<Time32SecondType>().value(idx)) * 1_000_000_000,
        )),
        DataType::Time32(TimeUnit::Millisecond) => CellValue::String(time_text(
            i64::from(array.as_primitive::<Time32MillisecondType>().value(idx)) * 1_000_000,
        )),
        DataType::Time64(TimeUnit::Microsecond) => CellValue::String(time_text(
            array.as_primitive::<Time64MicrosecondType>().value(idx) * 1_000,
        )),
        DataType::Time64(TimeUnit::Nanosecond) => CellValue::String(time_text(
            array.as_primitive::<Time64NanosecondType>().value(idx),
        )),
        DataType::Timestamp(unit, tz) => {
            let raw = match unit {
                TimeUnit::Second => array.as_primitive::<TimestampSecondType>().value(idx),
                TimeUnit::Millisecond => {
                    array.as_primitive::<TimestampMillisecondType>().value(idx)
                }
                TimeUnit::Microsecond => {
                    array.as_primitive::<TimestampMicrosecondType>().value(idx)
                }
                TimeUnit::Nanosecond => array.as_primitive::<TimestampNanosecondType>().value(idx),
            };
            CellValue::String(timestamp_text(raw, *unit, tz.is_some()))
        }
        DataType::Interval(IntervalUnit::MonthDayNano) => {
            let v = array.as_primitive::<IntervalMonthDayNanoType>().value(idx);
            CellValue::String(interval_text(v.months, v.days, v.nanoseconds))
        }
        DataType::Dictionary(_, _) => match dictionary_entry(array, idx) {
            Some((values, key)) => arrow_cell(values, key, hint, options),
            None => CellValue::String(formatted(array, idx, options)),
        },
        DataType::List(_)
        | DataType::LargeList(_)
        | DataType::FixedSizeList(_, _)
        | DataType::Struct(_)
        | DataType::Map(_, _) => CellValue::String(arrow_json(array, idx, options).render()),
        _ => CellValue::String(formatted(array, idx, options)),
    }
}

/// Arrow's own display for the types nothing above handles (DECIMAL256,
/// UNION, run-end encoded, …).
fn formatted(array: &dyn Array, idx: usize, options: &FormatOptions<'_>) -> String {
    match ArrayFormatter::try_new(array, options) {
        Ok(formatter) => formatter.value(idx).to_string(),
        Err(e) => format!("<unsupported value: {e}>"),
    }
}

fn dict<K: ArrowDictionaryKeyType>(array: &dyn Array, idx: usize) -> Option<(&dyn Array, usize)> {
    let dictionary = array.as_dictionary_opt::<K>()?;
    Some((
        dictionary.values().as_ref(),
        dictionary.keys().value(idx).as_usize(),
    ))
}

/// The dictionary (ENUM) values array and the row's index into it.
fn dictionary_entry(array: &dyn Array, idx: usize) -> Option<(&dyn Array, usize)> {
    dict::<UInt8Type>(array, idx)
        .or_else(|| dict::<UInt16Type>(array, idx))
        .or_else(|| dict::<UInt32Type>(array, idx))
        .or_else(|| dict::<UInt64Type>(array, idx))
        .or_else(|| dict::<Int8Type>(array, idx))
        .or_else(|| dict::<Int16Type>(array, idx))
        .or_else(|| dict::<Int32Type>(array, idx))
        .or_else(|| dict::<Int64Type>(array, idx))
}

/// Render a scaled integer as decimal text: `12345` at scale 3 → `12.345`.
fn decimal_text(value: i128, scale: i8) -> String {
    let sign = if value < 0 { "-" } else { "" };
    let digits = value.unsigned_abs().to_string();
    if scale <= 0 {
        return format!(
            "{sign}{digits}{}",
            "0".repeat(scale.unsigned_abs() as usize)
        );
    }
    let scale = scale as usize;
    let padded = if digits.len() <= scale {
        format!("{}{digits}", "0".repeat(scale - digits.len() + 1))
    } else {
        digits
    };
    let (whole, fraction) = padded.split_at(padded.len() - scale);
    format!("{sign}{whole}.{fraction}")
}

fn date32_text(days: i32) -> String {
    match days {
        i32::MAX => "infinity".to_string(),
        i32::MIN => "-infinity".to_string(),
        _ => chrono::DateTime::from_timestamp(i64::from(days) * 86_400, 0)
            .map(|dt| dt.date_naive().format("%Y-%m-%d").to_string())
            .unwrap_or_else(|| days.to_string()),
    }
}

fn date64_text(millis: i64) -> String {
    chrono::DateTime::from_timestamp_millis(millis)
        .map(|dt| dt.date_naive().format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| millis.to_string())
}

fn time_text(nanos_since_midnight: i64) -> String {
    let secs = nanos_since_midnight.div_euclid(1_000_000_000);
    let nanos = nanos_since_midnight.rem_euclid(1_000_000_000);
    u32::try_from(secs)
        .ok()
        .and_then(|s| chrono::NaiveTime::from_num_seconds_from_midnight_opt(s, nanos as u32))
        .map(|t| t.format("%H:%M:%S%.f").to_string())
        .unwrap_or_else(|| nanos_since_midnight.to_string())
}

/// DuckDB TIMESTAMP text (`2024-02-29 13:45:30.123456`); TIMESTAMPTZ values
/// are UTC instants and render with an explicit `+00:00`.
fn timestamp_text(raw: i64, unit: TimeUnit, with_zone: bool) -> String {
    let parsed = match unit {
        TimeUnit::Second => chrono::DateTime::from_timestamp(raw, 0),
        TimeUnit::Millisecond => chrono::DateTime::from_timestamp_millis(raw),
        TimeUnit::Microsecond => chrono::DateTime::from_timestamp_micros(raw),
        TimeUnit::Nanosecond => Some(chrono::DateTime::from_timestamp_nanos(raw)),
    };
    match parsed {
        Some(dt) if with_zone => dt.format("%Y-%m-%d %H:%M:%S%.f+00:00").to_string(),
        Some(dt) => dt.naive_utc().format("%Y-%m-%d %H:%M:%S%.f").to_string(),
        None => match raw {
            i64::MAX => "infinity".to_string(),
            i64::MIN => "-infinity".to_string(),
            _ => raw.to_string(),
        },
    }
}

/// PostgreSQL-style interval text: `1 year 2 months 3 days 04:05:06.5`.
fn interval_text(months: i32, days: i32, nanos: i64) -> String {
    let mut parts = Vec::new();
    let years = months / 12;
    let months = months % 12;
    let plural = |n: i32, unit: &str| {
        if n.abs() == 1 {
            format!("{n} {unit}")
        } else {
            format!("{n} {unit}s")
        }
    };
    if years != 0 {
        parts.push(plural(years, "year"));
    }
    if months != 0 {
        parts.push(plural(months, "month"));
    }
    if days != 0 {
        parts.push(plural(days, "day"));
    }
    if nanos != 0 || parts.is_empty() {
        let sign = if nanos < 0 { "-" } else { "" };
        let total = nanos.unsigned_abs();
        let hours = total / 3_600_000_000_000;
        let minutes = (total / 60_000_000_000) % 60;
        let seconds = (total / 1_000_000_000) % 60;
        let fraction = total % 1_000_000_000;
        let mut time = format!("{sign}{hours:02}:{minutes:02}:{seconds:02}");
        if fraction != 0 {
            let digits = format!("{fraction:09}");
            time.push('.');
            time.push_str(digits.trim_end_matches('0'));
        }
        parts.push(time);
    }
    parts.join(" ")
}

/// A tiny order-preserving JSON tree: `serde_json::Value` sorts object keys
/// unless the `preserve_order` feature is on, and STRUCT field order is
/// part of the value.
#[derive(Debug, Clone, PartialEq)]
enum Json {
    Null,
    Bool(bool),
    /// Pre-rendered numeric literal.
    Number(String),
    Text(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl Json {
    fn render(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }

    fn write(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Json::Number(n) => out.push_str(n),
            Json::Text(s) => out.push_str(&serde_json::to_string(s).unwrap_or_default()),
            Json::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write(out);
                }
                out.push(']');
            }
            Json::Object(fields) => {
                out.push('{');
                for (i, (key, value)) in fields.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push_str(&serde_json::to_string(key).unwrap_or_default());
                    out.push(':');
                    value.write(out);
                }
                out.push('}');
            }
        }
    }

    /// Object keys must be strings; other key types keep their rendering.
    fn into_key(self) -> String {
        match self {
            Json::Text(s) => s,
            other => other.render(),
        }
    }
}

fn float_json(v: f64) -> Json {
    if v.is_finite() {
        Json::Number(v.to_string())
    } else {
        Json::Text(v.to_string())
    }
}

/// One Arrow cell as JSON: scalars by type, LIST/ARRAY as arrays, STRUCT as
/// objects (field order kept), MAP as objects keyed by the rendered key.
fn arrow_json(array: &dyn Array, idx: usize, options: &FormatOptions<'_>) -> Json {
    if array.is_null(idx) {
        return Json::Null;
    }
    match array.data_type() {
        DataType::List(_) => {
            let values = array.as_list::<i32>().value(idx);
            Json::Array(
                (0..values.len())
                    .map(|i| arrow_json(values.as_ref(), i, options))
                    .collect(),
            )
        }
        DataType::LargeList(_) => {
            let values = array.as_list::<i64>().value(idx);
            Json::Array(
                (0..values.len())
                    .map(|i| arrow_json(values.as_ref(), i, options))
                    .collect(),
            )
        }
        DataType::FixedSizeList(_, _) => {
            let values = array.as_fixed_size_list().value(idx);
            Json::Array(
                (0..values.len())
                    .map(|i| arrow_json(values.as_ref(), i, options))
                    .collect(),
            )
        }
        DataType::Struct(fields) => {
            let strukt = array.as_struct();
            Json::Object(
                fields
                    .iter()
                    .zip(strukt.columns())
                    .map(|(field, column)| {
                        (
                            field.name().clone(),
                            arrow_json(column.as_ref(), idx, options),
                        )
                    })
                    .collect(),
            )
        }
        DataType::Map(_, _) => {
            let entries = array.as_map().value(idx);
            let keys = entries.column(0);
            let values = entries.column(1);
            Json::Object(
                (0..entries.len())
                    .map(|i| {
                        (
                            arrow_json(keys.as_ref(), i, options).into_key(),
                            arrow_json(values.as_ref(), i, options),
                        )
                    })
                    .collect(),
            )
        }
        DataType::Dictionary(_, _) => match dictionary_entry(array, idx) {
            Some((values, key)) => arrow_json(values, key, options),
            None => Json::Text(formatted(array, idx, options)),
        },
        DataType::Float16 | DataType::Float32 | DataType::Float64 => {
            match arrow_cell(array, idx, None, options) {
                CellValue::F64(f) => float_json(f),
                other => cell_json(other),
            }
        }
        DataType::Decimal128(_, scale) => Json::Number(decimal_text(
            array.as_primitive::<Decimal128Type>().value(idx),
            *scale,
        )),
        _ => cell_json(arrow_cell(array, idx, None, options)),
    }
}

fn cell_json(cell: CellValue) -> Json {
    match cell {
        CellValue::Null => Json::Null,
        CellValue::Bool(b) => Json::Bool(b),
        CellValue::I64(n) => Json::Number(n.to_string()),
        CellValue::F64(f) => float_json(f),
        CellValue::String(s) => Json::Text(s),
        CellValue::Bytes(bytes) => Json::Text(format!(
            "\\x{}",
            bytes.iter().map(|b| format!("{b:02X}")).collect::<String>()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::{EventPayload, SecretProvider, SecretRef, row_channel};

    fn memory_profile() -> ConnectionProfile {
        let mut p = ConnectionProfile::sql_login(
            "duckdb",
            MEMORY_DATABASE,
            "",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: String::new(),
            },
        );
        p.database_type = sqlly_core::DatabaseType::DuckDb;
        p
    }

    /// `:memory:` is one shared scratch database per process, so tests that
    /// create objects each get their own file database instead.
    fn fresh_profile() -> (tempfile::TempDir, ConnectionProfile) {
        let dir = tempfile::tempdir().unwrap();
        let mut p = memory_profile();
        p.server = dir.path().join("test.duckdb").to_string_lossy().to_string();
        (dir, p)
    }

    fn payloads(sub: &mut sqlly_core::EventSubscription) -> Vec<EventPayload> {
        let mut out = Vec::new();
        while let Ok(event) = sub.try_recv() {
            out.push(event.payload);
        }
        out
    }

    async fn scalar_row(sql: &str) -> (Vec<ResultColumn>, Vec<CellValue>) {
        let client = open_client(&memory_profile()).await.unwrap();
        let mut sets = query_rows_on_client(&client, sql, &[]).await.unwrap();
        let set = sets.remove(0);
        let row = set.rows.into_iter().next().expect("one row");
        (set.columns, row)
    }

    /// With external access off an instance still runs ordinary SQL on its
    /// own data, but file scans and attaching other databases are refused.
    #[test]
    fn external_access_policy_blocks_files_but_keeps_queries_working() {
        let dir = tempfile::tempdir().unwrap();
        let conn = ::duckdb::Connection::open(dir.path().join("locked.duckdb")).unwrap();
        apply_instance_policy(&conn, true, false);

        conn.execute_batch("CREATE TABLE t AS SELECT 1 AS x; INSERT INTO t VALUES (2);")
            .unwrap();
        let sum: i64 = conn
            .query_row("SELECT sum(x) FROM t", [], |row| row.get(0))
            .unwrap();
        assert_eq!(sum, 3);
        let one: i64 = conn.query_row("SELECT 1", [], |row| row.get(0)).unwrap();
        assert_eq!(one, 1);

        let hosts = dir.path().join("hosts.csv");
        std::fs::write(&hosts, "ip,name\n127.0.0.1,localhost\n").unwrap();
        let scan = conn.query_row(
            &format!("SELECT count(*) FROM read_csv('{}')", hosts.display()),
            [],
            |row| row.get::<_, i64>(0),
        );
        assert!(scan.is_err(), "file scans must be refused: {scan:?}");
        let attach = conn.execute_batch(&format!(
            "ATTACH '{}' AS other",
            dir.path().join("other.duckdb").display()
        ));
        assert!(attach.is_err(), "ATTACH must be refused: {attach:?}");
        let copy = conn.execute_batch(&format!(
            "COPY t TO '{}' (FORMAT CSV)",
            dir.path().join("out.csv").display()
        ));
        assert!(copy.is_err(), "COPY TO must be refused: {copy:?}");

        // The policy cannot be undone from SQL once applied.
        assert!(
            conn.execute_batch("SET enable_external_access = true")
                .is_err()
        );

        // An instance opened without the policy keeps file access.
        let open = ::duckdb::Connection::open(dir.path().join("open.duckdb")).unwrap();
        apply_instance_policy(&open, true, true);
        let rows: i64 = open
            .query_row(
                &format!("SELECT count(*) FROM read_csv('{}')", hosts.display()),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rows, 1);
    }

    #[test]
    fn database_path_prefers_server_and_treats_database_as_the_catalog() {
        let mut p = memory_profile();
        assert_eq!(database_path(&p).unwrap(), MEMORY_DATABASE);
        assert_eq!(catalog_to_use(&p, MEMORY_DATABASE), None);
        // The explorer passes the catalog name it listed.
        p.database = Some("memory".into());
        assert_eq!(database_path(&p).unwrap(), MEMORY_DATABASE);
        assert_eq!(
            catalog_to_use(&p, MEMORY_DATABASE).as_deref(),
            Some("memory")
        );
        // A caller that passes the path itself as the database is not a switch.
        p.database = Some(MEMORY_DATABASE.into());
        assert_eq!(catalog_to_use(&p, MEMORY_DATABASE), None);
        // Server empty: the database field is the path.
        p.server = String::new();
        p.database = Some("C:/tmp/x.duckdb".into());
        assert_eq!(database_path(&p).unwrap(), "C:/tmp/x.duckdb");
        assert_eq!(catalog_to_use(&p, "C:/tmp/x.duckdb"), None);
        p.database = Some("  ".into());
        assert!(database_path(&p).is_err());
        assert_eq!(quote_identifier("we\"ird"), "\"we\"\"ird\"");
    }

    #[tokio::test]
    async fn database_field_switches_to_that_catalog() {
        let (_dir, mut profile) = fresh_profile();
        // The file's own catalog (its stem) is what list_databases reports.
        profile.database = Some("test".into());
        let client = open_client(&profile).await.unwrap();
        let sets = query_rows_on_client(&client, "SELECT current_database()", &[])
            .await
            .unwrap();
        assert_eq!(sets[0].rows[0][0], CellValue::String("test".into()));
        drop(client);
        profile.database = Some("no_such_catalog".into());
        let err = open_client(&profile)
            .await
            .err()
            .expect("unknown catalog fails");
        assert!(
            matches!(&err, StudioError::Connection(m) if m.contains("no_such_catalog")),
            "{err:?}"
        );
    }

    #[test]
    fn leading_trivia_and_keywords() {
        assert_eq!(
            strip_leading_trivia("  -- c\n /* b */ SELECT 1"),
            "SELECT 1"
        );
        assert_eq!(strip_leading_trivia("-- only a comment"), "");
        assert_eq!(
            leading_keyword("\n  with x as (select 1) select * from x"),
            "WITH"
        );
        assert!(starts_with_query_keyword(
            "/* hi */ SELECT count(*) AS \"Count\""
        ));
        assert!(!starts_with_query_keyword("INSERT INTO t VALUES (1)"));
    }

    #[test]
    fn decimal_text_places_the_point() {
        assert_eq!(decimal_text(12345, 3), "12.345");
        assert_eq!(decimal_text(-5, 2), "-0.05");
        assert_eq!(decimal_text(7, 0), "7");
        assert_eq!(decimal_text(0, 4), "0.0000");
        assert_eq!(
            decimal_text(-123456789012345678901234567890, 10),
            "-12345678901234567890.1234567890"
        );
    }

    #[test]
    fn temporal_text_matches_duckdb_spelling() {
        assert_eq!(date32_text(19782), "2024-02-29");
        assert_eq!(date32_text(i32::MAX), "infinity");
        assert_eq!(time_text(49_530_500_000_000), "13:45:30.500");
        assert_eq!(
            timestamp_text(1_709_214_330_123_456, TimeUnit::Microsecond, false),
            "2024-02-29 13:45:30.123456"
        );
        assert_eq!(
            timestamp_text(1_709_214_330, TimeUnit::Second, true),
            "2024-02-29 13:45:30+00:00"
        );
        assert_eq!(
            interval_text(14, 3, 4 * 3_600_000_000_000),
            "1 year 2 months 3 days 04:00:00"
        );
        assert_eq!(interval_text(0, 0, 1_500_000_000), "00:00:01.5");
        assert_eq!(interval_text(0, 0, 0), "00:00:00");
        assert_eq!(interval_text(-1, 0, 0), "-1 month");
    }

    #[test]
    fn json_writer_keeps_field_order_and_escapes() {
        let value = Json::Object(vec![
            ("z".into(), Json::Number("1".into())),
            ("a".into(), Json::Text("q\"uote".into())),
            ("n".into(), Json::Array(vec![Json::Null, Json::Bool(true)])),
        ]);
        assert_eq!(
            value.render(),
            "{\"z\":1,\"a\":\"q\\\"uote\",\"n\":[null,true]}"
        );
    }

    /// Open in Terminal refuses while the app holds the file: the flag must
    /// follow the live client, not linger after it drops.
    #[tokio::test]
    async fn file_in_use_tracks_live_clients() {
        let (_dir, profile) = fresh_profile();
        assert!(!file_in_use(&profile.server));
        let client = open_client(&profile).await.unwrap();
        assert!(file_in_use(&profile.server));
        drop(client);
        // The connection thread releases its instance as it winds down.
        for _ in 0..200 {
            if !file_in_use(&profile.server) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(!file_in_use(&profile.server));
    }

    #[test]
    fn file_lock_conflicts_are_told_apart_from_other_open_errors() {
        assert!(is_file_lock_conflict(
            "IO Error: Cannot open file \"C:\\t\\edit.duckdb\": The process cannot access the file because it is being used by another process."
        ));
        assert!(is_file_lock_conflict(
            "IO Error: Could not set lock on file \"/t/edit.duckdb\": Conflicting lock is held"
        ));
        assert!(!is_file_lock_conflict(
            "IO Error: Cannot open file \"/nope/x.duckdb\": No such file or directory"
        ));
        assert!(!is_file_lock_conflict(
            "Serialization Error: not a DuckDB database file"
        ));
    }

    #[test]
    fn reopening_a_file_right_after_its_last_client_closes_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reopen.duckdb");
        let path = path.to_str().unwrap().to_string();
        {
            let (_shared, conn) = acquire(&path).expect("open");
            conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY)")
                .unwrap();
        }
        for i in 0..40 {
            let (shared, conn) = acquire(&path).expect("open");
            conn.execute("INSERT INTO t VALUES (?)", [i]).unwrap();
            drop(conn);
            // Drop the last strong reference on another thread, the way a
            // connection thread does, and reopen immediately: the reopen must
            // wait for that close instead of opening a second instance.
            let closer = std::thread::spawn(move || drop(shared));
            let (again, conn) = acquire(&path).expect("reopen while the old instance closes");
            let count: i64 = conn
                .query_row("SELECT count(*) FROM t", [], |row| row.get(0))
                .unwrap();
            assert_eq!(count, i64::from(i) + 1, "every committed row is visible");
            drop(conn);
            drop(again);
            closer.join().unwrap();
        }
        assert!(!file_in_use(&path), "the last close releases the file");
        assert!(
            !REGISTRY.lock().files.contains_key(&registry_key(&path)),
            "a closed instance removes its own registry entry"
        );
    }

    #[tokio::test]
    async fn open_client_rejects_unusable_paths() {
        let mut p = memory_profile();
        p.server = "Z:/definitely/not/a/dir/x.duckdb".into();
        assert!(open_client(&p).await.is_err());
    }

    #[tokio::test]
    async fn execute_streams_full_contract() {
        let (_dir, profile) = fresh_profile();
        let client = open_client(&profile).await.unwrap();
        let events = EventBus::new(256);
        let mut sub = events.subscribe();
        let (tx, mut rx) = row_channel(64);

        execute_on_client(
            &client,
            &profile,
            "CREATE TABLE t (id INTEGER, name VARCHAR);\n\
             INSERT INTO t VALUES (1, 'a'), (2, 'b');\n\
             SELECT id, name FROM t ORDER BY id;",
            &events,
            Some(&tx),
        )
        .await
        .unwrap();
        drop(tx);

        let seen = payloads(&mut sub);
        assert!(matches!(seen[0], EventPayload::QueryStarted { .. }));
        assert!(seen.iter().any(|p| matches!(
            p,
            EventPayload::RowCount {
                rows_affected: 2,
                ..
            }
        )));
        let started: Vec<_> = seen
            .iter()
            .filter_map(|p| match p {
                EventPayload::ResultSetStarted { columns, .. } => Some(columns.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(started.len(), 1, "INSERT must not open a grid: {seen:?}");
        assert_eq!(started[0][0].name, "id");
        assert_eq!(started[0][0].data_type, "integer");
        assert_eq!(started[0][1].data_type, "varchar");
        assert!(matches!(
            seen.last().unwrap(),
            EventPayload::QueryCompleted { .. }
        ));

        let batch = rx.recv().await.expect("rows batch");
        assert_eq!(batch.rows.len(), 2);
        assert_eq!(batch.rows[0][0], CellValue::I64(1));
        assert_eq!(batch.rows[1][1], CellValue::String("b".into()));
    }

    #[tokio::test]
    async fn count_named_query_column_is_still_a_grid() {
        let (columns, row) = scalar_row("SELECT count(*) AS \"Count\" FROM range(3)").await;
        assert_eq!(columns[0].name, "Count");
        assert_eq!(row[0], CellValue::I64(3));

        let (_dir, profile) = fresh_profile();
        let client = open_client(&profile).await.unwrap();
        let events = EventBus::new(64);
        let mut sub = events.subscribe();
        execute_on_client(
            &client,
            &profile,
            "SELECT count(*) AS \"Count\" FROM range(3)",
            &events,
            None,
        )
        .await
        .unwrap();
        assert!(
            payloads(&mut sub)
                .iter()
                .any(|p| matches!(p, EventPayload::ResultSetStarted { .. }))
        );
    }

    #[tokio::test]
    async fn insert_returning_streams_rows() {
        let (_dir, profile) = fresh_profile();
        let client = open_client(&profile).await.unwrap();
        let events = EventBus::new(64);
        let mut sub = events.subscribe();
        execute_on_client(
            &client,
            &profile,
            "CREATE TABLE r (id INTEGER); INSERT INTO r VALUES (7) RETURNING id * 2 AS twice;",
            &events,
            None,
        )
        .await
        .unwrap();
        let seen = payloads(&mut sub);
        assert!(seen.iter().any(|p| matches!(p, EventPayload::ResultSetStarted { columns, .. } if columns[0].name == "twice")));
        assert!(seen.iter().any(
            |p| matches!(p, EventPayload::Rows { rows, .. } if rows[0][0] == CellValue::I64(14))
        ));
    }

    #[test]
    fn extension_statements_are_recognized() {
        for sql in [
            "INSTALL httpfs",
            "install httpfs; select 1",
            "select 1; LOAD spatial",
            "FORCE INSTALL aws",
            "UPDATE EXTENSIONS",
            "-- sneaky\nINSTALL httpfs",
            "/* x */ load 'x.duckdb_extension'",
            "SET autoinstall_known_extensions = true",
            "RESET autoload_known_extensions",
            "SET GLOBAL custom_extension_repository = 'http://x'",
            "PRAGMA allow_unsigned_extensions",
        ] {
            assert!(installs_or_loads_extension(sql), "{sql}");
        }
        for sql in [
            "SELECT 'install httpfs'",
            "select * from installs",
            "SET threads = 4",
            "update t set load = 1",
            "-- INSTALL httpfs\nselect 1",
        ] {
            assert!(!installs_or_loads_extension(sql), "{sql}");
        }
    }

    #[tokio::test]
    async fn execute_reports_sql_errors() {
        let (_dir, profile) = fresh_profile();
        let client = open_client(&profile).await.unwrap();
        let events = EventBus::new(64);
        let _sub = events.subscribe();
        let result = execute_on_client(&client, &profile, "SELEC nope", &events, None).await;
        assert!(matches!(result, Err(StudioError::QueryExecution(_))));
        // The connection is still usable afterwards.
        let sets = query_rows_on_client(&client, "SELECT 1", &[])
            .await
            .unwrap();
        assert_eq!(sets[0].rows[0][0], CellValue::I64(1));
    }

    #[tokio::test]
    async fn select_only_policy_blocks_mutations() {
        let (_dir, mut profile) = fresh_profile();
        profile.block_all_but_select = true;
        let client = open_client(&profile).await.unwrap();
        let events = EventBus::new(64);
        let _sub = events.subscribe();
        let result = execute_on_client(&client, &profile, "DROP TABLE t", &events, None).await;
        assert!(matches!(result, Err(StudioError::PermissionDenied(_))));
    }

    #[tokio::test]
    async fn rollback_policy_leaves_no_trace() {
        let (_dir, mut profile) = fresh_profile();
        profile.auto_wrap_rollback = true;
        let client = open_client(&profile).await.unwrap();
        let events = EventBus::new(256);
        let _sub = events.subscribe();

        let mut plain = profile.clone();
        plain.auto_wrap_rollback = false;
        execute_on_client(
            &client,
            &plain,
            "CREATE TABLE t (id INTEGER); INSERT INTO t VALUES (1);",
            &events,
            None,
        )
        .await
        .unwrap();

        execute_on_client(&client, &profile, "DELETE FROM t;", &events, None)
            .await
            .unwrap();
        let sets = query_rows_on_client(&client, "SELECT COUNT(*) FROM t", &[])
            .await
            .unwrap();
        assert_eq!(sets[0].rows[0][0], CellValue::I64(1), "delete rolled back");
    }

    #[tokio::test]
    async fn query_rows_binds_parameters() {
        let (_dir, profile) = fresh_profile();
        let client = open_client(&profile).await.unwrap();
        execute_params_on_client(&client, "CREATE TABLE p (id INTEGER, name VARCHAR)", &[])
            .await
            .unwrap();
        let inserted = execute_params_on_client(
            &client,
            "INSERT INTO p VALUES (?, ?), (?, ?)",
            &[
                SqlParam::I64(1),
                SqlParam::String("alpha".into()),
                SqlParam::I64(2),
                SqlParam::String("beta".into()),
            ],
        )
        .await
        .unwrap();
        assert_eq!(inserted, 2);

        let sets = query_rows_on_client(
            &client,
            "SELECT name FROM p WHERE id = ?",
            &[SqlParam::I64(2)],
        )
        .await
        .unwrap();
        assert_eq!(sets[0].rows, vec![vec![CellValue::String("beta".into())]]);
    }

    #[tokio::test]
    async fn value_mapping_covers_scalar_types() {
        let (columns, row) = scalar_row(
            "SELECT NULL AS n, true AS b, 1::TINYINT AS i1, 2::SMALLINT AS i2, 3::INTEGER AS i4, 4::BIGINT AS i8, \
             9::HUGEINT AS h_small, 170141183460469231731687303715884105727::HUGEINT AS h_big, \
             5::UTINYINT AS u1, 18446744073709551615::UBIGINT AS u8, \
             1.5::FLOAT AS f4, 2.5::DOUBLE AS f8, 12.345::DECIMAL(10,3) AS d, -0.05::DECIMAL(5,2) AS dn, \
             'text' AS s, '\\xDE\\xAD'::BLOB AS bl, \
             DATE '2024-02-29' AS dt, TIME '13:45:30.5' AS tm, \
             TIMESTAMP '2024-02-29 13:45:30.123456' AS ts, TIMESTAMPTZ '2024-02-29 13:45:30+02' AS tstz, \
             INTERVAL '1 year 2 months 3 days 4 hours' AS iv, \
             '8f2c0b7e-1d5a-4c3e-9b8a-7f6e5d4c3b2a'::UUID AS id",
        )
        .await;
        let names: Vec<_> = columns.iter().map(|c| c.data_type.as_str()).collect();
        // DuckDB 2.0 types a bare NULL literal as SQLNULL (1.x said INTEGER).
        assert_eq!(
            names,
            [
                "null",
                "boolean",
                "tinyint",
                "smallint",
                "integer",
                "bigint",
                "hugeint",
                "hugeint",
                "utinyint",
                "ubigint",
                "float",
                "double",
                "decimal(10,3)",
                "decimal(5,2)",
                "varchar",
                "blob",
                "date",
                "time",
                "timestamp",
                "timestamp with time zone",
                "interval",
                "uuid",
            ]
        );
        assert_eq!(row[0], CellValue::Null);
        assert_eq!(row[1], CellValue::Bool(true));
        assert_eq!(row[2], CellValue::I64(1));
        assert_eq!(row[3], CellValue::I64(2));
        assert_eq!(row[4], CellValue::I64(3));
        assert_eq!(row[5], CellValue::I64(4));
        assert_eq!(row[6], CellValue::I64(9));
        assert_eq!(
            row[7],
            CellValue::String("170141183460469231731687303715884105727".into())
        );
        assert_eq!(row[8], CellValue::I64(5));
        assert_eq!(row[9], CellValue::String("18446744073709551615".into()));
        assert_eq!(row[10], CellValue::F64(1.5));
        assert_eq!(row[11], CellValue::F64(2.5));
        assert_eq!(row[12], CellValue::String("12.345".into()));
        assert_eq!(row[13], CellValue::String("-0.05".into()));
        assert_eq!(row[14], CellValue::String("text".into()));
        assert_eq!(row[15], CellValue::Bytes(vec![0xDE, 0xAD]));
        assert_eq!(row[16], CellValue::String("2024-02-29".into()));
        assert_eq!(row[17], CellValue::String("13:45:30.500".into()));
        assert_eq!(
            row[18],
            CellValue::String("2024-02-29 13:45:30.123456".into())
        );
        assert_eq!(
            row[19],
            CellValue::String("2024-02-29 11:45:30+00:00".into())
        );
        assert_eq!(
            row[20],
            CellValue::String("1 year 2 months 3 days 04:00:00".into())
        );
        assert_eq!(
            row[21],
            CellValue::String("8f2c0b7e-1d5a-4c3e-9b8a-7f6e5d4c3b2a".into())
        );
    }

    #[tokio::test]
    async fn nested_values_render_as_json_text() {
        let (_dir, profile) = fresh_profile();
        let client = open_client(&profile).await.unwrap();
        execute_batch_on_client(&client, "CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy')")
            .await
            .unwrap();
        let mut sets = query_rows_on_client(
            &client,
            "SELECT [1, NULL, 3] AS l, {'z': 1, 'a': 'x', 'inner': {'k': [true]}} AS s, \
             MAP {'k': 1.5, 'j': NULL} AS m, [1, 2]::INTEGER[2] AS a, 'happy'::mood AS e, \
             ['a', 'b']::VARCHAR[] AS ls, [DATE '2024-01-02'] AS ld, [12.5::DECIMAL(4,1)] AS ldec, \
             '{\"j\": 1}'::JSON AS js, [{'n': 1}, {'n': 2}] AS los",
            &[],
        )
        .await
        .unwrap();
        let set = sets.remove(0);
        let names: Vec<_> = set.columns.iter().map(|c| c.data_type.as_str()).collect();
        assert_eq!(
            names,
            [
                "list", "struct", "map", "array", "enum", "list", "list", "list", "json", "list"
            ]
        );
        let row = &set.rows[0];
        assert_eq!(row[0], CellValue::String("[1,null,3]".into()));
        assert_eq!(
            row[1],
            CellValue::String("{\"z\":1,\"a\":\"x\",\"inner\":{\"k\":[true]}}".into())
        );
        assert_eq!(row[2], CellValue::String("{\"k\":1.5,\"j\":null}".into()));
        assert_eq!(row[3], CellValue::String("[1,2]".into()));
        assert_eq!(row[4], CellValue::String("happy".into()));
        assert_eq!(row[5], CellValue::String("[\"a\",\"b\"]".into()));
        assert_eq!(row[6], CellValue::String("[\"2024-01-02\"]".into()));
        assert_eq!(row[7], CellValue::String("[12.5]".into()));
        assert_eq!(row[8], CellValue::String("{\"j\": 1}".into()));
        assert_eq!(row[9], CellValue::String("[{\"n\":1},{\"n\":2}]".into()));
    }

    #[tokio::test]
    async fn multiple_result_sets_are_indexed_in_order() {
        let (_dir, profile) = fresh_profile();
        let client = open_client(&profile).await.unwrap();
        let events = EventBus::new(256);
        let mut sub = events.subscribe();
        let (tx, mut rx) = row_channel(64);
        execute_on_client(
            &client,
            &profile,
            "-- leading comment\nSELECT 1 AS a; /* mid */ SELECT 'x' AS b, 'y' AS c; -- trailing",
            &events,
            Some(&tx),
        )
        .await
        .unwrap();
        drop(tx);

        let indices: Vec<usize> = payloads(&mut sub)
            .iter()
            .filter_map(|p| match p {
                EventPayload::ResultSetStarted {
                    result_set_index, ..
                } => Some(*result_set_index),
                _ => None,
            })
            .collect();
        assert_eq!(indices, vec![0, 1]);
        let b1 = rx.recv().await.unwrap();
        let b2 = rx.recv().await.unwrap();
        assert_eq!((b1.result_set_index, b2.result_set_index), (0, 1));
    }

    #[tokio::test]
    async fn ddl_and_set_report_zero_affected_without_a_grid() {
        let (_dir, profile) = fresh_profile();
        let client = open_client(&profile).await.unwrap();
        let events = EventBus::new(64);
        let mut sub = events.subscribe();
        execute_on_client(
            &client,
            &profile,
            "CREATE TABLE z (i INTEGER); SET threads = 2; CREATE TABLE z2 AS SELECT i FROM range(3) t(i);",
            &events,
            None,
        )
        .await
        .unwrap();
        let seen = payloads(&mut sub);
        assert!(
            !seen
                .iter()
                .any(|p| matches!(p, EventPayload::ResultSetStarted { .. })),
            "{seen:?}"
        );
        let counts: Vec<u64> = seen
            .iter()
            .filter_map(|p| match p {
                EventPayload::RowCount { rows_affected, .. } => Some(*rows_affected),
                _ => None,
            })
            .collect();
        assert_eq!(counts, vec![0, 0, 3]);
    }

    #[test]
    fn registry_key_resolves_missing_files_through_their_directory() {
        let dir = tempfile::tempdir().unwrap();
        let canonical_dir = dir.path().canonicalize().unwrap();
        let missing = dir.path().join("new.duckdb");
        assert_eq!(
            registry_key(&missing.to_string_lossy()),
            canonical_dir.join("new.duckdb")
        );
        assert_eq!(
            registry_key("relative-only.duckdb"),
            PathBuf::from("relative-only.duckdb")
        );
    }

    #[tokio::test]
    async fn memory_database_is_shared_across_clients() {
        let profile = memory_profile();
        let first = open_client(&profile).await.unwrap();
        execute_batch_on_client(
            &first,
            "CREATE TABLE shared_memory_probe (i INTEGER); INSERT INTO shared_memory_probe VALUES (5);",
        )
        .await
        .unwrap();
        drop(first);
        let second = open_client(&profile).await.unwrap();
        let sets = query_rows_on_client(&second, "SELECT i FROM shared_memory_probe", &[])
            .await
            .unwrap();
        assert_eq!(sets[0].rows[0][0], CellValue::I64(5));
        execute_batch_on_client(&second, "DROP TABLE shared_memory_probe")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn interrupt_cancels_a_running_statement() {
        let (_dir, profile) = fresh_profile();
        let client = open_client(&profile).await.unwrap();
        let handle = interrupt_handle(&client);
        let events = EventBus::new(64);
        let _sub = events.subscribe();
        let canceller = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            handle.interrupt();
        });
        let started = std::time::Instant::now();
        let result = execute_on_client(
            &client,
            &profile,
            "SELECT sum(i) FROM range(500000000000::BIGINT) t(i)",
            &events,
            None,
        )
        .await;
        canceller.await.unwrap();
        assert!(
            matches!(result, Err(StudioError::QueryExecution(_))),
            "{result:?}"
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(30));
        // The connection survives the interrupt.
        let sets = query_rows_on_client(&client, "SELECT 2", &[])
            .await
            .unwrap();
        assert_eq!(sets[0].rows[0][0], CellValue::I64(2));
    }

    #[tokio::test]
    async fn transport_lists_databases_and_tests_connection() {
        let (_dir, profile) = fresh_profile();
        let transport = DuckDbTransport;
        transport.test_connection(&profile).await.unwrap();
        let dbs = transport.list_databases(&profile).await.unwrap();
        assert_eq!(dbs.len(), 1);
        assert_eq!(dbs[0].name, "test");
    }

    #[tokio::test]
    async fn file_database_persists_and_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sample.duckdb");
        let (_dir, mut profile) = fresh_profile();
        profile.server = path.to_string_lossy().to_string();
        {
            let client = open_client(&profile).await.unwrap();
            execute_batch_on_client(
                &client,
                "CREATE TABLE f (i INTEGER); INSERT INTO f VALUES (42);",
            )
            .await
            .unwrap();
        }
        let client = open_client(&profile).await.unwrap();
        let sets = query_rows_on_client(&client, "SELECT i FROM f", &[])
            .await
            .unwrap();
        assert_eq!(sets[0].rows[0][0], CellValue::I64(42));
        let dbs = DuckDbTransport.list_databases(&profile).await.unwrap();
        assert_eq!(dbs[0].name, "sample");
    }

    #[tokio::test]
    async fn file_database_can_be_opened_twice_in_process() {
        // The app opens several connections to one profile at once (query,
        // catalog, row editing). DuckDB shares one database instance per
        // path inside a process, so a second open must not hit a file lock.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shared.duckdb");
        let (_dir, mut profile) = fresh_profile();
        profile.server = path.to_string_lossy().to_string();
        let first = open_client(&profile).await.unwrap();
        let second = open_client(&profile).await.unwrap();
        execute_batch_on_client(
            &first,
            "CREATE TABLE s (i INTEGER); INSERT INTO s VALUES (1);",
        )
        .await
        .unwrap();
        let sets = query_rows_on_client(&second, "SELECT count(*) FROM s", &[])
            .await
            .unwrap();
        assert_eq!(sets[0].rows[0][0], CellValue::I64(1));
    }

    #[tokio::test]
    async fn bundled_parquet_and_json_extensions_work_offline() {
        let dir = tempfile::tempdir().unwrap();
        let parquet = dir.path().join("t.parquet");
        let client = open_client(&memory_profile()).await.unwrap();
        execute_batch_on_client(
            &client,
            &format!(
                "COPY (SELECT 7 AS x, 'seven' AS y) TO '{}' (FORMAT PARQUET)",
                parquet.to_string_lossy().replace('\'', "''")
            ),
        )
        .await
        .unwrap();
        let sets = query_rows_on_client(
            &client,
            &format!(
                "SELECT y FROM read_parquet('{}')",
                parquet.to_string_lossy().replace('\'', "''")
            ),
            &[],
        )
        .await
        .unwrap();
        assert_eq!(sets[0].rows[0][0], CellValue::String("seven".into()));
        let sets = query_rows_on_client(
            &client,
            "SELECT json_extract('{\"a\": [1, 2]}', '$.a[1]')",
            &[],
        )
        .await
        .unwrap();
        assert_eq!(sets[0].rows[0][0], CellValue::String("2".into()));
    }

    #[tokio::test]
    async fn open_client_runs_startup_sql() {
        let (_dir, mut profile) = fresh_profile();
        profile.startup_sql = Some(
            "CREATE TABLE startup_probe (id INTEGER); INSERT INTO startup_probe VALUES (1);"
                .to_string(),
        );
        let client = open_client(&profile).await.unwrap();
        let sets = query_rows_on_client(&client, "SELECT id FROM startup_probe", &[])
            .await
            .unwrap();
        assert_eq!(sets[0].rows[0][0], CellValue::I64(1));
    }

    #[tokio::test]
    async fn open_client_fails_when_startup_sql_errors() {
        let (_dir, mut profile) = fresh_profile();
        profile.startup_sql = Some("SELECT no_such_table;".to_string());
        let err = open_client(&profile).await;
        assert!(matches!(
            err,
            Err(StudioError::Connection(msg)) if msg.starts_with("Startup SQL failed:")
        ));
    }
}
