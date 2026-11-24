//! # Notebook local workspaces
//!
//! One DuckDB scratch database per notebook, at
//! `<root>/<notebook id>/local.duckdb`, where materialized query results and
//! imported files live as named relations that local SQL cells join and
//! aggregate. A [`LocalWorkspace`] opens that file through the driver's
//! process-wide registry (so the app's own DuckDB clients and the workspace
//! share one instance, never two on one file) and then locks the instance
//! down before any query runs:
//!
//! - extension autoload, autoinstall and community extensions off, so no
//!   statement can fetch or load code;
//! - external access off, so queries cannot read, write or attach any file;
//!   the one exception is the workspace's own `exchange/` directory, which
//!   only [`LocalWorkspace::import_file`] and [`LocalWorkspace::export`] fill
//!   and empty again: an approved import is copied in there (after the path
//!   checks in `files`) before DuckDB scans it, and an export is written
//!   there before it moves to the approved destination;
//! - the configuration locked, so a statement cannot undo any of this.
//!
//! Queries ([`LocalWorkspace::query`]) run in a read-only DuckDB transaction
//! that is always rolled back; only the typed entry points here write.
//! With an [`EncryptionKey`] the database file is encrypted at rest
//! (DuckDB's `ATTACH … (ENCRYPTION_KEY …)`, AES-GCM in the bundled and the
//! prebuilt Windows library alike); the key never touches the file system.
//!
//! Every relation gets a row in [`PROVENANCE_TABLE`] naming where it came
//! from (connection label and opaque profile id, database, a hash of the
//! source SQL, extraction time, row count, column types, completeness and a
//! checksum) and never any credential, connection string or SQL text.
//!
//! Everything here is blocking: callers run it on a worker thread.

mod files;
mod hygiene;
mod ingest;
#[cfg(test)]
mod tests;
pub mod type_map;

pub use files::{
    ExportFormat, ExportOutcome, ExportSource, ImportFormat, ImportRequest, PathRejection,
};
pub use hygiene::{EvictionReport, WorkspaceUsage, evict_lru, remove_workspace, workspace_usages};
pub use ingest::{MaterializeRequest, SourceBatch};
pub use type_map::{LocalType, TypeFamily, local_type_for};

use super::SharedDatabase;
use ::duckdb::arrow::array::Array;
use serde::{Deserialize, Serialize};
use sqlly_core::{CellValue, ConnectionProfile, DatabaseType, ResultColumn};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};

/// The database file inside a notebook's workspace directory.
pub const WORKSPACE_FILE: &str = "local.duckdb";
/// Where approved imports are staged and exports are written first.
const EXCHANGE_DIR: &str = "exchange";
/// DuckDB's spill directory for this workspace.
const TEMP_DIR: &str = "tmp";
/// Touched on every open; its modification time orders LRU eviction.
const LAST_USED_FILE: &str = "last_used";
/// The provenance table every workspace carries.
pub const PROVENANCE_TABLE: &str = "_sqlly_provenance";
/// Relation names starting with this are reserved for SQLLY.
const RESERVED_PREFIX: &str = "_sqlly";
/// The catalog an encrypted workspace file is attached as.
pub const ENCRYPTED_CATALOG: &str = "notebook";
const MAX_NOTEBOOK_ID_LEN: usize = 128;
const MAX_RELATION_NAME_LEN: usize = 63;

// ── Errors ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum LocalStoreError {
    #[error("'{0}' is not a valid notebook id")]
    InvalidNotebookId(String),
    #[error("'{name}' is not a valid relation name: {reason}")]
    InvalidRelationName { name: String, reason: &'static str },
    #[error("a relation named '{0}' already exists")]
    RelationExists(String),
    #[error("there is no relation named '{0}'")]
    RelationNotFound(String),
    #[error("{0}")]
    PathRejected(PathRejection),
    #[error("the file is {size} bytes, over the {limit}-byte import limit")]
    FileTooLarge { size: u64, limit: u64 },
    #[error("the export is {size} bytes, over the {limit}-byte export limit")]
    ExportTooLarge { size: u64, limit: u64 },
    #[error("the workspace uses {used} bytes, at or over its {limit}-byte disk quota")]
    DiskQuotaExceeded { used: u64, limit: u64 },
    #[error("only read-only queries run against a notebook workspace ({0})")]
    NotReadOnly(String),
    #[error("file import and export are turned off for DuckDB in this engine")]
    FileExchangeUnavailable,
    #[error("the workspace's encryption key is missing or wrong")]
    EncryptionKey,
    #[error("a key was given but the workspace file is not encrypted")]
    NotEncrypted,
    #[error("the workspace database is already open without notebook protections")]
    UnprotectedInstance,
    #[error("the file could not be read as {format}: {message}")]
    Malformed {
        format: &'static str,
        message: String,
    },
    #[error("canceled")]
    Canceled,
    #[error("the source failed: {0}")]
    Source(String),
    #[error("DuckDB: {0}")]
    Engine(String),
    #[error("{0}")]
    Io(String),
    #[error("the workspace for notebook '{0}' is still open")]
    WorkspaceOpen(String),
    #[error("the workspace for notebook '{0}' still holds relations")]
    WorkspaceNotEmpty(String),
    #[error("the local data worker stopped: {0}")]
    WorkerStopped(String),
}

pub type LocalResult<T> = Result<T, LocalStoreError>;

impl From<::duckdb::Error> for LocalStoreError {
    fn from(error: ::duckdb::Error) -> Self {
        LocalStoreError::Engine(error.to_string())
    }
}

impl From<std::io::Error> for LocalStoreError {
    fn from(error: std::io::Error) -> Self {
        LocalStoreError::Io(error.to_string())
    }
}

// ── Options ─────────────────────────────────────────────────────────────

/// Resource limits for one workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceQuota {
    /// The workspace directory's size (database, WAL, spill) at which new
    /// relations are refused.
    pub max_disk_bytes: u64,
    /// Rows one relation may hold; ingestion stops there and reports a
    /// partial relation.
    pub max_rows_per_relation: u64,
    /// Value bytes one relation may hold (text and blob lengths, eight per
    /// number), with the same partial semantics.
    pub max_bytes_per_relation: u64,
    /// Largest file an import accepts.
    pub max_import_file_bytes: u64,
    /// Largest file an export may write.
    pub max_export_bytes: u64,
    /// Rows a query returns, summed over its result sets.
    pub max_query_rows: usize,
    /// DuckDB `memory_limit` for the workspace instance.
    pub memory_limit_bytes: Option<u64>,
    /// DuckDB `threads` for the workspace instance.
    pub threads: Option<u32>,
}

