//! File import and export for a workspace.
//!
//! DuckDB never opens a user path itself. An import checks the approved
//! path (a regular file, not a symlink, not a directory, within the size
//! limit), opens it, confirms the opened handle is the file that was
//! checked, and copies it into the workspace's exchange directory, hashing
//! and counting bytes as it goes; DuckDB then scans that private copy, the
//! only directory it may read. An export is written into the exchange
//! directory and then moved to the approved destination. Paths reach SQL as
//! bound parameters where DuckDB accepts them and as escaped literals of
//! paths this module generated otherwise.

use super::ingest::{self, FileOrigin, Limits, MaterializeRequest, SourceBatch};
use super::{
    CancelToken, LocalColumn, LocalResult, LocalStoreError, MaterializeOutcome, PartialReason,
    SourceInfo, SourceKind, WorkspaceQuota, WriteMode, hex, in_transaction, quote_ident,
    quote_literal, write_provenance,
};
use sha2::{Digest, Sha256};
use sqlly_core::{CellValue, DatabaseType, ResultColumn};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Why a path was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathRejection {
    NotFound,
    /// A directory, device, socket or other non-file.
    NotARegularFile,
    /// The path is a symbolic link.
    Symlink,
    /// The file changed between the check and the read.
    ChangedWhileReading,
    /// The destination exists and overwriting was not approved.
    DestinationExists,
    /// The destination's folder does not exist.
    DestinationFolderMissing,
}

impl std::fmt::Display for PathRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            PathRejection::NotFound => "the file does not exist",
            PathRejection::NotARegularFile => "the path is not a regular file",
            PathRejection::Symlink => "the path is a symbolic link",
            PathRejection::ChangedWhileReading => "the file changed while it was being read",
            PathRejection::DestinationExists => "the destination file already exists",
            PathRejection::DestinationFolderMissing => "the destination folder does not exist",
        })
    }
}

fn rejected(reason: PathRejection) -> LocalStoreError {
    LocalStoreError::PathRejected(reason)
}

/// The formats an import reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportFormat {
    /// CSV/TSV with DuckDB's dialect, header and type sniffing.
    Csv,
    Parquet,
    /// A JSON array of objects, or newline-delimited JSON (sniffed).
    Json,
    /// Newline-delimited JSON, one object per line.
    NdJson,
    /// One table or view of a SQLite database, read with SQLite itself
    /// (DuckDB's SQLite scanner is a downloadable extension).
    Sqlite {
        table: String,
    },
}

impl ImportFormat {
    fn label(&self) -> &'static str {
        match self {
            ImportFormat::Csv => "CSV",
            ImportFormat::Parquet => "Parquet",
            ImportFormat::Json => "JSON",
            ImportFormat::NdJson => "newline-delimited JSON",
            ImportFormat::Sqlite { .. } => "SQLite",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ImportRequest<'a> {
    pub relation: &'a str,
    /// The path the user approved.
    pub path: &'a Path,
    pub format: ImportFormat,
    pub mode: WriteMode,
    /// The notebook cell and run doing the import, for provenance.
    pub cell_id: Option<String>,
    pub run_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    /// Zstandard-compressed Parquet.
    Parquet,
    /// CSV with a header row.
    Csv,
}

impl ExportFormat {
    fn extension(self) -> &'static str {
        match self {
            ExportFormat::Parquet => "parquet",
            ExportFormat::Csv => "csv",
        }
    }

    fn copy_options(self) -> &'static str {
        match self {
            ExportFormat::Parquet => "FORMAT parquet, COMPRESSION zstd",
            ExportFormat::Csv => "FORMAT csv, HEADER true",
        }
    }
}

/// What an export writes: a relation, or one read-only query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportSource<'a> {
    Relation(&'a str),
    Query(&'a str),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportOutcome {
    pub path: PathBuf,
    pub bytes_written: u64,
    pub rows: u64,
}

/// Remove whatever an earlier, interrupted run left in the exchange
/// directory.
pub(super) fn empty_directory(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let removed = match entry.file_type() {
            Ok(kind) if kind.is_dir() => std::fs::remove_dir_all(&path),
            _ => std::fs::remove_file(&path),
        };
        if let Err(error) = removed {
            tracing::debug!(%error, "could not clear a notebook exchange file");
        }
    }
}

