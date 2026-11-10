//! Persistent per-(connection, database) cache of the IntelliSense data model.
//!
//! The live completion state is built from server metadata into memory only,
//! so every app launch (and every first visit to a database within a launch)
//! used to start with an empty catalog until the metadata pull finished — or
//! stayed empty if it failed. This module writes each freshly pulled
//! [`ModelSnapshot`] to disk and lets the next load seed IntelliSense (and the
//! explorer tree) from that file instantly while a background refresh
//! revalidates against the server.
//!
//! Files live beside the validation catalogs, one JSON file per
//! (connection fingerprint, database):
//! `<catalog root>/DataModels/<sanitized-identity>.<hash>.json`. The sanitized
//! prefix is derived from [`ConnectionProfile::catalog_identity`] and the
//! database name; an FNV-1a hash of the raw identity disambiguates collisions.

use serde::{Deserialize, Serialize};
use sqlly_core::ConnectionProfile;
use sqlly_lang::sqlmodel::ModelSnapshot;

/// Bump when the persisted layout changes; loads of other versions are
/// discarded (treated as a cache miss) rather than migrated. Version 2 adds a
/// hash suffix to cache filenames (pre-hash stems could collide across
/// connections) and rejects payloads whose `database` does not match the
/// requested one.
pub const DATAMODEL_CACHE_VERSION: u32 = 2;