impl Default for WorkspaceQuota {
    fn default() -> Self {
        const GIB: u64 = 1024 * 1024 * 1024;
        Self {
            max_disk_bytes: 4 * GIB,
            max_rows_per_relation: 10_000_000,
            max_bytes_per_relation: 2 * GIB,
            max_import_file_bytes: 2 * GIB,
            max_export_bytes: 4 * GIB,
            max_query_rows: 100_000,
            memory_limit_bytes: Some(GIB),
            threads: None,
        }
    }
}

/// A caller-held encryption key (the app keeps it in the OS keychain).
/// `Debug` never prints it.
#[derive(Clone, PartialEq, Eq)]
pub struct EncryptionKey(String);

impl EncryptionKey {
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }
}

impl std::fmt::Debug for EncryptionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("EncryptionKey(..)")
    }
}

#[derive(Debug, Clone, Default)]
pub struct WorkspaceOptions {
    pub quota: WorkspaceQuota,
    /// Encrypt the database file at rest. A workspace created with a key
    /// can only be reopened with the same key.
    pub encryption_key: Option<EncryptionKey>,
    /// Remove the workspace directory (best-effort overwrite first) once
    /// the workspace and every client on it have closed.
    pub delete_on_close: bool,
}

// ── Cancellation ────────────────────────────────────────────────────────

/// Cancels one workspace operation: ingestion stops at the next row, and a
/// running DuckDB statement (an import scan, an export, a query) is
/// interrupted. Canceled operations roll back and leave nothing behind.
#[derive(Clone, Default)]
pub struct CancelToken {
    canceled: Arc<AtomicBool>,
    armed: Arc<parking_lot::Mutex<Option<Arc<::duckdb::InterruptHandle>>>>,
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.canceled.store(true, Ordering::SeqCst);
        if let Some(interrupt) = self.armed.lock().as_ref() {
            interrupt.interrupt();
        }
    }

    pub fn is_canceled(&self) -> bool {
        self.canceled.load(Ordering::SeqCst)
    }

    fn check(&self) -> LocalResult<()> {
        if self.is_canceled() {
            Err(LocalStoreError::Canceled)
        } else {
            Ok(())
        }
    }

    /// Route [`cancel`](Self::cancel) to `interrupt` until the guard drops.
    fn arm(&self, interrupt: &Arc<::duckdb::InterruptHandle>) -> ArmedCancel<'_> {
        *self.armed.lock() = Some(Arc::clone(interrupt));
        ArmedCancel(self)
    }

    /// An engine error caused by this token's interrupt reads as a cancel.
    fn classify(&self, error: LocalStoreError) -> LocalStoreError {
        if self.is_canceled() {
            LocalStoreError::Canceled
        } else {
            error
        }
    }
}

struct ArmedCancel<'a>(&'a CancelToken);

impl Drop for ArmedCancel<'_> {
    fn drop(&mut self) {
        *self.0.armed.lock() = None;
    }
}

// ── Provenance ──────────────────────────────────────────────────────────

/// Where a relation's rows came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    /// A query against a connection.
    Query,
    /// An imported file.
    File,
    /// A local cell's output, derived from other relations.
    Local,
}

impl SourceKind {
    fn as_str(self) -> &'static str {
        match self {
            SourceKind::Query => "query",
            SourceKind::File => "file",
            SourceKind::Local => "local",
        }
    }

    fn parse(text: &str) -> SourceKind {
        match text {
            "file" => SourceKind::File,
            "local" => SourceKind::Local,
            _ => SourceKind::Query,
        }
    }
}

/// The non-secret description of a source the caller attaches to a
/// materialization. Build it from the connection's display name and opaque
/// id; never put a connection string, user name or password in it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceInfo {
    pub kind: SourceKind,
    /// The engine the rows came from; picks the type mapping.
    pub engine: DatabaseType,
    /// What the user calls the source: the connection's name, or the file
    /// name for an import.
    pub label: String,
    /// The connection profile's opaque id.
    pub profile_id: Option<String>,
    pub database: Option<String>,
    pub environment: Option<String>,
    /// A hash of the source SQL (never the text itself).
    pub sql_hash: Option<String>,
    /// The notebook cell and run that produced the rows.
    pub cell_id: Option<String>,
    pub run_id: Option<String>,
    /// Workspace relations a local output was computed from.
    pub upstream: Vec<String>,
    /// When the rows were read from the source (RFC 3339); now if absent.
    pub extracted_at: Option<String>,
    /// Set when the rows handed over are already incomplete (the source
    /// output was capped or sampled), so the relation is partial even when
    /// every handed-over row lands.
    pub source_incomplete: Option<String>,
}

impl SourceInfo {
    pub fn query(engine: DatabaseType, label: impl Into<String>) -> Self {
        Self {
            kind: SourceKind::Query,
            engine,
            label: label.into(),
            profile_id: None,
            database: None,
            environment: None,
            sql_hash: None,
            cell_id: None,
            run_id: None,
            upstream: Vec::new(),
            extracted_at: None,
            source_incomplete: None,
        }
    }

    /// The identifying fields of a connection profile: name, id, engine,
    /// database and environment, nothing else.
    pub fn from_profile(profile: &ConnectionProfile) -> Self {
        let mut source = Self::query(profile.database_type, profile.name.clone());
        source.profile_id = Some(profile.id.0.to_string());
        source.database = profile.database.clone().filter(|d| !d.trim().is_empty());
        source.environment = serde_json::to_value(profile.environment)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string));
        source
    }
}

/// Why a relation holds fewer rows than its source had.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum PartialReason {
    RowLimit {
        limit: u64,
    },
    ByteLimit {
        limit: u64,
    },
    DiskQuota {
        limit: u64,
    },
    /// The rows handed over were already incomplete.
    SourceIncomplete {
        detail: String,
    },
}

/// One materialized column.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalColumn {
    pub name: String,
    /// The type the source reported (empty for imports DuckDB typed).
    pub source_type: String,
    /// The DuckDB type the column has.
    pub local_type: String,
    /// True when the mapped type did not fit every value and the column was
    /// kept as VARCHAR instead.
    pub fell_back_to_text: bool,
}