/// A file in the exchange directory, removed when dropped.
pub(super) struct StagedFile {
    pub path: PathBuf,
    pub sha256: String,
    pub original: PathBuf,
}

impl Drop for StagedFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(unix)]
fn same_file(checked: &std::fs::Metadata, opened: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    checked.dev() == opened.dev() && checked.ino() == opened.ino()
}

#[cfg(not(unix))]
fn same_file(checked: &std::fs::Metadata, opened: &std::fs::Metadata) -> bool {
    checked.len() == opened.len() && checked.modified().ok() == opened.modified().ok()
}

/// Open an approved import path: a regular file (not a symlink, directory
/// or device) no larger than `limit`, and the opened handle is that file.
pub(super) fn open_approved(path: &Path, limit: u64) -> LocalResult<File> {
    let checked = std::fs::symlink_metadata(path).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => rejected(PathRejection::NotFound),
        _ => LocalStoreError::from(error),
    })?;
    if checked.file_type().is_symlink() {
        return Err(rejected(PathRejection::Symlink));
    }
    if !checked.is_file() {
        return Err(rejected(PathRejection::NotARegularFile));
    }
    if checked.len() > limit {
        return Err(LocalStoreError::FileTooLarge {
            size: checked.len(),
            limit,
        });
    }
    let file = File::open(path)?;
    let opened = file.metadata()?;
    if !opened.is_file() || !same_file(&checked, &opened) {
        return Err(rejected(PathRejection::ChangedWhileReading));
    }
    Ok(file)
}

/// The compression suffix DuckDB reads from a file name, kept on the staged
/// copy so `.csv.gz` stays readable.
fn compression_suffix(path: &Path) -> &'static str {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if name.ends_with(".gz") {
        ".gz"
    } else if name.ends_with(".zst") {
        ".zst"
    } else {
        ""
    }
}

