//! SQLite-backed persistent catalog.
//!
//! One database file per *scope*, where a scope is identified by a stable
//! connection fingerprint plus an optional database name. Opening the same scope
//! twice (across connections/process restarts) rehydrates the catalog so we can
//! diff a freshly-observed schema against what we saw last time.
//!
//! Row model:
//! - every object stores its `object_hash` (always populated) and an optional
//!   `source_timestamp` (the newer of create/modify, when the engine reports
//!   one).
//! - `ai_cache_valid` is the staleness flag: `0` = stale/needs rebuild, `1` =
//!   current. Any schema change flips it to `0`; only the opt-in AI-cache
//!   builder flips it back to `1`.
//! - objects that disappear from a sync become tombstones (`is_deleted = 1`)
//!   rather than being dropped, so we never lose history to a transiently empty
//!   source. A tombstone is resurrected if the object reappears.
//!
//! The implementation is split by concern: this module owns the store handle
//! and open/lifecycle; siblings hold one `impl CatalogStore` block each for
//! sync, search, AI-card caching, embeddings, relationships, object queries,
//! and maintenance, with `schema` holding the DDL and migration free functions.

mod ai_cache;
mod embeddings;
mod maintenance;
mod objects;
mod relationships;
mod schema;
mod search;
mod sql_util;
mod sync;

pub use ai_cache::{AiCacheStatus, NukeAiDataReport, StoredObjectCard};
pub use embeddings::EmbeddingObjectMeta;
pub use maintenance::{MaintenanceOptions, MaintenanceReport};
pub use objects::{StoredObject, ValidationIndexRow};
pub use relationships::{NeighborDirection, NeighborEdge, StoredRelationship};
pub use search::{SchemaSearchHit, SearchFilter};
pub use sync::{ObjectChange, SyncCoverage, SyncReport};

use parking_lot::Mutex;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, params};
use thiserror::Error;

use schema::{
    SCHEMA_SQL, check_schema_generation, migrate_legacy_ai_cards, migrate_relationship_metadata,
    stamp_schema_generation, verify_integrity,
};
#[cfg(not(target_arch = "wasm32"))]
use schema::{back_up_corrupt_catalog, is_corrupt_catalog_error};
use search::ensure_search_index;

/// Errors that can arise talking to the catalog store.
#[derive(Debug, Error)]
pub enum CatalogError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("catalog io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("catalog file failed its integrity check: {0}")]
    Corrupt(String),
    #[error(
        "catalog was written by a newer app (schema generation {stamped}, this build supports {supported}); update the app"
    )]
    NewerSchema { stamped: i32, supported: i32 },
}

impl CatalogError {
    pub fn message(&self) -> sqlly_lang::DisplayMessage {
        use sqlly_lang::DisplayMessage;
        match self {
            Self::Sqlite(error) => DisplayMessage::raw(error.to_string()),
            Self::Io(error) => DisplayMessage::raw(error.to_string()),
            Self::Json(error) => DisplayMessage::raw(error.to_string()),
            Self::Corrupt(detail) => {
                DisplayMessage::new("services.ai.catalog.corrupt", self.to_string())
                    .with_arg("detail", detail)
            }
            Self::NewerSchema { stamped, supported } => {
                DisplayMessage::new("services.ai.catalog.newer_schema", self.to_string())
                    .with_arg("stamped", stamped.to_string())
                    .with_arg("supported", supported.to_string())
            }
        }
    }
}

// Re-export so callers can share the error type.
pub type CatalogResult<T> = Result<T, CatalogError>;

/// The persistent catalog.
pub struct CatalogStore {
    conn: Mutex<Connection>,
}

/// Identifies which catalog file to open. Constructed from the connection
/// fingerprint and database name the engine is scoped to.
#[derive(Debug, Clone)]
pub struct CatalogScope {
    /// Directory holding `Catalogs/<fingerprint>.sqlite`.
    pub root: PathBuf,
    /// Stable connection identity (server|port|database|auth|...), excluding
    /// secrets.
    pub connection_fingerprint: String,
    /// Logical database name within the connection (may be empty/None for
    /// single-DB engines).
    pub database: Option<String>,
}

impl CatalogScope {
    /// Resolve the on-disk path for this scope's SQLite file.
    pub fn db_path(&self) -> PathBuf {
        let stem = match &self.database {
            Some(db) if !db.is_empty() => {
                sanitize(&format!("{}.{}", self.connection_fingerprint, db))
            }
            _ => sanitize(&self.connection_fingerprint),
        };
        self.root.join("Catalogs").join(format!("{stem}.sqlite"))
    }
}

/// Replace anything that isn't a safe filename byte with `_`.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(unix)]
fn tighten_catalog_dir_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mode = meta.permissions().mode();
        if mode & 0o777 != 0o700 {
            let _ = std::fs::set_permissions(
                path,
                std::fs::Permissions::from_mode(mode & !0o777 | 0o700),
            );
        }
    }
}

#[cfg(unix)]
fn tighten_catalog_file_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mode = meta.permissions().mode();
        if mode & 0o077 != 0 {
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode & !0o077));
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn prepare_owner_only_sqlite_file(path: &Path) -> Result<(), CatalogError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        #[cfg(unix)]
        tighten_catalog_dir_permissions(parent);
    }
    if !path.exists() {
        let mut options = std::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let _ = options.open(path)?;
    } else {
        #[cfg(unix)]
        tighten_catalog_file_permissions(path);
    }
    Ok(())
}