/// A relation's provenance row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProvenanceRecord {
    pub relation: String,
    pub source_kind: SourceKind,
    pub source_label: String,
    pub engine: Option<String>,
    pub profile_id: Option<String>,
    pub database: Option<String>,
    pub environment: Option<String>,
    pub source_sql_hash: Option<String>,
    /// The approved path of an imported file.
    pub source_path: Option<String>,
    pub cell_id: Option<String>,
    pub run_id: Option<String>,
    pub upstream: Vec<String>,
    pub extracted_at: String,
    pub materialized_at: String,
    pub row_count: u64,
    pub byte_count: u64,
    pub columns: Vec<LocalColumn>,
    pub complete: bool,
    pub partial_reason: Option<PartialReason>,
    /// `rows-sha256:<hex>` over the ingested values, or `file-sha256:<hex>`
    /// over an imported file's bytes.
    pub checksum: Option<String>,
}

/// What a materialization or import produced.
#[derive(Debug, Clone, PartialEq)]
pub struct MaterializeOutcome {
    pub relation: String,
    pub row_count: u64,
    pub byte_count: u64,
    pub columns: Vec<LocalColumn>,
    pub partial: Option<PartialReason>,
    pub checksum: Option<String>,
}

impl MaterializeOutcome {
    pub fn is_complete(&self) -> bool {
        self.partial.is_none()
    }
}

/// Replace an existing relation of the same name, or refuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum WriteMode {
    Replace,
    #[default]
    FailIfExists,
}

/// One result set from [`LocalWorkspace::query`].
#[derive(Debug, Clone, PartialEq)]
pub struct LocalResultSet {
    pub columns: Vec<ResultColumn>,
    pub rows: Vec<Vec<CellValue>>,
    /// More rows were available than the row cap allowed.
    pub truncated: bool,
}

/// A relation as listed by [`LocalWorkspace::relations`].
#[derive(Debug, Clone, PartialEq)]
pub struct RelationInfo {
    pub name: String,
    pub is_view: bool,
    pub row_count: u64,
    pub column_count: u64,
    /// Bytes of storage blocks the relation's checkpointed data occupies
    /// (0 until DuckDB checkpoints it).
    pub storage_bytes: u64,
    pub provenance: Option<ProvenanceRecord>,
}

// ── Names ───────────────────────────────────────────────────────────────

/// Notebook ids become a directory name: ASCII letters, digits, `-` and
/// `_` only.
pub fn validate_notebook_id(id: &str) -> LocalResult<()> {
    let ok = !id.is_empty()
        && id.len() <= MAX_NOTEBOOK_ID_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if ok {
        Ok(())
    } else {
        Err(LocalStoreError::InvalidNotebookId(id.to_string()))
    }
}

/// Relation names are plain SQL identifiers, so local SQL can name them
/// unquoted: a letter or `_`, then letters, digits and `_`, up to 63
/// characters, not starting with the reserved `_sqlly`. DuckDB compares
/// names case-insensitively, so `Orders` and `orders` are the same relation.
pub fn validate_relation_name(name: &str) -> LocalResult<()> {
    let reject = |reason: &'static str| {
        Err(LocalStoreError::InvalidRelationName {
            name: name.to_string(),
            reason,
        })
    };
    let mut chars = name.chars();
    match chars.next() {
        None => return reject("it is empty"),
        Some(c) if !(c.is_ascii_alphabetic() || c == '_') => {
            return reject("it must start with a letter or an underscore");
        }
        _ => {}
    }
    if name.len() > MAX_RELATION_NAME_LEN {
        return reject("it is longer than 63 characters");
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return reject("only letters, digits and underscores are allowed");
    }
    if name.to_ascii_lowercase().starts_with(RESERVED_PREFIX) {
        return reject("names starting with _sqlly are reserved");
    }
    Ok(())
}

pub(crate) fn quote_ident(name: &str) -> String {
    super::quote_identifier(name)
}

/// A SQL string literal (DuckDB strings have no backslash escapes).
pub(crate) fn quote_literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ── The workspace ───────────────────────────────────────────────────────

struct Handle {
    conn: parking_lot::Mutex<::duckdb::Connection>,
    /// Keeps the registry instance alive for as long as the workspace is
    /// open; dropped after `conn`.
    _shared: Arc<SharedDatabase>,
}

/// A notebook's local analytical database. Methods block; share it behind
/// an `Arc` and call it from worker threads. Operations on one workspace
/// run one at a time.
pub struct LocalWorkspace {
    notebook_id: String,
    dir: PathBuf,
    db_path: PathBuf,
    /// Canonical exchange directory, or `None` when the engine forbids
    /// DuckDB file access altogether.
    exchange: Option<PathBuf>,
    catalog: Option<String>,
    quota: WorkspaceQuota,
    delete_on_close: bool,
    interrupt: Arc<::duckdb::InterruptHandle>,
    handle: Option<Handle>,
}

impl std::fmt::Debug for LocalWorkspace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalWorkspace")
            .field("notebook_id", &self.notebook_id)
            .field("db_path", &self.db_path)
            .field("encrypted", &self.catalog.is_some())
            .finish()
    }
}

impl LocalWorkspace {
    /// Open (creating if needed) the workspace for `notebook_id` under
    /// `root`.
    pub fn open(notebook_id: &str, root: &Path, options: WorkspaceOptions) -> LocalResult<Self> {
        validate_notebook_id(notebook_id)?;
        let dir = root.join(notebook_id);
        create_private_dir(root)?;
        create_private_dir(&dir)?;
        let db_path = dir.join(WORKSPACE_FILE);
        if std::fs::symlink_metadata(&db_path).is_ok_and(|m| !m.file_type().is_file()) {
            return Err(LocalStoreError::PathRejected(
                PathRejection::NotARegularFile,
            ));
        }
        let exchange_dir = dir.join(EXCHANGE_DIR);
        create_private_dir(&exchange_dir)?;
        let temp_dir = dir.join(TEMP_DIR);
        create_private_dir(&temp_dir)?;
        let exchange_dir = canonical(&exchange_dir)?;

        let key = options.encryption_key.as_ref();
        let (shared, conn, fresh) = acquire_workspace(&db_path, key)?;
        if fresh {
            // Nothing else in the process has this workspace open, so
            // anything in the exchange directory is left over from a run
            // that never finished.
            files::empty_directory(&exchange_dir);
        }
        let catalog = key.map(|_| ENCRYPTED_CATALOG.to_string());
        let exchange = if fresh || !is_locked(&conn)? {
            apply_workspace_policy(&conn, &exchange_dir, &temp_dir, &options.quota, key)?
        } else {
            verify_joined_instance(&conn, &exchange_dir, key)?
        };
        if let Some(catalog) = &catalog {
            conn.execute_batch(&format!("USE {}", quote_ident(catalog)))?;
        }
        conn.execute_batch(&provenance_ddl())?;
        touch_last_used(&dir);

        let interrupt = conn.interrupt_handle();
        Ok(Self {
            notebook_id: notebook_id.to_string(),
            dir,
            db_path,
            exchange: exchange.then_some(exchange_dir),
            catalog,
            quota: options.quota,
            delete_on_close: options.delete_on_close,
            interrupt,
            handle: Some(Handle {
                conn: parking_lot::Mutex::new(conn),
                _shared: shared,
            }),
        })
    }