fn new_private_file(path: &Path) -> std::io::Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// Copy an approved file into the exchange directory.
pub(super) fn stage_import(
    path: &Path,
    exchange: &Path,
    quota: &WorkspaceQuota,
    cancel: &CancelToken,
) -> LocalResult<StagedFile> {
    let limit = quota.max_import_file_bytes;
    let mut source = open_approved(path, limit)?;
    let mut staged = StagedFile {
        path: exchange.join(format!(
            "import-{}{}",
            uuid::Uuid::new_v4().simple(),
            compression_suffix(path)
        )),
        sha256: String::new(),
        original: path.to_path_buf(),
    };
    let mut target = new_private_file(&staged.path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    let mut copied = 0u64;
    loop {
        cancel.check()?;
        let read = source.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        copied += read as u64;
        if copied > limit {
            return Err(LocalStoreError::FileTooLarge {
                size: copied,
                limit,
            });
        }
        hasher.update(&buffer[..read]);
        target.write_all(&buffer[..read])?;
    }
    target.sync_all()?;
    staged.sha256 = hex(&hasher.finalize());
    Ok(staged)
}

fn file_label(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

fn file_source(request: &ImportRequest<'_>, engine: DatabaseType) -> SourceInfo {
    let mut source = SourceInfo::query(engine, file_label(request.path));
    source.kind = SourceKind::File;
    source.cell_id = request.cell_id.clone();
    source.run_id = request.run_id.clone();
    source
}

pub(super) fn import_staged(
    conn: &::duckdb::Connection,
    request: &ImportRequest<'_>,
    staged: &StagedFile,
    limits: &Limits,
    cancel: &CancelToken,
) -> LocalResult<MaterializeOutcome> {
    let origin = FileOrigin {
        path: staged.original.to_string_lossy().into_owned(),
        sha256: staged.sha256.clone(),
    };
    let format_label = request.format.label();
    let malformed = |error: LocalStoreError| match error {
        LocalStoreError::Engine(message) => LocalStoreError::Malformed {
            format: format_label,
            message,
        },
        other => other,
    };
    let reader = match &request.format {
        ImportFormat::Sqlite { table } => {
            return import_sqlite(conn, request, table, staged, &origin, limits, cancel)
                .map_err(malformed);
        }
        ImportFormat::Csv => "read_csv_auto(?)",
        ImportFormat::Parquet => "read_parquet(?)",
        ImportFormat::Json => "read_json_auto(?)",
        ImportFormat::NdJson => "read_json_auto(?, format = 'newline_delimited')",
    };
    let source = file_source(request, DatabaseType::DuckDb);
    let target = quote_ident(request.relation);
    let staged_path = staged.path.to_string_lossy().into_owned();
    in_transaction(conn, |conn| {
        ingest::prepare_target(conn, request.relation, request.mode)?;
        let fetch = limits.max_rows.saturating_add(1);
        conn.execute(
            &format!("CREATE TABLE {target} AS SELECT * FROM {reader} LIMIT {fetch}"),
            [&staged_path],
        )
        .map_err(|e| malformed(e.into()))?;
        cancel.check()?;

        let mut partial = None;
        let mut rows = count_rows(conn, &target)?;
        if rows > limits.max_rows {
            conn.execute_batch(&format!(
                "DELETE FROM {target} WHERE rowid = (SELECT max(rowid) FROM {target})"
            ))?;
            rows = limits.max_rows;
            partial = Some(PartialReason::RowLimit {
                limit: limits.max_rows,
            });
        }
        let described = describe(conn, &target)?;
        let row_bytes = described
            .iter()
            .map(|(name, _)| {
                format!(
                    "coalesce(strlen(CAST({} AS VARCHAR)), 0)",
                    quote_ident(name)
                )
            })
            .collect::<Vec<_>>()
            .join(" + ");
        let mut bytes = total_bytes(conn, &target, &row_bytes)?;
        if bytes > limits.max_bytes {
            conn.execute_batch(&format!(
                "DELETE FROM {target} WHERE rowid >= (SELECT min(rowid) FROM \
                 (SELECT rowid, sum({row_bytes}) OVER (ORDER BY rowid ROWS UNBOUNDED PRECEDING) \
                 AS running FROM {target}) WHERE running > {})",
                limits.max_bytes
            ))?;
            rows = count_rows(conn, &target)?;
            bytes = total_bytes(conn, &target, &row_bytes)?;
            partial = Some(limits.byte_partial());
        }

        let columns: Vec<LocalColumn> = described
            .into_iter()
            .map(|(name, local_type)| LocalColumn {
                name,
                source_type: String::new(),
                local_type,
                fell_back_to_text: false,
            })
            .collect();
        let materialize = MaterializeRequest {
            relation: request.relation,
            columns: &[],
            source: &source,
            mode: request.mode,
        };
        let checksum = Some(format!("file-sha256:{}", origin.sha256));
        let record = ingest::provenance_record(
            &materialize,
            Some(&origin),
            rows,
            bytes,
            &columns,
            partial.clone(),
            checksum.clone(),
        );
        write_provenance(conn, &record)?;
        Ok(MaterializeOutcome {
            relation: request.relation.to_string(),
            row_count: rows,
            byte_count: bytes,
            columns,
            partial,
            checksum,
        })
    })
}

fn count_rows(conn: &::duckdb::Connection, target: &str) -> LocalResult<u64> {
    let n: i64 = conn.query_row(&format!("SELECT count(*) FROM {target}"), [], |row| {
        row.get(0)
    })?;
    Ok(u64::try_from(n).unwrap_or(0))
}

fn total_bytes(conn: &::duckdb::Connection, target: &str, row_bytes: &str) -> LocalResult<u64> {
    let n: Option<i64> = conn.query_row(
        &format!("SELECT CAST(sum({row_bytes}) AS BIGINT) FROM {target}"),
        [],
        |row| row.get(0),
    )?;
    Ok(n.and_then(|n| u64::try_from(n).ok()).unwrap_or(0))
}

fn describe(conn: &::duckdb::Connection, target: &str) -> LocalResult<Vec<(String, String)>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT column_name, column_type FROM (DESCRIBE {target})"
    ))?;
    let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Rows per batch when reading a SQLite table.