impl CatalogStore {
    /// Open (creating if needed) the catalog for a scope. A catalog file that
    /// fails SQLite's integrity check is backed up beside itself (suffixed
    /// `.corrupt-<millis>`) and rebuilt fresh — the catalog is a cache of the
    /// source database, so a rebuild costs one re-sync, never user data.
    pub fn open(scope: &CatalogScope) -> Result<Self, CatalogError> {
        let path = scope.db_path();
        // On wasm the path names a database in sqlite-wasm-rs's in-memory
        // VFS — there is no directory tree to create.
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
            #[cfg(unix)]
            tighten_catalog_dir_permissions(parent);
        }
        match Self::open_path(&path) {
            Ok(store) => Ok(store),
            #[cfg(not(target_arch = "wasm32"))]
            Err(error) if is_corrupt_catalog_error(&error) => {
                back_up_corrupt_catalog(&path, &error.to_string());
                Self::open_path(&path)
            }
            Err(other) => Err(other),
        }
    }

    /// Open an explicit path. Used by in-memory tests.
    pub fn open_path(path: &Path) -> Result<Self, CatalogError> {
        #[cfg(not(target_arch = "wasm32"))]
        prepare_owner_only_sqlite_file(path)?;
        let conn = Connection::open(path)?;
        #[cfg(unix)]
        tighten_catalog_file_permissions(path);
        verify_integrity(&conn)?;
        check_schema_generation(&conn)?;
        conn.execute_batch(SCHEMA_SQL)?;
        migrate_relationship_metadata(&conn)?;
        migrate_legacy_ai_cards(&conn)?;
        ensure_search_index(&conn)?;
        stamp_schema_generation(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Open an in-memory catalog (tests + ephemeral engine sessions).
    pub fn open_in_memory() -> Result<Self, CatalogError> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA_SQL)?;
        migrate_relationship_metadata(&conn)?;
        migrate_legacy_ai_cards(&conn)?;
        ensure_search_index(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Record scope metadata.
    pub fn set_meta(&self, key: &str, value: &str) -> Result<(), CatalogError> {
        self.conn.lock().execute(
            "INSERT INTO catalog_meta(key,value) VALUES(?1,?2)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn get_meta(&self, key: &str) -> Result<Option<String>, CatalogError> {
        Ok(self
            .conn
            .lock()
            .query_row(
                "SELECT value FROM catalog_meta WHERE key=?1",
                params![key],
                |r| r.get(0),
            )
            .optional()?)
    }
}

/// Shared fixtures for the store test modules.
#[cfg(test)]
pub(super) mod test_support {
    use crate::catalog::wire::{
        CatalogColumn, CatalogObject, CatalogObjectKind, CatalogSource, SourceKind,
    };

    pub fn db_source(create: Option<i64>, modify: Option<i64>) -> CatalogSource {
        CatalogSource {
            kind: SourceKind::Database,
            path: Some("profile-A".into()),
            create_date: create,
            modify_date: modify,
        }
    }

    pub fn file_source(create: Option<i64>, modify: Option<i64>) -> CatalogSource {
        CatalogSource {
            kind: SourceKind::File,
            path: Some("/repo/schema.sql".into()),
            create_date: create,
            modify_date: modify,
        }
    }

    pub fn table(name: &str, cols: &[&str], source: CatalogSource) -> CatalogObject {
        CatalogObject {
            schema: Some("dbo".into()),
            name: name.into(),
            kind: CatalogObjectKind::Table,
            columns: cols
                .iter()
                .enumerate()
                .map(|(i, c)| CatalogColumn {
                    name: (*c).into(),
                    type_name: Some("INT".into()),
                    ordinal: i as i64 + 1,
                    nullable: false,
                })
                .collect(),
            primary_key: vec![],
            foreign_keys: vec![],
            parameters: vec![],
            definition: None,
            source,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{db_source, table};
    use super::*;

    #[test]
    fn scope_db_path_is_deterministic_and_sanitized() {
        let scope = CatalogScope {
            root: PathBuf::from("/tmp/sqlly"),
            connection_fingerprint: "srv|1433|db|sql|yes|no".into(),
            database: Some("Sales".into()),
        };
        let p = scope.db_path();
        assert!(p.starts_with("/tmp/sqlly/Catalogs"));
        assert!(p.to_string_lossy().ends_with(".sqlite"));
        // No pipe chars leak into the filename.
        let fname = p.file_name().unwrap().to_string_lossy();
        assert!(!fname.contains('|'), "{fname}");
    }

    #[test]
    fn open_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let scope = CatalogScope {
            root: dir.path().to_path_buf(),
            connection_fingerprint: "conn1".into(),
            database: None,
        };
        {
            let store = CatalogStore::open(&scope).unwrap();
            store
                .sync(&[table("users", &["id"], db_source(Some(1), Some(2)))])
                .unwrap();
        }
        // Reopen same scope — data must survive.
        let store = CatalogStore::open(&scope).unwrap();
        assert_eq!(store.live_object_count().unwrap(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn open_path_creates_owner_only_sqlite_on_unix() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog.sqlite");
        let _store = CatalogStore::open_path(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