    pub fn notebook_id(&self) -> &str {
        &self.notebook_id
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    pub fn quota(&self) -> &WorkspaceQuota {
        &self.quota
    }

    pub fn is_encrypted(&self) -> bool {
        self.catalog.is_some()
    }

    /// Whether imports and exports can run (the engine may forbid DuckDB
    /// file access entirely).
    pub fn file_exchange_available(&self) -> bool {
        self.exchange.is_some()
    }

    /// A DuckDB connection profile for this workspace, read-only enforced,
    /// for the app's regular execute path (streaming, spill and Cancel
    /// behave exactly like any DuckDB query). Clients opened from it join
    /// this workspace's instance and its locked-down settings.
    pub fn connection_profile(&self, name: &str) -> ConnectionProfile {
        let mut profile = ConnectionProfile::sql_login(
            name,
            path_text(&self.db_path),
            "",
            sqlly_core::SecretRef {
                provider: sqlly_core::SecretProvider::InMemory,
                key: String::new(),
            },
        );
        profile.database_type = DatabaseType::DuckDb;
        profile.database = self.catalog.clone();
        profile.block_all_but_select = true;
        profile.enforce_read_only = true;
        profile
    }

    fn conn(&self) -> parking_lot::MutexGuard<'_, ::duckdb::Connection> {
        self.handle
            .as_ref()
            .expect("handle lives until drop")
            .conn
            .lock()
    }

    fn exchange(&self) -> LocalResult<&Path> {
        self.exchange
            .as_deref()
            .ok_or(LocalStoreError::FileExchangeUnavailable)
    }

    /// Bytes the workspace directory occupies (database, WAL, spill).
    pub fn disk_usage(&self) -> u64 {
        hygiene::dir_size(&self.dir)
    }

    fn check_disk_quota(&self) -> LocalResult<u64> {
        let used = self.disk_usage();
        if used >= self.quota.max_disk_bytes {
            return Err(LocalStoreError::DiskQuotaExceeded {
                used,
                limit: self.quota.max_disk_bytes,
            });
        }
        Ok(self.quota.max_disk_bytes - used)
    }

    // ── Ingestion ──

    /// Create relation `request.relation` from `batches` of rows shaped
    /// like `request.columns`, streaming them through DuckDB's appender.
    /// Stops at the row/byte quota and reports a partial relation; a
    /// cancel, a source error or an engine error rolls everything back.
    pub fn materialize<I>(
        &self,
        request: &MaterializeRequest<'_>,
        batches: I,
        cancel: &CancelToken,
    ) -> LocalResult<MaterializeOutcome>
    where
        I: IntoIterator<Item = SourceBatch>,
    {
        validate_relation_name(request.relation)?;
        cancel.check()?;
        let disk_left = self.check_disk_quota()?;
        let conn = self.conn();
        let _armed = cancel.arm(&self.interrupt);
        let limits = ingest::Limits::new(&self.quota, disk_left);
        ingest::materialize(&conn, request, batches.into_iter(), &limits, cancel)
            .map_err(|e| cancel.classify(e))
    }

    // ── Files ──

    /// Import an approved file into a relation.
    pub fn import_file(
        &self,
        request: &ImportRequest<'_>,
        cancel: &CancelToken,
    ) -> LocalResult<MaterializeOutcome> {
        validate_relation_name(request.relation)?;
        cancel.check()?;
        let exchange = self.exchange()?;
        let disk_left = self.check_disk_quota()?;
        let staged = files::stage_import(request.path, exchange, &self.quota, cancel)?;
        let conn = self.conn();
        let _armed = cancel.arm(&self.interrupt);
        let limits = ingest::Limits::new(&self.quota, disk_left);
        files::import_staged(&conn, request, &staged, &limits, cancel)
            .map_err(|e| cancel.classify(e))
    }

    /// Export a relation or a read-only query to an approved path as
    /// Parquet or CSV.
    pub fn export(
        &self,
        source: &ExportSource<'_>,
        format: ExportFormat,
        destination: &Path,
        overwrite: bool,
        cancel: &CancelToken,
    ) -> LocalResult<ExportOutcome> {
        cancel.check()?;
        let exchange = self.exchange()?;
        if let ExportSource::Relation(name) = source {
            validate_relation_name(name)?;
        }
        let destination = files::check_destination(destination, overwrite)?;
        let conn = self.conn();
        if let ExportSource::Relation(name) = source {
            relation_kind(&conn, name)?
                .ok_or_else(|| LocalStoreError::RelationNotFound(name.to_string()))?;
        }
        let _armed = cancel.arm(&self.interrupt);
        files::export(
            &conn,
            source,
            format,
            exchange,
            &destination,
            overwrite,
            self.quota.max_export_bytes,
        )
        .map_err(|e| cancel.classify(e))
    }

    // ── Queries ──