const SQLITE_BATCH: usize = 2048;

/// A `file:` URI that opens the staged copy immutable: read-only, with no
/// `-wal`/`-shm` files created or consulted.
fn sqlite_uri(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    let escaped = text
        .replace('%', "%25")
        .replace('?', "%3F")
        .replace('#', "%23");
    if escaped.starts_with('/') {
        format!("file:{escaped}?immutable=1")
    } else {
        format!("file:///{escaped}?immutable=1")
    }
}

fn sqlite_cell(value: rusqlite::types::ValueRef<'_>) -> CellValue {
    use rusqlite::types::ValueRef;
    match value {
        ValueRef::Null => CellValue::Null,
        ValueRef::Integer(n) => CellValue::I64(n),
        ValueRef::Real(f) => CellValue::F64(f),
        ValueRef::Text(t) => CellValue::String(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => CellValue::Bytes(b.to_vec()),
    }
}

fn import_sqlite(
    conn: &::duckdb::Connection,
    request: &ImportRequest<'_>,
    table: &str,
    staged: &StagedFile,
    origin: &FileOrigin,
    limits: &Limits,
    cancel: &CancelToken,
) -> LocalResult<MaterializeOutcome> {
    use rusqlite::OpenFlags;
    let sqlite_error = |e: rusqlite::Error| LocalStoreError::Engine(e.to_string());
    let db = rusqlite::Connection::open_with_flags(
        sqlite_uri(&staged.path),
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(sqlite_error)?;
    // A hostile file's views and triggers must not run functions with side
    // effects, and nothing may write.
    db.execute_batch("PRAGMA trusted_schema = OFF; PRAGMA query_only = ON;")
        .map_err(sqlite_error)?;
    let exists: bool = db
        .query_row(
            "SELECT count(*) > 0 FROM sqlite_schema WHERE type IN ('table', 'view') AND name = ?1",
            [table],
            |row| row.get(0),
        )
        .map_err(sqlite_error)?;
    if !exists {
        return Err(LocalStoreError::RelationNotFound(table.to_string()));
    }
    let mut stmt = db
        .prepare(&format!("SELECT * FROM {}", quote_ident(table)))
        .map_err(sqlite_error)?;
    let columns: Vec<ResultColumn> = stmt
        .columns()
        .map(|c| ResultColumn {
            name: c.name().to_string(),
            data_type: c.decl_type().unwrap_or("").to_ascii_lowercase(),
            nullable: true,
        })
        .collect();
    let width = columns.len();
    let mut rows = stmt.query([]).map_err(sqlite_error)?;
    let mut finished = false;
    let batches = std::iter::from_fn(|| {
        if finished {
            return None;
        }
        let mut batch: Vec<Vec<CellValue>> = Vec::with_capacity(SQLITE_BATCH);
        while batch.len() < SQLITE_BATCH {
            match rows.next() {
                Ok(Some(row)) => batch.push(
                    (0..width)
                        .map(|i| row.get_ref(i).map(sqlite_cell).unwrap_or(CellValue::Null))
                        .collect(),
                ),
                Ok(None) => {
                    finished = true;
                    break;
                }
                Err(error) => {
                    finished = true;
                    return Some(Err(error.to_string()));
                }
            }
        }
        (!batch.is_empty()).then_some(Ok(batch))
    });
    let batches =
        batches.map(|batch: SourceBatch| batch.map_err(|message| format!("SQLite: {message}")));
    let source = file_source(request, DatabaseType::Sqlite);
    let materialize = MaterializeRequest {
        relation: request.relation,
        columns: &columns,
        source: &source,
        mode: request.mode,
    };
    ingest::materialize_with_origin(conn, &materialize, batches, limits, cancel, Some(origin))
}

// ── Export ──────────────────────────────────────────────────────────────

/// Check an approved export destination: its folder exists, and it is not
/// a directory or symlink, nor an existing file unless `overwrite`.
pub(super) fn check_destination(destination: &Path, overwrite: bool) -> LocalResult<PathBuf> {
    let name = destination
        .file_name()
        .ok_or_else(|| rejected(PathRejection::NotARegularFile))?;
    let parent = match destination.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let parent =
        super::canonical(parent).map_err(|_| rejected(PathRejection::DestinationFolderMissing))?;
    if !parent.is_dir() {
        return Err(rejected(PathRejection::DestinationFolderMissing));
    }
    let destination = parent.join(name);
    if let Ok(meta) = std::fs::symlink_metadata(&destination) {
        if meta.file_type().is_symlink() {
            return Err(rejected(PathRejection::Symlink));
        }
        if !meta.is_file() {
            return Err(rejected(PathRejection::NotARegularFile));
        }
        if !overwrite {
            return Err(rejected(PathRejection::DestinationExists));
        }
    }
    Ok(destination)
}

pub(super) fn export(
    conn: &::duckdb::Connection,
    source: &ExportSource<'_>,
    format: ExportFormat,
    exchange: &Path,
    destination: &Path,
    overwrite: bool,
    limit: u64,
) -> LocalResult<ExportOutcome> {
    let query = match source {
        ExportSource::Relation(name) => format!("SELECT * FROM {}", quote_ident(name)),
        ExportSource::Query(sql) => {
            let mut units = super::read_only_units(sql)?;
            if units.len() != 1 {
                return Err(LocalStoreError::NotReadOnly(
                    "an export takes exactly one query".to_string(),
                ));
            }
            units.remove(0)
        }
    };
    let staged = StagedFile {
        path: exchange.join(format!(
            "export-{}.{}",
            uuid::Uuid::new_v4().simple(),
            format.extension()
        )),
        sha256: String::new(),
        original: destination.to_path_buf(),
    };
    let copy = format!(
        "COPY ({query}) TO {} ({})",
        quote_literal(&staged.path.to_string_lossy()),
        format.copy_options()
    );
    conn.execute_batch("BEGIN TRANSACTION READ ONLY")?;
    let copied = conn.query_row(&copy, [], |row| row.get::<_, i64>(0));
    let _ = conn.execute_batch("ROLLBACK");
    let rows = u64::try_from(copied?).unwrap_or(0);

    let size = std::fs::metadata(&staged.path)?.len();
    if size > limit {
        return Err(LocalStoreError::ExportTooLarge { size, limit });
    }
    place(&staged.path, destination, overwrite)?;
    Ok(ExportOutcome {
        path: destination.to_path_buf(),
        bytes_written: std::fs::metadata(destination)?.len(),
        rows,
    })
}

/// Move the finished export into place. Overwriting renames over the
/// destination (replacing a file, never following a link); otherwise a hard
/// link claims the name only if it is still free. Both fall back to a copy
/// across file systems.
fn place(staged: &Path, destination: &Path, overwrite: bool) -> LocalResult<()> {
    let placed = if overwrite {
        std::fs::rename(staged, destination)
    } else {
        std::fs::hard_link(staged, destination)
    };
    match placed {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(rejected(PathRejection::DestinationExists))
        }
        Err(_) => {
            let mut options = std::fs::OpenOptions::new();
            options.write(true);
            if overwrite {
                options.create(true).truncate(true);
            } else {
                options.create_new(true);
            }
            let mut target = options.open(destination).map_err(|error| {
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    rejected(PathRejection::DestinationExists)
                } else {
                    error.into()
                }
            })?;
            std::io::copy(&mut File::open(staged)?, &mut target)?;
            target.sync_all()?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_uris_escape_query_characters() {
        let uri = sqlite_uri(Path::new("/tmp/a b/x?#%.db"));
        assert_eq!(uri, "file:/tmp/a b/x%3F%23%25.db?immutable=1");
        let windows = sqlite_uri(Path::new("C:\\Users\\me\\x.db"));
        assert_eq!(windows, "file:///C:/Users/me/x.db?immutable=1");
    }

    #[test]
    fn compression_suffix_survives_staging() {
        assert_eq!(compression_suffix(Path::new("a/b.csv.gz")), ".gz");
        assert_eq!(compression_suffix(Path::new("b.JSON.ZST")), ".zst");
        assert_eq!(compression_suffix(Path::new("b.parquet")), "");
    }
}
