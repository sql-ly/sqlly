//! Per-tab last-result cache: one gzipped `encode_sets` payload per tab key
//! under `<data_dir>/SQLLY/result-cache/`, plus a small JSON index carrying
//! the banner metadata. Payloads are bounded by rows and compressed bytes and
//! are never truncated — an over-limit result simply is not cached.

#![cfg(not(target_arch = "wasm32"))]

use crate::intellisense_feedback::redact_sql;
use crate::query_version_history::{SnapshotSet, decode_sets, encode_sets, gunzip, gzip};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

pub fn base_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("SQLLY")
        .join("result-cache")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CachedResultMeta {
    pub key: String,
    pub completed_unix_ms: i64,
    pub engine: String,
    pub query: String,
    pub result_sets: usize,
    pub rows: usize,
    pub bytes: u64,
}

/// The engine a cache entry's `engine` names (written with
/// `DatabaseType::display_name`), or `None` for a name no engine has.
pub fn engine_from_name(name: &str) -> Option<sqlly_lang::sqlmodel::DatabaseType> {
    sqlly_lang::sqlmodel::DatabaseType::ALL
        .into_iter()
        .find(|db| db.display_name() == name)
}

#[derive(Debug, Clone)]
pub struct CacheInput {
    pub engine: String,
    pub query: String,
    pub completed_unix_ms: i64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ResultCacheIndex {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    entries: Vec<CachedResultMeta>,
}

pub struct ResultCache {
    dir: PathBuf,
    mutation: std::sync::Mutex<()>,
}

impl ResultCache {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            mutation: std::sync::Mutex::new(()),
        }
    }

    pub fn default_store() -> Self {
        Self::new(base_dir())
    }

    fn index_path(&self) -> PathBuf {
        self.dir.join("index.json")
    }

    fn payload_path(&self, key: &str) -> Result<PathBuf, String> {
        validate_key(key)?;
        Ok(self.dir.join(format!("{key}.bin.gz")))
    }

    fn try_list(&self) -> Result<Vec<CachedResultMeta>, String> {
        let path = self.index_path();
        if !path.exists() {
            return Ok(Vec::new());
        }
        let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
        serde_json::from_str::<ResultCacheIndex>(&text)
            .map(|index| index.entries)
            .map_err(|e| e.to_string())
    }

    fn write_index(&self, entries: Vec<CachedResultMeta>) -> Result<(), String> {
        std::fs::create_dir_all(&self.dir).map_err(|e| e.to_string())?;
        let index = ResultCacheIndex {
            version: 1,
            entries,
        };
        let text = serde_json::to_string_pretty(&index).map_err(|e| e.to_string())?;
        write_owner_only_atomic(&self.index_path(), text.as_bytes()).map_err(|e| e.to_string())
    }

    /// Encode, gzip, cap-check, write `<key>.bin.gz` with 0o600, update index.
    /// Returns `Ok(None)` when over the caps (and removes any stale entry for the key).
    pub fn store(
        &self,
        key: &str,
        meta_input: CacheInput,
        sets: &[SnapshotSet],
        max_rows: usize,
        max_bytes: u64,
    ) -> Result<Option<CachedResultMeta>, String> {
        validate_key(key)?;
        let _guard = self
            .mutation
            .lock()
            .map_err(|_| crate::localization::text("results.cache.store_busy"))?;

        let rows: usize = sets.iter().map(|s| s.rows.len()).sum();
        if rows > max_rows {
            let _ = self.remove_unlocked(key);
            return Ok(None);
        }

        let encoded = encode_sets(sets);
        let gz = gzip(&encoded).map_err(|e| e.to_string())?;
        if gz.len() as u64 > max_bytes {
            let _ = self.remove_unlocked(key);
            return Ok(None);
        }

        let payload_path = self.payload_path(key)?;
        std::fs::create_dir_all(&self.dir).map_err(|e| e.to_string())?;
        write_owner_only_atomic(&payload_path, &gz).map_err(|e| e.to_string())?;

        let meta = CachedResultMeta {
            key: key.to_string(),
            completed_unix_ms: meta_input.completed_unix_ms,
            engine: meta_input.engine,
            query: redact_sql(&meta_input.query).chars().take(2000).collect(),
            result_sets: sets.len(),
            rows,
            bytes: gz.len() as u64,
        };

        let mut entries = self.try_list()?;
        entries.retain(|entry| entry.key != key);
        entries.push(meta.clone());
        if let Err(error) = self.write_index(entries) {
            let _ = std::fs::remove_file(&payload_path);
            return Err(error);
        }
        Ok(Some(meta))
    }

    pub fn load(&self, key: &str) -> Result<Option<(CachedResultMeta, Vec<SnapshotSet>)>, String> {
        validate_key(key)?;
        let _guard = self
            .mutation
            .lock()
            .map_err(|_| crate::localization::text("results.cache.store_busy"))?;
        let entries = self.try_list()?;
        let Some(meta) = entries.into_iter().find(|entry| entry.key == key) else {
            return Ok(None);
        };
        let payload_path = self.payload_path(key)?;
        if !payload_path.exists() {
            return Ok(None);
        }
        let gz = std::fs::read(&payload_path).map_err(|e| e.to_string())?;
        let raw = gunzip(&gz).map_err(|e| e.to_string())?;
        let sets = decode_sets(&raw)?;
        Ok(Some((meta, sets)))
    }

    pub fn remove(&self, key: &str) -> Result<(), String> {
        validate_key(key)?;
        let _guard = self
            .mutation
            .lock()
            .map_err(|_| crate::localization::text("results.cache.store_busy"))?;
        self.remove_unlocked(key)
    }

    fn remove_unlocked(&self, key: &str) -> Result<(), String> {
        let mut entries = self.try_list()?;
        let before = entries.len();
        entries.retain(|entry| entry.key != key);
        if entries.len() != before {
            self.write_index(entries)?;
        }
        if let Ok(path) = self.payload_path(key) {
            let _ = std::fs::remove_file(path);
        }
        Ok(())
    }

    pub fn clear_all(&self) -> Result<(), String> {
        let _guard = self
            .mutation
            .lock()
            .map_err(|_| crate::localization::text("results.cache.store_busy"))?;
        if self.dir.exists() {
            for entry in std::fs::read_dir(&self.dir).map_err(|e| e.to_string())? {
                let entry = entry.map_err(|e| e.to_string())?;
                let path = entry.path();
                if path.is_file() {
                    std::fs::remove_file(&path).map_err(|e| e.to_string())?;
                }
            }
        }
        Ok(())
    }

    /// Delete payloads not in the index and index rows without a payload.
    pub fn retain_keys(&self, live: &[String]) -> Result<(), String> {
        let _guard = self
            .mutation
            .lock()
            .map_err(|_| crate::localization::text("results.cache.store_busy"))?;
        let live_set: std::collections::HashSet<&str> = live.iter().map(String::as_str).collect();
        let mut entries = self.try_list()?;
        let before = entries.len();
        entries.retain(|entry| {
            live_set.contains(entry.key.as_str())
                && self
                    .payload_path(&entry.key)
                    .ok()
                    .is_some_and(|path| path.exists())
        });
        if entries.len() != before {
            self.write_index(entries.clone())?;
        }
        if self.dir.exists() {
            for entry in std::fs::read_dir(&self.dir).map_err(|e| e.to_string())? {
                let entry = entry.map_err(|e| e.to_string())?;
                let name = entry.file_name().to_string_lossy().to_string();
                if name.ends_with(".bin.gz") {
                    let key = name.trim_end_matches(".bin.gz");
                    if !live_set.contains(key) {
                        let _ = std::fs::remove_file(entry.path());
                    }
                }
            }
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub fn total_bytes(&self) -> u64 {
        let Ok(entries) = self.try_list() else {
            return 0;
        };
        entries
            .iter()
            .filter_map(|meta| self.payload_path(&meta.key).ok())
            .filter_map(|path| std::fs::metadata(path).ok())
            .map(|meta| meta.len())
            .sum::<u64>()
            + if self.index_path().exists() {
                std::fs::metadata(self.index_path())
                    .map(|m| m.len())
                    .unwrap_or(0)
            } else {
                0
            }
    }
}

fn validate_key(key: &str) -> Result<(), String> {
    if uuid::Uuid::parse_str(key).is_err() {
        return Err(crate::localization::text("results.cache.invalid_key"));
    }
    Ok(())
}

fn write_owner_only_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("result-cache");
    let tmp = path.with_file_name(format!(".{file_name}.{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cached entry is restored as the engine it ran on, read back from
    /// the name the cache wrote; a name no engine has leaves it unknown.
    #[test]
    fn engine_names_read_back_as_the_engine() {
        for db in sqlly_lang::sqlmodel::DatabaseType::ALL {
            assert_eq!(engine_from_name(db.display_name()), Some(db), "{db:?}");
        }
        assert_eq!(engine_from_name("Some Other Engine"), None);
        assert_eq!(engine_from_name(""), None);
    }
    use crate::query_version_history::SnapshotSet;
    use sqlly_core::{CellValue, ResultColumn};

    fn temp_store() -> ResultCache {
        let dir = std::env::temp_dir().join(format!(
            "sqlly-result-cache-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        ResultCache::new(dir)
    }

    fn sample_sets() -> Vec<SnapshotSet> {
        vec![SnapshotSet {
            columns: vec![ResultColumn {
                name: "id".into(),
                data_type: "int".to_string(),
                nullable: true,
            }],
            rows: vec![vec![CellValue::I64(1)]],
            rows_affected: None,
        }]
    }

    #[test]
    fn store_load_remove_round_trip() {
        let store = temp_store();
        let key = uuid::Uuid::new_v4().to_string();
        let sets = sample_sets();
        let meta = store
            .store(
                &key,
                CacheInput {
                    engine: "PostgreSQL".into(),
                    query: "SELECT 1".into(),
                    completed_unix_ms: 1_000,
                },
                &sets,
                10_000,
                32 * 1024 * 1024,
            )
            .unwrap()
            .expect("stored");
        assert_eq!(meta.rows, 1);
        let (loaded_meta, loaded_sets) = store.load(&key).unwrap().expect("loaded");
        assert_eq!(loaded_meta, meta);
        assert_eq!(loaded_sets.len(), 1);
        store.remove(&key).unwrap();
        assert!(store.load(&key).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&store.dir);
    }

    #[test]
    fn store_redacts_query_and_caps_length() {
        let store = temp_store();
        let key = uuid::Uuid::new_v4().to_string();
        let meta = store
            .store(
                &key,
                CacheInput {
                    engine: "SQLite".into(),
                    query: "SELECT password = 'secret'".into(),
                    completed_unix_ms: 1,
                },
                &sample_sets(),
                10_000,
                32 * 1024 * 1024,
            )
            .unwrap()
            .expect("stored");
        assert!(meta.query.contains("'<redacted>'"));
        assert!(!meta.query.contains("secret"));

        let key2 = uuid::Uuid::new_v4().to_string();
        let long_query = format!("SELECT {}", "x".repeat(5_000));
        let meta2 = store
            .store(
                &key2,
                CacheInput {
                    engine: "SQLite".into(),
                    query: long_query,
                    completed_unix_ms: 2,
                },
                &sample_sets(),
                10_000,
                32 * 1024 * 1024,
            )
            .unwrap()
            .expect("stored");
        assert_eq!(meta2.query.chars().count(), 2_000);
        let _ = std::fs::remove_dir_all(&store.dir);
    }

    #[test]
    fn store_skips_over_row_cap_and_removes_stale_entry() {
        let store = temp_store();
        let key = uuid::Uuid::new_v4().to_string();
        store
            .store(
                &key,
                CacheInput {
                    engine: "SQLite".into(),
                    query: "SELECT 1".into(),
                    completed_unix_ms: 1,
                },
                &sample_sets(),
                10_000,
                32 * 1024 * 1024,
            )
            .unwrap()
            .expect("stored");
        assert!(
            store
                .store(
                    &key,
                    CacheInput {
                        engine: "SQLite".into(),
                        query: "SELECT 1".into(),
                        completed_unix_ms: 2,
                    },
                    &sample_sets(),
                    0,
                    32 * 1024 * 1024,
                )
                .unwrap()
                .is_none()
        );
        assert!(store.load(&key).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&store.dir);
    }

    #[test]
    fn store_skips_over_byte_cap() {
        let store = temp_store();
        let key = uuid::Uuid::new_v4().to_string();
        assert!(
            store
                .store(
                    &key,
                    CacheInput {
                        engine: "SQLite".into(),
                        query: "SELECT 1".into(),
                        completed_unix_ms: 1,
                    },
                    &sample_sets(),
                    10_000,
                    1,
                )
                .unwrap()
                .is_none()
        );
        assert!(store.load(&key).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&store.dir);
    }

    #[test]
    fn retain_keys_deletes_unlisted_payloads_and_repairs_index() {
        let store = temp_store();
        let kept = uuid::Uuid::new_v4().to_string();
        let orphan = uuid::Uuid::new_v4().to_string();
        store
            .store(
                &kept,
                CacheInput {
                    engine: "SQLite".into(),
                    query: "SELECT 1".into(),
                    completed_unix_ms: 1,
                },
                &sample_sets(),
                10_000,
                32 * 1024 * 1024,
            )
            .unwrap();
        store
            .store(
                &orphan,
                CacheInput {
                    engine: "SQLite".into(),
                    query: "SELECT 2".into(),
                    completed_unix_ms: 2,
                },
                &sample_sets(),
                10_000,
                32 * 1024 * 1024,
            )
            .unwrap();
        store.retain_keys(std::slice::from_ref(&kept)).unwrap();
        assert!(store.load(&kept).unwrap().is_some());
        assert!(store.load(&orphan).unwrap().is_none());
        assert!(!store.payload_path(&orphan).unwrap().exists());
        let _ = std::fs::remove_dir_all(&store.dir);
    }

    #[test]
    fn load_rejects_non_uuid_key() {
        let store = temp_store();
        assert!(store.load("../x").is_err());
        let _ = std::fs::remove_dir_all(&store.dir);
    }

    #[cfg(unix)]
    #[test]
    fn payload_and_index_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let store = temp_store();
        let key = uuid::Uuid::new_v4().to_string();
        store
            .store(
                &key,
                CacheInput {
                    engine: "SQLite".into(),
                    query: "SELECT 1".into(),
                    completed_unix_ms: 1,
                },
                &sample_sets(),
                10_000,
                32 * 1024 * 1024,
            )
            .unwrap()
            .expect("stored");
        let payload_mode = std::fs::metadata(store.payload_path(&key).unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        let index_mode = std::fs::metadata(store.index_path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(payload_mode, 0o600);
        assert_eq!(index_mode, 0o600);
        let _ = std::fs::remove_dir_all(&store.dir);
    }

    #[test]
    fn clear_all_empties_dir_and_total_bytes_is_zero() {
        let store = temp_store();
        let key = uuid::Uuid::new_v4().to_string();
        store
            .store(
                &key,
                CacheInput {
                    engine: "SQLite".into(),
                    query: "SELECT 1".into(),
                    completed_unix_ms: 1,
                },
                &sample_sets(),
                10_000,
                32 * 1024 * 1024,
            )
            .unwrap();
        assert!(store.total_bytes() > 0);
        store.clear_all().unwrap();
        assert_eq!(store.total_bytes(), 0);
        let _ = std::fs::remove_dir_all(&store.dir);
    }
}