    /// Run read-only SQL against the workspace: every statement must be a
    /// query (SELECT, WITH, FROM, VALUES, TABLE, SHOW, DESCRIBE, SUMMARIZE,
    /// EXPLAIN, PIVOT/UNPIVOT), and the batch runs in a read-only DuckDB
    /// transaction that is rolled back. Rows are capped at `max_rows` (the
    /// quota's `max_query_rows` when `None`), summed over result sets.
    pub fn query(
        &self,
        sql: &str,
        max_rows: Option<usize>,
        cancel: &CancelToken,
    ) -> LocalResult<Vec<LocalResultSet>> {
        let units = read_only_units(sql)?;
        cancel.check()?;
        let cap = max_rows.unwrap_or(self.quota.max_query_rows);
        let conn = self.conn();
        let _armed = cancel.arm(&self.interrupt);
        run_read_only(&conn, &units, cap).map_err(|e| cancel.classify(e))
    }

    /// Column statistics for a relation via DuckDB `SUMMARIZE`: one row per
    /// column with min, max, approximate distinct count, average, standard
    /// deviation, quartiles, count and null percentage.
    pub fn summarize(&self, relation: &str, cancel: &CancelToken) -> LocalResult<LocalResultSet> {
        validate_relation_name(relation)?;
        cancel.check()?;
        let conn = self.conn();
        relation_kind(&conn, relation)?
            .ok_or_else(|| LocalStoreError::RelationNotFound(relation.to_string()))?;
        let _armed = cancel.arm(&self.interrupt);
        let sql = format!("SUMMARIZE {}", quote_ident(relation));
        let mut sets = run_read_only(&conn, &[sql], usize::MAX).map_err(|e| cancel.classify(e))?;
        Ok(sets.pop().unwrap_or(LocalResultSet {
            columns: Vec::new(),
            rows: Vec::new(),
            truncated: false,
        }))
    }

    // ── Hygiene ──

    /// The workspace's relations (tables and views), provenance attached.
    pub fn relations(&self) -> LocalResult<Vec<RelationInfo>> {
        let conn = self.conn();
        let block_size = block_size(&conn);
        let mut stmt = conn.prepare(
            "SELECT table_name, false, estimated_size, column_count FROM duckdb_tables() \
             WHERE database_name = current_database() AND schema_name = 'main' \
             UNION ALL \
             SELECT view_name, true, 0, column_count FROM duckdb_views() \
             WHERE database_name = current_database() AND schema_name = 'main' AND NOT internal \
             ORDER BY 1",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, bool>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (name, is_view, rows, columns) = row?;
            if name.to_ascii_lowercase().starts_with(RESERVED_PREFIX) {
                continue;
            }
            let storage_bytes = if is_view {
                0
            } else {
                storage_blocks(&conn, &name) * block_size
            };
            let provenance = read_provenance(&conn, &name)?;
            out.push(RelationInfo {
                name,
                is_view,
                row_count: u64::try_from(rows).unwrap_or(0),
                column_count: u64::try_from(columns).unwrap_or(0),
                storage_bytes,
                provenance,
            });
        }
        Ok(out)
    }

    /// Drop a relation and its provenance. Returns whether it existed.
    pub fn drop_relation(&self, relation: &str) -> LocalResult<bool> {
        validate_relation_name(relation)?;
        let conn = self.conn();
        let Some(is_view) = relation_kind(&conn, relation)? else {
            return Ok(false);
        };
        in_transaction(&conn, |conn| {
            drop_relation_sql(conn, relation, is_view)?;
            delete_provenance(conn, relation)
        })?;
        Ok(true)
    }

    /// Drop every relation and provenance row, then checkpoint so the file
    /// can reuse the space.
    pub fn clear(&self) -> LocalResult<()> {
        let names: Vec<(String, bool)> = self
            .relations()?
            .into_iter()
            .map(|r| (r.name, r.is_view))
            .collect();
        let conn = self.conn();
        in_transaction(&conn, |conn| {
            for (name, is_view) in names.iter().filter(|(_, v)| *v) {
                drop_relation_sql(conn, name, *is_view)?;
            }
            for (name, is_view) in names.iter().filter(|(_, v)| !*v) {
                drop_relation_sql(conn, name, *is_view)?;
            }
            conn.execute(&format!("DELETE FROM {PROVENANCE_TABLE}"), [])?;
            Ok(())
        })?;
        conn.execute_batch("CHECKPOINT")?;
        Ok(())
    }

    // ── Provenance ──

    pub fn provenance(&self, relation: &str) -> LocalResult<Option<ProvenanceRecord>> {
        validate_relation_name(relation)?;
        read_provenance(&self.conn(), relation)
    }

    pub fn all_provenance(&self) -> LocalResult<Vec<ProvenanceRecord>> {
        read_all_provenance(&self.conn())
    }

    /// The provenance of `relation` and, transitively, of every relation it
    /// was derived from, so a joined result names all of its sources.
    pub fn lineage(&self, relation: &str) -> LocalResult<Vec<ProvenanceRecord>> {
        validate_relation_name(relation)?;
        let all = read_all_provenance(&self.conn())?;
        let mut out: Vec<ProvenanceRecord> = Vec::new();
        let mut pending = vec![relation.to_ascii_lowercase()];
        while let Some(name) = pending.pop() {
            if out.iter().any(|r| r.relation.eq_ignore_ascii_case(&name)) {
                continue;
            }
            if let Some(record) = all.iter().find(|r| r.relation.eq_ignore_ascii_case(&name)) {
                pending.extend(record.upstream.iter().map(|u| u.to_ascii_lowercase()));
                out.push(record.clone());
            }
        }
        Ok(out)
    }

    /// Close the workspace now. With `delete_on_close`, waits for every
    /// other client on the database to close and then removes the
    /// directory; returns whether it was removed.
    pub fn close(mut self) -> bool {
        self.release()
    }

    /// Close the workspace and remove its directory, waiting for every
    /// other client on the database to close first. Returns whether it was
    /// removed (not when a client is still open after the wait).
    pub fn close_and_remove(mut self) -> bool {
        self.delete_on_close = true;
        self.release()
    }

    fn release(&mut self) -> bool {
        let Some(handle) = self.handle.take() else {
            return false;
        };
        drop(handle);
        if !self.delete_on_close {
            return false;
        }
        if !wait_until_closed(&self.db_path, super::CLOSE_WAIT) {
            tracing::warn!(
                notebook = %self.notebook_id,
                "notebook workspace still open elsewhere; not deleting it"
            );
            return false;
        }
        hygiene::secure_remove_dir(&self.dir);
        true
    }
}

impl Drop for LocalWorkspace {
    fn drop(&mut self) {
        self.release();
    }
}

