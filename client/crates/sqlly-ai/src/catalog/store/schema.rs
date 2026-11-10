//! Catalog DDL, integrity checking, and schema-generation migrations.

use std::collections::HashSet;

use rusqlite::{Connection, params};

use chrono::Utc;

use crate::catalog::card::{business_meaning_card, extract_prebuilt_text};

use super::CatalogError;
use super::ai_cache::upsert_card;

pub(super) const SCHEMA_SQL: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA busy_timeout = 5000;

CREATE TABLE IF NOT EXISTS catalog_meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS catalog_object (
    object_key        TEXT PRIMARY KEY,
    catalog           TEXT,
    namespace         TEXT,
    name              TEXT NOT NULL,
    kind              TEXT NOT NULL,
    source_kind       TEXT NOT NULL,
    source_path       TEXT,
    source_timestamp  INTEGER,
    object_hash       TEXT NOT NULL,
    body_json         TEXT NOT NULL,
    ai_cache_valid    INTEGER NOT NULL DEFAULT 0,
    ai_card_json      TEXT,
    is_deleted        INTEGER NOT NULL DEFAULT 0,
    first_seen_at     INTEGER NOT NULL,
    last_synced_at    INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS ix_object_hash ON catalog_object(object_hash);
CREATE INDEX IF NOT EXISTS ix_object_ai_stale ON catalog_object(ai_cache_valid) WHERE ai_cache_valid = 0;
CREATE INDEX IF NOT EXISTS ix_object_kind ON catalog_object(kind);

CREATE TABLE IF NOT EXISTS catalog_object_card (
    card_id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    object_key               TEXT NOT NULL,
    card_type                TEXT NOT NULL,
    content                  TEXT NOT NULL,
    token_estimate           INTEGER NOT NULL,
    content_hash             TEXT NOT NULL,
    generated_by             TEXT NOT NULL,
    generated_at             INTEGER NOT NULL,
    card_format_version      INTEGER NOT NULL,
    confidence               REAL NOT NULL,
    contains_model_inference INTEGER NOT NULL,
    UNIQUE(object_key, card_type)
);

CREATE INDEX IF NOT EXISTS ix_catalog_object_card_object_key ON catalog_object_card(object_key);

CREATE TABLE IF NOT EXISTS catalog_column (
    object_key TEXT NOT NULL,
    ordinal    INTEGER NOT NULL,
    name       TEXT NOT NULL,
    type_name  TEXT,
    nullable   INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (object_key, ordinal)
);

CREATE INDEX IF NOT EXISTS ix_column_name ON catalog_column(name);

CREATE TABLE IF NOT EXISTS catalog_relationship (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    from_object_key   TEXT NOT NULL,
    from_column       TEXT NOT NULL,
    to_object_key     TEXT,
    to_namespace      TEXT,
    to_name           TEXT,
    to_column         TEXT,
    is_inferred       INTEGER NOT NULL DEFAULT 0,
    confidence        REAL NOT NULL DEFAULT 1.0,
    relationship_type TEXT NOT NULL DEFAULT 'ForeignKey'
);

CREATE INDEX IF NOT EXISTS ix_rel_from ON catalog_relationship(from_object_key);
CREATE INDEX IF NOT EXISTS ix_rel_to ON catalog_relationship(to_object_key);

CREATE TABLE IF NOT EXISTS catalog_search_document (
    object_key        TEXT PRIMARY KEY,
    document_type     TEXT NOT NULL,
    title             TEXT NOT NULL,
    body              TEXT NOT NULL,
    keywords          TEXT NOT NULL,
    content_hash      TEXT NOT NULL,
    generator_version INTEGER NOT NULL,
    generated_at      INTEGER NOT NULL
);

CREATE VIRTUAL TABLE IF NOT EXISTS catalog_search_fts USING fts5(
    title,
    body,
    keywords,
    object_key UNINDEXED,
    object_type UNINDEXED,
    namespace UNINDEXED,
    tokenize = 'unicode61'
);

CREATE TABLE IF NOT EXISTS catalog_embedding (
    object_key    TEXT PRIMARY KEY,
    content_hash  TEXT NOT NULL,
    dims          INTEGER NOT NULL,
    embedding     BLOB NOT NULL,
    generated_at  INTEGER NOT NULL,
    provider_id   TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS ix_catalog_embedding_content_hash
ON catalog_embedding(content_hash);

CREATE INDEX IF NOT EXISTS ix_catalog_object_live_key
ON catalog_object(object_key) WHERE is_deleted = 0;

CREATE INDEX IF NOT EXISTS ix_catalog_object_live_ns_name
ON catalog_object(namespace, name) WHERE is_deleted = 0;

CREATE INDEX IF NOT EXISTS ix_catalog_embedding_provider_id
ON catalog_embedding(provider_id);

CREATE INDEX IF NOT EXISTS ix_catalog_object_tombstone_purge
ON catalog_object(is_deleted, last_synced_at) WHERE is_deleted = 1;
"#;

/// The catalog's schema generation, tracked in `PRAGMA user_version`. Bumped
/// when a migration lands; a catalog stamped with a HIGHER generation than
/// this build understands is refused (a newer app wrote it) instead of being
/// half-migrated backwards.
const CATALOG_SCHEMA_VERSION: i32 = 2;

/// SQLite's own corruption check, run before any migration touches the file.
/// `quick_check` skips index-content verification, keeping open cheap; real
/// page-level corruption still surfaces.
pub(super) fn verify_integrity(conn: &Connection) -> Result<(), CatalogError> {
    let verdict: String = conn.query_row("PRAGMA quick_check(1)", [], |r| r.get(0))?;
    if verdict == "ok" {
        return Ok(());
    }
    Err(CatalogError::Corrupt(verdict))
}

/// Refuse catalogs stamped by a NEWER schema generation than this build
/// understands; older stamps (including 0, the pre-versioning default) are
/// migrated forward by the idempotent schema batch.
pub(super) fn check_schema_generation(conn: &Connection) -> Result<(), CatalogError> {
    let stamped: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if stamped > CATALOG_SCHEMA_VERSION {
        return Err(CatalogError::NewerSchema {
            stamped,
            supported: CATALOG_SCHEMA_VERSION,
        });
    }
    Ok(())
}

pub(super) fn stamp_schema_generation(conn: &Connection) -> Result<(), CatalogError> {
    conn.execute_batch(&format!("PRAGMA user_version = {CATALOG_SCHEMA_VERSION};"))?;
    Ok(())
}

/// Whether an open-time error means the FILE itself is unusable (page-level
/// corruption or not a SQLite database at all) — the cases a backup-and-rebuild
/// recovers from, as opposed to I/O or logic errors it must not mask.
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn is_corrupt_catalog_error(error: &CatalogError) -> bool {
    match error {
        CatalogError::Corrupt(_) => true,
        CatalogError::Sqlite(rusqlite::Error::SqliteFailure(e, _)) => matches!(
            e.code,
            rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase
        ),
        _ => false,
    }
}

/// Back up a corrupt catalog file (and its WAL/SHM siblings) beside itself so
/// the rebuild never silently destroys evidence.
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn back_up_corrupt_catalog(path: &std::path::Path, detail: &str) {
    let millis = Utc::now().timestamp_millis();
    tracing::warn!(
        path = %path.display(),
        detail,
        "catalog failed its integrity check; backing it up and rebuilding"
    );
    for suffix in ["", "-wal", "-shm"] {
        let source = std::path::PathBuf::from(format!("{}{suffix}", path.display()));
        if source.exists() {
            let backup =
                std::path::PathBuf::from(format!("{}.corrupt-{millis}{suffix}", path.display()));
            if let Err(error) = std::fs::rename(&source, &backup) {
                tracing::warn!(%error, source = %source.display(), "corrupt-catalog backup failed; removing instead");
                let _ = std::fs::remove_file(&source);
            }
        }
    }
}

/// Additively backfill the `catalog_relationship` metadata columns
/// (`is_inferred`, `confidence`, `relationship_type`) on catalogs created before
/// inferred-relationship support existed. Follows the same idempotent
/// `PRAGMA table_info` + `ALTER TABLE ADD COLUMN` style used elsewhere: each
/// column is added only when absent, and existing declared-FK rows inherit the
/// defaults (`0`, `1.0`, `'ForeignKey'`) so they remain authoritative.
pub(super) fn migrate_relationship_metadata(conn: &Connection) -> Result<(), CatalogError> {
    let mut existing: HashSet<String> = HashSet::new();
    {
        let mut stmt = conn.prepare("PRAGMA table_info(catalog_relationship)")?;
        let cols = stmt.query_map([], |r| r.get::<_, String>(1))?;
        for c in cols {
            existing.insert(c?.to_ascii_lowercase());
        }
    }

    if !existing.contains("is_inferred") {
        conn.execute(
            "ALTER TABLE catalog_relationship ADD COLUMN is_inferred INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    if !existing.contains("confidence") {
        conn.execute(
            "ALTER TABLE catalog_relationship ADD COLUMN confidence REAL NOT NULL DEFAULT 1.0",
            [],
        )?;
    }
    if !existing.contains("relationship_type") {
        conn.execute(
            "ALTER TABLE catalog_relationship ADD COLUMN relationship_type TEXT NOT NULL DEFAULT 'ForeignKey'",
            [],
        )?;
    }
    Ok(())
}

pub(super) fn migrate_legacy_ai_cards(conn: &Connection) -> Result<(), CatalogError> {
    let mut rows = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT object_key, ai_card_json
             FROM catalog_object
             WHERE ai_card_json IS NOT NULL",
        )?;
        let mapped =
            stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for row in mapped {
            rows.push(row?);
        }
    }

    let now = Utc::now().timestamp_millis();
    for (object_key, ai_card_json) in rows {
        let exists: i64 = conn.query_row(
            "SELECT COUNT(*) FROM catalog_object_card WHERE object_key=?1 AND card_type='business_meaning'",
            params![object_key],
            |r| r.get(0),
        )?;
        if exists == 0 {
            let text = extract_prebuilt_text(&ai_card_json).unwrap_or(ai_card_json);
            let card = business_meaning_card(text, "legacy_ai_card_json");
            upsert_card(conn, &object_key, &card, now)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{db_source, table};
    use super::super::{CatalogError, CatalogScope, CatalogStore};

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn corrupt_catalog_is_backed_up_and_rebuilt() {
        let dir =
            std::env::temp_dir().join(format!("sqlly-catalog-corrupt-{}", std::process::id()));
        // A killed or failed earlier run leaves this dir behind, and PIDs are
        // reused on long-lived CI agents. Start from an empty directory.
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let scope = CatalogScope {
            root: dir.clone(),
            connection_fingerprint: "test".to_string(),
            database: None,
        };
        let path = scope.db_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // A file that is definitely not a SQLite database.
        std::fs::write(&path, b"this is not a sqlite database, not even close").unwrap();

        let store = CatalogStore::open(&scope).expect("corrupt catalog must be rebuilt, not fatal");
        store
            .sync(&[table("users", &["id"], db_source(Some(1), Some(2)))])
            .unwrap();

        // The corrupt original was preserved beside the rebuilt file.
        let backups: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".corrupt-"))
            .collect();
        assert!(!backups.is_empty(), "corrupt file must be backed up");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn newer_schema_generation_is_refused() {
        let dir = std::env::temp_dir().join(format!("sqlly-catalog-newer-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("newer.sqlite");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch("PRAGMA user_version = 9999;").unwrap();
        }
        let error = match CatalogStore::open_path(&path) {
            Err(error) => error,
            Ok(_) => panic!("newer generation must refuse"),
        };
        assert!(matches!(error, CatalogError::NewerSchema { .. }), "{error}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn v1_catalog_migrates_to_schema_generation_2() {
        let dir =
            std::env::temp_dir().join(format!("sqlly-catalog-v1-migrate-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("v1.sqlite");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                r#"
                CREATE TABLE catalog_object (
                    object_key TEXT PRIMARY KEY,
                    catalog TEXT,
                    namespace TEXT,
                    name TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    source_kind TEXT NOT NULL,
                    source_path TEXT,
                    source_timestamp INTEGER,
                    object_hash TEXT NOT NULL,
                    body_json TEXT NOT NULL,
                    ai_cache_valid INTEGER NOT NULL DEFAULT 0,
                    ai_card_json TEXT,
                    is_deleted INTEGER NOT NULL DEFAULT 0,
                    first_seen_at INTEGER NOT NULL,
                    last_synced_at INTEGER NOT NULL
                );
                CREATE TABLE catalog_embedding (
                    object_key TEXT PRIMARY KEY,
                    content_hash TEXT NOT NULL,
                    dims INTEGER NOT NULL,
                    embedding BLOB NOT NULL,
                    generated_at INTEGER NOT NULL,
                    provider_id TEXT NOT NULL
                );
                PRAGMA user_version = 1;
                "#,
            )
            .unwrap();
        }

        let store = CatalogStore::open_path(&path).unwrap();
        store
            .sync(&[table("users", &["id"], db_source(Some(1), Some(2)))])
            .unwrap();

        let version: i32 = rusqlite::Connection::open(&path)
            .unwrap()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 2);

        let has_provider_index: bool = rusqlite::Connection::open(&path)
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type='index' AND name='ix_catalog_embedding_provider_id'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
            > 0;
        assert!(has_provider_index);

        std::fs::remove_dir_all(&dir).ok();
    }
}