/// One persisted data model: the snapshot plus the catalog-level overlays the
/// snapshot cannot carry (the schema list including empty schemas), stamped
/// with when it was pulled from the server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedDataModel {
    pub version: u32,
    /// Unix seconds (UTC) when the snapshot was pulled from the server.
    pub fetched_at_unix: i64,
    pub database: String,
    /// Human-readable server/host this snapshot came from, for the settings
    /// cache manager. `#[serde(default)]` keeps pre-existing v1 cache files
    /// (written before this field existed) readable — they load with an empty
    /// server and self-heal to a populated one on the next refresh.
    #[serde(default)]
    pub server: String,
    /// The frozen [`ConnectionProfile::catalog_identity`] the snapshot was
    /// pulled under, so the cache manager can map a file back to the live
    /// connection(s) that own it. `#[serde(default)]` for the same
    /// backward-compatibility reason as `server`.
    #[serde(default)]
    pub identity: String,
    /// Schema names, including empty schemas (the `list_schemas` overlay).
    pub schemas: Vec<String>,
    pub snapshot: ModelSnapshot,
}

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use super::*;
    use std::path::PathBuf;

    /// Replace anything that isn't a safe filename byte with `_` — same rule
    /// the SQLite catalog files use, so both artifacts of one connection sit
    /// side by side with matching stems.
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

    /// FNV-1a 64 over the raw (unsanitized) identity, hex-encoded. Same
    /// dependency-free algorithm as `sqlly_lang::fsmodel::content_hash`; disambiguates
    /// stems whose sanitized forms collide (`My DB` vs `My_DB`). Not a security
    /// hash.
    fn identity_hash(raw: &str) -> String {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for &b in raw.as_bytes() {
            hash ^= b as u64;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        format!("{hash:016x}")
    }

    /// On-disk path for a (connection, database) data model.
    pub fn cache_path(profile: &ConnectionProfile, database: &str) -> PathBuf {
        let raw = format!("{}.{}", profile.catalog_identity(), database);
        let stem = format!("{}.{}", sanitize(&raw), identity_hash(&raw));
        crate::catalog::catalog_root()
            .join("DataModels")
            .join(format!("{stem}.json"))
    }

    /// Load the persisted data model for a (connection, database), if one
    /// exists and matches the current cache version. Any read/parse failure is
    /// a cache miss — the caller falls back to a live pull.
    pub fn load(profile: &ConnectionProfile, database: &str) -> Option<PersistedDataModel> {
        let path = cache_path(profile, database);
        let bytes = std::fs::read(&path).ok()?;
        let parsed: PersistedDataModel = serde_json::from_slice(&bytes)
            .map_err(|e| {
                tracing::warn!("discarding unreadable data-model cache {path:?}: {e}");
                e
            })
            .ok()?;
        (parsed.version == DATAMODEL_CACHE_VERSION && parsed.database == database).then_some(parsed)
    }

    /// Persist a freshly pulled data model. Written atomically (temp file +
    /// rename) so a crash mid-write never leaves a truncated cache behind.
    pub fn save(
        profile: &ConnectionProfile,
        database: &str,
        snapshot: &ModelSnapshot,
        schemas: &[String],
        fetched_at_unix: i64,
    ) -> std::io::Result<()> {
        let path = cache_path(profile, database);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let payload = PersistedDataModel {
            version: DATAMODEL_CACHE_VERSION,
            fetched_at_unix,
            database: database.to_string(),
            server: profile.server.trim().to_string(),
            identity: profile.catalog_identity(),
            schemas: schemas.to_vec(),
            snapshot: snapshot.clone(),
        };
        let bytes = serde_json::to_vec(&payload).map_err(std::io::Error::other)?;
        // web_time on wasm: std::time::SystemTime panics there (this path is
        // unreachable on wasm — the create_dir_all above fails first — but a
        // clock that cannot panic keeps that invariant local).
        #[cfg(not(target_arch = "wasm32"))]
        use std::time::SystemTime;
        #[cfg(target_arch = "wasm32")]
        use web_time::SystemTime;
        let unique = format!(
            "{}.{}.json.tmp",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let tmp = path.with_extension(unique);
        std::fs::write(&tmp, &bytes)?;
        if let Err(e) = std::fs::rename(&tmp, &path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        Ok(())
    }

    /// The directory that holds every data-model cache file. Handed to the
    /// settings "reveal in file manager" button and to [`list_entries`].
    pub fn cache_dir() -> PathBuf {
        crate::catalog::catalog_root().join("DataModels")
    }

    /// Only the small header fields of a cache file, so [`list_entries`] can
    /// describe every cache without deserializing the (potentially large)
    /// snapshot payload — serde skips the ignored `snapshot`/`schemas` keys as
    /// `IgnoredAny` instead of allocating them.
    #[derive(Deserialize)]
    struct CacheHeader {
        #[serde(default)]
        version: u32,
        #[serde(default)]
        fetched_at_unix: i64,
        #[serde(default)]
        database: String,
        #[serde(default)]
        server: String,
        #[serde(default)]
        identity: String,
    }

    /// Enumerate every data-model cache file currently on disk, for the
    /// settings cache manager. Unreadable, mis-versioned, or non-`.json` files
    /// are skipped rather than surfaced. The returned list is unsorted; the
    /// caller orders it for display. Never errors — a missing cache directory
    /// (nothing has ever been cached) is an empty list.
    pub fn list_entries() -> Vec<CachedModelEntry> {
        list_entries_in(&cache_dir())
    }

    /// [`list_entries`] against an explicit directory. Split out so tests can
    /// point it at a temp directory without touching `$SQLLY_CATALOG_DIR`.
    pub(crate) fn list_entries_in(dir: &std::path::Path) -> Vec<CachedModelEntry> {
        let Ok(read_dir) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut entries = Vec::new();
        for dir_entry in read_dir.flatten() {
            let path = dir_entry.path();
            // Skip `.json.tmp` half-writes and any stray non-cache file.
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(metadata) = dir_entry.metadata() else {
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            let Ok(header) = serde_json::from_slice::<CacheHeader>(&bytes) else {
                continue;
            };
            if header.version != DATAMODEL_CACHE_VERSION {
                continue;
            }
            entries.push(CachedModelEntry {
                path,
                server: header.server,
                identity: header.identity,
                database: header.database,
                fetched_at_unix: header.fetched_at_unix,
                size_bytes: metadata.len(),
            });
        }
        entries
    }

    /// Delete a single data-model cache file. An already-absent file is a
    /// success (the caller's goal — the cache being gone — is satisfied), so a
    /// double-click or a concurrent refresh's cleanup never surfaces an error.
    pub fn delete(path: &std::path::Path) -> std::io::Result<()> {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
}

/// One data-model cache file described for the settings cache manager.
#[derive(Debug, Clone)]
pub struct CachedModelEntry {
    /// Absolute path to the JSON cache file — the delete target and the row's
    /// stable identity in the UI.
    pub path: std::path::PathBuf,
    /// Human-readable server/host, or empty for legacy files written before
    /// the field existed (they self-heal on the next refresh).
    pub server: String,
    /// The frozen [`ConnectionProfile::catalog_identity`] the snapshot was
    /// pulled under; empty for legacy files. Lets the app evict the matching
    /// in-memory snapshot when this file is cleared.
    pub identity: String,
    /// Database the snapshot describes.
    pub database: String,
    /// Unix seconds (UTC) when the snapshot was pulled from the server.
    pub fetched_at_unix: i64,
    /// On-disk size of the cache file in bytes.
    pub size_bytes: u64,
}

#[cfg(not(target_arch = "wasm32"))]
pub use native::{cache_dir, cache_path, delete, list_entries, load, save};

// The browser build has no filesystem; the persistent cache is a no-op there
// and IntelliSense keeps its in-memory-only behavior.
#[cfg(target_arch = "wasm32")]
pub fn load(_profile: &ConnectionProfile, _database: &str) -> Option<PersistedDataModel> {
    None
}

#[cfg(target_arch = "wasm32")]
pub fn save(
    _profile: &ConnectionProfile,
    _database: &str,
    _snapshot: &ModelSnapshot,
    _schemas: &[String],
    _fetched_at_unix: i64,
) -> std::io::Result<()> {
    Ok(())
}

// The browser build never persists a cache, so the manager has nothing to
// enumerate, reveal, or delete.
#[cfg(target_arch = "wasm32")]
pub fn list_entries() -> Vec<CachedModelEntry> {
    Vec::new()
}

#[cfg(target_arch = "wasm32")]
pub fn cache_dir() -> std::path::PathBuf {
    std::path::PathBuf::from("/sqlly/catalogs/DataModels")
}

#[cfg(target_arch = "wasm32")]
pub fn delete(_path: &std::path::Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use sqlly_core::connection::{SecretProvider, SecretRef};
    use sqlly_lang::sqlmodel::ModelSnapshot;
    use sqlly_testkit::EnvGuard;

    fn sql_profile(server: &str) -> ConnectionProfile {
        ConnectionProfile::sql_login(
            "local",
            server,
            "sa",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: "password".to_string(),
            },
        )
    }

    /// A cache file's on-disk JSON, mirroring what `save` writes.
    fn write_cache(dir: &std::path::Path, stem: &str, model: &PersistedDataModel) {
        std::fs::create_dir_all(dir).unwrap();
        let bytes = serde_json::to_vec(model).unwrap();
        std::fs::write(dir.join(format!("{stem}.json")), bytes).unwrap();
    }

    fn sample(database: &str, server: &str, identity: &str) -> PersistedDataModel {
        PersistedDataModel {
            version: DATAMODEL_CACHE_VERSION,
            fetched_at_unix: 1_700_000_000,
            database: database.to_string(),
            server: server.to_string(),
            identity: identity.to_string(),
            schemas: vec!["dbo".to_string()],
            snapshot: ModelSnapshot::empty(),
        }
    }

    #[test]
    fn list_entries_reports_server_database_and_size() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        write_cache(
            dir,
            "a",
            &sample("Sales", "prod-sql", "prod-sql|1433|Sales|sql:sa|t|f"),
        );
        write_cache(
            dir,
            "b",
            &sample("Ops", "prod-sql", "prod-sql|1433|Ops|sql:sa|t|f"),
        );

        let mut entries = native::list_entries_in(dir);
        entries.sort_by(|a, b| a.database.cmp(&b.database));
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].database, "Ops");
        assert_eq!(entries[0].server, "prod-sql");
        assert_eq!(entries[0].identity, "prod-sql|1433|Ops|sql:sa|t|f");
        assert!(entries[0].size_bytes > 0);
        assert!(entries[1].database == "Sales");
    }

    #[test]
    fn list_entries_skips_wrong_version_tmp_and_junk() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        write_cache(dir, "good", &sample("Keep", "s", "s"));
        // Wrong version → treated as a miss, not surfaced.
        let mut stale = sample("Drop", "s", "s");
        stale.version = DATAMODEL_CACHE_VERSION + 1;
        write_cache(dir, "stale", &stale);
        // A half-written temp file and an unrelated file are both ignored.
        std::fs::write(dir.join("half.json.tmp"), b"{").unwrap();
        std::fs::write(dir.join("notes.txt"), b"hi").unwrap();

        let entries = native::list_entries_in(dir);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].database, "Keep");
    }

    #[test]
    fn list_entries_reads_legacy_file_without_server_or_identity() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        // A pre-`server`/`identity` file: those keys are simply absent.
        let json = serde_json::json!({
            "version": DATAMODEL_CACHE_VERSION,
            "fetched_at_unix": 1_700_000_000i64,
            "database": "Legacy",
            "schemas": ["dbo"],
            "snapshot": ModelSnapshot::empty(),
        });
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("legacy.json"), serde_json::to_vec(&json).unwrap()).unwrap();

        let entries = native::list_entries_in(dir);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].database, "Legacy");
        assert_eq!(entries[0].server, "");
        assert_eq!(entries[0].identity, "");
    }

    #[test]
    fn list_entries_on_missing_directory_is_empty() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("never-created");
        assert!(native::list_entries_in(&missing).is_empty());
    }

    #[test]
    fn delete_removes_the_file_and_is_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("x.json");
        std::fs::write(&path, b"{}").unwrap();
        assert!(path.exists());

        delete(&path).unwrap();
        assert!(!path.exists());
        // Deleting an already-absent file is still Ok — the goal is met.
        delete(&path).unwrap();
    }

    // `cache_path()` is `catalog_root().join("DataModels")/…`, and
    // `catalog_root()` reads PROCESS-GLOBAL env (SQLLY_CATALOG_DIR, and on the
    // non-mac legacy branch, HOME). Any test that compares cache_path() results
    // across more than one call therefore MUST pin the root under `EnvGuard` —
    // otherwise a concurrent env-mutating test (there are several below) can
    // move `catalog_root()` between the calls. That is exactly how
    // `same_identity_same_path` flaked on the Windows agent: path_a resolved to
    // the real `~/Library/Application Support/SQLLY/Catalogs` legacy dir (which
    // exists there) while path_b picked up a sibling test's temp SQLLY_CATALOG_DIR.
    // Holding the guard both serializes against those mutators and pins the root.
    #[test]
    fn colliding_sanitized_identities_get_distinct_paths() {
        let temp = tempfile::tempdir().unwrap();
        let mut env = EnvGuard::acquire();
        env.set("SQLLY_CATALOG_DIR", temp.path().to_str().unwrap());
        let profile_a = sql_profile("my db");
        let profile_b = sql_profile("my_db");
        assert_ne!(
            native::cache_path(&profile_a, "Sales"),
            native::cache_path(&profile_b, "Sales")
        );
        assert_ne!(
            native::cache_path(&sql_profile("localhost"), "My DB"),
            native::cache_path(&sql_profile("localhost"), "My_DB")
        );
    }

    #[test]
    fn same_identity_same_path() {
        // See the note above: pin the catalog root so both cache_path() calls
        // read the same `catalog_root()` and a concurrent env mutation can't
        // make the two "same identity" paths differ.
        let temp = tempfile::tempdir().unwrap();
        let mut env = EnvGuard::acquire();
        env.set("SQLLY_CATALOG_DIR", temp.path().to_str().unwrap());
        let profile = sql_profile("localhost");
        let path_a = native::cache_path(&profile, "Sales");
        let path_b = native::cache_path(&profile, "Sales");
        assert_eq!(path_a, path_b);
    }

    #[test]
    fn load_rejects_database_mismatch() {
        let temp = tempfile::tempdir().unwrap();
        let mut env = EnvGuard::acquire();
        env.set("SQLLY_CATALOG_DIR", temp.path().to_str().unwrap());
        let profile = sql_profile("localhost");
        let snapshot = ModelSnapshot::empty();
        let schemas = vec!["dbo".to_string()];

        native::save(&profile, "DbA", &snapshot, &schemas, 1_700_000_000).unwrap();
        let wrong_path = native::cache_path(&profile, "DbB");
        std::fs::copy(native::cache_path(&profile, "DbA"), &wrong_path).unwrap();

        assert!(native::load(&profile, "DbA").is_some());
        assert!(native::load(&profile, "DbB").is_none());
    }

    #[test]
    fn load_rejects_old_version() {
        let temp = tempfile::tempdir().unwrap();
        let mut env = EnvGuard::acquire();
        env.set("SQLLY_CATALOG_DIR", temp.path().to_str().unwrap());
        let profile = sql_profile("localhost");
        let path = native::cache_path(&profile, "Legacy");
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).unwrap();
        }
        let payload = PersistedDataModel {
            version: 1,
            fetched_at_unix: 1_700_000_000,
            database: "Legacy".to_string(),
            server: String::new(),
            identity: String::new(),
            schemas: vec!["dbo".to_string()],
            snapshot: ModelSnapshot::empty(),
        };
        std::fs::write(path, serde_json::to_vec(&payload).unwrap()).unwrap();

        assert!(native::load(&profile, "Legacy").is_none());
    }
}