/// `canonicalize` without the Windows `\\?\` verbatim prefix, which
/// DuckDB's allowed-directory check would not match against the plain
/// paths it sees in statements.
pub(crate) fn canonical(path: &Path) -> std::io::Result<PathBuf> {
    let resolved = path.canonicalize()?;
    #[cfg(windows)]
    {
        let text = resolved.to_string_lossy();
        if let Some(rest) = text.strip_prefix(r"\\?\")
            && !rest.starts_with("UNC\\")
        {
            return Ok(PathBuf::from(rest));
        }
    }
    Ok(resolved)
}

fn create_private_dir(dir: &Path) -> LocalResult<()> {
    if let Ok(meta) = std::fs::symlink_metadata(dir) {
        if meta.file_type().is_symlink() {
            return Err(LocalStoreError::PathRejected(PathRejection::Symlink));
        }
        if !meta.is_dir() {
            return Err(LocalStoreError::PathRejected(
                PathRejection::NotARegularFile,
            ));
        }
        return Ok(());
    }
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

fn touch_last_used(dir: &Path) {
    let marker = dir.join(LAST_USED_FILE);
    let result = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&marker)
        .and_then(|file| file.set_modified(std::time::SystemTime::now()));
    if let Err(error) = result {
        tracing::debug!(%error, "could not touch the notebook workspace marker");
    }
}

// ── Opening through the registry ────────────────────────────────────────

/// `acquire` for a workspace file: join this process's live instance of the
/// file, or open one (plain, or as an encrypted attachment of a private
/// in-memory instance) and register it under the file's key so every other
/// client in the process joins it. Follows the registry's rules: wait for a
/// closing instance of the same file, never sweep dead entries, and drop
/// nothing while holding the lock. Returns whether the instance is new.
fn acquire_workspace(
    path: &Path,
    encryption: Option<&EncryptionKey>,
) -> LocalResult<(Arc<SharedDatabase>, ::duckdb::Connection, bool)> {
    let text = path_text(path);
    let key = super::registry_key(&text);
    let mut registry = super::REGISTRY.lock();
    let deadline = std::time::Instant::now() + super::CLOSE_WAIT;
    while registry
        .files
        .get(&key)
        .is_some_and(|weak| weak.strong_count() == 0)
    {
        if super::INSTANCE_CLOSED
            .wait_until(&mut registry, deadline)
            .timed_out()
        {
            break;
        }
    }
    let opened = match registry.files.get(&key).and_then(Weak::upgrade) {
        Some(shared) => Ok((shared, false)),
        None => {
            let own_instance_closing = registry.files.contains_key(&key);
            let root = match encryption {
                None => super::open_file(&text, own_instance_closing).map_err(|error| {
                    if error.to_string().contains("encrypted database") {
                        LocalStoreError::EncryptionKey
                    } else {
                        error.into()
                    }
                }),
                Some(secret) => open_encrypted(&text, secret, own_instance_closing),
            };
            root.map(|root| {
                let shared = Arc::new(SharedDatabase::new(root, Some(key.clone())));
                registry.files.insert(key, Arc::downgrade(&shared));
                (shared, true)
            })
        }
    };
    drop(registry);
    let (shared, fresh) = opened?;
    let conn = shared.root().lock().try_clone()?;
    Ok((shared, conn, fresh))
}

/// A private in-memory instance with the encrypted workspace file attached
/// as [`ENCRYPTED_CATALOG`]. The key goes to DuckDB as a literal in the
/// ATTACH statement only.
fn open_encrypted(
    path: &str,
    secret: &EncryptionKey,
    own_instance_closing: bool,
) -> LocalResult<::duckdb::Connection> {
    let conn = ::duckdb::Connection::open_in_memory()?;
    let attach = format!(
        "ATTACH {} AS {} (ENCRYPTION_KEY {})",
        quote_literal(path),
        quote_ident(ENCRYPTED_CATALOG),
        quote_literal(&secret.0)
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match conn.execute_batch(&attach) {
            Ok(()) => return Ok(conn),
            Err(error) => {
                let message = error.to_string();
                if message.contains("is not encrypted") {
                    return Err(LocalStoreError::NotEncrypted);
                }
                if message.contains("encryption key") || message.contains("encrypted database") {
                    return Err(LocalStoreError::EncryptionKey);
                }
                if own_instance_closing
                    && super::is_file_lock_conflict(&message)
                    && std::time::Instant::now() < deadline
                {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    continue;
                }
                return Err(LocalStoreError::Engine(message));
            }
        }
    }
}

fn setting(conn: &::duckdb::Connection, name: &str) -> LocalResult<String> {
    Ok(conn.query_row(
        &format!(
            "SELECT CAST(current_setting({}) AS VARCHAR)",
            quote_literal(name)
        ),
        [],
        |row| row.get::<_, String>(0),
    )?)
}

fn is_locked(conn: &::duckdb::Connection) -> LocalResult<bool> {
    Ok(setting(conn, "lock_configuration")? == "true")
}

/// Lock a workspace instance down (see the module docs). Returns whether
/// the exchange directory is reachable: an engine that already turned off
/// all DuckDB file access (a proxy engine serving browsers) keeps it off.
fn apply_workspace_policy(
    conn: &::duckdb::Connection,
    exchange: &Path,
    temp: &Path,
    quota: &WorkspaceQuota,
    encryption: Option<&EncryptionKey>,
) -> LocalResult<bool> {
    conn.execute_batch(
        "SET autoinstall_known_extensions = false; \
         SET autoload_known_extensions = false; \
         SET allow_community_extensions = false;",
    )?;
    let mut best_effort = vec![format!(
        "SET temp_directory = {}",
        quote_literal(&path_text(temp))
    )];
    if let Some(bytes) = quota.memory_limit_bytes {
        best_effort.push(format!("SET memory_limit = '{bytes}B'"));
    }
    if let Some(threads) = quota.threads {
        best_effort.push(format!("SET threads = {}", threads.max(1)));
    }
    if encryption.is_some() {
        best_effort.push("SET temp_file_encryption = true".to_string());
    }
    for statement in best_effort {
        if let Err(error) = conn.execute_batch(&statement) {
            tracing::warn!(%error, statement, "notebook workspace setting not applied");
        }
    }
    let external_access_on = setting(conn, "enable_external_access")? == "true";
    let exchange_reachable = external_access_on
        && conn
            .execute_batch(&format!(
                "SET allowed_directories = [{}]",
                quote_literal(&exchange_prefix(exchange))
            ))
            .is_ok();
    conn.execute_batch("SET enable_external_access = false; SET lock_configuration = true;")?;
    Ok(exchange_reachable)
}

/// A joined instance someone else locked: it must carry the same policy.
fn verify_joined_instance(
    conn: &::duckdb::Connection,
    exchange: &Path,
    encryption: Option<&EncryptionKey>,
) -> LocalResult<bool> {
    if setting(conn, "enable_external_access")? != "false"
        || setting(conn, "autoload_known_extensions")? != "false"
        || setting(conn, "autoinstall_known_extensions")? != "false"
    {
        return Err(LocalStoreError::UnprotectedInstance);
    }
    if encryption.is_some() {
        let encrypted: bool = conn
            .query_row(
                "SELECT count(*) > 0 FROM duckdb_databases() WHERE database_name = ? AND encrypted",
                [ENCRYPTED_CATALOG],
                |row| row.get(0),
            )
            .unwrap_or(false);
        if !encrypted {
            return Err(LocalStoreError::EncryptionKey);
        }
    }
    Ok(setting(conn, "allowed_directories")?.contains(&exchange_prefix(exchange)))
}

/// The exchange directory as an `allowed_directories` prefix: with the
/// trailing separator, so a sibling `exchange2/` never matches.
fn exchange_prefix(exchange: &Path) -> String {
    let mut text = path_text(exchange);
    if !text.ends_with(std::path::MAIN_SEPARATOR) {
        text.push(std::path::MAIN_SEPARATOR);
    }
    text
}

/// Wait until this process no longer holds `path` open (its instance
/// finished closing and left the registry).
fn wait_until_closed(path: &Path, timeout: std::time::Duration) -> bool {
    let key = super::registry_key(&path_text(path));
    let deadline = std::time::Instant::now() + timeout;
    let mut registry = super::REGISTRY.lock();
    while registry.files.contains_key(&key) {
        if super::INSTANCE_CLOSED
            .wait_until(&mut registry, deadline)
            .timed_out()
        {
            return !registry.files.contains_key(&key);
        }
    }
    true
}

// ── Relations ───────────────────────────────────────────────────────────

/// `Some(is_view)` when a table or view named `name` exists in the
/// workspace catalog (case-insensitively).
pub(crate) fn relation_kind(conn: &::duckdb::Connection, name: &str) -> LocalResult<Option<bool>> {
    let mut stmt = conn.prepare(
        "SELECT false FROM duckdb_tables() WHERE database_name = current_database() \
         AND schema_name = 'main' AND lower(table_name) = lower(?) \
         UNION ALL SELECT true FROM duckdb_views() WHERE database_name = current_database() \
         AND schema_name = 'main' AND lower(view_name) = lower(?) AND NOT internal",
    )?;
    let mut rows = stmt.query([name, name])?;
    match rows.next()? {
        Some(row) => Ok(Some(row.get(0)?)),
        None => Ok(None),
    }
}

pub(crate) fn drop_relation_sql(
    conn: &::duckdb::Connection,
    name: &str,
    is_view: bool,
) -> LocalResult<()> {
    let kind = if is_view { "VIEW" } else { "TABLE" };
    conn.execute_batch(&format!("DROP {kind} IF EXISTS {}", quote_ident(name)))?;
    Ok(())
}

/// Run `body` in a transaction: committed on success, rolled back on any
/// error.
pub(crate) fn in_transaction<T>(
    conn: &::duckdb::Connection,
    body: impl FnOnce(&::duckdb::Connection) -> LocalResult<T>,
) -> LocalResult<T> {
    conn.execute_batch("BEGIN TRANSACTION")?;
    match body(conn) {
        Ok(value) => match conn.execute_batch("COMMIT") {
            Ok(()) => Ok(value),
            Err(error) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(error.into())
            }
        },
        Err(error) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(error)
        }
    }
}

fn block_size(conn: &::duckdb::Connection) -> u64 {
    conn.query_row(
        "SELECT block_size FROM pragma_database_size() WHERE database_name = current_database()",
        [],
        |row| row.get::<_, i64>(0),
    )
    .ok()
    .and_then(|n| u64::try_from(n).ok())
    .unwrap_or(256 * 1024)
}

fn storage_blocks(conn: &::duckdb::Connection, table: &str) -> u64 {
    conn.query_row(
        "SELECT count(DISTINCT block_id) FROM pragma_storage_info(?) WHERE persistent AND block_id >= 0",
        [table],
        |row| row.get::<_, i64>(0),
    )
    .ok()
    .and_then(|n| u64::try_from(n).ok())
    .unwrap_or(0)
}

// ── Read-only queries ───────────────────────────────────────────────────

/// Statements a workspace query may contain.
const READ_ONLY_KEYWORDS: [&str; 12] = [
    "SELECT",
    "WITH",
    "VALUES",
    "FROM",
    "TABLE",
    "SHOW",
    "DESCRIBE",
    "DESC",
    "SUMMARIZE",
    "EXPLAIN",
    "PIVOT",
    "UNPIVOT",
];

fn read_only_units(sql: &str) -> LocalResult<Vec<String>> {
    if super::installs_or_loads_extension(sql) {
        return Err(LocalStoreError::NotReadOnly(
            "extensions cannot be installed or loaded".to_string(),
        ));
    }
    let units: Vec<String> = sqlly_lang::seclex::execution_units(sql, DatabaseType::DuckDb)
        .into_iter()
        .map(|unit| unit.text.to_string())
        .collect();
    if units.is_empty() {
        return Err(LocalStoreError::NotReadOnly(
            "the query is empty".to_string(),
        ));
    }
    for unit in &units {
        let keyword = super::leading_keyword(unit);
        if !READ_ONLY_KEYWORDS.contains(&keyword.as_str()) {
            return Err(LocalStoreError::NotReadOnly(format!(
                "{} statements are not allowed",
                if keyword.is_empty() {
                    "these"
                } else {
                    &keyword
                }
            )));
        }
    }
    Ok(units)
}

/// Run already-screened statements in a read-only transaction that is
/// always rolled back.
pub(crate) fn run_read_only(
    conn: &::duckdb::Connection,
    statements: &[String],
    max_rows: usize,
) -> LocalResult<Vec<LocalResultSet>> {
    conn.execute_batch("BEGIN TRANSACTION READ ONLY")?;
    let result = collect_result_sets(conn, statements, max_rows);
    let _ = conn.execute_batch("ROLLBACK");
    result
}

fn collect_result_sets(
    conn: &::duckdb::Connection,
    statements: &[String],
    max_rows: usize,
) -> LocalResult<Vec<LocalResultSet>> {
    let mut sets = Vec::new();
    let mut remaining = max_rows;
    let options = super::format_options();
    for text in statements {
        let (stmt, _) = super::prepare_and_execute(conn, text)?;
        let (columns, hints) = super::describe_columns(&stmt);
        let mut rows = Vec::new();
        let mut truncated = false;
        'batches: while let Some(batch) = stmt.step()? {
            for row in 0..batch.len() {
                if remaining == 0 {
                    truncated = true;
                    break 'batches;
                }
                rows.push(super::batch_row(&batch, row, &hints, &options));
                remaining -= 1;
            }
        }
        sets.push(LocalResultSet {
            columns,
            rows,
            truncated,
        });
    }
    Ok(sets)
}

// ── Provenance storage ──────────────────────────────────────────────────

fn provenance_ddl() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {PROVENANCE_TABLE} (\
         relation VARCHAR PRIMARY KEY, \
         source_kind VARCHAR NOT NULL, \
         source_label VARCHAR NOT NULL, \
         engine VARCHAR, \
         profile_id VARCHAR, \
         database_name VARCHAR, \
         environment VARCHAR, \
         source_sql_hash VARCHAR, \
         source_path VARCHAR, \
         cell_id VARCHAR, \
         run_id VARCHAR, \
         upstream JSON, \
         extracted_at VARCHAR NOT NULL, \
         materialized_at VARCHAR NOT NULL, \
         row_count BIGINT NOT NULL, \
         byte_count BIGINT NOT NULL, \
         column_types JSON NOT NULL, \
         complete BOOLEAN NOT NULL, \
         partial_reason JSON, \
         checksum VARCHAR)"
    )
}

const PROVENANCE_COLUMNS: &str = "relation, source_kind, source_label, engine, profile_id, \
     database_name, environment, source_sql_hash, source_path, cell_id, run_id, \
     CAST(upstream AS VARCHAR), extracted_at, materialized_at, row_count, byte_count, \
     CAST(column_types AS VARCHAR), complete, CAST(partial_reason AS VARCHAR), checksum";

pub(crate) fn write_provenance(
    conn: &::duckdb::Connection,
    record: &ProvenanceRecord,
) -> LocalResult<()> {
    delete_provenance(conn, &record.relation)?;
    let params: Vec<Box<dyn ::duckdb::ToSql>> = vec![
        Box::new(record.relation.clone()),
        Box::new(record.source_kind.as_str().to_string()),
        Box::new(record.source_label.clone()),
        Box::new(record.engine.clone()),
        Box::new(record.profile_id.clone()),
        Box::new(record.database.clone()),
        Box::new(record.environment.clone()),
        Box::new(record.source_sql_hash.clone()),
        Box::new(record.source_path.clone()),
        Box::new(record.cell_id.clone()),
        Box::new(record.run_id.clone()),
        Box::new(to_json(&record.upstream)),
        Box::new(record.extracted_at.clone()),
        Box::new(record.materialized_at.clone()),
        Box::new(i64::try_from(record.row_count).unwrap_or(i64::MAX)),
        Box::new(i64::try_from(record.byte_count).unwrap_or(i64::MAX)),
        Box::new(to_json(&record.columns)),
        Box::new(record.complete),
        Box::new(record.partial_reason.as_ref().map(to_json)),
        Box::new(record.checksum.clone()),
    ];
    conn.execute(
        &format!(
            "INSERT INTO {PROVENANCE_TABLE} VALUES \
             (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
        ),
        ::duckdb::params_from_iter(params.iter().map(|p| p.as_ref())),
    )?;
    Ok(())
}

fn to_json<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".to_string())
}

fn delete_provenance(conn: &::duckdb::Connection, relation: &str) -> LocalResult<()> {
    conn.execute(
        &format!("DELETE FROM {PROVENANCE_TABLE} WHERE lower(relation) = lower(?)"),
        [relation],
    )?;
    Ok(())
}

fn read_provenance(
    conn: &::duckdb::Connection,
    relation: &str,
) -> LocalResult<Option<ProvenanceRecord>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {PROVENANCE_COLUMNS} FROM {PROVENANCE_TABLE} WHERE lower(relation) = lower(?)"
    ))?;
    let mut rows = stmt.query([relation])?;
    match rows.next()? {
        Some(row) => Ok(Some(provenance_from_row(row)?)),
        None => Ok(None),
    }
}

fn read_all_provenance(conn: &::duckdb::Connection) -> LocalResult<Vec<ProvenanceRecord>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {PROVENANCE_COLUMNS} FROM {PROVENANCE_TABLE} ORDER BY relation"
    ))?;
    let mut rows = stmt.query([])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(provenance_from_row(row)?);
    }
    Ok(out)
}

fn provenance_from_row(row: &::duckdb::Row<'_>) -> LocalResult<ProvenanceRecord> {
    fn from_json<T: serde::de::DeserializeOwned + Default>(text: Option<String>) -> T {
        text.and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }
    let kind: String = row.get(1)?;
    let partial: Option<String> = row.get(18)?;
    Ok(ProvenanceRecord {
        relation: row.get(0)?,
        source_kind: SourceKind::parse(&kind),
        source_label: row.get(2)?,
        engine: row.get(3)?,
        profile_id: row.get(4)?,
        database: row.get(5)?,
        environment: row.get(6)?,
        source_sql_hash: row.get(7)?,
        source_path: row.get(8)?,
        cell_id: row.get(9)?,
        run_id: row.get(10)?,
        upstream: from_json(row.get(11)?),
        extracted_at: row.get(12)?,
        materialized_at: row.get(13)?,
        row_count: u64::try_from(row.get::<_, i64>(14)?).unwrap_or(0),
        byte_count: u64::try_from(row.get::<_, i64>(15)?).unwrap_or(0),
        columns: from_json(row.get(16)?),
        complete: row.get(17)?,
        partial_reason: partial.and_then(|t| serde_json::from_str(&t).ok()),
        checksum: row.get(19)?,
    })
}
